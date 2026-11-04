//! User-named, credential-isolated OpenAI Chat Completions and Anthropic endpoints.

use std::{path::Path, sync::Mutex, time::Duration};

use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{
    anthropic::{self, AnthropicProvider},
    openai::OpenAiProvider,
    sse, EventSink, Provider,
};
use crate::{
    models::ModelChoice,
    types::{Request, Response},
};

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    Openai,
    Anthropic,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Definition {
    pub id: String,
    pub protocol: Protocol,
    pub base_url: String,
    pub api_key: Option<String>,
    pub model: String,
}

impl std::fmt::Debug for Definition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Definition")
            .field("id", &self.id)
            .field("protocol", &self.protocol)
            .field("base_url", &self.base_url)
            .field("api_key", &self.api_key.as_ref().map(|_| "[redacted]"))
            .field("model", &self.model)
            .finish()
    }
}

/// Canonical ids cannot collide with built-ins, aliases, or command selectors.
pub fn normalize_id(id: &str) -> Result<String> {
    ensure!(
        !id.chars().any(char::is_control),
        "provider id must not contain controls"
    );
    let id = id.trim().to_ascii_lowercase();
    ensure!(
        !id.is_empty() && id.len() <= 64
            && id.as_bytes()[0].is_ascii_alphanumeric()
            && id.bytes().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_')),
        "provider id must be 1–64 ASCII letters, digits, hyphens or underscores, starting with a letter or digit"
    );
    ensure!(
        !matches!(
            id.as_str(),
            "codex"
                | "chatgpt"
                | "grok"
                | "openai"
                | "ollama"
                | "anthropic"
                | "claude"
                | "opencode-go"
                | "go"
                | "opencode"
                | "zen"
                | "opencode-zen"
                | "xai"
                | "custom"
                | "all"
        ),
        "provider id is reserved; choose a unique name"
    );
    Ok(id)
}

fn normalize(definition: &Definition, require_model: bool) -> Result<Definition> {
    let mut d = definition.clone();
    // Reject controls before trimming, including newlines a URL parser might ignore.
    ensure!(
        !d.id.chars().any(char::is_control),
        "provider id must not contain controls"
    );
    d.id = normalize_id(&d.id)?;
    ensure!(
        !d.base_url.chars().any(char::is_control),
        "base URL must not contain controls"
    );
    let mut url = reqwest::Url::parse(d.base_url.trim())
        .map_err(|_| anyhow::anyhow!("base URL must be an absolute HTTP or HTTPS URL"))?;
    ensure!(
        matches!(url.scheme(), "http" | "https") && url.host_str().is_some(),
        "base URL must use HTTP or HTTPS and have a host"
    );
    let authority = d
        .base_url
        .trim()
        .split_once("://")
        .map(|(_, rest)| rest.split(['/', '?', '#']).next().unwrap_or_default())
        .unwrap_or_default();
    ensure!(
        url.username().is_empty() && url.password().is_none() && !authority.contains('@'),
        "base URL must not contain credentials"
    );
    ensure!(
        url.query().is_none() && url.fragment().is_none(),
        "base URL must not contain a query or fragment"
    );
    let path = url.path().trim_end_matches('/');
    let suffix = match d.protocol {
        Protocol::Openai => "/chat/completions",
        Protocol::Anthropic => "/messages",
    };
    let mut path = path
        .strip_suffix(suffix)
        .unwrap_or(path)
        .trim_end_matches('/')
        .to_owned();
    // Anthropic's origin takes /v1. Non-root gateway paths are authoritative:
    // appending /v1 there can break proxies exposing their own Messages route.
    // This is idempotent when a normalized definition is saved and reloaded.
    if d.protocol == Protocol::Anthropic && path.is_empty() {
        path.push_str("/v1");
    }
    url.set_path(&path);
    d.base_url = url.to_string().trim_end_matches('/').to_owned();
    d.api_key = match d.api_key {
        Some(key) => {
            ensure!(
                !key.chars().any(char::is_control),
                "API key must not contain controls"
            );
            let key = key.trim().to_owned();
            ensure!(
                key.is_empty() || reqwest::header::HeaderValue::from_str(&key).is_ok(),
                "API key is not a valid HTTP header value"
            );
            (!key.is_empty()).then_some(key)
        }
        None => None,
    };
    d.model = d.model.trim().to_owned();
    ensure!(
        !d.model.chars().any(char::is_control),
        "model must not contain controls"
    );
    ensure!(
        !require_model || !d.model.is_empty(),
        "provider needs a model"
    );
    Ok(d)
}

/// Read the private JSON array. Invalid existing data is never silently discarded.
pub fn load(home: &Path) -> Result<Vec<Definition>> {
    let path = home.join("providers.json");
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).context("reading providers.json"),
    };
    // Serde errors can quote arbitrary input, including a misplaced API key.
    let definitions: Vec<Definition> = serde_json::from_slice(&bytes).map_err(|e| {
        anyhow::anyhow!(
            "invalid providers.json at line {}, column {}; fix the file before changing providers",
            e.line(),
            e.column()
        )
    })?;
    let mut result: Vec<Definition> = Vec::new();
    for (i, definition) in definitions.iter().enumerate() {
        let d = normalize(definition, true)
            .with_context(|| format!("invalid providers.json entry {}", i + 1))?;
        ensure!(
            !result.iter().any(|known| known.id == d.id),
            "providers.json contains duplicate provider ids"
        );
        result.push(d);
    }
    Ok(result)
}

pub fn get(home: &Path, id: &str) -> Result<Option<Definition>> {
    let id = normalize_id(id)?;
    Ok(load(home)?.into_iter().find(|d| d.id == id))
}

fn persist(home: &Path, definitions: &[Definition]) -> Result<()> {
    crate::config::write_private(
        &home.join("providers.json"),
        &serde_json::to_vec_pretty(definitions)?,
    )
}

pub fn save(home: &Path, definition: &Definition) -> Result<()> {
    let d = normalize(definition, true)?;
    let mut definitions = load(home)?;
    match definitions.iter_mut().find(|known| known.id == d.id) {
        Some(known) => *known = d,
        None => definitions.push(d),
    }
    persist(home, &definitions)
}

pub fn remove(home: &Path, id: &str) -> Result<bool> {
    let id = normalize_id(id)?;
    let mut definitions = load(home)?;
    let before = definitions.len();
    definitions.retain(|d| d.id != id);
    let changed = definitions.len() != before;
    if changed {
        persist(home, &definitions)?;
    }
    Ok(changed)
}

/// Clear only this endpoint's saved key, keeping its URL, protocol and model.
pub fn logout(home: &Path, id: &str) -> Result<bool> {
    let id = normalize_id(id)?;
    let mut definitions = load(home)?;
    let changed = definitions
        .iter_mut()
        .find(|d| d.id == id)
        .is_some_and(|d| d.api_key.take().is_some());
    if changed {
        persist(home, &definitions)?;
    }
    Ok(changed)
}

/// True for discovery's explicit credential rejection (never use a fallback).
pub fn is_auth_error(error: &anyhow::Error) -> bool {
    error.chain().any(|e| {
        e.downcast_ref::<sse::HttpError>()
            .is_some_and(|e| matches!(e.status, 401 | 403))
    })
}

#[derive(Clone)]
struct Metadata {
    choice: ModelChoice,
    anthropic: Option<anthropic::ModelInfo>,
}

struct Catalog {
    id: String,
    base_url: String,
    protocol: Protocol,
    models: Vec<Metadata>,
}

// No credentials in the cache; names and protocols stay isolated even at one URL.
static CATALOGS: Mutex<Vec<Catalog>> = Mutex::new(Vec::new());

fn metadata(d: &Definition, model: &str) -> Option<Metadata> {
    CATALOGS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .find(|c| c.id == d.id && c.base_url == d.base_url && c.protocol == d.protocol)?
        .models
        .iter()
        .find(|m| m.choice.model == model)
        .cloned()
}

/// Bound the entire discovery, including pagination. Error bodies are deliberately
/// not rendered: compatible gateways sometimes echo Authorization headers.
pub async fn discover(definition: &Definition) -> Result<Vec<ModelChoice>> {
    let d = normalize(definition, false)?;
    tokio::time::timeout(Duration::from_secs(6), discover_inner(&d))
        .await
        .map_err(|_| anyhow::anyhow!("model discovery timed out after 6 seconds"))?
}

async fn discover_inner(d: &Definition) -> Result<Vec<ModelChoice>> {
    let client = crate::http::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(3))
        .timeout(Duration::from_secs(5))
        .build()
        .context("creating model discovery client")?;
    let mut models: Vec<Metadata> = Vec::new();
    let mut after: Option<String> = None;
    for page_number in 0..10 {
        let mut url = reqwest::Url::parse(&format!("{}/models", d.base_url))?;
        if d.protocol == Protocol::Anthropic {
            if let Some(after) = &after {
                url.query_pairs_mut().append_pair("after_id", after);
            }
        }
        let mut request = client
            .get(url)
            .header(reqwest::header::USER_AGENT, crate::config::USER_AGENT);
        request = match d.protocol {
            Protocol::Openai => match &d.api_key {
                Some(key) => request.bearer_auth(key),
                None => request,
            },
            Protocol::Anthropic => {
                request = request.header("anthropic-version", anthropic::VERSION);
                anthropic::authorize(request, d.api_key.as_deref(), false)
            }
        };
        let mut response = request.send().await.map_err(|e| {
            if e.is_timeout() {
                anyhow::anyhow!("model discovery timed out")
            } else if e.is_connect() {
                anyhow::anyhow!("could not connect to model discovery endpoint")
            } else {
                anyhow::anyhow!("model discovery request failed")
            }
        })?;
        let status = response.status();
        if !status.is_success() {
            return Err(sse::HttpError {
                status: status.as_u16(),
                retry_after: None,
                message: if matches!(status.as_u16(), 401 | 403) {
                    format!(
                        "API access denied · HTTP {} · check this provider's API key",
                        status.as_u16()
                    )
                } else {
                    format!("model discovery failed · HTTP {}", status.as_u16())
                },
            }
            .into());
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| anyhow::anyhow!("reading model discovery response failed"))?
        {
            ensure!(
                bytes.len() + chunk.len() <= 4 * 1024 * 1024,
                "model discovery response exceeds 4 MiB"
            );
            bytes.extend_from_slice(&chunk);
        }
        let page: Value = serde_json::from_slice(&bytes)
            .map_err(|_| anyhow::anyhow!("model discovery returned invalid JSON"))?;
        let data = page["data"]
            .as_array()
            .context("model discovery response has no data array")?;
        for value in data {
            let Some(model) = value["id"]
                .as_str()
                .map(str::trim)
                .filter(|m| !m.is_empty() && !m.chars().any(char::is_control))
            else {
                continue;
            };
            if models.iter().any(|m| m.choice.model == model) {
                continue;
            }
            let info = match d.protocol {
                Protocol::Anthropic => anthropic::parse_model(value),
                Protocol::Openai => {
                    if value["type"]
                        .as_str()
                        .is_some_and(|k| !matches!(k, "language" | "chat" | "model"))
                    {
                        continue;
                    }
                    None
                }
            };
            let efforts = info
                .as_ref()
                .map(|m| {
                    if m.efforts.is_empty() && m.thinking == anthropic::Thinking::Unknown {
                        anthropic::default_efforts(model)
                    } else {
                        m.efforts.clone()
                    }
                })
                .unwrap_or_default();
            let context_window = info
                .as_ref()
                .and_then(|m| m.context)
                .or_else(|| value["context_window"].as_u64().filter(|n| *n > 0));
            models.push(Metadata {
                choice: ModelChoice {
                    provider: d.id.clone(),
                    model: model.to_owned(),
                    context_window,
                    efforts,
                    ..Default::default()
                },
                anthropic: info,
            });
        }
        if d.protocol != Protocol::Anthropic || page["has_more"].as_bool() != Some(true) {
            break;
        }
        let next = page["last_id"].as_str().map(str::to_owned).or_else(|| {
            data.last()
                .and_then(|v| v["id"].as_str())
                .map(str::to_owned)
        });
        ensure!(
            !data.is_empty() && next.is_some() && next != after && page_number < 9,
            "model discovery pagination did not complete"
        );
        after = next;
    }
    ensure!(
        !models.is_empty(),
        "model discovery returned no chat models; configure a model manually"
    );
    let choices: Vec<_> = models.iter().map(|m| m.choice.clone()).collect();
    super::remember_listing(
        &d.id,
        choices.iter().map(|m| m.model.clone()).collect(),
        Vec::new(),
    );
    let mut catalogs = CATALOGS.lock().unwrap_or_else(|e| e.into_inner());
    catalogs.retain(|c| c.id != d.id || c.base_url != d.base_url || c.protocol != d.protocol);
    catalogs.push(Catalog {
        id: d.id.clone(),
        base_url: d.base_url.clone(),
        protocol: d.protocol,
        models,
    });
    Ok(choices)
}

pub fn build(
    home: &Path,
    id: &str,
    model: &str,
    effort: Option<String>,
) -> Result<Box<dyn Provider>> {
    let d = get(home, id)?.ok_or_else(|| crate::config::unknown_provider(id))?;
    let model = if model.trim().is_empty() {
        d.model.clone()
    } else {
        model.trim().to_owned()
    };
    ensure!(
        !model.chars().any(char::is_control),
        "model must not contain controls"
    );
    Ok(Box::new(CustomProvider::new(d, model, effort)))
}

struct CustomProvider {
    definition: Definition,
    inner: Box<dyn Provider>,
}

impl CustomProvider {
    fn new(d: Definition, model: String, effort: Option<String>) -> Self {
        let inner: Box<dyn Provider> = match d.protocol {
            Protocol::Openai => {
                let mut provider = OpenAiProvider::with_label(
                    d.id.clone(),
                    d.base_url.clone(),
                    d.api_key.clone(),
                    model,
                );
                provider.set_effort(effort);
                Box::new(provider)
            }
            Protocol::Anthropic => {
                let mut provider = AnthropicProvider::new(
                    d.id.clone(),
                    d.base_url.clone(),
                    d.api_key.clone(),
                    model.clone(),
                );
                if let Some(info) = metadata(&d, &model).and_then(|m| m.anthropic) {
                    provider.thinking = info.thinking;
                    if let Some(max) = info.max_output {
                        provider.max_tokens = max;
                    }
                }
                provider.set_effort(effort);
                Box::new(provider)
            }
        };
        Self {
            definition: d,
            inner,
        }
    }
}

#[async_trait::async_trait]
impl Provider for CustomProvider {
    fn name(&self) -> &str {
        &self.definition.id
    }
    fn model(&self) -> &str {
        self.inner.model()
    }
    fn endpoint(&self) -> Option<String> {
        Some(format!("{}/models", self.definition.base_url))
    }
    // Never infer a hosted vendor (or a different wire protocol) from a URL/model.
    fn pricing_provider(&self) -> &str {
        self.name()
    }
    fn effort(&self) -> Option<&str> {
        self.inner.effort()
    }
    fn set_effort(&mut self, effort: Option<String>) {
        self.inner.set_effort(effort);
    }
    fn context_window(&self) -> Option<u64> {
        metadata(&self.definition, self.model()).and_then(|m| m.choice.context_window)
    }
    fn efforts(&self) -> Vec<String> {
        metadata(&self.definition, self.model())
            .map(|m| m.choice.efforts)
            .unwrap_or_else(|| {
                if self.definition.protocol == Protocol::Anthropic {
                    anthropic::default_efforts(self.model())
                } else {
                    Vec::new()
                }
            })
    }
    fn set_model(&mut self, model: String) {
        *self = Self::new(
            self.definition.clone(),
            model,
            self.effort().map(str::to_owned),
        );
    }
    fn fork(&self) -> Box<dyn Provider> {
        Box::new(Self::new(
            self.definition.clone(),
            self.model().to_owned(),
            self.effort().map(str::to_owned),
        ))
    }
    async fn stream(&self, req: Request<'_>, on_event: EventSink<'_>) -> Result<Response> {
        self.inner.stream(req, on_event).await.map_err(|error| {
            let text = format!("{error:#}");
            let text = match &self.definition.api_key {
                Some(key) => text.replace(key, "[redacted]"),
                None => text,
            };
            // Keep typed HTTP statuses so transient failures still retry.
            if let Some(http) = error
                .chain()
                .find_map(|e| e.downcast_ref::<sse::HttpError>())
            {
                sse::HttpError {
                    status: http.status,
                    retry_after: http.retry_after,
                    message: text,
                }
                .into()
            } else {
                match error.downcast::<reqwest::Error>() {
                    Ok(error) => error.without_url().into(),
                    Err(_) => anyhow::anyhow!(text),
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{provider::sse::tests::events, types::Item};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        sync::mpsc,
    };

    fn definition(id: &str, protocol: Protocol, base_url: &str, key: Option<&str>) -> Definition {
        Definition {
            id: id.into(),
            protocol,
            base_url: base_url.into(),
            api_key: key.map(str::to_owned),
            model: "shared".into(),
        }
    }

    // One write per reply also lets tests close an error response without
    // requiring its untrusted body to be read by discovery.
    pub(crate) async fn server(
        replies: Vec<(u16, String)>,
    ) -> (
        String,
        mpsc::UnboundedReceiver<String>,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (tx, rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            for (status, body) in replies {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                loop {
                    let mut buf = [0; 2048];
                    let n = socket.read(&mut buf).await.unwrap();
                    assert!(n > 0);
                    bytes.extend_from_slice(&buf[..n]);
                    if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                        let header = String::from_utf8_lossy(&bytes[..end]);
                        let length = header
                            .lines()
                            .find_map(|line| {
                                line.split_once(':')
                                    .filter(|(k, _)| k.eq_ignore_ascii_case("content-length"))
                                    .map(|(_, v)| v.trim().parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        if bytes.len() >= end + 4 + length {
                            break;
                        }
                    }
                }
                tx.send(String::from_utf8(bytes).unwrap()).unwrap();
                let response = format!("HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                let _ = socket.write_all(response.as_bytes()).await;
            }
        });
        (url, rx, task)
    }

    #[test]
    fn ids_urls_and_normalization() {
        for id in [
            "openai",
            "CODEX",
            "chatgpt",
            "grok",
            "go",
            "zen",
            "opencode-zen",
            "claude",
            "anthropic",
            "ollama",
            "xai",
            "custom",
            "all",
            "a/b",
            "a b",
            "../a",
            "-a",
            "a\n",
            "a\t",
            "é",
            "",
        ] {
            assert!(normalize_id(id).is_err(), "{id:?}");
        }
        assert_eq!(normalize_id(" Work_2 ").unwrap(), "work_2");
        for url in [
            "ftp://host/v1",
            "file:///tmp/models",
            "https://user:secret@host/v1",
            "https://host/v1?q=secret",
            "https://host/v1#secret",
            "https://host/\n",
            "host/v1",
        ] {
            assert!(
                normalize(&definition("work", Protocol::Openai, url, None), true).is_err(),
                "{url}"
            );
        }
        for (protocol, url, expected) in [
            (
                Protocol::Openai,
                "http://localhost:8000/gateway/v1/chat/completions/",
                "http://localhost:8000/gateway/v1",
            ),
            (
                Protocol::Openai,
                "https://example.test/api",
                "https://example.test/api",
            ),
            (
                Protocol::Anthropic,
                "https://example.test",
                "https://example.test/v1",
            ),
            (
                Protocol::Anthropic,
                "https://example.test/anthropic",
                "https://example.test/anthropic",
            ),
            (
                Protocol::Anthropic,
                "https://example.test/gateway/v1/messages/",
                "https://example.test/gateway/v1",
            ),
            (
                Protocol::Anthropic,
                "http://[::1]:1234/gateway/messages",
                "http://[::1]:1234/gateway",
            ),
        ] {
            let normalized =
                normalize(&definition("Work", protocol, url, Some(" key ")), true).unwrap();
            assert_eq!(normalized.base_url, expected);
            assert_eq!(normalized.id, "work");
            assert_eq!(normalized.api_key.as_deref(), Some("key"));
            assert_eq!(normalize(&normalized, true).unwrap().base_url, expected);
        }
        let mut d = definition(
            "work",
            Protocol::Openai,
            "http://localhost",
            Some("key\nleak"),
        );
        assert!(normalize(&d, true).is_err());
        d.api_key = Some(" ".into());
        assert!(normalize(&d, true).unwrap().api_key.is_none());
        d.model.clear();
        assert!(normalize(&d, false).is_ok());
        assert!(normalize(&d, true).is_err());
    }

    #[test]
    fn private_persistence_logout_and_removal_are_isolated() {
        let home = tempfile::tempdir().unwrap();
        let a = definition(
            "Work",
            Protocol::Openai,
            "https://a.test/v1/chat/completions",
            Some("alpha-secret"),
        );
        let b = definition(
            "personal",
            Protocol::Anthropic,
            "https://b.test/v1",
            Some("beta-secret"),
        );
        save(home.path(), &a).unwrap();
        save(home.path(), &b).unwrap();
        assert_eq!(load(home.path()).unwrap().len(), 2);
        assert!(!format!("{a:?}").contains("alpha-secret"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(home.path().join("providers.json"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        assert!(logout(home.path(), "WORK").unwrap());
        assert!(!logout(home.path(), "work").unwrap());
        let saved = get(home.path(), "work").unwrap().unwrap();
        assert!(saved.api_key.is_none());
        assert_eq!(saved.base_url, "https://a.test/v1");
        assert_eq!(saved.model, "shared");
        assert_eq!(
            get(home.path(), "personal")
                .unwrap()
                .unwrap()
                .api_key
                .as_deref(),
            Some("beta-secret")
        );
        assert!(remove(home.path(), "work").unwrap());
        assert!(!remove(home.path(), "work").unwrap());
        assert_eq!(load(home.path()).unwrap().len(), 1);
        assert_eq!(
            build(home.path(), "personal", "other", Some("high".into()))
                .unwrap()
                .model(),
            "other"
        );
    }

    #[test]
    fn malformed_existing_data_is_never_overwritten() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("providers.json");
        let d = definition("work", Protocol::Openai, "https://example.test/v1", None);
        let duplicate = serde_json::to_string(&vec![d.clone(), d.clone()]).unwrap();
        let mut invalid = d.clone();
        invalid.base_url = "https://user:secret@host/v1".into();
        for contents in [
            "not json".into(),
            "{}".into(),
            "[{\"api_key\":\"secret\",\"protocol\":\"secret\"}]".into(),
            duplicate,
            serde_json::to_string(&vec![invalid]).unwrap(),
        ] {
            std::fs::write(&path, &contents).unwrap();
            let error = load(home.path()).unwrap_err();
            assert!(!format!("{error:#}").contains("secret"));
            assert!(save(home.path(), &d).is_err());
            assert!(logout(home.path(), "work").is_err());
            assert!(remove(home.path(), "work").is_err());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), contents);
        }
    }

    #[tokio::test]
    async fn discovery_and_streaming_keep_routes_protocols_and_keys_isolated() {
        let anthropic_stream = events(&[
            serde_json::json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hello"}}),
            serde_json::json!({"type":"message_stop"}),
        ]);
        let (url, mut requests, task) = server(vec![
            (200, r#"{"data":[{"id":"shared","context_window":12345},{"id":"image","type":"image"}]}"#.into()),
            (200, r#"{"data":[{"id":"shared","max_input_tokens":98765,"max_tokens":1024,"capabilities":{"thinking":{"supported":false}}}]}"#.into()),
            (200, "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hello\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n".into()),
            (200, anthropic_stream),
        ]).await;
        let home = tempfile::tempdir().unwrap();
        let mut a = definition(
            "isolated-openai",
            Protocol::Openai,
            &format!("{url}/gateway/v1/chat/completions"),
            Some("alpha-secret"),
        );
        let b = definition(
            "isolated-anthropic",
            Protocol::Anthropic,
            &format!("{url}/gateway/messages"),
            Some("beta-secret"),
        );
        a.model.clear(); // Login discovers before a default model has been picked.
        let choices = discover(&a).await.unwrap();
        assert_eq!(choices.len(), 1);
        assert_eq!(choices[0].provider, "isolated-openai");
        a.model = choices[0].model.clone();
        let choices = discover(&b).await.unwrap();
        assert!(choices[0].efforts.is_empty());
        save(home.path(), &a).unwrap();
        save(home.path(), &b).unwrap();
        let mut openai = build(home.path(), &a.id, "shared", Some("low".into())).unwrap();
        let anthropic = build(home.path(), &b.id, "shared", None).unwrap();
        assert_eq!(openai.context_window(), Some(12345));
        assert_eq!(anthropic.context_window(), Some(98765));
        assert_eq!(openai.pricing_provider(), "isolated-openai");
        assert_eq!(anthropic.endpoint(), Some(format!("{url}/gateway/models")));
        openai.set_model("other".into());
        assert_eq!(openai.fork().model(), "other");
        openai.set_model("shared".into());
        for provider in [&openai, &anthropic] {
            let response = provider
                .stream(
                    Request {
                        system: "",
                        items: &[],
                        tools: &[],
                    },
                    &mut |_| {},
                )
                .await
                .unwrap();
            assert_eq!(
                response.items,
                vec![Item::Assistant {
                    text: "hello".into()
                }]
            );
        }
        task.await.unwrap();
        let mut seen = Vec::new();
        while let Ok(request) = requests.try_recv() {
            seen.push(request.to_lowercase());
        }
        assert_eq!(seen.len(), 4);
        for (i, request) in seen.iter().enumerate() {
            if i % 2 == 0 {
                assert!(request.contains("authorization: bearer alpha-secret"));
                assert!(!request.contains("beta-secret") && !request.contains("x-api-key:"));
            } else {
                assert!(request.contains("x-api-key: beta-secret"));
                assert!(request.contains("anthropic-version:"));
                assert!(!request.contains("alpha-secret") && !request.contains("authorization:"));
            }
        }
        assert!(seen[0].starts_with("get /gateway/v1/models "));
        assert!(seen[1].starts_with("get /gateway/models "));
        assert!(seen[2].starts_with("post /gateway/v1/chat/completions "));
        assert!(seen[3].starts_with("post /gateway/messages "));
        assert!(seen[3].contains("\"max_tokens\":1024"));
        assert!(!seen[3].contains("\"thinking\""));
    }

    #[tokio::test]
    async fn discovery_auth_errors_are_typed_and_never_echo_keys() {
        for protocol in [Protocol::Openai, Protocol::Anthropic] {
            for status in [401, 403, 404, 500] {
                let (url, _requests, task) = server(vec![(
                    status,
                    r#"{"error":{"message":"echo secret-key"}}"#.into(),
                )])
                .await;
                let d = definition("auth-test", protocol, &url, Some("secret-key"));
                let error = discover(&d).await.unwrap_err();
                assert_eq!(is_auth_error(&error), matches!(status, 401 | 403));
                assert!(error.to_string().contains(&format!("HTTP {status}")));
                assert!(!format!("{error:#}").contains("secret-key"));
                task.await.unwrap();
            }
        }
    }

    #[tokio::test]
    async fn discovery_paginates_anthropic_and_preserves_capabilities() {
        let (url, mut requests, task) = server(vec![
            (200, r#"{"data":[{"id":"claude-first","max_input_tokens":100000,"capabilities":{"thinking":{"supported":true,"types":{"adaptive":{"supported":true}}},"effort":{"supported":true,"low":{"supported":true},"high":{"supported":true}}}}],"has_more":true,"last_id":"a/b"}"#.into()),
            (200, r#"{"data":[{"id":"claude-second"}],"has_more":false}"#.into()),
        ]).await;
        let d = definition("paged", Protocol::Anthropic, &url, None);
        let choices = discover(&d).await.unwrap();
        assert_eq!(choices.len(), 2);
        assert_eq!(choices[0].efforts, ["low", "high"]);
        let home = tempfile::tempdir().unwrap();
        save(home.path(), &d).unwrap();
        let mut provider =
            build(home.path(), "paged", "claude-first", Some("high".into())).unwrap();
        assert_eq!(provider.context_window(), Some(100000));
        assert_eq!(provider.efforts(), ["low", "high"]);
        provider.set_model("claude-second".into());
        assert_eq!(provider.context_window(), None);
        assert!(!provider.efforts().is_empty());
        task.await.unwrap();
        assert!(requests.try_recv().unwrap().starts_with("GET /v1/models "));
        assert!(requests
            .try_recv()
            .unwrap()
            .starts_with("GET /v1/models?after_id=a%2Fb "));
    }

    #[tokio::test]
    async fn discovery_invalid_empty_and_redirect_responses_are_safe() {
        for (status, body) in [
            (200, "not JSON secret-key"),
            (200, "{}"),
            (200, "{\"data\":[]}"),
            (302, "secret-key"),
        ] {
            let (url, _requests, task) = server(vec![(status, body.into())]).await;
            let d = definition(
                "invalid-discovery",
                Protocol::Openai,
                &url,
                Some("secret-key"),
            );
            let error = discover(&d).await.unwrap_err();
            assert!(!is_auth_error(&error));
            assert!(!format!("{error:#}").contains("secret-key"));
            task.await.unwrap();
        }
    }
}
