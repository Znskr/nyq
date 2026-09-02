use super::text_scalar;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct State {
    pub owner: Option<text_scalar::Owner>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BeginTransition {
    pub next: State,
    pub should_report: bool,
}

pub fn begin(current: State, owner: text_scalar::Owner) -> BeginTransition {
    if current.owner == Some(owner) {
        return BeginTransition {
            next: current,
            should_report: false,
        };
    }
    BeginTransition {
        next: State { owner: Some(owner) },
        should_report: true,
    }
}

pub fn clear() -> State {
    State::default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_limit_rejection_coalesces_only_within_one_owner_episode() {
        let composer = begin(State::default(), text_scalar::Owner::Composer);
        assert!(composer.should_report);
        assert_eq!(Some(text_scalar::Owner::Composer), composer.next.owner);

        let repeated = begin(composer.next, text_scalar::Owner::Composer);
        assert!(!repeated.should_report);
        assert_eq!(composer.next, repeated.next);

        let question = begin(repeated.next, text_scalar::Owner::QuestionFreeform);
        assert!(question.should_report);
        assert_eq!(
            Some(text_scalar::Owner::QuestionFreeform),
            question.next.owner,
        );
    }

    #[test]
    fn input_limit_rejection_clear_starts_a_new_episode() {
        let active = begin(State::default(), text_scalar::Owner::ApprovalAmendment).next;
        assert_eq!(Some(text_scalar::Owner::ApprovalAmendment), active.owner);
        let cleared = clear();
        assert!(cleared.owner.is_none());
        assert!(begin(cleared, text_scalar::Owner::ApprovalAmendment).should_report);
    }
}
