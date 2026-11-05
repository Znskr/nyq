//! Extensions: programs in any language that add tools, slash commands,
//! event handlers and a status-line segment to nyq.
//!
//! An extension is a directory with an `extension.json` manifest, found in
//! `<home>/extensions/<name>/` and the project's `.nyq/extensions/<name>/`.
//! nyq starts each enabled one in the background and talks JSON-RPC 2.0 to
//! it over stdio, one message per line; its stderr goes to
//! `<home>/logs/ext-<name>.log`. Project extensions start only once trusted
//! (`/extensions trust` records the directory's content hash in
//! `<home>/trusted-extensions.json`); `"extensions": {"name": false}` in
//! settings.toml disables one. A crashed extension is restarted once.
//!
//! The wire types are defined in [`protocol`].

pub mod cli;
mod conn;
pub mod protocol;

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

use crate::tools::{LiveTools, Registry, Tool, ToolCtx, ToolOutput};
use crate::types::ToolSpec;
use conn::Conn;
use protocol::{CommandDef, Event, Manifest, Registration, ToolDef};

/// Model-facing tool names: `ext__<extension>__<tool>`.
pub const TOOL_PREFIX: &str = "ext__";
pub const MANIFEST: &str = "extension.json";
pub const STARTUP_TIMEOUT: Duration = Duration::from_secs(15);
const TOOL_TIMEOUT: Duration = Duration::from_secs(600);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(600);
/// How often a crashed extension is started again before it is reported.
const RESTARTS: u32 = 1;
/// Directories the trust hash skips (dependencies installed in place).
const UNHASHED: &[&str] = &[".git", "node_modules", "__pycache__", ".venv"];

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub fn user_dir(home: &Path) -> PathBuf {
    home.join("extensions")
}

pub fn project_dir(cwd: &Path) -> PathBuf {
    cwd.join(".nyq").join("extensions")
}

pub fn log_path(home: &Path, name: &str) -> PathBuf {
    home.join("logs").join(format!("ext-{name}.log"))
}

fn trust_file(home: &Path) -> PathBuf {
    home.join("trusted-extensions.json")
}

/// Append a line to an extension's log.
fn log(home: &Path, name: &str, text: &str) {
    use std::io::Write;
    let path = log_path(home, name);
    let _ = std::fs::create_dir_all(path.parent().unwrap_or(home));
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(
            f,
            "[nyq {}] {text}",
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
        );
    }
}

/// An extension directory found on disk.
#[derive(Clone, Debug)]
pub struct Found {
    pub manifest: Manifest,
    pub dir: PathBuf,
    /// From the project's `.nyq/extensions`.
    pub project: bool,
    /// Content hash of the directory (project extensions only).
    pub hash: Option<String>,
    pub trusted: bool,
    pub enabled: bool,
}

/// Names disabled with `[extensions]` `name = false` in settings.toml.
fn disabled() -> HashSet<String> {
    match crate::settings::current().extra.get("extensions") {
        Some(Value::Object(m)) => m
            .iter()
            .filter(|(_, v)| **v == Value::Bool(false))
            .map(|(k, _)| k.clone())
            .collect(),
        _ => HashSet::new(),
    }
}

/// The extensions in one directory, sorted by directory name.
fn scan(root: &Path, warnings: &mut Vec<String>) -> Vec<(PathBuf, Manifest)> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.join(MANIFEST).is_file())
        .collect();
    dirs.sort();
    let mut out = Vec::new();
    for dir in dirs {
        let file = dir.join(MANIFEST);
        match std::fs::read_to_string(&file)
            .map_err(|e| e.to_string())
            .and_then(|t| Manifest::parse(&t))
        {
            Ok(m) => out.push((dir, m)),
            Err(e) => warnings.push(format!("ignoring {}: {e}", file.display())),
        }
    }
    out
}

/// Every extension: the user's first, then the project's (a name already
/// taken is skipped).
pub fn discover(home: &Path, cwd: &Path) -> (Vec<Found>, Vec<String>) {
    let mut warnings = Vec::new();
    let off = disabled();
    let trusts = read_trusts(home).unwrap_or_else(|e| {
        warnings.push(format!("ignoring {}: {e}", trust_file(home).display()));
        BTreeMap::new()
    });
    let mut found: Vec<Found> = Vec::new();
    let user = user_dir(home);
    let project = project_dir(cwd);
    let mut roots = vec![(user.clone(), false)];
    if project != user {
        roots.push((project, true));
    }
    for (root, is_project) in roots {
        for (dir, manifest) in scan(&root, &mut warnings) {
            if found.iter().any(|f| f.manifest.name == manifest.name) {
                warnings.push(format!(
                    "ignoring {}: an extension named `{}` is already loaded",
                    dir.display(),
                    manifest.name
                ));
                continue;
            }
            let hash = is_project.then(|| dir_hash(&dir));
            let trusted = !is_project
                || trusts
                    .get(&trust_key(&dir))
                    .is_some_and(|h| Some(h) == hash.as_ref());
            if !trusted {
                warnings.push(format!(
                    "project extension `{}` in {} is not trusted and will not start; review it, then run /extensions trust",
                    manifest.name,
                    dir.display()
                ));
            }
            let enabled = !off.contains(&manifest.name);
            found.push(Found {
                manifest,
                dir,
                project: is_project,
                hash,
                trusted,
                enabled,
            });
        }
    }
    (found, warnings)
}

/// SHA-256 over the relative paths and contents of every file in `dir`
/// (symlinks by target), skipping VCS and dependency directories.
pub fn dir_hash(dir: &Path) -> String {
    fn walk(root: &Path, dir: &Path, files: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            let path = e.path();
            let Ok(ty) = e.file_type() else { continue };
            if ty.is_dir() {
                if !UNHASHED.iter().any(|u| e.file_name() == *u) {
                    walk(root, &path, files);
                }
            } else {
                files.push(path.strip_prefix(root).unwrap_or(&path).to_path_buf());
            }
        }
    }
    let mut files = Vec::new();
    walk(dir, dir, &mut files);
    files.sort();
    let mut h = Sha256::new();
    for rel in files {
        let path = dir.join(&rel);
        h.update(crate::tools::slash_path(&rel).as_bytes());
        h.update([0]);
        match std::fs::read_link(&path) {
            Ok(target) => h.update(format!("link:{}", target.display()).as_bytes()),
            Err(_) => h.update(std::fs::read(&path).unwrap_or_default()),
        }
        h.update([0]);
    }
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

fn trust_key(dir: &Path) -> String {
    std::fs::canonicalize(dir)
        .unwrap_or_else(|_| dir.to_path_buf())
        .display()
        .to_string()
}

fn read_trusts(home: &Path) -> Result<BTreeMap<String, String>, String> {
    let text = match std::fs::read_to_string(trust_file(home)) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(e) => return Err(e.to_string()),
    };
    if text.trim().is_empty() {
        return Ok(BTreeMap::new());
    }
    serde_json::from_str(&text).map_err(|e| format!("not valid JSON: {e}"))
}

/// Trust the project extensions as `found` lists them (their hash as
/// loaded), refusing any that changed since. Returns the trusted names.
pub fn trust(home: &Path, found: &[Found]) -> Result<Vec<String>> {
    let mut trusts = read_trusts(home).map_err(|e| {
        anyhow::anyhow!(
            "{} is malformed ({e}); fix or remove it first",
            trust_file(home).display()
        )
    })?;
    let mut names = Vec::new();
    for f in found.iter().filter(|f| f.project && !f.trusted) {
        let now = dir_hash(&f.dir);
        anyhow::ensure!(
            f.hash.as_deref() == Some(now.as_str()),
            "{} changed since it was loaded; review it with /extensions, then trust it again",
            f.dir.display()
        );
        trusts.insert(trust_key(&f.dir), now);
        names.push(f.manifest.name.clone());
    }
    if names.is_empty() {
        return Ok(names);
    }
    let mut out = serde_json::to_string_pretty(&trusts)?;
    out.push('\n');
    crate::session::write_private(&trust_file(home), out.as_bytes())
        .with_context(|| format!("writing {}", trust_file(home).display()))?;
    Ok(names)
}

#[derive(Clone, Debug, PartialEq)]
enum State {
    Disabled,
    Untrusted,
    Starting,
    Ready,
    Failed(String),
    Stopped,
}

struct Ext {
    found: Found,
    state: Mutex<State>,
    reg: Mutex<Registration>,
    /// Commands kept after dropping names other commands already use.
    commands: Mutex<Vec<CommandDef>>,
    conn: Mutex<Option<Arc<Conn>>>,
    restarts: AtomicU32,
    status: Mutex<Option<String>>,
}

impl Ext {
    fn name(&self) -> &str {
        &self.found.manifest.name
    }

    fn state(&self) -> State {
        lock(&self.state).clone()
    }

    fn set_state(&self, s: State) {
        *lock(&self.state) = s;
    }

    fn conn(&self) -> Option<Arc<Conn>> {
        lock(&self.conn).clone()
    }

    fn subscribes(&self, ev: Event) -> bool {
        self.state() == State::Ready && lock(&self.reg).events.contains(&ev)
    }
}

/// How a session was started; sent in `initialize` as `mode`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Tui,
    Print,
    Acp,
}

impl Mode {
    fn as_str(self) -> &'static str {
        match self {
            Mode::Tui => "tui",
            Mode::Print => "print",
            Mode::Acp => "acp",
        }
    }
}

/// A message an extension asked to send as the user (`session/send`).
struct Queued {
    steer: bool,
    text: String,
}

struct Shared {
    home: PathBuf,
    cwd: PathBuf,
    mode: Mode,
    session_id: Mutex<String>,
    exts: Mutex<Vec<Arc<Ext>>>,
    live: LiveTools,
    queue: Mutex<Vec<Queued>>,
    wake: tokio::sync::Notify,
    on_change: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    /// Slash commands extensions may not take (nyq's own).
    reserved: Mutex<HashSet<String>>,
    /// The source of the last `session_start`: an extension that comes up
    /// later gets that event when it is ready.
    started: Mutex<Option<String>>,
}

/// The session's extensions. Cheap to clone.
#[derive(Clone)]
pub struct Extensions(Arc<Shared>);

/// What `user_prompt` handlers decided.
#[derive(Debug, Default, PartialEq)]
pub struct PromptEdit {
    /// Do not send the prompt; the reason is shown.
    pub blocked: Option<String>,
    /// The prompt as rewritten.
    pub prompt: Option<String>,
    /// Text to add to the conversation before the prompt.
    pub context: Vec<String>,
}

/// What a slash command returned.
#[derive(Debug, Default, PartialEq)]
pub struct CommandResult {
    pub message: Option<String>,
    pub prompt: Option<String>,
}

impl Extensions {
    /// Find the extensions for `cwd` (none start yet; see [`Extensions::start`]).
    pub fn load(home: &Path, cwd: &Path, mode: Mode) -> (Self, Vec<String>) {
        let shared = Shared {
            home: home.to_path_buf(),
            cwd: cwd.to_path_buf(),
            mode,
            session_id: Mutex::new(String::new()),
            exts: Mutex::new(Vec::new()),
            live: LiveTools::default(),
            queue: Mutex::new(Vec::new()),
            wake: tokio::sync::Notify::new(),
            on_change: Mutex::new(None),
            reserved: Mutex::new(HashSet::new()),
            started: Mutex::new(None),
        };
        let ext = Extensions(Arc::new(shared));
        let warnings = ext.rediscover();
        (ext, warnings)
    }

    fn rediscover(&self) -> Vec<String> {
        let (found, warnings) = discover(&self.0.home, &self.0.cwd);
        *lock(&self.0.exts) = found
            .into_iter()
            .map(|f| {
                let state = match (f.enabled, f.trusted) {
                    (false, _) => State::Disabled,
                    (_, false) => State::Untrusted,
                    _ => State::Stopped,
                };
                Arc::new(Ext {
                    found: f,
                    state: Mutex::new(state),
                    reg: Default::default(),
                    commands: Default::default(),
                    conn: Mutex::new(None),
                    restarts: AtomicU32::new(0),
                    status: Mutex::new(None),
                })
            })
            .collect();
        warnings
    }

    fn exts(&self) -> Vec<Arc<Ext>> {
        lock(&self.0.exts).clone()
    }

    /// Receive extension tools in `tools` (applied before each model request).
    pub fn attach(&self, tools: &mut Registry) {
        tools.attach_extensions(self.0.live.clone());
    }

    pub fn set_session(&self, id: &str) {
        *lock(&self.0.session_id) = id.to_string();
    }

    /// Commands extensions may not register (nyq's own and their aliases).
    pub fn reserve(&self, names: impl IntoIterator<Item = String>) {
        lock(&self.0.reserved).extend(
            names
                .into_iter()
                .map(|n| n.trim_start_matches('/').to_string()),
        );
    }

    /// Called whenever commands or status texts change.
    pub fn on_change(&self, f: impl Fn() + Send + Sync + 'static) {
        *lock(&self.0.on_change) = Some(Arc::new(f));
    }

    fn changed(&self) {
        let f = lock(&self.0.on_change).clone();
        if let Some(f) = f {
            f();
        }
    }

    /// Start every enabled, trusted extension in the background.
    pub fn start(&self) {
        for ext in self.exts() {
            if ext.state() == State::Stopped {
                ext.set_state(State::Starting);
                tokio::spawn(launch(Arc::downgrade(&self.0), ext));
            }
        }
    }

    /// Wait until no extension is still starting (at most `timeout`).
    pub async fn wait_ready(&self, timeout: Duration) {
        let deadline = tokio::time::Instant::now() + timeout;
        while self.exts().iter().any(|e| e.state() == State::Starting)
            && tokio::time::Instant::now() < deadline
        {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// Stop every extension, after a `session_end` event.
    pub async fn shutdown(&self) {
        self.event_all(Event::SessionEnd, json!({})).await;
        let exts = self.exts();
        for ext in &exts {
            let conn = lock(&ext.conn).take();
            if let Some(conn) = conn {
                conn.close().await;
            }
            if matches!(ext.state(), State::Ready | State::Starting) {
                ext.set_state(State::Stopped);
            }
            lock(&ext.status).take();
        }
        self.0
            .live
            .push(Box::new(|r: &mut Registry| r.remove_prefix(TOOL_PREFIX)));
    }

    /// `/extensions reload`: stop them all, look again, start the enabled ones.
    pub async fn reload(&self) -> Vec<String> {
        self.shutdown().await;
        let warnings = self.rediscover();
        self.start();
        self.changed();
        warnings
    }

    /// `/extensions trust`: trust the project extensions as listed.
    pub fn trust(&self) -> Result<Vec<String>> {
        let found: Vec<Found> = self.exts().iter().map(|e| e.found.clone()).collect();
        trust(&self.0.home, &found)
    }

    /// Slash commands as `(/name, description)` for the menus.
    pub fn commands(&self) -> Vec<(String, String)> {
        self.exts()
            .iter()
            .filter(|e| e.state() == State::Ready)
            .flat_map(|e| {
                let name = e.name().to_string();
                lock(&e.commands)
                    .iter()
                    .map(|c| {
                        let hint = if c.args_hint.is_empty() {
                            String::new()
                        } else {
                            format!(" {}", c.args_hint)
                        };
                        (
                            format!("/{}", c.name),
                            format!("{}{hint} ({name})", c.description),
                        )
                    })
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    fn command_owner(&self, cmd: &str) -> Option<Arc<Ext>> {
        let cmd = cmd.strip_prefix('/')?;
        self.exts()
            .into_iter()
            .find(|e| e.state() == State::Ready && lock(&e.commands).iter().any(|c| c.name == cmd))
    }

    pub fn has_command(&self, cmd: &str) -> bool {
        self.command_owner(cmd).is_some()
    }

    /// Run `/cmd args` (`command/run`).
    pub async fn run_command(
        &self,
        cmd: &str,
        args: &str,
        cancel: &CancellationToken,
    ) -> Result<CommandResult> {
        let ext = self
            .command_owner(cmd)
            .with_context(|| format!("no extension command {cmd}"))?;
        let conn = ext
            .conn()
            .with_context(|| format!("extension `{}` is not running", ext.name()))?;
        let params = json!({"name": cmd.trim_start_matches('/'), "args": args});
        let v = tokio::select! {
            r = conn.request("command/run", params, COMMAND_TIMEOUT) => r.map_err(|e| anyhow::anyhow!("{cmd} ({}): {e}", ext.name()))?,
            _ = cancel.cancelled() => anyhow::bail!("{cmd} cancelled"),
        };
        let text = |k: &str| {
            v.get(k)
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .map(String::from)
        };
        Ok(CommandResult {
            message: text("message"),
            prompt: text("prompt"),
        })
    }

    /// Status-line segments, in extension order.
    pub fn statuses(&self) -> Vec<String> {
        self.exts()
            .iter()
            .filter_map(|e| lock(&e.status).clone())
            .collect()
    }

    /// Messages extensions queued with `session/send`: steering ones, or
    /// the oldest follow-up.
    pub fn take_queued(&self, steer: bool) -> Vec<String> {
        let mut q = lock(&self.0.queue);
        if steer {
            let (take, keep): (Vec<_>, Vec<_>) = q.drain(..).partition(|m| m.steer);
            *q = keep;
            take.into_iter().map(|m| m.text).collect()
        } else {
            match q.iter().position(|m| !m.steer) {
                Some(i) => vec![q.remove(i).text],
                None => Vec::new(),
            }
        }
    }

    /// Everything queued, in order (when nyq is idle).
    pub fn take_all_queued(&self) -> Vec<String> {
        lock(&self.0.queue).drain(..).map(|m| m.text).collect()
    }

    /// Resolves once an extension queues a message.
    pub async fn queued(&self) {
        self.0.wake.notified().await
    }

    /// Rows for `/extensions` and `nyq extensions list`.
    pub fn lines(&self) -> Vec<String> {
        let exts = self.exts();
        let mut out = Vec::new();
        if exts.is_empty() {
            out.push(format!(
                "No extensions. Install one with `nyq extensions install SOURCE`, or add a directory to {} or {}.",
                user_dir(&self.0.home).display(),
                project_dir(&self.0.cwd).display()
            ));
        }
        for e in &exts {
            let state = match e.state() {
                State::Disabled => "disabled in settings".to_string(),
                State::Untrusted => {
                    "untrusted — not running; /extensions trust enables it".to_string()
                }
                State::Starting => "starting".to_string(),
                State::Ready => "running".to_string(),
                State::Failed(why) => format!("failed: {why}"),
                State::Stopped => "stopped".to_string(),
            };
            let m = &e.found.manifest;
            let scope = if e.found.project { "project" } else { "user" };
            let version = if m.version.is_empty() {
                String::new()
            } else {
                format!(" {}", m.version)
            };
            out.push(format!("{}{version} · {scope} · {state}", m.name));
            if !m.description.is_empty() {
                out.push(format!("  {}", m.description));
            }
            if e.state() == State::Ready {
                let reg = lock(&e.reg);
                let tools: Vec<String> = reg
                    .tools
                    .iter()
                    .map(|t| tool_name(&m.name, &t.name))
                    .collect();
                let commands: Vec<String> = lock(&e.commands)
                    .iter()
                    .map(|c| format!("/{}", c.name))
                    .collect();
                let events: Vec<&str> = reg.events.iter().map(|e| e.as_str()).collect();
                for (label, list) in [
                    ("tools", tools.join(", ")),
                    ("commands", commands.join(", ")),
                    ("events", events.join(", ")),
                ] {
                    if !list.is_empty() {
                        out.push(format!("  {label}: {list}"));
                    }
                }
                if let Some(s) = lock(&e.status).as_deref() {
                    out.push(format!("  status: {s}"));
                }
            }
            out.push(format!(
                "  {} · log {}",
                e.found.dir.display(),
                log_path(&self.0.home, &m.name).display()
            ));
        }
        out
    }

    // ---- events ----

    async fn event(&self, ext: &Ext, ev: Event, mut payload: Value) -> Option<Value> {
        let conn = ext.conn()?;
        payload["session_id"] = json!(lock(&self.0.session_id).clone());
        match conn
            .request(&ev.method(), payload, ext.found.manifest.event_timeout)
            .await
        {
            Ok(v) => Some(v),
            Err(e) => {
                log(
                    &self.0.home,
                    ext.name(),
                    &format!("{} skipped: {e}", ev.method()),
                );
                None
            }
        }
    }

    async fn event_all(&self, ev: Event, payload: Value) {
        for ext in self.exts().iter().filter(|e| e.subscribes(ev)) {
            self.event(ext, ev, payload.clone()).await;
        }
    }

    /// Observe-only events run in the background.
    fn event_background(&self, ev: Event, payload: Value) {
        if self.exts().iter().any(|e| e.subscribes(ev)) {
            let this = self.clone();
            tokio::spawn(async move { this.event_all(ev, payload).await });
        }
    }

    pub fn session_start(&self, source: &str, session_id: Option<&str>) {
        if let Some(id) = session_id {
            self.set_session(id);
        }
        let ready: Vec<Arc<Ext>> = {
            let mut started = lock(&self.0.started);
            *started = Some(source.to_string());
            self.exts()
                .into_iter()
                .filter(|e| e.subscribes(Event::SessionStart))
                .collect()
        };
        for ext in ready {
            self.session_start_one(ext, source);
        }
    }

    fn session_start_one(&self, ext: Arc<Ext>, source: &str) {
        let (this, payload) = (self.clone(), json!({"source": source, "cwd": self.0.cwd}));
        tokio::spawn(async move { this.event(&ext, Event::SessionStart, payload).await });
    }

    pub fn turn_end(&self, text: &str) {
        self.event_background(Event::TurnEnd, json!({"text": text}));
    }

    /// `user_prompt`: each handler sees the prompt as rewritten so far; the
    /// first block wins.
    pub async fn user_prompt(&self, prompt: &str) -> PromptEdit {
        let mut edit = PromptEdit::default();
        for ext in self
            .exts()
            .iter()
            .filter(|e| e.subscribes(Event::UserPrompt))
        {
            let current = edit.prompt.as_deref().unwrap_or(prompt);
            let Some(v) = self
                .event(ext, Event::UserPrompt, json!({"prompt": current}))
                .await
            else {
                continue;
            };
            if let Some(reason) = text_field(&v, "block") {
                edit.blocked = Some(format!(
                    "prompt blocked by extension {}: {reason}",
                    ext.name()
                ));
                return edit;
            }
            if let Some(p) = text_field(&v, "prompt") {
                edit.prompt = Some(p);
            }
            if let Some(c) = text_field(&v, "context") {
                edit.context.push(format!(
                    "[extension {} added this context]\n{c}",
                    ext.name()
                ));
            }
        }
        edit
    }

    /// `tool_call`: `Err` is the reason a handler blocked the call; `Ok`
    /// carries rewritten input.
    pub async fn tool_call(&self, tool: &str, input: &Value) -> Result<Option<Value>, String> {
        let mut updated: Option<Value> = None;
        for ext in self.exts().iter().filter(|e| e.subscribes(Event::ToolCall)) {
            let current = updated.as_ref().unwrap_or(input);
            let Some(v) = self
                .event(
                    ext,
                    Event::ToolCall,
                    json!({"tool": tool, "input": current}),
                )
                .await
            else {
                continue;
            };
            if let Some(reason) = text_field(&v, "block") {
                return Err(format!(
                    "This call was blocked by extension {}: {reason}. Do not retry it; find another way or ask the user.",
                    ext.name()
                ));
            }
            if let Some(i) = v.get("input").filter(|i| i.is_object()) {
                updated = Some(i.clone());
            }
        }
        Ok(updated)
    }

    /// `tool_result`: handlers may replace the text the model sees.
    pub async fn tool_result(&self, tool: &str, input: &Value, out: &mut ToolOutput) {
        for ext in self
            .exts()
            .iter()
            .filter(|e| e.subscribes(Event::ToolResult))
        {
            let payload = json!({"tool": tool, "input": input, "content": out.content, "is_error": out.is_error});
            if let Some(content) = self
                .event(ext, Event::ToolResult, payload)
                .await
                .and_then(|v| text_field(&v, "content"))
            {
                out.content = content;
            }
        }
    }

    /// `before_compact`: extra instructions for the summary.
    pub async fn before_compact(&self) -> Vec<String> {
        let mut out = Vec::new();
        for ext in self
            .exts()
            .iter()
            .filter(|e| e.subscribes(Event::BeforeCompact))
        {
            out.extend(
                self.event(ext, Event::BeforeCompact, json!({}))
                    .await
                    .and_then(|v| text_field(&v, "instructions")),
            );
        }
        out
    }

    async fn call_tool(
        &self,
        ext_name: &str,
        tool: &str,
        input: Value,
        cancel: &CancellationToken,
    ) -> ToolOutput {
        let Some(ext) = self.exts().into_iter().find(|e| e.name() == ext_name) else {
            return ToolOutput::err(format!("extension `{ext_name}` is not loaded"));
        };
        // A crashed extension may be coming back.
        let deadline = tokio::time::Instant::now() + STARTUP_TIMEOUT;
        let waiting = || match ext.state() {
            State::Starting => true,
            State::Ready => ext.conn().is_none_or(|c| !c.alive()),
            _ => false,
        };
        while waiting() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let Some(conn) = ext.conn().filter(|_| ext.state() == State::Ready) else {
            return ToolOutput::err(format!(
                "extension `{ext_name}` is not running (see /extensions)"
            ));
        };
        let request = conn.request(
            "tool/call",
            json!({"name": tool, "input": input}),
            TOOL_TIMEOUT,
        );
        tokio::select! {
            r = request => match r {
                Ok(v) => protocol::tool_output(&v),
                Err(e) => ToolOutput::err(format!("extension `{ext_name}` tool `{tool}` failed: {e}")),
            },
            _ = cancel.cancelled() => ToolOutput::err("Cancelled by user."),
        }
    }
}

fn text_field(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(String::from)
}

pub fn tool_name(ext: &str, tool: &str) -> String {
    format!("{TOOL_PREFIX}{ext}__{tool}")
}

/// Start (or restart) one extension and register what it offers.
async fn launch(shared: Weak<Shared>, ext: Arc<Ext>) {
    let Some(s) = shared.upgrade() else { return };
    let this = Extensions(s);
    let name = ext.name().to_string();
    match connect(&this, &ext).await {
        Ok((conn, reg, warnings)) => {
            for w in &warnings {
                crate::ui::info(&format!("extension {name}: {w}"));
                log(&this.0.home, &name, w);
            }
            if ext.state() != State::Starting {
                // Stopped (reload, shutdown) while it started.
                conn.close().await;
                return;
            }
            register(&this, &ext, reg);
            *lock(&ext.conn) = Some(conn);
            let source = {
                let started = lock(&this.0.started);
                ext.set_state(State::Ready);
                started.clone()
            };
            if let Some(source) = source.filter(|_| ext.subscribes(Event::SessionStart)) {
                this.session_start_one(ext.clone(), &source);
            }
        }
        Err(e) => {
            log(&this.0.home, &name, &format!("failed to start: {e:#}"));
            crate::ui::error(&format!(
                "extension {name} failed to start: {e:#} (log: {})",
                log_path(&this.0.home, &name).display()
            ));
            ext.set_state(State::Failed(format!("{e:#}")));
        }
    }
    this.changed();
}

async fn connect(
    this: &Extensions,
    ext: &Arc<Ext>,
) -> Result<(Arc<Conn>, Registration, Vec<String>)> {
    let name = ext.name().to_string();
    let handler: conn::Handler = {
        let (shared, ext) = (Arc::downgrade(&this.0), Arc::downgrade(ext));
        Arc::new(move |method, params| {
            let (shared, ext) = (shared.clone(), ext.clone());
            Box::pin(async move {
                match (shared.upgrade(), ext.upgrade()) {
                    (Some(s), Some(e)) => incoming(Extensions(s), e, &method, params).await,
                    _ => Err("nyq is shutting down".into()),
                }
            })
        })
    };
    let on_exit: Box<dyn FnOnce(String) + Send> = {
        let (shared, weak) = (Arc::downgrade(&this.0), Arc::downgrade(ext));
        Box::new(move |status| {
            if let (Some(s), Some(e)) = (shared.upgrade(), weak.upgrade()) {
                exited(Extensions(s), e, status);
            }
        })
    };
    let log_file = log_path(&this.0.home, &name);
    log(
        &this.0.home,
        &name,
        &format!(
            "starting `{}` in {}",
            ext.found.manifest.command.join(" "),
            ext.found.dir.display()
        ),
    );
    let conn = Conn::spawn(
        &ext.found.dir,
        &ext.found.manifest,
        &log_file,
        handler,
        on_exit,
    )?;
    let params = json!({
        "nyq_version": env!("CARGO_PKG_VERSION"),
        "protocol": protocol::PROTOCOL,
        "cwd": this.0.cwd,
        "session_id": lock(&this.0.session_id).clone(),
        "mode": this.0.mode.as_str(),
    });
    let v = match conn.request("initialize", params, STARTUP_TIMEOUT).await {
        Ok(v) => v,
        Err(e) => {
            conn.close().await;
            anyhow::bail!("initialize: {e}");
        }
    };
    let (reg, warnings) = protocol::parse_registration(&v);
    Ok((conn, reg, warnings))
}

/// Put an extension's tools in the registry and keep its usable commands.
fn register(this: &Extensions, ext: &Arc<Ext>, reg: Registration) {
    let name = ext.name().to_string();
    let reserved = lock(&this.0.reserved).clone();
    let taken: HashSet<String> = this
        .exts()
        .iter()
        .filter(|e| !Arc::ptr_eq(e, ext))
        .flat_map(|e| {
            lock(&e.commands)
                .iter()
                .map(|c| c.name.clone())
                .collect::<Vec<_>>()
        })
        .collect();
    let mut commands = Vec::new();
    for c in &reg.commands {
        if reserved.contains(&c.name) || taken.contains(&c.name) {
            crate::ui::info(&format!(
                "extension {name}: /{} is already a command; skipped",
                c.name
            ));
        } else {
            commands.push(c.clone());
        }
    }
    *lock(&ext.commands) = commands;
    *lock(&ext.status) = reg.status.clone();
    let weak = Arc::downgrade(&this.0);
    let tools: Vec<ExtTool> = reg
        .tools
        .iter()
        .map(|t| ExtTool {
            ext: name.clone(),
            def: t.clone(),
            shared: weak.clone(),
        })
        .collect();
    let prefix = format!("{TOOL_PREFIX}{name}__");
    this.0.live.push(Box::new(move |r: &mut Registry| {
        r.remove_prefix(&prefix);
        for t in tools {
            let _ = r.try_insert(Box::new(t));
        }
    }));
    *lock(&ext.reg) = reg;
}

/// The process ended: restart it once, then report it.
fn exited(this: Extensions, ext: Arc<Ext>, status: String) {
    let name = ext.name().to_string();
    let was_running = lock(&ext.conn)
        .take()
        .is_some_and(|c| !c.closing.load(Ordering::SeqCst));
    if !was_running || ext.state() != State::Ready {
        log(&this.0.home, &name, &format!("exited ({status})"));
        return;
    }
    log(
        &this.0.home,
        &name,
        &format!("exited unexpectedly ({status})"),
    );
    this.0.live.push(Box::new({
        let prefix = format!("{TOOL_PREFIX}{name}__");
        move |r: &mut Registry| r.remove_prefix(&prefix)
    }));
    lock(&ext.status).take();
    if ext.restarts.fetch_add(1, Ordering::SeqCst) < RESTARTS {
        crate::ui::info(&format!(
            "extension {name} exited ({status}); restarting it"
        ));
        ext.set_state(State::Starting);
        tokio::spawn(launch(Arc::downgrade(&this.0), ext));
    } else {
        let why = format!(
            "exited ({status}) after a restart; see {}",
            log_path(&this.0.home, &name).display()
        );
        crate::ui::error(&format!("extension {name} {why}"));
        ext.set_state(State::Failed(why));
    }
    this.changed();
}

/// A request or notification from an extension.
async fn incoming(
    this: Extensions,
    ext: Arc<Ext>,
    method: &str,
    params: Value,
) -> Result<Value, String> {
    let name = ext.name().to_string();
    let text = |k: &str| {
        params
            .get(k)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    let tui = this.0.mode == Mode::Tui;
    match method {
        "ui/notify" => {
            let msg = format!("[{name}] {}", text("text"));
            match text("level").as_str() {
                "error" | "warning" => crate::ui::error(&msg),
                _ => crate::ui::info(&msg),
            }
            Ok(Value::Null)
        }
        "ui/confirm" => {
            if !tui {
                return Ok(json!(false));
            }
            let options = ["Yes".to_string(), "No".to_string()];
            let answer =
                crate::ui::tui::ask_active(&format!("[{name}] {}", text("text")), &options).await;
            Ok(json!(matches!(answer, Some(Some(a)) if a == "Yes")))
        }
        "ui/input" => {
            if !tui {
                return Ok(Value::Null);
            }
            match crate::ui::tui::ask_active(&format!("[{name}] {}", text("prompt")), &[]).await {
                Some(Some(answer)) => Ok(json!(answer)),
                _ => Ok(Value::Null),
            }
        }
        "session/send" => {
            let body = text("text");
            if body.trim().is_empty() {
                return Err("\"text\" is required".into());
            }
            let steer = text("queue") == "steer";
            log(
                &this.0.home,
                &name,
                &format!(
                    "session/send ({})",
                    if steer { "steer" } else { "follow_up" }
                ),
            );
            lock(&this.0.queue).push(Queued { steer, text: body });
            this.0.wake.notify_one();
            Ok(Value::Null)
        }
        "status/set" => {
            let t = text("text");
            *lock(&ext.status) = (!t.trim().is_empty()).then(|| crate::ui::one_line(&t, 60));
            this.changed();
            Ok(Value::Null)
        }
        "log" => {
            log(&this.0.home, &name, &text("text"));
            Ok(Value::Null)
        }
        other => Err(format!("unknown method `{other}`")),
    }
}

/// An extension tool, offered to the model as `ext__<extension>__<tool>`.
struct ExtTool {
    ext: String,
    def: ToolDef,
    shared: Weak<Shared>,
}

#[async_trait::async_trait]
impl Tool for ExtTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: tool_name(&self.ext, &self.def.name),
            description: self.def.description.clone(),
            parameters: self.def.input_schema.clone(),
        }
    }

    fn needs_approval(&self, _args: &Value) -> bool {
        !self.def.read_only
    }

    fn summary(&self, args: &Value) -> String {
        crate::ui::one_line(&args.to_string(), 100)
    }

    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolOutput {
        let problems = crate::mcp_schema::check(&self.def.input_schema, &args);
        if !problems.is_empty() {
            return ToolOutput::err(format!(
                "Invalid arguments for {}: {}. Fix them to match the tool's input schema and call it again.",
                self.spec().name,
                problems.join("; ")
            ));
        }
        let Some(shared) = self.shared.upgrade() else {
            return ToolOutput::err("extensions are shut down");
        };
        Extensions(shared)
            .call_tool(&self.ext, &self.def.name, args, &ctx.cancel)
            .await
    }
}

/// `/extensions [list|reload|trust]`.
pub async fn slash(ext: &Extensions, args: &[&str]) {
    match args.first().copied() {
        None | Some("list") => crate::ui::list(&ext.lines()),
        Some("reload") => {
            for w in ext.reload().await {
                crate::ui::error(&w);
            }
            ext.wait_ready(Duration::from_secs(5)).await;
            crate::ui::list(&ext.lines());
        }
        Some("trust") => match ext.trust() {
            Ok(names) if names.is_empty() => crate::ui::info("No untrusted project extensions."),
            Ok(names) => {
                crate::ui::info(&format!("Trusted {}.", names.join(", ")));
                for w in ext.reload().await {
                    crate::ui::error(&w);
                }
            }
            Err(e) => crate::ui::error(&format!("{e:#}")),
        },
        Some(other) => crate::ui::error(&format!(
            "unknown /extensions argument `{other}` (try list, reload or trust)"
        )),
    }
}

#[cfg(test)]
mod tests;
