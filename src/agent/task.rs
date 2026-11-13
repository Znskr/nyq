//! W9: the task engine in the loop (design §4).
//!
//! A task ends when a validator accepts a typed result, not when the model
//! stops calling tools. The step loop, dispatch and the trailing note reach
//! the task engine only through the methods here, each from one delimited
//! call site:
//!
//! | call site | method | what it does |
//! |---|---|---|
//! | `step.rs` `drive`, start | [`Agent::task_begin`] | new [`TaskState`]; start the verifier and its baseline |
//! | `step.rs`, before each request | [`Agent::task_before_request`] | wait briefly for a running verify, fold it in, climb the ladder, charge the step budget |
//! | `step.rs`, tool specs | [`Agent::task_specs`] | add `submit`; add the `intent` field |
//! | `note.rs`, trailing note | [`Agent::task_note_sections`] | task, budget, repair framing; verify state |
//! | `step.rs`, after a response | [`Agent::task_spent`] | token and cost budgets |
//! | `step.rs`, no tool calls | [`Agent::task_no_submit`] | "you must call submit" nudge, or end |
//! | `step.rs`, after a tool round | [`Agent::task_after_round`] | submitted / exhausted / ladder |
//! | `step.rs`, step limit | [`Agent::task_out_of_steps`] | an unfinished typed task is exhausted |
//! | `dispatch.rs` `run_calls` | [`Agent::task_owns`], [`Agent::task_submit`] | the `submit` tool |
//! | `dispatch.rs` `run_calls` | [`Agent::task_observe`] | touched files, verify scheduling, cycle detector |
//! | `dispatch.rs` `prepare` | [`Agent::task_prepare_call`] | strip and record `intent`; snapshot files for the judge |
//! | `compact.rs`, summary prompt | [`Agent::task_compaction_brief`] | goal, schema, verify state for the summary |
//!
//! Intents have one store, the session working set's (`crate::tools::workset`):
//! the `intent` field and `note_intent` both record there, it is persisted with
//! the session, the trailing note renders it, and the cycle detector reads
//! new entries from it after each call. Which tools are mutating, and which
//! paths a call writes, come from their declared effects (`Tool::effects`).
//!
//! With no task set ([`Agent::set_task`]) and verification off, which is the
//! default for interactive chat, every one of these is a no-op: a chat turn is
//! the task with no schema and the always-pass validator.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use super::{Agent, TurnEnd, Ui, ASK_USER};
use crate::task::engine::{self, TaskOutcome, TaskSpec, TaskState, Tier, VerifyMode, SUBMIT};
use crate::task::intent;
use crate::task::memo::{Evidence, MemoEvidence, Replay};
use crate::task::{
    llm, tier, AlwaysPass, BuildSystem, CheckRecord, CommandRunner, Exec, FailKind, JudgeRunner,
    JudgeValidator, LadderAction, LadderEvent, SchemaValidator, ShellRunner, StallReason,
    TaskOptions, ValidateCx, Validator, ValidatorSpec, Verdict, Verifier,
};
use crate::tools::workset::{Intent, IntentKind};
use crate::tools::{Concurrency, ToolOutput};
use crate::types::{Item, Usage};

/// Recent tool calls shown to the diagnosis.
const DIAGNOSE_ACTIONS: usize = 12;

/// The task engine's per-agent state.
#[derive(Default)]
pub struct TaskRuntime {
    /// The active task; None is a plain chat turn.
    spec: Option<TaskSpec>,
    /// The running turn's state; None when the turn is not managed.
    state: Option<TaskState>,
    outcome: Option<TaskOutcome>,
    /// The background verifier, once started; it outlives turns so the
    /// build state carries over.
    verifier: Option<Verifier>,
    /// The workspace's build system, detected once.
    build_system: Option<Option<Arc<dyn BuildSystem>>>,
    /// The options in effect for this turn (read at its start).
    turn_options: Option<TaskOptions>,
    /// Test seams: settings, the command runner and the judge.
    pub(crate) options: Option<TaskOptions>,
    pub(crate) runner: Option<Arc<dyn CommandRunner>>,
    pub(crate) judge: Option<Arc<dyn JudgeRunner>>,
    /// What the accepted result's `--memo` entry keeps from this run.
    memo_evidence: Option<MemoEvidence>,
}

/// How a run of a task's checks ended.
struct Checked {
    verdict: Verdict,
    /// The label of the check that did not pass.
    by: String,
    records: Vec<CheckRecord>,
    /// `(rubric, model)` of each judge check whose model is known.
    judges: Vec<(String, String)>,
}

/// What the loop does when a typed task's model stops without `submit`.
pub(super) enum NoSubmit {
    /// Send this and keep going.
    Nudge(Item),
    /// The budget ran out; the outcome is recorded.
    End,
}

impl Agent {
    // -----------------------------------------------------------------
    // Public surface
    // -----------------------------------------------------------------

    /// Make `spec` the task for the following turns (None: plain chat).
    pub fn set_task(&mut self, spec: Option<TaskSpec>) {
        self.task.spec = spec;
        self.task.state = None;
        self.task.outcome = None;
        self.task.memo_evidence = None;
    }

    pub fn task_spec(&self) -> Option<&TaskSpec> {
        self.task.spec.as_ref()
    }

    /// How the last turn's task ended: a validated value, or why it gave up.
    pub fn take_task_outcome(&mut self) -> Option<TaskOutcome> {
        self.task.outcome.take()
    }

    /// What the accepted result's `--memo` entry should keep (set when a
    /// task with `memo` accepts a submission).
    pub fn take_memo_evidence(&mut self) -> Option<MemoEvidence> {
        self.task.memo_evidence.take()
    }

    /// The last turn's state (budget, ladder, touched files). For typed
    /// subagents (W11) and tests.
    #[allow(dead_code)]
    pub fn task_state(&self) -> Option<&TaskState> {
        self.task.state.as_ref()
    }

    /// Stop the background verifier (cancelling a running build).
    pub fn stop_verifier(&mut self) {
        self.task.verifier = None;
    }

    // -----------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------

    fn task_options(&self) -> TaskOptions {
        self.task
            .options
            .clone()
            .or_else(|| self.task.turn_options.clone())
            .unwrap_or_else(TaskOptions::current)
    }

    fn task_typed(&self) -> bool {
        self.task.spec.as_ref().is_some_and(TaskSpec::is_typed)
    }

    /// A task with its own budget: typed, or a headless run. An untyped
    /// interactive spec (`TaskSpec::chat()`) behaves like no task at all.
    fn task_explicit(&self) -> bool {
        self.task
            .spec
            .as_ref()
            .is_some_and(|s| s.is_typed() || s.headless)
    }

    fn task_build_system(&mut self) -> Option<Arc<dyn BuildSystem>> {
        if self.task.build_system.is_none() {
            self.task.build_system =
                Some(crate::task::detect_build_system(&self.cwd).map(Arc::from));
        }
        self.task.build_system.clone().flatten()
    }

    fn task_runner(&self) -> Arc<dyn CommandRunner> {
        self.task
            .runner
            .clone()
            .unwrap_or_else(|| Arc::new(ShellRunner::new((*self.sandbox).clone())))
    }

    /// Whether the background verifier should run for this turn, starting it
    /// when it should.
    fn task_ensure_verifier(&mut self, opts: &TaskOptions) -> bool {
        let spec = self.task.spec.as_ref();
        // A subagent (it records usage against its parent) verifies only when
        // its own task asks; settings alone start the root's verifier.
        let root = self.usage_session_id.is_none();
        let wanted = match spec.map_or(VerifyMode::Auto, |s| s.verify) {
            VerifyMode::Off => false,
            VerifyMode::On => true,
            VerifyMode::Auto => {
                spec.is_some_and(TaskSpec::has_check)
                    || (root
                        && match opts.verify_auto {
                            Some(false) => opts.verify_required,
                            Some(true) => true,
                            None => opts.verify_required,
                        })
            }
        };
        if !wanted {
            self.task.verifier = None;
            return false;
        }
        let tests = opts.verify_tests || spec.is_some_and(TaskSpec::wants_tests);
        let Some(bs) = self.task_build_system() else {
            return false;
        };
        if self.task.verifier.is_none() {
            let exec = Exec::new(self.task_runner());
            self.task.verifier = Some(Verifier::start(bs, exec, tests, opts.verify_debounce));
        }
        true
    }

    fn intents_on(&self, opts: &TaskOptions) -> bool {
        opts.intents_required || opts.intents_enabled.unwrap_or(self.task_explicit())
    }

    /// Record helper-call usage in the session and against the budget.
    fn task_account(&mut self, usage: Option<Usage>, ui: &mut dyn Ui) {
        self.account_usage(usage, ui);
        self.task_spent(usage);
    }

    fn finish_exhausted(&mut self, why: &str, ui: &mut dyn Ui) -> TurnEnd {
        if let Some(st) = &self.task.state {
            let report = st.report("exhausted", why);
            ui.notice(&report.summary());
            self.task.outcome = Some(TaskOutcome::Exhausted(report));
        }
        self.task_turn_end();
        TurnEnd::Done
    }

    /// Extensions hear that the turn ended, as they do for a normal end.
    fn task_turn_end(&self) {
        if let Some(ext) = self.hooks.extensions() {
            let last = self.items.iter().rev().find_map(|i| match i {
                Item::Assistant { text } => Some(text.as_str()),
                _ => None,
            });
            ext.turn_end(last.unwrap_or(""));
        }
    }

    /// `path` relative to the workspace; None when it is empty or outside.
    fn workspace_relative(&self, path: &str) -> Option<PathBuf> {
        if path.is_empty() {
            return None;
        }
        let full = super::lexical(&self.cwd, path)?;
        let base = super::lexical(&self.cwd, "").unwrap_or_else(|| self.cwd.clone());
        full.strip_prefix(&base)
            .ok()
            .filter(|rel| !rel.as_os_str().is_empty())
            .map(Path::to_path_buf)
    }

    // -----------------------------------------------------------------
    // Step loop
    // -----------------------------------------------------------------

    /// The loop's step limit, raised to a task's own step budget.
    pub(super) fn task_max_steps(&self, limit: usize) -> usize {
        match &self.task.spec {
            Some(s) if self.task_explicit() => limit.max(s.limits.steps as usize),
            _ => limit,
        }
    }

    /// Start of a turn: a fresh [`TaskState`] when the turn is managed (a
    /// typed task, or the verifier is on).
    pub(super) fn task_begin(&mut self) {
        self.task.state = None;
        self.task.outcome = None;
        let opts = self
            .task
            .options
            .clone()
            .unwrap_or_else(TaskOptions::current);
        self.task.turn_options = Some(opts.clone());
        let verify = self.task_ensure_verifier(&opts);
        if !self.task_typed() && !verify {
            return;
        }
        let spec = self.task.spec.as_ref();
        let goal = spec
            .map(|s| s.goal.clone())
            .filter(|g| !g.trim().is_empty())
            .or_else(|| {
                self.items.iter().rev().find_map(|i| match i {
                    Item::User { text, .. } => Some(text.clone()),
                    _ => None,
                })
            })
            .unwrap_or_default();
        let limits = spec.map(|s| s.limits).unwrap_or_default();
        let tier_available = spec.is_none_or(|s| s.tier.allow_bump && s.tier.run != Tier::High)
            && tier::bump_available(self.provider.as_ref(), &opts);
        let mut st = TaskState::new(goal, limits, tier_available);
        // The cycle detector sees the intents stated from here on.
        st.intents_seen = crate::tools::workset::lock(&self.workset).intents_recorded();
        if let Some(v) = &self.task.verifier {
            let snap = v.snapshot();
            st.verify_seen = snap.runs;
            st.verify_base_writes = snap.writes;
            st.verify_streak_seen = snap.verify.consecutive_build_failures;
            st.was_red = snap.verify.build_is_red();
            if !snap.running && !snap.pending {
                if snap.runs > 0 {
                    st.started_red = Some(snap.verify.build_is_red());
                } else {
                    // Nothing known yet: a baseline build tells whether the
                    // task starts red.
                    v.run_now();
                }
            }
        }
        self.task.state = Some(st);
    }

    /// Before each request: wait briefly for a running verify, fold its
    /// result in, act on the ladder, and charge the step budget.
    pub(super) async fn task_before_request(
        &mut self,
        ui: &mut dyn Ui,
        cancel: &CancellationToken,
    ) -> Result<Option<TurnEnd>> {
        if self.task.state.is_none() {
            return Ok(None);
        }
        let opts = self.task_options();
        if let Some(v) = &self.task.verifier {
            v.settle(opts.verify_wait).await;
        }
        self.fold_verify(&opts);
        if let Some(end) = self.climb(ui, cancel).await? {
            return Ok(Some(end));
        }
        if self.task_explicit() {
            let Some(st) = &mut self.task.state else {
                return Ok(None);
            };
            if let Some(ex) = st.budget.exhausted() {
                return Ok(Some(
                    self.finish_exhausted(engine::exhaustion_reason(ex), ui),
                ));
            }
            st.budget.record_step();
        }
        Ok(None)
    }

    /// Fold new verifier results into the turn: the starting state, red
    /// streaks (which climb the ladder) and recoveries.
    fn fold_verify(&mut self, opts: &TaskOptions) {
        let (Some(v), Some(st)) = (&self.task.verifier, &mut self.task.state) else {
            return;
        };
        let snap = v.snapshot();
        if snap.runs == st.verify_seen {
            return;
        }
        st.verify_seen = snap.runs;
        let red = snap.verify.build_is_red();
        if st.started_red.is_none() {
            // Only a run that saw the workspace as the turn found it says
            // whether the task started red.
            st.started_red = Some(red && snap.ran_at_writes == st.verify_base_writes);
        }
        if red {
            if let Some(b) = &snap.verify.build {
                st.note_error(&format!("[build] {}", b.summary));
            }
            let streak = snap.verify.consecutive_build_failures;
            if snap.verify.progress_check_due(opts.verify_progress_every)
                && streak != st.verify_streak_seen
            {
                st.verify_streak_seen = streak;
                let note = engine::stall_note(StallReason::ConsecutiveFailures, st);
                st.stall(StallReason::ConsecutiveFailures, note);
            }
        } else if st.was_red {
            st.ladder.step(LadderEvent::Recovered);
            st.verify_streak_seen = 0;
        }
        if let Some(t) = snap.verify.tests.as_ref().filter(|t| !t.ok) {
            st.note_error(&format!("[tests] {}", t.summary));
        }
        st.was_red = red;
    }

    /// The tools to offer: `submit` for a typed task, and the `intent` field
    /// on mutating tools when intents are on.
    pub(super) fn task_specs(&self, specs: &mut Vec<crate::types::ToolSpec>) {
        if let Some(submit) = self.task.spec.as_ref().and_then(TaskSpec::submit_spec) {
            specs.retain(|s| s.name != SUBMIT);
            specs.push(submit);
        }
        let opts = self.task_options();
        if self.intents_on(&opts) {
            for s in specs.iter_mut() {
                if self.tools.get(&s.name).is_some_and(intent::is_mutating) {
                    intent::wrap(s, opts.intents_required);
                }
            }
        }
    }

    /// The task engine's two sections of the trailing note: the task (goal,
    /// how to finish, budget, or the repair framing) and the latest verify
    /// state. Either is empty when there is nothing to show.
    pub(super) fn task_note_sections(&self) -> (String, String) {
        let Some(st) = &self.task.state else {
            return (String::new(), String::new());
        };
        let snap = self.task.verifier.as_ref().map(Verifier::snapshot);
        let red = snap.as_ref().is_some_and(|s| {
            s.verify.build_is_red() || s.verify.tests.as_ref().is_some_and(|t| !t.ok)
        });
        let older = snap
            .as_ref()
            .filter(|_| red)
            .map(|s| s.older_failures.clone())
            .unwrap_or_default();
        let verify = snap.as_ref().map(|s| &s.verify);
        (
            engine::task_section(self.task.spec.as_ref(), st, verify),
            engine::verify_section(verify, &older),
        )
    }

    /// What a compaction summary must carry over from the task engine: the
    /// task's goal, result schema and checks, and the latest verify state.
    pub(super) fn task_compaction_brief(&self) -> String {
        let goal = self.task.state.as_ref().map_or("", |s| s.goal.as_str());
        let task = engine::compaction_brief(self.task.spec.as_ref(), goal);
        let verify = self
            .task
            .verifier
            .as_ref()
            .map(|v| engine::verify_section(Some(&v.snapshot().verify), &[]))
            .unwrap_or_default();
        [task, verify]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    /// Charge a response's tokens and cost to the task's budget.
    pub(super) fn task_spent(&mut self, usage: Option<Usage>) {
        if !self.task_explicit() {
            return;
        }
        let (Some(u), Some(st)) = (usage, &mut self.task.state) else {
            return;
        };
        st.budget.spend_tokens(u.input_tokens + u.output_tokens);
        if st.budget.cost_left.is_some() {
            if let Some(price) = crate::pricing::lookup(
                &crate::config::home(),
                self.provider.pricing_provider(),
                self.provider.model(),
            ) {
                st.budget.spend_cost(price.cost(u));
            }
        }
    }

    /// The model answered without tool calls. A typed task does not end that
    /// way: nudge it to call `submit` (a syntax charge), or end when the
    /// budget is gone. None: not a typed task, carry on as usual.
    pub(super) fn task_no_submit(&mut self, ui: &mut dyn Ui) -> Option<NoSubmit> {
        let spec = self.task.spec.as_ref().filter(|s| s.is_typed())?;
        let nudge = engine::no_submit_nudge(spec);
        let st = self.task.state.as_mut()?;
        st.budget.record_syntax_error();
        st.nudges += 1;
        st.note_error("[submit] The model stopped without calling submit.");
        if let Some(ex) = st.budget.exhausted() {
            self.finish_exhausted(engine::exhaustion_reason(ex), ui);
            return Some(NoSubmit::End);
        }
        Some(NoSubmit::Nudge(Item::user(nudge)))
    }

    /// After a tool round: the task may be finished (submitted, exhausted)
    /// or stuck (climb the ladder).
    pub(super) async fn task_after_round(
        &mut self,
        ui: &mut dyn Ui,
        cancel: &CancellationToken,
    ) -> Result<Option<TurnEnd>> {
        if self.task.outcome.is_some() {
            self.task_turn_end();
            return Ok(Some(TurnEnd::Done));
        }
        let Some(st) = &self.task.state else {
            return Ok(None);
        };
        if self.task_explicit() {
            if let Some(ex) = st.budget.exhausted() {
                return Ok(Some(
                    self.finish_exhausted(engine::exhaustion_reason(ex), ui),
                ));
            }
        }
        self.climb(ui, cancel).await
    }

    /// The loop's step limit ended a typed task that never submitted.
    pub(super) fn task_out_of_steps(&mut self, ui: &mut dyn Ui) {
        if self.task_typed() && self.task.outcome.is_none() {
            self.finish_exhausted("step limit reached without an accepted submission", ui);
        }
    }

    // -----------------------------------------------------------------
    // Dispatch
    // -----------------------------------------------------------------

    /// Whether a call is the task engine's (`submit` while a schema is
    /// active) rather than the tool registry's.
    pub(super) fn task_owns(&self, name: &str) -> bool {
        name == SUBMIT && self.task_typed()
    }

    /// Whether the task already has its result (later calls do not run).
    pub(super) fn task_done(&self) -> bool {
        matches!(self.task.outcome, Some(TaskOutcome::Submitted(_)))
    }

    /// Run `submit`: parse, check the schema, then the task's validators in
    /// order. A pass records the value; a failure charges the budget and
    /// comes back as an error result with the feedback.
    pub(super) async fn task_submit(
        &mut self,
        arguments: &str,
        ui: &mut dyn Ui,
        cancel: &CancellationToken,
    ) -> ToolOutput {
        ui.tool_start(SUBMIT, "result");
        let out = self.check_submission(arguments, ui, cancel).await;
        ui.tool_end(SUBMIT, &out);
        out
    }

    async fn check_submission(
        &mut self,
        arguments: &str,
        ui: &mut dyn Ui,
        cancel: &CancellationToken,
    ) -> ToolOutput {
        let Some(spec) = self.task.spec.clone() else {
            return ToolOutput::err("There is no task to submit.");
        };
        let Some(schema) = spec.schema.clone() else {
            return ToolOutput::err("This task has no result to submit.");
        };
        let args: Value = match serde_json::from_str(if arguments.trim().is_empty() {
            "{}"
        } else {
            arguments
        }) {
            Ok(v) => v,
            Err(e) => {
                let feedback = format!("The submit arguments are not valid JSON: {e}");
                self.record_checks(None, vec![CheckRecord::failed("json", &feedback)]);
                return self.reject(FailKind::Syntax, &feedback, "json");
            }
        };
        let value = match spec.submitted_value(args.clone()) {
            Ok(v) => v,
            Err(e) => {
                let feedback =
                    format!("The submitted value does not match the required schema:\n- {e}");
                self.record_checks(Some(args), vec![CheckRecord::failed("schema", &feedback)]);
                return self.reject(FailKind::Syntax, &feedback, "schema");
            }
        };
        let Some(Checked {
            verdict,
            by,
            records,
            judges,
        }) = self
            .run_checks(&spec, schema, &value, None, ui, cancel)
            .await
        else {
            return ToolOutput::err("Cancelled by user.");
        };
        self.record_checks(Some(value.clone()), records);
        match verdict {
            Verdict::Pass => {
                if let Some(st) = &self.task.state {
                    crate::trace::log("task", || {
                        format!("submit accepted after {} rejections", st.failures)
                    });
                }
                if spec.memo.is_some() {
                    let touched = self
                        .task
                        .state
                        .as_ref()
                        .map(|s| s.touched.clone())
                        .unwrap_or_default();
                    self.task.memo_evidence = Some(MemoEvidence {
                        touched: Evidence::capture(&self.cwd, &touched),
                        judges,
                    });
                }
                self.task.outcome = Some(TaskOutcome::Submitted(value));
                ToolOutput::ok("Accepted: the result passed every check. The task is complete.")
            }
            Verdict::Fail { kind, feedback } => self.reject(kind, &feedback, &by),
            Verdict::Escalate(why) => {
                if let Some(st) = &mut self.task.state {
                    st.note_error(&format!("[{by}] {why}"));
                    st.escalation = Some(format!("the `{by}` check could not decide: {why}"));
                }
                ToolOutput::err(format!(
                    "The `{by}` check could not decide ({why}). The harness is escalating the task."
                ))
            }
        }
    }

    /// Re-run `spec`'s checks (schema, validators, TaskValidate hooks) on a
    /// result cached by an earlier run, outside any task turn: a `--memo`
    /// hit is used only when they all pass again. Some(the judges that
    /// passed, with their models) when they do; None when one fails.
    ///
    /// With `replay`, the checks see the entry's evidence. `touched:` and
    /// the judge read the touched files: they get the recorded set only when
    /// every file still has its recorded hash, and otherwise the hit fails
    /// (a miss). A judge whose pass was recorded under the same key (rubric,
    /// value, fingerprint, model, touched hashes) is not asked again. So a
    /// replay passes only checks whose current inputs equal those of a run
    /// they passed, or that pass again now.
    pub async fn recheck(
        &mut self,
        spec: &TaskSpec,
        value: &Value,
        replay: Option<&Replay<'_>>,
        ui: &mut dyn Ui,
        cancel: &CancellationToken,
    ) -> Option<Vec<(String, String)>> {
        let schema = spec.schema.clone()?;
        let reads_touched = spec
            .validators
            .iter()
            .any(|v| matches!(v, ValidatorSpec::FilesTouched(_) | ValidatorSpec::Judge(_)));
        let replay = match replay {
            Some(r) if reads_touched => Some((r.evidence.intact(&self.cwd)?, r)),
            Some(r) => Some((Vec::new(), r)),
            None => None,
        };
        let replay = replay.as_ref().map(|(t, r)| (t.as_slice(), *r));
        match self
            .run_checks(spec, schema, value, replay, ui, cancel)
            .await?
        {
            Checked {
                verdict: Verdict::Pass,
                judges,
                ..
            } => Some(judges),
            _ => None,
        }
    }

    /// The schema check, then `spec`'s validators in order, then the
    /// TaskValidate hooks, stopping at the first that does not pass: its
    /// verdict, its label and every check's record. None when cancelled.
    /// `replay` (a `--memo` re-check) gives the touched files and the
    /// recorded judge passes, in place of the turn's.
    async fn run_checks(
        &mut self,
        spec: &TaskSpec,
        schema: Value,
        value: &Value,
        replay: Option<(&[PathBuf], &Replay<'_>)>,
        ui: &mut dyn Ui,
        cancel: &CancellationToken,
    ) -> Option<Checked> {
        let mut cx = self.validate_cx(cancel);
        if let Some((touched, _)) = replay {
            cx.touched = touched.to_vec();
        }
        let mut judges: Vec<Arc<llm::LlmJudge>> = Vec::new();
        let mut judge_models = Vec::new();
        let mut checks: Vec<(String, Box<dyn Validator>)> =
            vec![("schema".into(), Box::new(SchemaValidator::new(schema)))];
        for v in &spec.validators {
            let check = match v {
                ValidatorSpec::Judge(rubric) => {
                    let runner = self.judge_runner(&spec.goal, &mut judges, cancel);
                    let model = runner.model_id();
                    if let Some(model) = &model {
                        judge_models.push((rubric.clone(), model.clone()));
                    }
                    let cached = replay.zip(model.as_deref()).is_some_and(|((_, r), m)| {
                        r.evidence.judge_passed(rubric, m, value, r.fingerprint)
                    });
                    if cached {
                        crate::trace::log("task", || {
                            format!("memo: reusing the judge's pass for `{rubric}`")
                        });
                        Box::new(AlwaysPass) as Box<dyn Validator>
                    } else {
                        Box::new(JudgeValidator::new(rubric.clone(), runner))
                    }
                }
                _ => self.validator(v),
            };
            checks.push((v.label(), check));
        }
        // W12: the TaskValidate hooks are one more check, after the
        // built-in ones.
        if self.hooks.has_task_validate() {
            checks.push((
                "hook".into(),
                Box::new(HookValidator {
                    hooks: self.hooks.clone(),
                    cwd: self.cwd.clone(),
                    session: self
                        .session
                        .as_ref()
                        .map(|s| s.id.clone())
                        .or_else(|| self.usage_session_id.clone()),
                    cancel: cancel.clone(),
                }),
            ));
        }
        // Checks that run commands see a quiescent workspace: no other
        // writer (a parallel child, say) changes it under them.
        let _lease = if spec.validators.iter().any(ValidatorSpec::executes) {
            let writer = crate::orchestration::writer(&self.cwd);
            tokio::select! {
                lock = writer.lock_unknown() => Some(lock),
                _ = cancel.cancelled() => return None,
            }
        } else {
            None
        };
        let mut verdict = Verdict::Pass;
        let mut by = String::new();
        let mut records = Vec::new();
        for (label, check) in &checks {
            if cancel.is_cancelled() {
                return None;
            }
            let v = check.check(value, &cx).await;
            records.push(CheckRecord::of(label, &v));
            if !v.is_pass() {
                verdict = v;
                by = label.clone();
                break;
            }
        }
        drop(_lease);
        for j in judges {
            let used: Vec<Usage> =
                std::mem::take(&mut *j.usage.lock().unwrap_or_else(|e| e.into_inner()));
            for u in used {
                self.task_account(Some(u), ui);
            }
        }
        Some(Checked {
            verdict,
            by,
            records,
            judges: judge_models,
        })
    }

    /// Keep the latest submission and its check verdicts on the turn state.
    fn record_checks(&mut self, value: Option<Value>, checks: Vec<CheckRecord>) {
        if let Some(st) = &mut self.task.state {
            st.last_submission = value;
            st.checks = checks;
        }
    }

    /// A rejected submission: charge the budget, maybe queue a stall, and
    /// tell the model what to fix.
    fn reject(&mut self, kind: FailKind, feedback: &str, by: &str) -> ToolOutput {
        let Some(st) = &mut self.task.state else {
            return ToolOutput::err(feedback.to_string());
        };
        match kind {
            FailKind::Syntax => st.budget.record_syntax_error(),
            FailKind::Semantic => st.budget.record_semantic_error(),
        }
        st.failures += 1;
        st.note_error(&format!("[{by}] {feedback}"));
        if st.budget.exhausted().is_none() {
            if st.budget.is_low() {
                let note = engine::stall_note(StallReason::BudgetLow, st);
                st.stall(StallReason::BudgetLow, note);
            } else if st.failures >= 2 {
                let note = engine::stall_note(StallReason::ConsecutiveFailures, st);
                st.stall(StallReason::ConsecutiveFailures, note);
            }
        }
        let kind = match kind {
            FailKind::Syntax => "malformed",
            FailKind::Semantic => "rejected",
        };
        let b = &st.budget;
        ToolOutput::err(format!(
            "{feedback}\n\n[submit {kind} by the `{by}` check. Left: {} malformed and {} rejected submissions. \
             Fix the problem and call submit again.]",
            b.syntax_left, b.semantic_left
        ))
    }

    fn validate_cx(&mut self, cancel: &CancellationToken) -> ValidateCx {
        let exec = Exec::new(self.task_runner()).with_cancel(cancel.clone());
        let touched = self
            .task
            .state
            .as_ref()
            .map(|s| s.touched.clone())
            .unwrap_or_default();
        let mut cx = ValidateCx::new(&self.cwd, exec).with_touched(touched);
        if let Some(bs) = self.task_build_system() {
            cx = cx.with_build_system(bs);
        }
        cx
    }

    /// The validator for every spec but the judge ([`Agent::judge_runner`]).
    fn validator(&self, v: &ValidatorSpec) -> Box<dyn Validator> {
        v.validator().unwrap_or_else(|| Box::new(AlwaysPass))
    }

    /// The judge: the injected one, else an LLM judge on the high tier (kept
    /// in `judges` so its usage can be accounted). `goal` is the task's, for
    /// a re-check outside any turn.
    fn judge_runner(
        &self,
        goal: &str,
        judges: &mut Vec<Arc<llm::LlmJudge>>,
        cancel: &CancellationToken,
    ) -> Arc<dyn JudgeRunner> {
        match &self.task.judge {
            Some(j) => j.clone(),
            None => {
                let opts = self.task_options();
                let (provider, _) = tier::provider_for(self.provider.as_ref(), Tier::High, &opts);
                let st = self.task.state.as_ref();
                let judge = Arc::new(llm::LlmJudge::new(
                    provider,
                    st.map_or_else(|| goal.to_string(), |s| s.goal.clone()),
                    st.map(|s| s.originals.clone()).unwrap_or_default(),
                    cancel.clone(),
                ));
                judges.push(judge.clone());
                judge
            }
        }
    }

    /// Whether `intent` in a call to `name` is the task engine's field: the
    /// tool is mutating by its declared effects and has no argument of that
    /// name itself.
    fn intent_field_of(&self, name: &str) -> bool {
        self.tools.get(name).is_some_and(intent::is_mutating)
            && !self.tools.spec_of(name).is_some_and(intent::declares_field)
    }

    /// The files a call writes, by its declared effects, as the task tracks
    /// them (see [`Agent::tracked`]).
    fn written_files(&self, name: &str, args: &Value) -> Vec<PathBuf> {
        let Some(tool) = self.tools.get(name) else {
            return Vec::new();
        };
        tool.effects(args)
            .writes
            .paths()
            .iter()
            .filter_map(|p| self.tracked(p))
            .collect()
    }

    /// A written path as the task tracks it: relative to the workspace, or
    /// absolute under another writable root (`--add-dir`); None elsewhere
    /// (nyq's own stores, such as the memory file).
    fn tracked(&self, path: &Path) -> Option<PathBuf> {
        let text = path.to_string_lossy();
        if let Some(rel) = self.workspace_relative(&text) {
            return Some(rel);
        }
        // The same file through another spelling of the workspace (git
        // reports a symlinked checkout by its real path, as a merge does).
        if path.is_absolute() {
            let real = |p: &Path| -> Option<PathBuf> {
                std::fs::canonicalize(p).ok().or_else(|| {
                    Some(
                        std::fs::canonicalize(p.parent()?)
                            .ok()?
                            .join(p.file_name()?),
                    )
                })
            };
            if let (Some(full), Ok(base)) = (real(path), std::fs::canonicalize(&self.cwd)) {
                if let Ok(rel) = full.strip_prefix(&base) {
                    if !rel.as_os_str().is_empty() {
                        return Some(rel.to_path_buf());
                    }
                }
            }
        }
        let full = super::lexical(&self.cwd, &text)?;
        self.sandbox
            .writable
            .iter()
            .any(|root| full.starts_with(root))
            .then_some(full)
    }

    /// Before a call is approved and run: take the `intent` field off a
    /// mutating call and record it in the session's intent store; reject the
    /// call when intents are required and it has none. Snapshot a file the
    /// judge will diff.
    pub(super) fn task_prepare_call(&mut self, name: &str, args: &mut Value) -> Option<ToolOutput> {
        if !self.intent_field_of(name) {
            return None;
        }
        let opts = self.task_options();
        match intent::take(args) {
            Some(Some(i)) => {
                crate::tools::workset::lock(&self.workset).record(Intent {
                    tool: name.to_string(),
                    ..i
                });
            }
            _ if self.intents_on(&opts) => {
                if opts.intents_required {
                    if let Some(st) = &mut self.task.state {
                        if self.task.spec.is_some() {
                            st.budget.record_syntax_error();
                        }
                        st.note_error(&format!("[intent] `{name}` was called without an intent."));
                    }
                    return Some(ToolOutput::err(format!(
                        "`{name}` changes the workspace, so it must include `intent`: {{\"did\": …, \"why\": …, \"next\": …}}. \
                         It did not run: make the call again with an intent."
                    )));
                }
                if let Some(st) = &mut self.task.state {
                    st.intents_missing += 1;
                }
            }
            _ => {}
        }
        let judge = self.task.spec.as_ref().is_some_and(TaskSpec::has_judge);
        if judge && self.task.state.is_some() {
            for rel in self.written_files(name, args) {
                let path = self.cwd.join(&rel);
                if let Some(st) = &mut self.task.state {
                    st.originals
                        .entry(rel)
                        .or_insert_with(|| std::fs::read_to_string(path).ok());
                }
            }
        }
        None
    }

    /// After a call ran (a model call, or a script's sub-call): feed the
    /// cycle detector (the intents stated since the last call, then the call
    /// itself), and on a write track the files and schedule the verifier.
    /// `ran` is the call's final footprint from dispatch (its schedule class
    /// and normalized write paths, from its declared effects after hooks);
    /// None when it did not run through a wave.
    pub(super) fn task_observe(
        &mut self,
        name: &str,
        arguments: &str,
        out: &ToolOutput,
        ran: Option<(&Concurrency, &[PathBuf])>,
    ) {
        if self.task.state.is_none() {
            return;
        }
        let mut args: Value = serde_json::from_str(arguments).unwrap_or_default();
        if self.intent_field_of(name) {
            intent::take(&mut args);
        }
        let files: Vec<PathBuf> = ran
            .map(|(_, writes)| writes.iter().filter_map(|p| self.tracked(p)).collect())
            .unwrap_or_default();
        let coordination = self
            .tools
            .get(name)
            .is_some_and(|t| t.kind() == crate::tools::ToolKind::Coordination);
        // Whether the call changed the workspace: a declared write to a
        // tracked file, a process (not a read-only command), an undeclared
        // tool, or a tool that reports a change itself (a shared subagent, a
        // worktree merge). A script's sub-calls are observed one by one, so
        // the script call itself is only a step for the cycle detector.
        let done = |ok: bool| ok && (out.changed || !out.is_error);
        let wrote = crate::scripting::kind_of(name).is_none()
            && match ran.map(|(class, _)| class) {
                Some(Concurrency::WriteFs(_)) => done(!files.is_empty()),
                Some(Concurrency::Exec) => done(true),
                Some(Concurrency::Barrier) => out.changed || (!out.is_error && !coordination),
                Some(Concurrency::Pure | Concurrency::ReadFs) | None => out.changed,
            };
        let intents: Vec<String> = {
            let ws = crate::tools::workset::lock(&self.workset);
            let seen = self.task.state.as_ref().map_or(0, |s| s.intents_seen);
            let new = ws.intents_since(seen).iter().map(Intent::line).collect();
            if let Some(st) = &mut self.task.state {
                st.intents_seen = ws.intents_recorded();
            }
            new
        };
        let Some(st) = &mut self.task.state else {
            return;
        };
        for line in intents {
            if let Some(c) = st.cycles.push_intent(&line) {
                st.stall(StallReason::Cycle, c.note());
            }
        }
        let step = crate::task::Step::new(name, &args, &out.content, out.is_error);
        if let Some(c) = st.cycles.push_step(step) {
            st.stall(StallReason::Cycle, c.note());
        }
        if !wrote {
            return;
        }
        st.writes += 1;
        // A failed call that still changed its files touched them too.
        for rel in files {
            st.touch(rel);
        }
        if let Some(v) = &self.task.verifier {
            v.schedule();
        }
    }

    // -----------------------------------------------------------------
    // The escalation ladder
    // -----------------------------------------------------------------

    /// Feed a queued stall (or a validator's escalation) to the ladder and
    /// carry out what it says. Some: the turn ends.
    async fn climb(
        &mut self,
        ui: &mut dyn Ui,
        cancel: &CancellationToken,
    ) -> Result<Option<TurnEnd>> {
        let Some(st) = &mut self.task.state else {
            return Ok(None);
        };
        if let Some(why) = st.escalation.take() {
            return self.escalate(&why, ui, cancel).await;
        }
        let Some((reason, note)) = st.stall.take() else {
            return Ok(None);
        };
        let mut action = st.ladder.step(LadderEvent::Stalled(reason));
        crate::trace::log("task", || format!("stalled ({reason:?}) → {action:?}"));
        loop {
            if cancel.is_cancelled() {
                return Ok(Some(TurnEnd::Cancelled));
            }
            let next = match action {
                LadderAction::Continue => return Ok(None),
                LadderAction::Nudge => {
                    self.push_counted(Item::user(note));
                    return Ok(None);
                }
                LadderAction::RunProgressCheck => {
                    let made = self.progress_check(ui, cancel).await;
                    LadderEvent::ProgressChecked { made }
                }
                LadderAction::RunDiagnose => {
                    let has_solution = self.diagnose(ui, cancel).await;
                    LadderEvent::Diagnosed { has_solution }
                }
                LadderAction::ResetContext => {
                    self.reset_task_context(ui, cancel).await;
                    return Ok(None);
                }
                LadderAction::BumpTier => {
                    self.bump_tier(ui);
                    return Ok(None);
                }
                LadderAction::Escalate => {
                    let why = match reason {
                        StallReason::Cycle => "it keeps repeating itself",
                        StallReason::BudgetLow => "its budget is nearly gone",
                        StallReason::ConsecutiveFailures => "it keeps failing without progress",
                    };
                    return self.escalate(why, ui, cancel).await;
                }
            };
            let Some(st) = &mut self.task.state else {
                return Ok(None);
            };
            action = st.ladder.step(next);
            crate::trace::log("task", || format!("{next:?} → {action:?}"));
        }
    }

    /// Rung 2: a cheap low-tier call over the recent errors. An error counts
    /// as progress, so a broken helper never makes the ladder climb.
    async fn progress_check(&mut self, ui: &mut dyn Ui, cancel: &CancellationToken) -> bool {
        let Some(st) = &self.task.state else {
            return true;
        };
        let goal = st.goal.clone();
        let errors: Vec<String> = st.errors.iter().cloned().collect();
        let opts = self.task_options();
        let (provider, warning) = tier::provider_for(self.provider.as_ref(), Tier::Low, &opts);
        if let Some(w) = warning {
            ui.notice(&w);
        }
        ui.notice("The task keeps failing; checking whether it is making progress…");
        match llm::progress_check(provider.as_ref(), &goal, &errors, cancel).await {
            Ok((p, usage)) => {
                self.task_account(usage, ui);
                ui.notice(&format!(
                    "Progress check: {}{}",
                    if p.made { "progress" } else { "no progress" },
                    if p.why.is_empty() {
                        String::new()
                    } else {
                        format!(" ({})", p.why)
                    }
                ));
                p.made
            }
            Err(e) => {
                ui.notice(&format!("The progress check failed ({e}); carrying on."));
                true
            }
        }
    }

    /// Rung 3: a high-tier, read-only look at the evidence. A plan goes in
    /// as a SYSTEM INTERVENTION and an approach-correction intent.
    async fn diagnose(&mut self, ui: &mut dyn Ui, cancel: &CancellationToken) -> bool {
        let Some(st) = &self.task.state else {
            return false;
        };
        let goal = st.goal.clone();
        let failures = st.failures.max(st.verify_streak_seen);
        let errors: Vec<String> = st.errors.iter().cloned().collect();
        let files: Vec<(String, String)> = st
            .touched
            .iter()
            .filter_map(|p| {
                let text = std::fs::read_to_string(self.cwd.join(p)).ok()?;
                Some((crate::tools::slash_path(p), text))
            })
            .collect();
        let actions = recent_actions(&self.items, DIAGNOSE_ACTIONS);
        let opts = self.task_options();
        let (provider, warning) = tier::provider_for(self.provider.as_ref(), Tier::High, &opts);
        if let Some(w) = warning {
            ui.notice(&w);
        }
        ui.notice("No progress: asking for a diagnosis…");
        let ev = llm::Evidence {
            goal: &goal,
            errors: &errors,
            actions: &actions,
            files: &files,
        };
        match llm::diagnose(provider.as_ref(), &ev, cancel).await {
            Ok((d, usage)) => {
                self.task_account(usage, ui);
                if d.has_solution && !d.plan.is_empty() {
                    ui.notice("The diagnosis found a way forward.");
                    self.push_counted(Item::user(engine::intervention(&d.plan, failures)));
                    crate::tools::workset::lock(&self.workset).record(Intent {
                        tool: "diagnose".into(),
                        kind: IntentKind::ApproachCorrection,
                        ..Intent::new(&d.plan, "approach correction after repeated failures", "")
                    });
                    true
                } else {
                    ui.notice("The diagnosis found no way forward.");
                    false
                }
            }
            Err(e) => {
                ui.notice(&format!("The diagnosis failed ({e})."));
                false
            }
        }
    }

    /// Rung 4: summarise into a handoff note through the compaction path and
    /// start again from it. The working set lives outside the transcript.
    async fn reset_task_context(&mut self, ui: &mut dyn Ui, cancel: &CancellationToken) {
        ui.notice(
            "The task keeps failing the same way; resetting the context with a handoff summary.",
        );
        match self.compact(ui, cancel).await {
            Ok(true) => {
                if let Some(st) = &mut self.task.state {
                    st.cycles.clear();
                    let note = engine::reset_note(&st.goal);
                    self.push_counted(Item::user(note));
                }
            }
            Ok(false) => {}
            Err(e) => ui.notice(&format!("The context reset failed: {e:#}")),
        }
    }

    /// Rung 5: switch to the `tier.high` model for the rest of the turn.
    fn bump_tier(&mut self, ui: &mut dyn Ui) {
        let opts = self.task_options();
        match tier::bumped(self.provider.as_ref(), &opts) {
            Ok(p) => {
                ui.notice(&format!(
                    "Switching to the high tier ({}/{}) for this task.",
                    p.name(),
                    p.model()
                ));
                let window = crate::config::context_window_for(p.name(), p.context_window());
                self.provider = p;
                self.set_context_window(window);
            }
            Err(e) => ui.notice(&format!("Could not switch to the high tier: {e:#}")),
        }
        if let Some(st) = &mut self.task.state {
            st.ladder.set_tier_available(false);
        }
    }

    /// Rung 6: give up. Interactive runs ask the user when they can; otherwise
    /// the turn ends with a report (headless: exit 2).
    async fn escalate(
        &mut self,
        why: &str,
        ui: &mut dyn Ui,
        cancel: &CancellationToken,
    ) -> Result<Option<TurnEnd>> {
        let headless = self.task.spec.as_ref().is_some_and(|s| s.headless);
        if !headless && self.tools.get(ASK_USER).is_some() {
            let question = format!("The task seems stuck: {why}. How should it continue?");
            let args = json!({"questions": [{
                "question": question,
                "options": [
                    {"label": "Keep trying", "description": "Reset the escalation and let it try again."},
                    {"label": "Stop", "description": "End the task here with a report."}
                ]
            }]});
            self.attention("question", &question);
            let answer = super::ask::ask_user(&args, ui, cancel).await;
            if cancel.is_cancelled() {
                return Ok(Some(TurnEnd::Cancelled));
            }
            let text = answer
                .content
                .strip_prefix("The user answered: ")
                .map(str::trim)
                .unwrap_or("");
            if !answer.is_error && !text.is_empty() && text != "Stop" {
                if let Some(st) = &mut self.task.state {
                    st.ladder.step(LadderEvent::Recovered);
                }
                if text != "Keep trying" {
                    self.push_counted(Item::user(format!(
                        "[The user's guidance for the stuck task]\n{text}"
                    )));
                }
                return Ok(None);
            }
        }
        if let Some(st) = &self.task.state {
            let report = st.report("escalated", why);
            ui.notice(&report.summary());
            self.task.outcome = Some(TaskOutcome::Escalated(report));
        }
        self.task_turn_end();
        Ok(Some(TurnEnd::Done))
    }
}

/// The last `n` tool calls with the first line of their results, oldest
/// first, for the diagnosis.
fn recent_actions(items: &[Item], n: usize) -> String {
    let mut lines = Vec::new();
    for (i, item) in items.iter().enumerate().rev() {
        if lines.len() == n {
            break;
        }
        let Item::ToolCall {
            id,
            name,
            arguments,
        } = item
        else {
            continue;
        };
        let args: String = arguments.chars().take(200).collect();
        let result = items[i..].iter().find_map(|r| match r {
            Item::ToolResult {
                id: rid,
                output,
                is_error,
                ..
            } if rid == id => Some(format!(
                "{} {}",
                if *is_error { "error:" } else { "ok:" },
                output
                    .lines()
                    .next()
                    .unwrap_or("")
                    .chars()
                    .take(200)
                    .collect::<String>()
            )),
            _ => None,
        });
        lines.push(format!(
            "- {name}({args}) → {}",
            result.unwrap_or_else(|| "no result".into())
        ));
    }
    lines.reverse();
    lines.join("\n")
}

/// The TaskValidate hooks as a validator (design §9): exit 0 passes; a
/// block decision or non-zero exit fails the submission with its reason.
struct HookValidator {
    hooks: Arc<crate::hooks::Hooks>,
    cwd: PathBuf,
    session: Option<String>,
    cancel: CancellationToken,
}

#[async_trait::async_trait]
impl Validator for HookValidator {
    async fn check(&self, value: &Value, _cx: &ValidateCx) -> Verdict {
        match crate::hooks::run_task_validate_hooks(
            &self.hooks,
            value,
            &self.cwd,
            self.session.as_deref(),
            &self.cancel,
        )
        .await
        {
            Ok(()) => Verdict::Pass,
            Err(feedback) => Verdict::Fail {
                kind: FailKind::Semantic,
                feedback,
            },
        }
    }

    fn name(&self) -> &str {
        "hook"
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::agent::test_support::*;
    use crate::provider::{EventSink, Provider};
    use crate::task::build::FakeRunner;
    use crate::task::verifier::tests::FakeBuild;
    use crate::task::{BudgetLimits, CommandOutput, JudgeVerdict, Rung};
    use crate::types::{Request, Response, ToolSpec};

    /// What one request carried.
    #[derive(Clone, Debug)]
    struct Seen {
        tools: Vec<ToolSpec>,
        /// The newest item's text, trailing note included.
        last: String,
    }

    impl Seen {
        fn names(&self) -> Vec<&str> {
            self.tools.iter().map(|t| t.name.as_str()).collect()
        }
    }

    /// A scripted provider that records each request; forks share both.
    struct Recording {
        left: Arc<Mutex<Vec<Response>>>,
        seen: Arc<Mutex<Vec<Seen>>>,
    }

    #[async_trait::async_trait]
    impl Provider for Recording {
        fn name(&self) -> &str {
            "fake"
        }
        fn model(&self) -> &str {
            "fake"
        }
        fn set_model(&mut self, _: String) {}
        fn fork(&self) -> Box<dyn Provider> {
            Box::new(Recording {
                left: self.left.clone(),
                seen: self.seen.clone(),
            })
        }
        async fn stream(&self, req: Request<'_>, _: EventSink<'_>) -> anyhow::Result<Response> {
            let last = match req.items.last() {
                Some(Item::User { text, .. }) => text.clone(),
                Some(Item::ToolResult { output, .. }) => output.clone(),
                _ => String::new(),
            };
            self.seen.lock().unwrap().push(Seen {
                tools: req.tools.to_vec(),
                last,
            });
            let mut left = self.left.lock().unwrap();
            anyhow::ensure!(!left.is_empty(), "script exhausted");
            Ok(left.remove(0))
        }
    }

    /// A tool under `name` that answers with its arguments and counts runs.
    /// It declares what `edit_file` declares: it writes its `path`.
    struct Echo(&'static str, Arc<std::sync::atomic::AtomicUsize>);

    #[async_trait::async_trait]
    impl crate::tools::Tool for Echo {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: self.0.into(),
                description: String::new(),
                parameters: json!({"type": "object", "properties": {"path": {"type": "string"}}}),
            }
        }
        fn effects(&self, args: &Value) -> crate::tools::Effects {
            crate::tools::Effects::writes_to(vec![PathBuf::from(
                args.get("path").and_then(Value::as_str).unwrap_or_default(),
            )])
        }
        fn summary(&self, _: &Value) -> String {
            String::new()
        }
        async fn run(&self, args: Value, _: &crate::tools::ToolCtx) -> ToolOutput {
            self.1.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            ToolOutput::ok(args.to_string())
        }
    }

    struct Fixture {
        agent: Agent,
        seen: Arc<Mutex<Vec<Seen>>>,
        runs: Arc<std::sync::atomic::AtomicUsize>,
        dir: tempfile::TempDir,
    }

    impl Fixture {
        fn new(script: Vec<Response>, spec: Option<TaskSpec>) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let seen = Arc::new(Mutex::new(Vec::new()));
            let runs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let mut agent = scripted(Vec::new());
            agent.items.clear();
            agent.cwd = dir.path().to_path_buf();
            agent.provider = Box::new(Recording {
                left: Arc::new(Mutex::new(script)),
                seen: seen.clone(),
            });
            agent.tools.add(Box::new(Echo("edit_file", runs.clone())));
            agent.task.options = Some(TaskOptions::default());
            // No build system unless a test brings one.
            agent.task.build_system = Some(None);
            agent.set_task(spec);
            Self {
                agent,
                seen,
                runs,
                dir,
            }
        }

        async fn run(&mut self, prompt: &str) -> (TurnEnd, Vec<String>) {
            let mut ui = NoticeCatch(Vec::new());
            let end = self
                .agent
                .turn(Item::user(prompt), &mut ui, CancellationToken::new())
                .await
                .unwrap();
            (end, ui.0)
        }

        fn seen(&self) -> Vec<Seen> {
            self.seen.lock().unwrap().clone()
        }
    }

    fn answer_schema() -> Value {
        json!({"type": "object", "properties": {"answer": {"type": "integer"}}, "required": ["answer"]})
    }

    fn file_task(limits: BudgetLimits) -> TaskSpec {
        let mut spec = TaskSpec::typed(
            "name an existing file",
            json!({"type": "object", "properties": {"file": {"type": "string"}}, "required": ["file"]}),
        );
        spec.validators = vec![ValidatorSpec::Exists("/file".into())];
        spec.limits = limits;
        spec.headless = true;
        spec
    }

    fn submit(id: &str, args: Value) -> Response {
        reply(vec![call(id, SUBMIT, args)])
    }

    fn text(t: &str) -> Response {
        reply(vec![Item::Assistant { text: t.into() }])
    }

    #[tokio::test]
    async fn a_chat_turn_is_untouched() {
        // No task, and the explicit chat task, behave the same.
        for spec in [None, Some(TaskSpec::chat())] {
            let mut f = Fixture::new(vec![text("hi")], spec);
            let (end, notices) = f.run("hello").await;
            assert!(matches!(end, TurnEnd::Done));
            assert!(notices.is_empty(), "{notices:?}");
            let seen = f.seen();
            assert_eq!(seen[0].names(), ["edit_file"], "no submit");
            assert!(seen[0].tools[0].parameters["properties"]
                .get("intent")
                .is_none());
            assert!(!seen[0].last.contains("[Task]"), "{}", seen[0].last);
            assert!(f.agent.task_state().is_none());
            assert!(f.agent.take_task_outcome().is_none());
            assert_eq!(f.agent.task_max_steps(7), 7);
        }
    }

    #[tokio::test]
    async fn verify_auto_starts_the_verifier_for_the_root_agent_only() {
        let fake = FakeRunner::new().on("fake-build", vec![CommandOutput::ok("")]);
        for (subagent, started) in [(false, true), (true, false)] {
            let mut f = Fixture::new(Vec::new(), None);
            f.agent.task.runner = Some(fake.clone());
            f.agent.task.build_system = Some(Some(Arc::new(FakeBuild(f.dir.path().into()))));
            f.agent.task.options = Some(TaskOptions {
                verify_auto: Some(true),
                ..TaskOptions::default()
            });
            if subagent {
                f.agent.usage_session_id = Some("parent".into());
            }
            f.agent.items.push(Item::user("fix it"));
            f.agent.task_begin();
            assert_eq!(
                f.agent.task.verifier.is_some(),
                started,
                "subagent: {subagent}"
            );
            // A chat turn with the verifier on is managed, but has no submit
            // and no budget of its own.
            assert_eq!(f.agent.task_state().is_some(), started);
            if started {
                assert_eq!(f.agent.task_state().unwrap().goal, "fix it");
                let mut specs = Vec::new();
                f.agent.task_specs(&mut specs);
                assert!(specs.is_empty());
            }
        }
    }

    #[tokio::test]
    async fn an_accepted_submit_ends_the_turn_with_the_value() {
        let spec = TaskSpec::typed("what is six times seven", answer_schema());
        let mut f = Fixture::new(vec![submit("1", json!({"answer": 42}))], Some(spec));
        let (end, _) = f.run("go").await;
        assert!(matches!(end, TurnEnd::Done));
        assert_eq!(
            f.agent.take_task_outcome(),
            Some(TaskOutcome::Submitted(json!({"answer": 42})))
        );
        let seen = f.seen();
        assert_eq!(seen.len(), 1, "one request");
        assert!(seen[0].names().contains(&SUBMIT));
        let submit = seen[0].tools.iter().find(|t| t.name == SUBMIT).unwrap();
        assert_eq!(submit.parameters, answer_schema());
        assert!(
            seen[0]
                .last
                .contains("[Task]\nYOUR CURRENT TASK: what is six times seven"),
            "{}",
            seen[0].last
        );
        assert!(seen[0].last.contains("Finish by calling `submit`"));
        assert!(results(&f.agent)[0].starts_with("Accepted"));
    }

    #[tokio::test]
    async fn rejected_submits_charge_syntax_then_semantic_which_resets_syntax() {
        let mut f = Fixture::new(
            vec![
                submit("1", json!({"file": 3})),
                submit("2", json!({"file": "missing.txt"})),
                submit("3", json!({"file": "here.txt"})),
            ],
            Some(file_task(BudgetLimits::default())),
        );
        std::fs::write(f.dir.path().join("here.txt"), "x").unwrap();
        f.run("go").await;
        let r = results(&f.agent);
        // A schema failure is a syntax charge…
        assert!(
            r[0].contains("does not match the required schema"),
            "{}",
            r[0]
        );
        assert!(
            r[0].contains("malformed by the `schema` check. Left: 2 malformed and 5 rejected"),
            "{}",
            r[0]
        );
        // …a validator failure a semantic one, which resets the syntax count.
        assert!(
            r[1].contains("These paths do not exist: missing.txt"),
            "{}",
            r[1]
        );
        assert!(
            r[1].contains("rejected by the `exists:/file` check. Left: 3 malformed and 4 rejected"),
            "{}",
            r[1]
        );
        let is_error: Vec<bool> = f
            .agent
            .items
            .iter()
            .filter_map(|i| match i {
                Item::ToolResult { is_error, .. } => Some(*is_error),
                _ => None,
            })
            .collect();
        assert_eq!(is_error, [true, true, false]);
        let st = f.agent.task_state().unwrap();
        assert_eq!((st.budget.syntax_left, st.budget.semantic_left), (3, 4));
        assert_eq!(st.failures, 2);
        assert_eq!(
            f.agent.take_task_outcome(),
            Some(TaskOutcome::Submitted(json!({"file": "here.txt"})))
        );
    }

    #[tokio::test]
    async fn stopping_without_submit_gets_a_nudge() {
        let spec = TaskSpec::typed("answer", answer_schema());
        let mut f = Fixture::new(
            vec![
                text("The answer is 42."),
                submit("1", json!({"answer": 42})),
            ],
            Some(spec),
        );
        f.run("go").await;
        let seen = f.seen();
        assert_eq!(seen.len(), 2);
        assert!(
            seen[1].last.starts_with("[Task] You must call `submit`"),
            "{}",
            seen[1].last
        );
        let st = f.agent.task_state().unwrap();
        assert_eq!((st.nudges, st.budget.syntax_left), (1, 2));
        assert!(matches!(
            f.agent.take_task_outcome(),
            Some(TaskOutcome::Submitted(_))
        ));
    }

    #[tokio::test]
    async fn a_spent_budget_ends_the_task_with_a_report() {
        let mut spec = TaskSpec::typed("answer", answer_schema());
        spec.limits.syntax = 2;
        spec.headless = true;
        // Two replies without submit spend the syntax budget; a third
        // request would fail the turn ("script exhausted").
        let mut f = Fixture::new(vec![text("a"), text("b")], Some(spec));
        let (end, notices) = f.run("go").await;
        assert!(matches!(end, TurnEnd::Done));
        let outcome = f.agent.take_task_outcome().unwrap();
        assert_eq!(outcome.exit_code(), 2);
        let TaskOutcome::Exhausted(report) = outcome else {
            panic!("{outcome:?}")
        };
        assert!(
            report.reason.starts_with("syntax budget exhausted"),
            "{}",
            report.reason
        );
        assert_eq!(report.budget.syntax_left, 0);
        assert!(
            notices.iter().any(|n| n.starts_with("Task exhausted")),
            "{notices:?}"
        );

        // The step budget ends a task that keeps calling tools.
        let mut spec = TaskSpec::typed("answer", answer_schema());
        spec.limits.steps = 1;
        let mut f = Fixture::new(
            vec![reply(vec![call("1", "edit_file", json!({"path": "a"}))])],
            Some(spec),
        );
        f.run("go").await;
        let Some(TaskOutcome::Exhausted(report)) = f.agent.take_task_outcome() else {
            panic!()
        };
        assert_eq!(report.reason, "step budget exhausted");
        assert_eq!(f.seen().len(), 1);
    }

    #[tokio::test]
    async fn a_red_build_after_a_write_reframes_the_task() {
        let mut spec = TaskSpec::typed("add the parser", answer_schema());
        spec.validators = vec![ValidatorSpec::Build];
        let mut f = Fixture::new(
            vec![
                reply(vec![call("1", "edit_file", json!({"path": "src/lib.rs"}))]),
                text("thinking"),
            ],
            Some(spec),
        );
        // Green at the start (the baseline), red after the write.
        let fake = FakeRunner::new().on(
            "fake-build",
            vec![
                CommandOutput::ok(""),
                CommandOutput::failed(1, "error: expected `;`"),
            ],
        );
        f.agent.task.runner = Some(fake.clone());
        f.agent.task.build_system = Some(Some(Arc::new(FakeBuild(f.dir.path().into()))));
        f.agent.task.options = Some(TaskOptions {
            verify_debounce: std::time::Duration::ZERO,
            verify_wait: std::time::Duration::from_secs(10),
            ..TaskOptions::default()
        });
        // The second reply has no submit; the third request (the nudge) ends
        // the script, which fails the turn. Only the requests matter here.
        let mut ui = NoticeCatch(Vec::new());
        let _ = f
            .agent
            .turn(Item::user("go"), &mut ui, CancellationToken::new())
            .await;
        let seen = f.seen();
        assert!(
            seen[0].last.contains("YOUR CURRENT TASK: add the parser"),
            "{}",
            seen[0].last
        );
        assert!(
            seen[0].last.contains("Last compilation: Succeeded!"),
            "{}",
            seen[0].last
        );
        assert!(
            seen[1].last.contains(
                "[Task]\nFix the build before continuing. Your original task: add the parser"
            ),
            "{}",
            seen[1].last
        );
        assert!(
            seen[1].last.contains("Last compilation: Failed."),
            "{}",
            seen[1].last
        );
        assert!(!seen[1].last.contains("YOUR CURRENT TASK"));
        let st = f.agent.task_state().unwrap();
        assert_eq!(st.started_red, Some(false));
        assert_eq!(st.touched, [PathBuf::from("src/lib.rs")]);
        assert_eq!(
            fake.calls()
                .iter()
                .filter(|c| c.contains("fake-build"))
                .count(),
            2,
            "the baseline and one build after the write"
        );
    }

    #[tokio::test]
    async fn repeated_failures_climb_the_ladder() {
        let mut f = Fixture::new(
            vec![
                submit("1", json!({"file": "x1"})),
                // Failure 2 → nudge.
                submit("2", json!({"file": "x2"})),
                // Failure 3 → progress check (none) → diagnosis (a plan).
                submit("3", json!({"file": "x3"})),
                reply(vec![call(
                    "p",
                    llm::PROGRESS_TOOL,
                    json!({"made": false, "why": "same error"}),
                )]),
                reply(vec![call(
                    "d",
                    llm::DIAGNOSE_TOOL,
                    json!({"has_solution": true, "plan": "Create the file first."}),
                )]),
                // Failure 4 → context reset (a compaction).
                submit("4", json!({"file": "x4"})),
                text("Summary: tried x1..x4, none exist."),
                submit("5", json!({"file": "ok.txt"})),
            ],
            Some(file_task(BudgetLimits {
                semantic: 10,
                ..BudgetLimits::default()
            })),
        );
        std::fs::write(f.dir.path().join("ok.txt"), "x").unwrap();
        let (_, notices) = f.run("go").await;
        let seen = f.seen();
        assert_eq!(seen.len(), 8, "{notices:?}");
        assert!(!seen[1].last.contains("[Task] This has now failed"));
        assert!(
            seen[2].last.contains("[Task] This has now failed 2 times"),
            "{}",
            seen[2].last
        );
        assert_eq!(seen[3].names(), [llm::PROGRESS_TOOL]);
        assert_eq!(seen[4].names(), [llm::DIAGNOSE_TOOL]);
        assert!(
            seen[5].last.starts_with("SYSTEM INTERVENTION"),
            "{}",
            seen[5].last
        );
        assert!(seen[5].last.contains("Create the file first."));
        assert!(seen[6].names().is_empty(), "the compaction request");
        assert!(
            seen[7]
                .last
                .starts_with("[Task] The harness reset the context"),
            "{}",
            seen[7].last
        );
        assert!(matches!(f.agent.items[0], Item::Compaction { .. }));
        let st = f.agent.task_state().unwrap();
        assert_eq!(st.ladder.rung(), Rung::ContextReset);
        assert_eq!(st.failures, 4);
        // The diagnosis went into the session's one intent store.
        let ws = crate::tools::workset::lock(&f.agent.workset);
        let last = &ws.recent_intents(1)[0];
        assert_eq!(last.kind, IntentKind::ApproachCorrection);
        assert_eq!(last.did, "Create the file first.");
        drop(ws);
        assert!(
            notices
                .iter()
                .any(|n| n.contains("no progress (same error)")),
            "{notices:?}"
        );
        assert_eq!(
            f.agent.take_task_outcome(),
            Some(TaskOutcome::Submitted(json!({"file": "ok.txt"})))
        );
    }

    #[tokio::test]
    async fn the_intent_field_is_offered_stripped_and_recorded() {
        let spec = TaskSpec::typed("edit", answer_schema());
        let mut f = Fixture::new(
            vec![
                reply(vec![call(
                    "1",
                    "edit_file",
                    json!({"path": "a.txt", "intent": {"did": "fix a", "why": "it is wrong", "next": "submit"}}),
                )]),
                submit("2", json!({"answer": 1})),
            ],
            Some(spec),
        );
        f.run("go").await;
        let seen = f.seen();
        let edit = seen[0]
            .tools
            .iter()
            .find(|t| t.name == "edit_file")
            .unwrap();
        assert_eq!(
            edit.parameters["properties"]["intent"]["required"],
            json!(["did", "why"])
        );
        assert!(
            edit.parameters.get("required").is_none(),
            "optional by default"
        );
        // The tool saw its own arguments only.
        assert_eq!(results(&f.agent)[0], r#"{"path":"a.txt"}"#);
        let rec = crate::tools::workset::lock(&f.agent.workset)
            .recent_intents(5)
            .to_vec();
        assert_eq!(rec.len(), 1);
        assert_eq!(rec[0].line(), "fix a — because it is wrong — next: submit");
        assert_eq!(rec[0].tool, "edit_file");
        assert_eq!(
            f.agent.task_state().unwrap().touched,
            [PathBuf::from("a.txt")]
        );
        // Rendered once, in its own section of the next request's note.
        let last = &seen[1].last;
        assert_eq!(last.matches("[Recent intents]").count(), 1, "{last}");
        assert_eq!(last.matches("did: fix a").count(), 1, "{last}");
        assert!(
            last.find("[Task]") < last.find("[Recent intents]"),
            "{last}"
        );
        assert!(
            last.find("[Recent intents]") < last.find("[Session state]"),
            "{last}"
        );
        // A read-only tool gets no intent field.
        f.agent.tools.add(Box::new(ReadOnly));
        let mut specs = f.agent.advertised();
        f.agent.task_specs(&mut specs);
        let read = specs.iter().find(|t| t.name == "peek").unwrap();
        assert!(read.parameters["properties"].get("intent").is_none());
    }

    /// A tool that only reads.
    struct ReadOnly;

    #[async_trait::async_trait]
    impl crate::tools::Tool for ReadOnly {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "peek".into(),
                description: String::new(),
                parameters: json!({"type": "object", "properties": {}}),
            }
        }
        fn effects(&self, _: &Value) -> crate::tools::Effects {
            crate::tools::Effects::reads_only()
        }
        fn summary(&self, _: &Value) -> String {
            String::new()
        }
        async fn run(&self, _: Value, _: &crate::tools::ToolCtx) -> ToolOutput {
            ToolOutput::ok("peeked")
        }
    }

    #[tokio::test]
    async fn note_intent_feeds_the_cycle_detector_from_the_same_store() {
        let spec = TaskSpec::typed("edit", answer_schema());
        let note = |id: &str| {
            reply(vec![call(
                id,
                "note_intent",
                json!({"did": "retry the same fix", "why": "it failed"}),
            )])
        };
        let mut f = Fixture::new(
            vec![
                note("1"),
                note("2"),
                note("3"),
                submit("4", json!({"answer": 1})),
            ],
            Some(spec),
        );
        crate::tools::workset::register(&mut f.agent.tools);
        f.run("go").await;
        let st = f.agent.task_state().unwrap();
        assert_eq!(
            st.intents_seen,
            crate::tools::workset::lock(&f.agent.workset).intents_recorded()
        );
        // Three identical intents in a row are a cycle: rung 1, a nudge.
        assert_ne!(st.ladder.rung(), Rung::Idle);
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn task_validate_hooks_run_after_the_built_in_checks() {
        let spec = TaskSpec::typed("answer", answer_schema());
        let mut f = Fixture::new(
            vec![
                submit("1", json!({"answer": 1})),
                submit("2", json!({"answer": 42})),
            ],
            Some(spec),
        );
        f.agent.hooks = hooks_with(
            crate::hooks::Event::TaskValidate,
            r#"grep -q '"answer":42' || { echo '{"decision":"block","reason":"the answer is 42"}'; }"#,
        );
        f.run("go").await;
        let r = results(&f.agent);
        assert!(r[0].contains("the answer is 42"), "{}", r[0]);
        assert!(r[0].contains("rejected by the `hook` check"), "{}", r[0]);
        let st = f.agent.task_state().unwrap();
        let verdicts: Vec<(&str, &str)> = st
            .checks
            .iter()
            .map(|c| (c.validator.as_str(), c.verdict))
            .collect();
        assert_eq!(verdicts, [("schema", "pass"), ("hook", "pass")]);
        assert_eq!(
            f.agent.take_task_outcome(),
            Some(TaskOutcome::Submitted(json!({"answer": 42})))
        );
    }

    /// The real built-in tools and a fake build the verifier runs: the
    /// runner counts the builds.
    fn with_builtins_and_verifier(f: &mut Fixture) -> Arc<FakeRunner> {
        f.agent.tools = crate::tools::Registry::builtin();
        let fake = FakeRunner::new().on("fake-build", vec![CommandOutput::ok("")]);
        f.agent.task.runner = Some(fake.clone());
        f.agent.task.build_system = Some(Some(Arc::new(FakeBuild(f.dir.path().into()))));
        f.agent.task.options = Some(TaskOptions {
            verify_auto: Some(true),
            verify_debounce: std::time::Duration::ZERO,
            verify_wait: std::time::Duration::from_secs(10),
            ..TaskOptions::default()
        });
        fake
    }

    fn builds(fake: &FakeRunner) -> usize {
        fake.calls()
            .iter()
            .filter(|c| c.contains("fake-build"))
            .count()
    }

    fn touched_task(file: &str) -> TaskSpec {
        let mut spec = TaskSpec::typed("make the file", answer_schema());
        spec.validators = vec![ValidatorSpec::FilesTouched(vec![PathBuf::from(file)])];
        spec
    }

    /// An `apply_patch` edit is a write like any other: its files (from
    /// the patch) are touched, so `touched:` accepts it, and the verifier
    /// runs after it.
    #[tokio::test]
    async fn an_apply_patch_edit_is_touched_and_verified() {
        let patch = "*** Begin Patch\n*** Add File: a.rs\n+pub fn a() {}\n*** End Patch\n";
        let mut f = Fixture::new(
            vec![
                reply(vec![call("1", "apply_patch", json!({"patch": patch}))]),
                submit("2", json!({"answer": 1})),
            ],
            Some(touched_task("a.rs")),
        );
        let fake = with_builtins_and_verifier(&mut f);
        f.run("go").await;
        assert!(
            results(&f.agent)[0].contains("a.rs"),
            "{:?}",
            results(&f.agent)
        );
        assert_eq!(
            f.agent.take_task_outcome(),
            Some(TaskOutcome::Submitted(json!({"answer": 1}))),
            "{:?}",
            results(&f.agent)
        );
        let st = f.agent.task_state().unwrap();
        assert_eq!(st.touched, [PathBuf::from("a.rs")]);
        assert_eq!(
            builds(&fake),
            2,
            "the baseline and one build after the patch"
        );
    }

    /// Writes a `run_code` script makes through its tools count: the file
    /// is touched and the verifier runs, though the script call itself is
    /// not a write.
    #[cfg(feature = "scripting")]
    #[tokio::test]
    async fn a_scripts_writes_are_observed_one_by_one() {
        let code = r#"await tools.write_file({path: "b.txt", content: "x\n"}); return "ok";"#;
        let mut f = Fixture::new(
            vec![
                reply(vec![call(
                    "1",
                    crate::scripting::RUN_CODE,
                    json!({"code": code}),
                )]),
                submit("2", json!({"answer": 1})),
            ],
            Some(touched_task("b.txt")),
        );
        let fake = with_builtins_and_verifier(&mut f);
        f.run("go").await;
        assert_eq!(
            std::fs::read_to_string(f.dir.path().join("b.txt")).unwrap(),
            "x\n"
        );
        assert_eq!(
            f.agent.take_task_outcome(),
            Some(TaskOutcome::Submitted(json!({"answer": 1}))),
            "{:?}",
            results(&f.agent)
        );
        let st = f.agent.task_state().unwrap();
        assert_eq!(st.touched, [PathBuf::from("b.txt")]);
        assert_eq!(st.writes, 1, "the sub-call, not the script as well");
        assert_eq!(
            builds(&fake),
            2,
            "the baseline and one build after the write"
        );
    }

    /// A call that writes files it never declared (a worktree merge, a
    /// shared child) keeps undo snapshots of them: those files are touched,
    /// so `touched:` accepts the merge's work.
    #[tokio::test]
    async fn files_a_merge_writes_are_touched() {
        struct Merge;
        #[async_trait::async_trait]
        impl crate::tools::Tool for Merge {
            fn spec(&self) -> ToolSpec {
                ToolSpec {
                    name: "merge_stub".into(),
                    description: String::new(),
                    parameters: json!({}),
                }
            }
            fn summary(&self, _: &Value) -> String {
                String::new()
            }
            async fn run(&self, _: Value, ctx: &crate::tools::ToolCtx) -> ToolOutput {
                let mut undo = crate::undo::UndoLog::default();
                undo.snapshot(
                    &ctx.cwd,
                    crate::sandbox::Mode::Off,
                    &[PathBuf::from("src/merged.rs")],
                );
                std::fs::create_dir_all(ctx.cwd.join("src")).unwrap();
                std::fs::write(ctx.cwd.join("src/merged.rs"), "pub fn m() {}\n").unwrap();
                ToolOutput {
                    changed: true,
                    undo,
                    ..ToolOutput::ok("Applied the child's changes (1 file).")
                }
            }
        }
        let mut f = Fixture::new(
            vec![
                reply(vec![call("1", "merge_stub", json!({}))]),
                submit("2", json!({"answer": 1})),
            ],
            Some(touched_task("src/merged.rs")),
        );
        f.agent.tools.add(Box::new(Merge));
        f.run("go").await;
        assert_eq!(
            f.agent.take_task_outcome(),
            Some(TaskOutcome::Submitted(json!({"answer": 1}))),
            "{:?}",
            results(&f.agent)
        );
        let st = f.agent.task_state().unwrap();
        assert_eq!(st.touched, [PathBuf::from("src/merged.rs")]);
    }

    /// What counts as a write, from the dispatch footprint and the result.
    #[tokio::test]
    async fn observed_writes_follow_the_footprint_and_reported_changes() {
        let mut f = Fixture::new(Vec::new(), Some(TaskSpec::typed("g", answer_schema())));
        f.agent.tools = crate::tools::Registry::builtin();
        f.agent.items.push(Item::user("go"));
        f.agent.task_begin();
        let changed = ToolOutput {
            changed: true,
            ..ToolOutput::ok("done")
        };
        let ok = ToolOutput::ok("done");
        let writes = |st: &TaskState| st.writes;
        // A shared general subagent reports its change (it schedules Pure).
        f.agent
            .task_observe("subagent", "{}", &changed, Some((&Concurrency::Pure, &[])));
        assert_eq!(writes(f.agent.task_state().unwrap()), 1);
        // An explore child, or a spawn/list action, changed nothing.
        f.agent
            .task_observe("subagent", "{}", &ok, Some((&Concurrency::Pure, &[])));
        f.agent
            .task_observe("subagent", "{}", &ok, Some((&Concurrency::Barrier, &[])));
        assert_eq!(writes(f.agent.task_state().unwrap()), 1);
        // A worktree merge that applied did; the files it merged come from
        // its undo records (see the test below).
        let merged = f.dir.path().join("merged.rs");
        f.agent.task_observe(
            "subagent",
            "{}",
            &changed,
            Some((&Concurrency::Barrier, &[merged])),
        );
        assert_eq!(writes(f.agent.task_state().unwrap()), 2);
        assert_eq!(
            f.agent.task_state().unwrap().touched,
            [PathBuf::from("merged.rs")]
        );
        // A script call is not a write itself: its sub-calls are observed.
        #[cfg(feature = "scripting")]
        {
            f.agent
                .task_observe(crate::scripting::RUN_CODE, "{}", &changed, None);
            assert_eq!(writes(f.agent.task_state().unwrap()), 2);
        }
        // A declared write inside the workspace is tracked; one into nyq's
        // own stores (the memory file) is not a workspace change.
        let inside = f.dir.path().join("src/x.rs");
        f.agent.task_observe(
            "write_file",
            "{}",
            &changed,
            Some((&Concurrency::WriteFs(vec![inside.clone()]), &[inside])),
        );
        let outside = PathBuf::from("/nonexistent/.nyq/memory.json");
        f.agent.task_observe(
            "memory",
            "{}",
            &changed,
            Some((&Concurrency::WriteFs(vec![outside.clone()]), &[outside])),
        );
        let st = f.agent.task_state().unwrap();
        assert_eq!(st.writes, 3);
        assert_eq!(
            st.touched,
            [PathBuf::from("merged.rs"), PathBuf::from("src/x.rs")]
        );
    }

    /// Every section once, in order: task, verify, working set, intents,
    /// session state.
    #[tokio::test]
    async fn the_note_has_each_section_once_in_order() {
        let mut f = Fixture::new(Vec::new(), Some(TaskSpec::typed("add a", answer_schema())));
        f.agent.context_window = 200_000;
        std::fs::write(f.dir.path().join("a.rs"), "fn a() {}\n").unwrap();
        let fake = FakeRunner::new().on("fake-build", vec![CommandOutput::ok("")]);
        f.agent.task.runner = Some(fake);
        f.agent.task.build_system = Some(Some(Arc::new(FakeBuild(f.dir.path().into()))));
        f.agent.task.options = Some(TaskOptions {
            verify_auto: Some(true),
            ..TaskOptions::default()
        });
        f.agent.items.push(Item::user("go"));
        f.agent.task_begin();
        f.agent
            .task
            .verifier
            .as_ref()
            .unwrap()
            .settle(std::time::Duration::from_secs(10))
            .await;
        f.agent.fold_verify(&f.agent.task_options());
        {
            let mut ws = crate::tools::workset::lock(&f.agent.workset);
            ws.open(
                f.dir.path(),
                f.dir.path().join("a.rs"),
                crate::tools::workset::Mode::Focused,
            );
            ws.record_intent("opened a.rs", "to edit it", "");
        }
        let note = f.agent.trailing_note();
        let mut at = 0;
        for header in [
            "[Task]",
            "[Verify]",
            "[Working set]",
            "[Recent intents]",
            "[Session state]",
        ] {
            assert_eq!(note.matches(header).count(), 1, "{header} once:\n{note}");
            let pos = note.find(header).unwrap();
            assert!(pos >= at, "{header} out of order:\n{note}");
            at = pos;
        }
        assert_eq!(note.matches("did: opened a.rs").count(), 1, "{note}");
        assert!(note.contains("Last compilation: Succeeded!"), "{note}");
        // The compaction brief carries the goal, the schema and the build.
        let brief = f.agent.task_compaction_brief();
        assert!(brief.contains("Task goal: add a"), "{brief}");
        assert!(brief.contains(r#""required":["answer"]"#), "{brief}");
        assert!(brief.contains("Last compilation: Succeeded!"), "{brief}");
    }

    #[tokio::test]
    async fn a_required_intent_rejects_the_call_with_a_syntax_charge() {
        let spec = TaskSpec::typed("edit", answer_schema());
        let mut f = Fixture::new(
            vec![
                reply(vec![call("1", "edit_file", json!({"path": "a.txt"}))]),
                submit("2", json!({"answer": 1})),
            ],
            Some(spec),
        );
        f.agent.task.options = Some(TaskOptions {
            intents_required: true,
            ..TaskOptions::default()
        });
        f.run("go").await;
        let edit = f.seen()[0]
            .tools
            .iter()
            .find(|t| t.name == "edit_file")
            .unwrap()
            .clone();
        assert_eq!(edit.parameters["required"], json!(["intent"]));
        let r = results(&f.agent);
        assert!(r[0].contains("must include `intent`"), "{}", r[0]);
        assert_eq!(
            f.runs.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "did not run"
        );
        assert_eq!(f.agent.task_state().unwrap().budget.syntax_left, 2);
    }

    struct FakeJudge(Mutex<Vec<Result<JudgeVerdict, String>>>);

    #[async_trait::async_trait]
    impl JudgeRunner for FakeJudge {
        async fn judge(&self, _: &str, _: &Value, _: &ValidateCx) -> Result<JudgeVerdict, String> {
            self.0.lock().unwrap().remove(0)
        }
    }

    #[tokio::test]
    async fn the_judge_rejects_then_passes_and_a_broken_judge_escalates() {
        let mut spec = TaskSpec::typed("document it", answer_schema());
        spec.validators = vec![ValidatorSpec::Judge("the README mentions it".into())];
        spec.headless = true;
        let mut f = Fixture::new(
            vec![
                submit("1", json!({"answer": 1})),
                submit("2", json!({"answer": 2})),
            ],
            Some(spec.clone()),
        );
        f.agent.task.judge = Some(Arc::new(FakeJudge(Mutex::new(vec![
            Ok(JudgeVerdict {
                complete: false,
                feedback: "the README is unchanged".into(),
            }),
            Ok(JudgeVerdict {
                complete: true,
                feedback: String::new(),
            }),
        ]))));
        f.run("go").await;
        assert!(results(&f.agent)[0].contains("the README is unchanged"));
        assert_eq!(
            f.agent.take_task_outcome(),
            Some(TaskOutcome::Submitted(json!({"answer": 2})))
        );

        let mut f = Fixture::new(vec![submit("1", json!({"answer": 1}))], Some(spec));
        f.agent.task.judge = Some(Arc::new(FakeJudge(Mutex::new(vec![
            Err("no model".into()),
        ]))));
        f.run("go").await;
        let Some(TaskOutcome::Escalated(report)) = f.agent.take_task_outcome() else {
            panic!()
        };
        assert!(
            report
                .reason
                .contains("could not decide: judge could not run: no model"),
            "{}",
            report.reason
        );
    }

    #[tokio::test]
    async fn calls_after_an_accepted_submit_do_not_run() {
        let spec = TaskSpec::typed("answer", answer_schema());
        let mut f = Fixture::new(
            vec![reply(vec![
                call("1", SUBMIT, json!({"answer": 1})),
                call("2", "edit_file", json!({"path": "a"})),
            ])],
            Some(spec),
        );
        f.run("go").await;
        assert_eq!(f.runs.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(results(&f.agent)[1].starts_with("Not run"));
        assert!(matches!(
            f.agent.take_task_outcome(),
            Some(TaskOutcome::Submitted(_))
        ));
    }

    #[test]
    fn recent_actions_pair_calls_with_results() {
        let items = vec![
            call("1", "bash", json!({"command": "ls"})),
            Item::ToolResult {
                id: "1".into(),
                output: "a\nb".into(),
                is_error: false,
                labels: Vec::new(),
            },
            call("2", "edit_file", json!({"path": "x"})),
        ];
        assert_eq!(
            recent_actions(&items, 5),
            "- bash({\"command\":\"ls\"}) → ok: a\n- edit_file({\"path\":\"x\"}) → no result"
        );
        assert_eq!(
            recent_actions(&items, 1),
            "- edit_file({\"path\":\"x\"}) → no result"
        );
    }

    // --- memo replays (G3) ---

    /// A judge that answers `complete` and counts its calls.
    struct CountingJudge {
        model: Option<String>,
        complete: bool,
        calls: std::sync::atomic::AtomicUsize,
    }

    impl CountingJudge {
        fn new(model: Option<&str>, complete: bool) -> Arc<Self> {
            Arc::new(Self {
                model: model.map(String::from),
                complete,
                calls: Default::default(),
            })
        }

        fn calls(&self) -> usize {
            self.calls.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl JudgeRunner for CountingJudge {
        async fn judge(&self, _: &str, _: &Value, cx: &ValidateCx) -> Result<JudgeVerdict, String> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            // It reads the touched files, as the LLM judge does.
            assert!(!cx.touched.is_empty(), "the judge sees the touched files");
            Ok(JudgeVerdict {
                complete: self.complete,
                feedback: "not there".into(),
            })
        }

        fn model_id(&self) -> Option<String> {
            self.model.clone()
        }
    }

    async fn memo_lookup(
        agent: &mut Agent,
        spec: &TaskSpec,
        home: &Path,
    ) -> crate::task::headless::MemoLookup {
        crate::task::headless::memo_lookup(
            agent,
            spec,
            home,
            &mut QuietUi,
            &CancellationToken::new(),
        )
        .await
    }

    /// A `--memo` task that writes a file keeps the touched files with their
    /// hashes and the judges that passed. A replay restores the touched set,
    /// so `touched:` passes, and reuses the judge's pass without asking it,
    /// until the judge model or a touched file changes.
    #[tokio::test]
    async fn a_memo_replay_restores_touched_files_and_reuses_the_judge() {
        use crate::task::headless::MemoLookup;
        use crate::task::memo::{hash_file_at, Evidence, MemoStore, TouchedFile};

        let mut spec = TaskSpec::typed("make the file", answer_schema());
        spec.validators = vec![
            ValidatorSpec::FilesTouched(vec![PathBuf::from("b.txt")]),
            ValidatorSpec::Judge("b.txt says hi".into()),
        ];
        spec.memo = Some(engine::MemoSpec {
            files: vec![PathBuf::from("in.txt")],
        });
        let mut f = Fixture::new(
            vec![
                reply(vec![call(
                    "1",
                    "write_file",
                    json!({"path": "b.txt", "content": "hi\n"}),
                )]),
                submit("2", json!({"answer": 1})),
            ],
            Some(spec.clone()),
        );
        f.agent.tools = crate::tools::Registry::builtin();
        let root = f.dir.path().to_path_buf();
        std::fs::write(root.join("in.txt"), "v1").unwrap();
        let judge = CountingJudge::new(Some("fake/judge"), true);
        f.agent.task.judge = Some(judge.clone());
        f.run("go").await;
        let value = json!({"answer": 1});
        assert_eq!(
            f.agent.take_task_outcome(),
            Some(TaskOutcome::Submitted(value.clone())),
            "{:?}",
            results(&f.agent)
        );
        assert_eq!(judge.calls(), 1);
        let evidence = f.agent.take_memo_evidence().expect("memo evidence");
        assert_eq!(
            evidence.touched.touched,
            [TouchedFile {
                path: PathBuf::from("b.txt"),
                hash: hash_file_at(&root, Path::new("b.txt")),
            }]
        );
        assert_eq!(
            evidence.judges,
            [("b.txt says hi".to_string(), "fake/judge".to_string())]
        );

        // A later run: no task state, as in `nyq -p --memo`.
        f.agent.set_task(None);
        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        let MemoLookup::Miss(key) = memo_lookup(&mut f.agent, &spec, home).await else {
            panic!("a first lookup misses");
        };
        crate::task::headless::memo_put(home, &root, &key, &value, Some(evidence)).unwrap();
        assert_eq!(
            memo_lookup(&mut f.agent, &spec, home).await,
            MemoLookup::Hit(value.clone())
        );
        assert_eq!(judge.calls(), 1, "the judge's pass was reused");

        // Another judge model is asked; its pass is recorded and reused.
        let other = CountingJudge::new(Some("fake/other"), true);
        f.agent.task.judge = Some(other.clone());
        for _ in 0..2 {
            assert_eq!(
                memo_lookup(&mut f.agent, &spec, home).await,
                MemoLookup::Hit(value.clone())
            );
        }
        assert_eq!(other.calls(), 1);
        // A judge without a model id is always asked.
        let anonymous = CountingJudge::new(None, true);
        f.agent.task.judge = Some(anonymous.clone());
        for _ in 0..2 {
            assert_eq!(
                memo_lookup(&mut f.agent, &spec, home).await,
                MemoLookup::Hit(value.clone())
            );
        }
        assert_eq!(anonymous.calls(), 2);
        // A new judge model that rejects it now: a miss.
        let strict = CountingJudge::new(Some("fake/strict"), false);
        f.agent.task.judge = Some(strict.clone());
        assert_eq!(
            memo_lookup(&mut f.agent, &spec, home).await,
            MemoLookup::Miss(key.clone())
        );
        assert_eq!(strict.calls(), 1);

        // A touched file that changed since: a miss, without asking the
        // judge, and the entry is dropped.
        f.agent.task.judge = Some(judge.clone());
        let store = MemoStore::for_workspace(home, &root);
        let mut sealed = Evidence::capture(&root, &[PathBuf::from("b.txt")]);
        sealed.add_judge("b.txt says hi", "fake/judge", &value, key.fingerprint());
        store.put_with(&key, &value, &sealed).unwrap();
        assert_eq!(
            memo_lookup(&mut f.agent, &spec, home).await,
            MemoLookup::Hit(value.clone())
        );
        std::fs::write(root.join("b.txt"), "bye\n").unwrap();
        assert_eq!(
            memo_lookup(&mut f.agent, &spec, home).await,
            MemoLookup::Miss(key.clone())
        );
        assert!(store.get::<Value>(&key).is_none(), "the entry was dropped");
        // Deleting it misses too.
        std::fs::write(root.join("b.txt"), "hi\n").unwrap();
        store.put_with(&key, &value, &sealed).unwrap();
        std::fs::remove_file(root.join("b.txt")).unwrap();
        assert_eq!(
            memo_lookup(&mut f.agent, &spec, home).await,
            MemoLookup::Miss(key.clone())
        );
        assert_eq!(judge.calls(), 1, "a changed file is never judged");
    }

    /// No memo poisoning: a cached result passes only checks its current
    /// inputs pass. An entry without evidence (an older nyq's) misses on
    /// `touched:`; a judge's pass recorded for another value, rubric or
    /// fingerprint is not reused, and the judge is asked.
    #[tokio::test]
    async fn a_memo_replay_reuses_nothing_recorded_for_other_inputs() {
        use crate::task::headless::MemoLookup;
        use crate::task::memo::{Evidence, MemoStore};

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        std::fs::write(root.join("in.txt"), "v1").unwrap();
        std::fs::write(root.join("b.txt"), "hi\n").unwrap();
        let mut agent = crate::agent::test_support::test_agent();
        agent.cwd = root.to_path_buf();
        let memo = Some(engine::MemoSpec {
            files: vec![PathBuf::from("in.txt")],
        });
        let value = json!({"answer": 1});

        // `touched:` with no recorded touched files.
        let mut touched = TaskSpec::typed("make the file", answer_schema());
        touched.validators = vec![ValidatorSpec::FilesTouched(vec![PathBuf::from("b.txt")])];
        touched.memo = memo.clone();
        let MemoLookup::Miss(key) = memo_lookup(&mut agent, &touched, home).await else {
            panic!("a first lookup misses");
        };
        crate::task::headless::memo_put(home, root, &key, &value, None).unwrap();
        assert_eq!(
            memo_lookup(&mut agent, &touched, home).await,
            MemoLookup::Miss(key.clone())
        );
        // The touched set must hold the file `touched:` names.
        let store = MemoStore::for_workspace(home, root);
        std::fs::write(root.join("c.txt"), "c").unwrap();
        let other_file = Evidence::capture(root, &[PathBuf::from("c.txt")]);
        store.put_with(&key, &value, &other_file).unwrap();
        assert_eq!(
            memo_lookup(&mut agent, &touched, home).await,
            MemoLookup::Miss(key.clone())
        );

        // A judge that would reject the value now.
        let mut judged = TaskSpec::typed("make the file", answer_schema());
        judged.validators = vec![ValidatorSpec::Judge("b.txt says hi".into())];
        judged.memo = memo;
        let strict = CountingJudge::new(Some("fake/judge"), false);
        agent.task.judge = Some(strict.clone());
        let MemoLookup::Miss(key) = memo_lookup(&mut agent, &judged, home).await else {
            panic!("a first lookup misses");
        };
        let fp = key.fingerprint().to_string();
        let files = [PathBuf::from("b.txt")];
        for (rubric, judged_value, fingerprint) in [
            ("b.txt says hi", json!({"answer": 2}), fp.as_str()),
            ("something else", value.clone(), fp.as_str()),
            ("b.txt says hi", value.clone(), "files:another"),
        ] {
            let mut forged = Evidence::capture(root, &files);
            forged.add_judge(rubric, "fake/judge", &judged_value, fingerprint);
            store.put_with(&key, &value, &forged).unwrap();
            let before = strict.calls();
            assert_eq!(
                memo_lookup(&mut agent, &judged, home).await,
                MemoLookup::Miss(key.clone()),
                "{rubric} {judged_value} {fingerprint}"
            );
            assert_eq!(strict.calls(), before + 1, "the judge was asked");
        }
        // The matching record is reused (the judge is not asked).
        let mut genuine = Evidence::capture(root, &files);
        genuine.add_judge("b.txt says hi", "fake/judge", &value, &fp);
        store.put_with(&key, &value, &genuine).unwrap();
        let before = strict.calls();
        assert_eq!(
            memo_lookup(&mut agent, &judged, home).await,
            MemoLookup::Hit(value.clone())
        );
        assert_eq!(strict.calls(), before);
    }
}
