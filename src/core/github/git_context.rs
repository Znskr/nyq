//! Git repository snapshot helper (ported 1:1 from
//! zig-reference/src/core/github/git_context.zig).

use std::process::Command;

/// Owned snapshot of the current Git repository state.
pub struct Snapshot {
    pub in_git_repo: bool,
    pub text: Vec<u8>,
}

/// Capture a best-effort Git snapshot of the current directory.
pub fn snapshot() -> Snapshot {
    let in_git_repo = is_git_repository();
    let branch = run_git(&[b"branch", b"--show-current"]);
    let status = run_git(&[b"status", b"--short", b"--branch"]);
    let log = run_git(&[b"log", b"--oneline", b"-5"]);
    let staged = run_git(&[b"diff", b"--stat", b"--cached"]);
    let unstaged = run_git(&[b"diff", b"--stat"]);

    Snapshot {
        in_git_repo,
        text: format_snapshot(
            branch.as_deref(),
            status.as_deref(),
            log.as_deref(),
            staged.as_deref(),
            unstaged.as_deref(),
        ),
    }
}

fn build_git_argv(args: &[&[u8]]) -> Vec<Vec<u8>> {
    let mut argv: Vec<Vec<u8>> = Vec::new();
    argv.push(b"git".to_vec());
    argv.push(b"--no-optional-locks".to_vec());
    for arg in args {
        argv.push(arg.to_vec());
    }
    argv
}

/// Returns true when the current directory is inside a Git worktree.
/// Any failure to run or parse Git is treated as unavailable.
pub fn is_git_repository() -> bool {
    inside_work_tree(run_git(&[b"rev-parse", b"--is-inside-work-tree"]).as_deref())
}

fn inside_work_tree(result: Option<&[u8]>) -> bool {
    result == Some(b"true".as_slice())
}

fn run_git(args: &[&[u8]]) -> Option<Vec<u8>> {
    let argv = build_git_argv(args);
    let output = run_argv(&argv).ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = output.stdout;
    let trimmed = trim(&stdout);
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.len() == stdout.len() {
        Some(stdout)
    } else {
        Some(trimmed.to_vec())
    }
}

fn run_argv(argv: &[Vec<u8>]) -> std::io::Result<std::process::Output> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let mut cmd = Command::new(std::ffi::OsStr::from_bytes(&argv[0]));
        for arg in &argv[1..] {
            cmd.arg(std::ffi::OsStr::from_bytes(arg));
        }
        return cmd.output();
    }
    #[cfg(not(unix))]
    {
        let mut cmd = Command::new(std::ffi::OsString::from(
            String::from_utf8_lossy(&argv[0]).into_owned(),
        ));
        for arg in &argv[1..] {
            cmd.arg(std::ffi::OsString::from(
                String::from_utf8_lossy(arg).into_owned(),
            ));
        }
        return cmd.output();
    }
}

fn format_snapshot(
    branch: Option<&[u8]>,
    status: Option<&[u8]>,
    log: Option<&[u8]>,
    staged: Option<&[u8]>,
    unstaged: Option<&[u8]>,
) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();
    out.extend_from_slice(b"Git snapshot\n");
    out.extend_from_slice(b"Branch: ");
    out.extend_from_slice(branch.unwrap_or(b"unavailable"));
    out.extend_from_slice(b"\n\nStatus:\n");
    write_body(&mut out, status, b"unavailable");
    out.extend_from_slice(b"\nRecent commits:\n");
    write_body(&mut out, log, b"unavailable");
    out.extend_from_slice(b"\nStaged diff stat:\n");
    write_body(&mut out, staged, b"none");
    out.extend_from_slice(b"\nUnstaged diff stat:\n");
    write_body(&mut out, unstaged, b"none");
    out
}

fn write_body(out: &mut Vec<u8>, value: Option<&[u8]>, fallback: &[u8]) {
    let body = value.unwrap_or(fallback);
    out.extend_from_slice(body);
    if body.is_empty() || body[body.len() - 1] != b'\n' {
        out.push(b'\n');
    }
}

fn trim(mut bytes: &[u8]) -> &[u8] {
    while bytes.first().is_some_and(|b| matches!(b, b' ' | b'\t' | b'\r' | b'\n')) {
        bytes = &bytes[1..];
    }
    while bytes.last().is_some_and(|b| matches!(b, b' ' | b'\t' | b'\r' | b'\n')) {
        bytes = &bytes[..bytes.len() - 1];
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_formatting_null_git_values_match_core_fallback_text_exactly() {
        let text = format_snapshot(None, None, None, None, None);
        assert_eq!(
            text,
            b"Git snapshot\nBranch: unavailable\n\nStatus:\nunavailable\n\nRecent commits:\nunavailable\n\nStaged diff stat:\nnone\n\nUnstaged diff stat:\nnone\n"
        );
    }

    #[test]
    fn snapshot_formatting_populated_sections_preserve_layout_exactly() {
        let text = format_snapshot(
            Some(b"main"),
            Some(b"## main\n M src/main.zig"),
            Some(b"abc123 first\n"),
            Some(b" src/main.zig | 2 ++"),
            Some(b" src/core/github/git_context.zig | 5 +++++"),
        );
        assert_eq!(
            text,
            b"Git snapshot\nBranch: main\n\nStatus:\n## main\n M src/main.zig\n\nRecent commits:\nabc123 first\n\nStaged diff stat:\n src/main.zig | 2 ++\n\nUnstaged diff stat:\n src/core/github/git_context.zig | 5 +++++\n"
        );
    }

    #[test]
    fn git_argv_read_only_commands_disable_optional_locks() {
        let argv = build_git_argv(&[b"status", b"--short", b"--branch"]);
        assert_eq!(argv.len(), 5);
        assert_eq!(argv[0], b"git");
        assert_eq!(argv[1], b"--no-optional-locks");
        assert_eq!(argv[2], b"status");
        assert_eq!(argv[3], b"--short");
        assert_eq!(argv[4], b"--branch");
    }

    #[test]
    fn git_repository_detection_treats_git_failure_as_unavailable() {
        // The Zig reference drives this through a FailingAllocator; Rust cannot
        // inject an allocation failure, so the equivalent contract is exercised
        // through the decision helper: any failed run must yield `false`.
        assert!(!inside_work_tree(None));
        assert!(!inside_work_tree(Some(b"false")));
        assert!(inside_work_tree(Some(b"true")));
    }

    #[test]
    fn snapshot_returns_owned_git_status_text() {
        let state = snapshot();
        assert!(state.text.windows(b"Git snapshot".len()).any(|w| w == b"Git snapshot"));
        assert!(state.text.windows(b"Branch:".len()).any(|w| w == b"Branch:"));
    }
}
