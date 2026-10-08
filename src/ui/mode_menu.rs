//! `/permissions` mode switcher. Same shape as the other menus: filter,
//! arrow keys, Enter to choose, Esc to close.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

pub struct Row {
    pub id: &'static str,
    pub label: &'static str,
    pub detail: &'static str,
}

const ROWS: &[Row] = &[
    Row {
        id: "ask",
        label: "Ask",
        detail: "Ask before risky tool calls",
    },
    Row {
        id: "auto",
        label: "Auto",
        detail: "Classifier allows, asks, or denies",
    },
    Row {
        id: "yolo",
        label: "Yolo",
        detail: "Never ask; deny rules still block",
    },
    Row {
        id: "rules",
        label: "Rules",
        detail: "List rules and session grants",
    },
];

pub struct ModeMenu {
    pub filter: String,
    pub selected: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    None,
    Close,
    Select(&'static str),
}

impl ModeMenu {
    pub fn new() -> Self {
        Self {
            filter: String::new(),
            selected: 0,
        }
    }

    pub fn rows(&self) -> Vec<&'static Row> {
        let q = self.filter.to_ascii_lowercase();
        ROWS.iter()
            .filter(|row| {
                q.is_empty()
                    || row.id.contains(q.as_str())
                    || row.label.to_ascii_lowercase().contains(&q)
                    || row.detail.to_ascii_lowercase().contains(&q)
            })
            .collect()
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Action {
        let rows = self.rows();
        let count = rows.len();
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL)
            && !key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Esc => return Action::Close,
            KeyCode::Char('c') if ctrl => return Action::Close,
            KeyCode::Up => {
                self.selected = self
                    .selected
                    .checked_sub(1)
                    .unwrap_or(count.saturating_sub(1));
            }
            KeyCode::Down => {
                self.selected = if count == 0 {
                    0
                } else {
                    (self.selected + 1) % count
                }
            }
            KeyCode::Backspace => {
                self.filter.pop();
                self.selected = 0;
            }
            KeyCode::Enter => {
                if let Some(row) = rows.get(self.selected.min(count.saturating_sub(1))) {
                    return Action::Select(row.id);
                }
            }
            KeyCode::Char(ch) if text_key(key, ch) => {
                self.filter.push(ch);
                self.selected = 0;
            }
            _ => {}
        }
        Action::None
    }
}

fn text_key(key: KeyEvent, ch: char) -> bool {
    !ch.is_control()
        && !key.modifiers.contains(KeyModifiers::CONTROL)
        && !key.modifiers.contains(KeyModifiers::ALT)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn filters_and_selects_a_mode() {
        let mut menu = ModeMenu::new();
        assert_eq!(menu.rows().len(), 4);
        assert_eq!(menu.on_key(key(KeyCode::Char('y'))), Action::None);
        assert_eq!(menu.on_key(key(KeyCode::Char('o'))), Action::None);
        assert_eq!(menu.rows().len(), 1);
        assert_eq!(menu.rows()[0].id, "yolo");
        assert_eq!(menu.on_key(key(KeyCode::Enter)), Action::Select("yolo"));
        menu.on_key(key(KeyCode::Backspace));
        menu.on_key(key(KeyCode::Backspace));
        assert_eq!(menu.rows().len(), 4);
        menu.on_key(key(KeyCode::Down));
        menu.on_key(key(KeyCode::Down));
        assert_eq!(menu.on_key(key(KeyCode::Enter)), Action::Select("yolo"));
        assert_eq!(menu.on_key(key(KeyCode::Esc)), Action::Close);
    }
}
