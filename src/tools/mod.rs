//! Tools the model can call. Built-in workspace tools live in submodules; MCP
//! tools are adapted into the same [`Tool`] trait by `crate::mcp`.

mod bash;
pub(crate) use bash::run_shell;
mod fs;
pub mod image;
pub(crate) mod safeio;
mod search;
pub mod web;
pub mod subagent;

use std::collections::HashSet;
use std::fs::File;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::types::ToolSpec;

/// Directories and files the read tools must not open: nyq's own tokens,
/// the codex CLI login, SSH keys, cloud credentials, and `~/.netrc`.
#[derive(Clone)]
pub(crate) struct SecretRoots {
    roots: Vec<PathBuf>,
}

impl SecretRoots {
    pub(crate) fn load() -> Self {
        let mut roots = Vec::new();
        let fx = crate::config::home();
        add_secret(&mut roots, fx.clone());
        if let Some(user) = dirs::home_dir() {
            add_secret(&mut roots, user.join(".fx"));
            add_secret(&mut roots, user.join(".codex"));
            add_secret(&mut roots, user.join(".ssh"));
            add_secret(&mut roots, user.join(".aws"));
            add_secret(&mut roots, user.join(".netrc"));
        }
        let auth = crate::config::codex_auth_path(&fx);
        // Always deny the auth file itself. Its parent is denied only when it
        // is a known credential directory (`~/.fx`, `~/.codex`), which are
        // added above. A custom `NYQ_CODEX_AUTH` such as `/repo/auth.json`
        // must not make the rest of that directory unreadable.
        add_secret(&mut roots, auth);
        Self { roots }
    }

    /// `Some` when `user_path` or `resolved` lands in a credential location.
    pub(crate) fn reject(&self, user_path: &str, resolved: &Path) -> Option<&'static str> {
        let hit = self.blocks_path(resolved) || expand_tilde(user_path).is_some_and(|p| self.blocks_path(&p));
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
        self.roots.iter().any(|root| path == root || path.starts_with(root))
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
/// (user skills live under `~/.fx`). At most [`MAX_ROOTED_TEXT`] bytes are read.
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
    file.take(MAX_ROOTED_TEXT.saturating_add(1)).read_to_end(&mut buf).map_err(|e| e.to_string())?;
    if buf.len() as u64 > MAX_ROOTED_TEXT {
        return Err(format!("file exceeds {MAX_ROOTED_TEXT} bytes"));
    }
    String::from_utf8(buf).map_err(|_| "not UTF-8 text".into())
}

/// Whether `user_path` resolves outside `cwd` (including through a symlink).
pub(crate) fn outside_workspace(cwd: &Path, user_path: &str) -> bool {
    !crate::agent::inside(cwd, user_path)
}

/// A `path` argument that points outside the workspace needs approval.
pub(crate) fn outside_path_arg(cwd: &Path, args: &Value) -> bool {
    let path = args.get("path").and_then(Value::as_str).unwrap_or(".");
    outside_workspace(cwd, path)
}

/// A path as shown to the model and user: always with `/` separators.
pub(crate) fn slash_path(p: &std::path::Path) -> String {
    let s = p.display().to_string();
    if cfg!(windows) { s.replace('\\', "/") } else { s }
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
    /// The session's `--yolo` flag, inherited by subagents.
    pub yolo: bool,
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
        Self { content: content.into(), ..Self::default() }
    }
    pub fn err(content: impl Into<String>) -> Self {
        Self { content: content.into(), is_error: true, ..Self::default() }
    }
}

#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
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
    /// When true, [`crate::agent::Agent::call_tool`] waits for `run` after
    /// cancellation so the tool can kill children and notify peers. Tools
    /// that only need the request dropped leave this false.
    fn handles_cancel(&self) -> bool {
        false
    }
}

#[derive(Default)]
pub struct Registry {
    tools: Vec<Box<dyn Tool>>,
}

impl Registry {
    /// The built-in workspace tools: read_file, write_file, edit_file, bash,
    /// glob, grep, list_dir, view_image, web_fetch, web_search, subagent.
    pub fn builtin() -> Self {
        let mut r = Self::default();
        fs::register(&mut r);
        search::register(&mut r);
        bash::register(&mut r);
        image::register(&mut r);
        web::register(&mut r);
        subagent::register(&mut r);
        r
    }

    pub fn add(&mut self, tool: Box<dyn Tool>) {
        self.try_insert(tool).expect("duplicate tool name");
    }

    /// Insert `tool`, refusing to replace an existing name.
    pub fn try_insert(&mut self, tool: Box<dyn Tool>) -> Result<(), String> {
        let name = tool.spec().name.clone();
        if self.tools.iter().any(|t| t.spec().name == name) {
            return Err(format!("tool `{name}` collides with an existing tool"));
        }
        self.tools.push(tool);
        Ok(())
    }

    pub fn remove_names(&mut self, names: &[String]) {
        if names.is_empty() {
            return;
        }
        self.tools.retain(|t| !names.iter().any(|n| n == &t.spec().name));
    }

    pub fn remove_prefix(&mut self, prefix: &str) {
        self.tools.retain(|t| !t.spec().name.starts_with(prefix));
    }

    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools.iter().find(|t| t.spec().name == name).map(|t| t.as_ref())
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools.iter().map(|t| t.spec()).collect()
    }
}

#[cfg(test)]
impl SecretRoots {
    fn from_paths(paths: impl IntoIterator<Item = PathBuf>) -> Self {
        let mut roots = Vec::new();
        for path in paths {
            add_secret(&mut roots, path);
        }
        Self { roots }
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
    fn custom_auth_file_does_not_block_its_parent() {
        let _lock = crate::provider::sse::tests::ENV.lock().unwrap();
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
        assert!(!roots.blocks_path(&project), "a custom auth parent must stay readable");
        assert!(!roots.blocks_path(dir.path()));
        match old {
            Some(v) => std::env::set_var("NYQ_CODEX_AUTH", v),
            None => std::env::remove_var("NYQ_CODEX_AUTH"),
        }
    }
}
