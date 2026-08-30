//! Filesystem-derived statusline identity (workspace label + Git branch).
//!
//! The UI only receives terminal-safe strings. Repeated renders stat HEAD
//! without rereading it unless its metadata changes.

use std::fs;
use std::path::{Path, PathBuf};

use crate::core::shared::text_utils;

const max_git_metadata_bytes: usize = 4096;
const max_encoded_workspace_bytes: usize = 4096 * 4;
const max_encoded_branch_bytes: usize = 512;

#[derive(Clone, Copy, Default)]
pub struct Snapshot<'a> {
    pub workspace_label: &'a [u8],
    pub git_branch: Option<&'a [u8]>,
}

#[derive(Clone, Copy)]
struct HeadSignature {
    inode: u64,
    mtime_ns: i128,
    size: u64,
}

impl HeadSignature {
    fn eql(a: HeadSignature, b: HeadSignature) -> bool {
        a.inode == b.inode && a.mtime_ns == b.mtime_ns && a.size == b.size
    }
}

enum ParsedHead<'a> {
    Branch(&'a [u8]),
    Detached(&'a [u8]),
}

enum HeadPathResolution {
    Missing,
    Invalid,
    Found(PathBuf),
}

/// Owns the filesystem-derived state used by the footer. The UI only receives
/// terminal-safe strings, and repeated renders stat HEAD without rereading it
/// unless its metadata changes.
pub struct Runtime {
    pub enabled: bool,
    workspace_root: Vec<u8>,
    workspace_label: Vec<u8>,
    head_path: Vec<u8>,
    branch_label: Vec<u8>,
    head_signature: Option<HeadSignature>,
}

impl Default for Runtime {
    fn default() -> Self {
        Runtime {
            enabled: false,
            workspace_root: Vec::new(),
            workspace_label: Vec::new(),
            head_path: Vec::new(),
            branch_label: Vec::new(),
            head_signature: None,
        }
    }
}

impl Runtime {
    pub fn deinit(&mut self) {
        self.enabled = false;
        self.workspace_root.clear();
        self.workspace_label.clear();
        self.head_path.clear();
        self.branch_label.clear();
        self.head_signature = None;
    }

    pub fn snapshot(&self) -> Snapshot<'_> {
        Snapshot {
            workspace_label: &self.workspace_label,
            git_branch: if !self.branch_label.is_empty() {
                Some(&self.branch_label)
            } else {
                None
            },
        }
    }

    pub fn refresh(&mut self, workspace_root: &[u8]) -> Snapshot<'_> {
        if !self.enabled {
            return Snapshot::default();
        }
        if self.workspace_root != workspace_root {
            self.rebuild_workspace(workspace_root);
        }
        self.refresh_branch();
        self.snapshot()
    }

    fn rebuild_workspace(&mut self, workspace_root: &[u8]) {
        if workspace_root.is_empty() {
            self.deinit();
            return;
        }

        let next_root = workspace_root.to_vec();
        let encoded_path = encode_workspace_path(workspace_root);
        let next_head_path = resolve_head_path(workspace_root);

        self.workspace_root = next_root;
        self.workspace_label = encoded_path;
        self.head_path = next_head_path.unwrap_or_default();
        self.branch_label.clear();
        self.head_signature = None;
    }

    fn refresh_branch(&mut self) {
        if self.head_path.is_empty() {
            return;
        }
        let head_path = match to_path(&self.head_path) {
            Some(path) => path,
            None => {
                self.branch_label.clear();
                self.head_signature = None;
                return;
            }
        };
        let meta = match fs::symlink_metadata(head_path) {
            Ok(meta) => meta,
            Err(_) => {
                self.branch_label.clear();
                self.head_signature = None;
                return;
            }
        };
        if !meta.is_file() {
            self.branch_label.clear();
            self.head_signature = None;
            return;
        }

        let signature = HeadSignature {
            inode: inode(&meta),
            mtime_ns: mtime_ns(&meta),
            size: meta.len(),
        };
        if let Some(previous) = self.head_signature {
            if HeadSignature::eql(previous, signature) {
                return;
            }
        }

        let head = match read_small_file(head_path, max_git_metadata_bytes) {
            Some(head) => head,
            None => {
                self.branch_label.clear();
                self.head_signature = Some(signature);
                return;
            }
        };

        if let Some(parsed) = parse_head(&head) {
            let mut detached_buf: Vec<u8> = Vec::new();
            let raw: &[u8] = match parsed {
                ParsedHead::Branch(branch) => branch,
                ParsedHead::Detached(sha) => {
                    detached_buf.extend_from_slice(b"detached:");
                    detached_buf.extend_from_slice(sha);
                    &detached_buf
                }
            };
            let encoded = text_utils::encode_terminal_safe(raw, max_encoded_branch_bytes);
            self.branch_label.clear();
            self.branch_label = encoded.bytes;
        } else {
            self.branch_label.clear();
        }
        self.head_signature = Some(signature);
    }
}

fn encode_workspace_path(workspace_root: &[u8]) -> Vec<u8> {
    match text_utils::encode_terminal_safe_path_tail(workspace_root, max_encoded_workspace_bytes) {
        Ok(encoded) => encoded.bytes,
        Err(text_utils::PathTailError::PathBasenameTooLong) => {
            text_utils::encode_terminal_safe(workspace_root, max_encoded_workspace_bytes).bytes
        }
    }
}

fn resolve_head_path(workspace_root: &[u8]) -> Option<Vec<u8>> {
    let workspace_root = std::str::from_utf8(workspace_root).ok()?;
    let mut candidate = Path::new(workspace_root);
    loop {
        match resolve_head_path_at(candidate) {
            HeadPathResolution::Found(path) => return Some(path.to_str()?.as_bytes().to_vec()),
            HeadPathResolution::Invalid => return None,
            HeadPathResolution::Missing => {}
        }
        let parent = candidate.parent()?;
        if parent == candidate {
            return None;
        }
        candidate = parent;
    }
}

fn resolve_head_path_at(candidate: &Path) -> HeadPathResolution {
    let dot_git = candidate.join(".git");
    let meta = match fs::symlink_metadata(&dot_git) {
        Ok(meta) => meta,
        Err(err) => {
            return match err.kind() {
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory => {
                    HeadPathResolution::Missing
                }
                _ => HeadPathResolution::Invalid,
            };
        }
    };

    if meta.is_dir() {
        return HeadPathResolution::Found(dot_git.join("HEAD"));
    }
    if !meta.is_file() {
        return HeadPathResolution::Invalid;
    }

    let content = match read_small_file(&dot_git, max_git_metadata_bytes) {
        Some(content) => content,
        None => return HeadPathResolution::Invalid,
    };
    let trimmed = trim(&content);
    let prefix = b"gitdir:";
    if !trimmed.starts_with(prefix) {
        return HeadPathResolution::Invalid;
    }

    let raw = trim(&trimmed[prefix.len()..]);
    if raw.is_empty() {
        return HeadPathResolution::Invalid;
    }
    let raw = match std::str::from_utf8(raw) {
        Ok(raw) => raw,
        Err(_) => return HeadPathResolution::Invalid,
    };
    let git_dir = if Path::new(raw).is_absolute() {
        PathBuf::from(raw)
    } else {
        candidate.join(raw)
    };
    HeadPathResolution::Found(git_dir.join("HEAD"))
}

fn read_small_file(path: &Path, max_bytes: usize) -> Option<Vec<u8>> {
    let bytes = fs::read(path).ok()?;
    if bytes.len() >= max_bytes {
        return None;
    }
    Some(bytes)
}

fn parse_head(head: &[u8]) -> Option<ParsedHead<'_>> {
    let trimmed = trim(head);
    if trimmed.is_empty() {
        return None;
    }

    let ref_prefix = b"ref:";
    if trimmed.starts_with(ref_prefix) {
        let reference = trim(&trimmed[ref_prefix.len()..]);
        if reference.is_empty() {
            return None;
        }
        let heads_prefix = b"refs/heads/";
        let branch = if reference.starts_with(heads_prefix) {
            &reference[heads_prefix.len()..]
        } else {
            reference
        };
        if branch.is_empty() {
            return None;
        }
        return Some(ParsedHead::Branch(branch));
    }

    if trimmed.len() < 7 {
        return None;
    }
    for &byte in trimmed {
        if !byte.is_ascii_hexdigit() {
            return None;
        }
    }
    Some(ParsedHead::Detached(&trimmed[..trimmed.len().min(12)]))
}

fn to_path(bytes: &[u8]) -> Option<&Path> {
    std::str::from_utf8(bytes).ok().map(Path::new)
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

#[cfg(unix)]
fn inode(meta: &fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    meta.ino()
}
#[cfg(not(unix))]
fn inode(_: &fs::Metadata) -> u64 {
    0
}

#[cfg(unix)]
fn mtime_ns(meta: &fs::Metadata) -> i128 {
    use std::os::unix::fs::MetadataExt;
    meta.mtime_nsec() as i128
}
#[cfg(not(unix))]
fn mtime_ns(_: &fs::Metadata) -> i128 {
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("nyq-statusline-{}-{}", name, std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn write_test_file(root: &Path, rel: &str, content: &[u8]) {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, content).unwrap();
    }

    #[test]
    fn workspace_statusline_identity_refreshes_branch_and_detached_head() {
        let root = temp_root("refresh");
        write_test_file(&root, "workspace/.git/HEAD", b"ref: refs/heads/main\n");

        let workspace = fs::canonicalize(root.join("workspace")).unwrap();
        let workspace_bytes = workspace.to_str().unwrap().as_bytes();
        let mut runtime = Runtime {
            enabled: true,
            ..Runtime::default()
        };

        let snapshot = runtime.refresh(workspace_bytes);
        assert_eq!(workspace_bytes, snapshot.workspace_label);
        assert_eq!(Some(b"main".as_slice()), snapshot.git_branch);

        write_test_file(
            &root,
            "workspace/.git/HEAD",
            b"ref: refs/heads/refreshed-branch\n",
        );
        let snapshot = runtime.refresh(workspace_bytes);
        assert_eq!(Some(b"refreshed-branch".as_slice()), snapshot.git_branch);

        write_test_file(
            &root,
            "workspace/.git/HEAD",
            b"0123456789abcdef0123456789abcdef01234567\n",
        );
        let snapshot = runtime.refresh(workspace_bytes);
        assert_eq!(Some(b"detached:0123456789ab".as_slice()), snapshot.git_branch);

        runtime.deinit();
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn workspace_statusline_identity_resolves_worktree_gitdir_files() {
        let root = temp_root("worktree");
        write_test_file(
            &root,
            "workspace/.git",
            b"gitdir: ../git-data/worktrees/active\n",
        );
        write_test_file(
            &root,
            "git-data/worktrees/active/HEAD",
            b"ref: refs/heads/worktree-branch\n",
        );

        let workspace = fs::canonicalize(root.join("workspace")).unwrap();
        let mut runtime = Runtime {
            enabled: true,
            ..Runtime::default()
        };

        let snapshot = runtime.refresh(workspace.to_str().unwrap().as_bytes());
        assert_eq!(Some(b"worktree-branch".as_slice()), snapshot.git_branch);

        runtime.deinit();
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn workspace_statusline_identity_finds_a_repository_above_the_working_directory() {
        let root = temp_root("parent");
        write_test_file(&root, "repo/.git/HEAD", b"ref: refs/heads/parent-repo\n");
        fs::create_dir_all(root.join("repo/packages/app")).unwrap();

        let workspace = fs::canonicalize(root.join("repo/packages/app")).unwrap();
        let workspace_bytes = workspace.to_str().unwrap().as_bytes();
        let mut runtime = Runtime {
            enabled: true,
            ..Runtime::default()
        };

        let snapshot = runtime.refresh(workspace_bytes);
        assert_eq!(workspace_bytes, snapshot.workspace_label);
        assert_eq!(Some(b"parent-repo".as_slice()), snapshot.git_branch);

        runtime.deinit();
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn workspace_statusline_identity_handles_non_git_and_hostile_paths() {
        let root = temp_root("hostile");
        fs::create_dir_all(root.join("unsafe-\x1b[31m-workspace")).unwrap();
        write_test_file(
            &root,
            "unsafe-\x1b[31m-workspace/.git",
            b"not a Git directory\n",
        );

        let workspace = fs::canonicalize(root.join("unsafe-\x1b[31m-workspace")).unwrap();
        let mut runtime = Runtime {
            enabled: true,
            ..Runtime::default()
        };

        let snapshot = runtime.refresh(workspace.to_str().unwrap().as_bytes());
        assert!(snapshot.git_branch.is_none());
        assert!(!snapshot.workspace_label.contains(&0x1b));
        assert!(
            snapshot
                .workspace_label
                .windows(4)
                .any(|w| w == b"\\x1b")
        );

        runtime.deinit();
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn workspace_statusline_identity_rejects_malformed_detached_head() {
        let root = temp_root("malformed");
        write_test_file(&root, "workspace/.git/HEAD", b"not-a-commit\x1b[31m\n");

        let workspace = fs::canonicalize(root.join("workspace")).unwrap();
        let mut runtime = Runtime {
            enabled: true,
            ..Runtime::default()
        };

        let snapshot = runtime.refresh(workspace.to_str().unwrap().as_bytes());
        assert!(snapshot.git_branch.is_none());

        runtime.deinit();
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn workspace_statusline_identity_represents_the_filesystem_root() {
        let mut runtime = Runtime {
            enabled: true,
            ..Runtime::default()
        };

        let snapshot = runtime.refresh(b"/");
        assert_eq!(b"/", snapshot.workspace_label);

        runtime.deinit();
    }

    #[test]
    fn workspace_statusline_disabled_refresh_preserves_cached_identity() {
        let root = temp_root("disabled");
        write_test_file(&root, "workspace/.git/HEAD", b"ref: refs/heads/main\n");

        let workspace = fs::canonicalize(root.join("workspace")).unwrap();
        let workspace_bytes = workspace.to_str().unwrap().as_bytes();
        let mut runtime = Runtime {
            enabled: true,
            ..Runtime::default()
        };

        let initial = runtime.refresh(workspace_bytes);
        assert_eq!(Some(b"main".as_slice()), initial.git_branch);

        runtime.enabled = false;
        write_test_file(
            &root,
            "workspace/.git/HEAD",
            b"ref: refs/heads/changed-while-disabled\n",
        );
        let disabled = runtime.refresh(workspace_bytes);
        assert_eq!(b"", disabled.workspace_label);
        assert!(disabled.git_branch.is_none());
        assert_eq!(workspace_bytes, runtime.workspace_root);
        assert_eq!(b"main", runtime.branch_label.as_slice());

        runtime.enabled = true;
        let reenabled = runtime.refresh(workspace_bytes);
        assert_eq!(
            Some(b"changed-while-disabled".as_slice()),
            reenabled.git_branch
        );

        runtime.deinit();
        let _ = fs::remove_dir_all(&root);
    }
}
