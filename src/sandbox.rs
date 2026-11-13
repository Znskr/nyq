//! Sandboxing for model-run shell commands.
//!
//! Linux re-executes nyq as `nyq __sandbox …`, which restricts its own
//! process and runs the shell as a child, so the whole command tree
//! inherits the policy and the helper can re-check `.git` links when the
//! child exits. The helper layers three mechanisms:
//! - a user+mount namespace that re-binds `.git/hooks`, `.git/config` and
//!   `.git/info` (for `attributes`) read-only over each writable root's `.git`,
//!   so commits and branches keep working while planted hooks and config
//!   redirects cannot run unsandboxed later. Self-bind mounts pin `.git`,
//!   the directories holding it, discovered worktree/submodule gitdirs and
//!   `core.hooksPath` targets, so `mv .git aside` fails with EBUSY instead
//!   of carrying the read-only mounts along. `.git` paths that are
//!   *symlinks* can never be mount points, so their identity (link text and
//!   resolved dev/ino) is recorded before the command and re-checked after
//!   it — also when it was killed: a swapped or deleted link is restored
//!   and the replacement quarantined for inspection;
//! - Landlock: reads everywhere, writes only to allowed roots, and TCP
//!   bind/connect denied on ABI v4+ when the network policy is off;
//! - a seccomp filter, also only when the network policy is off, that fails
//!   `socket()` with EACCES for AF_INET/AF_INET6 of any type (covering UDP)
//!   and for AF_UNIX — `NYQ_SANDBOX_UNIX=1` re-allows AF_UNIX — and fails
//!   `io_uring_setup`/`io_uring_enter`/`io_uring_register` with EPERM so
//!   IORING_OP_SOCKET cannot bypass the family check.
//!
//! macOS wraps the shell in `sandbox-exec` with an equivalent Seatbelt
//! profile, including deny rules for the same `.git` paths; the link-swap
//! check runs in the outer process after `sandbox-exec` exits. Other platforms
//! have no sandbox; `check` reports that. `NYQ_SANDBOX_GIT_WRITABLE=1` opts
//! out of git-metadata protection on both, `NYQ_SANDBOX_GIT_STRICT=1`
//! refuses to run when that protection cannot be applied (the default is a
//! warning and `partial` enforcement status).
#![cfg_attr(not(any(target_os = "linux", target_os = "macos")), allow(dead_code))]

use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum Mode {
    /// No sandbox: commands run with the user's full permissions.
    Off,
    /// Commands may read anything but write nothing (except temp dirs).
    ReadOnly,
    /// Commands may write only inside the workspace and temp dirs.
    #[default]
    WorkspaceWrite,
}

/// An environment flag counts as set when it is non-empty and not "0".
fn env_flag(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|v| !v.is_empty() && v.to_str() != Some("0"))
}

#[derive(Clone, Debug)]
pub struct Policy {
    pub mode: Mode,
    /// Allow network access from sandboxed commands.
    pub network: bool,
    /// Permit AF_UNIX sockets even when `network` is denied
    /// (`NYQ_SANDBOX_UNIX=1` sets the default). Only read by the Linux
    /// seccomp filter.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub unix_sockets: bool,
    /// Leave `.git/hooks`, `.git/config` and `.git/info` writable inside the
    /// sandbox, opting out of git-metadata protection
    /// (`NYQ_SANDBOX_GIT_WRITABLE=1` sets the default).
    pub git_writable: bool,
    /// Refuse to run the command when `.git` metadata protection cannot be
    /// applied, instead of warning and continuing
    /// (`NYQ_SANDBOX_GIT_STRICT=1` sets the default). Only read by the
    /// Linux helper.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub git_strict: bool,
    /// Extra writable roots besides the workspace (WorkspaceWrite only).
    pub writable: Vec<PathBuf>,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            mode: Mode::default(),
            network: false,
            unix_sockets: env_flag("NYQ_SANDBOX_UNIX"),
            git_writable: env_flag("NYQ_SANDBOX_GIT_WRITABLE"),
            git_strict: env_flag("NYQ_SANDBOX_GIT_STRICT"),
            writable: Vec::new(),
        }
    }
}

/// First argument of the hidden re-exec helper: `nyq __sandbox ...`.
pub const HELPER_ARG: &str = "__sandbox";

fn temp_roots() -> Vec<PathBuf> {
    let mut v = vec![PathBuf::from("/tmp"), PathBuf::from("/var/tmp")];
    if cfg!(target_os = "macos") {
        v.extend(["/private/tmp", "/private/var/folders"].map(PathBuf::from));
    }
    v
}

/// Canonical `path` if it is a real directory under a system temp root and is
/// not `/` or an ancestor of `home`. Otherwise None.
fn accepted_tmpdir(path: &Path, home: Option<&Path>) -> Option<PathBuf> {
    let canon = std::fs::canonicalize(path).ok()?;
    if !canon.is_dir() || canon == Path::new("/") {
        return None;
    }
    if let Some(home) = home {
        let home = std::fs::canonicalize(home).unwrap_or_else(|_| home.to_path_buf());
        if home.starts_with(&canon) {
            return None;
        }
    }
    let under = temp_roots().into_iter().any(|root| {
        let root = std::fs::canonicalize(&root).unwrap_or(root);
        canon.starts_with(&root)
    });
    under.then_some(canon)
}

fn warn_tmpdir(path: &Path) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static ONCE: AtomicBool = AtomicBool::new(false);
    if ONCE.swap(true, Ordering::Relaxed) {
        return;
    }
    crate::ui::error(&format!(
        "ignoring TMPDIR ({}); it is not under a system temporary directory",
        path.display()
    ));
}

/// Paths every sandboxed command may write: temp dirs and harmless devices.
fn scratch_paths() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = [
        "/tmp",
        "/var/tmp",
        "/dev/null",
        "/dev/zero",
        "/dev/full",
        "/dev/tty",
        "/dev/pts",
        "/dev/shm",
    ]
    .iter()
    .map(PathBuf::from)
    .collect();
    if let Some(t) = std::env::var_os("TMPDIR").filter(|t| !t.is_empty()) {
        let path = PathBuf::from(&t);
        match accepted_tmpdir(&path, dirs::home_dir().as_deref()) {
            Some(canon) => v.push(canon),
            None => warn_tmpdir(&path),
        }
    }
    if cfg!(target_os = "macos") {
        v.extend(["/private/tmp", "/private/var/folders"].map(PathBuf::from));
    }
    v
}

/// Writable roots for a command run in `cwd`.
fn writable_roots(policy: &Policy, cwd: &Path) -> Vec<PathBuf> {
    let mut v = scratch_paths();
    if policy.mode == Mode::WorkspaceWrite {
        v.push(cwd.to_path_buf());
        v.extend(policy.writable.iter().cloned());
    }
    v.retain(|p| p.exists());
    v
}

/// The real git directory behind a `.git` file (worktrees and submodules use
/// `gitdir: <path>`), resolved against the file's location.
fn resolve_gitfile(file: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(file).ok()?;
    let rel = text.trim().strip_prefix("gitdir:")?.trim();
    std::fs::canonicalize(file.parent()?.join(rel)).ok()
}

/// The directory shared by a repo's worktrees (holds `hooks` and `config`):
/// `gitdir/commondir` when the file exists, else `gitdir` itself.
fn common_gitdir(gitdir: &Path) -> PathBuf {
    std::fs::read_to_string(gitdir.join("commondir"))
        .ok()
        .and_then(|s| std::fs::canonicalize(gitdir.join(s.trim())).ok())
        .unwrap_or_else(|| gitdir.to_path_buf())
}

/// What to create when a required `.git` path is missing, so it can be
/// bound read-only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ensure {
    /// A single directory (e.g. `.git/hooks`).
    Dir,
    /// A directory chain (e.g. a `core.hooksPath` target).
    DirAll,
    /// An empty regular file (e.g. `.git/config`).
    File,
}

/// `.git` metadata a sandboxed command must not write or replace. Read-only
/// binds cover the contents; read-write self-binds pin the directories so
/// `rename`/`rmdir` fails with EBUSY — without them `mv .git .git-old`
/// carried the read-only mounts along and a fresh, writable `.git` took
/// their place. `cwd` and `writable` are the same roots the command may
/// write; paths are returned whether or not they exist.
#[derive(Clone, Debug, Default)]
pub struct GitTargets {
    /// Directories to self-bind over themselves (still writable): `.git`,
    /// the directories containing it, discovered gitdirs and their common
    /// dirs, `modules`/`worktrees` holders, `core.hooksPath` targets and
    /// every ancestor down to and including a writable root.
    pub pin: Vec<PathBuf>,
    /// Paths to create when missing, then bind read-only.
    pub required: Vec<(PathBuf, Ensure)>,
    /// Paths to bind read-only when they exist.
    pub readonly: Vec<PathBuf>,
    /// `.git` paths that are symlinks. A symlink can never be a mount
    /// point, so the link itself stays replaceable inside the sandbox;
    /// [`verify_links`] re-checks them after the command and reverts a
    /// swap. Recorded with their link text and resolved dev/ino.
    pub links: Vec<WatchedLink>,
    /// Coverage notes worth telling the user about (e.g. `.git` is a
    /// symlink, which stays replaceable but is watched and reverted).
    pub notes: Vec<String>,
}

/// Identity of a `.git`-related symlink recorded before a sandboxed
/// command runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WatchedLink {
    /// The symlink's own path (e.g. `<repo>/.git`, `<gitdir>/hooks`).
    pub path: PathBuf,
    /// What `read_link` returned at snapshot time.
    pub target: PathBuf,
    /// dev+ino the link resolved to at snapshot time; `None` when it did
    /// not resolve (a dangling link is still re-checked).
    pub ident: Option<(u64, u64)>,
}

/// Record `path` as a protected symlink — its link text plus the dev/ino
/// it resolves to — or `None` when it is not a symlink.
#[cfg(unix)]
fn watch_link(path: &Path) -> Option<WatchedLink> {
    use std::os::unix::fs::MetadataExt;
    if !path.is_symlink() {
        return None;
    }
    let target = std::fs::read_link(path).ok()?;
    let ident = std::fs::metadata(path).ok().map(|m| (m.dev(), m.ino()));
    Some(WatchedLink {
        path: path.to_path_buf(),
        target,
        ident,
    })
}

/// Windows has no sandboxed commands; the link list stays empty.
#[cfg(not(unix))]
fn watch_link(_path: &Path) -> Option<WatchedLink> {
    None
}

/// Move `path` aside to a `<name>.nyq-quarantine-<ts>` sibling so a planted
/// replacement survives for inspection but cannot run.
#[cfg(unix)]
fn quarantine(path: &Path) -> Result<PathBuf, String> {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "root".into());
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    for i in 0..100u32 {
        let suffix = if i == 0 {
            format!("nyq-quarantine-{ts}")
        } else {
            format!("nyq-quarantine-{ts}-{i}")
        };
        let cand = parent.join(format!("{name}.{suffix}"));
        if std::fs::symlink_metadata(&cand).is_err() {
            return match std::fs::rename(path, &cand) {
                Ok(()) => Ok(cand),
                Err(e) => Err(format!("rename to {}: {e}", cand.display())),
            };
        }
    }
    Err(format!("no free quarantine name beside {}", path.display()))
}

/// Recreate a watched link with its recorded target.
#[cfg(unix)]
fn relink(w: &WatchedLink) -> Result<(), String> {
    std::os::unix::fs::symlink(&w.target, &w.path).map_err(|e| e.to_string())
}

/// Re-check one watched `.git` link after a sandboxed command. `None` when
/// it is untouched; otherwise the swap is reverted — the impostor moved to
/// a quarantine name and the original link restored — and a loud warning
/// is returned for the tool output.
#[cfg(unix)]
fn verify_link(w: &WatchedLink) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    let path = w.path.display();
    let md = match std::fs::symlink_metadata(&w.path) {
        Ok(md) => md,
        Err(_) => {
            return Some(match relink(w) {
                Ok(()) => format!(
                    "SECURITY: {path} is a protected .git symlink that was deleted inside the \
                     sandbox; the original link was restored"
                ),
                Err(e) => format!(
                    "SECURITY: {path} is a protected .git symlink that was deleted inside the \
                     sandbox; restoring it failed: {e}"
                ),
            });
        }
    };
    if !md.file_type().is_symlink() {
        // A real file/dir sits where the link was — the classic swap.
        let kind = if md.is_dir() { "directory" } else { "file" };
        return Some(
            match quarantine(&w.path).and_then(|q| relink(w).map(|_| q)) {
                Ok(q) => format!(
                    "SECURITY: {path} is a protected .git symlink that was replaced by a real \
                 {kind} inside the sandbox; it was quarantined to {} and the original link \
                 restored — inspect it before deleting",
                    q.display()
                ),
                Err(e) => format!(
                    "SECURITY: {path} is a protected .git symlink that was replaced by a real \
                 {kind} inside the sandbox; quarantining it and restoring the link failed: {e}"
                ),
            },
        );
    }
    if std::fs::read_link(&w.path).ok().as_deref() != Some(w.target.as_path()) {
        // The link itself was retargeted: quarantine the impostor link.
        let new = std::fs::read_link(&w.path)
            .map(|t| t.display().to_string())
            .unwrap_or_default();
        return Some(
            match quarantine(&w.path).and_then(|q| relink(w).map(|_| q)) {
                Ok(q) => format!(
                    "SECURITY: {path} is a protected .git symlink that was retargeted to {new} \
                 inside the sandbox; the new link was quarantined to {} and the original \
                 restored",
                    q.display()
                ),
                Err(e) => format!(
                    "SECURITY: {path} is a protected .git symlink that was retargeted to {new} \
                 inside the sandbox; quarantining it and restoring the link failed: {e}"
                ),
            },
        );
    }
    // Same link text; make sure it still resolves to the same object —
    // `mv realgit aside; mkdir realgit` keeps the link but plants a new
    // gitdir behind it. Quarantine the impostor at the resolved path; the
    // original inode was moved aside by the command and cannot be found.
    let ident = std::fs::metadata(&w.path).ok().map(|m| (m.dev(), m.ino()));
    if ident != w.ident {
        let moved = std::fs::canonicalize(&w.path)
            .ok()
            .map(|real| match quarantine(&real) {
                Ok(q) => format!(
                    "; the impostor at {} was quarantined to {}",
                    real.display(),
                    q.display()
                ),
                Err(e) => format!(
                    "; quarantining the impostor at {} failed: {e}",
                    real.display()
                ),
            })
            .unwrap_or_else(|| "; the impostor could not be located".into());
        return Some(format!(
            "SECURITY: the target of {path} (a protected .git symlink) was replaced inside the \
             sandbox{moved}. The original {} was moved aside by the command; look for a renamed \
             copy and check the repository before trusting it",
            w.target.display()
        ));
    }
    None
}

/// Re-check every watched `.git` link after a sandboxed command and return
/// one warning per tampered link (empty when nothing was swapped).
#[cfg(unix)]
fn verify_links(watched: &[WatchedLink]) -> Vec<String> {
    watched.iter().filter_map(verify_link).collect()
}

/// Windows has no sandboxed commands; nothing to re-check.
#[cfg(not(unix))]
fn verify_links(_watched: &[WatchedLink]) -> Vec<String> {
    Vec::new()
}

/// Watches `.git` symlinks across a sandboxed command: `watch` records
/// them before spawn and `finish` re-checks them afterwards — a swapped or
/// deleted link is restored and the replacement quarantined for
/// inspection. `Drop` runs the check too, so timeout, kill and
/// early-return paths are covered as well.
#[derive(Debug)]
pub struct GitLinkGuard {
    watched: Vec<WatchedLink>,
    done: bool,
}

impl GitLinkGuard {
    /// Snapshot the `.git` links a command in `cwd` could swap. Empty when
    /// commands run unsandboxed or `.git` stays writable by policy.
    pub fn watch(policy: &Policy, cwd: &Path) -> Self {
        let watched = if policy.mode == Mode::Off || policy.git_writable {
            Vec::new()
        } else {
            git_targets(cwd, &writable_roots(policy, cwd)).links
        };
        Self {
            watched,
            done: false,
        }
    }

    /// Verify every watched link, quarantining replacements and restoring
    /// originals; returns one warning per tampered link. Idempotent —
    /// `Drop` runs whatever is left.
    pub fn finish(&mut self) -> Vec<String> {
        if self.done {
            return Vec::new();
        }
        self.done = true;
        verify_links(&self.watched)
    }
}

impl Drop for GitLinkGuard {
    fn drop(&mut self) {
        for warning in self.finish() {
            crate::ui::error(&warning);
        }
    }
}

/// Collapse `.` and `..` lexically without touching the filesystem.
fn normalize_path(p: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// Unescape and un-comment a git config value.
fn unquote_git(v: &str) -> String {
    let mut s = String::new();
    let mut quoted = false;
    let mut chars = v.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => quoted = !quoted,
            '\\' => match chars.next() {
                Some('n') => s.push('\n'),
                Some('t') => s.push('\t'),
                Some(other) => s.push(other),
                None => {}
            },
            '#' | ';' if !quoted => break,
            c => s.push(c),
        }
    }
    s.trim_end().to_string()
}

/// Resolve an `[include] path` value: `~/` to home, relative to the
/// directory of the config file that contains it.
fn expand_config_path(v: &str, dir: Option<&Path>) -> PathBuf {
    if let Some(rest) = v.strip_prefix("~/") {
        if let Some(h) = dirs::home_dir() {
            return h.join(rest);
        }
    }
    let p = PathBuf::from(v);
    if p.is_absolute() {
        p
    } else {
        dir.map(|d| d.join(&p)).unwrap_or(p)
    }
}

/// All values for `key` in `section` of a git config file, following
/// unconditional `[include] path=` chains up to four levels deep (parsing
/// the file directly, so no git command or hook runs). Section matches
/// `name` or `name.sub`, keys and names case-insensitively.
fn git_config_values(path: &Path, section: &str, key: &str, depth: u8) -> Vec<String> {
    if depth > 4 {
        return Vec::new();
    }
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut current = String::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(rest) = line.strip_prefix('[') {
            current = match rest.split_once('"') {
                // [name "sub"] → name.sub
                Some((name, sub)) => format!(
                    "{}.{}",
                    name.trim().to_lowercase(),
                    sub.split('"').next().unwrap_or(sub).trim().to_lowercase()
                ),
                None => rest.split(']').next().unwrap_or("").trim().to_lowercase(),
            };
            continue;
        }
        let (k, v) = match line.split_once('=') {
            Some((k, v)) => (k.trim(), unquote_git(v.trim())),
            None => (line.trim(), "true".to_string()), // a bare key means true
        };
        if current == section && k.eq_ignore_ascii_case(key) {
            out.push(v.clone());
        }
        if current == "include" && k.eq_ignore_ascii_case("path") && !v.is_empty() {
            let inc = expand_config_path(&v, path.parent());
            out.extend(git_config_values(&inc, section, key, depth + 1));
        }
    }
    out
}

/// Last value wins across `files` (worktree config outranks common config).
fn git_config_last(files: &[PathBuf], section: &str, key: &str) -> Option<String> {
    files
        .iter()
        .flat_map(|f| git_config_values(f, section, key, 0))
        .last()
}

/// Whether a boolean config key (e.g. `extensions.worktreeConfig`) is set.
fn git_config_truthy(path: &Path, section: &str, key: &str) -> bool {
    git_config_values(path, section, key, 0)
        .last()
        .is_some_and(|v| matches!(v.to_lowercase().as_str(), "true" | "yes" | "on" | "1"))
}

/// The worktree top-level behind a linked-worktree gitdir: the `<gitdir>/gitdir`
/// back-pointer holds the worktree's `.git` file path.
fn worktree_root(gitdir: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(gitdir.join("gitdir")).ok()?;
    let n = normalize_path(&PathBuf::from(text.trim()));
    n.parent().map(Path::to_path_buf)
}

/// A submodule's worktree directory from `core.worktree` in its config,
/// resolved relative to the gitdir.
fn module_root(gitdir: &Path) -> Option<PathBuf> {
    let w = git_config_last(&[gitdir.join("config")], "core", "worktree")?;
    let p = PathBuf::from(w);
    Some(normalize_path(&if p.is_absolute() {
        p
    } else {
        gitdir.join(p)
    }))
}

/// The `core.hooksPath` directory for `gitdir`, if configured. Relative
/// paths resolve against the worktree top-level (where hooks run).
fn hooks_path(gitdir: &Path, common: &Path, root: Option<&Path>) -> Option<PathBuf> {
    let files = vec![
        common.join("config"),
        gitdir.join("config"),
        gitdir.join("config.worktree"),
    ];
    let v = git_config_last(&files, "core", "hookspath")?;
    if v.trim().is_empty() {
        return None;
    }
    let p = PathBuf::from(v);
    let resolved = if p.is_absolute() {
        normalize_path(&p)
    } else {
        let root = root
            .map(Path::to_path_buf)
            .or_else(|| worktree_root(gitdir))
            .or_else(|| module_root(gitdir))?;
        normalize_path(&root.join(p))
    };
    // A hooks path that is the whole worktree cannot be carved out without
    // making the workspace read-only; that is not a real configuration.
    if root.is_some_and(|r| resolved == normalize_path(r)) {
        return None;
    }
    Some(resolved)
}

/// `dir` plus every ancestor that still sits under a writable root becomes
/// a mount point, so none of them can be moved aside. `dir` itself is
/// pinned even outside the roots (a gitdir need not be writable).
fn push_pin(
    out: &mut GitTargets,
    seen: &mut std::collections::HashSet<PathBuf>,
    dir: &Path,
    under: &dyn Fn(&Path) -> bool,
) {
    let mut d = Some(dir.to_path_buf());
    while let Some(p) = d {
        let inside = under(&p);
        if inside || p == *dir {
            if let Ok(c) = std::fs::canonicalize(&p) {
                if seen.insert(c.clone()) {
                    out.pin.push(c);
                }
            }
        }
        if !inside {
            break;
        }
        d = p.parent().map(Path::to_path_buf);
    }
}

/// Every `.git` path the sandbox pins or makes read-only: hooks, config and
/// metadata under every reachable git dir, so a planted hook or
/// `core.hooksPath` redirect cannot run later outside the sandbox — and
/// directories that must not be moved aside to hide the protection.
/// Submodule (`modules/*`) and linked-worktree (`worktrees/*`) gitdirs are
/// covered wherever they sit.
fn git_targets(cwd: &Path, writable: &[PathBuf]) -> GitTargets {
    let under = |p: &Path| writable.iter().any(|root| p.starts_with(root));
    let mut out = GitTargets::default();
    let mut pin_seen = std::collections::HashSet::new();
    // Queue of (gitdir, worktree top-level) pairs to expand.
    let mut work: Vec<(PathBuf, Option<PathBuf>)> = Vec::new();

    let consider = |dir: &Path,
                    out: &mut GitTargets,
                    pin_seen: &mut std::collections::HashSet<PathBuf>,
                    work: &mut Vec<(PathBuf, Option<PathBuf>)>| {
        if !under(dir) {
            return;
        }
        let git = dir.join(".git");
        match std::fs::metadata(&git) {
            Ok(m) if m.is_dir() => {
                // `.git` a directory — or a symlink to one. A symlink cannot
                // be a mount point, so the link itself stays replaceable
                // (`mv .git aside` works); it is recorded for the
                // post-command check and the resolved gitdir still gets
                // pinned and read-only below.
                push_pin(out, pin_seen, dir, &under);
                if let Ok(gd) = std::fs::canonicalize(&git) {
                    if let Some(l) = watch_link(&git) {
                        out.notes.push(format!(
                            "{} is a symlink; it stays replaceable inside the sandbox, but a \
                             swap is detected and reverted after the command",
                            git.display()
                        ));
                        out.links.push(l);
                    }
                    push_pin(out, pin_seen, &gd, &under);
                    work.push((gd, Some(dir.to_path_buf())));
                }
            }
            Ok(_) => {
                // A `gitdir:` pointer file (worktree, submodule): the file
                // gets a read-only bind, which also pins it against rename.
                // A *symlinked* pointer file is still replaceable, so it is
                // recorded for the post-command check.
                if let Some(gd) = resolve_gitfile(&git) {
                    out.links.extend(watch_link(&git));
                    out.readonly.push(git);
                    push_pin(out, pin_seen, dir, &under);
                    work.push((gd, Some(dir.to_path_buf())));
                }
            }
            Err(_) => {
                // A dangling `.git` symlink resolves to nothing yet, but the
                // link can still be swapped aside and replaced — record it.
                if let Some(l) = watch_link(&git) {
                    out.notes.push(format!(
                        "{} is a symlink; it stays replaceable inside the sandbox, but a \
                         swap is detected and reverted after the command",
                        git.display()
                    ));
                    out.links.push(l);
                }
            }
        }
    };
    for root in writable {
        consider(root, &mut out, &mut pin_seen, &mut work);
    }
    let mut dir = Some(cwd);
    while let Some(d) = dir {
        consider(d, &mut out, &mut pin_seen, &mut work);
        dir = d.parent();
    }

    let mut seen = std::collections::HashSet::new();
    let mut common_seen = std::collections::HashSet::new();
    while let Some((gitdir, root)) = work.pop() {
        if !seen.insert(gitdir.clone()) {
            continue;
        }
        let common = common_gitdir(&gitdir);
        push_pin(&mut out, &mut pin_seen, &gitdir, &under);
        push_pin(&mut out, &mut pin_seen, &common, &under);
        if common_seen.insert(common.clone()) {
            out.required.extend([
                (common.join("config"), Ensure::File),
                (common.join("hooks"), Ensure::Dir),
                // `info` holds `attributes` and `exclude`, which steer later
                // git runs the same way hooks do. Git creates it lazily, so
                // it may be missing — require it so the whole directory is
                // read-only even when it does not exist yet.
                (common.join("info"), Ensure::Dir),
            ]);
            // `hooks` may be a symlink whose target lives in a writable
            // root; the bind lands on the target through the link, but list
            // the real path too (it is what seatbelt matches). The link
            // itself cannot be pinned — a symlink is never a mount point —
            // so it is recorded for the post-command check, as are the
            // config/info links.
            let hooks = common.join("hooks");
            if let Some(l) = watch_link(&hooks) {
                out.notes.push(format!(
                    "{} is a symlink; it stays replaceable inside the sandbox, but a \
                     swap is detected and reverted after the command",
                    hooks.display()
                ));
                out.links.push(l);
            }
            for p in [common.join("config"), common.join("info")] {
                out.links.extend(watch_link(&p));
            }
            if let Ok(real) = std::fs::canonicalize(&hooks) {
                if real != hooks && under(&real) {
                    out.readonly.push(real.clone());
                    push_pin(&mut out, &mut pin_seen, &real, &under);
                }
            }
        }
        // Per-gitdir files that steer where shared metadata lives.
        for f in ["commondir", "gitdir", "config.worktree"] {
            let p = gitdir.join(f);
            if p.exists() {
                out.readonly.push(p.clone());
            }
            out.links.extend(watch_link(&p));
        }
        if git_config_truthy(&common.join("config"), "extensions", "worktreeconfig") {
            out.required
                .push((gitdir.join("config.worktree"), Ensure::File));
        }
        // Where hooks actually run when core.hooksPath is set.
        if let Some(hp) = hooks_path(&gitdir, &common, root.as_deref()) {
            if under(&hp) {
                out.links.extend(watch_link(&hp));
                out.required.push((hp.clone(), Ensure::DirAll));
                push_pin(&mut out, &mut pin_seen, &hp, &under);
            }
        }
        // Submodule and linked-worktree gitdirs under the common dir; the
        // holders get pinned so `mv .git/modules aside` fails too.
        for nest in ["modules", "worktrees"] {
            let holder = common.join(nest);
            if holder.is_dir() {
                push_pin(&mut out, &mut pin_seen, &holder, &under);
                if let Ok(entries) = std::fs::read_dir(&holder) {
                    for e in entries.flatten() {
                        if e.file_type().is_ok_and(|t| t.is_dir()) {
                            if let Ok(sub) = std::fs::canonicalize(e.path()) {
                                work.push((sub, None));
                            }
                        }
                    }
                }
            }
        }
    }
    // The same path can arrive from several discoveries.
    let mut ro_seen = std::collections::HashSet::new();
    out.readonly.retain(|p| ro_seen.insert(p.clone()));
    let mut req_seen = std::collections::HashSet::new();
    out.required.retain(|(p, _)| req_seen.insert(p.clone()));
    let mut link_seen = std::collections::HashSet::new();
    out.links.retain(|l| link_seen.insert(l.path.clone()));
    out.notes.dedup();
    out
}

#[cfg(not(windows))]
fn find_in_path(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

/// Resolve Git Bash without selecting the WSL launcher under Windows.
#[cfg(any(windows, test))]
fn windows_bash(
    paths: impl Iterator<Item = PathBuf>,
    system_root: Option<&Path>,
) -> Option<PathBuf> {
    // Empty or relative PATH entries resolve against the workspace; never run a planted bash.exe.
    paths
        .filter(|dir| dir.is_absolute())
        .filter_map(|dir| std::fs::canonicalize(dir.join("bash.exe")).ok())
        .find(|p| {
            let under_system = system_root.is_some_and(|root| {
                let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
                p.to_string_lossy().to_lowercase().starts_with(&format!(
                    "{}{}",
                    root.to_string_lossy().to_lowercase(),
                    std::path::MAIN_SEPARATOR
                ))
            });
            p.is_file() && !under_system
        })
}

fn shell_argv(script: &str, windows: bool, bash_exe: Option<&Path>, bash: bool) -> Vec<String> {
    if windows {
        return match bash_exe {
            Some(path) => vec![
                path.to_string_lossy().into_owned(),
                "-c".into(),
                script.into(),
            ],
            None => vec!["cmd".into(), "/S".into(), "/C".into(), script.into()],
        };
    }
    vec![
        if bash { "bash" } else { "sh" }.into(),
        "-c".into(),
        script.into(),
    ]
}

fn shell(script: &str) -> Vec<String> {
    #[cfg(windows)]
    {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let root = std::env::var_os("SystemRoot")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
        let bash = windows_bash(std::env::split_paths(&path), Some(&root));
        shell_argv(script, true, bash.as_deref(), false)
    }
    #[cfg(not(windows))]
    shell_argv(script, false, None, find_in_path("bash").is_some())
}

/// A shell command running `script` in `cwd`, confined according to the policy.
pub fn shell_command(
    policy: &Policy,
    cwd: &Path,
    script: &str,
) -> anyhow::Result<tokio::process::Command> {
    let argv = shell(script);
    let argv = match policy.mode {
        Mode::Off => argv,
        _ => wrap(policy, cwd, argv)?,
    };
    let mut c = shell_process(&argv);
    c.current_dir(cwd);
    Ok(c)
}

fn shell_process(argv: &[String]) -> tokio::process::Command {
    let mut c = tokio::process::Command::new(&argv[0]);
    #[cfg(windows)]
    if argv[0] == "cmd" {
        c.raw_arg(format!("/S /C \"{}\"", argv.last().unwrap()));
    } else {
        c.args(&argv[1..]);
    }
    #[cfg(not(windows))]
    c.args(&argv[1..]);
    c
}

/// `NYQ_SANDBOX_HELPER` may name this same binary (a path alias). Any other
/// path is refused so a helper cannot skip Landlock and run the command raw.
#[cfg(any(target_os = "linux", test))]
fn trusted_helper(current: &Path, override_path: Option<&Path>) -> anyhow::Result<PathBuf> {
    if let Some(over) = override_path {
        let current_real = std::fs::canonicalize(current).unwrap_or_else(|_| current.to_path_buf());
        let over_real = std::fs::canonicalize(over).unwrap_or_else(|_| over.to_path_buf());
        if current_real != over_real {
            anyhow::bail!(
                "NYQ_SANDBOX_HELPER ({}) is not this nyq binary",
                over.display()
            );
        }
    }
    Ok(current.to_path_buf())
}

#[cfg(target_os = "linux")]
fn wrap(policy: &Policy, cwd: &Path, argv: Vec<String>) -> anyhow::Result<Vec<String>> {
    let current = std::env::current_exe()?;
    let over = std::env::var_os("NYQ_SANDBOX_HELPER").map(PathBuf::from);
    let exe = trusted_helper(&current, over.as_deref())?;
    Ok(linux::helper_args(
        &exe,
        &writable_roots(policy, cwd),
        policy,
        argv,
    ))
}

#[cfg(target_os = "macos")]
fn wrap(policy: &Policy, cwd: &Path, argv: Vec<String>) -> anyhow::Result<Vec<String>> {
    let writable = writable_roots(policy, cwd);
    let git = if policy.git_writable {
        GitTargets::default()
    } else {
        git_targets(cwd, &writable)
    };
    let mut v = vec![
        "/usr/bin/sandbox-exec".to_string(),
        "-p".into(),
        seatbelt_profile(&writable, policy.network, &git),
    ];
    v.extend(argv);
    Ok(v)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn wrap(_policy: &Policy, _cwd: &Path, _argv: Vec<String>) -> anyhow::Result<Vec<String>> {
    anyhow::bail!("sandboxing is not supported on this platform")
}

/// A macOS Seatbelt (SBPL) profile allowing writes only under `roots`.
/// `git.required`/`git.readonly` list the `.git` paths that stay read-only
/// and `git.pin` the directories that must not be moved aside; deny rules
/// come last because Seatbelt lets later rules win.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn seatbelt_profile(roots: &[PathBuf], network: bool, git: &GitTargets) -> String {
    let quote = |p: &Path| {
        format!(
            "\"{}\"",
            p.display()
                .to_string()
                .replace('\\', "\\\\")
                .replace('"', "\\\"")
        )
    };
    let mut s =
        String::from("(version 1)\n(allow default)\n(deny file-write*)\n(allow file-write*");
    for r in roots {
        let kind = if r.is_dir() { "subpath" } else { "literal" };
        s.push_str(&format!(" ({kind} {})", quote(r)));
    }
    s.push_str(")\n");
    if !network {
        // `(deny network*)` includes unix sockets. Re-allowing them was an
        // escape through host daemons such as the Docker socket. The
        // `unix_sockets` escape hatch is Linux-only (seccomp); a static
        // profile cannot carve AF_UNIX back out safely.
        s.push_str("(deny network*)\n");
    }
    let readonly = git
        .required
        .iter()
        .map(|(p, _)| p)
        .chain(git.readonly.iter());
    let mut has_ro = false;
    for p in readonly {
        if !has_ro {
            s.push_str("(deny file-write*");
            has_ro = true;
        }
        s.push_str(&format!(" (literal {}) (subpath {})", quote(p), quote(p)));
    }
    if has_ro {
        s.push_str(")\n");
    }
    if !git.pin.is_empty() || !git.links.is_empty() {
        // Seatbelt cannot self-bind; denying file-write-unlink on the
        // directory node stops `mv .git aside` and `rmdir` the same way the
        // mount-point EBUSY does on Linux. Watched links are listed too:
        // whether the literal matches the link or its target, the rename
        // is refused — and the post-command link check reverts a swap that
        // still gets through.
        s.push_str("(deny file-write-unlink");
        for p in &git.pin {
            s.push_str(&format!(" (literal {})", quote(p)));
        }
        for l in &git.links {
            s.push_str(&format!(" (literal {})", quote(&l.path)));
        }
        s.push_str(")\n");
    }
    s
}

/// Error if the policy can't be enforced on this system.
pub fn check(policy: &Policy) -> Result<(), String> {
    if policy.mode == Mode::Off {
        return Ok(());
    }
    #[cfg(target_os = "linux")]
    return linux::abi()
        .map(|_| ())
        .ok_or_else(|| "Landlock is not supported or not enabled in this kernel".into());
    #[cfg(target_os = "macos")]
    return if Path::new("/usr/bin/sandbox-exec").exists() {
        Ok(())
    } else {
        Err("/usr/bin/sandbox-exec not found".into())
    };
    #[allow(unreachable_code)]
    Err("sandboxing is not supported on this platform".into())
}

/// Which OS facility enforces the sandbox.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    Landlock,
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    Seatbelt,
    None,
}

/// How much of the requested policy the backend can actually enforce.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase", tag = "level", content = "reasons")]
pub enum Enforcement {
    /// Every part of the policy is kernel-enforced.
    Full,
    /// The sandbox runs but some aspects are open, with the reasons.
    /// Only the Linux status check produces it: Seatbelt is all-or-nothing.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    Partial(Vec<String>),
    /// Nothing is enforced (`--sandbox off` or no usable backend).
    None,
}

/// Machine-readable enforcement report for the banner, doctor and status.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct SandboxStatus {
    pub backend: Backend,
    pub enforcement: Enforcement,
}

/// The enforcement level `policy` will get on this system, computed from the
/// Landlock ABI, seccomp and user-namespace availability on Linux, the
/// `sandbox-exec` binary on macOS, and nothing elsewhere.
pub fn status(policy: &Policy) -> SandboxStatus {
    if policy.mode == Mode::Off {
        return SandboxStatus {
            backend: Backend::None,
            enforcement: Enforcement::None,
        };
    }
    #[cfg(target_os = "linux")]
    {
        let Some(abi) = linux::abi() else {
            return SandboxStatus {
                backend: Backend::Landlock,
                enforcement: Enforcement::None,
            };
        };
        let mut reasons = Vec::new();
        if !policy.network {
            if abi < 4 {
                reasons.push(format!("Landlock ABI v{abi} cannot deny TCP"));
            }
            if !(linux::seccomp::supported() && linux::seccomp::available()) {
                reasons.push("no seccomp socket filter: UDP and unix sockets stay open".into());
            }
        }
        if policy.mode == Mode::WorkspaceWrite && !policy.git_writable {
            match linux::git::probe() {
                // The runtime outcome decides: the helper reports whether
                // its mounts applied, so Docker and pre-5.12 kernels show
                // partial instead of silently claiming full.
                Some(Err(e)) => reasons.push(format!(".git protection open: {e}")),
                Some(Ok(())) => {}
                None => {
                    if !linux::git::userns_available() {
                        reasons.push(
                            ".git hooks/config stay writable (user namespaces unavailable)".into(),
                        );
                    }
                }
            }
        }
        return SandboxStatus {
            backend: Backend::Landlock,
            enforcement: if reasons.is_empty() {
                Enforcement::Full
            } else {
                Enforcement::Partial(reasons)
            },
        };
    }
    #[cfg(target_os = "macos")]
    return SandboxStatus {
        backend: Backend::Seatbelt,
        enforcement: if Path::new("/usr/bin/sandbox-exec").exists() {
            Enforcement::Full
        } else {
            Enforcement::None
        },
    };
    #[allow(unreachable_code)]
    SandboxStatus {
        backend: Backend::None,
        enforcement: Enforcement::None,
    }
}

/// One line for the banner.
pub fn describe(policy: &Policy) -> String {
    let mode = match policy.mode {
        Mode::Off => return "sandbox: off — commands run with your full permissions".into(),
        Mode::ReadOnly => "read-only",
        Mode::WorkspaceWrite => "workspace-write",
    };
    let net = if policy.network {
        "network on"
    } else {
        "network off"
    };
    let st = status(policy);
    let engine: String = match st.backend {
        #[cfg(target_os = "linux")]
        Backend::Landlock => linux::abi()
            .map(|abi| format!("landlock v{abi}"))
            .unwrap_or_else(|| "landlock unavailable".into()),
        // Only `status` produces Landlock, and only on Linux; the variant
        // must still be covered here.
        #[cfg(not(target_os = "linux"))]
        Backend::Landlock => "landlock".into(),
        Backend::Seatbelt => "seatbelt".into(),
        Backend::None => "unavailable".into(),
    };
    let level = match st.enforcement {
        Enforcement::Full => "enforcement: full".into(),
        Enforcement::Partial(reasons) => {
            format!("enforcement: partial ({})", reasons.join("; "))
        }
        Enforcement::None => "not enforced".into(),
    };
    format!("sandbox: {mode} · {net} · {level} ({engine})")
}

/// Apply the policy encoded in `args` to this process, then exec the command.
pub fn helper_main(args: Vec<String>) -> ! {
    #[cfg(target_os = "linux")]
    linux::helper_main(args);
    #[cfg(not(target_os = "linux"))]
    {
        let _ = args;
        eprintln!("nyq: sandbox helper is only used on Linux");
        std::process::exit(126)
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::path::{Path, PathBuf};

    use landlock::{
        path_beneath_rules, Access, AccessFs, AccessNet, CompatLevel, Compatible, Ruleset,
        RulesetAttr, RulesetCreatedAttr, RulesetStatus, ABI,
    };

    const ABIS: [ABI; 6] = [ABI::V6, ABI::V5, ABI::V4, ABI::V3, ABI::V2, ABI::V1];

    /// Highest Landlock ABI the kernel supports, if any.
    pub fn abi() -> Option<u8> {
        ABIS.iter().zip((1..=6).rev()).find_map(|(abi, n)| {
            Ruleset::default()
                .set_compatibility(CompatLevel::HardRequirement)
                .handle_access(AccessFs::from_all(*abi))
                .and_then(|r| r.create())
                .ok()
                .map(|_| n)
        })
    }

    /// `exe __sandbox [--net] [--unix] [--git-writable] [--git-strict] [--write PATH]... -- argv...`
    pub fn helper_args(
        exe: &Path,
        writable: &[PathBuf],
        policy: &super::Policy,
        argv: Vec<String>,
    ) -> Vec<String> {
        let mut v = vec![exe.display().to_string(), super::HELPER_ARG.into()];
        if policy.network {
            v.push("--net".into());
        }
        if policy.unix_sockets {
            v.push("--unix".into());
        }
        if policy.git_writable {
            v.push("--git-writable".into());
        }
        if policy.git_strict {
            v.push("--git-strict".into());
        }
        for w in writable {
            v.push("--write".into());
            v.push(w.display().to_string());
        }
        v.push("--".into());
        v.extend(argv);
        v
    }

    /// What `parse` decodes from the helper arguments.
    pub struct Spec {
        pub writable: Vec<PathBuf>,
        pub network: bool,
        /// Allow AF_UNIX sockets even with the network denied.
        pub unix_sockets: bool,
        /// Leave `.git` metadata (hooks/config) writable.
        pub git_writable: bool,
        /// Refuse to run when `.git` protection cannot be applied.
        pub git_strict: bool,
        /// Probe mode: apply `.git` protection, report, and exit.
        pub git_probe: bool,
        pub argv: Vec<String>,
    }

    /// Inverse of `helper_args` (without exe and HELPER_ARG). Also accepts
    /// the internal `--git-probe`, used by the status check.
    pub fn parse(args: &[String]) -> Result<Spec, String> {
        let mut spec = Spec {
            writable: Vec::new(),
            network: false,
            unix_sockets: false,
            git_writable: false,
            git_strict: false,
            git_probe: false,
            argv: Vec::new(),
        };
        let mut it = args.iter();
        while let Some(a) = it.next() {
            match a.as_str() {
                "--net" => spec.network = true,
                "--unix" => spec.unix_sockets = true,
                "--git-writable" => spec.git_writable = true,
                "--git-strict" => spec.git_strict = true,
                "--git-probe" => spec.git_probe = true,
                "--write" => spec
                    .writable
                    .push(PathBuf::from(it.next().ok_or("--write needs a path")?)),
                "--" => {
                    spec.argv = it.cloned().collect();
                    if spec.argv.is_empty() {
                        return Err("no command given".into());
                    }
                    return Ok(spec);
                }
                other => return Err(format!("unknown argument {other}")),
            }
        }
        Err("missing `--` before the command".into())
    }

    fn restrict(writable: &[PathBuf], network: bool) -> Result<RulesetStatus, String> {
        let abi = ABI::V6;
        let mut ruleset = Ruleset::default()
            .handle_access(AccessFs::from_all(abi))
            .map_err(|e| e.to_string())?;
        if !network {
            ruleset = ruleset
                .handle_access(AccessNet::BindTcp | AccessNet::ConnectTcp)
                .map_err(|e| e.to_string())?;
        }
        let status = ruleset
            .create()
            .and_then(|r| r.add_rules(path_beneath_rules(["/"], AccessFs::from_read(abi))))
            .and_then(|r| r.add_rules(path_beneath_rules(writable, AccessFs::from_all(abi))))
            .and_then(|r| r.restrict_self())
            .map_err(|e| e.to_string())?;
        Ok(status.ruleset)
    }

    pub fn helper_main(args: Vec<String>) -> ! {
        let fail = |msg: String| -> ! {
            eprintln!("nyq sandbox: {msg}");
            std::process::exit(126)
        };
        let spec = parse(&args).unwrap_or_else(|e| fail(e));
        if spec.git_probe {
            // Used by `status()` to learn whether the mount-based `.git`
            // protection really applies here: prints a marker and exits
            // before Landlock, so the answer is about the mounts alone.
            let t = super::git_targets(
                &std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
                &spec.writable,
            );
            match if spec.git_writable {
                Ok(())
            } else {
                git::protect(&t)
            } {
                Ok(()) => {
                    eprintln!("nyq sandbox: git protection available");
                    std::process::exit(0);
                }
                Err(e) => fail(format!(".git metadata protection failed: {e}")),
            }
        }
        // The `.git` links are recorded here (before the command) and
        // re-checked after it exits; the outer nyq process repeats the
        // check for the kill paths it sees.
        let links = if spec.git_writable {
            Vec::new()
        } else {
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
            let targets = super::git_targets(&cwd, &spec.writable);
            for note in &targets.notes {
                eprintln!("nyq sandbox: {note}");
            }
            if let Err(e) = git::protect(&targets) {
                if spec.git_strict {
                    fail(format!(".git metadata protection failed: {e}"));
                }
                eprintln!("nyq sandbox: .git metadata stays writable: {e}");
            }
            targets.links
        };
        match restrict(&spec.writable, spec.network) {
            Ok(RulesetStatus::NotEnforced) => {
                fail("Landlock is not enforced by this kernel; refusing to run unconfined".into())
            }
            Ok(_) => {}
            Err(e) => fail(e),
        }
        if !spec.network {
            match seccomp::deny_sockets(spec.unix_sockets) {
                // On an architecture the filter does not cover, run anyway
                // (Landlock still applies); the status report says partial.
                Err(e) if !seccomp::supported() => {
                    eprintln!("nyq sandbox: {e}; UDP and unix sockets stay open")
                }
                Err(e) => fail(e),
                Ok(()) => {}
            }
        }
        // The command runs as a child (not exec) so the link check still
        // runs when it exits abnormally; the status — or the signal — is
        // propagated back unchanged.
        let mut child = match std::process::Command::new(&spec.argv[0])
            .args(&spec.argv[1..])
            .spawn()
        {
            Ok(c) => c,
            Err(e) => fail(format!("{}: {e}", spec.argv[0])),
        };
        let status = child
            .wait()
            .unwrap_or_else(|e| fail(format!("waiting for {}: {e}", spec.argv[0])));
        for note in super::verify_links(&links) {
            eprintln!("nyq sandbox: {note}");
        }
        match status.code() {
            Some(code) => std::process::exit(code),
            None => {
                use std::os::unix::process::ExitStatusExt;
                if let Some(sig) = status.signal() {
                    // Die the same way so callers see the real status.
                    unsafe {
                        libc::signal(sig, libc::SIG_DFL);
                        libc::kill(libc::getpid(), sig);
                    }
                }
                std::process::exit(1);
            }
        }
    }

    /// seccomp-bpf that makes `socket()` fail with EACCES for AF_INET,
    /// AF_INET6 (any type, so UDP too) and, unless the policy allows it,
    /// AF_UNIX. Landlock can only gate TCP; this closes the UDP and
    /// unix-socket gaps. `io_uring` could repeat `socket()` in the kernel
    /// (`IORING_OP_SOCKET`) without ever calling the syscall, so its three
    /// setup/enter/register syscalls fail with EPERM as well. `socketpair()`
    /// stays allowed: it only creates an already-connected anonymous pair —
    /// there is no address to reach the host with. The filter is installed
    /// via `prctl(PR_SET_NO_NEW_PRIVS)` + `seccomp(SECCOMP_SET_MODE_FILTER)`
    /// with a hand-written BPF program checked against the build
    /// architecture.
    pub mod seccomp {
        /// The socket filter has checked syscall numbers and audit arches for
        /// x86_64 and aarch64 only.
        pub fn supported() -> bool {
            cfg!(any(target_arch = "x86_64", target_arch = "aarch64"))
        }

        /// Kernel supports the `seccomp()` syscall (Linux 3.17+).
        pub fn available() -> bool {
            const SECCOMP_GET_ACTION_AVAIL: libc::c_uint = 2;
            let action = libc::SECCOMP_RET_ALLOW;
            unsafe {
                libc::syscall(
                    libc::SYS_seccomp,
                    SECCOMP_GET_ACTION_AVAIL,
                    0,
                    &action as *const _ as usize,
                ) == 0
            }
        }

        #[cfg(target_arch = "x86_64")]
        const AUDIT_ARCH: u32 = 0xC000_003E; // AUDIT_ARCH_X86_64
        #[cfg(target_arch = "aarch64")]
        const AUDIT_ARCH: u32 = 0xC000_00B7; // AUDIT_ARCH_AARCH64

        /// Build the BPF program for the socket filter. `allow_unix` leaves
        /// AF_UNIX open. Kept separate from installation so tests can
        /// interpret the program.
        #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
        fn filter_program(allow_unix: bool) -> Vec<libc::sock_filter> {
            const EACCES: u32 = 13;
            const EPERM: u32 = 1;

            // seccomp_data offsets: nr @0, arch @4, args[0] @16 (little-endian).
            const OFF_NR: u32 = 0;
            const OFF_ARCH: u32 = 4;
            const OFF_ARG0_LO: u32 = 16;

            const BPF_LD: u16 = 0x00;
            const BPF_W: u16 = 0x00;
            const BPF_ABS: u16 = 0x20;
            const BPF_JMP: u16 = 0x05;
            const BPF_JEQ: u16 = 0x10;
            const BPF_K: u16 = 0x00;
            const BPF_RET: u16 = 0x06;

            let stmt = |code: u16, k: u32| libc::sock_filter {
                code,
                jt: 0,
                jf: 0,
                k,
            };
            let jeq = |k: u32, jt: u8, jf: u8| libc::sock_filter {
                code: BPF_JMP | BPF_JEQ | BPF_K,
                jt,
                jf,
                k,
            };

            // io_uring issues IORING_OP_SOCKET (and bind/connect/accept) inside
            // the kernel during io_uring_enter, never touching socket() — it
            // bypassed the family check and reached the host over UDP and unix
            // sockets. Fail all three io_uring syscalls with EPERM when the
            // network is off. Numbers are 425/426/427 on both x86_64 and
            // aarch64 (asm-generic). On x86_64 the x32 ABI reuses the same
            // numbers with __X32_SYSCALL_BIT set; deny that variant too.
            let mut uring_nrs = vec![425u32, 426, 427];
            #[cfg(target_arch = "x86_64")]
            for nr in [425u32, 426, 427] {
                uring_nrs.push(nr | 0x4000_0000);
            }
            // socketpair() stays allowed: it only makes a connected AF_UNIX
            // pair and cannot name or reach a host socket.
            let mut socket_nrs = vec![libc::SYS_socket as u32];
            #[cfg(target_arch = "x86_64")]
            socket_nrs.push((libc::SYS_socket as u32) | 0x4000_0000);
            let mut denied = vec![libc::AF_INET as u32, libc::AF_INET6 as u32];
            if !allow_unix {
                denied.push(libc::AF_UNIX as u32);
            }
            let (n, k, m) = (socket_nrs.len(), uring_nrs.len(), denied.len());
            let allow1 = 3 + n + k; // ret ALLOW after the nr checks
            let domain = allow1 + 1; // `ld args[0]` — only reached for socket()
            let allow2 = domain + 1 + m;
            let eacces = allow2 + 1;
            let eperm = eacces + 1;
            let kill = eperm + 1;

            let mut f = Vec::with_capacity(kill + 1);
            f.push(stmt(BPF_LD | BPF_W | BPF_ABS, OFF_ARCH));
            // Wrong architecture (e.g. i386 compat, where socket() rides
            // socketcall): kill rather than risk a bypass.
            f.push(jeq(AUDIT_ARCH, 0, (kill - 2) as u8));
            f.push(stmt(BPF_LD | BPF_W | BPF_ABS, OFF_NR));
            for (i, nr) in socket_nrs.iter().enumerate() {
                // A match jumps to the domain checks; a mismatch falls
                // through to the next nr check, then to io_uring checks.
                f.push(jeq(*nr, (domain - (3 + i) - 1) as u8, 0));
            }
            for (i, nr) in uring_nrs.iter().enumerate() {
                // io_uring is denied outright: its socket ops never call
                // socket(), so checking the family cannot help.
                f.push(jeq(*nr, (eperm - (3 + n + i) - 1) as u8, 0));
            }
            f.push(stmt(BPF_RET | BPF_K, libc::SECCOMP_RET_ALLOW));
            // args[0] is the socket domain; the kernel reads an int, so the
            // low 32 bits decide. Matching only the low word also denies a
            // domain smuggled in the high bits.
            f.push(stmt(BPF_LD | BPF_W | BPF_ABS, OFF_ARG0_LO));
            for (j, dom) in denied.iter().enumerate() {
                f.push(jeq(*dom, (m - j) as u8, 0));
            }
            f.push(stmt(BPF_RET | BPF_K, libc::SECCOMP_RET_ALLOW));
            f.push(stmt(BPF_RET | BPF_K, libc::SECCOMP_RET_ERRNO | EACCES));
            f.push(stmt(BPF_RET | BPF_K, libc::SECCOMP_RET_ERRNO | EPERM));
            f.push(stmt(BPF_RET | BPF_K, libc::SECCOMP_RET_KILL_PROCESS));
            f
        }

        /// Install the socket filter. `allow_unix` leaves AF_UNIX open.
        #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
        pub fn deny_sockets(allow_unix: bool) -> Result<(), String> {
            let f = filter_program(allow_unix);
            let prog = libc::sock_fprog {
                len: f.len() as u16,
                filter: f.as_ptr() as *mut libc::sock_filter,
            };
            unsafe {
                if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                    return Err("PR_SET_NO_NEW_PRIVS failed".into());
                }
                if libc::syscall(libc::SYS_seccomp, libc::SECCOMP_SET_MODE_FILTER, 0, &prog) != 0 {
                    return Err(format!(
                        "seccomp filter install failed: {}",
                        std::io::Error::last_os_error()
                    ));
                }
            }
            Ok(())
        }

        /// Other architectures have no checked syscall numbers; the command
        /// still runs under Landlock and the status report says partial.
        #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
        pub fn deny_sockets(_allow_unix: bool) -> Result<(), String> {
            let _ = available();
            Err("seccomp socket filter is not built for this architecture".into())
        }

        #[cfg(all(test, any(target_arch = "x86_64", target_arch = "aarch64")))]
        mod tests {
            use super::*;

            /// Run the built program against a fake seccomp_data (nr, arch,
            /// args[0]) and return the SECCOMP_RET_* value it produces.
            /// Only the three opcodes the filter emits are interpreted.
            fn run(prog: &[libc::sock_filter], nr: u32, arch: u32, arg0: u64) -> u32 {
                let mut data = [0u8; 64];
                data[0..4].copy_from_slice(&nr.to_le_bytes());
                data[4..8].copy_from_slice(&arch.to_le_bytes());
                data[16..24].copy_from_slice(&arg0.to_le_bytes());
                let mut a: u32 = 0;
                let mut pc: usize = 0;
                loop {
                    let ins = &prog[pc];
                    match ins.code {
                        // ld [k]
                        0x20 => {
                            a = u32::from_le_bytes(
                                data[ins.k as usize..ins.k as usize + 4].try_into().unwrap(),
                            );
                            pc += 1;
                        }
                        // jeq k
                        0x15 => {
                            pc += 1 + if a == ins.k { ins.jt } else { ins.jf } as usize;
                        }
                        // ret k
                        0x06 => return ins.k,
                        c => panic!("unexpected opcode {c:#x}"),
                    }
                }
            }

            const ALLOW: u32 = libc::SECCOMP_RET_ALLOW;
            const EACCES_RET: u32 = libc::SECCOMP_RET_ERRNO | 13;
            const EPERM_RET: u32 = libc::SECCOMP_RET_ERRNO | 1;
            const KILL_RET: u32 = libc::SECCOMP_RET_KILL_PROCESS;

            #[test]
            fn io_uring_syscalls_fail_with_eperm() {
                let prog = filter_program(false);
                for nr in [425u32, 426, 427] {
                    assert_eq!(run(&prog, nr, AUDIT_ARCH, 0), EPERM_RET, "nr {nr}");
                }
                #[cfg(target_arch = "x86_64")]
                for nr in [425u32 | 0x4000_0000, 426 | 0x4000_0000, 427 | 0x4000_0000] {
                    assert_eq!(run(&prog, nr, AUDIT_ARCH, 0), EPERM_RET, "x32 nr {nr:#x}");
                }
                // A wrong-arch trap still kills rather than bypass.
                assert_eq!(run(&prog, 425, 0x4000_0028, 0), KILL_RET);
            }

            #[test]
            fn socket_family_and_other_calls_unchanged() {
                let prog = filter_program(false);
                let sock = libc::SYS_socket as u32;
                assert_eq!(
                    run(&prog, sock, AUDIT_ARCH, libc::AF_INET as u64),
                    EACCES_RET
                );
                assert_eq!(
                    run(&prog, sock, AUDIT_ARCH, libc::AF_INET6 as u64),
                    EACCES_RET
                );
                assert_eq!(
                    run(&prog, sock, AUDIT_ARCH, libc::AF_UNIX as u64),
                    EACCES_RET
                );
                // Non-denied families (NETLINK) and unrelated calls pass.
                assert_eq!(run(&prog, sock, AUDIT_ARCH, libc::AF_NETLINK as u64), ALLOW);
                assert_eq!(run(&prog, libc::SYS_openat as u32, AUDIT_ARCH, 0), ALLOW);
                // socketpair() rides its own syscall and stays allowed.
                assert_eq!(
                    run(&prog, libc::SYS_socketpair as u32, AUDIT_ARCH, 0),
                    ALLOW
                );
                // --unix re-allows AF_UNIX only.
                let prog = filter_program(true);
                assert_eq!(run(&prog, sock, AUDIT_ARCH, libc::AF_UNIX as u64), ALLOW);
                assert_eq!(
                    run(&prog, sock, AUDIT_ARCH, libc::AF_INET as u64),
                    EACCES_RET
                );
                // io_uring stays denied: IORING_OP_SOCKET ignores AF_UNIX.
                assert_eq!(run(&prog, 425, AUDIT_ARCH, 0), EPERM_RET);
            }
        }
    }

    /// Read-only bind mounts over `.git` metadata inside a private user+mount
    /// namespace. Landlock grants are union-based, so "writable `.git` minus
    /// hooks/config" is not expressible there; a bind mount can.
    pub mod git {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        use std::path::{Path, PathBuf};

        /// Unprivileged user namespaces look usable on this system (for the
        /// startup report; the helper still does the real check per command).
        pub fn userns_available() -> bool {
            if let Ok(s) = std::fs::read_to_string("/proc/sys/user/max_user_namespaces") {
                if s.trim().parse::<u64>().unwrap_or(0) == 0 {
                    return false;
                }
            }
            for (path, denied) in [
                ("/proc/sys/kernel/unprivileged_userns_clone", "0"),
                (
                    "/proc/sys/kernel/apparmor_restrict_unprivileged_userns",
                    "1",
                ),
                ("/proc/sys/kernel/unprivileged_userns_apparmor_policy", "0"),
            ] {
                if std::fs::read_to_string(path).is_ok_and(|s| s.trim() == denied) {
                    return false;
                }
            }
            true
        }

        fn write_proc(path: &str, value: &str) -> Result<(), String> {
            std::fs::write(path, value).map_err(|e| format!("{path}: {e}"))
        }

        /// Enter a new user+mount namespace with the caller's uid/gid mapped
        /// to themselves, so mounts we create stay private and file access
        /// keeps working as the same user.
        fn enter_namespaces() -> Result<(), String> {
            let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
            if unsafe { libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNS) } != 0 {
                return Err(format!(
                    "unshare user+mount namespace: {}",
                    std::io::Error::last_os_error()
                ));
            }
            write_proc("/proc/self/uid_map", &format!("{uid} {uid} 1"))?;
            write_proc("/proc/self/setgroups", "deny")?;
            write_proc("/proc/self/gid_map", &format!("{gid} {gid} 1"))?;
            // Keep our mounts from propagating to (and leaking into) shared
            // peer mounts outside the namespace.
            if unsafe {
                libc::mount(
                    std::ptr::null(),
                    c"/".as_ptr(),
                    std::ptr::null(),
                    libc::MS_PRIVATE | libc::MS_REC,
                    std::ptr::null(),
                )
            } != 0
            {
                return Err(format!(
                    "make mounts private: {}",
                    std::io::Error::last_os_error()
                ));
            }
            Ok(())
        }

        fn c_path(path: &Path) -> Result<CString, String> {
            CString::new(path.as_os_str().as_bytes())
                .map_err(|_| format!("{}: path contains NUL", path.display()))
        }

        /// Re-bind `path` over itself inside the private mount namespace
        /// (open_tree + optional mount_setattr + move_mount). A read-write
        /// self-bind keeps the contents writable but makes the path a mount
        /// point, so `rename`/`rmdir`/`umount` on it fails with EBUSY — this
        /// is what stops `mv .git .git-old` and planting a fresh `.git`.
        /// A symlink can never be a mount point, so for link destinations
        /// `MOVE_MOUNT_T_SYMLINKS` follows the link and the clone lands on
        /// the resolved target instead of failing with EINVAL.
        fn bind(path: &Path, read_only: bool) -> Result<(), String> {
            let c = c_path(path)?;
            let dest_flags = if path.is_symlink() {
                libc::MOVE_MOUNT_F_EMPTY_PATH | libc::MOVE_MOUNT_T_SYMLINKS
            } else {
                libc::MOVE_MOUNT_F_EMPTY_PATH
            };
            unsafe {
                let tree = libc::syscall(
                    libc::SYS_open_tree,
                    libc::AT_FDCWD,
                    c.as_ptr(),
                    libc::OPEN_TREE_CLONE | libc::OPEN_TREE_CLOEXEC,
                );
                if tree < 0 {
                    return Err(format!(
                        "open_tree {}: {}",
                        path.display(),
                        std::io::Error::last_os_error()
                    ));
                }
                if read_only {
                    let attr = libc::mount_attr {
                        attr_set: libc::MOUNT_ATTR_RDONLY,
                        attr_clr: 0,
                        propagation: 0,
                        userns_fd: 0,
                    };
                    if libc::syscall(
                        libc::SYS_mount_setattr,
                        tree,
                        c"".as_ptr(),
                        libc::AT_EMPTY_PATH,
                        &attr,
                        std::mem::size_of::<libc::mount_attr>(),
                    ) != 0
                    {
                        let e = std::io::Error::last_os_error();
                        libc::close(tree as i32);
                        return Err(format!("mount_setattr {}: {e}", path.display()));
                    }
                }
                if libc::syscall(
                    libc::SYS_move_mount,
                    tree,
                    c"".as_ptr(),
                    libc::AT_FDCWD,
                    c.as_ptr(),
                    dest_flags,
                ) != 0
                {
                    let e = std::io::Error::last_os_error();
                    libc::close(tree as i32);
                    return Err(format!("move_mount {}: {e}", path.display()));
                }
                libc::close(tree as i32);
            }
            Ok(())
        }

        /// Pin `targets.pin` with self-binds and bind every read-only path,
        /// then drop CAP_SYS_ADMIN from the bounding set so the command
        /// cannot undo the mounts.
        pub fn protect(targets: &super::super::GitTargets) -> Result<(), String> {
            // Capture the cwd before unshare: it points into the original
            // mount, and lookups that start from a stale cwd or dirfd never
            // cross the mounts we stack on top. Re-resolving it after the
            // binds lands the command inside the pinned view.
            let cwd = std::env::current_dir().ok();
            let pins: Vec<PathBuf> = targets.pin.iter().filter(|p| p.is_dir()).cloned().collect();
            let mut mounts = Vec::new();
            for (path, ensure) in &targets.required {
                if !path.exists() {
                    // A missing hooks dir, config or hooksPath could be
                    // created inside the sandbox; make it real first so it
                    // can be bound. If we cannot create it, neither can the
                    // sandboxed command.
                    let created = match ensure {
                        super::super::Ensure::Dir => std::fs::create_dir(path).map(|_| ()),
                        super::super::Ensure::DirAll => std::fs::create_dir_all(path).map(|_| ()),
                        super::super::Ensure::File => std::fs::write(path, ""),
                    };
                    if created.is_err() {
                        continue;
                    }
                }
                mounts.push(path.clone());
            }
            for path in &targets.readonly {
                if path.exists() {
                    mounts.push(path.clone());
                }
            }
            if pins.is_empty() && mounts.is_empty() {
                return Ok(());
            }
            enter_namespaces()?;
            // Pins first: the read-only binds then land inside an already
            // pinned tree.
            for p in &pins {
                bind(p, false)?;
            }
            for m in &mounts {
                bind(m, true)?;
            }
            // Without CAP_SYS_ADMIN in the bounding set, nothing in the
            // command tree can umount or rebind these paths — even inside a
            // user namespace it creates itself.
            const CAP_SYS_ADMIN: libc::c_int = 21;
            if unsafe { libc::prctl(libc::PR_CAPBSET_DROP, CAP_SYS_ADMIN, 0, 0, 0) } != 0 {
                return Err(format!(
                    "drop CAP_SYS_ADMIN: {}",
                    std::io::Error::last_os_error()
                ));
            }
            if let Some(dir) = cwd {
                std::env::set_current_dir(&dir)
                    .map_err(|e| format!("re-enter {} through the mounts: {e}", dir.display()))?;
            }
            Ok(())
        }

        /// Run `nyq __sandbox --git-probe` in a scratch repo to learn whether
        /// the mount-based `.git` protection really works on this system
        /// (unshare, open_tree, mount_setattr, move_mount — kernel 5.12+).
        /// `None` means the probe could not run — notably under `cargo test`,
        /// where `current_exe` is the test binary, not nyq. The answer is
        /// cached for the process.
        pub fn probe() -> Option<Result<(), String>> {
            static RESULT: std::sync::OnceLock<Option<Result<(), String>>> =
                std::sync::OnceLock::new();
            RESULT.get_or_init(probe_once).clone()
        }

        fn probe_once() -> Option<Result<(), String>> {
            let exe = std::env::current_exe().ok()?;
            let dir = std::env::temp_dir().join(format!("nyq-gitprobe-{}", std::process::id()));
            let found = (|| {
                std::fs::create_dir_all(dir.join(".git/hooks")).ok()?;
                std::fs::write(dir.join(".git/config"), "").ok()?;
                let out = std::process::Command::new(exe)
                    .args([super::super::HELPER_ARG, "--git-probe", "--write"])
                    .arg(&dir)
                    .args(["--", "true"])
                    .current_dir(&dir)
                    .output()
                    .ok()?;
                let stderr = String::from_utf8_lossy(&out.stderr);
                if out.status.success() && stderr.contains("git protection available") {
                    Some(Ok(()))
                } else {
                    stderr
                        .lines()
                        .find_map(|l| l.strip_prefix("nyq sandbox: "))
                        .map(|msg| Err(msg.to_string()))
                }
            })();
            let _ = std::fs::remove_dir_all(&dir);
            found
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn args_round_trip() {
            let argv = vec!["bash".to_string(), "-c".into(), "echo --write hi".into()];
            let policy = super::super::Policy {
                network: true,
                unix_sockets: true,
                git_writable: true,
                git_strict: true,
                ..Default::default()
            };
            let v = helper_args(
                Path::new("/bin/nyq"),
                &[PathBuf::from("/w s")],
                &policy,
                argv.clone(),
            );
            assert_eq!(&v[..2], ["/bin/nyq", "__sandbox"]);
            let spec = parse(&v[2..]).unwrap();
            assert_eq!(spec.writable, vec![PathBuf::from("/w s")]);
            assert!(spec.network && spec.unix_sockets && spec.git_writable && spec.git_strict);
            assert!(!spec.git_probe);
            assert_eq!(spec.argv, argv);
            assert!(parse(&["--net".into()]).is_err());
            assert!(parse(&["--bogus".into(), "--".into(), "x".into()]).is_err());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_bash_skips_system_launcher() {
        let dir = tempfile::tempdir().unwrap();
        let system = dir.path().join("Windows");
        let system_bin = system.join("System32");
        let git = dir.path().join("Git/bin");
        std::fs::create_dir_all(&system_bin).unwrap();
        std::fs::create_dir_all(&git).unwrap();
        std::fs::write(system_bin.join("bash.exe"), "").unwrap();
        std::fs::write(git.join("bash.exe"), "").unwrap();
        assert_eq!(
            windows_bash(
                vec![system_bin.clone(), git.clone()].into_iter(),
                Some(&system)
            ),
            Some(git.join("bash.exe").canonicalize().unwrap())
        );
        assert!(windows_bash(vec![system_bin].into_iter(), Some(&system)).is_none());
        // A relative or empty PATH entry (resolving into the workspace) is never used.
        assert!(
            windows_bash(vec![PathBuf::new(), PathBuf::from("rel")].into_iter(), None).is_none()
        );
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn cmd_preserves_quotes() {
        let script = "echo \"hi there\"";
        let mut command = shell_process(&shell_argv(script, true, None, false));
        let out = command.output().await.unwrap();
        assert!(out.status.success());
        assert_eq!(
            String::from_utf8(out.stdout).unwrap().trim(),
            "\"hi there\""
        );
    }

    #[test]
    fn seatbelt_profile_escapes_and_denies() {
        let git = GitTargets {
            pin: vec![PathBuf::from("/w"), PathBuf::from("/w/.git")],
            required: vec![
                (PathBuf::from("/w/.git/hooks"), Ensure::Dir),
                (PathBuf::from("/w/.git/config"), Ensure::File),
            ],
            readonly: vec![PathBuf::from("/w/.git/info")],
            links: vec![WatchedLink {
                path: PathBuf::from("/w/.git"),
                target: PathBuf::from("/w/realgit"),
                ident: Some((1, 2)),
            }],
            notes: Vec::new(),
        };
        let p = seatbelt_profile(&[PathBuf::from("/tmp/a\"b")], false, &git);
        assert!(p.contains("(deny file-write*)"));
        assert!(p.contains(r#""/tmp/a\"b""#));
        assert!(p.contains("(deny network*)"));
        assert!(!p.contains("unix-socket"));
        // Git metadata is denied after the allows, so the rest of .git (and
        // the workspace) stays writable while hooks and config are not.
        assert!(p.contains(r#"(literal "/w/.git/config")"#));
        assert!(p.contains(r#"(subpath "/w/.git/hooks")"#));
        assert!(p.find(".git").unwrap() > p.find("allow file-write*").unwrap());
        // .git itself and the repo dir are denied rename/unlink so the
        // deny rules cannot be moved aside (the Linux pins do the same);
        // a symlinked protected path joins the same deny list.
        assert!(p.contains(
            r#"(deny file-write-unlink (literal "/w") (literal "/w/.git") (literal "/w/.git"))"#
        ));
        let empty = GitTargets::default();
        assert!(!seatbelt_profile(&[], true, &empty).contains("deny network"));
        assert!(!seatbelt_profile(&[], false, &empty).contains(".git"));
    }

    #[test]
    fn git_targets_cover_dirs_and_worktrees() {
        let root = tempfile::tempdir().unwrap();
        let git = root.path().join(".git");
        std::fs::create_dir_all(git.join("hooks")).unwrap();
        std::fs::write(git.join("config"), "").unwrap();
        std::fs::create_dir_all(git.join("info")).unwrap();
        let roots = &[root.path().to_path_buf()];
        let canon = |p: &Path| std::fs::canonicalize(p).unwrap();
        let t = git_targets(root.path(), roots);
        assert!(
            t.required.contains(&(git.join("hooks"), Ensure::Dir)),
            "{t:?}"
        );
        assert!(
            t.required.contains(&(git.join("config"), Ensure::File)),
            "{t:?}"
        );
        assert!(
            t.required.contains(&(git.join("info"), Ensure::Dir)),
            "{t:?}"
        );
        // The repo dir, `.git` and the writable root are pinned so `mv .git`
        // and `mv repo` fail with EBUSY.
        assert!(t.pin.contains(&canon(&git)), "{t:?}");
        assert!(t.pin.contains(&canon(root.path())), "{t:?}");

        // A `.git` file (worktree pointer) is protected itself plus the real
        // gitdir's shared metadata.
        let main = tempfile::tempdir().unwrap();
        let real_gd = main.path().join("m");
        std::fs::create_dir_all(real_gd.join("hooks")).unwrap();
        std::fs::write(real_gd.join("config"), "").unwrap();
        let wt = root.path().join("wt");
        std::fs::create_dir(&wt).unwrap();
        std::fs::write(wt.join(".git"), format!("gitdir: {}", real_gd.display())).unwrap();
        let t = git_targets(&wt, roots);
        assert!(t.readonly.contains(&wt.join(".git")), "{t:?}");
        assert!(
            t.required.contains(&(real_gd.join("hooks"), Ensure::Dir)),
            "{t:?}"
        );
        assert!(
            t.required.contains(&(real_gd.join("config"), Ensure::File)),
            "{t:?}"
        );
        assert!(t.pin.contains(&canon(&wt)), "{t:?}");
        assert!(t.pin.contains(&canon(&real_gd)), "{t:?}");
    }

    #[test]
    fn git_targets_cover_modules_hookspath_and_worktree_config() {
        let root = tempfile::tempdir().unwrap();
        let git = root.path().join(".git");
        std::fs::create_dir_all(git.join("hooks")).unwrap();
        let canon = |p: &Path| std::fs::canonicalize(p).unwrap();
        // Submodule gitdir under .git/modules, a hooksPath target inside the
        // workspace, and extensions.worktreeConfig = true.
        let module = git.join("modules/sub");
        std::fs::create_dir_all(module.join("hooks")).unwrap();
        std::fs::write(module.join("config"), "").unwrap();
        std::fs::write(
            git.join("config"),
            "[core]\n\thooksPath = custom/hooks\n[extensions]\n\tworktreeConfig = true\n",
        )
        .unwrap();
        // A linked worktree gitdir with a commondir back-pointer.
        let wt_gd = git.join("worktrees/wt1");
        std::fs::create_dir_all(&wt_gd).unwrap();
        std::fs::write(wt_gd.join("commondir"), "../..").unwrap();
        let t = git_targets(root.path(), &[root.path().to_path_buf()]);
        // The submodule's own config and hooks are protected, not only the
        // main repo's.
        assert!(
            t.required.contains(&(module.join("config"), Ensure::File)),
            "{t:?}"
        );
        assert!(
            t.required.contains(&(module.join("hooks"), Ensure::Dir)),
            "{t:?}"
        );
        assert!(t.pin.contains(&canon(&module)), "{t:?}");
        // core.hooksPath is read without running git; the target must exist
        // (DirAll) so it can be bound read-only.
        assert!(
            t.required
                .contains(&(root.path().join("custom/hooks"), Ensure::DirAll)),
            "{t:?}"
        );
        // The worktree gitdir keeps its commondir and gets config.worktree.
        assert!(t.readonly.contains(&wt_gd.join("commondir")), "{t:?}");
        assert!(
            t.required
                .contains(&(wt_gd.join("config.worktree"), Ensure::File)),
            "{t:?}"
        );
        assert!(t.pin.contains(&canon(&wt_gd)), "{t:?}");
        assert!(t.pin.contains(&canon(&git.join("modules"))), "{t:?}");
        assert!(t.pin.contains(&canon(&git.join("worktrees"))), "{t:?}");
    }

    #[test]
    fn git_targets_require_info_and_cover_worktree_config() {
        // `.git/info` is required even when it does not exist yet, so a
        // planted `attributes`/`exclude` cannot apply outside the sandbox.
        let root = tempfile::tempdir().unwrap();
        let git = root.path().join(".git");
        std::fs::create_dir_all(git.join("hooks")).unwrap();
        std::fs::write(git.join("config"), "").unwrap();
        assert!(!git.join("info").exists());
        let t = git_targets(root.path(), &[root.path().to_path_buf()]);
        assert!(
            t.required.contains(&(git.join("info"), Ensure::Dir)),
            "{t:?}"
        );

        // An existing config.worktree is bound read-only, and with
        // extensions.worktreeConfig it is required even when missing.
        std::fs::write(git.join("config.worktree"), "[core]\n\tbare = false\n").unwrap();
        let t = git_targets(root.path(), &[root.path().to_path_buf()]);
        assert!(t.readonly.contains(&git.join("config.worktree")), "{t:?}");
        std::fs::write(
            git.join("config"),
            "[extensions]\n\tworktreeConfig = true\n",
        )
        .unwrap();
        let t = git_targets(root.path(), &[root.path().to_path_buf()]);
        assert!(
            t.required
                .contains(&(git.join("config.worktree"), Ensure::File)),
            "{t:?}"
        );
    }

    #[cfg(unix)]
    fn repo_with_git_link() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("realgit");
        std::fs::create_dir_all(real.join("hooks")).unwrap();
        std::fs::write(real.join("config"), "").unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        std::os::unix::fs::symlink(&real, repo.join(".git")).unwrap();
        (dir, real, repo)
    }

    /// Paths of `nyq-quarantine` siblings of `path`.
    #[cfg(unix)]
    fn quarantined(path: &Path) -> Vec<PathBuf> {
        let parent = path.parent().unwrap_or(path);
        let prefix = format!(
            "{}.nyq-quarantine-",
            path.file_name().unwrap().to_string_lossy()
        );
        std::fs::read_dir(parent)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(&prefix)
            })
            .collect()
    }

    #[cfg(unix)]
    #[test]
    fn link_guard_restores_a_swapped_git_symlink() {
        let (_dir, real, repo) = repo_with_git_link();
        let policy = Policy {
            mode: Mode::WorkspaceWrite,
            ..Default::default()
        };
        let mut guard = GitLinkGuard::watch(&policy, &repo);
        assert_eq!(guard.watched.len(), 1, "{:?}", guard.watched);
        // The attack: rename the link aside, plant a real .git with a hook.
        std::fs::rename(repo.join(".git"), repo.join("kept")).unwrap();
        std::fs::create_dir_all(repo.join(".git/hooks")).unwrap();
        std::fs::write(repo.join(".git/hooks/post-commit"), "#!/bin/sh").unwrap();

        let warnings = guard.finish();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("SECURITY"), "{warnings:?}");
        // The original link is back and the impostor was moved aside,
        // hook and all — never deleted.
        assert!(repo.join(".git").is_symlink());
        assert_eq!(std::fs::read_link(repo.join(".git")).unwrap(), real);
        assert!(!real.join("hooks/post-commit").exists());
        let q = quarantined(&repo.join(".git"));
        assert_eq!(q.len(), 1, "{q:?}");
        assert!(q[0].join("hooks/post-commit").exists());
        // Idempotent: a second finish reports nothing.
        assert!(guard.finish().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn link_guard_restores_a_deleted_hooks_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let git = repo.join(".git");
        std::fs::create_dir_all(&git).unwrap();
        std::fs::write(git.join("config"), "").unwrap();
        let hooks_target = dir.path().join("hooks-target");
        std::fs::create_dir(&hooks_target).unwrap();
        std::os::unix::fs::symlink(&hooks_target, git.join("hooks")).unwrap();
        let policy = Policy {
            mode: Mode::WorkspaceWrite,
            ..Default::default()
        };
        let mut guard = GitLinkGuard::watch(&policy, &repo);
        assert!(
            guard.watched.iter().any(|l| l.path == git.join("hooks")),
            "{:?}",
            guard.watched
        );
        // The command deletes the link outright: it is recreated.
        std::fs::remove_file(git.join("hooks")).unwrap();
        let warnings = guard.finish();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("deleted"), "{warnings:?}");
        assert_eq!(std::fs::read_link(git.join("hooks")).unwrap(), hooks_target);

        // A retargeted link is quarantined and the original restored.
        let mut guard = GitLinkGuard::watch(&policy, &repo);
        std::fs::remove_file(git.join("hooks")).unwrap();
        std::os::unix::fs::symlink("/tmp", git.join("hooks")).unwrap();
        let warnings = guard.finish();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("retargeted"), "{warnings:?}");
        assert_eq!(std::fs::read_link(git.join("hooks")).unwrap(), hooks_target);
        let q = quarantined(&git.join("hooks"));
        assert_eq!(q.len(), 1, "{q:?}");
        assert!(q[0].is_symlink());
    }

    #[cfg(unix)]
    #[test]
    fn link_guard_reports_an_untouched_link_as_clean() {
        let (_dir, _real, repo) = repo_with_git_link();
        let policy = Policy {
            mode: Mode::WorkspaceWrite,
            ..Default::default()
        };
        let mut guard = GitLinkGuard::watch(&policy, &repo);
        assert!(guard.finish().is_empty());
        // Mode::Off and git_writable watch nothing.
        let off = GitLinkGuard::watch(
            &Policy {
                mode: Mode::Off,
                ..Default::default()
            },
            &repo,
        );
        assert!(off.watched.is_empty());
        let writable = GitLinkGuard::watch(
            &Policy {
                git_writable: true,
                ..Default::default()
            },
            &repo,
        );
        assert!(writable.watched.is_empty());
    }

    #[test]
    fn git_config_reads_sections_includes_and_bools() {
        let dir = tempfile::tempdir().unwrap();
        let inc = dir.path().join("inc.conf");
        std::fs::write(&inc, "[core]\n\thooksPath = \"a/b\" # tail\n").unwrap();
        let cfg = dir.path().join("config");
        std::fs::write(
            &cfg,
            "; c\n[CORE]\n\thooksPath = first\n[include]\n\tpath = inc.conf\n[extensions]\n\tworktreeConfig\n",
        )
        .unwrap();
        assert_eq!(
            git_config_last(std::slice::from_ref(&cfg), "core", "hookspath"),
            Some("a/b".into())
        );
        assert!(git_config_truthy(&cfg, "extensions", "worktreeconfig"));
        assert!(!git_config_truthy(&cfg, "extensions", "objectformat"));
        // A relative include resolves against the config file's directory.
        assert_eq!(
            expand_config_path("inc.conf", Some(dir.path())),
            dir.path().join("inc.conf")
        );
    }

    #[test]
    fn shell_selection_matches_the_platform() {
        assert_eq!(
            shell_argv("echo hi", true, None, false),
            vec![
                "cmd".to_string(),
                "/S".into(),
                "/C".into(),
                "echo hi".into()
            ]
        );
        assert_eq!(
            shell_argv("echo hi", true, Some(Path::new("/git/bash.exe")), false)[..2],
            ["/git/bash.exe", "-c"]
        );
        assert_eq!(shell_argv("echo hi", false, None, false)[0], "sh");
        assert_eq!(shell_argv("echo hi", false, None, true)[0], "bash");
    }

    #[test]
    fn tmpdir_must_sit_under_a_system_temp_root() {
        // Not under /tmp: a workspace `tmp` must not become a writable root.
        let home = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("nyq-fake-home-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let nested = home.join("tmp");
        std::fs::create_dir_all(&nested).unwrap();
        assert!(accepted_tmpdir(&nested, Some(&home)).is_none());
        assert!(accepted_tmpdir(&home, Some(&home)).is_none());
        assert!(accepted_tmpdir(Path::new("/"), Some(&home)).is_none());
        if Path::new("/tmp").is_dir() {
            let t = tempfile::Builder::new()
                .prefix("nyq-tmp-")
                .tempdir_in("/tmp")
                .unwrap();
            assert!(accepted_tmpdir(t.path(), Some(&home)).is_some());
            #[cfg(unix)]
            {
                let link =
                    std::env::temp_dir().join(format!("nyq-tmpdir-link-{}", std::process::id()));
                let _ = std::fs::remove_file(&link);
                if std::os::unix::fs::symlink(&nested, &link).is_ok() {
                    assert!(accepted_tmpdir(&link, Some(&home)).is_none());
                    let _ = std::fs::remove_file(&link);
                }
            }
        }
        if Path::new("/var/tmp").is_dir() {
            let t = tempfile::Builder::new()
                .prefix("nyq-tmp-")
                .tempdir_in("/var/tmp")
                .unwrap();
            assert!(accepted_tmpdir(t.path(), Some(&home)).is_some());
        }
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn describe_reports_enforcement_level() {
        let off = describe(&Policy {
            mode: Mode::Off,
            ..Default::default()
        });
        assert!(off.contains("off"), "{off}");
        let p = describe(&Policy {
            mode: Mode::ReadOnly,
            network: false,
            ..Default::default()
        });
        assert!(p.contains("read-only"), "{p}");
        assert!(p.contains("enforcement:"), "{p}");
        #[cfg(target_os = "linux")]
        if p.contains("landlock v") {
            // A modern kernel enforces everything: Landlock writes and TCP,
            // seccomp UDP and unix sockets, mounts .git metadata.
            assert!(p.contains("enforcement: full"), "{p}");
            assert_eq!(p.matches("unix sockets").count(), 0, "{p}");
        }
    }

    #[test]
    fn status_off_is_not_enforced() {
        let st = status(&Policy {
            mode: Mode::Off,
            ..Default::default()
        });
        assert_eq!(st.backend, Backend::None);
        assert_eq!(st.enforcement, Enforcement::None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn git_writable_setting_removes_the_userns_reason() {
        let mut policy = Policy::default();
        let without = status(&policy);
        policy.git_writable = true;
        let with = status(&policy);
        let count = |s: &SandboxStatus| match &s.enforcement {
            Enforcement::Partial(r) => r.iter().filter(|x| x.contains(".git")).count(),
            _ => 0,
        };
        assert!(count(&without) >= count(&with), "{without:?} {with:?}");
    }

    #[test]
    fn helper_override_must_be_this_binary() {
        let exe = std::env::current_exe().unwrap();
        assert_eq!(trusted_helper(&exe, None).unwrap(), exe);
        assert_eq!(trusted_helper(&exe, Some(exe.as_path())).unwrap(), exe);
        let other = std::env::temp_dir().join(format!("nyq-not-the-binary-{}", std::process::id()));
        let err = trusted_helper(&exe, Some(&other)).unwrap_err();
        assert!(err.to_string().contains("NYQ_SANDBOX_HELPER"), "{err}");
    }

    #[test]
    fn off_runs_plain_shell() {
        let c = shell_command(
            &Policy {
                mode: Mode::Off,
                ..Default::default()
            },
            Path::new("/"),
            "true",
        )
        .unwrap();
        let prog = c.as_std().get_program().to_string_lossy().into_owned();
        assert!(
            prog == "bash"
                || prog == "sh"
                || prog == "cmd"
                || Path::new(&prog)
                    .file_name()
                    .is_some_and(|n| n == "bash.exe"),
            "{prog}"
        );
    }
}
