//! The task engine (design §4).
//!
//! The library layer:
//!
//! - [`build`]: [`BuildSystem`] detection and the verifier primitives (build,
//!   test, lint, format check) behind an injectable [`CommandRunner`], with
//!   timeouts, cancellation, error condensing and single-test reruns, and
//!   each ecosystem's add-a-dependency command (`add_dep_command`, library
//!   only: no model tool runs it). Also
//!   [`VerifyState`], which renders the "LATEST COMPILE/TEST STATE" note.
//! - [`validate`]: the [`Validator`] trait, [`Verdict`] and the built-in
//!   validators and combinators.
//! - [`budget`]: [`Budget`] (two failure counters, steps, tokens, cost,
//!   deadline) and the [`EscalationLadder`] state machine.
//! - [`cycle`]: [`CycleDetector`], loop detection over steps and intents.
//! - [`memo`]: [`MemoStore`], results cached under `~/.nyq/cache/<workspace>`,
//!   with the [`memo::Evidence`] a replay needs (touched-file hashes, judge
//!   passes).
//!
//! The runtime layer, which `crate::agent::task` wires into the loop:
//!
//! - [`engine`]: [`TaskSpec`], [`TaskState`], [`TaskOutcome`] and the texts
//!   the model sees (trailing-note task section, nudges, interventions).
//! - [`verifier`]: the debounced background [`Verifier`].
//! - [`llm`]: the forced-tool helper calls (progress check, diagnosis) and the
//!   [`LlmJudge`].
//! - [`tier`]: providers per model tier; [`options`]: the `[tier]`,
//!   `[verify]` and `[intents]` settings; [`intent`]: the intent field.
//! - [`headless`]: `nyq -p --schema` and friends.
//!
//! # How the wiring stage uses this
//!
//! **The `submit` tool.** A task exposes `submit`, whose input schema is the
//! output type. The arguments go through the task's validators in order
//! (usually `SchemaValidator` first, then `All([...])` or `Same([...])`):
//! - `Verdict::Fail { kind: Syntax, .. }` → `budget.record_syntax_error()`,
//!   and the feedback goes back as a `tool.result` with `is_error`;
//! - `Verdict::Fail { kind: Semantic, .. }` → `budget.record_semantic_error()`
//!   (which resets the syntax counter), same feedback path;
//! - `Verdict::Escalate(why)` → straight to the top of the ladder;
//! - `Verdict::Pass` → the task is done; store a memo if `Task.memo` is set.
//!
//! The wiring builds one [`Exec`] (with `ShellRunner::new(sandbox_policy)`, the
//! run's cancel token, and timeouts from settings) and one [`ValidateCx`] per
//! submit, filling `touched` from the files the task wrote.
//!
//! **The trailing-note verify state.** A debounced background verifier runs
//! `BuildSystem::build` (and `test` for the affected scope) after write events
//! and records the outcomes into a [`VerifyState`]. Every request's trailing
//! note appends `VerifyState::trailing_note()`. Older error tool results are
//! cut down with [`shorten_old_errors`] when the request is assembled, so only
//! the newest error is in full. `needs_repair_priority(started_red)` drives the
//! "fix the build first" reframing, and `progress_check_due(n)` says when to
//! climb to the progress check.
//!
//! **The ladder.** Each step the loop pushes `Step::new(tool, args, result,
//! is_error)` and the model's intent into the [`CycleDetector`]. A `Cycle`, a
//! low budget (`Budget::is_low`) or repeated build failures become a
//! `LadderEvent::Stalled(reason)`. The returned `LadderAction` says what to do:
//! inject `Cycle::note()`; run the cheap progress-check child task; run the
//! high-tier diagnose child task; hand off and reset the context (then
//! `CycleDetector::clear`); bump the model tier; or return an escalation. The
//! child tasks' answers go back in as `ProgressChecked` and `Diagnosed`, and a
//! green verify as `Recovered`. `Budget::exhausted` ends the task regardless of
//! the rung.
#![allow(dead_code, unused_imports)]

pub mod budget;
pub mod build;
pub mod cycle;
pub mod engine;
pub mod headless;
pub mod intent;
pub mod llm;
pub mod memo;
pub mod options;
pub mod tier;
pub mod validate;
pub mod verifier;

pub use budget::{
    Budget, BudgetLimits, EscalationLadder, Exhaustion, LadderAction, LadderEvent, Rung,
    StallReason,
};
pub use build::{
    condense, detect_build_system, shell_quote, shorten_old_errors, BuildSystem, CheckOutcome,
    CommandOutput, CommandRunner, CommandSpec, Condensed, Exec, OutcomeKind, ShellRunner,
    StepState, TestScope, VerifyState,
};
pub use cycle::{Cycle, CycleConfig, CycleDetector, Step};
pub use engine::{
    CheckRecord, TaskOutcome, TaskReport, TaskSpec, TaskState, Tier, TierPolicy, ValidatorSpec,
    VerifyMode, SUBMIT,
};
pub use llm::LlmJudge;
pub use memo::{fingerprint, hash_files, MemoKey, MemoStore};
pub use options::TaskOptions;
pub use validate::{
    All, AlwaysPass, BuildValidator, CustomCommandValidator, ExistsValidator, FailKind,
    FilesTouchedValidator, FormatValidator, JudgeRunner, JudgeValidator, JudgeVerdict,
    LintValidator, PathSource, Same, SchemaValidator, TestsValidator, ValidateCx, Validator,
    Verdict,
};
pub use verifier::Verifier;
