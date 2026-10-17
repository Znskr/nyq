//! Pure text renderers: markdown-lite lines and colored diffs. Every function
//! takes a `color` flag; with `color == false` the input text is preserved.

use nu_ansi_term::Style;
use unicode_width::UnicodeWidthChar;

use super::highlight::Code;
use super::theme::{fg, BLUE, BRIGHT, FAINT, GRAY, GREEN, MID, RED};


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

/// Width of horizontal rules and code-block frames.
pub const RULE: usize = 34;

/// An open fenced code block.
pub struct Fence {
    ch: char,
    n: usize,
    /// The info string (`rust`, `sh`, …); empty when none.
    pub lang: String,
    code: Option<Code>,
}

impl Fence {
    /// The fence `line` opens, if it is one.
    pub fn open(line: &str) -> Option<Fence> {
        let t = line.trim_start();
        let ch = t.chars().next().filter(|c| matches!(c, '`' | '~'))?;
        let n = t.chars().take_while(|&c| c == ch).count();
        let lang = t[n..].trim();
        (n >= 3 && !(ch == '`' && lang.contains('`'))).then(|| Fence { ch, n, lang: lang.to_string(), code: Code::new(lang) })
    }

    pub fn closes(&self, line: &str) -> bool {
        let t = line.trim();
        t.len() >= self.n && t.chars().all(|c| c == self.ch)
    }

    /// A line inside the block, highlighted when the language is known.
    pub fn code_line(&mut self, line: &str, color: bool) -> String {
        match &mut self.code {
            _ if !color => line.to_string(),
            Some(code) => code.line(line),
            None => paint(color, fg(MID), line),
        }
    }
}

/// Line-at-a-time markdown styler. Tracks fenced code blocks across lines.
#[derive(Default)]
pub struct Markdown {
    fence: Option<Fence>,
}

/// `---`, `***` or `___` (spaces allowed between).
fn is_rule(t: &str) -> bool {
    let t = t.trim_end();
    let Some(ch) = t.chars().next().filter(|c| matches!(c, '-' | '*' | '_')) else { return false };
    t.chars().all(|c| c == ch || c == ' ') && t.chars().filter(|&c| c == ch).count() >= 3
}

/// `1. ` or `12) `: the marker's length.
fn number_marker(t: &str) -> Option<usize> {
    let digits = t.chars().take_while(char::is_ascii_digit).count();
    ((1..=9).contains(&digits) && (t[digits..].starts_with(". ") || t[digits..].starts_with(") "))).then_some(digits + 1)
}

/// A list item, quote or paragraph line (no indent).
fn body(t: &str) -> String {
    if let Some(rest) = t.strip_prefix("- ").or_else(|| t.strip_prefix("* ")).or_else(|| t.strip_prefix("+ ")) {
        return format!("{} {}", dim().paint("•"), inline(rest, true));
    }
    if let Some(n) = number_marker(t) {
        return format!("{} {}", fg(GRAY).paint(&t[..n]), inline(&t[n + 1..], true));
    }
    inline(t, true)
}

impl Markdown {
    pub fn line(&mut self, line: &str, color: bool) -> String {
        let trimmed = line.trim_start();
        if let Some(f) = &mut self.fence {
            if f.closes(line) {
                self.fence = None;
                return paint(color, dim(), line);
            }
            return f.code_line(line, color);
        }
        if let Some(f) = Fence::open(line) {
            self.fence = Some(f);
            return paint(color, dim(), line);
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
        if is_rule(trimmed) {
            return format!("{indent}{}", fg(FAINT).paint("─".repeat(RULE)));
        }
        if trimmed.starts_with('>') {
            let (mut depth, mut rest) = (0, trimmed);
            while let Some(r) = rest.strip_prefix('>') {
                depth += 1;
                rest = r.strip_prefix(' ').unwrap_or(r);
            }
            return format!("{indent}{}{}", fg(FAINT).paint("│ ".repeat(depth)), body(rest));
        }
        format!("{indent}{}", body(trimmed))
    }
}

/// Where the span opened by the delimiter at `i` closes: italics with `*`
/// or `_` hug their text, and `_` only marks whole words (not snake_case).
fn italic_end(s: &str, i: usize) -> Option<usize> {
    let b = s.as_bytes();
    let d = b[i];
    let word = |j: usize| b.get(j).is_some_and(|c| c.is_ascii_alphanumeric());
    if b.get(i + 1).is_none_or(|c| *c == b' ' || *c == d) || (d == b'_' && i > 0 && word(i - 1)) {
        return None;
    }
    let j = s[i + 1..].find(d as char)? + i + 1;
    (b[j - 1] != b' ' && b.get(j + 1) != Some(&d) && !(d == b'_' && word(j + 1))).then_some(j)
}

/// A bare `http(s)://` URL at `i`: its end, without trailing punctuation.
fn url_end(s: &str, i: usize) -> Option<usize> {
    let b = s.as_bytes();
    if !(b[i..].starts_with(b"https://") || b[i..].starts_with(b"http://")) || (i > 0 && b[i - 1].is_ascii_alphanumeric()) {
        return None;
    }
    let mut j = s[i..].find(|c: char| c.is_whitespace() || "<>\"'`".contains(c)).map_or(s.len(), |j| j + i);
    while j > i && (b".,;:!?".contains(&b[j - 1]) || (b[j - 1] == b')' && !s[i..j].contains('('))) {
        j -= 1;
    }
    (j > i + 8).then_some(j)
}

/// Style `inline code`, **bold**, *italic* and [links](url) within one
/// line; bare URLs become terminal hyperlinks.
pub fn inline(s: &str, color: bool) -> String {
    if !color {
        return s.to_string();
    }
    let b = s.as_bytes();
    let link_style = fg(BLUE).underline();
    let (mut out, mut i, mut lit) = (String::new(), 0, 0);
    while i < b.len() {
        let mut skip = 1;
        let span = match b[i] {
            b'`' => {
                let n = b[i..].iter().take_while(|&&c| c == b'`').count();
                skip = n;
                let ticks = &s[i..i + n];
                s[i + n..]
                    .find(ticks)
                    .map(|j| j + i + n)
                    .filter(|&j| j > i + n)
                    .map(|j| (j + n, fg(GRAY).paint(&s[i + n..j]).to_string()))
            }
            b'*' if b[i..].starts_with(b"**") => {
                skip = 2;
                s[i + 2..]
                    .find("**")
                    .map(|j| j + i + 2)
                    .filter(|&j| j > i + 2)
                    .map(|j| (j + 2, Style::new().bold().paint(&s[i + 2..j]).to_string()))
            }
            b'*' | b'_' => italic_end(s, i).map(|j| (j + 1, Style::new().italic().paint(&s[i + 1..j]).to_string())),
            b'[' => s[i..].find("](").map(|j| j + i).and_then(|mid| {
                let end = s[mid..].find(')')? + mid;
                let (text, url) = (&s[i + 1..mid], s[mid + 2..end].trim());
                (!text.is_empty() && !text.contains('[') && !url.is_empty() && !url.contains(' '))
                    .then(|| (end + 1, link(url, &link_style.paint(text).to_string())))
            }),
            b'h' => url_end(s, i).map(|j| (j, link(&s[i..j], &link_style.paint(&s[i..j]).to_string()))),
            _ => None,
        };
        match span {
            Some((end, text)) => {
                out.push_str(&s[lit..i]);
                out.push_str(&text);
                i = end;
                lit = i;
            }
            None => i += skip,
        }
    }
    out.push_str(&s[lit..]);
    out
}

/// Byte length of the escape sequence `s` starts with (`s` begins with ESC):
/// a CSI runs to its final letter, an OSC to BEL or ST.
pub fn escape_len(s: &str) -> usize {
    let b = s.as_bytes();
    match b.get(1) {
        Some(b'[') => b[2..].iter().position(u8::is_ascii_alphabetic).map_or(b.len(), |p| p + 3),
        Some(b']') => {
            let mut i = 2;
            while i < b.len() {
                match b[i] {
                    7 => return i + 1,
                    0x1b if b.get(i + 1) == Some(&b'\\') => return i + 2,
                    _ => i += 1,
                }
            }
            b.len()
        }
        Some(_) => 1 + s[1..].chars().next().map_or(0, char::len_utf8),
        None => 1,
    }
}

/// The pieces of styled text: `Ok(escape)` or `Err(char)`.
fn pieces(s: &str) -> impl Iterator<Item = Result<&str, char>> {
    let mut i = 0;
    std::iter::from_fn(move || {
        let rest = s.get(i..).filter(|r| !r.is_empty())?;
        if rest.starts_with('\x1b') {
            let n = escape_len(rest);
            i += n;
            Some(Ok(&rest[..n]))
        } else {
            let c = rest.chars().next()?;
            i += c.len_utf8();
            Some(Err(c))
        }
    })
}

/// Display width of text that may contain ANSI escapes.
pub fn visible_width(s: &str) -> usize {
    pieces(s).filter_map(Result::err).map(|c| c.width().unwrap_or(0)).sum()
}

/// `text` as a terminal hyperlink (OSC 8) to `url`.
pub fn link(url: &str, text: &str) -> String {
    format!("\x1b]8;;{url}\x1b\\{text}\x1b]8;;\x1b\\")
}

/// A `file://` URL for a path, absolute or relative to `cwd`.
pub fn file_url(cwd: &str, path: &str) -> Option<String> {
    let p = std::path::Path::new(path);
    let abs = if p.is_absolute() { p.to_path_buf() } else if cwd.is_empty() { return None } else { std::path::Path::new(cwd).join(p) };
    let s = abs.to_string_lossy().replace('\\', "/");
    let s = s.replace(' ', "%20");
    Some(if s.starts_with('/') { format!("file://{s}") } else { format!("file:///{s}") })
}

/// Wrap styled text to `width` columns, at spaces where possible. Rows after
/// the first start with `cont`; colors and links carry over to them.
pub fn wrap_styled(s: &str, width: usize, cont: &str) -> Vec<String> {
    let width = width.max(1);
    let cont = if visible_width(cont) + 8 > width { "" } else { cont };
    let cont_w = visible_width(cont);
    // Open SGR sequences since the last reset, and the open link.
    #[derive(Clone, Default)]
    struct State {
        sgr: Vec<String>,
        link: Option<String>,
    }
    impl State {
        fn close(&self, row: &mut String) {
            if self.link.is_some() {
                row.push_str("\x1b]8;;\x1b\\");
            }
            if !self.sgr.is_empty() {
                row.push_str("\x1b[0m");
            }
        }
        fn open(&self) -> String {
            self.sgr.concat() + self.link.as_deref().unwrap_or("")
        }
    }
    let mut rows = Vec::new();
    let (mut row, mut w, mut state) = (String::new(), 0, State::default());
    // Where the row can break: byte offset of a space, width before it, state there.
    let mut brk: Option<(usize, usize, State)> = None;
    for piece in pieces(s) {
        match piece {
            Ok(esc) => {
                row.push_str(esc);
                if esc.starts_with("\x1b]8;") {
                    let url = esc.trim_start_matches("\x1b]8;").split_once(';').map_or("", |(_, u)| u);
                    let url = url.trim_end_matches(['\x07', '\\']).trim_end_matches('\x1b');
                    state.link = (!url.is_empty()).then(|| esc.to_string());
                } else if esc.ends_with('m') && esc.starts_with("\x1b[") {
                    if matches!(&esc[2..esc.len() - 1], "" | "0") {
                        state.sgr.clear();
                    } else {
                        state.sgr.push(esc.to_string());
                    }
                }
            }
            Err(c) => {
                let cw = c.width().unwrap_or(0);
                if c == ' ' && w + cw > width && w > cont_w {
                    // The space that doesn't fit is the break itself.
                    state.close(&mut row);
                    rows.push(std::mem::take(&mut row));
                    row = format!("{cont}{}", state.open());
                    w = cont_w;
                    brk = None;
                    continue;
                }
                if w + cw > width && w > cont_w {
                    match brk.take().filter(|(_, bw, _)| *bw > cont_w) {
                        Some((at, _, at_state)) => {
                            let carry = row[at + 1..].to_string();
                            row.truncate(at);
                            at_state.close(&mut row);
                            rows.push(std::mem::take(&mut row));
                            row = format!("{cont}{}{carry}", at_state.open());
                            w = cont_w + visible_width(&carry);
                        }
                        None => {
                            state.close(&mut row);
                            rows.push(std::mem::take(&mut row));
                            row = format!("{cont}{}", state.open());
                            w = cont_w;
                        }
                    }
                }
                if c == ' ' {
                    brk = Some((row.len(), w, state.clone()));
                }
                row.push(c);
                w += cw;
            }
        }
    }
    rows.push(row);
    rows
}

/// What continuation rows of a wrapped `row` start with: its indent and
/// quote bars, with a list marker (`•`, `-`, `1.`) turned into spaces.
pub fn continuation(row: &str) -> String {
    let mut out = String::new();
    let mut marker = false;
    let mut rest = row;
    while let Some(c) = rest.chars().next() {
        if c == '\x1b' {
            let n = escape_len(rest);
            if rest[..n].ends_with('m') {
                out.push_str(&rest[..n]);
            }
            rest = &rest[n..];
            continue;
        }
        let digits = rest.chars().take_while(char::is_ascii_digit).count();
        // The next visible character after byte `k`, skipping escapes.
        let then = |k: usize| pieces(&rest[k..]).find_map(Result::err);
        let n = match c {
            ' ' => {
                out.push(' ');
                1
            }
            '┃' | '│' if !marker => {
                out.push(c);
                c.len_utf8()
            }
            '•' | '›' | '-' | '*' | '+' if !marker && then(c.len_utf8()) == Some(' ') => {
                marker = true;
                out.push(' ');
                c.len_utf8()
            }
            '0'..='9' if !marker && (1..=9).contains(&digits) && (rest[digits..].starts_with('.') || rest[digits..].starts_with(')')) && then(digits + 1) == Some(' ') => {
                marker = true;
                out.push_str(&" ".repeat(digits + 1));
                digits + 1
            }
            _ => break,
        };
        rest = &rest[n..];
    }
    if out.contains('\x1b') {
        out.push_str("\x1b[0m");
    }
    out
}

/// `row` wrapped to `width` with a hanging indent (see `continuation`).
pub fn wrap_row(row: &str, width: usize) -> Vec<String> {
    if visible_width(row) <= width {
        return vec![row.to_string()];
    }
    wrap_styled(row, width, &continuation(row))
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
    let line_style = fg(FAINT);
    let header_style = fg(BRIGHT).bold();
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
    pieces(s).filter_map(Result::err).collect()
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

/// Colorize a unified diff (+ green, - red, @@ blue). File header lines
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
            fg(GREEN)
        } else if l.starts_with('-') {
            fg(RED)
        } else if l.starts_with("@@") {
            fg(BLUE)
        } else if l.starts_with('\\') {
            dim()
        } else {
            Style::new()
        };
        paint(color, style, l)
    })
}

/// A unified diff as rows with a line-number gutter (old numbers for removed
/// lines, new ones otherwise). File headers are dropped and hunks are
/// separated by `⋯`. Text without hunks is colored as by `diff`.
pub fn numbered_diff(text: &str, color: bool, width: usize, indent: &str) -> Vec<String> {
    let mut lines: Vec<Option<(usize, char, &str)>> = Vec::new();
    let (mut old, mut new, mut seen) = (0usize, 0usize, false);
    for l in text.lines() {
        if let Some(h) = l.strip_prefix("@@") {
            let start = |sign: char| {
                h.split_whitespace().find_map(|p| p.strip_prefix(sign)).and_then(|p| p.split(',').next()?.parse::<usize>().ok()).unwrap_or(1)
            };
            if seen {
                lines.push(None);
            }
            (old, new, seen) = (start('-'), start('+'), true);
            continue;
        }
        match l.chars().next() {
            _ if !seen => {}
            Some('\\') => {}
            Some('+') => {
                lines.push(Some((new, '+', &l[1..])));
                new += 1;
            }
            Some('-') => {
                lines.push(Some((old, '-', &l[1..])));
                old += 1;
            }
            _ => {
                lines.push(Some((new, ' ', l.get(1..).unwrap_or(""))));
                old += 1;
                new += 1;
            }
        }
    }
    if !seen {
        return diff(text, color, usize::MAX, width, indent).lines().map(str::to_string).collect();
    }
    let gutter = lines.iter().flatten().map(|(n, _, _)| n.to_string().len()).max().unwrap_or(1);
    let room = width.saturating_sub(visible_width(indent) + gutter + 3);
    lines
        .iter()
        .map(|l| match l {
            None => format!("{indent}{}", paint(color, fg(FAINT), &format!("{:>gutter$}", "⋯"))),
            Some((n, sign, t)) => {
                // The sign sits in the gutter, bold; the line keeps its color.
                let style = match sign {
                    '+' => fg(GREEN),
                    '-' => fg(RED),
                    _ => fg(GRAY),
                };
                format!(
                    "{indent}{} {}{}",
                    paint(color, fg(FAINT), &format!("{n:>gutter$}")),
                    paint(color, style.bold(), &format!("{sign} ")),
                    paint(color, style, &truncate(t.trim_end_matches('\r'), room))
                )
            }
        })
        .collect()
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
    fn italics_links_and_urls() {
        let out = inline("an *em* and _word_ but snake_case_name and 2 * 3 * 4", true);
        assert!(out.contains(&Style::new().italic().paint("em").to_string()), "{out:?}");
        assert!(out.contains(&Style::new().italic().paint("word").to_string()), "{out:?}");
        assert!(strip_ansi(&out).ends_with("but snake_case_name and 2 * 3 * 4"), "{out:?}");
        let out = inline("see [the docs](https://x.dev/a) now", true);
        assert!(out.contains("\x1b]8;;https://x.dev/a\x1b\\"), "{out:?}");
        assert_eq!(strip_ansi(&out), "see the docs now");
        assert_eq!(visible_width(&out), "see the docs now".len());
        let out = inline("visit https://example.com/x_(y). ok", true);
        assert!(out.contains("\x1b]8;;https://example.com/x_(y)\x1b\\"), "{out:?}");
        assert_eq!(strip_ansi(&out), "visit https://example.com/x_(y). ok");
        assert_eq!(inline("nohttps://x.y and [a](b c)", true), "nohttps://x.y and [a](b c)");
    }

    #[test]
    fn quotes_rules_and_numbered_lists() {
        let out = render("> quoted *text*\n> > deeper", true);
        let plain: Vec<String> = out.lines().map(strip_ansi).collect();
        assert_eq!(plain, ["│ quoted text", "│ │ deeper"]);
        assert_eq!(strip_ansi(&render("---", true)), "─".repeat(RULE));
        assert_eq!(strip_ansi(&render("  * * *", true)), format!("  {}", "─".repeat(RULE)));
        assert_eq!(strip_ansi(&render("  3. third `x`", true)), "  3. third x");
        assert_eq!(strip_ansi(&render("2024 was a year", true)), "2024 was a year");
        assert_eq!(strip_ansi(&render("+ plus item", true)), "• plus item");
    }

    #[test]
    fn code_fences_highlight_and_close() {
        let mut m = Markdown::default();
        m.line("```rust", true);
        let body = m.line("let x = 1;", true);
        assert_eq!(strip_ansi(&body), "let x = 1;");
        assert!(body.matches("\x1b[").count() > 2, "{body:?}");
        assert!(m.line("```rust", true).contains("rust"), "a fence with an info string does not close");
        m.line("```", true);
        assert!(m.line("# heading", true).contains("\x1b[1m"));
        assert!(Fence::open("```a`b").is_none());
        assert!(Fence::open("~~~").is_some_and(|f| f.lang.is_empty()));
    }

    #[test]
    fn escapes_and_links_have_no_width() {
        let l = link("https://a.b", "text");
        assert_eq!(visible_width(&l), 4);
        assert_eq!(strip_ansi(&format!("\x1b[1m{l}\x1b[0m!")), "text!");
        assert_eq!(escape_len("\x1b]8;;u\x07rest"), 7);
        assert_eq!(escape_len("\x1b[38;2;1;2;3mx"), 13);
        assert_eq!(escape_len("\x1b]0;unterminated"), 16);
        assert_eq!(file_url("/w", "src/a b.rs").as_deref(), Some("file:///w/src/a%20b.rs"));
        assert_eq!(file_url("", "rel.rs"), None);
    }

    #[test]
    fn styled_rows_wrap_with_hanging_indents() {
        let row = format!("  {} {}", dim().paint("•"), Style::new().bold().paint("alpha beta gamma delta"));
        let rows = wrap_row(&row, 14);
        assert_eq!(rows.iter().map(|r| strip_ansi(r)).collect::<Vec<_>>(), ["  • alpha beta", "    gamma", "    delta"]);
        assert!(rows.iter().all(|r| visible_width(r) <= 14));
        // Bold carries onto every continuation row and is closed on each.
        assert!(rows[1].contains("\x1b[1m") && rows[1].ends_with("\x1b[0m"), "{rows:?}");
        let quote = render("> > one two three four five six", true);
        let rows: Vec<String> = wrap_row(&quote, 16).iter().map(|r| strip_ansi(r)).collect();
        assert_eq!(rows, ["│ │ one two", "│ │ three four", "│ │ five six"]);
        assert_eq!(strip_ansi(&continuation("  12. item")), "      ");
        assert_eq!(strip_ansi(&continuation(&format!("{} text", fg(BLUE).paint("›")))), "  ");
        assert_eq!(continuation("plain"), "");
        // Links reopen on the next row and long words hard-wrap.
        let rows = wrap_styled(&link("https://a.b", "aaaa bbbb"), 6, "");
        assert_eq!(rows.len(), 2);
        assert!(rows[0].ends_with("\x1b]8;;\x1b\\") && rows[1].starts_with("\x1b]8;;https://a.b"), "{rows:?}");
        assert_eq!(wrap_styled("abcdefgh", 3, "").len(), 3);
        assert_eq!(wrap_row("short", 10), ["short"]);
    }

    #[test]
    fn numbered_diffs_have_a_gutter() {
        let d = "--- a/x\n+++ b/x\n@@ -8,3 +8,3 @@\n ctx\n-old\n+new\n ctx2\n@@ -120 +120 @@\n-a\n+b\n\\ No newline at end of file";
        let rows: Vec<String> = numbered_diff(d, true, 40, "  ").iter().map(|r| strip_ansi(r)).collect();
        assert_eq!(rows, ["    8   ctx", "    9 - old", "    9 + new", "   10   ctx2", "    ⋯", "  120 - a", "  120 + b"]);
        let colored = numbered_diff(d, true, 40, "");
        assert!(colored[1].contains(&fg(RED).paint("old").to_string()), "{colored:?}");
        assert!(colored[1].contains(&fg(RED).bold().paint("- ").to_string()), "{colored:?}");
        assert!(numbered_diff(&format!("@@ -1 +1 @@\n+{}", "x".repeat(99)), false, 20, "").iter().all(|r| visible_width(r) <= 20));
        assert_eq!(numbered_diff("not a diff", false, 40, ""), ["not a diff"]);
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
        assert!(out.contains(&fg(GREEN).paint("+new").to_string()));
        assert!(out.contains(&fg(RED).paint("-old").to_string()));
        assert!(out.contains(&fg(BLUE).paint("@@ -1,2 +1,2 @@").to_string()));
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
