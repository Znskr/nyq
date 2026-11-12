//! External agents over ACP (design §8.1): `subagent` with
//! `mode: "external:<name>"` launches `agents.external.<name>` from the
//! user's settings.toml as an Agent Client Protocol subprocess, sends one
//! prompt, collects the final message and routes the agent's permission
//! requests to the parent's approver.
//!
//! ```toml
//! [agents.external.claude]
//! command = "claude-code-acp"
//! args = []
//! env = { SOME_VAR = "1" }
//! ```
//!
//! An external agent runs **outside nyq's sandbox**: it is a separate program
//! with its own tools and its own permission system. nyq only sees what it
//! reports over ACP. Its `session/request_permission` requests are answered
//! by the parent's approver (allow once or reject; never "always"), or
//! allowed automatically when the parent runs in yolo mode, and denied when
//! nobody can be asked. The client advertises no file-system or terminal
//! capabilities, so the agent uses its own. Only the user's own settings.toml
//! may name a command: a repository's `.nyq/settings.toml` cannot.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_client_protocol as sdk;
use sdk::schema::v1 as acp;
use sdk::schema::ProtocolVersion;
use sdk::{ConnectTo, ConnectionTo, Responder};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::agent::{Approval, ApprovalRequest, Approver};

/// Collected message text is capped at this many bytes.
const MAX_TEXT: usize = 1024 * 1024;
/// Permission previews (the tool call's raw input) are cut to this length.
const MAX_PREVIEW: usize = 4 * 1024;
/// How long a cancelled agent gets to answer `session/cancel`.
const CANCEL_GRACE: Duration = Duration::from_secs(2);
/// The tail of the agent's stderr kept for error messages.
const MAX_STDERR: usize = 2 * 1024;

/// `[agents.external.<name>]` in the user's settings.toml.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Config {
    pub command: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
}

/// The named external agent from the user's own settings (never a project
/// layer: a repository must not be able to choose a command to launch).
pub fn config(name: &str) -> anyhow::Result<Config> {
    let (settings, warning) = crate::settings::load(&crate::config::home());
    if let Some(w) = warning {
        anyhow::bail!("external agent `{name}`: {w}");
    }
    config_from(&settings.extra, name)
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// [`config`] over the user file's keys that nyq doesn't manage itself.
fn config_from(extra: &serde_json::Map<String, Value>, name: &str) -> anyhow::Result<Config> {
    if !valid_name(name) {
        anyhow::bail!("invalid external agent name `{name}` (use letters, digits, '-' and '_')");
    }
    let table = extra
        .get("agents")
        .and_then(|a| a.get("external"))
        .and_then(Value::as_object);
    let Some(entry) = table.and_then(|t| t.get(name)) else {
        let known: Vec<&str> = table
            .map(|t| t.keys().map(String::as_str).collect())
            .unwrap_or_default();
        let hint = if known.is_empty() {
            "none are configured".to_string()
        } else {
            format!("configured: {}", known.join(", "))
        };
        anyhow::bail!(
            "no external agent `{name}` in your settings.toml ({hint}). Add \
             [agents.external.{name}] with command = \"...\" and optional args = [...]."
        );
    };
    let Some(entry) = entry.as_object() else {
        anyhow::bail!("[agents.external.{name}] must be a table");
    };
    let command = entry
        .get("command")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!("[agents.external.{name}] needs a non-empty string `command`")
        })?
        .to_string();
    let args = match entry.get("args") {
        None => Vec::new(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| {
                v.as_str().map(str::to_string).ok_or_else(|| {
                    anyhow::anyhow!("[agents.external.{name}] `args` must be strings")
                })
            })
            .collect::<anyhow::Result<_>>()?,
        Some(_) => anyhow::bail!("[agents.external.{name}] `args` must be a list of strings"),
    };
    let env = match entry.get("env") {
        None => BTreeMap::new(),
        Some(Value::Object(map)) => map
            .iter()
            .map(|(k, v)| {
                v.as_str()
                    .map(|v| (k.clone(), v.to_string()))
                    .ok_or_else(|| {
                        anyhow::anyhow!("[agents.external.{name}] `env` values must be strings")
                    })
            })
            .collect::<anyhow::Result<_>>()?,
        Some(_) => anyhow::bail!("[agents.external.{name}] `env` must be a table of strings"),
    };
    Ok(Config { command, args, env })
}

/// Called with (title, status) of each tool call an external agent reports.
pub type ToolObserver = Box<dyn Fn(&str, &str) + Send + Sync>;

/// One prompt to an external agent.
pub struct Run<'a> {
    pub task: &'a str,
    /// The session cwd sent in `session/new` and the process's cwd.
    pub cwd: &'a Path,
    /// Who answers the agent's `session/request_permission`; None denies.
    pub approver: Option<Arc<dyn Approver>>,
    /// Allow every permission request without asking (parent in yolo mode).
    pub auto_approve: bool,
    /// Shown with approval prompts, e.g. "external claude: fix the parser".
    pub origin: String,
    pub cancel: CancellationToken,
    pub timeout: Duration,
    /// Called for each tool call the agent reports (title, kind/status).
    pub on_tool: Option<ToolObserver>,
}

/// What came back from one prompt.
#[derive(Clone, Debug, Default)]
pub struct Outcome {
    /// The agent's message text (chunks concatenated).
    pub text: String,
    /// The `PromptResponse` stop reason, e.g. `end_turn`.
    pub stop_reason: String,
    /// (title, status) of each tool call the agent reported.
    pub tool_calls: Vec<(String, String)>,
}

/// Launch `cfg` and run one prompt.
pub async fn run(cfg: &Config, run: Run<'_>) -> anyhow::Result<Outcome> {
    use tokio::io::AsyncBufReadExt;
    use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
    let mut child = tokio::process::Command::new(&cfg.command)
        .args(&cfg.args)
        .envs(&cfg.env)
        .current_dir(run.cwd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| anyhow::anyhow!("could not start `{}`: {e}", cfg.command))?;
    let stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr_tail = Arc::new(Mutex::new(String::new()));
    if let Some(stderr) = child.stderr.take() {
        let tail = stderr_tail.clone();
        tokio::spawn(async move {
            let mut lines = tokio::io::BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                crate::trace::log("external-agent", || line.clone());
                let mut t = lock(&tail);
                t.push_str(&line);
                t.push('\n');
                if t.len() > MAX_STDERR {
                    let mut cut = t.len() - MAX_STDERR;
                    while !t.is_char_boundary(cut) {
                        cut += 1;
                    }
                    t.drain(..cut);
                }
            }
        });
    }
    let transport = sdk::ByteStreams::new(stdin.compat_write(), stdout.compat());
    let result = run_with_transport(transport, run).await;
    // Dropping the handle kills an agent that is still running.
    let _ = child.start_kill();
    let _ = tokio::time::timeout(Duration::from_secs(1), child.wait()).await;
    result.map_err(|e| {
        let tail = lock(&stderr_tail).trim().to_string();
        if tail.is_empty() {
            e
        } else {
            e.context(format!("agent stderr:\n{tail}"))
        }
    })
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// What the notification handler has collected so far.
#[derive(Default)]
struct Collected {
    /// Every message chunk.
    all: String,
    /// Message text since the last tool call.
    last: String,
    truncated: bool,
    /// Tool calls by id: (title, status), in report order.
    calls: Vec<(String, String, String)>,
}

impl Collected {
    fn push_text(&mut self, text: &str) {
        if self.all.len() + text.len() > MAX_TEXT {
            self.truncated = true;
            return;
        }
        self.all.push_str(text);
        self.last.push_str(text);
    }

    fn title_of(&self, id: &str) -> Option<String> {
        self.calls
            .iter()
            .find(|(i, ..)| i == id)
            .map(|(_, t, _)| t.clone())
    }

    fn text(&self) -> String {
        let mut text = if self.last.trim().is_empty() {
            self.all.trim().to_string()
        } else {
            self.last.trim().to_string()
        };
        if self.truncated {
            text.push_str("\n[… agent output truncated at 1 MiB]");
        }
        text
    }
}

/// Snake-case name of a serde enum value (statuses, stop reasons).
fn snake<T: serde::Serialize>(v: &T) -> String {
    match serde_json::to_value(v) {
        Ok(Value::String(s)) => s,
        _ => "unknown".into(),
    }
}

type OnTool = Arc<dyn Fn(&str, &str) + Send + Sync>;

fn on_update(state: &Mutex<Collected>, on_tool: Option<&OnTool>, update: acp::SessionUpdate) {
    match update {
        acp::SessionUpdate::AgentMessageChunk(chunk) => {
            if let acp::ContentBlock::Text(t) = chunk.content {
                lock(state).push_text(&t.text);
            }
        }
        acp::SessionUpdate::ToolCall(call) => {
            let status = snake(&call.status);
            {
                let mut s = lock(state);
                s.last.clear();
                s.calls.push((
                    call.tool_call_id.0.to_string(),
                    call.title.clone(),
                    status.clone(),
                ));
            }
            if let Some(f) = on_tool {
                f(&call.title, &status);
            }
        }
        acp::SessionUpdate::ToolCallUpdate(update) => {
            let id = update.tool_call_id.0.to_string();
            let status = update.fields.status.as_ref().map(snake);
            let report = {
                let mut s = lock(state);
                match s.calls.iter_mut().find(|(i, ..)| *i == id) {
                    Some(entry) => {
                        if let Some(title) = &update.fields.title {
                            entry.1 = title.clone();
                        }
                        if let Some(status) = &status {
                            entry.2 = status.clone();
                        }
                        status.map(|st| (entry.1.clone(), st))
                    }
                    None => {
                        let title = update.fields.title.clone().unwrap_or_else(|| id.clone());
                        let st = status.unwrap_or_else(|| "pending".into());
                        s.last.clear();
                        s.calls.push((id, title.clone(), st.clone()));
                        Some((title, st))
                    }
                }
            };
            if let (Some(f), Some((title, status))) = (on_tool, report) {
                f(&title, &status);
            }
        }
        _ => {}
    }
}

/// One external permission prompt at a time.
static APPROVALS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn pick(
    options: &[acp::PermissionOption],
    kind: acp::PermissionOptionKind,
) -> Option<acp::PermissionOptionId> {
    options
        .iter()
        .find(|o| o.kind == kind)
        .map(|o| o.option_id.clone())
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[… {} more bytes]", &s[..end], s.len() - end)
}

/// How to answer one `session/request_permission`.
struct Permissions {
    approver: Option<Arc<dyn Approver>>,
    auto_approve: bool,
    origin: String,
    state: Arc<Mutex<Collected>>,
}

impl Permissions {
    async fn decide(&self, req: &acp::RequestPermissionRequest) -> acp::RequestPermissionOutcome {
        let allow = || {
            pick(&req.options, acp::PermissionOptionKind::AllowOnce)
                .or_else(|| pick(&req.options, acp::PermissionOptionKind::AllowAlways))
        };
        let reject = || {
            pick(&req.options, acp::PermissionOptionKind::RejectOnce)
                .or_else(|| pick(&req.options, acp::PermissionOptionKind::RejectAlways))
        };
        let selected = |id: Option<acp::PermissionOptionId>| match id {
            Some(id) => {
                acp::RequestPermissionOutcome::Selected(acp::SelectedPermissionOutcome::new(id))
            }
            None => acp::RequestPermissionOutcome::Cancelled,
        };
        if self.auto_approve {
            return selected(allow());
        }
        let Some(approver) = &self.approver else {
            return selected(reject());
        };
        let id = req.tool_call.tool_call_id.0.to_string();
        let title = req
            .tool_call
            .fields
            .title
            .clone()
            .or_else(|| lock(&self.state).title_of(&id))
            .unwrap_or_else(|| "external agent".into());
        let summary = req
            .tool_call
            .fields
            .kind
            .as_ref()
            .map(snake)
            .unwrap_or_default();
        let preview = req.tool_call.fields.raw_input.as_ref().map(|v| {
            truncate(
                &serde_json::to_string_pretty(v).unwrap_or_default(),
                MAX_PREVIEW,
            )
        });
        let request = ApprovalRequest {
            tool: &title,
            summary: &summary,
            preview: preview.as_deref(),
            rule: None,
            save_rule: None,
        };
        let answer = {
            let _turn = APPROVALS.lock().await;
            approver.approve_noted(&self.origin, &request).await.0
        };
        match answer {
            // Never "always" for an external process: each call asks again.
            Approval::Yes | Approval::Always | Approval::AlwaysSave | Approval::AlwaysSaveLocal => {
                selected(allow())
            }
            Approval::No => selected(reject()),
        }
    }
}

/// How the prompt phase ended.
enum End {
    Done(acp::StopReason),
    Cancelled,
    TimedOut,
}

/// [`run`] over an already connected transport (tests use in-memory pipes).
pub async fn run_with_transport(
    transport: impl ConnectTo<sdk::Client> + 'static,
    run: Run<'_>,
) -> anyhow::Result<Outcome> {
    let state = Arc::new(Mutex::new(Collected::default()));
    let on_tool: Option<OnTool> = run.on_tool.map(Arc::from);
    let permissions = Arc::new(Permissions {
        approver: run.approver.clone(),
        auto_approve: run.auto_approve,
        origin: run.origin.clone(),
        state: state.clone(),
    });
    let (s1, t1, p1) = (state.clone(), on_tool.clone(), permissions.clone());
    let task = run.task.to_string();
    let cwd = run.cwd.to_path_buf();
    let cancel = run.cancel.clone();
    let timeout = run.timeout;
    let deadline = tokio::time::Instant::now() + timeout;

    let ended = sdk::Client
        .builder()
        .name("nyq")
        .on_receive_notification(
            async move |n: acp::SessionNotification, _cx: ConnectionTo<sdk::Agent>| {
                on_update(&s1, t1.as_ref(), n.update);
                Ok(())
            },
            sdk::on_receive_notification!(),
        )
        .on_receive_request(
            async move |req: acp::RequestPermissionRequest,
                        responder: Responder<acp::RequestPermissionResponse>,
                        _cx: ConnectionTo<sdk::Agent>| {
                // Answered off the dispatch loop: asking the user takes a while.
                let p = p1.clone();
                tokio::spawn(async move {
                    let outcome = p.decide(&req).await;
                    let _ = responder.respond(acp::RequestPermissionResponse::new(outcome));
                });
                Ok(())
            },
            sdk::on_receive_request!(),
        )
        .connect_with(transport, async move |cx: ConnectionTo<sdk::Agent>| {
            let setup = async {
                cx.send_request(
                    acp::InitializeRequest::new(ProtocolVersion::V1)
                        .client_info(acp::Implementation::new("nyq", env!("CARGO_PKG_VERSION"))),
                )
                .block_task()
                .await?;
                let session = cx
                    .send_request(acp::NewSessionRequest::new(cwd))
                    .block_task()
                    .await?;
                Ok::<_, sdk::Error>(session.session_id)
            };
            let session_id = tokio::select! {
                r = setup => r?,
                _ = cancel.cancelled() => return Ok(End::Cancelled),
                _ = tokio::time::sleep_until(deadline) => return Ok(End::TimedOut),
            };
            let prompt = cx
                .send_request(acp::PromptRequest::new(
                    session_id.clone(),
                    vec![acp::ContentBlock::Text(acp::TextContent::new(task))],
                ))
                .block_task();
            tokio::pin!(prompt);
            let end = tokio::select! {
                r = &mut prompt => return Ok(End::Done(r?.stop_reason)),
                _ = cancel.cancelled() => End::Cancelled,
                _ = tokio::time::sleep_until(deadline) => End::TimedOut,
            };
            // Ask the agent to stop; it should answer the prompt `cancelled`.
            let _ = cx.send_notification(acp::CancelNotification::new(session_id));
            let _ = tokio::time::timeout(CANCEL_GRACE, &mut prompt).await;
            Ok(end)
        })
        .await
        .map_err(|e| anyhow::anyhow!("external agent: {e}"))?;

    let collected = lock(&state);
    match ended {
        End::Cancelled => anyhow::bail!("external agent cancelled"),
        End::TimedOut => anyhow::bail!("external agent timed out after {}s", timeout.as_secs()),
        End::Done(reason) => Ok(Outcome {
            text: collected.text(),
            stop_reason: snake(&reason),
            tool_calls: collected
                .calls
                .iter()
                .map(|(_, title, status)| (title.clone(), status.clone()))
                .collect(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

    /// Records what it was asked and answers `answer`.
    struct Recorder {
        answer: Approval,
        seen: Mutex<Vec<(String, String, Option<String>)>>,
    }

    #[async_trait::async_trait]
    impl Approver for Recorder {
        async fn approve(&self, origin: &str, req: &ApprovalRequest<'_>) -> Approval {
            lock(&self.seen).push((
                origin.to_string(),
                req.tool.to_string(),
                req.preview.map(str::to_string),
            ));
            self.answer
        }
    }

    /// An in-process ACP agent: on prompt it reports a tool call, asks for
    /// permission, then says "allowed" or "denied". With `hang` it never
    /// answers the prompt until cancelled.
    fn fake_agent(hang: bool) -> impl ConnectTo<sdk::Client> + 'static {
        let (client_io, agent_io) = tokio::io::duplex(64 * 1024);
        let (cr, cw) = tokio::io::split(client_io);
        let (ar, aw) = tokio::io::split(agent_io);
        let cancelled = Arc::new(tokio::sync::Notify::new());
        let c2 = cancelled.clone();
        tokio::spawn(async move {
            let _ = sdk::Agent
                .builder()
                .name("fake")
                .on_receive_request(
                    async move |req: acp::InitializeRequest,
                                responder: Responder<acp::InitializeResponse>,
                                _cx: ConnectionTo<sdk::Client>| {
                        responder.respond(acp::InitializeResponse::new(req.protocol_version))
                    },
                    sdk::on_receive_request!(),
                )
                .on_receive_request(
                    async move |req: acp::NewSessionRequest,
                                responder: Responder<acp::NewSessionResponse>,
                                _cx: ConnectionTo<sdk::Client>| {
                        assert!(req.cwd.is_absolute());
                        responder.respond(acp::NewSessionResponse::new(acp::SessionId::new("s1")))
                    },
                    sdk::on_receive_request!(),
                )
                .on_receive_notification(
                    async move |_n: acp::CancelNotification, _cx: ConnectionTo<sdk::Client>| {
                        c2.notify_one();
                        Ok(())
                    },
                    sdk::on_receive_notification!(),
                )
                .on_receive_request(
                    async move |req: acp::PromptRequest,
                                responder: Responder<acp::PromptResponse>,
                                cx: ConnectionTo<sdk::Client>| {
                        let cancelled = cancelled.clone();
                        tokio::spawn(async move {
                            let sid = req.session_id.clone();
                            let say = |text: &str| {
                                let _ = cx.send_notification(acp::SessionNotification::new(
                                    sid.clone(),
                                    acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
                                        text.to_string().into(),
                                    )),
                                ));
                            };
                            if hang {
                                cancelled.notified().await;
                                let _ = responder
                                    .respond(acp::PromptResponse::new(acp::StopReason::Cancelled));
                                return;
                            }
                            say("Let me edit the file.");
                            let _ = cx.send_notification(acp::SessionNotification::new(
                                sid.clone(),
                                acp::SessionUpdate::ToolCall(
                                    acp::ToolCall::new("t1", "Edit src/lib.rs")
                                        .status(acp::ToolCallStatus::Pending),
                                ),
                            ));
                            let ask = acp::RequestPermissionRequest::new(
                                sid.clone(),
                                acp::ToolCallUpdate::new(
                                    "t1",
                                    acp::ToolCallUpdateFields::new()
                                        .raw_input(serde_json::json!({"path": "src/lib.rs"})),
                                ),
                                vec![
                                    acp::PermissionOption::new(
                                        "always",
                                        "Always",
                                        acp::PermissionOptionKind::AllowAlways,
                                    ),
                                    acp::PermissionOption::new(
                                        "once",
                                        "Allow",
                                        acp::PermissionOptionKind::AllowOnce,
                                    ),
                                    acp::PermissionOption::new(
                                        "no",
                                        "Reject",
                                        acp::PermissionOptionKind::RejectOnce,
                                    ),
                                ],
                            );
                            let allowed = match cx.send_request(ask).block_task().await {
                                Ok(r) => match r.outcome {
                                    acp::RequestPermissionOutcome::Selected(s) => {
                                        // "always" must never be chosen for us.
                                        &*s.option_id.0 == "once"
                                    }
                                    _ => false,
                                },
                                Err(_) => false,
                            };
                            let status = if allowed {
                                acp::ToolCallStatus::Completed
                            } else {
                                acp::ToolCallStatus::Failed
                            };
                            let _ = cx.send_notification(acp::SessionNotification::new(
                                sid.clone(),
                                acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
                                    "t1",
                                    acp::ToolCallUpdateFields::new().status(status),
                                )),
                            ));
                            say(if allowed {
                                "Edit allowed"
                            } else {
                                "Edit denied"
                            });
                            say(" and done.");
                            let _ = responder
                                .respond(acp::PromptResponse::new(acp::StopReason::EndTurn));
                        });
                        Ok(())
                    },
                    sdk::on_receive_request!(),
                )
                .connect_to(sdk::ByteStreams::new(aw.compat_write(), ar.compat()))
                .await;
        });
        sdk::ByteStreams::new(cw.compat_write(), cr.compat())
    }

    fn base(cwd: &Path) -> Run<'_> {
        Run {
            task: "fix it",
            cwd,
            approver: None,
            auto_approve: false,
            origin: "external fake: fix it".into(),
            cancel: CancellationToken::new(),
            timeout: Duration::from_secs(10),
            on_tool: None,
        }
    }

    #[tokio::test]
    async fn approver_allows_and_sees_origin_and_title() {
        let dir = tempfile::tempdir().unwrap();
        let approver = Arc::new(Recorder {
            answer: Approval::Always,
            seen: Mutex::new(Vec::new()),
        });
        let tools = Arc::new(Mutex::new(Vec::new()));
        let t2 = tools.clone();
        let mut run = base(dir.path());
        run.approver = Some(approver.clone());
        run.on_tool = Some(Box::new(move |title, status| {
            lock(&t2).push(format!("{title}:{status}"));
        }));
        let out = run_with_transport(fake_agent(false), run).await.unwrap();
        // Only the text after the last tool call is the report.
        assert_eq!(out.text, "Edit allowed and done.");
        assert_eq!(out.stop_reason, "end_turn");
        assert_eq!(
            out.tool_calls,
            vec![("Edit src/lib.rs".to_string(), "completed".to_string())]
        );
        let seen = lock(&approver.seen);
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].0, "external fake: fix it");
        assert_eq!(seen[0].1, "Edit src/lib.rs");
        assert!(seen[0].2.as_deref().unwrap().contains("src/lib.rs"));
        assert_eq!(
            *lock(&tools),
            vec!["Edit src/lib.rs:pending", "Edit src/lib.rs:completed"]
        );
    }

    #[tokio::test]
    async fn without_approver_permission_is_denied() {
        let dir = tempfile::tempdir().unwrap();
        let out = run_with_transport(fake_agent(false), base(dir.path()))
            .await
            .unwrap();
        assert_eq!(out.text, "Edit denied and done.");
        assert_eq!(out.tool_calls[0].1, "failed");
    }

    #[tokio::test]
    async fn approver_no_rejects() {
        let dir = tempfile::tempdir().unwrap();
        let approver = Arc::new(Recorder {
            answer: Approval::No,
            seen: Mutex::new(Vec::new()),
        });
        let mut run = base(dir.path());
        run.approver = Some(approver.clone());
        let out = run_with_transport(fake_agent(false), run).await.unwrap();
        assert!(out.text.contains("denied"));
        assert_eq!(lock(&approver.seen).len(), 1);
    }

    #[tokio::test]
    async fn auto_approve_allows_without_asking() {
        let dir = tempfile::tempdir().unwrap();
        let approver = Arc::new(Recorder {
            answer: Approval::No,
            seen: Mutex::new(Vec::new()),
        });
        let mut run = base(dir.path());
        run.approver = Some(approver.clone());
        run.auto_approve = true;
        let out = run_with_transport(fake_agent(false), run).await.unwrap();
        assert!(out.text.contains("allowed"), "{}", out.text);
        assert!(lock(&approver.seen).is_empty());
    }

    #[tokio::test]
    async fn cancel_and_timeout_stop_a_hanging_agent() {
        let dir = tempfile::tempdir().unwrap();
        let mut run = base(dir.path());
        let cancel = run.cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            cancel.cancel();
        });
        run.timeout = Duration::from_secs(30);
        let err = run_with_transport(fake_agent(true), run).await.unwrap_err();
        assert!(err.to_string().contains("cancelled"), "{err}");

        let mut run = base(dir.path());
        run.timeout = Duration::from_millis(200);
        let err = run_with_transport(fake_agent(true), run).await.unwrap_err();
        assert!(err.to_string().contains("timed out"), "{err}");
    }

    #[tokio::test]
    async fn missing_command_fails_to_start() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config {
            command: "nyq-no-such-external-agent-binary".into(),
            ..Default::default()
        };
        let err = super::run(&cfg, base(dir.path())).await.unwrap_err();
        assert!(err.to_string().contains("could not start"), "{err}");
    }

    #[test]
    fn config_parsing() {
        let extra = |v: Value| v.as_object().unwrap().clone();
        let good = extra(serde_json::json!({
            "agents": {"external": {
                "claude": {"command": "claude-acp", "args": ["--x"], "env": {"A": "1"}},
                "bare": {"command": "nyq", "args": ["acp"]},
                "empty": {"command": "  "},
                "badargs": {"command": "x", "args": [1]},
            }}
        }));
        let c = config_from(&good, "claude").unwrap();
        assert_eq!(c.command, "claude-acp");
        assert_eq!(c.args, vec!["--x"]);
        assert_eq!(c.env.get("A").map(String::as_str), Some("1"));
        assert_eq!(config_from(&good, "bare").unwrap().args, vec!["acp"]);
        assert!(config_from(&good, "empty")
            .unwrap_err()
            .to_string()
            .contains("command"));
        assert!(config_from(&good, "badargs").is_err());
        assert!(config_from(&good, "../x")
            .unwrap_err()
            .to_string()
            .contains("invalid"));
        let missing = config_from(&good, "codex").unwrap_err().to_string();
        assert!(
            missing.contains("configured: ") && missing.contains("claude"),
            "{missing}"
        );
        let none = config_from(&serde_json::Map::new(), "codex")
            .unwrap_err()
            .to_string();
        assert!(none.contains("none are configured"), "{none}");
    }
}
