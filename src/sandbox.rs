//! Sandboxing for model-run shell commands.
//!
//! Linux uses Landlock: nyq re-executes itself as `nyq __sandbox …`, which
//! restricts its own process (writes only to allowed roots, optionally no TCP)
//! and then execs the shell, so the whole command tree inherits the policy.
//! macOS wraps the shell in `sandbox-exec` with an equivalent profile. Other
//! platforms have no sandbox; `check` reports that.
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

#[derive(Clone, Debug, Default)]
pub struct Policy {
    pub mode: Mode,
    /// Allow network access from sandboxed commands.
    pub network: bool,
    /// Extra writable roots besides the workspace (WorkspaceWrite only).
    pub writable: Vec<PathBuf>,
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
    let mut v: Vec<PathBuf> = ["/tmp", "/var/tmp", "/dev/null", "/dev/zero", "/dev/full", "/dev/tty", "/dev/pts", "/dev/shm"]
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

fn find_in_path(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?).map(|d| d.join(name)).find(|p| p.is_file())
}

/// `windows` / `bash_exe` / `bash` are injected so tests can cover every branch.
fn shell_argv(script: &str, windows: bool, bash_exe: bool, bash: bool) -> Vec<String> {
    let script = script.to_string();
    if windows && !bash_exe {
        return vec!["cmd".into(), "/C".into(), script];
    }
    let sh = if windows || bash { "bash" } else { "sh" };
    vec![sh.into(), "-c".into(), script]
}

/// The shell used for commands: bash when available, else sh (cmd on Windows).
fn shell(script: &str) -> Vec<String> {
    shell_argv(script, cfg!(windows), find_in_path("bash.exe").is_some(), find_in_path("bash").is_some())
}

/// A shell command running `script` in `cwd`, confined according to the policy.
pub fn shell_command(policy: &Policy, cwd: &Path, script: &str) -> anyhow::Result<tokio::process::Command> {
    let argv = shell(script);
    let argv = match policy.mode {
        Mode::Off => argv,
        _ => wrap(policy, cwd, argv)?,
    };
    let mut c = tokio::process::Command::new(&argv[0]);
    c.args(&argv[1..]).current_dir(cwd);
    Ok(c)
}

#[cfg(target_os = "linux")]
fn wrap(policy: &Policy, cwd: &Path, argv: Vec<String>) -> anyhow::Result<Vec<String>> {
    let exe = match std::env::var_os("NYQ_SANDBOX_HELPER") {
        Some(p) => PathBuf::from(p),
        None => std::env::current_exe()?,
    };
    Ok(linux::helper_args(&exe, &writable_roots(policy, cwd), policy.network, argv))
}

#[cfg(target_os = "macos")]
fn wrap(policy: &Policy, cwd: &Path, argv: Vec<String>) -> anyhow::Result<Vec<String>> {
    let mut v = vec!["/usr/bin/sandbox-exec".to_string(), "-p".into(), seatbelt_profile(&writable_roots(policy, cwd), policy.network)];
    v.extend(argv);
    Ok(v)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn wrap(_policy: &Policy, _cwd: &Path, _argv: Vec<String>) -> anyhow::Result<Vec<String>> {
    anyhow::bail!("sandboxing is not supported on this platform")
}

/// A macOS Seatbelt (SBPL) profile allowing writes only under `roots`.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn seatbelt_profile(roots: &[PathBuf], network: bool) -> String {
    let quote = |p: &Path| format!("\"{}\"", p.display().to_string().replace('\\', "\\\\").replace('"', "\\\""));
    let mut s = String::from("(version 1)\n(allow default)\n(deny file-write*)\n(allow file-write*");
    for r in roots {
        let kind = if r.is_dir() { "subpath" } else { "literal" };
        s.push_str(&format!(" ({kind} {})", quote(r)));
    }
    s.push_str(")\n");
    if !network {
        // `(deny network*)` includes unix sockets. Re-allowing them was an
        // escape through host daemons such as the Docker socket.
        s.push_str("(deny network*)\n");
    }
    s
}

/// Error if the policy can't be enforced on this system.
pub fn check(policy: &Policy) -> Result<(), String> {
    if policy.mode == Mode::Off {
        return Ok(());
    }
    #[cfg(target_os = "linux")]
    return linux::abi().map(|_| ()).ok_or_else(|| "Landlock is not supported or not enabled in this kernel".into());
    #[cfg(target_os = "macos")]
    return if Path::new("/usr/bin/sandbox-exec").exists() { Ok(()) } else { Err("/usr/bin/sandbox-exec not found".into()) };
    #[allow(unreachable_code)]
    Err("sandboxing is not supported on this platform".into())
}

/// One line for the banner.
pub fn describe(policy: &Policy) -> String {
    let mode = match policy.mode {
        Mode::Off => return "sandbox: off — commands run with your full permissions".into(),
        Mode::ReadOnly => "read-only",
        Mode::WorkspaceWrite => "workspace-write",
    };
    let net = if policy.network { "network on" } else { "network off" };
    #[cfg(target_os = "linux")]
    if let Some(abi) = linux::abi() {
        // TCP rules need Landlock ABI v4. Landlock cannot restrict AF_UNIX.
        let net = if !policy.network && abi < 4 { "network not restricted (kernel too old)" } else { net };
        return format!("sandbox: {mode} · {net} (landlock v{abi}; unix sockets not restricted)");
    }
    let engine = if cfg!(target_os = "macos") { "seatbelt" } else { "unavailable" };
    format!("sandbox: {mode} · {net} ({engine})")
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
        path_beneath_rules, Access, AccessFs, AccessNet, CompatLevel, Compatible, Ruleset, RulesetAttr,
        RulesetCreatedAttr, RulesetStatus, ABI,
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

    /// `exe __sandbox [--net] [--write PATH]... -- argv...`
    pub fn helper_args(exe: &Path, writable: &[PathBuf], network: bool, argv: Vec<String>) -> Vec<String> {
        let mut v = vec![exe.display().to_string(), super::HELPER_ARG.into()];
        if network {
            v.push("--net".into());
        }
        for w in writable {
            v.push("--write".into());
            v.push(w.display().to_string());
        }
        v.push("--".into());
        v.extend(argv);
        v
    }

    /// Inverse of `helper_args` (without exe and HELPER_ARG).
    pub fn parse(args: &[String]) -> Result<(Vec<PathBuf>, bool, Vec<String>), String> {
        let (mut writable, mut network) = (Vec::new(), false);
        let mut it = args.iter();
        while let Some(a) = it.next() {
            match a.as_str() {
                "--net" => network = true,
                "--write" => writable.push(PathBuf::from(it.next().ok_or("--write needs a path")?)),
                "--" => {
                    let argv: Vec<String> = it.cloned().collect();
                    if argv.is_empty() {
                        return Err("no command given".into());
                    }
                    return Ok((writable, network, argv));
                }
                other => return Err(format!("unknown argument {other}")),
            }
        }
        Err("missing `--` before the command".into())
    }

    fn restrict(writable: &[PathBuf], network: bool) -> Result<RulesetStatus, String> {
        let abi = ABI::V6;
        let mut ruleset = Ruleset::default().handle_access(AccessFs::from_all(abi)).map_err(|e| e.to_string())?;
        if !network {
            ruleset = ruleset.handle_access(AccessNet::BindTcp | AccessNet::ConnectTcp).map_err(|e| e.to_string())?;
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
        let (writable, network, argv) = parse(&args).unwrap_or_else(|e| fail(e));
        match restrict(&writable, network) {
            Ok(RulesetStatus::NotEnforced) => fail("Landlock is not enforced by this kernel; refusing to run unconfined".into()),
            Ok(_) => {}
            Err(e) => fail(e),
        }
        let err = std::process::Command::new(&argv[0]).args(&argv[1..]).exec();
        fail(format!("{}: {err}", argv[0]))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn args_round_trip() {
            let argv = vec!["bash".to_string(), "-c".into(), "echo --write hi".into()];
            let v = helper_args(Path::new("/bin/nyq"), &[PathBuf::from("/w s")], false, argv.clone());
            assert_eq!(&v[..2], ["/bin/nyq", "__sandbox"]);
            assert_eq!(parse(&v[2..]).unwrap(), (vec![PathBuf::from("/w s")], false, argv));
            assert!(parse(&["--net".into()]).is_err());
            assert!(parse(&["--bogus".into(), "--".into(), "x".into()]).is_err());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seatbelt_profile_escapes_and_denies() {
        let p = seatbelt_profile(&[PathBuf::from("/tmp/a\"b")], false);
        assert!(p.contains("(deny file-write*)"));
        assert!(p.contains(r#""/tmp/a\"b""#));
        assert!(p.contains("(deny network*)"));
        assert!(!p.contains("unix-socket"));
        assert!(!seatbelt_profile(&[], true).contains("deny network"));
    }

    #[test]
    fn shell_selection_matches_the_platform() {
        assert_eq!(
            shell_argv("echo hi", true, false, false),
            vec!["cmd".to_string(), "/C".into(), "echo hi".into()]
        );
        assert_eq!(shell_argv("echo hi", true, true, false)[..2], ["bash", "-c"]);
        assert_eq!(shell_argv("echo hi", false, false, false)[0], "sh");
        assert_eq!(shell_argv("echo hi", false, false, true)[0], "bash");
    }

    #[test]
    fn tmpdir_must_sit_under_a_system_temp_root() {
        // Not under /tmp: a workspace `tmp` must not become a writable root.
        let home = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target").join(format!("nyq-fake-home-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let nested = home.join("tmp");
        std::fs::create_dir_all(&nested).unwrap();
        assert!(accepted_tmpdir(&nested, Some(&home)).is_none());
        assert!(accepted_tmpdir(&home, Some(&home)).is_none());
        assert!(accepted_tmpdir(Path::new("/"), Some(&home)).is_none());
        if Path::new("/tmp").is_dir() {
            let t = tempfile::Builder::new().prefix("nyq-tmp-").tempdir_in("/tmp").unwrap();
            assert!(accepted_tmpdir(t.path(), Some(&home)).is_some());
            #[cfg(unix)]
            {
                let link = std::env::temp_dir().join(format!("nyq-tmpdir-link-{}", std::process::id()));
                let _ = std::fs::remove_file(&link);
                if std::os::unix::fs::symlink(&nested, &link).is_ok() {
                    assert!(accepted_tmpdir(&link, Some(&home)).is_none());
                    let _ = std::fs::remove_file(&link);
                }
            }
        }
        if Path::new("/var/tmp").is_dir() {
            let t = tempfile::Builder::new().prefix("nyq-tmp-").tempdir_in("/var/tmp").unwrap();
            assert!(accepted_tmpdir(t.path(), Some(&home)).is_some());
        }
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn describe_mentions_the_unix_socket_gap_on_linux() {
        let p = describe(&Policy { mode: Mode::ReadOnly, network: false, ..Default::default() });
        assert!(p.contains("read-only"), "{p}");
        #[cfg(target_os = "linux")]
        if p.contains("landlock") {
            assert!(p.contains("unix sockets not restricted"), "{p}");
        }
    }

    #[test]
    fn off_runs_plain_shell() {
        let c = shell_command(&Policy { mode: Mode::Off, ..Default::default() }, Path::new("/"), "true").unwrap();
        let prog = c.as_std().get_program().to_string_lossy().into_owned();
        assert!(prog == "bash" || prog == "sh" || prog == "cmd", "{prog}");
    }
}
