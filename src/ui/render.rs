//! Pure text renderers: markdown-lite lines and colored diffs. Every function
//! takes a `color` flag; with `color == false` the input text is preserved.

use nu_ansi_term::{Color, Style};

const CODE_BLOCK: Color = Color::Fixed(109);
const INLINE_CODE: Color = Color::Fixed(116);

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
pub fn one_line(s: &str, max: usize) -> String {
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

pub fn looks_like_diff(s: &str) -> bool {
    s.lines().any(|l| l.starts_with("@@")) || (s.contains("\n+++ ") && s.starts_with("--- "))
}

/// Render at most `max` lines of `text`, each prefixed by `indent` and styled
/// by `style`, plus a `… N more lines` note when truncated.
pub fn capped<'a>(lines: impl Iterator<Item = &'a str>, max: usize, indent: &str, color: bool, style: impl Fn(&str) -> String) -> String {
    let lines: Vec<&str> = lines.collect();
    let mut out = String::new();
    for l in lines.iter().take(max) {
        out.push_str(indent);
        out.push_str(&style(l.trim_end_matches('\r')));
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
pub fn diff(text: &str, color: bool, max: usize, indent: &str) -> String {
    let has_hunks = text.lines().any(|l| l.starts_with("@@"));
    let mut seen_hunk = false;
    let lines = text.lines().filter(move |l| {
        seen_hunk |= l.starts_with("@@");
        !(has_hunks && !seen_hunk && (l.starts_with("--- ") || l.starts_with("+++ ")))
    });
    capped(lines, max, indent, color, |l| {
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
        let out = diff(d, true, 60, "  ");
        assert!(!out.contains("a/x")); // header dropped
        assert!(out.contains(&Color::Green.paint("+new").to_string()));
        assert!(out.contains(&Color::Red.paint("-old").to_string()));
        assert!(out.contains(&Color::Cyan.paint("@@ -1,2 +1,2 @@").to_string()));
        assert_eq!(diff(d, false, 60, ""), "@@ -1,2 +1,2 @@\n ctx\n-old\n+new\n");
        let long: String = (0..100).map(|i| format!("+{i}\n")).collect();
        let out = diff(&long, false, 60, "");
        assert_eq!(out.lines().count(), 61);
        assert!(out.ends_with("… 40 more lines\n"));
    }

    #[test]
    fn diff_detection_and_one_line() {
        assert!(looks_like_diff("@@ -1 +1 @@\n-a\n+b"));
        assert!(!looks_like_diff("just some text\n- a list"));
        assert_eq!(one_line("  hello  ", 10), "hello");
        assert_eq!(one_line("abcdef", 3), "abc…");
        assert_eq!(one_line("a\nb", 10), "a…");
    }
}
