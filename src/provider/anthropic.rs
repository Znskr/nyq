//! Anthropic Messages API (`POST {base}/messages`), streamed.
//!
//! Thinking blocks become [`Item::Reasoning`] with `id` [`THINKING`] (text in
//! `summary`, signature in `encrypted`) or [`REDACTED`] (opaque data in
//! `encrypted`), and are replayed unchanged on the next request.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use eventsource_stream::Eventsource;
use futures::StreamExt;
use serde_json::{json, Value};

use super::{sse, EventSink, Provider};
use crate::types::{Item, Request, Response, StreamEvent, Usage};

pub const THINKING: &str = "thinking";
pub const REDACTED: &str = "redacted_thinking";
const VERSION: &str = "2023-06-01";

#[derive(Clone)]
pub struct AnthropicProvider {
    pub label: String,
    pub base_url: String,
    pub api_key: Option<String>,
    pub model: String,
    pub effort: Option<String>,
    /// Output token cap per response.
    pub max_tokens: u64,
    pub headers: Vec<(String, String)>,
}

impl AnthropicProvider {
    pub fn new(label: String, base_url: String, api_key: Option<String>, model: String) -> Self {
        Self { label, base_url, api_key, model, effort: None, max_tokens: 32_000, headers: Vec::new() }
    }

    fn body(&self, req: Request<'_>) -> Value {
        let mut messages: Vec<Value> = Vec::new();
        // Consecutive blocks of the same role share one message.
        let mut push = |role: &str, block: Value| match messages.last_mut() {
            Some(last) if last["role"] == role => last["content"].as_array_mut().unwrap().push(block),
            _ => messages.push(json!({"role": role, "content": [block]})),
        };
        for item in req.items {
            match item {
                Item::User { text, images } => {
                    for image in images {
                        push("user", json!({"type":"image","source":{"type":"base64","media_type":image.media_type,"data":image.data}}));
                    }
                    if !text.is_empty() || images.is_empty() {
                        push("user", json!({"type":"text","text":text}));
                    }
                }
                Item::Compaction { summary } => push("user", json!({"type":"text","text":crate::types::compaction_text(summary)})),
                Item::ToolResult { id, output, is_error } => push(
                    "user",
                    json!({"type":"tool_result","tool_use_id":id,"content":if output.is_empty() { "(no output)" } else { output },"is_error":is_error}),
                ),
                Item::Assistant { text } if text.is_empty() => {}
                Item::Assistant { text } => push("assistant", json!({"type":"text","text":text})),
                Item::Reasoning { id, summary, encrypted: Some(signature) } if id.as_deref() == Some(THINKING) => {
                    push("assistant", json!({"type":"thinking","thinking":summary,"signature":signature}))
                }
                Item::Reasoning { id, encrypted: Some(data), .. } if id.as_deref() == Some(REDACTED) => {
                    push("assistant", json!({"type":"redacted_thinking","data":data}))
                }
                // Reasoning from other protocols cannot be replayed here.
                Item::Reasoning { .. } => {}
                Item::ToolCall { id, name, arguments } => {
                    let input: Value = serde_json::from_str(arguments).ok().filter(Value::is_object).unwrap_or_else(|| json!({}));
                    push("assistant", json!({"type":"tool_use","id":id,"name":name,"input":input}));
                }
            }
        }
        // Cache the conversation prefix up to the newest message.
        if let Some(block) = messages.last_mut().and_then(|m| m["content"].as_array_mut()).and_then(|c| c.last_mut()) {
            if !matches!(block["type"].as_str(), Some(THINKING | REDACTED)) {
                block["cache_control"] = json!({"type":"ephemeral"});
            }
        }
        let mut body = json!({
            "model": self.model,
            "max_tokens": self.max_tokens,
            "stream": true,
            "messages": messages,
        });
        if !req.system.is_empty() {
            body["system"] = json!([{"type":"text","text":req.system,"cache_control":{"type":"ephemeral"}}]);
        }
        if !req.tools.is_empty() {
            body["tools"] = req.tools.iter().map(|t| json!({"name":t.name,"description":t.description,"input_schema":t.parameters})).collect();
        }
        if let Some(effort) = &self.effort {
            if self.model.contains("claude") {
                // Current Claude models think adaptively; effort sets the depth.
                body["thinking"] = json!({"type":"adaptive","display":"summarized"});
                body["output_config"] = json!({"effort": effort});
            } else {
                let budget = match effort.as_str() {
                    "minimal" | "low" => 2_048,
                    "medium" => 8_192,
                    "high" => 16_384,
                    _ => 32_768,
                }
                .min(self.max_tokens.saturating_sub(1).max(1_024));
                body["thinking"] = json!({"type":"enabled","budget_tokens":budget});
            }
        }
        body
    }
}

/// A content block being streamed.
enum Block {
    Text(String),
    Thinking { text: String, signature: String },
    Redacted(String),
    Tool { id: String, name: String, json: String },
}

fn usage_from(usage: &Value, into: &mut Usage) {
    let get = |k: &str| usage[k].as_u64();
    let read = get("cache_read_input_tokens");
    if let Some(input) = get("input_tokens") {
        into.input_tokens = input + read.unwrap_or(0) + get("cache_creation_input_tokens").unwrap_or(0);
    }
    if let Some(read) = read {
        into.cached_tokens = read;
    }
    if let Some(output) = get("output_tokens") {
        into.output_tokens = output;
    }
}

pub async fn read_stream(http: reqwest::Response, label: &str, on_event: EventSink<'_>) -> Result<Response> {
    let mut events = sse::checked(http).await?.bytes_stream().eventsource();
    let mut blocks: BTreeMap<u64, Block> = BTreeMap::new();
    let mut usage = Usage::default();
    let mut saw_usage = false;
    let mut completed = false;
    let mut truncated = false;
    while let Some(event) = events.next().await {
        let event = event?;
        let data: Value = serde_json::from_str(&event.data)?;
        let kind = data["type"].as_str().unwrap_or(&event.event);
        let index = data["index"].as_u64().unwrap_or(0);
        match kind {
            "message_start" => {
                let u = &data["message"]["usage"];
                if u.is_object() {
                    usage_from(u, &mut usage);
                    saw_usage = true;
                }
            }
            "content_block_start" => {
                let block = &data["content_block"];
                let new = match block["type"].as_str().unwrap_or_default() {
                    "text" => Block::Text(sse::string(block, "text")),
                    "thinking" => Block::Thinking { text: sse::string(block, "thinking"), signature: sse::string(block, "signature") },
                    "redacted_thinking" => Block::Redacted(sse::string(block, "data")),
                    "tool_use" => {
                        let (id, name) = (sse::string(block, "id"), sse::string(block, "name"));
                        on_event(StreamEvent::ToolCallStart { id: id.clone(), name: name.clone() });
                        Block::Tool { id, name, json: String::new() }
                    }
                    _ => continue,
                };
                blocks.insert(index, new);
            }
            "content_block_delta" => {
                let delta = &data["delta"];
                match (blocks.get_mut(&index), delta["type"].as_str().unwrap_or_default()) {
                    (Some(Block::Text(text)), "text_delta") => {
                        let piece = sse::string(delta, "text");
                        text.push_str(&piece);
                        on_event(StreamEvent::Text(piece));
                    }
                    (Some(Block::Thinking { text, .. }), "thinking_delta") => {
                        let piece = sse::string(delta, "thinking");
                        text.push_str(&piece);
                        on_event(StreamEvent::Reasoning(piece));
                    }
                    (Some(Block::Thinking { signature, .. }), "signature_delta") => signature.push_str(&sse::string(delta, "signature")),
                    (Some(Block::Tool { json, .. }), "input_json_delta") => json.push_str(&sse::string(delta, "partial_json")),
                    _ => {}
                }
            }
            "message_delta" => {
                if data["usage"].is_object() {
                    usage_from(&data["usage"], &mut usage);
                    saw_usage = true;
                }
                match data["delta"]["stop_reason"].as_str() {
                    None | Some("end_turn" | "tool_use" | "stop_sequence" | "pause_turn" | "refusal") => {}
                    // The output cap is a finished reply, not a protocol failure.
                    Some("max_tokens") => truncated = true,
                    Some(other) => anyhow::bail!("{label} stream stopped early: stop_reason {other}"),
                }
            }
            "message_stop" => {
                completed = true;
                break;
            }
            "error" => anyhow::bail!("{}", sse::failure(&data)),
            _ => {}
        }
    }
    anyhow::ensure!(completed, "{label} stream ended before message_stop");
    let items = blocks
        .into_values()
        .filter_map(|block| match block {
            Block::Text(text) if text.is_empty() => None,
            Block::Text(text) => Some(Item::Assistant { text }),
            Block::Thinking { text, signature } => Some(Item::Reasoning { id: Some(THINKING.into()), summary: text, encrypted: Some(signature) }),
            Block::Redacted(data) => Some(Item::Reasoning { id: Some(REDACTED.into()), summary: String::new(), encrypted: Some(data) }),
            Block::Tool { id, name, json } => Some(Item::ToolCall { id, name, arguments: if json.trim().is_empty() { "{}".into() } else { json } }),
        })
        .collect();
    Ok(Response { items, usage: saw_usage.then_some(usage), truncated })
}

#[async_trait::async_trait]
impl Provider for AnthropicProvider {
    fn name(&self) -> &str {
        &self.label
    }
    fn model(&self) -> &str {
        &self.model
    }
    fn set_model(&mut self, model: String) {
        self.model = model;
    }
    fn effort(&self) -> Option<&str> {
        self.effort.as_deref()
    }
    fn set_effort(&mut self, effort: Option<String>) {
        self.effort = effort.filter(|e| e != "auto");
    }
    fn fork(&self) -> Box<dyn Provider> {
        Box::new(self.clone())
    }
    async fn stream(&self, req: Request<'_>, on_event: EventSink<'_>) -> Result<Response> {
        let mut request = reqwest::Client::new()
            .post(format!("{}/messages", self.base_url.trim_end_matches('/')))
            .header("anthropic-version", VERSION)
            .header("accept", "text/event-stream")
            .json(&self.body(req));
        if let Some(key) = &self.api_key {
            request = request.header("x-api-key", key);
        }
        for (k, v) in &self.headers {
            request = request.header(k, v);
        }
        read_stream(request.send().await.context("sending request")?, &self.label, on_event).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        provider::sse::tests::{events, server},
        types::{Image, ToolSpec},
    };

    fn provider(url: String) -> AnthropicProvider {
        AnthropicProvider::new("opencode".into(), url, Some("key".into()), "claude-test".into())
    }

    #[test]
    fn request_body() {
        let mut p = provider("unused".into());
        let items = vec![
            Item::User { text: "look".into(), images: vec![Image { media_type: "image/png".into(), data: "AAA=".into() }] },
            Item::Reasoning { id: Some(THINKING.into()), summary: "think".into(), encrypted: Some("sig".into()) },
            Item::Reasoning { id: Some("rs_1".into()), summary: "foreign".into(), encrypted: Some("blob".into()) },
            Item::Assistant { text: "checking".into() },
            Item::ToolCall { id: "t1".into(), name: "read_file".into(), arguments: "{\"path\":\"a\"}".into() },
            Item::ToolCall { id: "t2".into(), name: "bash".into(), arguments: "not json".into() },
            Item::ToolResult { id: "t1".into(), output: "contents".into(), is_error: false },
            Item::ToolResult { id: "t2".into(), output: String::new(), is_error: true },
            Item::Reasoning { id: Some(REDACTED.into()), summary: String::new(), encrypted: Some("opaque".into()) },
            Item::Assistant { text: "done".into() },
            Item::user("thanks"),
        ];
        let tools = vec![ToolSpec { name: "read_file".into(), description: "d".into(), parameters: json!({"type":"object"}) }];
        let body = p.body(Request { system: "sys", items: &items, tools: &tools });
        let m = body["messages"].as_array().unwrap();
        assert_eq!(m.len(), 5);
        assert_eq!(m[0]["content"][0]["source"], json!({"type":"base64","media_type":"image/png","data":"AAA="}));
        assert_eq!(m[0]["content"][1], json!({"type":"text","text":"look"}));
        assert_eq!(
            m[1]["content"],
            json!([
                {"type":"thinking","thinking":"think","signature":"sig"},
                {"type":"text","text":"checking"},
                {"type":"tool_use","id":"t1","name":"read_file","input":{"path":"a"}},
                {"type":"tool_use","id":"t2","name":"bash","input":{}}
            ])
        );
        assert_eq!(m[2]["content"][0], json!({"type":"tool_result","tool_use_id":"t1","content":"contents","is_error":false}));
        assert_eq!(m[2]["content"][1]["is_error"], true);
        assert_eq!(m[3]["content"][0], json!({"type":"redacted_thinking","data":"opaque"}));
        assert_eq!(m[4]["content"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(body["system"][0]["text"], "sys");
        assert_eq!(body["tools"][0]["input_schema"], json!({"type":"object"}));
        assert!(body.get("thinking").is_none());
        p.set_effort(Some("high".into()));
        let body = p.body(Request { system: "", items: &[], tools: &[] });
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert_eq!(body["output_config"]["effort"], "high");
        assert!(body.get("system").is_none());
        p.model = "minimax-m3".into();
        let body = p.body(Request { system: "", items: &[], tools: &[] });
        assert_eq!(body["thinking"], json!({"type":"enabled","budget_tokens":16384}));
    }

    #[tokio::test]
    async fn streaming() {
        let body = events(&[
            json!({"type":"message_start","message":{"usage":{"input_tokens":10,"cache_read_input_tokens":90,"cache_creation_input_tokens":5,"output_tokens":1}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"hmm"}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"SIG"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"ping"}),
            json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"hé"}}),
            json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"llo"}}),
            json!({"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"toolu_1","name":"read_file","input":{}}}),
            json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"pa"}}),
            json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"th\":\"x\"}"}}),
            json!({"type":"content_block_start","index":3,"content_block":{"type":"tool_use","id":"toolu_2","name":"list_dir","input":{}}}),
            json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":42}}),
            json!({"type":"message_stop"}),
        ]);
        let (url, mut requests, task) = server(vec![(200, body)]).await;
        let mut p = provider(format!("{url}/zen/v1"));
        p.headers.push(("x-opencode-session".into(), "conv".into()));
        let mut seen = Vec::new();
        let response = p.stream(Request { system: "s", items: &[Item::user("hi")], tools: &[] }, &mut |e| seen.push(e)).await.unwrap();
        assert_eq!(
            response.items,
            vec![
                Item::Reasoning { id: Some(THINKING.into()), summary: "hmm".into(), encrypted: Some("SIG".into()) },
                Item::Assistant { text: "héllo".into() },
                Item::ToolCall { id: "toolu_1".into(), name: "read_file".into(), arguments: "{\"path\":\"x\"}".into() },
                Item::ToolCall { id: "toolu_2".into(), name: "list_dir".into(), arguments: "{}".into() },
            ]
        );
        assert_eq!(response.usage, Some(Usage { input_tokens: 105, output_tokens: 42, cached_tokens: 90 }));
        assert_eq!(
            seen,
            vec![
                StreamEvent::Reasoning("hmm".into()),
                StreamEvent::Text("hé".into()),
                StreamEvent::Text("llo".into()),
                StreamEvent::ToolCallStart { id: "toolu_1".into(), name: "read_file".into() },
                StreamEvent::ToolCallStart { id: "toolu_2".into(), name: "list_dir".into() },
            ]
        );
        let request = requests.recv().await.unwrap().to_lowercase();
        assert!(request.starts_with("post /zen/v1/messages "));
        for header in ["x-api-key: key", "anthropic-version: 2023-06-01", "x-opencode-session: conv"] {
            assert!(request.contains(header), "{header}");
        }
        task.await.unwrap();
    }

    #[tokio::test]
    async fn max_tokens_keeps_the_partial_reply() {
        let body = events(&[
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"partial"}}),
            json!({"type":"message_delta","delta":{"stop_reason":"max_tokens"},"usage":{"output_tokens":8}}),
            json!({"type":"message_stop"}),
        ]);
        let (url, _requests, task) = server(vec![(200, body)]).await;
        let p = provider(url);
        let response = p.stream(Request { system: "", items: &[], tools: &[] }, &mut |_| {}).await.unwrap();
        assert!(response.truncated);
        assert_eq!(response.items, vec![Item::Assistant { text: "partial".into() }]);
        assert_eq!(response.usage.map(|u| u.output_tokens), Some(8));
        task.await.unwrap();
    }

    #[tokio::test]
    async fn errors() {
        let (url, _requests, task) = server(vec![
            (200, events(&[json!({"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}})])),
            (200, events(&[json!({"type":"message_delta","delta":{"stop_reason":"model_context_window_exceeded"}})])),
            (200, events(&[json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":"partial"}})])),
            (401, "bad key".into()),
        ])
        .await;
        let p = provider(url);
        for expected in ["Overloaded", "model_context_window_exceeded", "before message_stop", "401"] {
            let err = p.stream(Request { system: "", items: &[], tools: &[] }, &mut |_| {}).await.unwrap_err();
            assert!(format!("{err:#}").contains(expected), "{err:#}");
        }
        task.await.unwrap();
    }
}
