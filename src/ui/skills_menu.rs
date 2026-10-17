//! `/skills` menu: every discovered skill, grouped by where it was found
//! (nyq, Agents, Claude Code, ...). Typing filters, Tab and Shift+Tab pick a
//! group, Enter puts `/skill-name ` in the editor, → shows the skill, Delete
//! removes one nyq installed (after a confirmation), Ctrl+N starts an
//! install, Esc closes.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::skills::Skill;

pub struct Entry {
    pub skill: Skill,
    /// Which agent's skill root it came from (see [`crate::skills::source`]).
    pub group: &'static str,
    /// In a directory `/skills remove` may delete from.
    pub managed: bool,
    /// The `/skills info` lines, shown with the SKILL.md when viewing it.
    pub info: Vec<String>,
}

pub struct SkillsMenu {
    entries: Vec<Entry>,
    /// The groups that have skills, in [`crate::skills::SOURCES`] order.
    pub groups: Vec<&'static str>,
    pub filter: String,
    /// `None` shows every group.
    pub group: Option<&'static str>,
    pub selected: usize,
    /// The skill (by id) waiting for a remove confirmation.
    pub confirm: Option<String>,
    /// A one-line note above the rows (why a skill cannot be removed).
    pub notice: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    None,
    Close,
    /// Put this text in the editor (a skill invocation, or `/skills install `).
    Insert(String),
    /// Show this skill (by id) full screen.
    View(String),
    /// Run this command (a confirmed `/skills remove ID`).
    Run(String),
}

impl SkillsMenu {
    pub fn new(mut entries: Vec<Entry>) -> Self {
        let order = |g: &str| crate::skills::SOURCES.iter().position(|s| *s == g).unwrap_or(usize::MAX);
        // Grouped by source, project before user within a group, then by name.
        entries.sort_by(|a, b| {
            (order(a.group), a.skill.scope != "project", &a.skill.id).cmp(&(order(b.group), b.skill.scope != "project", &b.skill.id))
        });
        let mut groups: Vec<&'static str> = entries.iter().map(|e| e.group).collect();
        groups.dedup();
        Self { entries, groups, filter: String::new(), group: None, selected: 0, confirm: None, notice: None }
    }

    /// Entries in the chosen group whose name, description, group, scope or
    /// location contain every word of the filter.
    pub fn rows(&self) -> Vec<&Entry> {
        let words: Vec<String> = self.filter.split_whitespace().map(str::to_lowercase).collect();
        self.entries
            .iter()
            .filter(|e| self.group.is_none_or(|g| g == e.group))
            .filter(|e| {
                let s = &e.skill;
                let hay = format!("{} {} {} {} {}", s.id, s.description, e.group, s.scope, s.location).to_lowercase();
                words.iter().all(|w| hay.contains(w.as_str()))
            })
            .collect()
    }

    fn cycle(&mut self, delta: isize) {
        let n = self.groups.len() as isize + 1;
        let now = self.group.and_then(|g| self.groups.iter().position(|x| *x == g)).map_or(0, |i| i as isize + 1);
        let next = (now + delta).rem_euclid(n);
        self.group = (next > 0).then(|| self.groups[next as usize - 1]);
        self.selected = 0;
    }

    pub fn on_key(&mut self, key: KeyEvent, is_command: impl Fn(&str) -> bool) -> Action {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL) && !key.modifiers.contains(KeyModifiers::ALT);
        self.notice = None;
        if let Some(id) = self.confirm.take() {
            return match key.code {
                KeyCode::Enter | KeyCode::Char('y' | 'Y') => Action::Run(format!("/skills remove {id}")),
                _ => Action::None,
            };
        }
        let rows = self.rows();
        let count = rows.len();
        let current = rows.get(self.selected.min(count.saturating_sub(1))).copied();
        match key.code {
            KeyCode::Esc => return Action::Close,
            KeyCode::Char('c') if ctrl => return Action::Close,
            KeyCode::Up => self.selected = self.selected.checked_sub(1).unwrap_or(count.saturating_sub(1)),
            KeyCode::Down => self.selected = if count == 0 { 0 } else { (self.selected + 1) % count },
            KeyCode::Tab => self.cycle(1),
            KeyCode::BackTab => self.cycle(-1),
            KeyCode::Backspace => {
                self.filter.pop();
                self.selected = 0;
            }
            KeyCode::Enter => {
                if let Some(e) = current {
                    // `/name` unless a command has that name; `$name` works anywhere.
                    let id = &e.skill.id;
                    let slash = format!("/{id}");
                    return Action::Insert(if is_command(&slash) { format!("${id} ") } else { format!("{slash} ") });
                }
            }
            KeyCode::Right => {
                if let Some(e) = current {
                    return Action::View(e.skill.id.clone());
                }
            }
            KeyCode::Delete | KeyCode::Char('x') if key.code == KeyCode::Delete || ctrl => match current {
                Some(e) if e.managed => self.confirm = Some(e.skill.id.clone()),
                Some(e) => {
                    self.notice = Some(format!(
                        "{} is in {} ({}); nyq only removes skills it installed. Delete it there.",
                        e.skill.id, e.skill.location, e.group
                    ))
                }
                None => {}
            },
            KeyCode::Char('n') if ctrl => return Action::Insert("/skills install ".into()),
            KeyCode::Char(ch) if !ch.is_control() && !ctrl && !key.modifiers.contains(KeyModifiers::ALT) => {
                self.filter.push(ch);
                self.selected = 0;
            }
            _ => {}
        }
        Action::None
    }

    /// The entry for `id`.
    pub fn entry(&self, id: &str) -> Option<&Entry> {
        self.entries.iter().find(|e| e.skill.id == id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn menu() -> SkillsMenu {
        let t = tempfile::tempdir().unwrap();
        let (cwd, home) = (t.path().join("proj"), t.path().join("home"));
        for (root, name, desc) in [
            (cwd.join(".claude/skills"), "review", "Review a pull request"),
            (cwd.join(".nyq/skills"), "pdf", "Fill PDF forms"),
            (cwd.join(".agents/skills"), "model", "A skill named like a command"),
            (home.join("skills"), "review", "user review"),
        ] {
            std::fs::create_dir_all(root.join(name)).unwrap();
            std::fs::write(root.join(name).join("SKILL.md"), format!("---\nname: {name}\ndescription: {desc}\n---\nbody\n")).unwrap();
        }
        let skills = crate::skills::discover_with(&home, None, &cwd);
        let entries = skills
            .into_iter()
            .map(|skill| Entry { group: crate::skills::source(&skill), managed: skill.location.contains("nyq"), info: Vec::new(), skill })
            .collect();
        SkillsMenu::new(entries)
    }

    fn ids(m: &SkillsMenu) -> Vec<String> {
        m.rows().iter().map(|e| e.skill.id.clone()).collect()
    }

    #[test]
    fn groups_filters_and_runs() {
        let mut m = menu();
        assert_eq!(m.groups, ["nyq", "Agents", "Claude Code"]);
        assert_eq!(ids(&m)[0], "pdf");
        assert_eq!(ids(&m)[1].split('@').next(), Some("review"), "project before user within nyq");
        assert_eq!(ids(&m)[1..3].iter().filter(|i| i.starts_with("review@")).count(), 1);
        m.on_key(key(KeyCode::Tab), |_| false);
        assert_eq!(m.group, Some("nyq"));
        m.on_key(key(KeyCode::Tab), |_| false);
        assert_eq!(ids(&m), ["model"]);
        // A skill named like a command is invoked with `$`.
        assert_eq!(m.on_key(key(KeyCode::Enter), |c| c == "/model"), Action::Insert("$model ".into()));
        m.on_key(key(KeyCode::BackTab), |_| false);
        m.on_key(key(KeyCode::BackTab), |_| false);
        assert_eq!(m.group, None);
        for c in "pdf form".chars() {
            m.on_key(key(KeyCode::Char(c)), |_| false);
        }
        assert_eq!(ids(&m), ["pdf"]);
        assert_eq!(m.on_key(key(KeyCode::Enter), |_| false), Action::Insert("/pdf ".into()));
        assert_eq!(m.on_key(key(KeyCode::Right), |_| false), Action::View("pdf".into()));
        assert_eq!(m.on_key(ctrl('n'), |_| false), Action::Insert("/skills install ".into()));
        assert_eq!(m.on_key(key(KeyCode::Esc), |_| false), Action::Close);
    }

    #[test]
    fn removes_only_managed_skills_after_confirming() {
        let mut m = menu();
        for c in "pull".chars() {
            m.on_key(key(KeyCode::Char(c)), |_| false);
        }
        assert_eq!(ids(&m), ["review"]);
        assert_eq!(m.on_key(key(KeyCode::Delete), |_| false), Action::None);
        assert!(m.confirm.is_none());
        assert!(m.notice.as_deref().unwrap().contains("only removes skills it installed"));
        m.filter.clear();
        m.selected = 0;
        assert_eq!(m.on_key(ctrl('x'), |_| false), Action::None);
        assert_eq!(m.confirm.as_deref(), Some("pdf"));
        // Anything but Enter or y cancels.
        assert_eq!(m.on_key(key(KeyCode::Char('n')), |_| false), Action::None);
        assert!(m.confirm.is_none() && m.filter.is_empty());
        m.on_key(key(KeyCode::Delete), |_| false);
        assert_eq!(m.on_key(key(KeyCode::Char('y')), |_| false), Action::Run("/skills remove pdf".into()));
    }
}
