//! Landlock enforcement through the real `nyq __sandbox` helper (Linux only).
#![cfg(target_os = "linux")]

use std::path::Path;
use std::process::{Command, Output};

fn sandboxed(writable: &[&Path], net: bool, script: &str, cwd: &Path) -> Output {
    let mut c = Command::new(env!("CARGO_BIN_EXE_nyq"));
    c.arg("__sandbox");
    if net {
        c.arg("--net");
    }
    for w in writable {
        c.arg("--write").arg(w);
    }
    c.args(["--", "bash", "-c", script])
        .current_dir(cwd)
        .output()
        .unwrap()
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
