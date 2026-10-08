//! User settings in `<home>/settings.json` and the catalog the `/settings`
//! menu shows. Saving merges into the existing file, so keys other parts of
//! nyq read (`provider`, `models`) and unknown keys survive.

use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Footer segments that `/statusline` toggles.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Segment {
    Model,
    Provider,
    Context,
    Tokens,
    Sandbox,
    Folder,
    Branch,
}

impl Segment {
    pub const ALL: [Segment; 7] = [
        Segment::Model,
        Segment::Provider,
        Segment::Context,
        Segment::Tokens,
        Segment::Sandbox,
        Segment::Folder,
        Segment::Branch,
    ];

    pub fn key(self) -> &'static str {
        match self {
            Segment::Model => "model",
            Segment::Provider => "provider",
            Segment::Context => "context",
            Segment::Tokens => "tokens",
            Segment::Sandbox => "sandbox",
            Segment::Folder => "folder",
            Segment::Branch => "branch",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Segment::Model => "Effort · model",
            Segment::Provider => "Provider",
            Segment::Context => "Context",
            Segment::Tokens => "Tokens",
            Segment::Sandbox => "Sandbox",
            Segment::Folder => "Folder",
            Segment::Branch => "Git branch",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Segment::Model => "Show reasoning effort, model and fast mode",
            Segment::Provider => "Show the provider",
            Segment::Context => "Show context-window usage",
            Segment::Tokens => "Show input and output tokens",
            Segment::Sandbox => "Show the sandbox and permission mode",
            Segment::Folder => "Show the workspace folder",
            Segment::Branch => "Show the current Git branch",
        }
    }

    pub fn parse(s: &str) -> Option<Segment> {
        let s = s.trim().to_ascii_lowercase();
        Segment::ALL
            .into_iter()
            .find(|seg| seg.key() == s)
            .or(match s.as_str() {
                "effort" => Some(Segment::Model),
                "git" => Some(Segment::Branch),
                "workspace" | "cwd" => Some(Segment::Folder),
                "usage" => Some(Segment::Tokens),
                _ => None,
            })
    }
}

fn yes() -> bool {
    true
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusLine {
    #[serde(default = "yes")]
    pub model: bool,
    #[serde(default = "yes")]
    pub provider: bool,
    #[serde(default = "yes")]
    pub context: bool,
    #[serde(default = "yes")]
    pub tokens: bool,
    #[serde(default = "yes")]
    pub sandbox: bool,
    #[serde(default = "yes")]
    pub folder: bool,
    #[serde(default = "yes")]
    pub branch: bool,
}

impl Default for StatusLine {
    fn default() -> Self {
        Self {
            model: true,
            provider: true,
            context: true,
            tokens: true,
            sandbox: true,
            folder: true,
            branch: true,
        }
    }
}

impl StatusLine {
    pub fn get(&self, seg: Segment) -> bool {
        *self.slot(seg)
    }

    pub fn set(&mut self, seg: Segment, on: bool) {
        *self.slot_mut(seg) = on;
    }

    fn slot(&self, seg: Segment) -> &bool {
        match seg {
            Segment::Model => &self.model,
            Segment::Provider => &self.provider,
            Segment::Context => &self.context,
            Segment::Tokens => &self.tokens,
            Segment::Sandbox => &self.sandbox,
            Segment::Folder => &self.folder,
            Segment::Branch => &self.branch,
        }
    }

    fn slot_mut(&mut self, seg: Segment) -> &mut bool {
        match seg {
            Segment::Model => &mut self.model,
            Segment::Provider => &mut self.provider,
            Segment::Context => &mut self.context,
            Segment::Tokens => &mut self.tokens,
            Segment::Sandbox => &mut self.sandbox,
            Segment::Folder => &mut self.folder,
            Segment::Branch => &mut self.branch,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Default reasoning effort for Codex models (`auto` or absent = model default).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// Codex fast mode (the `priority` service tier) where the model offers it.
    pub fast_mode: bool,
    /// `ask` (default), `auto` (classifier), or `yolo` (never ask). Deny rules still block.
    pub permission_mode: String,
    /// Model id for the auto-mode classifier. Empty uses the session model.
    /// A small or cheap model is the right choice when one is configured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub classifier_model: Option<String>,
    /// Default sandbox when `--sandbox` isn't given: workspace-write, read-only or off.
    pub sandbox: String,
    /// Compact automatically at this share of the context window (percent; 0 = never).
    pub auto_compact: u8,
    /// Terminal bell: `off`, `on` (long turns and prompts) or `max` (every turn).
    pub sound: String,
    /// Desktop notifications for long turns and prompts.
    pub notifications: bool,
    /// A turn counts as long after this many seconds.
    pub notify_after_secs: u64,
    pub statusline: StatusLine,
    /// Save accepted prompts to `<home>/history`.
    pub prompt_history: bool,
    /// Keys nyq doesn't manage here (`provider`, `models`, …), kept on save.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            effort: None,
            fast_mode: false,
            permission_mode: "ask".into(),
            classifier_model: None,
            sandbox: "workspace-write".into(),
            auto_compact: 80,
            // Elsewhere the bell is the only sound, so it stays opt-in.
            sound: if cfg!(target_os = "macos") {
                "on"
            } else {
                "off"
            }
            .into(),
            notifications: false,
            notify_after_secs: 10,
            statusline: StatusLine::default(),
            prompt_history: true,
            extra: Map::new(),
        }
    }
}

static CURRENT: Mutex<Option<Settings>> = Mutex::new(None);

/// The settings in effect (loaded once from the profile).
pub fn current() -> Settings {
    let mut guard = CURRENT.lock().unwrap_or_else(|e| e.into_inner());
    // Tests never read the real profile.
    guard
        .get_or_insert_with(|| {
            if cfg!(test) {
                Settings::default()
            } else {
                load(&crate::config::home()).0
            }
        })
        .clone()
}

/// Make `s` the settings in effect (without saving).
pub fn install(s: Settings) {
    *CURRENT.lock().unwrap_or_else(|e| e.into_inner()) = Some(s);
}

/// The configured classifier model, when it is non-empty.
pub fn classifier_model() -> Option<String> {
    current()
        .classifier_model
        .map(|model| model.trim().to_string())
        .filter(|model| !model.is_empty())
}

/// Change the settings in effect and save them to `home`.
pub fn update(home: &Path, f: impl FnOnce(&mut Settings)) -> Result<Settings> {
    let mut s = current();
    f(&mut s);
    install(s.clone());
    save(home, &s)?;
    Ok(s)
}

/// Read `<home>/settings.json`. Problems are returned as a warning and the
/// defaults are used.
pub fn load(home: &Path) -> (Settings, Option<String>) {
    let path = home.join("settings.json");
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (Settings::default(), None),
        Err(e) => {
            return (
                Settings::default(),
                Some(format!("reading {}: {e}", path.display())),
            )
        }
    };
    match serde_json::from_str::<Settings>(&text) {
        Ok(s) => (s, None),
        Err(e) => (
            Settings::default(),
            Some(format!("{}: {e} (using default settings)", path.display())),
        ),
    }
}

/// Write the settings atomically (0600), merged over the file's other keys.
pub fn save(home: &Path, s: &Settings) -> Result<()> {
    let path = home.join("settings.json");
    let mut merged = match std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
    {
        Some(Value::Object(m)) => m,
        _ => Map::new(),
    };
    let Value::Object(ours) = serde_json::to_value(s)? else {
        unreachable!("settings serialize to an object")
    };
    if s.effort.is_none() {
        merged.remove("effort");
    }
    merged.extend(ours);
    let bytes = serde_json::to_vec_pretty(&Value::Object(merged))?;
    crate::session::write_private(&path, &bytes)
        .with_context(|| format!("writing {}", path.display()))
}

// ------------------------------------------------------------------ catalog

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Category {
    All,
    StatusLine,
    Agent,
    Notifications,
    Advanced,
}

impl Category {
    pub const ALL: [Category; 5] = [
        Category::All,
        Category::StatusLine,
        Category::Agent,
        Category::Notifications,
        Category::Advanced,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Category::All => "All",
            Category::StatusLine => "Status line",
            Category::Agent => "Agent",
            Category::Notifications => "Notifications",
            Category::Advanced => "Advanced",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Id {
    Model,
    Effort,
    Fast,
    Permission,
    Sandbox,
    AutoCompact,
    Sound,
    Notifications,
    NotifyAfter,
    Segment(Segment),
    PromptHistory,
}

/// Session state the menu shows next to the saved settings.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Live {
    pub model: String,
    pub effort: Option<String>,
    /// Efforts the current model supports (empty: not adjustable).
    pub efforts: Vec<String>,
    pub fast: bool,
    pub fast_supported: bool,
    /// Session permission mode (`ask`, `auto`, or `yolo`).
    pub permission_mode: String,
}

pub struct Spec {
    pub id: Id,
    pub category: Category,
    pub label: &'static str,
    pub description: &'static str,
}

/// Every setting in menu order.
pub fn specs() -> Vec<Spec> {
    let mut v = vec![
        Spec {
            id: Id::Model,
            category: Category::Agent,
            label: "Model",
            description: "Choose the model used for new turns",
        },
        Spec {
            id: Id::Effort,
            category: Category::Agent,
            label: "Reasoning effort",
            description: "Control how much reasoning the model applies",
        },
        Spec {
            id: Id::Fast,
            category: Category::Agent,
            label: "Fast mode",
            description: "Use faster inference when the model supports it",
        },
        Spec {
            id: Id::Permission,
            category: Category::Agent,
            label: "Permission mode",
            description:
                "Ask before risky calls, let a classifier decide (auto), or run them freely (yolo)",
        },
        Spec {
            id: Id::Sandbox,
            category: Category::Agent,
            label: "Default sandbox",
            description: "Sandbox for shell commands from the next start",
        },
        Spec {
            id: Id::AutoCompact,
            category: Category::Agent,
            label: "Auto-compact at",
            description: "Summarize the conversation at this share of the context",
        },
        Spec {
            id: Id::Sound,
            category: Category::Notifications,
            label: "Sound level",
            description: "Terminal bell: off, on (long turns, prompts) or max",
        },
        Spec {
            id: Id::Notifications,
            category: Category::Notifications,
            label: "Desktop notifications",
            description: "Notify when a long turn ends or nyq needs you",
        },
        Spec {
            id: Id::NotifyAfter,
            category: Category::Notifications,
            label: "Long turn after",
            description: "Turns longer than this ring and notify",
        },
    ];
    v.extend(Segment::ALL.into_iter().map(|seg| Spec {
        id: Id::Segment(seg),
        category: Category::StatusLine,
        label: seg.label(),
        description: seg.description(),
    }));
    v.push(Spec {
        id: Id::PromptHistory,
        category: Category::Advanced,
        label: "Prompt history",
        description: "Save accepted prompts for history across sessions",
    });
    v
}

fn on_off(b: bool) -> String {
    if b { "on" } else { "off" }.into()
}

/// The value shown for a setting.
pub fn value(id: Id, s: &Settings, live: &Live) -> String {
    match id {
        Id::Model => live.model.clone(),
        Id::Effort => live.effort.clone().unwrap_or_else(|| "auto".into()),
        Id::Fast if !live.fast_supported && !live.fast => "unavailable".into(),
        Id::Fast => on_off(live.fast),
        Id::Permission => {
            if live.permission_mode.is_empty() {
                "ask".into()
            } else {
                live.permission_mode.clone()
            }
        }
        Id::Sandbox => s.sandbox.clone(),
        Id::AutoCompact if s.auto_compact == 0 => "off".into(),
        Id::AutoCompact => format!("{}%", s.auto_compact),
        Id::Sound => s.sound.clone(),
        Id::Notifications => on_off(s.notifications),
        Id::NotifyAfter => format!("{}s", s.notify_after_secs),
        Id::Segment(seg) => on_off(s.statusline.get(seg)),
        Id::PromptHistory => on_off(s.prompt_history),
    }
}

/// The values a setting cycles through (empty: not cycled in place).
pub fn options(id: Id, live: &Live) -> Vec<String> {
    let list = |v: &[&str]| v.iter().map(|s| s.to_string()).collect();
    match id {
        Id::Model => Vec::new(),
        Id::Effort if live.efforts.is_empty() => Vec::new(),
        Id::Effort => std::iter::once("auto".to_string())
            .chain(live.efforts.iter().cloned())
            .collect(),
        Id::Fast if !live.fast_supported && !live.fast => Vec::new(),
        Id::Permission => list(&["ask", "auto", "yolo"]),
        Id::Sandbox => list(&["workspace-write", "read-only", "off"]),
        Id::AutoCompact => list(&["off", "50%", "60%", "70%", "80%", "90%"]),
        Id::Sound => list(&["off", "on", "max"]),
        Id::NotifyAfter => list(&["0s", "5s", "10s", "30s", "60s", "120s"]),
        Id::Fast | Id::Notifications | Id::Segment(_) | Id::PromptHistory => list(&["off", "on"]),
    }
}

/// The next value after stepping `delta` through the options.
pub fn cycle(id: Id, s: &Settings, live: &Live, delta: i32) -> Option<String> {
    let opts = options(id, live);
    if opts.is_empty() {
        return None;
    }
    let cur = value(id, s, live);
    let n = opts.len() as i32;
    let i = opts
        .iter()
        .position(|o| o.eq_ignore_ascii_case(&cur))
        .map_or(if delta > 0 { -1 } else { 0 }, |i| i as i32);
    Some(opts[(i + delta).rem_euclid(n) as usize].clone())
}

/// Store a saved setting's new value. Returns false for session-only
/// settings that the app applies itself (model, effort, fast, permission).
pub fn apply(id: Id, value: &str, s: &mut Settings) -> bool {
    let on = value == "on";
    match id {
        Id::Sandbox => s.sandbox = value.into(),
        Id::AutoCompact => s.auto_compact = value.trim_end_matches('%').parse().unwrap_or(0),
        Id::Sound => s.sound = value.into(),
        Id::Notifications => s.notifications = on,
        Id::NotifyAfter => s.notify_after_secs = value.trim_end_matches('s').parse().unwrap_or(10),
        Id::Segment(seg) => s.statusline.set(seg, on),
        Id::PromptHistory => s.prompt_history = on,
        Id::Model | Id::Effort | Id::Fast | Id::Permission => return false,
    }
    true
}

/// The settings of `category` matching every word of `query`.
pub fn filtered(category: Category, query: &str, s: &Settings, live: &Live) -> Vec<Spec> {
    let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    specs()
        .into_iter()
        .filter(|spec| category == Category::All || spec.category == category)
        .filter(|spec| {
            let hay = format!(
                "{} {} {}",
                spec.label,
                spec.description,
                value(spec.id, s, live)
            )
            .to_lowercase();
            words.iter().all(|w| hay.contains(w))
        })
        .collect()
}

/// The sandbox mode the settings ask for by default.
pub fn sandbox_mode(s: &Settings) -> crate::sandbox::Mode {
    use clap::ValueEnum;
    crate::sandbox::Mode::from_str(&s.sandbox, true).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_merges_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("settings.json"),
            r#"{"provider":"codex","models":{"codex":"m"},"custom":1}"#,
        )
        .unwrap();
        let (mut s, warn) = load(dir.path());
        assert!(warn.is_none());
        assert_eq!(s.auto_compact, 80);
        s.fast_mode = true;
        s.sound = "max".into();
        s.statusline.set(Segment::Branch, false);
        s.effort = Some("high".into());
        save(dir.path(), &s).unwrap();
        let raw: Value =
            serde_json::from_slice(&std::fs::read(dir.path().join("settings.json")).unwrap())
                .unwrap();
        assert_eq!(raw["provider"], "codex");
        assert_eq!(raw["models"]["codex"], "m");
        assert_eq!(raw["custom"], 1);
        assert_eq!(raw["statusline"]["branch"], false);
        assert_eq!(raw["effort"], "high");
        let (back, _) = load(dir.path());
        assert_eq!(back, s);
        // Clearing the effort removes the key instead of leaving a stale one.
        s.effort = None;
        save(dir.path(), &s).unwrap();
        let raw: Value =
            serde_json::from_slice(&std::fs::read(dir.path().join("settings.json")).unwrap())
                .unwrap();
        assert!(raw.get("effort").is_none());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join("settings.json"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // Only the settings file is left behind (no temp files).
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn partial_and_bad_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("settings.json"),
            r#"{"statusline":{"tokens":false}}"#,
        )
        .unwrap();
        let (s, warn) = load(dir.path());
        assert!(warn.is_none());
        assert!(!s.statusline.tokens && s.statusline.model && s.prompt_history);
        std::fs::write(dir.path().join("settings.json"), r#"{"sound": 5}"#).unwrap();
        let (s, warn) = load(dir.path());
        assert!(warn.is_some());
        assert_eq!(s, Settings::default());
        assert_eq!(load(&dir.path().join("missing")).0, Settings::default());
    }

    #[test]
    fn catalog_cycles_values() {
        let s = Settings {
            sound: "off".into(),
            ..Settings::default()
        };
        let live = Live {
            efforts: vec!["low".into(), "high".into()],
            ..Live::default()
        };
        assert_eq!(cycle(Id::Sound, &s, &live, 1).as_deref(), Some("on"));
        assert_eq!(cycle(Id::Sound, &s, &live, -1).as_deref(), Some("max"));
        assert_eq!(cycle(Id::Effort, &s, &live, 1).as_deref(), Some("low"));
        assert_eq!(cycle(Id::Effort, &s, &live, -1).as_deref(), Some("high"));
        assert_eq!(cycle(Id::Model, &s, &live, 1), None);
        // Fast mode can't be turned on for a model without it.
        assert_eq!(value(Id::Fast, &s, &live), "unavailable");
        assert_eq!(cycle(Id::Fast, &s, &live, 1), None);
        let fast = Live {
            fast_supported: true,
            ..live.clone()
        };
        assert_eq!(cycle(Id::Fast, &s, &fast, 1).as_deref(), Some("on"));
        assert_eq!(cycle(Id::AutoCompact, &s, &live, 1).as_deref(), Some("90%"));
        let mut t = s.clone();
        assert!(apply(Id::AutoCompact, "off", &mut t) && t.auto_compact == 0);
        assert!(apply(Id::NotifyAfter, "30s", &mut t) && t.notify_after_secs == 30);
        assert!(apply(Id::Segment(Segment::Folder), "off", &mut t) && !t.statusline.folder);
        assert!(!apply(Id::Fast, "on", &mut t));
        assert_eq!(
            filtered(Category::StatusLine, "", &s, &live).len(),
            Segment::ALL.len()
        );
        assert_eq!(
            filtered(Category::All, "git branch", &s, &live)[0].id,
            Id::Segment(Segment::Branch)
        );
        assert!(filtered(Category::Agent, "nothing like this", &s, &live).is_empty());
        assert_eq!(Segment::parse("git"), Some(Segment::Branch));
        assert_eq!(
            sandbox_mode(&Settings {
                sandbox: "read-only".into(),
                ..s
            }),
            crate::sandbox::Mode::ReadOnly
        );
    }
}
