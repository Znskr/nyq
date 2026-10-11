//! Background shell processes: `bash` with `run_in_background` starts one and
//! returns its id; `bash_output` returns what it printed since the last read
//! plus its status; `process_write` sends it input (when started with
//! `interactive`); `process_wait` blocks until its output matches, a port
//! opens, or it exits; `kill_shell` stops it. Output is kept in memory (the
//! last [`BUF_CAP`] bytes per process), a dev-server URL it prints is noted,
//! and every process is killed when nyq exits.

use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use regex::Regex;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::bash::{exit_note, finish_pumps, kill_group, reap_leftovers, render_output, spawn, WinJob, GRACE};
use super::{Registry, Tool, ToolCtx, ToolOutput};
use crate::types::ToolSpec;

/// Output kept per process; older bytes are dropped (and reported as such).
const BUF_CAP: usize = 1024 * 1024;
/// A finished process whose output has all been read is dropped after this.
const DONE_KEEP: Duration = Duration::from_secs(60);
const WAIT_DEFAULT: u64 = 30;
const WAIT_MAX: u64 = 600;
const POLL: Duration = Duration::from_millis(100);
/// How long `process_write` lets the process answer before reading its output.
const WRITE_SETTLE: Duration = Duration::from_millis(300);

/// A local server address as dev servers print it (`http://localhost:5173/`).
static SERVER_URL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"https?://(?:localhost|127\.0\.0\.1|0\.0\.0\.0|\[::1?\]|[A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)*):[0-9]{2,5}(?:/[^\s'\x22<>)\]]*)?").unwrap()
});
static ANSI: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\x1b\[[0-9;?]*[A-Za-z]").unwrap());

/// The first server URL in `text`, ignoring terminal colors.
fn server_url(text: &str) -> Option<String> {
    let plain = ANSI.replace_all(text, "");
    let found = SERVER_URL.find(&plain)?.as_str().trim_end_matches(['.', ',', ';', ':']);
    Some(found.to_string())
}

#[derive(Clone, Debug, PartialEq)]
enum State {
    Running,
    /// Exited on its own; `None` when it ended cleanly, else the exit note.
    Exited(Option<String>),
    Killed,
    Failed(String),
}

struct Proc {
    id: u32,
    command: String,
    cwd: PathBuf,
    /// The dev-server URL it printed, if any.
    url: Option<String>,
    /// Open stdin for `process_write` (interactive processes only).
    stdin: Option<Arc<tokio::sync::Mutex<tokio::process::ChildStdin>>>,
    started: Instant,
    ended: Option<Instant>,
    state: State,
    pid: u32,
    job: Option<Arc<WinJob>>,
    kill_requested: bool,
    buf: Vec<u8>,
    /// Absolute offset of `buf[0]` (bytes dropped from the front).
    base: u64,
    /// Absolute offset up to which `bash_output` has returned output.
    read: u64,
}

impl Proc {
    fn push(&mut self, chunk: &[u8]) {
        if self.url.is_none() {
            // Look a little before the chunk too, in case a URL was split.
            let from = self.buf.len().saturating_sub(256);
            let mut window = self.buf[from..].to_vec();
            window.extend_from_slice(chunk);
            self.url = server_url(&String::from_utf8_lossy(&window));
        }
        self.buf.extend_from_slice(chunk);
        if self.buf.len() > BUF_CAP {
            let extra = self.buf.len() - BUF_CAP;
            self.buf.drain(..extra);
            self.base += extra as u64;
        }
    }

    fn runtime(&self) -> Duration {
        self.ended.unwrap_or_else(Instant::now).saturating_duration_since(self.started)
    }

    fn status(&self) -> String {
        let t = duration(self.runtime());
        match &self.state {
            State::Running if self.url.is_some() => format!("running for {t}, serving {}", self.url.as_deref().unwrap_or("")),
            State::Running => format!("running for {t}"),
            State::Exited(None) => format!("exited with code 0 after {t}"),
            State::Exited(Some(note)) => format!("exited ({note}) after {t}"),
            State::Killed => format!("killed after {t}"),
            State::Failed(e) => format!("failed: {e}"),
        }
    }

    fn short_state(&self) -> String {
        match &self.state {
            State::Running => "running".into(),
            State::Exited(None) => "exit 0".into(),
            State::Exited(Some(note)) => note.replace("exit code ", "exit "),
            State::Killed => "killed".into(),
            State::Failed(_) => "failed".into(),
        }
    }
}

#[derive(Default)]
struct Table {
    procs: Vec<Proc>,
    next: u32,
}

static TABLE: LazyLock<Mutex<Table>> = LazyLock::new(Default::default);

fn table() -> std::sync::MutexGuard<'static, Table> {
    TABLE.lock().unwrap_or_else(|e| e.into_inner())
}

fn with_proc<T>(id: u32, f: impl FnOnce(&mut Proc) -> T) -> Option<T> {
    compact_finished(&mut table());
    table().procs.iter_mut().find(|p| p.id == id).map(f)
}

fn fully_read(p: &Proc) -> bool {
    p.read >= p.base + p.buf.len() as u64
}

fn finished_and_read(p: &Proc, now: Instant, keep: Duration) -> bool {
    matches!(p.ended, Some(ended) if fully_read(p) && now.saturating_duration_since(ended) >= keep)
}

/// Drop processes that have exited, been fully read, and sat that way for a while.
fn compact_finished(table: &mut Table) {
    let now = Instant::now();
    table.procs.retain(|p| !finished_and_read(p, now, DONE_KEEP));
}

fn duration(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m{:02}s", s / 60, s % 60),
        _ => format!("{}h{:02}m", s / 3600, (s % 3600) / 60),
    }
}

/// Start `command` in the background (sandboxed like `bash`). Returns its id.
/// An `interactive` process keeps its stdin open for `process_write`.
pub(crate) fn start(command: &str, cwd: &Path, sandbox: &crate::sandbox::Policy, interactive: bool) -> Result<u32, String> {
    let (mut child, job) = spawn(command, cwd, sandbox, interactive).map_err(|e| e.to_string())?;
    let pid = child.id().unwrap_or(0);
    let job = job.map(Arc::new);
    let stdin = child.stdin.take().map(|s| Arc::new(tokio::sync::Mutex::new(s)));
    let id = {
        let mut t = table();
        compact_finished(&mut t);
        t.next += 1;
        let id = t.next;
        t.procs.push(Proc {
            id,
            command: command.to_string(),
            cwd: cwd.to_path_buf(),
            url: None,
            stdin,
            started: Instant::now(),
            ended: None,
            state: State::Running,
            pid,
            job: job.clone(),
            kill_requested: false,
            buf: Vec::new(),
            base: 0,
            read: 0,
        });
        id
    };
    let mut pumps = Vec::new();
    if let Some(out) = child.stdout.take() {
        pumps.push(tokio::spawn(pump(out, id)));
    }
    if let Some(err) = child.stderr.take() {
        pumps.push(tokio::spawn(pump(err, id)));
    }
    tokio::spawn(async move {
        let status = child.wait().await;
        reap_leftovers(&mut child, pid, job.as_deref()).await;
        finish_pumps(pumps).await;
        with_proc(id, |p| {
            p.ended = Some(Instant::now());
            p.stdin = None;
            p.state = match status {
                _ if p.kill_requested => State::Killed,
                Ok(s) => State::Exited(exit_note(&s)),
                Err(e) => State::Failed(e.to_string()),
            };
        });
    });
    Ok(id)
}

async fn pump<R: tokio::io::AsyncRead + Unpin>(mut r: R, id: u32) {
    let mut buf = [0u8; 8192];
    loop {
        match r.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                with_proc(id, |p| p.push(&buf[..n]));
            }
        }
    }
}

/// New output since the last read, with a status header.
pub(crate) fn read_new(id: u32) -> Result<String, String> {
    with_proc(id, |p| {
        let mut dropped = 0;
        if p.read < p.base {
            dropped = p.base - p.read;
            p.read = p.base;
        }
        let from = (p.read - p.base) as usize;
        let mut slice = &p.buf[from..];
        if p.state == State::Running {
            // Hold back a multi-byte character that is still being written.
            if let Err(e) = std::str::from_utf8(slice) {
                if e.error_len().is_none() {
                    slice = &slice[..e.valid_up_to()];
                }
            }
        }
        p.read += slice.len() as u64;
        let mut out = format!("[process {} {}]\n", p.id, p.status());
        if dropped > 0 {
            out.push_str(&format!("[{dropped} bytes of earlier output were dropped before this read]\n"));
        }
        if slice.is_empty() {
            out.push_str("(no new output)");
        } else {
            out.push_str(&render_output(slice, 0));
        }
        out
    })
    .ok_or_else(|| format!("no background process with id {id}"))
}

/// Stop process `id` (TERM, then KILL after a grace period).
pub(crate) async fn kill(id: u32) -> Result<String, String> {
    let (pid, job) = with_proc(id, |p| {
        if p.state != State::Running {
            return Err(format!("process {id} is not running ({})", p.status()));
        }
        p.kill_requested = true;
        Ok((p.pid, p.job.clone()))
    })
    .ok_or_else(|| format!("no background process with id {id}"))??;
    kill_group(pid, "TERM", job.as_deref());
    let running = || with_proc(id, |p| p.state == State::Running).unwrap_or(false);
    let deadline = Instant::now() + GRACE;
    while running() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    if running() {
        kill_group(pid, "KILL", job.as_deref());
        let deadline = Instant::now() + GRACE * 2;
        while running() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
    Ok(with_proc(id, |p| format!("process {id}: {}", p.status())).unwrap_or_default())
}

/// Send `text` to the process's stdin, closing it afterwards when `close`.
pub(crate) async fn write(id: u32, text: &str, close: bool) -> Result<(), String> {
    let stdin = with_proc(id, |p| {
        if p.state != State::Running {
            return Err(format!("process {id} is not running ({})", p.status()));
        }
        p.stdin.clone().ok_or_else(|| {
            format!("process {id} has no open stdin; start it with bash {{\"run_in_background\": true, \"interactive\": true}}")
        })
    })
    .ok_or_else(|| format!("no background process with id {id}"))??;
    let mut pipe = stdin.lock().await;
    let result = async {
        pipe.write_all(text.as_bytes()).await?;
        pipe.flush().await
    }
    .await;
    if let Err(e) = result {
        with_proc(id, |p| p.stdin = None);
        return Err(format!("could not write to process {id}: {e}"));
    }
    if close {
        let _ = pipe.shutdown().await;
        with_proc(id, |p| p.stdin = None);
    }
    Ok(())
}

/// What `process_wait` waits for; with neither, the process's exit.
pub(crate) struct WaitFor {
    pub pattern: Option<Regex>,
    pub port: Option<u16>,
}

async fn port_open(port: u16) -> bool {
    let connect = tokio::net::TcpStream::connect(("127.0.0.1", port));
    matches!(tokio::time::timeout(Duration::from_millis(250), connect).await, Ok(Ok(_)))
}

/// Wait until `cond` holds, the process exits, `timeout` passes or `cancel`
/// fires; then return why, followed by the new output.
pub(crate) async fn wait(id: u32, cond: &WaitFor, timeout: Duration, cancel: &tokio_util::sync::CancellationToken) -> Result<String, String> {
    let deadline = Instant::now() + timeout;
    let reason = loop {
        let (running, matched) = with_proc(id, |p| {
            let from = p.read.saturating_sub(p.base) as usize;
            let unread = String::from_utf8_lossy(&p.buf[from.min(p.buf.len())..]);
            let matched = cond.pattern.as_ref().is_some_and(|re| re.is_match(&ANSI.replace_all(&unread, "")));
            (p.state == State::Running, matched)
        })
        .ok_or_else(|| format!("no background process with id {id}"))?;
        if matched {
            break "the output matched the pattern".to_string();
        }
        if let Some(port) = cond.port {
            if port_open(port).await {
                break format!("port {port} is accepting connections");
            }
        }
        if !running {
            break if cond.pattern.is_some() || cond.port.is_some() {
                "the process ended before the condition was met".to_string()
            } else {
                "the process ended".to_string()
            };
        }
        if Instant::now() >= deadline {
            break format!("timed out after {}s; the condition was not met", timeout.as_secs());
        }
        tokio::select! {
            _ = tokio::time::sleep(POLL) => {}
            _ = cancel.cancelled() => return Err("Cancelled by user.".into()),
        }
    };
    Ok(format!("[wait: {reason}]\n{}", read_new(id)?))
}

/// One line per tracked process, for the model's per-turn context.
pub(crate) fn context_lines() -> Vec<String> {
    let mut t = table();
    compact_finished(&mut t);
    t.procs
        .iter()
        .map(|p| {
            let cmd: String = p.command.lines().next().unwrap_or("").chars().take(120).collect();
            let stdin = if p.stdin.is_some() { ", stdin open" } else { "" };
            format!("- #{} `{cmd}` in {} (pid {}): {}{stdin}", p.id, p.cwd.display(), p.pid, p.status())
        })
        .collect()
}

/// Kill every running background process (on exit).
pub(crate) fn kill_all() {
    for p in table().procs.iter_mut().filter(|p| p.state == State::Running) {
        p.kill_requested = true;
        kill_group(p.pid, "KILL", p.job.as_deref());
    }
}

/// `id`, `#id`, or `last` (the most recent process).
fn parse_id(v: &Value) -> Option<u32> {
    if let Some(n) = v.as_u64() {
        return u32::try_from(n).ok();
    }
    let s = v.as_str()?.trim();
    if s == "last" {
        return table().procs.last().map(|p| p.id);
    }
    s.trim_start_matches('#').parse().ok()
}

/// Lines for `/background`.
pub(crate) fn list_lines() -> Vec<String> {
    let mut t = table();
    compact_finished(&mut t);
    let t = &*t;
    if t.procs.is_empty() {
        return vec!["No background processes.".into()];
    }
    t.procs
        .iter()
        .map(|p| {
            let cmd: String = p.command.lines().next().unwrap_or("").chars().take(80).collect();
            format!("  #{:<3} {:<10} {:>7}  {cmd}", p.id, p.short_state(), duration(p.runtime()))
        })
        .collect()
}

/// `/background` and `/background kill ID|last`.
pub(crate) async fn slash(args: &[&str]) {
    match args {
        [] | ["list"] => crate::ui::list(&list_lines()),
        ["kill" | "stop", which] => match parse_id(&Value::String(which.to_string())) {
            Some(id) => match kill(id).await {
                Ok(msg) => crate::ui::info(&format!("  Stopped {msg}")),
                Err(e) => crate::ui::error(&e),
            },
            None => crate::ui::error("usage: /background kill ID|last"),
        },
        _ => crate::ui::error("usage: /background [list] | /background kill ID|last"),
    }
}

fn id_arg(args: &Value, tool: &str) -> Result<u32, ToolOutput> {
    args.get("id").and_then(parse_id).ok_or_else(|| ToolOutput::err(format!("{tool}: missing or invalid `id` (a background process id)")))
}

const ID_SCHEMA: &str = "Background process id returned by bash with run_in_background.";

struct ProcessWrite;

#[async_trait::async_trait]
impl Tool for ProcessWrite {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "process_write".into(),
            description: "Send input to the stdin of a background process started with bash \
                {\"run_in_background\": true, \"interactive\": true}, e.g. a REPL line or an answer to a \
                prompt. Include \"\\n\" to press Enter. Returns the process's output shortly afterwards; \
                use process_wait to block until a specific reply. close_stdin sends end-of-file."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "id": {"type": "integer", "description": ID_SCHEMA},
                    "text": {"type": "string", "description": "Text to write, exactly (add \\n for Enter)."},
                    "close_stdin": {"type": "boolean", "description": "Close stdin after writing (end of input)."}
                },
                "required": ["id"]
            }),
        }
    }

    fn needs_approval(&self, _args: &Value) -> bool {
        true
    }

    fn summary(&self, args: &Value) -> String {
        let text: String = args.get("text").and_then(Value::as_str).unwrap_or("").escape_debug().take(80).collect();
        format!("{} {text}", BashOutput.summary(args)).trim_end().to_string()
    }

    fn preview(&self, args: &Value, _ctx: &ToolCtx) -> Option<String> {
        let text = args.get("text").and_then(Value::as_str).unwrap_or("");
        let close = if args.get("close_stdin").and_then(Value::as_bool) == Some(true) { "\n(then close stdin)" } else { "" };
        Some(format!("{text}{close}"))
    }

    async fn run(&self, args: Value, _ctx: &ToolCtx) -> ToolOutput {
        let id = match id_arg(&args, "process_write") {
            Ok(id) => id,
            Err(out) => return out,
        };
        let text = args.get("text").and_then(Value::as_str).unwrap_or("");
        let close = args.get("close_stdin").and_then(Value::as_bool).unwrap_or(false);
        if text.is_empty() && !close {
            return ToolOutput::err("process_write: give `text` to send, or close_stdin.");
        }
        if let Err(e) = write(id, text, close).await {
            return ToolOutput::err(format!("process_write: {e}"));
        }
        tokio::time::sleep(WRITE_SETTLE).await;
        read_new(id).map_or_else(ToolOutput::err, ToolOutput::ok)
    }
}

struct ProcessWait;

#[async_trait::async_trait]
impl Tool for ProcessWait {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "process_wait".into(),
            description: "Wait for a background process instead of polling bash_output: until its new output \
                matches `pattern` (a regex), until TCP `port` on localhost accepts connections, or (with \
                neither) until it exits. Also returns when the process ends or after timeout_secs \
                (default 30, max 600). Returns why it stopped waiting plus the output since the last read."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "id": {"type": "integer", "description": ID_SCHEMA},
                    "pattern": {"type": "string", "description": "Regular expression to wait for in new output."},
                    "port": {"type": "integer", "description": "Wait until this localhost TCP port accepts connections."},
                    "timeout_secs": {"type": "integer", "description": "Give up after this many seconds (default 30, max 600)."}
                },
                "required": ["id"]
            }),
        }
    }

    fn summary(&self, args: &Value) -> String {
        let what = args
            .get("pattern")
            .and_then(Value::as_str)
            .map(|p| format!("/{p}/"))
            .or_else(|| args.get("port").map(|p| format!("port {p}")))
            .unwrap_or_else(|| "exit".into());
        format!("{} {what}", BashOutput.summary(args))
    }

    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolOutput {
        let id = match id_arg(&args, "process_wait") {
            Ok(id) => id,
            Err(out) => return out,
        };
        let pattern = match args.get("pattern").and_then(Value::as_str).filter(|p| !p.is_empty()) {
            Some(p) => match Regex::new(p) {
                Ok(re) => Some(re),
                Err(e) => return ToolOutput::err(format!("process_wait: invalid pattern {p:?}: {e}")),
            },
            None => None,
        };
        let port = match args.get("port").filter(|v| !v.is_null()) {
            Some(v) => match v.as_u64().and_then(|n| u16::try_from(n).ok()).filter(|n| *n > 0) {
                Some(n) => Some(n),
                None => return ToolOutput::err("process_wait: `port` must be a TCP port number."),
            },
            None => None,
        };
        let secs = args.get("timeout_secs").and_then(Value::as_u64).unwrap_or(WAIT_DEFAULT).clamp(1, WAIT_MAX);
        match wait(id, &WaitFor { pattern, port }, Duration::from_secs(secs), &ctx.cancel).await {
            Ok(text) => ToolOutput::ok(text),
            Err(e) => ToolOutput::err(format!("process_wait: {e}")),
        }
    }
}

struct BashOutput;

#[async_trait::async_trait]
impl Tool for BashOutput {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "bash_output".into(),
            description: "Read output from a background process started with bash run_in_background: returns \
                only what it printed since the last read, plus whether it is still running or its exit status."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {"id": {"type": "integer", "description": ID_SCHEMA}},
                "required": ["id"]
            }),
        }
    }

    fn summary(&self, args: &Value) -> String {
        args.get("id").map(|v| format!("#{}", v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string()))).unwrap_or_default()
    }

    async fn run(&self, args: Value, _ctx: &ToolCtx) -> ToolOutput {
        match id_arg(&args, "bash_output") {
            Ok(id) => read_new(id).map_or_else(ToolOutput::err, ToolOutput::ok),
            Err(out) => out,
        }
    }
}

struct KillShell;

#[async_trait::async_trait]
impl Tool for KillShell {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "kill_shell".into(),
            description: "Stop a background process started with bash run_in_background (its whole process group).".into(),
            parameters: json!({
                "type": "object",
                "properties": {"id": {"type": "integer", "description": ID_SCHEMA}},
                "required": ["id"]
            }),
        }
    }

    fn summary(&self, args: &Value) -> String {
        BashOutput.summary(args)
    }

    async fn run(&self, args: Value, _ctx: &ToolCtx) -> ToolOutput {
        match id_arg(&args, "kill_shell") {
            Ok(id) => kill(id).await.map_or_else(ToolOutput::err, |m| ToolOutput::ok(format!("Stopped {m}."))),
            Err(out) => out,
        }
    }
}

/// The `bash` result for a command started with `run_in_background`.
pub(crate) fn started_message(command: &str, cwd: &Path, sandbox: &crate::sandbox::Policy, interactive: bool) -> ToolOutput {
    match start(command, cwd, sandbox, interactive) {
        Ok(id) => {
            let input = if interactive { format!(" Send input with process_write {{\"id\": {id}, \"text\": \"…\\n\"}}.") } else { String::new() };
            ToolOutput::ok(format!(
                "Started background process {id}. Read its output with bash_output {{\"id\": {id}}}, or wait for a \
                 line, a port or its exit with process_wait {{\"id\": {id}, \"pattern\": \"…\"}}.{input} Stop it with \
                 kill_shell {{\"id\": {id}}}. It is killed when nyq exits."
            ))
        }
        Err(e) => ToolOutput::err(format!("bash: {e}")),
    }
}

pub fn register(r: &mut Registry) {
    r.add(Box::new(BashOutput));
    r.add(Box::new(ProcessWrite));
    r.add(Box::new(ProcessWait));
    r.add(Box::new(KillShell));
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn policy() -> crate::sandbox::Policy {
        crate::sandbox::Policy { mode: crate::sandbox::Mode::Off, ..Default::default() }
    }

    async fn wait_until(id: u32, f: impl Fn(&Proc) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !with_proc(id, |p| f(p)).unwrap() {
            assert!(Instant::now() < deadline, "timed out waiting on process {id}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn output_is_read_incrementally_and_exit_status_reported() {
        let dir = tempfile::tempdir().unwrap();
        let id = start("echo one; sleep 0.3; echo two; exit 3", dir.path(), &policy(), false).unwrap();
        wait_until(id, |p| String::from_utf8_lossy(&p.buf).contains("one")).await;
        let first = read_new(id).unwrap();
        assert!(first.contains("running for"), "{first}");
        assert!(first.contains("one") && !first.contains("two"), "{first}");
        wait_until(id, |p| p.state != State::Running).await;
        let second = read_new(id).unwrap();
        assert!(second.contains("exited (exit code 3)"), "{second}");
        assert!(second.contains("two") && !second.contains("one"), "{second}");
        let third = read_new(id).unwrap();
        assert!(third.ends_with("(no new output)"), "{third}");
        assert!(kill(id).await.unwrap_err().contains("not running"));
        assert!(list_lines().iter().any(|l| l.contains(&format!("#{id}")) && l.contains("exit 3")));
    }

    #[tokio::test]
    async fn kill_stops_the_process_group() {
        let dir = tempfile::tempdir().unwrap();
        let id = start("echo started; sleep 885533 & sleep 885533", dir.path(), &policy(), false).unwrap();
        wait_until(id, |p| !p.buf.is_empty()).await;
        let msg = kill(id).await.unwrap();
        assert!(msg.contains("killed"), "{msg}");
        assert!(read_new(id).unwrap().contains("killed after"));
        tokio::time::sleep(Duration::from_millis(200)).await;
        let pgrep = std::process::Command::new("pgrep").arg("-f").arg("885533").output().unwrap();
        assert!(pgrep.stdout.is_empty(), "leftover processes: {}", String::from_utf8_lossy(&pgrep.stdout));
        assert!(read_new(999_999).is_err());
    }

    #[tokio::test]
    async fn bash_tool_starts_in_background_and_buffer_is_capped() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolCtx {
            cwd: dir.path().into(),
            sandbox: Arc::new(policy()),
            ..Default::default()
        };
        let mut r = Registry::default();
        super::super::bash::register(&mut r);
        register(&mut r);
        let started = Instant::now();
        let out = r
            .get("bash")
            .unwrap()
            .run(json!({"command": "head -c 1500000 /dev/zero | tr '\\0' x; sleep 30", "run_in_background": true}), &ctx)
            .await;
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(!out.is_error, "{}", out.content);
        let id: u32 = out.content.split_whitespace().nth(3).unwrap().trim_end_matches('.').parse().unwrap();
        wait_until(id, |p| p.base + p.buf.len() as u64 >= 1_500_000).await;
        assert_eq!(with_proc(id, |p| p.buf.len()).unwrap(), BUF_CAP);
        let read = r.get("bash_output").unwrap().run(json!({"id": id}), &ctx).await;
        assert!(read.content.contains("bytes of earlier output were dropped"), "{}", &read.content[..200]);
        assert!(read.content.contains("[output truncated"));
        let killed = r.get("kill_shell").unwrap().run(json!({"id": format!("#{id}")}), &ctx).await;
        assert!(!killed.is_error, "{}", killed.content);
    }

    #[tokio::test]
    async fn interactive_input_and_waiting() {
        let dir = tempfile::tempdir().unwrap();
        let cancel = tokio_util::sync::CancellationToken::new();
        let id = start("while read line; do echo \"got $line\"; done; echo bye", dir.path(), &policy(), true).unwrap();
        write(id, "hello\n", false).await.unwrap();
        let cond = WaitFor { pattern: Some(Regex::new("got hel+o").unwrap()), port: None };
        let out = wait(id, &cond, Duration::from_secs(10), &cancel).await.unwrap();
        assert!(out.starts_with("[wait: the output matched the pattern]") && out.contains("got hello"), "{out}");
        write(id, "", true).await.unwrap();
        let exit = WaitFor { pattern: None, port: None };
        let out = wait(id, &exit, Duration::from_secs(10), &cancel).await.unwrap();
        assert!(out.contains("[wait: the process ended]") && out.contains("bye"), "{out}");
        assert!(write(id, "x", false).await.unwrap_err().contains("not running"));

        let quiet = start("sleep 30", dir.path(), &policy(), false).unwrap();
        assert!(write(quiet, "x", false).await.unwrap_err().contains("no open stdin"));
        let cond = WaitFor { pattern: Some(Regex::new("never").unwrap()), port: None };
        let out = wait(quiet, &cond, Duration::from_secs(1), &cancel).await.unwrap();
        assert!(out.contains("timed out after 1s"), "{out}");
        kill(quiet).await.unwrap();
    }

    #[tokio::test]
    async fn waits_for_a_port_and_notes_server_urls() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let dir = tempfile::tempdir().unwrap();
        let cmd = format!("printf '\\033[32m  Local:   http://localhost:{port}/\\033[0m\\n'; sleep 30");
        let id = start(&cmd, dir.path(), &policy(), false).unwrap();
        let cancel = tokio_util::sync::CancellationToken::new();
        let out = wait(id, &WaitFor { pattern: None, port: Some(port) }, Duration::from_secs(10), &cancel).await.unwrap();
        assert!(out.contains(&format!("port {port} is accepting")), "{out}");
        wait_until(id, |p| p.url.is_some()).await;
        let line = context_lines().into_iter().find(|l| l.starts_with(&format!("- #{id} "))).unwrap();
        assert!(line.contains(&format!("serving http://localhost:{port}/")), "{line}");
        kill(id).await.unwrap();
    }

    #[test]
    fn server_urls_are_found_in_colored_output() {
        assert_eq!(server_url("  ➜  Local:   \x1b[36mhttp://localhost:\x1b[1m5173\x1b[22m/\x1b[39m").as_deref(), Some("http://localhost:5173/"));
        assert_eq!(server_url("Listening on http://0.0.0.0:8000.").as_deref(), Some("http://0.0.0.0:8000"));
        assert_eq!(server_url("see https://docs.rs/tokio for docs"), None);
    }
}

#[cfg(test)]
mod retain {
    use super::*;

    fn proc(command: &str, ended: Option<Instant>, buf: &str, read: u64) -> Proc {
        Proc {
            id: 1,
            command: command.into(),
            cwd: PathBuf::new(),
            url: None,
            stdin: None,
            started: Instant::now(),
            ended,
            state: State::Exited(Some("exit code 0".into())),
            pid: 1,
            job: None,
            kill_requested: false,
            buf: buf.as_bytes().to_vec(),
            base: 0,
            read,
        }
    }

    #[test]
    fn read_finished_processes_are_dropped_and_unread_ones_stay() {
        let ended = Instant::now();
        let later = ended + DONE_KEEP;
        let mut procs = vec![
            proc("done", Some(ended), "all", 3),
            proc("pending", Some(ended), "still here", 0),
            proc("live", None, "running", 0),
        ];
        procs.retain(|p| !finished_and_read(p, later, DONE_KEEP));
        let commands: Vec<_> = procs.iter().map(|p| p.command.as_str()).collect();
        assert_eq!(commands, ["pending", "live"]);
        assert!(!finished_and_read(&procs[0], ended, DONE_KEEP));
    }
}
