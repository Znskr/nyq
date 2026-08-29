#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Owner {
    Composer,
    QuestionFreeform,
    ApprovalAmendment,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Scalar {
    pub owner: Owner,
    pub bytes: [u8; 4],
    pub len: usize,
}

impl Scalar {
    pub fn slice(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DropReason {
    InvalidUtf8,
    OwnerChanged,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Drop {
    pub reason: DropReason,
    pub owner: Owner,
    pub bytes: usize,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Step {
    pub scalar: Option<Scalar>,
    pub dropped: Option<Drop>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct State {
    pub owner: Option<Owner>,
    pub bytes: [u8; 4],
    pub len: usize,
    pub expected_len: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Transition {
    pub next: State,
    pub step: Step,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pending {
    pub owner: Owner,
    pub bytes: usize,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ResetTransition {
    pub next: State,
    pub dropped: Option<Pending>,
}

pub fn has_pending(state: State) -> bool {
    state.len > 0
}

pub fn reset(state: State) -> ResetTransition {
    ResetTransition {
        dropped: state.owner.map(|owner| Pending {
            owner,
            bytes: state.len,
        }),
        ..ResetTransition::default()
    }
}

pub fn advance(state: State, owner: Owner, byte: u8) -> Transition {
    if state.owner.is_some_and(|current| current != owner) {
        let restarted = advance(State::default(), owner, byte);
        let mut step = restarted.step;
        let restarted_drop_bytes = step.dropped.map_or(0, |drop| drop.bytes);
        step.dropped = Some(Drop {
            reason: DropReason::OwnerChanged,
            owner: state.owner.expect("owner was checked above"),
            bytes: state.len + restarted_drop_bytes,
        });
        return Transition {
            next: restarted.next,
            step,
        };
    }

    if state.len == 0 {
        let expected_len = match utf8_sequence_len(byte) {
            Some(len) => len,
            None => {
                return Transition {
                    next: State::default(),
                    step: Step {
                        dropped: Some(Drop {
                            reason: DropReason::InvalidUtf8,
                            owner,
                            bytes: 1,
                        }),
                        ..Step::default()
                    },
                };
            }
        };
        let mut bytes = [0; 4];
        bytes[0] = byte;
        if expected_len == 1 {
            return Transition {
                next: State::default(),
                step: Step {
                    scalar: Some(Scalar {
                        owner,
                        bytes,
                        len: 1,
                    }),
                    ..Step::default()
                },
            };
        }
        return Transition {
            next: State {
                owner: Some(owner),
                bytes,
                len: 1,
                expected_len,
            },
            step: Step::default(),
        };
    }

    if byte & 0xc0 != 0x80 {
        let restarted = advance(State::default(), owner, byte);
        let mut step = restarted.step;
        let restarted_drop_bytes = step.dropped.map_or(0, |drop| drop.bytes);
        step.dropped = Some(Drop {
            reason: DropReason::InvalidUtf8,
            owner,
            bytes: state.len + restarted_drop_bytes,
        });
        return Transition {
            next: restarted.next,
            step,
        };
    }

    let mut next = state;
    next.bytes[next.len] = byte;
    next.len += 1;
    if next.len < next.expected_len {
        return Transition {
            next,
            step: Step::default(),
        };
    }

    if core::str::from_utf8(&next.bytes[..next.len]).is_err() {
        return Transition {
            next: State::default(),
            step: Step {
                dropped: Some(Drop {
                    reason: DropReason::InvalidUtf8,
                    owner,
                    bytes: next.len,
                }),
                ..Step::default()
            },
        };
    }
    Transition {
        next: State::default(),
        step: Step {
            scalar: Some(Scalar {
                owner,
                bytes: next.bytes,
                len: next.len,
            }),
            ..Step::default()
        },
    }
}

fn utf8_sequence_len(byte: u8) -> Option<usize> {
    match byte {
        0x00..=0x7f => Some(1),
        0xc0..=0xdf => Some(2),
        0xe0..=0xef => Some(3),
        0xf0..=0xf7 => Some(4),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_scalar_admission_completes_utf8_atomically() {
        let mut transition = advance(State::default(), Owner::Composer, 0xe2);
        assert!(transition.step.scalar.is_none());
        assert!(transition.step.dropped.is_none());
        transition = advance(transition.next, Owner::Composer, 0x82);
        assert!(transition.step.scalar.is_none());
        transition = advance(transition.next, Owner::Composer, 0xac);
        let scalar = transition.step.scalar.expect("complete scalar");
        assert_eq!(scalar.slice(), "€".as_bytes());
        assert_eq!(scalar.owner, Owner::Composer);
        assert!(!has_pending(transition.next));
    }

    #[test]
    fn text_scalar_admission_rejects_invalid_utf8_without_retaining_bytes() {
        let transition = advance(State::default(), Owner::Composer, 0xe2);
        let transition = advance(transition.next, Owner::Composer, 0x28);
        assert_eq!(transition.step.scalar.unwrap().slice(), b"(");
        assert_eq!(
            transition.step.dropped.unwrap().reason,
            DropReason::InvalidUtf8
        );
        assert_eq!(transition.step.dropped.unwrap().bytes, 1);
        assert!(!has_pending(transition.next));
    }

    #[test]
    fn text_scalar_admission_drops_an_incomplete_scalar_when_its_owner_changes() {
        let transition = advance(State::default(), Owner::Composer, 0xe2);
        let transition = advance(transition.next, Owner::QuestionFreeform, 0xc3);
        let dropped = transition.step.dropped.unwrap();
        assert_eq!(dropped.reason, DropReason::OwnerChanged);
        assert_eq!(dropped.owner, Owner::Composer);
        assert_eq!(dropped.bytes, 1);
        assert_eq!(transition.next.owner, Some(Owner::QuestionFreeform));
        assert_eq!(transition.next.len, 1);
    }

    #[test]
    fn text_scalar_reset_reports_pending_ownership_without_mutating_input() {
        let pending = advance(State::default(), Owner::ApprovalAmendment, 0xe2).next;
        let transition = reset(pending);
        assert_eq!(transition.dropped.unwrap().owner, Owner::ApprovalAmendment);
        assert_eq!(transition.dropped.unwrap().bytes, 1);
        assert!(!has_pending(transition.next));
    }
}
