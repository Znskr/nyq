//! What the `/skills` and `/tools` menus share: rows in groups with a filter,
//! group tabs with counts, a preview of the selected row under the list, an
//! optional input line, and key hints that wrap to the terminal width.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use nu_ansi_term::{Color, Style};
use unicode_width::UnicodeWidthChar;

use super::render::{sanitize_text, truncate, visible_width};
use super::theme::{fg, BRIGHT, FAINT, GRAY, MID, YELLOW};

/// Rows a list shows at most; fewer on a short terminal.
const LIST_ROWS: usize = 8;
/// Label column width at most.
const LABEL_MAX: usize = 28;

/// The filter, selection and group of a grouped list.
#[derive(Default)]
pub struct List {
    pub filter: String,
    pub selected: usize,
    /// Index into the menu's groups; `None` shows every group.
    pub group: Option<usize>,
}

impl List {
    /// The filter's words, lowercased.
    pub fn words(&self) -> Vec<String> {
        self.filter
            .split_whitespace()
            .map(str::to_lowercase)
            .collect()
    }

    /// Navigation and filter typing over `count` rows in `groups` groups.
    /// Returns false for keys it leaves to the menu.
    pub fn on_key(&mut self, key: KeyEvent, count: usize, groups: usize) -> bool {
        let mods = key.modifiers & (KeyModifiers::CONTROL | KeyModifiers::ALT);
        match key.code {
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
            KeyCode::PageUp => self.selected = self.selected.saturating_sub(LIST_ROWS),
            KeyCode::PageDown => {
                self.selected = (self.selected + LIST_ROWS).min(count.saturating_sub(1))
            }
            KeyCode::Tab => self.cycle(1, groups),
            KeyCode::BackTab => self.cycle(-1, groups),
            KeyCode::Backspace => {
                self.filter.pop();
                self.selected = 0;
            }
            KeyCode::Char(c) if !c.is_control() && mods.is_empty() => {
                self.filter.push(c);
                self.selected = 0;
            }
            _ => return false,
        }
        true
    }

    fn cycle(&mut self, delta: isize, groups: usize) {
        let n = groups as isize + 1;
        let now = self.group.map_or(0, |g| g as isize + 1);
        let next = (now + delta).rem_euclid(n);
        self.group = (next > 0).then(|| next as usize - 1);
        self.selected = 0;
    }
}

/// Whether `hay` contains every word (lowercase) of a filter.
pub fn matches(hay: &str, words: &[String]) -> bool {
    let hay = hay.to_lowercase();
    words.iter().all(|w| hay.contains(w.as_str()))
}

/// Which characters of `text` fall inside a case-insensitive match of a word.
fn hits(text: &str, words: &[String]) -> Vec<bool> {
    let lower: Vec<char> = text
        .chars()
        .map(|c| c.to_lowercase().next().unwrap_or(c))
        .collect();
    let mut marks = vec![false; lower.len()];
    for w in words {
        let w: Vec<char> = w.chars().collect();
        if w.is_empty() || w.len() > lower.len() {
            continue;
        }
        for start in 0..=lower.len() - w.len() {
            if lower[start..start + w.len()] == w[..] {
                marks[start..start + w.len()]
                    .iter_mut()
                    .for_each(|m| *m = true);
            }
        }
    }
    marks
}

/// `text` in `base`, with the characters that match the filter `words` in `hit`.
pub fn highlight(text: &str, words: &[String], base: Style, hit: Style) -> String {
    let marks = hits(text, words);
    let mut out = String::new();
    let mut run = String::new();
    let mut on = false;
    let flush = |run: &mut String, on: bool, out: &mut String| {
        if !run.is_empty() {
            out.push_str(
                &(if on { hit } else { base })
                    .paint(run.as_str())
                    .to_string(),
            );
            run.clear();
        }
    };
    for (c, m) in text.chars().zip(marks) {
        if m != on {
            flush(&mut run, on, &mut out);
            on = m;
        }
        run.push(c);
    }
    flush(&mut run, on, &mut out);
    out
}

/// A short colored state next to a row (`disabled`, `asks`, ...).
pub struct Badge {
    pub text: String,
    pub color: Color,
}

impl Badge {
    pub fn new(text: impl Into<String>, color: Color) -> Self {
        Self {
            text: text.into(),
            color,
        }
    }
}

pub struct Row {
    pub label: String,
    pub detail: String,
    pub badges: Vec<Badge>,
    /// Drawn faint (a disabled skill or tool).
    pub dim: bool,
}

/// One frame of a list menu.
pub struct View<'a> {
    pub title: &'a str,
    /// Groups after "All", with how many rows each has under the filter.
    pub tabs: Vec<(String, usize)>,
    pub group: Option<usize>,
    /// Rows under the filter in every group.
    pub total: usize,
    pub filter: &'a str,
    /// Rows of the chosen group.
    pub rows: Vec<Row>,
    pub selected: usize,
    /// Shown instead of rows when there are none.
    pub empty: Vec<String>,
    /// About the selected row: the first line bright, the rest gray.
    pub preview: Vec<String>,
    /// A confirmation, progress or result line, and its color.
    pub note: Option<(String, Color)>,
    /// An input line: its prompt and text.
    pub input: Option<(String, String)>,
    pub hints: Vec<(&'a str, &'a str)>,
}

fn paint(style: Style, s: &str) -> String {
    if s.is_empty() {
        String::new()
    } else {
        style.paint(s).to_string()
    }
}

/// Plain text cut into rows of at most `width` columns, breaking at spaces
/// where it can; at most `max` rows, the last one ending in `…` when cut.
pub fn wrap_text(text: &str, width: usize, max: usize) -> Vec<String> {
    let width = width.max(1);
    let mut rows: Vec<String> = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        let w = visible_width(word);
        let lw = visible_width(&line);
        if !line.is_empty() && lw + 1 + w > width {
            rows.push(std::mem::take(&mut line));
        }
        if w > width {
            // A word longer than the row: cut it character by character.
            for c in word.chars() {
                if visible_width(&line) + c.width().unwrap_or(0) > width {
                    rows.push(std::mem::take(&mut line));
                }
                line.push(c);
            }
            continue;
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        rows.push(line);
    }
    if rows.len() > max {
        rows.truncate(max);
        if let Some(last) = rows.last_mut() {
            *last = truncate(&format!("{last} …"), width);
            if !last.ends_with('…') {
                last.push('…');
            }
        }
    }
    rows
}

/// `left` and `right` on one row, right-aligned when they fit.
fn spread(left: &str, right: &str, width: usize) -> String {
    let gap = width.saturating_sub(visible_width(left) + visible_width(right));
    if right.is_empty() || gap < 2 {
        left.to_string()
    } else {
        format!("{left}{}{right}", " ".repeat(gap))
    }
}

/// Key hints packed into as few rows of `width` as they need.
fn hint_rows(pairs: &[(&str, &str)], width: usize) -> Vec<String> {
    let mut rows = Vec::new();
    let (mut row, mut used) = (String::new(), 0);
    for (k, a) in pairs {
        let w = visible_width(k) + 1 + visible_width(a);
        if used > 0 && used + 3 + w > width {
            rows.push(std::mem::take(&mut row));
            used = 0;
        }
        if used > 0 {
            row.push_str("   ");
            used += 3;
        }
        row.push_str(&format!("{} {}", paint(fg(BRIGHT), k), paint(fg(GRAY), a)));
        used += w;
    }
    rows.push(row);
    rows
}

/// The group tabs, `[All N]` first; when they don't fit, tabs before the
/// chosen one give way (marked `‹`).
fn tab_row(v: &View, width: usize) -> String {
    let tabs: Vec<(String, bool)> =
        std::iter::once((format!("All {}", v.total), v.group.is_none()))
            .chain(v.tabs.iter().enumerate().map(|(i, (label, n))| {
                (format!("{} {n}", sanitize_text(label)), v.group == Some(i))
            }))
            .collect();
    let shown = |from: usize| -> (String, usize) {
        let mut out = String::new();
        let mut w = 0;
        if from > 0 {
            out.push_str(&paint(fg(FAINT), "‹ "));
            w += 2;
        }
        for (i, (t, on)) in tabs.iter().enumerate().skip(from) {
            if i > from {
                out.push_str("  ");
                w += 2;
            }
            let text = if *on { format!("[{t}]") } else { t.clone() };
            w += visible_width(&text);
            out.push_str(&paint(if *on { fg(BRIGHT) } else { fg(GRAY) }, &text));
        }
        (out, w)
    };
    let active = tabs.iter().position(|(_, on)| *on).unwrap_or(0);
    let mut from = 0;
    loop {
        let (row, w) = shown(from);
        if w <= width || from >= active {
            return row;
        }
        from += 1;
    }
}

/// Draw `v` in `width` columns, using at most about `height` rows (the
/// list shrinks first).
pub fn render(v: &View, width: usize, height: usize) -> Vec<String> {
    let w = width.max(20);
    let rule = || paint(fg(FAINT), &"─".repeat(w));
    let mut top = vec![rule()];
    let count = format!("{} ", v.rows.len());
    let prefix = format!(
        "{} {}",
        paint(fg(BRIGHT).bold(), v.title),
        paint(fg(GRAY), &count)
    );
    let room = w.saturating_sub(visible_width(&prefix) + 1);
    top.push(if v.filter.is_empty() {
        format!(
            "{prefix} {}",
            paint(fg(FAINT), &truncate("Type to filter", room))
        )
    } else {
        format!(
            "{prefix} {}",
            paint(fg(MID), &truncate(&sanitize_text(v.filter), room))
        )
    });
    top.push(tab_row(v, w));
    top.push(String::new());

    let mut bottom = Vec::new();
    if !v.rows.is_empty() && !v.preview.is_empty() {
        bottom.push(paint(
            fg(FAINT),
            &format!("  {}", "╌".repeat(w.saturating_sub(4))),
        ));
        for (i, line) in v.preview.iter().enumerate() {
            let style = if i == 0 { fg(MID) } else { fg(GRAY) };
            for row in wrap_text(&sanitize_text(line), w.saturating_sub(4), 2) {
                bottom.push(paint(style, &format!("  {row}")));
            }
        }
    }
    if let Some((note, color)) = &v.note {
        for row in wrap_text(&sanitize_text(note), w.saturating_sub(2), 3) {
            bottom.push(paint(fg(*color), &format!("  {row}")));
        }
    }
    if let Some((prompt, text)) = &v.input {
        let room = w.saturating_sub(visible_width(prompt) + 5).max(1);
        let chars: Vec<char> = text.chars().collect();
        let shown: String = chars[chars.len().saturating_sub(room)..].iter().collect();
        bottom.push(format!(
            "{} {} {}{}",
            paint(fg(YELLOW), "┃"),
            paint(fg(BRIGHT).bold(), prompt),
            paint(fg(BRIGHT), &sanitize_text(&shown)),
            paint(fg(BRIGHT), "▏")
        ));
    }
    bottom.push(rule());
    let mut hints = hint_rows(&v.hints, w);

    let mut list = Vec::new();
    if v.rows.is_empty() {
        for line in &v.empty {
            for row in wrap_text(line, w.saturating_sub(4), 4) {
                list.push(paint(fg(GRAY), &format!("  {row}")));
            }
        }
    }
    // Keep room for the input line and a margin above the menu.
    let fixed = top.len() + bottom.len() + hints.len() + 3;
    let max_rows = height.saturating_sub(fixed).clamp(3, LIST_ROWS);
    let selected = v.selected.min(v.rows.len().saturating_sub(1));
    let start = selected.saturating_sub(max_rows - 1);
    let label_w = v
        .rows
        .iter()
        .map(|r| visible_width(&r.label))
        .max()
        .unwrap_or(0)
        .min(LABEL_MAX)
        + 2;
    let words: Vec<String> = v.filter.split_whitespace().map(str::to_lowercase).collect();
    for (i, row) in v.rows.iter().enumerate().skip(start).take(max_rows) {
        let sel = i == selected;
        let lead = if sel {
            paint(fg(BRIGHT), "› ")
        } else {
            "  ".into()
        };
        let badges: Vec<String> = row
            .badges
            .iter()
            .map(|b| paint(fg(b.color), &b.text))
            .collect();
        let badges = badges.join(&paint(fg(FAINT), " · "));
        let badge_w = visible_width(&badges);
        let label_w = label_w.min(w.saturating_sub(badge_w + 6).max(4));
        let label = truncate(&sanitize_text(&row.label), label_w - 1);
        let pad = " ".repeat(label_w.saturating_sub(visible_width(&label)));
        let (base, detail_style) = match (sel, row.dim) {
            (_, true) => (fg(FAINT), fg(FAINT)),
            (true, false) => (fg(BRIGHT).bold(), fg(GRAY)),
            (false, false) => (fg(MID), fg(GRAY)),
        };
        let hit = fg(YELLOW).bold().underline();
        let room = w.saturating_sub(2 + label_w + badge_w + 2);
        let detail = truncate(&sanitize_text(&row.detail), room);
        let left = format!(
            "{lead}{}{pad}{}",
            highlight(&label, &words, base, hit),
            highlight(&detail, &words, detail_style, hit)
        );
        list.push(spread(&left, &badges, w));
    }
    if !v.rows.is_empty() {
        let range = format!(
            "{}–{} of {}",
            start + 1,
            (start + max_rows).min(v.rows.len()),
            v.rows.len()
        );
        let last = hints.last_mut().expect("a hint row");
        *last = spread(last, &paint(fg(GRAY), &range), w);
    }
    top.into_iter()
        .chain(list)
        .chain(std::iter::once(String::new()))
        .chain(bottom)
        .chain(hints)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::render::strip_ansi;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn list_keys_move_filter_and_cycle_groups() {
        let mut l = List::default();
        assert!(l.on_key(key(KeyCode::Up), 3, 2));
        assert_eq!(l.selected, 2);
        assert!(l.on_key(key(KeyCode::Down), 3, 2));
        assert_eq!(l.selected, 0);
        l.on_key(key(KeyCode::BackTab), 3, 2);
        assert_eq!(l.group, Some(1));
        l.on_key(key(KeyCode::Tab), 3, 2);
        assert_eq!(l.group, None);
        l.on_key(key(KeyCode::Char('P')), 3, 2);
        l.on_key(key(KeyCode::Char('d')), 3, 2);
        assert_eq!(l.words(), ["pd"]);
        assert!(!l.on_key(
            KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL),
            3,
            2
        ));
        assert!(!l.on_key(key(KeyCode::Enter), 3, 2));
    }

    #[test]
    fn highlights_matched_characters() {
        let words = vec!["pdf".to_string(), "orm".to_string()];
        assert!(matches("Fill PDF forms", &words));
        assert!(!matches("Fill PDF", &words));
        assert_eq!(
            hits("a PdF form", &words),
            [false, false, true, true, true, false, false, true, true, true]
        );
        let painted = highlight("PdF x", &words, Style::new(), Style::new().bold());
        assert_eq!(strip_ansi(&painted), "PdF x");
        assert!(painted.starts_with("\x1b[1mPdF"), "{painted:?}");
    }

    #[test]
    fn wraps_text_and_hints() {
        assert_eq!(
            wrap_text("one two three four", 9, 5),
            ["one two", "three", "four"]
        );
        assert_eq!(
            wrap_text("one two three four", 9, 2),
            ["one two", "three …"]
        );
        assert_eq!(wrap_text("abcdefghij", 4, 5), ["abcd", "efgh", "ij"]);
        let rows = hint_rows(&[("Enter", "Run"), ("^E", "Args"), ("Esc", "Close")], 20);
        let rows: Vec<String> = rows.iter().map(|r| strip_ansi(r)).collect();
        assert_eq!(rows, ["Enter Run   ^E Args", "Esc Close"]);
    }

    #[test]
    fn renders_within_80_columns() {
        let rows = (0..12)
            .map(|i| Row {
                label: format!("a-rather-long-skill-name-number-{i}"),
                detail: "A description that goes on for quite a while and will not fit in the row"
                    .into(),
                badges: vec![Badge::new("disabled", GRAY), Badge::new("asks", YELLOW)],
                dim: i == 3,
            })
            .collect();
        let v = View {
            title: "Skills",
            tabs: [
                "nyq",
                "Agents",
                "Claude Code",
                "Codex",
                "OpenCode",
                "Claw",
                "Workspace",
            ]
            .iter()
            .map(|t| (t.to_string(), 12))
            .collect(),
            group: Some(6),
            total: 84,
            filter: "skill",
            rows,
            selected: 9,
            empty: Vec::new(),
            preview: vec!["Description ".repeat(20), "Claude Code · project".into()],
            note: Some(("Copy it?".into(), YELLOW)),
            input: Some(("Install from".into(), "x".repeat(200))),
            hints: vec![
                ("Enter", "Run"),
                ("^E", "Args"),
                ("→", "View"),
                ("Tab", "Source"),
                ("^T", "On/off"),
                ("^O", "Copy to nyq"),
                ("^N", "Install"),
                ("Del", "Remove"),
                ("Esc", "Close"),
            ],
        };
        let out = render(&v, 80, 40);
        for row in &out {
            assert!(
                visible_width(row) <= 80,
                "{} > 80: {}",
                visible_width(row),
                strip_ansi(row)
            );
        }
        let text = strip_ansi(&out.join("\n"));
        assert!(
            text.contains("Skills 12") && text.contains("[Workspace 12]") && text.contains("‹"),
            "{text}"
        );
        assert!(
            text.contains("› a-rather-long") && text.contains("disabled · asks"),
            "{text}"
        );
        assert!(
            text.contains("3–10 of 12")
                && text.contains("Copy it?")
                && text.contains("Install from"),
            "{text}"
        );
        // A short terminal keeps the hints and shrinks the list.
        let short = render(&v, 80, 20);
        assert!(short.len() < out.len());
        assert!(strip_ansi(short.last().unwrap()).contains("of 12"));
    }
}
