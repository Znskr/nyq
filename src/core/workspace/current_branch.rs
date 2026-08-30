//! Reads the current Git branch for a working directory without spawning Git.
//!
//! The branch is recovered from a `.git` directory or linked-worktree
//! `gitdir:` file by walking up from `cwd`. Missing, detached, malformed, or
//! unreadable metadata is treated as absent.

use std::fs;
use std::path::{Path, PathBuf};

use crate::core::shared::text_utils;

const max_branch_bytes: usize = 255;
const max_metadata_bytes: usize = 4096;

enum GitDirResolution {
    Missing,
    Invalid,
    Found(PathBuf),
}

/// Returns an owned branch name when `cwd` is inside a Git worktree.
/// Missing, detached, malformed, or unreadable metadata is absent.
pub fn read(cwd: &[u8]) -> Option<Vec<u8>> {
    let cwd = std::str::from_utf8(cwd).ok()?;
    let cwd_path = Path::new(cwd);
    if !cwd_path.is_absolute() {
        return None;
    }
    let git_dir = resolve_git_dir(cwd_path)?;
    let head_path = git_dir.join("HEAD");
    let head = read_metadata_file(&head_path)?;
    let branch = parse_head(&head)?;
    Some(branch.to_vec())
}

fn parse_head(head: &[u8]) -> Option<&[u8]> {
    let trimmed = trim(head);
    let prefix = b"ref: refs/heads/";
    if !trimmed.starts_with(prefix) {
        return None;
    }
    let branch = &trimmed[prefix.len()..];
    if branch.is_empty() || branch.len() > max_branch_bytes || !text_utils::is_terminal_safe(branch) {
        return None;
    }
    Some(branch)
}

fn resolve_git_dir(cwd: &Path) -> Option<PathBuf> {
    let mut candidate = cwd;
    loop {
        match resolve_git_dir_at(candidate) {
            GitDirResolution::Found(path) => return Some(path),
            GitDirResolution::Invalid => return None,
            GitDirResolution::Missing => {}
        }
        let parent = candidate.parent()?;
        if parent == candidate {
            return None;
        }
        candidate = parent;
    }
}

fn resolve_git_dir_at(candidate: &Path) -> GitDirResolution {
    let dot_git = candidate.join(".git");
    let meta = match fs::symlink_metadata(&dot_git) {
        Ok(meta) => meta,
        Err(err) => {
            return match err.kind() {
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory => {
                    GitDirResolution::Missing
                }
                _ => GitDirResolution::Invalid,
            };
        }
    };
    if meta.is_dir() {
        return GitDirResolution::Found(dot_git);
    }
    if !meta.is_file() || meta.len() > max_metadata_bytes as u64 {
        return GitDirResolution::Invalid;
    }

    let content = match read_metadata_file(&dot_git) {
        Some(content) => content,
        None => return GitDirResolution::Invalid,
    };
    let trimmed = trim(&content);
    let prefix = b"gitdir:";
    if !trimmed.starts_with(prefix) {
        return GitDirResolution::Invalid;
    }
    let raw = trim(&trimmed[prefix.len()..]);
    if raw.is_empty() {
        return GitDirResolution::Invalid;
    }
    let raw = std::str::from_utf8(raw).ok();
    let raw = match raw {
        Some(raw) => raw,
        None => return GitDirResolution::Invalid,
    };
    let git_dir = if Path::new(raw).is_absolute() {
        PathBuf::from(raw)
    } else {
        candidate.join(raw)
    };
    let meta = match fs::symlink_metadata(&git_dir) {
        Ok(meta) => meta,
        Err(_) => return GitDirResolution::Invalid,
    };
    if !meta.is_dir() {
        return GitDirResolution::Invalid;
    }
    GitDirResolution::Found(git_dir)
}

fn read_metadata_file(path: &Path) -> Option<Vec<u8>> {
    let meta = fs::symlink_metadata(path).ok()?;
    if !meta.is_file() || meta.len() >= max_metadata_bytes as u64 {
        return None;
    }
    fs::read(path).ok()
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

    fn temp_root(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "nyq-current-branch-{}-{}",
            name,
            std::process::id()
        ));
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
    fn current_branch_parses_only_bounded_local_branch_refs() {
        assert_eq!(Some(b"main".as_slice()), parse_head(b"ref: refs/heads/main\n"));
        assert_eq!(
            Some(b"feature/media-ui".as_slice()),
            parse_head(b"ref: refs/heads/feature/media-ui\n")
        );
        assert!(parse_head(b"0123456789abcdef0123456789abcdef01234567\n").is_none());
        assert!(parse_head(b"ref: refs/tags/v1\n").is_none());
        assert!(parse_head(b"ref: refs/heads/bad\x1b[31m\n").is_none());
        let long = format!("ref: refs/heads/{}\n", "x".repeat(256));
        assert!(parse_head(long.as_bytes()).is_none());
    }

    #[test]
    fn current_branch_reads_nested_worktree_head_without_spawning_git() {
        let root = temp_root("nested");
        write_test_file(&root, "repo/.git/HEAD", b"ref: refs/heads/feature/media-ui\n");
        fs::create_dir_all(root.join("repo/src/nested")).unwrap();
        let cwd = fs::canonicalize(root.join("repo/src/nested")).unwrap();

        let branch = read(cwd.to_str().unwrap().as_bytes()).unwrap();
        assert_eq!(b"feature/media-ui", branch.as_slice());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn current_branch_follows_a_linked_worktree_gitdir_file() {
        let root = temp_root("linked");
        write_test_file(
            &root,
            "workspace/.git",
            b"gitdir: ../git-data/worktrees/workspace\n",
        );
        write_test_file(
            &root,
            "git-data/worktrees/workspace/HEAD",
            b"ref: refs/heads/worktree-branch\n",
        );
        let cwd = fs::canonicalize(root.join("workspace")).unwrap();

        let branch = read(cwd.to_str().unwrap().as_bytes()).unwrap();
        assert_eq!(b"worktree-branch", branch.as_slice());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn current_branch_rejects_missing_detached_malformed_oversized_and_symlinked_metadata() {
        let root = temp_root("rejects");
        fs::create_dir_all(root.join("repo/.git")).unwrap();
        let cwd = fs::canonicalize(root.join("repo")).unwrap();
        let cwd_bytes = cwd.to_str().unwrap().as_bytes();

        assert!(read(cwd_bytes).is_none());
        write_test_file(&root, "repo/.git/HEAD", b"0123456789abcdef\n");
        assert!(read(cwd_bytes).is_none());
        write_test_file(&root, "repo/.git/HEAD", b"ref: refs/tags/v1\n");
        assert!(read(cwd_bytes).is_none());
        let oversized = format!("ref: refs/heads/{}\n", "x".repeat(max_metadata_bytes + 1));
        write_test_file(&root, "repo/.git/HEAD", oversized.as_bytes());
        assert!(read(cwd_bytes).is_none());

        fs::create_dir_all(root.join("linked/actual-git")).unwrap();
        write_test_file(
            &root,
            "linked/actual-git/HEAD",
            b"ref: refs/heads/symlinked\n",
        );
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("actual-git", root.join("linked/.git")).unwrap();
        }
        #[cfg(windows)]
        {
            std::os::windows::fs::symlink_dir("actual-git", root.join("linked/.git")).unwrap();
        }
        let linked_cwd = fs::canonicalize(root.join("linked")).unwrap();
        assert!(read(linked_cwd.to_str().unwrap().as_bytes()).is_none());

        let _ = fs::remove_dir_all(&root);
    }
}
