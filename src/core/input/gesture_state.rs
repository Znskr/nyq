pub const ctrl_c_exit_window_ms: i64 = 3000;
pub const escape_clear_window_ms: i64 = 500;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct State {
    pub ctrl_c_exit_armed_ms: Option<i64>,
    pub escape_clear_armed_ms: Option<i64>,
}

impl State {
    pub fn ctrl_c_exit_armed(self) -> bool {
        self.ctrl_c_exit_armed_ms.is_some()
    }

    pub fn ctrl_c_exit_armed_at(self) -> Option<i64> {
        self.ctrl_c_exit_armed_ms
    }

    pub fn escape_clear_armed(self) -> bool {
        self.escape_clear_armed_ms.is_some()
    }

    pub fn escape_clear_armed_at(self) -> Option<i64> {
        self.escape_clear_armed_ms
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PressResult {
    Armed,
    Activated,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PressTransition {
    pub next: State,
    pub result: PressResult,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClearTransition {
    pub next: State,
    pub cleared: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResetTransition {
    pub next: State,
    pub cleared_ctrl_c_exit: bool,
    pub cleared_escape_clear: bool,
}

impl ResetTransition {
    pub fn cleared_any(self) -> bool {
        self.cleared_ctrl_c_exit || self.cleared_escape_clear
    }
}

pub fn press_ctrl_c_exit(current: State, now_ms: i64) -> PressTransition {
    if let Some(armed_ms) = current.ctrl_c_exit_armed_ms {
        if now_ms - armed_ms < ctrl_c_exit_window_ms {
            return PressTransition {
                next: current,
                result: PressResult::Activated,
            };
        }
    }

    let mut next = current;
    next.ctrl_c_exit_armed_ms = Some(now_ms);
    PressTransition {
        next,
        result: PressResult::Armed,
    }
}

pub fn press_escape_clear(current: State, now_ms: i64) -> PressTransition {
    if let Some(armed_ms) = current.escape_clear_armed_ms {
        if now_ms - armed_ms <= escape_clear_window_ms {
            let mut next = current;
            next.escape_clear_armed_ms = None;
            return PressTransition {
                next,
                result: PressResult::Activated,
            };
        }
    }

    let mut next = current;
    next.escape_clear_armed_ms = Some(now_ms);
    PressTransition {
        next,
        result: PressResult::Armed,
    }
}

pub fn disarm_ctrl_c_exit(current: State) -> ClearTransition {
    let mut next = current;
    next.ctrl_c_exit_armed_ms = None;
    ClearTransition {
        next,
        cleared: current.ctrl_c_exit_armed_ms.is_some(),
    }
}

pub fn disarm_escape_clear(current: State) -> ClearTransition {
    let mut next = current;
    next.escape_clear_armed_ms = None;
    ClearTransition {
        next,
        cleared: current.escape_clear_armed_ms.is_some(),
    }
}

pub fn expire_ctrl_c_exit(current: State, now_ms: i64) -> ClearTransition {
    let armed_ms = match current.ctrl_c_exit_armed_ms {
        Some(ms) => ms,
        None => {
            return ClearTransition {
                next: current,
                cleared: false,
            }
        }
    };
    if now_ms - armed_ms < ctrl_c_exit_window_ms {
        return ClearTransition {
            next: current,
            cleared: false,
        };
    }
    disarm_ctrl_c_exit(current)
}

pub fn expire_escape_clear(current: State, now_ms: i64) -> ClearTransition {
    let armed_ms = match current.escape_clear_armed_ms {
        Some(ms) => ms,
        None => {
            return ClearTransition {
                next: current,
                cleared: false,
            }
        }
    };
    if now_ms - armed_ms <= escape_clear_window_ms {
        return ClearTransition {
            next: current,
            cleared: false,
        };
    }
    disarm_escape_clear(current)
}

pub fn reset(current: State) -> ResetTransition {
    ResetTransition {
        next: State::default(),
        cleared_ctrl_c_exit: current.ctrl_c_exit_armed_ms.is_some(),
        cleared_escape_clear: current.escape_clear_armed_ms.is_some(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ctrl_c_exit_activates_only_strictly_inside_its_window() {
        let armed = press_ctrl_c_exit(State::default(), 100);
        assert_eq!(PressResult::Armed, armed.result);
        assert_eq!(Some(100), armed.next.ctrl_c_exit_armed_at());

        let activated = press_ctrl_c_exit(armed.next, 100 + ctrl_c_exit_window_ms - 1);
        assert_eq!(PressResult::Activated, activated.result);
        assert_eq!(armed.next, activated.next);

        let rearmed = press_ctrl_c_exit(armed.next, 100 + ctrl_c_exit_window_ms);
        assert_eq!(PressResult::Armed, rearmed.result);
        assert_eq!(
            Some(100 + ctrl_c_exit_window_ms),
            rearmed.next.ctrl_c_exit_armed_at(),
        );
    }

    #[test]
    fn escape_clear_activates_through_its_inclusive_window() {
        let armed = press_escape_clear(State::default(), 200);
        assert_eq!(PressResult::Armed, armed.result);
        assert_eq!(Some(200), armed.next.escape_clear_armed_at());

        let activated = press_escape_clear(armed.next, 200 + escape_clear_window_ms);
        assert_eq!(PressResult::Activated, activated.result);
        assert!(!activated.next.escape_clear_armed());

        let rearmed = press_escape_clear(armed.next, 201 + escape_clear_window_ms);
        assert_eq!(PressResult::Armed, rearmed.result);
        assert_eq!(
            Some(201 + escape_clear_window_ms),
            rearmed.next.escape_clear_armed_at(),
        );
    }

    #[test]
    fn gesture_expiry_preserves_boundary_behavior() {
        let ctrl_c = press_ctrl_c_exit(State::default(), 300).next;
        assert!(!expire_ctrl_c_exit(ctrl_c, 300 + ctrl_c_exit_window_ms - 1).cleared);
        assert!(expire_ctrl_c_exit(ctrl_c, 300 + ctrl_c_exit_window_ms).cleared);

        let escape = press_escape_clear(State::default(), 400).next;
        assert!(!expire_escape_clear(escape, 400 + escape_clear_window_ms).cleared);
        assert!(expire_escape_clear(escape, 401 + escape_clear_window_ms).cleared);
    }

    #[test]
    fn gesture_disarm_and_reset_report_exactly_what_they_clear() {
        let ctrl_c = press_ctrl_c_exit(State::default(), 10).next;
        let both = press_escape_clear(ctrl_c, 20).next;

        let escape_cleared = disarm_escape_clear(both);
        assert!(escape_cleared.cleared);
        assert!(escape_cleared.next.ctrl_c_exit_armed());
        assert!(!escape_cleared.next.escape_clear_armed());

        let reset_transition = reset(both);
        assert!(reset_transition.cleared_any());
        assert!(reset_transition.cleared_ctrl_c_exit);
        assert!(reset_transition.cleared_escape_clear);
        assert_eq!(State::default(), reset_transition.next);

        assert!(!reset(State::default()).cleared_any());
    }
}
