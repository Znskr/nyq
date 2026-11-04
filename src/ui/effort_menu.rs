//! Thinking-level picker: only the active model's supported efforts, plus auto.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

pub struct EffortMenu {
    levels: Vec<String>,
    pub filter: String,
    pub selected: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    None,
    Close,
    Select(String),
}

impl EffortMenu {
    pub fn new(efforts: &[String], current: Option<&str>) -> Self {
        let mut levels = vec!["auto".to_string()];
        for effort in efforts {
            if !levels.contains(effort) {
                levels.push(effort.clone());
            }
        }
        let selected = levels
            .iter()
            .position(|e| e == current.unwrap_or("auto"))
            .unwrap_or(0);
        Self {
            levels,
            filter: String::new(),
            selected,
        }
    }

    pub fn rows(&self) -> Vec<&str> {
        let query = self.filter.to_ascii_lowercase();
        self.levels
            .iter()
            .filter(|e| e.to_ascii_lowercase().contains(&query))
            .map(String::as_str)
            .collect()
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Action {
        let rows = self.rows();
        let count = rows.len();
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => return Action::Close,
            KeyCode::Char('c') if ctrl => return Action::Close,
            KeyCode::Up => {
                self.selected = self
                    .selected
                    .checked_sub(1)
                    .unwrap_or(count.saturating_sub(1))
            }
            KeyCode::Down => {
                self.selected = if count == 0 {
                    0
                } else {
                    (self.selected + 1) % count
                }
            }
            KeyCode::Enter => {
                if let Some(level) = rows.get(self.selected.min(count.saturating_sub(1))) {
                    return Action::Select((*level).to_string());
                }
            }
            KeyCode::Backspace => {
                self.filter.pop();
                self.selected = 0;
            }
            KeyCode::Char(ch)
                if !ch.is_control() && !ctrl && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                self.filter.push(ch);
                self.selected = 0;
            }
            _ => {}
        }
        Action::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn selects_current_filters_and_cancels() {
        let mut menu = EffortMenu::new(
            &["low".into(), "high".into(), "provider-next".into()],
            Some("high"),
        );
        assert_eq!(menu.rows(), ["auto", "low", "high", "provider-next"]);
        assert_eq!(
            menu.on_key(key(KeyCode::Enter)),
            Action::Select("high".into())
        );
        menu.on_key(key(KeyCode::Down));
        assert_eq!(
            menu.on_key(key(KeyCode::Enter)),
            Action::Select("provider-next".into())
        );
        menu.on_key(key(KeyCode::Down));
        assert_eq!(
            menu.on_key(key(KeyCode::Enter)),
            Action::Select("auto".into())
        );
        menu.on_key(key(KeyCode::Char('h')));
        assert_eq!(menu.rows(), ["high"]);
        assert_eq!(
            menu.on_key(key(KeyCode::Enter)),
            Action::Select("high".into())
        );
        menu.on_key(key(KeyCode::Char('z')));
        assert_eq!(menu.on_key(key(KeyCode::Enter)), Action::None);
        assert_eq!(menu.on_key(key(KeyCode::Esc)), Action::Close);
    }

    #[test]
    fn unsupported_models_only_offer_auto() {
        let mut menu = EffortMenu::new(&[], Some("high"));
        assert_eq!(menu.rows(), ["auto"]);
        menu.on_key(key(KeyCode::Up));
        assert_eq!(
            menu.on_key(key(KeyCode::Enter)),
            Action::Select("auto".into())
        );
    }
}
