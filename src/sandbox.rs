//! Sandboxing for model-run shell commands.
//!
//! Linux re-executes nyq as `nyq __sandbox …`, which restricts its own
//! process and then execs the shell, so the whole command tree inherits the
//! policy. The helper layers three mechanisms:
//! - a user+mount namespace that re-binds `.git/hooks`, `.git/config` and
//!   `.git/info` (for `attributes`) read-only over each writable root's `.git`,
//!   so commits and branches keep working while planted hooks and config
//!   redirects cannot run unsandboxed later;
//! - Landlock: reads everywhere, writes only to allowed roots, and TCP
//!   bind/connect denied on ABI v4+ when the network policy is off;
//! - a seccomp filter, also only when the network policy is off, that fails
//!   `socket()` with EACCES for AF_INET/AF_INET6 of any type (covering UDP)
//!   and for AF_UNIX. `NYQ_SANDBOX_UNIX=1` re-allows AF_UNIX.
//!
//! macOS wraps the shell in `sandbox-exec` with an equivalent Seatbelt
//! profile, including deny rules for the same `.git` paths. Other platforms
//! have no sandbox; `check` reports that. `NYQ_SANDBOX_GIT_WRITABLE=1` opts
//! out of git-metadata protection on both.
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
    /// (`NYQ_SANDBOX_UNIX=1` sets the default).
    pub unix_sockets: bool,
    /// Leave `.git/hooks`, `.git/config` and `.git/info` writable inside the
    /// sandbox, opting out of git-metadata protection
    /// (`NYQ_SANDBOX_GIT_WRITABLE=1` sets the default).
    pub git_writable: bool,
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

/// `.git` paths a sandboxed command must not write: hooks and config under
/// every reachable git dir, so a planted hook or `core.hooksPath` redirect
/// cannot run later outside the sandbox. `cwd` and `writable` are the same
/// roots the command may write. Paths are returned whether or not they exist;
/// a caller that needs a real mount point may create a missing `hooks`
/// directory or `config` file.
fn protected_git_paths(cwd: &Path, writable: &[PathBuf]) -> Vec<PathBuf> {
    let mut gitdirs: Vec<PathBuf> = Vec::new();
    let mut pointers: Vec<PathBuf> = Vec::new();
    let mut consider = |dir: &Path| {
        if !writable.iter().any(|root| dir.starts_with(root)) {
            return;
        }
        let git = dir.join(".git");
        match std::fs::metadata(&git) {
            Ok(m) if m.is_dir() => gitdirs.push(git),
            Ok(_) => {
                if let Some(gd) = resolve_gitfile(&git) {
                    pointers.push(git);
                    gitdirs.push(gd);
                }
            }
            Err(_) => {}
        }
    };
    for root in writable {
        consider(root);
    }
    let mut dir = Some(cwd);
    while let Some(d) = dir {
        consider(d);
        dir = d.parent();
    }

    let mut seen = std::collections::HashSet::new();
    let mut out: Vec<PathBuf> = pointers;
    for gitdir in gitdirs {
        let gd = common_gitdir(&gitdir);
        if !seen.insert(gd.clone()) {
            continue;
        }
        out.extend([gd.join("hooks"), gd.join("config")]);
        let info = gd.join("info");
        if info.join("attributes").exists() || info.is_dir() {
            out.push(if info.join("attributes").exists() {
                info.join("attributes")
            } else {
                info
            });
        }
    }
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
        Vec::new()
    } else {
        protected_git_paths(cwd, &writable)
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

/// A macOS Seatbelt (SBPL) profile allowing writes only under `roots`. `git`
/// lists the `.git` paths that stay read-only; deny rules come last because
/// Seatbelt lets later rules win.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn seatbelt_profile(roots: &[PathBuf], network: bool, git: &[PathBuf]) -> String {
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
    if !git.is_empty() {
        s.push_str("(deny file-write*");
        for p in git {
            s.push_str(&format!(" (literal {}) (subpath {})", quote(p), quote(p)));
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
        if policy.mode == Mode::WorkspaceWrite
            && !policy.git_writable
            && !linux::git::userns_available()
        {
            reasons.push(".git hooks/config stay writable (user namespaces unavailable)".into());
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
    let engine = match st.backend {
        #[cfg(target_os = "linux")]
        Backend::Landlock => linux::abi()
            .map(|abi| format!("landlock v{abi}"))
            .unwrap_or_else(|| "landlock unavailable".into()),
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
    use std::os::unix::process::CommandExt;
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

    /// `exe __sandbox [--net] [--unix] [--git-writable] [--write PATH]... -- argv...`
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
        pub argv: Vec<String>,
    }

    /// Inverse of `helper_args` (without exe and HELPER_ARG).
    pub fn parse(args: &[String]) -> Result<Spec, String> {
        let mut spec = Spec {
            writable: Vec::new(),
            network: false,
            unix_sockets: false,
            git_writable: false,
            argv: Vec::new(),
        };
        let mut it = args.iter();
        while let Some(a) = it.next() {
            match a.as_str() {
                "--net" => spec.network = true,
                "--unix" => spec.unix_sockets = true,
                "--git-writable" => spec.git_writable = true,
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
        if !spec.git_writable {
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
            let targets = super::protected_git_paths(&cwd, &spec.writable);
            if let Err(e) = git::protect(&targets) {
                eprintln!("nyq sandbox: .git metadata stays writable: {e}");
            }
        }
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
        let err = std::process::Command::new(&spec.argv[0])
            .args(&spec.argv[1..])
            .exec();
        fail(format!("{}: {err}", spec.argv[0]))
    }

    /// seccomp-bpf that makes `socket()` fail with EACCES for AF_INET,
    /// AF_INET6 (any type, so UDP too) and, unless the policy allows it,
    /// AF_UNIX. Landlock can only gate TCP; this closes the UDP and
    /// unix-socket gaps. The filter is installed via
    /// `prctl(PR_SET_NO_NEW_PRIVS)` + `seccomp(SECCOMP_SET_MODE_FILTER)` with
    /// a hand-written BPF program checked against the build architecture.
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

        /// Install the socket filter. `allow_unix` leaves AF_UNIX open.
        #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
        pub fn deny_sockets(allow_unix: bool) -> Result<(), String> {
            #[cfg(target_arch = "x86_64")]
            const AUDIT_ARCH: u32 = 0xC000_003E; // AUDIT_ARCH_X86_64
            #[cfg(target_arch = "aarch64")]
            const AUDIT_ARCH: u32 = 0xC000_00B7; // AUDIT_ARCH_AARCH64
            const EACCES: u32 = 13;

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

            // Syscall numbers to check. On x86_64 the x32 ABI reuses the same
            // numbers with __X32_SYSCALL_BIT set; deny that variant too.
            let mut socket_nrs = vec![libc::SYS_socket as u32];
            #[cfg(target_arch = "x86_64")]
            socket_nrs.push((libc::SYS_socket as u32) | 0x4000_0000);
            let mut denied = vec![libc::AF_INET as u32, libc::AF_INET6 as u32];
            if !allow_unix {
                denied.push(libc::AF_UNIX as u32);
            }
            let (n, m) = (socket_nrs.len(), denied.len());
            let allow_idx = 3 + n + 1 + m; // instruction index of `ret ALLOW`

            let mut f = Vec::with_capacity(allow_idx + 3);
            f.push(stmt(BPF_LD | BPF_W | BPF_ABS, OFF_ARCH));
            // Wrong architecture (e.g. i386 compat, where socket() rides
            // socketcall): kill rather than risk a bypass.
            f.push(jeq(AUDIT_ARCH, 0, (allow_idx + 2 - 2) as u8));
            f.push(stmt(BPF_LD | BPF_W | BPF_ABS, OFF_NR));
            for (i, nr) in socket_nrs.iter().enumerate() {
                // A match jumps to the domain checks; the last nr check must
                // jump to ALLOW on mismatch, not fall through to them.
                let jf = if i + 1 == n { (m + 1) as u8 } else { 0 };
                f.push(jeq(*nr, (n - 1 - i) as u8, jf));
            }
            // args[0] is the socket domain; the kernel reads an int, so the
            // low 32 bits decide. Matching only the low word also denies a
            // domain smuggled in the high bits.
            f.push(stmt(BPF_LD | BPF_W | BPF_ABS, OFF_ARG0_LO));
            for (j, dom) in denied.iter().enumerate() {
                f.push(jeq(*dom, (m - j) as u8, 0));
            }
            f.push(stmt(BPF_RET | BPF_K, libc::SECCOMP_RET_ALLOW));
            f.push(stmt(BPF_RET | BPF_K, libc::SECCOMP_RET_ERRNO | EACCES));
            f.push(stmt(BPF_RET | BPF_K, libc::SECCOMP_RET_KILL_PROCESS));

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

        /// Re-bind `path` read-only over itself (open_tree + mount_setattr +
        /// move_mount), inside the private mount namespace.
        fn bind_read_only(path: &Path) -> Result<(), String> {
            let c = c_path(path)?;
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
                if libc::syscall(
                    libc::SYS_move_mount,
                    tree,
                    c"".as_ptr(),
                    libc::AT_FDCWD,
                    c.as_ptr(),
                    libc::MOVE_MOUNT_F_EMPTY_PATH,
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

        /// Bind every `.git` metadata path read-only, then drop CAP_SYS_ADMIN
        /// from the bounding set so the command cannot undo the mounts.
        pub fn protect(targets: &[PathBuf]) -> Result<(), String> {
            let mut mounts = Vec::new();
            for target in targets {
                if !target.exists() {
                    // A missing hooks dir or config could be created inside
                    // the sandbox; make it real first so it can be bound.
                    let name = target.file_name().and_then(|n| n.to_str());
                    let created = match name {
                        Some("hooks") => std::fs::create_dir(target),
                        Some("config") => std::fs::write(target, ""),
                        _ => continue,
                    };
                    if created.is_err() {
                        continue;
                    }
                }
                mounts.push(target.clone());
            }
            if mounts.is_empty() {
                return Ok(());
            }
            enter_namespaces()?;
            for m in &mounts {
                bind_read_only(m)?;
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
            Ok(())
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
            assert!(spec.network && spec.unix_sockets && spec.git_writable);
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
        let git = vec![
            PathBuf::from("/w/.git/hooks"),
            PathBuf::from("/w/.git/config"),
        ];
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
        assert!(!seatbelt_profile(&[], true, &[]).contains("deny network"));
        assert!(!seatbelt_profile(&[], false, &[]).contains(".git"));
    }

    #[test]
    fn protected_git_paths_cover_dirs_and_worktrees() {
        let root = tempfile::tempdir().unwrap();
        let git = root.path().join(".git");
        std::fs::create_dir_all(git.join("hooks")).unwrap();
        std::fs::write(git.join("config"), "").unwrap();
        std::fs::create_dir_all(git.join("info")).unwrap();
        let roots = &[root.path().to_path_buf()];
        let out = protected_git_paths(root.path(), roots);
        assert!(out.contains(&git.join("hooks")), "{out:?}");
        assert!(out.contains(&git.join("config")), "{out:?}");
        assert!(out.contains(&git.join("info")), "{out:?}");

        // A `.git` file (worktree pointer) is protected itself plus the real
        // gitdir's shared metadata.
        let main = tempfile::tempdir().unwrap();
        let real_gd = main.path().join("m");
        std::fs::create_dir_all(real_gd.join("hooks")).unwrap();
        std::fs::write(real_gd.join("config"), "").unwrap();
        let wt = root.path().join("wt");
        std::fs::create_dir(&wt).unwrap();
        std::fs::write(wt.join(".git"), format!("gitdir: {}", real_gd.display())).unwrap();
        let out = protected_git_paths(&wt, roots);
        assert!(out.contains(&wt.join(".git")), "{out:?}");
        assert!(out.contains(&real_gd.join("hooks")), "{out:?}");
        assert!(out.contains(&real_gd.join("config")), "{out:?}");
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
