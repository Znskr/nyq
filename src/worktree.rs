//! Git worktree isolation for subagents (design §8.1, "write isolation").
//!
//! `subagent` with `isolation: "worktree"` gives the child its own worktree
//! at `<repo>/.nyq/worktrees/<child>` on a new branch from `HEAD`. The child's
//! cwd and sandbox roots point there, so it never touches the parent's files.
//! When it finishes the parent gets a diff summary; `subagent merge` applies
//! the child's changes to the parent workspace as a patch. A patch that does
//! not apply cleanly is reported file by file and nothing is written: nyq
//! never resolves conflicts itself. Stopping or merging removes the worktree
//! and its branch.
//!
//! Everything here runs `git` directly in the parent process (not in the
//! sandbox): the worktree's metadata lives in the parent's `.git`, which the
//! sandbox keeps read-only.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

/// Where worktrees live, relative to the repository root.
pub const DIR: &str = ".nyq/worktrees";
/// The diff shown to the parent is cut to this many bytes.
const MAX_SHOWN_DIFF: usize = 12_000;

/// A child's worktree, saved with its subagent record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Worktree {
    /// The repository root the worktree was made from (the parent side).
    pub repo: PathBuf,
    /// The worktree directory (the child's cwd).
    pub path: PathBuf,
    pub branch: String,
    /// The commit it started from.
    pub base: String,
}

/// What a merge did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Merge {
    /// The child changed nothing; the worktree was removed.
    Empty,
    /// The patch applied; these files changed in the parent workspace.
    Applied { files: Vec<String>, stat: String },
    /// The patch does not apply. Nothing was written and the worktree is kept.
    Conflict { files: Vec<String>, detail: String },
}

async fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = git_raw(dir, args, None).await?;
    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

async fn git_raw(dir: &Path, args: &[&str], stdin: Option<&[u8]>) -> Result<std::process::Output> {
    use tokio::io::AsyncWriteExt;
    let mut cmd = tokio::process::Command::new("git");
    cmd.arg("-C")
        .arg(dir)
        .args(args)
        // A user's hooks, pager or editor must not run or block here.
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_EDITOR", "true")
        .env("GIT_PAGER", "cat")
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = cmd.spawn().context("running git (is it installed?)")?;
    if let (Some(data), Some(mut pipe)) = (stdin, child.stdin.take()) {
        pipe.write_all(data).await?;
        drop(pipe);
    }
    Ok(child.wait_with_output().await?)
}

/// The repository root containing `cwd`.
pub async fn repo_root(cwd: &Path) -> Result<PathBuf> {
    let top = git(cwd, &["rev-parse", "--show-toplevel"])
        .await
        .context("worktree isolation needs a git repository")?;
    Ok(PathBuf::from(top.trim()))
}

/// Create `<repo>/.nyq/worktrees/<child>` on a new branch from `HEAD`.
/// Uncommitted parent changes are not part of it (they are not in `HEAD`).
pub async fn create(cwd: &Path, child: &str) -> Result<Worktree> {
    if !crate::subagents::valid_name(child) {
        bail!("invalid worktree name `{child}`");
    }
    let repo = repo_root(cwd).await?;
    let base = git(&repo, &["rev-parse", "--verify", "HEAD^{commit}"])
        .await
        .context("worktree isolation needs at least one commit")?
        .trim()
        .to_string();
    let path = repo.join(DIR).join(child);
    if path.exists() {
        bail!(
            "{} already exists; merge or stop the child that owns it first",
            path.display()
        );
    }
    std::fs::create_dir_all(path.parent().expect("has parent"))?;
    exclude(&repo).await;
    let suffix = &uuid::Uuid::new_v4().simple().to_string()[..8];
    let branch = format!("nyq/{child}-{suffix}");
    let p = path.to_string_lossy().into_owned();
    git(&repo, &["worktree", "add", "--quiet", "-b", &branch, &p, &base]).await?;
    Ok(Worktree {
        repo,
        path,
        branch,
        base,
    })
}

/// Keep `.nyq/worktrees/` out of the parent's `git status` and searches.
async fn exclude(repo: &Path) {
    let Ok(info) = git(repo, &["rev-parse", "--git-path", "info/exclude"]).await else {
        return;
    };
    let file = repo.join(info.trim());
    let line = format!("/{DIR}/");
    let current = std::fs::read_to_string(&file).unwrap_or_default();
    if current.lines().any(|l| l.trim() == line) {
        return;
    }
    if let Some(parent) = file.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let sep = if current.is_empty() || current.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    let _ = std::fs::write(&file, format!("{current}{sep}{line}\n"));
}

impl Worktree {
    /// Stage everything in the worktree (new files included) and return the
    /// binary patch against the base, with its `--stat`.
    pub async fn diff(&self) -> Result<(String, String)> {
        if !self.path.is_dir() {
            bail!("worktree {} is gone", self.path.display());
        }
        git(&self.path, &["add", "-A"]).await?;
        let patch = git(
            &self.path,
            &["diff", "--cached", "--binary", "--no-color", &self.base],
        )
        .await?;
        let stat = git(
            &self.path,
            &["diff", "--cached", "--stat", "--no-color", &self.base],
        )
        .await?;
        Ok((patch, stat.trim_end().to_string()))
    }

    /// What the parent sees when the child finishes: the change summary and
    /// the start of the diff, plus how to merge.
    pub async fn summary(&self, child: &str) -> String {
        match self.diff().await {
            Err(e) => format!("[worktree {}: could not diff: {e:#}]", self.path.display()),
            Ok((patch, _)) if patch.trim().is_empty() => format!(
                "[worktree {}: no changes. `subagent` action \"merge\" with name \"{child}\" removes it.]",
                self.path.display()
            ),
            Ok((patch, stat)) => {
                let shown = if patch.len() > MAX_SHOWN_DIFF {
                    let mut end = MAX_SHOWN_DIFF;
                    while !patch.is_char_boundary(end) {
                        end -= 1;
                    }
                    format!(
                        "{}\n[… diff truncated, {} more bytes]",
                        &patch[..end],
                        patch.len() - end
                    )
                } else {
                    patch
                };
                format!(
                    "[worktree {} on branch {} — changes are NOT in your workspace yet]\n{stat}\n\n{shown}\nReview the diff, then call `subagent` with action \"merge\" and name \"{child}\" to apply it as a patch (conflicts are reported, never resolved), or action \"stop\" to discard it.",
                    self.path.display(),
                    self.branch
                )
            }
        }
    }

    /// Apply the child's changes to the parent workspace. On success (or
    /// when there is nothing to apply) the worktree is removed; on conflict
    /// nothing is written and it is kept for inspection.
    /// Files about to change are snapshotted into `undo` first.
    pub async fn merge(&self, undo: &mut crate::undo::UndoLog) -> Result<Merge> {
        let (patch, stat) = self.diff().await?;
        if patch.trim().is_empty() {
            self.remove()?;
            return Ok(Merge::Empty);
        }
        let files = patch_files(&patch);
        let check = git_raw(
            &self.repo,
            &["apply", "--check", "--binary", "--whitespace=nowarn", "-"],
            Some(patch.as_bytes()),
        )
        .await?;
        if !check.status.success() {
            let detail = String::from_utf8_lossy(&check.stderr).trim().to_string();
            let mut conflicted = conflict_files(&detail);
            if conflicted.is_empty() {
                conflicted = files;
            }
            return Ok(Merge::Conflict {
                files: conflicted,
                detail,
            });
        }
        for f in &files {
            undo.snapshot(
                &self.repo,
                crate::sandbox::Mode::Off,
                "write_file",
                &serde_json::json!({ "path": f }),
            );
        }
        let applied = git_raw(
            &self.repo,
            &["apply", "--binary", "--whitespace=nowarn", "-"],
            Some(patch.as_bytes()),
        )
        .await?;
        if !applied.status.success() {
            let detail = String::from_utf8_lossy(&applied.stderr).trim().to_string();
            return Ok(Merge::Conflict {
                files: conflict_files(&detail),
                detail,
            });
        }
        self.remove()?;
        Ok(Merge::Applied { files, stat })
    }

    /// Remove the worktree and its branch. Missing pieces are not an error.
    /// Blocking but quick; also used from the synchronous `/agents stop`.
    pub fn remove(&self) -> Result<()> {
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(&self.repo)
                .args(args)
                .env("GIT_TERMINAL_PROMPT", "0")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .output()
        };
        let p = self.path.to_string_lossy().into_owned();
        if self.path.exists() {
            let out = git(&["worktree", "remove", "--force", &p]);
            if !out.as_ref().is_ok_and(|o| o.status.success()) {
                // Not a registered worktree any more: drop the directory.
                std::fs::remove_dir_all(&self.path)
                    .with_context(|| format!("removing {}", self.path.display()))?;
            }
        }
        let _ = git(&["worktree", "prune"]);
        let _ = git(&["branch", "-D", &self.branch]);
        Ok(())
    }
}

/// Paths a patch touches, from its `diff --git a/x b/y` headers.
fn patch_files(patch: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in patch.lines() {
        if let Some(rest) = line.strip_prefix("diff --git a/") {
            if let Some((_, b)) = rest.split_once(" b/") {
                if !out.iter().any(|f| f == b) {
                    out.push(b.to_string());
                }
            }
        }
    }
    out
}

/// Files `git apply` complained about ("error: patch failed: src/a.rs:3",
/// "error: src/a.rs: does not match index", "… already exists in working directory").
fn conflict_files(stderr: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in stderr.lines() {
        let Some(rest) = line.strip_prefix("error: ") else {
            continue;
        };
        let file = if let Some(r) = rest.strip_prefix("patch failed: ") {
            r.rsplit_once(':').map(|(f, _)| f).unwrap_or(r)
        } else if let Some((f, _)) = rest.split_once(": ") {
            f
        } else {
            continue;
        };
        if !file.is_empty() && !out.iter().any(|f| f == file) {
            out.push(file.to_string());
        }
    }
    out
}

/// The parent-facing text for a merge result.
pub fn describe(child: &str, merge: &Merge) -> (String, bool) {
    match merge {
        Merge::Empty => (format!("`{child}` changed nothing; its worktree was removed."), false),
        Merge::Applied { files, stat } => (
            format!(
                "Applied `{child}`'s changes to the workspace ({} file{}); its worktree and branch were removed.\n{stat}",
                files.len(),
                if files.len() == 1 { "" } else { "s" }
            ),
            false,
        ),
        Merge::Conflict { files, detail } => (
            format!(
                "Merge of `{child}` stopped: its patch does not apply cleanly to the workspace, so nothing was written. Conflicting files: {}.\n{detail}\nThe worktree is kept: inspect it, reconcile the files yourself, then stop `{child}` to remove it.",
                if files.is_empty() { "(unknown)".to_string() } else { files.join(", ") }
            ),
            true,
        ),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A repository with one commit holding `a.txt`.
    pub(crate) fn repo() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        let run = |args: &[&str]| {
            let ok = std::process::Command::new("git")
                .arg("-C")
                .arg(d.path())
                .args(args)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .success();
            assert!(ok, "git {args:?}");
        };
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "t@example.invalid"]);
        run(&["config", "user.name", "t"]);
        run(&["config", "commit.gpgsign", "false"]);
        std::fs::write(d.path().join("a.txt"), "one\ntwo\nthree\n").unwrap();
        run(&["add", "a.txt"]);
        run(&["commit", "-q", "-m", "init"]);
        d
    }

    #[tokio::test]
    async fn create_diff_merge_and_cleanup() {
        let d = repo();
        let wt = create(d.path(), "fixer").await.unwrap();
        assert_eq!(wt.path, d.path().canonicalize().unwrap().join(DIR).join("fixer"));
        assert!(wt.path.join("a.txt").is_file());
        // Excluded from the parent's status.
        let status = git(d.path(), &["status", "--porcelain"]).await.unwrap();
        assert!(status.trim().is_empty(), "{status}");
        // A second child with the same name is refused while it exists.
        assert!(create(d.path(), "fixer").await.is_err());

        std::fs::write(wt.path.join("a.txt"), "one\nTWO\nthree\n").unwrap();
        std::fs::write(wt.path.join("new.txt"), "fresh\n").unwrap();
        let summary = wt.summary("fixer").await;
        assert!(summary.contains("new.txt") && summary.contains("+TWO"), "{summary}");
        // The parent is untouched until the merge.
        assert_eq!(
            std::fs::read_to_string(d.path().join("a.txt")).unwrap(),
            "one\ntwo\nthree\n"
        );
        match wt.merge(&mut Default::default()).await.unwrap() {
            Merge::Applied { files, .. } => {
                assert_eq!(files, ["a.txt", "new.txt"]);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            std::fs::read_to_string(d.path().join("a.txt")).unwrap(),
            "one\nTWO\nthree\n"
        );
        assert_eq!(
            std::fs::read_to_string(d.path().join("new.txt")).unwrap(),
            "fresh\n"
        );
        assert!(!wt.path.exists());
        let branches = git(d.path(), &["branch", "--list", "nyq/*"]).await.unwrap();
        assert!(branches.trim().is_empty(), "{branches}");
    }

    #[tokio::test]
    async fn conflicting_merge_writes_nothing_and_keeps_the_worktree() {
        let d = repo();
        let wt = create(d.path(), "c1").await.unwrap();
        std::fs::write(wt.path.join("a.txt"), "one\nchild\nthree\n").unwrap();
        std::fs::write(wt.path.join("b.txt"), "child file\n").unwrap();
        // The parent edits the same line meanwhile.
        std::fs::write(d.path().join("a.txt"), "one\nparent\nthree\n").unwrap();
        let merge = wt.merge(&mut Default::default()).await.unwrap();
        match &merge {
            Merge::Conflict { files, detail } => {
                assert_eq!(files, &["a.txt"], "{detail}");
            }
            other => panic!("{other:?}"),
        }
        let (text, is_error) = describe("c1", &merge);
        assert!(is_error && text.contains("a.txt") && text.contains("nothing was written"));
        // Nothing applied, not even the non-conflicting new file.
        assert_eq!(
            std::fs::read_to_string(d.path().join("a.txt")).unwrap(),
            "one\nparent\nthree\n"
        );
        assert!(!d.path().join("b.txt").exists());
        assert!(wt.path.is_dir());
        wt.remove().unwrap();
        assert!(!wt.path.exists());
    }

    #[tokio::test]
    async fn empty_merge_and_errors() {
        let d = repo();
        let wt = create(d.path(), "idle").await.unwrap();
        assert!(wt.summary("idle").await.contains("no changes"));
        assert_eq!(wt.merge(&mut Default::default()).await.unwrap(), Merge::Empty);
        assert!(!wt.path.exists());
        let plain = tempfile::tempdir().unwrap();
        assert!(create(plain.path(), "x").await.is_err());
        assert!(create(d.path(), "../x").await.is_err());
    }

    #[test]
    fn parses_apply_errors() {
        let err = "error: patch failed: src/a.rs:3\nerror: src/a.rs: patch does not apply\nerror: b.txt: already exists in working directory\n";
        assert_eq!(conflict_files(err), ["src/a.rs", "b.txt"]);
    }
}
