//! Configurable key bindings: named actions with default keys, overridden by
//! `<home>/keybindings.toml` (`action = "ctrl+x"` or a list; `[]` unbinds).
//!
//! Only global and input-editor actions are bindable. Approval, question and
//! secret prompts and the keys inside menus and pickers (arrows, Enter, Esc,
//! Tab) stay fixed, so a binding can never answer an approval.

use std::path::Path;
use std::sync::{Arc, Mutex};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Where an action is looked up. Global keys are checked before the editor's,
/// so every context can conflict with every other.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Context {
    Global,
    Editor,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum KeyAction {
    Reload,
    ExpandTools,
    PasteImage,
    ModelMenu,
    CycleEffort,
    Suspend,
    Submit,
    Newline,
    FollowUp,
    Interrupt,
    Clear,
    Exit,
    Dequeue,
    HistorySearch,
    HistoryPrev,
    HistoryNext,
    Complete,
    Undo,
    Redo,
    CharLeft,
    CharRight,
    WordLeft,
    WordRight,
    LineStart,
    LineEnd,
    InputStart,
    InputEnd,
    DeleteWord,
    DeleteWordForward,
    DeleteToLineStart,
    DeleteToLineEnd,
    Yank,
}

pub struct Spec {
    pub action: KeyAction,
    pub name: &'static str,
    pub context: Context,
    pub defaults: &'static [&'static str],
    pub description: &'static str,
}

const fn spec(
    action: KeyAction,
    name: &'static str,
    context: Context,
    defaults: &'static [&'static str],
    description: &'static str,
) -> Spec {
    Spec {
        action,
        name,
        context,
        defaults,
        description,
    }
}

use Context::{Editor, Global};
use KeyAction as A;

/// Every bindable action, in lookup order (an earlier action wins a conflict).
pub const SPECS: &[Spec] = &[
    spec(
        A::Reload,
        "reload",
        Global,
        &["ctrl+g"],
        "Relaunch into an installed upgrade",
    ),
    spec(
        A::ExpandTools,
        "expand_tools",
        Global,
        &["ctrl+o"],
        "Full transcript with every tool call; review a pending approval",
    ),
    spec(
        A::PasteImage,
        "paste_image",
        Global,
        &["ctrl+v"],
        "Attach an image from the clipboard",
    ),
    spec(
        A::ModelMenu,
        "model_menu",
        Global,
        &["ctrl+l"],
        "Switch model (/model)",
    ),
    spec(
        A::CycleEffort,
        "cycle_effort",
        Global,
        &["shift+tab"],
        "Next reasoning effort",
    ),
    spec(
        A::Suspend,
        "suspend",
        Global,
        &["ctrl+z"],
        "Suspend to the shell (Unix)",
    ),
    spec(
        A::Submit,
        "submit",
        Editor,
        &["enter"],
        "Send the message (steers while working); after a trailing \\ inserts a newline",
    ),
    spec(
        A::Newline,
        "newline",
        Editor,
        &["shift+enter", "ctrl+j"],
        "Insert a newline",
    ),
    spec(
        A::FollowUp,
        "follow_up",
        Editor,
        &["alt+enter"],
        "While working, queue a follow-up; otherwise insert a newline",
    ),
    spec(
        A::Interrupt,
        "interrupt",
        Editor,
        &["esc"],
        "Interrupt the turn; twice clears the input",
    ),
    spec(
        A::Clear,
        "clear",
        Editor,
        &["ctrl+c"],
        "Clear the input; when empty interrupt, or press twice to exit",
    ),
    spec(
        A::Exit,
        "exit",
        Editor,
        &["ctrl+d"],
        "Exit on an empty input; otherwise delete the next character",
    ),
    spec(
        A::Dequeue,
        "dequeue",
        Editor,
        &["alt+up"],
        "Move the newest queued message back into the input",
    ),
    spec(
        A::HistorySearch,
        "history_search",
        Editor,
        &["ctrl+r"],
        "Reverse-search prompt history (again for older matches)",
    ),
    spec(
        A::HistoryPrev,
        "history_prev",
        Editor,
        &["up", "ctrl+p"],
        "Up a row; on the first row, the older prompt",
    ),
    spec(
        A::HistoryNext,
        "history_next",
        Editor,
        &["down", "ctrl+n"],
        "Down a row; on the last row, the newer prompt",
    ),
    spec(
        A::Complete,
        "complete",
        Editor,
        &["tab"],
        "Complete a command",
    ),
    spec(
        A::Undo,
        "undo",
        Editor,
        &["ctrl+_", "ctrl+7", "ctrl+-", "super+z"],
        "Undo",
    ),
    spec(
        A::Redo,
        "redo",
        Editor,
        &["alt+_", "ctrl+shift+z", "super+shift+z"],
        "Redo",
    ),
    spec(
        A::CharLeft,
        "char_left",
        Editor,
        &["left", "ctrl+b"],
        "Cursor left",
    ),
    spec(
        A::CharRight,
        "char_right",
        Editor,
        &["right", "ctrl+f"],
        "Cursor right",
    ),
    spec(
        A::WordLeft,
        "word_left",
        Editor,
        &["ctrl+left", "alt+left", "alt+b"],
        "Previous word",
    ),
    spec(
        A::WordRight,
        "word_right",
        Editor,
        &["ctrl+right", "alt+right", "alt+f"],
        "Next word",
    ),
    spec(
        A::LineStart,
        "line_start",
        Editor,
        &["home", "ctrl+a", "super+left"],
        "Start of line",
    ),
    spec(
        A::LineEnd,
        "line_end",
        Editor,
        &["end", "ctrl+e", "super+right"],
        "End of line",
    ),
    spec(
        A::InputStart,
        "input_start",
        Editor,
        &["ctrl+home", "super+up", "super+home", "alt+<"],
        "Start of input",
    ),
    spec(
        A::InputEnd,
        "input_end",
        Editor,
        &["ctrl+end", "super+down", "super+end", "alt+>"],
        "End of input",
    ),
    spec(
        A::DeleteWord,
        "delete_word",
        Editor,
        &["alt+backspace", "ctrl+w"],
        "Delete the previous word",
    ),
    spec(
        A::DeleteWordForward,
        "delete_word_forward",
        Editor,
        &["alt+d", "ctrl+delete", "alt+delete"],
        "Delete the next word",
    ),
    spec(
        A::DeleteToLineStart,
        "delete_to_line_start",
        Editor,
        &["ctrl+u", "super+backspace"],
        "Delete to the start of the line",
    ),
    spec(
        A::DeleteToLineEnd,
        "delete_to_line_end",
        Editor,
        &["ctrl+k"],
        "Delete to the end of the line (joins at the end)",
    ),
    spec(
        A::Yank,
        "yank",
        Editor,
        &["ctrl+y"],
        "Paste the last deleted text",
    ),
];

/// The same actions on their default keys with extra modifiers, which the
/// keys accepted before they were configurable (`ctrl+enter` sent, `alt+home`
/// went to the line start). They stay bound until the action is set in
/// keybindings.toml, and `/keys` only counts them.
pub const VARIANTS: &[(KeyAction, &[&str])] = &[
    (
        A::Reload,
        &["ctrl+shift+g", "ctrl+super+g", "ctrl+super+shift+g"],
    ),
    (
        A::ExpandTools,
        &["ctrl+shift+o", "ctrl+super+o", "ctrl+super+shift+o"],
    ),
    (
        A::PasteImage,
        &["ctrl+shift+v", "ctrl+super+v", "ctrl+super+shift+v"],
    ),
    (
        A::ModelMenu,
        &["ctrl+shift+l", "ctrl+super+l", "ctrl+super+shift+l"],
    ),
    (
        A::CycleEffort,
        &[
            "alt+shift+tab",
            "ctrl+shift+tab",
            "super+shift+tab",
            "alt+super+shift+tab",
            "ctrl+alt+shift+tab",
            "ctrl+super+shift+tab",
            "ctrl+alt+super+shift+tab",
        ],
    ),
    (A::Suspend, &["ctrl+super+z"]),
    (
        A::Submit,
        &[
            "ctrl+enter",
            "super+enter",
            "ctrl+alt+enter",
            "ctrl+super+enter",
            "ctrl+alt+super+enter",
        ],
    ),
    (
        A::Newline,
        &[
            "ctrl+shift+enter",
            "ctrl+shift+j",
            "ctrl+super+j",
            "super+shift+enter",
            "ctrl+alt+shift+enter",
            "ctrl+super+shift+enter",
            "ctrl+super+shift+j",
            "ctrl+alt+super+shift+enter",
        ],
    ),
    (
        A::FollowUp,
        &[
            "alt+shift+enter",
            "alt+super+enter",
            "alt+super+shift+enter",
        ],
    ),
    (
        A::Interrupt,
        &[
            "alt+esc",
            "ctrl+esc",
            "shift+esc",
            "super+esc",
            "alt+shift+esc",
            "alt+super+esc",
            "ctrl+alt+esc",
            "ctrl+shift+esc",
            "ctrl+super+esc",
            "super+shift+esc",
            "alt+super+shift+esc",
            "ctrl+alt+shift+esc",
            "ctrl+alt+super+esc",
            "ctrl+super+shift+esc",
            "ctrl+alt+super+shift+esc",
        ],
    ),
    (
        A::Clear,
        &["ctrl+shift+c", "ctrl+super+c", "ctrl+super+shift+c"],
    ),
    (
        A::Exit,
        &["ctrl+shift+d", "ctrl+super+d", "ctrl+super+shift+d"],
    ),
    (A::Dequeue, &["alt+super+up"]),
    (
        A::HistorySearch,
        &["ctrl+shift+r", "ctrl+super+r", "ctrl+super+shift+r"],
    ),
    (
        A::HistoryPrev,
        &[
            "ctrl+up",
            "ctrl+alt+up",
            "ctrl+shift+p",
            "ctrl+super+p",
            "ctrl+super+shift+p",
        ],
    ),
    (
        A::HistoryNext,
        &[
            "alt+down",
            "ctrl+down",
            "ctrl+alt+down",
            "ctrl+shift+n",
            "ctrl+super+n",
            "ctrl+super+shift+n",
        ],
    ),
    (
        A::Complete,
        &[
            "alt+tab",
            "ctrl+tab",
            "super+tab",
            "alt+super+tab",
            "ctrl+alt+tab",
            "ctrl+super+tab",
            "ctrl+alt+super+tab",
        ],
    ),
    (
        A::Undo,
        &[
            "alt+super+z",
            "ctrl+super+-",
            "ctrl+super+7",
            "ctrl+super+_",
            "ctrl+alt+super+z",
        ],
    ),
    (
        A::Redo,
        &[
            "alt+super+_",
            "alt+super+shift+z",
            "ctrl+super+shift+z",
            "ctrl+alt+super+shift+z",
        ],
    ),
    (
        A::CharLeft,
        &[
            "ctrl+alt+left",
            "ctrl+shift+b",
            "ctrl+super+b",
            "ctrl+super+shift+b",
        ],
    ),
    (
        A::CharRight,
        &[
            "ctrl+alt+right",
            "ctrl+shift+f",
            "ctrl+super+f",
            "ctrl+super+shift+f",
        ],
    ),
    (
        A::WordLeft,
        &[
            "alt+shift+b",
            "alt+super+b",
            "alt+super+left",
            "ctrl+super+left",
            "alt+super+shift+b",
        ],
    ),
    (
        A::WordRight,
        &[
            "alt+shift+f",
            "alt+super+f",
            "alt+super+right",
            "ctrl+super+right",
            "alt+super+shift+f",
        ],
    ),
    (
        A::LineStart,
        &[
            "alt+home",
            "ctrl+alt+home",
            "ctrl+shift+a",
            "ctrl+alt+super+left",
        ],
    ),
    (
        A::LineEnd,
        &[
            "alt+end",
            "ctrl+alt+end",
            "ctrl+shift+e",
            "ctrl+super+e",
            "ctrl+alt+super+right",
            "ctrl+super+shift+e",
        ],
    ),
    (
        A::InputStart,
        &[
            "alt+super+<",
            "alt+super+home",
            "ctrl+super+home",
            "ctrl+super+up",
            "ctrl+alt+super+home",
            "ctrl+alt+super+up",
        ],
    ),
    (
        A::InputEnd,
        &[
            "alt+super+>",
            "alt+super+down",
            "alt+super+end",
            "ctrl+super+down",
            "ctrl+super+end",
            "ctrl+alt+super+down",
            "ctrl+alt+super+end",
        ],
    ),
    (
        A::DeleteWord,
        &[
            "alt+shift+backspace",
            "alt+super+backspace",
            "ctrl+shift+w",
            "ctrl+super+w",
            "alt+super+shift+backspace",
            "ctrl+super+shift+w",
        ],
    ),
    (
        A::DeleteWordForward,
        &[
            "alt+shift+d",
            "alt+shift+delete",
            "alt+super+d",
            "alt+super+delete",
            "ctrl+shift+delete",
            "ctrl+super+delete",
            "alt+super+shift+d",
            "alt+super+shift+delete",
            "ctrl+super+shift+delete",
        ],
    ),
    (
        A::DeleteToLineStart,
        &[
            "ctrl+shift+u",
            "ctrl+super+backspace",
            "ctrl+super+u",
            "super+shift+backspace",
            "ctrl+alt+super+backspace",
            "ctrl+super+shift+backspace",
            "ctrl+super+shift+u",
            "ctrl+alt+super+shift+backspace",
        ],
    ),
    (
        A::DeleteToLineEnd,
        &["ctrl+shift+k", "ctrl+super+k", "ctrl+super+shift+k"],
    ),
    (
        A::Yank,
        &["ctrl+shift+y", "ctrl+super+y", "ctrl+super+shift+y"],
    ),
];

fn index(action: KeyAction) -> usize {
    SPECS
        .iter()
        .position(|s| s.action == action)
        .expect("every action has a spec")
}

/// The modifiers a binding can name; others on an event (Hyper, Meta) are
/// ignored, as they always were.
const MODS: KeyModifiers = KeyModifiers::CONTROL
    .union(KeyModifiers::ALT)
    .union(KeyModifiers::SHIFT)
    .union(KeyModifiers::SUPER);

/// A key with its modifiers, normalized so that a parsed binding and a key
/// event compare equal: letters are lowercase with Shift as a modifier,
/// other characters carry no Shift (it is part of the character), and
/// Shift+Tab is Tab with Shift.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Chord {
    code: KeyCode,
    mods: KeyModifiers,
}

impl Chord {
    fn new(code: KeyCode, mods: KeyModifiers) -> Self {
        let mut mods = mods & MODS;
        let code = match code {
            KeyCode::BackTab => {
                mods |= KeyModifiers::SHIFT;
                KeyCode::Tab
            }
            KeyCode::Char(c) if c.is_uppercase() => {
                mods |= KeyModifiers::SHIFT;
                KeyCode::Char(c.to_lowercase().next().unwrap_or(c))
            }
            KeyCode::Char(c) if !c.is_alphabetic() => {
                mods -= KeyModifiers::SHIFT;
                KeyCode::Char(c)
            }
            code => code,
        };
        Chord { code, mods }
    }

    pub fn of(key: &KeyEvent) -> Self {
        Chord::new(key.code, key.modifiers)
    }

    /// Parse `ctrl+x`, `alt+enter`, `shift+tab`, `f5`, `esc`, `ctrl+plus`.
    pub fn parse(text: &str) -> Result<Chord, String> {
        let s = text.trim().to_lowercase();
        // A trailing `+` is the plus key itself (`ctrl++`).
        let (prefix, key) = match s.strip_suffix("++") {
            Some(p) => (p, "+"),
            None if s == "+" => ("", "+"),
            None => s.rsplit_once('+').unwrap_or(("", &s)),
        };
        let mut mods = KeyModifiers::NONE;
        for m in prefix
            .split('+')
            .filter(|m| !prefix.is_empty() || !m.is_empty())
        {
            mods |= match m {
                "ctrl" | "control" => KeyModifiers::CONTROL,
                "alt" | "meta" | "option" => KeyModifiers::ALT,
                "shift" => KeyModifiers::SHIFT,
                "super" | "cmd" | "command" | "win" => KeyModifiers::SUPER,
                _ => return Err(format!("unknown modifier `{m}` in `{text}`")),
            };
        }
        let code = match key {
            "esc" | "escape" => KeyCode::Esc,
            "enter" | "return" => KeyCode::Enter,
            "tab" => KeyCode::Tab,
            "backtab" => KeyCode::BackTab,
            "backspace" => KeyCode::Backspace,
            "delete" | "del" => KeyCode::Delete,
            "insert" | "ins" => KeyCode::Insert,
            "home" => KeyCode::Home,
            "end" => KeyCode::End,
            "pageup" | "pgup" => KeyCode::PageUp,
            "pagedown" | "pgdn" => KeyCode::PageDown,
            "up" => KeyCode::Up,
            "down" => KeyCode::Down,
            "left" => KeyCode::Left,
            "right" => KeyCode::Right,
            "space" => KeyCode::Char(' '),
            "plus" => KeyCode::Char('+'),
            "minus" => KeyCode::Char('-'),
            f if f.len() > 1
                && f.starts_with('f')
                && f[1..].parse::<u8>().is_ok_and(|n| (1..=24).contains(&n)) =>
            {
                KeyCode::F(f[1..].parse().unwrap())
            }
            k if k.chars().count() == 1 => KeyCode::Char(k.chars().next().unwrap()),
            "" => return Err(format!("no key in `{text}`")),
            _ => return Err(format!("unknown key `{key}` in `{text}`")),
        };
        let chord = Chord::new(code, mods);
        // A plain character types text; it can't also be a shortcut.
        if matches!(chord.code, KeyCode::Char(_))
            && !chord
                .mods
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
        {
            return Err(format!(
                "`{text}` types text; bind it with ctrl, alt or super"
            ));
        }
        Ok(chord)
    }

    /// `ctrl+shift+z`, `alt+enter`, `f5`.
    pub fn label(&self) -> String {
        let mut parts: Vec<String> = [
            (KeyModifiers::CONTROL, "ctrl"),
            (KeyModifiers::ALT, "alt"),
            (KeyModifiers::SUPER, "super"),
            (KeyModifiers::SHIFT, "shift"),
        ]
        .iter()
        .filter(|(m, _)| self.mods.contains(*m))
        .map(|(_, n)| n.to_string())
        .collect();
        parts.push(match self.code {
            KeyCode::Esc => "esc".into(),
            KeyCode::Enter => "enter".into(),
            KeyCode::Tab => "tab".into(),
            KeyCode::Backspace => "backspace".into(),
            KeyCode::Delete => "delete".into(),
            KeyCode::Insert => "insert".into(),
            KeyCode::Home => "home".into(),
            KeyCode::End => "end".into(),
            KeyCode::PageUp => "pageup".into(),
            KeyCode::PageDown => "pagedown".into(),
            KeyCode::Up => "up".into(),
            KeyCode::Down => "down".into(),
            KeyCode::Left => "left".into(),
            KeyCode::Right => "right".into(),
            KeyCode::F(n) => format!("f{n}"),
            KeyCode::Char(' ') => "space".into(),
            KeyCode::Char('+') => "plus".into(),
            KeyCode::Char(c) => c.to_string(),
            other => format!("{other:?}").to_lowercase(),
        });
        parts.join("+")
    }

    /// `Ctrl+Shift+Z`, `Esc`: for help text.
    pub fn title(&self) -> String {
        self.label()
            .split('+')
            .map(|p| {
                let mut c = p.chars();
                c.next()
                    .map(|f| f.to_uppercase().chain(c).collect())
                    .unwrap_or_default()
            })
            .collect::<Vec<String>>()
            .join("+")
    }
}

/// The keys bound to each action.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Keymap {
    /// Indexed like [`SPECS`].
    keys: Vec<Vec<Chord>>,
    /// Whether keybindings.toml set the action (its variants are gone).
    custom: Vec<bool>,
}

impl Default for Keymap {
    fn default() -> Self {
        let parse = |k: &&str| Chord::parse(k).expect("default keys parse");
        let keys = SPECS
            .iter()
            .map(|s| {
                s.defaults
                    .iter()
                    .chain(variants(s.action))
                    .map(parse)
                    .collect()
            })
            .collect();
        Keymap {
            keys,
            custom: vec![false; SPECS.len()],
        }
    }
}

fn variants(action: KeyAction) -> &'static [&'static str] {
    VARIANTS
        .iter()
        .find(|(a, _)| *a == action)
        .map_or(&[], |(_, v)| v)
}

impl Keymap {
    /// The defaults with the overrides in `text` (keybindings.toml) applied,
    /// and warnings for anything that was skipped or conflicts.
    pub fn parse(text: &str) -> (Keymap, Vec<String>) {
        let mut map = Keymap::default();
        let mut warnings = Vec::new();
        let doc = match text.parse::<toml_edit::DocumentMut>() {
            Ok(doc) => doc,
            Err(e) => {
                let msg = e.to_string();
                let first = msg.lines().next().unwrap_or("").trim().to_string();
                return (
                    map,
                    vec![format!("keybindings.toml: {first}; using the default keys")],
                );
            }
        };
        // Which actions the file sets, in file order.
        let mut set: Vec<usize> = Vec::new();
        for (name, item) in doc.iter() {
            let Some(i) = SPECS.iter().position(|s| s.name == name) else {
                warnings.push(format!(
                    "keybindings.toml: unknown action `{name}` (see /keys)"
                ));
                continue;
            };
            let values: Vec<Option<&str>> = if let Some(s) = item.as_str() {
                vec![Some(s)]
            } else if let Some(list) = item.as_array() {
                list.iter().map(|v| v.as_str()).collect()
            } else {
                warnings.push(format!(
                    "keybindings.toml: `{name}` must be a key string or a list of them"
                ));
                continue;
            };
            let mut keys = Vec::new();
            let given = values.len();
            for v in values {
                match v.map(Chord::parse) {
                    Some(Ok(c)) if !keys.contains(&c) => keys.push(c),
                    Some(Ok(_)) => {}
                    Some(Err(e)) => warnings.push(format!("keybindings.toml: {name}: {e}")),
                    None => {
                        warnings.push(format!("keybindings.toml: {name}: keys must be strings"))
                    }
                }
            }
            // Nothing usable keeps the defaults; only `[]` unbinds.
            if keys.is_empty() && given > 0 {
                continue;
            }
            map.keys[i] = keys;
            map.custom[i] = true;
            set.retain(|&j| j != i);
            set.push(i);
        }
        // A key the file binds moves away from any action that only has it
        // by default.
        for &i in &set {
            for k in map.keys[i].clone() {
                for j in (0..SPECS.len()).filter(|j| !set.contains(j)) {
                    map.keys[j].retain(|c| *c != k);
                }
            }
        }
        // Variants go with the default keys they extend (`ctrl+shift+o`
        // with `ctrl+o`).
        for j in (0..SPECS.len()).filter(|j| !set.contains(j)) {
            let defaults: Vec<Chord> = SPECS[j]
                .defaults
                .iter()
                .filter_map(|k| Chord::parse(k).ok())
                .collect();
            let kept = map.keys[j].clone();
            map.keys[j].retain(|v| {
                let mut owners = defaults
                    .iter()
                    .filter(|d| d.code == v.code && v.mods.contains(d.mods));
                defaults.contains(v)
                    || owners.clone().next().is_none()
                    || owners.any(|d| kept.contains(d))
            });
        }
        warnings.extend(map.conflicts());
        (map, warnings)
    }

    /// Keys bound to more than one action; the earlier action in [`SPECS`] wins.
    pub fn conflicts(&self) -> Vec<String> {
        let mut out = Vec::new();
        for (i, keys) in self.keys.iter().enumerate() {
            for k in keys {
                if let Some(j) = (i + 1..SPECS.len()).find(|&j| self.keys[j].contains(k)) {
                    out.push(format!(
                        "keybindings.toml: {} is bound to both {} and {}; {} wins",
                        k.label(),
                        SPECS[i].name,
                        SPECS[j].name,
                        SPECS[i].name
                    ));
                }
            }
        }
        out
    }

    pub fn keys(&self, action: KeyAction) -> &[Chord] {
        &self.keys[index(action)]
    }

    pub fn matches(&self, action: KeyAction, key: &KeyEvent) -> bool {
        self.keys(action).contains(&Chord::of(key))
    }

    /// The action `key` triggers in `context`.
    pub fn action(&self, context: Context, key: &KeyEvent) -> Option<KeyAction> {
        let chord = Chord::of(key);
        SPECS
            .iter()
            .zip(&self.keys)
            .find(|(s, keys)| s.context == context && keys.contains(&chord))
            .map(|(s, _)| s.action)
    }

    /// The first key for `action` (`ctrl+o`) for status hints, or `None` when unbound.
    pub fn hint(&self, action: KeyAction) -> Option<String> {
        self.keys(action).first().map(Chord::label)
    }

    /// Like [`Keymap::hint`] but `Ctrl+O`, and "unbound" when unbound.
    pub fn title(&self, action: KeyAction) -> String {
        self.keys(action)
            .first()
            .map_or_else(|| "unbound".into(), Chord::title)
    }

    /// `/keys`: every action with its keys.
    pub fn lines(&self) -> Vec<String> {
        let width = SPECS.iter().map(|s| s.name.len()).max().unwrap_or(0);
        let labels: Vec<String> = SPECS
            .iter()
            .zip(&self.keys)
            .zip(&self.custom)
            .map(|((s, keys), &custom)| {
                if keys.is_empty() {
                    return "(unbound)".to_string();
                }
                let extra: Vec<Chord> = if custom {
                    Vec::new()
                } else {
                    variants(s.action)
                        .iter()
                        .filter_map(|k| Chord::parse(k).ok())
                        .collect()
                };
                let shown: Vec<String> = keys
                    .iter()
                    .filter(|k| !extra.contains(k))
                    .map(Chord::label)
                    .collect();
                let hidden = keys.len() - shown.len();
                if shown.is_empty() {
                    return keys.iter().map(Chord::label).collect::<Vec<_>>().join(", ");
                }
                let mut label = shown.join(", ");
                if hidden > 0 {
                    label.push_str(&format!(" (+{hidden} with extra modifiers)"));
                }
                label
            })
            .collect();
        let key_width = labels.iter().map(String::len).max().unwrap_or(0);
        let mut out = Vec::new();
        for (context, heading) in [(Global, "Global"), (Editor, "Input")] {
            out.push(heading.to_string());
            for (s, keys) in SPECS
                .iter()
                .zip(&labels)
                .filter(|(s, _)| s.context == context)
            {
                out.push(format!(
                    "  {:width$}  {keys:key_width$}  {}",
                    s.name, s.description
                ));
            }
        }
        out
    }
}

/// The keymap from `<home>/keybindings.toml` (defaults when it is absent).
pub fn load(home: &Path) -> (Keymap, Vec<String>) {
    let path = home.join("keybindings.toml");
    match std::fs::read_to_string(&path) {
        Ok(text) => Keymap::parse(&text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Keymap::default(), Vec::new()),
        Err(e) => (
            Keymap::default(),
            vec![format!("could not read {}: {e}", path.display())],
        ),
    }
}

static CURRENT: Mutex<Option<Arc<Keymap>>> = Mutex::new(None);

/// Use `keys` for the rest of the session.
pub fn install(keys: Keymap) {
    *CURRENT.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(keys));
}

/// The keymap in effect (the defaults until [`install`]).
pub fn current() -> Arc<Keymap> {
    CURRENT
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_with(Default::default)
        .clone()
}

/// The first key for `action`, or "unbound", for status hints.
pub fn hint(action: KeyAction) -> String {
    current().hint(action).unwrap_or_else(|| "unbound".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use KeyCode::*;
    use KeyModifiers as M;

    fn ev(code: KeyCode, mods: M) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    #[test]
    fn parses_key_strings() {
        let p = |s: &str| Chord::parse(s).map(|c| c.label());
        assert_eq!(p("ctrl+x").unwrap(), "ctrl+x");
        assert_eq!(p("Ctrl+X").unwrap(), "ctrl+x");
        assert_eq!(p("alt+enter").unwrap(), "alt+enter");
        assert_eq!(p("shift+tab").unwrap(), "shift+tab");
        assert_eq!(p("backtab").unwrap(), "shift+tab");
        assert_eq!(p("f5").unwrap(), "f5");
        assert_eq!(p("esc").unwrap(), "esc");
        assert_eq!(p("ctrl++").unwrap(), "ctrl+plus");
        assert_eq!(p("ctrl+shift+z").unwrap(), "ctrl+shift+z");
        assert_eq!(p("shift+super+z").unwrap(), "super+shift+z");
        assert_eq!(
            p("alt+shift+<").unwrap(),
            "alt+<",
            "shift is part of the character"
        );
        assert_eq!(p("super+space").unwrap(), "super+space");
        assert!(p("hyper+x").unwrap_err().contains("unknown modifier"));
        assert!(p("ctrl+banana").unwrap_err().contains("unknown key"));
        assert!(p("ctrl+").unwrap_err().contains("no key"));
        assert!(p("f25").is_err());
        assert!(p("x").unwrap_err().contains("types text"));
        assert!(p("shift+x").unwrap_err().contains("types text"));
    }

    #[test]
    fn events_match_like_the_old_hard_coded_keys() {
        let k = Keymap::default();
        let at = |code, mods| k.action(Editor, &ev(code, mods));
        assert_eq!(
            k.action(Global, &ev(Char('o'), M::CONTROL)),
            Some(A::ExpandTools)
        );
        assert_eq!(
            k.action(Global, &ev(BackTab, M::SHIFT)),
            Some(A::CycleEffort)
        );
        assert_eq!(
            k.action(Global, &ev(BackTab, M::NONE)),
            Some(A::CycleEffort)
        );
        assert_eq!(
            k.action(Global, &ev(Char('z'), M::CONTROL | M::SHIFT)),
            None,
            "ctrl+shift+z is redo"
        );
        assert_eq!(at(Char('Z'), M::CONTROL | M::SHIFT), Some(A::Redo));
        assert_eq!(at(Char('_'), M::CONTROL | M::SHIFT), Some(A::Undo));
        assert_eq!(at(Char('7'), M::CONTROL), Some(A::Undo));
        assert_eq!(at(Char('<'), M::ALT | M::SHIFT), Some(A::InputStart));
        assert_eq!(at(Enter, M::NONE), Some(A::Submit));
        assert_eq!(at(Enter, M::SHIFT), Some(A::Newline));
        assert_eq!(at(Enter, M::ALT), Some(A::FollowUp));
        assert_eq!(at(Up, M::ALT), Some(A::Dequeue));
        assert_eq!(at(Up, M::SUPER), Some(A::InputStart));
        assert_eq!(at(Char('c'), M::CONTROL | M::ALT), None, "AltGr types text");
        assert_eq!(at(Char('a'), M::NONE), None);
        assert!(k.conflicts().is_empty(), "{:?}", k.conflicts());
        assert_eq!(k.hint(A::ExpandTools).as_deref(), Some("ctrl+o"));
        assert_eq!(k.title(A::Interrupt), "Esc");
        assert_eq!(k.title(A::Redo), "Alt+_");
    }

    #[test]
    fn overrides_unbinds_and_warnings() {
        let (k, w) = Keymap::parse(
            r#"
            expand_tools = "ctrl+t"
            history_search = ["ctrl+r", "alt+r"]
            newline = "shift+enter"
            paste_image = []
            "#,
        );
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(
            k.action(Global, &ev(Char('t'), M::CONTROL)),
            Some(A::ExpandTools)
        );
        assert_eq!(k.action(Global, &ev(Char('o'), M::CONTROL)), None);
        assert_eq!(
            k.action(Editor, &ev(Char('r'), M::ALT)),
            Some(A::HistorySearch)
        );
        assert_eq!(k.action(Editor, &ev(Char('j'), M::CONTROL)), None);
        assert!(k.keys(A::PasteImage).is_empty());
        assert_eq!(k.title(A::PasteImage), "unbound");
        assert!(k
            .lines()
            .iter()
            .any(|l| l.contains("paste_image") && l.contains("(unbound)")));
        assert_eq!(
            k.keys(A::Submit),
            Keymap::default().keys(A::Submit),
            "others keep their defaults"
        );

        // A key the file binds leaves the action that had it by default.
        let (k, w) = Keymap::parse("delete_word = \"ctrl+o\"");
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(k.action(Global, &ev(Char('o'), M::CONTROL)), None);
        assert_eq!(
            k.action(Editor, &ev(Char('o'), M::CONTROL)),
            Some(A::DeleteWord)
        );
        assert_eq!(
            k.action(Global, &ev(Char('o'), M::CONTROL | M::SHIFT)),
            None,
            "its variants go too"
        );
        let line = |name: &str| {
            k.lines()
                .into_iter()
                .find(|l| l.trim_start().starts_with(&format!("{name} ")))
                .unwrap()
        };
        assert!(
            line("expand_tools").contains("(unbound)"),
            "{}",
            line("expand_tools")
        );
        assert!(
            line("delete_word").contains(" ctrl+o "),
            "{}",
            line("delete_word")
        );
        assert!(
            !line("delete_word").contains("ctrl+w"),
            "the file replaces the defaults"
        );
        assert!(Keymap::default()
            .lines()
            .iter()
            .any(|l| l.contains(" enter (+5 with extra modifiers) ")));
        // Stealing one of several defaults keeps the rest and their variants.
        let (k, _) = Keymap::parse("yank = \"home\"");
        assert_eq!(k.keys(A::LineStart)[0].label(), "ctrl+a");
        assert_eq!(
            k.action(Editor, &ev(Home, M::ALT)),
            None,
            "alt+home went with home"
        );
        assert_eq!(
            k.action(Editor, &ev(Char('a'), M::CONTROL | M::SHIFT)),
            Some(A::LineStart)
        );

        let (k, w) = Keymap::parse(
            "frobnicate = \"ctrl+x\"\nundo = [\"ctrl+nope\", 3, \"ctrl+u\"]\nredo = 5\nyank = \"y\"\n",
        );
        assert_eq!(w.len(), 5, "{w:?}");
        assert!(w[0].contains("unknown action `frobnicate`"));
        assert!(w[1].contains("unknown key `nope`"));
        assert!(w[2].contains("must be strings"));
        assert!(w[3].contains("`redo` must be a key string"));
        assert!(w[4].contains("types text"));
        assert_eq!(k.keys(A::Undo).len(), 1, "the good key in a list stays");
        assert_eq!(k.keys(A::Redo), Keymap::default().keys(A::Redo));
        assert_eq!(
            k.keys(A::Yank),
            Keymap::default().keys(A::Yank),
            "a rejected key keeps the defaults"
        );
        let (k, _) = Keymap::parse("model_menu = \"ctrl+banana\"");
        assert_eq!(k, Keymap::default(), "no usable key keeps the defaults");

        let (k, w) = Keymap::parse("not toml = [");
        assert_eq!(k, Keymap::default());
        assert!(w[0].contains("using the default keys"), "{w:?}");
    }

    #[test]
    fn conflicts_warn_and_the_earlier_action_wins() {
        let (k, w) = Keymap::parse("reload = \"f5\"\nsubmit = [\"enter\", \"f5\"]\n");
        assert_eq!(
            w,
            ["keybindings.toml: f5 is bound to both reload and submit; reload wins"]
        );
        assert_eq!(k.action(Global, &ev(F(5), M::NONE)), Some(A::Reload));
    }

    #[test]
    fn loads_from_the_profile() {
        let home = tempfile::tempdir().unwrap();
        assert_eq!(load(home.path()), (Keymap::default(), Vec::new()));
        std::fs::write(home.path().join("keybindings.toml"), "model_menu = \"f2\"").unwrap();
        let (k, w) = load(home.path());
        assert!(w.is_empty());
        assert_eq!(k.hint(A::ModelMenu).as_deref(), Some("f2"));
    }

    /// What a key did before bindings were configurable (no selection, no
    /// search, idle): `Some(Some(action))`, `Some(None)` for the keys that
    /// stay fixed (typing, Backspace, Delete, Super+A), `None` for nothing.
    fn old_action(code: KeyCode, m: M) -> Option<Option<KeyAction>> {
        let (ctrl, alt, shift, sup) = (
            m.contains(M::CONTROL) && !m.contains(M::ALT),
            m.contains(M::ALT) && !m.contains(M::CONTROL),
            m.contains(M::SHIFT),
            m.contains(M::SUPER),
        );
        let act = |a| Some(Some(a));
        // Global keys, checked before the editor.
        match code {
            Char('g') if ctrl => return act(A::Reload),
            Char('o') if ctrl => return act(A::ExpandTools),
            Char('v') if ctrl => return act(A::PasteImage),
            Char('l') if ctrl => return act(A::ModelMenu),
            BackTab => return act(A::CycleEffort),
            Char('z') if ctrl && !shift => return act(A::Suspend),
            _ => {}
        }
        match code {
            Char('r') if ctrl => return act(A::HistorySearch),
            Char('_' | '7' | '-') if ctrl => return act(A::Undo),
            Char('_') if alt => return act(A::Redo),
            Char('z' | 'Z') if (ctrl || sup) && shift => return act(A::Redo),
            Char('z') if sup => return act(A::Undo),
            _ => {}
        }
        match code {
            Char('c') if ctrl => act(A::Clear),
            Esc => act(A::Interrupt),
            Up if alt => act(A::Dequeue),
            Char('d') if ctrl => act(A::Exit),
            Enter if alt => act(A::FollowUp),
            Enter if shift => act(A::Newline),
            Char('j') if ctrl => act(A::Newline),
            Enter => act(A::Submit),
            Char('a') if sup => Some(None),
            Home if ctrl => act(A::InputStart),
            End if ctrl => act(A::InputEnd),
            Up | Home if sup => act(A::InputStart),
            Down | End if sup => act(A::InputEnd),
            Char('<') if alt => act(A::InputStart),
            Char('>') if alt => act(A::InputEnd),
            Left if ctrl || alt => act(A::WordLeft),
            Right if ctrl || alt => act(A::WordRight),
            Char('b') if alt => act(A::WordLeft),
            Char('f') if alt => act(A::WordRight),
            Char('b') if ctrl => act(A::CharLeft),
            Char('f') if ctrl => act(A::CharRight),
            Backspace if alt => act(A::DeleteWord),
            Char('w') if ctrl => act(A::DeleteWord),
            Char('d') if alt => act(A::DeleteWordForward),
            Delete if ctrl || alt => act(A::DeleteWordForward),
            Char('u') if ctrl => act(A::DeleteToLineStart),
            Backspace if sup => act(A::DeleteToLineStart),
            Char('k') if ctrl => act(A::DeleteToLineEnd),
            Char('y') if ctrl => act(A::Yank),
            Left if sup => act(A::LineStart),
            Right if sup => act(A::LineEnd),
            Home => act(A::LineStart),
            Char('a') if ctrl => act(A::LineStart),
            End => act(A::LineEnd),
            Char('e') if ctrl => act(A::LineEnd),
            Backspace | Delete => Some(None),
            Left => act(A::CharLeft),
            Right => act(A::CharRight),
            Up => act(A::HistoryPrev),
            Char('p') if ctrl => act(A::HistoryPrev),
            Down => act(A::HistoryNext),
            Char('n') if ctrl => act(A::HistoryNext),
            Tab => act(A::Complete),
            Char(c) if !ctrl && !alt && !sup && !c.is_control() => Some(None),
            _ => None,
        }
    }

    /// What a key does now: global actions first, then the editor's (which
    /// sees Shift+movement as the plain movement).
    fn new_action(k: &Keymap, mut key: KeyEvent) -> Option<KeyAction> {
        k.action(Global, &key).or_else(|| {
            if matches!(key.code, Left | Right | Up | Down | Home | End) {
                key.modifiers -= M::SHIFT;
            }
            k.action(Editor, &key)
        })
    }

    /// Every key that did something before still does the same by default
    /// (the old checks ignored extra modifiers, so the defaults list them).
    #[test]
    fn default_keys_do_everything_the_old_keys_did() {
        let k = Keymap::default();
        let mut codes = vec![
            Esc, Enter, Tab, BackTab, Backspace, Delete, Home, End, Left, Right, Up, Down,
        ];
        codes.extend("abcdefghijklmnopqrstuvwxyz_7-<> ".chars().map(Char));
        codes.extend(('A'..='Z').map(Char));
        let mut missing = Vec::new();
        for code in codes {
            for bits in 0..32u8 {
                let mut m = M::NONE;
                for (bit, flag) in [M::CONTROL, M::ALT, M::SHIFT, M::SUPER, M::HYPER]
                    .into_iter()
                    .enumerate()
                {
                    if bits & (1 << bit) != 0 {
                        m |= flag;
                    }
                }
                // Shifted letters arrive uppercase; Shift+Tab arrives as BackTab.
                if matches!(code, Char(c) if c.is_uppercase()) && !m.contains(M::SHIFT)
                    || code == Tab && m.contains(M::SHIFT)
                {
                    continue;
                }
                let now = new_action(&k, ev(code, m));
                if let Some(old) = old_action(code, m) {
                    if old != now {
                        missing.push(format!(
                            "MISSING {:?} {}",
                            old,
                            Chord::of(&ev(code, m)).label()
                        ));
                    }
                }
            }
        }
        missing.sort();
        missing.dedup();
        assert!(missing.is_empty(), "{}", missing.join("\n"));
    }
}
