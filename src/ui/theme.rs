//! The color palette. UI code paints with the role colors below (written as
//! their dark-theme values); `color` maps them to the active theme and to the
//! 256-color palette when the terminal lacks truecolor.
//!
//! The theme comes from `NYQ_THEME=light|dark`, else the `theme` setting,
//! else the terminal background (an OSC 11 query, then `COLORFGBG`).
//! The `theme_colors` setting replaces single roles, for both themes or one.
//! `NO_COLOR` turns every role off (bold and italics stay), and a locale
//! that isn't UTF-8 switches the UI to ASCII glyphs (see `ascii`).

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::RwLock;
use std::time::Duration;

use nu_ansi_term::{Color, Style};
use serde_json::{Map, Value};

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

/// The roles by their `theme_colors` name.
pub const ROLES: [(&str, Color); 10] = [
    ("bright", BRIGHT),
    ("mid", MID),
    ("gray", GRAY),
    ("faint", FAINT),
    ("red", RED),
    ("yellow", YELLOW),
    ("green", GREEN),
    ("blue", BLUE),
    ("magenta", MAGENTA),
    ("orange", ORANGE),
];

/// Overridden role colors, indexed by theme and then by `ROLES` position.
type Palette = [[Option<Color>; ROLES.len()]; 2];

static PALETTE: RwLock<Palette> = RwLock::new([[None; ROLES.len()]; 2]);
/// Some role is overridden, so `PALETTE` is worth reading.
static OVERRIDDEN: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Theme {
    Dark,
    Light,
}

const LIGHT: u8 = 1;
const NO_TRUECOLOR: u8 = 2;
const DETECTED_LIGHT: u8 = 4;
const NO_COLOR: u8 = 8;
const ASCII: u8 = 16;

/// Bits above. Zero (dark, truecolor) until `init` runs, which tests rely on.
static MODE: AtomicU8 = AtomicU8::new(0);

#[cfg(test)]
thread_local! {
    /// Bits for this test thread only, so tests can render without colors or
    /// in ASCII without disturbing tests running alongside.
    static TEST_MODE: std::cell::Cell<Option<u8>> = const { std::cell::Cell::new(None) };
}

fn bits() -> u8 {
    #[cfg(test)]
    if let Some(bits) = TEST_MODE.with(|m| m.get()) {
        return bits;
    }
    MODE.load(Ordering::Relaxed)
}

/// Render this test thread without colors (`NO_COLOR`), in ASCII, or in 256 colors.
#[cfg(test)]
pub fn test_mode(no_color: bool, ascii: bool, truecolor: bool) {
    let bits = [
        (no_color, NO_COLOR),
        (ascii, ASCII),
        (!truecolor, NO_TRUECOLOR),
    ];
    TEST_MODE.with(|m| {
        m.set(Some(
            bits.iter().filter(|(on, _)| *on).map(|(_, b)| b).sum(),
        ))
    });
}

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
    let detected = if query {
        query_background(Duration::from_millis(200)).map(from_rgb)
    } else {
        None
    }
    .or_else(|| {
        std::env::var("COLORFGBG")
            .ok()
            .and_then(|v| from_colorfgbg(&v))
    })
    .unwrap_or(Theme::Dark);
    let mut bits = if detected == Theme::Light {
        DETECTED_LIGHT
    } else {
        0
    };
    let env = |k: &str| std::env::var(k).ok();
    if !truecolor(env) {
        bits |= NO_TRUECOLOR;
    }
    if env("NO_COLOR").is_some_and(|v| !v.is_empty()) {
        bits |= NO_COLOR;
    }
    if !utf8_locale(env) {
        bits |= ASCII;
    }
    MODE.store(bits, Ordering::Relaxed);
    apply_setting(setting);
}

/// Whether the locale promises UTF-8: the first of `LC_ALL`, `LC_CTYPE` and
/// `LANG` that is set decides. With none set (Windows, bare containers) the
/// terminal is trusted to cope.
fn utf8_locale(env: impl Fn(&str) -> Option<String>) -> bool {
    match ["LC_ALL", "LC_CTYPE", "LANG"]
        .iter()
        .find_map(|k| env(k).filter(|v| !v.is_empty()))
    {
        Some(v) => {
            let v = v.to_ascii_lowercase();
            v.contains("utf-8") || v.contains("utf8")
        }
        None => true,
    }
}

/// Colors are off (`NO_COLOR`).
pub fn no_color() -> bool {
    bits() & NO_COLOR != 0
}

/// Glyphs must be ASCII: the locale isn't UTF-8.
pub fn ascii() -> bool {
    bits() & ASCII != 0
}

/// 24-bit color is on, so blends (the banner's gradient) render smoothly.
pub fn truecolor_on() -> bool {
    bits() & (NO_TRUECOLOR | NO_COLOR) == 0
}

/// Re-apply the `theme` setting (`auto`, `light` or `dark`); `NYQ_THEME` wins.
pub fn apply_setting(setting: &str) {
    let bits = MODE.load(Ordering::Relaxed);
    let detected = if bits & DETECTED_LIGHT != 0 {
        Theme::Light
    } else {
        Theme::Dark
    };
    let theme = std::env::var("NYQ_THEME")
        .ok()
        .and_then(|v| parse(&v))
        .or_else(|| parse(setting))
        .unwrap_or(detected);
    let bits = if theme == Theme::Light {
        bits | LIGHT
    } else {
        bits & !LIGHT
    };
    MODE.store(bits, Ordering::Relaxed);
}

/// Install the `theme_colors` overrides (replacing earlier ones) and return
/// warnings for the entries that were skipped.
pub fn set_colors(settings: &Map<String, Value>) -> Vec<String> {
    let (palette, warnings) = parse_colors(settings);
    let any = palette.iter().flatten().any(Option::is_some);
    *PALETTE.write().unwrap_or_else(|e| e.into_inner()) = palette;
    OVERRIDDEN.store(any, Ordering::Relaxed);
    warnings
}

/// The problems `set_colors` would report, without installing anything.
pub fn color_warnings(settings: &Map<String, Value>) -> Vec<String> {
    parse_colors(settings).1
}

/// The palette `theme_colors` describes: top-level roles apply to both
/// themes, the `dark` and `light` tables win for theirs. Bad entries are
/// skipped with a warning.
fn parse_colors(settings: &Map<String, Value>) -> (Palette, Vec<String>) {
    let mut palette: Palette = [[None; ROLES.len()]; 2];
    let mut warnings = Vec::new();
    let mut set = |path: String, role: &str, v: &Value, themes: &[Theme]| {
        let Some(i) = ROLES
            .iter()
            .position(|(name, _)| name.eq_ignore_ascii_case(role.trim()))
        else {
            let names: Vec<&str> = ROLES.iter().map(|(name, _)| *name).collect();
            return Err(format!(
                "{path}: unknown role (one of {}) (ignored)",
                names.join(", ")
            ));
        };
        let c = parse_color(v).ok_or_else(|| {
            format!("{path}: expected \"#rrggbb\" or a color index 0-255, got {v} (ignored)")
        })?;
        for &t in themes {
            palette[t as usize][i] = Some(c);
        }
        Ok(())
    };
    // The top level first, so the theme tables overwrite it.
    for (role, v) in settings.iter().filter(|(k, _)| parse(k).is_none()) {
        warnings.extend(
            set(
                format!("theme_colors.{role}"),
                role,
                v,
                &[Theme::Dark, Theme::Light],
            )
            .err(),
        );
    }
    for (table, v) in settings {
        let Some(theme) = parse(table) else { continue };
        let Some(entries) = v.as_object() else {
            warnings.push(format!(
                "theme_colors.{table}: expected a table of role colors (ignored)"
            ));
            continue;
        };
        for (role, v) in entries {
            warnings.extend(set(format!("theme_colors.{table}.{role}"), role, v, &[theme]).err());
        }
    }
    (palette, warnings)
}

/// `"#rrggbb"` or an xterm color index (0-255).
fn parse_color(v: &Value) -> Option<Color> {
    if let Some(n) = v.as_u64() {
        return u8::try_from(n).ok().map(Color::Fixed);
    }
    let hex = v.as_str()?.trim().strip_prefix('#')?;
    if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
    Some(Color::Rgb(byte(0)?, byte(2)?, byte(4)?))
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
    Some(if bg == 7 || bg == 15 {
        Theme::Light
    } else {
        Theme::Dark
    })
}

/// The background from an OSC 11 reply: `ESC ] 11 ; rgb:RRRR/GGGG/BBBB`.
fn parse_osc11(reply: &str) -> Option<(u8, u8, u8)> {
    let rest = &reply[reply.find("]11;")? + 4..];
    let rest = rest
        .strip_prefix("rgb:")
        .or_else(|| rest.strip_prefix("rgba:"))?;
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
        let mut fd = libc::pollfd {
            fd: 0,
            events: libc::POLLIN,
            revents: 0,
        };
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
    let cube_err = [(r, lr), (g, lg), (b, lb)]
        .iter()
        .map(|&(v, l)| (v as i32 - cube(l)).pow(2))
        .sum::<i32>();
    let avg = (r as i32 + g as i32 + b as i32) / 3;
    let gray = ((avg - 3).clamp(0, 230) / 10) as u8;
    let gray_v = 8 + 10 * gray as i32;
    let gray_err = [r, g, b]
        .iter()
        .map(|&v| (v as i32 - gray_v).pow(2))
        .sum::<i32>();
    if gray_err < cube_err {
        232 + gray
    } else {
        16 + 36 * lr + 6 * lg + lb
    }
}

/// `c` in the active theme and color depth.
pub fn color(c: Color) -> Color {
    let bits = bits();
    let theme = if bits & LIGHT != 0 {
        Theme::Light
    } else {
        Theme::Dark
    };
    let truecolor = bits & NO_TRUECOLOR == 0;
    if !OVERRIDDEN.load(Ordering::Relaxed) {
        return resolve(c, theme, truecolor, None);
    }
    resolve(
        c,
        theme,
        truecolor,
        Some(&PALETTE.read().unwrap_or_else(|e| e.into_inner())),
    )
}

/// A role's override, else its built-in color for `theme`, at the color depth.
fn resolve(c: Color, theme: Theme, truecolor: bool, palette: Option<&Palette>) -> Color {
    let over = palette.and_then(|p| p[theme as usize][ROLES.iter().position(|&(_, r)| r == c)?]);
    let c = over.unwrap_or(if theme == Theme::Light { light(c) } else { c });
    match c {
        Color::Rgb(r, g, b) if !truecolor => Color::Fixed(to_256(r, g, b)),
        c => c,
    }
}

/// A style in role color `c`; no color at all under `NO_COLOR`.
pub fn fg(c: Color) -> Style {
    if no_color() {
        Style::new()
    } else {
        Style::new().fg(color(c))
    }
}

/// `style` on a role-colored background; reversed under `NO_COLOR`.
pub fn on(style: Style, c: Color) -> Style {
    if no_color() {
        style.reverse()
    } else {
        style.on(color(c))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn detects_backgrounds() {
        assert_eq!(
            parse_osc11("\x1b]11;rgb:ffff/ffff/ffff\x1b\\"),
            Some((255, 255, 255))
        );
        assert_eq!(
            parse_osc11("junk\x1b]11;rgb:1e/1e/2e\x07\x1b[?62;c"),
            Some((30, 30, 46))
        );
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
            move |k: &str| {
                pairs
                    .iter()
                    .find(|(n, _)| *n == k)
                    .map(|(_, v)| v.to_string())
            }
        };
        assert!(truecolor(env(&[("COLORTERM", "truecolor")])));
        assert!(truecolor(env(&[("TERM_PROGRAM", "WezTerm")])));
        assert!(!truecolor(env(&[
            ("TERM", "xterm-256color"),
            ("TERM_PROGRAM", "Apple_Terminal")
        ])));
    }

    #[test]
    fn utf8_locales_and_no_color() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                pairs
                    .iter()
                    .find(|(n, _)| *n == k)
                    .map(|(_, v)| v.to_string())
            }
        };
        assert!(utf8_locale(env(&[("LANG", "en_US.UTF-8")])));
        assert!(utf8_locale(env(&[("LC_CTYPE", "C.utf8"), ("LANG", "C")])));
        assert!(utf8_locale(env(&[])), "nothing set trusts the terminal");
        assert!(
            !utf8_locale(env(&[("LC_ALL", "C"), ("LANG", "en_US.UTF-8")])),
            "LC_ALL wins"
        );
        assert!(!utf8_locale(env(&[("LANG", "POSIX")])));
        assert!(
            utf8_locale(env(&[("LC_ALL", ""), ("LANG", "de_DE.UTF-8")])),
            "empty values don't count"
        );
        test_mode(true, true, true);
        assert!(no_color() && ascii() && !truecolor_on());
        assert_eq!(fg(BLUE), Style::new());
        assert_eq!(on(Style::new(), YELLOW), Style::new().reverse());
        test_mode(false, false, false);
        assert!(!no_color() && !ascii() && !truecolor_on());
        assert_eq!(fg(BLUE), Style::new().fg(Color::Fixed(74)));
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

    fn colors(v: Value) -> (Palette, Vec<String>) {
        parse_colors(v.as_object().unwrap())
    }

    fn role(name: &str) -> usize {
        ROLES.iter().position(|(n, _)| *n == name).unwrap()
    }

    #[test]
    fn parses_role_colors() {
        assert_eq!(
            parse_color(&json!("#5fAFd7")),
            Some(Color::Rgb(95, 175, 215))
        );
        assert_eq!(parse_color(&json!(" #000000 ")), Some(Color::Rgb(0, 0, 0)));
        assert_eq!(parse_color(&json!(208)), Some(Color::Fixed(208)));
        for bad in [
            json!("5fafd7"),
            json!("#5faf"),
            json!("#5fafzz"),
            json!(256),
            json!(-1),
            json!(1.5),
            json!(true),
        ] {
            assert_eq!(parse_color(&bad), None, "{bad}");
        }
        let (p, warnings) = colors(json!({"blue": "#5fafd7", "red": 160}));
        assert!(warnings.is_empty(), "{warnings:?}");
        for t in [Theme::Dark, Theme::Light] {
            assert_eq!(p[t as usize][role("blue")], Some(Color::Rgb(95, 175, 215)));
            assert_eq!(p[t as usize][role("red")], Some(Color::Fixed(160)));
        }
    }

    #[test]
    fn theme_tables_win_over_the_top_level() {
        let (p, warnings) = colors(json!({
            "light": {"blue": "#005f87", "green": 28},
            "blue": "#5fafd7",
            "dark": {"faint": "#444444"},
        }));
        assert!(warnings.is_empty(), "{warnings:?}");
        let check =
            |theme, c, want| assert_eq!(resolve(c, theme, true, Some(&p)), want, "{theme:?} {c:?}");
        check(Theme::Dark, BLUE, Color::Rgb(95, 175, 215));
        check(Theme::Light, BLUE, Color::Rgb(0, 95, 135));
        check(Theme::Light, GREEN, Color::Fixed(28));
        check(Theme::Dark, GREEN, GREEN);
        check(Theme::Dark, FAINT, Color::Rgb(68, 68, 68));
        // Unset roles keep their built-in colors; other colors pass through.
        check(Theme::Light, FAINT, light(FAINT));
        check(Theme::Light, Color::Green, Color::Green);
        assert_eq!(resolve(BLUE, Theme::Light, true, None), light(BLUE));
    }

    #[test]
    fn overrides_fall_back_to_256_colors() {
        let (p, _) = colors(json!({"blue": "#d75f5f", "red": 9}));
        assert_eq!(
            resolve(BLUE, Theme::Dark, false, Some(&p)),
            Color::Fixed(167)
        );
        assert_eq!(resolve(RED, Theme::Light, false, Some(&p)), Color::Fixed(9));
        assert_eq!(
            resolve(GRAY, Theme::Dark, false, Some(&p)),
            Color::Fixed(245)
        );
    }

    #[test]
    fn bad_entries_warn_and_are_skipped() {
        let (p, warnings) = colors(json!({
            "blu": "#5fafd7",
            "red": "crimson",
            "green": "#00af00",
            "light": 3,
            "dark": {"blue": 300, "purple": 250, "Mid": 250},
        }));
        assert_eq!(warnings.len(), 5, "{warnings:?}");
        for start in [
            "theme_colors.blu: unknown role",
            "theme_colors.red: expected",
            "theme_colors.light: expected a table",
            "theme_colors.dark.blue: expected",
            "theme_colors.dark.purple: unknown role",
        ] {
            assert!(
                warnings.iter().any(|w| w.starts_with(start)),
                "{start}: {warnings:?}"
            );
        }
        assert_eq!(p.iter().flatten().filter(|c| c.is_some()).count(), 3);
        assert_eq!(
            p[Theme::Light as usize][role("green")],
            Some(Color::Rgb(0, 175, 0))
        );
        assert_eq!(color_warnings(&Map::new()), Vec::<String>::new());
    }
}
