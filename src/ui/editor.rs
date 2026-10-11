//! Multi-line prompt editor: a pure state machine driven by key events.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use unicode_width::UnicodeWidthChar;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Nothing for the app to do (the editor handled the key or ignored it).
    None,
    /// Enter: submit the text (when busy, the app queues it as steering).
    Submit(String),
    /// Alt+Enter while busy: queue the text as a follow-up.
    FollowUp(String),
    /// Esc: interrupt the running turn.
    Interrupt,
    /// Ctrl+C: the editor was cleared if it had text; `exit` is true on a
    /// second Ctrl+C with an empty editor.
    CtrlC { exit: bool },
    /// Ctrl+D on an empty editor.
    Exit,
    /// Alt+Up: move queued messages back into the editor.
    Dequeue,
}

/// A rendered editor: wrapped display lines and the cursor position within them.
pub struct View {
    pub lines: Vec<String>,
    pub cursor_row: usize,
    pub cursor_col: usize,
}

pub struct Editor {
    lines: Vec<String>,
    row: usize,
    col: usize,
    history: Vec<String>,
    browsing: Option<usize>,
    draft: String,
    commands: Vec<(String, String)>,
    empty_ctrl_c: bool,
    /// Ctrl+R reverse search through history, while active.
    search: Option<Search>,
}

struct Search {
    query: String,
    /// History index of the current match.
    at: Option<usize>,
    /// The text from before the search, restored on Esc.
    draft: String,
    failed: bool,
}

impl Default for Editor {
    fn default() -> Self {
        Self::new(Vec::new(), Vec::new())
    }
}

impl Editor {
    pub fn new(history: Vec<String>, commands: Vec<(String, String)>) -> Self {
        Self {
            lines: vec![String::new()],
            row: 0,
            col: 0,
            history,
            browsing: None,
            draft: String::new(),
            commands,
            empty_ctrl_c: false,
            search: None,
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent, busy: bool) -> Action {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return Action::None;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL) && !key.modifiers.contains(KeyModifiers::ALT);
        let alt = key.modifiers.contains(KeyModifiers::ALT) && !key.modifiers.contains(KeyModifiers::CONTROL);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        if let Some(action) = self.search_key(key, ctrl, alt) {
            return action;
        }
        let double = self.empty_ctrl_c;
        self.empty_ctrl_c = false;
        match key.code {
            KeyCode::Char('c') if ctrl => {
                let empty = self.is_empty();
                if empty {
                    self.empty_ctrl_c = true;
                } else {
                    self.set_text("");
                }
                return Action::CtrlC {
                    exit: empty && double,
                };
            }
            KeyCode::Esc => return Action::Interrupt,
            KeyCode::Up if alt => return Action::Dequeue,
            KeyCode::Char('d') if ctrl => {
                if self.is_empty() {
                    return Action::Exit;
                }
                self.delete();
            }
            KeyCode::Enter if alt && busy => return self.submit(true),
            KeyCode::Enter if alt || shift => self.paste("\n"),
            KeyCode::Char('j') if ctrl => self.paste("\n"),
            KeyCode::Enter => {
                if self.lines[self.row].chars().take(self.col).last() == Some('\\') {
                    self.backspace();
                    self.paste("\n");
                } else {
                    return self.submit(false);
                }
            }
            KeyCode::Left if ctrl => self.word_left(),
            KeyCode::Right if ctrl => self.word_right(),
            KeyCode::Char('b') if alt => self.word_left(),
            KeyCode::Char('f') if alt => self.word_right(),
            KeyCode::Backspace if alt => self.delete_word(),
            KeyCode::Char('w') if ctrl => self.delete_word(),
            KeyCode::Char('u') if ctrl => {
                let end = self.byte_col();
                self.lines[self.row].replace_range(..end, "");
                self.col = 0;
            }
            KeyCode::Char('k') if ctrl => {
                let start = self.byte_col();
                self.lines[self.row].truncate(start);
            }
            KeyCode::Home | KeyCode::Char('a') if key.code == KeyCode::Home || ctrl => self.col = 0,
            KeyCode::End | KeyCode::Char('e') if key.code == KeyCode::End || ctrl => {
                self.col = self.len()
            }
            KeyCode::Backspace => self.backspace(),
            KeyCode::Delete => self.delete(),
            KeyCode::Left => self.left(),
            KeyCode::Right => self.right(),
            KeyCode::Up => {
                if self.row > 0 {
                    self.row -= 1;
                    self.col = self.col.min(self.len());
                } else {
                    self.recall(true);
                }
            }
            KeyCode::Down => {
                if self.row + 1 < self.lines.len() {
                    self.row += 1;
                    self.col = self.col.min(self.len());
                } else {
                    self.recall(false);
                }
            }
            KeyCode::Tab => self.complete(),
            KeyCode::Char(c) if !ctrl && !alt && !c.is_control() => self.paste(&c.to_string()),
            _ => {}
        }
        Action::None
    }

    pub fn paste(&mut self, text: &str) {
        self.empty_ctrl_c = false;
        let text = text
            .replace("\r\n", "\n")
            .replace('\r', "\n")
            .replace('\t', "    ");
        let byte = self.byte_col();
        let tail = self.lines[self.row].split_off(byte);
        let parts: Vec<&str> = text.split('\n').collect();
        self.lines[self.row].push_str(parts[0]);
        if parts.len() == 1 {
            self.col += parts[0].chars().count();
        } else {
            for part in &parts[1..] {
                self.row += 1;
                self.lines.insert(self.row, (*part).into());
            }
            self.col = parts.last().unwrap().chars().count();
        }
        self.lines[self.row].push_str(&tail);
    }
    pub fn text(&self) -> String {
        self.lines.join("\n")
    }
    pub fn set_text(&mut self, text: &str) {
        self.lines = text.split('\n').map(str::to_owned).collect();
        self.row = self.lines.len() - 1;
        self.col = self.len();
        self.browsing = None;
        self.empty_ctrl_c = false;
    }
    pub fn is_empty(&self) -> bool {
        self.lines.iter().all(String::is_empty) && self.lines.len() == 1
    }
    /// Record a submitted entry in history (dedup consecutive).
    pub fn push_history(&mut self, entry: &str) {
        if !entry.trim().is_empty() && self.history.last().map(String::as_str) != Some(entry) {
            self.history.push(entry.into());
        }
        self.browsing = None;
    }
    pub fn render(&self, width: usize) -> View {
        let width = width.max(1);
        let mut view = View {
            lines: Vec::new(),
            cursor_row: 0,
            cursor_col: 0,
        };
        for (row, line) in self.lines.iter().enumerate() {
            view.lines.push(String::new());
            let mut col = 0;
            for (i, c) in line.chars().enumerate() {
                let w = c.width().unwrap_or(0);
                if w > 0 && col > 0 && col + w > width {
                    view.lines.push(String::new());
                    col = 0;
                }
                if row == self.row && i == self.col {
                    view.cursor_row = view.lines.len() - 1;
                    view.cursor_col = col;
                }
                view.lines.last_mut().unwrap().push(c);
                col += w;
            }
            if row == self.row && self.col == line.chars().count() {
                if col >= width {
                    view.lines.push(String::new());
                    col = 0;
                }
                view.cursor_row = view.lines.len() - 1;
                view.cursor_col = col;
            }
        }
        view
    }
    /// Slash-command suggestions for the current text: (command, description).
    pub fn suggestions(&self) -> Vec<(String, String)> {
        self.matches().take(8).cloned().collect()
    }

    fn len(&self) -> usize {
        self.lines[self.row].chars().count()
    }
    fn byte_col(&self) -> usize {
        self.lines[self.row]
            .char_indices()
            .nth(self.col)
            .map_or(self.lines[self.row].len(), |(i, _)| i)
    }
    fn left(&mut self) {
        if self.col > 0 {
            self.col -= 1;
        } else if self.row > 0 {
            self.row -= 1;
            self.col = self.len();
        }
    }
    fn right(&mut self) {
        if self.col < self.len() {
            self.col += 1;
        } else if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = 0;
        }
    }
    fn before(&self) -> Option<char> {
        if self.col > 0 {
            self.lines[self.row].chars().nth(self.col - 1)
        } else {
            (self.row > 0).then_some('\n')
        }
    }
    fn after(&self) -> Option<char> {
        self.lines[self.row]
            .chars()
            .nth(self.col)
            .or_else(|| (self.row + 1 < self.lines.len()).then_some('\n'))
    }
    // Words are runs of non-whitespace, including punctuation.
    fn word_left(&mut self) {
        while self.before().is_some_and(char::is_whitespace) {
            self.left();
        }
        while self.before().is_some_and(|c| !c.is_whitespace()) {
            self.left();
        }
    }
    fn word_right(&mut self) {
        while self.after().is_some_and(char::is_whitespace) {
            self.right();
        }
        while self.after().is_some_and(|c| !c.is_whitespace()) {
            self.right();
        }
    }
    fn backspace(&mut self) {
        if self.col > 0 {
            self.left();
            self.delete();
        } else if self.row > 0 {
            let tail = self.lines.remove(self.row);
            self.row -= 1;
            self.col = self.len();
            self.lines[self.row].push_str(&tail);
        }
    }
    fn delete(&mut self) {
        if self.col < self.len() {
            let start = self.byte_col();
            let end = start + self.after().unwrap().len_utf8();
            self.lines[self.row].replace_range(start..end, "");
        } else if self.row + 1 < self.lines.len() {
            let tail = self.lines.remove(self.row + 1);
            self.lines[self.row].push_str(&tail);
        }
    }
    fn delete_word(&mut self) {
        let end = (self.row, self.col);
        self.word_left();
        let start = (self.row, self.col);
        let count = if start.0 == end.0 {
            end.1 - start.1
        } else {
            self.lines[start.0].chars().count() - start.1
                + 1
                + self.lines[start.0 + 1..end.0]
                    .iter()
                    .map(|l| l.chars().count() + 1)
                    .sum::<usize>()
                + end.1
        };
        for _ in 0..count {
            self.delete();
        }
    }
    fn submit(&mut self, follow_up: bool) -> Action {
        let text = self.text();
        if text.trim().is_empty() {
            return Action::None;
        }
        self.set_text("");
        if follow_up {
            Action::FollowUp(text)
        } else {
            Action::Submit(text)
        }
    }
    fn recall(&mut self, older: bool) {
        let next = match (older, self.browsing) {
            (true, None) if !self.history.is_empty() => {
                self.draft = self.text();
                Some(self.history.len() - 1)
            }
            (true, Some(i)) => Some(i.saturating_sub(1)),
            (false, Some(i)) if i + 1 < self.history.len() => Some(i + 1),
            (false, Some(_)) => None,
            _ => return,
        };
        let text = next.map_or_else(|| self.draft.clone(), |i| self.history[i].clone());
        self.set_text(&text);
        self.browsing = next;
    }
    /// Ctrl+R starts (or steps) a reverse history search; typing refines it,
    /// Esc restores the earlier text, any other key keeps the match and acts.
    fn search_key(&mut self, key: KeyEvent, ctrl: bool, alt: bool) -> Option<Action> {
        let Some(search) = &mut self.search else {
            if ctrl && key.code == KeyCode::Char('r') {
                self.search = Some(Search { query: String::new(), at: None, draft: self.text(), failed: false });
                return Some(Action::None);
            }
            return None;
        };
        let older = match key.code {
            KeyCode::Char('r') if ctrl => true,
            KeyCode::Char(c) if !ctrl && !alt && !c.is_control() => {
                search.query.push(c);
                false
            }
            KeyCode::Backspace => {
                search.query.pop();
                false
            }
            KeyCode::Esc | KeyCode::Char('c' | 'g') if key.code == KeyCode::Esc || ctrl => {
                let draft = std::mem::take(&mut search.draft);
                self.search = None;
                self.set_text(&draft);
                return Some(Action::None);
            }
            _ => {
                self.search = None;
                return None;
            }
        };
        let before = if older { search.at.unwrap_or(self.history.len()) } else { self.history.len() };
        let found = (!search.query.is_empty())
            .then(|| self.history[..before].iter().rposition(|h| h.contains(&search.query)))
            .flatten();
        search.failed = found.is_none() && !search.query.is_empty();
        if let Some(i) = found {
            search.at = Some(i);
            let text = self.history[i].clone();
            self.set_text(&text);
        }
        Some(Action::None)
    }

    /// What the status line shows during a Ctrl+R search.
    pub fn search_status(&self) -> Option<String> {
        self.search.as_ref().map(|s| {
            format!(
                "reverse search: {}{}",
                s.query,
                if s.failed { " (no match)" } else { "" }
            )
        })
    }

    /// Replace the commands that come and go with MCP servers (`/mcp:…`).
    pub fn set_mcp_commands(&mut self, extra: Vec<(String, String)>) {
        self.commands.retain(|(name, _)| !name.starts_with("/mcp:"));
        self.commands.extend(extra);
    }
    fn matches(&self) -> impl Iterator<Item = &(String, String)> {
        let prefix = &self.lines[0];
        let eligible = self.lines.len() == 1
            && prefix.starts_with('/')
            && !prefix.chars().any(char::is_whitespace);
        self.commands
            .iter()
            .filter(move |(name, _)| eligible && name.starts_with(prefix))
    }
    fn complete(&mut self) {
        let matches: Vec<_> = self.matches().map(|(name, _)| name.clone()).collect();
        if matches.len() == 1 {
            self.set_text(&format!("{} ", matches[0]));
        } else if let Some(first) = matches.first() {
            let prefix: String = first
                .chars()
                .enumerate()
                .take_while(|(i, c)| matches.iter().all(|s| s.chars().nth(*i) == Some(*c)))
                .map(|(_, c)| c)
                .collect();
            self.set_text(&prefix);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use KeyCode::*;
    use KeyModifiers as M;

    fn key(e: &mut Editor, code: KeyCode, modifiers: M) -> Action {
        e.handle_key(KeyEvent::new(code, modifiers), false)
    }
    fn plain(e: &mut Editor, code: KeyCode) -> Action {
        key(e, code, M::NONE)
    }
    fn ctrl(e: &mut Editor, c: char) -> Action {
        key(e, Char(c), M::CONTROL)
    }
    fn view(e: &Editor, width: usize, lines: &[&str], cursor: (usize, usize)) {
        let v = e.render(width);
        assert_eq!(v.lines, lines);
        assert_eq!((v.cursor_row, v.cursor_col), cursor);
    }

    #[test]
    fn altgr_is_text_even_for_shortcut_letters() {
        let mut e = Editor::default();
        for c in "@\\{}[]|~€cdukjabfl".chars() {
            assert_eq!(e.handle_key(KeyEvent::new(Char(c), M::CONTROL | M::ALT), false), Action::None);
        }
        assert_eq!(e.text(), "@\\{}[]|~€cdukjabfl");
    }

    #[test]
    fn insertion_paste_and_event_kinds() {
        let mut e = Editor::default();
        assert!(e.is_empty());
        view(&e, 10, &[""], (0, 0));
        plain(&mut e, Char('界'));
        key(&mut e, Char('A'), M::SHIFT);
        key(&mut e, Char('x'), M::CONTROL);
        key(&mut e, Char('x'), M::ALT);
        plain(&mut e, Char('\t'));
        e.handle_key(
            KeyEvent::new_with_kind(Char('x'), M::NONE, KeyEventKind::Release),
            false,
        );
        e.handle_key(
            KeyEvent::new_with_kind(Char('🙂'), M::NONE, KeyEventKind::Repeat),
            false,
        );
        assert_eq!(e.text(), "界A🙂");
        plain(&mut e, Left);
        e.paste("a\r\nb\rc\td");
        assert_eq!(e.text(), "界Aa\nb\nc    d🙂");
        assert_eq!((e.row, e.col), (2, 6));
        e.set_text("one\n");
        assert_eq!((e.row, e.col), (1, 0));
        assert!(!e.is_empty());
    }

    #[test]
    fn character_movement_deletion_and_joins() {
        let mut e = Editor::default();
        e.set_text("界🙂\na");
        plain(&mut e, Home);
        plain(&mut e, Left);
        assert_eq!((e.row, e.col), (0, 2));
        plain(&mut e, Right);
        assert_eq!((e.row, e.col), (1, 0));
        plain(&mut e, Backspace);
        assert_eq!(e.text(), "界🙂a");
        assert_eq!(e.col, 2);
        plain(&mut e, Backspace);
        assert_eq!(e.text(), "界a");
        plain(&mut e, Delete);
        assert_eq!(e.text(), "界");
        e.set_text("ab\n界");
        plain(&mut e, Up);
        plain(&mut e, End);
        plain(&mut e, Delete);
        assert_eq!(e.text(), "ab界");
        ctrl(&mut e, 'a');
        plain(&mut e, Left);
        plain(&mut e, Backspace);
        assert_eq!(e.col, 0);
        ctrl(&mut e, 'e');
        plain(&mut e, Right);
        plain(&mut e, Delete);
        assert_eq!(e.col, 3);
        ctrl(&mut e, 'd');
        assert_eq!(e.text(), "ab界");
        plain(&mut e, Home);
        ctrl(&mut e, 'd');
        assert_eq!(e.text(), "b界");
    }

    #[test]
    fn words_and_line_deletion() {
        let mut e = Editor::default();
        e.set_text("one 界🙂  three");
        key(&mut e, Left, M::CONTROL);
        assert_eq!(e.col, 8);
        key(&mut e, Char('b'), M::ALT);
        assert_eq!(e.col, 4);
        key(&mut e, Right, M::CONTROL);
        assert_eq!(e.col, 6);
        key(&mut e, Char('f'), M::ALT);
        assert_eq!(e.col, 13);
        ctrl(&mut e, 'w');
        assert_eq!(e.text(), "one 界🙂  ");
        key(&mut e, Backspace, M::ALT);
        assert_eq!(e.text(), "one ");
        e.set_text("abc界def");
        plain(&mut e, Left);
        plain(&mut e, Left);
        ctrl(&mut e, 'k');
        assert_eq!(e.text(), "abc界d");
        ctrl(&mut e, 'u');
        assert!(e.is_empty());
        e.set_text("one\n   ");
        ctrl(&mut e, 'w');
        assert!(e.is_empty());
        key(&mut e, Left, M::CONTROL);
        key(&mut e, Right, M::CONTROL);
        ctrl(&mut e, 'w');
    }

    #[test]
    fn enter_variants_and_continuation() {
        let mut e = Editor::default();
        e.set_text("  \n ");
        assert_eq!(plain(&mut e, Enter), Action::None);
        assert_eq!(e.text(), "  \n ");
        e.set_text("hello\\tail");
        for _ in 0..4 {
            plain(&mut e, Left);
        }
        assert_eq!(plain(&mut e, Enter), Action::None);
        assert_eq!(e.text(), "hello\ntail");
        assert_eq!((e.row, e.col), (1, 0));
        assert_eq!(plain(&mut e, Enter), Action::Submit("hello\ntail".into()));
        assert!(e.is_empty());
        assert!(e.history.is_empty());
        e.set_text("a\\");
        plain(&mut e, Enter);
        assert_eq!(e.text(), "a\n");
        key(&mut e, Enter, M::SHIFT);
        ctrl(&mut e, 'j');
        key(&mut e, Enter, M::ALT);
        assert_eq!(e.text(), "a\n\n\n\n");
        assert_eq!(
            e.handle_key(KeyEvent::new(Enter, M::ALT), true),
            Action::FollowUp("a\n\n\n\n".into())
        );
        assert!(e.is_empty());
        assert_eq!(
            e.handle_key(KeyEvent::new(Enter, M::ALT), true),
            Action::None
        );
        e.set_text(" ");
        assert_eq!(
            e.handle_key(KeyEvent::new(Enter, M::ALT), true),
            Action::None
        );
        assert_eq!(e.text(), " ");
        e.set_text("busy");
        assert_eq!(
            e.handle_key(KeyEvent::new(Enter, M::NONE), true),
            Action::Submit("busy".into())
        );
    }

    #[test]
    fn history_and_draft_restore() {
        let mut e = Editor::new(vec!["older".into(), "newer".into()], vec![]);
        e.set_text("draft\n界🙂");
        plain(&mut e, Up);
        assert_eq!(e.text(), "draft\n界🙂");
        assert_eq!((e.row, e.col), (0, 2));
        plain(&mut e, Up);
        assert_eq!(e.text(), "newer");
        plain(&mut e, Up);
        plain(&mut e, Up);
        assert_eq!(e.text(), "older");
        plain(&mut e, Down);
        assert_eq!(e.text(), "newer");
        plain(&mut e, Down);
        assert_eq!(e.text(), "draft\n界🙂");
        assert_eq!((e.row, e.col), (1, 2));
        plain(&mut e, Down);
        assert_eq!(e.text(), "draft\n界🙂");
        e.push_history("newer");
        e.push_history(" ");
        assert_eq!(e.history.len(), 2);
        e.push_history("third");
        assert_eq!(e.history.len(), 3);
        let mut empty = Editor::default();
        plain(&mut empty, Up);
        plain(&mut empty, Down);
        assert!(empty.is_empty());
    }

    #[test]
    fn reverse_search_finds_older_matches() {
        let mut e = Editor::new(vec!["cargo build".into(), "ls -la".into(), "cargo test".into()], Vec::new());
        e.paste("draft");
        ctrl(&mut e, 'r');
        assert_eq!(e.search_status().unwrap(), "reverse search: ");
        for c in "cargo".chars() {
            plain(&mut e, Char(c));
        }
        assert_eq!(e.text(), "cargo test");
        ctrl(&mut e, 'r');
        assert_eq!(e.text(), "cargo build");
        plain(&mut e, Char('x'));
        assert_eq!(e.search_status().unwrap(), "reverse search: cargox (no match)");
        assert_eq!(e.text(), "cargo build");
        plain(&mut e, Esc);
        assert_eq!((e.text().as_str(), e.search_status()), ("draft", None));
        // Enter keeps the match and submits it.
        ctrl(&mut e, 'r');
        plain(&mut e, Char('l'));
        plain(&mut e, Char('s'));
        assert_eq!(plain(&mut e, Enter), Action::Submit("ls -la".into()));
        assert!(e.search_status().is_none());
    }

    #[test]
    fn completion_and_suggestions() {
        let mut e = Editor::new(
            vec![],
            vec![
                ("/help".into(), "help".into()),
                ("/hello".into(), "hello".into()),
                ("/new".into(), "new".into()),
            ],
        );
        e.set_text("/h");
        assert_eq!(e.suggestions().len(), 2);
        plain(&mut e, Tab);
        assert_eq!(e.text(), "/hel");
        e.set_text("/help");
        plain(&mut e, Tab);
        assert_eq!(e.text(), "/help ");
        assert!(e.suggestions().is_empty());
        for text in ["/missing", "word", "/h x", "/h\n", "/h\t"] {
            e.set_text(text);
            plain(&mut e, Tab);
            assert_eq!(e.text(), text);
            assert!(e.suggestions().is_empty());
        }
        e.commands = (0..12)
            .map(|i| (format!("/c{i}"), format!("desc{i}")))
            .collect();
        e.set_text("/");
        assert_eq!(e.suggestions(), e.commands[..8]);
        plain(&mut e, Tab);
        assert_eq!(e.text(), "/c");
        e.set_text("/c11");
        plain(&mut e, Tab);
        assert_eq!(e.text(), "/c11 ");
    }

    #[test]
    fn control_actions_and_double_ctrl_c() {
        let mut e = Editor::default();
        assert_eq!(plain(&mut e, Esc), Action::Interrupt);
        assert_eq!(key(&mut e, Up, M::ALT), Action::Dequeue);
        assert_eq!(ctrl(&mut e, 'd'), Action::Exit);
        e.set_text(" ");
        assert_eq!(ctrl(&mut e, 'c'), Action::CtrlC { exit: false });
        assert!(e.is_empty());
        assert_eq!(ctrl(&mut e, 'c'), Action::CtrlC { exit: false });
        assert_eq!(ctrl(&mut e, 'c'), Action::CtrlC { exit: true });
        plain(&mut e, Left);
        assert_eq!(ctrl(&mut e, 'c'), Action::CtrlC { exit: false });
        e.handle_key(
            KeyEvent::new_with_kind(Char('c'), M::CONTROL, KeyEventKind::Release),
            false,
        );
        assert_eq!(ctrl(&mut e, 'c'), Action::CtrlC { exit: true });
        e.paste("");
        assert_eq!(ctrl(&mut e, 'c'), Action::CtrlC { exit: false });
        e.set_text("");
        assert_eq!(ctrl(&mut e, 'c'), Action::CtrlC { exit: false });
    }

    #[test]
    fn wrapping_and_unicode_cursor() {
        let mut e = Editor::default();
        e.set_text("abcd");
        view(&e, 4, &["abcd", ""], (1, 0));
        plain(&mut e, Left);
        view(&e, 4, &["abcd"], (0, 3));
        e.set_text("abc界🙂z");
        view(&e, 4, &["abc", "界🙂", "z"], (2, 1));
        plain(&mut e, Left);
        view(&e, 4, &["abc", "界🙂", "z"], (2, 0));
        plain(&mut e, Left);
        view(&e, 4, &["abc", "界🙂", "z"], (1, 2));
        plain(&mut e, Left);
        view(&e, 4, &["abc", "界🙂", "z"], (1, 0));
        e.set_text("界🙂\nx");
        view(&e, 2, &["界", "🙂", "x"], (2, 1));
        view(&e, 1, &["界", "🙂", "x", ""], (3, 0));
        e.set_text("a\u{301}b");
        view(&e, 1, &["a\u{301}", "b", ""], (2, 0));
        e.set_text("ab\nc");
        plain(&mut e, Up);
        plain(&mut e, End);
        view(&e, 2, &["ab", "", "c"], (1, 0));
    }
    #[test]
    fn vertical_clamping_and_cursor_at_every_wrap_boundary() {
        let mut e = Editor::default();
        e.set_text("long line\nx\nlast");
        plain(&mut e, Up);
        assert_eq!((e.row, e.col), (1, 1));
        plain(&mut e, Up);
        assert_eq!((e.row, e.col), (0, 1));
        plain(&mut e, End);
        plain(&mut e, Down);
        assert_eq!((e.row, e.col), (1, 1));
        plain(&mut e, Down);
        assert_eq!((e.row, e.col), (2, 1));

        e.set_text("a界🙂b");
        for (col, expected) in [
            (0, (0, 0)),
            (1, (0, 1)),
            (2, (1, 0)),
            (3, (1, 2)),
            (4, (2, 0)),
        ] {
            e.col = col;
            let v = e.render(3);
            assert_eq!((v.cursor_row, v.cursor_col), expected);
            assert_eq!(
                v.lines,
                if col == 4 {
                    vec!["a界", "🙂b", ""]
                } else {
                    vec!["a界", "🙂b"]
                }
            );
        }
    }
}
