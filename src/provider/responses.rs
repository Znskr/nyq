//! OpenAI Responses API: request body, stream parsing, and a provider for any
//! base URL with bearer-key auth (xAI, OpenCode). Codex reuses the body and
//! stream code with its own ChatGPT auth and headers.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use eventsource_stream::Eventsource;
use futures::StreamExt;
use serde_json::{json, Value};

use super::{sse, EventSink, Provider};
use crate::types::{Item, Request, Response, StreamEvent, Usage};

/// Backend-specific request options.
#[derive(Clone, Copy, Default)]
pub struct Options {
    /// Replace our `web_search` function tool with the native one (Codex).
    pub native_web_search: bool,
    /// Ask for terse text (`text.verbosity: low`), which Codex accepts.
    pub low_verbosity: bool,
}

/// Reasoning items produced by the Anthropic Messages protocol carry a
/// signature, not a Responses `encrypted_content`; never replay those here.
pub(crate) fn is_anthropic_reasoning(id: &Option<String>) -> bool {
    matches!(id.as_deref(), Some(super::anthropic::THINKING | super::anthropic::REDACTED))
}

pub fn body(model: &str, effort: Option<&str>, req: Request<'_>, opts: Options) -> Value {
    let input: Vec<Value> = req.items.iter().filter_map(|item| Some(match item {
        Item::User { text, images } => {
            let mut content = vec![json!({"type":"input_text","text":text})];
            content.extend(images.iter().map(|i| json!({"type":"input_image","detail":"auto","image_url":i.data_url()})));
            json!({"role":"user","content":content})
        }
        Item::Compaction { summary } => json!({"role":"user","content":[{"type":"input_text","text":crate::types::compaction_text(summary)}]}),
        Item::Assistant { text } => json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":text}]}),
        Item::Reasoning { id, .. } if is_anthropic_reasoning(id) => return None,
        Item::Reasoning { summary, encrypted: Some(encrypted), .. } => json!({"type":"reasoning","summary":if summary.is_empty() { vec![] } else { vec![json!({"type":"summary_text","text":summary})] },"encrypted_content":encrypted}),
        Item::Reasoning { encrypted: None, .. } => return None,
        Item::ToolCall { id, name, arguments } => json!({"type":"function_call","call_id":id,"name":name,"arguments":arguments}),
        Item::ToolResult { id, output, .. } => json!({"type":"function_call_output","call_id":id,"output":output}),
    })).collect();
    let tools: Vec<_> = req
        .tools
        .iter()
        .map(|t| {
            if opts.native_web_search && t.name == "web_search" {
                json!({"type":"web_search"})
            } else {
                json!({"type":"function","name":t.name,"description":t.description,"parameters":t.parameters,"strict":false})
            }
        })
        .collect();
    let mut body = json!({"model":model,"store":false,"stream":true,"instructions":req.system,"input":input,"tools":tools,"tool_choice":"auto","parallel_tool_calls":true,"include":["reasoning.encrypted_content"]});
    if opts.low_verbosity {
        body["text"] = json!({"verbosity":"low"});
    }
    if let Some(effort) = effort {
        body["reasoning"] = json!({"effort":if effort == "minimal" { "low" } else { effort },"summary":"auto"});
    }
    body
}

pub(crate) fn output_item(item: &Value) -> Option<Item> {
    match item["type"].as_str()? {
        "message" => Some(Item::Assistant {
            text: item["content"]
                .as_array()?
                .iter()
                .filter(|part| part["type"] == "output_text" || part["type"] == "refusal")
                .filter_map(|part| part["text"].as_str().or_else(|| part["refusal"].as_str()))
                .collect::<String>(),
        }),
        "function_call" => Some(Item::ToolCall {
            id: sse::string(item, "call_id"),
            name: sse::string(item, "name"),
            arguments: sse::string(item, "arguments"),
        }),
        "reasoning" => Some(Item::Reasoning {
            id: item["id"].as_str().map(str::to_owned),
            summary: item["summary"]
                .as_array()
                .map(|parts| parts.iter().filter_map(|part| part["text"].as_str()).collect::<String>())
                .unwrap_or_default(),
            encrypted: item["encrypted_content"].as_str().map(str::to_owned),
        }),
        _ => None,
    }
}

/// Read a Responses event stream into a [`Response`]. `label` names the
/// backend in errors.
pub async fn read_stream(http: reqwest::Response, label: &str, on_event: EventSink<'_>) -> Result<Response> {
    let mut events = sse::checked(http).await?.bytes_stream().eventsource();
    let mut items = BTreeMap::new();
    let mut response = Response::default();
    let mut completed = false;
    let mut searches = std::collections::HashSet::new();
    while let Some(event) = events.next().await {
        let event = event?;
        if event.data.trim() == "[DONE]" {
            break;
        }
        let data: Value = serde_json::from_str(&event.data)?;
        let kind = data["type"].as_str().unwrap_or(&event.event);
        match kind {
            "response.output_text.delta" | "response.refusal.delta" => on_event(StreamEvent::Text(sse::string(&data, "delta"))),
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                on_event(StreamEvent::Reasoning(sse::string(&data, "delta")))
            }
            "response.output_item.added" if data["item"]["type"] == "function_call" => {
                on_event(StreamEvent::ToolCallStart {
                    id: sse::string(&data["item"], "call_id"),
                    name: sse::string(&data["item"], "name"),
                });
            }
            // Native web search runs server-side: show activity, but it
            // is not a tool call for us to run (and is never replayed).
            "response.output_item.added" | "response.output_item.done" if data["item"]["type"] == "web_search_call" => {
                let item = &data["item"];
                let id = sse::string(item, "id");
                let has_query = item["action"]["query"].as_str().is_some();
                if (kind == "response.output_item.added" || has_query) && searches.insert(id.clone()) {
                    on_event(StreamEvent::ToolCallStart { id, name: "web_search".into() });
                }
            }
            "response.output_item.done" => {
                if let Some(item) = output_item(&data["item"]) {
                    let index = data["output_index"].as_u64().context("output item missing output_index")?;
                    items.insert(index, item);
                }
            }
            "response.completed" => {
                let usage = &data["response"]["usage"];
                if !usage.is_null() {
                    response.usage = Some(Usage {
                        input_tokens: usage["input_tokens"].as_u64().unwrap_or(0),
                        output_tokens: usage["output_tokens"].as_u64().unwrap_or(0),
                        cached_tokens: usage["input_tokens_details"]["cached_tokens"].as_u64().unwrap_or(0),
                        reasoning_tokens: usage["output_tokens_details"]["reasoning_tokens"].as_u64().unwrap_or(0),
                        ..Default::default()
                    });
                }
                completed = true;
                break;
            }
            "response.failed" | "error" | "response.incomplete" => anyhow::bail!("{}", sse::failure(&data)),
            _ => {}
        }
    }
    anyhow::ensure!(completed, "{label} stream ended before response.completed");
    response.items = items.into_values().collect();
    Ok(response)
}

/// A Responses API endpoint at `{base_url}/responses` with a bearer key.
pub struct ResponsesProvider {
    pub label: String,
    pub base_url: String,
    pub api_key: Option<String>,
    pub model: String,
    pub effort: Option<String>,
    /// Extra request headers (client identification, conversation id).
    pub headers: Vec<(String, String)>,
}

impl ResponsesProvider {
    pub async fn send(&self, req: Request<'_>, on_event: EventSink<'_>) -> Result<Response> {
        let body = body(&self.model, self.effort.as_deref(), req, Options::default());
        let mut request = crate::http::client()
            .post(format!("{}/responses", self.base_url.trim_end_matches('/')))
            .header("accept", "text/event-stream")
            .json(&body);
        if let Some(key) = &self.api_key {
            request = request.bearer_auth(key);
        }
        for (k, v) in &self.headers {
            request = request.header(k, v);
        }
        read_stream(request.send().await?, &self.label, on_event).await
    }
}

#[async_trait::async_trait]
impl Provider for ResponsesProvider {
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
        Box::new(Self {
            label: self.label.clone(),
            base_url: self.base_url.clone(),
            api_key: self.api_key.clone(),
            model: self.model.clone(),
            effort: self.effort.clone(),
            headers: self.headers.clone(),
        })
    }
    async fn stream(&self, req: Request<'_>, on_event: EventSink<'_>) -> Result<Response> {
        self.send(req, on_event).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::sse::tests::{events, server};

    #[tokio::test]
    async fn generic_endpoint() {
        let body = events(&[
            json!({"type":"response.reasoning_text.delta","delta":"hm"}),
            json!({"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","call_id":"c1","name":"read_file"}}),
            json!({"type":"response.output_item.done","output_index":1,"item":{"type":"function_call","call_id":"c1","name":"read_file","arguments":"{\"path\":\"a\"}"}}),
            json!({"type":"response.output_item.done","output_index":0,"item":{"type":"message","content":[{"type":"output_text","text":"ok"}]}}),
            json!({"type":"response.completed","response":{"usage":{"input_tokens":3,"output_tokens":2}}}),
        ]);
        let (url, mut requests, task) = server(vec![(200, body)]).await;
        let mut provider = ResponsesProvider {
            label: "xai".into(),
            base_url: format!("{url}/v1"),
            api_key: Some("k".into()),
            model: "grok".into(),
            effort: None,
            headers: vec![("User-Agent".into(), "nyq/test".into())],
        };
        provider.set_effort(Some("high".into()));
        let items = [
            Item::user("hi"),
            Item::Reasoning { id: Some(crate::provider::anthropic::THINKING.into()), summary: "x".into(), encrypted: Some("sig".into()) },
        ];
        let mut seen = Vec::new();
        let response = provider
            .stream(Request { system: "sys", items: &items, tools: &[] }, &mut |e| seen.push(e))
            .await
            .unwrap();
        assert_eq!(
            response.items,
            vec![
                Item::Assistant { text: "ok".into() },
                Item::ToolCall { id: "c1".into(), name: "read_file".into(), arguments: "{\"path\":\"a\"}".into() }
            ]
        );
        assert_eq!(seen[0], StreamEvent::Reasoning("hm".into()));
        let request = requests.recv().await.unwrap();
        let lower = request.to_lowercase();
        assert!(lower.starts_with("post /v1/responses "));
        assert!(lower.contains("authorization: bearer k") && lower.contains("user-agent: nyq/test"));
        let sent: Value = serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(sent["reasoning"]["effort"], "high");
        assert!(sent.get("text").is_none());
        // Anthropic thinking signatures are not replayed as encrypted reasoning.
        assert_eq!(sent["input"].as_array().unwrap().len(), 1);
        task.await.unwrap();
    }
}
