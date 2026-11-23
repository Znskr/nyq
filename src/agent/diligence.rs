//! Diligence for code-changing turns, scaled by the agent effort setting
//! (`agent_effort`: `low`, `medium`, `high`).
//!
//! Plausible but wrong changes mostly come from turns that end without
//! running the changed code, that ship a test failure they saw, or that fix
//! where a problem shows up instead of where it starts. Three generic
//! behaviours address that, using only what the turn itself did:
//!
//! * **approach** (medium and up): when a request reads like a bug to fix, one
//!   short note after it: reproduce first, take the expected behaviour from
//!   the code, set up the project's environment, fix the cause, and work from
//!   the workspace;
//! * **verify** (medium and up): when the model is about to answer and the
//!   turn changed code, it is sent back if nothing ran the code since the last
//!   change, or if the last such run reported failures (medium: once, high: up
//!   to three times, never twice for the same state of the code);
//! * **review** (high): once, a self-review of the diff before the final
//!   answer (source of the problem, other call sites, edge cases,
//!   preconditions, compatibility, honest reporting).
//!
//! A run is a shell command that executes code (tests, scripts, `python -c`,
//! build tools: [`runs_code`]); a change is a file tool's write or a change in
//! the git state, so edits made through the shell count too. Nothing is added
//! to the system prompt or to turns that change no code. Headless runs (`-p`)
//! default to medium, interactive turns to low (off); `agent_effort` or
//! `NYQ_AGENT_EFFORT` sets it for both. Subagents and typed tasks (which end
//! through `submit` and their validators) are not gated.

use std::path::PathBuf;
use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

use super::{Agent, Ui};
use crate::tools::{Concurrency, ToolOutput};
use crate::types::Item;

/// How much a turn checks its own code changes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum Effort {
    #[default]
    Low,
    Medium,
    High,
}

impl Effort {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "low" | "off" | "none" => Some(Self::Low),
            "medium" | "normal" => Some(Self::Medium),
            "high" | "max" => Some(Self::High),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }

    /// Verify/failure notes per turn.
    fn verify_notes(self) -> usize {
        match self {
            Self::Low => 0,
            Self::Medium => 1,
            Self::High => 3,
        }
    }
}

/// The configured agent effort and where it came from: `NYQ_AGENT_EFFORT`,
/// else `agent_effort` in settings; None when unset (or not a known level).
pub fn effort_setting() -> (Option<Effort>, &'static str) {
    if let Some(e) = std::env::var("NYQ_AGENT_EFFORT")
        .ok()
        .as_deref()
        .and_then(Effort::parse)
    {
        return (Some(e), "NYQ_AGENT_EFFORT");
    }
    match crate::settings::current()
        .agent_effort
        .as_deref()
        .and_then(Effort::parse)
    {
        Some(e) => (Some(e), "settings"),
        None => (None, "default"),
    }
}

/// The setting for display (`--dump-config`, doctor).
pub fn describe_effort(setting: Option<Effort>) -> String {
    setting.map_or_else(
        || "auto (medium headless, low interactive)".into(),
        |e| e.name().into(),
    )
}

#[derive(Clone, Debug)]
struct Run {
    /// Git state right after the run (None outside a git work tree).
    after: Option<u64>,
    failed: bool,
}

/// Per-turn state (reset at the start of each turn).
#[derive(Default)]
pub struct Diligence {
    effort: Effort,
    active: bool,
    /// Git state at the start of the turn.
    baseline: Option<u64>,
    /// A file tool changed a workspace file this turn / since the last run.
    tool_wrote: bool,
    wrote_since_run: bool,
    last_run: Option<Run>,
    notes: usize,
    /// Git state when the last verify note was sent: the same state does not
    /// get a second one (the model said why it could not run anything).
    noted_at: Option<Option<u64>>,
    reviewed: bool,
}

/// Wrappers and launchers that run the command after them.
const WRAPPERS: &[&str] = &[
    "timeout", "time", "env", "nice", "stdbuf", "xvfb-run", "sudo", "command", "exec",
];
/// Runners whose next word is `run` before the command (`uv run pytest`).
const RUN_PREFIXES: &[&str] = &["uv", "poetry", "pipenv", "pdm", "hatch", "rye", "pixi"];
/// `python -m` modules that do not run the project's code.
const NOT_RUNS: &[&str] = &[
    "pip",
    "py_compile",
    "compileall",
    "venv",
    "virtualenv",
    "ensurepip",
    "site",
    "json.tool",
    "http.server",
    "pydoc",
];

/// Whether a shell command runs code: tests, scripts, a program built from
/// the workspace. Reading, searching, editing and installing do not count.
pub fn runs_code(cmd: &str) -> bool {
    cmd.split(['\n', ';', '&', '|', '(', ')'])
        .any(|seg| segment_runs(seg.trim()))
}

fn segment_runs(seg: &str) -> bool {
    let words: Vec<&str> = seg.split_whitespace().collect();
    let mut i = 0;
    while i < words.len() {
        let w = words[i];
        if w.contains('=') && !w.starts_with('-') && !w.starts_with("./") {
            i += 1; // VAR=value
        } else if WRAPPERS.contains(&w) {
            i += 1;
            // timeout's duration, env's -u NAME and similar options.
            while i < words.len()
                && (words[i].starts_with('-')
                    || words[i].chars().next().is_some_and(|c| c.is_ascii_digit()))
            {
                i += 1;
            }
        } else if RUN_PREFIXES.contains(&w) && words.get(i + 1) == Some(&"run") {
            i += 2;
        } else {
            break;
        }
    }
    let Some(first) = words.get(i) else {
        return false;
    };
    let rest = &words[i + 1..];
    let prog = first.rsplit('/').next().unwrap_or(first);
    let arg = |n: usize| rest.get(n).copied().unwrap_or("");
    if prog.starts_with("python") || prog.starts_with("pypy") {
        if let Some(p) = rest.iter().position(|w| *w == "-m") {
            let module = arg(p + 1);
            return !module.is_empty() && !NOT_RUNS.contains(&module);
        }
        return rest
            .iter()
            .any(|w| *w == "-c" || *w == "-" || w.ends_with(".py") || w.starts_with("<<"))
            || seg.contains("<<");
    }
    if first.ends_with("runtests.py") || first.ends_with("runtests") || first.ends_with("bin/test")
    {
        return true;
    }
    match prog {
        "pytest" | "py.test" | "tox" | "nox" | "nosetests" | "trial" | "jest" | "vitest"
        | "mocha" | "rspec" | "phpunit" | "ctest" | "make" | "mvn" | "gradle" | "gradlew"
        | "npx" | "bun" | "deno" => true,
        "cargo" => matches!(arg(0), "test" | "run" | "nextest" | "bench"),
        "go" => matches!(arg(0), "test" | "run"),
        "dotnet" => matches!(arg(0), "test" | "run"),
        "npm" | "pnpm" | "yarn" => matches!(arg(0), "test" | "t" | "run" | "exec"),
        "node" | "ruby" | "perl" | "julia" | "Rscript" | "php" => !arg(0).is_empty(),
        "bash" | "sh" | "zsh" => arg(0).ends_with(".sh"),
        _ => first.starts_with("./") && !first.ends_with('/'),
    }
}

/// Failure markers of common test runners and of a crash, in a run's output.
static FAILED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?m)(^FAILED\b|^FAIL:|^ERROR:|^ERROR\s|\b[1-9][0-9]* (failed|errors?)\b|\b(failures|errors)=[1-9]|^Traceback \(most recent call last\)|^E\s+\w*(Error|Exception)\b|panicked at|^--- FAIL|\bFAILED \(|^\s*✕|Tests:\s+[1-9][0-9]* failed)",
    )
    .expect("failure regex")
});

/// Whether a run's result reports failures (a non-zero exit, or markers in
/// the output's tail: `cmd | tail` hides the exit code).
pub fn run_failed(out: &ToolOutput) -> bool {
    let text = out.content.as_str();
    let mut start = text.len().saturating_sub(4000);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    out.is_error || FAILED.is_match(&text[start..])
}

/// A request that reads like something to fix.
static BUG: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(bugs?|fix(es|ed|ing)?|fail(s|ed|ing|ure)?|errors?|exceptions?|traceback|crash(es|ed|ing)?|broken|incorrect(ly)?|wrong|regression|raises?|doesn'?t work|not working|unexpected(ly)?|issue)\b",
    )
    .expect("bug regex")
});

pub fn looks_like_bug_fix(request: &str) -> bool {
    let head = request.get(..8000).unwrap_or(request);
    BUG.is_match(head)
}

/// Source files, by extension: a change to anything else (notes, data,
/// generated output) does not ask for a test run.
const CODE_EXTENSIONS: &[&str] = &[
    "py", "pyx", "pyi", "rs", "go", "js", "mjs", "cjs", "ts", "tsx", "jsx", "java", "kt", "kts",
    "scala", "c", "h", "cc", "cpp", "cxx", "hpp", "hh", "cs", "fs", "rb", "php", "swift", "m",
    "mm", "sh", "bash", "pl", "pm", "lua", "r", "jl", "ex", "exs", "erl", "hs", "ml", "mli", "clj",
    "dart", "vue", "svelte", "zig", "nim", "sql",
];
/// Directories that hold environments and build output, not the project's code.
const NOT_SOURCE_DIRS: &[&str] = &[
    ".git",
    ".venv",
    "venv",
    "env",
    ".env",
    "site-packages",
    "node_modules",
    "__pycache__",
    ".tox",
    ".nox",
    "build",
    "dist",
    ".eggs",
    "target",
    ".mypy_cache",
    ".pytest_cache",
];

pub fn is_source_path(path: &str) -> bool {
    let p = std::path::Path::new(path);
    let ext_ok = p
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| CODE_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()));
    ext_ok
        && !p.components().any(|c| {
            let c = c.as_os_str().to_string_lossy();
            NOT_SOURCE_DIRS.contains(&c.as_ref()) || c.ends_with(".egg-info")
        })
}

/// Paths a file tool's arguments name (`path`, `file_path`, `paths`, and the
/// file headers of a patch).
fn tool_paths(args: &Value) -> Vec<String> {
    let mut out = Vec::new();
    for key in ["path", "file_path", "file", "target"] {
        if let Some(p) = args.get(key).and_then(Value::as_str) {
            out.push(p.to_string());
        }
    }
    if let Some(list) = args.get("paths").and_then(Value::as_array) {
        out.extend(list.iter().filter_map(Value::as_str).map(str::to_string));
    }
    for key in ["patch", "input", "diff"] {
        if let Some(text) = args.get(key).and_then(Value::as_str) {
            out.extend(PATCH_FILE.captures_iter(text).map(|c| c[1].to_string()));
        }
    }
    out
}

static PATCH_FILE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^(?:\*\*\* (?:Add|Update|Delete) File: |\+\+\+ b/|--- a/)(\S+)")
        .expect("patch regex")
});

/// A hash of the source files that differ from HEAD (or are new), contents
/// included, so an edit made through the shell is seen. None outside a git
/// work tree. Large change sets are hashed by size and modification time.
pub(super) fn code_fingerprint(cwd: &std::path::Path) -> Option<u64> {
    use std::hash::{Hash, Hasher};
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["status", "--porcelain=v1", "-z", "--untracked-files=all"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    let mut entries: Vec<(String, String)> = Vec::new();
    let mut fields = out.stdout.split(|b| *b == 0).peekable();
    while let Some(f) = fields.next() {
        if f.len() < 4 {
            continue;
        }
        let status = String::from_utf8_lossy(&f[..2]).to_string();
        let path = String::from_utf8_lossy(&f[3..]).to_string();
        if status.starts_with('R') || status.starts_with('C') {
            fields.next(); // the original path of a rename or copy
        }
        if is_source_path(&path) {
            entries.push((status, path));
        }
    }
    entries.sort();
    let by_content = entries.len() <= 500;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for (status, path) in &entries {
        status.hash(&mut h);
        path.hash(&mut h);
        let full = cwd.join(path);
        match std::fs::metadata(&full) {
            Ok(m) if by_content && m.len() <= 1 << 20 => {
                std::fs::read(&full).unwrap_or_default().hash(&mut h);
            }
            Ok(m) => {
                m.len().hash(&mut h);
                m.modified().ok().hash(&mut h);
            }
            Err(_) => 0u8.hash(&mut h),
        }
    }
    Some(h.finish())
}

pub(crate) const APPROACH_NOTE: &str = "[Approach] This reads like a problem to fix in the code. Before editing, \
reproduce it: a small script or test that shows the wrong behaviour, run to confirm it fails. Work out the correct \
behaviour from the code, its tests and docs, not only from the report's example, and try a few variants of the \
input. If the project cannot be imported or its tests cannot run, set up its environment first (a virtualenv, or \
`python -m ensurepip`, with the configured package index) rather than stubbing parts out. Fix the cause where the \
wrong value is produced, not only where it shows up. Work from the code here: looking elsewhere for a ready-made \
fix costs time.";

pub(crate) const VERIFY_NOTE: &str = "[Verify] You changed code and nothing has run it since the last change. \
Before your final answer, run your reproduction and the existing tests closest to the change (the test modules for \
the files you edited; a narrow selection with a timeout, so a slow suite does not stall you). If a test fails, fix \
the cause instead of deselecting the test or explaining it away. If running anything is truly impossible here, say \
exactly what was not run.";

pub(crate) const FAILED_NOTE: &str = "[Verify] The last run after your change reported failures (see its output \
above). Find out whether your change caused them, fix the cause and run the tests again. Only a failure that also \
happens on the original code may stay, and your answer must say so.";

pub(crate) const REVIEW_NOTE: &str = "[Review] Before your final answer, review your diff (`git diff`). Is the fix \
at the source of the problem rather than where it shows up? Do other call sites, subclasses or overrides of what \
you changed need the same change? Which inputs and edge cases does it handle (empty or missing values, other forms \
of the same input)? Does code you now call have preconditions you must meet? Is existing behaviour kept where it \
should be? Fix what you find and rerun the related tests. Then answer, stating exactly what you ran and what \
passed.";

impl Agent {
    /// Start of a turn: the effort in use, the git baseline, and the approach
    /// note for a request that reads like a bug to fix.
    pub(super) fn diligence_begin(&mut self) {
        let root = self.usage_session_id.is_none();
        let headless = self.task.headless;
        // A test (or an embedding) may fix the effort for this agent.
        let setting = self.task.effort.or(effort_setting().0);
        let explicit = setting.is_some();
        let effort = setting.unwrap_or(if headless {
            Effort::Medium
        } else {
            Effort::Low
        });
        let active =
            root && effort >= Effort::Medium && (headless || explicit) && !self.task_typed();
        self.task.diligence = Diligence {
            effort,
            active,
            baseline: if active {
                code_fingerprint(&self.cwd)
            } else {
                None
            },
            ..Diligence::default()
        };
        if !active {
            return;
        }
        let request = self.items.iter().rev().find_map(|i| match i {
            Item::User { text, .. } if !text.starts_with('[') => Some(text.as_str()),
            _ => None,
        });
        if request.is_some_and(looks_like_bug_fix) {
            crate::trace::log("task", || {
                format!("agent effort {}: approach note", effort.name())
            });
            self.push_counted(Item::user(APPROACH_NOTE));
        }
    }

    /// One tool call's result (next to [`Agent::drift_observe`]).
    pub(super) fn diligence_observe(
        &mut self,
        name: &str,
        arguments: &str,
        out: &ToolOutput,
        ran: Option<(&Concurrency, &[PathBuf])>,
    ) {
        if !self.task.diligence.active {
            return;
        }
        let args: Value = serde_json::from_str(arguments).unwrap_or_default();
        let home = dirs::home_dir();
        let (wrote, _) = super::drift::classify(name, &args, out, ran, &self.cwd, home.as_deref());
        // A file tool's write to a source file (shell edits show in the git state).
        let wrote = wrote && tool_paths(&args).iter().any(|p| is_source_path(p));
        let command = args.get("command").and_then(Value::as_str);
        let is_run = command.is_some_and(runs_code);
        let after = is_run.then(|| code_fingerprint(&self.cwd));
        let d = &mut self.task.diligence;
        if wrote {
            d.tool_wrote = true;
            d.wrote_since_run = true;
        }
        if let Some(after) = after {
            d.last_run = Some(Run {
                after,
                failed: run_failed(out),
            });
            d.wrote_since_run = false;
        }
    }

    /// The model is about to give its final answer: the note to send back
    /// instead, if the turn's code changes are unverified, failing, or (high)
    /// not yet reviewed.
    pub(super) fn diligence_finish(&mut self, ui: &mut dyn Ui) -> Option<Item> {
        if !self.task.diligence.active {
            return None;
        }
        let now = code_fingerprint(&self.cwd);
        let d = &mut self.task.diligence;
        let changed = d.tool_wrote || (now.is_some() && now != d.baseline);
        if !changed {
            return None;
        }
        let unverified = match &d.last_run {
            None => true,
            Some(r) => d.wrote_since_run || (now.is_some() && r.after != now),
        };
        let failing = !unverified && d.last_run.as_ref().is_some_and(|r| r.failed);
        let can_note = d.notes < d.effort.verify_notes() && d.noted_at != Some(now);
        let (note, notice) = if (unverified || failing) && can_note {
            d.notes += 1;
            d.noted_at = Some(now);
            if unverified {
                (VERIFY_NOTE, "The change has not been run since it was made: asking for a test run before finishing.")
            } else {
                (
                    FAILED_NOTE,
                    "The last test run after the change failed: asking for a fix before finishing.",
                )
            }
        } else if d.effort == Effort::High && !d.reviewed {
            d.reviewed = true;
            (
                REVIEW_NOTE,
                "Asking for a review of the diff before finishing.",
            )
        } else {
            return None;
        };
        crate::trace::log("task", || {
            format!("agent effort {}: {}", d.effort.name(), &note[..10])
        });
        ui.notice(notice);
        Some(Item::user(note))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effort_levels_parse() {
        assert_eq!(Effort::parse("HIGH"), Some(Effort::High));
        assert_eq!(Effort::parse(" medium "), Some(Effort::Medium));
        assert_eq!(Effort::parse("off"), Some(Effort::Low));
        assert_eq!(Effort::parse("extreme"), None);
        assert!(Effort::Low < Effort::Medium && Effort::Medium < Effort::High);
    }

    #[test]
    fn commands_that_run_code() {
        for cmd in [
            "python -m pytest tests/test_x.py -q",
            "cd /w/repo && PYTHONPATH=. python tests/runtests.py queries --parallel 1",
            "timeout 300 python -m pytest -x 2>&1 | tail -20",
            "python -c 'import x; print(x.f())'",
            "python reproduce.py",
            "python - <<'EOF'\nprint(1)\nEOF",
            "uv run pytest -q",
            "cargo test -p core",
            "go test ./...",
            "npm test",
            "npx jest src",
            "bin/test sympy/core",
            "./runtests.py",
            "make check",
            "node scripts/check.js",
            "bash run_tests.sh",
        ] {
            assert!(runs_code(cmd), "{cmd}");
        }
        for cmd in [
            "grep -rn foo src | head",
            "sed -n '1,80p' src/a.py",
            "git diff",
            "python -m pip install -e .",
            "pip install pytest",
            "python -m py_compile src/a.py",
            "python -m venv .venv",
            "ls ./tests",
            "cat > t.py <<'EOF'",
            "sed -i 's/a/b/' x.py",
        ] {
            assert!(!runs_code(cmd), "{cmd}");
        }
    }

    #[test]
    fn failures_are_read_from_the_output_tail() {
        let ok = |s: &str| ToolOutput::ok(s);
        assert!(run_failed(&ok("==== 1 failed, 12 passed in 3.1s ====")));
        assert!(run_failed(&ok(
            "Ran 4 tests in 0.1s\n\nFAILED (failures=1)"
        )));
        assert!(run_failed(&ok("Ran 4 tests\n\nFAILED (errors=2)")));
        assert!(run_failed(&ok(
            "Traceback (most recent call last):\n  File \"x\"\nValueError: boom"
        )));
        assert!(run_failed(&ToolOutput::err("exit code 1")));
        assert!(!run_failed(&ok("==== 12 passed in 3.1s ====")));
        assert!(!run_failed(&ok("Ran 31 tests in 0.2s\n\nOK (skipped=1)")));
        assert!(!run_failed(&ok("0 failed")));
    }

    #[test]
    fn bug_fix_requests_are_recognised() {
        assert!(looks_like_bug_fix(
            "QuerySet.exclude() raises FieldError on annotated relations"
        ));
        assert!(looks_like_bug_fix(
            "The parser gives the wrong result for empty input"
        ));
        assert!(looks_like_bug_fix("Fix the crash in the exporter"));
        assert!(!looks_like_bug_fix("Add a --verbose flag to the CLI"));
        assert!(!looks_like_bug_fix("Summarise what this module does"));
    }

    use crate::agent::test_support::*;
    use crate::agent::TurnEnd;
    use crate::types::Response;
    use serde_json::json;
    use tokio_util::sync::CancellationToken;

    /// A shell stand-in in a git workspace: `sed -i` edits `x.py`, `echo >
    /// notes.txt` writes a text file, a command with FAIL prints a failing
    /// test summary, anything else a passing one.
    struct FakeShell(std::path::PathBuf);

    #[async_trait::async_trait]
    impl crate::tools::Tool for FakeShell {
        fn spec(&self) -> crate::types::ToolSpec {
            crate::types::ToolSpec {
                name: "bash".into(),
                description: String::new(),
                parameters: json!({}),
            }
        }
        fn effects(&self, _: &Value) -> crate::tools::Effects {
            unknown_fx(&json!({}))
        }
        fn summary(&self, _: &Value) -> String {
            String::new()
        }
        async fn run(&self, args: Value, _: &crate::tools::ToolCtx) -> ToolOutput {
            let cmd = args["command"].as_str().unwrap_or_default();
            if cmd.starts_with("sed -i") {
                let path = self.0.join("x.py");
                let mut text = std::fs::read_to_string(&path).unwrap_or_default();
                text.push_str(&format!("# {cmd}\n"));
                std::fs::write(path, text).unwrap();
                return ToolOutput::ok("");
            }
            if cmd.starts_with("echo") {
                std::fs::write(self.0.join("notes.txt"), cmd).unwrap();
                return ToolOutput::ok("");
            }
            if cmd.contains("FAIL") {
                return ToolOutput::ok("==== 1 failed, 3 passed ====");
            }
            ToolOutput::ok("==== 4 passed ====")
        }
    }

    fn git_workspace() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("x.py"), "x = 1\n").unwrap();
        for args in [
            vec!["init", "-q"],
            vec!["add", "."],
            vec![
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-qm",
                "init",
            ],
        ] {
            let ok = std::process::Command::new("git")
                .arg("-C")
                .arg(dir.path())
                .args(&args)
                .status()
                .unwrap()
                .success();
            assert!(ok, "git {args:?}");
        }
        dir
    }

    fn bash(id: &str, cmd: &str) -> Response {
        reply(vec![call(id, "bash", json!({"command": cmd}))])
    }

    fn answer(text: &str) -> Response {
        Response {
            truncated: false,
            items: vec![Item::Assistant { text: text.into() }],
            usage: None,
        }
    }

    async fn run_turn(
        script: Vec<Response>,
        headless: bool,
        effort: Option<&str>,
        prompt: &str,
    ) -> (crate::agent::Agent, Vec<String>, bool) {
        let dir = git_workspace();
        let mut agent = scripted(script);
        agent.cwd = dir.path().to_path_buf();
        agent
            .tools
            .add(Box::new(FakeShell(dir.path().to_path_buf())));
        agent.task.step_cap = Some(None);
        agent.task.headless = headless;
        agent.task.effort = effort.and_then(Effort::parse);
        let mut ui = NoticeCatch(Vec::new());
        let end = agent
            .turn(Item::user(prompt), &mut ui, CancellationToken::new())
            .await;
        let ok = matches!(end, Ok(TurnEnd::Done));
        (agent, ui.0, ok)
    }

    fn notes(agent: &crate::agent::Agent, prefix: &str) -> usize {
        agent
            .items
            .iter()
            .filter(|i| matches!(i, Item::User { text, .. } if text.starts_with(prefix)))
            .count()
    }

    #[tokio::test]
    async fn an_unverified_change_is_sent_back_once_at_medium() {
        // edit, answer -> verify note; run passing tests, answer -> done.
        let (agent, notices, ok) = run_turn(
            vec![
                bash("1", "sed -i 's/a/b/' x.py"),
                answer("done"),
                bash("2", "python -m pytest tests -q"),
                answer("done, 4 passed"),
            ],
            true,
            None,
            "Fix the crash in the exporter",
        )
        .await;
        assert!(ok);
        assert_eq!(notes(&agent, "[Approach]"), 1);
        assert_eq!(notes(&agent, "[Verify]"), 1);
        assert_eq!(notes(&agent, "[Review]"), 0);
        assert!(
            notices.iter().any(|n| n.contains("asking for a test run")),
            "{notices:?}"
        );
    }

    #[tokio::test]
    async fn a_failing_run_and_the_review_at_high() {
        // edit, failing run, answer -> failure note; edit, passing run,
        // answer -> review note; answer -> done.
        let (agent, _, ok) = run_turn(
            vec![
                bash("1", "sed -i 's/a/b/' x.py"),
                bash("2", "python -m pytest FAIL"),
                answer("done"),
                bash("3", "sed -i 's/b/c/' x.py"),
                bash("4", "python -m pytest tests"),
                answer("fixed"),
                answer("reviewed, 4 passed"),
            ],
            true,
            Some("high"),
            "Add a flag",
        )
        .await;
        assert!(ok);
        assert_eq!(notes(&agent, "[Approach]"), 0);
        assert_eq!(notes(&agent, "[Verify] The last run"), 1);
        assert_eq!(notes(&agent, "[Review]"), 1);
    }

    #[tokio::test]
    async fn the_same_state_is_not_sent_back_twice() {
        // The model answers again without running anything: no second note.
        let (agent, _, ok) = run_turn(
            vec![
                bash("1", "sed -i 's/a/b/' x.py"),
                answer("done"),
                answer("nothing can run here: no interpreter"),
                answer("reviewed; nothing could be run"),
            ],
            true,
            Some("high"),
            "Add a flag",
        )
        .await;
        assert!(ok);
        assert_eq!(notes(&agent, "[Verify]"), 1);
        // High still asks for the review once.
        assert_eq!(notes(&agent, "[Review]"), 1);
    }

    #[tokio::test]
    async fn interactive_turns_and_low_effort_are_not_gated() {
        let script = || vec![bash("1", "sed -i 's/a/b/' x.py"), answer("done")];
        let (agent, _, ok) = run_turn(script(), false, None, "Fix the bug").await;
        assert!(ok && notes(&agent, "[") == 0);
        let (agent, _, ok) = run_turn(script(), true, Some("low"), "Fix the bug").await;
        assert!(ok && notes(&agent, "[") == 0);
    }

    #[tokio::test]
    async fn text_only_edits_are_not_gated() {
        let (agent, _, ok) = run_turn(
            vec![
                bash("1", "echo hello > notes.txt"),
                answer("wrote the notes"),
            ],
            true,
            Some("high"),
            "Fix the typo in the notes",
        )
        .await;
        assert!(ok);
        assert_eq!(notes(&agent, "[Verify]") + notes(&agent, "[Review]"), 0);
    }

    #[test]
    fn source_paths() {
        assert!(is_source_path("src/app/models.py"));
        assert!(is_source_path("lib/x.RS"));
        assert!(!is_source_path("notes.txt"));
        assert!(!is_source_path(".venv/lib/python3.12/site-packages/x.py"));
        assert!(!is_source_path("node_modules/a/index.js"));
        assert!(!is_source_path("pkg.egg-info/x.py"));
    }

    #[tokio::test]
    async fn turns_without_changes_are_not_gated() {
        let (agent, _, ok) = run_turn(
            vec![
                bash("1", "grep -rn foo ."),
                answer("foo is defined in a.py"),
            ],
            true,
            Some("high"),
            "Where is foo?",
        )
        .await;
        assert!(ok);
        assert_eq!(notes(&agent, "[Verify]") + notes(&agent, "[Review]"), 0);
    }
}
