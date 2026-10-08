//! ChatGPT-subscription Codex Responses API provider.

use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

use anyhow::{Context, Result};
use eventsource_stream::Eventsource;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::Mutex;

use super::{sse, EventSink, Provider};
use crate::types::{Item, Request, Response, StreamEvent, Usage};

pub struct CodexProvider {
    pub auth_path: PathBuf,
    pub model: String,
    pub effort: Option<String>,
    auth: Arc<Mutex<Option<Auth>>>,
    session_id: String,
    client: reqwest::Client,
}

#[derive(Clone, Deserialize, Serialize)]
struct Auth {
    version: Value,
    access_token: String,
    refresh_token: String,
    expires_at_ms: i64,
    account_id: String,
    /// Borrowed from the codex CLI (`~/.codex/auth.json`): use, never refresh
    /// or rewrite, so the codex CLI keeps ownership of its refresh token.
    #[serde(skip)]
    read_only: bool,
}

impl Auth {
    fn parse(bytes: &[u8]) -> Result<Self> {
        let v: Value = serde_json::from_slice(bytes)?;
        let Some(t) = v.get("tokens") else {
            return Ok(serde_json::from_value(v)?);
        };
        let field = |k: &str| {
            t[k].as_str()
                .map(str::to_owned)
                .with_context(|| format!("codex auth file missing tokens.{k}"))
        };
        let access_token = field("access_token")?;
        Ok(Self {
            version: Value::Null,
            expires_at_ms: jwt_exp_ms(&access_token).unwrap_or(0),
            refresh_token: field("refresh_token").unwrap_or_default(),
            account_id: field("account_id")?,
            access_token,
            read_only: true,
        })
    }
}

/// The `exp` claim of a JWT, in milliseconds.
/// The Codex client version reported when listing models. The backend only
/// lists newer models (e.g. the GPT-6 family) to new enough clients, so use
/// the installed codex CLI's version when it is newer than our baseline.
pub async fn client_version() -> String {
    const BASELINE: &str = "0.160.1";
    if let Ok(v) = std::env::var("NYQ_CODEX_CLIENT_VERSION") {
        return v;
    }
    let installed = std::process::Command::new("codex")
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.split_whitespace().last().map(str::to_string));
    let latest = latest_published().await;
    let best = newest(BASELINE, installed.as_deref());
    newest(best, latest.as_deref()).to_string()
}

/// The newest codex CLI version published on npm, cached for a day under the
/// nyq home so listing models stays fast and works offline.
async fn latest_published() -> Option<String> {
    let cache = crate::config::home().join("nyq").join("codex-version");
    let fresh = std::fs::metadata(&cache)
        .and_then(|m| m.modified())
        .is_ok_and(|t| t.elapsed().is_ok_and(|age| age < std::time::Duration::from_secs(86_400)));
    let cached = || std::fs::read_to_string(&cache).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    if fresh {
        return cached();
    }
    let fetched = async {
        let client = reqwest::Client::builder().timeout(std::time::Duration::from_secs(3)).build().ok()?;
        let body: serde_json::Value = client
            .get("https://registry.npmjs.org/@openai/codex/latest")
            .header("User-Agent", concat!("nyq/", env!("CARGO_PKG_VERSION")))
            .send()
            .await
            .ok()?
            .json()
            .await
            .ok()?;
        body["version"].as_str().map(str::to_string)
    }
    .await;
    match fetched {
        Some(v) => {
            let _ = std::fs::create_dir_all(cache.parent().unwrap_or(&cache));
            let _ = std::fs::write(&cache, &v);
            Some(v)
        }
        None => cached(),
    }
}

fn newest<'a>(baseline: &'a str, other: Option<&'a str>) -> &'a str {
    let parse = |v: &str| -> Option<Vec<u64>> { v.trim_start_matches('v').split('.').map(|p| p.parse().ok()).collect() };
    match other {
        Some(o) if parse(o).zip(parse(baseline)).is_some_and(|(a, b)| a > b) => o,
        _ => baseline,
    }
}

pub fn jwt_exp_ms(token: &str) -> Option<i64> {
    use base64::Engine;
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .ok()?;
    serde_json::from_slice::<Value>(&bytes)
        .ok()?["exp"]
        .as_i64()
        .and_then(|s| s.checked_mul(1000))
}

impl CodexProvider {
    pub fn new(auth_path: PathBuf, model: String, effort: Option<String>) -> Self {
        Self {
            auth_path,
            model,
            effort,
            auth: Arc::new(Mutex::new(None)),
            session_id: uuid::Uuid::new_v4().to_string(),
            client: reqwest::Client::new(),
        }
    }

    pub async fn list_models(&self) -> Result<Vec<crate::models::ModelChoice>> {
        #[derive(Deserialize)]
        struct Catalog {
            models: Vec<Model>,
        }
        #[derive(Deserialize)]
        struct Model {
            slug: String,
            visibility: String,
            supported_in_api: Option<bool>,
            #[serde(default)]
            priority: i64,
            #[serde(default)]
            supported_reasoning_levels: Vec<Level>,
            context_window: Option<u64>,
        }
        #[derive(Deserialize)]
        struct Level {
            effort: String,
        }
        let url = match std::env::var("NYQ_CODEX_MODELS_URL") {
            Ok(url) => url,
            Err(_) => format!("https://chatgpt.com/backend-api/codex/models?client_version={}", client_version().await),
        };
        let send = |auth: Auth| {
            self.client
                .get(&url)
                .bearer_auth(auth.access_token)
                .header("chatgpt-account-id", auth.account_id)
                .header("originator", "nyq")
                .timeout(std::time::Duration::from_secs(5))
                .send()
        };
        let auth = self.token(None).await?;
        let mut http = send(auth.clone()).await?;
        if http.status() == reqwest::StatusCode::UNAUTHORIZED {
            http = send(self.token(Some(&auth.access_token)).await?).await?;
        }
        let mut catalog: Catalog = sse::checked(http).await?.json().await?;
        catalog
            .models
            .retain(|m| m.visibility == "list" && m.supported_in_api != Some(false));
        catalog.models.sort_by_key(|m| m.priority);
        Ok(catalog
            .models
            .into_iter()
            .map(|m| crate::models::ModelChoice {
                provider: "codex".into(),
                model: m.slug,
                context_window: m.context_window,
                efforts: m
                    .supported_reasoning_levels
                    .into_iter()
                    .map(|l| l.effort)
                    .collect(),
            })
            .collect())
    }

    fn body(&self, req: Request<'_>) -> Value {
        let input: Vec<Value> = req.items.iter().filter_map(|item| Some(match item {
            Item::User { text, images } => {
                let mut content = vec![json!({"type":"input_text","text":text})];
                content.extend(images.iter().map(|i| json!({"type":"input_image","detail":"auto","image_url":i.data_url()})));
                json!({"role":"user","content":content})
            }
            Item::Compaction { summary } => json!({"role":"user","content":[{"type":"input_text","text":crate::types::compaction_text(summary)}]}),
            Item::Assistant { text } => json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":text}]}),
            Item::Reasoning { summary, encrypted: Some(encrypted), .. } => json!({"type":"reasoning","summary":if summary.is_empty() { vec![] } else { vec![json!({"type":"summary_text","text":summary})] },"encrypted_content":encrypted}),
            Item::Reasoning { encrypted: None, .. } => return None,
            Item::ToolCall { id, name, arguments } => json!({"type":"function_call","call_id":id,"name":name,"arguments":arguments}),
            Item::ToolResult { id, output, .. } => json!({"type":"function_call_output","call_id":id,"output":output}),
        })).collect();
        // Codex has native web search; it replaces our DuckDuckGo function tool.
        let tools: Vec<_> = req.tools.iter().map(|t| if t.name == "web_search" { json!({"type":"web_search"}) } else { json!({"type":"function","name":t.name,"description":t.description,"parameters":t.parameters,"strict":false}) }).collect();
        let mut body = json!({"model":self.model,"store":false,"stream":true,"instructions":req.system,"input":input,"tools":tools,"tool_choice":"auto","parallel_tool_calls":true,"include":["reasoning.encrypted_content"],"text":{"verbosity":"low"}});
        if let Some(effort) = &self.effort {
            body["reasoning"] =
                json!({"effort":if effort == "minimal" { "low" } else { effort },"summary":"auto"});
        }
        body
    }

    fn token_usable(auth: &Auth, rejected: Option<&str>) -> bool {
        let expired = auth.expires_at_ms.saturating_sub(60_000) <= chrono::Utc::now().timestamp_millis();
        !expired && rejected != Some(auth.access_token.as_str())
    }

    async fn load_auth(&self) -> Result<Auth> {
        let bytes = tokio::fs::read(&self.auth_path).await.context("reading Codex auth file")?;
        Auth::parse(&bytes).context("parsing Codex auth file")
    }

    /// Pick up a token another fork or process already rotated into the auth file.
    async fn reread_usable(&self, previous: &Auth, rejected: Option<&str>) -> Option<Auth> {
        let fresh = self.load_auth().await.ok()?;
        let changed = fresh.access_token != previous.access_token || fresh.refresh_token != previous.refresh_token;
        (changed && Self::token_usable(&fresh, rejected)).then_some(fresh)
    }

    async fn refresh(&self, auth: &Auth) -> Result<Auth> {
        let url = std::env::var("NYQ_CODEX_TOKEN_URL").unwrap_or_else(|_| {
            let issuer = std::env::var("NYQ_CODEX_ISSUER").unwrap_or_else(|_| "https://auth.openai.com".into());
            format!("{}/oauth/token", issuer.trim_end_matches('/'))
        });
        let result: Value = sse::checked(
            self.client
                .post(url)
                .json(&json!({
                    "client_id": "app_EMoamEEZ73f0CkXaXp7hrann",
                    "grant_type": "refresh_token",
                    "refresh_token": auth.refresh_token,
                }))
                .send()
                .await?,
        )
        .await?
        .json()
        .await?;
        let mut refreshed = auth.clone();
        refreshed.access_token = result["access_token"]
            .as_str()
            .context("token response missing access_token")?
            .to_owned();
        if let Some(token) = result["refresh_token"].as_str() {
            refreshed.refresh_token = token.to_owned();
        }
        let seconds = result["expires_in"].as_i64().context("token response missing expires_in")?;
        anyhow::ensure!(seconds > 0, "token response has invalid expires_in");
        refreshed.expires_at_ms = chrono::Utc::now().timestamp_millis().saturating_add(seconds.saturating_mul(1000));
        self.save_auth(&refreshed)?;
        Ok(refreshed)
    }

    async fn token(&self, rejected: Option<&str>) -> Result<Auth> {
        let mut cached = self.auth.lock().await;
        if cached.is_none() {
            *cached = Some(self.load_auth().await?);
        }
        let auth = cached.as_ref().unwrap().clone();
        if Self::token_usable(&auth, rejected) {
            return Ok(auth);
        }
        if let Some(fresh) = self.reread_usable(&auth, rejected).await {
            *cached = Some(fresh.clone());
            return Ok(fresh);
        }
        let auth = cached.as_ref().unwrap().clone();
        anyhow::ensure!(
            !auth.read_only,
            "the codex CLI login has expired; run `codex` once to refresh it"
        );
        match self.refresh(&auth).await {
            Ok(new_auth) => {
                *cached = Some(new_auth.clone());
                Ok(new_auth)
            }
            Err(e) => {
                if let Some(fresh) = self.reread_usable(&auth, rejected).await {
                    *cached = Some(fresh.clone());
                    return Ok(fresh);
                }
                Err(e)
            }
        }
    }

    fn save_auth(&self, auth: &Auth) -> Result<()> {
        use std::io::Write;
        let parent = self
            .auth_path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."));
        let temp = parent.join(format!(".nyq-auth-{}.tmp", uuid::Uuid::new_v4()));
        let result = (|| -> Result<()> {
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&temp)?;
            file.write_all(&serde_json::to_vec(auth)?)?;
            file.sync_all()?;
            crate::tools::safeio::rename_over(&temp, &self.auth_path)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temp);
        }
        result.context("saving refreshed Codex auth file")
    }

    async fn send(&self, url: &str, body: &Value, auth: &Auth) -> Result<reqwest::Response> {
        Ok(self
            .client
            .post(url)
            .bearer_auth(&auth.access_token)
            .header("chatgpt-account-id", &auth.account_id)
            .header("originator", "nyq")
            .header("OpenAI-Beta", "responses=experimental")
            .header("accept", "text/event-stream")
            .header("session_id", &self.session_id)
            .json(body)
            .send()
            .await?)
    }
}

fn output_item(item: &Value) -> Option<Item> {
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
                .map(|parts| {
                    parts
                        .iter()
                        .filter_map(|part| part["text"].as_str())
                        .collect::<String>()
                })
                .unwrap_or_default(),
            encrypted: item["encrypted_content"].as_str().map(str::to_owned),
        }),
        _ => None,
    }
}

#[async_trait::async_trait]
impl Provider for CodexProvider {
    fn set_conversation(&mut self, id: &str) {
        self.session_id = id.to_string();
    }
    fn fork(&self) -> Box<dyn Provider> {
        Box::new(Self {
            auth_path: self.auth_path.clone(),
            model: self.model.clone(),
            effort: self.effort.clone(),
            auth: Arc::clone(&self.auth),
            session_id: uuid::Uuid::new_v4().to_string(),
            client: self.client.clone(),
        })
    }
    fn name(&self) -> &str {
        "codex"
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

    async fn stream(&self, req: Request<'_>, on_event: EventSink<'_>) -> Result<Response> {
        let body = self.body(req);
        let url = std::env::var("NYQ_CODEX_URL")
            .unwrap_or_else(|_| "https://chatgpt.com/backend-api/codex/responses".into());
        let auth = self.token(None).await?;
        let mut http = self.send(&url, &body, &auth).await?;
        if http.status() == reqwest::StatusCode::UNAUTHORIZED {
            let refreshed = self.token(Some(&auth.access_token)).await?;
            http = self.send(&url, &body, &refreshed).await?;
        }
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
                "response.output_text.delta" | "response.refusal.delta" => {
                    on_event(StreamEvent::Text(sse::string(&data, "delta")))
                }
                "response.reasoning_summary_text.delta" => {
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
                "response.output_item.added" | "response.output_item.done"
                    if data["item"]["type"] == "web_search_call" =>
                {
                    let item = &data["item"];
                    let id = sse::string(item, "id");
                    let has_query = item["action"]["query"].as_str().is_some();
                    if (kind == "response.output_item.added" || has_query) && searches.insert(id.clone()) {
                        on_event(StreamEvent::ToolCallStart { id, name: "web_search".into() });
                    }
                }
                "response.output_item.done" => {
                    if let Some(item) = output_item(&data["item"]) {
                        let index = data["output_index"]
                            .as_u64()
                            .context("output item missing output_index")?;
                        items.insert(index, item);
                    }
                }
                "response.completed" => {
                    let usage = &data["response"]["usage"];
                    if !usage.is_null() {
                        response.usage = Some(Usage {
                            input_tokens: usage["input_tokens"].as_u64().unwrap_or(0),
                            output_tokens: usage["output_tokens"].as_u64().unwrap_or(0),
                            cached_tokens: usage["input_tokens_details"]["cached_tokens"]
                                .as_u64()
                                .unwrap_or(0),
                        });
                    }
                    completed = true;
                    break;
                }
                "response.failed" | "error" | "response.incomplete" => {
                    anyhow::bail!("{}", sse::failure(&data))
                }
                _ => {}
            }
        }
        anyhow::ensure!(completed, "Codex stream ended before response.completed");
        response.items = items.into_values().collect();
        Ok(response)
    }
}

#[cfg(test)]
// Tests serialize env-var mutation with a std Mutex held for the whole test.
#[allow(clippy::await_holding_lock)]
mod tests {
    use super::*;
    use crate::{
        provider::sse::tests::{events, server, Env, ENV},
        types::ToolSpec,
    };

    #[test]
    fn refusal_is_assistant_text() {
        let item = output_item(&json!({
            "type": "message",
            "content": [
                {"type": "output_text", "text": "Hello. "},
                {"type": "refusal", "refusal": "I won't do that."}
            ]
        }))
        .unwrap();
        assert_eq!(item, Item::Assistant { text: "Hello. I won't do that.".into() });
        let text_field = output_item(&json!({
            "type": "message",
            "content": [{"type": "refusal", "text": "No."}]
        }))
        .unwrap();
        assert_eq!(text_field, Item::Assistant { text: "No.".into() });
    }

    fn auth_file(dir: &tempfile::TempDir, expires: i64) -> PathBuf {
        let path = dir.path().join("auth.json");
        std::fs::write(&path, json!({"version":1,"access_token":"old","refresh_token":"refresh","expires_at_ms":expires,"account_id":"account"}).to_string()).unwrap();
        path
    }

    #[tokio::test]
    async fn models_endpoint() {
        let _lock = ENV.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let mut provider = CodexProvider::new(auth_file(&dir, i64::MAX), "test".into(), None);
        provider.set_effort(Some("high".into()));
        assert_eq!(provider.effort(), Some("high"));
        provider.set_effort(Some("auto".into()));
        assert_eq!(provider.effort(), None);
        let (url, mut requests, task) = server(vec![(200, json!({"models":[
            {"slug":"later","visibility":"list","supported_in_api":true,"priority":2,"context_window":272000,"supported_reasoning_levels":[{"effort":"low"},{"effort":"high"}]},
            {"slug":"hidden","visibility":"hide","priority":0},
            {"slug":"unsupported","visibility":"list","supported_in_api":false,"priority":0},
            {"slug":"first","visibility":"list","priority":1}
        ]}).to_string())]).await;
        let old = std::env::var_os("NYQ_CODEX_MODELS_URL");
        std::env::set_var(
            "NYQ_CODEX_MODELS_URL",
            format!("{url}/models?client_version=test"),
        );
        let result = provider.list_models().await;
        if let Some(old) = old {
            std::env::set_var("NYQ_CODEX_MODELS_URL", old);
        } else {
            std::env::remove_var("NYQ_CODEX_MODELS_URL");
        }
        let models = result.unwrap();
        assert_eq!(
            models.iter().map(|m| m.model.as_str()).collect::<Vec<_>>(),
            vec!["first", "later"]
        );
        assert_eq!(models[1].efforts, vec!["low", "high"]);
        assert_eq!(models[1].context_window, Some(272000));
        assert_eq!(models[0].provider, "codex");
        let request = requests.recv().await.unwrap().to_lowercase();
        assert!(request.starts_with("get /models?client_version=test "));
        for header in [
            "authorization: bearer old",
            "chatgpt-account-id: account",
            "originator: nyq",
        ] {
            assert!(request.contains(header));
        }
        task.await.unwrap();
    }

    #[test]
    fn client_version_prefers_newer_installed_codex() {
        assert_eq!(newest("0.160.1", Some("0.170.0")), "0.170.0");
        assert_eq!(newest("0.160.1", Some("0.150.9")), "0.160.1");
        assert_eq!(newest("0.160.1", Some("garbage")), "0.160.1");
        assert_eq!(newest("0.160.1", None), "0.160.1");
    }

    #[test]
    fn request_body() {
        let provider = CodexProvider::new(PathBuf::new(), "test".into(), Some("minimal".into()));
        let items = vec![
            Item::user("question"),
            Item::Assistant {
                text: "answer".into(),
            },
            Item::Reasoning {
                id: Some("r".into()),
                summary: "thought".into(),
                encrypted: Some("opaque".into()),
            },
            Item::Reasoning {
                id: None,
                summary: "".into(),
                encrypted: Some("empty".into()),
            },
            Item::Reasoning {
                id: None,
                summary: "skip".into(),
                encrypted: None,
            },
            Item::ToolCall {
                id: "c".into(),
                name: "one".into(),
                arguments: "{}".into(),
            },
            Item::ToolResult {
                id: "c".into(),
                output: "done".into(),
                is_error: false,
            },
        ];
        let tools = vec![ToolSpec {
            name: "one".into(),
            description: "desc".into(),
            parameters: json!({"type":"object"}),
        }];
        let body = provider.body(Request {
            system: "sys",
            items: &items,
            tools: &tools,
        });
        assert_eq!(
            body["input"],
            json!([
                {"role":"user","content":[{"type":"input_text","text":"question"}]},
                {"type":"message","role":"assistant","content":[{"type":"output_text","text":"answer"}]},
                {"type":"reasoning","summary":[{"type":"summary_text","text":"thought"}],"encrypted_content":"opaque"},
                {"type":"reasoning","summary":[],"encrypted_content":"empty"},
                {"type":"function_call","call_id":"c","name":"one","arguments":"{}"},
                {"type":"function_call_output","call_id":"c","output":"done"}
            ])
        );
        assert_eq!(body["instructions"], "sys");
        assert_eq!(body["reasoning"], json!({"effort":"low","summary":"auto"}));
        assert_eq!(
            body["tools"][0],
            json!({"type":"function","name":"one","description":"desc","parameters":{"type":"object"},"strict":false})
        );
        assert_eq!(body["store"], false);
        assert_eq!(body["stream"], true);
        let provider = CodexProvider::new(PathBuf::new(), "test".into(), None);
        assert!(provider
            .body(Request {
                system: "",
                items: &[],
                tools: &[]
            })
            .get("reasoning")
            .is_none());
    }

    fn completed() -> Value {
        json!({"type":"response.completed","response":{"usage":{"input_tokens":12,"output_tokens":5,"input_tokens_details":{"cached_tokens":4}}}})
    }

    #[tokio::test]
    async fn streaming() {
        let _lock = ENV.lock().unwrap();
        let _env = Env;
        let dir = tempfile::tempdir().unwrap();
        let provider = CodexProvider::new(auth_file(&dir, i64::MAX), "test".into(), None);
        let body = events(&[
            json!({"type":"response.reasoning_summary_text.delta","delta":"think"}),
            json!({"type":"response.output_text.delta","delta":"héllo"}),
            json!({"type":"response.output_item.added","output_index":2,"item":{"type":"function_call","call_id":"a","name":"one"}}),
            json!({"type":"response.output_item.added","output_index":3,"item":{"type":"function_call","call_id":"b","name":"two"}}),
            // Done events can arrive out of order; indices define transcript order.
            json!({"type":"response.output_item.done","output_index":3,"item":{"type":"function_call","call_id":"b","name":"two","arguments":"{}"}}),
            json!({"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","id":"r","summary":[{"type":"summary_text","text":"think"}],"encrypted_content":"opaque"}}),
            json!({"type":"response.output_item.done","output_index":2,"item":{"type":"function_call","call_id":"a","name":"one","arguments":"{\"x\":1}"}}),
            json!({"type":"response.output_item.done","output_index":1,"item":{"type":"message","content":[{"type":"output_text","text":"hé"},{"type":"output_text","text":"llo"}]}}),
            completed(),
        ]);
        let (url, mut requests, task) = server(vec![(200, body)]).await;
        Env::set("NYQ_CODEX_URL", &url);
        let mut received = Vec::new();
        let response = provider
            .stream(
                Request {
                    system: "sys",
                    items: &[],
                    tools: &[],
                },
                &mut |e| received.push(e),
            )
            .await
            .unwrap();
        assert_eq!(
            response.items,
            vec![
                Item::Reasoning {
                    id: Some("r".into()),
                    summary: "think".into(),
                    encrypted: Some("opaque".into())
                },
                Item::Assistant {
                    text: "héllo".into()
                },
                Item::ToolCall {
                    id: "a".into(),
                    name: "one".into(),
                    arguments: "{\"x\":1}".into()
                },
                Item::ToolCall {
                    id: "b".into(),
                    name: "two".into(),
                    arguments: "{}".into()
                }
            ]
        );
        assert_eq!(
            response.usage,
            Some(Usage {
                input_tokens: 12,
                output_tokens: 5,
                cached_tokens: 4
            })
        );
        assert_eq!(
            received,
            vec![
                StreamEvent::Reasoning("think".into()),
                StreamEvent::Text("héllo".into()),
                StreamEvent::ToolCallStart {
                    id: "a".into(),
                    name: "one".into()
                },
                StreamEvent::ToolCallStart {
                    id: "b".into(),
                    name: "two".into()
                }
            ]
        );
        let request = requests.recv().await.unwrap().to_lowercase();
        for header in [
            "authorization: bearer old",
            "chatgpt-account-id: account",
            "originator: nyq",
            "openai-beta: responses=experimental",
            "accept: text/event-stream",
        ] {
            assert!(request.contains(header));
        }
        assert!(request.contains(&format!("session_id: {}", provider.session_id)));
        task.await.unwrap();
    }

    #[tokio::test]
    async fn native_web_search() {
        let _lock = ENV.lock().unwrap();
        let _env = Env;
        let dir = tempfile::tempdir().unwrap();
        let provider = CodexProvider::new(auth_file(&dir, i64::MAX), "test".into(), None);
        let tools = vec![
            ToolSpec { name: "web_search".into(), description: "ddg".into(), parameters: json!({"type":"object"}) },
            ToolSpec { name: "web_fetch".into(), description: "fetch".into(), parameters: json!({"type":"object"}) },
        ];
        let req = || Request { system: "", items: &[], tools: &tools };
        let body = provider.body(req());
        assert_eq!(body["tools"][0], json!({"type":"web_search"}));
        assert_eq!(body["tools"][1]["name"], "web_fetch");

        let events = events(&[
            json!({"type":"response.output_item.added","output_index":0,"item":{"type":"web_search_call","id":"ws_1","status":"in_progress"}}),
            json!({"type":"response.web_search_call.searching","output_index":0,"item_id":"ws_1"}),
            json!({"type":"response.output_item.done","output_index":0,"item":{"type":"web_search_call","id":"ws_1","status":"completed","action":{"type":"search","query":"rust","sources":[]},"extra":1}}),
            // A search only reported on completion still shows activity once.
            json!({"type":"response.output_item.done","output_index":1,"item":{"type":"web_search_call","id":"ws_2","status":"completed","action":{"type":"search","query":"more"}}}),
            json!({"type":"response.output_item.done","output_index":2,"item":{"type":"web_search_call","id":"ws_3","status":"completed","action":{"type":"open_page","url":"https://x.org"}}}),
            json!({"type":"response.output_item.done","output_index":3,"item":{"type":"message","content":[{"type":"output_text","text":"found it"}]}}),
            completed(),
        ]);
        let (url, mut requests, task) = server(vec![(200, events)]).await;
        Env::set("NYQ_CODEX_URL", &url);
        let mut received = Vec::new();
        let response = provider.stream(req(), &mut |e| received.push(e)).await.unwrap();
        assert_eq!(response.items, vec![Item::Assistant { text: "found it".into() }]);
        let start = |id: &str| StreamEvent::ToolCallStart { id: id.into(), name: "web_search".into() };
        assert_eq!(received, vec![start("ws_1"), start("ws_2")]);
        let request = requests.recv().await.unwrap();
        assert!(request.contains(r#"{"type":"web_search"}"#));
        task.await.unwrap();
    }

    #[tokio::test]
    async fn errors() {
        let _lock = ENV.lock().unwrap();
        let _env = Env;
        let dir = tempfile::tempdir().unwrap();
        let provider = CodexProvider::new(auth_file(&dir, i64::MAX), "test".into(), None);
        let (url, _requests, task) = server(vec![
            (429, "usage limit reached".into()),
            (200, events(&[json!({"type":"response.failed","response":{"error":{"message":"model failed"}}})])),
            (200, events(&[json!({"type":"error","message":"bad request"})])),
            (200, events(&[json!({"type":"response.output_text.delta","delta":"partial"})])),
        ]).await;
        Env::set("NYQ_CODEX_URL", &url);
        for expected in [
            "429",
            "model failed",
            "bad request",
            "before response.completed",
        ] {
            let error = provider
                .stream(
                    Request {
                        system: "",
                        items: &[],
                        tools: &[],
                    },
                    &mut |_| {},
                )
                .await
                .unwrap_err()
                .to_string();
            assert!(error.contains(expected), "{error}");
            if expected == "429" {
                assert!(error.contains("usage limit reached"));
            }
        }
        task.await.unwrap();
    }

    #[tokio::test]
    async fn refresh_on_401_and_cache() {
        let _lock = ENV.lock().unwrap();
        let _env = Env;
        let dir = tempfile::tempdir().unwrap();
        let path = auth_file(&dir, i64::MAX);
        let provider = CodexProvider::new(path.clone(), "test".into(), None);
        let (url, mut requests, task) = server(vec![
            (401, "expired".into()),
            (200, events(&[completed()])),
            (200, events(&[completed()])),
        ])
        .await;
        let (token_url, mut token_requests, token_task) = server(vec![(
            200,
            json!({"access_token":"new","refresh_token":"rotated","expires_in":3600}).to_string(),
        )])
        .await;
        Env::set("NYQ_CODEX_URL", &url);
        Env::set("NYQ_CODEX_TOKEN_URL", &token_url);
        for _ in 0..2 {
            provider
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
        }
        let old = requests.recv().await.unwrap();
        let new = requests.recv().await.unwrap();
        let cached = requests.recv().await.unwrap();
        assert!(old.contains("Bearer old"));
        assert!(new.contains("Bearer new") && cached.contains("Bearer new"));
        for request in [&old, &new, &cached] {
            assert!(request.contains(&provider.session_id));
        }
        let token_request = token_requests.recv().await.unwrap();
        let body: Value =
            serde_json::from_str(token_request.split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(
            body,
            json!({"client_id":"app_EMoamEEZ73f0CkXaXp7hrann","grant_type":"refresh_token","refresh_token":"refresh"})
        );
        let saved: Value = serde_json::from_slice(&std::fs::read(path.clone()).unwrap()).unwrap();
        assert_eq!(saved["access_token"], "new");
        assert_eq!(saved["refresh_token"], "rotated");
        assert_eq!(saved["account_id"], "account");
        assert_eq!(saved["version"], 1);
        assert!(saved["expires_at_ms"].as_i64().unwrap() > chrono::Utc::now().timestamp_millis());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        task.await.unwrap();
        token_task.await.unwrap();
    }

    #[tokio::test]
    async fn refresh_before_request() {
        let _lock = ENV.lock().unwrap();
        let _env = Env;
        let dir = tempfile::tempdir().unwrap();
        let path = auth_file(&dir, chrono::Utc::now().timestamp_millis() + 30_000);
        let provider = CodexProvider::new(path.clone(), "test".into(), None);
        let (url, mut requests, task) = server(vec![(200, events(&[completed()]))]).await;
        let (token_url, _token_requests, token_task) = server(vec![(
            200,
            json!({"access_token":"new","expires_in":3600}).to_string(),
        )])
        .await;
        Env::set("NYQ_CODEX_URL", &url);
        Env::set("NYQ_CODEX_TOKEN_URL", &token_url);
        provider
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
        assert!(requests.recv().await.unwrap().contains("Bearer new"));
        let saved: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(saved["refresh_token"], "refresh");
        task.await.unwrap();
        token_task.await.unwrap();
    }

    fn jwt(exp: &str) -> String {
        use base64::Engine;
        let enc = |s: &str| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(s.as_bytes());
        format!("{}.{}.x", enc(r#"{"alg":"none"}"#), enc(&format!(r#"{{"exp":{exp}}}"#)))
    }

    #[test]
    fn jwt_exp_ms_rejects_overflow() {
        assert_eq!(jwt_exp_ms(&jwt("1700000000")), Some(1_700_000_000_000));
        assert_eq!(jwt_exp_ms(&jwt(&i64::MAX.to_string())), None);
        assert_eq!(jwt_exp_ms(&jwt(&i64::MIN.to_string())), None);
        assert_eq!(jwt_exp_ms(&jwt("9e18")), None);
        assert_eq!(jwt_exp_ms("not-a-jwt"), None);
    }

    #[tokio::test]
    async fn fork_shares_rotated_refresh_token() {
        let _lock = ENV.lock().unwrap();
        let _env = Env;
        let dir = tempfile::tempdir().unwrap();
        let path = auth_file(&dir, chrono::Utc::now().timestamp_millis() + 30_000);
        let parent = CodexProvider::new(path.clone(), "test".into(), None);
        let child = parent.fork();
        let (url, mut requests, task) = server(vec![
            (200, events(&[completed()])),
            (200, events(&[completed()])),
        ])
        .await;
        let (token_url, mut token_requests, token_task) = server(vec![(
            200,
            json!({"access_token":"new","refresh_token":"rotated","expires_in":3600}).to_string(),
        )])
        .await;
        Env::set("NYQ_CODEX_URL", &url);
        Env::set("NYQ_CODEX_TOKEN_URL", &token_url);
        let req = || Request { system: "", items: &[], tools: &[] };
        child.stream(req(), &mut |_| {}).await.unwrap();
        parent.stream(req(), &mut |_| {}).await.unwrap();
        assert!(requests.recv().await.unwrap().contains("Bearer new"));
        assert!(requests.recv().await.unwrap().contains("Bearer new"));
        let token_request = token_requests.recv().await.unwrap();
        assert!(token_request.contains("\"refresh_token\":\"refresh\""), "{token_request}");
        assert!(token_requests.try_recv().is_err());
        let saved: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(saved["refresh_token"], "rotated");
        assert_eq!(saved["access_token"], "new");
        task.await.unwrap();
        token_task.await.unwrap();
    }

    #[tokio::test]
    async fn stale_cache_rereads_rotated_auth_file() {
        let _lock = ENV.lock().unwrap();
        let _env = Env;
        Env::set("NYQ_CODEX_TOKEN_URL", "http://127.0.0.1:1");
        let dir = tempfile::tempdir().unwrap();
        let path = auth_file(&dir, i64::MAX);
        let provider = CodexProvider::new(path.clone(), "test".into(), None);
        assert_eq!(provider.token(None).await.unwrap().access_token, "old");
        std::fs::write(
            &path,
            json!({
                "version": 1,
                "access_token": "fresh",
                "refresh_token": "fresh-refresh",
                "expires_at_ms": i64::MAX,
                "account_id": "account"
            })
            .to_string(),
        )
        .unwrap();
        let got = provider.token(Some("old")).await.unwrap();
        assert_eq!(got.access_token, "fresh");
        assert_eq!(got.refresh_token, "fresh-refresh");
    }
}

