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

/// io_uring does IORING_OP_SOCKET in the kernel without calling socket(),
/// which bypassed the seccomp family check. The filter now fails all three
/// io_uring syscalls with EPERM when the network is off.
#[test]
fn io_uring_is_denied_when_network_is_off() {
    if !landlock_available() || !socket_filter_supported() || !on_path("python3") {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    // io_uring_setup is nr 425 on both x86_64 and aarch64; print the errno
    // (0 when the syscall succeeded).
    let code = "import ctypes,sys\n\
lib = ctypes.CDLL('libc.so.6', use_errno=True)\n\
r = lib.syscall(425, 8, None)\n\
print(ctypes.get_errno() if r == -1 else 0)";
    let probe = ["python3", "-c", code];
    // What the same call does with no filter at all.
    let baseline = Command::new("python3").args(["-c", code]).output().unwrap();
    let blocked = sandboxed_args(&[], false, &[], &probe, dir.path());
    assert_eq!(
        String::from_utf8_lossy(&blocked.stdout).trim(),
        "1", // EPERM
        "io_uring_setup should fail with EPERM: {}",
        String::from_utf8_lossy(&blocked.stderr)
    );
    // With the network on, no filter is installed and the call behaves the
    // same as it does unsandboxed (success, ENOSYS or a sysctl EPERM).
    let allowed = sandboxed_args(&[], true, &[], &probe, dir.path());
    assert_eq!(
        String::from_utf8_lossy(&allowed.stdout).trim(),
        String::from_utf8_lossy(&baseline.stdout).trim(),
        "with --net the kernel answer should reach the command"
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

/// Initialize a repo with one commit at `dir`, outside the sandbox.
fn git_repo_at(dir: &Path) {
    for args in [
        vec!["init", "-q"],
        vec!["config", "user.email", "t@t"],
        vec!["config", "user.name", "t"],
    ] {
        assert!(Command::new("git")
            .args(&args)
            .current_dir(dir)
            .output()
            .unwrap()
            .status
            .success());
    }
    std::fs::write(dir.join("a.txt"), "a").unwrap();
    for args in [vec!["add", "a.txt"], vec!["commit", "-qm", "init"]] {
        assert!(Command::new("git")
            .args(&args)
            .current_dir(dir)
            .output()
            .unwrap()
            .status
            .success());
    }
}

/// `mv .git aside` used to carry the read-only mounts along, letting a
/// fresh writable `.git` take their place. `.git`, the repo dir and every
/// ancestor inside the writable roots are now mount points, so rename and
/// rmdir fail with EBUSY.
#[test]
fn git_dir_cannot_be_moved_aside() {
    if !landlock_available() || !on_path("git") {
        return;
    }
    let repo = tempfile::tempdir().unwrap();
    git_repo_at(repo.path());
    let roots: Vec<&Path> = vec![Path::new("/dev/null"), repo.path()];

    // The classic attack: move .git aside and plant a writable replacement.
    let script = "mv .git .git-old; mkdir -p .git/hooks; echo '#!/bin/sh' > .git/hooks/post-commit";
    let Some(out) = git_protected(&roots, &["bash", "-c", script], repo.path()) else {
        return;
    };
    assert!(!out.status.success());
    assert!(!repo.path().join(".git-old").exists());
    assert!(repo.path().join(".git/config").is_file());
    assert!(!repo.path().join(".git/hooks/post-commit").exists());

    // The repo dir itself is pinned, so the whole repo cannot be swapped
    // out either (writable root is the parent here).
    let parent = tempfile::tempdir().unwrap();
    let inner = parent.path().join("repo");
    std::fs::create_dir(&inner).unwrap();
    git_repo_at(&inner);
    let proots: Vec<&Path> = vec![Path::new("/dev/null"), parent.path()];
    let script = format!(
        "mv '{}' '{}'",
        inner.display(),
        parent.path().join("aside").display()
    );
    let out = git_protected(&proots, &["bash", "-c", &script], &inner).unwrap();
    assert!(
        !out.status.success(),
        "moving the repo aside should fail with EBUSY: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(inner.join(".git/config").is_file());
    assert!(!parent.path().join("aside").exists());
}

/// Commits, branches and stashes write inside `.git` — the pins and
/// read-only binds must leave that working.
#[test]
fn git_commit_branch_and_stash_still_work() {
    if !landlock_available() || !on_path("git") {
        return;
    }
    let repo = tempfile::tempdir().unwrap();
    git_repo_at(repo.path());
    let roots: Vec<&Path> = vec![Path::new("/dev/null"), repo.path()];
    let script = "echo b >> a.txt && git add a.txt && git commit -qm two \
                  && git checkout -qb side && echo c >> a.txt && git stash -q \
                  && git stash list && git log --oneline -1";
    let Some(out) = git_protected(&roots, &["bash", "-c", script], repo.path()) else {
        return;
    };
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("two"), "{stdout}");
    assert!(stdout.contains("stash@{0}"), "{stdout}");
}

/// Worktree `.git` files, submodule gitdirs under `.git/modules`, the
/// `commondir` back-pointer and a configured `core.hooksPath` all get the
/// same protection as `.git` itself.
#[test]
fn worktree_submodule_and_hooks_path_are_protected() {
    if !landlock_available() || !on_path("git") {
        return;
    }
    let base = tempfile::tempdir().unwrap();
    let main = base.path().join("main");
    std::fs::create_dir(&main).unwrap();
    git_repo_at(&main);
    assert!(Command::new("git")
        .args(["worktree", "add", "-q", "../wt"])
        .current_dir(&main)
        .output()
        .unwrap()
        .status
        .success());
    let wt = base.path().join("wt");
    let wt_gd = std::fs::read_dir(main.join(".git/worktrees"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    // A submodule's gitdir (the layout git itself uses).
    let module = main.join(".git/modules/sub");
    std::fs::create_dir_all(module.join("hooks")).unwrap();
    std::fs::write(module.join("config"), "").unwrap();
    // A hooksPath pointing into the workspace.
    assert!(Command::new("git")
        .args(["config", "core.hooksPath", "myhooks"])
        .current_dir(&main)
        .output()
        .unwrap()
        .status
        .success());
    let roots: Vec<&Path> = vec![Path::new("/dev/null"), base.path()];

    // The worktree's `.git` pointer file cannot be renamed away.
    let Some(out) = git_protected(&roots, &["bash", "-c", "mv .git .git2"], &wt) else {
        return;
    };
    assert!(!out.status.success(), ".git file should be pinned");

    // Submodule metadata is read-only and its dir cannot be moved.
    for script in [
        "echo x > .git/modules/sub/hooks/evil",
        "echo x > .git/modules/sub/config",
        "mv .git/modules .git/modules-old",
    ] {
        let out = git_protected(&roots, &["bash", "-c", script], &main).unwrap();
        assert!(!out.status.success(), "{script} should be denied");
    }
    assert!(!module.join("hooks/evil").exists());
    assert!(!main.join(".git/modules-old").exists());

    // The worktree gitdir's commondir back-pointer is read-only too.
    let rel = wt_gd.strip_prefix(&main).unwrap().join("commondir");
    let script = format!("echo ../.. > '{}'", rel.display());
    let out = git_protected(&roots, &["bash", "-c", &script], &main).unwrap();
    assert!(!out.status.success(), "commondir write should be denied");

    // The hooksPath dir was materialized read-only and pinned.
    for script in ["echo x > myhooks/evil", "mv myhooks myhooks-old"] {
        let out = git_protected(&roots, &["bash", "-c", script], &main).unwrap();
        assert!(!out.status.success(), "{script} should be denied");
    }
    assert!(!main.join("myhooks/evil").exists());
    assert!(!main.join("myhooks-old").exists());
    // Reading the worktree pointer still works.
    let out = git_protected(&roots, &["git", "status", "--porcelain"], &wt).unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A symlinked `.git` or `.git/hooks` gets its *target* protected: the
/// resolved gitdir is pinned and its metadata read-only, and writes through
/// the link are denied. The link itself is not a mount point and can still
/// be renamed aside — an accepted gap that the helper warns about.
#[test]
fn symlinked_git_and_hooks_targets_are_protected() {
    if !landlock_available() || !on_path("git") {
        return;
    }
    let base = tempfile::tempdir().unwrap();
    // A gitdir reached through a .git symlink.
    let real = base.path().join("realgit");
    std::fs::create_dir_all(real.join("hooks")).unwrap();
    std::fs::write(real.join("config"), "").unwrap();
    let repo = base.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    std::os::unix::fs::symlink(&real, repo.join(".git")).unwrap();
    let roots: Vec<&Path> = vec![Path::new("/dev/null"), base.path()];

    // Writes through the link land on the read-only target.
    let Some(out) = git_protected(&roots, &["bash", "-c", "echo x > .git/hooks/p"], &repo) else {
        return;
    };
    assert!(!out.status.success(), "write through link should be denied");
    assert!(!real.join("hooks/p").exists());
    // The link is reported as replaceable.
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("is a symlink"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // The real gitdir itself is pinned.
    let script = format!(
        "mv '{}' '{}'",
        real.display(),
        base.path().join("r2").display()
    );
    let out = git_protected(&roots, &["bash", "-c", &script], &repo).unwrap();
    assert!(!out.status.success(), "target dir should be pinned");
    assert!(!base.path().join("r2").exists());
    // The link can still be renamed — known limitation, warned above.
    let out = git_protected(&roots, &["bash", "-c", "mv .git l2"], &repo).unwrap();
    assert!(out.status.success());
    assert!(repo.join("l2").is_symlink());

    // A hooks dir that is a symlink: writes through it are denied and the
    // real target cannot be moved either.
    let repo2 = base.path().join("repo2");
    std::fs::create_dir(&repo2).unwrap();
    git_repo_at(&repo2);
    let hooks_target = base.path().join("hooks-target");
    std::fs::rename(repo2.join(".git/hooks"), &hooks_target).unwrap();
    std::os::unix::fs::symlink(&hooks_target, repo2.join(".git/hooks")).unwrap();
    for script in [
        "echo x > .git/hooks/planted",
        "mv ../hooks-target ../hooks-target2",
    ] {
        let out = git_protected(&roots, &["bash", "-c", script], &repo2).unwrap();
        assert!(!out.status.success(), "{script} should be denied");
    }
    assert!(!hooks_target.join("planted").exists());
    assert!(!base.path().join("hooks-target2").exists());
}

/// `--git-probe` reports whether the mount protection applies (it feeds the
/// `partial` enforcement status) and `--git-strict` would refuse the
/// command instead of warning.
#[test]
fn git_probe_reports_protection_and_strict_mode_runs() {
    if !landlock_available() {
        return;
    }
    let repo = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(repo.path().join(".git/hooks")).unwrap();
    std::fs::write(repo.path().join(".git/config"), "").unwrap();
    let roots: Vec<&Path> = vec![repo.path()];
    let out = sandboxed_args(&roots, false, &["--git-probe"], &["true"], repo.path());
    let stderr = String::from_utf8_lossy(&out.stderr);
    if !out.status.success() {
        eprintln!("skipping: .git protection unavailable here: {stderr}");
        return;
    }
    assert!(stderr.contains("git protection available"), "{stderr}");
    // With protection applying, strict mode runs commands normally.
    let out = sandboxed_args(&roots, false, &["--git-strict"], &["true"], repo.path());
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
