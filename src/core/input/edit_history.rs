const MAX_ENTRIES: usize = 100;
const MAX_RETAINED_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub start: usize,
    pub removed: Vec<u8>,
    pub inserted: Vec<u8>,
    pub cursor_before: usize,
    pub cursor_after: usize,
}

impl Entry {
    fn init(
        start: usize,
        removed: &[u8],
        inserted: &[u8],
        cursor_before: usize,
        cursor_after: usize,
    ) -> Entry {
        Entry {
            start,
            removed: removed.to_vec(),
            inserted: inserted.to_vec(),
            cursor_before,
            cursor_after,
        }
    }

    fn retained_bytes(&self) -> usize {
        self.removed.len().saturating_add(self.inserted.len())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Prepared {
    Record(Entry),
    Boundary,
}

impl Prepared {
    pub fn deinit(&mut self) {
        if let Prepared::Record(entry) = self {
            entry.removed.clear();
            entry.inserted.clear();
        }
        *self = Prepared::Boundary;
    }
}

#[derive(Default)]
pub struct State {
    pub undo_stack: Vec<Entry>,
    pub redo_stack: Vec<Entry>,
    pub retained_bytes: usize,
}

impl State {
    pub fn deinit(&mut self) {
        clear_stack(&mut self.retained_bytes, &mut self.undo_stack);
        clear_stack(&mut self.retained_bytes, &mut self.redo_stack);
        *self = State::default();
    }

    pub fn reset(&mut self) {
        clear_stack(&mut self.retained_bytes, &mut self.undo_stack);
        clear_stack(&mut self.retained_bytes, &mut self.redo_stack);
        debug_assert_eq!(self.retained_bytes, 0);
    }

    pub fn prepare(
        &self,
        start: usize,
        removed: &[u8],
        inserted: &[u8],
        cursor_before: usize,
        cursor_after: usize,
    ) -> Prepared {
        let retained = removed.len().saturating_add(inserted.len());
        if retained > MAX_RETAINED_BYTES {
            return Prepared::Boundary;
        }
        Prepared::Record(Entry::init(
            start,
            removed,
            inserted,
            cursor_before,
            cursor_after,
        ))
    }

    pub fn commit(&mut self, prepared: &mut Prepared) {
        match prepared {
            Prepared::Boundary => self.reset(),
            Prepared::Record(_) => {
                let entry = match std::mem::replace(prepared, Prepared::Boundary) {
                    Prepared::Record(entry) => entry,
                    Prepared::Boundary => unreachable!(),
                };
                clear_stack(&mut self.retained_bytes, &mut self.redo_stack);
                self.retained_bytes = self.retained_bytes.saturating_add(entry.retained_bytes());
                self.undo_stack.push(entry);
                while self.undo_stack.len() > MAX_ENTRIES
                    || self.retained_bytes > MAX_RETAINED_BYTES
                {
                    let dropped = self.undo_stack.remove(0);
                    self.retained_bytes =
                        self.retained_bytes.saturating_sub(dropped.retained_bytes());
                }
            }
        }
    }

    pub fn peek_undo(&self) -> Option<&Entry> {
        self.undo_stack.last()
    }

    pub fn peek_redo(&self) -> Option<&Entry> {
        self.redo_stack.last()
    }

    pub fn prepare_undo(&mut self) -> bool {
        !self.undo_stack.is_empty()
    }

    pub fn prepare_redo(&mut self) -> bool {
        !self.redo_stack.is_empty()
    }

    pub fn commit_undo(&mut self) {
        let entry = self.undo_stack.pop().expect("prepare_undo verified capacity");
        self.redo_stack.push(entry);
    }

    pub fn commit_redo(&mut self) {
        let entry = self.redo_stack.pop().expect("prepare_redo verified capacity");
        self.undo_stack.push(entry);
    }
}

fn clear_stack(retained_bytes: &mut usize, stack: &mut Vec<Entry>) {
    for entry in stack.drain(..) {
        *retained_bytes = retained_bytes.saturating_sub(entry.retained_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_edit_history_moves_owned_deltas_between_undo_and_redo() {
        let mut state = State::default();

        let mut prepared = state.prepare(2, "old".as_bytes(), "new".as_bytes(), 5, 5);
        state.commit(&mut prepared);
        assert_eq!("old".as_bytes(), state.peek_undo().unwrap().removed.as_slice());

        assert!(state.prepare_undo());
        state.commit_undo();
        assert!(state.peek_undo().is_none());
        assert_eq!("new".as_bytes(), state.peek_redo().unwrap().inserted.as_slice());

        assert!(state.prepare_redo());
        state.commit_redo();
        assert!(state.peek_redo().is_none());
        assert_eq!(1, state.undo_stack.len());
    }

    #[test]
    fn structured_boundaries_discard_both_edit_directions() {
        let mut state = State::default();

        let mut first = state.prepare(0, "".as_bytes(), "a".as_bytes(), 0, 1);
        state.commit(&mut first);
        assert!(state.prepare_undo());
        state.commit_undo();

        let mut boundary = Prepared::Boundary;
        state.commit(&mut boundary);
        assert!(state.peek_undo().is_none());
        assert!(state.peek_redo().is_none());
        assert_eq!(0, state.retained_bytes);
    }
}
