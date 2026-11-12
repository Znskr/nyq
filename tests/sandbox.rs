//! Landlock enforcement through the real `nyq __sandbox` helper (Linux only).
#![cfg(target_os = "linux")]

use std::path::Path;
use std::process::{Command, Output};

fn sandboxed(writable: &[&Path], net: bool, script: &str, cwd: &Path) -> Output {
    sandboxed_args(writable, net, &[], &["bash", "-c", script], cwd)
}

fn sandboxed_args(
    writable: &[&Path],
    net: bool,
    flags: &[&str],
    argv: &[&str],
    cwd: &Path,
) -> Output {
    let mut c = Command::new(env!("CARGO_BIN_EXE_nyq"));
    c.arg("__sandbox");
    if net {
        c.arg("--net");
    }
    c.args(flags);
    for w in writable {
        c.arg("--write").arg(w);
    }
    c.arg("--").args(argv).current_dir(cwd).output().unwrap()
}

fn on_path(name: &str) -> bool {
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .any(|d| d.join(name).is_file())
}

fn landlock_available() -> bool {
    // The helper refuses to run unconfined, so a trivial command tells us.
    let dir = tempfile::tempdir().unwrap();
    let ok = sandboxed(&[], false, "true", dir.path()).status.success();
    if !ok {
        eprintln!("skipping: Landlock unavailable");
    }
    ok
}

#[test]
fn writes_only_inside_allowed_roots() {
    if !landlock_available() {
        return;
    }
    let work = tempfile::tempdir().unwrap();
    // A directory outside /tmp, so it is not covered by the scratch rules.
    let outside = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let script = format!(
        "echo hi > inside.txt && echo no > '{}/outside.txt'",
        outside.path().display()
    );
    let out = sandboxed(&[work.path()], false, &script, work.path());
    assert!(!out.status.success());
    assert_eq!(
        std::fs::read_to_string(work.path().join("inside.txt")).unwrap(),
        "hi\n"
    );
    assert!(!outside.path().join("outside.txt").exists());
    assert!(String::from_utf8_lossy(&out.stderr).contains("Permission denied"));

    // Reading outside is fine.
    std::fs::write(outside.path().join("r.txt"), "readable").unwrap();
    let out = sandboxed(
        &[work.path()],
        false,
        &format!("cat '{}/r.txt'", outside.path().display()),
        work.path(),
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), "readable");
}

#[test]
fn read_only_mode_blocks_workspace_writes() {
    if !landlock_available() {
        return;
    }
    let work = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let out = sandboxed(&[], false, "echo x > f.txt", work.path());
    assert!(!out.status.success());
    assert!(!work.path().join("f.txt").exists());
}

#[test]
fn network_is_blocked_unless_allowed() {
    if !landlock_available() {
        return;
    }
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let dir = tempfile::tempdir().unwrap();
    let probe = format!("exec 3<>/dev/tcp/127.0.0.1/{port}");
    let blocked = sandboxed(&[], false, &probe, dir.path());
    let allowed = sandboxed(&[], true, &probe, dir.path());
    assert!(
        allowed.status.success(),
        "{}",
        String::from_utf8_lossy(&allowed.stderr)
    );
    // Kernels before Landlock ABI v4 cannot restrict TCP.
    let abi4 = std::fs::read_to_string("/sys/kernel/security/lsm")
        .map_or(true, |l| l.contains("landlock"));
    if abi4 {
        assert!(!blocked.status.success(), "connect should be denied");
    }
}

/// The seccomp socket filter is only built for these architectures.
fn socket_filter_supported() -> bool {
    cfg!(any(target_arch = "x86_64", target_arch = "aarch64"))
}

#[test]
fn udp_socket_is_blocked_unless_network_allowed() {
    if !landlock_available() || !socket_filter_supported() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    // bash's /dev/udp uses socket(AF_INET, SOCK_DGRAM): Landlock never
    // covered UDP; the seccomp filter fails socket() with EACCES.
    let blocked = sandboxed(&[], false, "exec 3<>/dev/udp/127.0.0.1/53", dir.path());
    assert!(
        !blocked.status.success(),
        "UDP socket should be denied: {}",
        String::from_utf8_lossy(&blocked.stdout)
    );
    let out = sandboxed(&[], true, "exec 3<>/dev/udp/127.0.0.1/53", dir.path());
    assert!(
        out.status.success(),
        "UDP socket should be allowed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn unix_socket_is_blocked_unless_explicitly_allowed() {
    if !landlock_available() || !socket_filter_supported() || !on_path("python3") {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let probe = [
        "python3",
        "-c",
        "import socket; socket.socket(socket.AF_UNIX)",
    ];
    let blocked = sandboxed_args(&[], false, &[], &probe, dir.path());
    assert!(!blocked.status.success(), "AF_UNIX should be denied");
    // An explicit setting re-allows AF_UNIX while INET families stay shut.
    let allowed = sandboxed_args(&[], false, &["--unix"], &probe, dir.path());
    assert!(
        allowed.status.success(),
        "{}",
        String::from_utf8_lossy(&allowed.stderr)
    );
}

fn git_protected(writable: &[&Path], argv: &[&str], cwd: &Path) -> Option<Output> {
    let out = sandboxed_args(writable, false, &[], argv, cwd);
    let stderr = String::from_utf8_lossy(&out.stderr);
    if stderr.contains(".git metadata stays writable") {
        eprintln!("skipping: user namespaces unavailable");
        return None;
    }
    Some(out)
}

#[test]
fn git_hooks_and_config_are_read_only() {
    if !landlock_available() || !on_path("git") {
        return;
    }
    let repo = tempfile::tempdir().unwrap();
    for args in [
        vec!["init", "-q"],
        vec!["config", "user.email", "t@t"],
        vec!["config", "user.name", "t"],
    ] {
        assert!(Command::new("git")
            .args(&args)
            .current_dir(repo.path())
            .output()
            .unwrap()
            .status
            .success());
    }
    let roots: Vec<&Path> = vec![Path::new("/dev/null"), repo.path()];

    let hook = git_protected(
        &roots,
        &["bash", "-c", "echo '#!/bin/sh' > .git/hooks/post-commit"],
        repo.path(),
    );
    let Some(hook) = hook else { return };
    assert!(!hook.status.success(), "hook write should be denied");
    assert!(!repo.path().join(".git/hooks/post-commit").exists());

    let config = git_protected(
        &roots,
        &["git", "config", "--local", "core.hooksPath", "/tmp/evil"],
        repo.path(),
    )
    .unwrap();
    assert!(!config.status.success(), "config write should be denied");
    assert!(!std::fs::read_to_string(repo.path().join(".git/config"))
        .unwrap()
        .contains("hooksPath"));

    // The rest of .git stays writable: staging and committing still work.
    std::fs::write(repo.path().join("a.txt"), "a").unwrap();
    let commit = git_protected(
        &roots,
        &[
            "bash",
            "-c",
            "git add a.txt && git commit -qm init && git log --oneline",
        ],
        repo.path(),
    )
    .unwrap();
    assert!(
        commit.status.success(),
        "{}",
        String::from_utf8_lossy(&commit.stderr)
    );
    assert!(String::from_utf8_lossy(&commit.stdout).contains("init"));
}

#[test]
fn git_writable_flag_opts_out() {
    if !landlock_available() || !on_path("git") {
        return;
    }
    let repo = tempfile::tempdir().unwrap();
    Command::new("git")
        .args(["init", "-q"])
        .current_dir(repo.path())
        .output()
        .unwrap();
    let roots: Vec<&Path> = vec![repo.path()];
    let out = sandboxed_args(
        &roots,
        false,
        &["--git-writable"],
        &["bash", "-c", "echo x > .git/hooks/post-commit"],
        repo.path(),
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
