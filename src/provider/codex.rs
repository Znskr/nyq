//! ChatGPT-subscription Codex Responses API provider.

use std::{collections::BTreeMap, path::PathBuf};

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
    auth: Mutex<Option<Auth>>,
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
        let Some(t) = v.get("tokens") else { return Ok(serde_json::from_value(v)?) };
        let field = |k: &str| t[k].as_str().map(str::to_owned).with_context(|| format!("codex auth file missing tokens.{k}"));
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
pub fn jwt_exp_ms(token: &str) -> Option<i64> {
    use base64::Engine;
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    serde_json::from_slice::<Value>(&bytes).ok()?["exp"].as_i64().map(|s| s * 1000)
}

impl CodexProvider {
    pub fn new(auth_path: PathBuf, model: String, effort: Option<String>) -> Self {
        Self {
            auth_path,
            model,
            effort,
            auth: Mutex::new(None),
            session_id: uuid::Uuid::new_v4().to_string(),
            client: reqwest::Client::new(),
        }
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
        let tools: Vec<_> = req.tools.iter().map(|t| json!({"type":"function","name":t.name,"description":t.description,"parameters":t.parameters,"strict":false})).collect();
        let mut body = json!({"model":self.model,"store":false,"stream":true,"instructions":req.system,"input":input,"tools":tools,"tool_choice":"auto","parallel_tool_calls":true,"include":["reasoning.encrypted_content"],"text":{"verbosity":"low"}});
        if let Some(effort) = &self.effort {
            body["reasoning"] =
                json!({"effort":if effort == "minimal" { "low" } else { effort },"summary":"auto"});
        }
        body
    }

    async fn token(&self, rejected: Option<&str>) -> Result<Auth> {
        let mut cached = self.auth.lock().await;
        if cached.is_none() {
            let bytes = tokio::fs::read(&self.auth_path)
                .await
                .context("reading Codex auth file")?;
            *cached = Some(Auth::parse(&bytes).context("parsing Codex auth file")?);
        }
        let auth = cached.as_ref().unwrap();
        let expired =
            auth.expires_at_ms.saturating_sub(60_000) <= chrono::Utc::now().timestamp_millis();
        if !expired && rejected != Some(auth.access_token.as_str()) {
            return Ok(auth.clone());
        }
        anyhow::ensure!(!auth.read_only, "the codex CLI login has expired; run `codex` once to refresh it");
        let url = std::env::var("NYQ_CODEX_TOKEN_URL")
            .unwrap_or_else(|_| "https://auth.openai.com/oauth/token".into());
        let result: Value = sse::checked(self.client.post(url).json(&json!({"client_id":"app_EMoamEEZ73f0CkXaXp7hrann","grant_type":"refresh_token","refresh_token":auth.refresh_token})).send().await?).await?.json().await?;
        let mut refreshed = auth.clone();
        refreshed.access_token = result["access_token"]
            .as_str()
            .context("token response missing access_token")?
            .to_owned();
        if let Some(token) = result["refresh_token"].as_str() {
            refreshed.refresh_token = token.to_owned();
        }
        let seconds = result["expires_in"]
            .as_i64()
            .context("token response missing expires_in")?;
        anyhow::ensure!(seconds > 0, "token response has invalid expires_in");
        refreshed.expires_at_ms = chrono::Utc::now()
            .timestamp_millis()
            .saturating_add(seconds.saturating_mul(1000));
        self.save_auth(&refreshed)?;
        *cached = Some(refreshed.clone());
        Ok(refreshed)
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
            std::fs::rename(&temp, &self.auth_path)?;
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
                .filter(|part| part["type"] == "output_text")
                .filter_map(|part| part["text"].as_str())
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
    fn name(&self) -> &str {
        "codex"
    }
    fn model(&self) -> &str {
        &self.model
    }
    fn set_model(&mut self, model: String) {
        self.model = model;
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
        while let Some(event) = events.next().await {
            let event = event?;
            if event.data.trim() == "[DONE]" {
                break;
            }
            let data: Value = serde_json::from_str(&event.data)?;
            let kind = data["type"].as_str().unwrap_or(&event.event);
            match kind {
                "response.output_text.delta" => {
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

    fn auth_file(dir: &tempfile::TempDir, expires: i64) -> PathBuf {
        let path = dir.path().join("auth.json");
        std::fs::write(&path, json!({"version":1,"access_token":"old","refresh_token":"refresh","expires_at_ms":expires,"account_id":"account"}).to_string()).unwrap();
        path
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
}
