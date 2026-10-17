//! Multi-line prompt editor: a pure state machine driven by key events.

use std::cell::Cell;
use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use unicode_width::UnicodeWidthChar;

use super::keys::{Context, KeyAction, Keymap};

/// Pastes longer than this (in characters) fold into a placeholder.
const FOLD_PASTE: usize = 1000;
const MAX_UNDO: usize = 200;
const MAX_KILLS: usize = 16;

/// (row, column in characters).
type Pos = (usize, usize);

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
    /// Alt+Up: move a queued message back into the editor.
    Dequeue,
    /// Copy (or cut) the selection: put this text on the clipboard.
    Copy(String),
}

/// What the previous key did, for grouping undo steps and appending kills.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Edit {
    Other,
    Typing,
    Kill,
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
    /// The entries in `commands` that extensions registered.
    ext_commands: Vec<String>,
    /// User-invocable skills: (name, description), offered as `$name`.
    skills: Vec<(String, String)>,
    empty_ctrl_c: bool,
    /// Ctrl+R reverse search through history, while active.
    search: Option<Search>,
    /// Esc on an idle, non-empty draft: a second Esc clears it.
    esc_armed: bool,
    undo: Vec<(Vec<String>, Pos)>,
    redo: Vec<(Vec<String>, Pos)>,
    last: Edit,
    kills: Vec<String>,
    /// The other end of the selection (Shift+movement), when there is one.
    anchor: Option<Pos>,
    /// Preferred display column while moving up and down.
    goal: Option<usize>,
    /// Wrap width of the last render, for moving by display rows.
    width: Cell<usize>,
    /// Folded pastes: (placeholder, text), expanded on submit.
    pastes: Vec<(String, String)>,
    keys: Arc<Keymap>,
}

struct Search {
    query: String,
    /// History index of the current match.
    at: Option<usize>,
    /// The text from before the search, restored on Esc.
    draft: String,
    failed: bool,
}

/// One row of the `/` (or `$`) popup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Suggestion {
    pub label: String,
    pub detail: String,
    pub tag: String,
    /// The editor text once this is chosen (without a trailing space).
    pub fill: String,
    /// Enter runs `fill`; otherwise Enter puts it in the editor to type more.
    pub run: bool,
    /// An argument of the command already typed (not a command or skill).
    pub arg: bool,
}

impl Default for Editor {
    fn default() -> Self {
        Self::new(Vec::new(), Vec::new())
    }
}

fn byte(line: &str, col: usize) -> usize {
    line.char_indices().nth(col).map_or(line.len(), |(i, _)| i)
}

fn is_image_token(s: &str) -> bool {
    s.strip_prefix("[Image #")
        .and_then(|r| r.strip_suffix(']'))
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
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
            ext_commands: Vec::new(),
            skills: Vec::new(),
            empty_ctrl_c: false,
            search: None,
            esc_armed: false,
            undo: Vec::new(),
            redo: Vec::new(),
            last: Edit::Other,
            kills: Vec::new(),
            anchor: None,
            goal: None,
            width: Cell::new(usize::MAX),
            pastes: Vec::new(),
            keys: super::keys::current(),
        }
    }

    /// Use `keys` for the editing actions.
    #[cfg(test)]
    pub fn set_keys(&mut self, keys: Arc<Keymap>) {
        self.keys = keys;
    }

    pub fn handle_key(&mut self, key: KeyEvent, busy: bool) -> Action {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return Action::None;
        }
        let m = key.modifiers;
        let (ctrl, alt, shift, sup) = (
            m.contains(KeyModifiers::CONTROL) && !m.contains(KeyModifiers::ALT),
            m.contains(KeyModifiers::ALT) && !m.contains(KeyModifiers::CONTROL),
            m.contains(KeyModifiers::SHIFT),
            m.contains(KeyModifiers::SUPER),
        );
        if let Some(action) = self.search_key(key, ctrl, alt) {
            return action;
        }
        // Shift+movement extends the selection, so it moves like the plain key.
        let moving = matches!(
            key.code,
            KeyCode::Left | KeyCode::Right | KeyCode::Up | KeyCode::Down | KeyCode::Home | KeyCode::End
        );
        let selecting = shift && moving;
        let mut bound = key;
        if selecting {
            bound.modifiers -= KeyModifiers::SHIFT;
        }
        let act = self.keys.action(Context::Editor, &bound);
        if let Some(a @ (KeyAction::Undo | KeyAction::Redo)) = act {
            self.undo_step(a == KeyAction::Redo);
            return Action::None;
        }
        let before = (self.lines.clone(), self.pos());
        let prev = std::mem::replace(&mut self.last, Edit::Other);
        let action = self.dispatch(key, act, busy, prev, selecting, (ctrl, alt, sup));
        if self.lines != before.0 {
            // Typing a word is one undo step.
            if !(prev == Edit::Typing && self.last == Edit::Typing) {
                self.record(before);
            }
        }
        action
    }

    /// Act on `key`, which the keymap bound to `act` (if anything).
    fn dispatch(
        &mut self,
        key: KeyEvent,
        act: Option<KeyAction>,
        busy: bool,
        prev: Edit,
        selecting: bool,
        (ctrl, alt, sup): (bool, bool, bool),
    ) -> Action {
        let double = std::mem::take(&mut self.empty_ctrl_c);
        let esc = std::mem::take(&mut self.esc_armed);
        let goal = self.goal.take();
        if selecting {
            self.anchor.get_or_insert(self.pos());
        } else if let Some(sel) = self.selection() {
            match key.code {
                KeyCode::Char('c') if ctrl || sup => {
                    self.anchor = None;
                    return Action::Copy(self.slice(sel.0, sel.1));
                }
                KeyCode::Char('x') if ctrl || sup => {
                    self.anchor = None;
                    return Action::Copy(self.remove(sel.0, sel.1));
                }
                KeyCode::Backspace | KeyCode::Delete => {
                    self.anchor = None;
                    self.remove(sel.0, sel.1);
                    return Action::None;
                }
                _ if act == Some(KeyAction::Interrupt) && !busy => {
                    self.anchor = None;
                    return Action::None;
                }
                KeyCode::Char(c) if !ctrl && !alt && !sup && !c.is_control() => {
                    self.anchor = None;
                    self.remove(sel.0, sel.1);
                }
                _ => self.anchor = None,
            }
        } else {
            self.anchor = None;
        }
        let Some(act) = act else {
            match key.code {
                KeyCode::Char('a') if sup => {
                    self.anchor = Some((0, 0));
                    self.jump(false);
                }
                KeyCode::Backspace => self.backspace(),
                KeyCode::Delete => self.delete(),
                KeyCode::Char(c) if !ctrl && !alt && !sup && !c.is_control() => {
                    self.insert(&c.to_string());
                    if !c.is_whitespace() {
                        self.last = Edit::Typing;
                    }
                }
                _ => {}
            }
            return Action::None;
        };
        match act {
            KeyAction::Clear => {
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
            KeyAction::Interrupt => {
                if !busy && esc && !self.is_empty() {
                    self.set_text("");
                    return Action::None;
                }
                self.esc_armed = !busy && !self.is_empty();
                return Action::Interrupt;
            }
            KeyAction::Dequeue => return Action::Dequeue,
            KeyAction::Exit => {
                if self.is_empty() {
                    return Action::Exit;
                }
                self.delete();
            }
            KeyAction::FollowUp if busy => return self.submit(true),
            KeyAction::FollowUp | KeyAction::Newline => self.insert("\n"),
            KeyAction::Submit => {
                if self.lines[self.row].chars().take(self.col).last() == Some('\\') {
                    self.backspace();
                    self.insert("\n");
                } else {
                    return self.submit(false);
                }
            }
            KeyAction::InputStart => self.jump(true),
            KeyAction::InputEnd => self.jump(false),
            KeyAction::WordLeft => self.word_left(),
            KeyAction::WordRight => self.word_right(),
            KeyAction::CharLeft => self.left(),
            KeyAction::CharRight => self.right(),
            KeyAction::DeleteWord => {
                let end = self.pos();
                self.word_left();
                self.kill(self.pos(), end, prev, true);
            }
            KeyAction::DeleteWordForward => self.kill_word_right(prev),
            KeyAction::DeleteToLineStart => self.kill((self.row, 0), self.pos(), prev, true),
            KeyAction::DeleteToLineEnd => {
                let end = if self.col < self.len() || self.row + 1 == self.lines.len() {
                    (self.row, self.len())
                } else {
                    (self.row + 1, 0)
                };
                self.kill(self.pos(), end, prev, false);
            }
            KeyAction::Yank => {
                if let Some(text) = self.kills.last().cloned() {
                    self.insert(&text);
                }
            }
            KeyAction::LineStart => self.col = 0,
            KeyAction::LineEnd => self.col = self.len(),
            KeyAction::HistoryPrev => {
                if !self.vertical(true, goal) && self.anchor.is_none() {
                    self.recall(true);
                }
            }
            KeyAction::HistoryNext => {
                if !self.vertical(false, goal) && self.anchor.is_none() {
                    self.recall(false);
                }
            }
            KeyAction::Complete => self.complete(),
            // Global actions, and undo and redo, are handled before this.
            _ => {}
        }
        Action::None
    }

    /// Insert pasted text at the cursor (replacing a selection). A long paste
    /// shows as a `[Pasted text #N, M lines]` placeholder until it is sent.
    pub fn paste(&mut self, text: &str) {
        self.empty_ctrl_c = false;
        let before = (self.lines.clone(), self.pos());
        if let Some((a, b)) = self.selection() {
            self.remove(a, b);
        }
        self.anchor = None;
        let text = normalize(text);
        if text.chars().count() > FOLD_PASTE {
            let lines = text.lines().count().max(1);
            let token = format!(
                "[Pasted text #{}, {lines} line{}]",
                self.pastes.len() + 1,
                if lines == 1 { "" } else { "s" }
            );
            self.insert(&token);
            self.pastes.push((token, text));
        } else {
            self.insert(&text);
        }
        if self.lines != before.0 {
            self.record(before);
        }
        self.last = Edit::Other;
    }

    /// Insert an atomic token such as `[Image #1]`, spaced from its neighbours.
    pub fn insert_token(&mut self, token: &str) {
        let lead = if self.before().is_some_and(|c| !c.is_whitespace()) { " " } else { "" };
        let trail = if self.after().is_some_and(char::is_whitespace) { "" } else { " " };
        self.paste(&format!("{lead}{token}{trail}"));
    }

    /// Remove every occurrence of `token` from the draft.
    pub fn remove_token(&mut self, token: &str) {
        let text = self.text();
        if text.contains(token) {
            self.set_text(text.replace(&format!("{token} "), "").replace(token, "").trim_end());
        }
    }

    fn insert(&mut self, text: &str) {
        let text = normalize(text);
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
    /// The text as it is sent: folded pastes expanded.
    pub fn submitted_text(&self) -> String {
        self.expand(&self.text())
    }
    fn expand(&self, text: &str) -> String {
        self.pastes
            .iter()
            .fold(text.to_string(), |t, (token, full)| t.replace(token, full))
    }
    pub fn set_text(&mut self, text: &str) {
        self.lines = text.split('\n').map(str::to_owned).collect();
        self.row = self.lines.len() - 1;
        self.col = self.len();
        self.browsing = None;
        self.empty_ctrl_c = false;
        self.anchor = None;
        self.goal = None;
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
        self.width.set(width);
        let sel = self.selection();
        let selected = |row: usize, i: usize| sel.is_some_and(|(a, b)| (row, i) >= a && (row, i) < b);
        let mut view = View {
            lines: Vec::new(),
            cursor_row: 0,
            cursor_col: 0,
        };
        for (row, line) in self.lines.iter().enumerate() {
            view.lines.push(String::new());
            let mut col = 0;
            let mut inverse = false;
            for (i, c) in line.chars().enumerate() {
                let w = c.width().unwrap_or(0);
                if w > 0 && col > 0 && col + w > width {
                    if std::mem::take(&mut inverse) {
                        view.lines.last_mut().unwrap().push_str("\x1b[27m");
                    }
                    view.lines.push(String::new());
                    col = 0;
                }
                if row == self.row && i == self.col {
                    view.cursor_row = view.lines.len() - 1;
                    view.cursor_col = col;
                }
                if selected(row, i) != inverse {
                    inverse = !inverse;
                    view.lines.last_mut().unwrap().push_str(if inverse { "\x1b[7m" } else { "\x1b[27m" });
                }
                view.lines.last_mut().unwrap().push(c);
                col += w;
            }
            if inverse {
                view.lines.last_mut().unwrap().push_str("\x1b[27m");
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
    /// Popup rows for the current text: commands (ranked exact, prefix,
    /// then substring, aliases included) and skills after `/`, skills after
    /// `$`, or the arguments of the command already typed.
    pub fn suggestions(&self) -> Vec<Suggestion> {
        if self.lines.len() != 1 {
            return Vec::new();
        }
        let line = &self.lines[0];
        if line.starts_with('$') && !line.chars().any(char::is_whitespace) {
            return self.skill_suggestions(line);
        }
        if !line.starts_with('/') {
            return Vec::new();
        }
        if line.chars().any(char::is_whitespace) {
            return arg_suggestions(line);
        }
        let mut ranked: Vec<(u8, usize, &(String, String))> = self
            .commands
            .iter()
            .enumerate()
            .filter_map(|(i, c)| {
                let names = std::iter::once(c.0.as_str()).chain(crate::slash::aliases(&c.0));
                names.filter_map(|n| crate::slash::rank(n, line)).min().map(|r| (r, i, c))
            })
            .collect();
        ranked.sort_by_key(|(r, i, _)| (*r, *i));
        let mut out: Vec<Suggestion> = ranked
            .into_iter()
            .map(|(_, _, (name, desc))| Suggestion {
                label: name.clone(),
                detail: desc.clone(),
                tag: crate::slash::category(name).label().to_string(),
                fill: name.clone(),
                run: true,
                arg: false,
            })
            .collect();
        if line.len() > 1 {
            out.extend(self.skill_suggestions(line));
        }
        out
    }

    fn skill_suggestions(&self, query: &str) -> Vec<Suggestion> {
        let mut ranked: Vec<(u8, &(String, String))> = self
            .skills
            .iter()
            .filter_map(|s| crate::slash::rank(&s.0, query).map(|r| (r, s)))
            .collect();
        ranked.sort_by_key(|(r, _)| *r);
        ranked
            .into_iter()
            .map(|(_, (name, desc))| Suggestion {
                label: format!("${name}"),
                detail: desc.lines().next().unwrap_or("").to_string(),
                tag: "skill".into(),
                fill: format!("${name}"),
                run: false,
                arg: false,
            })
            .collect()
    }

    /// Every command and its description (for the `/help` menu).
    pub fn commands(&self) -> &[(String, String)] {
        &self.commands
    }

    /// Replace the skills offered after `/` and `$`.
    pub fn set_skills(&mut self, skills: Vec<(String, String)>) {
        self.skills = skills;
    }

    /// The text after `@` in the word at the cursor, for the file picker.
    pub fn at_query(&self) -> Option<String> {
        let line = &self.lines[self.row];
        let b = self.byte_col();
        if line[b..].chars().next().is_some_and(|c| !c.is_whitespace()) {
            return None;
        }
        let head = &line[..b];
        let start = head
            .char_indices()
            .rev()
            .find(|(_, c)| c.is_whitespace())
            .map_or(0, |(i, c)| i + c.len_utf8());
        head[start..].strip_prefix('@').map(str::to_owned)
    }

    /// Replace the `@query` at the cursor with `@path `.
    pub fn complete_at(&mut self, path: &str) {
        let Some(query) = self.at_query() else { return };
        let before = (self.lines.clone(), self.pos());
        self.remove((self.row, self.col - query.chars().count() - 1), self.pos());
        self.insert(&format!("@{path} "));
        self.record(before);
    }

    fn pos(&self) -> Pos {
        (self.row, self.col)
    }
    fn len(&self) -> usize {
        self.lines[self.row].chars().count()
    }
    fn byte_col(&self) -> usize {
        byte(&self.lines[self.row], self.col)
    }
    /// The selected range, ordered, when it is not empty.
    fn selection(&self) -> Option<(Pos, Pos)> {
        let a = self.anchor?;
        let b = self.pos();
        (a != b).then(|| (a.min(b), a.max(b)))
    }
    fn slice(&self, a: Pos, b: Pos) -> String {
        (a.0..=b.0)
            .map(|r| {
                let l = &self.lines[r];
                let s = if r == a.0 { byte(l, a.1) } else { 0 };
                let e = if r == b.0 { byte(l, b.1) } else { l.len() };
                &l[s..e]
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
    /// Delete the text between `a` and `b` (in order), leaving the cursor at `a`.
    fn remove(&mut self, a: Pos, b: Pos) -> String {
        let text = self.slice(a, b);
        let tail = self.lines[b.0][byte(&self.lines[b.0], b.1)..].to_string();
        let start = byte(&self.lines[a.0], a.1);
        self.lines[a.0].truncate(start);
        self.lines[a.0].push_str(&tail);
        self.lines.drain(a.0 + 1..=b.0);
        (self.row, self.col) = a;
        text
    }
    /// Delete into the kill ring; consecutive kills join into one entry.
    fn kill(&mut self, a: Pos, b: Pos, prev: Edit, backward: bool) {
        let text = self.remove(a, b);
        match self.kills.last_mut() {
            Some(last) if prev == Edit::Kill => {
                if backward {
                    last.insert_str(0, &text);
                } else {
                    last.push_str(&text);
                }
            }
            _ if text.is_empty() => {}
            _ => {
                self.kills.push(text);
                if self.kills.len() > MAX_KILLS {
                    self.kills.remove(0);
                }
            }
        }
        self.last = Edit::Kill;
    }
    fn kill_word_right(&mut self, prev: Edit) {
        let start = self.pos();
        self.word_right();
        self.kill(start, self.pos(), prev, false);
    }
    /// Length in characters of an image or paste token ending at the cursor.
    fn token_before(&self) -> Option<usize> {
        let head = &self.lines[self.row][..self.byte_col()];
        let token = &head[head.rfind('[')?..];
        self.is_token(token).then(|| token.chars().count())
    }
    fn token_after(&self) -> Option<usize> {
        let tail = &self.lines[self.row][self.byte_col()..];
        let token = &tail[..tail.find(']')? + 1];
        (tail.starts_with('[') && self.is_token(token)).then(|| token.chars().count())
    }
    fn is_token(&self, s: &str) -> bool {
        is_image_token(s) || self.pastes.iter().any(|(t, _)| t == s)
    }
    fn left(&mut self) {
        if let Some(n) = self.token_before() {
            self.col -= n;
        } else if self.col > 0 {
            self.col -= 1;
        } else if self.row > 0 {
            self.row -= 1;
            self.col = self.len();
        }
    }
    fn right(&mut self) {
        if let Some(n) = self.token_after() {
            self.col += n;
        } else if self.col < self.len() {
            self.col += 1;
        } else if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = 0;
        }
    }
    /// Move to the start (`start`) or end of the whole draft.
    fn jump(&mut self, start: bool) {
        if start {
            (self.row, self.col) = (0, 0);
        } else {
            self.row = self.lines.len() - 1;
            self.col = self.len();
        }
    }
    /// Start columns of the display rows that line `row` wraps into.
    fn segments(&self, row: usize) -> Vec<usize> {
        let width = self.width.get();
        let mut starts = vec![0];
        let mut col = 0;
        for (i, c) in self.lines[row].chars().enumerate() {
            let w = c.width().unwrap_or(0);
            if w > 0 && col > 0 && col + w > width {
                starts.push(i);
                col = 0;
            }
            col += w;
        }
        starts
    }
    /// Move one display row up or down, keeping the preferred column.
    /// Returns false at the first or last row.
    fn vertical(&mut self, up: bool, goal: Option<usize>) -> bool {
        let segs = self.segments(self.row);
        let seg = segs.iter().rposition(|&s| s <= self.col).unwrap_or(0);
        let chars: Vec<char> = self.lines[self.row].chars().collect();
        let x: usize = chars[segs[seg]..self.col].iter().map(|c| c.width().unwrap_or(0)).sum();
        // At the end of a full row the cursor shows on an extra row below it.
        let past_end = self.col == chars.len() && self.col > 0 && x >= self.width.get();
        let x = goal.unwrap_or(if past_end { 0 } else { x });
        let (row, seg) = if up {
            if past_end {
                (self.row, seg)
            } else if seg > 0 {
                (self.row, seg - 1)
            } else if self.row > 0 {
                (self.row - 1, self.segments(self.row - 1).len() - 1)
            } else {
                return false;
            }
        } else if seg + 1 < segs.len() {
            (self.row, seg + 1)
        } else if self.row + 1 < self.lines.len() {
            (self.row + 1, 0)
        } else {
            return false;
        };
        let segs = self.segments(row);
        let chars: Vec<char> = self.lines[row].chars().collect();
        // A wrapped row's last position belongs to the next row.
        let limit = segs.get(seg + 1).map_or(chars.len(), |e| e - 1);
        let (mut col, mut acc) = (segs[seg], 0);
        while col < limit {
            let w = chars[col].width().unwrap_or(0);
            if acc + w > x {
                break;
            }
            acc += w;
            col += 1;
        }
        (self.row, self.col) = (row, col);
        self.goal = Some(x);
        true
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
        let n = self.token_before().unwrap_or(1);
        if self.col > 0 {
            self.remove((self.row, self.col - n), self.pos());
        } else if self.row > 0 {
            self.remove((self.row - 1, self.lines[self.row - 1].chars().count()), self.pos());
        }
    }
    fn delete(&mut self) {
        let n = self.token_after().unwrap_or(1);
        if self.col < self.len() {
            self.remove(self.pos(), (self.row, self.col + n));
        } else if self.row + 1 < self.lines.len() {
            self.remove(self.pos(), (self.row + 1, 0));
        }
    }
    fn record(&mut self, before: (Vec<String>, Pos)) {
        self.undo.push(before);
        if self.undo.len() > MAX_UNDO {
            self.undo.remove(0);
        }
        self.redo.clear();
    }
    fn undo_step(&mut self, redo: bool) {
        let (from, to) = if redo {
            (&mut self.redo, &mut self.undo)
        } else {
            (&mut self.undo, &mut self.redo)
        };
        if let Some((lines, (row, col))) = from.pop() {
            to.push((std::mem::replace(&mut self.lines, lines), (self.row, self.col)));
            (self.row, self.col) = (row, col);
            self.anchor = None;
        }
        self.last = Edit::Other;
    }
    fn submit(&mut self, follow_up: bool) -> Action {
        let text = self.submitted_text();
        if text.trim().is_empty() {
            return Action::None;
        }
        self.set_text("");
        self.pastes.clear();
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
        let step = self.keys.matches(KeyAction::HistorySearch, &key);
        let Some(search) = &mut self.search else {
            if step {
                self.search = Some(Search { query: String::new(), at: None, draft: self.text(), failed: false });
                return Some(Action::None);
            }
            return None;
        };
        let older = match key.code {
            _ if step => true,
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

    /// Whether a Ctrl+R search is active (it owns Esc, Enter and arrows).
    pub fn searching(&self) -> bool {
        self.search.is_some()
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
    /// Replace the slash commands extensions registered (`ext` lists the
    /// current ones); nyq's own commands are kept.
    pub fn set_extension_commands(&mut self, ext: Vec<(String, String)>) {
        let old = std::mem::take(&mut self.ext_commands);
        self.commands.retain(|(name, _)| !old.contains(name));
        self.ext_commands = ext.iter().map(|(n, _)| n.clone()).collect();
        self.commands.extend(ext);
    }
    /// Commands that start with the text (Tab completion in the editor).
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

/// Completions for the word being typed after a command.
fn arg_suggestions(line: &str) -> Vec<Suggestion> {
    let words: Vec<&str> = line.split_whitespace().collect();
    let open = line.ends_with(char::is_whitespace);
    let (prior, partial) = if open {
        (&words[1..], "")
    } else {
        (&words[1..words.len() - 1], words[words.len() - 1])
    };
    let head = &line[..line.len() - partial.len()];
    crate::slash::arg_options(words[0], prior)
        .iter()
        .filter(|(name, _, _)| name.starts_with(&partial.to_lowercase()))
        .map(|(name, desc, more)| Suggestion {
            label: name.to_string(),
            detail: desc.to_string(),
            tag: String::new(),
            fill: format!("{head}{name}"),
            run: !more,
            arg: true,
        })
        .collect()
}

fn normalize(text: &str) -> String {
    text.replace("\r\n", "\n")
        .replace('\r', "\n")
        .replace('\t', "    ")
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
        assert_eq!((e.row, e.col), (0, 4));
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
        assert_eq!(e.suggestions()[0].tag, "General");
        plain(&mut e, Tab);
        assert_eq!(e.text(), "/hel");
        e.set_text("/help");
        plain(&mut e, Tab);
        assert_eq!(e.text(), "/help ");
        assert!(e.suggestions().is_empty());
        for text in ["/missing", "word", "/h x", "/h\n", "/h\t", "$x"] {
            e.set_text(text);
            plain(&mut e, Tab);
            assert_eq!(e.text(), text);
            assert!(e.suggestions().is_empty());
        }
        e.commands = (0..12)
            .map(|i| (format!("/c{i}"), format!("desc{i}")))
            .collect();
        e.set_text("/");
        let all: Vec<String> = e.suggestions().into_iter().map(|s| s.fill).collect();
        assert_eq!(all.len(), 12, "every command is reachable");
        plain(&mut e, Tab);
        assert_eq!(e.text(), "/c");
        e.set_text("/c11");
        plain(&mut e, Tab);
        assert_eq!(e.text(), "/c11 ");
    }

    #[test]
    fn ranked_matches_aliases_arguments_and_skills() {
        let mut e = Editor::new(
            vec![],
            vec![
                ("/models-x".into(), "x".into()),
                ("/model".into(), "model".into()),
                ("/usage".into(), "usage".into()),
                ("/permissions".into(), "perms".into()),
            ],
        );
        e.set_skills(vec![("pdf-tools".into(), "Work with PDFs\nmore".into())]);
        let fills = |e: &Editor| e.suggestions().into_iter().map(|s| s.fill).collect::<Vec<_>>();
        e.set_text("/model");
        assert_eq!(fills(&e), ["/model", "/models-x"]);
        e.set_text("/ode");
        assert_eq!(fills(&e), ["/models-x", "/model"]);
        e.set_text("/cost");
        assert_eq!(fills(&e), ["/usage"]);
        e.set_text("/pdf");
        let s = e.suggestions();
        assert_eq!((s[0].label.as_str(), s[0].detail.as_str(), s[0].run), ("$pdf-tools", "Work with PDFs", false));
        e.set_text("$p");
        assert_eq!(fills(&e), ["$pdf-tools"]);
        e.set_text("/permissions re");
        let s = e.suggestions();
        let labels: Vec<_> = s.iter().map(|s| s.label.as_str()).collect();
        assert_eq!(labels, ["reload", "remember", "revoke", "reset"]);
        assert!(s.iter().all(|s| s.arg));
        assert_eq!((s[3].fill.as_str(), s[3].run), ("/permissions reset", true));
        assert!(!s[1].run);
        e.set_text("/permissions ");
        assert_eq!(e.suggestions()[0].fill, "/permissions ask");
        e.set_text("/usage 7");
        assert_eq!(fills(&e), ["/usage 7d"]);
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
        e.render(2);
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
        // The column wanted before a short line comes back after it.
        plain(&mut e, Up);
        assert_eq!((e.row, e.col), (0, 4));
        plain(&mut e, End);
        plain(&mut e, Down);
        assert_eq!((e.row, e.col), (1, 1));
        plain(&mut e, Down);
        assert_eq!((e.row, e.col), (2, 4));

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

    #[test]
    fn undo_redo_groups_words() {
        let mut e = Editor::default();
        for c in "ab cd".chars() {
            plain(&mut e, Char(c));
        }
        ctrl(&mut e, '_');
        assert_eq!(e.text(), "ab ");
        ctrl(&mut e, '7');
        assert_eq!(e.text(), "ab");
        ctrl(&mut e, '_');
        assert!(e.is_empty());
        ctrl(&mut e, '_');
        key(&mut e, Char('_'), M::ALT);
        assert_eq!(e.text(), "ab");
        key(&mut e, Char('Z'), M::CONTROL | M::SHIFT);
        assert_eq!(e.text(), "ab ");
        // A new edit drops the redo steps.
        plain(&mut e, Char('x'));
        key(&mut e, Char('_'), M::ALT);
        assert_eq!(e.text(), "ab x");
        // Clearing and pasting are undoable too.
        ctrl(&mut e, 'c');
        ctrl(&mut e, '_');
        assert_eq!(e.text(), "ab x");
        e.paste("yz");
        ctrl(&mut e, '-');
        assert_eq!(e.text(), "ab x");
        assert_eq!(e.col, 4);
    }

    #[test]
    fn kill_ring_and_yank() {
        let mut e = Editor::default();
        e.set_text("one two three");
        ctrl(&mut e, 'w');
        ctrl(&mut e, 'w');
        assert_eq!(e.text(), "one ");
        ctrl(&mut e, 'y');
        assert_eq!(e.text(), "one two three");
        ctrl(&mut e, 'a');
        key(&mut e, Char('d'), M::ALT);
        assert_eq!(e.text(), " two three");
        ctrl(&mut e, 'k');
        assert!(e.is_empty());
        ctrl(&mut e, 'y');
        assert_eq!(e.text(), "one two three", "consecutive kills join");
        e.set_text("a\nb");
        plain(&mut e, Up);
        ctrl(&mut e, 'a');
        ctrl(&mut e, 'k');
        ctrl(&mut e, 'k');
        assert_eq!(e.text(), "b");
        plain(&mut e, End);
        ctrl(&mut e, 'y');
        assert_eq!(e.text(), "ba\n");
        plain(&mut e, Left);
        key(&mut e, Delete, M::CONTROL);
        assert_eq!(e.text(), "ba");
    }

    #[test]
    fn selection_copy_cut_and_replace() {
        let mut e = Editor::default();
        e.set_text("hello world");
        plain(&mut e, Home);
        for _ in 0..5 {
            key(&mut e, Right, M::SHIFT);
        }
        assert_eq!(e.render(80).lines, ["\x1b[7mhello\x1b[27m world"]);
        assert_eq!(ctrl(&mut e, 'c'), Action::Copy("hello".into()));
        assert_eq!(e.text(), "hello world");
        key(&mut e, Right, M::CONTROL | M::SHIFT);
        assert_eq!(ctrl(&mut e, 'x'), Action::Copy(" world".into()));
        assert_eq!(e.text(), "hello");
        key(&mut e, Home, M::SHIFT);
        key(&mut e, Char('J'), M::SHIFT);
        assert_eq!(e.text(), "J");
        // A selection across lines, highlighted on each wrapped row.
        e.set_text("ab\ncd");
        plain(&mut e, Left);
        key(&mut e, Up, M::SHIFT);
        assert_eq!(e.render(1).lines, ["a", "\x1b[7mb\x1b[27m", "\x1b[7mc\x1b[27m", "d"]);
        plain(&mut e, Backspace);
        assert_eq!(e.text(), "ad");
        // Plain movement and Esc drop the selection; Shift+Up never recalls history.
        key(&mut e, Left, M::SHIFT);
        plain(&mut e, Left);
        assert_eq!(ctrl(&mut e, 'c'), Action::CtrlC { exit: false });
        e.set_text("x");
        key(&mut e, Left, M::SHIFT);
        assert_eq!(plain(&mut e, Esc), Action::None);
        assert_eq!(e.text(), "x");
        e.history = vec!["old".into()];
        key(&mut e, Up, M::SHIFT);
        assert_eq!(e.text(), "x");
        key(&mut e, Char('a'), M::SUPER);
        assert_eq!(key(&mut e, Char('c'), M::SUPER), Action::Copy("x".into()));
    }

    #[test]
    fn emacs_keys_and_draft_jumps() {
        let mut e = Editor::new(vec!["old".into()], vec![]);
        e.set_text("ab\ncd");
        ctrl(&mut e, 'b');
        assert_eq!(e.pos(), (1, 1));
        ctrl(&mut e, 'f');
        ctrl(&mut e, 'p');
        assert_eq!(e.pos(), (0, 2));
        ctrl(&mut e, 'n');
        assert_eq!(e.pos(), (1, 2));
        key(&mut e, Home, M::CONTROL);
        assert_eq!(e.pos(), (0, 0));
        key(&mut e, End, M::CONTROL);
        assert_eq!(e.pos(), (1, 2));
        key(&mut e, Char('<'), M::ALT);
        assert_eq!(e.pos(), (0, 0));
        key(&mut e, Char('>'), M::ALT);
        assert_eq!(e.pos(), (1, 2));
        key(&mut e, Left, M::ALT);
        assert_eq!(e.pos(), (1, 0));
        ctrl(&mut e, 'p');
        ctrl(&mut e, 'p');
        assert_eq!(e.text(), "old");
    }

    #[test]
    fn rebound_and_unbound_keys() {
        let (keys, warnings) = Keymap::parse(
            "history_search = \"alt+r\"\nnewline = []\nline_start = \"f2\"\ndelete_word = \"ctrl+backspace\"",
        );
        assert!(warnings.is_empty(), "{warnings:?}");
        let mut e = Editor::new(vec!["cargo test".into()], vec![]);
        e.set_keys(Arc::new(keys));
        ctrl(&mut e, 'r');
        assert!(!e.searching(), "ctrl+r no longer searches");
        key(&mut e, Char('r'), M::ALT);
        assert!(e.searching());
        for c in "test".chars() {
            plain(&mut e, Char(c));
        }
        assert_eq!(e.text(), "cargo test");
        plain(&mut e, Esc);
        e.set_text("ab");
        key(&mut e, Enter, M::SHIFT);
        ctrl(&mut e, 'j');
        assert_eq!(e.text(), "ab", "newline is unbound");
        plain(&mut e, F(2));
        assert_eq!(e.pos(), (0, 0));
        ctrl(&mut e, 'e');
        key(&mut e, Backspace, M::CONTROL);
        assert_eq!(e.text(), "");
        // Alt+Enter still queues a follow-up while busy.
        e.set_text("next");
        assert_eq!(e.handle_key(KeyEvent::new(Enter, M::ALT), true), Action::FollowUp("next".into()));
    }

    #[test]
    fn double_esc_clears_an_idle_draft() {
        let mut e = Editor::default();
        e.set_text("draft");
        assert_eq!(plain(&mut e, Esc), Action::Interrupt);
        assert_eq!(plain(&mut e, Esc), Action::None);
        assert!(e.is_empty());
        ctrl(&mut e, '_');
        assert_eq!(e.text(), "draft");
        // Another key in between, or a running turn, keeps the draft.
        plain(&mut e, Esc);
        plain(&mut e, Left);
        assert_eq!(plain(&mut e, Esc), Action::Interrupt);
        let busy_esc = |e: &mut Editor| e.handle_key(KeyEvent::new(Esc, M::NONE), true);
        assert_eq!(busy_esc(&mut e), Action::Interrupt);
        assert_eq!(busy_esc(&mut e), Action::Interrupt);
        assert_eq!(e.text(), "draft");
    }

    #[test]
    fn up_and_down_move_by_display_rows() {
        let mut e = Editor::new(vec!["old".into()], vec![]);
        e.set_text("abcdefghij");
        e.render(4);
        plain(&mut e, Up);
        assert_eq!(e.pos(), (0, 6));
        plain(&mut e, Up);
        assert_eq!(e.pos(), (0, 2));
        plain(&mut e, Down);
        plain(&mut e, Down);
        assert_eq!(e.pos(), (0, 10));
        plain(&mut e, Up);
        plain(&mut e, Up);
        plain(&mut e, Up);
        assert_eq!(e.text(), "old");
        // From the extra row after a full row, Up lands on that row's start.
        e.set_text("abcdefgh\nx");
        plain(&mut e, Up);
        plain(&mut e, End);
        assert_eq!(e.render(4).cursor_row, 2);
        plain(&mut e, Up);
        assert_eq!(e.pos(), (0, 4));
        // A wrapped row's last position is the next row's first, so stop short.
        e.set_text("abcdefgh");
        e.col = 7;
        plain(&mut e, Up);
        assert_eq!(e.pos(), (0, 3));
    }

    #[test]
    fn long_pastes_fold_and_expand_on_submit() {
        let mut e = Editor::default();
        let long = format!("{}\n{}", "x".repeat(800), "y".repeat(800));
        e.paste("see ");
        e.paste(&long);
        assert_eq!(e.text(), "see [Pasted text #1, 2 lines]");
        assert_eq!(e.submitted_text(), format!("see {long}"));
        plain(&mut e, Left);
        assert_eq!(e.col, 4, "the placeholder moves as one unit");
        plain(&mut e, Delete);
        assert_eq!(e.text(), "see ");
        ctrl(&mut e, '_');
        plain(&mut e, End);
        plain(&mut e, Backspace);
        assert_eq!(e.text(), "see ");
        ctrl(&mut e, '_');
        assert_eq!(plain(&mut e, Enter), Action::Submit(format!("see {long}")));
        e.paste(&"z".repeat(1001));
        assert_eq!(e.text(), "[Pasted text #1, 1 line]");
        // A typed lookalike isn't a placeholder.
        e.set_text("[Pasted text #9, 1 line]");
        plain(&mut e, Backspace);
        assert_eq!(e.text(), "[Pasted text #9, 1 line");
    }

    #[test]
    fn image_tokens_are_atomic() {
        let mut e = Editor::default();
        e.set_text("look");
        e.insert_token("[Image #1]");
        assert_eq!(e.text(), "look [Image #1] ");
        plain(&mut e, Backspace);
        plain(&mut e, Backspace);
        assert_eq!(e.text(), "look ");
        e.insert_token("[Image #2]");
        plain(&mut e, Home);
        e.insert_token("[Image #3]");
        assert_eq!(e.text(), "[Image #3] look [Image #2] ");
        e.remove_token("[Image #2]");
        assert_eq!(e.text(), "[Image #3] look");
        e.remove_token("[Image #3]");
        assert_eq!(e.text(), "look");
        assert!(!is_image_token("[Image #]") && !is_image_token("[Image #1x]"));
    }

    #[test]
    fn at_file_query_and_completion() {
        let mut e = Editor::default();
        e.set_text("see @src/ma");
        assert_eq!(e.at_query().as_deref(), Some("src/ma"));
        e.complete_at("src/main.rs");
        assert_eq!(e.text(), "see @src/main.rs ");
        assert_eq!(e.at_query(), None);
        ctrl(&mut e, '_');
        assert_eq!(e.text(), "see @src/ma");
        e.set_text("@");
        assert_eq!(e.at_query().as_deref(), Some(""));
        e.set_text("a@b");
        assert_eq!(e.at_query(), None);
        e.set_text("@ab cd");
        e.col = 2;
        assert_eq!(e.at_query(), None);
    }
}
