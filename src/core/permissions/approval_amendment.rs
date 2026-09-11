//! Approval amendment drafts for the two editable permission choices.
//!
//! Ported from `zig-reference/src/core/permissions/approval_amendment.zig`.

use crate::core::input::editor_state::delete_range;
use crate::core::input::text_boundaries::{
    logical_line_end, logical_line_start, next_character_end, next_word_delete_end, next_word_end,
    previous_character_start, previous_whitespace_delimited_token_start, previous_word_start,
};
use crate::core::shared::debug_trace;

/// Per-choice amendable draft state (0 = allow, 2 = deny).
#[derive(Default)]
pub struct State {
    active_choice: Option<u8>,
    yes_draft: Vec<u8>,
    yes_cursor: usize,
    no_draft: Vec<u8>,
    no_cursor: usize,
}

impl State {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn begin(&mut self, choice: u8) {
        if Self::slot_is_yes(choice).is_some() {
            self.active_choice = Some(choice);
        }
    }

    pub fn is_active(&self) -> bool {
        self.active_choice.is_some()
    }

    pub fn active_choice(&self) -> Option<u8> {
        self.active_choice
    }

    pub fn draft_for_choice(&self, choice: u8) -> &[u8] {
        match choice {
            0 => &self.yes_draft,
            2 => &self.no_draft,
            _ => b"",
        }
    }

    pub fn cursor_for_choice(&self, choice: u8) -> usize {
        match choice {
            0 => self.yes_cursor,
            2 => self.no_cursor,
            _ => 0,
        }
    }

    pub fn insert(&mut self, byte: u8) -> Result<(), std::convert::Infallible> {
        if byte >= 0x80 {
            return Ok(());
        }
        self.insert_slice(&[byte])
    }

    pub fn insert_slice(&mut self, bytes: &[u8]) -> Result<(), std::convert::Infallible> {
        debug_assert!(std::str::from_utf8(bytes).is_ok());
        let Some((draft, cursor)) = self.active_parts_mut() else {
            return Ok(());
        };
        draft.splice(*cursor..*cursor, bytes.iter().copied());
        *cursor += bytes.len();
        Ok(())
    }

    pub fn backspace(&mut self) {
        let Some((draft, cursor)) = self.active_parts_mut() else {
            return;
        };
        if *cursor == 0 {
            return;
        }
        let start = previous_character_start(draft, *cursor);
        delete_range(draft, cursor, start, *cursor);
    }

    pub fn cursor_left(&mut self) {
        let Some((draft, cursor)) = self.active_parts_mut() else {
            return;
        };
        if *cursor > 0 {
            *cursor = previous_character_start(draft, *cursor);
        }
    }

    pub fn cursor_right(&mut self) {
        let Some((draft, cursor)) = self.active_parts_mut() else {
            return;
        };
        if *cursor < draft.len() {
            *cursor = next_character_end(draft, *cursor);
        }
    }

    pub fn cursor_home(&mut self) {
        if let Some((_, cursor)) = self.active_parts_mut() {
            *cursor = 0;
        }
    }

    pub fn cursor_end(&mut self) {
        if let Some((draft, cursor)) = self.active_parts_mut() {
            *cursor = draft.len();
        }
    }

    pub fn cursor_word_left(&mut self) {
        let Some((draft, cursor)) = self.active_parts_mut() else {
            return;
        };
        *cursor = previous_word_start(draft, *cursor);
    }

    pub fn cursor_word_right(&mut self) {
        let Some((draft, cursor)) = self.active_parts_mut() else {
            return;
        };
        *cursor = next_word_end(draft, *cursor);
    }

    pub fn delete_next(&mut self) {
        let Some((draft, cursor)) = self.active_parts_mut() else {
            return;
        };
        if *cursor >= draft.len() {
            return;
        }
        let end = next_character_end(draft, *cursor);
        delete_range(draft, cursor, *cursor, end);
    }

    pub fn delete_word_left(&mut self) {
        let Some((draft, cursor)) = self.active_parts_mut() else {
            return;
        };
        let start = previous_word_start(draft, *cursor);
        delete_range(draft, cursor, start, *cursor);
    }

    pub fn delete_whitespace_delimited_word_left(&mut self) {
        let Some((draft, cursor)) = self.active_parts_mut() else {
            return;
        };
        let line_start = logical_line_start(draft, *cursor);
        let start = previous_whitespace_delimited_token_start(draft, *cursor, line_start);
        delete_range(draft, cursor, start, *cursor);
    }

    pub fn delete_word_right(&mut self) {
        let Some((draft, cursor)) = self.active_parts_mut() else {
            return;
        };
        let end = next_word_delete_end(draft, *cursor);
        delete_range(draft, cursor, *cursor, end);
    }

    pub fn delete_to_line_start(&mut self) {
        let Some((draft, cursor)) = self.active_parts_mut() else {
            return;
        };
        let start = logical_line_start(draft, *cursor);
        delete_range(draft, cursor, start, *cursor);
    }

    pub fn delete_to_line_end(&mut self) {
        let Some((draft, cursor)) = self.active_parts_mut() else {
            return;
        };
        let line_end = logical_line_end(draft, *cursor);
        let end = if *cursor == line_end && line_end < draft.len() {
            line_end + 1
        } else {
            line_end
        };
        delete_range(draft, cursor, *cursor, end);
    }

    /// Single mapping from an amendable choice (0 = allow, 2 = deny) to the
    /// draft and cursor it owns. Every edit path dispatches through here so
    /// the two choices cannot drift.
    fn slot_is_yes(choice: u8) -> Option<bool> {
        match choice {
            0 => Some(true),
            2 => Some(false),
            _ => None,
        }
    }

    fn active_is_yes(&self) -> Option<bool> {
        Self::slot_is_yes(self.active_choice?)
    }

    fn active_parts_mut(&mut self) -> Option<(&mut Vec<u8>, &mut usize)> {
        match self.active_is_yes()? {
            true => Some((&mut self.yes_draft, &mut self.yes_cursor)),
            false => Some((&mut self.no_draft, &mut self.no_cursor)),
        }
    }

    pub fn clear_active(&mut self) {
        self.active_choice = None;
    }

    pub fn discard(&mut self, reason: &str) {
        Self::discard_draft(&mut self.yes_draft, "yes", reason);
        Self::discard_draft(&mut self.no_draft, "no", reason);
        self.active_choice = None;
        self.yes_cursor = 0;
        self.no_cursor = 0;
    }

    pub fn discard_after_submission(&mut self, selected_choice: u8) {
        if selected_choice == 0 {
            self.yes_draft.clear();
        } else {
            Self::discard_draft(&mut self.yes_draft, "yes", "accepted_submission_unselected");
        }
        if selected_choice == 2 {
            self.no_draft.clear();
        } else {
            Self::discard_draft(&mut self.no_draft, "no", "accepted_submission_unselected");
        }
        self.active_choice = None;
        self.yes_cursor = 0;
        self.no_cursor = 0;
    }

    fn discard_draft(draft: &mut Vec<u8>, choice: &str, reason: &str) {
        if !draft.is_empty() {
            debug_trace::logf(
                "permission",
                format_args!(
                    "approval feedback discarded reason={reason} choice={choice} bytes={}",
                    draft.len()
                ),
            );
        }
        draft.clear();
    }
}

/// Drop-in replacement for the allocator-taking deinit in the Zig source.
impl Drop for State {
    fn drop(&mut self) {
        self.discard("deinit");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes every test that touches the global trace state.
    static TRACE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn keeps_separate_drafts_and_discards_them_together() {
        let mut amendment = State::new();

        amendment.begin(0);
        amendment.insert(b'r').unwrap();
        amendment.insert(b'e').unwrap();
        assert_eq!(amendment.draft_for_choice(0), b"re");

        amendment.begin(2);
        amendment.insert(b'n').unwrap();
        amendment.insert(b'o').unwrap();
        assert_eq!(amendment.draft_for_choice(0), b"re");
        assert_eq!(amendment.draft_for_choice(2), b"no");

        amendment.discard("test");
        assert_eq!(amendment.active_choice(), None);
        assert_eq!(amendment.draft_for_choice(0), b"");
        assert_eq!(amendment.draft_for_choice(2), b"");
    }

    #[test]
    fn edits_both_choices_identically_and_clamps_at_draft_bounds() {
        let mut amendment = State::new();

        amendment.insert(b'x').unwrap();
        amendment.backspace();
        amendment.cursor_left();
        amendment.cursor_right();
        assert_eq!(amendment.draft_for_choice(0), b"");
        assert_eq!(amendment.draft_for_choice(2), b"");

        amendment.begin(1);
        assert_eq!(amendment.active_choice(), None);

        for choice in [0u8, 2] {
            amendment.begin(choice);
            amendment.insert(b'a').unwrap();
            amendment.insert(b'c').unwrap();
            amendment.cursor_left();
            amendment.insert(b'b').unwrap();
            amendment.cursor_right();
            amendment.backspace();
            amendment.cursor_left();
            amendment.cursor_left();
            amendment.cursor_left();
            amendment.backspace();
            amendment.cursor_right();
            assert_eq!(amendment.draft_for_choice(choice), b"ab");
            assert_eq!(amendment.cursor_for_choice(choice), 1);
        }
        assert_eq!(amendment.draft_for_choice(0), b"ab");
        assert_eq!(amendment.cursor_for_choice(0), 1);
    }

    #[test]
    fn edits_terminal_characters_and_navigation_keys() {
        let mut amendment = State::new();

        amendment.begin(0);
        amendment.insert_slice("A🙂B".as_bytes()).unwrap();
        amendment.cursor_left();
        amendment.cursor_left();
        assert_eq!(amendment.cursor_for_choice(0), 1);
        amendment.cursor_right();
        assert_eq!(amendment.cursor_for_choice(0), 5);
        amendment.backspace();
        assert_eq!(amendment.draft_for_choice(0), "AB".as_bytes());
        assert_eq!(amendment.cursor_for_choice(0), 1);

        amendment.discard("test_reset");
        amendment.begin(2);
        amendment.insert_slice("alpha beta 👨‍👩‍👧‍👦".as_bytes()).unwrap();
        amendment.cursor_word_left();
        assert_eq!(amendment.cursor_for_choice(2), "alpha beta ".len());
        amendment.delete_next();
        assert_eq!(amendment.draft_for_choice(2), "alpha beta ".as_bytes());
        amendment.cursor_home();
        assert_eq!(amendment.cursor_for_choice(2), 0);
        amendment.cursor_word_right();
        assert_eq!(amendment.cursor_for_choice(2), "alpha".len());
        amendment.cursor_end();
        assert_eq!(amendment.cursor_for_choice(2), amendment.draft_for_choice(2).len());
    }

    #[test]
    fn applies_word_and_logical_line_deletions_to_either_draft() {
        let mut amendment = State::new();

        for choice in [0u8, 2] {
            amendment.begin(choice);
            amendment.insert_slice(b"alpha beta").unwrap();
            amendment.delete_whitespace_delimited_word_left();
            assert_eq!(amendment.draft_for_choice(choice), b"alpha ");
            assert_eq!(amendment.cursor_for_choice(choice), 6);

            amendment.delete_word_left();
            assert_eq!(amendment.draft_for_choice(choice), b"");
            assert_eq!(amendment.cursor_for_choice(choice), 0);

            amendment.insert_slice(b"one two\nthree four").unwrap();
            amendment.cursor_home();
            amendment.delete_word_right();
            assert_eq!(amendment.draft_for_choice(choice), b"two\nthree four");

            amendment.cursor_end();
            amendment.cursor_word_left();
            amendment.delete_to_line_start();
            assert_eq!(amendment.draft_for_choice(choice), b"two\nfour");
            assert_eq!(amendment.cursor_for_choice(choice), "two\n".len());

            amendment.delete_to_line_end();
            assert_eq!(amendment.draft_for_choice(choice), b"two\n");
        }

        amendment.discard("test_reset");
        amendment.begin(0);
        amendment.insert_slice(b"alpha\nbeta").unwrap();
        amendment.cursor_home();
        amendment.cursor_word_right();
        amendment.delete_to_line_end();
        assert_eq!(amendment.draft_for_choice(0), b"alphabeta");
        assert_eq!(amendment.cursor_for_choice(0), "alpha".len());
    }

    #[test]
    fn deletion_keeps_terminal_characters_intact() {
        let mut amendment = State::new();

        amendment.begin(0);
        amendment.insert_slice("one e\u{301}lan 🙂".as_bytes()).unwrap();
        amendment.delete_word_left();
        assert_eq!(amendment.draft_for_choice(0), "one e\u{301}lan ".as_bytes());
        amendment.delete_word_left();
        assert_eq!(amendment.draft_for_choice(0), "one ".as_bytes());
    }

    #[test]
    fn traces_discarded_drafts_with_their_reason() {
        let _lock = TRACE_LOCK.lock().unwrap();
        let trace_path = std::env::temp_dir().join(format!(
            "approval-feedback-{}.log",
            crate::core::shared::io::nano_timestamp()
        ));

        debug_trace::reset_for_test();
        debug_trace::configure_for_test_with_scopes(&trace_path, "permission").unwrap();

        let mut amendment = State::new();
        amendment.begin(0);
        amendment.insert(b'y').unwrap();
        amendment.begin(2);
        amendment.insert(b'n').unwrap();
        amendment.discard("cancelled");
        drop(amendment);

        debug_trace::shutdown();
        let trace = std::fs::read_to_string(&trace_path).unwrap_or_default();
        assert!(trace.contains(
            "approval feedback discarded reason=cancelled choice=yes bytes=1"
        ));
        assert!(trace.contains(
            "approval feedback discarded reason=cancelled choice=no bytes=1"
        ));
        debug_trace::reset_for_test();
    }

    #[test]
    fn traces_the_unselected_draft_after_submission() {
        let _lock = TRACE_LOCK.lock().unwrap();
        let trace_path = std::env::temp_dir().join(format!(
            "approval-feedback-accepted-{}.log",
            crate::core::shared::io::nano_timestamp()
        ));

        debug_trace::reset_for_test();
        debug_trace::configure_for_test_with_scopes(&trace_path, "permission").unwrap();

        let mut amendment = State::new();
        amendment.begin(0);
        amendment.insert(b'y').unwrap();
        amendment.begin(2);
        amendment.insert(b'n').unwrap();
        amendment.discard_after_submission(0);
        drop(amendment);

        debug_trace::shutdown();
        let trace = std::fs::read_to_string(&trace_path).unwrap_or_default();
        assert!(trace.contains(
            "approval feedback discarded reason=accepted_submission_unselected choice=no bytes=1"
        ));
        debug_trace::reset_for_test();
    }
}
