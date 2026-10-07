//! Run shell commands via `bash -c` (falling back to `sh`) in their own process
//! group, with a timeout, cancellation, and merged output.

use std::io;
use std::process::Stdio;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::AsyncReadExt;
use tokio::process::Child;
use tokio::sync::mpsc;

use super::{Tool, ToolCtx, ToolOutput, Registry};
use crate::types::ToolSpec;

const MAX_OUTPUT: usize = 30_000;
const DEFAULT_TIMEOUT: u64 = 120;
const MAX_TIMEOUT: u64 = 600;
/// How long to wait after the child exits for its output pipes to close, and
/// how long a terminated process group gets before we SIGKILL it.
const GRACE: Duration = Duration::from_secs(2);

struct Bash;

#[async_trait::async_trait]
impl Tool for Bash {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "bash".into(),
            description: "Run a shell command in the workspace and return its combined stdout and \
                stderr (truncated to the last 30000 chars). Non-zero exits are reported as errors \
                with \"exit code N\" appended. The command runs in its own process group with a \
                timeout in seconds (default 120, max 600); on timeout or cancellation the whole \
                group is terminated."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "The command line to run with bash -c."},
                    "timeout_secs": {"type": "integer", "description": "Kill the process group after this many seconds (default 120, max 600)."}
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

    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolOutput {
        let Some(command) = args.get("command").and_then(Value::as_str).filter(|c| !c.is_empty()) else {
            return ToolOutput::err("bash: missing required argument `command`.");
        };
        let timeout = Duration::from_secs(
            args.get("timeout_secs")
                .and_then(|v| v.as_u64().or_else(|| v.as_f64().map(|f| f as u64)))
                .unwrap_or(DEFAULT_TIMEOUT)
                .clamp(1, MAX_TIMEOUT),
        );

        let mut child = match spawn(command, ctx) {
            Ok(c) => c,
            Err(e) => return ToolOutput::err(format!("bash: {e}")),
        };
        let pid = child.id().unwrap_or(0);

        // Pump both pipes into one channel so output is merged in arrival order.
        let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
        if let Some(out) = child.stdout.take() {
            tokio::spawn(pump(out, tx.clone()));
        }
        if let Some(err) = child.stderr.take() {
            tokio::spawn(pump(err, tx.clone()));
        }
        drop(tx);

        enum Outcome {
            Exit(std::process::ExitStatus),
            Cancelled,
            TimedOut,
        }
        let outcome = tokio::select! {
            status = child.wait() => Outcome::Exit(match status {
                Ok(s) => s,
                Err(e) => return ToolOutput::err(format!("bash: waiting for the command: {e}")),
            }),
            _ = ctx.cancel.cancelled() => Outcome::Cancelled,
            _ = tokio::time::sleep(timeout) => Outcome::TimedOut,
        };

        let (status, timed_out) = match outcome {
            Outcome::Exit(status) => (Some(status), false),
            Outcome::Cancelled => {
                terminate(&mut child, pid).await;
                return ToolOutput::err("Cancelled by user.");
            }
            Outcome::TimedOut => (terminate(&mut child, pid).await, true),
        };

        let output = drain(&mut rx, GRACE).await;
        let bytes = render_output(output);
        let exit = status.as_ref().and_then(exit_note);
        let is_error = timed_out || exit.is_some();
        let mut content = bytes;
        if timed_out {
            push_line(&mut content, &format!("command timed out after {}s (process group terminated)", timeout.as_secs()));
        }
        if let Some(note) = &exit {
            push_line(&mut content, note);
        }
        if content.is_empty() {
            content = "(no output)".into();
        }
        let denied = ["Permission denied", "Read-only file system", "Operation not permitted", "Network is unreachable"];
        if is_error && ctx.sandbox.mode != crate::sandbox::Mode::Off && denied.iter().any(|d| content.contains(d)) {
            push_line(&mut content, "(The sandbox blocks writes outside the workspace and, unless allowed, network access. If this command needs that, ask the user to rerun nyq with --sandbox off or --allow-network.)");
        }
        ToolOutput { content, is_error, display: None, images: Vec::new() }
    }
}

fn spawn(command: &str, ctx: &ToolCtx) -> io::Result<Child> {
    let mut cmd = crate::sandbox::shell_command(&ctx.sandbox, &ctx.cwd, command).map_err(io::Error::other)?;
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0); // its own group, so we can kill the whole tree
    cmd.spawn()
}

async fn pump<R: tokio::io::AsyncRead + Unpin>(mut r: R, tx: mpsc::UnboundedSender<Vec<u8>>) {
    let mut buf = [0u8; 8192];
    loop {
        match r.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if tx.send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
        }
    }
}

/// SIGTERM (then SIGKILL) the whole process group and reap the child.
async fn terminate(child: &mut Child, pid: u32) -> Option<std::process::ExitStatus> {
    kill_group(pid, "TERM");
    if let Ok(status) = tokio::time::timeout(GRACE, child.wait()).await {
        return status.ok();
    }
    kill_group(pid, "KILL");
    let _ = child.kill().await;
    child.wait().await.ok()
}

fn kill_group(pid: u32, signal: &str) {
    if pid == 0 || cfg!(not(unix)) {
        return;
    }
    let _ = std::process::Command::new("kill")
        .args([signal, "--", &format!("-{pid}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Collect remaining output; pipes still open after `grace` are abandoned.
async fn drain(rx: &mut mpsc::UnboundedReceiver<Vec<u8>>, grace: Duration) -> Vec<u8> {
    let mut out = Vec::new();
    let deadline = tokio::time::sleep(grace);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            chunk = rx.recv() => match chunk {
                Some(c) => out.extend_from_slice(&c),
                None => break,
            },
            _ = &mut deadline => break,
        }
    }
    out
}

/// Lossy-decode and keep only the last `MAX_OUTPUT` chars, with a note.
fn render_output(bytes: Vec<u8>) -> String {
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let len = text.chars().count();
    if len <= MAX_OUTPUT {
        return text;
    }
    let skipped = len - MAX_OUTPUT;
    format!("[output truncated: first {skipped} chars dropped]\n{}", text.chars().skip(skipped).collect::<String>())
}

/// Appended to output when the exit status is not clean success.
fn exit_note(status: &std::process::ExitStatus) -> Option<String> {
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
        ToolCtx { cwd: dir.to_path_buf(), cancel: cancel.clone(), sandbox }
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
    async fn output_truncated_to_last_30k() {
        let out = run(json!({"command": "printf 'x%.0s' $(seq 1 20000); printf 'z%.0s' $(seq 1 20000)"})).await;
        assert!(!out.is_error);
        assert!(out.content.starts_with("[output truncated: first "), "{}", &out.content[..60]);
        assert_eq!(out.content.matches('x').count(), 10_000);
        assert_eq!(out.content.matches('z').count(), 20_000);
    }

    #[tokio::test]
    async fn summary_specs_and_defaults() {
        assert!(Bash.needs_approval(&json!({"command": "ls"})));
        assert_eq!(Bash.summary(&json!({"command": "first\nsecond"})), "first");
        assert_eq!(Bash.summary(&json!({"command": "x".repeat(200) + "\nrest"})).chars().count(), 120);
        assert_eq!(Bash.summary(&json!({})), "");
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
