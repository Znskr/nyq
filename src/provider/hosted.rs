//! API-key model services: OpenCode Go, OpenCode Zen, xAI and the Anthropic
//! API. OpenCode speaks OpenAI-compatible chat completions and serves some
//! models over the Responses or Anthropic Messages protocol (per model, from
//! models.dev); xAI is used through the Responses API; Anthropic (or any
//! Anthropic-compatible endpoint, via `ANTHROPIC_BASE_URL`) through the
//! Messages API.
//!
//! API keys: the service's environment variable (for Anthropic also
//! `ANTHROPIC_AUTH_TOKEN`, sent as a bearer token), else (OpenCode only) the
//! OpenCode CLI's `auth.json`, else nyq's own `<home>/auth.json` written by
//! `nyq login <service>`.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result};
use serde_json::{json, Value};

use super::{
    anthropic::{self, AnthropicProvider},
    openai::OpenAiProvider,
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
    Anthropic,
}

/// The Anthropic API, unless `ANTHROPIC_BASE_URL` points elsewhere.
pub const ANTHROPIC_URL: &str = "https://api.anthropic.com/v1";

/// Holds a bearer token for Anthropic-compatible gateways.
pub const ANTHROPIC_AUTH_TOKEN: &str = "ANTHROPIC_AUTH_TOKEN";

impl Service {
    pub const ALL: [Service; 4] = [
        Service::OpencodeGo,
        Service::Opencode,
        Service::Xai,
        Service::Anthropic,
    ];

    /// Provider id, as used by `--provider`, the switcher tab and models.dev.
    pub fn id(self) -> &'static str {
        match self {
            Service::OpencodeGo => "opencode-go",
            Service::Opencode => "opencode",
            Service::Xai => "xai",
            Service::Anthropic => "anthropic",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "opencode-go" | "go" => Some(Service::OpencodeGo),
            "opencode" | "zen" | "opencode-zen" => Some(Service::Opencode),
            "xai" => Some(Service::Xai),
            "anthropic" | "claude" => Some(Service::Anthropic),
            _ => None,
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Service::OpencodeGo => "OpenCode Go",
            Service::Opencode => "OpenCode Zen",
            Service::Xai => "xAI",
            Service::Anthropic => "Anthropic API",
        }
    }

    /// The environment variable holding the API key.
    pub fn key_env(self) -> &'static str {
        match self {
            Service::OpencodeGo | Service::Opencode => "OPENCODE_API_KEY",
            Service::Xai => "XAI_API_KEY",
            Service::Anthropic => "ANTHROPIC_API_KEY",
        }
    }

    /// The model a service starts with (for Anthropic, `ANTHROPIC_MODEL`
    /// when set, as compatible endpoints document it).
    pub fn default_model(self) -> String {
        match self {
            Service::OpencodeGo => "glm-5.3".into(),
            Service::Opencode => "claude-sonnet-5-5".into(),
            Service::Xai => "grok-4.7".into(),
            Service::Anthropic => {
                env("ANTHROPIC_MODEL").unwrap_or_else(|| "claude-opus-5-5".into())
            }
        }
    }

    /// API base URL; `NYQ_<SERVICE>_BASE_URL` overrides it (for Anthropic
    /// also `ANTHROPIC_BASE_URL`, given with or without the `/v1`).
    pub fn base_url(self) -> String {
        let (var, default) = match self {
            Service::OpencodeGo => ("NYQ_OPENCODE_GO_BASE_URL", "https://opencode.ai/zen/go/v1"),
            Service::Opencode => ("NYQ_OPENCODE_BASE_URL", "https://opencode.ai/zen/v1"),
            Service::Xai => ("NYQ_XAI_BASE_URL", "https://api.x.ai/v1"),
            Service::Anthropic => {
                return match env("NYQ_ANTHROPIC_BASE_URL").or_else(|| env("ANTHROPIC_BASE_URL")) {
                    Some(url) => anthropic_base(&url),
                    None => ANTHROPIC_URL.into(),
                };
            }
        };
        env(var).unwrap_or_else(|| default.into())
    }

    /// The models.dev provider whose catalog describes (and prices) this
    /// service's models. An Anthropic-compatible endpoint of a known vendor
    /// uses that vendor's catalog.
    pub fn catalog_id(self) -> &'static str {
        if self != Service::Anthropic {
            return self.id();
        }
        let base = self.base_url();
        let host = reqwest::Url::parse(&base)
            .ok()
            .and_then(|u| u.host_str().map(str::to_ascii_lowercase))
            .unwrap_or_default();
        let is = |domain: &str| host == domain || host.ends_with(&format!(".{domain}"));
        if is("deepseek.com") {
            "deepseek"
        } else if is("moonshot.ai") || is("moonshot.cn") {
            "moonshotai"
        } else if is("minimax.io") || is("minimaxi.com") {
            "minimax"
        } else if is("z.ai") {
            "zai"
        } else if is("bigmodel.cn") {
            "zhipuai"
        } else {
            "anthropic"
        }
    }

    fn is_opencode(self) -> bool {
        matches!(self, Service::OpencodeGo | Service::Opencode)
    }
}

fn env(var: &str) -> Option<String> {
    std::env::var(var)
        .ok()
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
}

/// A Messages API base ending in `/v1`: SDK-style bases
/// (`https://api.deepseek.com/anthropic`) get it appended; a pasted
/// `…/v1/messages` loses the endpoint.
pub fn anthropic_base(url: &str) -> String {
    let url = url.trim().trim_end_matches('/');
    let url = url
        .strip_suffix("/messages")
        .unwrap_or(url)
        .trim_end_matches('/');
    if url.ends_with("/v1") {
        url.to_owned()
    } else {
        format!("{url}/v1")
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
    v[id]["key"]
        .as_str()
        .map(str::trim)
        .filter(|k| !k.is_empty())
        .map(str::to_owned)
}

/// The API key for `service`, or None when it is not configured.
pub fn api_key(home: &Path, service: Service) -> Option<String> {
    if let Some(key) = env(service.key_env()) {
        return Some(key);
    }
    if service == Service::Anthropic {
        if let Some(token) = env(ANTHROPIC_AUTH_TOKEN) {
            return Some(token);
        }
    }
    if service.is_opencode() {
        if let Some(key) = opencode_auth_files()
            .iter()
            .find_map(|f| read_key(f, service.id()))
        {
            return Some(key);
        }
    }
    read_key(&own_auth(home), service.id())
}

/// Where `service`'s key comes from (same precedence as [`api_key`]): its
/// environment variable, the OpenCode CLI's login, or nyq's `auth.json`.
pub fn key_source(home: &Path, service: Service) -> Option<String> {
    if env(service.key_env()).is_some() {
        return Some(service.key_env().into());
    }
    if bearer(service) {
        return Some(ANTHROPIC_AUTH_TOKEN.into());
    }
    if service.is_opencode()
        && opencode_auth_files()
            .iter()
            .any(|f| read_key(f, service.id()).is_some())
    {
        return Some("OpenCode CLI login".into());
    }
    read_key(&own_auth(home), service.id()).map(|_| "stored by nyq".into())
}

/// Whether [`api_key`] is a bearer token (`ANTHROPIC_AUTH_TOKEN`, used when
/// `ANTHROPIC_API_KEY` is unset) rather than an `x-api-key` key.
pub fn bearer(service: Service) -> bool {
    service == Service::Anthropic
        && env(service.key_env()).is_none()
        && env(ANTHROPIC_AUTH_TOKEN).is_some()
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

/// Probe `base_url` with `key` through the models list (Anthropic: with
/// `x-api-key` and `anthropic-version`).
pub async fn check_key(service: Service, base_url: &str, key: &str) -> KeyCheck {
    let request = crate::http::client()
        .get(format!("{}/models", base_url.trim_end_matches('/')))
        .header(reqwest::header::USER_AGENT, crate::config::USER_AGENT)
        .timeout(std::time::Duration::from_secs(10));
    let request = match service {
        Service::Anthropic => request
            .header("x-api-key", key.trim())
            .header("anthropic-version", anthropic::VERSION),
        _ => request.bearer_auth(key.trim()),
    };
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
    /// Send the key as a bearer token (Anthropic: `ANTHROPIC_AUTH_TOKEN`).
    pub bearer: bool,
    pub model: String,
    pub effort: Option<String>,
    session: String,
    models: Arc<Models>,
    /// models.dev provider id for metadata and prices.
    catalog: String,
}

impl HostedProvider {
    pub fn new(
        service: Service,
        base_url: String,
        api_key: String,
        model: String,
        effort: Option<String>,
        models: Arc<Models>,
    ) -> Self {
        Self {
            service,
            base_url,
            api_key,
            bearer: false,
            model,
            effort: effort.filter(|e| e != "auto"),
            session: uuid::Uuid::new_v4().to_string(),
            models,
            catalog: service.id().into(),
        }
    }

    /// Build from the environment and profile: key, base URL, models.dev data.
    pub fn configured(
        home: &Path,
        service: Service,
        model: String,
        effort: Option<String>,
    ) -> Result<Self> {
        let key = api_key(home, service).with_context(|| match service {
            Service::Anthropic => format!(
                "the Anthropic API needs an API key: set ANTHROPIC_API_KEY (or {ANTHROPIC_AUTH_TOKEN} for a bearer token) or run `nyq login anthropic`"
            ),
            _ => format!("{} needs an API key: set {} or run `nyq login {}`", service.title(), service.key_env(), service.id()),
        })?;
        let catalog = service.catalog_id();
        let models = crate::catalog::load(home, catalog);
        let mut provider = Self::new(
            service,
            service.base_url(),
            key,
            model,
            effort,
            Arc::new(models),
        );
        provider.bearer = bearer(service);
        provider.catalog = catalog.into();
        Ok(provider)
    }

    fn meta(&self) -> Option<&ModelMeta> {
        self.models.get(&self.model)
    }

    /// What the Models API said about the current model (Anthropic only).
    fn info(&self) -> Option<anthropic::ModelInfo> {
        (self.service == Service::Anthropic)
            .then(|| anthropic::known(&self.base_url, &self.model).flatten())
            .flatten()
    }

    pub fn protocol(&self) -> Protocol {
        match self.service {
            Service::Xai => Protocol::Responses,
            Service::Anthropic => Protocol::Anthropic,
            // A missing catalog used to force chat completions for every model.
            Service::OpencodeGo | Service::Opencode => self
                .meta()
                .map_or_else(|| infer_protocol(&self.model), ModelMeta::protocol),
        }
    }

    fn headers(&self) -> Vec<(String, String)> {
        let mut headers = vec![(
            "User-Agent".to_string(),
            crate::config::USER_AGENT.to_string(),
        )];
        match self.service {
            Service::OpencodeGo | Service::Opencode => {
                headers.push(("x-opencode-session".into(), self.session.clone()))
            }
            // Routes a conversation to the same cache.
            Service::Xai => headers.push(("x-grok-conv-id".into(), self.session.clone())),
            Service::Anthropic => {}
        }
        headers
    }

    fn chat(&self) -> OpenAiProvider {
        OpenAiProvider {
            effort: self.effort.clone(),
            headers: self.headers(),
            reasoning_field: self.meta().and_then(|m| m.interleaved.clone()),
            ..OpenAiProvider::with_label(
                self.service.id().into(),
                self.base_url.clone(),
                Some(self.api_key.clone()),
                self.model.clone(),
            )
        }
    }

    /// Models from `GET {base}/models`, described with models.dev metadata.
    /// Image, video and embedding models, and models served over protocols
    /// nyq does not speak, are left out.
    pub async fn list_models(&self) -> Result<Vec<ModelChoice>> {
        if self.service == Service::Anthropic {
            return self.list_anthropic().await;
        }
        let listed = tokio::time::timeout(
            std::time::Duration::from_secs(6),
            self.chat().list_models_detailed(),
        )
        .await
        .context("timed out listing models")??;
        super::remember_listing(
            self.service.id(),
            listed.iter().map(|m| m.id.clone()).collect(),
            Vec::new(),
        );
        Ok(listed
            .into_iter()
            .filter_map(|m| {
                let meta = self.models.get(&m.id);
                if meta.is_some_and(|meta| {
                    !meta.text_output || meta.npm.as_deref() == Some("@ai-sdk/google")
                }) {
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

impl HostedProvider {
    /// The Models API's list; when the endpoint has none (many compatible
    /// ones), the models.dev catalog for it plus the current model.
    async fn list_anthropic(&self) -> Result<Vec<ModelChoice>> {
        let choice = |id: String, context: Option<u64>, efforts: Vec<String>| {
            let meta = self.models.get(&id);
            ModelChoice {
                provider: self.service.id().into(),
                context_window: context.or(meta.and_then(|m| m.context)),
                efforts: if efforts.is_empty() {
                    self.efforts_for(&id, None)
                } else {
                    efforts
                },
                price: meta.and_then(ModelMeta::price),
                model: id,
            }
        };
        let listed = tokio::time::timeout(
            std::time::Duration::from_secs(8),
            anthropic::list_models(&self.base_url, Some(&self.api_key), self.bearer),
        )
        .await
        .context("timed out listing models");
        let error = match listed {
            Ok(Ok(models)) if !models.is_empty() => {
                super::remember_listing(
                    self.service.id(),
                    models.iter().map(|m| m.id.clone()).collect(),
                    Vec::new(),
                );
                return Ok(models
                    .into_iter()
                    .map(|m| choice(m.id, m.context, m.efforts))
                    .collect());
            }
            Ok(Ok(_)) => anyhow::anyhow!("the endpoint lists no models"),
            Ok(Err(e)) => e,
            Err(e) => e,
        };
        // A refused key is not something the catalog can paper over.
        if error.chain().any(|c| {
            c.downcast_ref::<super::sse::HttpError>()
                .is_some_and(|e| matches!(e.status, 401 | 403))
        }) {
            return Err(error);
        }
        let mut ids: Vec<String> = self
            .models
            .iter()
            .filter(|(_, m)| m.text_output)
            .map(|(id, _)| id.clone())
            .collect();
        ids.sort();
        if !ids.contains(&self.model) {
            ids.insert(0, self.model.clone());
        }
        Ok(ids
            .into_iter()
            .map(|id| choice(id, None, Vec::new()))
            .collect())
    }

    /// Efforts for `id`: the Models API's, else the catalog's, else the
    /// defaults for how the model thinks.
    fn efforts_for(&self, id: &str, info: Option<&anthropic::ModelInfo>) -> Vec<String> {
        if let Some(info) =
            info.filter(|i| i.thinking != anthropic::Thinking::Unknown || !i.efforts.is_empty())
        {
            return info.efforts.clone();
        }
        match self
            .models
            .get(id)
            .map(|m| m.efforts.clone())
            .filter(|e| !e.is_empty())
        {
            Some(efforts) => efforts,
            None => anthropic::default_efforts(id),
        }
    }

    async fn stream_anthropic(
        &self,
        req: Request<'_>,
        on_event: EventSink<'_>,
    ) -> Result<Response> {
        let info = anthropic::describe(
            &self.base_url,
            Some(&self.api_key),
            self.bearer,
            &self.model,
        )
        .await;
        let mut provider = AnthropicProvider::new(
            self.service.id().into(),
            self.base_url.clone(),
            Some(self.api_key.clone()),
            self.model.clone(),
        );
        provider.bearer = self.bearer;
        provider.effort = self.effort.clone();
        provider.headers = self.headers();
        provider.think_by_default = true;
        provider.thinking = info
            .as_ref()
            .map_or(anthropic::Thinking::Unknown, |i| i.thinking);
        if let Some(output) = info
            .as_ref()
            .and_then(|i| i.max_output)
            .or_else(|| self.meta().and_then(|m| m.output))
        {
            provider.max_tokens = output.clamp(1_024, 64_000);
        }
        provider.stream(req, on_event).await
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
    fn endpoint(&self) -> Option<String> {
        Some(self.base_url.clone())
    }
    fn pricing_provider(&self) -> &str {
        &self.catalog
    }
    fn efforts(&self) -> Vec<String> {
        if self.service == Service::Anthropic {
            return self.efforts_for(&self.model, self.info().as_ref());
        }
        self.meta().map(|m| m.efforts.clone()).unwrap_or_default()
    }
    fn context_window(&self) -> Option<u64> {
        self.info()
            .and_then(|i| i.context)
            .or_else(|| self.meta().and_then(|m| m.context))
    }
    fn set_conversation(&mut self, id: &str) {
        self.session = id.to_string();
    }
    fn fork(&self) -> Box<dyn Provider> {
        let mut fork = Self::new(
            self.service,
            self.base_url.clone(),
            self.api_key.clone(),
            self.model.clone(),
            self.effort.clone(),
            Arc::clone(&self.models),
        );
        fork.session = uuid::Uuid::new_v4().to_string();
        fork.bearer = self.bearer;
        fork.catalog = self.catalog.clone();
        Box::new(fork)
    }
    async fn stream(&self, req: Request<'_>, on_event: EventSink<'_>) -> Result<Response> {
        if self.service == Service::Anthropic {
            return self.stream_anthropic(req, on_event).await;
        }
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
                let mut provider = AnthropicProvider::new(
                    label,
                    self.base_url.clone(),
                    Some(self.api_key.clone()),
                    self.model.clone(),
                );
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
        let meta = |npm: Option<&str>| ModelMeta {
            npm: npm.map(str::to_owned),
            text_output: true,
            ..ModelMeta::default()
        };
        let mut models = Models::new();
        models.insert(
            "glm".into(),
            ModelMeta {
                interleaved: Some("reasoning_content".into()),
                efforts: vec!["low".into(), "high".into()],
                context: Some(1000),
                cost: Some((1.0, 2.0)),
                ..meta(None)
            },
        );
        models.insert("grok".into(), meta(Some("@ai-sdk/openai")));
        models.insert(
            "claude".into(),
            ModelMeta {
                output: Some(128_000),
                ..meta(Some("@ai-sdk/anthropic"))
            },
        );
        models.insert("gemini".into(), meta(Some("@ai-sdk/google")));
        models.insert(
            "imagine".into(),
            ModelMeta {
                text_output: false,
                ..meta(None)
            },
        );
        Arc::new(models)
    }

    fn provider(service: Service, url: &str, model: &str) -> HostedProvider {
        HostedProvider::new(
            service,
            url.into(),
            "sk-test".into(),
            model.into(),
            None,
            models(),
        )
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
            HostedProvider::new(
                Service::Opencode,
                "http://unused".into(),
                "k".into(),
                model.into(),
                None,
                Arc::new(Models::new()),
            )
            .protocol()
        };
        assert_eq!(bare("claude-sonnet-4"), Protocol::Anthropic);
        assert_eq!(bare("Claude-Opus"), Protocol::Anthropic);
        assert_eq!(bare("gpt-5.5"), Protocol::Responses);
        assert_eq!(bare("grok-4"), Protocol::Responses);
        assert_eq!(bare("glm-5"), Protocol::Chat);
        assert_eq!(p(Service::Xai, "anything"), Protocol::Responses);
        assert!(Service::parse("vercel").is_none());
        assert!(Service::parse("grok").is_none());
        assert!(Service::parse("Grok").is_none());
        assert!(Service::parse("codex").is_none());
        for s in Service::ALL {
            assert_eq!(Service::parse(s.id()), Some(s));
        }
    }

    #[tokio::test]
    async fn each_protocol_sends_session_and_user_agent() {
        let chat = events(&[
            json!({"choices":[{"index":0,"delta":{"content":"a"},"finish_reason":"stop"}]}),
        ]);
        let responses = events(&[
            json!({"type":"response.output_item.done","output_index":0,"item":{"type":"message","content":[{"type":"output_text","text":"b"}]}}),
            json!({"type":"response.completed","response":{}}),
        ]);
        let messages = events(&[
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":"c"}}),
            json!({"type":"message_stop"}),
        ]);
        let (url, mut requests, task) =
            server(vec![(200, chat), (200, responses), (200, messages)]).await;
        let base = format!("{url}/zen/go/v1");
        let mut p = provider(Service::OpencodeGo, &base, "glm");
        p.set_conversation("conv-1");
        p.set_effort(Some("high".into()));
        let items = [
            Item::user("hi"),
            Item::Reasoning {
                id: None,
                summary: "r".into(),
                encrypted: None,
            },
            Item::Assistant { text: "x".into() },
            Item::user("again"),
        ];
        let mut texts = Vec::new();
        for model in ["glm", "grok", "claude"] {
            p.set_model(model.into());
            let r = p
                .stream(
                    Request {
                        system: "s",
                        items: &items,
                        tools: &[],
                    },
                    &mut |_| {},
                )
                .await
                .unwrap();
            texts.push(r.items);
        }
        assert_eq!(
            texts,
            ["a", "b", "c"].map(|t| vec![Item::Assistant { text: t.into() }])
        );
        let version = format!("user-agent: nyq/{}", env!("CARGO_PKG_VERSION"));
        for (path, auth) in [
            ("/chat/completions", "authorization: bearer sk-test"),
            ("/responses", "authorization: bearer sk-test"),
            ("/messages", "x-api-key: sk-test"),
        ] {
            let request = requests.recv().await.unwrap();
            let lower = request.to_lowercase();
            assert!(
                lower.starts_with(&format!("post /zen/go/v1{path} ")),
                "{lower}"
            );
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
        assert_eq!(
            (fork.name(), fork.model(), fork.effort()),
            ("opencode-go", "claude", Some("high"))
        );
    }

    #[tokio::test]
    async fn xai_uses_responses_with_conversation_id() {
        let responses = events(&[json!({"type":"response.completed","response":{}})]);
        let (url, mut requests, task) = server(vec![(200, responses)]).await;
        let mut xai = provider(Service::Xai, &url, "grok-x");
        xai.set_conversation("c");
        xai.stream(
            Request {
                system: "",
                items: &[],
                tools: &[],
            },
            &mut |_| {},
        )
        .await
        .unwrap();
        let first = requests.recv().await.unwrap();
        assert!(first.starts_with("POST /responses "));
        assert!(first.to_lowercase().contains("x-grok-conv-id: c"));
        task.await.unwrap();
    }

    #[tokio::test]
    async fn listing_uses_metadata() {
        let (url, mut requests, task) = server(vec![(
            200,
            json!({"data":[{"id":"glm"},{"id":"imagine"},{"id":"gemini"},{"id":"fresh"}]})
                .to_string(),
        )])
        .await;
        let models = provider(Service::OpencodeGo, &url, "glm")
            .list_models()
            .await
            .unwrap();
        assert_eq!(
            models.iter().map(|m| m.model.as_str()).collect::<Vec<_>>(),
            ["glm", "fresh"]
        );
        assert_eq!(models[0].efforts, ["low", "high"]);
        assert_eq!(models[0].context_window, Some(1000));
        assert_eq!(models[0].price.as_deref(), Some("$1/$2 per 1M"));
        assert_eq!(models[1].provider, "opencode-go");
        let request = requests.recv().await.unwrap().to_lowercase();
        assert!(
            request.starts_with("get /models ")
                && request.contains("authorization: bearer sk-test")
        );
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
        assert_eq!(
            api_key(home, Service::OpencodeGo).as_deref(),
            Some("own-go")
        );
        assert_eq!(api_key(home, Service::Opencode), None);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(home.join("auth.json"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        // The OpenCode CLI's login wins over nyq's, per service id. (Found via
        // HOME, which Windows does not use to locate the home directory.)
        #[cfg(unix)]
        {
            let cli = user.path().join(".local/share/opencode");
            std::fs::create_dir_all(&cli).unwrap();
            std::fs::write(cli.join("auth.json"), r#"{"opencode-go":{"type":"api","key":"cli-go"},"opencode":{"type":"api","key":"cli-zen"}}"#).unwrap();
            assert_eq!(
                api_key(home, Service::OpencodeGo).as_deref(),
                Some("cli-go")
            );
            assert_eq!(api_key(home, Service::Opencode).as_deref(), Some("cli-zen"));
        }
        // XDG_DATA_HOME is checked first.
        let xdg = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(xdg.path().join("opencode")).unwrap();
        std::fs::write(
            xdg.path().join("opencode/auth.json"),
            r#"{"opencode-go":{"key":"xdg-go"}}"#,
        )
        .unwrap();
        std::env::set_var("XDG_DATA_HOME", xdg.path());
        assert_eq!(
            api_key(home, Service::OpencodeGo).as_deref(),
            Some("xdg-go")
        );
        // xAI never reads the OpenCode file; the environment wins everywhere.
        assert_eq!(api_key(home, Service::Xai).as_deref(), Some("own-xai"));
        std::env::set_var("OPENCODE_API_KEY", "env-key");
        std::env::set_var("XAI_API_KEY", "env-xai");
        assert_eq!(
            api_key(home, Service::OpencodeGo).as_deref(),
            Some("env-key")
        );
        assert_eq!(api_key(home, Service::Xai).as_deref(), Some("env-xai"));
        std::env::remove_var("XAI_API_KEY");
        assert!(remove_key(home, Service::Xai).unwrap());
        assert!(!remove_key(home, Service::Xai).unwrap());
        assert_eq!(api_key(home, Service::Xai), None);
        assert_eq!(
            read_key(&home.join("auth.json"), "opencode-go").as_deref(),
            Some("own-go")
        );
        assert!(save_key(home, Service::Xai, "  ").is_err());
        for (k, v) in saved {
            match v {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
    }

    const ANTHROPIC_VARS: [&str; 7] = [
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_AUTH_TOKEN",
        "ANTHROPIC_BASE_URL",
        "NYQ_ANTHROPIC_BASE_URL",
        "ANTHROPIC_MODEL",
        "HOME",
        "XDG_DATA_HOME",
    ];

    /// Clears the Anthropic variables for a test; restores them on drop.
    struct AnthropicEnv(
        Vec<(&'static str, Option<std::ffi::OsString>)>,
        #[allow(dead_code)] std::sync::MutexGuard<'static, ()>,
    );
    impl AnthropicEnv {
        fn new() -> Self {
            let lock = ENV.lock().unwrap_or_else(|e| e.into_inner());
            let saved = ANTHROPIC_VARS
                .iter()
                .map(|k| (*k, std::env::var_os(k)))
                .collect();
            for k in ANTHROPIC_VARS {
                std::env::remove_var(k);
            }
            Self(saved, lock)
        }
    }
    impl Drop for AnthropicEnv {
        fn drop(&mut self) {
            for (k, v) in &self.0 {
                match v {
                    Some(v) => std::env::set_var(k, v),
                    None => std::env::remove_var(k),
                }
            }
        }
    }

    #[test]
    fn anthropic_endpoint_and_credentials() {
        let _env = AnthropicEnv::new();
        let user = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", user.path());
        assert_eq!(
            anthropic_base("https://api.deepseek.com/anthropic"),
            "https://api.deepseek.com/anthropic/v1"
        );
        assert_eq!(anthropic_base("https://api.anthropic.com/"), ANTHROPIC_URL);
        assert_eq!(
            anthropic_base("http://localhost:4000/v1/messages"),
            "http://localhost:4000/v1"
        );
        assert_eq!(Service::parse("Claude"), Some(Service::Anthropic));
        let a = Service::Anthropic;
        assert_eq!(
            (
                a.base_url().as_str(),
                a.catalog_id(),
                a.default_model().as_str()
            ),
            (ANTHROPIC_URL, "anthropic", "claude-opus-5-5")
        );
        std::env::set_var("ANTHROPIC_BASE_URL", "https://api.deepseek.com/anthropic");
        std::env::set_var("ANTHROPIC_MODEL", "deepseek-v4");
        assert_eq!(
            (
                a.base_url().as_str(),
                a.catalog_id(),
                a.default_model().as_str()
            ),
            (
                "https://api.deepseek.com/anthropic/v1",
                "deepseek",
                "deepseek-v4"
            )
        );
        std::env::set_var("ANTHROPIC_BASE_URL", "https://api.moonshot.ai/anthropic");
        assert_eq!(a.catalog_id(), "moonshotai");
        std::env::set_var("NYQ_ANTHROPIC_BASE_URL", "http://127.0.0.1:4000");
        assert_eq!(
            (a.base_url().as_str(), a.catalog_id()),
            ("http://127.0.0.1:4000/v1", "anthropic")
        );

        // nyq's stored key < ANTHROPIC_AUTH_TOKEN (bearer) < ANTHROPIC_API_KEY.
        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        assert_eq!((api_key(home, a), key_source(home, a)), (None, None));
        save_key(home, a, "sk-ant-own").unwrap();
        assert_eq!(api_key(home, a).as_deref(), Some("sk-ant-own"));
        assert_eq!(key_source(home, a).as_deref(), Some("stored by nyq"));
        assert!(!bearer(a));
        std::env::set_var("ANTHROPIC_AUTH_TOKEN", "gateway-token");
        assert_eq!(
            (api_key(home, a).as_deref(), bearer(a)),
            (Some("gateway-token"), true)
        );
        assert_eq!(key_source(home, a).as_deref(), Some("ANTHROPIC_AUTH_TOKEN"));
        let configured = HostedProvider::configured(home, a, "m".into(), None).unwrap();
        assert!(
            configured.bearer
                && configured.endpoint().as_deref() == Some("http://127.0.0.1:4000/v1")
        );
        std::env::set_var("ANTHROPIC_API_KEY", "sk-ant-env");
        assert_eq!(
            (api_key(home, a).as_deref(), bearer(a)),
            (Some("sk-ant-env"), false)
        );
        assert_eq!(key_source(home, a).as_deref(), Some("ANTHROPIC_API_KEY"));
        assert!(!bearer(Service::Xai));
    }

    fn anthropic(url: &str, model: &str, models: Models) -> HostedProvider {
        HostedProvider::new(
            Service::Anthropic,
            format!("{url}/v1"),
            "sk-ant".into(),
            model.into(),
            None,
            Arc::new(models),
        )
    }

    #[tokio::test]
    async fn anthropic_uses_listed_capabilities() {
        let info = json!({"id":"claude-opus-x","max_input_tokens":1000000,"max_tokens":128000,"capabilities":{
            "image_input":{"supported":true},
            "thinking":{"supported":true,"types":{"enabled":{"supported":false},"adaptive":{"supported":true}}},
            "effort":{"supported":true,"low":{"supported":true},"medium":{"supported":true},"high":{"supported":true},"xhigh":{"supported":true},"max":{"supported":true}}}});
        let reply = || {
            events(&[
                json!({"type":"message_start","message":{"usage":{"input_tokens":3,"cache_read_input_tokens":2000,"cache_creation_input_tokens":100,"output_tokens":1}}}),
                json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":"hi"}}),
                json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":5}}),
                json!({"type":"message_stop"}),
            ])
        };
        let (url, mut requests, task) = server(vec![
            (200, info.to_string()),
            (200, reply()),
            (200, reply()),
        ])
        .await;
        let mut p = anthropic(&url, "claude-opus-x", Models::new());
        p.set_effort(Some("xhigh".into()));
        let tools = [crate::types::ToolSpec {
            name: "read_file".into(),
            description: "d".into(),
            parameters: json!({"type":"object"}),
        }];
        for _ in 0..2 {
            let r = p
                .stream(
                    Request {
                        system: "sys",
                        items: &[Item::user("hi")],
                        tools: &tools,
                    },
                    &mut |_| {},
                )
                .await
                .unwrap();
            assert_eq!(r.items, vec![Item::Assistant { text: "hi".into() }]);
            let usage = r.usage.unwrap();
            assert_eq!(
                (
                    usage.input_tokens,
                    usage.cached_tokens,
                    usage.cache_write_tokens,
                    usage.output_tokens
                ),
                (2103, 2000, 100, 5)
            );
        }
        let lookup = requests.recv().await.unwrap().to_lowercase();
        assert!(
            lookup.starts_with("get /v1/models/claude-opus-x "),
            "{lookup}"
        );
        for header in ["x-api-key: sk-ant", "anthropic-version: 2023-06-01"] {
            assert!(lookup.contains(header), "{header}");
        }
        // The lookup is made once; both turns go to the Messages API.
        for _ in 0..2 {
            let request = requests.recv().await.unwrap();
            let lower = request.to_lowercase();
            assert!(
                lower.starts_with("post /v1/messages ")
                    && lower.contains("x-api-key: sk-ant")
                    && !lower.contains("authorization"),
                "{lower}"
            );
            assert!(
                lower.contains("anthropic-version: 2023-06-01")
                    && lower.contains("user-agent: nyq/")
            );
            let body = body_of(&request);
            assert_eq!(
                body["thinking"],
                json!({"type":"adaptive","display":"summarized"})
            );
            assert_eq!(body["output_config"]["effort"], "xhigh");
            assert_eq!(body["max_tokens"], 64_000);
            assert_eq!(body["stream"], true);
            assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
            assert_eq!(
                body["messages"][0]["content"][0]["cache_control"]["type"],
                "ephemeral"
            );
            assert_eq!(body["tools"][0]["name"], "read_file");
        }
        task.await.unwrap();
        assert_eq!(p.efforts(), ["low", "medium", "high", "xhigh", "max"]);
        assert_eq!(p.context_window(), Some(1_000_000));
        assert_eq!(
            (p.name(), p.pricing_provider(), p.protocol()),
            ("anthropic", "anthropic", Protocol::Anthropic)
        );
    }

    #[tokio::test]
    async fn compatible_endpoint_without_model_details() {
        let reply = events(&[json!({"type":"message_stop"})]);
        let (url, mut requests, task) = server(vec![
            (
                404,
                r#"{"error":{"type":"not_found_error","message":"no"}}"#.into(),
            ),
            (200, reply.clone()),
            (200, reply),
        ])
        .await;
        let mut p = anthropic(&url, "deepseek-v4", Models::new());
        p.bearer = true;
        p.set_effort(Some("high".into()));
        p.stream(
            Request {
                system: "",
                items: &[Item::user("hi")],
                tools: &[],
            },
            &mut |_| {},
        )
        .await
        .unwrap();
        p.set_effort(None);
        p.stream(
            Request {
                system: "",
                items: &[Item::user("hi")],
                tools: &[],
            },
            &mut |_| {},
        )
        .await
        .unwrap();
        let _lookup = requests.recv().await.unwrap();
        let first = requests.recv().await.unwrap();
        assert!(
            first
                .to_lowercase()
                .contains("authorization: bearer sk-ant")
                && !first.to_lowercase().contains("x-api-key")
        );
        // Not Claude: a thinking budget under the default output cap.
        assert_eq!(
            body_of(&first)["thinking"],
            json!({"type":"enabled","budget_tokens":16384})
        );
        // Without an effort, a budget model is not asked to think.
        assert!(body_of(&requests.recv().await.unwrap())
            .get("thinking")
            .is_none());
        task.await.unwrap();
        assert_eq!(p.efforts(), ["low", "medium", "high"]);
    }

    #[tokio::test]
    async fn anthropic_listing_pages_and_falls_back() {
        let page1 = json!({"data":[{"id":"claude-opus-x","max_input_tokens":1000000,"capabilities":{"thinking":{"supported":true,"types":{"adaptive":{"supported":true}}},"effort":{"supported":true,"low":{"supported":true},"high":{"supported":true}}}}],"has_more":true,"last_id":"claude-opus-x"});
        let page2 = json!({"data":[{"id":"kimi-k3"}],"has_more":false});
        let (url, mut requests, task) = server(vec![
            (200, page1.to_string()),
            (200, page2.to_string()),
            (404, "not here".into()),
            (401, r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#.into()),
        ])
        .await;
        let mut catalog = Models::new();
        catalog.insert(
            "kimi-k3".into(),
            ModelMeta {
                text_output: true,
                context: Some(256_000),
                cost: Some((0.6, 2.5)),
                ..ModelMeta::default()
            },
        );
        catalog.insert(
            "painter".into(),
            ModelMeta {
                text_output: false,
                ..ModelMeta::default()
            },
        );
        let p = anthropic(&url, "kimi-k3", catalog);
        let listed = p.list_models().await.unwrap();
        assert_eq!(
            listed.iter().map(|m| m.model.as_str()).collect::<Vec<_>>(),
            ["claude-opus-x", "kimi-k3"]
        );
        assert_eq!(
            (listed[0].efforts.clone(), listed[0].context_window),
            (vec!["low".to_string(), "high".into()], Some(1_000_000))
        );
        // Ids alone: the catalog fills in context and price.
        assert_eq!(
            (listed[1].context_window, listed[1].price.as_deref()),
            (Some(256_000), Some("$0.6/$2.5 per 1M"))
        );
        assert_eq!(listed[1].efforts, ["low", "medium", "high"]);
        assert!(listed.iter().all(|m| m.provider == "anthropic"));
        let first = requests.recv().await.unwrap().to_lowercase();
        assert!(
            first.starts_with("get /v1/models?limit=1000 ") && first.contains("x-api-key: sk-ant"),
            "{first}"
        );
        assert!(requests
            .recv()
            .await
            .unwrap()
            .starts_with("GET /v1/models?limit=1000&after_id=claude-opus-x "));
        // No Models API: the catalog's text models plus the current one.
        let fallback = p.list_models().await.unwrap();
        assert_eq!(
            fallback
                .iter()
                .map(|m| m.model.as_str())
                .collect::<Vec<_>>(),
            ["kimi-k3"]
        );
        // A refused key is reported, not hidden behind the catalog.
        let err = p.list_models().await.unwrap_err();
        assert!(format!("{err:#}").contains("invalid x-api-key"), "{err:#}");
        task.await.unwrap();
    }
}
