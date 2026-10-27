//! The full transcript viewer (Ctrl+O): a full-screen, scrollable view of
//! the whole conversation with expanded tool output and search. The `Log`
//! records what the inline UI shows, with full tool outputs.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use nu_ansi_term::{Color, Style};
use unicode_width::UnicodeWidthChar;

use super::chrome::{self, Mark};
use super::render::{
    looks_like_diff, numbered_diff, sanitize_text, strip_ansi, truncate, visible_width, wrap_row,
    Markdown,
};
use super::theme::{fg, on, BRIGHT, FAINT, GRAY, MID, RED, YELLOW};

/// Tool output kept per call.
const MAX_OUTPUT: usize = 256 * 1024;
/// Output rows per tool call in the compact view.
const COMPACT_ROWS: usize = 3;

#[derive(Clone, Debug, PartialEq)]
pub enum Entry {
    User(String),
    Assistant(String),
    Reasoning(String),
    Tool {
        name: String,
        summary: String,
        output: Option<String>,
        display: Option<String>,
        is_error: bool,
    },
    Info(String),
}

/// Everything shown in this session, for the viewer.
#[derive(Default)]
pub struct Log {
    pub entries: Vec<Entry>,
    /// Bumped on every change, so an open viewer knows to re-render.
    pub version: u64,
    /// The last entry is still streaming.
    open: bool,
}

fn capped(s: &str) -> String {
    if s.len() <= MAX_OUTPUT {
        return s.to_string();
    }
    let mut cut = MAX_OUTPUT;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}\n… [{} more bytes]", &s[..cut], s.len() - cut)
}

impl Log {
    fn push(&mut self, e: Entry) {
        self.open = false;
        self.entries.push(e);
        self.version += 1;
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.open = false;
        self.version += 1;
    }

    pub fn user(&mut self, text: &str) {
        self.push(Entry::User(text.to_string()));
    }

    pub fn info(&mut self, text: &str) {
        self.push(Entry::Info(text.to_string()));
    }

    /// Streamed or whole assistant text (`reasoning`: model thinking).
    pub fn text(&mut self, delta: &str, reasoning: bool) {
        self.version += 1;
        if self.open {
            match (self.entries.last_mut(), reasoning) {
                (Some(Entry::Assistant(t)), false) | (Some(Entry::Reasoning(t)), true) => {
                    return t.push_str(delta)
                }
                _ => {}
            }
        }
        self.push(if reasoning {
            Entry::Reasoning(delta.into())
        } else {
            Entry::Assistant(delta.into())
        });
        self.open = true;
    }

    /// The streaming message is complete.
    pub fn end_message(&mut self) {
        self.open = false;
    }

    pub fn tool_start(&mut self, name: &str, summary: &str) {
        self.push(Entry::Tool {
            name: name.into(),
            summary: summary.into(),
            output: None,
            display: None,
            is_error: false,
        });
    }

    /// Attach a result to the latest tool call still waiting for one.
    pub fn tool_end(&mut self, content: &str, display: Option<&str>, is_error: bool) {
        self.version += 1;
        let pending = self
            .entries
            .iter_mut()
            .rev()
            .find(|e| matches!(e, Entry::Tool { output: None, .. }));
        if let Some(Entry::Tool {
            output,
            display: d,
            is_error: err,
            ..
        }) = pending
        {
            *output = Some(capped(content));
            *d = display.map(capped);
            *err = is_error;
        }
    }
}

fn paint(style: Style, s: &str) -> String {
    if s.is_empty() {
        String::new()
    } else {
        style.paint(s).to_string()
    }
}

/// Hard-wrap plain text to `width` columns.
fn wrap(s: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut rows = vec![String::new()];
    let mut w = 0;
    for c in s.chars() {
        let cw = c.width().unwrap_or(0);
        if w + cw > width && w > 0 {
            rows.push(String::new());
            w = 0;
        }
        rows.last_mut().unwrap().push(c);
        w += cw;
    }
    rows
}

/// Rows for `text` behind `lead` (first row) / `cont` (the rest), each `style`d.
fn block(rows: &mut Vec<String>, text: &str, width: usize, lead: &str, cont: &str, style: Style) {
    let room = width.saturating_sub(visible_width(lead)).max(1);
    let mut first = true;
    for line in sanitize_text(text).lines() {
        for piece in wrap(line, room) {
            let prefix = if first { lead } else { cont };
            first = false;
            rows.push(format!("{prefix}{}", paint(style, &piece)));
        }
    }
}

/// At most `max` of `rows`, with a note saying how many were left out.
fn cap(rows: &mut Vec<String>, from: usize, max: usize, bar: &str) {
    let n = rows.len() - from;
    if n > max {
        rows.truncate(from + max);
        rows.push(format!(
            "{bar}{}",
            paint(fg(FAINT), &format!("… {} more lines", n - max))
        ));
    }
}

/// Render the whole log at `width` columns. `compact` folds reasoning to one
/// line and tool output to a few rows.
pub fn render(entries: &[Entry], width: usize, compact: bool) -> Vec<String> {
    let mut rows = Vec::new();
    for e in entries {
        if !rows.is_empty() {
            rows.push(String::new());
        }
        match e {
            Entry::User(t) => {
                if chrome::fancy() {
                    let lead = format!("{} ", paint(fg(chrome::ACCENT).bold(), "›"));
                    block(&mut rows, t.trim(), width, &lead, "  ", fg(BRIGHT).bold());
                } else {
                    let bar = paint(fg(BRIGHT), "┃ ");
                    block(&mut rows, t.trim(), width, &bar, &bar, Style::new().bold());
                }
            }
            Entry::Assistant(t) => {
                let mut md = Markdown::default();
                for line in sanitize_text(t.trim()).lines() {
                    rows.extend(wrap_row(&format!("  {}", md.line(line, true)), width));
                }
            }
            Entry::Reasoning(t) if compact => {
                let first = t
                    .lines()
                    .map(str::trim)
                    .find(|l| !l.is_empty())
                    .unwrap_or("")
                    .trim_matches('*');
                let line = sanitize_text(&format!("• Thought · {first}"));
                rows.push(paint(fg(GRAY).italic(), &truncate(&line, width)));
            }
            Entry::Reasoning(t) => {
                rows.push(paint(fg(GRAY).italic(), "• Thinking"));
                block(&mut rows, t.trim(), width, "  ", "  ", fg(GRAY).italic());
            }
            Entry::Tool {
                name,
                summary,
                output,
                display,
                is_error,
            } => {
                let mark = match (output, is_error) {
                    (None, _) => Mark::Running,
                    (Some(_), true) => Mark::Failed,
                    (Some(_), false) => Mark::Ok,
                };
                let lead = if chrome::fancy() {
                    chrome::mark(mark, 0)
                } else {
                    paint(fg(if *is_error { RED } else { BRIGHT }), "●")
                };
                let head = format!(
                    "{lead} {}",
                    paint(fg(if *is_error { RED } else { BRIGHT }), name)
                );
                let summary = sanitize_text(summary).replace('\n', " ");
                let room = width.saturating_sub(visible_width(&head) + 1);
                rows.push(format!(
                    "{head} {}",
                    paint(fg(GRAY), &truncate(&summary, room))
                ));
                let bar = paint(fg(FAINT), "  │ ");
                let from = rows.len();
                match (display.as_deref().filter(|d| looks_like_diff(d)), output) {
                    (Some(d), _) => {
                        rows.extend(numbered_diff(&sanitize_text(d), true, width, &bar));
                        if compact {
                            cap(&mut rows, from, COMPACT_ROWS * 4, &bar);
                        }
                    }
                    (None, Some(o)) if !o.trim().is_empty() => {
                        block(
                            &mut rows,
                            o.trim_end(),
                            width,
                            &bar,
                            &bar,
                            fg(if *is_error { RED } else { GRAY }),
                        );
                        if compact {
                            cap(&mut rows, from, COMPACT_ROWS, &bar);
                        }
                    }
                    (None, Some(_)) => {
                        rows.push(format!("{bar}{}", paint(fg(FAINT), "(no output)")))
                    }
                    (None, None) => rows.push(format!("{bar}{}", paint(fg(FAINT), "(running…)"))),
                }
            }
            Entry::Info(t) => block(&mut rows, t, width, "", "", fg(GRAY)),
        }
    }
    rows
}

/// Rows whose text contains `query` (case-insensitive).
pub fn find_matches(plain: &[String], query: &str) -> Vec<usize> {
    let q = query.to_lowercase();
    if q.is_empty() {
        return Vec::new();
    }
    plain
        .iter()
        .enumerate()
        .filter(|(_, r)| r.contains(&q))
        .map(|(i, _)| i)
        .collect()
}

/// `row` (plain) with every occurrence of `query` highlighted.
fn highlight(row: &str, query: &str, current: bool) -> String {
    let lower = row.to_lowercase();
    let q = query.to_lowercase();
    let hit = if current {
        on(fg(Color::Black), YELLOW)
    } else {
        Style::new().reverse()
    };
    if q.is_empty() || lower.len() != row.len() {
        return paint(hit, row);
    }
    let mut out = String::new();
    let mut at = 0;
    while let Some(i) = lower[at..].find(&q) {
        let start = at + i;
        out.push_str(&paint(fg(MID), &row[at..start]));
        out.push_str(&paint(hit, &row[start..start + q.len()]));
        at = start + q.len();
    }
    out.push_str(&paint(fg(MID), &row[at..]));
    out
}

#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    Stay,
    Close,
}

pub struct Viewer {
    rows: Vec<String>,
    /// Lowercased plain text of each row, for search.
    plain: Vec<String>,
    pub top: usize,
    width: usize,
    height: usize,
    version: u64,
    /// Reasoning and tool output folded (←), or everything (→).
    compact: bool,
    compact_shown: bool,
    /// The search being typed after `/`.
    typing: Option<String>,
    query: String,
    matches: Vec<usize>,
    current: usize,
    message: Option<String>,
}

impl Viewer {
    pub fn new(log: &Log, width: usize, height: usize) -> Self {
        let mut v = Viewer {
            rows: Vec::new(),
            plain: Vec::new(),
            top: 0,
            width: 0,
            height,
            version: u64::MAX,
            compact: false,
            compact_shown: false,
            typing: None,
            query: String::new(),
            matches: Vec::new(),
            current: 0,
            message: None,
        };
        v.refresh(log, width, height);
        v.top = v.max_top();
        v
    }

    fn body(&self) -> usize {
        self.height.saturating_sub(2).max(1)
    }

    fn max_top(&self) -> usize {
        self.rows.len().saturating_sub(self.body())
    }

    /// Re-render when the log or the size changed; follows the end if it was there.
    pub fn refresh(&mut self, log: &Log, width: usize, height: usize) -> bool {
        if log.version == self.version
            && width == self.width
            && height == self.height
            && self.compact == self.compact_shown
        {
            return false;
        }
        let at_end = self.top >= self.max_top();
        self.rows = render(&log.entries, width.max(1), self.compact);
        self.compact_shown = self.compact;
        self.plain = self
            .rows
            .iter()
            .map(|r| strip_ansi(r).to_lowercase())
            .collect();
        self.width = width;
        self.height = height;
        self.version = log.version;
        self.matches = find_matches(&self.plain, &self.query);
        self.current = self.current.min(self.matches.len().saturating_sub(1));
        self.top = if at_end {
            self.max_top()
        } else {
            self.top.min(self.max_top())
        };
        true
    }

    /// Mouse wheel: three rows per notch.
    pub fn wheel(&mut self, up: bool) {
        self.scroll(if up { -3 } else { 3 });
    }

    fn scroll(&mut self, delta: isize) {
        self.top = (self.top as isize + delta).clamp(0, self.max_top() as isize) as usize;
    }

    /// Show match `i` (wrapping), a few rows below the top.
    fn jump(&mut self, i: usize) {
        if self.matches.is_empty() {
            self.message = Some(format!("No matches for \"{}\"", self.query));
            return;
        }
        self.current = i % self.matches.len();
        self.top = self.matches[self.current]
            .saturating_sub(self.body() / 3)
            .min(self.max_top());
        self.message = None;
    }

    /// Whether the search query is being typed.
    pub fn searching(&self) -> bool {
        self.typing.is_some()
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Action {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if let Some(t) = &mut self.typing {
            match key.code {
                KeyCode::Esc => self.typing = None,
                KeyCode::Enter => {
                    self.query = self.typing.take().unwrap_or_default();
                    self.matches = find_matches(&self.plain, &self.query);
                    if self.query.is_empty() {
                        self.message = None;
                    } else {
                        // The first match at or after the top of the screen.
                        let first = self
                            .matches
                            .iter()
                            .position(|&r| r >= self.top)
                            .unwrap_or(0);
                        self.jump(first);
                    }
                }
                KeyCode::Backspace => {
                    t.pop();
                }
                KeyCode::Char('c') if ctrl => self.typing = None,
                KeyCode::Char(c) if !ctrl => t.push(c),
                _ => {}
            }
            return Action::Stay;
        }
        let page = self.body().saturating_sub(1).max(1) as isize;
        self.message = None;
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => return Action::Close,
            KeyCode::Char('o' | 'c' | 't') if ctrl => return Action::Close,
            KeyCode::Char('f') if ctrl => self.typing = Some(String::new()),
            KeyCode::Char('/') => self.typing = Some(String::new()),
            KeyCode::Char('n') if !self.query.is_empty() => self.jump(self.current + 1),
            KeyCode::Char('N') if !self.query.is_empty() => {
                self.jump(self.current + self.matches.len().max(1) - 1)
            }
            KeyCode::Up | KeyCode::Char('k') => self.scroll(-1),
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Enter => self.scroll(1),
            KeyCode::PageUp | KeyCode::Char('b') => self.scroll(-page),
            KeyCode::PageDown | KeyCode::Char(' ') => self.scroll(page),
            KeyCode::Char('u') if ctrl => self.scroll(-page / 2),
            KeyCode::Char('d') if ctrl => self.scroll(page / 2),
            KeyCode::Left => self.compact = true,
            KeyCode::Right => self.compact = false,
            KeyCode::Tab => self.compact = !self.compact,
            KeyCode::Home | KeyCode::Char('g') => self.top = 0,
            KeyCode::End | KeyCode::Char('G') => self.top = self.max_top(),
            _ => {}
        }
        Action::Stay
    }

    /// Exactly `height` rows: header, body, key hints.
    pub fn frame(&self) -> Vec<String> {
        let w = self.width.max(1);
        let body = self.body();
        let end = (self.top + body).min(self.rows.len());
        let range = if self.rows.is_empty() {
            "empty".to_string()
        } else {
            format!("{}–{} of {}", self.top + 1, end, self.rows.len())
        };
        let view = if self.compact { "compact" } else { "full" };
        let mut head = format!(
            "{} {}",
            paint(fg(BRIGHT).bold(), "Transcript"),
            paint(fg(GRAY), &format!("{view} · {range}"))
        );
        if !self.query.is_empty() {
            let pos = if self.matches.is_empty() {
                "no matches".into()
            } else {
                format!("{}/{}", self.current + 1, self.matches.len())
            };
            head.push_str(&paint(
                fg(GRAY),
                &format!("  /{} {pos}", truncate(&self.query, 30)),
            ));
        }
        let mut out = vec![head];
        let current_row = self.matches.get(self.current).copied();
        for i in self.top..self.top + body {
            let row = match self.rows.get(i) {
                Some(_) if !self.query.is_empty() && self.matches.binary_search(&i).is_ok() => {
                    highlight(
                        &strip_ansi(&self.rows[i]),
                        &self.query,
                        Some(i) == current_row,
                    )
                }
                Some(r) => r.clone(),
                None => String::new(),
            };
            out.push(if visible_width(&row) > w {
                format!("{}\x1b[0m", truncate(&strip_ansi(&row), w))
            } else {
                row
            });
        }
        let hints: &[(&str, &str)] = &[
            ("↑↓", "Scroll"),
            ("PgUp/PgDn", "Page"),
            ("Home/End", "Jump"),
            ("←→", "Compact/Full"),
            ("/", "Search"),
            ("n/N", "Next"),
            ("q", "Close"),
        ];
        let footer = match (&self.typing, &self.message) {
            (Some(t), _) => format!(
                "{}{}{}",
                paint(fg(BRIGHT), "/"),
                paint(fg(MID), t),
                paint(fg(FAINT), "  Enter Find · Esc Cancel")
            ),
            (None, Some(m)) => paint(fg(YELLOW), m),
            (None, None) => hints
                .iter()
                .map(|(k, a)| format!("{} {}", paint(fg(BRIGHT), k), paint(fg(GRAY), a)))
                .collect::<Vec<_>>()
                .join("   "),
        };
        out.push(footer);
        out.truncate(self.height.max(1));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    fn log() -> Log {
        let mut log = Log::default();
        log.user("find the bug");
        log.text("Thinking about ", true);
        log.text("it", true);
        log.end_message();
        log.text("Looking at **main**.\n", false);
        log.text("Done.", false);
        log.end_message();
        log.tool_start("bash", "cargo test");
        log.tool_end("line one\nNEEDLE here\nline three", None, false);
        log.tool_start("read_file", "src/lib.rs");
        log.tool_end("fn x() {}", None, false);
        for i in 0..40 {
            log.info(&format!("filler {i}"));
        }
        log.info("another needle at the end");
        log
    }

    #[test]
    fn log_merges_streams_and_pairs_results() {
        let l = log();
        assert_eq!(l.entries[1], Entry::Reasoning("Thinking about it".into()));
        assert_eq!(
            l.entries[2],
            Entry::Assistant("Looking at **main**.\nDone.".into())
        );
        let Entry::Tool { output, .. } = &l.entries[3] else {
            panic!()
        };
        assert!(output.as_deref().unwrap().contains("NEEDLE"));
        let Entry::Tool { name, output, .. } = &l.entries[4] else {
            panic!()
        };
        assert_eq!(
            (name.as_str(), output.as_deref()),
            ("read_file", Some("fn x() {}"))
        );
    }

    #[test]
    fn rendering_expands_tool_output() {
        let rows: Vec<String> = render(&log().entries, 40, false)
            .iter()
            .map(|r| strip_ansi(r))
            .collect();
        assert!(rows.iter().any(|r| r == "› find the bug"), "{rows:?}");
        assert!(rows.iter().any(|r| r == "✓ bash cargo test"), "{rows:?}");
        assert!(rows.iter().any(|r| r == "  │ NEEDLE here"), "{rows:?}");
        assert!(
            rows.iter().any(|r| r.contains("Looking at main.")),
            "{rows:?}"
        );
        assert!(rows.iter().any(|r| r == "  Thinking about it"), "{rows:?}");
        assert!(render(&log().entries, 20, false)
            .iter()
            .all(|r| visible_width(r) <= 20));
        // Long assistant lines wrap with their styling.
        let mut l = Log::default();
        l.text("**bold words that go on and on** and on", false);
        let rows = render(&l.entries, 20, false);
        assert!(
            rows.len() > 1 && rows.iter().all(|r| visible_width(r) <= 20),
            "{rows:?}"
        );
        assert!(rows[1].contains("\x1b[1m"), "{rows:?}");
    }

    #[test]
    fn search_finds_and_cycles() {
        let l = log();
        let mut v = Viewer::new(&l, 60, 10);
        assert_eq!(v.top, v.max_top(), "opens at the end");
        assert_eq!(v.frame().len(), 10);
        v.on_key(key(KeyCode::Home));
        assert_eq!(v.top, 0);
        v.on_key(key(KeyCode::Char('/')));
        for c in "needle".chars() {
            v.on_key(key(KeyCode::Char(c)));
        }
        assert!(strip_ansi(v.frame().last().unwrap()).contains("/needle"));
        v.on_key(key(KeyCode::Enter));
        assert_eq!(v.matches.len(), 2);
        let first = v.matches[0];
        assert!(v.top <= first && first < v.top + v.body());
        assert!(strip_ansi(&v.frame()[0]).contains("1/2"));
        v.on_key(key(KeyCode::Char('n')));
        assert_eq!(v.current, 1);
        assert!(v.top <= v.matches[1] && v.matches[1] < v.top + v.body());
        v.on_key(key(KeyCode::Char('n')));
        assert_eq!(v.current, 0, "wraps around");
        v.on_key(key(KeyCode::Char('N')));
        assert_eq!(v.current, 1);
        assert_eq!(find_matches(&v.plain, "NEEDLE"), v.matches);
        assert!(find_matches(&v.plain, "").is_empty());
        v.on_key(key(KeyCode::Char('/')));
        v.on_key(key(KeyCode::Char('z')));
        v.on_key(key(KeyCode::Char('z')));
        v.on_key(key(KeyCode::Char('q')));
        v.on_key(key(KeyCode::Enter));
        assert!(v.matches.is_empty());
        assert!(strip_ansi(v.frame().last().unwrap()).contains("No matches"));
        assert_eq!(v.on_key(key(KeyCode::Char('q'))), Action::Close);
    }

    #[test]
    fn paging_and_following() {
        let mut l = log();
        let mut v = Viewer::new(&l, 60, 10);
        v.on_key(key(KeyCode::Home));
        v.on_key(key(KeyCode::PageDown));
        assert_eq!(v.top, 7);
        v.on_key(key(KeyCode::PageUp));
        assert_eq!(v.top, 0);
        v.on_key(key(KeyCode::End));
        l.info("new line");
        assert!(v.refresh(&l, 60, 10));
        assert_eq!(v.top, v.max_top(), "follows new output at the end");
        assert!(!v.refresh(&l, 60, 10));
        assert_eq!(
            v.on_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL)),
            Action::Close
        );
    }

    #[test]
    fn compact_view_folds_output_and_wheel_scrolls() {
        let mut l = log();
        l.tool_start("bash", "seq 10");
        l.tool_end(
            &(1..=10)
                .map(|i| i.to_string())
                .collect::<Vec<_>>()
                .join("\n"),
            None,
            false,
        );
        l.tool_start("edit_file", "a.rs");
        l.tool_end(
            "Edited a.rs.",
            Some("--- a/a.rs\n+++ b/a.rs\n@@ -3,2 +3,2 @@\n ctx\n-old\n+new\n"),
            false,
        );
        let full: Vec<String> = render(&l.entries, 60, false)
            .iter()
            .map(|r| strip_ansi(r))
            .collect();
        assert!(full.iter().any(|r| r == "  │ 10"), "{full:?}");
        assert!(full.iter().any(|r| r == "  │ 4 - old"), "{full:?}");
        assert!(full.iter().any(|r| r == "  │ 4 + new"), "{full:?}");
        let compact: Vec<String> = render(&l.entries, 60, true)
            .iter()
            .map(|r| strip_ansi(r))
            .collect();
        assert!(
            compact.iter().any(|r| r == "  │ … 7 more lines"),
            "{compact:?}"
        );
        assert!(
            compact.iter().any(|r| r == "• Thought · Thinking about it"),
            "{compact:?}"
        );
        let mut v = Viewer::new(&l, 60, 10);
        v.on_key(key(KeyCode::Left));
        assert!(v.refresh(&l, 60, 10));
        assert!(strip_ansi(&v.frame()[0]).contains("compact"));
        let end = v.top;
        v.wheel(true);
        assert_eq!(v.top, end - 3);
        v.wheel(false);
        assert_eq!(v.top, end);
        v.on_key(key(KeyCode::Right));
        assert!(v.refresh(&l, 60, 10));
    }

    #[test]
    fn highlight_marks_matches() {
        let h = highlight("a Needle b", "needle", false);
        assert_eq!(strip_ansi(&h), "a Needle b");
        assert!(h.contains("\x1b[7m"));
    }
}
