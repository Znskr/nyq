//! Build-system detection and the verifier primitives (design §4.2).
//!
//! A [`BuildSystem`] knows how to build, test, lint and format-check one kind of
//! project (cargo, go, uv/pytest, npm/pnpm/yarn, cmake/ctest, a plain Makefile).
//! Implementations only *describe* their commands and how to read the output;
//! the provided async methods run them through a [`CommandRunner`], apply a
//! timeout and cancellation, condense the output (first N errors in full plus a
//! count) and, for a test run with a large failure, re-run one failing test on
//! its own so the model sees a small, focused report (promptyped's
//! `tryRerunOneByOne`).
//!
//! Every step returns a [`CheckOutcome`].

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

// ---------------------------------------------------------------------------
// Command execution
// ---------------------------------------------------------------------------

/// A shell command to run: one script line, interpreted by the platform shell.
#[derive(Clone, Debug)]
pub struct CommandSpec {
    pub command: String,
    pub cwd: PathBuf,
    pub timeout: Duration,
    pub cancel: CancellationToken,
}

/// What a command did. `output` is stdout and stderr merged.
#[derive(Clone, Debug, Default)]
pub struct CommandOutput {
    /// None when the process never started or was killed before it exited.
    pub exit_code: Option<i32>,
    pub output: String,
    pub timed_out: bool,
    pub cancelled: bool,
}

impl CommandOutput {
    pub fn success(&self) -> bool {
        self.exit_code == Some(0) && !self.timed_out && !self.cancelled
    }

    pub fn ok(output: impl Into<String>) -> Self {
        Self {
            exit_code: Some(0),
            output: output.into(),
            ..Self::default()
        }
    }

    pub fn failed(code: i32, output: impl Into<String>) -> Self {
        Self {
            exit_code: Some(code),
            output: output.into(),
            ..Self::default()
        }
    }

    pub fn timeout(output: impl Into<String>) -> Self {
        Self {
            output: output.into(),
            timed_out: true,
            ..Self::default()
        }
    }

    pub fn cancelled() -> Self {
        Self {
            cancelled: true,
            ..Self::default()
        }
    }
}

/// How the task engine runs commands. The wiring stage injects nyq's sandboxed
/// runner ([`ShellRunner`]); tests inject [`FakeRunner`].
#[async_trait]
pub trait CommandRunner: Send + Sync {
    async fn run(&self, spec: &CommandSpec) -> CommandOutput;
}

/// Runs commands through nyq's bash tool runner: own process group, sandbox
/// policy, timeout, cancellation.
#[derive(Clone, Debug, Default)]
pub struct ShellRunner {
    pub policy: crate::sandbox::Policy,
}

impl ShellRunner {
    pub fn new(policy: crate::sandbox::Policy) -> Self {
        Self { policy }
    }
}

#[async_trait]
impl CommandRunner for ShellRunner {
    async fn run(&self, spec: &CommandSpec) -> CommandOutput {
        let out = crate::tools::run_shell(
            &spec.command,
            &spec.cwd,
            &self.policy,
            &spec.cancel,
            Some(spec.timeout),
        )
        .await;
        from_tool_output(out.content, out.is_error)
    }
}

/// Recovers the exit status from the text the bash tool returns: it appends
/// `exit code N` (or `killed by signal N`) as a last line, and a
/// `command timed out after Ns` line on timeout.
fn from_tool_output(content: String, is_error: bool) -> CommandOutput {
    if content == "Cancelled by user." {
        return CommandOutput::cancelled();
    }
    let timed_out = content.contains("command timed out after ");
    let mut lines: Vec<&str> = content.lines().collect();
    let mut exit_code = None;
    if let Some(last) = lines.last() {
        if let Some(rest) = last.strip_prefix("exit code ") {
            let digits: String = rest
                .chars()
                .take_while(|c| c.is_ascii_digit() || *c == '-')
                .collect();
            if let Ok(code) = digits.parse::<i32>() {
                exit_code = Some(code);
                lines.pop();
            }
        } else if last.starts_with("killed by signal ") {
            lines.pop();
        }
    }
    if exit_code.is_none() && !is_error && !timed_out {
        exit_code = Some(0);
    }
    let mut output = lines.join("\n");
    if output == "(no output)" {
        output.clear();
    }
    CommandOutput {
        exit_code,
        output,
        timed_out,
        cancelled: false,
    }
}

/// Everything a build step needs to run: the runner, limits and cancellation.
#[derive(Clone)]
pub struct Exec {
    pub runner: Arc<dyn CommandRunner>,
    pub cancel: CancellationToken,
    pub build_timeout: Duration,
    pub test_timeout: Duration,
    /// Errors shown in full in a summary.
    pub max_errors: usize,
    /// A failing test run longer than this many lines triggers the
    /// single-test rerun.
    pub rerun_threshold_lines: usize,
    /// At most this many failing tests are rerun individually.
    pub max_reruns: usize,
}

impl Exec {
    pub fn new(runner: Arc<dyn CommandRunner>) -> Self {
        Self {
            runner,
            cancel: CancellationToken::new(),
            build_timeout: Duration::from_secs(300),
            test_timeout: Duration::from_secs(300),
            max_errors: 5,
            rerun_threshold_lines: 200,
            max_reruns: 3,
        }
    }

    pub fn with_cancel(mut self, cancel: CancellationToken) -> Self {
        self.cancel = cancel;
        self
    }

    pub fn with_timeouts(mut self, build: Duration, test: Duration) -> Self {
        self.build_timeout = build;
        self.test_timeout = test;
        self
    }

    pub fn with_max_errors(mut self, n: usize) -> Self {
        self.max_errors = n.max(1);
        self
    }

    /// Run one command in `cwd` with `timeout`.
    pub async fn run(&self, command: &str, cwd: &Path, timeout: Duration) -> CommandOutput {
        self.runner
            .run(&CommandSpec {
                command: command.to_string(),
                cwd: cwd.to_path_buf(),
                timeout,
                cancel: self.cancel.clone(),
            })
            .await
    }
}

// ---------------------------------------------------------------------------
// Outcomes
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutcomeKind {
    Passed,
    Failed,
    /// The build system has no such step (no lint script, say). `ok` is true.
    Skipped,
    TimedOut,
    Cancelled,
}

/// The result of one build, test, lint or format step.
#[derive(Clone, Debug)]
pub struct CheckOutcome {
    pub ok: bool,
    /// Short, model-facing text: on failure the first errors in full plus the
    /// error count.
    pub summary: String,
    /// The raw merged output.
    pub full_output: String,
    /// For test runs: how many tests failed, when it could be told.
    pub failed_tests: Option<u32>,
    pub duration: Duration,
    /// Not in the minimal design: tells a timeout, a cancellation and a
    /// skipped step apart from a plain failure.
    pub kind: OutcomeKind,
}

impl CheckOutcome {
    pub fn skipped(summary: impl Into<String>) -> Self {
        Self {
            ok: true,
            summary: summary.into(),
            full_output: String::new(),
            failed_tests: None,
            duration: Duration::ZERO,
            kind: OutcomeKind::Skipped,
        }
    }
}

// ---------------------------------------------------------------------------
// The trait
// ---------------------------------------------------------------------------

/// Which tests to run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TestScope {
    All,
    /// One test, by the name the build system reports it under.
    Test(String),
    /// Tests in these files (paths relative to the project root). Systems that
    /// cannot target files run everything.
    Files(Vec<String>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    Build,
    Test,
    Lint,
    Format,
}

impl Step {
    fn label(self) -> &'static str {
        match self {
            Step::Build => "build",
            Step::Test => "tests",
            Step::Lint => "lint",
            Step::Format => "format check",
        }
    }
}

/// One kind of project. Implementations describe commands and parse output;
/// the async methods do the running.
#[async_trait]
pub trait BuildSystem: Send + Sync {
    /// Short name: "cargo", "go", "uv", "pnpm", "cmake", "make".
    fn name(&self) -> &str;
    fn root(&self) -> &Path;
    /// `Some` when `root` looks like a project of this kind.
    fn detect(root: &Path) -> Option<Self>
    where
        Self: Sized;
    /// `path` is relative to the project root, with `/` separators.
    fn is_source(&self, path: &str) -> bool;
    fn is_test(&self, path: &str) -> bool;

    /// None when the project has no such step.
    fn build_command(&self) -> Option<String>;
    fn test_command(&self, scope: &TestScope) -> Option<String>;
    fn lint_command(&self) -> Option<String> {
        None
    }
    fn format_command(&self) -> Option<String> {
        None
    }
    /// Whether `line` opens a new error block in the tool's output.
    fn is_error_start(&self, line: &str) -> bool;
    /// Names of failed tests in a test run's output, in order, without
    /// duplicates.
    fn parse_failed_tests(&self, _output: &str) -> Vec<String> {
        Vec::new()
    }

    async fn build(&self, ex: &Exec) -> CheckOutcome {
        run_step(self, ex, Step::Build).await
    }

    /// Run tests. A big failure re-runs failing tests one at a time and
    /// reports the first that still fails.
    async fn test(&self, ex: &Exec, scope: &TestScope) -> CheckOutcome {
        run_tests(self, ex, scope).await
    }

    async fn lint(&self, ex: &Exec) -> CheckOutcome {
        run_step(self, ex, Step::Lint).await
    }

    async fn format_check(&self, ex: &Exec) -> CheckOutcome {
        run_step(self, ex, Step::Format).await
    }

    /// Run one test alone, by the name `parse_failed_tests` reported.
    async fn rerun_test(&self, ex: &Exec, name: &str) -> CheckOutcome {
        self.test(ex, &TestScope::Test(name.to_string())).await
    }
}

fn command_for<S: BuildSystem + ?Sized>(sys: &S, step: Step) -> Option<String> {
    match step {
        Step::Build => sys.build_command(),
        Step::Test => sys.test_command(&TestScope::All),
        Step::Lint => sys.lint_command(),
        Step::Format => sys.format_command(),
    }
}

async fn run_step<S: BuildSystem + ?Sized>(sys: &S, ex: &Exec, step: Step) -> CheckOutcome {
    let Some(command) = command_for(sys, step) else {
        return CheckOutcome::skipped(format!(
            "{}: no {} step for this project (skipped).",
            sys.name(),
            step.label()
        ));
    };
    let timeout = match step {
        Step::Test => ex.test_timeout,
        _ => ex.build_timeout,
    };
    execute(sys, ex, step, &command, timeout).await
}

/// Run `command` and turn the result into an outcome.
async fn execute<S: BuildSystem + ?Sized>(
    sys: &S,
    ex: &Exec,
    step: Step,
    command: &str,
    timeout: Duration,
) -> CheckOutcome {
    let started = Instant::now();
    let out = ex.run(command, sys.root(), timeout).await;
    let duration = started.elapsed();
    let label = format!("`{}`", display_command(command));
    if out.cancelled {
        return CheckOutcome {
            ok: false,
            summary: format!("{label} was cancelled."),
            full_output: out.output,
            failed_tests: None,
            duration,
            kind: OutcomeKind::Cancelled,
        };
    }
    if out.timed_out {
        let tail = tail_lines(&out.output, 30);
        return CheckOutcome {
            ok: false,
            summary: format!(
                "{label} timed out after {}s and was terminated. Last output:\n{tail}",
                timeout.as_secs()
            ),
            full_output: out.output,
            failed_tests: None,
            duration,
            kind: OutcomeKind::TimedOut,
        };
    }
    if out.success() {
        return CheckOutcome {
            ok: true,
            summary: format!(
                "{label}: {} ok ({:.1}s)",
                step.label(),
                duration.as_secs_f32()
            ),
            full_output: out.output,
            failed_tests: (step == Step::Test).then_some(0),
            duration,
            kind: OutcomeKind::Passed,
        };
    }
    let failed = if step == Step::Test {
        let names = sys.parse_failed_tests(&out.output);
        (!names.is_empty()).then_some(names.len() as u32)
    } else {
        None
    };
    let condensed = condense(&out.output, |l| sys.is_error_start(l), ex.max_errors);
    let code = match out.exit_code {
        Some(c) => format!("exit code {c}"),
        None => "did not run to completion".to_string(),
    };
    let head = match (step, failed) {
        (Step::Test, Some(n)) => format!("{label} failed ({code}, {n} failing test(s)):"),
        _ => format!("{label} failed ({code}):"),
    };
    CheckOutcome {
        ok: false,
        summary: format!("{head}\n{}", condensed.render()),
        full_output: out.output,
        failed_tests: failed,
        duration,
        kind: OutcomeKind::Failed,
    }
}

async fn run_tests<S: BuildSystem + ?Sized>(sys: &S, ex: &Exec, scope: &TestScope) -> CheckOutcome {
    let Some(command) = sys.test_command(scope) else {
        return CheckOutcome::skipped(format!(
            "{}: no test step for this project (skipped).",
            sys.name()
        ));
    };
    let first = execute(sys, ex, Step::Test, &command, ex.test_timeout).await;
    if first.kind != OutcomeKind::Failed {
        return first;
    }
    let names = sys.parse_failed_tests(&first.full_output);
    let lines = first.full_output.lines().count();
    let single = matches!(scope, TestScope::Test(_));
    if single || names.is_empty() || lines <= ex.rerun_threshold_lines {
        return first;
    }
    // A long failure report: look at the failing tests one at a time.
    let total = names.len();
    for name in names.iter().take(ex.max_reruns) {
        let rerun = sys.rerun_test(ex, name).await;
        match rerun.kind {
            OutcomeKind::Failed | OutcomeKind::TimedOut => {
                return CheckOutcome {
                    ok: false,
                    summary: format!(
                        "{total} test(s) failed; the full report was {lines} lines, so only `{name}` is shown, re-run on its own:\n{}",
                        rerun.summary
                    ),
                    full_output: first.full_output,
                    failed_tests: Some(total as u32),
                    duration: first.duration + rerun.duration,
                    kind: OutcomeKind::Failed,
                };
            }
            OutcomeKind::Cancelled => return rerun,
            _ => {}
        }
    }
    CheckOutcome {
        ok: false,
        summary: format!(
            "{total} test(s) failed in the full run ({}), but the ones re-run on their own passed. That points at a test interaction or a flaky test. Start of the report:\n{}",
            names.join(", "),
            head_lines(&first.full_output, 40)
        ),
        full_output: first.full_output,
        failed_tests: Some(total as u32),
        duration: first.duration,
        kind: OutcomeKind::Failed,
    }
}

// ---------------------------------------------------------------------------
// Error condensing
// ---------------------------------------------------------------------------

/// The first errors of a tool's output, in full, and how many there were.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Condensed {
    pub text: String,
    pub total: usize,
    pub shown: usize,
}

impl Condensed {
    pub fn render(&self) -> String {
        if self.total > self.shown {
            format!(
                "{}\n… {} more error(s) not shown ({} in total). Fix these first; the rest may follow from them.",
                self.text,
                self.total - self.shown,
                self.total
            )
        } else if self.total > 1 {
            format!("{}\n({} errors in total)", self.text, self.total)
        } else {
            self.text.clone()
        }
    }
}

const MAX_BLOCK_LINES: usize = 40;
const MAX_SUMMARY_CHARS: usize = 8_000;

/// Splits `output` into error blocks (a block starts at a line `is_start`
/// accepts and runs to the next start) and keeps the first `max_errors` in
/// full. When no line opens a block the head and tail of the output are used.
pub fn condense(output: &str, is_start: impl Fn(&str) -> bool, max_errors: usize) -> Condensed {
    let max_errors = max_errors.max(1);
    let mut blocks: Vec<Vec<&str>> = Vec::new();
    for line in output.lines() {
        if is_start(line) {
            blocks.push(vec![line]);
        } else if let Some(block) = blocks.last_mut() {
            block.push(line);
        }
    }
    if blocks.is_empty() {
        let lines: Vec<&str> = output.lines().collect();
        let text = if lines.len() <= 80 {
            output.trim_end().to_string()
        } else {
            format!(
                "{}\n… {} lines omitted …\n{}",
                lines[..40].join("\n"),
                lines.len() - 70,
                lines[lines.len() - 30..].join("\n")
            )
        };
        return Condensed {
            text: clip_chars(&text, MAX_SUMMARY_CHARS),
            total: 0,
            shown: 0,
        };
    }
    let total = blocks.len();
    let shown = total.min(max_errors);
    let mut parts = Vec::with_capacity(shown);
    for block in blocks.iter().take(shown) {
        let mut b: Vec<&str> = block.clone();
        while b.last().is_some_and(|l| l.trim().is_empty()) {
            b.pop();
        }
        if b.len() > MAX_BLOCK_LINES {
            let extra = b.len() - MAX_BLOCK_LINES;
            b.truncate(MAX_BLOCK_LINES);
            parts.push(format!("{}\n… {extra} more lines", b.join("\n")));
        } else {
            parts.push(b.join("\n"));
        }
    }
    Condensed {
        text: clip_chars(&parts.join("\n\n"), MAX_SUMMARY_CHARS),
        total,
        shown,
    }
}

/// Earlier error messages cut down, the newest kept whole (promptyped
/// `shortenOldErrorMessages`). `msgs` is oldest first.
pub fn shorten_old_errors(msgs: &[String], keep_chars: usize) -> Vec<String> {
    let last = msgs.len().saturating_sub(1);
    msgs.iter()
        .enumerate()
        .map(|(i, m)| {
            if i == last || m.chars().count() <= keep_chars {
                m.clone()
            } else {
                format!("{}… (truncated)", clip_chars(m, keep_chars))
            }
        })
        .collect()
}

fn clip_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push_str("\n… (truncated)");
    out
}

fn head_lines(s: &str, n: usize) -> String {
    s.lines().take(n).collect::<Vec<_>>().join("\n")
}

fn tail_lines(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

/// The first line of a command, for display.
fn display_command(command: &str) -> String {
    let line = command.lines().next().unwrap_or("");
    let line = line.trim();
    if line.chars().count() > 100 {
        format!("{}…", line.chars().take(100).collect::<String>())
    } else {
        line.to_string()
    }
}

/// Quote `s` as one shell word.
pub fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "_-./:=@%+,".contains(c))
    {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// `path:12:5: message` or `path:12: message` at the start of a line.
fn has_loc_prefix(line: &str) -> bool {
    let mut it = line.splitn(4, ':');
    let (Some(path), Some(l), Some(next)) = (it.next(), it.next(), it.next()) else {
        return false;
    };
    !path.is_empty()
        && path.chars().any(|c| c.is_alphabetic())
        && !path.contains(' ')
        && !l.is_empty()
        && l.bytes().all(|b| b.is_ascii_digit())
        && (next.starts_with(' ') || (!next.is_empty() && next.bytes().all(|b| b.is_ascii_digit())))
}

fn ext(path: &str) -> &str {
    Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn has_dir(path: &str, names: &[&str]) -> bool {
    let mut parts: Vec<&str> = path.split('/').collect();
    parts.pop();
    parts.iter().any(|p| names.contains(p))
}

fn push_unique(v: &mut Vec<String>, s: String) {
    if !s.is_empty() && !v.contains(&s) {
        v.push(s);
    }
}

// ---------------------------------------------------------------------------
// cargo
// ---------------------------------------------------------------------------

pub struct Cargo {
    root: PathBuf,
}

#[async_trait]
impl BuildSystem for Cargo {
    fn name(&self) -> &str {
        "cargo"
    }
    fn root(&self) -> &Path {
        &self.root
    }
    fn detect(root: &Path) -> Option<Self> {
        root.join("Cargo.toml").is_file().then(|| Self {
            root: root.to_path_buf(),
        })
    }
    fn is_source(&self, path: &str) -> bool {
        ext(path) == "rs" || file_name(path) == "Cargo.toml"
    }
    fn is_test(&self, path: &str) -> bool {
        ext(path) == "rs"
            && (has_dir(path, &["tests", "benches"])
                || file_name(path) == "tests.rs"
                || file_name(path).ends_with("_test.rs"))
    }
    fn build_command(&self) -> Option<String> {
        Some("cargo build --workspace --all-targets --color never".into())
    }
    fn test_command(&self, scope: &TestScope) -> Option<String> {
        let base = "RUST_BACKTRACE=1 cargo test --workspace --color never";
        Some(match scope {
            TestScope::All => base.to_string(),
            TestScope::Test(name) => format!("{base} {} -- --exact", shell_quote(name)),
            TestScope::Files(files) => {
                let stems: Vec<String> = files
                    .iter()
                    .filter_map(|f| {
                        let parts: Vec<&str> = f.split('/').collect();
                        (parts.len() == 2 && parts[0] == "tests" && ext(f) == "rs")
                            .then(|| file_name(f).trim_end_matches(".rs").to_string())
                    })
                    .collect();
                if stems.is_empty() || stems.len() != files.len() {
                    base.to_string()
                } else {
                    let targets: Vec<String> = stems
                        .iter()
                        .map(|s| format!("--test {}", shell_quote(s)))
                        .collect();
                    format!("{base} {}", targets.join(" "))
                }
            }
        })
    }
    fn lint_command(&self) -> Option<String> {
        Some("cargo clippy --workspace --all-targets --color never -- -D warnings".into())
    }
    fn format_command(&self) -> Option<String> {
        Some("cargo fmt --all -- --check".into())
    }
    fn is_error_start(&self, line: &str) -> bool {
        (line.starts_with("error")
            && !line.starts_with("error: aborting due to")
            && !line.starts_with("error: could not compile")
            && !line.starts_with("error: test failed")
            && !line.starts_with("error: build failed"))
            || line.starts_with("Diff in ")
    }
    fn parse_failed_tests(&self, output: &str) -> Vec<String> {
        let mut names = Vec::new();
        for line in output.lines() {
            let l = line.trim();
            // libtest: `test tests::name ... FAILED`
            if let Some(rest) = l.strip_prefix("test ") {
                if let Some(name) = rest.strip_suffix(" ... FAILED") {
                    push_unique(&mut names, name.trim().to_string());
                }
            }
            // nextest: `FAIL [   0.004s] crate tests::name`
            if let Some(rest) = l.strip_prefix("FAIL [") {
                if let Some((_, tail)) = rest.split_once(']') {
                    push_unique(&mut names, tail.trim().to_string());
                }
            }
        }
        names
    }
}

// ---------------------------------------------------------------------------
// go
// ---------------------------------------------------------------------------

pub struct Go {
    root: PathBuf,
}

#[async_trait]
impl BuildSystem for Go {
    fn name(&self) -> &str {
        "go"
    }
    fn root(&self) -> &Path {
        &self.root
    }
    fn detect(root: &Path) -> Option<Self> {
        root.join("go.mod").is_file().then(|| Self {
            root: root.to_path_buf(),
        })
    }
    fn is_source(&self, path: &str) -> bool {
        ext(path) == "go" || file_name(path) == "go.mod"
    }
    fn is_test(&self, path: &str) -> bool {
        file_name(path).ends_with("_test.go")
    }
    fn build_command(&self) -> Option<String> {
        // Compile the test binaries too, without running anything.
        Some("go build ./... && go test -count=1 -run '^$' ./...".into())
    }
    fn test_command(&self, scope: &TestScope) -> Option<String> {
        Some(match scope {
            TestScope::All => "go test -count=1 -timeout 120s ./...".into(),
            TestScope::Test(name) => {
                // `Parent/sub` names: anchor each segment.
                let re: Vec<String> = name.split('/').map(regex_escape_go).collect();
                let re = format!("^{}$", re.join("$/^"));
                format!(
                    "go test -count=1 -timeout 60s -run {} ./...",
                    shell_quote(&re)
                )
            }
            TestScope::Files(files) => {
                let mut dirs: Vec<String> = Vec::new();
                for f in files {
                    let d = Path::new(f)
                        .parent()
                        .and_then(|p| p.to_str())
                        .filter(|p| !p.is_empty())
                        .unwrap_or(".");
                    push_unique(&mut dirs, format!("./{}", d.trim_start_matches("./")));
                }
                if dirs.is_empty() {
                    "go test -count=1 -timeout 120s ./...".into()
                } else {
                    let pkgs: Vec<String> = dirs.iter().map(|d| shell_quote(d)).collect();
                    format!("go test -count=1 -timeout 120s {}", pkgs.join(" "))
                }
            }
        })
    }
    fn lint_command(&self) -> Option<String> {
        Some("go vet ./...".into())
    }
    fn format_command(&self) -> Option<String> {
        Some(
            "out=$(gofmt -l .); if [ -n \"$out\" ]; then echo \"gofmt would change:\"; echo \"$out\"; exit 1; fi"
                .into(),
        )
    }
    fn is_error_start(&self, line: &str) -> bool {
        has_loc_prefix(line) || line.starts_with("--- FAIL:") || line.starts_with("panic:")
    }
    fn parse_failed_tests(&self, output: &str) -> Vec<String> {
        let mut names = Vec::new();
        let mut in_timeout_list = false;
        for line in output.lines() {
            let t = line.trim();
            if let Some(rest) = t.strip_prefix("--- FAIL:") {
                if let Some(name) = rest.split_whitespace().next() {
                    push_unique(&mut names, name.to_string());
                }
            }
            if line.contains("panic: test timed out") {
                in_timeout_list = false;
            }
            if t == "running tests:" {
                in_timeout_list = true;
                continue;
            }
            if in_timeout_list {
                if line.starts_with(char::is_whitespace) && !t.is_empty() {
                    if let Some(name) = t.split_whitespace().next() {
                        push_unique(&mut names, name.to_string());
                    }
                } else {
                    in_timeout_list = false;
                }
            }
        }
        // Only top-level tests can be passed to -run directly.
        names
    }
}

fn regex_escape_go(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if "\\.+*?()|[]{}^$".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

// ---------------------------------------------------------------------------
// uv / pytest
// ---------------------------------------------------------------------------

pub struct Python {
    root: PathBuf,
    /// "uv run " when the project uses uv, otherwise empty.
    prefix: &'static str,
}

#[async_trait]
impl BuildSystem for Python {
    fn name(&self) -> &str {
        if self.prefix.is_empty() {
            "pytest"
        } else {
            "uv"
        }
    }
    fn root(&self) -> &Path {
        &self.root
    }
    fn detect(root: &Path) -> Option<Self> {
        let uv = root.join("uv.lock").is_file()
            || std::fs::read_to_string(root.join("pyproject.toml"))
                .map(|s| s.contains("[tool.uv"))
                .unwrap_or(false);
        let python = root.join("pyproject.toml").is_file()
            || root.join("pytest.ini").is_file()
            || root.join("setup.py").is_file()
            || root.join("tox.ini").is_file();
        python.then(|| Self {
            root: root.to_path_buf(),
            prefix: if uv { "uv run " } else { "" },
        })
    }
    fn is_source(&self, path: &str) -> bool {
        matches!(ext(path), "py" | "pyi") || file_name(path) == "pyproject.toml"
    }
    fn is_test(&self, path: &str) -> bool {
        let name = file_name(path);
        matches!(ext(path), "py" | "pyi")
            && (has_dir(path, &["tests", "test"])
                || name.starts_with("test_")
                || name.ends_with("_test.py")
                || name == "conftest.py")
    }
    fn build_command(&self) -> Option<String> {
        // Python has no build; byte-compiling catches syntax errors.
        Some(format!(
            "{}python -m compileall -q -x '(^|/)(\\.venv|venv|node_modules|\\.git)(/|$)' .",
            self.prefix
        ))
    }
    fn test_command(&self, scope: &TestScope) -> Option<String> {
        let base = format!("{}pytest -q", self.prefix);
        Some(match scope {
            TestScope::All => base,
            TestScope::Test(name) => format!("{base} {}", shell_quote(name)),
            TestScope::Files(files) if files.is_empty() => base,
            TestScope::Files(files) => {
                let args: Vec<String> = files.iter().map(|f| shell_quote(f)).collect();
                format!("{base} {}", args.join(" "))
            }
        })
    }
    fn lint_command(&self) -> Option<String> {
        Some(format!("{}ruff check .", self.prefix))
    }
    fn format_command(&self) -> Option<String> {
        Some(format!("{}ruff format --check .", self.prefix))
    }
    fn is_error_start(&self, line: &str) -> bool {
        (line.starts_with("___") && line.trim_end().ends_with("___"))
            || has_loc_prefix(line)
            || line.starts_with("*** Error compiling")
            || line.starts_with("Would reformat:")
    }
    fn parse_failed_tests(&self, output: &str) -> Vec<String> {
        let mut names = Vec::new();
        for line in output.lines() {
            let t = line.trim_start();
            if let Some(rest) = t
                .strip_prefix("FAILED ")
                .or_else(|| t.strip_prefix("ERROR "))
            {
                let name = rest.split(" - ").next().unwrap_or(rest).trim();
                push_unique(&mut names, name.to_string());
            }
        }
        names
    }
}

// ---------------------------------------------------------------------------
// npm / pnpm / yarn
// ---------------------------------------------------------------------------

pub struct Node {
    root: PathBuf,
    pm: &'static str,
    scripts: Vec<String>,
    has_tsconfig: bool,
}

impl Node {
    fn has_script(&self, name: &str) -> bool {
        self.scripts.iter().any(|s| s == name)
    }

    fn run_script(&self, name: &str) -> String {
        match self.pm {
            "yarn" => format!("yarn {name}"),
            pm => format!("{pm} run {name}"),
        }
    }

    /// Run a locally installed binary.
    fn exec_bin(&self, bin: &str) -> String {
        match self.pm {
            "pnpm" => format!("pnpm exec {bin}"),
            "yarn" => format!("yarn {bin}"),
            _ => format!("npx --no-install {bin}"),
        }
    }
}

#[async_trait]
impl BuildSystem for Node {
    fn name(&self) -> &str {
        self.pm
    }
    fn root(&self) -> &Path {
        &self.root
    }
    fn detect(root: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(root.join("package.json")).ok()?;
        let json: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
        let scripts = json
            .get("scripts")
            .and_then(|s| s.as_object())
            .map(|o| o.keys().cloned().collect())
            .unwrap_or_default();
        let pm = if root.join("pnpm-lock.yaml").is_file() {
            "pnpm"
        } else if root.join("yarn.lock").is_file() {
            "yarn"
        } else {
            "npm"
        };
        Some(Self {
            root: root.to_path_buf(),
            pm,
            scripts,
            has_tsconfig: root.join("tsconfig.json").is_file(),
        })
    }
    fn is_source(&self, path: &str) -> bool {
        matches!(
            ext(path),
            "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" | "mts" | "cts" | "vue" | "svelte"
        ) || file_name(path) == "package.json"
    }
    fn is_test(&self, path: &str) -> bool {
        let name = file_name(path);
        self.is_source(path)
            && (name.contains(".test.")
                || name.contains(".spec.")
                || has_dir(path, &["__tests__", "tests", "test"]))
    }
    fn build_command(&self) -> Option<String> {
        if self.has_tsconfig {
            Some(format!("{} --noEmit", self.exec_bin("tsc")))
        } else if self.has_script("build") {
            Some(self.run_script("build"))
        } else {
            None
        }
    }
    fn test_command(&self, scope: &TestScope) -> Option<String> {
        if !self.has_script("test") {
            return None;
        }
        let base = match self.pm {
            "yarn" => "yarn test".to_string(),
            pm => format!("{pm} test"),
        };
        // Both vitest and jest take `-t <name pattern>` and file paths.
        let sep = if self.pm == "yarn" { "" } else { "-- " };
        Some(match scope {
            TestScope::All => format!("CI=1 {base}"),
            TestScope::Test(name) => format!("CI=1 {base} {sep}-t {}", shell_quote(name)),
            TestScope::Files(files) if files.is_empty() => format!("CI=1 {base}"),
            TestScope::Files(files) => {
                let args: Vec<String> = files.iter().map(|f| shell_quote(f)).collect();
                format!("CI=1 {base} {sep}{}", args.join(" "))
            }
        })
    }
    fn lint_command(&self) -> Option<String> {
        self.has_script("lint").then(|| self.run_script("lint"))
    }
    fn format_command(&self) -> Option<String> {
        [
            "format:check",
            "format-check",
            "check-format",
            "prettier:check",
        ]
        .iter()
        .find(|s| self.has_script(s))
        .map(|s| self.run_script(s))
    }
    fn is_error_start(&self, line: &str) -> bool {
        let t = line.trim_start();
        // tsc: `src/a.ts(3,5): error TS2322:` or `src/a.ts:3:5 - error TS2322:`
        (line.contains("): error TS") || line.contains(" - error TS"))
            || t.starts_with("FAIL ")
            || t.starts_with("● ")
            || t.starts_with("× ")
            || (has_loc_prefix(line) && line.contains("error"))
    }
    fn parse_failed_tests(&self, output: &str) -> Vec<String> {
        let mut names = Vec::new();
        for line in output.lines() {
            let t = line.trim();
            if let Some(rest) = t.strip_prefix("FAIL ") {
                push_unique(
                    &mut names,
                    rest.split(" [").next().unwrap_or(rest).trim().to_string(),
                );
            } else if let Some(rest) = t.strip_prefix("● ") {
                push_unique(&mut names, rest.trim().to_string());
            } else if let Some(rest) = t.strip_prefix("× ") {
                let n = rest.rsplit_once(' ').filter(|(_, d)| d.ends_with("ms"));
                push_unique(
                    &mut names,
                    n.map(|(a, _)| a).unwrap_or(rest).trim().to_string(),
                );
            }
        }
        names
    }
}

// ---------------------------------------------------------------------------
// cmake / ctest
// ---------------------------------------------------------------------------

pub struct CMake {
    root: PathBuf,
}

#[async_trait]
impl BuildSystem for CMake {
    fn name(&self) -> &str {
        "cmake"
    }
    fn root(&self) -> &Path {
        &self.root
    }
    fn detect(root: &Path) -> Option<Self> {
        root.join("CMakeLists.txt").is_file().then(|| Self {
            root: root.to_path_buf(),
        })
    }
    fn is_source(&self, path: &str) -> bool {
        matches!(
            ext(path),
            "c" | "cc" | "cpp" | "cxx" | "h" | "hh" | "hpp" | "hxx" | "cmake"
        ) || file_name(path) == "CMakeLists.txt"
    }
    fn is_test(&self, path: &str) -> bool {
        let name = file_name(path);
        self.is_source(path)
            && (has_dir(path, &["test", "tests"])
                || name.starts_with("test_")
                || name.contains("_test.")
                || name.contains(".test."))
    }
    fn build_command(&self) -> Option<String> {
        Some("cmake -S . -B build && cmake --build build".into())
    }
    fn test_command(&self, scope: &TestScope) -> Option<String> {
        let base = "ctest --test-dir build --output-on-failure";
        Some(match scope {
            TestScope::Test(name) => {
                format!(
                    "{base} -R {}",
                    shell_quote(&format!("^{}$", regex_escape_go(name)))
                )
            }
            _ => base.to_string(),
        })
    }
    fn is_error_start(&self, line: &str) -> bool {
        line.contains(": error")
            || line.contains(": fatal error")
            || line.starts_with("CMake Error")
            || line.contains("undefined reference to")
    }
    fn parse_failed_tests(&self, output: &str) -> Vec<String> {
        // "The following tests FAILED:" then `\t  3 - name (Failed)` lines.
        let mut names = Vec::new();
        let mut in_list = false;
        for line in output.lines() {
            if line.contains("The following tests FAILED") {
                in_list = true;
                continue;
            }
            if in_list {
                let t = line.trim();
                let Some((num, rest)) = t.split_once(" - ") else {
                    if !t.is_empty() {
                        in_list = false;
                    }
                    continue;
                };
                if num.bytes().all(|b| b.is_ascii_digit()) && !num.is_empty() {
                    let name = rest.rsplit_once(" (").map(|(n, _)| n).unwrap_or(rest);
                    push_unique(&mut names, name.trim().to_string());
                } else {
                    in_list = false;
                }
            }
        }
        names
    }
}

// ---------------------------------------------------------------------------
// generic Makefile
// ---------------------------------------------------------------------------

pub struct Make {
    root: PathBuf,
    targets: Vec<String>,
}

impl Make {
    fn has_target(&self, name: &str) -> bool {
        self.targets.iter().any(|t| t == name)
    }
}

/// Target names defined in a Makefile (`name:` at the start of a line).
fn make_targets(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.lines() {
        if line.starts_with(['\t', ' ', '#', '.']) {
            continue;
        }
        let Some((head, tail)) = line.split_once(':') else {
            continue;
        };
        if tail.starts_with('=') || head.contains(['=', '$', '%']) {
            continue;
        }
        for t in head.split_whitespace() {
            push_unique(&mut out, t.to_string());
        }
    }
    out
}

#[async_trait]
impl BuildSystem for Make {
    fn name(&self) -> &str {
        "make"
    }
    fn root(&self) -> &Path {
        &self.root
    }
    fn detect(root: &Path) -> Option<Self> {
        ["Makefile", "makefile", "GNUmakefile"]
            .iter()
            .find_map(|n| {
                let text = std::fs::read_to_string(root.join(n)).ok()?;
                Some(Self {
                    root: root.to_path_buf(),
                    targets: make_targets(&text),
                })
            })
    }
    fn is_source(&self, path: &str) -> bool {
        matches!(
            ext(path),
            "c" | "cc"
                | "cpp"
                | "cxx"
                | "h"
                | "hpp"
                | "rs"
                | "go"
                | "py"
                | "js"
                | "ts"
                | "java"
                | "rb"
                | "sh"
                | "mk"
        ) || matches!(file_name(path), "Makefile" | "makefile" | "GNUmakefile")
    }
    fn is_test(&self, path: &str) -> bool {
        let name = file_name(path);
        self.is_source(path)
            && (has_dir(path, &["test", "tests"])
                || name.starts_with("test_")
                || name.contains("_test.")
                || name.contains(".test."))
    }
    fn build_command(&self) -> Option<String> {
        Some("make".into())
    }
    fn test_command(&self, scope: &TestScope) -> Option<String> {
        let _ = scope; // a Makefile has no portable way to select one test
        if self.has_target("test") {
            Some("make test".into())
        } else if self.has_target("check") {
            Some("make check".into())
        } else {
            None
        }
    }
    fn lint_command(&self) -> Option<String> {
        self.has_target("lint").then(|| "make lint".to_string())
    }
    fn format_command(&self) -> Option<String> {
        ["fmt-check", "format-check", "check-format", "check-fmt"]
            .iter()
            .find(|t| self.has_target(t))
            .map(|t| format!("make {t}"))
    }
    fn is_error_start(&self, line: &str) -> bool {
        line.contains(": error")
            || line.contains(": fatal error")
            || line.starts_with("make: ***")
            || line.contains("undefined reference to")
    }
}

// ---------------------------------------------------------------------------
// Detection and verify state
// ---------------------------------------------------------------------------

/// Detect the project's build system. Order matters: the first language
/// manifest wins, and a bare Makefile is the last resort.
pub fn detect_build_system(root: &Path) -> Option<Box<dyn BuildSystem>> {
    fn boxed<T: BuildSystem + 'static>(b: Option<T>) -> Option<Box<dyn BuildSystem>> {
        b.map(|b| Box::new(b) as Box<dyn BuildSystem>)
    }
    boxed(Cargo::detect(root))
        .or_else(|| boxed(Go::detect(root)))
        .or_else(|| boxed(Python::detect(root)))
        .or_else(|| boxed(Node::detect(root)))
        .or_else(|| boxed(CMake::detect(root)))
        .or_else(|| boxed(Make::detect(root)))
}

/// The latest result of one verifier step, as the trailing note shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StepState {
    pub ok: bool,
    pub summary: String,
    pub failed_tests: Option<u32>,
}

impl StepState {
    fn of(o: &CheckOutcome) -> Option<Self> {
        (o.kind != OutcomeKind::Skipped && o.kind != OutcomeKind::Cancelled).then(|| Self {
            ok: o.ok,
            summary: o.summary.clone(),
            failed_tests: o.failed_tests,
        })
    }
}

/// What the verifier last saw. The background verifier records into this after
/// write events; the trailing note renders it on every step.
#[derive(Clone, Debug, Default)]
pub struct VerifyState {
    pub build: Option<StepState>,
    pub tests: Option<StepState>,
    pub lint: Option<StepState>,
    pub consecutive_build_failures: u32,
}

impl VerifyState {
    pub fn record_build(&mut self, o: &CheckOutcome) {
        if let Some(s) = StepState::of(o) {
            if s.ok {
                self.consecutive_build_failures = 0;
            } else {
                self.consecutive_build_failures += 1;
            }
            self.build = Some(s);
        }
    }

    pub fn record_tests(&mut self, o: &CheckOutcome) {
        if let Some(s) = StepState::of(o) {
            self.tests = Some(s);
        }
    }

    pub fn record_lint(&mut self, o: &CheckOutcome) {
        if let Some(s) = StepState::of(o) {
            self.lint = Some(s);
        }
    }

    pub fn build_is_red(&self) -> bool {
        self.build.as_ref().is_some_and(|b| !b.ok)
    }

    /// promptyped's repair-priority rule: the build is red now and the task
    /// did not start out red.
    pub fn needs_repair_priority(&self, started_red: bool) -> bool {
        self.build_is_red() && !started_red
    }

    /// True every `every` consecutive build failures: time for a progress
    /// check (promptyped `handleConsecutiveCompileFailures`).
    pub fn progress_check_due(&self, every: u32) -> bool {
        every > 0
            && self.consecutive_build_failures > 0
            && self.consecutive_build_failures.is_multiple_of(every)
    }

    /// The "LATEST COMPILE/TEST STATE" block, or "" when nothing ran yet.
    pub fn trailing_note(&self) -> String {
        if self.build.is_none() && self.tests.is_none() && self.lint.is_none() {
            return String::new();
        }
        fn describe(s: &Option<StepState>) -> String {
            match s {
                None => "not run yet".into(),
                Some(s) if s.ok => "Succeeded!".into(),
                Some(s) => format!("Failed. {}", s.summary),
            }
        }
        let mut note = String::from(
            "LATEST COMPILE/TEST STATE (these are the newest results, even if older messages show something different):\n",
        );
        note.push_str(&format!("Last compilation: {}\n", describe(&self.build)));
        note.push_str(&format!("Last test run: {}\n", describe(&self.tests)));
        if self.lint.is_some() {
            note.push_str(&format!("Last lint: {}\n", describe(&self.lint)));
        }
        note
    }
}

// ---------------------------------------------------------------------------
// Test fake
// ---------------------------------------------------------------------------

/// A scripted runner for tests. Rules match when the command contains the
/// pattern; the first matching rule answers, popping its queue and repeating
/// the last response. Unmatched commands exit 127.
#[cfg(test)]
#[derive(Default)]
pub struct FakeRunner {
    rules: std::sync::Mutex<Vec<(String, std::collections::VecDeque<CommandOutput>)>>,
    calls: std::sync::Mutex<Vec<String>>,
}

#[cfg(test)]
impl FakeRunner {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn on(self: &Arc<Self>, pattern: &str, responses: Vec<CommandOutput>) -> Arc<Self> {
        self.rules
            .lock()
            .unwrap()
            .push((pattern.to_string(), responses.into()));
        self.clone()
    }

    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

#[cfg(test)]
#[async_trait]
impl CommandRunner for FakeRunner {
    async fn run(&self, spec: &CommandSpec) -> CommandOutput {
        self.calls.lock().unwrap().push(spec.command.clone());
        let mut rules = self.rules.lock().unwrap();
        for (pattern, queue) in rules.iter_mut() {
            if spec.command.contains(pattern.as_str()) {
                return if queue.len() > 1 {
                    queue.pop_front().unwrap()
                } else {
                    queue.front().cloned().unwrap_or_default()
                };
            }
        }
        CommandOutput::failed(127, format!("fake runner: no rule for `{}`", spec.command))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exec(fake: &Arc<FakeRunner>) -> Exec {
        Exec::new(fake.clone())
    }

    fn cargo() -> Cargo {
        Cargo {
            root: PathBuf::from("/proj"),
        }
    }

    const CARGO_ERRORS: &str = "   Compiling foo v0.1.0 (/p)\n\
error[E0308]: mismatched types\n --> src/lib.rs:2:5\n  |\n2 |     1\n  |     ^ expected `()`\n\n\
error[E0425]: cannot find value `x` in this scope\n --> src/lib.rs:5:5\n\n\
error[E0599]: no method named `foo`\n --> src/lib.rs:9:5\n\n\
error: aborting due to 3 previous errors\n";

    // --- helpers ---

    #[test]
    fn condense_keeps_first_errors_and_counts() {
        let c = condense(CARGO_ERRORS, |l| cargo().is_error_start(l), 2);
        assert_eq!((c.total, c.shown), (3, 2));
        assert!(c.text.contains("E0308") && c.text.contains("E0425"));
        assert!(!c.text.contains("E0599"));
        assert!(!c.text.contains("Compiling"), "preamble is dropped");
        let r = c.render();
        assert!(r.contains("1 more error(s) not shown (3 in total)"), "{r}");
    }

    #[test]
    fn condense_all_shown_notes_total() {
        let c = condense(CARGO_ERRORS, |l| cargo().is_error_start(l), 10);
        assert_eq!((c.total, c.shown), (3, 3));
        assert!(c.render().ends_with("(3 errors in total)"));
        let one = condense("error: x\n  detail", |l| l.starts_with("error"), 5);
        assert_eq!(one.render(), "error: x\n  detail");
    }

    #[test]
    fn condense_without_blocks_falls_back_to_the_output() {
        let c = condense("plain failure\nmore", |_| false, 3);
        assert_eq!(c.total, 0);
        assert_eq!(c.text, "plain failure\nmore");
        let long: String = (0..200).map(|i| format!("line {i}\n")).collect();
        let c = condense(&long, |_| false, 3);
        assert!(c.text.contains("line 0") && c.text.contains("line 199"));
        assert!(c.text.contains("lines omitted"));
        assert!(!c.text.contains("line 100\n"));
    }

    #[test]
    fn condense_caps_block_length_and_total_size() {
        let mut out = String::from("error: big\n");
        for i in 0..100 {
            out.push_str(&format!("  detail {i}\n"));
        }
        let c = condense(&out, |l| l.starts_with("error"), 1);
        assert!(c.text.contains("… 61 more lines"), "{}", c.text);
        assert!(c.text.contains("detail 38") && !c.text.contains("detail 60"));

        let huge = format!("error: {}", "x".repeat(20_000));
        let c = condense(&huge, |l| l.starts_with("error"), 1);
        assert!(c.text.chars().count() < 8_100);
        assert!(c.text.ends_with("(truncated)"));
    }

    #[test]
    fn shorten_old_errors_keeps_only_the_newest_whole() {
        let long = "e".repeat(500);
        let msgs = vec![long.clone(), "short".to_string(), long.clone()];
        let out = shorten_old_errors(&msgs, 50);
        assert!(out[0].ends_with("… (truncated)") && out[0].chars().count() < 80);
        assert_eq!(out[1], "short");
        assert_eq!(out[2], long);
        assert!(shorten_old_errors(&[], 10).is_empty());
        assert_eq!(
            shorten_old_errors(&["x".repeat(99)], 10),
            vec!["x".repeat(99)]
        );
    }

    #[test]
    fn shell_quoting() {
        assert_eq!(shell_quote("tests::a_b"), "tests::a_b");
        assert_eq!(shell_quote("has space"), "'has space'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("$(rm -rf)"), "'$(rm -rf)'");
    }

    #[test]
    fn loc_prefix() {
        assert!(has_loc_prefix("main.go:12:5: undefined: x"));
        assert!(has_loc_prefix("a/b.py:3: error"));
        assert!(has_loc_prefix("src/x.c:10:2: error: boom"));
        assert!(!has_loc_prefix("note: see main.go:12:5: here"));
        assert!(!has_loc_prefix("12:5: nope"));
        assert!(!has_loc_prefix("plain"));
    }

    #[test]
    fn tool_output_parsing() {
        let o = from_tool_output("built\nexit code 101".into(), true);
        assert_eq!((o.exit_code, o.output.as_str()), (Some(101), "built"));
        let o = from_tool_output("fine".into(), false);
        assert_eq!(o.exit_code, Some(0));
        let o = from_tool_output("(no output)".into(), false);
        assert_eq!((o.exit_code, o.output.as_str()), (Some(0), ""));
        let o = from_tool_output("x\nexit code 1 (no matches found)".into(), false);
        assert_eq!(o.exit_code, Some(1), "a benign exit is still an exit code");
        assert!(!o.success());
        let o = from_tool_output("Cancelled by user.".into(), true);
        assert!(o.cancelled);
        let o = from_tool_output(
            "slow\ncommand timed out after 3s (process group terminated)\nkilled by signal 15"
                .into(),
            true,
        );
        assert!(o.timed_out && o.exit_code.is_none());
        let o = from_tool_output("bash: no such shell".into(), true);
        assert_eq!(o.exit_code, None);
        assert!(!o.success());
    }

    // --- detection ---

    fn dir_with(files: &[(&str, &str)]) -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        for (name, body) in files {
            let p = d.path().join(name);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, body).unwrap();
        }
        d
    }

    fn detected(files: &[(&str, &str)]) -> Option<String> {
        let d = dir_with(files);
        detect_build_system(d.path()).map(|b| b.name().to_string())
    }

    #[test]
    fn detects_each_system() {
        assert_eq!(detected(&[("Cargo.toml", "")]).as_deref(), Some("cargo"));
        assert_eq!(detected(&[("go.mod", "module x")]).as_deref(), Some("go"));
        assert_eq!(
            detected(&[("pyproject.toml", "[project]")]).as_deref(),
            Some("pytest")
        );
        assert_eq!(
            detected(&[("pyproject.toml", "[project]"), ("uv.lock", "")]).as_deref(),
            Some("uv")
        );
        assert_eq!(
            detected(&[("pyproject.toml", "[tool.uv]\nx=1")]).as_deref(),
            Some("uv")
        );
        assert_eq!(detected(&[("package.json", "{}")]).as_deref(), Some("npm"));
        assert_eq!(
            detected(&[("package.json", "{}"), ("pnpm-lock.yaml", "")]).as_deref(),
            Some("pnpm")
        );
        assert_eq!(
            detected(&[("package.json", "{}"), ("yarn.lock", "")]).as_deref(),
            Some("yarn")
        );
        assert_eq!(
            detected(&[("CMakeLists.txt", "")]).as_deref(),
            Some("cmake")
        );
        assert_eq!(
            detected(&[("Makefile", "all:\n\ttrue\n")]).as_deref(),
            Some("make")
        );
        assert_eq!(detected(&[("README.md", "")]), None);
    }

    #[test]
    fn detection_priority() {
        assert_eq!(
            detected(&[
                ("Cargo.toml", ""),
                ("Makefile", "all:\n"),
                ("package.json", "{}")
            ])
            .as_deref(),
            Some("cargo")
        );
        assert_eq!(
            detected(&[("package.json", "{}"), ("Makefile", "all:\n")]).as_deref(),
            Some("npm")
        );
        assert_eq!(
            detected(&[("CMakeLists.txt", ""), ("Makefile", "all:\n")]).as_deref(),
            Some("cmake")
        );
    }

    #[test]
    fn broken_package_json_still_detects() {
        assert_eq!(
            detected(&[("package.json", "{ not json")]).as_deref(),
            Some("npm")
        );
    }

    // --- commands ---

    #[test]
    fn cargo_commands() {
        let c = cargo();
        assert!(c
            .build_command()
            .unwrap()
            .starts_with("cargo build --workspace --all-targets"));
        assert!(
            c.lint_command().unwrap().contains("clippy")
                && c.lint_command().unwrap().contains("-D warnings")
        );
        assert_eq!(c.format_command().unwrap(), "cargo fmt --all -- --check");
        assert!(c
            .test_command(&TestScope::All)
            .unwrap()
            .ends_with("--color never"));
        let t = c.test_command(&TestScope::Test("a::b".into())).unwrap();
        assert!(t.ends_with("a::b -- --exact"), "{t}");
        let f = c
            .test_command(&TestScope::Files(vec![
                "tests/api.rs".into(),
                "tests/db.rs".into(),
            ]))
            .unwrap();
        assert!(f.ends_with("--test api --test db"), "{f}");
        let mixed = c
            .test_command(&TestScope::Files(vec![
                "tests/api.rs".into(),
                "src/lib.rs".into(),
            ]))
            .unwrap();
        assert!(!mixed.contains("--test"), "{mixed}");
    }

    #[test]
    fn go_commands() {
        let g = Go {
            root: PathBuf::from("/p"),
        };
        assert!(g.build_command().unwrap().contains("go build ./..."));
        assert_eq!(g.lint_command().unwrap(), "go vet ./...");
        assert!(g.format_command().unwrap().contains("gofmt -l"));
        let t = g.test_command(&TestScope::Test("TestX".into())).unwrap();
        assert!(t.contains("-run '^TestX$'"), "{t}");
        let t = g
            .test_command(&TestScope::Test("TestX/sub case".into()))
            .unwrap();
        assert!(t.contains("^TestX$/^sub case$"), "{t}");
        let t = g
            .test_command(&TestScope::Files(vec![
                "pkg/a/a_test.go".into(),
                "main_test.go".into(),
            ]))
            .unwrap();
        assert!(t.ends_with("./pkg/a ./."), "{t}");
    }

    #[test]
    fn python_commands() {
        let d = dir_with(&[("pyproject.toml", ""), ("uv.lock", "")]);
        let p = Python::detect(d.path()).unwrap();
        assert_eq!(p.test_command(&TestScope::All).unwrap(), "uv run pytest -q");
        assert_eq!(
            p.test_command(&TestScope::Test("tests/a.py::t".into()))
                .unwrap(),
            "uv run pytest -q tests/a.py::t"
        );
        assert_eq!(p.lint_command().unwrap(), "uv run ruff check .");
        assert!(p
            .build_command()
            .unwrap()
            .starts_with("uv run python -m compileall"));
        let d = dir_with(&[("pytest.ini", "")]);
        let p = Python::detect(d.path()).unwrap();
        assert_eq!(p.test_command(&TestScope::All).unwrap(), "pytest -q");
    }

    #[test]
    fn node_commands() {
        let pkg = r#"{"scripts":{"test":"vitest run","lint":"eslint .","format:check":"prettier --check ."}}"#;
        let d = dir_with(&[
            ("package.json", pkg),
            ("tsconfig.json", "{}"),
            ("pnpm-lock.yaml", ""),
        ]);
        let n = Node::detect(d.path()).unwrap();
        assert_eq!(n.build_command().unwrap(), "pnpm exec tsc --noEmit");
        assert_eq!(n.test_command(&TestScope::All).unwrap(), "CI=1 pnpm test");
        assert_eq!(
            n.test_command(&TestScope::Test("adds up".into())).unwrap(),
            "CI=1 pnpm test -- -t 'adds up'"
        );
        assert_eq!(
            n.test_command(&TestScope::Files(vec!["src/a.test.ts".into()]))
                .unwrap(),
            "CI=1 pnpm test -- src/a.test.ts"
        );
        assert_eq!(n.lint_command().unwrap(), "pnpm run lint");
        assert_eq!(n.format_command().unwrap(), "pnpm run format:check");

        let d = dir_with(&[("package.json", pkg), ("yarn.lock", "")]);
        let n = Node::detect(d.path()).unwrap();
        assert_eq!(n.build_command(), None, "no tsconfig and no build script");
        assert_eq!(
            n.test_command(&TestScope::Test("x".into())).unwrap(),
            "CI=1 yarn test -t x"
        );

        let d = dir_with(&[
            ("package.json", r#"{"scripts":{"build":"vite build"}}"#),
            ("tsconfig.json", ""),
        ]);
        let n = Node::detect(d.path()).unwrap();
        assert_eq!(n.build_command().unwrap(), "npx --no-install tsc --noEmit");
        let d = dir_with(&[("package.json", r#"{"scripts":{"build":"vite build"}}"#)]);
        let n = Node::detect(d.path()).unwrap();
        assert_eq!(n.build_command().unwrap(), "npm run build");
        assert_eq!(n.test_command(&TestScope::All), None);
        assert_eq!(n.lint_command(), None);
    }

    #[test]
    fn cmake_commands() {
        let c = CMake {
            root: PathBuf::from("/p"),
        };
        assert!(c.build_command().unwrap().contains("cmake --build build"));
        assert_eq!(
            c.test_command(&TestScope::All).unwrap(),
            "ctest --test-dir build --output-on-failure"
        );
        let t = c
            .test_command(&TestScope::Test("unit.core".into()))
            .unwrap();
        assert!(t.ends_with("-R '^unit\\.core$'"), "{t}");
    }

    #[test]
    fn makefile_targets() {
        let text = "CC=gcc\n.PHONY: all test\nall: main.o\n\t$(CC) -o main main.o\n\ntest: all\n\t./run\nlint fmt-check:\n\ttrue\n%.o: %.c\n\t$(CC) -c $<\nFOO := bar\n";
        let t = make_targets(text);
        assert_eq!(t, vec!["all", "test", "lint", "fmt-check"]);
        let m = Make {
            root: PathBuf::from("/p"),
            targets: t,
        };
        assert_eq!(m.build_command().unwrap(), "make");
        assert_eq!(m.test_command(&TestScope::All).unwrap(), "make test");
        assert_eq!(m.lint_command().unwrap(), "make lint");
        assert_eq!(m.format_command().unwrap(), "make fmt-check");
        let bare = Make {
            root: PathBuf::from("/p"),
            targets: vec!["all".into()],
        };
        assert_eq!(bare.test_command(&TestScope::All), None);
        assert_eq!(bare.lint_command(), None);
        let check = Make {
            root: PathBuf::from("/p"),
            targets: vec!["check".into()],
        };
        assert_eq!(check.test_command(&TestScope::All).unwrap(), "make check");
    }

    // --- parsing failed tests ---

    #[test]
    fn cargo_failed_tests() {
        let out = "running 3 tests\ntest a::ok ... ok\ntest a::bad ... FAILED\ntest b::worse ... FAILED\n\nfailures:\n\n---- a::bad stdout ----\nboom\n\nfailures:\n    a::bad\n    b::worse\n\ntest result: FAILED. 1 passed; 2 failed;\n";
        assert_eq!(cargo().parse_failed_tests(out), vec!["a::bad", "b::worse"]);
        let nextest = "        FAIL [   0.004s] mycrate tests::broken\n        FAIL [   0.004s] mycrate tests::broken\n";
        assert_eq!(
            cargo().parse_failed_tests(nextest),
            vec!["mycrate tests::broken"]
        );
        assert!(cargo().parse_failed_tests("all good").is_empty());
        let doc = "test src/lib.rs - foo (line 3) ... FAILED";
        assert_eq!(
            cargo().parse_failed_tests(doc),
            vec!["src/lib.rs - foo (line 3)"]
        );
    }

    #[test]
    fn go_failed_tests() {
        let g = Go {
            root: PathBuf::from("/p"),
        };
        let out = "--- FAIL: TestA (0.00s)\n    a_test.go:10: bad\n--- FAIL: TestB/sub (0.01s)\n--- FAIL: TestA (0.00s)\nFAIL\n";
        assert_eq!(g.parse_failed_tests(out), vec!["TestA", "TestB/sub"]);
        let timeout = "panic: test timed out after 10s\n\trunning tests:\n\t\tTestSlow (10s)\n\t\tTestSlower (10s)\n\ngoroutine 1\n";
        assert_eq!(
            g.parse_failed_tests(timeout),
            vec!["TestSlow", "TestSlower"]
        );
    }

    #[test]
    fn pytest_failed_tests() {
        let d = dir_with(&[("pytest.ini", "")]);
        let p = Python::detect(d.path()).unwrap();
        let out = "=== short test summary info ===\nFAILED tests/a.py::test_x - assert 1 == 2\nFAILED tests/a.py::test_x - assert 1 == 2\nERROR tests/b.py::test_y\n";
        assert_eq!(
            p.parse_failed_tests(out),
            vec!["tests/a.py::test_x", "tests/b.py::test_y"]
        );
    }

    #[test]
    fn node_failed_tests_and_errors() {
        let d = dir_with(&[("package.json", "{}")]);
        let n = Node::detect(d.path()).unwrap();
        let out = " FAIL  src/a.test.ts > adds up\n ● other thing\n × third 12ms\n";
        assert_eq!(
            n.parse_failed_tests(out),
            vec!["src/a.test.ts > adds up", "other thing", "third"]
        );
        assert!(n.is_error_start("src/a.ts(3,5): error TS2322: nope"));
        assert!(n.is_error_start("src/a.ts:3:5 - error TS2322: nope"));
        assert!(!n.is_error_start("  at Object.<anonymous>"));
    }

    #[test]
    fn ctest_failed_tests() {
        let c = CMake {
            root: PathBuf::from("/p"),
        };
        let out = "50% tests passed, 2 tests failed out of 4\n\nThe following tests FAILED:\n\t  2 - unit.parse (Failed)\n\t  4 - unit.io (Timeout)\nErrors while running CTest\n";
        assert_eq!(c.parse_failed_tests(out), vec!["unit.parse", "unit.io"]);
    }

    #[test]
    fn source_and_test_file_classification() {
        let c = cargo();
        assert!(
            c.is_source("src/main.rs") && c.is_source("Cargo.toml") && !c.is_source("README.md")
        );
        assert!(
            c.is_test("tests/api.rs")
                && c.is_test("src/agent/tests.rs")
                && !c.is_test("src/main.rs")
        );
        let g = Go {
            root: PathBuf::from("/p"),
        };
        assert!(g.is_source("x/y.go") && g.is_test("x/y_test.go") && !g.is_test("x/y.go"));
        let d = dir_with(&[("pytest.ini", "")]);
        let p = Python::detect(d.path()).unwrap();
        assert!(
            p.is_test("tests/test_a.py") && p.is_test("pkg/test_b.py") && p.is_test("conftest.py")
        );
        assert!(!p.is_test("pkg/b.py") && p.is_source("pkg/b.py"));
        let d = dir_with(&[("package.json", "{}")]);
        let n = Node::detect(d.path()).unwrap();
        assert!(
            n.is_test("src/a.test.ts") && n.is_test("src/__tests__/a.ts") && !n.is_test("src/a.ts")
        );
        assert!(n.is_source("src/a.tsx") && !n.is_source("logo.png"));
        let cm = CMake {
            root: PathBuf::from("/p"),
        };
        assert!(
            cm.is_source("src/a.cpp") && cm.is_source("CMakeLists.txt") && cm.is_test("test/a.cpp")
        );
    }

    // --- running steps with the fake ---

    #[tokio::test]
    async fn build_pass() {
        let f = FakeRunner::new().on("cargo build", vec![CommandOutput::ok("Finished")]);
        let o = cargo().build(&exec(&f)).await;
        assert!(o.ok && o.kind == OutcomeKind::Passed);
        assert!(o.summary.contains("cargo build") && o.summary.contains("ok"));
        assert_eq!(o.full_output, "Finished");
        assert_eq!(o.failed_tests, None);
        assert_eq!(f.calls().len(), 1);
    }

    #[tokio::test]
    async fn build_failure_is_condensed() {
        let f = FakeRunner::new().on(
            "cargo build",
            vec![CommandOutput::failed(101, CARGO_ERRORS)],
        );
        let o = cargo().build(&exec(&f).with_max_errors(2)).await;
        assert!(!o.ok && o.kind == OutcomeKind::Failed);
        assert!(o.summary.contains("exit code 101"), "{}", o.summary);
        assert!(o.summary.contains("E0308") && o.summary.contains("E0425"));
        assert!(!o.summary.contains("E0599"));
        assert!(o.summary.contains("1 more error(s)"));
        assert_eq!(o.full_output, CARGO_ERRORS, "full output is kept");
    }

    #[tokio::test]
    async fn timeout_and_cancel_have_their_own_kinds() {
        let f = FakeRunner::new().on(
            "cargo build",
            vec![CommandOutput::timeout("compiling...\nstill compiling")],
        );
        let o = cargo()
            .build(&exec(&f).with_timeouts(Duration::from_secs(7), Duration::from_secs(9)))
            .await;
        assert_eq!(o.kind, OutcomeKind::TimedOut);
        assert!(!o.ok);
        assert!(o.summary.contains("timed out after 7s") && o.summary.contains("still compiling"));

        let f = FakeRunner::new().on("cargo test", vec![CommandOutput::cancelled()]);
        let o = cargo().test(&exec(&f), &TestScope::All).await;
        assert_eq!(o.kind, OutcomeKind::Cancelled);
        assert!(!o.ok && o.summary.contains("cancelled"));
    }

    #[tokio::test]
    async fn missing_step_is_skipped_not_failed() {
        let d = dir_with(&[("package.json", "{}")]);
        let n = Node::detect(d.path()).unwrap();
        let f = FakeRunner::new();
        let ex = exec(&f);
        for o in [
            n.build(&ex).await,
            n.lint(&ex).await,
            n.format_check(&ex).await,
            n.test(&ex, &TestScope::All).await,
        ] {
            assert!(o.ok && o.kind == OutcomeKind::Skipped, "{:?}", o);
            assert!(o.summary.contains("skipped"));
        }
        assert!(f.calls().is_empty(), "nothing was run");
    }

    #[tokio::test]
    async fn lint_and_format_run_their_commands() {
        let f = FakeRunner::new()
            .on(
                "clippy",
                vec![CommandOutput::failed(
                    101,
                    "error: this could be simpler\n --> src/a.rs:1:1\n",
                )],
            )
            .on(
                "fmt",
                vec![CommandOutput::failed(1, "Diff in /p/src/a.rs:1:\n-x\n+y\n")],
            );
        let l = cargo().lint(&exec(&f)).await;
        assert!(!l.ok && l.summary.contains("this could be simpler"));
        let m = cargo().format_check(&exec(&f)).await;
        assert!(!m.ok && m.summary.contains("Diff in"));
    }

    #[tokio::test]
    async fn small_test_failure_is_reported_directly() {
        let out = "test a::bad ... FAILED\ntest result: FAILED. 0 passed; 1 failed;\n";
        let f = FakeRunner::new().on("cargo test", vec![CommandOutput::failed(101, out)]);
        let o = cargo().test(&exec(&f), &TestScope::All).await;
        assert!(!o.ok);
        assert_eq!(o.failed_tests, Some(1));
        assert!(o.summary.contains("1 failing test"));
        assert_eq!(f.calls().len(), 1, "no rerun for a small report");
    }

    fn big_failure() -> String {
        let mut s = String::from("running 3 tests\ntest a::bad ... FAILED\ntest b::worse ... FAILED\ntest c::ok ... ok\n");
        for i in 0..300 {
            s.push_str(&format!("backtrace line {i}\n"));
        }
        s
    }

    #[tokio::test]
    async fn big_test_failure_reruns_a_single_test() {
        let f = FakeRunner::new()
            .on(
                "-- --exact",
                vec![CommandOutput::failed(
                    101,
                    "---- a::bad stdout ----\nassertion failed: 1 == 2\ntest a::bad ... FAILED\n",
                )],
            )
            .on(
                "cargo test",
                vec![CommandOutput::failed(101, big_failure())],
            );
        let o = cargo().test(&exec(&f), &TestScope::All).await;
        assert!(!o.ok);
        assert_eq!(o.failed_tests, Some(2));
        assert!(
            o.summary.contains("only `a::bad` is shown"),
            "{}",
            o.summary
        );
        assert!(o.summary.contains("assertion failed"));
        assert!(
            !o.summary.contains("backtrace line 100"),
            "the big report is not in the summary"
        );
        assert!(o.full_output.contains("backtrace line 299"));
        let calls = f.calls();
        assert_eq!(calls.len(), 2);
        assert!(calls[1].contains("a::bad -- --exact"));
    }

    #[tokio::test]
    async fn rerun_moves_on_when_a_test_passes_alone() {
        let f = FakeRunner::new()
            .on(
                "a::bad -- --exact",
                vec![CommandOutput::ok("test a::bad ... ok")],
            )
            .on(
                "b::worse -- --exact",
                vec![CommandOutput::failed(
                    101,
                    "test b::worse ... FAILED\nreal failure",
                )],
            )
            .on(
                "cargo test",
                vec![CommandOutput::failed(101, big_failure())],
            );
        let o = cargo().test(&exec(&f), &TestScope::All).await;
        assert!(
            o.summary.contains("only `b::worse` is shown"),
            "{}",
            o.summary
        );
        assert_eq!(f.calls().len(), 3);
    }

    #[tokio::test]
    async fn all_reruns_passing_is_reported_as_flaky() {
        let f = FakeRunner::new()
            .on("-- --exact", vec![CommandOutput::ok("ok")])
            .on(
                "cargo test",
                vec![CommandOutput::failed(101, big_failure())],
            );
        let o = cargo().test(&exec(&f), &TestScope::All).await;
        assert!(!o.ok);
        assert!(
            o.summary.contains("test interaction or a flaky test"),
            "{}",
            o.summary
        );
        assert!(o.summary.contains("a::bad, b::worse"));
        assert_eq!(o.failed_tests, Some(2));
    }

    #[tokio::test]
    async fn reruns_are_capped() {
        let mut out = String::new();
        for i in 0..10 {
            out.push_str(&format!("test t::n{i} ... FAILED\n"));
        }
        for i in 0..300 {
            out.push_str(&format!("noise {i}\n"));
        }
        let f = FakeRunner::new()
            .on("-- --exact", vec![CommandOutput::ok("ok")])
            .on("cargo test", vec![CommandOutput::failed(101, out)]);
        let mut ex = exec(&f);
        ex.max_reruns = 2;
        let o = cargo().test(&ex, &TestScope::All).await;
        assert_eq!(f.calls().len(), 3, "one full run and two reruns");
        assert_eq!(o.failed_tests, Some(10));
    }

    #[tokio::test]
    async fn single_test_scope_never_reruns() {
        let f = FakeRunner::new().on(
            "cargo test",
            vec![CommandOutput::failed(101, big_failure())],
        );
        let o = cargo()
            .test(&exec(&f), &TestScope::Test("a::bad".into()))
            .await;
        assert!(!o.ok);
        assert_eq!(f.calls().len(), 1);
    }

    #[tokio::test]
    async fn rerun_test_runs_exactly_that_test() {
        let f = FakeRunner::new().on("cargo test", vec![CommandOutput::ok("ok")]);
        let o = cargo().rerun_test(&exec(&f), "tests::x").await;
        assert!(o.ok && o.failed_tests == Some(0));
        assert!(f.calls()[0].ends_with("tests::x -- --exact"));
    }

    #[tokio::test]
    async fn passing_tests_report_zero_failures() {
        let f = FakeRunner::new().on("cargo test", vec![CommandOutput::ok("test result: ok")]);
        let o = cargo().test(&exec(&f), &TestScope::All).await;
        assert!(o.ok);
        assert_eq!(o.failed_tests, Some(0));
    }

    #[tokio::test]
    async fn unscripted_command_fails_with_127() {
        let f = FakeRunner::new();
        let o = cargo().build(&exec(&f)).await;
        assert!(!o.ok);
        assert!(o.summary.contains("exit code 127"));
    }

    #[tokio::test]
    async fn fake_queue_pops_then_repeats_last() {
        let f = FakeRunner::new().on(
            "x",
            vec![
                CommandOutput::failed(1, "first"),
                CommandOutput::ok("second"),
            ],
        );
        let ex = exec(&f);
        let d = Duration::from_secs(1);
        assert_eq!(ex.run("x", Path::new("."), d).await.output, "first");
        assert_eq!(ex.run("x", Path::new("."), d).await.output, "second");
        assert_eq!(ex.run("x", Path::new("."), d).await.output, "second");
    }

    // --- verify state ---

    fn outcome(ok: bool, summary: &str, failed: Option<u32>) -> CheckOutcome {
        CheckOutcome {
            ok,
            summary: summary.into(),
            full_output: String::new(),
            failed_tests: failed,
            duration: Duration::ZERO,
            kind: if ok {
                OutcomeKind::Passed
            } else {
                OutcomeKind::Failed
            },
        }
    }

    #[test]
    fn verify_state_trailing_note() {
        let mut v = VerifyState::default();
        assert_eq!(v.trailing_note(), "");
        v.record_build(&outcome(false, "`cargo build` failed: E0308", None));
        let n = v.trailing_note();
        assert!(n.starts_with("LATEST COMPILE/TEST STATE"));
        assert!(n.contains("Last compilation: Failed. `cargo build` failed: E0308"));
        assert!(n.contains("Last test run: not run yet"));
        v.record_build(&outcome(true, "ok", None));
        v.record_tests(&outcome(true, "ok", Some(0)));
        let n = v.trailing_note();
        assert!(
            n.contains("Last compilation: Succeeded!") && n.contains("Last test run: Succeeded!")
        );
        assert!(!n.contains("Last lint"));
        v.record_lint(&outcome(false, "lint bad", None));
        assert!(v.trailing_note().contains("Last lint: Failed. lint bad"));
    }

    #[test]
    fn verify_state_counts_consecutive_failures() {
        let mut v = VerifyState::default();
        assert!(!v.progress_check_due(3));
        for _ in 0..2 {
            v.record_build(&outcome(false, "x", None));
        }
        assert!(!v.progress_check_due(3));
        v.record_build(&outcome(false, "x", None));
        assert!(v.progress_check_due(3));
        v.record_build(&outcome(false, "x", None));
        assert!(!v.progress_check_due(3));
        assert!(!v.progress_check_due(0));
        v.record_build(&outcome(true, "x", None));
        assert_eq!(v.consecutive_build_failures, 0);
        assert!(!v.build_is_red());
    }

    #[test]
    fn verify_state_ignores_skipped_and_cancelled() {
        let mut v = VerifyState::default();
        v.record_build(&CheckOutcome::skipped("none"));
        assert!(v.build.is_none());
        let mut c = outcome(false, "cancelled", None);
        c.kind = OutcomeKind::Cancelled;
        v.record_build(&c);
        assert!(v.build.is_none());
        assert_eq!(v.consecutive_build_failures, 0);
    }

    #[test]
    fn repair_priority_rule() {
        let mut v = VerifyState::default();
        v.record_build(&outcome(false, "x", None));
        assert!(v.needs_repair_priority(false));
        assert!(!v.needs_repair_priority(true), "the task started red");
        v.record_build(&outcome(true, "x", None));
        assert!(!v.needs_repair_priority(false));
    }

    // --- the real shell runner ---

    fn off() -> ShellRunner {
        ShellRunner::new(crate::sandbox::Policy {
            mode: crate::sandbox::Mode::Off,
            ..Default::default()
        })
    }

    fn spec(cmd: &str, secs: u64, cancel: CancellationToken) -> CommandSpec {
        CommandSpec {
            command: cmd.into(),
            cwd: std::env::temp_dir(),
            timeout: Duration::from_secs(secs),
            cancel,
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shell_runner_exit_codes_and_output() {
        let r = off();
        let o = r
            .run(&spec("echo hello", 10, CancellationToken::new()))
            .await;
        assert!(o.success(), "{o:?}");
        assert_eq!(o.output.trim(), "hello");
        let o = r
            .run(&spec("echo oops >&2; exit 3", 10, CancellationToken::new()))
            .await;
        assert_eq!(o.exit_code, Some(3));
        assert_eq!(o.output.trim(), "oops");
        // grep exiting 1 is "benign" to the bash tool but still a failure here.
        let o = r
            .run(&spec("echo a | grep zzz", 10, CancellationToken::new()))
            .await;
        assert_eq!(o.exit_code, Some(1));
        assert!(!o.success());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shell_runner_times_out() {
        let started = Instant::now();
        let r = off();
        let o = r.run(&spec("sleep 30", 1, CancellationToken::new())).await;
        assert!(o.timed_out && !o.success(), "{o:?}");
        assert!(started.elapsed() < Duration::from_secs(20));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shell_runner_cancels() {
        let cancel = CancellationToken::new();
        let c2 = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            c2.cancel();
        });
        let started = Instant::now();
        let o = off().run(&spec("sleep 30", 60, cancel)).await;
        assert!(o.cancelled && !o.success(), "{o:?}");
        assert!(started.elapsed() < Duration::from_secs(20));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn custom_runner_through_the_trait_object() {
        let ex = Exec::new(Arc::new(off()));
        let o = ex
            .run(
                "echo via-exec",
                &std::env::temp_dir(),
                Duration::from_secs(10),
            )
            .await;
        assert_eq!(o.output.trim(), "via-exec");
    }

    // --- real cargo on a tiny crate ---

    fn cargo_available() -> bool {
        std::process::Command::new("cargo")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn real_cargo_on_a_tiny_crate() {
        if !cargo_available() {
            eprintln!("cargo not available; skipping");
            return;
        }
        let good = "pub fn add(a: i32, b: i32) -> i32 { a + b }\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn adds() { assert_eq!(super::add(1, 2), 3); }\n}\n";
        let d = dir_with(&[
            ("Cargo.toml", "[package]\nname = \"tiny\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n"),
            ("src/lib.rs", good),
        ]);
        let root = d.path();
        let sys = detect_build_system(root).expect("cargo detected");
        assert_eq!(sys.name(), "cargo");
        let ex = Exec::new(Arc::new(off()))
            .with_timeouts(Duration::from_secs(240), Duration::from_secs(240));

        let b = sys.build(&ex).await;
        assert!(b.ok, "build: {}", b.summary);
        let t = sys.test(&ex, &TestScope::All).await;
        assert!(t.ok && t.failed_tests == Some(0), "tests: {}", t.summary);

        // A failing test is found by name and counted.
        let failing = good.replace("), 3)", "), 4)");
        std::fs::write(root.join("src/lib.rs"), failing).unwrap();
        let t = sys.test(&ex, &TestScope::All).await;
        assert!(!t.ok, "{}", t.summary);
        assert_eq!(t.failed_tests, Some(1), "{}", t.summary);
        assert!(t.summary.contains("1 failing test"), "{}", t.summary);
        let r = sys.rerun_test(&ex, "tests::adds").await;
        assert!(!r.ok && r.failed_tests == Some(1), "{}", r.summary);

        // A compile error is condensed to the error itself.
        std::fs::write(root.join("src/lib.rs"), "pub fn add() -> i32 { \"x\" }\n").unwrap();
        let b = sys.build(&ex).await;
        assert!(!b.ok);
        assert!(b.summary.contains("error[E0308]"), "{}", b.summary);
        assert!(b.summary.contains("src/lib.rs"), "{}", b.summary);
        assert!(!b.summary.contains("Compiling"), "{}", b.summary);
    }
}
