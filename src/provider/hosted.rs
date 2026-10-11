//! API-key model services: OpenCode Go, OpenCode Zen, xAI and the Vercel AI
//! Gateway. Each speaks OpenAI-compatible chat completions; OpenCode also
//! serves some models over the Responses or Anthropic Messages protocol (per
//! model, from models.dev), and xAI is used through the Responses API.
//!
//! API keys: the service's environment variable, else (OpenCode only) the
//! OpenCode CLI's `auth.json`, else nyq's own `<home>/auth.json` written by
//! `nyq login <service>`.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result};
use serde_json::{json, Value};

use super::{
    anthropic::AnthropicProvider,
    openai::{EffortStyle, OpenAiProvider},
    responses::ResponsesProvider,
    EventSink, Provider,
};
use crate::{
    catalog::{ModelMeta, Models, Protocol},
    models::ModelChoice,
    types::{Request, Response},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Service {
    OpencodeGo,
    Opencode,
    Xai,
    Vercel,
}

impl Service {
    pub const ALL: [Service; 4] = [Service::OpencodeGo, Service::Opencode, Service::Xai, Service::Vercel];

    /// Provider id, as used by `--provider`, the switcher tab and models.dev.
    pub fn id(self) -> &'static str {
        match self {
            Service::OpencodeGo => "opencode-go",
            Service::Opencode => "opencode",
            Service::Xai => "xai",
            Service::Vercel => "vercel",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "opencode-go" | "go" => Some(Service::OpencodeGo),
            "opencode" | "zen" | "opencode-zen" => Some(Service::Opencode),
            "xai" | "grok" => Some(Service::Xai),
            "vercel" | "gateway" | "ai-gateway" => Some(Service::Vercel),
            _ => None,
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Service::OpencodeGo => "OpenCode Go",
            Service::Opencode => "OpenCode Zen",
            Service::Xai => "xAI",
            Service::Vercel => "Vercel AI Gateway",
        }
    }

    /// The environment variable holding the API key.
    pub fn key_env(self) -> &'static str {
        match self {
            Service::OpencodeGo | Service::Opencode => "OPENCODE_API_KEY",
            Service::Xai => "XAI_API_KEY",
            Service::Vercel => "AI_GATEWAY_API_KEY",
        }
    }

    pub fn default_model(self) -> &'static str {
        match self {
            Service::OpencodeGo => "glm-5.3",
            Service::Opencode => "claude-sonnet-5-5",
            Service::Xai => "grok-4.7",
            Service::Vercel => "anthropic/claude-sonnet-5-5",
        }
    }

    /// API base URL; `NYQ_<SERVICE>_BASE_URL` overrides it.
    pub fn base_url(self) -> String {
        let (var, default) = match self {
            Service::OpencodeGo => ("NYQ_OPENCODE_GO_BASE_URL", "https://opencode.ai/zen/go/v1"),
            Service::Opencode => ("NYQ_OPENCODE_BASE_URL", "https://opencode.ai/zen/v1"),
            Service::Xai => ("NYQ_XAI_BASE_URL", "https://api.x.ai/v1"),
            Service::Vercel => ("NYQ_VERCEL_BASE_URL", "https://ai-gateway.vercel.sh/v1"),
        };
        std::env::var(var).ok().filter(|v| !v.trim().is_empty()).unwrap_or_else(|| default.into())
    }

    fn is_opencode(self) -> bool {
        matches!(self, Service::OpencodeGo | Service::Opencode)
    }
}

fn own_auth(home: &Path) -> PathBuf {
    home.join("auth.json")
}

/// Where the OpenCode CLI keeps its credentials.
fn opencode_auth_files() -> Vec<PathBuf> {
    let mut files = Vec::new();
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
        files.push(PathBuf::from(xdg).join("opencode").join("auth.json"));
    }
    if let Some(home) = dirs::home_dir() {
        files.push(home.join(".local/share/opencode/auth.json"));
        #[cfg(target_os = "macos")]
        files.push(home.join("Library/Application Support/opencode/auth.json"));
    }
    files
}

fn read_key(path: &Path, id: &str) -> Option<String> {
    let v: Value = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    v[id]["key"].as_str().map(str::trim).filter(|k| !k.is_empty()).map(str::to_owned)
}

/// The API key for `service`, or None when it is not configured.
pub fn api_key(home: &Path, service: Service) -> Option<String> {
    if let Some(key) = std::env::var(service.key_env()).ok().filter(|k| !k.trim().is_empty()) {
        return Some(key.trim().to_owned());
    }
    if service.is_opencode() {
        if let Some(key) = opencode_auth_files().iter().find_map(|f| read_key(f, service.id())) {
            return Some(key);
        }
    }
    read_key(&own_auth(home), service.id())
}

/// Where `service`'s key comes from (same precedence as [`api_key`]): its
/// environment variable, the OpenCode CLI's login, or nyq's `auth.json`.
pub fn key_source(home: &Path, service: Service) -> Option<String> {
    if std::env::var(service.key_env()).ok().is_some_and(|k| !k.trim().is_empty()) {
        return Some(service.key_env().into());
    }
    if service.is_opencode() && opencode_auth_files().iter().any(|f| read_key(f, service.id()).is_some()) {
        return Some("OpenCode CLI login".into());
    }
    read_key(&own_auth(home), service.id()).map(|_| "stored by nyq".into())
}

pub(crate) fn read_own(home: &Path) -> Result<serde_json::Map<String, Value>> {
    match std::fs::read(own_auth(home)) {
        Ok(bytes) => serde_json::from_slice(&bytes).context("parsing auth.json"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Default::default()),
        Err(e) => Err(e).context("reading auth.json"),
    }
}

/// Store `key` for `service` in `<home>/auth.json` (mode 0600).
pub fn save_key(home: &Path, service: Service, key: &str) -> Result<()> {
    let key = key.trim();
    anyhow::ensure!(!key.is_empty(), "the API key is empty");
    let mut all = read_own(home)?;
    all.insert(service.id().into(), json!({"type":"api","key":key}));
    crate::config::write_private(&own_auth(home), &serde_json::to_vec_pretty(&all)?)
}

/// What `service` said about an API key.
#[derive(Debug, PartialEq, Eq)]
pub enum KeyCheck {
    Accepted,
    /// HTTP 401/403: the key is wrong (the formatted error).
    Refused(String),
    /// The check could not be made (network trouble, an unexpected status).
    Unknown(String),
}

/// Probe `base_url` with `key`: the models list, or for the AI Gateway (whose
/// model list is public) the credit balance.
pub async fn check_key(service: Service, base_url: &str, key: &str) -> KeyCheck {
    let path = if service == Service::Vercel { "credits" } else { "models" };
    let request = reqwest::Client::new()
        .get(format!("{}/{path}", base_url.trim_end_matches('/')))
        .bearer_auth(key.trim())
        .header(reqwest::header::USER_AGENT, crate::config::USER_AGENT)
        .timeout(std::time::Duration::from_secs(10));
    let response = match request.send().await {
        Ok(r) => r,
        Err(e) => return KeyCheck::Unknown(format!("{e}")),
    };
    let status = response.status();
    if status.is_success() {
        return KeyCheck::Accepted;
    }
    let message = super::sse::http_error(status, &response.text().await.unwrap_or_default());
    match status.as_u16() {
        // A valid AI Gateway key without credits gets a 403 from the balance.
        403 if service == Service::Vercel && message.contains("credit") => KeyCheck::Accepted,
        401 | 403 => KeyCheck::Refused(message),
        _ => KeyCheck::Unknown(message),
    }
}

/// Remove nyq's stored key for `service`; false if there was none.
pub fn remove_key(home: &Path, service: Service) -> Result<bool> {
    let mut all = read_own(home)?;
    if all.remove(service.id()).is_none() {
        return Ok(false);
    }
    crate::config::write_private(&own_auth(home), &serde_json::to_vec_pretty(&all)?)?;
    Ok(true)
}

pub struct HostedProvider {
    pub service: Service,
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub effort: Option<String>,
    session: String,
    models: Arc<Models>,
}

impl HostedProvider {
    pub fn new(service: Service, base_url: String, api_key: String, model: String, effort: Option<String>, models: Arc<Models>) -> Self {
        Self {
            service,
            base_url,
            api_key,
            model,
            effort: effort.filter(|e| e != "auto"),
            session: uuid::Uuid::new_v4().to_string(),
            models,
        }
    }

    /// Build from the environment and profile: key, base URL, models.dev data.
    pub fn configured(home: &Path, service: Service, model: String, effort: Option<String>) -> Result<Self> {
        let key = api_key(home, service).with_context(|| {
            format!("{} needs an API key: set {} or run `nyq login {}`", service.title(), service.key_env(), service.id())
        })?;
        let models = crate::catalog::load(home, service.id());
        Ok(Self::new(service, service.base_url(), key, model, effort, Arc::new(models)))
    }

    fn meta(&self) -> Option<&ModelMeta> {
        self.models.get(&self.model)
    }

    pub fn protocol(&self) -> Protocol {
        match self.service {
            Service::Xai => Protocol::Responses,
            Service::Vercel => Protocol::Chat,
            // A missing catalog used to force chat completions for every model.
            Service::OpencodeGo | Service::Opencode => {
                self.meta().map_or_else(|| infer_protocol(&self.model), ModelMeta::protocol)
            }
        }
    }

    fn headers(&self) -> Vec<(String, String)> {
        let mut headers = vec![("User-Agent".to_string(), crate::config::USER_AGENT.to_string())];
        match self.service {
            Service::OpencodeGo | Service::Opencode => headers.push(("x-opencode-session".into(), self.session.clone())),
            // Routes a conversation to the same cache.
            Service::Xai => headers.push(("x-grok-conv-id".into(), self.session.clone())),
            Service::Vercel => {}
        }
        headers
    }

    fn chat(&self) -> OpenAiProvider {
        OpenAiProvider {
            effort: self.effort.clone(),
            effort_style: if self.service == Service::Vercel { EffortStyle::ReasoningObject } else { EffortStyle::ReasoningEffort },
            headers: self.headers(),
            reasoning_field: self.meta().and_then(|m| m.interleaved.clone()),
            ..OpenAiProvider::with_label(self.service.id().into(), self.base_url.clone(), Some(self.api_key.clone()), self.model.clone())
        }
    }

    /// Models from `GET {base}/models`, described with models.dev metadata.
    /// Image, video and embedding models, and models served over protocols
    /// nyq does not speak, are left out.
    pub async fn list_models(&self) -> Result<Vec<ModelChoice>> {
        let listed = tokio::time::timeout(std::time::Duration::from_secs(6), self.chat().list_models_detailed())
            .await
            .context("timed out listing models")??;
        Ok(listed
            .into_iter()
            .filter_map(|m| {
                let meta = self.models.get(&m.id);
                if meta.is_some_and(|meta| !meta.text_output || meta.npm.as_deref() == Some("@ai-sdk/google")) {
                    return None;
                }
                Some(ModelChoice {
                    provider: self.service.id().into(),
                    context_window: m.context_window.or(meta.and_then(|x| x.context)),
                    efforts: meta.map(|x| x.efforts.clone()).unwrap_or_default(),
                    price: meta.and_then(ModelMeta::price),
                    model: m.id,
                })
            })
            .collect())
    }
}

/// Backstop when models.dev has no row for this id.
fn infer_protocol(model: &str) -> Protocol {
    let id = model.to_ascii_lowercase();
    if id.starts_with("claude") {
        Protocol::Anthropic
    } else if id.starts_with("grok") || id.starts_with("gpt") {
        Protocol::Responses
    } else {
        Protocol::Chat
    }
}

#[async_trait::async_trait]
impl Provider for HostedProvider {
    fn name(&self) -> &str {
        self.service.id()
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
        self.meta().map(|m| m.efforts.clone()).unwrap_or_default()
    }
    fn context_window(&self) -> Option<u64> {
        self.meta().and_then(|m| m.context)
    }
    fn set_conversation(&mut self, id: &str) {
        self.session = id.to_string();
    }
    fn fork(&self) -> Box<dyn Provider> {
        let mut fork = Self::new(self.service, self.base_url.clone(), self.api_key.clone(), self.model.clone(), self.effort.clone(), Arc::clone(&self.models));
        fork.session = uuid::Uuid::new_v4().to_string();
        Box::new(fork)
    }
    async fn stream(&self, req: Request<'_>, on_event: EventSink<'_>) -> Result<Response> {
        let label = self.service.id().to_string();
        match self.protocol() {
            Protocol::Chat => self.chat().stream(req, on_event).await,
            Protocol::Responses => {
                let provider = ResponsesProvider {
                    label,
                    base_url: self.base_url.clone(),
                    api_key: Some(self.api_key.clone()),
                    model: self.model.clone(),
                    effort: self.effort.clone(),
                    headers: self.headers(),
                };
                provider.send(req, on_event).await
            }
            Protocol::Anthropic => {
                let mut provider = AnthropicProvider::new(label, self.base_url.clone(), Some(self.api_key.clone()), self.model.clone());
                provider.effort = self.effort.clone();
                provider.headers = self.headers();
                if let Some(output) = self.meta().and_then(|m| m.output) {
                    provider.max_tokens = output.clamp(1_024, 64_000);
                }
                provider.stream(req, on_event).await
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::await_holding_lock)]
mod tests {
    use super::*;
    use crate::{
        catalog::ModelMeta,
        provider::sse::tests::{events, server, ENV},
        types::Item,
    };

    fn models() -> Arc<Models> {
        let meta = |npm: Option<&str>| ModelMeta { npm: npm.map(str::to_owned), text_output: true, ..ModelMeta::default() };
        let mut models = Models::new();
        models.insert("glm".into(), ModelMeta { interleaved: Some("reasoning_content".into()), efforts: vec!["low".into(), "high".into()], context: Some(1000), cost: Some((1.0, 2.0)), ..meta(None) });
        models.insert("grok".into(), meta(Some("@ai-sdk/openai")));
        models.insert("claude".into(), ModelMeta { output: Some(128_000), ..meta(Some("@ai-sdk/anthropic")) });
        models.insert("gemini".into(), meta(Some("@ai-sdk/google")));
        models.insert("imagine".into(), ModelMeta { text_output: false, ..meta(None) });
        Arc::new(models)
    }

    fn provider(service: Service, url: &str, model: &str) -> HostedProvider {
        HostedProvider::new(service, url.into(), "sk-test".into(), model.into(), None, models())
    }

    fn body_of(request: &str) -> Value {
        serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap()
    }

    #[test]
    fn protocol_selection() {
        let p = |s, m| provider(s, "http://unused", m).protocol();
        assert_eq!(p(Service::OpencodeGo, "glm"), Protocol::Chat);
        assert_eq!(p(Service::OpencodeGo, "grok"), Protocol::Responses);
        assert_eq!(p(Service::Opencode, "claude"), Protocol::Anthropic);
        assert_eq!(p(Service::Opencode, "unknown"), Protocol::Chat);
        let bare = |model: &str| {
            HostedProvider::new(Service::Opencode, "http://unused".into(), "k".into(), model.into(), None, Arc::new(Models::new())).protocol()
        };
        assert_eq!(bare("claude-sonnet-4"), Protocol::Anthropic);
        assert_eq!(bare("Claude-Opus"), Protocol::Anthropic);
        assert_eq!(bare("gpt-5.5"), Protocol::Responses);
        assert_eq!(bare("grok-4"), Protocol::Responses);
        assert_eq!(bare("glm-5"), Protocol::Chat);
        assert_eq!(p(Service::Xai, "anything"), Protocol::Responses);
        assert_eq!(p(Service::Vercel, "claude"), Protocol::Chat);
        assert_eq!(Service::parse("Grok"), Some(Service::Xai));
        assert_eq!(Service::parse("gateway"), Some(Service::Vercel));
        assert!(Service::parse("codex").is_none());
        for s in Service::ALL {
            assert_eq!(Service::parse(s.id()), Some(s));
        }
    }

    #[tokio::test]
    async fn each_protocol_sends_session_and_user_agent() {
        let chat = events(&[json!({"choices":[{"index":0,"delta":{"content":"a"},"finish_reason":"stop"}]})]);
        let responses = events(&[
            json!({"type":"response.output_item.done","output_index":0,"item":{"type":"message","content":[{"type":"output_text","text":"b"}]}}),
            json!({"type":"response.completed","response":{}}),
        ]);
        let messages = events(&[
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":"c"}}),
            json!({"type":"message_stop"}),
        ]);
        let (url, mut requests, task) = server(vec![(200, chat), (200, responses), (200, messages)]).await;
        let base = format!("{url}/zen/go/v1");
        let mut p = provider(Service::OpencodeGo, &base, "glm");
        p.set_conversation("conv-1");
        p.set_effort(Some("high".into()));
        let items = [Item::user("hi"), Item::Reasoning { id: None, summary: "r".into(), encrypted: None }, Item::Assistant { text: "x".into() }, Item::user("again")];
        let mut texts = Vec::new();
        for model in ["glm", "grok", "claude"] {
            p.set_model(model.into());
            let r = p.stream(Request { system: "s", items: &items, tools: &[] }, &mut |_| {}).await.unwrap();
            texts.push(r.items);
        }
        assert_eq!(texts, ["a", "b", "c"].map(|t| vec![Item::Assistant { text: t.into() }]));
        let version = format!("user-agent: nyq/{}", env!("CARGO_PKG_VERSION"));
        for (path, auth) in [("/chat/completions", "authorization: bearer sk-test"), ("/responses", "authorization: bearer sk-test"), ("/messages", "x-api-key: sk-test")] {
            let request = requests.recv().await.unwrap();
            let lower = request.to_lowercase();
            assert!(lower.starts_with(&format!("post /zen/go/v1{path} ")), "{lower}");
            for header in [auth, "x-opencode-session: conv-1", &version] {
                assert!(lower.contains(header), "{path}: {header}");
            }
            let body = body_of(&request);
            match path {
                "/chat/completions" => {
                    assert_eq!(body["reasoning_effort"], "high");
                    assert_eq!(body["messages"][2]["reasoning_content"], "r");
                }
                "/responses" => assert_eq!(body["reasoning"]["effort"], "high"),
                _ => {
                    assert_eq!(body["max_tokens"], 64_000);
                    assert_eq!(body["thinking"]["type"], "adaptive");
                }
            }
        }
        task.await.unwrap();
        // Forks get their own conversation id.
        let fork = p.fork();
        assert_eq!((fork.name(), fork.model(), fork.effort()), ("opencode-go", "claude", Some("high")));
    }

    #[tokio::test]
    async fn xai_and_vercel() {
        let responses = events(&[json!({"type":"response.completed","response":{}})]);
        let chat = events(&[json!({"choices":[{"index":0,"delta":{"content":"v"},"finish_reason":"stop"}]})]);
        let (url, mut requests, task) = server(vec![(200, responses), (200, chat)]).await;
        let mut xai = provider(Service::Xai, &url, "grok-x");
        xai.set_conversation("c");
        xai.stream(Request { system: "", items: &[], tools: &[] }, &mut |_| {}).await.unwrap();
        let mut vercel = provider(Service::Vercel, &url, "anthropic/claude");
        vercel.set_effort(Some("low".into()));
        vercel.stream(Request { system: "", items: &[], tools: &[] }, &mut |_| {}).await.unwrap();
        let first = requests.recv().await.unwrap();
        assert!(first.starts_with("POST /responses "));
        assert!(first.to_lowercase().contains("x-grok-conv-id: c"));
        let second = requests.recv().await.unwrap();
        assert!(second.starts_with("POST /chat/completions ") && !second.to_lowercase().contains("x-opencode-session"));
        assert_eq!(body_of(&second)["reasoning"], json!({"effort":"low"}));
        task.await.unwrap();
    }

    #[tokio::test]
    async fn listing_uses_metadata() {
        let (url, mut requests, task) = server(vec![(
            200,
            json!({"data":[{"id":"glm"},{"id":"imagine"},{"id":"gemini"},{"id":"fresh"}]}).to_string(),
        )])
        .await;
        let models = provider(Service::OpencodeGo, &url, "glm").list_models().await.unwrap();
        assert_eq!(models.iter().map(|m| m.model.as_str()).collect::<Vec<_>>(), ["glm", "fresh"]);
        assert_eq!(models[0].efforts, ["low", "high"]);
        assert_eq!(models[0].context_window, Some(1000));
        assert_eq!(models[0].price.as_deref(), Some("$1/$2 per 1M"));
        assert_eq!(models[1].provider, "opencode-go");
        let request = requests.recv().await.unwrap().to_lowercase();
        assert!(request.starts_with("get /models ") && request.contains("authorization: bearer sk-test"));
        task.await.unwrap();
    }

    #[test]
    fn key_precedence() {
        let _lock = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let keys = ["HOME", "XDG_DATA_HOME", "OPENCODE_API_KEY", "XAI_API_KEY"];
        let saved: Vec<_> = keys.iter().map(|k| (*k, std::env::var_os(k))).collect();
        let user = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", user.path());
        for k in &keys[1..] {
            std::env::remove_var(k);
        }
        let home = home.path();
        assert_eq!(api_key(home, Service::OpencodeGo), None);
        // nyq's own login.
        save_key(home, Service::OpencodeGo, " own-go \n").unwrap();
        save_key(home, Service::Xai, "own-xai").unwrap();
        assert_eq!(api_key(home, Service::OpencodeGo).as_deref(), Some("own-go"));
        assert_eq!(api_key(home, Service::Opencode), None);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(home.join("auth.json")).unwrap().permissions().mode() & 0o777, 0o600);
        }
        // The OpenCode CLI's login wins over nyq's, per service id. (Found via
        // HOME, which Windows does not use to locate the home directory.)
        #[cfg(unix)]
        {
            let cli = user.path().join(".local/share/opencode");
            std::fs::create_dir_all(&cli).unwrap();
            std::fs::write(cli.join("auth.json"), r#"{"opencode-go":{"type":"api","key":"cli-go"},"opencode":{"type":"api","key":"cli-zen"}}"#).unwrap();
            assert_eq!(api_key(home, Service::OpencodeGo).as_deref(), Some("cli-go"));
            assert_eq!(api_key(home, Service::Opencode).as_deref(), Some("cli-zen"));
        }
        // XDG_DATA_HOME is checked first.
        let xdg = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(xdg.path().join("opencode")).unwrap();
        std::fs::write(xdg.path().join("opencode/auth.json"), r#"{"opencode-go":{"key":"xdg-go"}}"#).unwrap();
        std::env::set_var("XDG_DATA_HOME", xdg.path());
        assert_eq!(api_key(home, Service::OpencodeGo).as_deref(), Some("xdg-go"));
        // xAI never reads the OpenCode file; the environment wins everywhere.
        assert_eq!(api_key(home, Service::Xai).as_deref(), Some("own-xai"));
        std::env::set_var("OPENCODE_API_KEY", "env-key");
        std::env::set_var("XAI_API_KEY", "env-xai");
        assert_eq!(api_key(home, Service::OpencodeGo).as_deref(), Some("env-key"));
        assert_eq!(api_key(home, Service::Xai).as_deref(), Some("env-xai"));
        std::env::remove_var("XAI_API_KEY");
        assert!(remove_key(home, Service::Xai).unwrap());
        assert!(!remove_key(home, Service::Xai).unwrap());
        assert_eq!(api_key(home, Service::Xai), None);
        assert_eq!(read_key(&home.join("auth.json"), "opencode-go").as_deref(), Some("own-go"));
        assert!(save_key(home, Service::Vercel, "  ").is_err());
        for (k, v) in saved {
            match v {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
    }
}
