//! Anthropic Messages API (`POST {base}/messages`), streamed.
//!
//! Thinking blocks become [`Item::Reasoning`] with `id` [`THINKING`] (text in
//! `summary`, signature in `encrypted`) or [`REDACTED`] (opaque data in
//! `encrypted`), and are replayed unchanged on the next request.
//!
//! Also the Models API (`GET {base}/models`), whose capabilities say how a
//! model thinks and which efforts it takes.

use std::collections::{BTreeMap, HashMap};

use anyhow::{Context, Result};
use eventsource_stream::Eventsource;
use futures::StreamExt;
use serde_json::{json, Value};

use super::{sse, EventSink, Provider};
use crate::types::{Item, Request, Response, StreamEvent, Usage};

pub const THINKING: &str = "thinking";
pub const REDACTED: &str = "redacted_thinking";
pub const VERSION: &str = "2023-06-01";

/// How a model is asked to think.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Thinking {
    /// Not known: guessed from the model id ([`infer_thinking`]).
    #[default]
    Unknown,
    /// `{"type":"adaptive"}`, depth set by `output_config.effort`.
    Adaptive,
    /// `{"type":"enabled","budget_tokens":N}`.
    Budget,
    /// The model does not think.
    Off,
}

/// Adaptive thinking for current Claude models (4.6 and later), a token
/// budget for older Claude models and for other models behind
/// Anthropic-compatible endpoints.
pub fn infer_thinking(model: &str) -> Thinking {
    let id = model.to_ascii_lowercase();
    if !id.contains("claude") {
        return Thinking::Budget;
    }
    match claude_version(&id) {
        Some(version) if version < (4, 6) => Thinking::Budget,
        _ => Thinking::Adaptive,
    }
}

/// `(major, minor)` from ids like `claude-opus-4-6`, `claude-3-7-sonnet-…`
/// or `claude-sonnet-4-20250514` (a date is not a minor version).
fn claude_version(id: &str) -> Option<(u32, u32)> {
    let mut numbers = id.split(['-', '.', '@', '/']).skip_while(|p| p.parse::<u32>().is_err());
    let major = numbers.next()?.parse().ok()?;
    let minor = numbers.next().filter(|p| p.len() <= 2).and_then(|p| p.parse().ok()).unwrap_or(0);
    Some((major, minor))
}

/// Efforts a model takes when the Models API has not said: the adaptive
/// ladder for current Claude models, three budgets otherwise.
pub fn default_efforts(model: &str) -> Vec<String> {
    let id = model.to_ascii_lowercase();
    let mut efforts = vec!["low", "medium", "high"];
    if infer_thinking(&id) == Thinking::Adaptive {
        if claude_version(&id).is_none_or(|v| v >= (4, 7)) {
            efforts.push("xhigh");
        }
        efforts.push("max");
    }
    efforts.into_iter().map(String::from).collect()
}

#[derive(Clone)]
pub struct AnthropicProvider {
    pub label: String,
    pub base_url: String,
    pub api_key: Option<String>,
    /// Send the key as `Authorization: Bearer` (`ANTHROPIC_AUTH_TOKEN`)
    /// instead of `x-api-key`.
    pub bearer: bool,
    pub model: String,
    pub effort: Option<String>,
    pub thinking: Thinking,
    /// Ask adaptive models to think even when no effort is chosen.
    pub think_by_default: bool,
    /// Output token cap per response.
    pub max_tokens: u64,
    pub headers: Vec<(String, String)>,
}

impl AnthropicProvider {
    pub fn new(label: String, base_url: String, api_key: Option<String>, model: String) -> Self {
        Self {
            label,
            base_url,
            api_key,
            bearer: false,
            model,
            effort: None,
            thinking: Thinking::Unknown,
            think_by_default: false,
            max_tokens: 32_000,
            headers: Vec::new(),
        }
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
        // Cache the conversation prefix up to the newest message, and up to
        // the previous user message (where the last request's breakpoint
        // was), so long tool rounds still find it. With the system prompt
        // that is three of the four breakpoints a request may carry.
        let previous_user = messages.len().checked_sub(1).and_then(|last| messages[..last].iter().rposition(|m| m["role"] == "user"));
        for index in messages.len().checked_sub(1).into_iter().chain(previous_user) {
            let blocks = messages[index]["content"].as_array_mut().unwrap();
            if let Some(block) = blocks.iter_mut().rev().find(|b| !matches!(b["type"].as_str(), Some(THINKING | REDACTED))) {
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
        let thinking = match self.thinking {
            Thinking::Unknown => infer_thinking(&self.model),
            known => known,
        };
        match (thinking, &self.effort) {
            // Current Claude models think adaptively; effort sets the depth.
            (Thinking::Adaptive, Some(effort)) => {
                body["thinking"] = json!({"type":"adaptive","display":"summarized"});
                body["output_config"] = json!({"effort": effort});
            }
            (Thinking::Adaptive, None) if self.think_by_default => {
                body["thinking"] = json!({"type":"adaptive","display":"summarized"});
            }
            (Thinking::Budget, Some(effort)) => {
                let budget = match effort.as_str() {
                    "minimal" | "low" => 2_048,
                    "medium" => 8_192,
                    "high" => 16_384,
                    _ => 32_768,
                }
                .min(self.max_tokens.saturating_sub(1).max(1_024));
                body["thinking"] = json!({"type":"enabled","budget_tokens":budget});
            }
            _ => {}
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
    let write = get("cache_creation_input_tokens");
    if let Some(input) = get("input_tokens") {
        into.input_tokens = input + read.unwrap_or(0) + write.unwrap_or(0);
    }
    if let Some(write) = write {
        into.cache_write_tokens = write;
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
            "error" => {
                // Overload and server errors mid-stream are as transient as
                // their HTTP forms, so retries treat them alike.
                let status = match data["error"]["type"].as_str() {
                    Some("overloaded_error") => 529,
                    Some("rate_limit_error") => 429,
                    Some("api_error") => 500,
                    _ => anyhow::bail!("{}", sse::failure(&data)),
                };
                return Err(sse::HttpError { status, retry_after: None, message: format!("{label}: {}", sse::failure(&data)) }.into());
            }
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
        let request = reqwest::Client::new()
            .post(format!("{}/messages", self.base_url.trim_end_matches('/')))
            .header("anthropic-version", VERSION)
            .header("accept", "text/event-stream")
            .json(&self.body(req));
        let mut request = authorize(request, self.api_key.as_deref(), self.bearer);
        for (k, v) in &self.headers {
            request = request.header(k, v);
        }
        read_stream(request.send().await.context("sending request")?, &self.label, on_event).await
    }
}

/// `x-api-key`, or `Authorization: Bearer` for `ANTHROPIC_AUTH_TOKEN`.
pub fn authorize(request: reqwest::RequestBuilder, key: Option<&str>, bearer: bool) -> reqwest::RequestBuilder {
    match key {
        Some(key) if bearer => request.bearer_auth(key),
        Some(key) => request.header("x-api-key", key),
        None => request,
    }
}

/// A model as the Models API describes it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ModelInfo {
    pub id: String,
    /// Context window (`max_input_tokens`).
    pub context: Option<u64>,
    /// Output cap (`max_tokens`).
    pub max_output: Option<u64>,
    pub thinking: Thinking,
    /// Efforts it takes, lowest first (empty: none, or not reported).
    pub efforts: Vec<String>,
    pub image_input: Option<bool>,
}

const EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

/// One entry of `GET /v1/models`. Endpoints that only list ids (most
/// Anthropic-compatible ones) leave the rest unknown.
pub fn parse_model(value: &Value) -> Option<ModelInfo> {
    let id = value["id"].as_str().map(str::trim).filter(|id| !id.is_empty())?.to_owned();
    let caps = &value["capabilities"];
    let supported = |v: &Value| v["supported"].as_bool();
    let thinking = match (supported(&caps["thinking"]), supported(&caps["thinking"]["types"]["adaptive"]), supported(&caps["thinking"]["types"]["enabled"])) {
        (Some(false), ..) => Thinking::Off,
        (_, Some(true), _) => Thinking::Adaptive,
        (_, _, Some(true)) => Thinking::Budget,
        _ => Thinking::Unknown,
    };
    let efforts = match supported(&caps["effort"]) {
        Some(true) => EFFORTS.iter().filter(|e| supported(&caps["effort"][**e]) == Some(true)).map(|e| e.to_string()).collect(),
        _ if thinking == Thinking::Budget => vec!["low".into(), "medium".into(), "high".into()],
        _ => Vec::new(),
    };
    Some(ModelInfo {
        id,
        context: value["max_input_tokens"].as_u64().filter(|n| *n > 0),
        max_output: value["max_tokens"].as_u64().filter(|n| *n > 0),
        thinking,
        efforts,
        image_input: supported(&caps["image_input"]),
    })
}

fn models_request(client: &reqwest::Client, url: String, key: Option<&str>, bearer: bool) -> reqwest::RequestBuilder {
    let request = client
        .get(url)
        .header("anthropic-version", VERSION)
        .header(reqwest::header::USER_AGENT, crate::config::USER_AGENT)
        .timeout(std::time::Duration::from_secs(10));
    authorize(request, key, bearer)
}

/// Every model `GET {base}/models` lists, following `has_more`/`last_id`.
pub async fn list_models(base_url: &str, key: Option<&str>, bearer: bool) -> Result<Vec<ModelInfo>> {
    let client = reqwest::Client::new();
    let base = base_url.trim_end_matches('/');
    let mut models = Vec::new();
    let mut after: Option<String> = None;
    for _ in 0..10 {
        let mut url = format!("{base}/models?limit=1000");
        if let Some(id) = &after {
            url.push_str("&after_id=");
            url.push_str(&id.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-._~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect::<String>());
        }
        let response = sse::checked(models_request(&client, url, key, bearer).send().await.context("listing models")?).await?;
        let page: Value = response.json().await.context("parsing the model list")?;
        let data = page["data"].as_array().context("the model list has no data")?;
        models.extend(data.iter().filter_map(parse_model));
        after = page["last_id"].as_str().map(str::to_owned).or_else(|| models.last().map(|m: &ModelInfo| m.id.clone()));
        if page["has_more"].as_bool() != Some(true) || data.is_empty() {
            break;
        }
    }
    remember(base_url, &models);
    Ok(models)
}

/// `GET {base}/models/{id}`; None when the endpoint does not describe it.
pub async fn retrieve_model(base_url: &str, key: Option<&str>, bearer: bool, model: &str) -> Option<ModelInfo> {
    let url = format!("{}/models/{}", base_url.trim_end_matches('/'), model);
    let response = models_request(&reqwest::Client::new(), url, key, bearer).timeout(std::time::Duration::from_secs(4)).send().await.ok()?;
    let value: Value = sse::checked(response).await.ok()?.json().await.ok()?;
    parse_model(&value).filter(|m| m.id == model)
}

/// What the Models API said per (base URL, model id); None = asked, no answer.
type Known = HashMap<(String, String), Option<ModelInfo>>;
static KNOWN: std::sync::Mutex<Option<Known>> = std::sync::Mutex::new(None);

fn key(base_url: &str, model: &str) -> (String, String) {
    (base_url.trim_end_matches('/').to_owned(), model.to_owned())
}

pub fn remember(base_url: &str, models: &[ModelInfo]) {
    let mut known = KNOWN.lock().unwrap_or_else(|e| e.into_inner());
    let known = known.get_or_insert_with(HashMap::new);
    for m in models {
        known.insert(key(base_url, &m.id), Some(m.clone()));
    }
}

/// Some(answer) once the model was listed or looked up.
pub fn known(base_url: &str, model: &str) -> Option<Option<ModelInfo>> {
    KNOWN.lock().unwrap_or_else(|e| e.into_inner()).as_ref()?.get(&key(base_url, model)).cloned()
}

/// Look the model up once per process (a failed lookup is remembered too).
pub async fn describe(base_url: &str, key_: Option<&str>, bearer: bool, model: &str) -> Option<ModelInfo> {
    if let Some(answer) = known(base_url, model) {
        return answer;
    }
    let info = retrieve_model(base_url, key_, bearer, model).await;
    KNOWN.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(HashMap::new).insert(key(base_url, model), info.clone());
    info
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
        assert_eq!(response.usage, Some(Usage { input_tokens: 105, output_tokens: 42, cached_tokens: 90, cache_write_tokens: 5, ..Default::default() }));
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

    #[test]
    fn thinking_follows_the_model() {
        assert_eq!(infer_thinking("claude-opus-5-5"), Thinking::Adaptive);
        assert_eq!(infer_thinking("claude-opus-4-6"), Thinking::Adaptive);
        assert_eq!(infer_thinking("anthropic/claude-sonnet-5"), Thinking::Adaptive);
        assert_eq!(infer_thinking("claude-haiku-4-5"), Thinking::Budget);
        assert_eq!(infer_thinking("claude-sonnet-4-20250514"), Thinking::Budget);
        assert_eq!(infer_thinking("claude-3-7-sonnet-20250219"), Thinking::Budget);
        assert_eq!(infer_thinking("deepseek-v4"), Thinking::Budget);
        assert_eq!(default_efforts("claude-opus-4-6"), ["low", "medium", "high", "max"]);
        assert_eq!(default_efforts("claude-opus-5-5"), ["low", "medium", "high", "xhigh", "max"]);
        assert_eq!(default_efforts("kimi-k3"), ["low", "medium", "high"]);

        let full = json!({"id":"claude-x","display_name":"X","max_input_tokens":1000000,"max_tokens":128000,"capabilities":{
            "image_input":{"supported":true},
            "thinking":{"supported":true,"types":{"enabled":{"supported":false},"adaptive":{"supported":true}}},
            "effort":{"supported":true,"low":{"supported":true},"medium":{"supported":true},"high":{"supported":true},"xhigh":{"supported":false},"max":{"supported":true}}}});
        let info = parse_model(&full).unwrap();
        assert_eq!(info.thinking, Thinking::Adaptive);
        assert_eq!(info.efforts, ["low", "medium", "high", "max"]);
        assert_eq!((info.context, info.max_output, info.image_input), (Some(1_000_000), Some(128_000), Some(true)));
        let budget = json!({"id":"claude-old","capabilities":{"thinking":{"supported":true,"types":{"enabled":{"supported":true},"adaptive":{"supported":false}}},"effort":{"supported":false}}});
        let info = parse_model(&budget).unwrap();
        assert_eq!((info.thinking, info.efforts.len()), (Thinking::Budget, 3));
        let off = parse_model(&json!({"id":"tiny","capabilities":{"thinking":{"supported":false}}})).unwrap();
        assert_eq!((off.thinking, off.efforts.len()), (Thinking::Off, 0));
        // Compatible endpoints often list bare ids.
        assert_eq!(parse_model(&json!({"id":"glm-5"})).unwrap(), ModelInfo { id: "glm-5".into(), ..Default::default() });
        assert!(parse_model(&json!({"id":" "})).is_none());
    }

    #[test]
    fn thinking_request_per_mode() {
        let mut p = provider("unused".into());
        let body = |p: &AnthropicProvider| p.body(Request { system: "", items: &[], tools: &[] });
        p.think_by_default = true;
        assert_eq!(body(&p)["thinking"], json!({"type":"adaptive","display":"summarized"}));
        assert!(body(&p).get("output_config").is_none());
        p.thinking = Thinking::Off;
        p.effort = Some("high".into());
        assert!(body(&p).get("thinking").is_none());
        p.thinking = Thinking::Budget;
        p.max_tokens = 4_096;
        assert_eq!(body(&p)["thinking"], json!({"type":"enabled","budget_tokens":4095}));
        p.effort = None;
        assert!(body(&p).get("thinking").is_none());
    }

    #[test]
    fn cache_breakpoints_cover_system_and_recent_turns() {
        let p = provider("unused".into());
        let items = vec![
            Item::user("one"),
            Item::Assistant { text: "a".into() },
            Item::user("two"),
            Item::Reasoning { id: Some(THINKING.into()), summary: "s".into(), encrypted: Some("sig".into()) },
            Item::ToolCall { id: "t".into(), name: "bash".into(), arguments: "{}".into() },
            Item::ToolResult { id: "t".into(), output: "ok".into(), is_error: false },
        ];
        let body = p.body(Request { system: "sys", items: &items, tools: &[] });
        let marked: Vec<(usize, usize)> = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .enumerate()
            .flat_map(|(i, m)| m["content"].as_array().unwrap().iter().enumerate().filter(|(_, b)| b.get("cache_control").is_some()).map(move |(j, _)| (i, j)))
            .collect();
        // The newest message (the tool result) and the previous user turn.
        assert_eq!(marked, [(2, 0), (4, 0)]);
        assert_eq!(body["messages"][3]["content"][0]["type"], "thinking");
        assert!(body["system"][0].get("cache_control").is_some());
    }

    #[tokio::test]
    async fn overload_is_retried_and_bearer_tokens_are_sent() {
        let ok = events(&[
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":"done"}}),
            json!({"type":"message_stop"}),
        ]);
        let (url, mut requests, task) = server(vec![
            (529, r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#.into()),
            (200, events(&[json!({"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}})])),
            (200, ok),
        ])
        .await;
        let mut p = provider(url);
        p.bearer = true;
        let mut seen = Vec::new();
        let response = crate::provider::stream_with_retry(&p, Request { system: "", items: &[], tools: &[] }, &mut |e| seen.push(e)).await.unwrap();
        assert_eq!(response.items, vec![Item::Assistant { text: "done".into() }]);
        assert_eq!(seen.iter().filter(|e| matches!(e, StreamEvent::Retrying(_))).count(), 2, "{seen:?}");
        let request = requests.recv().await.unwrap().to_lowercase();
        assert!(request.contains("authorization: bearer key") && !request.contains("x-api-key"), "{request}");
        task.await.unwrap();
    }
}
