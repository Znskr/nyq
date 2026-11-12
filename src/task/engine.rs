//! The task engine's runtime types (design §4.1): what a task is
//! ([`TaskSpec`]), what one running turn of it has spent and seen
//! ([`TaskState`]), how it ended ([`TaskOutcome`]), and the texts the loop
//! shows the model (the trailing-note task section, nudges, interventions).
//!
//! A task ends when a validator accepts a typed result, not when the model
//! stops calling tools. A plain chat turn is the degenerate task: no schema,
//! the always-pass validator, nothing in the trailing note, so it behaves as
//! nyq always did. Everything here is pure; `crate::agent::task` wires it into
//! the loop.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{json, Map, Value};

use super::budget::{Budget, BudgetLimits, EscalationLadder, Exhaustion, Rung, StallReason};
use super::build::{shorten_old_errors, VerifyState};
use super::cycle::CycleDetector;
use super::memo::MemoKey;
use crate::types::ToolSpec;

/// The tool a typed task finishes with.
pub const SUBMIT: &str = "submit";

/// Recent failure texts kept for the progress check and diagnosis.
const KEEP_ERRORS: usize = 8;

/// Older verify errors in the trailing note are cut to this many characters.
const OLD_ERROR_CHARS: usize = 200;

// ---------------------------------------------------------------------------
// Spec
// ---------------------------------------------------------------------------

/// A validator, declared by name so a spec can be printed, hashed into a memo
/// key and built into a [`super::Validator`] when `submit` runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ValidatorSpec {
    AlwaysPass,
    Build,
    Tests,
    Lint,
    /// A shell command; exit code 0 passes.
    Cmd(String),
    /// A JSON pointer into the submitted value at a path or a list of paths
    /// that must exist.
    Exists(String),
    /// An LLM judge with this rubric checks the change is present and complete.
    Judge(String),
    /// The task must have changed these files.
    FilesTouched(Vec<PathBuf>),
}

impl ValidatorSpec {
    /// `build`, `tests`, `lint`, `cmd:<shell>`, `exists:<json-pointer>`,
    /// `judge:<rubric>` or `touched:<path>[,<path>…]`.
    pub fn parse(s: &str) -> Result<Self, String> {
        let s = s.trim();
        let (kind, rest) = match s.split_once(':') {
            Some((k, r)) => (k.trim(), Some(r.trim())),
            None => (s, None),
        };
        let some = |r: Option<&str>| r.filter(|r| !r.is_empty()).map(String::from);
        Ok(match (kind, rest) {
            ("build", None) => Self::Build,
            ("tests" | "test", None) => Self::Tests,
            ("lint", None) => Self::Lint,
            ("cmd", r) if some(r).is_some() => Self::Cmd(some(r).unwrap_or_default()),
            ("exists", Some(p)) => Self::Exists(if p.is_empty() || p.starts_with('/') {
                p.to_string()
            } else {
                format!("/{p}")
            }),
            ("judge", r) if some(r).is_some() => Self::Judge(some(r).unwrap_or_default()),
            ("touched", r) if some(r).is_some() => Self::FilesTouched(
                some(r)
                    .unwrap_or_default()
                    .split(',')
                    .map(str::trim)
                    .filter(|p| !p.is_empty())
                    .map(PathBuf::from)
                    .collect(),
            ),
            _ => {
                return Err(format!(
                    "unknown validator `{s}`: use build, tests, lint, cmd:<shell>, exists:<json-pointer>, judge:<rubric> or touched:<path,…>"
                ))
            }
        })
    }

    /// The text [`ValidatorSpec::parse`] reads back.
    pub fn label(&self) -> String {
        match self {
            Self::AlwaysPass => "always_pass".into(),
            Self::Build => "build".into(),
            Self::Tests => "tests".into(),
            Self::Lint => "lint".into(),
            Self::Cmd(c) => format!("cmd:{c}"),
            Self::Exists(p) => format!("exists:{p}"),
            Self::Judge(r) => format!("judge:{r}"),
            Self::FilesTouched(p) => format!(
                "touched:{}",
                p.iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(",")
            ),
        }
    }

    /// Build, tests or lint: checks a build system runs.
    pub fn is_check(&self) -> bool {
        matches!(self, Self::Build | Self::Tests | Self::Lint)
    }

    /// Whether the check runs a command in the workspace (and so should see
    /// it quiescent: the caller holds the workspace writer while it runs).
    pub fn executes(&self) -> bool {
        matches!(self, Self::Build | Self::Tests | Self::Lint | Self::Cmd(_))
    }

    /// The validator for every spec but the judge, which needs a model
    /// (the agent builds that one).
    pub fn validator(&self) -> Option<Box<dyn super::Validator>> {
        use super::validate::*;
        Some(match self {
            Self::AlwaysPass => Box::new(AlwaysPass),
            Self::Build => Box::new(BuildValidator),
            Self::Tests => Box::new(TestsValidator::new(super::TestScope::All)),
            Self::Lint => Box::new(LintValidator),
            Self::Cmd(c) => Box::new(CustomCommandValidator::new(c.clone())),
            Self::Exists(p) => Box::new(ExistsValidator::in_value(p.clone())),
            Self::FilesTouched(p) => Box::new(FilesTouchedValidator::new(p.clone())),
            Self::Judge(_) => return None,
        })
    }
}

/// How one check judged a submission (`{validator, verdict, feedback?}`),
/// kept for reports and typed subagent results.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CheckRecord {
    pub validator: String,
    /// `pass`, `fail` or `escalate`.
    pub verdict: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub feedback: Option<String>,
}

impl CheckRecord {
    pub fn of(validator: &str, verdict: &super::Verdict) -> Self {
        use super::Verdict;
        let (verdict, feedback) = match verdict {
            Verdict::Pass => ("pass", None),
            Verdict::Fail { feedback, .. } => ("fail", Some(feedback.clone())),
            Verdict::Escalate(why) => ("escalate", Some(why.clone())),
        };
        Self {
            validator: validator.into(),
            verdict,
            feedback,
        }
    }

    pub fn failed(validator: &str, feedback: &str) -> Self {
        Self {
            validator: validator.into(),
            verdict: "fail",
            feedback: Some(feedback.into()),
        }
    }
}

/// Model tiers (design §10): the classifier, progress check and syntax repair
/// use `Low`; judges and diagnosis use `High`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Tier {
    Low,
    #[default]
    Medium,
    High,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TierPolicy {
    /// The tier the task itself runs on (the session model is `Medium`).
    pub run: Tier,
    /// Whether the ladder may switch to the `tier.high` model.
    pub allow_bump: bool,
}

impl Default for TierPolicy {
    fn default() -> Self {
        Self {
            run: Tier::Medium,
            allow_bump: true,
        }
    }
}

/// Whether the background verifier runs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum VerifyMode {
    /// From settings, the build system and the validators.
    #[default]
    Auto,
    On,
    Off,
}

/// Memoisation: the result is cached under a key of the goal, the schema, the
/// validators and the contents of these files.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MemoSpec {
    pub files: Vec<PathBuf>,
}

/// What to do and when it counts as done.
#[derive(Clone, Debug, PartialEq)]
pub struct TaskSpec {
    pub goal: String,
    /// JSON Schema of the result. `None` is a plain chat turn: no `submit`.
    pub schema: Option<Value>,
    /// Run in order after the schema check when `submit` is called.
    pub validators: Vec<ValidatorSpec>,
    pub limits: BudgetLimits,
    pub tier: TierPolicy,
    pub memo: Option<MemoSpec>,
    pub verify: VerifyMode,
    /// Nobody to ask: an escalation ends the run (headless exit 2).
    pub headless: bool,
}

impl TaskSpec {
    /// A plain chat turn: no schema, the always-pass validator.
    pub fn chat() -> Self {
        Self {
            goal: String::new(),
            schema: None,
            validators: vec![ValidatorSpec::AlwaysPass],
            limits: BudgetLimits::default(),
            tier: TierPolicy::default(),
            memo: None,
            verify: VerifyMode::Auto,
            headless: false,
        }
    }

    /// A task that ends with a value matching `schema`.
    pub fn typed(goal: impl Into<String>, schema: Value) -> Self {
        Self {
            goal: goal.into(),
            schema: Some(schema),
            validators: Vec::new(),
            ..Self::chat()
        }
    }

    pub fn is_typed(&self) -> bool {
        self.schema.is_some()
    }

    pub fn has_check(&self) -> bool {
        self.validators.iter().any(ValidatorSpec::is_check)
    }

    pub fn has_judge(&self) -> bool {
        self.validators
            .iter()
            .any(|v| matches!(v, ValidatorSpec::Judge(_)))
    }

    pub fn wants_tests(&self) -> bool {
        self.validators.contains(&ValidatorSpec::Tests)
    }

    /// The validators as `--validate` names them (the always-pass one left out).
    pub fn labels(&self) -> Vec<String> {
        self.validators
            .iter()
            .filter(|v| **v != ValidatorSpec::AlwaysPass)
            .map(ValidatorSpec::label)
            .collect()
    }

    /// The memo key: goal, schema, validators and the listed files' contents.
    pub fn memo_key(&self, root: &Path) -> Option<MemoKey> {
        let memo = self.memo.as_ref()?;
        let inputs = json!({"schema": self.schema, "validators": self.labels()});
        Some(MemoKey::new(root, &self.goal, &inputs, &memo.files))
    }

    /// The `submit` tool: its parameters are the result schema (wrapped in
    /// `{value}` when the schema is not an object, since tool arguments must
    /// be one).
    pub fn submit_spec(&self) -> Option<ToolSpec> {
        let schema = self.schema.as_ref()?;
        let checks = self.labels();
        let checks = if checks.is_empty() {
            String::new()
        } else {
            format!(
                " When you call it these checks run in order: {}. If one fails, its feedback comes back as an error: fix the problem and call submit again.",
                checks.join(", ")
            )
        };
        Some(ToolSpec {
            name: SUBMIT.into(),
            description: format!(
                "Finish the task by submitting its result. The arguments ARE the result and must match this schema.{checks} The task does not end until a submission is accepted; call it once the work is complete."
            ),
            parameters: submit_parameters(schema),
        })
    }

    /// The result inside `submit`'s arguments.
    pub fn submitted_value(&self, args: Value) -> Result<Value, String> {
        let Some(schema) = &self.schema else {
            return Ok(args);
        };
        if !needs_wrapper(schema) {
            return Ok(args);
        }
        match args {
            Value::Object(mut m) if m.contains_key("value") => {
                Ok(m.remove("value").unwrap_or_default())
            }
            _ => Err("missing field `value` at `.`: pass the result as {\"value\": …}".into()),
        }
    }
}

fn needs_wrapper(schema: &Value) -> bool {
    match schema.get("type") {
        Some(Value::String(t)) => t != "object",
        Some(_) => true,
        None => schema.get("properties").is_none(),
    }
}

/// `submit`'s parameter schema for a result schema.
pub fn submit_parameters(schema: &Value) -> Value {
    let mut inner = schema.clone();
    if let Some(m) = inner.as_object_mut() {
        m.remove("$schema");
        m.remove("$id");
    }
    if !needs_wrapper(schema) {
        return inner;
    }
    // Definitions stay at the root so `#/$defs/…` references still resolve.
    let mut outer = Map::new();
    outer.insert("type".into(), "object".into());
    if let Some(m) = inner.as_object_mut() {
        for k in ["$defs", "definitions"] {
            if let Some(d) = m.remove(k) {
                outer.insert(k.into(), d);
            }
        }
    }
    outer.insert("properties".into(), json!({ "value": inner }));
    outer.insert("required".into(), json!(["value"]));
    outer.insert("additionalProperties".into(), false.into());
    Value::Object(outer)
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// One running turn of a task.
#[derive(Clone, Debug)]
pub struct TaskState {
    /// The spec's goal, or the turn's prompt for a chat turn.
    pub goal: String,
    pub budget: Budget,
    pub ladder: EscalationLadder,
    pub cycles: CycleDetector,
    /// Files this turn changed, relative to the workspace, oldest first.
    pub touched: Vec<PathBuf>,
    /// Whether the build was red when the turn started (None: not known yet).
    pub started_red: Option<bool>,
    /// Successful writes so far.
    pub writes: u64,
    /// Rejected submissions so far.
    pub failures: u32,
    /// Recent failure texts (validator feedback, red builds), newest last.
    pub errors: VecDeque<String>,
    /// The next stall to feed the ladder, with its nudge text.
    pub stall: Option<(StallReason, String)>,
    /// A validator gave up: go straight to the top of the ladder.
    pub escalation: Option<String>,
    /// "You must call submit" nudges sent.
    pub nudges: u32,
    /// Verifier runs already folded in.
    pub verify_seen: u64,
    /// The verifier's write count when the turn started: a run that started
    /// at this count saw the workspace as the turn found it.
    pub verify_base_writes: u64,
    /// The red-build streak already reported to the ladder.
    pub verify_streak_seen: u32,
    pub was_red: bool,
    /// File contents before this turn first wrote them (for the judge's diff).
    pub originals: HashMap<PathBuf, Option<String>>,
    /// Intents in the session's store already fed to the cycle detector
    /// (see `crate::tools::workset::Workset::intents_since`).
    pub intents_seen: u64,
    /// Mutating calls made without an intent while intents were offered:
    /// accepted, but counted (design §4.4).
    pub intents_missing: u32,
    /// The latest submission, parsed, and how each check judged it.
    pub last_submission: Option<Value>,
    pub checks: Vec<CheckRecord>,
}

impl TaskState {
    pub fn new(goal: impl Into<String>, limits: BudgetLimits, tier_available: bool) -> Self {
        Self {
            goal: goal.into(),
            budget: Budget::new(limits),
            ladder: EscalationLadder::new(tier_available),
            cycles: CycleDetector::default(),
            touched: Vec::new(),
            started_red: None,
            writes: 0,
            failures: 0,
            errors: VecDeque::new(),
            stall: None,
            escalation: None,
            nudges: 0,
            verify_seen: 0,
            verify_base_writes: 0,
            verify_streak_seen: 0,
            was_red: false,
            originals: HashMap::new(),
            intents_seen: 0,
            intents_missing: 0,
            last_submission: None,
            checks: Vec::new(),
        }
    }

    pub fn touch(&mut self, rel: PathBuf) {
        if !self.touched.contains(&rel) {
            self.touched.push(rel);
        }
    }

    pub fn note_error(&mut self, text: &str) {
        let text = text.trim();
        if text.is_empty() || self.errors.back().is_some_and(|e| e == text) {
            return;
        }
        self.errors.push_back(text.to_string());
        while self.errors.len() > KEEP_ERRORS {
            self.errors.pop_front();
        }
    }

    /// Queue a stall for the ladder. The first one in a round wins, so one
    /// round climbs at most one rung.
    pub fn stall(&mut self, reason: StallReason, note: impl Into<String>) {
        if self.stall.is_none() {
            self.stall = Some((reason, note.into()));
        }
    }

    pub fn report(&self, status: &str, reason: impl Into<String>) -> TaskReport {
        let b = &self.budget;
        let l = b.limits();
        let errors = shorten_old_errors(
            &self.errors.iter().cloned().collect::<Vec<_>>(),
            OLD_ERROR_CHARS,
        );
        TaskReport {
            status: status.into(),
            reason: reason.into(),
            goal: self.goal.clone(),
            rung: rung_name(self.ladder.rung()).into(),
            budget: BudgetReport {
                syntax_left: b.syntax_left,
                syntax: l.syntax,
                semantic_left: b.semantic_left,
                semantic: l.semantic,
                steps_left: b.steps_left,
                steps: l.steps,
                tokens_left: b.tokens_left,
                cost_left: b.cost_left,
                elapsed_secs: (b.elapsed_at(Instant::now()).as_secs_f64() * 10.0).round() / 10.0,
            },
            errors,
            touched: self
                .touched
                .iter()
                .map(|p| crate::tools::slash_path(p))
                .collect(),
            intents_missing: self.intents_missing,
        }
    }
}

pub fn rung_name(r: Rung) -> &'static str {
    match r {
        Rung::Idle => "none",
        Rung::Nudge => "nudge",
        Rung::ProgressCheck => "progress check",
        Rung::Diagnose => "diagnose",
        Rung::ContextReset => "context reset",
        Rung::TierBump => "tier bump",
        Rung::Escalate => "escalate",
    }
}

pub fn exhaustion_reason(e: Exhaustion) -> &'static str {
    match e {
        Exhaustion::Deadline => "wall-clock budget exhausted",
        Exhaustion::Cost => "cost budget exhausted",
        Exhaustion::Tokens => "token budget exhausted",
        Exhaustion::Steps => "step budget exhausted",
        Exhaustion::Semantic => "semantic budget exhausted: too many rejected submissions",
        Exhaustion::Syntax => "syntax budget exhausted: too many malformed or missing submissions",
    }
}

// ---------------------------------------------------------------------------
// Outcome and report
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct BudgetReport {
    pub syntax_left: u32,
    pub syntax: u32,
    pub semantic_left: u32,
    pub semantic: u32,
    pub steps_left: u32,
    pub steps: u32,
    pub tokens_left: Option<u64>,
    pub cost_left: Option<f64>,
    pub elapsed_secs: f64,
}

/// Why a task gave up: printed on stderr by headless runs that exit 2.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct TaskReport {
    /// `exhausted` or `escalated`.
    pub status: String,
    pub reason: String,
    pub goal: String,
    /// The ladder rung reached.
    pub rung: String,
    pub budget: BudgetReport,
    /// The latest failures, newest last; older ones shortened.
    pub errors: Vec<String>,
    pub touched: Vec<String>,
    /// Mutating calls made without the offered `intent`.
    pub intents_missing: u32,
}

impl TaskReport {
    /// A few lines for a person.
    pub fn summary(&self) -> String {
        let mut s = format!(
            "Task {}: {} (ladder: {}).",
            self.status, self.reason, self.rung
        );
        if let Some(e) = self.errors.last() {
            let line = e.lines().next().unwrap_or("");
            s.push_str(&format!("\nLast error: {line}"));
        }
        s
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum TaskOutcome {
    /// A validated result.
    Submitted(Value),
    /// A budget ran out.
    Exhausted(TaskReport),
    /// The ladder (or a validator) gave up.
    Escalated(TaskReport),
}

impl TaskOutcome {
    /// Headless exit status: 0 validated, 2 exhausted or escalated.
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Submitted(_) => 0,
            Self::Exhausted(_) | Self::Escalated(_) => 2,
        }
    }

    pub fn report(&self) -> Option<&TaskReport> {
        match self {
            Self::Submitted(_) => None,
            Self::Exhausted(r) | Self::Escalated(r) => Some(r),
        }
    }
}

// ---------------------------------------------------------------------------
// Texts
// ---------------------------------------------------------------------------

/// promptyped `getTask`'s repair framing: the build broke during this task.
pub fn repair_framing(goal: &str) -> String {
    format!(
        "Fix the build before continuing. Your original task: {goal}\n\
         Do not work on it until the build is green again; the failure is under LATEST COMPILE/TEST STATE below. \
         Fix it in small steps rather than one large change."
    )
}

fn budget_line(b: &Budget) -> String {
    let l = b.limits();
    let mut parts = vec![
        format!("{}/{} malformed submissions", b.syntax_left, l.syntax),
        format!("{}/{} rejected submissions", b.semantic_left, l.semantic),
        format!("{}/{} steps", b.steps_left, l.steps),
    ];
    if let (Some(left), Some(total)) = (b.tokens_left, l.tokens) {
        parts.push(format!("{left}/{total} tokens"));
    }
    if let (Some(left), Some(total)) = (b.cost_left, l.cost) {
        parts.push(format!("${left:.2}/${total:.2}"));
    }
    if let (Some(deadline), Some(wall)) = (b.deadline, l.wall) {
        let left = deadline.saturating_duration_since(Instant::now()).as_secs();
        parts.push(format!("{left}s/{}s", wall.as_secs()));
    }
    format!("Budget left: {}.", parts.join(", "))
}

/// The task section of the trailing note: the task (or the repair framing),
/// how to finish, and the budget. Empty for a plain chat turn whose build
/// is fine.
pub fn task_section(
    spec: Option<&TaskSpec>,
    state: &TaskState,
    verify: Option<&VerifyState>,
) -> String {
    let typed = spec.is_some_and(TaskSpec::is_typed);
    let repair =
        verify.is_some_and(|v| v.needs_repair_priority(state.started_red.unwrap_or(false)));
    let mut out = Vec::new();
    if repair && !state.goal.trim().is_empty() {
        out.push(repair_framing(state.goal.trim()));
    } else if typed {
        out.push(format!("YOUR CURRENT TASK: {}", state.goal.trim()));
    }
    if let Some(spec) = spec.filter(|s| s.is_typed()) {
        let checks = spec.labels();
        let mut finish = String::from(
            "Finish by calling `submit`; its arguments are the result and must match its schema.",
        );
        if !checks.is_empty() {
            finish.push_str(&format!(" Checks run on submit: {}.", checks.join(", ")));
        }
        out.push(finish);
        let mut line = budget_line(&state.budget);
        if state.ladder.rung() != Rung::Idle {
            line.push_str(&format!(" Escalation: {}.", rung_name(state.ladder.rung())));
        }
        out.push(line);
    }
    if out.is_empty() {
        return String::new();
    }
    format!("[Task]\n{}", out.join("\n"))
}

/// The verify section of the trailing note: the latest build and test state
/// (promptyped's LATEST COMPILE/TEST STATE), with older distinct failures
/// shortened. Empty until the verifier has a result.
pub fn verify_section(verify: Option<&VerifyState>, older_errors: &[String]) -> String {
    let Some(v) = verify else {
        return String::new();
    };
    let block = v.trailing_note();
    if block.is_empty() {
        return String::new();
    }
    let mut block = block.trim_end().to_string();
    // Only the newest error is in the block itself; earlier distinct ones
    // are listed cut down (promptyped `shortenOldErrorMessages`).
    let older: Vec<String> = older_errors
        .iter()
        .map(|e| {
            let one: String = e.split_whitespace().collect::<Vec<_>>().join(" ");
            if one.chars().count() > OLD_ERROR_CHARS {
                format!("{}…", one.chars().take(OLD_ERROR_CHARS).collect::<String>())
            } else {
                one
            }
        })
        .collect();
    if !older.is_empty() {
        block.push_str("\nEarlier failures (shortened, oldest first):");
        for e in older {
            block.push_str("\n- ");
            block.push_str(&e);
        }
    }
    format!("[Verify]\n{block}")
}

/// Both task-engine sections of the trailing note, task first.
pub fn task_note(
    spec: Option<&TaskSpec>,
    state: &TaskState,
    verify: Option<&VerifyState>,
    older_errors: &[String],
) -> String {
    [
        task_section(spec, state, verify),
        verify_section(verify, older_errors),
    ]
    .into_iter()
    .filter(|s| !s.is_empty())
    .collect::<Vec<_>>()
    .join("\n\n")
}

/// The task as the compaction summary must keep it: the goal, the result
/// schema `submit` takes, and the checks. Empty for a plain chat turn.
pub fn compaction_brief(spec: Option<&TaskSpec>, goal: &str) -> String {
    let Some(spec) = spec.filter(|s| s.is_typed()) else {
        return String::new();
    };
    let goal = if spec.goal.trim().is_empty() {
        goal
    } else {
        &spec.goal
    };
    let mut out = format!("Task goal: {}", goal.trim());
    if let Some(schema) = &spec.schema {
        out.push_str(&format!(
            "\nResult schema (the arguments of `submit`): {}",
            serde_json::to_string(schema).unwrap_or_default()
        ));
    }
    let checks = spec.labels();
    if !checks.is_empty() {
        out.push_str(&format!("\nChecks run on submit: {}", checks.join(", ")));
    }
    out
}

/// The model stopped without `submit` while a schema is active.
pub fn no_submit_nudge(spec: &TaskSpec) -> String {
    let wrapped = spec.schema.as_ref().is_some_and(needs_wrapper);
    format!(
        "[Task] You must call `submit` with the task's result to finish; a reply without it does not end the task. \
         If the work is not done yet, keep working with the tools first. The arguments of `submit` are the result{}, \
         and must match its schema.",
        if wrapped { " wrapped as {\"value\": …}" } else { "" }
    )
}

/// Ladder rung 1 for a stall that is not a cycle.
pub fn stall_note(reason: StallReason, state: &TaskState) -> String {
    match reason {
        StallReason::BudgetLow => format!(
            "[Task] Your budget is running low. {} Concentrate on getting a submission accepted.",
            budget_line(&state.budget)
        ),
        StallReason::ConsecutiveFailures | StallReason::Cycle => format!(
            "[Task] This has now failed {} times. Repeating the same fix will not change the outcome: re-read the latest error, \
             check your assumptions about the code, and try a different approach.",
            state.failures.max(state.verify_streak_seen).max(2)
        ),
    }
}

/// Ladder rung 3 found a way out (promptyped's SYSTEM INTERVENTION).
pub fn intervention(plan: &str, failures: u32) -> String {
    format!(
        "SYSTEM INTERVENTION: the task has failed {failures} times without progress. \
         Based on an analysis of the errors, a potential solution is:\n{}\nPrioritise implementing this approach.",
        plan.trim()
    )
}

/// Ladder rung 4: the history was summarised; start over from it.
pub fn reset_note(goal: &str) -> String {
    format!(
        "[Task] The harness reset the context because the task kept failing the same way: the conversation so far is \
         summarised above. Start again from that summary with a fresh approach. Your task: {}",
        goal.trim()
    )
}

/// Parse `--budget syntax=N,semantic=N,steps=N,tokens=N,cost=X,wall=S` on top
/// of the defaults. `wall` takes seconds, or a number with `s`, `m` or `h`.
pub fn parse_budget(s: &str, mut limits: BudgetLimits) -> Result<BudgetLimits, String> {
    for part in s.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let (k, v) = part
            .split_once('=')
            .ok_or_else(|| format!("budget: expected NAME=VALUE, got `{part}`"))?;
        let (k, v) = (k.trim(), v.trim());
        let int = |v: &str| {
            v.parse::<u64>()
                .map_err(|_| format!("budget: `{k}` needs a whole number, got `{v}`"))
        };
        let u32_of = |v: &str| {
            int(v).and_then(|n| u32::try_from(n).map_err(|_| format!("budget: `{k}` is too large")))
        };
        match k {
            "syntax" => limits.syntax = u32_of(v)?.max(1),
            "semantic" => limits.semantic = u32_of(v)?.max(1),
            "steps" => limits.steps = u32_of(v)?.max(1),
            "tokens" => limits.tokens = Some(int(v)?.max(1)),
            "cost" => {
                let c: f64 = v
                    .trim_start_matches('$')
                    .parse()
                    .map_err(|_| format!("budget: `cost` needs a number of dollars, got `{v}`"))?;
                if !(c.is_finite() && c > 0.0) {
                    return Err("budget: `cost` must be positive".into());
                }
                limits.cost = Some(c);
            }
            "wall" => limits.wall = Some(parse_duration(v)?),
            _ => {
                return Err(format!(
                    "budget: unknown `{k}` (use syntax, semantic, steps, tokens, cost, wall)"
                ))
            }
        }
    }
    Ok(limits)
}

fn parse_duration(v: &str) -> Result<Duration, String> {
    let (num, mult) = match v.chars().last() {
        Some('s') => (&v[..v.len() - 1], 1.0),
        Some('m') => (&v[..v.len() - 1], 60.0),
        Some('h') => (&v[..v.len() - 1], 3600.0),
        _ => (v, 1.0),
    };
    let secs: f64 = num
        .trim()
        .parse()
        .map_err(|_| format!("budget: `wall` needs seconds (or 90s, 5m, 1h), got `{v}`"))?;
    if !(secs.is_finite() && secs > 0.0) {
        return Err("budget: `wall` must be positive".into());
    }
    Ok(Duration::from_secs_f64(secs * mult))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::build::{CheckOutcome, OutcomeKind};

    fn outcome(ok: bool, summary: &str) -> CheckOutcome {
        CheckOutcome {
            ok,
            summary: summary.into(),
            full_output: String::new(),
            failed_tests: None,
            duration: Duration::ZERO,
            kind: if ok {
                OutcomeKind::Passed
            } else {
                OutcomeKind::Failed
            },
        }
    }

    #[test]
    fn validator_specs_round_trip() {
        for s in [
            "build",
            "tests",
            "lint",
            "cmd:make check && echo ok",
            "exists:/files",
            "judge:the README documents --schema",
            "touched:src/a.rs,src/b.rs",
        ] {
            assert_eq!(ValidatorSpec::parse(s).unwrap().label(), s);
        }
        assert_eq!(
            ValidatorSpec::parse("exists:files").unwrap(),
            ValidatorSpec::Exists("/files".into())
        );
        assert_eq!(ValidatorSpec::parse("test").unwrap(), ValidatorSpec::Tests);
        for bad in ["", "cmd:", "judge: ", "nope", "build:x"] {
            assert!(ValidatorSpec::parse(bad).is_err(), "{bad}");
        }
        assert!(ValidatorSpec::Lint.is_check() && !ValidatorSpec::Cmd("x".into()).is_check());
    }

    #[test]
    fn chat_is_untyped_and_always_passes() {
        let chat = TaskSpec::chat();
        assert!(!chat.is_typed());
        assert_eq!(chat.validators, vec![ValidatorSpec::AlwaysPass]);
        assert!(chat.submit_spec().is_none());
        let state = TaskState::new("hello", BudgetLimits::default(), false);
        assert_eq!(task_note(None, &state, None, &[]), "");
        assert_eq!(task_note(Some(&chat), &state, None, &[]), "");
    }

    #[test]
    fn object_schemas_are_submit_parameters_and_others_are_wrapped() {
        let obj =
            json!({"$schema": "x", "type": "object", "properties": {"a": {"type": "integer"}}});
        let spec = TaskSpec::typed("g", obj);
        let submit = spec.submit_spec().unwrap();
        assert_eq!(submit.name, SUBMIT);
        assert!(submit.parameters.get("$schema").is_none());
        assert_eq!(submit.parameters["properties"]["a"]["type"], "integer");
        assert_eq!(
            spec.submitted_value(json!({"a": 1})).unwrap(),
            json!({"a": 1})
        );

        let arr = json!({"type": "array", "items": {"$ref": "#/$defs/x"}, "$defs": {"x": {"type": "string"}}});
        let spec = TaskSpec::typed("g", arr);
        let p = spec.submit_spec().unwrap().parameters;
        assert_eq!(p["type"], "object");
        assert_eq!(p["required"], json!(["value"]));
        assert_eq!(p["$defs"]["x"]["type"], "string");
        assert!(p["properties"]["value"].get("$defs").is_none());
        assert_eq!(
            spec.submitted_value(json!({"value": ["a"]})).unwrap(),
            json!(["a"])
        );
        assert!(spec.submitted_value(json!({"other": 1})).is_err());
        assert!(no_submit_nudge(&spec).contains("{\"value\": …}"));
    }

    #[test]
    fn repair_priority_reframes_the_task() {
        let spec = TaskSpec::typed("add a --schema flag", json!({"type": "object"}));
        let mut state = TaskState::new("add a --schema flag", BudgetLimits::default(), false);
        let mut v = VerifyState::default();
        v.record_build(&outcome(true, "ok"));
        let green = task_note(Some(&spec), &state, Some(&v), &[]);
        assert!(
            green.starts_with("[Task]\nYOUR CURRENT TASK: add a --schema flag"),
            "{green}"
        );
        assert!(green.contains("Last compilation: Succeeded!"));
        assert!(green.contains(
            "Budget left: 3/3 malformed submissions, 5/5 rejected submissions, 100/100 steps."
        ));

        v.record_build(&outcome(false, "error[E0308]: mismatched types"));
        state.started_red = Some(false);
        let red = task_note(
            Some(&spec),
            &state,
            Some(&v),
            &["old error\nwith lines".into()],
        );
        assert!(
            red.starts_with(
                "[Task]\nFix the build before continuing. Your original task: add a --schema flag"
            ),
            "{red}"
        );
        assert!(!red.contains("YOUR CURRENT TASK"));
        assert!(red.contains("Last compilation: Failed. error[E0308]"));
        assert!(red.contains("Earlier failures (shortened, oldest first):\n- old error with lines"));

        // A task that started red works on the task, with the state shown.
        state.started_red = Some(true);
        let started_red = task_note(Some(&spec), &state, Some(&v), &[]);
        assert!(started_red.contains("YOUR CURRENT TASK"), "{started_red}");

        // A chat turn shows only the verify state, or the framing when red.
        state.started_red = Some(false);
        let chat = task_note(None, &state, Some(&v), &[]);
        assert!(chat.starts_with("[Task]\nFix the build before continuing."));
        assert!(!chat.contains("submit"));
    }

    #[test]
    fn task_and_verify_are_separate_sections_in_order() {
        let spec = TaskSpec::typed("g", json!({"type": "object"}));
        let state = TaskState::new("g", BudgetLimits::default(), false);
        let mut v = VerifyState::default();
        assert_eq!(verify_section(Some(&v), &[]), "", "nothing run yet");
        v.record_build(&outcome(true, "ok"));
        let task = task_section(Some(&spec), &state, Some(&v));
        let verify = verify_section(Some(&v), &[]);
        assert!(
            task.starts_with("[Task]\n") && !task.contains("LATEST COMPILE"),
            "{task}"
        );
        assert!(
            verify.starts_with("[Verify]\nLATEST COMPILE/TEST STATE"),
            "{verify}"
        );
        let both = task_note(Some(&spec), &state, Some(&v), &[]);
        assert_eq!(both, format!("{task}\n\n{verify}"));
        assert_eq!(both.matches("[Task]").count(), 1);
        // A green chat turn has the verify section only.
        assert_eq!(task_note(None, &state, Some(&v), &[]), verify);
    }

    #[test]
    fn the_compaction_brief_keeps_goal_schema_and_checks() {
        let mut spec = TaskSpec::typed(
            "list the files",
            json!({"type": "object", "required": ["files"]}),
        );
        spec.validators = vec![ValidatorSpec::Exists("/files".into())];
        let brief = compaction_brief(Some(&spec), "ignored");
        assert!(brief.contains("Task goal: list the files"), "{brief}");
        assert!(brief.contains(r#""required":["files"]"#), "{brief}");
        assert!(
            brief.contains("Checks run on submit: exists:/files"),
            "{brief}"
        );
        assert_eq!(compaction_brief(Some(&TaskSpec::chat()), "g"), "");
        assert_eq!(compaction_brief(None, "g"), "");
    }

    #[test]
    fn budgets_parse() {
        let l = parse_budget(
            "syntax=2, semantic=4,steps=30,tokens=5000,cost=$1.5,wall=2m",
            BudgetLimits::default(),
        )
        .unwrap();
        assert_eq!((l.syntax, l.semantic, l.steps), (2, 4, 30));
        assert_eq!(l.tokens, Some(5000));
        assert_eq!(l.cost, Some(1.5));
        assert_eq!(l.wall, Some(Duration::from_secs(120)));
        assert_eq!(
            parse_budget("wall=90", BudgetLimits::default())
                .unwrap()
                .wall,
            Some(Duration::from_secs(90))
        );
        assert_eq!(
            parse_budget("", BudgetLimits::default()).unwrap(),
            BudgetLimits::default()
        );
        for bad in [
            "syntax",
            "steps=x",
            "cost=-1",
            "wall=0",
            "nope=1",
            "syntax=99999999999",
        ] {
            assert!(parse_budget(bad, BudgetLimits::default()).is_err(), "{bad}");
        }
    }

    #[test]
    fn reports_serialise() {
        let mut s = TaskState::new("g", BudgetLimits::default(), false);
        s.budget.record_syntax_error();
        s.note_error("first");
        s.note_error("first");
        s.note_error("second");
        s.touch(PathBuf::from("src/a.rs"));
        s.touch(PathBuf::from("src/a.rs"));
        let r = s.report("exhausted", exhaustion_reason(Exhaustion::Syntax));
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["status"], "exhausted");
        assert_eq!(v["budget"]["syntax_left"], 2);
        assert_eq!(v["errors"], json!(["first", "second"]));
        assert_eq!(v["touched"], json!(["src/a.rs"]));
        assert!(r.summary().contains("Last error: second"));
        assert_eq!(TaskOutcome::Exhausted(r.clone()).exit_code(), 2);
        assert_eq!(TaskOutcome::Escalated(r).exit_code(), 2);
        assert_eq!(TaskOutcome::Submitted(json!(1)).exit_code(), 0);
    }
}
