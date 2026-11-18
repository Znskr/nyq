//! Off-task drift: the turn keeps fetching, installing or poking around
//! outside the workspace without changing anything in it.
//!
//! This extends the W9 stall handling (`task.rs`) to plain turns. Every tool
//! call is classified ([`classify`]): did it write a file inside the
//! workspace, and was it off-site (a download or install, a web tool, or work
//! in a temporary or home directory outside the workspace)? After
//! [`QUIET_ROUNDS`] consecutive tool rounds without a workspace write, with
//! enough of their calls off-site, the turn gets a steer back to the code. In
//! a typed task the steer is a [`StallReason::OffTask`] stall, so the
//! escalation ladder handles it like any other; in a plain turn it is sent
//! as a note directly. A change the shell made in a git workspace (an edit
//! with `sed -i`, a generated file) counts as a write, so a turn editing
//! through the shell is not interrupted.

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

use super::{Agent, Ui};
use crate::task::StallReason;
use crate::tools::{Concurrency, ToolOutput};
use crate::types::Item;

/// Consecutive tool rounds without a workspace write before a steer.
pub(crate) const QUIET_ROUNDS: usize = 15;
/// Off-site calls those rounds must include (and at least a third of them).
const MIN_OFFSITE: usize = 5;
/// Steers per turn.
const MAX_STEERS: usize = 3;

/// Per-turn drift state (reset at the start of each turn).
#[derive(Default)]
pub struct Drift {
    /// Tool rounds since the last workspace write.
    quiet: usize,
    /// Calls in those rounds, and how many were off-site.
    calls: usize,
    offsite: usize,
    /// The current round so far.
    round_wrote: bool,
    round_calls: usize,
    round_offsite: usize,
    steers: usize,
    /// `git status`/`git diff` fingerprint at the start of the quiet stretch
    /// (None: not a git workspace, or not taken yet).
    baseline: Option<u64>,
}

/// Fetching or installing from the network.
static FETCH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?x)\b(
            curl | wget | aria2c
          | git \s+ (clone|fetch)
          | (pip3?|python[0-9.]*\s+-m\s+pip) \s+ (download|install)
          | uv \s+ (pip|add|venv|tool|python\s+install)
          | (npm|pnpm) \s+ (i|install|ci|add) | yarn \s+ add
          | cargo \s+ install | go \s+ (get|install)
          | (apt-get|apt|dnf|yum|brew|conda|mamba) \s+ (install|download)
        )\b",
    )
    .expect("fetch regex")
});
/// `cd` into an absolute directory, or a search from the filesystem root.
static CD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?:^|[\s;&|(])cd\s+["']?(/[^\s;&|)"']*)"#).expect("cd regex"));
static ROOT_FIND: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\bfind\s+/(?:\s|$)").expect("find regex"));
/// Absolute paths a command mentions.
static ABS_PATH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?:^|[\s'"=:;|&(<>])(/[A-Za-z0-9_.@+~-][^\s'";|&)<>]*)"#).expect("path regex")
});

/// What one tool call did, for drift: (wrote inside the workspace, off-site).
pub(crate) fn classify(
    name: &str,
    arguments: &Value,
    out: &ToolOutput,
    ran: Option<(&Concurrency, &[PathBuf])>,
    cwd: &Path,
    home: Option<&Path>,
) -> (bool, bool) {
    let inside = |p: &Path| p.starts_with(cwd);
    let wrote = !out.is_error
        && (out.changed
            || matches!(ran, Some((Concurrency::WriteFs(_), writes)) if writes.iter().any(|w| inside(w))));
    if matches!(name, "web_fetch" | "web_search") {
        return (wrote, true);
    }
    // A temporary or home directory outside the workspace.
    let elsewhere = |p: &str| {
        let path = Path::new(p);
        !inside(path)
            && (path.starts_with("/tmp")
                || path.starts_with("/var/tmp")
                || home.is_some_and(|h| path.starts_with(h) && !cwd.starts_with(path)))
    };
    let offsite = if let Some(cmd) = arguments.get("command").and_then(Value::as_str) {
        FETCH.is_match(cmd)
            || ROOT_FIND.is_match(cmd)
            || CD.captures_iter(cmd).any(|c| !inside(Path::new(&c[1])))
            || ABS_PATH.captures_iter(cmd).any(|c| elsewhere(&c[1]))
    } else {
        ["path", "file_path", "dir"]
            .iter()
            .filter_map(|k| arguments.get(*k).and_then(Value::as_str))
            .any(|p| p.starts_with('/') && !inside(Path::new(p)))
    };
    (wrote, offsite)
}

/// A hash of the workspace's git state (status and unstaged/staged diff), to
/// notice changes made through the shell. None outside a git work tree.
fn fingerprint(cwd: &Path) -> Option<u64> {
    use std::hash::{Hash, Hasher};
    let run = |args: &[&str]| {
        std::process::Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| o.stdout)
    };
    let status = run(&["status", "--porcelain=v1", "--untracked-files=all"])?;
    let diff = run(&["diff", "--no-ext-diff", "HEAD"]).unwrap_or_default();
    let mut h = std::collections::hash_map::DefaultHasher::new();
    status.hash(&mut h);
    diff.hash(&mut h);
    Some(h.finish())
}

/// The steer, in the voice of the existing stall notes.
pub(crate) fn steer_note(quiet: usize, offsite: usize, cwd: &Path) -> String {
    format!(
        "[Steer] The last {quiet} steps changed nothing in the workspace ({}); {offsite} of their tool calls went to \
         downloading or installing, or to files outside it. The work is in this workspace: make your best edit to \
         the code here now, then check it with what is available locally. If something truly blocks you, say what \
         it is instead of looking further afield.",
        cwd.display()
    )
}

impl Agent {
    /// Start of a turn: a fresh drift state.
    pub(super) fn drift_begin(&mut self) {
        self.task.drift = Drift::default();
    }

    /// One tool call's result (next to [`Agent::task_observe`]).
    pub(super) fn drift_observe(
        &mut self,
        name: &str,
        arguments: &str,
        out: &ToolOutput,
        ran: Option<(&Concurrency, &[PathBuf])>,
    ) {
        let args: Value = serde_json::from_str(arguments).unwrap_or_default();
        let home = dirs::home_dir();
        let (wrote, offsite) = classify(name, &args, out, ran, &self.cwd, home.as_deref());
        let d = &mut self.task.drift;
        d.round_calls += 1;
        d.round_wrote |= wrote;
        d.round_offsite += usize::from(offsite);
    }

    /// After a tool round: count it, and steer when the turn has drifted.
    pub(super) fn drift_after_round(&mut self, ui: &mut dyn Ui) {
        let cwd = self.cwd.clone();
        let d = &mut self.task.drift;
        let (wrote, calls, offsite) = (d.round_wrote, d.round_calls, d.round_offsite);
        (d.round_wrote, d.round_calls, d.round_offsite) = (false, 0, 0);
        if wrote {
            (d.quiet, d.calls, d.offsite, d.baseline) = (0, 0, 0, None);
            return;
        }
        if d.quiet == 0 {
            d.baseline = fingerprint(&cwd);
        }
        d.quiet += 1;
        d.calls += calls;
        d.offsite += offsite;
        if d.quiet < QUIET_ROUNDS
            || d.steers >= MAX_STEERS
            || d.offsite < MIN_OFFSITE.max(d.calls.div_ceil(3))
        {
            return;
        }
        // Edits through the shell count as writes.
        let now = fingerprint(&cwd);
        let changed = now.is_some() && now != d.baseline;
        let (quiet, offsite) = (d.quiet, d.offsite);
        (d.quiet, d.calls, d.offsite, d.baseline) = (0, 0, 0, now);
        if changed {
            return;
        }
        d.steers += 1;
        let note = steer_note(quiet, offsite, &cwd);
        crate::trace::log("task", || {
            format!("off-task drift after {quiet} quiet rounds ({offsite} off-site calls)")
        });
        // A typed task climbs its escalation ladder; a plain turn gets the steer.
        if let Some(st) = self.task_state_mut() {
            st.stall(StallReason::OffTask, note);
        } else {
            ui.notice(&format!(
                "No workspace changes in {quiet} steps, mostly off-site work: steering back to the code."
            ));
            self.push_counted(Item::user(note));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ok() -> ToolOutput {
        ToolOutput::ok("fine")
    }

    #[test]
    fn classifies_downloads_temp_dirs_and_workspace_writes() {
        let cwd = Path::new("/work/repo");
        let home = Path::new("/home/u");
        let c = |name: &str, args: Value| classify(name, &args, &ok(), None, cwd, Some(home));
        assert_eq!(
            c(
                "bash",
                json!({"command": "pip download django==3.2 -d /tmp/dl"})
            ),
            (false, true)
        );
        assert_eq!(
            c(
                "bash",
                json!({"command": "curl -sL https://example.com/x.py"})
            ),
            (false, true)
        );
        assert_eq!(
            c("bash", json!({"command": "cd /tmp/copy && grep -rn foo ."})),
            (false, true)
        );
        assert_eq!(
            c("bash", json!({"command": "find / -name '*.py'"})),
            (false, true)
        );
        assert_eq!(
            c("bash", json!({"command": "ls /home/u/.cache/pip"})),
            (false, true)
        );
        assert_eq!(c("web_fetch", json!({"url": "https://x"})), (false, true));
        assert_eq!(
            c("read_file", json!({"path": "/tmp/x/models.py"})),
            (false, true)
        );
        // Ordinary work in the workspace is not off-site.
        assert_eq!(
            c(
                "bash",
                json!({"command": "python -m pytest tests/test_x.py"})
            ),
            (false, false)
        );
        assert_eq!(
            c("bash", json!({"command": "cd /work/repo/src && ls"})),
            (false, false)
        );
        assert_eq!(
            c(
                "bash",
                json!({"command": "/usr/bin/python3 setup.py build"})
            ),
            (false, false)
        );
        assert_eq!(
            c("read_file", json!({"path": "src/lib.rs"})),
            (false, false)
        );
        // A file-tool write inside the workspace is a write.
        let writes = [PathBuf::from("/work/repo/src/lib.rs")];
        let class = Concurrency::WriteFs(writes.to_vec());
        let ran = Some((&class, &writes[..]));
        assert_eq!(
            classify(
                "edit_file",
                &json!({"path": "src/lib.rs"}),
                &ok(),
                ran,
                cwd,
                Some(home)
            ),
            (true, false)
        );
        // ...unless it failed.
        assert_eq!(
            classify(
                "edit_file",
                &json!({"path": "src/lib.rs"}),
                &ToolOutput::err("no"),
                ran,
                cwd,
                Some(home)
            ),
            (false, false)
        );
    }

    use crate::agent::test_support::*;
    use crate::agent::TurnEnd;
    use crate::types::Response;
    use tokio_util::sync::CancellationToken;

    async fn run_rounds(command: &str, rounds: usize) -> (crate::agent::Agent, Vec<String>) {
        let (tool, _) = probe("mcp__x__sh", unknown_fx, false);
        let mut script: Vec<Response> = (0..rounds)
            .map(|_| reply(vec![call("a", "mcp__x__sh", json!({"command": command}))]))
            .collect();
        script.push(Response {
            truncated: false,
            items: vec![Item::Assistant {
                text: "done".into(),
            }],
            usage: None,
        });
        let mut agent = scripted(script);
        agent.tools.add(Box::new(tool));
        agent.task.step_cap = Some(None);
        let mut ui = NoticeCatch(Vec::new());
        let end = agent
            .turn(Item::user("go"), &mut ui, CancellationToken::new())
            .await
            .unwrap();
        assert!(matches!(end, TurnEnd::Done));
        (agent, ui.0)
    }

    fn steers(agent: &crate::agent::Agent) -> usize {
        agent
            .items
            .iter()
            .filter(|i| matches!(i, Item::User { text, .. } if text.starts_with("[Steer]")))
            .count()
    }

    #[tokio::test]
    async fn quiet_off_site_rounds_get_one_steer() {
        let (agent, notices) = run_rounds(
            "curl -sL https://example.com/pkg.tar.gz -o /tmp/pkg.tgz",
            QUIET_ROUNDS + 1,
        )
        .await;
        assert_eq!(steers(&agent), 1);
        assert!(
            notices
                .iter()
                .any(|n| n.contains("steering back to the code")),
            "{notices:?}"
        );
    }

    #[tokio::test]
    async fn quiet_local_rounds_are_left_alone() {
        let (agent, _) = run_rounds("python -m pytest -q", QUIET_ROUNDS + 5).await;
        assert_eq!(steers(&agent), 0);
    }

    #[test]
    fn the_steer_names_the_workspace_and_asks_for_an_edit() {
        let note = steer_note(15, 9, Path::new("/w"));
        assert!(
            note.starts_with("[Steer]")
                && note.contains("(/w)")
                && note.contains("make your best edit")
        );
    }
}
