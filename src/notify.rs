//! The terminal bell and desktop notifications for a long turn that ended
//! and for prompts that wait on you (approvals, questions), per settings.

use std::io::Write;

use crate::settings::Settings;

pub enum Event<'a> {
    /// A turn finished (`failed`: with an error) after `secs` seconds.
    TurnEnd { secs: u64, failed: bool },
    /// nyq waits for an answer; the text says what it needs.
    Attention(&'a str),
}

/// What to do for one event.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Plan {
    pub bell: bool,
    /// (title, body) of a desktop notification.
    pub desktop: Option<(String, String)>,
}

pub fn plan(s: &Settings, event: &Event) -> Plan {
    match event {
        Event::TurnEnd { secs, failed } => {
            let long = *secs >= s.notify_after_secs;
            let what = if *failed { "Stopped" } else { "Finished" };
            Plan {
                bell: s.sound == "max" || (s.sound == "on" && long),
                desktop: (s.notifications && long).then(|| ("nyq".into(), format!("{what} after {}", duration(*secs)))),
            }
        }
        Event::Attention(what) => Plan {
            bell: s.sound != "off",
            desktop: s.notifications.then(|| ("nyq".into(), what.to_string())),
        },
    }
}

fn duration(secs: u64) -> String {
    match secs {
        0..=59 => format!("{secs}s"),
        _ => format!("{}m {}s", secs / 60, secs % 60),
    }
}

/// Terminal notification escape flavors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Osc {
    /// `OSC 9 ; text` (iTerm2, WezTerm, Ghostty, kitty, Windows Terminal).
    Nine,
    /// `OSC 777 ; notify ; title ; body` (rxvt, foot, VTE, Ghostty, WezTerm).
    SevenSevenSeven,
}

/// The flavor this terminal understands, from `TERM`, `TERM_PROGRAM` and
/// whether `VTE_VERSION` is set.
pub fn osc_for(term: &str, term_program: &str, vte: bool) -> Osc {
    if vte || term.contains("rxvt") || term.starts_with("foot") || term_program == "foot" {
        Osc::SevenSevenSeven
    } else {
        Osc::Nine
    }
}

/// Text safe inside an OSC string: no control characters, one line.
fn clean(s: &str, max: usize) -> String {
    s.chars().map(|c| if c.is_control() { ' ' } else { c }).take(max).collect::<String>().trim().to_string()
}

/// The escape sequence for a desktop notification; inside tmux it is wrapped
/// in a passthrough so it reaches the outer terminal.
pub fn escape(osc: Osc, title: &str, body: &str, tmux: bool) -> String {
    let (title, body) = (clean(title, 80), clean(body, 200));
    let seq = match osc {
        // OSC 9 ; 4 ; … is a progress report in some terminals, so the text
        // always starts with the title.
        Osc::Nine => format!("\x1b]9;{title}: {body}\x07"),
        Osc::SevenSevenSeven => format!("\x1b]777;notify;{};{body}\x07", title.replace(';', ",")),
    };
    if tmux {
        format!("\x1bPtmux;{}\x1b\\", seq.replace('\x1b', "\x1b\x1b"))
    } else {
        seq
    }
}

/// `NYQ_SOUND=off|on|max` overrides the sound setting; other values are ignored.
fn sound_override(value: Option<&str>) -> Option<&'static str> {
    match value?.trim().to_ascii_lowercase().as_str() {
        "off" | "0" => Some("off"),
        "on" | "1" => Some("on"),
        "max" => Some("max"),
        _ => None,
    }
}

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_default()
}

/// Ring and/or notify on `out` (the terminal). Desktop notifications use
/// the system notifier when running in a local graphical session, and the
/// terminal's escape sequence otherwise (e.g. over SSH).
pub fn emit(out: &mut impl Write, plan: &Plan) {
    if plan.bell {
        let _ = out.write_all(b"\x07");
    }
    if let Some((title, body)) = &plan.desktop {
        if !native(title, body) {
            let osc = osc_for(&env("TERM"), &env("TERM_PROGRAM"), std::env::var_os("VTE_VERSION").is_some());
            let _ = out.write_all(escape(osc, title, body, std::env::var_os("TMUX").is_some()).as_bytes());
        }
    }
    let _ = out.flush();
}

/// Plan and emit `event` on stdout.
pub fn notify(event: &Event) {
    let mut settings = crate::settings::current();
    if let Some(sound) = sound_override(std::env::var("NYQ_SOUND").ok().as_deref()) {
        settings.sound = sound.into();
    }
    let plan = plan(&settings, event);
    if plan != Plan::default() {
        emit(&mut std::io::stdout(), &plan);
    }
}

#[cfg_attr(windows, allow(dead_code))]
fn spawn_detached(mut cmd: std::process::Command) -> bool {
    use std::process::Stdio;
    match cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn() {
        Ok(mut child) => {
            std::thread::spawn(move || child.wait());
            true
        }
        Err(_) => false,
    }
}

#[cfg(target_os = "macos")]
fn native(title: &str, body: &str) -> bool {
    if std::env::var_os("SSH_CONNECTION").is_some() {
        return false;
    }
    let quote = |s: &str| format!("\"{}\"", clean(s, 200).replace('\\', "\\\\").replace('"', "\\\""));
    let mut cmd = std::process::Command::new("osascript");
    cmd.arg("-e").arg(format!("display notification {} with title {}", quote(body), quote(title)));
    spawn_detached(cmd)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn native(title: &str, body: &str) -> bool {
    let graphical = std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some();
    if !graphical || std::env::var_os("SSH_CONNECTION").is_some() {
        return false;
    }
    let mut cmd = std::process::Command::new("notify-send");
    cmd.args(["-a", "nyq", &clean(title, 80), &clean(body, 200)]);
    spawn_detached(cmd)
}

#[cfg(windows)]
fn native(_title: &str, _body: &str) -> bool {
    // Windows Terminal shows OSC 9 as a notification.
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_sequences() {
        assert_eq!(escape(Osc::Nine, "nyq", "Finished after 12s", false), "\x1b]9;nyq: Finished after 12s\x07");
        assert_eq!(escape(Osc::SevenSevenSeven, "a;b", "done", false), "\x1b]777;notify;a,b;done\x07");
        // Control characters can't end the sequence early or inject another.
        let evil = escape(Osc::Nine, "nyq", "x\x07\x1b]52;c;AAAA\x07", false);
        assert_eq!(evil.matches('\x07').count(), 1);
        assert_eq!(evil.matches('\x1b').count(), 1);
        assert_eq!(
            escape(Osc::Nine, "nyq", "hi", true),
            "\x1bPtmux;\x1b\x1b]9;nyq: hi\x07\x1b\\"
        );
        assert_eq!(osc_for("rxvt-unicode-256color", "", false), Osc::SevenSevenSeven);
        assert_eq!(osc_for("xterm-256color", "", true), Osc::SevenSevenSeven);
        assert_eq!(osc_for("foot", "", false), Osc::SevenSevenSeven);
        assert_eq!(osc_for("xterm-ghostty", "ghostty", false), Osc::Nine);
        assert_eq!(osc_for("xterm-256color", "iTerm.app", false), Osc::Nine);
    }

    #[test]
    fn plans_follow_settings() {
        let mut s = Settings { sound: "on".into(), notifications: true, notify_after_secs: 10, ..Settings::default() };
        let short = plan(&s, &Event::TurnEnd { secs: 3, failed: false });
        assert_eq!(short, Plan::default());
        let long = plan(&s, &Event::TurnEnd { secs: 75, failed: true });
        assert!(long.bell);
        assert_eq!(long.desktop, Some(("nyq".into(), "Stopped after 1m 15s".into())));
        assert!(plan(&s, &Event::Attention("Approve bash")).bell);
        s.sound = "max".into();
        s.notifications = false;
        assert_eq!(plan(&s, &Event::TurnEnd { secs: 1, failed: false }), Plan { bell: true, desktop: None });
        s.sound = "off".into();
        assert_eq!(plan(&s, &Event::Attention("Question")), Plan::default());
        let mut out = Vec::new();
        emit(&mut out, &Plan { bell: true, desktop: None });
        assert_eq!(out, b"\x07");
    }

    #[test]
    fn sound_env_overrides() {
        assert_eq!(sound_override(Some("MAX")), Some("max"));
        assert_eq!(sound_override(Some("off")), Some("off"));
        assert_eq!(sound_override(Some("loud")), None);
        assert_eq!(sound_override(None), None);
    }
}
