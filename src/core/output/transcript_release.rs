#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Policy {
    FinalityGated,
    AppendOnly,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ToolTurnFloor {
    pub turn_id: u64,
    pub start_byte: usize,
}

/// Activity-independent finality boundaries for one prepared transcript flow.
/// `tool_turn_floors` is owned when populated.
#[derive(Debug, Default)]
pub struct Candidates {
    pub mutation_pin_start: Option<usize>,
    pub assistant_tail_start: Option<usize>,
    pub tool_turn_floors: Vec<ToolTurnFloor>,
}

impl Candidates {
    pub fn deinit(&mut self) {
        self.tool_turn_floors.clear();
        *self = Candidates::default();
    }

    pub fn clone(&self) -> Candidates {
        Candidates {
            mutation_pin_start: self.mutation_pin_start,
            assistant_tail_start: self.assistant_tail_start,
            tool_turn_floors: self.tool_turn_floors.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct State {
    pub policy: Policy,
    pub assistant_tail_writable: bool,
}

impl Default for State {
    fn default() -> Self {
        State {
            policy: Policy::FinalityGated,
            assistant_tail_writable: true,
        }
    }
}

impl State {
    pub fn with_assistant_tail_writable(self, writable: bool) -> State {
        let mut next = self;
        next.assistant_tail_writable = writable;
        next
    }

    /// Returns the earliest byte that is not final, or null when the entire
    /// prepared flow may enter native terminal history.
    pub fn finality_floor(
        self,
        candidates: Candidates,
        finalized_tool_turn_watermark: u64,
        replaceable_start: Option<usize>,
    ) -> Option<usize> {
        if self.policy == Policy::AppendOnly {
            return None;
        }

        let mut floor = candidates.mutation_pin_start;
        for turn_floor in &candidates.tool_turn_floors {
            if turn_floor.turn_id <= finalized_tool_turn_watermark {
                continue;
            }
            floor = Some(earlier(floor, turn_floor.start_byte));
        }
        if let Some(start) = replaceable_start {
            floor = Some(earlier(floor, start));
        }
        if self.assistant_tail_writable {
            if let Some(start) = candidates.assistant_tail_start {
                floor = Some(earlier(floor, start));
            }
        }
        floor
    }
}

fn earlier(current: Option<usize>, candidate: usize) -> usize {
    match current {
        Some(value) => value.min(candidate),
        None => candidate,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcript_release_chooses_the_earliest_mutable_boundary() {
        let tool_turns = vec![
            ToolTurnFloor {
                turn_id: 4,
                start_byte: 48,
            },
            ToolTurnFloor {
                turn_id: 6,
                start_byte: 24,
            },
        ];
        let candidates = Candidates {
            mutation_pin_start: Some(32),
            assistant_tail_start: Some(16),
            tool_turn_floors: tool_turns,
        };

        assert_eq!(
            Some(16),
            State::default().finality_floor(candidates, 4, Some(40))
        );
    }

    #[test]
    fn transcript_release_ignores_final_producers_and_preserves_append_only_output() {
        let tool_turns = vec![ToolTurnFloor {
            turn_id: 4,
            start_byte: 8,
        }];
        let candidates = Candidates {
            mutation_pin_start: None,
            assistant_tail_start: Some(12),
            tool_turn_floors: tool_turns,
        };

        assert_eq!(
            None,
            State {
                assistant_tail_writable: false,
                ..Default::default()
            }
            .finality_floor(candidates.clone(), 4, None)
        );
        assert_eq!(
            None,
            State {
                policy: Policy::AppendOnly,
                ..Default::default()
            }
            .finality_floor(candidates, 0, Some(3))
        );
    }

    #[test]
    fn assistant_tail_writability_returns_a_new_state() {
        let open = State::default();
        let closed = open.with_assistant_tail_writable(false);
        assert!(open.assistant_tail_writable);
        assert!(!closed.assistant_tail_writable);
        assert!(closed.with_assistant_tail_writable(true).assistant_tail_writable);
    }
}
