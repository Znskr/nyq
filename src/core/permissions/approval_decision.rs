//! Approval prompt interaction state: choice selection plus amendment drafts.
//!
//! Ported from `zig-reference/src/core/permissions/approval_decision.zig`.

use crate::core::input::editor_state::{can_insert, InsertResult};
use crate::core::permissions::approval_amendment::State as AmendmentState;
use crate::core::permissions::permission_request::OwnedPermissionResponse;
use crate::core::permissions::permissions::permission_decision_from_index;
use crate::core::shared::types::ToolPermissionDecision;

pub const OPTION_COUNT: u8 = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Move {
    Previous,
    Next,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DraftAction {
    CursorLeft,
    CursorRight,
    CursorHome,
    CursorEnd,
    CursorWordLeft,
    CursorWordRight,
    DeleteNext,
    DeleteWordLeft,
    DeleteWhitespaceWordLeft,
    DeleteWordRight,
    DeleteToLineStart,
    DeleteToLineEnd,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Deny,
    Submit,
    Tab,
    Backspace,
    Number(u8),
    InsertAscii(u8),
    MoveChoice(Move),
    EditDraft(DraftAction),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    None,
    Redraw,
    Decision(ToolPermissionDecision),
    LimitExceeded,
}

pub use crate::core::input::editor_state::InsertResult as ContractInsertResult;

#[derive(Default)]
pub struct State {
    pub choice_index: u8,
    pub confirmation_only: bool,
    pub amendment: AmendmentState,
}

impl State {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn reset(&mut self, reason: &str) {
        self.choice_index = 0;
        self.confirmation_only = false;
        self.amendment.discard(reason);
    }

    pub fn reset_after_submission(&mut self) {
        let selected_choice = self.choice_index;
        self.choice_index = 0;
        self.confirmation_only = false;
        self.amendment.discard_after_submission(selected_choice);
    }

    fn selected_decision(&self) -> ToolPermissionDecision {
        if self.confirmation_only {
            return if self.choice_index == 0 {
                ToolPermissionDecision::Once
            } else {
                ToolPermissionDecision::Deny
            };
        }
        permission_decision_from_index(self.choice_index)
    }

    pub fn set_confirmation_only(&mut self, enabled: bool) {
        self.confirmation_only = enabled;
        if enabled && self.choice_index >= 2 {
            self.choice_index = 0;
        }
    }

    pub fn is_amending(&self) -> bool {
        self.amendment.is_active()
    }

    pub fn can_amend(&self, amendment_allowed: bool) -> bool {
        amendment_allowed && (self.choice_index == 0 || self.choice_index == 2)
    }

    pub fn amendment_choice(&self) -> Option<u8> {
        self.amendment.active_choice()
    }

    pub fn draft_for_choice(&self, choice: u8) -> &[u8] {
        self.amendment.draft_for_choice(choice)
    }

    pub fn draft_cursor_for_choice(&self, choice: u8) -> usize {
        self.amendment.cursor_for_choice(choice)
    }

    pub fn selected_draft(&self) -> &[u8] {
        self.draft_for_choice(self.choice_index)
    }

    pub fn materialize_response(
        &self,
        decision: ToolPermissionDecision,
    ) -> OwnedPermissionResponse {
        let draft = self.selected_draft();
        let feedback = if !draft.is_empty() {
            Some(draft.to_vec())
        } else {
            None
        };
        OwnedPermissionResponse::init(decision, feedback)
    }

    pub fn apply(
        &mut self,
        action: Action,
        amendment_allowed: Option<bool>,
        max_len: Option<usize>,
    ) -> Event {
        match action {
            Action::Deny => {
                self.amendment.discard("ctrl_c");
                Event::Decision(ToolPermissionDecision::Deny)
            }
            Action::Submit => Event::Decision(self.selected_decision()),
            Action::Tab => {
                if self.is_amending() {
                    return Event::Redraw;
                }
                let Some(allowed) = amendment_allowed else {
                    return Event::None;
                };
                if self.can_amend(allowed) {
                    self.amendment.begin(self.choice_index);
                } else {
                    self.move_choice(Move::Next);
                }
                Event::Redraw
            }
            Action::Backspace => {
                if !self.is_amending() {
                    return Event::None;
                }
                self.amendment.backspace();
                Event::Redraw
            }
            Action::Number(choice) => {
                if choice >= self.option_count() {
                    return Event::None;
                }
                if self.is_amending() {
                    return self.insert_ascii(b'1' + choice, max_len);
                }
                self.choice_index = choice;
                Event::Decision(self.selected_decision())
            }
            Action::InsertAscii(byte) => self.insert_ascii(byte, max_len),
            Action::MoveChoice(direction) => {
                self.move_choice(direction);
                Event::Redraw
            }
            Action::EditDraft(edit) => {
                if !self.is_amending() {
                    return Event::None;
                }
                self.apply_draft_action(edit);
                Event::Redraw
            }
        }
    }

    pub fn insert_amendment_slice(&mut self, bytes: &[u8], max_len: usize) -> InsertResult {
        if !self.is_amending() {
            return InsertResult::Inactive;
        }
        if bytes.is_empty() {
            return InsertResult::Inserted;
        }
        let Some(choice) = self.amendment_choice() else {
            return InsertResult::Inactive;
        };
        if !can_insert(self.draft_for_choice(choice).len(), bytes.len(), max_len) {
            return InsertResult::LimitExceeded;
        }
        let _ = self.amendment.insert_slice(bytes);
        InsertResult::Inserted
    }

    fn insert_ascii(&mut self, byte: u8, max_len: Option<usize>) -> Event {
        if !self.is_amending() || !(0x20..=0x7E).contains(&byte) {
            return Event::None;
        }
        if let Some(limit) = max_len {
            return match self.insert_amendment_slice(&[byte], limit) {
                InsertResult::Inserted => Event::Redraw,
                InsertResult::Inactive => Event::None,
                InsertResult::LimitExceeded => Event::LimitExceeded,
            };
        }
        let _ = self.amendment.insert(byte);
        Event::Redraw
    }

    fn move_choice(&mut self, direction: Move) {
        let count = self.option_count();
        let delta: i32 = match direction {
            Move::Previous => -1,
            Move::Next => 1,
        };
        let mut next = self.choice_index as i32 + delta;
        if next < 0 {
            next = count as i32 - 1;
        }
        if next >= count as i32 {
            next = 0;
        }
        self.choice_index = next as u8;
        self.amendment.clear_active();
    }

    fn option_count(&self) -> u8 {
        if self.confirmation_only {
            2
        } else {
            OPTION_COUNT
        }
    }

    fn apply_draft_action(&mut self, action: DraftAction) {
        match action {
            DraftAction::CursorLeft => self.amendment.cursor_left(),
            DraftAction::CursorRight => self.amendment.cursor_right(),
            DraftAction::CursorHome => self.amendment.cursor_home(),
            DraftAction::CursorEnd => self.amendment.cursor_end(),
            DraftAction::CursorWordLeft => self.amendment.cursor_word_left(),
            DraftAction::CursorWordRight => self.amendment.cursor_word_right(),
            DraftAction::DeleteNext => self.amendment.delete_next(),
            DraftAction::DeleteWordLeft => self.amendment.delete_word_left(),
            DraftAction::DeleteWhitespaceWordLeft => {
                self.amendment.delete_whitespace_delimited_word_left()
            }
            DraftAction::DeleteWordRight => self.amendment.delete_word_right(),
            DraftAction::DeleteToLineStart => self.amendment.delete_to_line_start(),
            DraftAction::DeleteToLineEnd => self.amendment.delete_to_line_end(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_selection_and_submission_actions() {
        let mut state = State::new();

        assert_eq!(state.selected_decision(), ToolPermissionDecision::Once);
        assert_eq!(
            state.apply(Action::MoveChoice(Move::Previous), Some(true), None),
            Event::Redraw
        );
        assert_eq!(state.choice_index, 2);
        assert_eq!(
            state.apply(Action::Submit, Some(true), None),
            Event::Decision(ToolPermissionDecision::Deny)
        );
        assert_eq!(
            state.apply(Action::Number(1), Some(true), None),
            Event::Decision(ToolPermissionDecision::Always)
        );
    }

    #[test]
    fn ignores_tab_without_an_active_request() {
        let mut state = State::new();

        assert_eq!(state.apply(Action::Tab, None, None), Event::None);
        assert_eq!(state.choice_index, 0);
    }

    #[test]
    fn preserves_independent_drafts_while_choices_move() {
        let mut state = State::new();

        assert_eq!(state.apply(Action::Tab, Some(true), None), Event::Redraw);
        state.apply(Action::InsertAscii(b'y'), Some(true), None);
        state.apply(Action::MoveChoice(Move::Previous), Some(true), None);
        assert_eq!(state.choice_index, 2);
        state.apply(Action::Tab, Some(true), None);
        state.apply(Action::InsertAscii(b'n'), Some(true), None);

        assert_eq!(state.draft_for_choice(0), b"y");
        assert_eq!(state.draft_for_choice(2), b"n");
        assert_eq!(state.amendment_choice(), Some(2));
    }

    #[test]
    fn bounded_insertion_is_atomic() {
        let mut state = State::new();

        state.apply(Action::Tab, Some(true), None);
        state.apply(Action::InsertAscii(b'a'), Some(true), Some(1));
        assert_eq!(
            state.apply(Action::InsertAscii(b'b'), Some(true), Some(1)),
            Event::LimitExceeded
        );
        assert_eq!(state.selected_draft(), b"a");
    }

    #[test]
    fn inserts_only_printable_ascii_actions() {
        let mut state = State::new();

        state.apply(Action::Tab, Some(true), None);
        assert_eq!(
            state.apply(Action::InsertAscii(0x1F), Some(true), None),
            Event::None
        );
        assert_eq!(
            state.apply(Action::InsertAscii(0x7F), Some(true), None),
            Event::None
        );
        assert_eq!(
            state.apply(Action::InsertAscii(0x80), Some(true), None),
            Event::None
        );
        assert_eq!(state.selected_draft(), b"");
    }

    #[test]
    fn deny_discards_amendment_state() {
        let mut state = State::new();

        state.apply(Action::Tab, Some(true), None);
        state.apply(Action::InsertAscii(b'x'), Some(true), None);
        assert_eq!(
            state.apply(Action::Deny, Some(true), None),
            Event::Decision(ToolPermissionDecision::Deny)
        );
        assert!(!state.is_amending());
        assert_eq!(state.draft_for_choice(0), b"");
    }

    #[test]
    fn materializes_owned_feedback() {
        let mut state = State::new();

        state.apply(Action::Tab, Some(true), None);
        state.apply(Action::InsertAscii(b'x'), Some(true), None);
        let response = state.materialize_response(ToolPermissionDecision::Once);

        assert_eq!(response.decision, ToolPermissionDecision::Once);
        assert_eq!(response.feedback.as_deref(), Some(b"x".as_slice()));
    }

    #[test]
    fn materializes_denial_from_transition() {
        let mut state = State::new();

        let event = state.apply(Action::Deny, Some(true), None);
        let response = state.materialize_response(match event {
            Event::Decision(d) => d,
            _ => panic!("expected decision event"),
        });

        assert_eq!(response.decision, ToolPermissionDecision::Deny);
        assert_eq!(state.choice_index, 0);
    }

    #[test]
    fn confirmation_only_reduces_choices_and_decisions() {
        let mut state = State::new();
        state.set_confirmation_only(true);
        assert_eq!(state.option_count(), 2);

        state.choice_index = 2;
        state.set_confirmation_only(true);
        assert_eq!(state.choice_index, 0);

        assert_eq!(
            state.apply(Action::Submit, None, None),
            Event::Decision(ToolPermissionDecision::Once)
        );
        state.apply(Action::MoveChoice(Move::Next), None, None);
        assert_eq!(state.choice_index, 1);
        assert_eq!(
            state.apply(Action::Submit, None, None),
            Event::Decision(ToolPermissionDecision::Deny)
        );
    }

    #[test]
    fn reset_after_submission_discards_only_unselected_draft() {
        let mut state = State::new();
        state.apply(Action::Tab, Some(true), None);
        state.apply(Action::InsertAscii(b'y'), Some(true), None);
        state.apply(Action::MoveChoice(Move::Previous), Some(true), None);
        state.apply(Action::Tab, Some(true), None);
        state.apply(Action::InsertAscii(b'n'), Some(true), None);

        state.reset_after_submission();
        assert_eq!(state.choice_index, 0);
        assert!(!state.is_amending());
        assert_eq!(state.draft_for_choice(0), b"");
        assert_eq!(state.draft_for_choice(2), b"");
    }
}
