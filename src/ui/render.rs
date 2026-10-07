//! Pure text renderers: markdown-lite lines and colored diffs. Every function
//! takes a `color` flag; with `color == false` the input text is preserved.

use nu_ansi_term::{Color, Style};
use unicode_width::UnicodeWidthChar;

const CODE_BLOCK: Color = Color::Rgb(208, 208, 208);
const INLINE_CODE: Color = Color::Rgb(138, 138, 138);
const TABLE_LINE: Color = Color::Rgb(88, 88, 88);

pub fn dim() -> Style {
    Style::new().dimmed()
}

/// Paint `s` with `style` when `color` is on.
pub fn paint(color: bool, style: Style, s: &str) -> String {
    if color && !s.is_empty() {
        style.paint(s).to_string()
    } else {
        s.to_string()
    }
}

/// First line of `s`, trimmed and capped at `max` chars; appends `…` when
/// anything was cut.
/// Drop terminal controls from untrusted text. Newlines stay, tabs expand to
/// stops of 8, and CSI / OSC sequences are consumed.
pub fn sanitize_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    let mut col = 0usize;
    while let Some(c) = chars.next() {
        match c {
            '\n' => {
                out.push('\n');
                col = 0;
            }
            '\t' => {
                let n = 8 - (col % 8);
                out.extend(std::iter::repeat_n(' ', n));
                col += n;
            }
            '\u{1b}' => consume_escape(&mut chars),
            c if c.is_control() => {}
            c => {
                out.push(c);
                col += c.width().unwrap_or(0);
            }
        }
    }
    out
}

fn consume_escape(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    match chars.peek().copied() {
        Some('[') => {
            chars.next();
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        }
        Some(']') => {
            chars.next();
            while let Some(c) = chars.next() {
                if c == '\u{7}' {
                    break;
                }
                if c == '\u{1b}' {
                    if chars.peek() == Some(&'\\') {
                        chars.next();
                    }
                    break;
                }
            }
        }
        Some(c) if c.is_ascii() && !c.is_control() => {
            chars.next();
        }
        _ => {}
    }
}

pub fn one_line(s: &str, max: usize) -> String {
    let s = sanitize_text(s);
    let s = s.trim();
    let first = s.lines().next().unwrap_or("").trim_end();
    let mut out: String = first.chars().take(max).collect();
    if out.len() < first.len() || first.len() < s.len() {
        out.push('…');
    }
    out
}

/// Line-at-a-time markdown styler. Tracks fenced code blocks across lines.
#[derive(Default)]
pub struct Markdown {
    fence: Option<(char, usize)>,
}

impl Markdown {
    pub fn line(&mut self, line: &str, color: bool) -> String {
        let trimmed = line.trim_start();
        if let Some((ch, n)) = self.fence {
            let t = trimmed.trim_end();
            if t.len() >= n && t.chars().all(|c| c == ch) {
                self.fence = None;
                return paint(color, dim(), line);
            }
            return paint(color, Style::new().fg(CODE_BLOCK), line);
        }
        for ch in ['`', '~'] {
            let n = trimmed.chars().take_while(|&c| c == ch).count();
            if n >= 3 {
                self.fence = Some((ch, n));
                return paint(color, dim(), line);
            }
        }
        if !color {
            return line.to_string();
        }
        let hashes = trimmed.chars().take_while(|&c| c == '#').count();
        if (1..=6).contains(&hashes) && trimmed[hashes..].starts_with(' ') {
            let text = trimmed[hashes..].trim().replace("**", "").replace('`', "");
            return Style::new().bold().paint(text).to_string();
        }
        let indent = &line[..line.len() - trimmed.len()];
        if let Some(rest) = trimmed.strip_prefix("- ").or_else(|| trimmed.strip_prefix("* ")) {
            return format!("{indent}{} {}", dim().paint("•"), inline(rest, color));
        }
        inline(line, color)
    }
}

/// Style `inline code` spans and **bold** runs within one line.
pub fn inline(s: &str, color: bool) -> String {
    if !color {
        return s.to_string();
    }
    let b = s.as_bytes();
    let (mut out, mut i, mut lit) = (String::new(), 0, 0);
    while i < b.len() {
        if b[i] == b'`' {
            let n = b[i..].iter().take_while(|&&c| c == b'`').count();
            let ticks = &s[i..i + n];
            if let Some(j) = s[i + n..].find(ticks).map(|j| j + i + n) {
                if j > i + n {
                    out.push_str(&s[lit..i]);
                    out.push_str(&Style::new().fg(INLINE_CODE).paint(&s[i + n..j]).to_string());
                    i = j + n;
                    lit = i;
                    continue;
                }
            }
            i += n;
        } else if b[i..].starts_with(b"**") {
            if let Some(j) = s[i + 2..].find("**").map(|j| j + i + 2) {
                if j > i + 2 {
                    out.push_str(&s[lit..i]);
                    out.push_str(&Style::new().bold().paint(&s[i + 2..j]).to_string());
                    i = j + 2;
                    lit = i;
                    continue;
                }
            }
            i += 2;
        } else {
            i += 1;
        }
    }
    out.push_str(&s[lit..]);
    out
}

/// Display width of text that may contain ANSI escapes.
pub fn visible_width(s: &str) -> usize {
    let mut w = 0;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            w += c.width().unwrap_or(0);
        }
    }
    w
}

/// Cut plain text to at most `width` columns, marking the cut with `…`.
pub fn truncate(s: &str, width: usize) -> String {
    let total: usize = s.chars().map(|c| c.width().unwrap_or(0)).sum();
    if total <= width {
        return s.to_string();
    }
    let mut out = String::new();
    let mut w = 0;
    for c in s.chars() {
        let cw = c.width().unwrap_or(0);
        if w + cw + 1 > width {
            break;
        }
        w += cw;
        out.push(c);
    }
    if width > 0 {
        out.push('…');
    }
    out
}

/// Cells of a markdown table row: outer pipes dropped, `\|` unescaped.
pub fn table_cells(line: &str) -> Vec<String> {
    let t = line.trim();
    let t = t.strip_prefix('|').unwrap_or(t);
    let t = if t.ends_with('|') && !t.ends_with("\\|") { &t[..t.len() - 1] } else { t };
    let mut cells = vec![String::new()];
    let mut chars = t.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&'|') => cells.last_mut().unwrap().push(chars.next().unwrap()),
            '|' => cells.push(String::new()),
            c => cells.last_mut().unwrap().push(c),
        }
    }
    cells.iter().map(|c| c.trim().to_string()).collect()
}

/// A table's header separator row, like `|---|:--:|`.
pub fn is_table_sep(line: &str) -> bool {
    line.contains('-')
        && table_cells(line).iter().all(|c| {
            let d = c.strip_prefix(':').unwrap_or(c);
            let d = d.strip_suffix(':').unwrap_or(d);
            !d.is_empty() && d.chars().all(|ch| ch == '-')
        })
}

/// Column widths for `natural` widths and `sep`-wide gaps within `width`:
/// the widest columns give way first, down to 3 columns each.
fn fit_columns(natural: &[usize], sep: usize, width: usize) -> Vec<usize> {
    let mut w = natural.to_vec();
    let budget = width.saturating_sub(sep * w.len().saturating_sub(1));
    while w.iter().sum::<usize>() > budget {
        let (i, &max) = w.iter().enumerate().max_by_key(|(i, &c)| (c, std::cmp::Reverse(*i))).unwrap();
        if max <= 3 {
            break;
        }
        w[i] -= 1;
    }
    w
}

/// Lay out a markdown table (header, separator and body lines) in `width`
/// columns with light box-drawing separators.
pub fn table(lines: &[String], width: usize, color: bool) -> Vec<String> {
    let line_style = Style::new().fg(TABLE_LINE);
    let header_style = Style::new().fg(Color::Rgb(238, 238, 238)).bold();
    let aligns: Vec<(bool, bool)> = lines.get(1).map(|l| table_cells(l)).unwrap_or_default().iter().map(|c| (c.starts_with(':'), c.ends_with(':'))).collect();
    let n = aligns.len().max(1);
    let rows: Vec<Vec<String>> = lines
        .iter()
        .take(1)
        .chain(lines.iter().skip(2))
        .map(|l| {
            let mut cells: Vec<String> = table_cells(l).into_iter().map(|c| inline(&c, color)).collect();
            cells.resize(n, String::new());
            cells
        })
        .collect();
    let natural: Vec<usize> = (0..n).map(|c| rows.iter().map(|r| visible_width(&r[c])).max().unwrap_or(0).max(1)).collect();
    let widths = fit_columns(&natural, 3, width);
    let mut out = Vec::new();
    for (r, row) in rows.iter().enumerate() {
        let cells: Vec<String> = row
            .iter()
            .zip(&widths)
            .enumerate()
            .map(|(c, (cell, &w))| {
                let text = if visible_width(cell) > w { truncate(&strip_ansi(cell), w) } else { cell.clone() };
                let pad = w.saturating_sub(visible_width(&text));
                let (left, right) = match aligns.get(c).copied().unwrap_or_default() {
                    (true, true) => (pad / 2, pad - pad / 2),
                    (false, true) => (pad, 0),
                    _ if c + 1 == n => (0, 0),
                    _ => (0, pad),
                };
                // The header is uniformly bright and bold (inline styles dropped).
                let text = if r == 0 { paint(color, header_style, &strip_ansi(&text)) } else { text };
                format!("{}{text}{}", " ".repeat(left), " ".repeat(right))
            })
            .collect();
        out.push(cells.join(&paint(color, line_style, " │ ")).trim_end().to_string());
        if r == 0 {
            let rule: Vec<String> = widths.iter().map(|&w| "─".repeat(w)).collect();
            out.push(paint(color, line_style, &rule.join("─┼─")));
        }
    }
    out
}

/// `s` without ANSI escape sequences.
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

pub fn looks_like_diff(s: &str) -> bool {
    s.lines().any(|l| l.starts_with("@@")) || (s.contains("\n+++ ") && s.starts_with("--- "))
}

/// Render at most `max` lines of `text`, each prefixed by `indent` and styled
/// by `style`, plus a `… N more lines` note when truncated.
pub fn capped<'a>(
    lines: impl Iterator<Item = &'a str>,
    max: usize,
    width: usize,
    indent: &str,
    color: bool,
    style: impl Fn(&str) -> String,
) -> String {
    let budget = width.saturating_sub(visible_width(indent));
    let lines: Vec<&str> = lines.collect();
    let mut out = String::new();
    for l in lines.iter().take(max) {
        let plain = truncate(l.trim_end_matches('\r'), budget);
        out.push_str(indent);
        out.push_str(&style(&plain));
        out.push('\n');
    }
    if lines.len() > max {
        let note = format!("… {} more lines", lines.len() - max);
        out.push_str(&format!("{indent}{}\n", paint(color, dim(), &note)));
    }
    out
}

/// Colorize a unified diff (+ green, - red, @@ cyan). File header lines
/// before the first hunk are dropped when hunks are present.
pub fn diff(text: &str, color: bool, max: usize, width: usize, indent: &str) -> String {
    let has_hunks = text.lines().any(|l| l.starts_with("@@"));
    let mut seen_hunk = false;
    let lines = text.lines().filter(move |l| {
        seen_hunk |= l.starts_with("@@");
        !(has_hunks && !seen_hunk && (l.starts_with("--- ") || l.starts_with("+++ ")))
    });
    capped(lines, max, width, indent, color, |l| {
        let style = if l.starts_with("+++ ") || l.starts_with("--- ") {
            Style::new().bold()
        } else if l.starts_with('+') {
            Style::new().fg(Color::Green)
        } else if l.starts_with('-') {
            Style::new().fg(Color::Red)
        } else if l.starts_with("@@") {
            Style::new().fg(Color::Cyan)
        } else if l.starts_with('\\') {
            dim()
        } else {
            Style::new()
        };
        paint(color, style, l)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(md: &str, color: bool) -> String {
        let mut m = Markdown::default();
        md.lines().map(|l| m.line(l, color)).collect::<Vec<_>>().join("\n")
    }

    #[test]
    fn plain_mode_preserves_text() {
        let src = "# Title\n\nSome `code` and **bold**.\n```rust\nfn main() {}\n```\n- item";
        assert_eq!(render(src, false), src);
    }

    #[test]
    fn headings_are_bold_without_hashes() {
        let out = render("## Hello `x`", true);
        assert!(out.contains("\x1b[1m"));
        assert!(out.contains("Hello x"));
        assert!(!out.contains('#'));
        // Not a heading: no space after hashes.
        assert_eq!(render("#hashtag", true), "#hashtag");
    }

    #[test]
    fn inline_code_and_bold() {
        let out = inline("run `cargo test` **now**", true);
        assert!(out.starts_with("run "));
        assert!(out.contains("cargo test") && !out.contains('`'));
        assert!(out.contains("\x1b[1mnow"));
        assert!(!out.contains("**"));
        // Unclosed markers stay literal.
        assert_eq!(inline("a `b and **c", true), "a `b and **c");
        assert_eq!(inline("2 ** 3", true), "2 ** 3");
    }

    #[test]
    fn code_blocks_are_not_markdown_styled() {
        let mut m = Markdown::default();
        let fence = m.line("```", true);
        assert!(fence.contains("```"));
        let body = m.line("# not a heading **x**", true);
        assert!(body.contains("# not a heading **x**"));
        assert!(m.line("````", true).contains("````")); // closes (longer fence ok)
        assert!(m.line("# real", true).contains("\x1b[1m"));
    }

    #[test]
    fn bullets_and_unicode() {
        let out = render("  - héllo `wörld`", true);
        assert!(out.starts_with("  ") && out.contains('•') && out.contains("héllo") && out.contains("wörld"));
    }

    #[test]
    fn diff_colors_and_caps() {
        let d = "--- a/x\n+++ b/x\n@@ -1,2 +1,2 @@\n ctx\n-old\n+new";
        let out = diff(d, true, 60, usize::MAX, "  ");
        assert!(!out.contains("a/x")); // header dropped
        assert!(out.contains(&Color::Green.paint("+new").to_string()));
        assert!(out.contains(&Color::Red.paint("-old").to_string()));
        assert!(out.contains(&Color::Cyan.paint("@@ -1,2 +1,2 @@").to_string()));
        assert_eq!(diff(d, false, 60, usize::MAX, ""), "@@ -1,2 +1,2 @@\n ctx\n-old\n+new\n");
        let long: String = (0..100).map(|i| format!("+{i}\n")).collect();
        let out = diff(&long, false, 60, usize::MAX, "");
        assert_eq!(out.lines().count(), 61);
        assert!(out.ends_with("… 40 more lines\n"));
    }

    #[test]
    fn diff_lines_are_width_limited() {
        let long = "x".repeat(400);
        let out = diff(&long, false, 5, 20, "");
        let line = out.lines().next().unwrap();
        assert!(line.chars().count() <= 20, "{line}");
        assert!(line.ends_with('…'));
        let colored = diff(&format!("+{long}"), true, 5, 24, "  ");
        assert!(visible_width(colored.lines().next().unwrap()) <= 24);
    }

    #[test]
    fn diff_detection_and_one_line() {
        assert!(looks_like_diff("@@ -1 +1 @@\n-a\n+b"));
        assert!(!looks_like_diff("just some text\n- a list"));
        assert_eq!(one_line("  hello  ", 10), "hello");
        assert_eq!(one_line("abcdef", 3), "abc…");
        assert_eq!(one_line("a\nb", 10), "a…");
    }

    #[test]
    fn sanitize_strips_terminal_controls() {
        assert_eq!(sanitize_text("a\nb"), "a\nb");
        assert_eq!(sanitize_text("a\tb"), format!("a{}b", " ".repeat(7)));
        assert_eq!(sanitize_text("\u{1b}[2Jhi"), "hi");
        assert_eq!(sanitize_text("\u{1b}]0;title\u{7}ok"), "ok");
        assert_eq!(sanitize_text("\u{1b}]52;c;eA==\u{1b}\\z"), "z");
        assert_eq!(sanitize_text("a\u{8}b\r"), "ab");
        let once = sanitize_text("a\tb\u{1b}[0m");
        assert_eq!(sanitize_text(&once), once);
    }

    fn lines(md: &str) -> Vec<String> {
        md.lines().map(str::to_string).collect()
    }

    #[test]
    fn table_cells_and_separators() {
        assert_eq!(table_cells("| a | b |"), vec!["a", "b"]);
        assert_eq!(table_cells("a|b"), vec!["a", "b"]);
        assert_eq!(table_cells(r"| x \| y | |"), vec!["x | y", ""]);
        assert!(is_table_sep("|---|:--:|---:|"));
        assert!(is_table_sep("--- | ---"));
        assert!(!is_table_sep("| a | b |"));
        assert!(!is_table_sep("|   |   |"));
        assert!(!is_table_sep(""));
    }

    #[test]
    fn table_aligns_columns() {
        let t = lines("| Name | Size |\n|------|-----:|\n| main.rs | 12 |\n| a | 1234 |");
        assert_eq!(
            table(&t, 80, false),
            vec!["Name    │ Size", "────────┼─────", "main.rs │   12", "a       │ 1234"]
        );
        // Centered columns, missing and extra cells.
        let t = lines("|a|b|c|\n|:-:|---|---|\n|x|\n|1|2|3|4|");
        assert_eq!(table(&t, 80, false), vec!["a │ b │ c", "──┼───┼──", "x │   │", "1 │ 2 │ 3"]);
    }

    #[test]
    fn wide_tables_shrink_the_widest_columns() {
        let t = lines("| id | description |\n|---|---|\n| 1 | a very long description that does not fit |");
        let out = table(&t, 24, false);
        assert!(out.iter().all(|l| visible_width(l) <= 24), "{out:?}");
        assert_eq!(out[0], "id │ description");
        assert_eq!(out[2], "1  │ a very long descri…");
        // Columns never shrink below three cells; then the row just overflows.
        let t = lines("|aaaaaa|bbbbbb|\n|-|-|\n|x|y|");
        assert_eq!(table(&t, 4, false)[0], "aa… │ bb…");
        assert_eq!(fit_columns(&[10, 4, 10], 3, 20), vec![5, 4, 5]);
        assert_eq!(fit_columns(&[2, 3], 3, 80), vec![2, 3]);
    }

    #[test]
    fn table_with_color_keeps_widths() {
        let t = lines("| k | v |\n|---|---|\n| `code` | **bold** |");
        let out = table(&t, 80, true);
        assert_eq!(visible_width(&out[2]), "code │ bold".chars().count());
        assert!(!strip_ansi(&out[2]).contains('`'));
        assert_eq!(strip_ansi(&out[0]), "k    │ v");
    }
}
