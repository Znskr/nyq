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
    Spec { action, name, context, defaults, description }
}

use Context::{Editor, Global};
use KeyAction as A;

/// Every bindable action, in lookup order (an earlier action wins a conflict).
pub const SPECS: &[Spec] = &[
    spec(A::Reload, "reload", Global, &["ctrl+g"], "Relaunch into an installed upgrade"),
    spec(A::ExpandTools, "expand_tools", Global, &["ctrl+o"], "Full transcript with every tool call; review a pending approval"),
    spec(A::PasteImage, "paste_image", Global, &["ctrl+v"], "Attach an image from the clipboard"),
    spec(A::ModelMenu, "model_menu", Global, &["ctrl+l"], "Switch model (/model)"),
    spec(A::CycleEffort, "cycle_effort", Global, &["shift+tab"], "Next reasoning effort"),
    spec(A::Suspend, "suspend", Global, &["ctrl+z"], "Suspend to the shell (Unix)"),
    spec(A::Submit, "submit", Editor, &["enter"], "Send the message (steers while working); after a trailing \\ inserts a newline"),
    spec(A::Newline, "newline", Editor, &["shift+enter", "ctrl+j"], "Insert a newline"),
    spec(A::FollowUp, "follow_up", Editor, &["alt+enter"], "While working, queue a follow-up; otherwise insert a newline"),
    spec(A::Interrupt, "interrupt", Editor, &["esc"], "Interrupt the turn; twice clears the input"),
    spec(A::Clear, "clear", Editor, &["ctrl+c"], "Clear the input; when empty interrupt, or press twice to exit"),
    spec(A::Exit, "exit", Editor, &["ctrl+d"], "Exit on an empty input; otherwise delete the next character"),
    spec(A::Dequeue, "dequeue", Editor, &["alt+up"], "Move the newest queued message back into the input"),
    spec(A::HistorySearch, "history_search", Editor, &["ctrl+r"], "Reverse-search prompt history (again for older matches)"),
    spec(A::HistoryPrev, "history_prev", Editor, &["up", "ctrl+p"], "Up a row; on the first row, the older prompt"),
    spec(A::HistoryNext, "history_next", Editor, &["down", "ctrl+n"], "Down a row; on the last row, the newer prompt"),
    spec(A::Complete, "complete", Editor, &["tab"], "Complete a command"),
    spec(A::Undo, "undo", Editor, &["ctrl+_", "ctrl+7", "ctrl+-", "super+z"], "Undo"),
    spec(A::Redo, "redo", Editor, &["alt+_", "ctrl+shift+z", "super+shift+z"], "Redo"),
    spec(A::CharLeft, "char_left", Editor, &["left", "ctrl+b"], "Cursor left"),
    spec(A::CharRight, "char_right", Editor, &["right", "ctrl+f"], "Cursor right"),
    spec(A::WordLeft, "word_left", Editor, &["ctrl+left", "alt+left", "alt+b"], "Previous word"),
    spec(A::WordRight, "word_right", Editor, &["ctrl+right", "alt+right", "alt+f"], "Next word"),
    spec(A::LineStart, "line_start", Editor, &["home", "ctrl+a", "super+left"], "Start of line"),
    spec(A::LineEnd, "line_end", Editor, &["end", "ctrl+e", "super+right"], "End of line"),
    spec(A::InputStart, "input_start", Editor, &["ctrl+home", "super+up", "super+home", "alt+<"], "Start of input"),
    spec(A::InputEnd, "input_end", Editor, &["ctrl+end", "super+down", "super+end", "alt+>"], "End of input"),
    spec(A::DeleteWord, "delete_word", Editor, &["alt+backspace", "ctrl+w"], "Delete the previous word"),
    spec(A::DeleteWordForward, "delete_word_forward", Editor, &["alt+d", "ctrl+delete", "alt+delete"], "Delete the next word"),
    spec(A::DeleteToLineStart, "delete_to_line_start", Editor, &["ctrl+u", "super+backspace"], "Delete to the start of the line"),
    spec(A::DeleteToLineEnd, "delete_to_line_end", Editor, &["ctrl+k"], "Delete to the end of the line (joins at the end)"),
    spec(A::Yank, "yank", Editor, &["ctrl+y"], "Paste the last deleted text"),
];

fn index(action: KeyAction) -> usize {
    SPECS.iter().position(|s| s.action == action).expect("every action has a spec")
}

/// The modifiers a binding can name; any other modifier on an event (Hyper,
/// Meta) keeps it from matching.
const MODS: KeyModifiers = KeyModifiers::CONTROL
    .union(KeyModifiers::ALT)
    .union(KeyModifiers::SHIFT)
    .union(KeyModifiers::SUPER)
    .union(KeyModifiers::HYPER)
    .union(KeyModifiers::META);

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
        for m in prefix.split('+').filter(|m| !prefix.is_empty() || !m.is_empty()) {
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
            f if f.len() > 1 && f.starts_with('f') && f[1..].parse::<u8>().is_ok_and(|n| (1..=24).contains(&n)) => {
                KeyCode::F(f[1..].parse().unwrap())
            }
            k if k.chars().count() == 1 => KeyCode::Char(k.chars().next().unwrap()),
            "" => return Err(format!("no key in `{text}`")),
            _ => return Err(format!("unknown key `{key}` in `{text}`")),
        };
        let chord = Chord::new(code, mods);
        // A plain character types text; it can't also be a shortcut.
        if matches!(chord.code, KeyCode::Char(_))
            && !chord.mods.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
        {
            return Err(format!("`{text}` types text; bind it with ctrl, alt or super"));
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
                c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
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
}

impl Default for Keymap {
    fn default() -> Self {
        let keys = SPECS
            .iter()
            .map(|s| s.defaults.iter().map(|k| Chord::parse(k).expect("default keys parse")).collect())
            .collect();
        Keymap { keys }
    }
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
                return (map, vec![format!("keybindings.toml: {first}; using the default keys")]);
            }
        };
        // Which actions the file sets, in file order.
        let mut set: Vec<usize> = Vec::new();
        for (name, item) in doc.iter() {
            let Some(i) = SPECS.iter().position(|s| s.name == name) else {
                warnings.push(format!("keybindings.toml: unknown action `{name}` (see /keys)"));
                continue;
            };
            let values: Vec<Option<&str>> = if let Some(s) = item.as_str() {
                vec![Some(s)]
            } else if let Some(list) = item.as_array() {
                list.iter().map(|v| v.as_str()).collect()
            } else {
                warnings.push(format!("keybindings.toml: `{name}` must be a key string or a list of them"));
                continue;
            };
            let mut keys = Vec::new();
            let given = values.len();
            for v in values {
                match v.map(Chord::parse) {
                    Some(Ok(c)) if !keys.contains(&c) => keys.push(c),
                    Some(Ok(_)) => {}
                    Some(Err(e)) => warnings.push(format!("keybindings.toml: {name}: {e}")),
                    None => warnings.push(format!("keybindings.toml: {name}: keys must be strings")),
                }
            }
            // Nothing usable keeps the defaults; only `[]` unbinds.
            if keys.is_empty() && given > 0 {
                continue;
            }
            map.keys[i] = keys;
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
        self.keys(action).first().map_or_else(|| "unbound".into(), Chord::title)
    }

    /// `/keys`: every action with its keys.
    pub fn lines(&self) -> Vec<String> {
        let width = SPECS.iter().map(|s| s.name.len()).max().unwrap_or(0);
        let labels: Vec<String> = self
            .keys
            .iter()
            .map(|keys| {
                if keys.is_empty() {
                    "(unbound)".to_string()
                } else {
                    keys.iter().map(Chord::label).collect::<Vec<_>>().join(", ")
                }
            })
            .collect();
        let key_width = labels.iter().map(String::len).max().unwrap_or(0);
        let mut out = Vec::new();
        for (context, heading) in [(Global, "Global"), (Editor, "Input")] {
            out.push(heading.to_string());
            for (s, keys) in SPECS.iter().zip(&labels).filter(|(s, _)| s.context == context) {
                out.push(format!("  {:width$}  {keys:key_width$}  {}", s.name, s.description));
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
        Err(e) => (Keymap::default(), vec![format!("could not read {}: {e}", path.display())]),
    }
}

static CURRENT: Mutex<Option<Arc<Keymap>>> = Mutex::new(None);

/// Use `keys` for the rest of the session.
pub fn install(keys: Keymap) {
    *CURRENT.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(keys));
}

/// The keymap in effect (the defaults until [`install`]).
pub fn current() -> Arc<Keymap> {
    CURRENT.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(Default::default).clone()
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
        assert_eq!(p("alt+shift+<").unwrap(), "alt+<", "shift is part of the character");
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
        assert_eq!(k.action(Global, &ev(Char('o'), M::CONTROL)), Some(A::ExpandTools));
        assert_eq!(k.action(Global, &ev(BackTab, M::SHIFT)), Some(A::CycleEffort));
        assert_eq!(k.action(Global, &ev(BackTab, M::NONE)), Some(A::CycleEffort));
        assert_eq!(k.action(Global, &ev(Char('z'), M::CONTROL | M::SHIFT)), None, "ctrl+shift+z is redo");
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
        assert_eq!(k.action(Global, &ev(Char('t'), M::CONTROL)), Some(A::ExpandTools));
        assert_eq!(k.action(Global, &ev(Char('o'), M::CONTROL)), None);
        assert_eq!(k.action(Editor, &ev(Char('r'), M::ALT)), Some(A::HistorySearch));
        assert_eq!(k.action(Editor, &ev(Char('j'), M::CONTROL)), None);
        assert!(k.keys(A::PasteImage).is_empty());
        assert_eq!(k.title(A::PasteImage), "unbound");
        assert!(k.lines().iter().any(|l| l.contains("paste_image") && l.contains("(unbound)")));
        assert_eq!(k.keys(A::Submit), Keymap::default().keys(A::Submit), "others keep their defaults");

        // A key the file binds leaves the action that had it by default.
        let (k, w) = Keymap::parse("delete_word = \"ctrl+o\"");
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(k.action(Global, &ev(Char('o'), M::CONTROL)), None);
        assert_eq!(k.action(Editor, &ev(Char('o'), M::CONTROL)), Some(A::DeleteWord));

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
        assert_eq!(k.keys(A::Yank), Keymap::default().keys(A::Yank), "a rejected key keeps the defaults");
        let (k, _) = Keymap::parse("model_menu = \"ctrl+banana\"");
        assert_eq!(k, Keymap::default(), "no usable key keeps the defaults");

        let (k, w) = Keymap::parse("not toml = [");
        assert_eq!(k, Keymap::default());
        assert!(w[0].contains("using the default keys"), "{w:?}");
    }

    #[test]
    fn conflicts_warn_and_the_earlier_action_wins() {
        let (k, w) = Keymap::parse("reload = \"f5\"\nsubmit = [\"enter\", \"f5\"]\n");
        assert_eq!(w, ["keybindings.toml: f5 is bound to both reload and submit; reload wins"]);
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
}
