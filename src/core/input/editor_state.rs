use super::text_boundaries;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InsertResult {
    Inserted,
    Inactive,
    LimitExceeded,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ByteResult {
    Consumed,
    Ignored,
    LimitExceeded,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SelectionRange {
    pub start: usize,
    pub end: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectionEdge {
    Start,
    End,
}

/// Owns the composer text allocation and its layout-independent edit state.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct State {
    pub input: Vec<u8>,
    pub cursor: usize,
    pub selection_anchor: Option<usize>,
}

impl State {
    pub fn deinit(&mut self) {
        *self = State::default();
    }

    pub fn set_text(&mut self, text: &[u8]) {
        self.input.clear();
        self.cursor = 0;
        self.selection_anchor = None;
        self.input.extend_from_slice(text);
        self.cursor = self.input.len();
    }

    pub fn clear_retaining_capacity(&mut self) {
        self.input.clear();
        self.cursor = 0;
        self.selection_anchor = None;
    }

    /// Exchanges owned text storage with `other` and resets transient edit state.
    pub fn swap_input(&mut self, other: &mut Vec<u8>) {
        std::mem::swap(&mut self.input, other);
        self.cursor = self.input.len();
        self.selection_anchor = None;
    }

    pub fn begin_selection(&mut self, raw_offset: usize) -> bool {
        let offset = raw_offset.min(self.input.len());
        let changed = self.selection_anchor != Some(offset) || self.cursor != offset;
        self.selection_anchor = Some(offset);
        self.cursor = offset;
        changed
    }

    pub fn extend_selection(&mut self, raw_offset: usize) -> bool {
        if self.selection_anchor.is_none() {
            return false;
        }
        let offset = raw_offset.min(self.input.len());
        if self.cursor == offset {
            return false;
        }
        self.cursor = offset;
        true
    }

    pub fn finish_selection(&mut self) {
        if self.selection_anchor == Some(self.cursor) {
            self.selection_anchor = None;
        }
    }

    pub fn clear_selection(&mut self) -> bool {
        if self.selection_anchor.is_none() {
            return false;
        }
        self.selection_anchor = None;
        true
    }

    pub fn selection_range(&self) -> Option<SelectionRange> {
        let anchor = self.selection_anchor?;
        if anchor == self.cursor {
            return None;
        }
        Some(SelectionRange {
            start: anchor.min(self.cursor),
            end: anchor.max(self.cursor),
        })
    }

    pub fn selected_text(&self) -> Option<&[u8]> {
        let selection = self.selection_range()?;
        Some(&self.input[selection.start..selection.end])
    }

    pub fn discard_selection(&mut self) -> Option<usize> {
        let dropped_bytes = self.selection_range().map(|s| s.end - s.start);
        self.selection_anchor = None;
        dropped_bytes
    }

    pub fn collapse_selection(&mut self, edge: SelectionEdge) -> bool {
        let selection = match self.selection_range() {
            Some(s) => s,
            None => {
                self.selection_anchor = None;
                return false;
            }
        };
        self.cursor = match edge {
            SelectionEdge::Start => selection.start,
            SelectionEdge::End => selection.end,
        };
        self.selection_anchor = None;
        true
    }

    pub fn set_cursor(&mut self, raw_offset: usize) -> bool {
        let offset = raw_offset.min(self.input.len());
        let changed = self.cursor != offset || self.selection_anchor.is_some();
        self.cursor = offset;
        self.selection_anchor = None;
        changed
    }

    pub fn move_cursor_to(&mut self, raw_offset: usize, extend_selection: bool) -> bool {
        if !extend_selection {
            return self.set_cursor(raw_offset);
        }
        let offset = raw_offset.min(self.input.len());
        if self.selection_anchor.is_none() {
            self.selection_anchor = Some(self.cursor);
        }
        if self.cursor == offset {
            return false;
        }
        self.cursor = offset;
        true
    }

    pub fn select_all(&mut self) -> bool {
        if self.input.is_empty() {
            return self.clear_selection();
        }
        let changed = self.selection_anchor != Some(0) || self.cursor != self.input.len();
        self.selection_anchor = Some(0);
        self.cursor = self.input.len();
        changed
    }

    pub fn move_character_left(&mut self) -> bool {
        if self.collapse_selection(SelectionEdge::Start) {
            return true;
        }
        if self.cursor == 0 {
            return false;
        }
        self.set_cursor(text_boundaries::previous_character_start(
            &self.input,
            self.cursor,
        ))
    }

    pub fn move_character_right(&mut self) -> bool {
        if self.collapse_selection(SelectionEdge::End) {
            return true;
        }
        if self.cursor >= self.input.len() {
            return false;
        }
        self.set_cursor(text_boundaries::next_character_end(
            &self.input,
            self.cursor,
        ))
    }

    pub fn move_home(&mut self) -> bool {
        self.set_cursor(0)
    }

    pub fn move_end(&mut self) -> bool {
        let end = self.input.len();
        self.set_cursor(end)
    }

    pub fn move_line_start(&mut self) -> bool {
        let start = text_boundaries::logical_line_start(&self.input, self.cursor);
        self.set_cursor(start)
    }

    pub fn move_line_end(&mut self) -> bool {
        let end = text_boundaries::logical_line_end(&self.input, self.cursor);
        self.set_cursor(end)
    }

    pub fn insert_slice_assume_capacity(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let cursor = self.cursor;
        self.input.splice(cursor..cursor, bytes.iter().copied());
        self.cursor += bytes.len();
        self.selection_anchor = None;
    }

    pub fn insert_slice(&mut self, bytes: &[u8]) {
        self.insert_slice_assume_capacity(bytes);
    }

    pub fn delete_text_range(&mut self, start: usize, end: usize) -> bool {
        let clamped_start = start.min(self.input.len());
        let clamped_end = end.max(clamped_start).min(self.input.len());
        if clamped_start == clamped_end {
            return false;
        }

        if let Some(anchor) = self.selection_anchor {
            self.selection_anchor = Some(offset_after_delete(anchor, clamped_start, clamped_end));
        }
        delete_range(&mut self.input, &mut self.cursor, clamped_start, clamped_end)
    }

    pub fn replace_byte_before_cursor(&mut self, expected: u8, replacement: u8) -> bool {
        if self.cursor == 0 {
            return false;
        }
        let index = self.cursor - 1;
        if self.input[index] != expected {
            return false;
        }
        self.input[index] = replacement;
        self.selection_anchor = None;
        true
    }
}

fn offset_after_delete(offset: usize, start: usize, end: usize) -> usize {
    if offset >= end {
        offset - (end - start)
    } else if offset > start {
        start
    } else {
        offset
    }
}

pub fn can_insert(current_len: usize, inserted_len: usize, max_len: usize) -> bool {
    if current_len > max_len {
        return false;
    }
    inserted_len <= max_len.saturating_sub(current_len)
}

pub fn can_replace(
    current_len: usize,
    replaced_len: usize,
    inserted_len: usize,
    max_len: usize,
) -> bool {
    debug_assert!(replaced_len <= current_len);
    can_insert(current_len - replaced_len, inserted_len, max_len)
}

pub fn delete_range(
    buffer: &mut Vec<u8>,
    cursor: &mut usize,
    start: usize,
    end: usize,
) -> bool {
    let clamped_start = start.min(buffer.len());
    let clamped_end = end.max(clamped_start).min(buffer.len());
    if clamped_start == clamped_end {
        return false;
    }
    let count = clamped_end - clamped_start;
    buffer.drain(clamped_start..clamped_end);
    if *cursor >= clamped_end {
        *cursor -= count;
    } else if *cursor > clamped_start {
        *cursor = clamped_start;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editor_state_owns_selection_and_unicode_cursor_transitions() {
        let mut state = State::default();
        state.set_text("Ae\u{301}B".as_bytes());
        assert!(state.move_character_left());
        assert_eq!(4, state.cursor);
        assert!(state.move_character_left());
        assert_eq!(1, state.cursor);

        assert!(state.begin_selection(1));
        assert!(state.extend_selection(4));
        assert_eq!("e\u{301}".as_bytes(), state.selected_text().unwrap());
        assert!(state.collapse_selection(SelectionEdge::End));
        assert_eq!(4, state.cursor);
        assert!(state.selection_range().is_none());
    }

    #[test]
    fn collapsing_an_empty_selection_clears_its_transient_anchor() {
        let mut state = State::default();
        state.set_text("abc".as_bytes());
        let _ = state.begin_selection(0);
        assert!(!state.collapse_selection(SelectionEdge::Start));
        assert_eq!(None, state.selection_anchor);

        let _ = state.begin_selection(state.input.len());
        assert!(!state.collapse_selection(SelectionEdge::End));
        assert_eq!(None, state.selection_anchor);
    }

    #[test]
    fn selection_extension_keeps_a_fixed_anchor_while_crossing_it() {
        let mut state = State::default();
        state.set_text("abcd".as_bytes());

        let _ = state.set_cursor(2);
        assert!(state.move_cursor_to(1, true));
        assert_eq!(Some(2), state.selection_anchor);
        assert!(state.move_cursor_to(3, true));
        assert_eq!(Some(2), state.selection_anchor);
        assert_eq!(
            SelectionRange { start: 2, end: 3 },
            state.selection_range().unwrap(),
        );

        assert!(state.move_cursor_to(0, false));
        assert_eq!(None, state.selection_anchor);
        assert!(state.select_all());
        assert_eq!(
            SelectionRange { start: 0, end: 4 },
            state.selection_range().unwrap(),
        );
    }

    #[test]
    fn editor_state_moves_across_logical_lines() {
        let mut state = State::default();
        state.set_text("alpha beta\ngamma".as_bytes());
        assert!(state.move_line_start());
        assert_eq!(11, state.cursor);
        assert!(state.move_home());
        assert_eq!(0, state.cursor);
        assert!(state.move_line_end());
        assert_eq!(10, state.cursor);
    }

    #[test]
    fn editor_state_insertion_and_deletion_keep_cursor_and_selection_consistent() {
        let mut state = State::default();
        state.set_text("alpha beta".as_bytes());
        let _ = state.set_cursor(5);
        state.insert_slice_assume_capacity("!".as_bytes());
        assert_eq!("alpha! beta".as_bytes(), state.input.as_slice());
        assert_eq!(6, state.cursor);

        let _ = state.begin_selection(5);
        let _ = state.extend_selection(7);
        assert!(state.delete_text_range(5, 7));
        assert_eq!("alphabeta".as_bytes(), state.input.as_slice());
        assert_eq!(5, state.cursor);
        assert_eq!(Some(5), state.selection_anchor);
        state.finish_selection();
        assert!(state.selection_range().is_none());
    }

    #[test]
    fn bounded_edit_admission_handles_inserts_and_replacements() {
        assert!(can_insert(4095, 1, 4096));
        assert!(!can_insert(4095, 2, 4096));
        assert!(!can_insert(5000, 0, 4096));
        assert!(can_replace(4094, 8, 10, 4096));
        assert!(!can_replace(4094, 8, 11, 4096));
    }

    #[test]
    fn range_deletion_clamps_offsets_and_preserves_cursor_position() {
        let mut buffer: Vec<u8> = b"alpha beta".to_vec();
        let mut cursor = buffer.len();
        assert!(delete_range(&mut buffer, &mut cursor, 5, 6));
        assert_eq!("alphabeta".as_bytes(), buffer.as_slice());
        assert_eq!(9, cursor);

        cursor = 3;
        assert!(delete_range(&mut buffer, &mut cursor, 1, 5));
        assert_eq!("abeta".as_bytes(), buffer.as_slice());
        assert_eq!(1, cursor);

        assert!(!delete_range(&mut buffer, &mut cursor, 99, 100));
        assert_eq!("abeta".as_bytes(), buffer.as_slice());
    }
}
