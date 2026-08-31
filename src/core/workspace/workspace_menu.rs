//! Compact workspace menu state for toggling saved additional roots.

use crate::core::workspace::workspace_access;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Action {
    Add,
    Remove(usize),
    Clear,
}

#[derive(Default)]
pub struct State {
    pub active: bool,
    pub selected_index: usize,
}

impl State {
    pub fn open(&mut self) {
        *self = State {
            active: true,
            selected_index: 0,
        };
    }

    pub fn close(&mut self) {
        *self = State::default();
    }

    pub fn action_count(entries: &[workspace_access::Entry]) -> usize {
        let saved_count = saved_entry_count(entries);
        1 + saved_count + usize::from(saved_count > 0)
    }

    pub fn row_count(entries: &[workspace_access::Entry]) -> usize {
        1 + entries.len() + usize::from(saved_entry_count(entries) > 0)
    }

    pub fn r#move(&mut self, delta: i32, entries: &[workspace_access::Entry]) -> bool {
        let count = State::action_count(entries);
        if !self.active || delta == 0 || count == 0 {
            return false;
        }
        let count_signed = count as i32;
        let mut next = (self.selected_index % count) as i32;
        next += delta;
        while next < 0 {
            next += count_signed;
        }
        while next >= count_signed {
            next -= count_signed;
        }
        self.selected_index = next as usize;
        true
    }

    pub fn selected_action(
        &self,
        entries: &[workspace_access::Entry],
    ) -> Option<Action> {
        if !self.active {
            return None;
        }
        let count = State::action_count(entries);
        if count == 0 {
            return None;
        }
        let selected = self.selected_index % count;
        if selected == 0 {
            return Some(Action::Add);
        }

        let mut saved_index: usize = 1;
        for (entry_index, entry) in entries.iter().enumerate() {
            if !entry.saved {
                continue;
            }
            if selected == saved_index {
                return Some(Action::Remove(entry_index));
            }
            saved_index += 1;
        }
        Some(Action::Clear)
    }

    pub fn selected_row(&self, entries: &[workspace_access::Entry]) -> Option<usize> {
        if !self.active {
            return None;
        }
        let selected = self.selected_index % State::action_count(entries);
        if selected == 0 {
            return Some(0);
        }

        let mut saved_index: usize = 1;
        for (entry_index, entry) in entries.iter().enumerate() {
            if !entry.saved {
                continue;
            }
            if selected == saved_index {
                return Some(entry_index + 1);
            }
            saved_index += 1;
        }
        Some(entries.len() + 1)
    }
}

fn saved_entry_count(entries: &[workspace_access::Entry]) -> usize {
    entries.iter().filter(|entry| entry.saved).count()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &[u8], saved: bool, command_line: bool) -> workspace_access::Entry {
        workspace_access::Entry {
            path: path.to_vec(),
            saved,
            command_line,
            available: true,
            active: true,
        }
    }

    #[test]
    fn workspace_menu_maps_compact_rows_to_existing_command_actions() {
        let entries = [
            entry(b"/tmp/saved-a", true, false),
            entry(b"/tmp/saved-b", true, false),
        ];
        let mut state = State::default();
        state.open();

        assert_eq!(Some(Action::Add), state.selected_action(&entries));
        assert!(state.r#move(1, &entries));
        assert_eq!(Some(Action::Remove(0)), state.selected_action(&entries));
        assert!(state.r#move(1, &entries));
        assert_eq!(Some(Action::Remove(1)), state.selected_action(&entries));
        assert!(state.r#move(1, &entries));
        assert_eq!(Some(Action::Clear), state.selected_action(&entries));
        assert!(state.r#move(1, &entries));
        assert_eq!(Some(Action::Add), state.selected_action(&entries));

        state.open();
        assert_eq!(1, State::action_count(&[]));
        assert_eq!(Some(Action::Add), state.selected_action(&[]));
    }

    #[test]
    fn workspace_menu_shows_launch_roots_without_offering_invalid_remove_actions() {
        let entries = [
            entry(b"/tmp/launch-only", false, true),
            entry(b"/tmp/saved", true, false),
        ];
        let mut state = State::default();
        state.open();

        assert_eq!(3, State::action_count(&entries));
        assert_eq!(4, State::row_count(&entries));
        assert!(state.r#move(1, &entries));
        assert_eq!(Some(Action::Remove(1)), state.selected_action(&entries));
        assert_eq!(Some(2), state.selected_row(&entries));
        assert!(state.r#move(1, &entries));
        assert_eq!(Some(Action::Clear), state.selected_action(&entries));
    }
}
