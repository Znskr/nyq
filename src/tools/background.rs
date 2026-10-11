//! Background shell processes: `bash` with `run_in_background` starts one and
//! returns its id; `bash_output` returns what it printed since the last read
//! plus its status; `kill_shell` stops it. Output is kept in memory (the last
//! [`BUF_CAP`] bytes per process) and every process is killed when nyq exits.

use std::path::Path;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::io::AsyncReadExt;

use super::bash::{exit_note, finish_pumps, kill_group, reap_leftovers, render_output, spawn, WinJob, GRACE};
use super::{Registry, Tool, ToolCtx, ToolOutput};
use crate::types::ToolSpec;

/// Output kept per process; older bytes are dropped (and reported as such).
const BUF_CAP: usize = 1024 * 1024;
/// A finished process whose output has all been read is dropped after this.
const DONE_KEEP: Duration = Duration::from_secs(60);

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
pub(crate) fn start(command: &str, cwd: &Path, sandbox: &crate::sandbox::Policy) -> Result<u32, String> {
    let (mut child, job) = spawn(command, cwd, sandbox).map_err(|e| e.to_string())?;
    let pid = child.id().unwrap_or(0);
    let job = job.map(Arc::new);
    let id = {
        let mut t = table();
        compact_finished(&mut t);
        t.next += 1;
        let id = t.next;
        t.procs.push(Proc {
            id,
            command: command.to_string(),
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

/// Kill every running background process (on exit).
pub(crate) fn kill_all() {
    for p in table().procs.iter_mut().filter(|p| p.state == State::Running) {
        p.kill_requested = true;
        kill_group(p.pid, "KILL", p.job.as_deref());
    }
}

/// Stop every background process and forget them (`/reset`).
pub(crate) fn forget_all() -> usize {
    kill_all();
    let mut t = table();
    let n = t.procs.len();
    t.procs.clear();
    n
}

/// Drop ANSI escape sequences (colors, cursor moves) from terminal output.
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        if chars.next_if_eq(&'[').is_some() {
            for n in chars.by_ref() {
                if ('@'..='~').contains(&n) {
                    break;
                }
            }
        } else if chars.next_if_eq(&']').is_some() {
            // OSC: up to BEL or ESC \.
            while let Some(n) = chars.next() {
                if n == '\x07' || (n == '\x1b' && chars.next_if_eq(&'\\').is_some()) {
                    break;
                }
            }
        } else {
            chars.next();
        }
    }
    out
}

/// The last local server URL (`http://localhost:3000`, `http://127.0.0.1:8080/`,
/// `http://0.0.0.0:5173`) the output mentions, ready to open.
fn server_url(text: &str) -> Option<String> {
    let mut found = None;
    for (i, _) in text.match_indices("http") {
        let rest = &text[i..];
        if !(rest.starts_with("http://") || rest.starts_with("https://")) {
            continue;
        }
        let end = rest
            .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>' | ')' | ']' | '`' | ','))
            .unwrap_or(rest.len());
        let url = rest[..end].trim_end_matches(['.', ';', ':']);
        let host = url.split("://").nth(1).unwrap_or("").split(['/', '?', '#']).next().unwrap_or("");
        let name = host.rsplit_once(':').map_or(host, |(h, _)| h);
        if matches!(name, "localhost" | "127.0.0.1" | "0.0.0.0" | "[::1]" | "[::]") {
            found = Some(url.replacen("0.0.0.0", "localhost", 1).replacen("[::]", "localhost", 1));
        }
    }
    found
}

/// Summary lines for `/background logs`: status, the first and last 20
/// lines of output, and a detected server URL.
fn log_lines(id: u32) -> Result<Vec<String>, String> {
    with_proc(id, |p| {
        let text = strip_ansi(&String::from_utf8_lossy(&p.buf));
        let cmd: String = p.command.lines().next().unwrap_or("").chars().take(80).collect();
        let mut out = vec![format!("#{} {} · {cmd}", p.id, p.status())];
        if p.base > 0 {
            out.push(format!("[{} earlier bytes were dropped]", p.base));
        }
        let lines: Vec<&str> = text.lines().collect();
        if lines.is_empty() {
            out.push("(no output yet)".into());
        } else if lines.len() <= 40 {
            out.extend(lines.iter().map(|l| l.to_string()));
        } else {
            out.extend(lines[..20].iter().map(|l| l.to_string()));
            out.push(format!("… {} more lines …", lines.len() - 40));
            out.extend(lines[lines.len() - 20..].iter().map(|l| l.to_string()));
        }
        if let Some(url) = server_url(&text) {
            out.push(format!("url: {url}"));
        }
        out
    })
    .ok_or_else(|| format!("no background process with id {id}"))
}

/// The server URL process `id` printed, and whether it is still running.
fn url_of(id: u32) -> Result<(String, bool), String> {
    with_proc(id, |p| {
        let url = server_url(&strip_ansi(&String::from_utf8_lossy(&p.buf)));
        url.map(|u| (u, p.state == State::Running)).ok_or_else(|| format!("process {id} has not printed a local server URL"))
    })
    .ok_or_else(|| format!("no background process with id {id}"))?
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
            let url = server_url(&strip_ansi(&String::from_utf8_lossy(&p.buf)))
                .map(|u| format!("  url: {u}"))
                .unwrap_or_default();
            format!("  #{:<3} {:<10} {:>7}  {cmd}{url}", p.id, p.short_state(), duration(p.runtime()))
        })
        .collect()
}

/// `/background`, `/background logs|open|kill ID|last`.
pub(crate) async fn slash(args: &[&str]) {
    let id = |which: &str| parse_id(&Value::String(which.to_string()));
    match args {
        [] | ["list"] => crate::ui::list(&list_lines()),
        ["logs" | "log", which] => match id(which).ok_or_else(|| "usage: /background logs ID|last".to_string()).and_then(log_lines) {
            Ok(lines) => crate::ui::list(&lines),
            Err(e) => crate::ui::error(&e),
        },
        ["open", which] => match id(which).ok_or_else(|| "usage: /background open ID|last".to_string()).and_then(url_of) {
            Ok((url, running)) => {
                crate::login::open_browser(&url);
                crate::ui::info(&format!("  Opened {url}"));
                if !running {
                    crate::ui::info("  That process has stopped, so the page may not load.");
                }
            }
            Err(e) => crate::ui::error(&e),
        },
        ["kill" | "stop", which] => match parse_id(&Value::String(which.to_string())) {
            Some(id) => match kill(id).await {
                Ok(msg) => crate::ui::info(&format!("  Stopped {msg}")),
                Err(e) => crate::ui::error(&e),
            },
            None => crate::ui::error("usage: /background kill ID|last"),
        },
        _ => crate::ui::error("usage: /background [list] | /background logs|open|kill ID|last"),
    }
}

fn id_arg(args: &Value, tool: &str) -> Result<u32, ToolOutput> {
    args.get("id").and_then(parse_id).ok_or_else(|| ToolOutput::err(format!("{tool}: missing or invalid `id` (a background process id)")))
}

const ID_SCHEMA: &str = "Background process id returned by bash with run_in_background.";

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
pub(crate) fn started_message(command: &str, cwd: &Path, sandbox: &crate::sandbox::Policy) -> ToolOutput {
    match start(command, cwd, sandbox) {
        Ok(id) => ToolOutput::ok(format!(
            "Started background process {id}. Read its output with bash_output {{\"id\": {id}}}; \
             stop it with kill_shell {{\"id\": {id}}}. It is killed when nyq exits."
        )),
        Err(e) => ToolOutput::err(format!("bash: {e}")),
    }
}

pub fn register(r: &mut Registry) {
    r.add(Box::new(BashOutput));
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
        let id = start("echo one; sleep 0.3; echo two; exit 3", dir.path(), &policy()).unwrap();
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

    #[test]
    fn server_urls_and_escapes() {
        assert_eq!(strip_ansi("\x1b[1;32mready\x1b[0m \x1b]8;;x\x07link"), "ready link");
        let out = "Local:   \x1b[36mhttp://localhost:5173/\x1b[39m\n  Network: http://192.168.1.4:5173/";
        assert_eq!(server_url(&strip_ansi(out)).as_deref(), Some("http://localhost:5173/"));
        assert_eq!(server_url("Listening on http://0.0.0.0:8000.").as_deref(), Some("http://localhost:8000"));
        assert_eq!(server_url("see https://example.com/docs").as_deref(), None);
        assert_eq!(server_url("a http://127.0.0.1:1 then http://localhost:2/x").as_deref(), Some("http://localhost:2/x"));
    }

    #[tokio::test]
    async fn logs_show_head_tail_and_url() {
        let dir = tempfile::tempdir().unwrap();
        let id = start("for i in $(seq 1 60); do echo line$i; done; echo 'at http://localhost:4321'", dir.path(), &policy()).unwrap();
        wait_until(id, |p| p.state != State::Running).await;
        let lines = log_lines(id).unwrap();
        assert!(lines[0].starts_with(&format!("#{id} exited with code 0")), "{lines:?}");
        assert_eq!(lines[1], "line1");
        assert!(lines.contains(&"… 21 more lines …".to_string()), "{lines:?}");
        assert_eq!(lines.last().unwrap(), "url: http://localhost:4321");
        assert_eq!(url_of(id).unwrap(), ("http://localhost:4321".to_string(), false));
        assert!(list_lines().iter().any(|l| l.contains(&format!("#{id}")) && l.ends_with("url: http://localhost:4321")));
        assert!(log_lines(999_999).is_err());
    }

    #[tokio::test]
    async fn kill_stops_the_process_group() {
        let dir = tempfile::tempdir().unwrap();
        let id = start("echo started; sleep 885533 & sleep 885533", dir.path(), &policy()).unwrap();
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
}

#[cfg(test)]
mod retain {
    use super::*;

    fn proc(command: &str, ended: Option<Instant>, buf: &str, read: u64) -> Proc {
        Proc {
            id: 1,
            command: command.into(),
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
