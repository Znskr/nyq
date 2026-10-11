//! Run shell commands via `bash -c` (falling back to `sh`) in their own process
//! group, with a timeout, cancellation, and merged output.

use std::io;
use std::process::Stdio;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::AsyncReadExt;
use tokio::process::Child;

use super::{Tool, ToolCtx, ToolOutput, Registry};
use crate::types::ToolSpec;

pub(super) const MAX_OUTPUT: usize = 30_000;
const DEFAULT_TIMEOUT: u64 = 120;
const MAX_TIMEOUT: u64 = 600;
/// How long to wait after the child exits for its output pipes to close, and
/// how long a terminated process group gets before we SIGKILL it.
pub(super) const GRACE: Duration = Duration::from_secs(2);

struct Bash;

#[async_trait::async_trait]
impl Tool for Bash {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "bash".into(),
            description: "Run a shell command in the workspace and return its combined stdout and \
                stderr. Long output keeps its beginning and end; the full text is stored and can be read \
                with read_tool_result. Non-zero exits are reported as errors with \"exit code N\" appended \
                (except benign ones such as grep finding no match or diff finding differences). The command runs in its own process group with a \
                timeout in seconds (default 120, max 600); on timeout or cancellation the whole \
                group is terminated. Set run_in_background for servers, watchers and other long-running \
                commands: it returns a process id at once; read new output with bash_output, wait for a line, \
                port or exit with process_wait, and stop it with kill_shell. Add interactive to drive a REPL \
                or prompt with process_write."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "The command line to run with bash -c."},
                    "timeout_secs": {"type": "integer", "description": "Kill the process group after this many seconds (default 120, max 600)."},
                    "run_in_background": {"type": "boolean", "description": "Start the command in the background and return its id immediately (no timeout)."},
                    "interactive": {"type": "boolean", "description": "With run_in_background: keep stdin open so process_write can send input (REPLs, prompts). Otherwise stdin is empty."}
                },
                "required": ["command"]
            }),
        }
    }

    fn needs_approval(&self, _args: &Value) -> bool {
        true
    }

    fn summary(&self, args: &Value) -> String {
        let cmd = args.get("command").and_then(Value::as_str).unwrap_or("");
        cmd.lines().next().unwrap_or("").chars().take(120).collect()
    }

    fn preview(&self, args: &Value, _ctx: &ToolCtx) -> Option<String> {
        args.get("command").and_then(Value::as_str).filter(|c| !c.is_empty()).map(str::to_string)
    }

    fn handles_cancel(&self) -> bool {
        true
    }

    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolOutput {
        let Some(command) = args.get("command").and_then(Value::as_str).filter(|c| !c.is_empty()) else {
            return ToolOutput::err("bash: missing required argument `command`.");
        };
        if args.get("run_in_background").and_then(Value::as_bool) == Some(true) {
            let interactive = args.get("interactive").and_then(Value::as_bool).unwrap_or(false);
            return super::background::started_message(command, &ctx.cwd, &ctx.sandbox, interactive);
        }
        let timeout = Duration::from_secs(
            args.get("timeout_secs")
                .and_then(|v| v.as_u64().or_else(|| v.as_f64().map(|f| f as u64)))
                .unwrap_or(DEFAULT_TIMEOUT)
                .clamp(1, MAX_TIMEOUT),
        );

        let store = Some((crate::config::home(), super::artifacts::session_key(ctx.session_id.as_deref())));
        run_command(command, &ctx.cwd, &ctx.sandbox, &ctx.cancel, Some(timeout), store).await
    }
}

/// Run a shell command, draining output into a bounded tail while it arrives.
/// `timeout` of `None` waits until the command exits or `cancel` fires.
pub(crate) async fn run_shell(
    command: &str,
    cwd: &std::path::Path,
    sandbox: &crate::sandbox::Policy,
    cancel: &tokio_util::sync::CancellationToken,
    timeout: Option<Duration>,
) -> ToolOutput {
    run_command(command, cwd, sandbox, cancel, timeout, None).await
}

async fn run_command(
    command: &str,
    cwd: &std::path::Path,
    sandbox: &crate::sandbox::Policy,
    cancel: &tokio_util::sync::CancellationToken,
    timeout: Option<Duration>,
    store: Option<(std::path::PathBuf, String)>,
) -> ToolOutput {
        let child = match spawn(command, cwd, sandbox, false) {
            Ok((child, job)) => (child, job),
            Err(e) => return ToolOutput::err(format!("bash: {e}")),
        };
        let job = child.1;
        let mut child = child.0;
        let pid = child.id().unwrap_or(0);

        let tail = std::sync::Arc::new(std::sync::Mutex::new(Capture { store, ..Capture::default() }));
        let mut pumps = Vec::new();
        if let Some(out) = child.stdout.take() {
            let tail = tail.clone();
            pumps.push(tokio::spawn(pump(out, tail)));
        }
        if let Some(err) = child.stderr.take() {
            let tail = tail.clone();
            pumps.push(tokio::spawn(pump(err, tail)));
        }

        enum Outcome {
            Exit(std::process::ExitStatus),
            Cancelled,
            TimedOut,
        }
        let outcome = match timeout {
            Some(timeout) => tokio::select! {
                status = child.wait() => Outcome::Exit(match status {
                    Ok(s) => s,
                    Err(e) => return ToolOutput::err(format!("bash: waiting for the command: {e}")),
                }),
                _ = cancel.cancelled() => Outcome::Cancelled,
                _ = tokio::time::sleep(timeout) => Outcome::TimedOut,
            },
            None => tokio::select! {
                status = child.wait() => Outcome::Exit(match status {
                    Ok(s) => s,
                    Err(e) => return ToolOutput::err(format!("bash: waiting for the command: {e}")),
                }),
                _ = cancel.cancelled() => Outcome::Cancelled,
            },
        };

        let (status, timed_out) = match outcome {
            Outcome::Exit(status) => {
                reap_leftovers(&mut child, pid, job.as_ref()).await;
                (Some(status), false)
            }
            Outcome::Cancelled => {
                terminate(&mut child, pid, job.as_ref()).await;
                abort_pumps(pumps).await;
                return ToolOutput::err("Cancelled by user.");
            }
            Outcome::TimedOut => (terminate(&mut child, pid, job.as_ref()).await, true),
        };

        finish_pumps(pumps).await;
        let bytes = tail.lock().unwrap_or_else(|e| e.into_inner()).render();
        let (exit, failed) = match status.as_ref().and_then(exit_note) {
            Some(note) => match status.as_ref().and_then(|s| s.code()).and_then(|code| benign_exit(command, code)) {
                Some(meaning) => (Some(format!("{note} ({meaning})")), false),
                None => (Some(note), true),
            },
            None => (None, false),
        };
        let is_error = timed_out || failed;
        let mut content = bytes;
        if timed_out {
            let secs = timeout.map(|d| d.as_secs()).unwrap_or(0);
            push_line(&mut content, &format!("command timed out after {secs}s (process group terminated)"));
        }
        if let Some(note) = &exit {
            push_line(&mut content, note);
        }
        if content.is_empty() {
            content = "(no output)".into();
        }
        let denied = ["Permission denied", "Read-only file system", "Operation not permitted", "Network is unreachable"];
        if is_error && sandbox.mode != crate::sandbox::Mode::Off && denied.iter().any(|d| content.contains(d)) {
            push_line(&mut content, "(The sandbox blocks writes outside the workspace and, unless allowed, network access. If this command needs that, ask the user to rerun nyq with --sandbox off or --allow-network.)");
        }
        ToolOutput { content, is_error, display: None, images: Vec::new(), changed: false, ..ToolOutput::default() }
}

/// Output kept from the start and the end of a long run. Together they stay
/// under the large-result limit, so the model sees both ends directly.
const HEAD_BYTES: usize = 6 * 1024;
const TAIL_BYTES: usize = 8 * 1024;

/// A command's output: its first and last bytes in memory and, once it
/// outgrows them, the whole stream in a stored tool result.
#[derive(Default)]
struct Capture {
    head: Vec<u8>,
    tail: Vec<u8>,
    dropped: usize,
    /// Home and session key for the stored result; None keeps only the ends.
    store: Option<(std::path::PathBuf, String)>,
    /// The stored result's id and open file once the output overflowed
    /// (the file is None after a write error).
    stored: Option<(String, Option<std::fs::File>)>,
}

impl Capture {
    fn push(&mut self, mut chunk: &[u8]) {
        if let Some((_, file)) = &mut self.stored {
            use std::io::Write;
            if file.as_mut().is_some_and(|f| f.write_all(chunk).is_err()) {
                *file = None;
            }
        }
        if self.head.len() < HEAD_BYTES {
            let n = chunk.len().min(HEAD_BYTES - self.head.len());
            self.head.extend_from_slice(&chunk[..n]);
            chunk = &chunk[n..];
        }
        self.tail.extend_from_slice(chunk);
        if self.tail.len() > TAIL_BYTES {
            if self.stored.is_none() {
                // Everything so far is still in memory: store it, then append.
                if let Some((home, key)) = &self.store {
                    let mut all = self.head.clone();
                    all.extend_from_slice(&self.tail);
                    self.stored = Some(match super::artifacts::store_open(home, key, "bash", &all) {
                        Ok((id, file)) => (id, Some(file)),
                        Err(_) => (String::new(), None),
                    });
                }
            }
            let extra = self.tail.len() - TAIL_BYTES;
            self.tail.drain(..extra);
            self.dropped += extra;
        }
    }

    fn render(&self) -> String {
        let head = String::from_utf8_lossy(&self.head);
        let tail = String::from_utf8_lossy(&self.tail);
        if self.dropped == 0 {
            return format!("{head}{tail}");
        }
        let note = match &self.stored {
            Some((id, Some(_))) => format!(
                "the full output is stored as tool result \"{id}\"; read it with {} {{\"id\": \"{id}\"}} (offset/limit or query)",
                super::artifacts::TOOL
            ),
            _ => "it was not kept".into(),
        };
        format!("{head}\n… [{} bytes of output omitted; {note}] …\n{tail}", self.dropped)
    }
}

/// What a non-zero exit means when it is not a failure: `grep` finding no
/// match, `diff` finding differences, `test` evaluating false.
fn benign_exit(command: &str, code: i32) -> Option<&'static str> {
    if code != 1 {
        return None;
    }
    // The exit status is the last pipeline stage's (without pipefail).
    let last = command.trim().rsplit(['|', ';', '\n']).next().unwrap_or("");
    let last = last.rsplit("&&").next().unwrap_or(last);
    let program = last
        .split_whitespace()
        .find(|w| !w.contains('=') && !matches!(*w, "command" | "sudo" | "!"))
        .map(crate::agent::program_base)?;
    match program {
        "grep" | "egrep" | "fgrep" | "rg" | "ag" => Some("no matches found"),
        "diff" | "cmp" => Some("the inputs differ"),
        "test" | "[" | "[[" => Some("the condition is false"),
        _ => None,
    }
}

pub(super) fn spawn(command: &str, cwd: &std::path::Path, sandbox: &crate::sandbox::Policy, stdin: bool) -> io::Result<(Child, Option<WinJob>)> {
    let mut cmd = crate::sandbox::shell_command(sandbox, cwd, command).map_err(io::Error::other)?;
    let input = if stdin { Stdio::piped() } else { Stdio::null() };
    cmd.stdin(input).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    #[cfg(unix)]
    {
        cmd.process_group(0); // its own group, so we can kill the whole tree
        Ok((cmd.spawn()?, None))
    }
    #[cfg(windows)]
    {
        // Suspended until the job assignment succeeds, so the process cannot
        // spawn a grandchild in the window before it is killable as a tree.
        const CREATE_SUSPENDED: u32 = 0x0000_0004;
        cmd.creation_flags(CREATE_SUSPENDED);
        let child = cmd.spawn()?;
        let Some(job) = assign_job_windows(&child) else {
            terminate_process(&child);
            return Err(io::Error::other(
                "could not assign the process to a kill-on-close job; refusing to run without process-tree termination",
            ));
        };
        if let Err(e) = resume_process(&child) {
            terminate_process(&child);
            unsafe { TerminateJobObject(job.handle, 1) };
            return Err(e);
        }
        Ok((child, Some(job)))
    }
}

async fn pump<R: tokio::io::AsyncRead + Unpin>(mut r: R, tail: std::sync::Arc<std::sync::Mutex<Capture>>) {
    let mut buf = [0u8; 8192];
    loop {
        match r.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if let Ok(mut tail) = tail.lock() {
                    tail.push(&buf[..n]);
                }
            }
        }
    }
}

pub(super) async fn finish_pumps(pumps: Vec<tokio::task::JoinHandle<()>>) {
    let deadline = tokio::time::Instant::now() + GRACE;
    for mut handle in pumps {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if tokio::time::timeout(left, &mut handle).await.is_err() {
            handle.abort();
            let _ = handle.await;
        }
    }
}

async fn abort_pumps(pumps: Vec<tokio::task::JoinHandle<()>>) {
    for handle in pumps {
        handle.abort();
        let _ = handle.await;
    }
}

/// SIGTERM (then SIGKILL) the whole process group and reap the child.
/// On Windows the job object kills every process assigned to it.
async fn terminate(child: &mut Child, pid: u32, job: Option<&WinJob>) -> Option<std::process::ExitStatus> {
    kill_group(pid, "TERM", job);
    if let Ok(status) = tokio::time::timeout(GRACE, child.wait()).await {
        return status.ok();
    }
    kill_group(pid, "KILL", job);
    let _ = child.kill().await;
    child.wait().await.ok()
}

pub(super) fn kill_group(pid: u32, signal: &str, job: Option<&WinJob>) {
    #[cfg(unix)]
    {
        let _ = job;
        if let Ok(pgid) = i32::try_from(pid) {
            if pgid > 0 {
                let sig = if signal == "KILL" { libc::SIGKILL } else { libc::SIGTERM };
                // SAFETY: plain syscall; a negative pid addresses the process group.
                unsafe { libc::kill(-pgid, sig) };
            }
        }
    }
    #[cfg(windows)]
    {
        let _ = (pid, signal);
        if let Some(job) = job {
            // SAFETY: `job.handle` is a live job object we own.
            unsafe { TerminateJobObject(job.handle, 1) };
        }
    }
}

/// Windows job that kills the whole process tree when closed or terminated.
/// On other platforms this is an empty placeholder.
#[cfg(windows)]
pub(super) struct WinJob {
    handle: *mut std::ffi::c_void,
}

#[cfg(not(windows))]
pub(super) struct WinJob;

#[cfg(windows)]
unsafe impl Send for WinJob {}
#[cfg(windows)]
unsafe impl Sync for WinJob {}

impl Drop for WinJob {
    fn drop(&mut self) {
        #[cfg(windows)]
        unsafe {
            CloseHandle(self.handle);
        }
    }
}

#[cfg(windows)]
fn assign_job_windows(child: &Child) -> Option<WinJob> {
    const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION: i32 = 9;
    const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x2000;
    #[repr(C)]
    struct BasicLimit {
        per_process_user_time_limit: i64,
        per_job_user_time_limit: i64,
        limit_flags: u32,
        minimum_working_set_size: usize,
        maximum_working_set_size: usize,
        active_process_limit: u32,
        affinity: usize,
        priority_class: u32,
        scheduling_class: u32,
    }
    #[repr(C)]
    struct ExtendedLimit {
        basic: BasicLimit,
        io_counters: [u64; 6],
        process_memory_limit: usize,
        job_memory_limit: usize,
        peak_process_memory_used: usize,
        peak_job_memory_used: usize,
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn CreateJobObjectW(attrs: *mut std::ffi::c_void, name: *const u16) -> *mut std::ffi::c_void;
        fn SetInformationJobObject(job: *mut std::ffi::c_void, class: i32, info: *const std::ffi::c_void, len: u32) -> i32;
        fn AssignProcessToJobObject(job: *mut std::ffi::c_void, process: *mut std::ffi::c_void) -> i32;
    }
    let job = unsafe { CreateJobObjectW(std::ptr::null_mut(), std::ptr::null()) };
    if job.is_null() {
        return None;
    }
    let mut info = ExtendedLimit {
        basic: BasicLimit {
            per_process_user_time_limit: 0,
            per_job_user_time_limit: 0,
            limit_flags: JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            minimum_working_set_size: 0,
            maximum_working_set_size: 0,
            active_process_limit: 0,
            affinity: 0,
            priority_class: 0,
            scheduling_class: 0,
        },
        io_counters: [0; 6],
        process_memory_limit: 0,
        job_memory_limit: 0,
        peak_process_memory_used: 0,
        peak_job_memory_used: 0,
    };
    let set = unsafe {
        SetInformationJobObject(
            job,
            JOB_OBJECT_EXTENDED_LIMIT_INFORMATION,
            (&mut info as *mut ExtendedLimit).cast(),
            std::mem::size_of::<ExtendedLimit>() as u32,
        )
    };
    let assigned = set != 0
        && child.raw_handle().is_some_and(|h| unsafe { AssignProcessToJobObject(job, h.cast()) } != 0);
    if !assigned {
        unsafe { CloseHandle(job) };
        return None;
    }
    Some(WinJob { handle: job })
}

#[cfg(windows)]
#[link(name = "kernel32")]
extern "system" {
    fn CloseHandle(handle: *mut std::ffi::c_void) -> i32;
    fn TerminateJobObject(job: *mut std::ffi::c_void, exit_code: u32) -> i32;
    fn TerminateProcess(process: *mut std::ffi::c_void, exit_code: u32) -> i32;
}

#[cfg(windows)]
fn resume_process(child: &Child) -> io::Result<()> {
    let Some(handle) = child.raw_handle() else {
        return Err(io::Error::other("missing process handle"));
    };
    #[link(name = "ntdll")]
    extern "system" {
        fn NtResumeProcess(process: *mut std::ffi::c_void) -> i32;
    }
    // SAFETY: `handle` is the child process, still suspended from creation.
    let status = unsafe { NtResumeProcess(handle.cast()) };
    if status < 0 {
        Err(io::Error::other(format!("could not resume the process (status {status:#x})")))
    } else {
        Ok(())
    }
}

#[cfg(windows)]
fn terminate_process(child: &Child) {
    if let Some(handle) = child.raw_handle() {
        // SAFETY: `handle` is the child process we are abandoning.
        unsafe { TerminateProcess(handle.cast(), 1) };
    }
}

/// After the shell leader has exited, stop anything still in its process group.
pub(super) async fn reap_leftovers(child: &mut Child, pid: u32, job: Option<&WinJob>) {
    #[cfg(not(unix))]
    let _ = pid;
    #[cfg(unix)]
    {
        let _ = (child, job);
        if !group_alive(pid) {
            return;
        }
        kill_group(pid, "TERM", None);
        let deadline = tokio::time::Instant::now() + GRACE;
        while group_alive(pid) && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        if group_alive(pid) {
            kill_group(pid, "KILL", None);
        }
    }
    #[cfg(windows)]
    {
        let _ = child;
        if let Some(job) = job {
            // SAFETY: `job.handle` is a live job object we own.
            unsafe { TerminateJobObject(job.handle, 1) };
        }
    }
}

#[cfg(unix)]
fn group_alive(pid: u32) -> bool {
    let Ok(pgid) = i32::try_from(pid) else { return false };
    if pgid <= 0 {
        return false;
    }
    // SAFETY: signal 0 only probes; a negative pid addresses the process group.
    unsafe { libc::kill(-pgid, 0) == 0 }
}

/// Lossy-decode and keep only the last `MAX_OUTPUT` chars, with a note.
pub(super) fn render_output(bytes: &[u8], dropped_bytes: usize) -> String {
    let text = String::from_utf8_lossy(bytes);
    let len = text.chars().count();
    let skipped = len.saturating_sub(MAX_OUTPUT);
    if dropped_bytes == 0 && skipped == 0 {
        return text.into_owned();
    }
    let kept: String = text.chars().skip(skipped).collect();
    format!("[output truncated: first {skipped} chars dropped]\n{kept}")
}

/// Appended to output when the exit status is not clean success.
pub(super) fn exit_note(status: &std::process::ExitStatus) -> Option<String> {
    if status.success() {
        return None;
    }
    #[cfg(unix)]
    if let Some(sig) = std::os::unix::process::ExitStatusExt::signal(status) {
        return Some(format!("killed by signal {sig}"));
    }
    Some(format!("exit code {}", status.code().unwrap_or(-1)))
}

fn push_line(s: &mut String, line: &str) {
    if !s.is_empty() {
        s.push('\n');
    }
    s.push_str(line);
}

pub fn register(r: &mut Registry) {
    r.add(Box::new(Bash));
}

// These use POSIX commands and process groups.
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::time::Instant;
    use tokio_util::sync::CancellationToken;

    fn make_ctx(dir: &std::path::Path, cancel: &CancellationToken) -> ToolCtx {
        // Sandbox enforcement is covered by tests/sandbox.rs, which can
        // re-exec the real binary; here the test harness is current_exe().
        let sandbox = std::sync::Arc::new(crate::sandbox::Policy { mode: crate::sandbox::Mode::Off, ..Default::default() });
        ToolCtx { cwd: dir.to_path_buf(), cancel: cancel.clone(), sandbox, ..Default::default() }
    }

    async fn run(args: Value) -> ToolOutput {
        let dir = tempfile::tempdir().unwrap();
        let ctx = make_ctx(dir.path(), &CancellationToken::new());
        Bash.run(args, &ctx).await
    }

    #[tokio::test]
    async fn echo_and_exit_codes() {
        let out = run(json!({"command": "echo hello; echo world"})).await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("hello") && out.content.contains("world"));

        let out = run(json!({"command": "echo oops >&2; echo out"})).await;
        assert!(!out.is_error);
        assert!(out.content.contains("oops") && out.content.contains("out"));

        let out = run(json!({"command": "echo before; exit 3"})).await;
        assert!(out.is_error);
        assert!(out.content.contains("before"));
        assert!(out.content.ends_with("exit code 3"));

        let out = run(json!({"command": "exit 0"})).await;
        assert!(!out.is_error);
        assert_eq!(out.content, "(no output)");
        assert!(run(json!({})).await.is_error);
        assert!(run(json!({"command": ""})).await.is_error);
    }

    #[tokio::test]
    async fn cwd_is_respected() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = make_ctx(dir.path(), &CancellationToken::new());
        let out = Bash.run(json!({"command": "pwd -P"}), &ctx).await;
        assert!(!out.is_error);
        assert_eq!(out.content.trim(), dir.path().canonicalize().unwrap().display().to_string());
    }

    #[tokio::test]
    async fn timeout_kills_the_process_group() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = make_ctx(dir.path(), &CancellationToken::new());
        let started = Instant::now();
        let out = Bash
            .run(
                json!({"command": "sleep 553399 & sleep 553399 & sleep 553399", "timeout_secs": 1}),
                &ctx,
            )
            .await;
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(out.is_error);
        assert!(out.content.contains("timed out after 1s"), "{}", out.content);

        // The background sleeps (same process group) died with the leader.
        tokio::time::sleep(Duration::from_millis(200)).await;
        let pgrep = std::process::Command::new("pgrep").arg("-f").arg("553399").output().unwrap();
        assert!(pgrep.stdout.is_empty(), "leftover processes: {}", String::from_utf8_lossy(&pgrep.stdout));
    }

    #[tokio::test]
    async fn cancellation_kills_the_process_group() {
        let cancel = CancellationToken::new();
        let dir = tempfile::tempdir().unwrap();
        let ctx = make_ctx(dir.path(), &cancel);
        let started = Instant::now();
        let handle = tokio::spawn(async move { Bash.run(json!({"command": "sleep 774411 & sleep 774411"}), &ctx).await });
        tokio::time::sleep(Duration::from_millis(200)).await;
        cancel.cancel();
        let out = handle.await.unwrap();
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(out.is_error);
        assert!(out.content.contains("Cancelled"));

        tokio::time::sleep(Duration::from_millis(200)).await;
        let pgrep = std::process::Command::new("pgrep").arg("-f").arg("774411").output().unwrap();
        assert!(pgrep.stdout.is_empty(), "leftover processes: {}", String::from_utf8_lossy(&pgrep.stdout));
    }

    #[tokio::test]
    async fn background_command_does_not_outlive_the_tool() {
        let started = Instant::now();
        let out = run(json!({"command": "sleep 664422 &"})).await;
        assert!(!out.is_error, "{}", out.content);
        assert!(started.elapsed() < Duration::from_secs(5), "reap waited too long");
        tokio::time::sleep(Duration::from_millis(200)).await;
        let pgrep = std::process::Command::new("pgrep").arg("-f").arg("664422").output().unwrap();
        assert!(pgrep.stdout.is_empty(), "leftover processes: {}", String::from_utf8_lossy(&pgrep.stdout));
    }

    #[tokio::test]
    async fn long_output_keeps_both_ends_and_stores_the_rest() {
        let home = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let ctx = make_ctx(dir.path(), &CancellationToken::new());
        let store = Some((home.path().to_path_buf(), "s1".to_string()));
        let command = "echo FIRST-ERROR; seq 1 20000; echo LAST-LINE";
        let out = run_command(command, dir.path(), &ctx.sandbox, &ctx.cancel, None, store).await;
        assert!(!out.is_error);
        assert!(out.content.starts_with("FIRST-ERROR\n1\n"), "{}", &out.content[..60]);
        assert!(out.content.trim_end().ends_with("LAST-LINE"));
        assert!(out.content.len() < super::super::artifacts::SPILL_AT, "{}", out.content.len());
        let id = out.content.split("tool result \"").nth(1).unwrap().split('"').next().unwrap();
        let full = super::super::artifacts::read(home.path(), "s1", id, 0, 1 << 20, Some("10000")).unwrap();
        assert!(full.contains("10000"), "{full}");
    }

    #[tokio::test]
    async fn benign_exit_codes_are_not_errors() {
        let out = run(json!({"command": "echo a | grep zzz"})).await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("exit code 1 (no matches found)"), "{}", out.content);
        let out = run(json!({"command": "grep -q x /nonexistent-file-nyq"})).await;
        assert!(out.is_error, "exit 2 is a real failure: {}", out.content);
        assert_eq!(benign_exit("FOO=1 diff a b", 1), Some("the inputs differ"));
        assert_eq!(benign_exit("cd x && [ -f y ]", 1), Some("the condition is false"));
        assert_eq!(benign_exit("grep x | wc -l", 1), None);
        assert_eq!(benign_exit("make", 1), None);
    }

    #[tokio::test]
    async fn summary_specs_and_defaults() {
        assert!(Bash.needs_approval(&json!({"command": "ls"})));
        assert_eq!(Bash.summary(&json!({"command": "first\nsecond"})), "first");
        assert_eq!(Bash.summary(&json!({"command": "x".repeat(200) + "\nrest"})).chars().count(), 120);
        assert_eq!(Bash.summary(&json!({})), "");
        let hidden = "cargo test\nrm -rf /";
        assert_eq!(Bash.summary(&json!({"command": hidden})), "cargo test");
        assert_eq!(Bash.preview(&json!({"command": hidden}), &ToolCtx::default()).as_deref(), Some(hidden));
        let spec = Bash.spec();
        assert_eq!(spec.name, "bash");
        assert_eq!(spec.parameters["required"], json!(["command"]));
        assert_eq!(spec.parameters["properties"]["timeout_secs"]["type"], "integer");
    }

    #[tokio::test]
    async fn grace_escalates_to_kill() {
        // TERM is ignored, so the SIGKILL fallback must still reap the group.
        let started = Instant::now();
        let out = run(json!({"command": "trap '' TERM; while :; do :; done", "timeout_secs": 1})).await;
        assert!(out.is_error);
        assert!(out.content.contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(10));
    }
}
