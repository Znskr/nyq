//! DeepSeek Messages API (`POST {base}/messages` on a base ending in `/v1`),
//! streamed — the Anthropic Messages wire plus DeepSeek's own extensions.
//!
//! Differences from the generic Anthropic adapter ([`super::anthropic`]):
//! - Reasoning effort `off|low|high|max` (default `high`): `low`, `high` and
//!   `max` serialize as `output_config.effort` beside
//!   `thinking: {"type":"enabled"}`; `off` sends
//!   `thinking: {"type":"disabled"}`. `budget_tokens` is never sent.
//! - `max_tokens` defaults to 256_000.
//! - Models declaring `system_prompt_update: in-history` keep the first
//!   top-level `system` and get a changed prompt as a system-role text
//!   message after the cached history, so the prefix stays cacheable.
//! - Models declaring `tool_update` take mid-conversation tool changes as
//!   system-role `tool_addition`/`tool_removal` blocks; tools added after
//!   the baseline are declared with `defer_loading`. Requests carrying
//!   those blocks send the `mid-conversation-tool-changes-2026-07-01` beta
//!   header. Other models get the full current tool list every request.
//! - DeepSeek's own usage names (`prompt_cache_hit_tokens`,
//!   `prompt_cache_miss_tokens`) map into [`Usage`] beside the Anthropic
//!   ones. A terminal response with no content blocks is a retryable
//!   failure.
//!
//! The model listing lives at `GET {origin}/models` (the origin of the
//! Messages base URL), not under `/v1`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use serde_json::{json, Value};

use super::{anthropic, sse, EventSink, Provider};
use crate::catalog::ModelMeta;
use crate::types::{Request, Response, ToolSpec, Usage};

/// `anthropic-beta` opt-in for in-history tool changes.
pub const TOOL_CHANGES_BETA: &str = "mid-conversation-tool-changes-2026-07-01";
/// Conversation id header the endpoint uses for routing and caching.
const SESSION_HEADER: &str = "x-deepseek-harness-session-id";

/// Default per-request output cap.
pub const DEFAULT_MAX_TOKENS: u64 = 256_000;
/// Context window when nothing describes the model.
pub const DEFAULT_CONTEXT: u64 = 1_000_000;
/// What the official listing declares for `deepseek-v4-pro`/`deepseek-flash`.
const KNOWN_CONTEXT: u64 = 1_048_576;

/// The reasoning efforts DeepSeek takes, lowest first.
pub const EFFORTS: [&str; 4] = ["off", "low", "high", "max"];

/// How a model takes mid-conversation tool changes (`tool_update`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolUpdate {
    /// A `tool_addition` block activates a `defer_loading` declaration;
    /// removals rebuild the baseline.
    AdditionOnly,
    /// `tool_removal` blocks are honored too, in conversation order.
    InHistory,
}

/// What the endpoint can do for the current model.
#[derive(Clone, Copy, Debug, Default)]
pub struct Caps {
    /// The endpoint reads the latest `system` message anywhere in
    /// `messages` as the effective prompt, so a changed prompt can follow
    /// the cached history instead of rewriting the leading `system`.
    pub system_in_history: bool,
    /// Mid-conversation tool changes the endpoint reads.
    pub tool_update: Option<ToolUpdate>,
    /// The model reads images; without it they degrade to placeholders.
    pub vision: bool,
}

fn is_flash(model: &str) -> bool {
    model
        .to_ascii_lowercase()
        .starts_with("deepseek-flash")
}

fn is_pro(model: &str) -> bool {
    model.to_ascii_lowercase().starts_with("deepseek-v4-pro")
}

/// Whether nyq knows `model` reads images without asking the endpoint.
pub fn vision_capable(model: &str) -> bool {
    builtin_caps(model).vision
}

/// Capabilities nyq knows without asking the endpoint: `deepseek-flash`
/// reads `system` messages in history, activates deferred tools from
/// `tool_addition` blocks and sees images; `deepseek-v4-pro` is text-only.
fn builtin_caps(model: &str) -> Caps {
    if is_flash(model) {
        Caps {
            system_in_history: true,
            tool_update: Some(ToolUpdate::AdditionOnly),
            vision: true,
        }
    } else {
        Caps::default()
    }
}

/// The effective capabilities for a model: a live listing's
/// `api_capabilities.anthropic_messages` wins wholesale when present
/// (absent members read as leading-only), then the models.dev catalog's
/// modalities, then the built-in defaults.
pub fn capabilities(model: &str, listed: Option<&ListedModel>, meta: Option<&ModelMeta>) -> Caps {
    match listed.and_then(|l| l.messages) {
        Some(messages) => Caps {
            system_in_history: messages.system_in_history,
            tool_update: messages.tool_update,
            vision: listed.is_some_and(|l| l.vision),
        },
        None => {
            let mut caps = builtin_caps(model);
            if let Some(image) = meta.and_then(|m| m.image_input) {
                caps.vision = image;
            }
            caps
        }
    }
}

/// The context window for a model: the listing, then the catalog, then
/// the known 1,048,576 of `deepseek-v4-pro`/`deepseek-flash`, then 1M.
pub fn context_window(model: &str, listed: Option<&ListedModel>, meta: Option<&ModelMeta>) -> u64 {
    listed
        .and_then(|l| l.context)
        .or_else(|| meta.and_then(|m| m.context))
        .unwrap_or(if is_flash(model) || is_pro(model) {
            KNOWN_CONTEXT
        } else {
            DEFAULT_CONTEXT
        })
}

/// The output cap for a model: the listing's `max_output_tokens`, then
/// the catalog's, then 256_000.
pub fn max_tokens(listed: Option<&ListedModel>, meta: Option<&ModelMeta>) -> u64 {
    listed
        .and_then(|l| l.max_output)
        .or_else(|| meta.and_then(|m| m.output))
        .unwrap_or(DEFAULT_MAX_TOKENS)
}

/// A `GET {origin}/models` entry, decoded per dsh's listing contract.
#[derive(Clone, Debug, Default)]
pub struct ListedModel {
    pub id: String,
    /// `context_window`.
    pub context: Option<u64>,
    /// `max_output_tokens`.
    pub max_output: Option<u64>,
    /// `input_modalities` includes `image` (absent modalities = text).
    pub vision: bool,
    /// `api_capabilities.anthropic_messages`; its presence hands update
    /// handling to its members entirely.
    pub messages: Option<ListedMessages>,
}

/// `api_capabilities.anthropic_messages` of one listing entry.
#[derive(Clone, Copy, Debug)]
pub struct ListedMessages {
    /// `system_prompt_update == "in-history"`.
    pub system_in_history: bool,
    /// `tool_update`, when it names a known mode.
    pub tool_update: Option<ToolUpdate>,
}

/// One `data` row of the model listing.
pub fn parse_listed(value: &Value) -> Option<ListedModel> {
    let id = value["id"]
        .as_str()
        .map(str::trim)
        .filter(|id| !id.is_empty())?
        .to_owned();
    let caps = &value["api_capabilities"]["anthropic_messages"];
    let messages = caps.is_object().then(|| ListedMessages {
        system_in_history: caps["system_prompt_update"].as_str() == Some("in-history"),
        tool_update: match caps["tool_update"].as_str() {
            Some("in-history") => Some(ToolUpdate::InHistory),
            Some("addition-only") => Some(ToolUpdate::AdditionOnly),
            _ => None,
        },
    });
    Some(ListedModel {
        id,
        context: value["context_window"].as_u64().filter(|n| *n > 0),
        max_output: value["max_output_tokens"].as_u64().filter(|n| *n > 0),
        vision: value["input_modalities"]
            .as_array()
            .is_some_and(|m| m.iter().any(|v| v.as_str() == Some("image"))),
        messages,
    })
}

/// `GET /models` at the origin of the Messages base URL.
pub fn models_url(base_url: &str) -> String {
    reqwest::Url::parse(base_url.trim_end_matches('/'))
        .ok()
        .map(|u| format!("{}/models", u.origin().ascii_serialization()))
        .unwrap_or_else(|| format!("{}/models", base_url.trim_end_matches('/')))
}

/// Every model `GET {origin}/models` lists.
pub async fn list_models(base_url: &str, key: Option<&str>) -> Result<Vec<ListedModel>> {
    let url = models_url(base_url);
    let request = crate::http::client()
        .get(&url)
        .header(reqwest::header::USER_AGENT, crate::config::USER_AGENT)
        .header("accept", "application/json")
        .timeout(std::time::Duration::from_secs(10));
    let request = anthropic::authorize(request, key, false);
    let response = sse::checked(request.send().await.context("listing models")?).await?;
    let page: Value = response.json().await.context("parsing the model list")?;
    let data = page["data"]
        .as_array()
        .context("the model list has no data")?;
    let models: Vec<ListedModel> = data.iter().filter_map(parse_listed).collect();
    remember(base_url, &models);
    Ok(models)
}

/// What the listing said per (models URL, model id); None = asked, no answer.
type Known = HashMap<(String, String), Option<ListedModel>>;
static KNOWN: Mutex<Option<Known>> = Mutex::new(None);

fn known_key(base_url: &str, model: &str) -> (String, String) {
    (models_url(base_url), model.to_owned())
}

fn remember(base_url: &str, models: &[ListedModel]) {
    let mut known = KNOWN.lock().unwrap_or_else(|e| e.into_inner());
    let known = known.get_or_insert_with(HashMap::new);
    for m in models {
        known.insert(known_key(base_url, &m.id), Some(m.clone()));
    }
}

/// Some(answer) once the model was listed.
pub fn known(base_url: &str, model: &str) -> Option<Option<ListedModel>> {
    KNOWN
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()?
        .get(&known_key(base_url, model))
        .cloned()
}

/// Look the model up once per process (a failed lookup is remembered too).
pub async fn describe(base_url: &str, key: Option<&str>, model: &str) -> Option<ListedModel> {
    if let Some(answer) = known(base_url, model) {
        return answer;
    }
    let info = list_models(base_url, key)
        .await
        .ok()
        .and_then(|models| models.into_iter().find(|m| m.id == model));
    KNOWN
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_with(HashMap::new)
        .insert(known_key(base_url, model), info.clone());
    info
}

/// A declared tool and how the wire currently presents it.
#[derive(Debug)]
struct DeclaredTool {
    name: String,
    spec: ToolSpec,
    /// The declaration carries `defer_loading` (added mid-conversation).
    deferred: bool,
}

/// An activation or removal notice replayed at the tail of each request.
#[derive(Clone, Debug)]
enum Update {
    Addition(String),
    Removal(String),
}

impl Update {
    fn block(&self) -> Value {
        let (kind, name) = match self {
            Update::Addition(n) => ("tool_addition", n),
            Update::Removal(n) => ("tool_removal", n),
        };
        json!({"type":kind,"tool":{"type":"tool_reference","name":name}})
    }
}

/// Wire state the endpoint remembers for this conversation. Because
/// update blocks are injected per request (the neutral items carry no
/// tool history), the net notices replay at the tail every time.
#[derive(Debug, Default)]
pub struct State {
    /// The top-level `system` first sent; in-history routes keep it.
    first_system: Option<String>,
    /// Tool declarations as sent, in declaration order.
    tools: Vec<DeclaredTool>,
    /// Notices replayed at the tail of every request, in order.
    updates: Vec<Update>,
}

/// The tool names the endpoint can call after replaying `updates`.
fn available(state: &State) -> std::collections::HashSet<String> {
    let mut avail: std::collections::HashSet<String> = state
        .tools
        .iter()
        .filter(|t| !t.deferred)
        .map(|t| t.name.clone())
        .collect();
    for update in &state.updates {
        match update {
            Update::Addition(n) => {
                avail.insert(n.clone());
            }
            Update::Removal(n) => {
                avail.remove(n);
            }
        }
    }
    avail
}

fn tool_decl(spec: &ToolSpec, deferred: bool) -> Value {
    let mut tool =
        json!({"name":spec.name,"description":spec.description,"input_schema":spec.parameters});
    if deferred {
        tool["defer_loading"] = json!(true);
    }
    tool
}

/// DeepSeek usage: Anthropic names, or its own
/// `prompt_cache_hit_tokens`/`prompt_cache_miss_tokens`.
fn usage_from(usage: &Value, into: &mut Usage) {
    let get = |k: &str| usage[k].as_u64();
    let read = get("cache_read_input_tokens").or_else(|| get("prompt_cache_hit_tokens"));
    let write = get("cache_creation_input_tokens");
    if let Some(input) = get("input_tokens").or_else(|| get("prompt_cache_miss_tokens")) {
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

/// The wire effort for a configured one: `off` disables thinking, the
/// default is `high`, and rungs DeepSeek lacks map to the nearest it has
/// (`minimal`/`medium` down or up, `xhigh` to `max`).
fn effort_level(effort: Option<&str>) -> Option<&'static str> {
    match effort.unwrap_or("high") {
        "off" | "none" => None,
        "minimal" | "low" => Some("low"),
        "high" | "medium" => Some("high"),
        "max" | "xhigh" => Some("max"),
        _ => Some("high"),
    }
}

#[derive(Clone)]
pub struct DeepSeekProvider {
    pub label: String,
    /// Messages API root ending in `/v1`.
    pub base_url: String,
    pub api_key: Option<String>,
    pub model: String,
    pub effort: Option<String>,
    /// Output token cap per response.
    pub max_tokens: u64,
    /// What the endpoint can do for the current model.
    pub caps: Caps,
    /// Context window reported to the agent loop.
    pub context_window: u64,
    /// Conversation id for `x-deepseek-harness-session-id`.
    pub session: Option<String>,
    pub headers: Vec<(String, String)>,
    /// Per-conversation wire state (a `stream` only takes `&self`).
    pub state: Arc<Mutex<State>>,
}

impl DeepSeekProvider {
    pub fn new(label: String, base_url: String, api_key: Option<String>, model: String) -> Self {
        Self {
            label,
            base_url,
            api_key,
            caps: builtin_caps(&model),
            model,
            effort: None,
            max_tokens: DEFAULT_MAX_TOKENS,
            context_window: DEFAULT_CONTEXT,
            session: None,
            headers: Vec::new(),
            state: Arc::new(Mutex::new(State::default())),
        }
    }

    /// The `system` value for this request and, for in-history routes, a
    /// changed prompt to append as a system message after the history.
    fn system_wire(&self, current: &str, state: &mut State) -> (String, Option<String>) {
        if !self.caps.system_in_history {
            return (current.to_owned(), None);
        }
        match &mut state.first_system {
            // Keep the leading system stable; the newest system message in
            // history carries every change.
            Some(first) => (
                first.clone(),
                (current != first.as_str()).then(|| current.to_owned()),
            ),
            slot => {
                *slot = Some(current.to_owned());
                (current.to_owned(), None)
            }
        }
    }

    /// The `tools` array and the update notices to append as a system
    /// message (empty for models without `tool_update`, which resend the
    /// full current list each request).
    fn tool_wire(&self, specs: &[ToolSpec], state: &mut State) -> (Vec<Value>, Vec<Value>) {
        let Some(mode) = self.caps.tool_update else {
            return (
                specs.iter().map(|s| tool_decl(s, false)).collect(),
                Vec::new(),
            );
        };
        if specs.is_empty() {
            // Tool-less requests (title, compaction, classifier) share this
            // state; an empty set is a tool-less request, not a removal of
            // the conversation's tools.
            return (Vec::new(), Vec::new());
        }
        let current: std::collections::HashSet<&str> =
            specs.iter().map(|t| t.name.as_str()).collect();
        let mut avail = available(state);
        let removed: Vec<String> = avail
            .iter()
            .filter(|n| !current.contains(n.as_str()))
            .cloned()
            .collect();
        let rebuild = state.tools.is_empty()
            || (mode == ToolUpdate::AdditionOnly
                && (!removed.is_empty()
                    || state
                        .updates
                        .iter()
                        .any(|u| matches!(u, Update::Removal(_)))));
        if rebuild {
            // First declarations, or a removal on an addition-only route:
            // rebuild the baseline with every current tool immediate.
            state.tools = specs
                .iter()
                .map(|spec| DeclaredTool {
                    name: spec.name.clone(),
                    spec: spec.clone(),
                    deferred: false,
                })
                .collect();
            state.updates.clear();
            return (
                specs.iter().map(|s| tool_decl(s, false)).collect(),
                Vec::new(),
            );
        }
        if mode == ToolUpdate::InHistory {
            for name in &removed {
                // In-history removals keep the declaration; the removal
                // follows the history instead.
                avail.remove(name);
                state.updates.push(Update::Removal(name.clone()));
            }
        }
        for spec in specs {
            let name = spec.name.as_str();
            match state.tools.iter_mut().find(|t| t.name == name) {
                Some(declared) => {
                    // Refresh the schema; its defer flag is declaration
                    // history and stays.
                    declared.spec = spec.clone();
                    if !avail.contains(name) {
                        // Removed earlier, offered again: re-activate.
                        state.updates.push(Update::Addition(name.to_owned()));
                    }
                }
                None => {
                    state.tools.push(DeclaredTool {
                        name: spec.name.clone(),
                        spec: spec.clone(),
                        deferred: true,
                    });
                    state.updates.push(Update::Addition(name.to_owned()));
                }
            }
            avail.insert(name.to_owned());
        }
        (
            state.tools.iter().map(|t| tool_decl(&t.spec, t.deferred)).collect(),
            state.updates.iter().map(Update::block).collect(),
        )
    }

    /// The request JSON, and whether it carries tool-change blocks (the
    /// `mid-conversation-tool-changes` beta header then applies).
    fn body(&self, req: Request<'_>) -> (Value, bool) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut messages = anthropic::wire_messages(req.items, anthropic::Reasoning::Lenient);
        if messages.is_empty() {
            // A fresh conversation starts a new wire series.
            *state = State::default();
        }
        if !self.caps.vision {
            for m in &mut messages {
                for block in m["content"].as_array_mut().into_iter().flatten() {
                    if block["type"] == "image" {
                        *block = json!({"type":"text","text":"[image omitted: the model is text-only]"});
                    }
                }
            }
        }
        let (tools, updates) = self.tool_wire(req.tools, &mut state);
        let (system, prompt_update) = self.system_wire(req.system, &mut state);
        if !updates.is_empty() {
            messages.push(json!({"role":"system","content":updates}));
        }
        if let Some(text) = prompt_update {
            messages.push(json!({"role":"system","content":[{"type":"text","text":text}]}));
        }
        let mut body = json!({
            "model": self.model,
            "max_tokens": self.max_tokens,
            "stream": true,
            "messages": messages,
        });
        if !system.is_empty() {
            body["system"] = json!(system);
        }
        if !tools.is_empty() {
            body["tools"] = json!(tools);
        }
        match effort_level(self.effort.as_deref()) {
            None => body["thinking"] = json!({"type":"disabled"}),
            Some(effort) => {
                body["thinking"] = json!({"type":"enabled"});
                body["output_config"] = json!({"effort": effort});
            }
        }
        (body, !updates.is_empty())
    }
}

#[async_trait::async_trait]
impl Provider for DeepSeekProvider {
    fn endpoint(&self) -> Option<String> {
        Some(models_url(&self.base_url))
    }
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
    fn efforts(&self) -> Vec<String> {
        EFFORTS.map(String::from).to_vec()
    }
    fn context_window(&self) -> Option<u64> {
        Some(self.context_window)
    }
    fn set_conversation(&mut self, id: &str) {
        self.session = Some(id.to_owned());
    }
    fn fork(&self) -> Box<dyn Provider> {
        let mut fork = self.clone();
        // A fork is a new conversation: no wire history to preserve.
        fork.state = Arc::new(Mutex::new(State::default()));
        Box::new(fork)
    }
    async fn stream(&self, req: Request<'_>, on_event: EventSink<'_>) -> Result<Response> {
        // x-api-key is not an Authorization header: never forward it
        // through redirects (including redirects to another host).
        let client = crate::http::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context("creating Messages client")?;
        let (body, tool_changes) = self.body(req);
        let mut request = client
            .post(format!("{}/messages", self.base_url.trim_end_matches('/')))
            .header("anthropic-version", anthropic::VERSION)
            .header("accept", "text/event-stream")
            .json(&body);
        request = anthropic::authorize(request, self.api_key.as_deref(), false);
        for (k, v) in &self.headers {
            request = request.header(k, v);
        }
        if let Some(session) = &self.session {
            request = request.header(SESSION_HEADER, session);
        }
        if tool_changes {
            request = request.header("anthropic-beta", TOOL_CHANGES_BETA);
        }
        anthropic::read_stream_opts(
            request.send().await.context("sending request")?,
            &self.label,
            anthropic::StreamOpts {
                usage: usage_from,
                empty_fails: true,
            },
            on_event,
        )
        .await
    }
}

#[cfg(test)]
#[allow(clippy::await_holding_lock)]
mod tests {
    use super::*;
    use crate::{
        provider::sse::tests::{events, server},
        types::{Image, Item},
    };

    fn tool(name: &str) -> ToolSpec {
        ToolSpec {
            name: name.into(),
            description: format!("{name} tool"),
            parameters: json!({"type":"object"}),
        }
    }

    fn provider(model: &str) -> DeepSeekProvider {
        DeepSeekProvider::new(
            "deepseek".into(),
            "http://x/v1".into(),
            Some("sk".into()),
            model.into(),
        )
    }

    fn request<'a>(system: &'a str, items: &'a [Item], tools: &'a [ToolSpec]) -> Request<'a> {
        Request {
            system,
            items,
            tools,
        }
    }

    fn thinking(id: Option<&str>, summary: &str, signature: Option<&str>) -> Item {
        Item::Reasoning {
            id: id.map(str::to_owned),
            summary: summary.into(),
            encrypted: signature.map(str::to_owned),
        }
    }

    #[test]
    fn request_body() {
        let mut p = provider("deepseek-v4-pro");
        p.caps.vision = true;
        p.set_effort(Some("max".into()));
        let items = [
            Item::User {
                text: "look".into(),
                images: vec![Image {
                    media_type: "image/png".into(),
                    data: "b64".into(),
                }],
            },
            Item::Assistant { text: "ok".into() },
            thinking(Some("thinking"), "why", Some("sig-1")),
            thinking(Some("redacted_thinking"), "n/a", Some("opaque")),
            thinking(Some("rs_7"), "foreign reasoning", None),
            Item::ToolCall {
                id: "t1".into(),
                name: "read".into(),
                arguments: "not-json".into(),
            },
            Item::ToolResult {
                id: "t1".into(),
                output: String::new(),
                is_error: false,
                labels: Vec::new(),
            },
            Item::user("again"),
        ];
        let tools = [tool("read")];
        let (body, changes) = p.body(request("sys", &items, &tools));
        assert!(!changes);
        assert_eq!(body["model"], "deepseek-v4-pro");
        assert_eq!(body["max_tokens"], 256_000);
        assert_eq!(body["stream"], true);
        assert_eq!(body["system"], "sys");
        // Thinking is explicit; effort goes to output_config; never a budget.
        assert_eq!(body["thinking"], json!({"type":"enabled"}));
        assert_eq!(body["output_config"], json!({"effort":"max"}));
        assert!(!body.to_string().contains("budget_tokens"));
        assert_eq!(body["tools"][0]["name"], "read");
        assert!(body["tools"][0].get("defer_loading").is_none());
        let messages = body["messages"].as_array().unwrap();
        // Images ride inline on a vision model.
        assert_eq!(
            messages[0]["content"],
            json!([
                {"type":"image","source":{"type":"base64","media_type":"image/png","data":"b64"}},
                {"type":"text","text":"look"}
            ])
        );
        let assistant = &messages[1];
        assert_eq!(assistant["role"], "assistant");
        assert_eq!(
            assistant["content"],
            json!([
                {"type":"text","text":"ok"},
                {"type":"thinking","thinking":"why","signature":"sig-1"},
                // Redacted blobs cannot be replayed; foreign reasoning keeps
                // its text without a signature; malformed tool JSON is {}.
                {"type":"thinking","thinking":"foreign reasoning"},
                {"type":"tool_use","id":"t1","name":"read","input":{}}
            ])
        );
        assert_eq!(
            messages[2]["content"],
            json!([
                {"type":"tool_result","tool_use_id":"t1","content":"(no output)","is_error":false},
                {"type":"text","text":"again"}
            ])
        );
    }

    #[test]
    fn effort_wires() {
        let items = [Item::user("hi")];
        for (effort, thinking, output_config) in [
            (None, json!({"type":"enabled"}), json!({"effort":"high"})),
            (Some("auto"), json!({"type":"enabled"}), json!({"effort":"high"})),
            (Some("low"), json!({"type":"enabled"}), json!({"effort":"low"})),
            (Some("minimal"), json!({"type":"enabled"}), json!({"effort":"low"})),
            (Some("high"), json!({"type":"enabled"}), json!({"effort":"high"})),
            (Some("medium"), json!({"type":"enabled"}), json!({"effort":"high"})),
            (Some("max"), json!({"type":"enabled"}), json!({"effort":"max"})),
            (Some("xhigh"), json!({"type":"enabled"}), json!({"effort":"max"})),
            (Some("off"), json!({"type":"disabled"}), Value::Null),
            (Some("none"), json!({"type":"disabled"}), Value::Null),
        ] {
            let mut p = provider("deepseek-v4-pro");
            p.set_effort(effort.map(str::to_owned));
            let (body, _) = p.body(request("s", &items, &[]));
            assert_eq!(body["thinking"], thinking, "{effort:?}");
            assert_eq!(body["output_config"], output_config, "{effort:?}");
            assert!(!body.to_string().contains("budget_tokens"));
        }
        assert_eq!(provider("deepseek-v4-pro").efforts(), ["off", "low", "high", "max"]);
    }

    #[test]
    fn images_degrade_on_text_only_models() {
        let p = provider("deepseek-v4-pro");
        let items = [Item::User {
            text: "what".into(),
            images: vec![Image {
                media_type: "image/png".into(),
                data: "b64".into(),
            }],
        }];
        let (body, _) = p.body(request("s", &items, &[]));
        assert_eq!(
            body["messages"][0]["content"],
            json!([
                {"type":"text","text":"[image omitted: the model is text-only]"},
                {"type":"text","text":"what"}
            ])
        );
    }

    #[test]
    fn system_prompt_updates_in_history() {
        // deepseek-flash keeps the first `system`; changes append after the
        // cached history as a system-role text message.
        let p = provider("deepseek-flash");
        let user = [Item::user("hi")];
        let (first, _) = p.body(request("P1", &user, &[]));
        assert_eq!(first["system"], "P1");
        assert_eq!(first["messages"].as_array().unwrap().len(), 1);
        let history = [
            Item::user("hi"),
            Item::Assistant { text: "hey".into() },
            Item::user("more"),
        ];
        let (changed, _) = p.body(request("P2", &history, &[]));
        assert_eq!(changed["system"], "P1", "the leading system stays stable");
        let messages = changed["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 4);
        assert_eq!(
            messages[3],
            json!({"role":"system","content":[{"type":"text","text":"P2"}]})
        );
        // The update is not durable history: it replays every request.
        let (same, _) = p.body(request("P2", &history, &[]));
        assert_eq!(
            same["messages"].as_array().unwrap().last().unwrap()["role"],
            "system"
        );
        // Back to the original prompt: the leading system is current again.
        let (back, _) = p.body(request("P1", &history, &[]));
        assert_eq!(back["messages"].as_array().unwrap().len(), 3);
        // A model without in-history support gets the current prompt.
        let pro = provider("deepseek-v4-pro");
        let _ = pro.body(request("P1", &history, &[]));
        let (other, _) = pro.body(request("P2", &history, &[]));
        assert_eq!(other["system"], "P2");
        assert_eq!(other["messages"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn tool_changes_addition_only() {
        let p = provider("deepseek-flash");
        let history = [
            Item::user("hi"),
            Item::Assistant { text: "hey".into() },
            Item::user("more"),
        ];
        // First declarations are the baseline: immediate, no notices.
        let (body, changes) = p.body(request("s", &history, &[tool("a")]));
        assert!(!changes);
        assert_eq!(body["tools"], json!([tool_decl(&tool("a"), false)]));
        // An addition is declared deferred and activated by a block that
        // rides a trailing system message.
        let (body, changes) = p.body(request("s", &history, &[tool("a"), tool("b")]));
        assert!(changes);
        assert_eq!(
            body["tools"],
            json!([
                tool_decl(&tool("a"), false),
                tool_decl(&tool("b"), true),
            ])
        );
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(
            messages[3],
            json!({"role":"system","content":[{"type":"tool_addition","tool":{"type":"tool_reference","name":"b"}}]})
        );
        // The notice replays on every later request.
        let (body, changes) = p.body(request("s", &history, &[tool("a"), tool("b")]));
        assert!(changes);
        assert_eq!(body["messages"][3]["role"], "system");
        // A removal on an addition-only route rebuilds the baseline: the
        // remaining tool is immediate again and no blocks go out.
        let (body, changes) = p.body(request("s", &history, &[tool("b")]));
        assert!(!changes);
        assert_eq!(body["tools"], json!([tool_decl(&tool("b"), false)]));
        assert_eq!(body["messages"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn tool_changes_in_history() {
        let mut p = provider("other-model");
        p.caps.tool_update = Some(ToolUpdate::InHistory);
        let history = [Item::user("hi"), Item::user("more")];
        let (body, _) = p.body(request("s", &history, &[tool("a"), tool("b")]));
        assert!(!body["messages"].as_array().unwrap().iter().any(|m| m["role"] == "system"));
        // A removal keeps the declaration and adds a removal notice.
        let (body, changes) = p.body(request("s", &history, &[tool("b")]));
        assert!(changes);
        assert_eq!(
            body["tools"],
            json!([tool_decl(&tool("a"), false), tool_decl(&tool("b"), false)])
        );
        assert_eq!(
            body["messages"].as_array().unwrap().last().unwrap()["content"],
            json!([{"type":"tool_removal","tool":{"type":"tool_reference","name":"a"}}])
        );
        // Offering it again re-activates; the replay keeps order.
        let (body, _) = p.body(request("s", &history, &[tool("a"), tool("b"), tool("c")]));
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 3);
        assert_eq!(tools[2]["defer_loading"], true);
        assert_eq!(
            body["messages"].as_array().unwrap().last().unwrap()["content"],
            json!([
                {"type":"tool_removal","tool":{"type":"tool_reference","name":"a"}},
                {"type":"tool_addition","tool":{"type":"tool_reference","name":"a"}},
                {"type":"tool_addition","tool":{"type":"tool_reference","name":"c"}}
            ])
        );
    }

    #[test]
    fn tool_changes_unsupported_route() {
        // No `tool_update`: the complete current list is resent each time.
        let p = provider("deepseek-v4-pro");
        let history = [Item::user("hi")];
        let (body, changes) = p.body(request("s", &history, &[tool("a"), tool("b")]));
        assert!(!changes);
        assert_eq!(body["tools"].as_array().unwrap().len(), 2);
        let (body, changes) = p.body(request("s", &history, &[tool("b")]));
        assert!(!changes);
        assert_eq!(body["tools"], json!([tool_decl(&tool("b"), false)]));
        assert!(!body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["role"] == "system"));
    }

    #[test]
    fn listing_decoding() {
        let entry = json!({
            "id": "deepseek-flash",
            "context_window": 1_048_576,
            "max_output_tokens": 256_000,
            "input_modalities": ["text", "image"],
            "api_capabilities": {"anthropic_messages": {
                "system_prompt_update": "in-history",
                "tool_update": "addition-only"
            }}
        });
        let listed = parse_listed(&entry).unwrap();
        assert_eq!(listed.context, Some(1_048_576));
        assert_eq!(listed.max_output, Some(256_000));
        assert!(listed.vision);
        let messages = listed.messages.unwrap();
        assert!(messages.system_in_history);
        assert_eq!(messages.tool_update, Some(ToolUpdate::AdditionOnly));
        // A declared update mode governs wholesale: absent = leading-only.
        let bare = parse_listed(&json!({"id":"m","api_capabilities":{"anthropic_messages":{}}})).unwrap();
        let caps = capabilities("deepseek-flash", Some(&bare), None);
        assert_eq!(caps.tool_update, None);
        assert!(!caps.system_in_history);
        assert!(!caps.vision);
        // Unlisted models fall back to built-in and catalog knowledge.
        let caps = capabilities("deepseek-flash", None, None);
        assert!(caps.system_in_history && caps.vision);
        assert_eq!(caps.tool_update, Some(ToolUpdate::AdditionOnly));
        let caps = capabilities("deepseek-v4-pro", None, None);
        assert!(!caps.system_in_history && !caps.vision && caps.tool_update.is_none());
        // Context: listing > catalog > known 1,048,576 > 1M.
        assert_eq!(context_window("deepseek-flash", Some(&listed), None), 1_048_576);
        assert_eq!(context_window("deepseek-v4-pro", None, None), 1_048_576);
        assert_eq!(context_window("unknown", None, None), 1_000_000);
        assert_eq!(max_tokens(Some(&listed), None), 256_000);
        assert_eq!(max_tokens(None, None), 256_000);
        assert_eq!(
            models_url("https://api.deepseek.com/anthropic/v1"),
            "https://api.deepseek.com/models"
        );
        assert_eq!(
            models_url("http://127.0.0.1:9/v1"),
            "http://127.0.0.1:9/models"
        );
    }

    #[tokio::test]
    async fn streams_and_reports_usage() {
        let reply = events(&[
            json!({"type":"message_start","message":{"usage":{"prompt_cache_miss_tokens":100,"prompt_cache_hit_tokens":2000,"output_tokens":1}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"why","signature":"sig-9"}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":" hard"}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"-full"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":"hi"}}),
            json!({"type":"content_block_stop","index":1}),
            json!({"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"t9","name":"read","input":{}}}),
            json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"a\":1}"}}),
            json!({"type":"content_block_stop","index":2}),
            json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":9}}),
            json!({"type":"message_stop"}),
        ]);
        let (url, mut requests, task) = server(vec![(200, reply)]).await;
        let mut p = provider("deepseek-flash");
        p.base_url = format!("{url}/anthropic/v1");
        p.api_key = Some("sk-test".into());
        p.set_conversation("conv-9");
        let items = [Item::user("go")];
        let tools = [tool("read")];
        let r = p
            .stream(request("s", &items, &tools), &mut |_| {})
            .await
            .unwrap();
        assert_eq!(
            r.items,
            vec![
                Item::Reasoning {
                    id: Some("thinking".into()),
                    summary: "why hard".into(),
                    encrypted: Some("sig-9-full".into()),
                },
                Item::Assistant { text: "hi".into() },
                Item::ToolCall {
                    id: "t9".into(),
                    name: "read".into(),
                    arguments: "{\"a\":1}".into(),
                },
            ]
        );
        let usage = r.usage.unwrap();
        // Cache-hit tokens land in cached_tokens and the total input.
        assert_eq!(
            (
                usage.input_tokens,
                usage.cached_tokens,
                usage.output_tokens
            ),
            (2100, 2000, 9)
        );
        let request = requests.recv().await.unwrap();
        let lower = request.to_lowercase();
        assert!(lower.starts_with("post /anthropic/v1/messages "), "{lower}");
        for header in [
            "x-api-key: sk-test",
            "anthropic-version: 2023-06-01",
            "x-deepseek-harness-session-id: conv-9",
        ] {
            assert!(lower.contains(header), "{header}: {lower}");
        }
        assert!(!lower.contains("authorization:"), "{lower}");
        // No tool-change blocks, no beta opt-in.
        assert!(!lower.contains("anthropic-beta"), "{lower}");
        task.await.unwrap();
    }

    #[tokio::test]
    async fn tool_change_blocks_send_beta_header() {
        let reply = events(&[
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":"x"}}),
            json!({"type":"message_stop"}),
        ]);
        let (url, mut requests, task) = server(vec![(200, reply.clone()), (200, reply)]).await;
        let mut p = provider("deepseek-flash");
        p.base_url = format!("{url}/v1");
        let history = [Item::user("hi")];
        p.stream(request("s", &history, &[tool("a")]), &mut |_| {})
            .await
            .unwrap();
        p.stream(
            request("s", &history, &[tool("a"), tool("b")]),
            &mut |_| {},
        )
        .await
        .unwrap();
        let first = requests.recv().await.unwrap().to_lowercase();
        assert!(!first.contains("anthropic-beta"), "{first}");
        let second = requests.recv().await.unwrap().to_lowercase();
        assert!(
            second.contains("anthropic-beta: mid-conversation-tool-changes-2026-07-01"),
            "{second}"
        );
        task.await.unwrap();
    }

    #[tokio::test]
    async fn empty_terminal_response_is_retryable() {
        let empty = events(&[json!({"type":"message_stop"})]);
        let reply = events(&[
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":"ok"}}),
            json!({"type":"message_stop"}),
        ]);
        let (url, mut requests, task) = server(vec![(200, empty), (200, reply)]).await;
        let mut p = provider("deepseek-v4-pro");
        p.base_url = url;
        let items = [Item::user("hi")];
        let mut events = Vec::new();
        let r = crate::provider::stream_with_retry(
            &p,
            request("s", &items, &[]),
            &mut |e| events.push(e),
        )
        .await
        .unwrap();
        assert_eq!(r.items, vec![Item::Assistant { text: "ok".into() }]);
        assert!(requests.recv().await.is_some());
        assert!(requests.recv().await.is_some());
        assert!(
            events.iter().any(|e| matches!(e, crate::types::StreamEvent::Retrying(_))),
            "{events:?}"
        );
        task.await.unwrap();
    }

    #[tokio::test]
    async fn error_classification() {
        // Auth errors are final; context-window and mid-stream server
        // errors are reported plainly.
        let cases: Vec<(u16, String)> = vec![
            (401, r#"{"type":"error","error":{"type":"authentication_error","message":"bad key"}}"#.into()),
        ];
        let (url, _requests, task) = server(cases).await;
        let mut p = provider("deepseek-v4-pro");
        p.base_url = url;
        let items = [Item::user("hi")];
        let err = p.stream(request("s", &items, &[]), &mut |_| {}).await.unwrap_err();
        assert!(format!("{err:#}").contains("401") && format!("{err:#}").contains("denied"), "{err:#}");
        task.await.unwrap();

        let stream_error = events(&[json!({"type":"error","error":{"type":"invalid_request_error","message":"model_context_window_exceeded"}})]);
        let (url, _r, task) = server(vec![(200, stream_error)]).await;
        let mut p = provider("deepseek-v4-pro");
        p.base_url = url;
        let err = p.stream(request("s", &items, &[]), &mut |_| {}).await.unwrap_err();
        assert!(
            format!("{err:#}").contains("context_window") || format!("{err:#}").contains("invalid_request"),
            "{err:#}"
        );
        task.await.unwrap();
    }

    #[tokio::test]
    async fn retries_after_server_error() {
        let reply = events(&[
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":"ok"}}),
            json!({"type":"message_stop"}),
        ]);
        let (url, _requests, task) = server(vec![(503, "busy".into()), (200, reply)]).await;
        let mut p = provider("deepseek-v4-pro");
        p.base_url = url;
        let items = [Item::user("hi")];
        let r = crate::provider::stream_with_retry(&p, request("s", &items, &[]), &mut |_| {})
            .await
            .unwrap();
        assert_eq!(r.items, vec![Item::Assistant { text: "ok".into() }]);
        task.await.unwrap();
    }
}
