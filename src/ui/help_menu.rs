//! `/help`: every command grouped by category. Tab and Shift+Tab pick a
//! category, typing filters, Enter runs the command (or puts it in the
//! editor when it needs an argument), Esc closes.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::slash::{self, Category};

pub struct Entry {
    pub command: String,
    pub description: String,
    pub category: Category,
}

pub struct HelpMenu {
    entries: Vec<Entry>,
    pub filter: String,
    /// `None` shows every category.
    pub category: Option<Category>,
    pub selected: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    None,
    Close,
    Run(String),
    Insert(String),
}

impl HelpMenu {
    pub fn new(commands: &[(String, String)]) -> Self {
        let mut entries: Vec<Entry> = commands
            .iter()
            .map(|(c, d)| Entry {
                command: c.clone(),
                description: d.clone(),
                category: slash::category(c),
            })
            .collect();
        // Grouped by category, in catalog order within each.
        entries.sort_by_key(|e| Category::ALL.iter().position(|c| *c == e.category));
        Self {
            entries,
            filter: String::new(),
            category: None,
            selected: 0,
        }
    }

    /// Entries in the chosen category whose command, description, category
    /// or aliases contain every word of the filter.
    pub fn rows(&self) -> Vec<&Entry> {
        let words: Vec<String> = self
            .filter
            .split_whitespace()
            .map(str::to_lowercase)
            .collect();
        self.entries
            .iter()
            .filter(|e| self.category.is_none_or(|c| c == e.category))
            .filter(|e| {
                let aliases = slash::aliases(&e.command);
                let hay = format!(
                    "{} {} {} {}",
                    e.command,
                    e.description,
                    e.category.label(),
                    aliases.join(" ")
                )
                .to_lowercase();
                words.iter().all(|w| hay.contains(w.as_str()))
            })
            .collect()
    }

    fn cycle(&mut self, delta: isize) {
        let n = Category::ALL.len() as isize + 1;
        let now = self.category.map_or(0, |c| {
            Category::ALL.iter().position(|x| *x == c).unwrap() as isize + 1
        });
        let next = (now + delta).rem_euclid(n);
        self.category = (next > 0).then(|| Category::ALL[next as usize - 1]);
        self.selected = 0;
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Action {
        let count = self.rows().len();
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL)
            && !key.modifiers.contains(KeyModifiers::ALT);
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
            KeyCode::Tab => self.cycle(1),
            KeyCode::BackTab => self.cycle(-1),
            KeyCode::Backspace => {
                self.filter.pop();
                self.selected = 0;
            }
            KeyCode::Enter => {
                let rows = self.rows();
                if let Some(e) = rows.get(self.selected.min(count.saturating_sub(1))) {
                    let cmd = e.command.clone();
                    return if slash::needs_args(&cmd) {
                        Action::Insert(format!("{cmd} "))
                    } else {
                        Action::Run(cmd)
                    };
                }
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

    fn menu() -> HelpMenu {
        let c = |a: &str, b: &str| (a.to_string(), b.to_string());
        HelpMenu::new(&[
            c("/version", "show nyq version"),
            c("/help", "show commands"),
            c("/usage", "usage totals"),
            c("/image", "attach an image"),
            c("/review", "my template"),
        ])
    }

    #[test]
    fn groups_filters_and_cycles_categories() {
        let mut m = menu();
        let names = |m: &HelpMenu| {
            m.rows()
                .iter()
                .map(|e| e.command.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            names(&m),
            ["/help", "/usage", "/image", "/review", "/version"]
        );
        m.on_key(key(KeyCode::Tab));
        assert_eq!(m.category, Some(Category::General));
        assert_eq!(names(&m), ["/help"]);
        m.on_key(key(KeyCode::BackTab));
        m.on_key(key(KeyCode::BackTab));
        assert_eq!(m.category, Some(Category::Product));
        m.on_key(key(KeyCode::Tab));
        assert_eq!(m.category, None);
        for ch in "cost".chars() {
            m.on_key(key(KeyCode::Char(ch)));
        }
        assert_eq!(names(&m), ["/usage"], "aliases match");
        assert_eq!(m.on_key(key(KeyCode::Enter)), Action::Run("/usage".into()));
        m.filter = "MEDIA attach".into();
        assert_eq!(
            m.on_key(key(KeyCode::Enter)),
            Action::Insert("/image ".into())
        );
        m.filter = "nothing-matches".into();
        assert_eq!(m.on_key(key(KeyCode::Enter)), Action::None);
        assert_eq!(m.on_key(key(KeyCode::Esc)), Action::Close);
    }
}
