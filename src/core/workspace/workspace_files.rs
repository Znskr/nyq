//! Workspace file and directory discovery.
//!
//! Prefers a trusted `git ls-files` listing when available and falls back to a
//! bounded recursive walk. Cancellation is cooperative: a caller-owned atomic
//! flag is polled between directory entries and while a Git child is running.

use std::borrow::Cow;
use std::collections::HashSet;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::core::shared::debug_trace;
use crate::core::workspace::ignored_dirs;

pub const default_candidate_cap: usize = 100_000;
pub const default_git_stdout_limit: usize = 32 * 1024 * 1024;
pub const max_relative_path_bytes: usize = 2048;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Source {
    Git,
    Recursive,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CapReason {
    CandidateCap,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Cancelled,
    GitFileListFailed,
}

type IoResult<T> = std::result::Result<T, Error>;

#[derive(Clone)]
pub struct Options {
    pub candidate_cap: usize,
    pub git_stdout_limit: usize,
    pub ignored_names: Vec<&'static [u8]>,
    pub force_fallback: bool,
    pub include_untracked: bool,
    pub only_untracked: bool,
    pub include_hidden: bool,
    pub sort_paths: bool,
    pub git_worktree_is_authoritative: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            candidate_cap: default_candidate_cap,
            git_stdout_limit: default_git_stdout_limit,
            ignored_names: ignored_dirs::ignored_directory_names
                .iter()
                .map(|name| name.as_bytes())
                .collect(),
            force_fallback: false,
            include_untracked: false,
            only_untracked: false,
            include_hidden: false,
            sort_paths: false,
            git_worktree_is_authoritative: false,
        }
    }
}

#[derive(Debug)]
pub struct Result {
    pub files: Vec<Vec<u8>>,
    pub source: Source,
    pub candidate_cap: usize,
    pub incomplete: bool,
    pub cap_reason: Option<CapReason>,
    pub skipped_overlong: usize,
}

#[derive(Debug)]
pub struct DirectoryResult {
    pub directories: Vec<Vec<u8>>,
    pub source: Source,
    pub candidate_cap: usize,
    pub incomplete: bool,
    pub cap_reason: Option<CapReason>,
    pub skipped_overlong: usize,
}

pub fn discover(workspace_root: &[u8], options: &Options) -> IoResult<Result> {
    discover_with_stop(workspace_root, options, None, trusted_git_executable())
}

pub fn discover_cancellable(
    workspace_root: &[u8],
    options: &Options,
    stop_requested: &AtomicBool,
) -> IoResult<Result> {
    discover_with_stop(
        workspace_root,
        options,
        Some(stop_requested),
        trusted_git_executable(),
    )
}

pub fn discover_directories_cancellable(
    workspace_root: &[u8],
    options: &Options,
    stop_requested: &AtomicBool,
) -> IoResult<DirectoryResult> {
    discover_directories_with_stop(
        workspace_root,
        options,
        Some(stop_requested),
        trusted_git_executable(),
    )
}

fn discover_with_stop(
    workspace_root: &[u8],
    options: &Options,
    stop_requested: Option<&AtomicBool>,
    git_executable: Option<&[u8]>,
) -> IoResult<Result> {
    check_canceled(stop_requested)?;
    if workspace_root.is_empty() {
        return Ok(Result {
            files: Vec::new(),
            source: Source::Recursive,
            candidate_cap: options.candidate_cap,
            incomplete: false,
            cap_reason: None,
            skipped_overlong: 0,
        });
    }

    if !options.force_fallback {
        match git_executable {
            Some(executable) => match git_raw_list(workspace_root, options, stop_requested, executable)
            {
                Ok(raw) => {
                    let parsed = parse_raw_list(&raw, Source::Git, options);
                    if options.only_untracked
                        || options.git_worktree_is_authoritative
                        || !parsed.files.is_empty()
                        || parsed.incomplete
                        || parsed.skipped_overlong > 0
                    {
                        return Ok(parsed);
                    }
                    debug_trace::logf(
                        "core",
                        format_args!(
                            "workspace file discovery git returned no candidates; falling back root={}",
                            String::from_utf8_lossy(workspace_root)
                        ),
                    );
                }
                Err(Error::Cancelled) => return Err(Error::Cancelled),
                Err(_) => {
                    if options.git_worktree_is_authoritative && has_git_metadata(workspace_root)? {
                        return Err(Error::GitFileListFailed);
                    }
                    debug_trace::logf(
                        "core",
                        format_args!(
                            "workspace file discovery falling back root={} err=git_failed",
                            String::from_utf8_lossy(workspace_root)
                        ),
                    );
                }
            },
            None => {
                debug_trace::logf(
                    "core",
                    format_args!(
                        "workspace file discovery trusted Git unavailable; falling back root={}",
                        String::from_utf8_lossy(workspace_root)
                    ),
                );
            }
        }
    }

    walk_workspace(workspace_root, options, stop_requested)
}

fn discover_directories_with_stop(
    workspace_root: &[u8],
    options: &Options,
    stop_requested: Option<&AtomicBool>,
    git_executable: Option<&[u8]>,
) -> IoResult<DirectoryResult> {
    check_canceled(stop_requested)?;
    if workspace_root.is_empty() {
        return Ok(DirectoryResult {
            directories: Vec::new(),
            source: Source::Recursive,
            candidate_cap: options.candidate_cap,
            incomplete: false,
            cap_reason: None,
            skipped_overlong: 0,
        });
    }

    if !options.force_fallback {
        match git_executable {
            Some(executable) => {
                match git_raw_ignored_directory_list(
                    workspace_root,
                    options,
                    stop_requested,
                    executable,
                ) {
                    Ok(raw) => {
                        let ignored_paths = parse_raw_ignored_directories(&raw);
                        let mut git_options = options.clone();
                        git_options.ignored_names = vec![b".git"];
                        return walk_workspace_directories(
                            workspace_root,
                            &git_options,
                            stop_requested,
                            Some(&ignored_paths),
                            Source::Git,
                        );
                    }
                    Err(Error::Cancelled) => return Err(Error::Cancelled),
                    Err(_) => {
                        if options.git_worktree_is_authoritative && has_git_metadata(workspace_root)? {
                            return Err(Error::GitFileListFailed);
                        }
                        debug_trace::logf(
                            "core",
                            format_args!(
                                "workspace directory discovery falling back root={} err=git_failed",
                                String::from_utf8_lossy(workspace_root)
                            ),
                        );
                    }
                }
            }
            None => {
                debug_trace::logf(
                    "core",
                    format_args!(
                        "workspace directory discovery trusted Git unavailable; falling back root={}",
                        String::from_utf8_lossy(workspace_root)
                    ),
                );
            }
        }
    }

    walk_workspace_directories(workspace_root, options, stop_requested, None, Source::Recursive)
}

fn has_git_metadata(workspace_root: &[u8]) -> IoResult<bool> {
    let Ok(workspace_root) = std::str::from_utf8(workspace_root) else {
        return Ok(false);
    };
    let mut current = Path::new(workspace_root);
    loop {
        if current.as_os_str().is_empty() {
            return Ok(false);
        }
        let marker = current.join(".git");
        match std::fs::symlink_metadata(&marker) {
            Ok(_) => return Ok(true),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(Error::GitFileListFailed),
        }
        let Some(parent) = current.parent() else {
            return Ok(false);
        };
        if parent == current {
            return Ok(false);
        }
        current = parent;
    }
}

fn git_tracked_files_argv(git_executable: &[u8]) -> [&[u8]; 5] {
    [
        git_executable,
        b"--no-optional-locks",
        b"ls-files",
        b"-z",
        b"--cached",
    ]
}

fn git_tracked_and_other_files_argv(git_executable: &[u8]) -> [&[u8]; 7] {
    [
        git_executable,
        b"--no-optional-locks",
        b"ls-files",
        b"-z",
        b"--cached",
        b"--others",
        b"--exclude-standard",
    ]
}

fn git_other_files_argv(git_executable: &[u8]) -> [&[u8]; 6] {
    [
        git_executable,
        b"--no-optional-locks",
        b"ls-files",
        b"-z",
        b"--others",
        b"--exclude-standard",
    ]
}

fn git_ignored_directories_argv(git_executable: &[u8]) -> [&[u8]; 8] {
    [
        git_executable,
        b"--no-optional-locks",
        b"ls-files",
        b"-z",
        b"--others",
        b"--ignored",
        b"--directory",
        b"--exclude-standard",
    ]
}

fn git_raw_list(
    workspace_root: &[u8],
    options: &Options,
    stop_requested: Option<&AtomicBool>,
    git_executable: &[u8],
) -> IoResult<Vec<u8>> {
    let argv: &[&[u8]] = if options.only_untracked {
        &git_other_files_argv(git_executable)
    } else if options.include_untracked {
        &git_tracked_and_other_files_argv(git_executable)
    } else {
        &git_tracked_files_argv(git_executable)
    };
    run_git_raw_list(workspace_root, argv, options.git_stdout_limit, stop_requested)
}

fn git_raw_ignored_directory_list(
    workspace_root: &[u8],
    options: &Options,
    stop_requested: Option<&AtomicBool>,
    git_executable: &[u8],
) -> IoResult<Vec<u8>> {
    let argv = git_ignored_directories_argv(git_executable);
    run_git_raw_list(workspace_root, &argv, options.git_stdout_limit, stop_requested)
}

fn run_git_raw_list(
    workspace_root: &[u8],
    argv: &[&[u8]],
    stdout_limit: usize,
    stop_requested: Option<&AtomicBool>,
) -> IoResult<Vec<u8>> {
    let outcome = run_command(workspace_root, argv, stdout_limit, 1024, stop_requested)
        .map_err(|_| Error::GitFileListFailed)?;
    match outcome {
        CommandOutcome::Cancelled => Err(Error::Cancelled),
        CommandOutcome::Exited {
            code: 0,
            stdout,
            stderr,
        } => {
            if stdout.len() > stdout_limit || stderr.len() > 1024 {
                return Err(Error::GitFileListFailed);
            }
            Ok(stdout)
        }
        _ => Err(Error::GitFileListFailed),
    }
}

enum CommandOutcome {
    Exited {
        code: i32,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
    },
    Cancelled,
}

fn run_command(
    workspace_root: &[u8],
    argv: &[&[u8]],
    stdout_limit: usize,
    stderr_limit: usize,
    stop_requested: Option<&AtomicBool>,
) -> std::io::Result<CommandOutcome> {
    if stop_requested.is_some_and(|s| s.load(Ordering::SeqCst)) {
        return Ok(CommandOutcome::Cancelled);
    }

    let mut cmd = Command::new(os_from_bytes(argv[0]));
    for arg in &argv[1..] {
        cmd.arg(os_from_bytes(arg));
    }
    if !workspace_root.is_empty() {
        cmd.current_dir(os_from_bytes(workspace_root));
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn()?;
    let stdout = child.stdout.take().ok_or_else(invalid_input)?;
    let stderr = child.stderr.take().ok_or_else(invalid_input)?;

    let stdout_handle =
        std::thread::spawn(move || read_capped(stdout, stdout_limit.saturating_add(1)));
    let stderr_handle =
        std::thread::spawn(move || read_capped(stderr, stderr_limit.saturating_add(1)));

    loop {
        if stop_requested.is_some_and(|s| s.load(Ordering::SeqCst)) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = stdout_handle.join();
            let _ = stderr_handle.join();
            return Ok(CommandOutcome::Cancelled);
        }
        match child.try_wait()? {
            Some(status) => {
                let stdout = stdout_handle.join().map_err(|_| invalid_input())??;
                let stderr = stderr_handle.join().map_err(|_| invalid_input())??;
                return Ok(CommandOutcome::Exited {
                    code: status.code().unwrap_or(-1),
                    stdout,
                    stderr,
                });
            }
            None => std::thread::sleep(Duration::from_millis(1)),
        }
    }
}

fn read_capped<R: Read>(mut reader: R, cap: usize) -> std::io::Result<Vec<u8>> {
    let mut buf = [0u8; 8192];
    let mut out = Vec::new();
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        if out.len() < cap {
            let take = (cap - out.len()).min(n);
            out.extend_from_slice(&buf[..take]);
        }
    }
    Ok(out)
}

fn check_canceled(stop_requested: Option<&AtomicBool>) -> IoResult<()> {
    if stop_requested.is_some_and(|s| s.load(Ordering::SeqCst)) {
        return Err(Error::Cancelled);
    }
    Ok(())
}

fn parse_raw_list(raw: &[u8], source: Source, options: &Options) -> Result {
    let mut files: Vec<Vec<u8>> = Vec::new();
    let mut incomplete = false;
    let mut skipped_overlong = 0usize;
    let sep: u8 = if raw.contains(&0) { 0 } else { b'\n' };

    for entry in raw.split(|&b| b == sep) {
        let trimmed = if sep == b'\n' {
            trim_end(entry, b'\r')
        } else {
            entry
        };
        if trimmed.is_empty() {
            continue;
        }
        if !options.include_hidden && path_contains_hidden_directory_component(trimmed) {
            continue;
        }
        if source != Source::Git && path_contains_ignored_dir(&options.ignored_names, trimmed) {
            continue;
        }
        if trimmed.len() > max_relative_path_bytes {
            skipped_overlong += 1;
            continue;
        }
        if files.len() >= options.candidate_cap {
            incomplete = true;
            break;
        }
        files.push(trimmed.to_vec());
    }
    if options.sort_paths {
        sort_path_list(&mut files);
    }

    Result {
        files,
        source,
        candidate_cap: options.candidate_cap,
        incomplete,
        cap_reason: if incomplete {
            Some(CapReason::CandidateCap)
        } else {
            None
        },
        skipped_overlong,
    }
}

fn parse_raw_ignored_directories(raw: &[u8]) -> HashSet<Vec<u8>> {
    let mut ignored: HashSet<Vec<u8>> = HashSet::new();
    let separator: u8 = if raw.contains(&0) { 0 } else { b'\n' };

    for entry in raw.split(|&b| b == separator) {
        let trimmed = if separator == b'\n' {
            trim_end(entry, b'\r')
        } else {
            entry
        };
        if trimmed.is_empty() || !is_sep(trimmed[trimmed.len() - 1]) {
            continue;
        }

        let mut path_end = trimmed.len();
        while path_end > 0 && is_sep(trimmed[path_end - 1]) {
            path_end -= 1;
        }
        if path_end == 0 {
            continue;
        }
        let path = &trimmed[..path_end];
        if !ignored.contains::<[u8]>(path) {
            ignored.insert(path.to_vec());
        }
    }
    ignored
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WalkTarget {
    Files,
    Directories,
}

struct WalkResult {
    paths: Vec<Vec<u8>>,
    incomplete: bool,
    skipped_overlong: usize,
}

struct Frame {
    entries: std::fs::ReadDir,
    prefix: Vec<u8>,
    retain_prefix: bool,
}

fn walk_workspace(
    workspace_root: &[u8],
    options: &Options,
    stop_requested: Option<&AtomicBool>,
) -> IoResult<Result> {
    let walked = walk_workspace_paths(workspace_root, options, stop_requested, WalkTarget::Files, None)?;
    Ok(Result {
        files: walked.paths,
        source: Source::Recursive,
        candidate_cap: options.candidate_cap,
        incomplete: walked.incomplete,
        cap_reason: if walked.incomplete {
            Some(CapReason::CandidateCap)
        } else {
            None
        },
        skipped_overlong: walked.skipped_overlong,
    })
}

fn walk_workspace_directories(
    workspace_root: &[u8],
    options: &Options,
    stop_requested: Option<&AtomicBool>,
    ignored_paths: Option<&HashSet<Vec<u8>>>,
    source: Source,
) -> IoResult<DirectoryResult> {
    let walked = walk_workspace_paths(
        workspace_root,
        options,
        stop_requested,
        WalkTarget::Directories,
        ignored_paths,
    )?;
    Ok(DirectoryResult {
        directories: walked.paths,
        source,
        candidate_cap: options.candidate_cap,
        incomplete: walked.incomplete,
        cap_reason: if walked.incomplete {
            Some(CapReason::CandidateCap)
        } else {
            None
        },
        skipped_overlong: walked.skipped_overlong,
    })
}

fn walk_workspace_paths(
    workspace_root: &[u8],
    options: &Options,
    stop_requested: Option<&AtomicBool>,
    target: WalkTarget,
    ignored_paths: Option<&HashSet<Vec<u8>>>,
) -> IoResult<WalkResult> {
    check_canceled(stop_requested)?;
    let mut paths: Vec<Vec<u8>> = Vec::new();

    let root = match std::str::from_utf8(workspace_root)
        .ok()
        .and_then(|root| std::fs::read_dir(root).ok())
    {
        Some(root) => root,
        None => {
            return Ok(WalkResult {
                paths,
                incomplete: false,
                skipped_overlong: 0,
            });
        }
    };

    let mut stack: Vec<Frame> = vec![Frame {
        entries: root,
        prefix: Vec::new(),
        retain_prefix: false,
    }];
    let mut incomplete = false;
    let mut skipped_overlong = 0usize;

    while let Some(mut frame) = stack.pop() {
        check_canceled(stop_requested)?;
        let entry = match frame.entries.next() {
            Some(Ok(entry)) => entry,
            Some(Err(_)) | None => continue,
        };

        let name = entry.file_name();
        let name_bytes = os_str_bytes(&name);
        let file_type = entry.file_type().ok();
        let is_dir = file_type.as_ref().is_some_and(|t| t.is_dir());
        let is_file_like = file_type.as_ref().is_some_and(|t| t.is_file() || t.is_symlink());

        let mut push_sub: Option<Frame> = None;

        if is_file_like {
            if target == WalkTarget::Files && !is_ignored_name(&options.ignored_names, &name_bytes) {
                if paths.len() >= options.candidate_cap {
                    incomplete = true;
                    break;
                }
                let rel = join_relative(&frame.prefix, &name_bytes);
                if rel.len() > max_relative_path_bytes {
                    skipped_overlong += 1;
                } else {
                    paths.push(rel);
                }
            }
        } else if is_dir {
            if options.include_hidden || !is_hidden_name(&name_bytes) {
                if !is_ignored_name(&options.ignored_names, &name_bytes) {
                    let new_prefix = join_relative(&frame.prefix, &name_bytes);
                    let mut skip = false;
                    if let Some(ignored) = ignored_paths {
                        if ignored.contains::<[u8]>(&new_prefix) {
                            skip = true;
                        }
                    }
                    if !skip {
                        if target == WalkTarget::Directories && paths.len() >= options.candidate_cap {
                            incomplete = true;
                            break;
                        }
                        if target == WalkTarget::Directories
                            && new_prefix.len() > max_relative_path_bytes
                        {
                            skipped_overlong += 1;
                        } else {
                            let retain_prefix = target == WalkTarget::Directories;
                            if retain_prefix {
                                paths.push(new_prefix.clone());
                            }
                            if let Ok(sub) = std::fs::read_dir(entry.path()) {
                                push_sub = Some(Frame {
                                    entries: sub,
                                    prefix: new_prefix,
                                    retain_prefix,
                                });
                            }
                        }
                    }
                }
            }
        }

        stack.push(frame);
        if let Some(sub) = push_sub {
            stack.push(sub);
        }
    }

    if options.sort_paths {
        sort_path_list(&mut paths);
    }

    Ok(WalkResult {
        paths,
        incomplete,
        skipped_overlong,
    })
}

fn sort_path_list(paths: &mut [Vec<u8>]) {
    paths.sort();
}

fn join_relative(prefix: &[u8], name: &[u8]) -> Vec<u8> {
    if prefix.is_empty() {
        return name.to_vec();
    }
    let mut out = Vec::with_capacity(prefix.len() + 1 + name.len());
    out.extend_from_slice(prefix);
    out.push(b'/');
    out.extend_from_slice(name);
    out
}

pub fn path_contains_ignored_dir(ignored: &[&[u8]], path: &[u8]) -> bool {
    path.split(|&b| b == b'/')
        .any(|component| is_ignored_name(ignored, component))
}

pub fn path_contains_hidden_directory_component(path: &[u8]) -> bool {
    let mut rest = path;
    while let Some(slash) = rest.iter().position(|&b| b == b'/') {
        let component = &rest[..slash];
        if is_hidden_name(component) {
            return true;
        }
        rest = &rest[slash + 1..];
    }
    false
}

pub fn is_hidden_name(name: &[u8]) -> bool {
    name.len() > 1 && name[0] == b'.'
}

fn is_ignored_name(ignored: &[&[u8]], name: &[u8]) -> bool {
    ignored.iter().any(|entry| entry == &name)
}

fn is_sep(byte: u8) -> bool {
    byte == b'/' || (cfg!(windows) && byte == b'\\')
}

fn trim_end(mut bytes: &[u8], pat: u8) -> &[u8] {
    while bytes.last() == Some(&pat) {
        bytes = &bytes[..bytes.len() - 1];
    }
    bytes
}

#[cfg(unix)]
fn os_str_bytes(value: &std::ffi::OsStr) -> Cow<'_, [u8]> {
    use std::os::unix::ffi::OsStrExt;
    Cow::Borrowed(value.as_bytes())
}

#[cfg(not(unix))]
fn os_str_bytes(value: &std::ffi::OsStr) -> Cow<'_, [u8]> {
    Cow::Owned(value.to_string_lossy().as_bytes().to_vec())
}

#[cfg(unix)]
fn os_from_bytes(bytes: &[u8]) -> std::ffi::OsString {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::OsStr::from_bytes(bytes).to_os_string()
}

#[cfg(not(unix))]
fn os_from_bytes(bytes: &[u8]) -> std::ffi::OsString {
    std::ffi::OsString::from(String::from_utf8_lossy(bytes).into_owned())
}

fn invalid_input() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, "non-utf8 path")
}

#[cfg(not(any(windows, target_os = "wasi")))]
fn trusted_git_executable() -> Option<&'static [u8]> {
    const CANDIDATES: &[&[u8]] = &[
        b"/usr/bin/git",
        b"/bin/git",
        b"/usr/local/bin/git",
        b"/opt/homebrew/bin/git",
        b"/opt/local/bin/git",
        b"/run/current-system/sw/bin/git",
    ];
    for candidate in CANDIDATES {
        let Ok(candidate_str) = std::str::from_utf8(candidate) else {
            continue;
        };
        if let Ok(meta) = std::fs::metadata(Path::new(candidate_str)) {
            if meta.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(windows)]
fn trusted_git_executable() -> Option<&'static [u8]> {
    const CANDIDATES: &[&[u8]] = &[
        b"C:\\Program Files\\Git\\cmd\\git.exe",
        b"C:\\Program Files\\Git\\bin\\git.exe",
    ];
    for candidate in CANDIDATES {
        let Ok(candidate_str) = std::str::from_utf8(candidate) else {
            continue;
        };
        if let Ok(meta) = std::fs::metadata(Path::new(candidate_str)) {
            if meta.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(target_os = "wasi")]
fn trusted_git_executable() -> Option<&'static [u8]> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::Instant;

    fn temp_root(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "nyq-workspace-files-{}-{}",
            name,
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn write_test_file(root: &Path, rel: &str, content: &[u8]) {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    fn contains_path(files: &[Vec<u8>], needle: &[u8]) -> bool {
        files.iter().any(|path| path.as_slice() == needle)
    }

    fn git_available() -> bool {
        Command::new("git")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn run_git_for_test(cwd: &Path, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .unwrap();
        assert!(output.status.success(), "git {:?} failed", args);
    }

    #[test]
    fn workspace_file_provider_tracked_git_argv_uses_nul_separated_cached_files() {
        let argv = git_tracked_files_argv(b"git");
        assert_eq!(b"git", argv[0]);
        assert_eq!(b"--no-optional-locks", argv[1]);
        assert_eq!(b"ls-files", argv[2]);
        assert_eq!(b"-z", argv[3]);
        assert_eq!(b"--cached", argv[4]);
    }

    #[test]
    fn workspace_file_provider_optional_untracked_git_argv_includes_exclude_standard() {
        let argv = git_tracked_and_other_files_argv(b"git");
        assert_eq!(b"git", argv[0]);
        assert_eq!(b"--no-optional-locks", argv[1]);
        assert_eq!(b"ls-files", argv[2]);
        assert_eq!(b"-z", argv[3]);
        assert_eq!(b"--cached", argv[4]);
        assert_eq!(b"--others", argv[5]);
        assert_eq!(b"--exclude-standard", argv[6]);
    }

    #[test]
    fn workspace_file_provider_untracked_git_argv_uses_others_exclude_standard() {
        let argv = git_other_files_argv(b"git");
        assert_eq!(b"git", argv[0]);
        assert_eq!(b"--no-optional-locks", argv[1]);
        assert_eq!(b"ls-files", argv[2]);
        assert_eq!(b"-z", argv[3]);
        assert_eq!(b"--others", argv[4]);
        assert_eq!(b"--exclude-standard", argv[5]);
    }

    #[test]
    fn workspace_directory_provider_asks_git_only_for_ignored_directory_roots() {
        let argv = git_ignored_directories_argv(b"git");
        assert_eq!(b"git", argv[0]);
        assert_eq!(b"--no-optional-locks", argv[1]);
        assert_eq!(b"ls-files", argv[2]);
        assert_eq!(b"-z", argv[3]);
        assert_eq!(b"--others", argv[4]);
        assert_eq!(b"--ignored", argv[5]);
        assert_eq!(b"--directory", argv[6]);
        assert_eq!(b"--exclude-standard", argv[7]);
    }

    #[test]
    fn workspace_directory_provider_parses_only_ignored_directory_entries() {
        let ignored = parse_raw_ignored_directories(
            b"ignored-dir/\x00ignored.txt\x00nested/ignored/\x00",
        );
        assert!(ignored.contains::<[u8]>(b"ignored-dir"));
        assert!(ignored.contains::<[u8]>(b"nested/ignored"));
        assert!(!ignored.contains::<[u8]>(b"ignored.txt"));
    }

    #[test]
    fn workspace_file_provider_caps_candidates_and_reports_incomplete_status() {
        let raw = b"a.zig\x00b.zig\x00c.zig\x00";
        let result = parse_raw_list(
            raw,
            Source::Git,
            &Options {
                candidate_cap: 2,
                ..Options::default()
            },
        );

        assert_eq!(2, result.files.len());
        assert!(result.incomplete);
        assert_eq!(Some(CapReason::CandidateCap), result.cap_reason);
        assert_eq!(b"a.zig", result.files[0].as_slice());
        assert_eq!(b"b.zig", result.files[1].as_slice());
    }

    #[test]
    fn workspace_file_provider_applies_bytewise_ordering_to_git_results_when_requested() {
        let result = parse_raw_list(
            b"z.zig\x00a.zig\x00m.zig\x00",
            Source::Git,
            &Options {
                sort_paths: true,
                ..Options::default()
            },
        );

        assert_eq!(b"a.zig", result.files[0].as_slice());
        assert_eq!(b"m.zig", result.files[1].as_slice());
        assert_eq!(b"z.zig", result.files[2].as_slice());
    }

    #[test]
    fn workspace_file_provider_git_discovery_includes_tracked_build_files_and_skips_hidden_directories() {
        let raw =
            b"build/script.ts\x00.eslint-plugin-local/rule.ts\x00src/.esbuild.ts\x00src/main.ts\x00";
        let result = parse_raw_list(raw, Source::Git, &Options::default());

        assert_eq!(3, result.files.len());
        assert!(contains_path(&result.files, b"build/script.ts"));
        assert!(contains_path(&result.files, b"src/.esbuild.ts"));
        assert!(contains_path(&result.files, b"src/main.ts"));
        assert!(!contains_path(&result.files, b".eslint-plugin-local/rule.ts"));
    }

    #[test]
    fn workspace_file_provider_recursive_fallback_skips_ignored_directories_and_includes_nested_files() {
        let root = temp_root("fallback-files");
        write_test_file(&root, "src/main.zig", b"main\n");
        write_test_file(&root, "src/nested/lib.zig", b"lib\n");
        write_test_file(&root, "node_modules/pkg/ignored.zig", b"ignored\n");
        write_test_file(&root, ".zig-cache/o/ignored.o", b"ignored\n");

        let workspace = std::fs::canonicalize(&root).unwrap();
        let result = discover(
            workspace.to_str().unwrap().as_bytes(),
            &Options {
                force_fallback: true,
                ..Options::default()
            },
        )
        .unwrap();

        assert_eq!(Source::Recursive, result.source);
        assert!(contains_path(&result.files, b"src/main.zig"));
        assert!(contains_path(&result.files, b"src/nested/lib.zig"));
        assert!(!contains_path(&result.files, b"node_modules/pkg/ignored.zig"));
        assert!(!contains_path(&result.files, b".zig-cache/o/ignored.o"));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn workspace_directory_provider_fallback_includes_nested_empty_directories_and_skips_ignored_roots() {
        let root = temp_root("fallback-dirs");
        std::fs::create_dir_all(root.join("empty/deep")).unwrap();
        std::fs::create_dir_all(root.join(".hidden/empty")).unwrap();
        std::fs::create_dir_all(root.join("ignored-dir/deep")).unwrap();

        let workspace = std::fs::canonicalize(&root).unwrap();
        let result = discover_directories_with_stop(
            workspace.to_str().unwrap().as_bytes(),
            &Options {
                force_fallback: true,
                include_hidden: true,
                ignored_names: vec![b"ignored-dir"],
                ..Options::default()
            },
            None,
            None,
        )
        .unwrap();

        assert_eq!(Source::Recursive, result.source);
        assert!(contains_path(&result.directories, b".hidden"));
        assert!(contains_path(&result.directories, b".hidden/empty"));
        assert!(contains_path(&result.directories, b"empty"));
        assert!(contains_path(&result.directories, b"empty/deep"));
        assert!(!contains_path(&result.directories, b"ignored-dir"));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn workspace_directory_provider_honors_its_cap_and_cancellation() {
        let root = temp_root("dir-cap");
        std::fs::create_dir_all(root.join("first")).unwrap();
        std::fs::create_dir_all(root.join("second")).unwrap();

        let workspace = std::fs::canonicalize(&root).unwrap();
        let result = discover_directories_with_stop(
            workspace.to_str().unwrap().as_bytes(),
            &Options {
                candidate_cap: 1,
                force_fallback: true,
                ..Options::default()
            },
            None,
            None,
        )
        .unwrap();
        assert_eq!(1, result.directories.len());
        assert!(result.incomplete);
        assert_eq!(Some(CapReason::CandidateCap), result.cap_reason);

        let stop_requested = AtomicBool::new(true);
        assert!(matches!(
            discover_directories_with_stop(
                workspace.to_str().unwrap().as_bytes(),
                &Options {
                    force_fallback: true,
                    ..Options::default()
                },
                Some(&stop_requested),
                None,
            ),
            Err(Error::Cancelled)
        ));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn workspace_file_provider_recursive_fallback_does_not_recurse_symlink_directories() {
        let root = temp_root("symlink");
        write_test_file(&root, "real/inside.txt", b"inside\n");
        std::os::unix::fs::symlink(root.join("real"), root.join("linked")).unwrap();

        let workspace = std::fs::canonicalize(&root).unwrap();
        let result = discover(
            workspace.to_str().unwrap().as_bytes(),
            &Options {
                force_fallback: true,
                ..Options::default()
            },
        )
        .unwrap();

        assert!(contains_path(&result.files, b"linked"));
        assert!(!contains_path(&result.files, b"linked/inside.txt"));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn workspace_file_provider_overlong_paths_are_skipped_without_aborting() {
        let long_path = vec![b'a'; max_relative_path_bytes + 1];
        let mut raw = b"kept.zig\x00".to_vec();
        raw.extend_from_slice(&long_path);
        raw.extend_from_slice(b"\x00also-kept.zig\x00");

        let result = parse_raw_list(&raw, Source::Git, &Options::default());

        assert_eq!(2, result.files.len());
        assert_eq!(1, result.skipped_overlong);
        assert!(contains_path(&result.files, b"kept.zig"));
        assert!(contains_path(&result.files, b"also-kept.zig"));
    }

    #[test]
    fn workspace_file_provider_falls_back_when_git_is_skipped() {
        let root = temp_root("git-skipped");
        write_test_file(&root, "nested/file.txt", b"file\n");

        let workspace = std::fs::canonicalize(&root).unwrap();
        let result = discover(
            workspace.to_str().unwrap().as_bytes(),
            &Options {
                force_fallback: true,
                ..Options::default()
            },
        )
        .unwrap();

        assert_eq!(Source::Recursive, result.source);
        assert!(contains_path(&result.files, b"nested/file.txt"));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(not(any(windows, target_os = "wasi")))]
    #[test]
    fn workspace_file_provider_treats_empty_successful_git_result_as_authoritative() {
        if !git_available() {
            return;
        }
        let root = temp_root("empty-git");
        write_test_file(&root, "untracked.txt", b"not tracked\n");
        run_git_for_test(&root, &["init", "--quiet"]);

        let workspace = std::fs::canonicalize(&root).unwrap();
        let result = discover_with_stop(
            workspace.to_str().unwrap().as_bytes(),
            &Options {
                git_worktree_is_authoritative: true,
                ..Options::default()
            },
            None,
            Some(b"git"),
        )
        .unwrap();

        assert_eq!(Source::Git, result.source);
        assert_eq!(0, result.files.len());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn workspace_file_provider_walks_a_git_worktree_when_no_trusted_git_executable_is_available() {
        let root = temp_root("no-git-exec");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        write_test_file(&root, "fallback.txt", b"fallback\n");

        let workspace = std::fs::canonicalize(&root).unwrap();
        let result = discover_with_stop(
            workspace.to_str().unwrap().as_bytes(),
            &Options {
                git_worktree_is_authoritative: true,
                ..Options::default()
            },
            None,
            None,
        )
        .unwrap();

        assert_eq!(Source::Recursive, result.source);
        assert!(contains_path(&result.files, b"fallback.txt"));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(not(target_os = "wasi"))]
    #[test]
    fn workspace_file_provider_does_not_recurse_after_a_selected_git_executable_fails() {
        let root = temp_root("git-fails");
        write_test_file(&root, ".git", b"gitdir: /missing\n");
        write_test_file(&root, "untracked.txt", b"not tracked\n");

        let workspace = std::fs::canonicalize(&root).unwrap();
        assert!(matches!(
            discover_with_stop(
                workspace.to_str().unwrap().as_bytes(),
                &Options {
                    git_worktree_is_authoritative: true,
                    ..Options::default()
                },
                None,
                Some(b"/definitely/missing/fx-git"),
            ),
            Err(Error::GitFileListFailed)
        ));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(not(any(windows, target_os = "wasi")))]
    #[test]
    fn workspace_file_provider_git_result_contains_tracked_files_only() {
        if !git_available() {
            return;
        }
        let root = temp_root("tracked-only");
        write_test_file(&root, ".gitignore", b"ignored.txt\n");
        write_test_file(&root, "tracked.txt", b"tracked\n");
        write_test_file(&root, ".hidden/tracked.txt", b"hidden\n");
        write_test_file(&root, "ignored.txt", b"ignored\n");
        write_test_file(&root, "untracked.txt", b"untracked\n");

        run_git_for_test(&root, &["init", "--quiet"]);
        run_git_for_test(
            &root,
            &["add", ".gitignore", "tracked.txt", ".hidden/tracked.txt"],
        );

        let workspace = std::fs::canonicalize(&root).unwrap();
        let result = discover_with_stop(
            workspace.to_str().unwrap().as_bytes(),
            &Options {
                include_hidden: true,
                ..Options::default()
            },
            None,
            Some(b"git"),
        )
        .unwrap();

        assert_eq!(Source::Git, result.source);
        assert!(contains_path(&result.files, b".gitignore"));
        assert!(contains_path(&result.files, b"tracked.txt"));
        assert!(contains_path(&result.files, b".hidden/tracked.txt"));
        assert!(!contains_path(&result.files, b"ignored.txt"));
        assert!(!contains_path(&result.files, b"untracked.txt"));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(not(any(windows, target_os = "wasi")))]
    #[test]
    fn workspace_directory_provider_uses_git_ignores_without_collapsing_nested_empty_directories() {
        if !git_available() {
            return;
        }
        let root = temp_root("dir-git");
        write_test_file(&root, ".gitignore", b"ignored-dir/\n");
        std::fs::create_dir_all(root.join("empty-dir")).unwrap();
        std::fs::create_dir_all(root.join("nested-empty/deep")).unwrap();
        std::fs::create_dir_all(root.join(".hidden/empty")).unwrap();
        std::fs::create_dir_all(root.join("ignored-dir/deep")).unwrap();
        write_test_file(&root, "build/tracked.txt", b"tracked\n");

        run_git_for_test(&root, &["init", "--quiet"]);
        run_git_for_test(&root, &["add", "build/tracked.txt"]);

        let workspace = std::fs::canonicalize(&root).unwrap();
        let result = discover_directories_with_stop(
            workspace.to_str().unwrap().as_bytes(),
            &Options {
                include_hidden: true,
                ..Options::default()
            },
            None,
            Some(b"git"),
        )
        .unwrap();

        assert_eq!(Source::Git, result.source);
        assert!(contains_path(&result.directories, b".hidden"));
        assert!(contains_path(&result.directories, b".hidden/empty"));
        assert!(contains_path(&result.directories, b"empty-dir"));
        assert!(contains_path(&result.directories, b"nested-empty"));
        assert!(contains_path(&result.directories, b"nested-empty/deep"));
        assert!(contains_path(&result.directories, b"build"));
        assert!(!contains_path(&result.directories, b"ignored-dir"));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn workspace_file_provider_cancellable_fallback_stops_before_traversal() {
        let stop_requested = AtomicBool::new(true);
        assert!(matches!(
            discover_cancellable(
                b"/unused",
                &Options {
                    force_fallback: true,
                    ..Options::default()
                },
                &stop_requested,
            ),
            Err(Error::Cancelled)
        ));
    }

    #[cfg(not(any(windows, target_os = "wasi")))]
    #[test]
    fn workspace_file_provider_cancellation_terminates_an_active_child() {
        let stop_requested = AtomicBool::new(false);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(Duration::from_millis(20));
                stop_requested.store(true, Ordering::SeqCst);
            });

            let started = Instant::now();
            let outcome = run_command(
                &[],
                &[b"/bin/sleep", b"60"],
                1024,
                1024,
                Some(&stop_requested),
            );
            assert!(matches!(outcome, Ok(CommandOutcome::Cancelled)));
            assert!(started.elapsed().as_millis() < 1000);
        });
    }
}
