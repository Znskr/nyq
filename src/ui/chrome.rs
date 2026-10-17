//! The look of the inline UI around the conversation: the startup banner,
//! the input frame, the status line, tool-call marks and the working line.
//! Colors all go through the theme (so `NO_COLOR`, light and dark,
//! `theme_colors` and 256-color terminals apply), and glyphs fall back to
//! ASCII when the locale isn't UTF-8; `fold` catches the ones painted elsewhere.

use std::borrow::Cow;
use std::time::Duration;

use nu_ansi_term::{Color, Style};

use super::keys::{KeyAction, Keymap};
use super::render::{sanitize_text, truncate, visible_width, RULE};
use super::statusline::{self, tilde, Kind};
use super::theme::{self, fg, BLUE, BRIGHT, FAINT, GRAY, GREEN, MAGENTA, MID, RED, YELLOW};
use super::tui::{clip_width, Footer};

/// Prompts, the spinner and the start of the logo's gradient.
pub const ACCENT: Color = BLUE;
/// The banner is one line below this width.
const WIDE_BANNER: usize = 60;
/// The input is a rounded box from this width on, else just a prompt mark.
const BOXED_INPUT: usize = 16;

const LOGO: [&str; 2] = ["█▀█ █▄█ █▀█", "▀ ▀ ▄▄█ ▀▀█"];
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const ASCII_SPINNER: [&str; 4] = ["|", "/", "-", "\\"];

fn paint(style: Style, s: &str) -> String {
    if s.is_empty() {
        String::new()
    } else {
        style.paint(s).to_string()
    }
}

fn sep() -> String {
    paint(fg(FAINT), " · ")
}

/// `s` with the UI's Unicode glyphs swapped for ASCII ones of the same width
/// when the locale isn't UTF-8; unchanged otherwise.
pub fn fold(s: &str) -> Cow<'_, str> {
    if !theme::ascii() || s.is_ascii() {
        return Cow::Borrowed(s);
    }
    Cow::Owned(s.chars().map(ascii_char).collect())
}

fn ascii_char(c: char) -> char {
    match c {
        '─' | '━' | '–' | '—' => '-',
        '│' | '┃' | '├' | '▌' | '▍' | '▎' | '▏' => '|',
        '└' => '`',
        '╭' | '╮' | '╰' | '╯' | '┼' | '┬' | '┴' | '┤' | '┌' | '┐' | '┘' => '+',
        '●' | '•' | '◆' | '◇' => '*',
        '·' => '-',
        '…' | '⋯' => '~',
        '›' | '→' | '▸' | '↳' => '>',
        '‹' | '←' => '<',
        '↑' => '^',
        '↓' => 'v',
        '✓' => '+',
        '✗' => 'x',
        '⎇' => '@',
        '▰' | '█' | '▀' | '▄' => '#',
        '▱' => '-',
        '\u{2800}'..='\u{28ff}' => '*',
        c => c,
    }
}

/// One frame of the working spinner.
pub fn spinner(frame: usize) -> &'static str {
    if theme::ascii() {
        ASCII_SPINNER[frame % ASCII_SPINNER.len()]
    } else {
        SPINNER[frame % SPINNER.len()]
    }
}

/// Where a tool call is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mark {
    Running,
    Ok,
    Failed,
}

/// The glyph before a tool call: the spinner while it runs, then ✓ or ✗.
pub fn mark(m: Mark, frame: usize) -> String {
    let ascii = theme::ascii();
    match m {
        Mark::Running => paint(fg(ACCENT), spinner(frame)),
        Mark::Ok => paint(fg(GREEN), if ascii { "+" } else { "✓" }),
        Mark::Failed => paint(fg(RED), if ascii { "x" } else { "✗" }),
    }
}

/// A tool call's duration: `0.4s`, `12s`, `3m 05s`.
pub fn elapsed(d: Duration) -> String {
    match d.as_secs() {
        0..=9 => format!("{:.1}s", d.as_secs_f64()),
        s => clock(s),
    }
}

/// Whole seconds for the working line: `12s`, `3m 05s`.
pub fn clock(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}m {:02}s", secs / 60, secs % 60)
    }
}

/// A tool call's note: a diff stat (`+3 / -1`) as green and red counts,
/// anything else gray.
pub fn note(note: &str) -> String {
    match note.split_once(" / ") {
        Some((add, del)) if add.starts_with('+') && del.starts_with('-') => {
            format!("{} {}", paint(fg(GREEN), add), paint(fg(RED), del))
        }
        _ => paint(fg(GRAY), note),
    }
}

/// `row` in a left-to-right gradient from the accent to magenta; a single
/// accent color without truecolor (and none under `NO_COLOR`).
fn gradient(row: &str) -> String {
    match (theme::color(ACCENT), theme::color(MAGENTA)) {
        (Color::Rgb(r1, g1, b1), Color::Rgb(r2, g2, b2)) if theme::truecolor_on() => {
            let last = row.chars().count().saturating_sub(1).max(1) as f64;
            let mix = |a: u8, b: u8, t: f64| (a as f64 + (b as f64 - a as f64) * t).round() as u8;
            row.chars()
                .enumerate()
                .map(|(i, c)| {
                    let t = i as f64 / last;
                    let color = Color::Rgb(mix(r1, r2, t), mix(g1, g2, t), mix(b1, b2, t));
                    if c == ' ' {
                        " ".to_string()
                    } else {
                        Style::new().fg(color).paint(c.to_string()).to_string()
                    }
                })
                .collect()
        }
        _ => paint(fg(ACCENT), row),
    }
}

/// Startup tips: a key (or prefix) and what it does. Unbound keys drop theirs.
pub fn tips(keys: &Keymap) -> Vec<(String, &'static str)> {
    let key = |a| keys.hint(a);
    [
        (Some("/help".to_string()), "lists every command"),
        (key(KeyAction::ExpandTools), "opens the full transcript"),
        (key(KeyAction::ModelMenu), "switches models"),
        (key(KeyAction::CycleEffort), "cycles the reasoning effort"),
        (Some("@".into()), "mentions a file, and $ runs a skill"),
        (key(KeyAction::FollowUp), "queues a follow-up while nyq works"),
        (Some("!".into()), "runs a shell command, like !git status"),
        (key(KeyAction::HistorySearch), "searches earlier prompts"),
        (key(KeyAction::PasteImage), "pastes an image from the clipboard"),
    ]
    .into_iter()
    .filter_map(|(k, text)| Some((k?, text)))
    .collect()
}

/// The startup banner: the logo beside the version and what the session
/// runs on (model · provider · folder), then tip number `tip` (wrapping).
/// One line below `WIDE_BANNER` columns; no logo in ASCII.
pub fn banner(version: &str, f: &Footer, keys: &Keymap, width: usize, tip: usize) -> Vec<String> {
    let version = format!("v{}", sanitize_text(version));
    let mut facts: Vec<String> = [&f.model, &f.provider].iter().filter(|s| !s.is_empty()).map(|s| sanitize_text(s)).collect();
    if !f.cwd.is_empty() {
        facts.push(sanitize_text(&tilde(&f.cwd)));
    }
    let facts = facts.join(" · ");
    let name = paint(fg(BRIGHT).bold(), "nyq");
    let wide = width >= WIDE_BANNER;
    let mut rows = Vec::new();
    if wide && !theme::ascii() {
        // The text starts two columns right of the logo.
        let room = width.saturating_sub(LOGO[0].chars().count() + 2);
        rows.push(format!("{}  {name} {}", gradient(LOGO[0]), paint(fg(GRAY), &truncate(&version, room.saturating_sub(4)))));
        rows.push(format!("{}  {}", gradient(LOGO[1]), paint(fg(GRAY), &truncate(&facts, room))));
    } else {
        let lead = format!("{} ", paint(fg(ACCENT), "◆"));
        let rest = if facts.is_empty() { version } else { format!("{version} · {facts}") };
        let room = width.saturating_sub(6).max(1);
        rows.push(format!("{lead}{name} {}", paint(fg(GRAY), &truncate(&rest, room))));
    }
    let tips = tips(keys);
    if wide && !tips.is_empty() {
        let (key, text) = &tips[tip % tips.len()];
        let indent = if theme::ascii() { 2 } else { LOGO[0].chars().count() + 2 };
        let room = width.saturating_sub(indent + 5 + key.chars().count());
        rows.push(format!(
            "{}{} {} {}",
            " ".repeat(indent),
            paint(fg(FAINT), "tip"),
            paint(fg(MID), key),
            paint(fg(GRAY), &truncate(text, room))
        ));
    }
    rows
}

/// Whether the input is boxed at `width`.
pub fn boxed(width: usize) -> bool {
    width >= BOXED_INPUT
}

/// Columns of editor text inside the input frame at `width`.
pub fn input_width(width: usize) -> usize {
    if boxed(width) {
        width - 6
    } else {
        width.saturating_sub(2).max(1)
    }
}

/// What the empty input says: how to start, or how to steer the work.
pub fn placeholder(busy: bool, keys: &Keymap) -> String {
    match (busy, keys.hint(KeyAction::FollowUp)) {
        (false, _) => "Ask anything · / for commands · $ for skills · @ for files".into(),
        (true, Some(k)) => format!("Steer the current task · {k} queues a follow-up"),
        (true, None) => "Steer the current task".into(),
    }
}

/// The editor rows (`input_width` wide) framed: a rounded box with a prompt
/// mark, accented while nyq waits for input and dimmed while it works. An
/// empty input shows `placeholder`. Returns the rows and where the first
/// text row and column are.
pub fn input_box(lines: &[String], placeholder: Option<&str>, width: usize, busy: bool) -> (Vec<String>, usize, usize) {
    let (border, prompt) = if busy { (fg(FAINT), fg(GRAY)) } else { (fg(GRAY), fg(ACCENT).bold()) };
    let inner = input_width(width);
    let text = |i: usize, l: &String| match placeholder {
        Some(p) if i == 0 && l.is_empty() => paint(fg(FAINT).italic(), &truncate(p, inner)),
        _ => l.clone(),
    };
    let lead = |i: usize| if i == 0 { paint(prompt, if theme::ascii() { ">" } else { "›" }) } else { " ".into() };
    if !boxed(width) {
        let rows = lines.iter().enumerate().map(|(i, l)| format!("{} {}", lead(i), text(i, l))).collect();
        return (rows, 0, 2);
    }
    let edge = "─".repeat(width - 2);
    let mut rows = vec![paint(border, &format!("╭{edge}╮"))];
    for (i, l) in lines.iter().enumerate() {
        let t = text(i, l);
        let pad = " ".repeat(inner.saturating_sub(visible_width(&t)));
        rows.push(format!("{} {} {t}{pad} {}", paint(border, "│"), lead(i), paint(border, "│")));
    }
    rows.push(paint(border, &format!("╰{edge}╯")));
    (rows, 1, 4)
}

/// A context gauge: five cells filled by `used` (0 to 1).
fn meter(used: f64, color: Color) -> String {
    let filled = ((used * 5.0).round() as usize).clamp(usize::from(used > 0.0), 5);
    let (on, off) = if theme::ascii() { ("#", "-") } else { ("▰", "▱") };
    format!("{}{}", paint(fg(color), &on.repeat(filled)), paint(fg(FAINT), &off.repeat(5 - filled)))
}

/// One right-hand segment, colored by what it says.
fn segment(kind: Kind, text: &str) -> String {
    let text = sanitize_text(text);
    match kind {
        Kind::Context(used) => {
            let color = if used >= 0.8 {
                RED
            } else if used >= 0.5 {
                YELLOW
            } else {
                GREEN
            };
            format!("{} {}", meter(used, color), paint(fg(GRAY), &text))
        }
        Kind::Mode => {
            let color = match text.as_str() {
                "ask" => GREEN,
                "auto" => ACCENT,
                "yolo" => RED,
                _ => GRAY,
            };
            paint(fg(color), &text)
        }
        Kind::Sandbox => paint(fg(if text.starts_with("no sandbox") { YELLOW } else { GRAY }), &text),
        Kind::Branch => match text.split_once(' ').filter(|_| !theme::ascii()) {
            Some((glyph, name)) => format!("{} {}", paint(fg(MAGENTA), glyph), paint(fg(GRAY), name)),
            None => paint(fg(GRAY), &text),
        },
        Kind::Title => paint(fg(GRAY), &text),
        Kind::Provider | Kind::Tokens | Kind::Folder | Kind::Extension => paint(fg(FAINT), &text),
    }
}

/// Which segment goes first when the status line runs out of room.
fn drop_rank(kind: Kind) -> u8 {
    match kind {
        Kind::Title => 0,
        Kind::Tokens => 1,
        Kind::Provider => 2,
        Kind::Extension => 3,
        Kind::Folder => 4,
        Kind::Branch => 5,
        Kind::Sandbox => 6,
        Kind::Context(_) => 7,
        Kind::Mode => 8,
    }
}

/// The footer: effort and model on the left; on the right `upgrade` (the
/// auto-upgrade status, may be empty) and the session's segments, colored
/// by kind and dropped, least useful first, until they fit.
pub fn status_line(f: &Footer, width: usize, upgrade: &str) -> String {
    let left = match statusline::left(f) {
        Some(text) => {
            let mut parts = text.split(" · ");
            let effort = parts.next().unwrap_or_default();
            let model = parts.next().unwrap_or_default();
            let mut out = format!("{}{}{}", paint(fg(MAGENTA), effort), sep(), paint(fg(MID), &sanitize_text(model)));
            for p in parts {
                out.push_str(&sep());
                out.push_str(&paint(fg(YELLOW), p));
            }
            out
        }
        None => String::new(),
    };
    if visible_width(&left) > width {
        return clip_width(&left, width);
    }
    let room = if left.is_empty() { width } else { width.saturating_sub(visible_width(&left) + 4) };
    let upgrade = if upgrade.is_empty() {
        String::new()
    } else {
        let color = if upgrade.starts_with("upgrading") { GRAY } else { YELLOW };
        paint(fg(color), &truncate(upgrade, room))
    };
    let mut segs = statusline::segments(f, width);
    let joined = |segs: &[(Kind, String)]| {
        let parts = std::iter::once(upgrade.clone()).filter(|u| !u.is_empty()).chain(segs.iter().map(|(k, t)| segment(*k, t)));
        parts.collect::<Vec<_>>().join(&sep())
    };
    let mut right = joined(&segs);
    while visible_width(&right) > room {
        let Some(i) = segs.iter().enumerate().min_by_key(|(_, (k, _))| drop_rank(*k)).map(|(i, _)| i) else { break };
        segs.remove(i);
        right = joined(&segs);
    }
    if visible_width(&right) > room {
        right = clip_width(&right, room);
    }
    if left.is_empty() {
        return right;
    }
    let gap = width.saturating_sub(visible_width(&left) + visible_width(&right));
    if right.is_empty() || gap < 2 {
        left
    } else {
        format!("{left}{}{right}", " ".repeat(gap))
    }
}

/// The working line: spinner and `label` (painted), then `facts` (time,
/// tokens, tool tally) and a glimpse of the reasoning (`detail`), with the
/// interrupt key on the right when there is room.
pub fn working_line(label: &str, facts: &[String], detail: &str, interrupt: Option<&str>, frame: usize, width: usize) -> String {
    let hint = interrupt.map(|k| format!("{}{}", paint(fg(GRAY), k), paint(fg(FAINT), " to interrupt"))).unwrap_or_default();
    let mut line = format!("{} {label}", paint(fg(ACCENT), spinner(frame)));
    let facts = facts.join(" · ");
    let room = |line: &str, reserve: usize| width.saturating_sub(visible_width(line) + reserve);
    let reserve = if hint.is_empty() { 0 } else { visible_width(&hint) + 2 };
    let reserve = if room(&line, reserve) < 8 + facts.chars().count().min(12) { 0 } else { reserve };
    if !facts.is_empty() {
        let facts = truncate(&facts, room(&line, reserve + 3));
        line.push_str(&sep());
        line.push_str(&paint(fg(GRAY), &facts));
    }
    let detail = truncate(detail, room(&line, reserve + 1));
    if !detail.is_empty() && detail != "…" {
        line.push(' ');
        line.push_str(&paint(fg(FAINT).italic(), &detail));
    }
    if reserve > 0 {
        let gap = width.saturating_sub(visible_width(&line) + visible_width(&hint));
        line.push_str(&" ".repeat(gap));
        line.push_str(&hint);
    }
    line
}

/// The row opening a fenced code block: a corner, the language, a rule.
pub fn fence_open(lang: &str) -> String {
    let lang = if lang.is_empty() { "code" } else { lang };
    let rule = "─".repeat(RULE.saturating_sub(lang.chars().count() + 4).max(1));
    format!("{}{} {}", paint(fg(FAINT), "╭─ "), paint(fg(GRAY).italic(), lang), paint(fg(FAINT), &rule))
}

/// The row closing a fenced code block.
pub fn fence_close() -> String {
    paint(fg(FAINT), &format!("╰{}", "─".repeat(RULE - 1)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::render::strip_ansi;

    fn footer() -> Footer {
        Footer {
            cwd: "/work/nyq".into(),
            provider: "openai".into(),
            model: "gpt-6-sol".into(),
            effort: Some("high".into()),
            sandbox: "sandboxed".into(),
            permission_mode: "auto".into(),
            context_tokens: 150_000,
            context_window: 200_000,
            branch: Some("main".into()),
            ..Footer::default()
        }
    }

    /// Whether `s` sets any foreground or background color.
    fn has_color(s: &str) -> bool {
        s.split("\x1b[").skip(1).any(|esc| {
            let params = esc.split('m').next().unwrap_or("");
            params.split(';').any(|p| matches!(p.parse::<u8>(), Ok(30..=49 | 90..=107)))
        })
    }

    #[test]
    fn wide_banner_has_logo_facts_and_a_tip() {
        theme::test_mode(false, false, true);
        let keys = Keymap::default();
        let rows = banner("0.5\x1b]bad\x07", &footer(), &keys, 110, 1);
        let plain: Vec<String> = rows.iter().map(|r| strip_ansi(r)).collect();
        assert_eq!(plain.len(), 3, "{plain:?}");
        assert_eq!(plain[0], format!("{}  nyq v0.5", LOGO[0]));
        assert_eq!(plain[1], format!("{}  gpt-6-sol · openai · /work/nyq", LOGO[1]));
        let (key, text) = &tips(&keys)[1];
        assert_eq!(plain[2], format!("{}tip {key} {text}", " ".repeat(13)));
        assert!(rows[0].contains("\x1b[38;2;"), "a truecolor gradient: {:?}", rows[0]);
        assert!(rows.iter().all(|r| visible_width(r) <= 110));
        // Without truecolor the logo is one accent color.
        theme::test_mode(false, false, false);
        let rows = banner("0.5", &footer(), &keys, 110, 0);
        assert!(!rows[0].contains("\x1b[38;2;") && rows[0].contains("\x1b[38;5;"), "{:?}", rows[0]);
    }

    #[test]
    fn narrow_banner_is_one_line() {
        theme::test_mode(false, false, true);
        let rows = banner("0.5", &footer(), &Keymap::default(), 50, 0);
        assert_eq!(rows.len(), 1, "{rows:?}");
        let plain = strip_ansi(&rows[0]);
        assert!(plain.starts_with("◆ nyq v0.5 · gpt-6-sol"), "{plain}");
        assert!(visible_width(&rows[0]) <= 50, "{plain}");
        assert!(banner("0.5", &Footer::default(), &Keymap::default(), 8, 0).iter().all(|r| visible_width(r) <= 8));
    }

    #[test]
    fn tips_use_the_configured_keys() {
        let (keys, warnings) = Keymap::parse("expand_tools = \"alt+t\"\nmodel_menu = []");
        assert!(warnings.is_empty(), "{warnings:?}");
        let tips = tips(&keys);
        assert!(tips.iter().any(|(k, t)| k == "alt+t" && t.contains("transcript")), "{tips:?}");
        assert!(!tips.iter().any(|(_, t)| t.contains("switches models")), "unbound keys drop their tip");
    }

    #[test]
    fn ascii_fallback_folds_glyphs() {
        theme::test_mode(false, true, true);
        let rows = banner("0.5", &footer(), &Keymap::default(), 110, 0);
        assert_eq!(rows.len(), 2, "no logo, but a tip: {rows:?}");
        let folded: Vec<String> = rows.iter().map(|r| strip_ansi(&fold(r))).collect();
        assert!(folded.iter().all(|r| r.is_ascii()), "{folded:?}");
        assert!(folded[0].starts_with("* nyq v0.5 - gpt-6-sol"), "{folded:?}");
        let (rows, _, _) = input_box(&[String::new()], Some("Ask"), 30, false);
        let rows: Vec<String> = rows.iter().map(|r| strip_ansi(&fold(r))).collect();
        assert_eq!(rows, [format!("+{}+", "-".repeat(28)), format!("| > Ask{}|", " ".repeat(22)), format!("+{}+", "-".repeat(28))]);
        let line = strip_ansi(&fold(&status_line(&footer(), 100, "")));
        assert!(line.is_ascii() && line.contains("git:main") && line.contains("####- 75% of 200k"), "{line}");
        assert_eq!(spinner(5), "/");
        assert_eq!(strip_ansi(&mark(Mark::Ok, 0)), "+");
        assert_eq!(fold("├ Read a…"), "| Read a~");
        assert_eq!(visible_width(&fold("╭─ rust ⋯ ✓ ✗ ⎇ ▰▱")), visible_width("╭─ rust ⋯ ✓ ✗ ⎇ ▰▱"));
        theme::test_mode(false, false, true);
        assert_eq!(fold("├ ✓"), "├ ✓", "UTF-8 locales keep their glyphs");
    }

    #[test]
    fn no_color_paints_nothing() {
        theme::test_mode(true, false, true);
        let keys = Keymap::default();
        let mut rows = banner("0.5", &footer(), &keys, 110, 0);
        rows.extend(input_box(&["hi".into()], None, 40, false).0);
        rows.push(status_line(&footer(), 120, "update ready"));
        rows.push(working_line("Working", &["3s".into()], "", Some("esc"), 0, 80));
        rows.push(mark(Mark::Failed, 0) + &note("+3 / -1") + &fence_open("rust"));
        for r in &rows {
            assert!(!has_color(r), "{r:?}");
        }
        assert!(strip_ansi(&rows[0]).starts_with(LOGO[0]));
        theme::test_mode(false, false, true);
        assert!(has_color(&status_line(&footer(), 120, "")));
    }

    #[test]
    fn input_box_frames_text_and_placeholder() {
        theme::test_mode(false, false, true);
        let (rows, row, col) = input_box(&[String::new()], Some("Ask anything · / for commands"), 24, false);
        let plain: Vec<String> = rows.iter().map(|r| strip_ansi(r)).collect();
        assert_eq!(plain[1], "│ › Ask anything · / … │");
        assert_eq!((row, col, input_width(24)), (1, 4, 18));
        assert!(rows.iter().all(|r| visible_width(r) == 24), "{plain:?}");
        let (rows, _, _) = input_box(&["one".into(), "two".into()], Some("unused"), 20, true);
        let plain: Vec<String> = rows.iter().map(|r| strip_ansi(r)).collect();
        assert_eq!(plain[1..3], ["│ › one            │", "│   two            │"]);
        // Narrow terminals get the prompt mark alone.
        let (rows, row, col) = input_box(&["x".into()], None, 10, false);
        assert_eq!((strip_ansi(&rows[0]), row, col), ("› x".into(), 0, 2));
        assert!(placeholder(true, &Keymap::default()).starts_with("Steer"));
    }

    #[test]
    fn status_line_colors_and_drops_segments() {
        theme::test_mode(false, false, true);
        let mut f = footer();
        f.title = "Fix the login test".into();
        f.usage.input_tokens = 1200;
        let line = strip_ansi(&status_line(&f, 160, ""));
        assert!(line.starts_with("high · gpt-6-sol"), "{line}");
        assert!(line.ends_with("Fix the login test · openai · ▰▰▰▰▱ 75% of 200k · ↑1.2k ↓0 · sandboxed · auto · /work/nyq · ⎇ main"), "{line}");
        // Narrower: the title and token counts go first, the mode stays.
        let line = strip_ansi(&status_line(&f, 80, ""));
        assert!(visible_width(&line) <= 80, "{line}");
        assert!(!line.contains("Fix the") && !line.contains("↑1.2k") && line.contains("auto"), "{line}");
        let line = strip_ansi(&status_line(&f, 30, "update ready"));
        assert!(visible_width(&line) <= 30, "{line}");
        // Context fullness picks the gauge color.
        let red = segment(Kind::Context(0.9), "90% of 200k");
        assert!(red.starts_with(&fg(RED).prefix().to_string()), "{red:?}");
        assert!(segment(Kind::Mode, "yolo").contains(&fg(RED).prefix().to_string()));
        assert_eq!(strip_ansi(&meter(0.01, GREEN)), "▰▱▱▱▱");
    }

    #[test]
    fn working_line_fits_and_keeps_the_interrupt_key() {
        theme::test_mode(false, false, true);
        let facts = ["12s".to_string(), "↑1.5k ↓20".into(), "2 read".into()];
        let line = working_line("Working", &facts, "", Some("esc"), 3, 80);
        let plain = strip_ansi(&line);
        assert!(plain.starts_with("⠸ Working · 12s · ↑1.5k ↓20 · 2 read"), "{plain}");
        assert!(plain.ends_with("esc to interrupt") && visible_width(&line) == 80, "{plain}");
        let narrow = working_line("Thinking", &facts, "about the parser", Some("ctrl+c"), 0, 30);
        assert!(visible_width(&narrow) <= 30, "{}", strip_ansi(&narrow));
        assert!(!strip_ansi(&working_line("Working", &[], "", None, 0, 40)).contains("interrupt"));
    }

    #[test]
    fn durations_notes_and_fences() {
        assert_eq!(elapsed(Duration::from_millis(420)), "0.4s");
        assert_eq!(elapsed(Duration::from_secs(12)), "12s");
        assert_eq!(elapsed(Duration::from_secs(185)), "3m 05s");
        assert_eq!(clock(7), "7s");
        theme::test_mode(false, false, true);
        assert_eq!(strip_ansi(&note("+3 / -1")), "+3 -1");
        assert!(note("+3 / -1").contains(&fg(GREEN).paint("+3").to_string()));
        assert_eq!(strip_ansi(&note("12 lines")), "12 lines");
        assert_eq!(strip_ansi(&fence_open("rust")), format!("╭─ rust {}", "─".repeat(RULE - 8)));
        assert_eq!(strip_ansi(&fence_open("")).chars().count(), RULE);
        assert_eq!(strip_ansi(&fence_close()).chars().count(), RULE);
    }
}
