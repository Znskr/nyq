//! One running extension process: newline-delimited JSON-RPC 2.0 over its
//! stdin/stdout, stderr appended to its log file.

use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

use super::protocol::Manifest;

/// How long a closing extension may take to exit after its stdin closes.
const GRACE: Duration = Duration::from_secs(2);

/// Answers a request or notification the extension sent: `(method, params)`.
pub type Handler =
    Arc<dyn Fn(String, Value) -> BoxFuture<'static, Result<Value, String>> + Send + Sync>;

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;

pub struct Conn {
    tx: Mutex<Option<mpsc::UnboundedSender<String>>>,
    pending: Pending,
    next: AtomicU64,
    /// Set by `close`: the exit that follows is expected.
    pub closing: Arc<AtomicBool>,
    /// Set once the extension's stdout closed (it exited or is exiting).
    gone: Arc<AtomicBool>,
    /// Dropping (or sending on) this kills the process.
    kill: Mutex<Option<oneshot::Sender<()>>>,
    exited: Mutex<Option<oneshot::Receiver<()>>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// `command[0]` relative to the extension's directory when it is a path
/// (`./bin/ext`), else looked up on PATH.
fn program(dir: &Path, cmd: &str) -> std::path::PathBuf {
    if cmd.starts_with("./")
        || cmd.starts_with("../")
        || (cmd.contains('/') && !Path::new(cmd).is_absolute())
    {
        dir.join(cmd)
    } else {
        cmd.into()
    }
}

impl Conn {
    /// Start the extension in `dir`. `on_exit` runs once the process has
    /// exited, with a description of how.
    pub fn spawn(
        dir: &Path,
        manifest: &Manifest,
        log: &Path,
        handler: Handler,
        on_exit: Box<dyn FnOnce(String) + Send>,
    ) -> anyhow::Result<Arc<Conn>> {
        if let Some(parent) = log.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let stderr = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)?;
        let mut cmd = tokio::process::Command::new(program(dir, &manifest.command[0]));
        cmd.args(&manifest.command[1..])
            .current_dir(dir)
            .envs(&manifest.env)
            .env("NYQ_EXTENSION_DIR", dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(stderr))
            .kill_on_drop(true);
        let mut child = cmd.spawn().map_err(|e| {
            anyhow::anyhow!("could not start `{}`: {e}", manifest.command.join(" "))
        })?;
        let mut stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        tokio::spawn(async move {
            while let Some(line) = rx.recv().await {
                if stdin.write_all(line.as_bytes()).await.is_err()
                    || stdin.write_all(b"\n").await.is_err()
                {
                    break;
                }
                let _ = stdin.flush().await;
            }
        });
        let pending: Pending = Default::default();
        let replies = tx.downgrade();
        let reader_pending = pending.clone();
        let gone = Arc::new(AtomicBool::new(false));
        let reader_gone = gone.clone();
        let reader = tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if let Some(method) = msg.get("method").and_then(Value::as_str) {
                    let fut = handler(
                        method.to_string(),
                        msg.get("params").cloned().unwrap_or(Value::Null),
                    );
                    let (id, replies) = (msg.get("id").cloned(), replies.clone());
                    tokio::spawn(async move {
                        let result = fut.await;
                        let (Some(id), Some(tx)) = (id, replies.upgrade()) else {
                            return;
                        };
                        let reply = match result {
                            Ok(v) => json!({"jsonrpc": "2.0", "id": id, "result": v}),
                            Err(e) => {
                                let code = if e.starts_with("unknown method") {
                                    -32601
                                } else {
                                    -32000
                                };
                                json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": e}})
                            }
                        };
                        let _ = tx.send(reply.to_string());
                    });
                } else if let Some(id) = msg.get("id").and_then(Value::as_u64) {
                    if let Some(waiter) = lock(&reader_pending).remove(&id) {
                        let result = match msg.get("error") {
                            Some(e) => Err(e
                                .get("message")
                                .and_then(Value::as_str)
                                .unwrap_or("error")
                                .to_string()),
                            None => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
                        };
                        let _ = waiter.send(result);
                    }
                }
            }
            // Fail whatever still waits for an answer.
            reader_gone.store(true, Ordering::SeqCst);
            lock(&reader_pending).clear();
        });
        let (kill_tx, kill_rx) = oneshot::channel::<()>();
        let (exited_tx, exited_rx) = oneshot::channel();
        let waiter_pending = pending.clone();
        tokio::spawn(async move {
            let status = tokio::select! {
                s = child.wait() => s,
                _ = kill_rx => {
                    let _ = child.start_kill();
                    child.wait().await
                }
            };
            let _ = tokio::time::timeout(GRACE, reader).await;
            lock(&waiter_pending).clear();
            let _ = exited_tx.send(());
            on_exit(match status {
                Ok(s) => s.to_string(),
                Err(e) => e.to_string(),
            });
        });
        Ok(Arc::new(Conn {
            tx: Mutex::new(Some(tx)),
            pending,
            next: AtomicU64::new(1),
            closing: Arc::new(AtomicBool::new(false)),
            gone,
            kill: Mutex::new(Some(kill_tx)),
            exited: Mutex::new(Some(exited_rx)),
        }))
    }

    /// Whether the process is still there to answer.
    pub fn alive(&self) -> bool {
        !self.gone.load(Ordering::SeqCst)
    }

    fn send(&self, msg: Value) -> Result<(), String> {
        let tx = lock(&self.tx).clone().ok_or("the extension is closing")?;
        tx.send(msg.to_string())
            .map_err(|_| "the extension is not running".to_string())
    }

    /// Send a request and wait up to `timeout` for its answer.
    pub async fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, String> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        lock(&self.pending).insert(id, tx);
        if let Err(e) =
            self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
        {
            lock(&self.pending).remove(&id);
            return Err(e);
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err("the extension exited before answering".into()),
            Err(_) => {
                lock(&self.pending).remove(&id);
                Err(format!("no answer within {}ms", timeout.as_millis()))
            }
        }
    }

    /// Close stdin and give the process a moment to exit, then kill it.
    pub async fn close(&self) {
        self.closing.store(true, Ordering::SeqCst);
        lock(&self.tx).take();
        let exited = lock(&self.exited).take();
        if let Some(exited) = exited {
            if tokio::time::timeout(GRACE, exited).await.is_err() {
                if let Some(kill) = lock(&self.kill).take() {
                    let _ = kill.send(());
                }
            }
        }
    }
}
