//! Tools the model can call. Built-in workspace tools live in submodules; MCP
//! tools are adapted into the same [`Tool`] trait by `crate::mcp`.

pub(crate) mod artifacts;
pub(crate) mod background;
mod bash;
pub(crate) use bash::{live_tail, run_shell};
mod fs;
pub(crate) use fs::FileLedger;
mod html;
pub mod image;
pub(crate) mod mcp_search;
pub(crate) mod memory;
pub(crate) mod patch;
pub(crate) mod redact;
#[cfg(feature = "scripting")]
mod run_code;
pub(crate) mod safeio;
mod search;
pub(crate) mod session_search;
pub mod subagent;
pub(crate) mod team;
pub(crate) mod typed;
pub mod web;
pub(crate) mod workset;

use std::collections::{BTreeMap, HashSet};
use std::fs::File;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::types::ToolSpec;

/// Session tools that are not MCP tools but still pass to subagents.
const SHARED: &[&str] = &[
    "skill",
    "install_skill",
    "mcp_list_resources",
    "mcp_read_resource",
];

/// Directories and files the read tools must not open: nyq's own tokens,
/// the codex CLI login, SSH keys, cloud credentials, and `~/.netrc`.
/// Installed skills (`~/.nyq/skills`) stay readable so their scripts and
/// documents can be opened.
#[derive(Clone)]
pub(crate) struct SecretRoots {
    roots: Vec<PathBuf>,
    /// Readable subdirectories of a secret root.
    open: Vec<PathBuf>,
}

impl SecretRoots {
    pub(crate) fn load() -> Self {
        let mut roots = Vec::new();
        let mut open = Vec::new();
        let home = crate::config::home();
        add_secret(&mut roots, home.clone());
        add_secret(&mut open, home.join("skills"));
        if let Some(user) = dirs::home_dir() {
            add_secret(&mut roots, user.join(".nyq"));
            add_secret(&mut open, user.join(".nyq").join("skills"));
            add_secret(&mut roots, user.join(".codex"));
            add_secret(&mut roots, user.join(".ssh"));
            add_secret(&mut roots, user.join(".aws"));
            add_secret(&mut roots, user.join(".netrc"));
        }
        let auth = crate::config::codex_auth_path(&home);
        // Always deny the auth file itself. Its parent is denied only when it
        // is a known credential directory (`~/.nyq`, `~/.codex`), which are
        // added above. A custom `NYQ_CODEX_AUTH` such as `/repo/auth.json`
        // must not make the rest of that directory unreadable.
        add_secret(&mut roots, auth);
        Self { roots, open }
    }

    /// `Some` when `user_path` or `resolved` lands in a credential location.
    pub(crate) fn reject(&self, user_path: &str, resolved: &Path) -> Option<&'static str> {
        let hit = self.blocks_path(resolved)
            || expand_tilde(user_path).is_some_and(|p| self.blocks_path(&p));
        hit.then_some("refusing to read credential files")
    }

    pub(crate) fn blocks_path(&self, path: &Path) -> bool {
        let lexical = lexical(path);
        if self.covers(&lexical) {
            return true;
        }
        self.covers(&resolved_path(path))
    }

    fn covers(&self, path: &Path) -> bool {
        let under = |dirs: &[PathBuf]| dirs.iter().any(|d| path == d || path.starts_with(d));
        under(&self.roots) && !under(&self.open)
    }
}

fn add_secret(roots: &mut Vec<PathBuf>, path: PathBuf) {
    let lexical = lexical(&path);
    if lexical.as_os_str().is_empty() || lexical == Path::new("/") {
        return;
    }
    let real = std::fs::canonicalize(&lexical).unwrap_or_else(|_| lexical.clone());
    for candidate in [lexical, real] {
        if !roots.iter().any(|r| r == &candidate) {
            roots.push(candidate);
        }
    }
}

fn expand_tilde(p: &str) -> Option<PathBuf> {
    if p == "~" {
        return dirs::home_dir();
    }
    let rest = p.strip_prefix("~/")?;
    Some(dirs::home_dir()?.join(rest))
}

fn lexical(path: &Path) -> PathBuf {
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };
    let mut out = PathBuf::new();
    for c in abs.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(out.components().next_back(), Some(Component::Normal(_))) {
                    out.pop();
                }
            }
            other => out.push(other),
        }
    }
    out
}

/// Existing prefix, canonicalized, with any missing suffix put back.
fn resolved_path(path: &Path) -> PathBuf {
    let lexical = lexical(path);
    if let Ok(real) = std::fs::canonicalize(&lexical) {
        return real;
    }
    let mut rest = Vec::new();
    let mut cur = lexical.as_path();
    while !cur.as_os_str().is_empty() {
        if let Ok(real) = std::fs::canonicalize(cur) {
            let mut out = real;
            for name in rest.iter().rev() {
                out.push(name);
            }
            return out;
        }
        let Some(name) = cur.file_name() else { break };
        rest.push(name.to_os_string());
        match cur.parent() {
            Some(parent) if parent != cur => cur = parent,
            _ => break,
        }
    }
    lexical
}

/// Instruction and skill files are capped before they are decoded.
pub(crate) const MAX_ROOTED_TEXT: u64 = 1024 * 1024;

/// Read a regular file that stays inside `root`.
///
/// The final component must not be a symlink. The canonical path must remain
/// under `root`. Credential locations are refused unless `root` itself is one
/// (user skills live under `~/.nyq`). At most [`MAX_ROOTED_TEXT`] bytes are read.
pub(crate) fn read_rooted_text(root: &Path, path: &Path) -> Result<String, String> {
    let meta = std::fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if meta.file_type().is_symlink() {
        return Err("refusing to follow a symlink".into());
    }
    if !meta.is_file() {
        return Err("not a regular file".into());
    }
    let root_real = std::fs::canonicalize(root).map_err(|e| e.to_string())?;
    let real = std::fs::canonicalize(path).map_err(|e| e.to_string())?;
    if !real.starts_with(&root_real) {
        return Err("outside the project root".into());
    }
    let secrets = SecretRoots::load();
    if secrets.blocks_path(&real) && !secrets.blocks_path(&root_real) {
        return Err("refusing to read credential files".into());
    }
    let file = File::open(&real).map_err(|e| e.to_string())?;
    if !file.metadata().map(|m| m.is_file()).unwrap_or(false) {
        return Err("not a regular file".into());
    }
    let mut buf = Vec::new();
    file.take(MAX_ROOTED_TEXT.saturating_add(1))
        .read_to_end(&mut buf)
        .map_err(|e| e.to_string())?;
    if buf.len() as u64 > MAX_ROOTED_TEXT {
        return Err(format!("file exceeds {MAX_ROOTED_TEXT} bytes"));
    }
    String::from_utf8(buf).map_err(|_| "not UTF-8 text".into())
}

/// Whether `user_path` resolves outside `cwd` (including through a symlink).
pub(crate) fn outside_workspace(cwd: &Path, user_path: &str) -> bool {
    !crate::agent::inside(cwd, user_path)
}

/// A `path` argument that points outside the workspace needs approval;
/// installed skills (`~/.nyq/skills`) can be read without asking.
pub(crate) fn outside_path_arg(cwd: &Path, args: &Value) -> bool {
    let path = args.get("path").and_then(Value::as_str).unwrap_or(".");
    outside_workspace(cwd, path) && !in_installed_skills(path)
}

fn in_installed_skills(path: &str) -> bool {
    let Some(full) =
        expand_tilde(path).or_else(|| Path::new(path).is_absolute().then(|| PathBuf::from(path)))
    else {
        return false;
    };
    // Fully resolved, so a link inside a skill that points elsewhere still asks.
    let skills = crate::config::home().join("skills");
    std::fs::canonicalize(&skills).is_ok_and(|skills| resolved_path(&full).starts_with(skills))
}

/// A path as shown to the model and user: always with `/` separators.
pub(crate) fn slash_path(p: &std::path::Path) -> String {
    let s = p.display().to_string();
    if cfg!(windows) {
        s.replace('\\', "/")
    } else {
        s
    }
}

#[derive(Clone, Default)]
pub struct ToolCtx {
    /// Workspace root; relative paths resolve against it.
    pub cwd: PathBuf,
    /// Fires when the user cancels the turn (Ctrl-C).
    pub cancel: CancellationToken,
    /// How shell commands are confined.
    pub sandbox: std::sync::Arc<crate::sandbox::Policy>,
    /// A copy of the session's model provider, for tools that run their own
    /// model calls (subagents). None in tests.
    pub provider: Option<std::sync::Arc<dyn crate::provider::Provider>>,
    /// True when the session mode is yolo. Callers that only set this still
    /// skip prompts; `permission_mode` is used when this is false.
    pub yolo: bool,
    /// The session permission mode, inherited by subagents.
    pub permission_mode: crate::permissions::Mode,
    /// Shared auto-mode decision cache for this session.
    pub review_cache: std::sync::Arc<
        std::sync::Mutex<std::collections::HashMap<String, crate::classifier::Verdict>>,
    >,
    /// Rules granted with "always", inherited by subagents.
    pub allowed: HashSet<String>,
    /// Asks the user about calls made outside the agent loop (a subagent's
    /// tool calls); None when nobody can be asked, so such calls are denied.
    pub approver: Option<std::sync::Arc<dyn crate::agent::Approver>>,
    /// The session's permission rules, applied to subagents' calls too.
    pub permissions: std::sync::Arc<crate::permissions::Permissions>,
    /// Rules granted with "always" while this call ran (a subagent's calls);
    /// the loop adds them to its session grants afterwards.
    pub granted: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    /// The conversation's id; scopes stored tool results (`read_tool_result`).
    pub session_id: Option<String>,
    /// The session's hooks; subagents run its tool hooks for their calls.
    pub hooks: std::sync::Arc<crate::hooks::Hooks>,
    /// Where this session's named subagents are saved; None without a session.
    pub subagent_dir: Option<PathBuf>,
    /// Session tools a subagent inherits: MCP tools and skills.
    pub shared_tools: Vec<std::sync::Arc<dyn Tool>>,
    /// The session working set (`open`/`focus`/`close`/`note_intent`). An
    /// empty set is harmless where no session owns it (tests, subagent ctxs).
    pub workset: std::sync::Arc<std::sync::Mutex<workset::Workset>>,
    /// Files this session read or wrote, for the read-before-edit and mtime
    /// guard the mutating file tools enforce.
    pub file_ledger: std::sync::Arc<FileLedger>,
    /// For a `subagent` call whose arguments copy untrusted content: the
    /// taint the child inherits (see `crate::taint::Delegation`). Set by
    /// dispatch, never by the model.
    pub delegation: Option<crate::taint::Delegation>,
}

#[derive(Clone, Debug, Default)]
pub struct ToolOutput {
    /// Text returned to the model.
    pub content: String,
    pub is_error: bool,
    /// Optional richer text for the user (e.g. a unified diff). Not sent to
    /// the model.
    pub display: Option<String>,
    /// Images for the model (sent in a follow-up user message).
    pub images: Vec<crate::types::Image>,
    /// The tool already changed the workspace, so a failed result must keep
    /// its undo snapshot.
    pub changed: bool,
    /// File snapshots returned by a subagent, including on failure.
    pub undo: crate::undo::UndoLog,
}

impl ToolOutput {
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            ..Self::default()
        }
    }
    pub fn err(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
            ..Self::default()
        }
    }
}

/// The filesystem paths a call may write, when it can enumerate them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Writes {
    /// Exactly these paths (an empty list means the call writes nothing).
    Known(Vec<PathBuf>),
    /// The call may write anywhere; its paths can't be listed (a shell
    /// command, an undeclared MCP or extension tool).
    Unknown,
}

impl Default for Writes {
    /// The safe default for a tool that does not declare its writes.
    fn default() -> Self {
        Self::Unknown
    }
}

impl Writes {
    /// True when the call provably writes nothing.
    pub fn is_empty(&self) -> bool {
        matches!(self, Self::Known(paths) if paths.is_empty())
    }

    /// The known write paths; empty when unknown.
    #[allow(dead_code)] // Part of the effects API; used by callers outside dispatch.
    pub fn paths(&self) -> &[PathBuf] {
        match self {
            Self::Known(paths) => paths,
            Self::Unknown => &[],
        }
    }
}

/// What a call with a given argument set may touch: filesystem, child
/// processes, the network. `Tool::effects` returns this per call.
#[derive(Clone, Debug)]
pub struct Effects {
    /// Reads filesystem state (files or directories).
    pub reads: bool,
    /// Paths written, or [`Writes::Unknown`] when they can't be listed.
    pub writes: Writes,
    /// Starts a child process.
    pub exec: bool,
    /// Talks to the network.
    #[allow(dead_code)]
    // Declared for consumers of the effects model (permissions, scheduling).
    pub net: bool,
}

impl Default for Effects {
    /// The safe default for a tool that declares no effects: it may write
    /// anywhere, so it schedules alone and holds the writer lease.
    fn default() -> Self {
        Self {
            reads: false,
            writes: Writes::Unknown,
            exec: false,
            net: false,
        }
    }
}

impl Effects {
    /// Reads only; touches nothing else.
    pub fn reads_only() -> Self {
        Self {
            reads: true,
            writes: Writes::Known(vec![]),
            exec: false,
            net: false,
        }
    }

    /// Reads, and writes exactly `paths` (as given in the arguments).
    pub fn writes_to(paths: Vec<PathBuf>) -> Self {
        Self {
            reads: true,
            writes: Writes::Known(paths),
            exec: false,
            net: false,
        }
    }

    /// No filesystem or process effects.
    pub fn none() -> Self {
        Self {
            reads: false,
            writes: Writes::Known(vec![]),
            exec: false,
            net: false,
        }
    }

    /// Whether the call mutates nothing: no writes and no child process.
    pub fn is_read_only(&self) -> bool {
        !self.exec && self.writes.is_empty()
    }

    /// The schedule class derived from the effects.
    fn concurrency(&self) -> Concurrency {
        if self.exec {
            return Concurrency::Exec;
        }
        match &self.writes {
            Writes::Unknown => Concurrency::Barrier,
            Writes::Known(paths) if !paths.is_empty() => Concurrency::WriteFs(paths.clone()),
            _ if self.reads => Concurrency::ReadFs,
            _ => Concurrency::Pure,
        }
    }
}

/// What a tool is, whatever one call's arguments are: the static
/// classification that name-level policy uses (path rules and grants, which
/// tools count as reads, the crash journal, the `intent` field), where a
/// probe of [`Tool::effects`] with made-up arguments can be wrong. The wave
/// scheduler and the writer lease still use each call's own effects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolKind {
    /// Reads files or directories (its `path`/`paths`) and changes nothing.
    Read,
    /// Writes the files its arguments name: `path`/`paths`, or the files a
    /// patch names.
    FileWrite,
    /// Runs a process.
    Exec,
    /// Talks to the network and changes nothing locally.
    Net,
    /// Delegates or coordinates other work (subagents, the team graph,
    /// scripts): its children's and sub-calls' own calls are checked, take
    /// the writer lease and carry intents; the coordinating call never holds
    /// the lease (waiting on a child that needs it would deadlock).
    Coordination,
    /// No filesystem, process or network effects (session-local state such
    /// as the working set, or reading a process's output).
    Pure,
    /// Anything else, or undeclared: it may write anywhere.
    Other,
}

impl ToolKind {
    /// The kind [`Effects`] imply for a call whose `path`/`paths` are
    /// `named`: what [`Tool::kind`]'s default reads off a probe.
    pub fn of_effects(e: &Effects, named: &[PathBuf]) -> Self {
        if e.exec {
            return Self::Exec;
        }
        match &e.writes {
            Writes::Unknown => Self::Other,
            Writes::Known(w) if !w.is_empty() => {
                if w.iter().all(|p| named.contains(p)) {
                    Self::FileWrite
                } else {
                    Self::Other
                }
            }
            _ if e.reads => Self::Read,
            _ if e.net => Self::Net,
            _ => Self::Pure,
        }
    }

    /// Whether a call changes nothing outside the session, so one cut off by
    /// a crash is safe to repeat (it is not journalled).
    pub fn is_pure_read(self) -> bool {
        matches!(self, Self::Read | Self::Net | Self::Pure)
    }

    /// Whether the tool itself changes the workspace (it gets the `intent`
    /// field; coordination tools' children and sub-calls carry their own).
    pub fn mutates(self) -> bool {
        matches!(self, Self::FileWrite | Self::Exec | Self::Other)
    }
}

/// How a call may run alongside its neighbours in the same tool round
/// (design §3). `Exec` and `Barrier` always run alone; `Pure`, `ReadFs`
/// and non-conflicting `WriteFs` calls may share a wave.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Concurrency {
    /// No filesystem or process effects; always parallel-safe.
    Pure,
    /// Reads the filesystem.
    ReadFs,
    /// Writes these paths; parallel when they are disjoint from every
    /// other read or write path in the wave.
    WriteFs(Vec<PathBuf>),
    /// Runs a process; may read or write anywhere.
    Exec,
    /// Unanalyzable or destructive.
    Barrier,
}

/// Path arguments a call reads, best effort: `path` plus the strings in
/// `paths`, resolved against `cwd` (unresolvable `..` escapes are dropped).
pub(crate) fn arg_paths(args: &Value, cwd: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut push = |s: &str| {
        if let Some(p) = crate::agent::lexical(cwd, s) {
            out.push(p);
        }
    };
    if let Some(p) = args.get("path").and_then(Value::as_str) {
        push(p);
    }
    if let Some(paths) = args.get("paths").and_then(Value::as_array) {
        for p in paths.iter().filter_map(Value::as_str) {
            push(p);
        }
    }
    out
}

#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    /// What this call may touch; the conservative default is "unknown, may
    /// write anywhere" so undeclared tools never run in parallel.
    fn effects(&self, _args: &Value) -> Effects {
        Effects::default()
    }
    /// Whether this call only reads: no writes and no child process. A
    /// network read still counts as read-only here; approval rules decide
    /// whether a remote call asks.
    fn is_read_only(&self, args: &Value) -> bool {
        self.effects(args).is_read_only()
    }
    /// How this call may be scheduled relative to its neighbours.
    fn concurrency(&self, args: &Value) -> Concurrency {
        self.effects(args).concurrency()
    }
    /// Like [`Tool::concurrency`], for tools whose class needs the
    /// workspace: bash's read-only command detection resolves the command's
    /// paths against `cwd` and the extra writable roots.
    fn concurrency_in(&self, args: &Value, cwd: &Path, extra: &[PathBuf]) -> Concurrency {
        let _ = (cwd, extra);
        self.concurrency(args)
    }
    /// Paths this call reads, best effort, resolved against `cwd` — the
    /// `path`/`paths` arguments by default. Used for wave conflict checks
    /// and nested-instruction lookup, never for permissions.
    fn read_paths(&self, args: &Value, cwd: &Path) -> Vec<PathBuf> {
        arg_paths(args, cwd)
    }
    /// The shell command this call runs, when the tool runs shell commands
    /// in the foreground (bash). Enables read-only and reversibility checks.
    fn shell_command(&self, _args: &Value) -> Option<String> {
        None
    }
    /// Whether running this call needs user approval (mutations, shell, MCP).
    fn needs_approval(&self, _args: &Value) -> bool {
        false
    }
    /// Like [`needs_approval`], but with the workspace root so reads outside
    /// it can ask even when the tool is otherwise free.
    fn needs_approval_for(&self, args: &Value, cwd: &Path) -> bool {
        let _ = cwd;
        self.needs_approval(args)
    }
    /// One-line human summary of a call, e.g. `src/main.rs` or `cargo test`.
    fn summary(&self, args: &Value) -> String;
    /// Optional preview shown in the approval prompt (e.g. the diff an edit
    /// would make). Must not have side effects.
    fn preview(&self, _args: &Value, _ctx: &ToolCtx) -> Option<String> {
        None
    }
    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolOutput;
    /// When true, dispatch waits for `run` after cancellation so the tool
    /// can kill children and notify peers. Tools that only need the request
    /// dropped leave this false.
    fn handles_cancel(&self) -> bool {
        false
    }
    /// The tool's static kind (see [`ToolKind`]). The default reads it off
    /// [`Tool::effects`] probed with a placeholder `path`/`paths`, which is
    /// right for tools whose effects come from those arguments or from none.
    /// A tool whose effects depend on other arguments must declare its kind
    /// (as `apply_patch`, whose paths come from the patch, does).
    fn kind(&self) -> ToolKind {
        let probe = serde_json::json!({"path": "x", "paths": ["y"]});
        ToolKind::of_effects(
            &self.effects(&probe),
            &[PathBuf::from("x"), PathBuf::from("y")],
        )
    }
}

/// A change to a [`Registry`] made outside the agent loop (an MCP server
/// connected in the background or changed its tool list).
pub type ToolPatch = Box<dyn FnOnce(&mut Registry) + Send>;

/// Shared between the MCP manager and the registry it serves: queued tool
/// changes, applied before each model request, and per-server instructions.
#[derive(Clone, Default)]
pub struct LiveTools {
    patches: std::sync::Arc<std::sync::Mutex<Vec<ToolPatch>>>,
    notes: std::sync::Arc<std::sync::Mutex<BTreeMap<String, String>>>,
}

impl LiveTools {
    pub fn push(&self, patch: ToolPatch) {
        self.patches
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(patch);
    }

    /// Set (or with None, clear) a server's instructions for the model.
    pub fn set_note(&self, server: &str, note: Option<String>) {
        let mut notes = self.notes.lock().unwrap_or_else(|e| e.into_inner());
        match note.filter(|n| !n.trim().is_empty()) {
            Some(n) => notes.insert(server.to_string(), n),
            None => notes.remove(server),
        };
    }

    pub fn notes(&self) -> BTreeMap<String, String> {
        self.notes.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

/// A registered tool plus its spec, cached once so lookups never rebuild
/// the parameter JSON. A patch replaces the entry, so live tool changes
/// refresh the cache without extra bookkeeping.
struct Entry {
    tool: std::sync::Arc<dyn Tool>,
    spec: ToolSpec,
}

#[derive(Default)]
pub struct Registry {
    tools: Vec<Entry>,
    /// MCP tools picked with `mcp_select_tool` while MCP tools are deferred.
    selected: HashSet<String>,
    live: Option<LiveTools>,
    /// Tool changes from extensions (`crate::extensions`).
    extensions: Option<LiveTools>,
    /// Names from the `disabled_tools` setting: never listed or callable,
    /// including tools (MCP, extensions) added later.
    disabled: HashSet<String>,
    /// The `enabled_tools` allow-list: when non-empty, names outside it are
    /// hidden the same way (profiles use it; empty means everything goes).
    enabled: HashSet<String>,
    /// Installed skills, which `mcp_search_tools` also finds.
    skills: Vec<crate::skills::Skill>,
    /// Read/write ledger for the file tools' read-before-edit guard.
    file_ledger: std::sync::Arc<FileLedger>,
}

/// Tools registered outside [`Registry::builtin`] that `disabled_tools` may name.
const OTHER_TOOLS: &[&str] = &[
    "skill",
    "install_skill",
    crate::agent::ASK_USER,
    "mcp_list_resources",
    "mcp_read_resource",
    "mcp_list_prompts",
    "mcp_get_prompt",
    // The task engine's completion tool (W9); profiles may name it already.
    "submit",
];

/// The `disabled_tools` entries that name no nyq tool. MCP (`mcp__…`) and
/// extension (`ext__…`) names can't be checked before those start.
pub fn unknown_tool_names(names: &[String]) -> Vec<String> {
    let known = Registry::all_builtin()
        .tools
        .iter()
        .map(|e| e.spec.name.clone())
        .collect::<Vec<_>>();
    names
        .iter()
        .filter(|n| {
            !(known.contains(n)
                || OTHER_TOOLS.contains(&n.as_str())
                || mcp_search::is_mcp(n)
                || n.starts_with(crate::extensions::TOOL_PREFIX))
        })
        .cloned()
        .collect()
}

impl Registry {
    /// The built-in workspace tools: read_file, write_file, edit_file, bash,
    /// glob, grep, list_dir, view_image, web_fetch, web_search, subagent,
    /// memory, bash_output, kill_shell, read_tool_result. Tools named in the
    /// `disabled_tools` setting are hidden; a non-empty `enabled_tools`
    /// hides everything outside it.
    pub fn builtin() -> Self {
        let mut r = Self::all_builtin();
        let s = crate::settings::current();
        r.disable(&s.disabled_tools);
        r.enable(&s.enabled_tools);
        if !crate::profiles::feature("memory") {
            r.disable(&["memory".to_string()]);
        }
        r
    }

    pub(crate) fn all_builtin() -> Self {
        let mut r = Self::default();
        fs::register(&mut r);
        search::register(&mut r);
        bash::register(&mut r);
        background::register(&mut r);
        memory::register(&mut r);
        artifacts::register(&mut r);
        session_search::register(&mut r);
        image::register(&mut r);
        web::register(&mut r);
        subagent::register(&mut r);
        workset::register(&mut r);
        team::register(&mut r);
        #[cfg(feature = "scripting")]
        run_code::register(&mut r);
        r
    }

    /// Hide these tools from the model, now and when added later.
    pub fn disable(&mut self, names: &[String]) {
        self.disabled.extend(names.iter().cloned());
    }

    /// Make `names` the disabled tools (after `disabled_tools` changed).
    pub fn set_disabled(&mut self, names: &[String]) {
        self.disabled = names.iter().cloned().collect();
    }

    /// Make `names` the tool allow-list (after `enabled_tools` changed);
    /// an empty list allows everything.
    pub fn enable(&mut self, names: &[String]) {
        self.enabled = names.iter().cloned().collect();
    }

    /// Not offered or callable: disabled, or outside a set allow-list.
    fn hidden(&self, name: &str) -> bool {
        self.disabled.contains(name) || (!self.enabled.is_empty() && !self.enabled.contains(name))
    }

    /// Every registered tool, disabled ones included, for `/tools`: its spec,
    /// whether it is disabled, and whether a call asks by default (probed
    /// with no arguments, inside `cwd`).
    pub fn catalog(&self, cwd: &Path) -> Vec<(ToolSpec, bool, bool)> {
        let none = Value::Object(Default::default());
        self.tools
            .iter()
            .map(|e| {
                let disabled = self.hidden(&e.spec.name);
                // Reads inside the workspace never ask (see `Agent::prepare`).
                let asks = e.tool.needs_approval_for(&none, cwd)
                    && !matches!(e.tool.concurrency(&none), Concurrency::ReadFs);
                (e.spec.clone(), disabled, asks)
            })
            .collect()
    }

    /// Receive tool changes and server instructions from `live`.
    pub fn attach(&mut self, live: LiveTools) {
        self.live = Some(live);
    }

    /// Receive tool changes from running extensions.
    pub fn attach_extensions(&mut self, live: LiveTools) {
        self.extensions = Some(live);
    }

    /// Apply queued tool changes.
    pub fn refresh(&mut self) {
        for live in [self.live.clone(), self.extensions.clone()]
            .into_iter()
            .flatten()
        {
            let patches =
                std::mem::take(&mut *live.patches.lock().unwrap_or_else(|e| e.into_inner()));
            for patch in patches {
                patch(self);
            }
        }
    }

    /// The skills `mcp_search_tools` searches along with MCP tools.
    pub fn set_skills(&mut self, skills: Vec<crate::skills::Skill>) {
        self.skills = skills;
    }

    /// The session's file ledger (a subagent's registry has its own).
    pub fn file_ledger(&self) -> std::sync::Arc<FileLedger> {
        self.file_ledger.clone()
    }

    /// Instructions of connected MCP servers, by server name.
    pub fn mcp_notes(&self) -> BTreeMap<String, String> {
        self.live.as_ref().map(LiveTools::notes).unwrap_or_default()
    }

    pub fn add(&mut self, tool: Box<dyn Tool>) {
        self.try_insert(tool).expect("duplicate tool name");
    }

    /// Insert `tool`, refusing to replace an existing name.
    pub fn try_insert(&mut self, tool: Box<dyn Tool>) -> Result<(), String> {
        self.try_insert_shared(std::sync::Arc::from(tool))
    }

    /// Like [`Registry::try_insert`], for a tool another registry also holds.
    pub fn try_insert_shared(&mut self, tool: std::sync::Arc<dyn Tool>) -> Result<(), String> {
        let spec = tool.spec();
        if self.tools.iter().any(|e| e.spec.name == spec.name) {
            return Err(format!(
                "tool `{}` collides with an existing tool",
                spec.name
            ));
        }
        self.tools.push(Entry { tool, spec });
        Ok(())
    }

    /// The session's own tools a subagent inherits: MCP tools (and MCP
    /// resources), extension tools and skills.
    pub fn shared(&self) -> Vec<std::sync::Arc<dyn Tool>> {
        self.tools
            .iter()
            .filter(|e| {
                let name = &e.spec.name;
                !self.hidden(name)
                    && (mcp_search::is_mcp(name)
                        || name.starts_with(crate::extensions::TOOL_PREFIX)
                        || SHARED.contains(&name.as_str()))
            })
            .map(|e| e.tool.clone())
            .collect()
    }

    pub fn remove_names(&mut self, names: &[String]) {
        if names.is_empty() {
            return;
        }
        self.tools
            .retain(|e| !names.iter().any(|n| n == &e.spec.name));
    }

    pub fn remove_prefix(&mut self, prefix: &str) {
        self.tools.retain(|e| !e.spec.name.starts_with(prefix));
    }

    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        if self.hidden(name) {
            return None;
        }
        self.tools
            .iter()
            .find(|e| e.spec.name == name)
            .map(|e| e.tool.as_ref())
    }

    /// The cached spec for `name`, built once when the tool was inserted.
    pub(crate) fn spec_of(&self, name: &str) -> Option<&ToolSpec> {
        if self.disabled.contains(name) {
            return None;
        }
        self.tools
            .iter()
            .find(|e| e.spec.name == name)
            .map(|e| &e.spec)
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools
            .iter()
            .map(|e| e.spec.clone())
            .filter(|s| !self.hidden(&s.name))
            .collect()
    }

    fn mcp_specs(&self) -> Vec<ToolSpec> {
        self.specs()
            .into_iter()
            .filter(|s| mcp_search::is_mcp(&s.name))
            .collect()
    }

    /// The tools offered to the model on the next request. When MCP tools are
    /// too many to list, only selected ones are, plus the search/select tools.
    pub fn advertised(&self) -> Vec<ToolSpec> {
        let mcp = self.mcp_specs();
        if !mcp_search::should_defer(&mcp) {
            return self.specs();
        }
        let mut out: Vec<ToolSpec> = self
            .specs()
            .into_iter()
            .filter(|s| !mcp_search::is_mcp(&s.name) || self.selected.contains(&s.name))
            .collect();
        out.extend(mcp_search::meta_specs(&mcp));
        out
    }

    /// `mcp_search_tools` / `mcp_select_tool` while MCP tools are deferred.
    pub fn is_meta(&self, name: &str) -> bool {
        (name == mcp_search::SEARCH || name == mcp_search::SELECT)
            && mcp_search::should_defer(&self.mcp_specs())
    }

    /// A direct call to an MCP tool whose schema is hidden until it is selected.
    pub fn deferred_block(&self, name: &str) -> Option<String> {
        if !mcp_search::is_mcp(name) {
            return None;
        }
        let mcp = self.mcp_specs();
        if !mcp_search::should_defer(&mcp) || self.selected.contains(name) {
            return None;
        }
        Some(format!(
            "`{name}` is not callable until you select it. Call {} to find it, then {} with its exact name.",
            mcp_search::SEARCH,
            mcp_search::SELECT
        ))
    }

    /// Run `mcp_search_tools` / `mcp_select_tool` (None for any other call,
    /// or when MCP tools are not being deferred).
    pub fn handle_meta(&mut self, name: &str, arguments: &str) -> Option<ToolOutput> {
        if name != mcp_search::SEARCH && name != mcp_search::SELECT {
            return None;
        }
        let mcp = self.mcp_specs();
        if !mcp_search::should_defer(&mcp) {
            return None;
        }
        let args: Value = serde_json::from_str(arguments).unwrap_or_default();
        if name == mcp_search::SEARCH {
            return Some(
                mcp_search::search(&mcp, &self.skills, &args, &self.mcp_notes())
                    .map_or_else(ToolOutput::err, ToolOutput::ok),
            );
        }
        let wanted = args
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        Some(if mcp.iter().any(|s| s.name == wanted) {
            self.selected.insert(wanted.to_string());
            ToolOutput::ok(format!("Selected `{wanted}`. It is callable from the next step on; call `{wanted}` with arguments matching its schema."))
        } else {
            ToolOutput::err(format!(
                "No connected MCP tool is named `{wanted}`; use {} to find exact names.",
                mcp_search::SEARCH
            ))
        })
    }
}

#[cfg(test)]
impl SecretRoots {
    fn from_paths(paths: impl IntoIterator<Item = PathBuf>) -> Self {
        let mut roots = Vec::new();
        for path in paths {
            add_secret(&mut roots, path);
        }
        Self {
            roots,
            open: Vec::new(),
        }
    }
}

#[cfg(test)]
mod registry_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Every built-in tool's static kind, decided here on purpose: a new
    /// tool fails this test until someone says what it is. (The default
    /// probe of `effects` once called `apply_patch` a read.)
    #[test]
    fn every_built_in_tool_has_the_intended_kind() {
        use ToolKind::*;
        let expected: &[(&str, ToolKind)] = &[
            ("read_file", Read),
            ("list_dir", Read),
            ("glob", Read),
            ("grep", Read),
            ("view_image", Read),
            ("vision", Read),
            (artifacts::TOOL, Read),
            (session_search::TOOL, Read),
            ("open", Read),
            ("focus", Read),
            ("workset", Read),
            ("write_file", FileWrite),
            ("edit_file", FileWrite),
            ("multi_edit", FileWrite),
            ("edit_range", FileWrite),
            ("apply_patch", FileWrite),
            ("bash", Exec),
            ("process_write", Exec),
            ("kill_shell", Exec),
            ("web_fetch", Net),
            ("web_search", Net),
            ("process_wait", Net),
            ("bash_output", Pure),
            ("unfocus", Pure),
            ("close", Pure),
            ("note_intent", Pure),
            ("team_task_list", Pure),
            ("memory", Other),
            ("subagent", Coordination),
            ("team_task_create", Coordination),
            ("team_task_claim", Coordination),
            ("team_task_update", Coordination),
            ("send_message", Coordination),
            ("wait_agent", Coordination),
            ("run_code", Coordination),
            ("workflow", Coordination),
        ];
        let r = Registry::all_builtin();
        for spec in r.specs() {
            let want = expected
                .iter()
                .find(|(n, _)| *n == spec.name)
                .map(|(_, k)| *k)
                .unwrap_or_else(|| panic!("add `{}` to this table with its kind", spec.name));
            assert_eq!(r.get(&spec.name).unwrap().kind(), want, "{}", spec.name);
        }
        // A patch's kind does not depend on whether a probe has a patch; a
        // call naming no file is never treated as a read.
        let patch = r.get("apply_patch").unwrap();
        assert!(!patch.effects(&serde_json::json!({})).is_read_only());
        assert_eq!(patch.kind(), FileWrite);
    }

    /// Counts how often its spec is built.
    struct Counted {
        calls: std::sync::Arc<AtomicUsize>,
        name: &'static str,
    }
    #[async_trait::async_trait]
    impl Tool for Counted {
        fn spec(&self) -> ToolSpec {
            self.calls.fetch_add(1, Ordering::SeqCst);
            ToolSpec {
                name: self.name.into(),
                description: String::new(),
                parameters: serde_json::json!({}),
            }
        }
        fn summary(&self, _: &Value) -> String {
            String::new()
        }
        async fn run(&self, _: Value, _: &ToolCtx) -> ToolOutput {
            ToolOutput::ok("x")
        }
    }

    #[test]
    fn a_spec_is_built_once_per_registration() {
        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        let mut r = Registry::default();
        r.add(Box::new(Counted {
            calls: calls.clone(),
            name: "t",
        }));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // Lookups, listings and catalog probes reuse the cached spec.
        for _ in 0..3 {
            r.get("t");
            r.spec_of("t");
            r.specs();
            r.catalog(Path::new("."));
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_live_patch_refreshes_the_cached_spec() {
        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        let live = LiveTools::default();
        let mut r = Registry::default();
        r.attach(live.clone());
        r.add(Box::new(Counted {
            calls: calls.clone(),
            name: "t",
        }));
        // A patch that swaps the tool builds its spec on insertion.
        let c = calls.clone();
        live.push(Box::new(move |r: &mut Registry| {
            r.remove_names(&["t".to_string()]);
            r.add(Box::new(Counted {
                calls: c,
                name: "t",
            }));
        }));
        r.refresh();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(r.get("t").is_some());
        r.specs();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    use serde_json::Map;

    struct Stub(&'static str);

    #[async_trait::async_trait]
    impl Tool for Stub {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: self.0.to_string(),
                description: String::new(),
                parameters: Value::Object(Map::new()),
            }
        }
        fn summary(&self, _args: &Value) -> String {
            String::new()
        }
        async fn run(&self, _args: Value, _ctx: &ToolCtx) -> ToolOutput {
            ToolOutput::default()
        }
    }

    /// A registry with a couple of fake names, no filesystem needed.
    fn reg() -> Registry {
        let mut r = Registry::default();
        r.add(Box::new(Stub("bash")));
        r.add(Box::new(Stub("view_image")));
        r
    }

    #[test]
    fn the_allow_list_hides_everything_else_and_disabled_still_wins() {
        let mut r = reg();
        assert!(r.get("bash").is_some() && r.get("view_image").is_some());
        r.enable(&["bash".to_string()]);
        assert!(r.get("bash").is_some());
        assert!(r.get("view_image").is_none());
        assert_eq!(r.specs().len(), 1);
        // disabled_tools still wins over the allow-list.
        r.disable(&["bash".to_string()]);
        assert!(r.get("bash").is_none());
        assert!(r.specs().is_empty());
        // An empty allow-list allows everything again.
        r.enable(&[]);
        assert!(r.get("view_image").is_some());
        assert!(r.get("bash").is_none());
    }
}

#[cfg(test)]
mod secret_tests {
    use super::*;

    #[test]
    fn secret_roots_follow_symlinks_and_stop_at_the_directory() {
        let dir = tempfile::tempdir().unwrap();
        let secret = dir.path().join("secret");
        std::fs::create_dir(&secret).unwrap();
        std::fs::write(secret.join("token"), "x").unwrap();
        let roots = SecretRoots::from_paths([secret.clone()]);
        assert!(roots.blocks_path(&secret.join("token")));
        assert!(!roots.blocks_path(&dir.path().join("other")));
        assert!(!roots.blocks_path(&secret.join("../other")));
        assert!(!roots.blocks_path(&dir.path().join("secret-backup")));
        #[cfg(unix)]
        {
            let link = dir.path().join("link");
            std::os::unix::fs::symlink(secret.join("token"), &link).unwrap();
            assert!(roots.blocks_path(&link));
        }
    }

    #[test]
    fn installed_skills_stay_readable() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let skill = home.join("skills/review");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(skill.join("check.sh"), "echo ok").unwrap();
        std::fs::write(home.join("auth.json"), "{}").unwrap();
        let mut roots = SecretRoots::from_paths([home.clone()]);
        add_secret(&mut roots.open, home.join("skills"));
        assert!(!roots.blocks_path(&skill.join("check.sh")));
        assert!(roots.blocks_path(&home.join("auth.json")));
        assert!(roots.blocks_path(&home.join("skills/../auth.json")));
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(home.join("auth.json"), skill.join("leak")).unwrap();
            assert!(roots.blocks_path(&skill.join("leak")));
        }
    }

    #[test]
    fn custom_auth_file_does_not_block_its_parent() {
        let _lock = crate::provider::sse::tests::ENV
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let auth = dir.path().join("auth.json");
        std::fs::write(&auth, "{}").unwrap();
        let project = dir.path().join("src/main.rs");
        std::fs::create_dir_all(project.parent().unwrap()).unwrap();
        std::fs::write(&project, "fn main() {}\n").unwrap();
        let old = std::env::var_os("NYQ_CODEX_AUTH");
        std::env::set_var("NYQ_CODEX_AUTH", &auth);
        let roots = SecretRoots::load();
        assert!(roots.blocks_path(&auth), "the auth file itself is denied");
        assert!(
            !roots.blocks_path(&project),
            "a custom auth parent must stay readable"
        );
        assert!(!roots.blocks_path(dir.path()));
        match old {
            Some(v) => std::env::set_var("NYQ_CODEX_AUTH", v),
            None => std::env::remove_var("NYQ_CODEX_AUTH"),
        }
    }
}
