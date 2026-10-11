//! The color palette. UI code paints with the role colors below (written as
//! their dark-theme values); `color` maps them to the active theme and to the
//! 256-color palette when the terminal lacks truecolor.
//!
//! The theme comes from `NYQ_THEME=light|dark`, else the `theme` setting,
//! else the terminal background (an OSC 11 query, then `COLORFGBG`).

use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

use nu_ansi_term::{Color, Style};

pub const BRIGHT: Color = Color::Rgb(238, 238, 238);
pub const MID: Color = Color::Rgb(208, 208, 208);
pub const GRAY: Color = Color::Rgb(138, 138, 138);
pub const FAINT: Color = Color::Rgb(88, 88, 88);
pub const RED: Color = Color::Rgb(215, 95, 95);
pub const YELLOW: Color = Color::Rgb(215, 175, 95);
pub const GREEN: Color = Color::Rgb(135, 175, 95);
pub const BLUE: Color = Color::Rgb(95, 175, 215);
pub const MAGENTA: Color = Color::Rgb(175, 135, 215);
pub const ORANGE: Color = Color::Rgb(215, 135, 95);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Theme {
    Dark,
    Light,
}

const LIGHT: u8 = 1;
const NO_TRUECOLOR: u8 = 2;
const DETECTED_LIGHT: u8 = 4;

/// Bits above. Zero (dark, truecolor) until `init` runs, which tests rely on.
static MODE: AtomicU8 = AtomicU8::new(0);

pub fn parse(s: &str) -> Option<Theme> {
    match s.trim().to_ascii_lowercase().as_str() {
        "light" => Some(Theme::Light),
        "dark" => Some(Theme::Dark),
        _ => None,
    }
}

/// Pick the theme and color depth. `query` asks the terminal for its
/// background (raw mode must be on and nothing else reading input).
pub fn init(setting: &str, query: bool) {
    let detected = if query { query_background(Duration::from_millis(200)).map(from_rgb) } else { None }
        .or_else(|| std::env::var("COLORFGBG").ok().and_then(|v| from_colorfgbg(&v)))
        .unwrap_or(Theme::Dark);
    let mut bits = if detected == Theme::Light { DETECTED_LIGHT } else { 0 };
    if !truecolor(|k| std::env::var(k).ok()) {
        bits |= NO_TRUECOLOR;
    }
    MODE.store(bits, Ordering::Relaxed);
    apply_setting(setting);
}

/// Re-apply the `theme` setting (`auto`, `light` or `dark`); `NYQ_THEME` wins.
pub fn apply_setting(setting: &str) {
    let bits = MODE.load(Ordering::Relaxed);
    let detected = if bits & DETECTED_LIGHT != 0 { Theme::Light } else { Theme::Dark };
    let theme = std::env::var("NYQ_THEME").ok().and_then(|v| parse(&v)).or_else(|| parse(setting)).unwrap_or(detected);
    let bits = if theme == Theme::Light { bits | LIGHT } else { bits & !LIGHT };
    MODE.store(bits, Ordering::Relaxed);
}

/// Whether the environment promises 24-bit color.
fn truecolor(env: impl Fn(&str) -> Option<String>) -> bool {
    let colorterm = env("COLORTERM").unwrap_or_default().to_ascii_lowercase();
    let term = env("TERM").unwrap_or_default().to_ascii_lowercase();
    let program = env("TERM_PROGRAM").unwrap_or_default();
    colorterm == "truecolor"
        || colorterm == "24bit"
        || term.contains("truecolor")
        || term.contains("24bit")
        || term.contains("direct")
        || env("WT_SESSION").is_some()
        || ["iTerm.app", "WezTerm", "vscode", "ghostty"].contains(&program.as_str())
}

/// A light background is one whose luminance is over half.
fn from_rgb((r, g, b): (u8, u8, u8)) -> Theme {
    let lum = 0.2126 * r as f64 + 0.7152 * g as f64 + 0.0722 * b as f64;
    if lum > 127.5 {
        Theme::Light
    } else {
        Theme::Dark
    }
}

/// `COLORFGBG` is `fg;bg` (sometimes `fg;default;bg`) in ANSI color numbers.
fn from_colorfgbg(v: &str) -> Option<Theme> {
    let bg: u8 = v.rsplit(';').next()?.trim().parse().ok()?;
    Some(if bg == 7 || bg == 15 { Theme::Light } else { Theme::Dark })
}

/// The background from an OSC 11 reply: `ESC ] 11 ; rgb:RRRR/GGGG/BBBB`.
fn parse_osc11(reply: &str) -> Option<(u8, u8, u8)> {
    let rest = &reply[reply.find("]11;")? + 4..];
    let rest = rest.strip_prefix("rgb:").or_else(|| rest.strip_prefix("rgba:"))?;
    let mut parts = rest.split('/').map(|p| {
        let hex: String = p.chars().take_while(char::is_ascii_hexdigit).collect();
        let v = u32::from_str_radix(&hex, 16).ok()?;
        let max = (1u32 << (4 * hex.len().clamp(1, 4))) - 1;
        Some((v * 255 / max) as u8)
    });
    Some((parts.next()??, parts.next()??, parts.next()??))
}

/// Ask the terminal for its background color. A device-attributes query goes
/// right after it: every terminal answers that one, so a terminal without
/// OSC 11 support doesn't cost the whole timeout.
#[cfg(unix)]
fn query_background(timeout: Duration) -> Option<(u8, u8, u8)> {
    use std::io::Write;
    let mut out = std::io::stdout();
    out.write_all(b"\x1b]11;?\x1b\\\x1b[c").ok()?;
    out.flush().ok()?;
    let deadline = std::time::Instant::now() + timeout;
    let mut buf = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            break;
        }
        let mut fd = libc::pollfd { fd: 0, events: libc::POLLIN, revents: 0 };
        // SAFETY: one valid pollfd for the duration of the call.
        if unsafe { libc::poll(&mut fd, 1, left.as_millis().max(1) as i32) } <= 0 {
            break;
        }
        let mut chunk = [0u8; 256];
        // SAFETY: reads at most `chunk.len()` bytes into `chunk`.
        let n = unsafe { libc::read(0, chunk.as_mut_ptr().cast(), chunk.len()) };
        if n <= 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n as usize]);
        let text = String::from_utf8_lossy(&buf);
        if text.find("\x1b[?").is_some_and(|i| text[i..].contains('c')) {
            break;
        }
    }
    parse_osc11(&String::from_utf8_lossy(&buf))
}

#[cfg(not(unix))]
fn query_background(_timeout: Duration) -> Option<(u8, u8, u8)> {
    None
}

/// The light-theme counterpart of a role color.
fn light(c: Color) -> Color {
    match c {
        BRIGHT => Color::Rgb(28, 28, 28),
        MID => Color::Rgb(58, 58, 58),
        GRAY => Color::Rgb(108, 108, 108),
        FAINT => Color::Rgb(168, 168, 168),
        RED => Color::Rgb(175, 0, 0),
        YELLOW => Color::Rgb(135, 95, 0),
        GREEN => Color::Rgb(0, 115, 0),
        BLUE => Color::Rgb(0, 95, 175),
        MAGENTA => Color::Rgb(135, 0, 175),
        ORANGE => Color::Rgb(175, 75, 0),
        c => c,
    }
}

/// The nearest xterm 256-color index of an RGB color.
pub fn to_256(r: u8, g: u8, b: u8) -> u8 {
    let level = |v: u8| if v < 48 { 0 } else if v < 115 { 1 } else { (v as u16 - 35) / 40 } as u8;
    let (lr, lg, lb) = (level(r), level(g), level(b));
    let cube = |l: u8| if l == 0 { 0 } else { 55 + 40 * l as i32 };
    let cube_err = [(r, lr), (g, lg), (b, lb)].iter().map(|&(v, l)| (v as i32 - cube(l)).pow(2)).sum::<i32>();
    let avg = (r as i32 + g as i32 + b as i32) / 3;
    let gray = ((avg - 3).clamp(0, 230) / 10) as u8;
    let gray_v = 8 + 10 * gray as i32;
    let gray_err = [r, g, b].iter().map(|&v| (v as i32 - gray_v).pow(2)).sum::<i32>();
    if gray_err < cube_err {
        232 + gray
    } else {
        16 + 36 * lr + 6 * lg + lb
    }
}

/// `c` in the active theme and color depth.
pub fn color(c: Color) -> Color {
    let bits = MODE.load(Ordering::Relaxed);
    let c = if bits & LIGHT != 0 { light(c) } else { c };
    match c {
        Color::Rgb(r, g, b) if bits & NO_TRUECOLOR != 0 => Color::Fixed(to_256(r, g, b)),
        c => c,
    }
}

pub fn fg(c: Color) -> Style {
    Style::new().fg(color(c))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_backgrounds() {
        assert_eq!(parse_osc11("\x1b]11;rgb:ffff/ffff/ffff\x1b\\"), Some((255, 255, 255)));
        assert_eq!(parse_osc11("junk\x1b]11;rgb:1e/1e/2e\x07\x1b[?62;c"), Some((30, 30, 46)));
        assert_eq!(parse_osc11("\x1b[?62;c"), None);
        assert_eq!(from_rgb((255, 255, 240)), Theme::Light);
        assert_eq!(from_rgb((30, 30, 46)), Theme::Dark);
        assert_eq!(from_colorfgbg("0;15"), Some(Theme::Light));
        assert_eq!(from_colorfgbg("15;default;0"), Some(Theme::Dark));
        assert_eq!(from_colorfgbg("x"), None);
        assert_eq!(parse(" Light "), Some(Theme::Light));
        assert_eq!(parse("auto"), None);
    }

    #[test]
    fn truecolor_needs_a_promise() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string())
        };
        assert!(truecolor(env(&[("COLORTERM", "truecolor")])));
        assert!(truecolor(env(&[("TERM_PROGRAM", "WezTerm")])));
        assert!(!truecolor(env(&[("TERM", "xterm-256color"), ("TERM_PROGRAM", "Apple_Terminal")])));
    }

    #[test]
    fn maps_to_256_colors() {
        assert_eq!(to_256(0, 0, 0), 16);
        assert_eq!(to_256(255, 255, 255), 231);
        assert_eq!(to_256(138, 138, 138), 245);
        assert_eq!(to_256(215, 95, 95), 167);
        assert_eq!(light(BRIGHT), Color::Rgb(28, 28, 28));
        assert_eq!(light(Color::Green), Color::Green);
    }
}
