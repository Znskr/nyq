//! User-configurable hooks: shell commands nyq runs at conversation events.
//!
//! Configuration lives in `<home>/hooks.json` (always active) and optionally
//! in `<cwd>/.nyq/hooks.json` (a project's hooks, active only once trusted:
//! `/hooks trust` records the file's path and content hash in
//! `<home>/trusted-hooks.json`; after the file changes it must be trusted
//! again). Each hook is `{event, matcher?, command, timeout_secs?}`; the
//! matcher is a tool-name glob (`bash`, `write_file`, `mcp__*`) and applies
//! to the tool events only.
//!
//! Hooks run through the shell, unsandboxed, in the project directory, with
//! a JSON description of the event on stdin; their stderr is shown to the
//! user. A hook that times out or fails to run never blocks anything — only
//! an explicit decision does:
//!
//! - `PreToolUse` (before validation, permissions and execution): a
//!   non-zero exit, or `{"decision":"block","reason":…}` on stdout, blocks
//!   the call; the reason is told to the model.
//! - `PostToolUse` (after a call ran) observes the call and its output.
//! - `UserPromptSubmit` (before a prompt is sent): `{"decision":"block"}`
//!   cancels the prompt, `{"context":"…"}` adds text to the conversation
//!   before it.
//! - `Stop` (after a turn finishes) observes the final answer.
//! - `SessionStart` (at startup, and on `/new` and `/resume`) observes the
//!   session.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

use crate::tools::ToolOutput;

const DEFAULT_TIMEOUT: u64 = 60;
const MAX_TIMEOUT: u64 = 600;
/// Hook stdout kept for decision parsing.
const MAX_STDOUT: usize = 64 * 1024;
/// Hook stderr kept to show the user.
const MAX_STDERR: usize = 8 * 1024;
/// Text fields of the event payload (prompt, output, answer) are capped.
const MAX_TEXT: usize = 10_000;
/// How long to wait for a finished or killed hook's pipes to close.
const GRACE: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    PreToolUse,
    PostToolUse,
    UserPromptSubmit,
    Stop,
    SessionStart,
}

impl Event {
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "PreToolUse" => Event::PreToolUse,
            "PostToolUse" => Event::PostToolUse,
            "UserPromptSubmit" => Event::UserPromptSubmit,
            "Stop" => Event::Stop,
            "SessionStart" => Event::SessionStart,
            _ => return None,
        })
    }

    fn as_str(self) -> &'static str {
        match self {
            Event::PreToolUse => "PreToolUse",
            Event::PostToolUse => "PostToolUse",
            Event::UserPromptSubmit => "UserPromptSubmit",
            Event::Stop => "Stop",
            Event::SessionStart => "SessionStart",
        }
    }

    fn is_tool_event(self) -> bool {
        matches!(self, Event::PreToolUse | Event::PostToolUse)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Hook {
    pub event: Event,
    pub matcher: Option<String>,
    pub command: String,
    pub timeout_secs: u64,
}

/// What UserPromptSubmit hooks decided about a prompt.
#[derive(Debug, PartialEq)]
pub enum PromptOutcome {
    /// Send it; the strings are context to add to the conversation first.
    Run(Vec<String>),
    /// Do not send it; the string explains why (shown to the user).
    Blocked(String),
}

/// The loaded hooks: the user's file, plus the project's when trusted.
#[derive(Default)]
pub struct Hooks {
    user_path: PathBuf,
    user: Vec<Hook>,
    project_path: PathBuf,
    project: Vec<Hook>,
    /// Content hash of the project file as loaded (what `/hooks` showed).
    project_hash: Option<String>,
    project_trusted: bool,
}

pub fn user_path(home: &Path) -> PathBuf {
    home.join("hooks.json")
}

pub fn project_path(cwd: &Path) -> PathBuf {
    cwd.join(".nyq").join("hooks.json")
}

fn trust_file(home: &Path) -> PathBuf {
    home.join("trusted-hooks.json")
}

impl Hooks {
    /// Read the user's hooks and the project's (trusted or not). Problems
    /// come back as warnings to show once; a malformed file is ignored.
    pub fn load(home: &Path, cwd: &Path) -> (Self, Vec<String>) {
        let mut hooks = Hooks::default();
        let mut warnings = Vec::new();
        let user_file = user_path(home);
        match read(&user_file).and_then(|text| text.map(|t| parse(&t)).transpose()) {
            Ok(None) => {}
            Ok(Some((parsed, warns))) => {
                warnings.extend(warns.iter().map(|w| format!("{}: {w}", user_file.display())));
                hooks.user = parsed;
            }
            Err(e) => warnings.push(format!("ignoring {}: {e}", user_file.display())),
        }
        hooks.user_path = user_file;
        let project_file = project_path(cwd);
        // The workspace may be the profile directory itself.
        if project_file != hooks.user_path {
            match read(&project_file) {
                Ok(None) => {}
                Ok(Some(text)) => {
                    let hash = content_hash(text.as_bytes());
                    match parse(&text) {
                        Ok((parsed, warns)) => {
                            warnings.extend(warns.iter().map(|w| format!("{}: {w}", project_file.display())));
                            // A project's hooks only run once its file is
                            // trusted (path + content hash recorded by
                            // /hooks trust).
                            let trusted = read_trusts(home).is_ok_and(|t| {
                                t.get(&trust_key(&project_file)).is_some_and(|h| h == &hash)
                            });
                            if !trusted {
                                warnings.push(format!(
                                    "project hooks in {} are not trusted and will not run; review them, then run /hooks trust",
                                    project_file.display()
                                ));
                            }
                            hooks.project = parsed;
                            hooks.project_hash = Some(hash);
                            hooks.project_path = project_file;
                            hooks.project_trusted = trusted;
                        }
                        Err(e) => warnings.push(format!("ignoring {}: {e}", project_file.display())),
                    }
                }
                Err(e) => warnings.push(format!("ignoring {}: {e}", project_file.display())),
            }
        }
        (hooks, warnings)
    }

    /// Whether a PostToolUse hook would fire for this tool (lets the loop
    /// avoid cloning a call's arguments when none does).
    pub(crate) fn observes_post_tool_use(&self, tool: &str) -> bool {
        !self.matching(Event::PostToolUse, Some(tool)).is_empty()
    }

    /// The hooks a subagent runs: the tool hooks only, so a user's
    /// PreToolUse guard also covers delegated calls, while turn and session
    /// events stay the parent's.
    pub fn for_subagent(&self) -> Self {
        let tools = |v: &[Hook]| v.iter().filter(|h| h.event.is_tool_event()).cloned().collect();
        Hooks {
            user_path: self.user_path.clone(),
            user: tools(&self.user),
            project_path: self.project_path.clone(),
            project: tools(&self.project),
            project_hash: self.project_hash.clone(),
            project_trusted: self.project_trusted,
        }
    }

    /// `/hooks trust`: record the project's hooks file (path + content
    /// hash), but only the version that was loaded and listed: a file that
    /// changed since must be reviewed again first.
    pub fn trust_project(&self, home: &Path, cwd: &Path) -> Result<PathBuf> {
        let path = project_path(cwd);
        let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        trust_file_content(home, &path, &text, self.project_hash.as_deref())?;
        Ok(path)
    }
}

fn read_trusts(home: &Path) -> Result<BTreeMap<String, String>, String> {
    let path = trust_file(home);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(e) => return Err(e.to_string()),
    };
    if text.trim().is_empty() {
        return Ok(BTreeMap::new());
    }
    let v: Value = serde_json::from_str(&text).map_err(|e| format!("not valid JSON: {e}"))?;
    let Value::Object(map) = v else {
        return Err("expected a JSON object of file path → content hash".into());
    };
    Ok(map.into_iter().filter_map(|(k, v)| v.as_str().map(|h| (k, h.to_string()))).collect())
}

/// The trust-store key for a project hooks file: its canonical path.
fn trust_key(path: &Path) -> String {
    std::fs::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .display()
        .to_string()
}

fn content_hash(bytes: &[u8]) -> String {
    let d = Sha256::digest(bytes);
    d.iter().fold(String::with_capacity(64), |mut s, b| {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// Record `text` (the content of the hooks file at `path`) as trusted when
/// it is valid and is the version that was reviewed (`reviewed` hash).
fn trust_file_content(home: &Path, path: &Path, text: &str, reviewed: Option<&str>) -> Result<()> {
    let (parsed, warnings) = parse(text).map_err(|e| anyhow::anyhow!("{} is malformed ({e}); fix it first", path.display()))?;
    anyhow::ensure!(warnings.is_empty(), "{} has invalid entries ({}); fix them first", path.display(), warnings.join("; "));
    anyhow::ensure!(!parsed.is_empty(), "{} has no hooks", path.display());
    let hash = content_hash(text.as_bytes());
    anyhow::ensure!(
        reviewed == Some(hash.as_str()),
        "{} changed since it was loaded; review it with /hooks, then trust it again",
        path.display()
    );
    let mut trusts = read_trusts(home)
        .map_err(|e| anyhow::anyhow!("{} is malformed ({e}); fix or remove it first", trust_file(home).display()))?;
    trusts.insert(trust_key(path), hash);
    let mut out = serde_json::to_string_pretty(&trusts)?;
    out.push('\n');
    crate::session::write_private(&trust_file(home), out.as_bytes())
        .with_context(|| format!("writing {}", trust_file(home).display()))?;
    Ok(())
}

/// Read one file: `Ok(None)` when it doesn't exist.
fn read(path: &Path) -> Result<Option<String>, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

/// Parse a hooks file: `{"hooks": [ {event, matcher?, command, timeout_secs?} ]}`.
/// Entry-level problems skip just that entry and come back as warnings.
fn parse(text: &str) -> Result<(Vec<Hook>, Vec<String>), String> {
    let v: Value = serde_json::from_str(text).map_err(|e| format!("not valid JSON: {e}"))?;
    let Value::Object(_) = v else {
        return Err("expected a JSON object with a \"hooks\" list".into());
    };
    let Some(Value::Array(entries)) = v.get("hooks") else {
        return match v.get("hooks") {
            None | Some(Value::Null) => Ok((Vec::new(), Vec::new())),
            Some(other) => Err(format!("\"hooks\" must be a list, got {other}")),
        };
    };
    let mut hooks = Vec::new();
    let mut warnings = Vec::new();
    for (i, entry) in entries.iter().enumerate() {
        match parse_hook(entry) {
            Ok(hook) => hooks.push(hook),
            Err(e) => warnings.push(format!("hooks[{i}]: {e}")),
        }
    }
    Ok((hooks, warnings))
}

fn parse_hook(v: &Value) -> Result<Hook, String> {
    let Some(map) = v.as_object() else {
        return Err(format!("expected an object, got {v}"));
    };
    let event = map.get("event").and_then(Value::as_str).and_then(Event::parse).ok_or_else(|| {
        format!(
            "\"event\" must be PreToolUse, PostToolUse, UserPromptSubmit, Stop or SessionStart, got {}",
            map.get("event").unwrap_or(&Value::Null)
        )
    })?;
    let command = map
        .get("command")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .ok_or_else(|| format!("\"command\" must be a non-empty string, got {}", map.get("command").unwrap_or(&Value::Null)))?
        .to_string();
    let matcher = match map.get("matcher") {
        None | Some(Value::Null) => None,
        Some(Value::String(m)) if !m.trim().is_empty() => Some(m.trim().to_string()),
        Some(other) => return Err(format!("\"matcher\" must be a non-empty string, got {other}")),
    };
    let timeout_secs = match map.get("timeout_secs") {
        None | Some(Value::Null) => DEFAULT_TIMEOUT,
        Some(Value::Number(n)) => match n.as_u64().filter(|t| *t >= 1) {
            Some(t) => t.min(MAX_TIMEOUT),
            None => return Err(format!("\"timeout_secs\" must be a positive integer, got {n}")),
        },
        Some(other) => return Err(format!("\"timeout_secs\" must be a positive integer, got {other}")),
    };
    if let Some(m) = &matcher {
        if !event.is_tool_event() {
            return Err(format!("matcher `{m}` applies only to PreToolUse and PostToolUse"));
        }
    }
    Ok(Hook { event, matcher, command, timeout_secs })
}

/// Result of running one hook command.
struct RunOutcome {
    /// The exit status code; `None` when the command could not run, timed
    /// out, or died without one.
    exit: Option<i32>,
    timed_out: bool,
    /// The user cancelled the turn while the hook ran (it was killed).
    cancelled: bool,
    stdout: String,
    stderr: String,
    /// Why the command could not run at all (spawn failure).
    error: Option<String>,
}

/// Run one hook command with the event JSON on stdin, bounded output
/// collection, and a timeout (or cancellation) that kills the whole process
/// group.
async fn run_hook(hook: &Hook, payload: &str, cwd: &Path, cancel: &CancellationToken) -> RunOutcome {
    let failed = |error: String| RunOutcome {
        exit: None,
        timed_out: false,
        cancelled: false,
        stdout: String::new(),
        stderr: String::new(),
        error: Some(error),
    };
    // Hooks are the user's own commands (project hooks only after trust),
    // so they run unsandboxed, like `!cmd`.
    let policy = crate::sandbox::Policy { mode: crate::sandbox::Mode::Off, ..Default::default() };
    let Ok(mut command) = crate::sandbox::shell_command(&policy, cwd, &hook.command) else {
        return failed("could not start a shell".into());
    };
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(e) => return failed(e.to_string()),
    };
    let pid = child.id().unwrap_or(0);
    if let Some(mut stdin) = child.stdin.take() {
        let payload = payload.to_string();
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            let _ = stdin.write_all(payload.as_bytes()).await;
            let _ = stdin.shutdown().await;
        });
    }
    let stdout = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let stderr = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut pumps = Vec::new();
    if let Some(out) = child.stdout.take() {
        pumps.push(tokio::spawn(pump(out, stdout.clone(), MAX_STDOUT)));
    }
    if let Some(err) = child.stderr.take() {
        pumps.push(tokio::spawn(pump(err, stderr.clone(), MAX_STDERR)));
    }

    enum Outcome {
        Exited(std::io::Result<std::process::ExitStatus>),
        TimedOut,
        Cancelled,
    }
    let outcome = tokio::select! {
        status = child.wait() => Outcome::Exited(status),
        _ = tokio::time::sleep(Duration::from_secs(hook.timeout_secs)) => Outcome::TimedOut,
        _ = cancel.cancelled() => Outcome::Cancelled,
    };
    let (exit, timed_out, cancelled) = match outcome {
        Outcome::Exited(Ok(status)) => (status.code(), false, false),
        Outcome::Exited(Err(e)) => {
            kill_child(&mut child, pid).await;
            finish_pumps(pumps).await;
            return failed(format!("waiting for the command: {e}"));
        }
        Outcome::TimedOut => {
            kill_child(&mut child, pid).await;
            (None, true, false)
        }
        Outcome::Cancelled => {
            kill_child(&mut child, pid).await;
            (None, false, true)
        }
    };
    finish_pumps(pumps).await;
    let take = |v: std::sync::Arc<std::sync::Mutex<Vec<u8>>>| {
        v.lock().unwrap_or_else(|e| e.into_inner()).clone()
    };
    RunOutcome {
        exit,
        timed_out,
        cancelled,
        stdout: String::from_utf8_lossy(&take(stdout)).into_owned(),
        stderr: String::from_utf8_lossy(&take(stderr)).into_owned(),
        error: None,
    }
}

async fn pump<R: tokio::io::AsyncRead + Unpin>(mut r: R, sink: std::sync::Arc<std::sync::Mutex<Vec<u8>>>, cap: usize) {
    use tokio::io::AsyncReadExt;
    let mut buf = [0u8; 4096];
    loop {
        match r.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if let Ok(mut v) = sink.lock() {
                    let room = cap.saturating_sub(v.len());
                    if room > 0 {
                        v.extend_from_slice(&buf[..n.min(room)]);
                    }
                }
            }
        }
    }
}

async fn finish_pumps(pumps: Vec<tokio::task::JoinHandle<()>>) {
    let deadline = tokio::time::Instant::now() + GRACE;
    for mut handle in pumps {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if tokio::time::timeout(left, &mut handle).await.is_err() {
            handle.abort();
            let _ = handle.await;
        }
    }
}

/// Kill the hook and everything it spawned, then reap it.
async fn kill_child(child: &mut tokio::process::Child, pid: u32) {
    #[cfg(unix)]
    {
        if let Ok(pgid) = i32::try_from(pid) {
            if pgid > 0 {
                // SAFETY: plain syscall; a negative pid addresses the process group.
                unsafe { libc::kill(-pgid, libc::SIGKILL) };
            }
        }
        let _ = child.start_kill();
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        let _ = child.kill().await;
    }
    let _ = child.wait().await;
}

impl Hooks {
    /// The hooks that fire for an event (and tool name, for the tool
    /// events): the user's first, then the project's when trusted.
    fn matching(&self, event: Event, tool: Option<&str>) -> Vec<&Hook> {
        let hit = |h: &Hook| {
            h.event == event
                && h.matcher.as_deref().is_none_or(|m| tool.is_some_and(|t| crate::permissions::glob(m, t)))
        };
        let mut out: Vec<&Hook> = self.user.iter().filter(|h| hit(h)).collect();
        if self.project_trusted {
            out.extend(self.project.iter().filter(|h| hit(h)));
        }
        out
    }

    /// Run the matching hooks in order, showing stderr and run problems to
    /// the user, until `done` says a result settles the event (or the user
    /// cancels). Returns the settling hook's label and outcome.
    async fn dispatch(
        &self,
        event: Event,
        tool: Option<&str>,
        payload: &Value,
        cwd: &Path,
        cancel: &CancellationToken,
        mut done: impl FnMut(&str, &RunOutcome) -> bool,
    ) {
        let hooks = self.matching(event, tool);
        if hooks.is_empty() {
            return;
        }
        let text = serde_json::to_string(payload).unwrap_or_default();
        for hook in hooks {
            if cancel.is_cancelled() {
                return;
            }
            let r = run_hook(hook, &text, cwd, cancel).await;
            report(hook, &r);
            if done(&cut(&hook.command, 40), &r) {
                return;
            }
        }
    }

    /// PreToolUse, before permissions and execution. Returns the error to
    /// give the model instead of running the call when a hook blocked it
    /// (the first block wins) or the user cancelled while a hook ran.
    pub async fn pre_tool_use(
        &self,
        tool: &str,
        args: &Value,
        cwd: &Path,
        session_id: Option<&str>,
        cancel: &CancellationToken,
    ) -> Option<String> {
        let payload = json!({
            "event": Event::PreToolUse.as_str(),
            "session_id": session_id,
            "cwd": cwd.display().to_string(),
            "tool": tool,
            "args": args,
        });
        let mut blocked = None;
        self.dispatch(Event::PreToolUse, Some(tool), &payload, cwd, cancel, |_, r| {
            blocked = block_reason(r);
            blocked.is_some()
        })
        .await;
        if let Some(reason) = blocked {
            return Some(format!(
                "This call was blocked by a PreToolUse hook and did not run: {reason}. \
                 Do not retry it; find another way or ask the user."
            ));
        }
        // A hook that was cut short by a cancel never vetted the call.
        cancel.is_cancelled().then(|| "Cancelled by user.".to_string())
    }

    /// PostToolUse, after a call ran, with its output.
    pub async fn post_tool_use(
        &self,
        tool: &str,
        args: &Value,
        output: &ToolOutput,
        cwd: &Path,
        session_id: Option<&str>,
        cancel: &CancellationToken,
    ) {
        let payload = json!({
            "event": Event::PostToolUse.as_str(),
            "session_id": session_id,
            "cwd": cwd.display().to_string(),
            "tool": tool,
            "args": args,
            "output": cap(&output.content, MAX_TEXT),
            "is_error": output.is_error,
        });
        self.dispatch(Event::PostToolUse, Some(tool), &payload, cwd, cancel, |_, _| false).await;
    }

    /// UserPromptSubmit, before a prompt is sent: a hook may block it or
    /// add context that is placed in the conversation before it.
    pub async fn user_prompt_submit(
        &self,
        prompt: &str,
        cwd: &Path,
        session_id: Option<&str>,
        cancel: &CancellationToken,
    ) -> PromptOutcome {
        let payload = json!({
            "event": Event::UserPromptSubmit.as_str(),
            "session_id": session_id,
            "cwd": cwd.display().to_string(),
            "prompt": cap(prompt, MAX_TEXT),
        });
        let mut context = Vec::new();
        let mut blocked = None;
        self.dispatch(Event::UserPromptSubmit, None, &payload, cwd, cancel, |label, r| {
            match prompt_decision(r) {
                PromptDecision::Block(reason) => blocked = Some(format!("prompt blocked by hook {label}: {reason}")),
                PromptDecision::Context(text) => context.push(format!("[hook {label} added this context]\n{text}")),
                PromptDecision::None => {}
            }
            blocked.is_some()
        })
        .await;
        match blocked {
            Some(msg) => PromptOutcome::Blocked(msg),
            None => PromptOutcome::Run(context),
        }
    }

    /// Stop, after a turn finished with this answer.
    pub async fn stop(&self, assistant_text: &str, cwd: &Path, session_id: Option<&str>, cancel: &CancellationToken) {
        let payload = json!({
            "event": Event::Stop.as_str(),
            "session_id": session_id,
            "cwd": cwd.display().to_string(),
            "assistant_text": cap(assistant_text, MAX_TEXT),
        });
        self.dispatch(Event::Stop, None, &payload, cwd, cancel, |_, _| false).await;
    }

    /// SessionStart: `source` is `startup`, `resume` or `new`.
    pub async fn session_start(&self, source: &str, cwd: &Path, session_id: Option<&str>) {
        let payload = json!({
            "event": Event::SessionStart.as_str(),
            "session_id": session_id,
            "cwd": cwd.display().to_string(),
            "source": source,
        });
        self.dispatch(Event::SessionStart, None, &payload, cwd, &CancellationToken::new(), |_, _| false).await;
    }

    /// Rows for `/hooks`.
    pub fn lines(&self) -> Vec<String> {
        let mut out = vec![format!(
            "Hooks file   {}",
            if self.user_path.as_os_str().is_empty() { "(none)".into() } else { self.user_path.display().to_string() }
        )];
        if self.user.is_empty() {
            out.push("  (no hooks)".into());
        } else {
            out.extend(self.user.iter().map(|h| format!("  {}", hook_row(h))));
        }
        if self.project_path.as_os_str().is_empty() {
            out.push("Project file (none; .nyq/hooks.json may add project hooks)".into());
        } else {
            let state = if self.project_trusted {
                "trusted".to_string()
            } else {
                "untrusted — not running; /hooks trust enables it".to_string()
            };
            out.push(format!("Project file {} · {state}", self.project_path.display()));
            out.extend(self.project.iter().map(|h| format!("  {}", hook_row(h))));
        }
        out.push("Hooks run unsandboxed via the shell with the event as JSON on stdin; their stderr is shown.".into());
        out
    }
}

#[cfg(all(test, unix))]
impl Hooks {
    /// A configuration whose user file is just this hook (for wiring tests).
    pub(crate) fn from_user_hook(hook: Hook) -> Self {
        Hooks { user: vec![hook], ..Default::default() }
    }
}

/// SessionStart hooks for an agent's current session.
pub async fn session_start(agent: &crate::agent::Agent, source: &str) {
    agent
        .hooks
        .session_start(source, &agent.cwd, agent.session.as_ref().map(|s| s.id.as_str()))
        .await;
}

/// Stop hooks after an agent's turn, with its final answer.
pub async fn stop(agent: &crate::agent::Agent, cancel: &CancellationToken) {
    let last = agent.items.iter().rev().find_map(|i| match i {
        crate::types::Item::Assistant { text } => Some(text.as_str()),
        _ => None,
    });
    let session = agent.session.as_ref().map(|s| s.id.as_str());
    agent.hooks.stop(last.unwrap_or(""), &agent.cwd, session, cancel).await;
}

/// UserPromptSubmit hooks for a prompt; any context they add is placed in
/// the conversation before it. Returns the block message when the prompt
/// must not run.
pub async fn prompt_submit(agent: &mut crate::agent::Agent, prompt: &str, cancel: &CancellationToken) -> Option<String> {
    let hooks = agent.hooks.clone();
    let session = agent.session.as_ref().map(|s| s.id.clone());
    match hooks.user_prompt_submit(prompt, &agent.cwd, session.as_deref(), cancel).await {
        PromptOutcome::Blocked(msg) => Some(msg),
        PromptOutcome::Run(_) if cancel.is_cancelled() => Some("cancelled".into()),
        PromptOutcome::Run(items) => {
            for text in items {
                crate::ui::info(text.lines().next().unwrap_or(""));
                agent.push_counted(crate::types::Item::user(text));
            }
            None
        }
    }
}

/// `/hooks` re-reads the hook files and lists them with their trust
/// state; `/hooks trust` trusts the project's file as last listed.
pub fn command(agent: &mut crate::agent::Agent, arg: Option<&str>) {
    let home = crate::config::home();
    match arg {
        None => {
            let (reloaded, warnings) = Hooks::load(&home, &agent.cwd);
            warnings.iter().for_each(|w| crate::ui::error(w));
            agent.hooks = std::sync::Arc::new(reloaded);
            crate::ui::list(&agent.hooks.lines());
        }
        Some("trust") => match agent.hooks.trust_project(&home, &agent.cwd) {
            Ok(path) => {
                let (reloaded, warnings) = Hooks::load(&home, &agent.cwd);
                warnings.iter().for_each(|w| crate::ui::error(w));
                agent.hooks = std::sync::Arc::new(reloaded);
                crate::ui::info(&format!("Trusted the project hooks in {}.", path.display()));
                crate::ui::list(&agent.hooks.lines());
            }
            Err(e) => crate::ui::error(&format!("{e:#}")),
        },
        Some(other) => crate::ui::error(&format!("unknown /hooks argument `{other}` (try /hooks or /hooks trust)")),
    }
}

/// Show a hook's stderr and any run problems to the user.
fn report(hook: &Hook, r: &RunOutcome) {
    let label = cut(&hook.command, 40);
    if r.cancelled {
        crate::ui::info(&format!("hook {label} cancelled"));
    } else if let Some(e) = &r.error {
        crate::ui::info(&format!("hook {label} could not run: {e}"));
    } else if r.timed_out {
        crate::ui::info(&format!("hook {label} timed out after {}s (skipped)", hook.timeout_secs));
    }
    let stderr = r.stderr.trim();
    if !stderr.is_empty() {
        crate::ui::info(&format!("hook {label}: {}", cut(stderr, 400)));
    }
}

/// Why a PreToolUse hook blocked this call: an explicit
/// `{"decision":"block","reason":…}` on stdout, else any non-zero exit
/// (with the hook's stderr as the reason). Timeouts and spawn failures
/// never block.
fn block_reason(r: &RunOutcome) -> Option<String> {
    if r.error.is_some() || r.timed_out || r.cancelled {
        return None;
    }
    if let Some(reason) = block_decision(&r.stdout) {
        return Some(reason);
    }
    if r.exit != Some(0) {
        let code = r
            .exit
            .map(|c| format!("exit code {c}"))
            .unwrap_or_else(|| "died without an exit status".into());
        let why = r.stderr.trim();
        return Some(if why.is_empty() { code } else { format!("{} ({code})", cut(why, 300)) });
    }
    None
}

/// `{"decision":"block","reason":…}` on stdout → the reason.
fn block_decision(stdout: &str) -> Option<String> {
    let obj = serde_json::from_str::<Value>(stdout.trim()).ok()?.as_object()?.clone();
    if obj.get("decision").and_then(Value::as_str) != Some("block") {
        return None;
    }
    let reason = obj
        .get("reason")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .unwrap_or("blocked by a hook");
    Some(reason.to_string())
}

#[derive(Debug, PartialEq)]
enum PromptDecision {
    Block(String),
    Context(String),
    None,
}

/// What a UserPromptSubmit hook's stdout asks for: a JSON object may block
/// the prompt (`{"decision":"block","reason":…}`) or add context
/// (`{"context":…}` or `additionalContext`); plain text printed by a hook
/// that exited 0 is context as is. Failed, timed-out and cancelled hooks
/// decide nothing, and a non-zero exit alone does not block a prompt.
fn prompt_decision(r: &RunOutcome) -> PromptDecision {
    let out = r.stdout.trim();
    if r.error.is_some() || r.timed_out || r.cancelled || out.is_empty() {
        return PromptDecision::None;
    }
    if let Ok(Value::Object(obj)) = serde_json::from_str::<Value>(out) {
        if let Some(reason) = block_decision(out) {
            return PromptDecision::Block(reason);
        }
        return ["context", "additionalContext"]
            .iter()
            .find_map(|k| obj.get(*k).and_then(Value::as_str).map(str::trim).filter(|t| !t.is_empty()))
            .map_or(PromptDecision::None, |t| PromptDecision::Context(t.to_string()));
    }
    if r.exit == Some(0) {
        PromptDecision::Context(out.to_string())
    } else {
        PromptDecision::None
    }
}

fn hook_row(h: &Hook) -> String {
    let matcher = h.matcher.as_deref().unwrap_or("*");
    format!(
        "{:<15} {:<18} {:<40} {}s",
        h.event.as_str(),
        matcher,
        cut(&h.command, 38),
        h.timeout_secs
    )
}

/// A string capped to `max` chars (an ellipsis marks the cut).
fn cap(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// The first non-empty line, trimmed and capped (for display).
fn cut(s: &str, max: usize) -> String {
    let first = s.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    cap(first, max)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn live() -> CancellationToken {
        CancellationToken::new()
    }

    fn hook(event: Event, matcher: Option<&str>, command: &str) -> Hook {
        Hook { event, matcher: matcher.map(String::from), command: command.into(), timeout_secs: DEFAULT_TIMEOUT }
    }

    fn hooks_with(user: Vec<Hook>, project: Vec<Hook>, trusted: bool) -> Hooks {
        Hooks {
            user,
            project,
            project_path: PathBuf::from("/w/.nyq/hooks.json"),
            project_hash: None,
            project_trusted: trusted,
            user_path: PathBuf::from("/h/hooks.json"),
        }
    }

    #[test]
    fn matchers_filter_tool_events() {
        let h = hooks_with(
            vec![
                hook(Event::PreToolUse, Some("bash"), "guard.sh"),
                hook(Event::PreToolUse, Some("mcp__*"), "audit.sh"),
                hook(Event::PostToolUse, None, "log.sh"),
                hook(Event::Stop, None, "done.sh"),
            ],
            vec![hook(Event::PreToolUse, Some("bash"), "project.sh")],
            true,
        );
        let fire = |e: Event, t: Option<&str>| h.matching(e, t).iter().map(|x| x.command.as_str()).collect::<Vec<&str>>();
        assert_eq!(fire(Event::PreToolUse, Some("bash")), ["guard.sh", "project.sh"]);
        assert_eq!(fire(Event::PreToolUse, Some("write_file")), [] as [&str; 0]);
        assert_eq!(fire(Event::PreToolUse, Some("mcp__wiki__ask")), ["audit.sh"]);
        assert_eq!(fire(Event::PostToolUse, Some("bash")), ["log.sh"]);
        assert_eq!(fire(Event::Stop, None), ["done.sh"]);
        // Untrusted project hooks never fire.
        let h = hooks_with(Vec::new(), vec![hook(Event::Stop, None, "project.sh")], false);
        assert_eq!(h.matching(Event::Stop, None).len(), 0);
    }

    #[test]
    fn parsing_accepts_and_skips_entries() {
        let (hooks, warnings) = parse(
            r#"{"hooks": [
                {"event": "PreToolUse", "matcher": "bash", "command": " a.sh ", "timeout_secs": 900},
                {"event": "Stop", "command": "b.sh"},
                {"event": "Nope", "command": "c.sh"},
                {"command": "d.sh"},
                {"event": "PreToolUse"},
                {"event": "PreToolUse", "command": "  "},
                {"event": "PreToolUse", "command": "e.sh", "timeout_secs": 0},
                {"event": "PreToolUse", "command": "f.sh", "matcher": ""},
                {"event": "Stop", "command": "g.sh", "matcher": "x"},
                {"event": "PreToolUse", "command": "h.sh", "matcher": null, "timeout_secs": null},
                {}
            ]}"#,
        )
        .unwrap();
        assert_eq!(
            hooks,
            vec![
                Hook { event: Event::PreToolUse, matcher: Some("bash".into()), command: "a.sh".into(), timeout_secs: MAX_TIMEOUT },
                hook(Event::Stop, None, "b.sh"),
                hook(Event::PreToolUse, None, "h.sh"),
            ]
        );
        assert_eq!(warnings.len(), 8, "{warnings:?}");
        assert!(warnings.iter().any(|w| w.contains("event\" must be")), "{warnings:?}");
        assert!(warnings.iter().any(|w| w.contains("command\" must be")), "{warnings:?}");
        assert!(warnings.iter().any(|w| w.contains("timeout_secs\" must be")), "{warnings:?}");
        assert!(warnings.iter().any(|w| w.contains("matcher")), "{warnings:?}");
        assert!(warnings.iter().any(|w| w.contains("applies only to")), "{warnings:?}");

        assert!(parse("[]").is_err());
        assert!(parse("{").is_err());
        assert!(parse(r#"{"hooks": 3}"#).is_err());
        assert_eq!(parse("{}").unwrap().0.len(), 0);
        assert_eq!(parse(r#"{"other": 1}"#).unwrap().0.len(), 0);
    }

    #[test]
    fn event_names() {
        for (name, event) in [
            ("PreToolUse", Event::PreToolUse),
            ("PostToolUse", Event::PostToolUse),
            ("UserPromptSubmit", Event::UserPromptSubmit),
            ("Stop", Event::Stop),
            ("SessionStart", Event::SessionStart),
        ] {
            assert_eq!(Event::parse(name), Some(event));
            assert_eq!(event.as_str(), name);
        }
        assert_eq!(Event::parse("pretooluse"), None);
    }

    fn outcome(exit: Option<i32>, stdout: &str, stderr: &str) -> RunOutcome {
        RunOutcome { exit, timed_out: false, cancelled: false, stdout: stdout.into(), stderr: stderr.into(), error: None }
    }

    #[test]
    fn blocking_decisions() {
        let json_block = |reason: &str| format!(r#"{{"decision":"block","reason":"{reason}"}}"#);
        // An explicit decision wins, at any exit status.
        assert_eq!(
            block_reason(&outcome(Some(0), &json_block("nope"), "")),
            Some("nope".into())
        );
        assert_eq!(
            block_reason(&outcome(Some(2), &json_block("nope"), "ignored")),
            Some("nope".into())
        );
        assert_eq!(block_reason(&outcome(Some(0), r#"{"decision":"block"}"#, "")), Some("blocked by a hook".into()));
        assert_eq!(
            block_reason(&outcome(Some(0), r#"{"decision":"block","reason":"  "}"#, "")),
            Some("blocked by a hook".into())
        );
        // A plain non-zero exit blocks, with stderr as the reason.
        assert_eq!(block_reason(&outcome(Some(2), "", "bad idea\n")), Some("bad idea (exit code 2)".into()));
        assert_eq!(block_reason(&outcome(Some(127), "noise", "")), Some("exit code 127".into()));
        assert_eq!(block_reason(&outcome(None, "", "killed")), Some("killed (died without an exit status)".into()));
        // Exit 0, other decisions, junk stdout and timeouts never block.
        assert_eq!(block_reason(&outcome(Some(0), "", "")), None);
        assert_eq!(block_reason(&outcome(Some(0), r#"{"decision":"allow"}"#, "")), None);
        assert_eq!(block_reason(&outcome(Some(0), "hello", "")), None);
        assert_eq!(
            block_reason(&RunOutcome { exit: None, timed_out: true, cancelled: false, stdout: String::new(), stderr: "late".into(), error: None }),
            None
        );
        assert_eq!(
            block_reason(&RunOutcome { exit: None, timed_out: false, cancelled: false, stdout: String::new(), stderr: String::new(), error: Some("spawn".into()) }),
            None
        );
        // Parse quirks: the decision must be a JSON object.
        assert_eq!(block_decision("[]"), None);
        assert_eq!(block_decision(""), None);
    }

    #[test]
    fn prompt_decisions() {
        let d = |exit, stdout: &str| prompt_decision(&outcome(exit, stdout, ""));
        let ctx = |t: &str| PromptDecision::Context(t.into());
        assert_eq!(d(Some(0), r#"{"context":"PROJECT NOTES"}"#), ctx("PROJECT NOTES"));
        assert_eq!(d(Some(0), r#"{"additionalContext":"alt"}"#), ctx("alt"));
        assert_eq!(d(Some(0), r#"{"context":"  "}"#), PromptDecision::None);
        assert_eq!(d(Some(0), r#"{"other":1}"#), PromptDecision::None);
        assert_eq!(d(Some(1), r#"{"decision":"block","reason":"x"}"#), PromptDecision::Block("x".into()));
        // Plain text from a successful hook is context; from a failed one, nothing.
        assert_eq!(d(Some(0), "  branch: main\n"), ctx("branch: main"));
        assert_eq!(d(Some(2), "branch: main"), PromptDecision::None);
        assert_eq!(d(Some(0), "[1, 2]"), ctx("[1, 2]"));
        assert_eq!(d(Some(0), ""), PromptDecision::None);
        let mut late = outcome(None, "partial", "");
        late.timed_out = true;
        assert_eq!(prompt_decision(&late), PromptDecision::None);
    }

    #[test]
    fn text_caps() {
        assert_eq!(cap("short", 10), "short");
        assert_eq!(cap("aaaaa", 3), "aa…");
        assert_eq!(cap("éééé", 2), "é…");
        assert_eq!(cut("  first line\nsecond  ", 40), "first line");
        assert_eq!(cut("only", 2), "o…");
        assert_eq!(cut("", 10), "");
    }

    #[test]
    fn payload_fields_are_capped() {
        let payload = json!({ "assistant_text": cap(&"x".repeat(20), 10) });
        assert_eq!(payload["assistant_text"].as_str().unwrap().chars().count(), 10);
    }

    #[test]
    fn load_reads_user_and_trusts_project() {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        // No files at all.
        let (h, warnings) = Hooks::load(home.path(), cwd.path());
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(h.matching(Event::Stop, None).is_empty());
        assert!(h.project_path.as_os_str().is_empty());

        // User hooks, with a bad entry reported.
        std::fs::write(
            user_path(home.path()),
            r#"{"hooks": [{"event": "Stop", "command": "done.sh"}, {"event": "Nope", "command": "x"}]}"#,
        )
        .unwrap();
        let (h, warnings) = Hooks::load(home.path(), cwd.path());
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("hooks.json"), "{warnings:?}");
        assert_eq!(h.matching(Event::Stop, None).len(), 1);

        // Project hooks exist but are untrusted: listed, warned, not run.
        let project = project_path(cwd.path());
        std::fs::create_dir_all(project.parent().unwrap()).unwrap();
        std::fs::write(&project, r#"{"hooks": [{"event": "Stop", "command": "p.sh"}]}"#).unwrap();
        let (h, warnings) = Hooks::load(home.path(), cwd.path());
        assert_eq!(h.project_path, project);
        assert!(h.lines().iter().any(|l| l.contains("untrusted")), "{:?}", h.lines());
        assert_eq!(h.matching(Event::Stop, None).len(), 1);
        assert!(warnings.iter().any(|w| w.contains("not trusted")), "{warnings:?}");

        // Trusted: they run; the user's hooks still come first.
        h.trust_project(home.path(), cwd.path()).unwrap();
        let (h, warnings) = Hooks::load(home.path(), cwd.path());
        assert!(!warnings.iter().any(|w| w.contains("not trusted")), "{warnings:?}");
        let fire: Vec<_> = h.matching(Event::Stop, None).iter().map(|x| x.command.clone()).collect();
        assert_eq!(fire, ["done.sh".to_string(), "p.sh".into()]);
        assert!(h.lines().iter().any(|l| l.contains("trusted") && !l.contains("untrusted")));

        // A changed project file is untrusted again, and the version loaded
        // before the change cannot be trusted in its place.
        std::fs::write(&project, r#"{"hooks": [{"event": "Stop", "command": "p2.sh"}]}"#).unwrap();
        let err = h.trust_project(home.path(), cwd.path()).unwrap_err().to_string();
        assert!(err.contains("changed since it was loaded"), "{err}");
        let (h, warnings) = Hooks::load(home.path(), cwd.path());
        assert!(warnings.iter().any(|w| w.contains("not trusted")), "{warnings:?}");
        assert_eq!(h.matching(Event::Stop, None).len(), 1);

        // The trust file is private and survives re-trusting other projects.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(trust_file(home.path())).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        let other = tempfile::tempdir().unwrap();
        let other_project = project_path(other.path());
        std::fs::create_dir_all(other_project.parent().unwrap()).unwrap();
        std::fs::write(&other_project, r#"{"hooks": [{"event": "Stop", "command": "o.sh"}]}"#).unwrap();
        Hooks::load(home.path(), other.path()).0.trust_project(home.path(), other.path()).unwrap();
        let (h, _) = Hooks::load(home.path(), other.path());
        assert!(h.project_trusted);
        let (h, _) = Hooks::load(home.path(), cwd.path());
        assert!(!h.project_trusted, "the stale trust must not match the changed file");
    }

    #[test]
    fn trust_refuses_bad_input() {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let trust = || {
            let (h, _) = Hooks::load(home.path(), cwd.path());
            h.trust_project(home.path(), cwd.path()).unwrap_err().to_string()
        };
        // No project hooks file.
        assert!(trust().contains("reading"));
        let project = project_path(cwd.path());
        std::fs::create_dir_all(project.parent().unwrap()).unwrap();
        // Malformed, invalid entries, and empty files are not trusted.
        std::fs::write(&project, "{oops").unwrap();
        assert!(trust().contains("malformed"));
        std::fs::write(&project, r#"{"hooks": [{"event": "Nope", "command": "x"}]}"#).unwrap();
        assert!(trust().contains("invalid entries"));
        std::fs::write(&project, "{}").unwrap();
        assert!(trust().contains("no hooks"));
        // A file that appeared after loading must be listed first.
        let (stale, _) = Hooks::load(home.path(), cwd.path());
        std::fs::write(&project, r#"{"hooks": [{"event": "Stop", "command": "p.sh"}]}"#).unwrap();
        assert!(stale.trust_project(home.path(), cwd.path()).is_err());
        // A malformed trust file is never overwritten.
        std::fs::write(trust_file(home.path()), "{bad").unwrap();
        assert!(trust().contains("malformed"));
        assert_eq!(std::fs::read_to_string(trust_file(home.path())).unwrap(), "{bad");
        // A malformed trust file also means nothing is trusted.
        let (h, warnings) = Hooks::load(home.path(), cwd.path());
        assert!(!h.project_trusted);
        assert!(warnings.iter().any(|w| w.contains("not trusted")), "{warnings:?}");
    }

    #[test]
    fn workspace_profile_is_loaded_once() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(user_path(home.path()), r#"{"hooks": [{"event": "Stop", "command": "solo.sh"}]}"#).unwrap();
        let (h, warnings) = Hooks::load(home.path(), home.path());
        assert!(warnings.is_empty(), "{warnings:?}");
        // The profile's own hooks.json is only the user's file; there is no
        // second, untrusted copy of it.
        assert!(h.project_path.as_os_str().is_empty());
        assert!(!h.project_trusted);
        assert_eq!(h.matching(Event::Stop, None).len(), 1);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn hooks_run_get_stdin_and_can_block() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_path_buf();
        // The event JSON arrives on stdin: dump it to a file.
        let dumped = dir.path().join("stdin.json");
        let h = hooks_with(
            vec![hook(Event::PreToolUse, Some("bash"), &format!("cat > {}", dumped.display()))],
            Vec::new(),
            false,
        );
        let args = json!({"command": "cargo test"});
        assert_eq!(h.pre_tool_use("bash", &args, &cwd, Some("sess-7"), &live()).await, None);
        let payload: Value = serde_json::from_str(&std::fs::read_to_string(&dumped).unwrap()).unwrap();
        assert_eq!(payload["event"], "PreToolUse");
        assert_eq!(payload["tool"], "bash");
        assert_eq!(payload["args"]["command"], "cargo test");
        assert_eq!(payload["session_id"], "sess-7");
        assert_eq!(payload["cwd"], cwd.display().to_string());

        // A blocking decision (stdout JSON) and a non-zero exit both block,
        // with the reason reaching the model.
        let h = hooks_with(
            vec![hook(Event::PreToolUse, None, r#"echo '{"decision":"block","reason":"not on my watch"}'"#)],
            Vec::new(),
            false,
        );
        let msg = h.pre_tool_use("bash", &args, &cwd, None, &live()).await.unwrap();
        assert!(msg.contains("blocked by a PreToolUse hook") && msg.contains(": not on my watch."), "{msg}");
        let h = hooks_with(vec![hook(Event::PreToolUse, None, "echo why not >&2; exit 3")], Vec::new(), false);
        let msg = h.pre_tool_use("bash", &args, &cwd, None, &live()).await.unwrap();
        assert!(msg.contains("why not (exit code 3)"), "{msg}");

        // The first block wins: later hooks do not run.
        let marker = dir.path().join("second-ran");
        let h = hooks_with(
            vec![hook(Event::PreToolUse, None, "exit 1"), hook(Event::PreToolUse, None, &format!("touch {}", marker.display()))],
            Vec::new(),
            false,
        );
        assert!(h.pre_tool_use("bash", &args, &cwd, None, &live()).await.is_some());
        assert!(!marker.exists());

        // A hook that does not match leaves the call alone.
        let h = hooks_with(vec![hook(Event::PreToolUse, Some("write_file"), "exit 1")], Vec::new(), false);
        assert_eq!(h.pre_tool_use("bash", &args, &cwd, None, &live()).await, None);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn prompts_can_be_blocked_or_gain_context() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_path_buf();
        let h = hooks_with(
            vec![hook(Event::UserPromptSubmit, None, r#"echo '{"context":"PROJECT NOTES"}'"#)],
            Vec::new(),
            false,
        );
        assert!(matches!(
            h.user_prompt_submit("fix it", &cwd, None, &live()).await,
            PromptOutcome::Run(items) if items.len() == 1
                && items[0].contains("added this context")
                && items[0].ends_with("PROJECT NOTES")
        ));
        let h = hooks_with(
            vec![hook(Event::UserPromptSubmit, None, r#"echo '{"decision":"block","reason":"after hours"}'"#)],
            Vec::new(),
            false,
        );
        assert!(matches!(
            h.user_prompt_submit("fix it", &cwd, None, &live()).await,
            PromptOutcome::Blocked(msg) if msg.contains("after hours") && msg.contains("blocked by hook")
        ));
        // A non-zero exit alone does not block a prompt.
        let h = hooks_with(vec![hook(Event::UserPromptSubmit, None, "exit 1")], Vec::new(), false);
        assert_eq!(h.user_prompt_submit("fix it", &cwd, None, &live()).await, PromptOutcome::Run(Vec::new()));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timeouts_kill_the_hook() {
        let dir = tempfile::tempdir().unwrap();
        let started = std::time::Instant::now();
        let mut slow = hook(Event::Stop, None, "sleep 30 & sleep 30; echo done");
        slow.timeout_secs = 1;
        let h = hooks_with(vec![slow], Vec::new(), false);
        h.stop("answer", dir.path(), None, &live()).await;
        assert!(started.elapsed() < std::time::Duration::from_secs(20));
        // The runner reports the timeout, and it never blocks a call.
        let mut timed = hook(Event::Stop, None, "sleep 30");
        timed.timeout_secs = 1;
        let r = run_hook(&timed, "{}", dir.path(), &live()).await;
        assert!(r.timed_out);
        assert_eq!(r.exit, None);
        assert!(r.error.is_none());
        assert!(block_reason(&r).is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancel_kills_the_hook_and_never_lets_the_call_through() {
        let dir = tempfile::tempdir().unwrap();
        let h = hooks_with(vec![hook(Event::PreToolUse, None, "sleep 30")], Vec::new(), false);
        let cancel = CancellationToken::new();
        let trigger = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            trigger.cancel();
        });
        let started = std::time::Instant::now();
        let msg = h.pre_tool_use("bash", &json!({}), dir.path(), None, &cancel).await;
        assert_eq!(msg.as_deref(), Some("Cancelled by user."));
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn misbehaving_hooks_do_not_hang() {
        let dir = tempfile::tempdir().unwrap();
        // Ignores a large stdin, and leaves a background child holding stdout.
        let mut h = hook(Event::Stop, None, "sleep 30 & echo hi");
        h.timeout_secs = 20;
        let started = std::time::Instant::now();
        let big = "x".repeat(1 << 20);
        let r = run_hook(&h, &big, dir.path(), &live()).await;
        assert_eq!(r.exit, Some(0));
        assert_eq!(r.stdout.trim(), "hi");
        assert!(started.elapsed() < Duration::from_secs(10));
        // A command that cannot be found is a failed hook, not a crash.
        let r = run_hook(&hook(Event::Stop, None, "/nonexistent/hook-binary"), "{}", dir.path(), &live()).await;
        assert_eq!(r.exit, Some(127));
        assert!(block_reason(&r).is_some(), "a broken PreToolUse guard fails closed");
    }

    #[test]
    fn subagents_get_only_tool_hooks() {
        let h = hooks_with(
            vec![hook(Event::PreToolUse, Some("bash"), "guard.sh"), hook(Event::Stop, None, "done.sh")],
            vec![hook(Event::PostToolUse, None, "p.sh"), hook(Event::SessionStart, None, "s.sh")],
            true,
        );
        let sub = h.for_subagent();
        assert_eq!(sub.matching(Event::PreToolUse, Some("bash")).len(), 1);
        assert_eq!(sub.matching(Event::PostToolUse, Some("bash")).len(), 1);
        assert!(sub.matching(Event::Stop, None).is_empty());
        assert!(sub.matching(Event::SessionStart, None).is_empty());
        let untrusted = hooks_with(Vec::new(), vec![hook(Event::PreToolUse, None, "p.sh")], false).for_subagent();
        assert!(untrusted.matching(Event::PreToolUse, Some("bash")).is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn session_start_and_post_tool_use_payloads() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_path_buf();
        let dumped = dir.path().join("events.jsonl");
        let dumper = |event: Event, matcher: Option<&str>| {
            let mut h = hook(event, matcher, "");
            h.command = format!("cat >> {}", dumped.display());
            h
        };
        let h = hooks_with(vec![dumper(Event::SessionStart, None)], Vec::new(), false);
        h.session_start("startup", &cwd, Some("s1")).await;
        let line: Value = serde_json::from_str(&std::fs::read_to_string(&dumped).unwrap()).unwrap();
        assert_eq!(line["event"], "SessionStart");
        assert_eq!(line["source"], "startup");
        assert_eq!(line["session_id"], "s1");

        std::fs::remove_file(&dumped).unwrap();
        let h = hooks_with(vec![dumper(Event::PostToolUse, Some("bash"))], Vec::new(), false);
        h.post_tool_use("bash", &json!({"command": "ls"}), &ToolOutput::err("boom"), &cwd, None, &live()).await;
        let line: Value = serde_json::from_str(&std::fs::read_to_string(&dumped).unwrap()).unwrap();
        assert_eq!(line["event"], "PostToolUse");
        assert_eq!(line["tool"], "bash");
        assert_eq!(line["is_error"], true);
        assert_eq!(line["output"], "boom");
    }
}
