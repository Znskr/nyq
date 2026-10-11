//! `nyq pr` and `nyq issue`: draft a GitHub pull request or issue with the
//! configured model from the repository state, review it, and publish it with
//! the GitHub CLI (`gh`).

use std::io::{IsTerminal, Write};
use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};

use crate::provider::Provider;
use crate::types::{Item, Request, StreamEvent};

/// Diff text sent to the model is cut to this many bytes.
const MAX_DIFF_BYTES: usize = 60_000;
/// At most this many commits are listed for a pull request.
const MAX_COMMITS: usize = 50;

const SYSTEM: &str = "You write concise, accurate GitHub pull requests and issues. \
Reply with only the requested draft: no preamble, no code fences around it.";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Workflow {
    PullRequest,
    Issue,
}

impl Workflow {
    fn command(self) -> &'static str {
        match self {
            Workflow::PullRequest => "pr",
            Workflow::Issue => "issue",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Draft {
    pub title: String,
    pub body: String,
}

impl Draft {
    fn render(&self) -> String {
        if self.body.is_empty() {
            format!("{}\n", self.title)
        } else {
            format!("{}\n\n{}\n", self.title, self.body)
        }
    }
}

/// Options shared by `nyq pr` and `nyq issue`.
#[derive(Debug, Default)]
pub struct Options {
    pub context: String,
    /// Publish without asking (still asks before pushing the branch).
    pub publish: bool,
    /// Open the pull request as a draft.
    pub draft: bool,
    /// Base branch for the pull request (default: the repository's default branch).
    pub base: Option<String>,
}

/// Split model output into a title (first non-empty line) and a body (the
/// rest, leading blank lines dropped). A `Title:` label or a leading
/// Markdown heading marker on the title line is removed.
pub fn parse_draft(text: &str) -> Result<Draft> {
    let trimmed = strip_fence(text.trim());
    let (first, rest) = trimmed.split_once('\n').unwrap_or((trimmed, ""));
    let mut title = first.trim();
    for prefix in ["Title:", "title:", "**Title:**", "TITLE:"] {
        if let Some(t) = title.strip_prefix(prefix) {
            title = t.trim();
        }
    }
    let title = title.trim_start_matches('#').trim().trim_matches('*').trim();
    if title.is_empty() {
        bail!("the model's reply has no title line");
    }
    Ok(Draft {
        title: title.to_string(),
        body: rest.trim_start_matches(['\r', '\n', ' ', '\t']).trim_end().to_string(),
    })
}

/// Drop one fence wrapping the whole reply (```` ```markdown ... ``` ````).
fn strip_fence(text: &str) -> &str {
    let Some(rest) = text.strip_prefix("```") else { return text };
    let Some((_, inner)) = rest.split_once('\n') else { return text };
    inner.strip_suffix("```").map(str::trim).unwrap_or(text)
}

/// Run a read-only git command in `cwd`; `None` when it fails or prints nothing.
fn git(cwd: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("--no-optional-locks")
        .args(args)
        .current_dir(cwd)
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout).trim_end().to_string();
    (out.status.success() && !text.trim().is_empty()).then_some(text)
}

pub fn is_git_repo(cwd: &Path) -> bool {
    git(cwd, &["rev-parse", "--is-inside-work-tree"]).as_deref() == Some("true")
}

/// The repository's default branch: `origin/HEAD`, else a local `main` or `master`.
pub fn default_branch(cwd: &Path) -> Option<String> {
    if let Some(head) = git(cwd, &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"]) {
        return Some(head.strip_prefix("origin/").unwrap_or(&head).to_string());
    }
    ["main", "master"]
        .into_iter()
        .find(|b| git(cwd, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{b}")]).is_some())
        .map(String::from)
}

/// What a pull request would contain.
#[derive(Debug)]
pub struct PrContext {
    pub branch: String,
    pub base: String,
    pub commits: String,
    pub stat: String,
    pub diff: String,
    /// `git status --short` when the working tree has uncommitted changes.
    pub uncommitted: Option<String>,
}

/// Gather the current branch's commits and diff against its merge base with `base`
/// (default: the repository's default branch; `origin/<base>` is preferred when it exists).
pub fn gather_pr(cwd: &Path, base: Option<&str>) -> Result<PrContext> {
    if !is_git_repo(cwd) {
        bail!("nyq pr: requires running inside a git repository");
    }
    let branch = git(cwd, &["branch", "--show-current"]).context("nyq pr: HEAD is detached; check out a branch first")?;
    let base = match base {
        Some(b) => b.to_string(),
        None => default_branch(cwd).context("nyq pr: could not find the default branch; pass --base BRANCH")?,
    };
    if base == branch {
        bail!("nyq pr: you are on the base branch `{base}`; create a feature branch first (or pass --base)");
    }
    let remote = format!("origin/{base}");
    let base_ref = if git(cwd, &["rev-parse", "--verify", "--quiet", &remote]).is_some() { remote } else { base.clone() };
    let merge_base = git(cwd, &["merge-base", "HEAD", &base_ref])
        .with_context(|| format!("nyq pr: no common history between `{branch}` and `{base_ref}`"))?;
    let range = format!("{merge_base}..HEAD");
    let commits = git(cwd, &["log", "--oneline", "--no-decorate", &format!("-{MAX_COMMITS}"), &range])
        .with_context(|| format!("nyq pr: `{branch}` has no commits ahead of `{base_ref}`"))?;
    let stat = git(cwd, &["diff", "--stat", &merge_base, "HEAD"]).unwrap_or_default();
    let diff = truncate(git(cwd, &["diff", &merge_base, "HEAD"]).unwrap_or_default(), MAX_DIFF_BYTES);
    let uncommitted = git(cwd, &["status", "--short"]);
    Ok(PrContext { branch, base, commits, stat, diff, uncommitted })
}

fn truncate(mut text: String, max: usize) -> String {
    if text.len() > max {
        let mut end = max;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push_str("\n[diff truncated]");
    }
    text
}

/// A short repository snapshot for issues; works outside a git repository.
pub fn snapshot(cwd: &Path) -> String {
    let section = |args: &[&str], fallback: &str| git(cwd, args).unwrap_or_else(|| fallback.to_string());
    format!(
        "Git snapshot\nBranch: {}\n\nStatus:\n{}\n\nRecent commits:\n{}\n\nStaged diff stat:\n{}\n\nUnstaged diff stat:\n{}\n",
        section(&["branch", "--show-current"], "unavailable"),
        section(&["status", "--short", "--branch"], "unavailable"),
        section(&["log", "--oneline", "-5"], "unavailable"),
        section(&["diff", "--stat", "--cached"], "none"),
        section(&["diff", "--stat"], "none"),
    )
}

fn context_clause(context: &str) -> String {
    let context = context.trim();
    if context.is_empty() { String::new() } else { format!("Additional context: {context}.\n\n") }
}

pub fn pr_prompt(ctx: &PrContext, context: &str) -> String {
    format!(
        "Draft a GitHub pull request for branch `{}` into `{}`. Write it in the language of the additional context, or English if there is none.\n\n{}\
Commits:\n{}\n\nDiff stat:\n{}\n\nDiff:\n{}\n\n\
Return only: Title, blank line, then a GitHub-flavored Markdown body with sections '## Summary' and '## Testing'. \
Keep the title under 72 characters and describe what changed and why; do not invent tests that the diff does not show.",
        ctx.branch, ctx.base, context_clause(context), ctx.commits, ctx.stat, ctx.diff
    )
}

pub fn issue_prompt(snapshot: &str, context: &str) -> String {
    format!(
        "Draft a GitHub issue from the context below. Write it in the language of the additional context, or English if there is none.\n\n{}\
Repository state:\n{snapshot}\n\
Return only: Title, blank line, then a GitHub-flavored Markdown body with sections '## Summary', '## Steps to Reproduce', '## Expected', and '## Actual'. \
Keep the title under 72 characters; leave a section brief rather than inventing details.",
        context_clause(context)
    )
}

/// Ask the model for a draft (no tools, nothing stored in the transcript).
/// Token use is recorded under `usage_id` (`cli-pr` or `cli-issue`).
pub async fn ask_model(home: &Path, provider: &dyn Provider, prompt: &str, usage_id: &str) -> Result<Draft> {
    let items = [Item::user(prompt)];
    let req = Request { system: SYSTEM, items: &items, tools: &[] };
    let mut ignore = |_: StreamEvent| {};
    let response = provider.stream(req, &mut ignore).await?;
    if let Some(usage) = response.usage {
        if let Err(e) = crate::usage::append(
            home,
            usage_id,
            provider.name(),
            provider.pricing_provider(),
            provider.model(),
            usage,
        ) {
            eprintln!("nyq: could not save usage: {e:#}");
        }
    }
    if response.truncated {
        anyhow::bail!("the draft was cut off at the output limit");
    }
    let text: String = response
        .items
        .iter()
        .filter_map(|item| match item {
            Item::Assistant { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    parse_draft(&text)
}

/// `gh pr|issue create` arguments for a draft.
pub fn publish_args(workflow: Workflow, draft: &Draft, opts: &Options) -> Vec<String> {
    let mut args: Vec<String> = [workflow.command(), "create", "--title", &draft.title, "--body", &draft.body]
        .into_iter()
        .map(String::from)
        .collect();
    if workflow == Workflow::PullRequest {
        if let Some(base) = &opts.base {
            args.extend(["--base".into(), base.clone()]);
        }
        if opts.draft {
            args.push("--draft".into());
        }
    }
    args
}

/// Turn a finished `gh` run into the created URL or an error message.
fn publish_result(status_ok: bool, stdout: &[u8], stderr: &[u8]) -> Result<String> {
    let stdout = String::from_utf8_lossy(stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(stderr).trim().to_string();
    if !status_ok {
        bail!("{}", if stderr.is_empty() { "gh command failed".to_string() } else { stderr });
    }
    Ok(if stdout.is_empty() { "created successfully".to_string() } else { stdout })
}

fn publish(cwd: &Path, workflow: Workflow, draft: &Draft, opts: &Options) -> Result<String> {
    let out = match Command::new("gh").args(publish_args(workflow, draft, opts)).current_dir(cwd).output() {
        Ok(out) => out,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => bail!("gh CLI not found in PATH"),
        Err(e) => return Err(e).context("running gh"),
    };
    publish_result(out.status.success(), &out.stdout, &out.stderr)
}

/// Why the branch must be pushed before a pull request can be opened, if it must.
fn push_needed(cwd: &Path, branch: &str) -> Option<String> {
    if git(cwd, &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"]).is_none() {
        return Some(format!("`{branch}` has no upstream branch"));
    }
    match git(cwd, &["rev-list", "--count", "@{u}..HEAD"]).and_then(|n| n.parse::<u64>().ok()) {
        Some(0) | None => None,
        Some(n) => Some(format!("`{branch}` has {n} unpushed commit{}", if n == 1 { "" } else { "s" })),
    }
}

fn push(cwd: &Path, branch: &str) -> Result<()> {
    let has_upstream = git(cwd, &["rev-parse", "--abbrev-ref", "@{u}"]).is_some();
    let mut cmd = Command::new("git");
    if has_upstream {
        cmd.arg("push");
    } else {
        cmd.args(["push", "-u", "origin", branch]);
    }
    let status = cmd.current_dir(cwd).status().context("running git push")?;
    anyhow::ensure!(status.success(), "git push failed");
    Ok(())
}

fn ask(question: &str) -> Result<String> {
    eprint!("{question}");
    std::io::stderr().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(line.trim().to_lowercase())
}

/// Open the draft in `$VISUAL`/`$EDITOR` and parse it back.
fn edit(draft: &Draft) -> Result<Draft> {
    let path = std::env::temp_dir().join(format!("nyq-draft-{}.md", uuid::Uuid::new_v4()));
    std::fs::write(&path, draft.render())?;
    let editor = std::env::var("VISUAL")
        .ok()
        .or_else(|| std::env::var("EDITOR").ok())
        .filter(|e| !e.trim().is_empty())
        .unwrap_or_else(|| if cfg!(windows) { "notepad".into() } else { "vi".into() });
    // The editor value may carry arguments (`code --wait`); let the shell split it.
    #[cfg(windows)]
    let status = Command::new("cmd").arg("/C").arg(format!("{editor} \"{}\"", path.display())).status();
    #[cfg(not(windows))]
    let status = Command::new("sh").arg("-c").arg(format!("{editor} \"$1\"")).arg("sh").arg(&path).status();
    let result = (|| {
        anyhow::ensure!(status.with_context(|| format!("running {editor}"))?.success(), "{editor} exited with an error");
        parse_draft(&std::fs::read_to_string(&path)?)
    })();
    let _ = std::fs::remove_file(&path);
    result
}

/// `nyq pr` / `nyq issue`.
pub async fn run(cwd: &Path, home: &Path, provider: &dyn Provider, workflow: Workflow, opts: Options) -> Result<()> {
    let name = workflow.command();
    let (prompt, branch) = match workflow {
        Workflow::PullRequest => {
            let ctx = gather_pr(cwd, opts.base.as_deref())?;
            if let Some(changes) = &ctx.uncommitted {
                let n = changes.lines().count();
                eprintln!("note: {n} uncommitted change{} not part of the pull request", if n == 1 { " is" } else { "s are" });
            }
            eprintln!("Drafting a pull request for {} into {} with {}...", ctx.branch, ctx.base, provider.model());
            (pr_prompt(&ctx, &opts.context), Some(ctx.branch))
        }
        Workflow::Issue => {
            eprintln!("Drafting an issue with {}...", provider.model());
            (issue_prompt(&snapshot(cwd), &opts.context), None)
        }
    };
    let usage_id = match workflow {
        Workflow::PullRequest => "cli-pr",
        Workflow::Issue => "cli-issue",
    };
    let mut draft = ask_model(home, provider, &prompt, usage_id).await.with_context(|| format!("nyq {name}: drafting failed"))?;
    let tty = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
    loop {
        print!("\n{}", draft.render());
        std::io::stdout().flush()?;
        if opts.publish {
            break;
        }
        if !tty {
            return Ok(());
        }
        match ask("\n[p]ublish, [e]dit, [c]ancel? ")?.as_str() {
            "p" | "publish" | "y" | "yes" => break,
            "e" | "edit" => match edit(&draft) {
                Ok(d) => draft = d,
                Err(e) => eprintln!("error: {e:#}"),
            },
            _ => {
                eprintln!("Cancelled; nothing was published.");
                return Ok(());
            }
        }
    }
    if let Some((branch, why)) = branch.and_then(|b| push_needed(cwd, &b).map(|why| (b, why))) {
        if !tty {
            bail!("nyq pr: {why}; push it first, then run nyq pr --publish again");
        }
        if !matches!(ask(&format!("{why}. Push it to origin now? [y/N] "))?.as_str(), "y" | "yes") {
            eprintln!("Not pushed; nothing was published.");
            return Ok(());
        }
        push(cwd, &branch)?;
    }
    let url = publish(cwd, workflow, &draft, &opts).map_err(|e| anyhow::anyhow!("nyq {name}: {e:#}"))?;
    println!("{url}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_draft_extracts_title_and_body() {
        let d = parse_draft("Title\n\n## Summary\nhello").unwrap();
        assert_eq!(d, Draft { title: "Title".into(), body: "## Summary\nhello".into() });
        assert!(parse_draft(" \n\t ").is_err());
        assert_eq!(parse_draft("Title only").unwrap().body, "");
        let d = parse_draft(" \r\n  Title with space  \r\n\r\n  - one\r\n  - two\r\n\n").unwrap();
        assert_eq!(d.title, "Title with space");
        assert_eq!(d.body, "- one\r\n  - two");
    }

    #[test]
    fn parse_draft_strips_labels_headings_and_fences() {
        assert_eq!(parse_draft("Title: Fix it\n\nbody").unwrap().title, "Fix it");
        assert_eq!(parse_draft("# Fix it\n\nbody").unwrap().title, "Fix it");
        assert_eq!(parse_draft("**Fix it**\nbody").unwrap().title, "Fix it");
        let d = parse_draft("```markdown\nFix it\n\n## Summary\nx\n```").unwrap();
        assert_eq!(d, Draft { title: "Fix it".into(), body: "## Summary\nx".into() });
        assert!(parse_draft("Title:\nbody").is_err());
    }

    #[tokio::test]
    async fn ask_model_records_usage_and_rejects_a_cut_off_draft() {
        struct Fake {
            truncated: bool,
        }
        #[async_trait::async_trait]
        impl crate::provider::Provider for Fake {
            fn name(&self) -> &str {
                "fake"
            }
            fn model(&self) -> &str {
                "m"
            }
            fn set_model(&mut self, _: String) {}
            fn fork(&self) -> Box<dyn crate::provider::Provider> {
                unimplemented!()
            }
            async fn stream(
                &self,
                _: Request<'_>,
                _: crate::provider::EventSink<'_>,
            ) -> Result<crate::types::Response> {
                Ok(crate::types::Response {
                    truncated: self.truncated,
                    items: vec![Item::Assistant {
                        text: "Title\n\n## Summary\nhello".into(),
                    }],
                    usage: Some(crate::types::Usage {
                        input_tokens: 3,
                        output_tokens: 4,
                        cached_tokens: 1,
                        ..Default::default()
                    }),
                })
            }
        }
        let home = tempfile::tempdir().unwrap();
        let draft = ask_model(home.path(), &Fake { truncated: false }, "prompt", "cli-pr")
            .await
            .unwrap();
        assert_eq!(draft.title, "Title");
        let log = std::fs::read_to_string(home.path().join("usage.jsonl")).unwrap();
        assert!(log.contains("cli-pr"), "{log}");
        assert!(log.contains("\"input\":3"), "{log}");
        assert!(log.contains("\"output\":4"), "{log}");
        let err = ask_model(home.path(), &Fake { truncated: true }, "prompt", "cli-pr")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("cut off"), "{err}");
    }

    #[test]
    fn edited_draft_round_trips() {
        let d = Draft { title: "T".into(), body: "## Summary\n- a".into() };
        assert_eq!(parse_draft(&d.render()).unwrap(), d);
    }

    #[test]
    fn publish_args_map_flags() {
        let d = Draft { title: "A title".into(), body: "Body".into() };
        let mut opts = Options::default();
        assert_eq!(publish_args(Workflow::Issue, &d, &opts), ["issue", "create", "--title", "A title", "--body", "Body"]);
        opts.base = Some("dev".into());
        opts.draft = true;
        assert_eq!(
            publish_args(Workflow::PullRequest, &d, &opts),
            ["pr", "create", "--title", "A title", "--body", "Body", "--base", "dev", "--draft"]
        );
        // Issue creation ignores the pull-request-only flags.
        assert_eq!(publish_args(Workflow::Issue, &d, &opts).len(), 6);
    }

    #[test]
    fn publish_results_follow_gh_output() {
        assert_eq!(publish_result(true, b" \nhttps://github.com/o/r/pull/1\n", b"").unwrap(), "https://github.com/o/r/pull/1");
        assert_eq!(publish_result(true, b" ", b"").unwrap(), "created successfully");
        assert_eq!(publish_result(false, b"x", b"\nfailed to create\n").unwrap_err().to_string(), "failed to create");
        assert_eq!(publish_result(false, b"x", b" ").unwrap_err().to_string(), "gh command failed");
    }

    #[test]
    fn prompts_carry_context_and_sections() {
        let ctx = PrContext {
            branch: "feature".into(),
            base: "main".into(),
            commits: "abc first".into(),
            stat: " a | 1 +".into(),
            diff: "+x".into(),
            uncommitted: None,
        };
        let p = pr_prompt(&ctx, " ready for review \n");
        assert!(p.contains("Additional context: ready for review."));
        assert!(p.contains("`feature` into `main`") && p.contains("abc first") && p.contains("+x"));
        assert!(p.contains("## Summary") && p.contains("## Testing") && !p.contains("## Steps to Reproduce"));
        let p = issue_prompt("Git snapshot\nBranch: unavailable\n", " \t");
        assert!(!p.contains("Additional context:"));
        for s in ["## Summary", "## Steps to Reproduce", "## Expected", "## Actual", "Branch: unavailable"] {
            assert!(p.contains(s), "{s}");
        }
    }

    #[test]
    fn truncation_respects_char_boundaries() {
        let t = truncate("ééé".into(), 3);
        assert!(t.starts_with('é') && t.ends_with("[diff truncated]"));
        assert_eq!(truncate("short".into(), 10), "short");
    }

    fn sh(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com", "-c", "commit.gpgsign=false", "-c", "init.defaultBranch=main"])
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", if cfg!(windows) { "NUL" } else { "/dev/null" })
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }

    #[test]
    fn gathers_branch_diff_against_merge_base() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        sh(dir, &["init", "-q"]);
        sh(dir, &["checkout", "-q", "-b", "main"]);
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        sh(dir, &["add", "."]);
        sh(dir, &["commit", "-q", "-m", "base commit"]);
        sh(dir, &["checkout", "-q", "-b", "feature"]);
        std::fs::write(dir.join("a.txt"), "one\ntwo\n").unwrap();
        sh(dir, &["commit", "-q", "-am", "add two"]);
        // A later commit on main must not show up in the feature diff.
        sh(dir, &["checkout", "-q", "main"]);
        std::fs::write(dir.join("b.txt"), "main only\n").unwrap();
        sh(dir, &["add", "."]);
        sh(dir, &["commit", "-q", "-m", "main moves on"]);
        sh(dir, &["checkout", "-q", "feature"]);
        std::fs::write(dir.join("dirty.txt"), "x").unwrap();

        assert!(is_git_repo(dir));
        assert_eq!(default_branch(dir).as_deref(), Some("main"));
        let ctx = gather_pr(dir, None).unwrap();
        assert_eq!((ctx.branch.as_str(), ctx.base.as_str()), ("feature", "main"));
        assert!(ctx.commits.contains("add two") && !ctx.commits.contains("base commit") && !ctx.commits.contains("main moves on"));
        assert!(ctx.diff.contains("+two") && !ctx.diff.contains("main only"));
        assert!(ctx.stat.contains("a.txt"));
        assert!(ctx.uncommitted.unwrap().contains("dirty.txt"));
        assert!(snapshot(dir).contains("Branch: feature"));

        // On the base branch itself there is nothing to propose.
        sh(dir, &["checkout", "-q", "main"]);
        assert!(gather_pr(dir, None).is_err());
        // An explicit base with no new commits is reported.
        sh(dir, &["checkout", "-q", "-b", "empty"]);
        assert!(gather_pr(dir, Some("main")).unwrap_err().to_string().contains("no commits ahead"));
    }

    #[test]
    fn outside_a_repository() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(!is_git_repo(tmp.path()));
        assert!(gather_pr(tmp.path(), None).unwrap_err().to_string().contains("git repository"));
        let snap = snapshot(tmp.path());
        assert!(snap.starts_with("Git snapshot\nBranch: unavailable\n") && snap.contains("Staged diff stat:\nnone"));
    }
}
