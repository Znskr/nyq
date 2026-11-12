//! Budgets and the escalation ladder (design §4.3).
//!
//! Two failure counters, as in promptyped: a *semantic* failure resets the
//! syntax counter, because it proves the syntax was right. Steps, tokens, cost
//! and wall-clock are plain allowances. When a budget runs low or the cycle
//! detector trips, [`EscalationLadder`] says what to do next. Everything here is
//! pure: callers pass the clock in where time matters.

use std::time::{Duration, Instant};

/// What a task may spend. `None` means unlimited.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BudgetLimits {
    /// Malformed submissions allowed in a row (reset by a semantic failure).
    pub syntax: u32,
    /// Well-formed but rejected submissions allowed.
    pub semantic: u32,
    pub steps: u32,
    pub tokens: Option<u64>,
    pub cost: Option<f64>,
    pub wall: Option<Duration>,
}

impl Default for BudgetLimits {
    fn default() -> Self {
        Self {
            syntax: 3,
            semantic: 5,
            steps: 100,
            tokens: None,
            cost: None,
            wall: None,
        }
    }
}

/// Which allowance ran out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exhaustion {
    Deadline,
    Cost,
    Tokens,
    Steps,
    Semantic,
    Syntax,
}

/// What is left of a task's allowance. A failure counter reaching 0 means all
/// of its failures are used up.
#[derive(Clone, Debug)]
pub struct Budget {
    pub syntax_left: u32,
    pub semantic_left: u32,
    pub steps_left: u32,
    pub tokens_left: Option<u64>,
    pub cost_left: Option<f64>,
    pub deadline: Option<Instant>,
    limits: BudgetLimits,
    started: Instant,
}

impl Budget {
    pub fn new(limits: BudgetLimits) -> Self {
        Self::starting_at(limits, Instant::now())
    }

    /// Like [`Budget::new`] with an explicit clock, for tests.
    pub fn starting_at(limits: BudgetLimits, now: Instant) -> Self {
        Self {
            syntax_left: limits.syntax,
            semantic_left: limits.semantic,
            steps_left: limits.steps,
            tokens_left: limits.tokens,
            cost_left: limits.cost,
            deadline: limits.wall.map(|w| now + w),
            limits,
            started: now,
        }
    }

    pub fn limits(&self) -> &BudgetLimits {
        &self.limits
    }

    /// A malformed submission.
    pub fn record_syntax_error(&mut self) {
        self.syntax_left = self.syntax_left.saturating_sub(1);
    }

    /// A well-formed submission the validators rejected. The syntax counter
    /// goes back to its initial value.
    pub fn record_semantic_error(&mut self) {
        self.semantic_left = self.semantic_left.saturating_sub(1);
        self.syntax_left = self.limits.syntax;
    }

    pub fn record_step(&mut self) {
        self.steps_left = self.steps_left.saturating_sub(1);
    }

    pub fn spend_tokens(&mut self, n: u64) {
        if let Some(t) = &mut self.tokens_left {
            *t = t.saturating_sub(n);
        }
    }

    pub fn spend_cost(&mut self, usd: f64) {
        if let Some(c) = &mut self.cost_left {
            *c = (*c - usd).max(0.0);
        }
    }

    /// The allowance that ran out, if any.
    pub fn exhausted(&self) -> Option<Exhaustion> {
        self.exhausted_at(Instant::now())
    }

    pub fn exhausted_at(&self, now: Instant) -> Option<Exhaustion> {
        if self.deadline.is_some_and(|d| now >= d) {
            Some(Exhaustion::Deadline)
        } else if self.cost_left.is_some_and(|c| c <= 0.0) {
            Some(Exhaustion::Cost)
        } else if self.tokens_left == Some(0) {
            Some(Exhaustion::Tokens)
        } else if self.steps_left == 0 {
            Some(Exhaustion::Steps)
        } else if self.semantic_left == 0 {
            Some(Exhaustion::Semantic)
        } else if self.syntax_left == 0 {
            Some(Exhaustion::Syntax)
        } else {
            None
        }
    }

    /// Running low: a failure counter at its last chance, or 20% or less of
    /// any other allowance. Used to start climbing the ladder before the
    /// budget is gone.
    pub fn is_low(&self) -> bool {
        self.is_low_at(Instant::now())
    }

    pub fn is_low_at(&self, now: Instant) -> bool {
        let l = &self.limits;
        let frac_u = |left: u64, total: u64| total > 0 && left * 5 <= total;
        if (l.syntax > 1 && self.syntax_left <= 1) || (l.semantic > 1 && self.semantic_left <= 1) {
            return true;
        }
        if frac_u(self.steps_left.into(), l.steps.into()) {
            return true;
        }
        if let (Some(left), Some(total)) = (self.tokens_left, l.tokens) {
            if frac_u(left, total) {
                return true;
            }
        }
        if let (Some(left), Some(total)) = (self.cost_left, l.cost) {
            if total > 0.0 && left <= total * 0.2 {
                return true;
            }
        }
        if let (Some(deadline), Some(wall)) = (self.deadline, l.wall) {
            let left = deadline.saturating_duration_since(now);
            if left.as_secs_f64() <= wall.as_secs_f64() * 0.2 {
                return true;
            }
        }
        false
    }

    pub fn elapsed_at(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.started)
    }
}

// ---------------------------------------------------------------------------
// Escalation ladder
// ---------------------------------------------------------------------------

/// Why the harness decided the task is stuck.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StallReason {
    /// The cycle detector tripped.
    Cycle,
    /// A budget is running low.
    BudgetLow,
    /// The build or the validators keep failing.
    ConsecutiveFailures,
}

/// The rungs, in order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Rung {
    /// Not climbing.
    Idle,
    /// Inject the repeat or cycle note.
    Nudge,
    /// A cheap child task asks whether the recent errors show progress.
    ProgressCheck,
    /// A high-tier read-only child task looks for a way out.
    Diagnose,
    /// Summarise into a handoff note, clear the task history, start fresh.
    ContextReset,
    /// Retry with the next model tier.
    TierBump,
    /// Give up to the parent.
    Escalate,
}

/// What happened, from the ladder's point of view.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LadderEvent {
    /// The task looks stuck.
    Stalled(StallReason),
    /// The progress check came back.
    ProgressChecked { made: bool },
    /// The diagnosis came back.
    Diagnosed { has_solution: bool },
    /// Real progress: the build went green, the error count fell, a test
    /// started passing.
    Recovered,
}

/// What the caller should do now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LadderAction {
    /// Add the repeat/cycle note to the next request.
    Nudge,
    /// Run the progress-check child task, then report `ProgressChecked`.
    RunProgressCheck,
    /// Run the diagnose child task, then report `Diagnosed`.
    RunDiagnose,
    /// Hand off and clear the task history.
    ResetContext,
    /// Switch to the next model tier.
    BumpTier,
    /// Return the escalation to the parent task or program.
    Escalate,
    /// Carry on; nothing to do.
    Continue,
}

/// The state machine Nudge → ProgressCheck → Diagnose → ContextReset →
/// TierBump → Escalate. Each `Stalled` climbs one rung. Transitions are pure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EscalationLadder {
    rung: Rung,
    tier_available: bool,
    last_reason: Option<StallReason>,
}

impl EscalationLadder {
    /// `tier_available` is false when the task already runs on the top tier;
    /// the ladder then skips the tier bump.
    pub fn new(tier_available: bool) -> Self {
        Self {
            rung: Rung::Idle,
            tier_available,
            last_reason: None,
        }
    }

    pub fn rung(&self) -> Rung {
        self.rung
    }

    pub fn last_reason(&self) -> Option<StallReason> {
        self.last_reason
    }

    pub fn set_tier_available(&mut self, available: bool) {
        self.tier_available = available;
    }

    fn climb(&mut self) -> LadderAction {
        self.rung = match self.rung {
            Rung::Idle => Rung::Nudge,
            Rung::Nudge => Rung::ProgressCheck,
            Rung::ProgressCheck => Rung::Diagnose,
            Rung::Diagnose => Rung::ContextReset,
            Rung::ContextReset if self.tier_available => Rung::TierBump,
            Rung::ContextReset | Rung::TierBump | Rung::Escalate => Rung::Escalate,
        };
        self.action()
    }

    fn action(&self) -> LadderAction {
        match self.rung {
            Rung::Idle => LadderAction::Continue,
            Rung::Nudge => LadderAction::Nudge,
            Rung::ProgressCheck => LadderAction::RunProgressCheck,
            Rung::Diagnose => LadderAction::RunDiagnose,
            Rung::ContextReset => LadderAction::ResetContext,
            Rung::TierBump => LadderAction::BumpTier,
            Rung::Escalate => LadderAction::Escalate,
        }
    }

    /// Feed one event and get the action to take.
    pub fn step(&mut self, event: LadderEvent) -> LadderAction {
        match event {
            LadderEvent::Stalled(reason) => {
                self.last_reason = Some(reason);
                self.climb()
            }
            LadderEvent::ProgressChecked { made } => {
                if self.rung != Rung::ProgressCheck {
                    return LadderAction::Continue;
                }
                if made {
                    // Carry on; the next stall asks again rather than
                    // repeating the nudge.
                    self.rung = Rung::Nudge;
                    LadderAction::Continue
                } else {
                    self.climb()
                }
            }
            LadderEvent::Diagnosed { has_solution } => {
                if self.rung != Rung::Diagnose {
                    return LadderAction::Continue;
                }
                if has_solution {
                    // The caller injects the plan as a SYSTEM INTERVENTION
                    // note. Staying on this rung makes the next stall a reset.
                    LadderAction::Continue
                } else {
                    self.climb()
                }
            }
            LadderEvent::Recovered => {
                self.rung = Rung::Idle;
                self.last_reason = None;
                LadderAction::Continue
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> BudgetLimits {
        BudgetLimits {
            syntax: 3,
            semantic: 4,
            steps: 10,
            tokens: Some(1000),
            cost: Some(1.0),
            wall: Some(Duration::from_secs(100)),
        }
    }

    #[test]
    fn syntax_errors_count_down() {
        let mut b = Budget::new(limits());
        b.record_syntax_error();
        b.record_syntax_error();
        assert_eq!(b.syntax_left, 1);
        assert_eq!(b.exhausted(), None);
        b.record_syntax_error();
        assert_eq!(b.exhausted(), Some(Exhaustion::Syntax));
        b.record_syntax_error(); // saturates
        assert_eq!(b.syntax_left, 0);
    }

    #[test]
    fn semantic_error_resets_syntax_counter() {
        let mut b = Budget::new(limits());
        b.record_syntax_error();
        b.record_syntax_error();
        assert_eq!(b.syntax_left, 1);
        b.record_semantic_error();
        assert_eq!(b.syntax_left, 3, "reset to the initial value");
        assert_eq!(b.semantic_left, 3);
    }

    #[test]
    fn semantic_exhaustion() {
        let mut b = Budget::new(limits());
        for _ in 0..4 {
            assert_eq!(b.exhausted(), None);
            b.record_semantic_error();
        }
        assert_eq!(b.exhausted(), Some(Exhaustion::Semantic));
    }

    #[test]
    fn steps_tokens_cost_exhaust() {
        let mut b = Budget::new(limits());
        for _ in 0..10 {
            b.record_step();
        }
        assert_eq!(b.exhausted(), Some(Exhaustion::Steps));

        let mut b = Budget::new(limits());
        b.spend_tokens(999);
        assert_eq!(b.exhausted(), None);
        b.spend_tokens(5000);
        assert_eq!(b.tokens_left, Some(0));
        assert_eq!(b.exhausted(), Some(Exhaustion::Tokens));

        let mut b = Budget::new(limits());
        b.spend_cost(0.6);
        assert_eq!(b.exhausted(), None);
        b.spend_cost(0.6);
        assert_eq!(b.exhausted(), Some(Exhaustion::Cost));
    }

    #[test]
    fn deadline_uses_the_clock_passed_in() {
        let t0 = Instant::now();
        let b = Budget::starting_at(limits(), t0);
        assert_eq!(b.exhausted_at(t0 + Duration::from_secs(99)), None);
        assert_eq!(
            b.exhausted_at(t0 + Duration::from_secs(100)),
            Some(Exhaustion::Deadline)
        );
        assert_eq!(
            b.elapsed_at(t0 + Duration::from_secs(7)),
            Duration::from_secs(7)
        );
    }

    #[test]
    fn unlimited_resources_never_exhaust() {
        let mut b = Budget::new(BudgetLimits::default());
        b.spend_tokens(u64::MAX);
        b.spend_cost(1e9);
        assert_eq!(b.tokens_left, None);
        assert_eq!(b.exhausted(), None);
        assert!(!b.is_low());
    }

    #[test]
    fn exhaustion_priority_order() {
        let mut b = Budget::new(limits());
        for _ in 0..10 {
            b.record_step();
        }
        b.spend_cost(2.0);
        assert_eq!(b.exhausted(), Some(Exhaustion::Cost), "cost before steps");
    }

    #[test]
    fn low_detection() {
        let t0 = Instant::now();
        let mut b = Budget::starting_at(limits(), t0);
        assert!(!b.is_low_at(t0));
        for _ in 0..8 {
            b.record_step();
        }
        assert!(b.is_low_at(t0), "2 of 10 steps left");

        let mut b = Budget::starting_at(limits(), t0);
        b.record_syntax_error();
        b.record_syntax_error();
        assert!(b.is_low_at(t0), "last syntax chance");

        let b = Budget::starting_at(limits(), t0);
        assert!(
            b.is_low_at(t0 + Duration::from_secs(85)),
            "wall clock nearly up"
        );
        assert!(!b.is_low_at(t0 + Duration::from_secs(50)));

        let mut b = Budget::starting_at(limits(), t0);
        b.spend_tokens(850);
        assert!(b.is_low_at(t0));
        let mut b = Budget::starting_at(limits(), t0);
        b.spend_cost(0.85);
        assert!(b.is_low_at(t0));
    }

    #[test]
    fn single_chance_counters_are_not_always_low() {
        let b = Budget::new(BudgetLimits {
            syntax: 1,
            semantic: 1,
            ..BudgetLimits::default()
        });
        assert!(!b.is_low());
    }

    // --- ladder ---

    const STALL: LadderEvent = LadderEvent::Stalled(StallReason::Cycle);

    #[test]
    fn ladder_climbs_every_rung_in_order() {
        let mut l = EscalationLadder::new(true);
        assert_eq!(l.rung(), Rung::Idle);
        assert_eq!(l.step(STALL), LadderAction::Nudge);
        assert_eq!(l.step(STALL), LadderAction::RunProgressCheck);
        assert_eq!(
            l.step(LadderEvent::ProgressChecked { made: false }),
            LadderAction::RunDiagnose
        );
        assert_eq!(
            l.step(LadderEvent::Diagnosed {
                has_solution: false
            }),
            LadderAction::ResetContext
        );
        assert_eq!(l.step(STALL), LadderAction::BumpTier);
        assert_eq!(l.step(STALL), LadderAction::Escalate);
        assert_eq!(l.step(STALL), LadderAction::Escalate, "stays at the top");
        assert_eq!(l.rung(), Rung::Escalate);
    }

    #[test]
    fn ladder_stall_alone_walks_through() {
        let mut l = EscalationLadder::new(true);
        let actions: Vec<_> = (0..6).map(|_| l.step(STALL)).collect();
        assert_eq!(
            actions,
            vec![
                LadderAction::Nudge,
                LadderAction::RunProgressCheck,
                LadderAction::RunDiagnose,
                LadderAction::ResetContext,
                LadderAction::BumpTier,
                LadderAction::Escalate
            ]
        );
    }

    #[test]
    fn progress_made_holds_the_ladder_down() {
        let mut l = EscalationLadder::new(true);
        l.step(STALL);
        assert_eq!(l.step(STALL), LadderAction::RunProgressCheck);
        assert_eq!(
            l.step(LadderEvent::ProgressChecked { made: true }),
            LadderAction::Continue
        );
        assert_eq!(l.rung(), Rung::Nudge);
        assert_eq!(l.step(STALL), LadderAction::RunProgressCheck, "asks again");
    }

    #[test]
    fn diagnosis_with_a_solution_stays_on_its_rung() {
        let mut l = EscalationLadder::new(true);
        for _ in 0..3 {
            l.step(STALL);
        }
        assert_eq!(l.rung(), Rung::Diagnose);
        assert_eq!(
            l.step(LadderEvent::Diagnosed { has_solution: true }),
            LadderAction::Continue
        );
        assert_eq!(l.rung(), Rung::Diagnose);
        assert_eq!(l.step(STALL), LadderAction::ResetContext);
    }

    #[test]
    fn tier_bump_is_skipped_at_the_top_tier() {
        let mut l = EscalationLadder::new(false);
        for _ in 0..4 {
            l.step(STALL);
        }
        assert_eq!(l.rung(), Rung::ContextReset);
        assert_eq!(l.step(STALL), LadderAction::Escalate);
    }

    #[test]
    fn tier_availability_can_change_midway() {
        let mut l = EscalationLadder::new(true);
        for _ in 0..4 {
            l.step(STALL);
        }
        l.set_tier_available(false);
        assert_eq!(l.step(STALL), LadderAction::Escalate);
    }

    #[test]
    fn recovery_resets_the_ladder() {
        let mut l = EscalationLadder::new(true);
        for _ in 0..4 {
            l.step(STALL);
        }
        assert_eq!(l.step(LadderEvent::Recovered), LadderAction::Continue);
        assert_eq!(l.rung(), Rung::Idle);
        assert_eq!(l.last_reason(), None);
        assert_eq!(l.step(STALL), LadderAction::Nudge);
    }

    #[test]
    fn out_of_phase_answers_are_ignored() {
        let mut l = EscalationLadder::new(true);
        assert_eq!(
            l.step(LadderEvent::ProgressChecked { made: false }),
            LadderAction::Continue
        );
        assert_eq!(
            l.step(LadderEvent::Diagnosed {
                has_solution: false
            }),
            LadderAction::Continue
        );
        assert_eq!(l.rung(), Rung::Idle);
        l.step(STALL);
        assert_eq!(
            l.step(LadderEvent::Diagnosed {
                has_solution: false
            }),
            LadderAction::Continue
        );
        assert_eq!(l.rung(), Rung::Nudge);
    }

    #[test]
    fn last_reason_is_remembered() {
        let mut l = EscalationLadder::new(true);
        l.step(LadderEvent::Stalled(StallReason::BudgetLow));
        assert_eq!(l.last_reason(), Some(StallReason::BudgetLow));
        l.step(LadderEvent::Stalled(StallReason::ConsecutiveFailures));
        assert_eq!(l.last_reason(), Some(StallReason::ConsecutiveFailures));
    }

    #[test]
    fn rungs_are_ordered() {
        assert!(Rung::Nudge < Rung::ProgressCheck);
        assert!(Rung::ProgressCheck < Rung::Diagnose);
        assert!(Rung::Diagnose < Rung::ContextReset);
        assert!(Rung::ContextReset < Rung::TierBump);
        assert!(Rung::TierBump < Rung::Escalate);
    }
}
