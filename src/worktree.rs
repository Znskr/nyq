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

/// Config every nyq-run `git` gets, ahead of its own arguments: diff output
/// must not depend on the user's (or a repository's) porcelain settings, and
/// no hook may run.
const NEUTRAL_CONFIG: [&str; 8] = [
    "-c",
    "core.hooksPath=/dev/null",
    "-c",
    "diff.noprefix=false",
    "-c",
    "diff.mnemonicPrefix=false",
    "-c",
    "diff.external=",
];

/// Environment for the same purpose: no user or system git config at all.
const NEUTRAL_ENV: [(&str, &str); 5] = [
    ("GIT_CONFIG_GLOBAL", "/dev/null"),
    ("GIT_CONFIG_SYSTEM", "/dev/null"),
    ("GIT_CONFIG_NOSYSTEM", "1"),
    ("GIT_TERMINAL_PROMPT", "0"),
    ("GIT_EXTERNAL_DIFF", ""),
];

/// Flags that make `git diff` print a plain, applyable patch whatever the
/// config or a `.gitattributes` in the (child-controlled) tree says.
const PATCH_FLAGS: [&str; 7] = [
    "--binary",
    "--no-color",
    "--no-ext-diff",
    "--no-textconv",
    "--no-renames",
    "--src-prefix=a/",
    "--dst-prefix=b/",
];

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
    cmd.args(NEUTRAL_CONFIG)
        .arg("-C")
        .arg(dir)
        .args(args)
        .envs(NEUTRAL_ENV)
        // A user's pager or editor must not run or block here.
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
    git(
        &repo,
        &["worktree", "add", "--quiet", "-b", &branch, &p, &base],
    )
    .await?;
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
        let mut args = vec!["diff", "--cached"];
        args.extend(PATCH_FLAGS);
        args.push(&self.base);
        let patch = git(&self.path, &args).await?;
        let stat = git(
            &self.path,
            &[
                "diff",
                "--cached",
                "--stat",
                "--no-color",
                "--no-ext-diff",
                "--no-textconv",
                "--no-renames",
                &self.base,
            ],
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
    ///
    /// `roots` are the parent's writable roots: a patch touching a path
    /// outside them is refused (the worktree is made from the repository
    /// root, which may be wider than the parent's cwd).
    pub async fn merge(&self, undo: &mut crate::undo::UndoLog, roots: &[PathBuf]) -> Result<Merge> {
        let (patch, stat) = self.diff().await?;
        if patch.trim().is_empty() {
            self.remove()?;
            return Ok(Merge::Empty);
        }
        let files = self.patch_files(&patch).await?;
        check_roots(&self.repo, &files, roots)?;
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
                &[std::path::PathBuf::from(f)],
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

    /// Paths the patch touches, as git itself reads them (`apply --numstat -z`
    /// is NUL-separated and unquoted, unlike the `diff --git` headers).
    async fn patch_files(&self, patch: &str) -> Result<Vec<String>> {
        let out = git_raw(
            &self.repo,
            &["apply", "--numstat", "-z", "--binary", "-"],
            Some(patch.as_bytes()),
        )
        .await?;
        if !out.status.success() {
            // An unreadable patch is a conflict later; list what the headers say.
            return Ok(header_files(patch));
        }
        let mut files: Vec<String> = Vec::new();
        for rec in out.stdout.split(|b| *b == 0).filter(|r| !r.is_empty()) {
            let text = String::from_utf8_lossy(rec);
            // "<added>\t<deleted>\t<path>"
            if let Some(path) = text.splitn(3, '\t').nth(2) {
                if !files.iter().any(|f| f == path) {
                    files.push(path.to_string());
                }
            }
        }
        Ok(files)
    }

    /// Remove the worktree and its branch. Missing pieces are not an error.
    /// Blocking but quick; also used from the synchronous `/agents stop`.
    pub fn remove(&self) -> Result<()> {
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(NEUTRAL_CONFIG)
                .arg("-C")
                .arg(&self.repo)
                .args(args)
                .envs(NEUTRAL_ENV)
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

/// Refuse patch paths that leave the parent's writable roots (or touch git
/// metadata). Paths are relative to the repository root.
fn check_roots(repo: &Path, files: &[String], roots: &[PathBuf]) -> Result<()> {
    use std::path::Component;
    let roots: Vec<PathBuf> = roots
        .iter()
        .map(|r| r.canonicalize().unwrap_or_else(|_| r.clone()))
        .collect();
    for f in files {
        let rel = Path::new(f);
        let plain = rel
            .components()
            .all(|c| matches!(c, Component::Normal(n) if n != ".git"));
        if !plain || rel.as_os_str().is_empty() {
            bail!("the patch touches `{f}`, which is not a plain path inside the workspace; nothing was written");
        }
        let abs = repo.join(rel);
        if !roots.iter().any(|root| abs.starts_with(root)) {
            bail!(
                "the patch touches `{f}`, outside your writable roots ({}); nothing was written and the worktree is kept",
                roots
                    .iter()
                    .map(|r| r.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
    }
    Ok(())
}

/// Paths a patch touches, from its `diff --git a/x b/y` headers.
fn header_files(patch: &str) -> Vec<String> {
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
        assert_eq!(
            wt.path,
            d.path().canonicalize().unwrap().join(DIR).join("fixer")
        );
        assert!(wt.path.join("a.txt").is_file());
        // Excluded from the parent's status.
        let status = git(d.path(), &["status", "--porcelain"]).await.unwrap();
        assert!(status.trim().is_empty(), "{status}");
        // A second child with the same name is refused while it exists.
        assert!(create(d.path(), "fixer").await.is_err());

        std::fs::write(wt.path.join("a.txt"), "one\nTWO\nthree\n").unwrap();
        std::fs::write(wt.path.join("new.txt"), "fresh\n").unwrap();
        let summary = wt.summary("fixer").await;
        assert!(
            summary.contains("new.txt") && summary.contains("+TWO"),
            "{summary}"
        );
        // The parent is untouched until the merge.
        assert_eq!(
            std::fs::read_to_string(d.path().join("a.txt")).unwrap(),
            "one\ntwo\nthree\n"
        );
        match wt
            .merge(&mut Default::default(), &[d.path().to_path_buf()])
            .await
            .unwrap()
        {
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
        let merge = wt
            .merge(&mut Default::default(), &[d.path().to_path_buf()])
            .await
            .unwrap();
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
        assert_eq!(
            wt.merge(&mut Default::default(), &[d.path().to_path_buf()])
                .await
                .unwrap(),
            Merge::Empty
        );
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

    fn write(dir: &Path, rel: &str, text: &str) {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    fn read(dir: &Path, rel: &str) -> String {
        std::fs::read_to_string(dir.join(rel)).unwrap()
    }

    /// Child changes: a nested new file plus an edit.
    fn child_edits(wt: &Worktree) {
        write(&wt.path, "src/sub/only_new.txt", "brand new\n");
        write(&wt.path, "a.txt", "one\nTWO\nthree\n");
    }

    fn assert_merged_in_place(d: &Path) {
        assert_eq!(read(d, "src/sub/only_new.txt"), "brand new\n");
        assert!(!d.join("sub/only_new.txt").exists());
        assert_eq!(read(d, "a.txt"), "one\nTWO\nthree\n");
    }

    #[tokio::test]
    async fn repo_diff_config_does_not_change_the_merge() {
        let d = repo();
        let run = |args: &[&str]| {
            assert!(std::process::Command::new("git")
                .arg("-C")
                .arg(d.path())
                .args(args)
                .status()
                .unwrap()
                .success());
        };
        let ext = d.path().join("ext.sh");
        std::fs::write(&ext, "#!/bin/sh\necho EXTERNAL DIFF OUTPUT\n").unwrap();
        run(&["config", "diff.noprefix", "true"]);
        run(&["config", "diff.mnemonicPrefix", "true"]);
        run(&["config", "diff.external", &format!("sh {}", ext.display())]);
        // A textconv driver selected by the child's own .gitattributes.
        run(&["config", "diff.evil.textconv", "echo CONVERTED #"]);
        let wt = create(d.path(), "cfg").await.unwrap();
        child_edits(&wt);
        write(&wt.path, ".gitattributes", "*.txt diff=evil\n");
        let (patch, _) = wt.diff().await.unwrap();
        assert!(
            !patch.contains("EXTERNAL") && !patch.contains("CONVERTED"),
            "{patch}"
        );
        assert!(patch.contains("diff --git a/src/sub/only_new.txt b/src/sub/only_new.txt"));
        let merged = wt
            .merge(&mut Default::default(), &[d.path().to_path_buf()])
            .await
            .unwrap();
        assert!(matches!(merged, Merge::Applied { .. }), "{merged:?}");
        assert_merged_in_place(d.path());
        assert_eq!(read(d.path(), ".gitattributes"), "*.txt diff=evil\n");
    }

    /// The user's own gitconfig (HOME) must not leak into the patch either.
    #[cfg(unix)]
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // Serializes env-var mutation.
    async fn user_gitconfig_noprefix_and_external_diff_are_ignored() {
        let _lock = crate::provider::sse::tests::ENV
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home = tempfile::tempdir().unwrap();
        let ext = home.path().join("ext.sh");
        std::fs::write(&ext, "#!/bin/sh\necho EXTERNAL DIFF OUTPUT\n").unwrap();
        std::fs::write(
            home.path().join(".gitconfig"),
            format!(
                "[diff]\n\tnoprefix = true\n\tmnemonicPrefix = true\n\texternal = sh {}\n",
                ext.display()
            ),
        )
        .unwrap();
        let old: Vec<_> = ["HOME", "XDG_CONFIG_HOME"]
            .iter()
            .map(|k| (*k, std::env::var_os(k)))
            .collect();
        std::env::set_var("HOME", home.path());
        std::env::set_var("XDG_CONFIG_HOME", home.path().join("xdg"));
        let d = repo();
        let result = async {
            let wt = create(d.path(), "home").await.unwrap();
            child_edits(&wt);
            wt.merge(&mut Default::default(), &[d.path().to_path_buf()])
                .await
        }
        .await;
        for (k, v) in old {
            match v {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
        let merged = result.unwrap();
        assert!(matches!(merged, Merge::Applied { .. }), "{merged:?}");
        assert_merged_in_place(d.path());
    }

    #[tokio::test]
    async fn patch_outside_the_parents_roots_is_rejected() {
        let d = repo();
        write(d.path(), "sub/in.txt", "in\n");
        write(d.path(), "other/out.txt", "out\n");
        let run = |args: &[&str]| {
            assert!(std::process::Command::new("git")
                .arg("-C")
                .arg(d.path())
                .args(args)
                .status()
                .unwrap()
                .success());
        };
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "more"]);
        let wt = create(d.path(), "scoped").await.unwrap();
        write(&wt.path, "sub/in.txt", "in changed\n");
        write(&wt.path, "other/out.txt", "out changed\n");
        let roots = [d.path().join("sub")];
        let err = wt
            .merge(&mut Default::default(), &roots)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("other/out.txt") && err.contains("outside"),
            "{err}"
        );
        // Nothing written, worktree kept.
        assert_eq!(read(d.path(), "sub/in.txt"), "in\n");
        assert_eq!(read(d.path(), "other/out.txt"), "out\n");
        assert!(wt.path.is_dir());
        // Narrowing the child's work to the root merges fine.
        write(&wt.path, "other/out.txt", "out\n");
        let merged = wt.merge(&mut Default::default(), &roots).await.unwrap();
        assert!(matches!(merged, Merge::Applied { .. }), "{merged:?}");
        assert_eq!(read(d.path(), "sub/in.txt"), "in changed\n");
    }

    #[test]
    fn check_roots_rejects_escapes_and_git_metadata() {
        let repo = Path::new("/r");
        let roots = [PathBuf::from("/r")];
        let ok = |f: &str| check_roots(repo, &[f.to_string()], &roots);
        assert!(ok("src/a.rs").is_ok());
        assert!(ok("../x").is_err());
        assert!(ok("/etc/passwd").is_err());
        assert!(ok(".git/hooks/pre-commit").is_err());
        assert!(ok("a/.git/config").is_err());
        assert!(check_roots(repo, &["src/a.rs".into()], &[PathBuf::from("/r/lib")]).is_err());
    }
}
