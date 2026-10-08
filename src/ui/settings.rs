//! The `/settings` menu (and `/statusline`, which opens it on the status
//! line tab): rows with their current value, changed in place.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::settings::{self as cfg, Category, Id, Live, Settings, Spec};

pub struct SettingsMenu {
    pub category: Category,
    pub filter: String,
    pub selected: usize,
}

#[derive(Debug, PartialEq)]
pub enum MenuAction {
    None,
    Close,
    /// Set this setting to this value.
    Change(Id, String),
    /// The model row: open the model switcher.
    OpenModels,
}

impl SettingsMenu {
    pub fn new(category: Category) -> Self {
        Self { category, filter: String::new(), selected: 0 }
    }

    pub fn items(&self, s: &Settings, live: &Live) -> Vec<Spec> {
        cfg::filtered(self.category, &self.filter, s, live)
    }

    fn step_category(&mut self, delta: isize) {
        let all = Category::ALL;
        let i = all.iter().position(|c| *c == self.category).unwrap_or(0) as isize;
        self.category = all[(i + delta).rem_euclid(all.len() as isize) as usize];
        self.selected = 0;
    }

    pub fn on_key(&mut self, key: KeyEvent, s: &Settings, live: &Live) -> MenuAction {
        let items = self.items(s, live);
        let count = items.len();
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL) && !key.modifiers.contains(KeyModifiers::ALT);
        let change = |delta: i32, selected: usize| match items.get(selected.min(count.saturating_sub(1))) {
            Some(spec) if spec.id == Id::Model => MenuAction::OpenModels,
            Some(spec) => cfg::cycle(spec.id, s, live, delta).map_or(MenuAction::None, |v| MenuAction::Change(spec.id, v)),
            None => MenuAction::None,
        };
        match key.code {
            KeyCode::Esc => return MenuAction::Close,
            KeyCode::Char('c') if ctrl => return MenuAction::Close,
            KeyCode::Up => self.selected = self.selected.checked_sub(1).unwrap_or(count.saturating_sub(1)),
            KeyCode::Down => self.selected = if count == 0 { 0 } else { (self.selected + 1) % count },
            KeyCode::Tab => self.step_category(1),
            KeyCode::BackTab => self.step_category(-1),
            KeyCode::Enter | KeyCode::Right => return change(1, self.selected),
            KeyCode::Left => return change(-1, self.selected),
            KeyCode::Backspace => {
                self.filter.pop();
                self.selected = 0;
            }
            KeyCode::Char(c) if !ctrl && !c.is_control() => {
                self.filter.push(c);
                self.selected = 0;
            }
            _ => {}
        }
        MenuAction::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::Segment;

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    #[test]
    fn navigates_and_changes() {
        let s = Settings::default();
        let live = Live { model: "m".into(), efforts: vec!["low".into()], ..Live::default() };
        let mut m = SettingsMenu::new(Category::All);
        assert_eq!(m.on_key(key(KeyCode::Enter), &s, &live), MenuAction::OpenModels);
        assert_eq!(m.on_key(key(KeyCode::Down), &s, &live), MenuAction::None);
        assert_eq!(m.on_key(key(KeyCode::Enter), &s, &live), MenuAction::Change(Id::Effort, "low".into()));
        assert_eq!(m.on_key(key(KeyCode::Left), &s, &live), MenuAction::Change(Id::Effort, "low".into()));
        m.on_key(key(KeyCode::Tab), &s, &live);
        assert_eq!(m.category, Category::StatusLine);
        assert_eq!(m.on_key(key(KeyCode::Enter), &s, &live), MenuAction::Change(Id::Segment(Segment::Model), "off".into()));
        m.on_key(key(KeyCode::BackTab), &s, &live);
        m.on_key(key(KeyCode::BackTab), &s, &live);
        assert_eq!(m.category, Category::Advanced);
        for c in "sound".chars() {
            m.on_key(key(KeyCode::Char(c)), &s, &live);
        }
        m.category = Category::All;
        assert_eq!(m.items(&s, &live)[0].id, Id::Sound);
        assert_eq!(m.on_key(key(KeyCode::Esc), &s, &live), MenuAction::Close);
    }
}
