//! The per-session working set: files the model keeps open, rendered from
//! disk into the trailing note at request-build time (see `agent::note`).
//! Focused files show in full with line numbers, source files open as
//! outlines, and pinned files (docs, specs) always show in full.
//!
//! The set itself is session metadata (a `workset` line, latest wins, and one
//! `intent` line per recorded intent), so a resumed session restores it. The
//! rendered contents are send-only: they never enter the stored transcript,
//! which keeps the cached prompt prefix stable.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use serde_json::{json, Value};

use super::{fs, Registry, SecretRoots, Tool, ToolCtx, ToolOutput};
use crate::types::ToolSpec;

/// Shared between the agent (rendering, persistence) and the tools (updates).
pub(crate) type Shared = Arc<Mutex<Workset>>;

pub(crate) fn lock(ws: &Shared) -> MutexGuard<'_, Workset> {
    ws.lock().unwrap_or_else(|e| e.into_inner())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Mode {
    Focused,
    Outline,
    Pinned,
}

impl Mode {
    fn as_str(self) -> &'static str {
        match self {
            Mode::Focused => "focused",
            Mode::Outline => "outline",
            Mode::Pinned => "pinned",
        }
    }
}

#[derive(Clone, Debug)]
struct Entry {
    /// Resolved absolute path (for reading).
    path: PathBuf,
    /// As shown to the model: relative to the workspace when inside it.
    display: String,
    mode: Mode,
    /// Last `open`/`focus` tick; drives least-recently-focused demotion.
    touched: u64,
}

/// One intent record (`{did, why, next}`), appended by `note_intent` and by
/// the `intent` field W9 adds to mutating tools.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Intent {
    pub did: String,
    pub why: String,
    #[serde(default)]
    pub next: String,
}

#[derive(Default)]
pub(crate) struct Workset {
    files: Vec<Entry>,
    /// All intents this session (oldest first, capped at [`MAX_INTENTS`]).
    intents: Vec<Intent>,
    /// Monotonic tick for least-recently-focused ordering.
    tick: u64,
    /// `files` changed since the last `take_files_dirty`.
    files_dirty: bool,
    /// Intents not yet appended to the session file.
    pending_intents: Vec<Intent>,
}

const MAX_ENTRIES: usize = 64;
const MAX_INTENTS: usize = 256;

/// The budgets `render` enforces, from settings.
pub(crate) struct Budgets {
    pub max_focused: usize,
    /// Token cap for rendered file contents (chars/4 estimate).
    pub max_tokens: u64,
    /// How many recent intents to show.
    pub intents: usize,
}

impl Budgets {
    pub(crate) fn from_settings(context_window: u64) -> Self {
        let s = crate::settings::current();
        Budgets {
            max_focused: s.workset.max_focused,
            max_tokens: context_window.saturating_mul(s.workset.max_tokens_pct as u64) / 100,
            intents: s.intents.show,
        }
    }
}

fn tokens(text: &str) -> u64 {
    (text.len() as u64).div_ceil(4)
}

fn resolve(cwd: &Path, p: &str) -> PathBuf {
    let path = Path::new(p);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

/// Path as shown to the model: relative to `cwd` when inside it.
fn display(cwd: &Path, path: &Path) -> String {
    path.strip_prefix(cwd)
        .map(|r| {
            if r.as_os_str().is_empty() {
                ".".into()
            } else {
                super::slash_path(r)
            }
        })
        .unwrap_or_else(|_| super::slash_path(path))
}

impl Workset {
    /// Wipe everything (`clear_context` dropped the transcript).
    pub(crate) fn clear(&mut self) {
        *self = Workset::default();
    }

    fn touch(&mut self) -> u64 {
        self.tick += 1;
        self.tick
    }

    fn find(&mut self, path: &Path) -> Option<&mut Entry> {
        self.files.iter_mut().find(|e| e.path == path)
    }

    /// Add or update `path`. Returns a description of the change.
    pub(crate) fn open(&mut self, cwd: &Path, path: PathBuf, mode: Mode) -> String {
        let display = display(cwd, &path);
        let touched = self.touch();
        let verb = match self.find(&path) {
            Some(e) => {
                e.mode = mode;
                e.touched = touched;
                "updated"
            }
            None => {
                self.files.push(Entry {
                    path,
                    display: display.clone(),
                    mode,
                    touched,
                });
                if self.files.len() > MAX_ENTRIES {
                    self.files.remove(0);
                }
                "opened"
            }
        };
        self.files_dirty = true;
        format!("{verb} {display} ({})", mode.as_str())
    }

    /// `open`/`focus` path checks, identical to `read_file`'s: secret roots
    /// reject before anything is touched, then the file must exist and be a
    /// regular file.
    fn check_path(cwd: &Path, user_path: &str) -> Result<PathBuf, ToolOutput> {
        let path = resolve(cwd, user_path);
        if let Some(msg) = SecretRoots::load().reject(user_path, &path) {
            return Err(ToolOutput::err(msg.to_string()));
        }
        match std::fs::metadata(&path) {
            Ok(m) if m.is_file() => Ok(path),
            Ok(m) if m.is_dir() => Err(ToolOutput::err(format!(
                "{} is a directory (use list_dir).",
                display(cwd, &path)
            ))),
            Ok(_) => Err(ToolOutput::err(format!(
                "{} is not a regular file.",
                display(cwd, &path)
            ))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(ToolOutput::err(format!(
                "file not found: {}",
                display(cwd, &path)
            ))),
            Err(e) => Err(ToolOutput::err(format!("{e}: {}", display(cwd, &path)))),
        }
    }

    /// Session-file state: `{"files": [{path, mode}]}`. Intent records ride in
    /// their own `intent` lines instead.
    pub(crate) fn to_json(&self) -> Value {
        json!({
            "files": self.files.iter().map(|e| json!({
                "path": e.display,
                "mode": e.mode.as_str(),
            })).collect::<Vec<_>>(),
        })
    }

    /// Restore `{"files": …}` (written by [`Workset::to_json`]); paths resolve
    /// against `cwd`. Unknown/malformed entries are skipped.
    pub(crate) fn restore_files(&mut self, state: &Value, cwd: &Path) {
        let Some(files) = state.get("files").and_then(Value::as_array) else {
            return;
        };
        for f in files {
            let Some(p) = f.get("path").and_then(Value::as_str) else {
                continue;
            };
            let mode = match f.get("mode").and_then(Value::as_str) {
                Some("focused") => Mode::Focused,
                Some("pinned") => Mode::Pinned,
                _ => Mode::Outline,
            };
            let touched = self.touch();
            self.files.push(Entry {
                path: resolve(cwd, p),
                display: p.to_string(),
                mode,
                touched,
            });
        }
        self.files_dirty = false;
    }

    /// Files-state changes to persist, when any.
    pub(crate) fn take_files_dirty(&mut self) -> Option<Value> {
        self.files_dirty.then(|| {
            self.files_dirty = false;
            self.to_json()
        })
    }

    /// Append an intent record (`note_intent` tool; W9 feeds the same store
    /// through this method from mutating tools).
    pub(crate) fn record_intent(&mut self, did: &str, why: &str, next: &str) {
        let intent = Intent {
            did: did.to_string(),
            why: why.to_string(),
            next: next.to_string(),
        };
        if self.intents.len() >= MAX_INTENTS {
            self.intents.remove(0);
        }
        self.intents.push(intent.clone());
        self.pending_intents.push(intent);
    }

    /// Intents written since the last `take_pending_intents`.
    pub(crate) fn take_pending_intents(&mut self) -> Vec<Intent> {
        std::mem::take(&mut self.pending_intents)
    }

    /// Restore intents replayed from the session file (not pending).
    pub(crate) fn restore_intents(&mut self, intents: Vec<Intent>) {
        self.intents = intents;
        self.pending_intents.clear();
    }

    /// The working-set section of the trailing note, rendered **from disk**.
    /// Order: outlines first, focused last. When over budget, the least
    /// recently focused files demote to outlines; if still over, entries are
    /// omitted oldest-first (outlines, then pinned, then focused).
    pub(crate) fn render(&self, budgets: &Budgets) -> String {
        // A zero token budget hides the file sections entirely.
        let show_files = budgets.max_tokens > 0;
        let mut sections: Vec<(usize, Mode, String)> = Vec::new();
        // Effective modes after the focused cap: oldest focused demote.
        let mut focused: Vec<usize> = self
            .files
            .iter()
            .enumerate()
            .filter(|(_, e)| e.mode == Mode::Focused)
            .map(|(i, _)| i)
            .collect();
        focused.sort_by_key(|i| self.files[*i].touched);
        let demote: Vec<usize> = focused
            .iter()
            .take(focused.len().saturating_sub(budgets.max_focused))
            .copied()
            .collect();
        let secrets = SecretRoots::load();
        for (i, e) in self.files.iter().enumerate().filter(|_| show_files) {
            let mode = if demote.contains(&i) {
                Mode::Outline
            } else {
                e.mode
            };
            let body = if let Some(msg) = secrets.reject(&e.display, &e.path) {
                format!("(unreadable: {msg})")
            } else {
                match mode {
                    Mode::Focused | Mode::Pinned => match fs::file_numbered(&e.path) {
                        Ok(text) => text.trim_end().to_string(),
                        Err(e) => format!("(unreadable: {})", first_line(&e.content)),
                    },
                    Mode::Outline => match fs::file_text(&e.path) {
                        Ok(text) => crate::outline::render(&e.path, &text).trim_end().to_string(),
                        Err(e) => format!("(unreadable: {})", first_line(&e.content)),
                    },
                }
            };
            sections.push((i, mode, body));
        }
        sections.sort_by_key(|(i, m, _)| {
            (
                match m {
                    Mode::Outline => 0,
                    Mode::Pinned => 1,
                    Mode::Focused => 2,
                },
                *i,
            )
        });
        // Omit until under budget: oldest outlines, then pinned, then focused.
        let mut omitted: Vec<String> = Vec::new();
        let total = |sections: &[(usize, Mode, String)]| -> u64 {
            sections.iter().map(|(_, _, b)| tokens(b)).sum()
        };
        for want in [Mode::Outline, Mode::Pinned, Mode::Focused] {
            while total(&sections) > budgets.max_tokens {
                let pos = sections
                    .iter()
                    .enumerate()
                    .filter(|(_, (_, m, _))| *m == want)
                    .min_by_key(|(_, (i, _, _))| self.files[*i].touched)
                    .map(|(pos, _)| pos);
                match pos {
                    Some(p) => {
                        let (i, _, _) = sections.remove(p);
                        omitted.push(self.files[i].display.clone());
                    }
                    None => break,
                }
            }
        }
        let mut out = String::new();
        if !sections.is_empty() || !omitted.is_empty() {
            out.push_str("[Working set]\n");
            for &(i, mode, ref body) in &sections {
                let e = &self.files[i];
                out.push_str(&format!("## {} ({})\n{}\n", e.display, mode.as_str(), body));
            }
            if !omitted.is_empty() {
                out.push_str(&format!(
                    "({} files omitted: {} — call `close` to make room)\n",
                    omitted.len(),
                    omitted.join(", ")
                ));
            }
        }
        if budgets.intents > 0 && !self.intents.is_empty() {
            out.push_str("\n[Recent intents]\n");
            for intent in self.intents.iter().rev().take(budgets.intents).rev() {
                out.push_str(&format!("- did: {}", one_line(&intent.did)));
                if !intent.why.is_empty() {
                    out.push_str(&format!(" · why: {}", one_line(&intent.why)));
                }
                if !intent.next.is_empty() {
                    out.push_str(&format!(" · next: {}", one_line(&intent.next)));
                }
                out.push('\n');
            }
        }
        out.trim_end().to_string()
    }
}

fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or("").to_string()
}

fn one_line(text: &str) -> String {
    text.lines().next().unwrap_or("").trim().to_string()
}

// ---------------------------------------------------------------------------
// Tools

fn path_arg<'a>(args: &'a Value, tool: &str) -> Result<&'a str, ToolOutput> {
    args.get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| ToolOutput::err(format!("{tool}: missing required argument `path`.")))
}

fn summary_path(args: &Value) -> String {
    let cwd = std::env::current_dir().unwrap_or_default();
    let p = args.get("path").and_then(Value::as_str).unwrap_or(".");
    display(&cwd, &resolve(&cwd, p))
}

fn enabled() -> Option<ToolOutput> {
    (!crate::settings::current().workset.enabled).then(|| {
        ToolOutput::err("the working set is disabled (workset.enabled = false in settings).")
    })
}

fn list_lines(ws: &Workset) -> Vec<String> {
    ws.files
        .iter()
        .map(|e| format!("{} ({})", e.display, e.mode.as_str()))
        .collect()
}

struct Open;

#[async_trait::async_trait]
impl Tool for Open {
    fn effects(&self, _args: &Value) -> super::Effects {
        super::Effects::reads_only()
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "open".into(),
            description: "Add a file to the session working set: its current disk contents are \
                rendered into every request, so you always see the latest version. `mode` is \
                `focused` (full text with line numbers), `outline` (signatures and types only), \
                or `pinned` (full text, for docs and specs). Default: outline for source files, \
                pinned otherwise. Use `focus` to read a file in full and `close` to drop one."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path, relative to the workspace root or absolute."},
                    "mode": {"type": "string", "enum": ["focused", "outline", "pinned"], "description": "How to show the file (default: outline for source, pinned otherwise)."}
                },
                "required": ["path"]
            }),
        }
    }

    fn summary(&self, args: &Value) -> String {
        summary_path(args)
    }

    fn needs_approval_for(&self, args: &Value, cwd: &Path) -> bool {
        super::outside_path_arg(cwd, args)
    }

    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolOutput {
        if let Some(e) = enabled() {
            return e;
        }
        let p = match path_arg(&args, "open") {
            Ok(p) => p,
            Err(e) => return e,
        };
        let path = match Workset::check_path(&ctx.cwd, p) {
            Ok(p) => p,
            Err(e) => return e,
        };
        let mode = match args.get("mode").and_then(Value::as_str) {
            Some("focused") => Mode::Focused,
            Some("outline") => Mode::Outline,
            Some("pinned") => Mode::Pinned,
            Some(other) => {
                return ToolOutput::err(format!(
                    "open: unknown mode `{other}` (expected focused, outline or pinned)."
                ))
            }
            None => {
                if crate::outline::is_source(&path) {
                    Mode::Outline
                } else {
                    Mode::Pinned
                }
            }
        };
        ToolOutput::ok(lock(&ctx.workset).open(&ctx.cwd, path, mode))
    }
}

struct Focus;

#[async_trait::async_trait]
impl Tool for Focus {
    fn effects(&self, _args: &Value) -> super::Effects {
        super::Effects::reads_only()
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "focus".into(),
            description: "Show a working-set file in full with line numbers (opens it focused \
                when it is not in the set). Only a limited number of files stay focused at once; \
                the rest render as outlines."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path, relative to the workspace root or absolute."}
                },
                "required": ["path"]
            }),
        }
    }

    fn summary(&self, args: &Value) -> String {
        summary_path(args)
    }

    fn needs_approval_for(&self, args: &Value, cwd: &Path) -> bool {
        super::outside_path_arg(cwd, args)
    }

    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolOutput {
        if let Some(e) = enabled() {
            return e;
        }
        let p = match path_arg(&args, "focus") {
            Ok(p) => p,
            Err(e) => return e,
        };
        let path = match Workset::check_path(&ctx.cwd, p) {
            Ok(p) => p,
            Err(e) => return e,
        };
        let msg = lock(&ctx.workset).open(&ctx.cwd, path, Mode::Focused);
        ToolOutput::ok(msg.replacen("opened", "focused", 1).replacen("updated", "focused", 1))
    }
}

struct Unfocus;

#[async_trait::async_trait]
impl Tool for Unfocus {
    fn effects(&self, _args: &Value) -> super::Effects {
        super::Effects::none()
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "unfocus".into(),
            description: "Demote a focused working-set file back to an outline (signatures and \
                types only), freeing focused space without closing it."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path, relative to the workspace root or absolute."}
                },
                "required": ["path"]
            }),
        }
    }

    fn summary(&self, args: &Value) -> String {
        summary_path(args)
    }

    fn needs_approval_for(&self, args: &Value, cwd: &Path) -> bool {
        super::outside_path_arg(cwd, args)
    }

    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolOutput {
        if let Some(e) = enabled() {
            return e;
        }
        let p = match path_arg(&args, "unfocus") {
            Ok(p) => p,
            Err(e) => return e,
        };
        let path = resolve(&ctx.cwd, p);
        let mut ws = lock(&ctx.workset);
        match ws.files.iter().position(|e| e.path == path) {
            Some(i) => {
                let (focused, display, mode) = {
                    let e = &ws.files[i];
                    (e.mode == Mode::Focused, e.display.clone(), e.mode)
                };
                if focused {
                    ws.files[i].mode = Mode::Outline;
                    ws.files_dirty = true;
                    ToolOutput::ok(format!("unfocused {display}"))
                } else {
                    ToolOutput::ok(format!("{display} is not focused ({})", mode.as_str()))
                }
            }
            None => ToolOutput::err(format!(
                "unfocus: {} is not in the working set.",
                display(&ctx.cwd, &path)
            )),
        }
    }
}

struct Close;

#[async_trait::async_trait]
impl Tool for Close {
    fn effects(&self, _args: &Value) -> super::Effects {
        super::Effects::none()
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "close".into(),
            description: "Remove a file from the working set so it stops rendering into every \
                request. Close files you no longer need when the working set is over budget."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path, relative to the workspace root or absolute."}
                },
                "required": ["path"]
            }),
        }
    }

    fn summary(&self, args: &Value) -> String {
        summary_path(args)
    }

    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolOutput {
        if let Some(e) = enabled() {
            return e;
        }
        let p = match path_arg(&args, "close") {
            Ok(p) => p,
            Err(e) => return e,
        };
        let path = resolve(&ctx.cwd, p);
        let mut ws = lock(&ctx.workset);
        match ws.files.iter().position(|e| e.path == path) {
            Some(i) => {
                let e = ws.files.remove(i);
                ws.files_dirty = true;
                ToolOutput::ok(format!("closed {}", e.display))
            }
            None => ToolOutput::err(format!(
                "close: {} is not in the working set.",
                display(&ctx.cwd, &path)
            )),
        }
    }
}

struct WorksetList;

#[async_trait::async_trait]
impl Tool for WorksetList {
    fn effects(&self, _args: &Value) -> super::Effects {
        super::Effects::reads_only()
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "workset".into(),
            description: "List the files in the working set and their modes (focused, outline, \
                pinned). Contents are read from disk on every request, so these entries are \
                always current."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {}
            }),
        }
    }

    fn summary(&self, _: &Value) -> String {
        String::new()
    }

    async fn run(&self, _: Value, ctx: &ToolCtx) -> ToolOutput {
        if let Some(e) = enabled() {
            return e;
        }
        let lines = list_lines(&lock(&ctx.workset));
        if lines.is_empty() {
            ToolOutput::ok("the working set is empty.")
        } else {
            ToolOutput::ok(lines.join("\n"))
        }
    }
}

struct NoteIntent;

#[async_trait::async_trait]
impl Tool for NoteIntent {
    fn effects(&self, _args: &Value) -> super::Effects {
        super::Effects::none()
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "note_intent".into(),
            description: "Record what you just did and why (`did`, `why`, `next`). The recent \
                intents are shown back to you each request so you keep your train of thought \
                across context compaction."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "did": {"type": "string", "description": "What you just did or changed."},
                    "why": {"type": "string", "description": "The reason for doing it."},
                    "next": {"type": "string", "description": "What you plan to do next (optional)."}
                },
                "required": ["did", "why"]
            }),
        }
    }

    fn summary(&self, args: &Value) -> String {
        args.get("did")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    }

    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolOutput {
        let (Some(did), Some(why)) = (
            args.get("did").and_then(Value::as_str),
            args.get("why").and_then(Value::as_str),
        ) else {
            return ToolOutput::err("note_intent: missing required arguments `did` and `why`.");
        };
        if did.trim().is_empty() {
            return ToolOutput::err("note_intent: `did` must not be empty.");
        }
        let next = args.get("next").and_then(Value::as_str).unwrap_or("");
        lock(&ctx.workset).record_intent(did, why, next);
        ToolOutput::ok("noted")
    }
}

/// The working-set tools; absent entirely when `workset.enabled` is off so
/// they are neither listed nor callable.
pub fn register(r: &mut Registry) {
    if !crate::settings::current().workset.enabled {
        return;
    }
    r.add(Box::new(Open));
    r.add(Box::new(Focus));
    r.add(Box::new(Unfocus));
    r.add(Box::new(Close));
    r.add(Box::new(WorksetList));
    r.add(Box::new(NoteIntent));
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio_util::sync::CancellationToken;

    fn ctx(dir: &Path, ws: &Shared) -> ToolCtx {
        ToolCtx {
            cwd: dir.to_path_buf(),
            cancel: CancellationToken::new(),
            workset: Arc::clone(ws),
            ..Default::default()
        }
    }

    fn run(tool: &dyn Tool, args: Value, dir: &Path, ws: &Shared) -> ToolOutput {
        futures::executor::block_on(tool.run(args, &ctx(dir, ws)))
    }

    #[test]
    fn open_defaults_and_modes() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn a() {}\n").unwrap();
        std::fs::write(dir.path().join("spec.md"), "# spec\n").unwrap();
        let ws = Shared::default();
        let out = run(&Open, json!({"path": "a.rs"}), dir.path(), &ws);
        assert_eq!(out.content, "opened a.rs (outline)");
        let out = run(&Open, json!({"path": "spec.md"}), dir.path(), &ws);
        assert_eq!(out.content, "opened spec.md (pinned)");
        let out = run(&Open, json!({"path": "a.rs", "mode": "focused"}), dir.path(), &ws);
        assert_eq!(out.content, "updated a.rs (focused)");
        assert!(run(&Open, json!({"path": "a.rs", "mode": "bogus"}), dir.path(), &ws).is_error);
        assert!(run(&Open, json!({"path": "missing.rs"}), dir.path(), &ws).is_error);
        assert!(run(&Open, json!({"path": "."}), dir.path(), &ws).is_error);
        let lines = {
            let ws = lock(&ws);
            list_lines(&ws)
        };
        assert_eq!(lines, vec!["a.rs (focused)", "spec.md (pinned)"]);
    }

    #[test]
    fn focus_unfocus_close() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn a() {}\n").unwrap();
        let ws = Shared::default();
        // focus opens the file if needed.
        run(&Focus, json!({"path": "a.rs"}), dir.path(), &ws);
        assert_eq!(lock(&ws).files[0].mode, Mode::Focused);
        run(&Unfocus, json!({"path": "a.rs"}), dir.path(), &ws);
        assert_eq!(lock(&ws).files[0].mode, Mode::Outline);
        run(&Close, json!({"path": "a.rs"}), dir.path(), &ws);
        assert!(lock(&ws).files.is_empty());
        assert!(run(&Close, json!({"path": "a.rs"}), dir.path(), &ws).is_error);
        assert!(run(&Unfocus, json!({"path": "a.rs"}), dir.path(), &ws).is_error);
    }

    #[test]
    fn files_dirty_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn a() {}\n").unwrap();
        let ws = Shared::default();
        {
            let mut w = lock(&ws);
            assert!(w.take_files_dirty().is_none());
        }
        run(&Open, json!({"path": "a.rs"}), dir.path(), &ws);
        let state = lock(&ws).take_files_dirty().unwrap();
        assert!(lock(&ws).take_files_dirty().is_none());
        let mut restored = Workset::default();
        restored.restore_files(&state, dir.path());
        assert_eq!(restored.files.len(), 1);
        assert_eq!(restored.files[0].display, "a.rs");
        assert_eq!(restored.files[0].mode, Mode::Outline);
    }

    #[test]
    fn note_intent_records_and_drains() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Shared::default();
        let out = run(
            &NoteIntent,
            json!({"did": "added tests", "why": "coverage", "next": "commit"}),
            dir.path(),
            &ws,
        );
        assert_eq!(out.content, "noted");
        assert!(run(&NoteIntent, json!({"did": "x"}), dir.path(), &ws).is_error);
        let pending = lock(&ws).take_pending_intents();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].did, "added tests");
        assert!(lock(&ws).take_pending_intents().is_empty());
    }

    #[test]
    fn render_orders_and_demotes() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..3 {
            std::fs::write(
                dir.path().join(format!("f{i}.rs")),
                format!("fn f{i}() {{\n    body\n}}\n"),
            )
            .unwrap();
        }
        let mut ws = Workset::default();
        ws.open(dir.path(), dir.path().join("f0.rs"), Mode::Focused);
        ws.open(dir.path(), dir.path().join("f1.rs"), Mode::Focused);
        ws.open(dir.path(), dir.path().join("f2.rs"), Mode::Outline);
        let budgets = Budgets {
            max_focused: 1,
            max_tokens: 1_000_000,
            intents: 0,
        };
        let note = ws.render(&budgets);
        // f0 is the least recently focused → demoted; f1 stays focused.
        let f0 = note.find("f0.rs").unwrap();
        let f1 = note.find("f1.rs").unwrap();
        assert!(f0 < f1, "{note}");
        assert!(note.contains("f0.rs (outline)"), "{note}");
        assert!(note.contains("f1.rs (focused)"), "{note}");
        // Outlines elide bodies; the focused file shows it in full.
        assert!(note.contains("fn f0() { … }"), "{note}");
        assert!(note.contains("fn f2() { … }"), "{note}");
        assert!(note.contains("    body"), "{note}");
    }

    #[test]
    fn render_omits_over_budget() {
        let dir = tempfile::tempdir().unwrap();
        let big = "x".repeat(4000);
        std::fs::write(dir.path().join("a.md"), &big).unwrap();
        std::fs::write(dir.path().join("b.md"), &big).unwrap();
        let mut ws = Workset::default();
        ws.open(dir.path(), dir.path().join("a.md"), Mode::Pinned);
        ws.open(dir.path(), dir.path().join("b.md"), Mode::Pinned);
        let budgets = Budgets {
            max_focused: 4,
            max_tokens: 100,
            intents: 0,
        };
        let note = ws.render(&budgets);
        assert!(note.contains("files omitted"), "{note}");
        assert!(note.contains("`close`"), "{note}");
    }

    #[test]
    fn render_recent_intents() {
        let mut ws = Workset::default();
        for i in 0..10 {
            ws.record_intent(&format!("did{i}"), "w", "n");
        }
        let note = ws.render(&Budgets {
            max_focused: 4,
            max_tokens: 1000,
            intents: 3,
        });
        assert!(note.contains("[Recent intents]"), "{note}");
        assert!(note.contains("did: did9"), "{note}");
        assert!(note.contains("did: did7"), "{note}");
        assert!(!note.contains("did: did6"), "{note}");
    }
}
