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
    Session,
}

impl Segment {
    pub const ALL: [Segment; 8] = [
        Segment::Model,
        Segment::Session,
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
            Segment::Session => "session",
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
            Segment::Session => "Session title",
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
            Segment::Session => "Show the session title",
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
                "title" => Some(Segment::Session),
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
    #[serde(default = "yes")]
    pub session: bool,
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
            session: true,
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
            Segment::Session => &self.session,
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
            Segment::Session => &mut self.session,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Default reasoning effort for Codex models (`auto` or absent = model default).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// Fast mode where the model offers it (Codex `priority` tier, AI Gateway fast speed).
    pub fast_mode: bool,
    /// `ask` (default), `auto` (classifier), or `yolo` (never ask). Deny rules still block.
    pub permission_mode: String,
    /// Model id for the auto-mode classifier. Empty picks the provider's
    /// small model when it has one (Codex: a mini model), else the session model.
    /// A small or cheap model is the right choice when one is configured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub classifier_model: Option<String>,
    /// Default sandbox when `--sandbox` isn't given: workspace-write, read-only or off.
    pub sandbox: String,
    /// Compact automatically at this share of the context window (percent; 0 = never).
    pub auto_compact: u8,
    /// Sound (a system chime where one can play, else the terminal bell): `off`, `on` (long turns and prompts) or `max` (every turn).
    pub sound: String,
    /// Desktop notifications for long turns and prompts.
    pub notifications: bool,
    /// A turn counts as long after this many seconds.
    pub notify_after_secs: u64,
    pub statusline: StatusLine,
    /// Save accepted prompts to `<home>/history`.
    pub prompt_history: bool,
    /// Download and install new releases in the background (interactive sessions).
    pub auto_upgrade: bool,
    /// Model round-trips per turn before nyq stops (default 100;
    /// `NYQ_MAX_AGENT_STEPS` overrides it).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_agent_steps: Option<usize>,
    /// Show categories and skill sources in slash-command results.
    pub slash_menu_categories: bool,
    /// `auto` (follow the terminal background), `dark` or `light`. `NYQ_THEME` overrides it.
    pub theme: String,
    /// Show finished tool groups as their header line only.
    pub collapse_tool_calls: bool,
    /// Push the shell output on screen into the scrollback at start, so the
    /// session begins at the top of the screen.
    pub startup_scrollback: bool,
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
            auto_upgrade: true,
            max_agent_steps: None,
            slash_menu_categories: true,
            theme: "auto".into(),
            collapse_tool_calls: false,
            startup_scrollback: false,
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

/// Change the settings in effect and save them to `home`. Returns them with
/// a notice when a project or workspace layer overrides a changed value.
/// Only the changed keys are written to the user file; an overridden change
/// is saved but the layer's value stays in effect here.
pub fn update(
    home: &Path,
    f: impl FnOnce(&mut Settings),
) -> Result<(Settings, Option<String>)> {
    let before = serde_json::to_value(current())?;
    let mut wanted = current();
    f(&mut wanted);
    let mut effective = serde_json::to_value(&wanted)?;
    let mut changes = Vec::new();
    diff(&before, &effective, &mut Vec::new(), &mut changes);
    write_changes(home, &changes)?;
    let overrides = overrides();
    let mut shadowed = Vec::new();
    for (path, _) in &changes {
        if let Some(layer) = overrides.layer(path) {
            shadowed.push(format!("{} ({})", path.join("."), layer.describe()));
            set_path(&mut effective, path, get_path(&before, path).cloned());
        }
    }
    let effective: Settings = serde_json::from_value(effective)?;
    install(effective.clone());
    let notice = (!shadowed.is_empty())
        .then(|| format!("Saved, but overridden in this workspace by {}", shadowed.join(", ")));
    Ok((effective, notice))
}

// ------------------------------------------------------------------ layers

/// A settings layer above the user's `settings.json`. Later layers win:
/// the user file, then the project's `.nyq/settings.json`, then the user's
/// override for this workspace (`workspaces.<path>` in the user file).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layer {
    Project,
    Workspace,
}

impl Layer {
    pub fn label(self) -> &'static str {
        match self {
            Layer::Project => "project",
            Layer::Workspace => "workspace",
        }
    }

    fn describe(self) -> &'static str {
        match self {
            Layer::Project => ".nyq/settings.json",
            Layer::Workspace => "workspaces in settings.json",
        }
    }
}

/// Keys a project file may set. Anything that loosens safety or picks
/// providers (permission mode, sandbox, models) stays in the user's own file.
const PROJECT_KEYS: &[&str] = &[
    "effort",
    "fast_mode",
    "auto_compact",
    "sound",
    "notifications",
    "notify_after_secs",
    "statusline",
    "prompt_history",
    "slash_menu_categories",
];

#[derive(Clone, Debug, Default)]
struct Overrides {
    project: Map<String, Value>,
    workspace: Map<String, Value>,
}

impl Overrides {
    /// The highest layer that sets `path`.
    fn layer(&self, path: &[String]) -> Option<Layer> {
        let has = |m: &Map<String, Value>| {
            get_path(&Value::Object(m.clone()), path).is_some()
        };
        if has(&self.workspace) {
            Some(Layer::Workspace)
        } else if has(&self.project) {
            Some(Layer::Project)
        } else {
            None
        }
    }
}

static OVERRIDES: Mutex<Option<Overrides>> = Mutex::new(None);

fn overrides() -> Overrides {
    OVERRIDES.lock().unwrap_or_else(|e| e.into_inner()).clone().unwrap_or_default()
}

fn get_path<'a>(v: &'a Value, path: &[String]) -> Option<&'a Value> {
    path.iter().try_fold(v, |v, k| v.get(k))
}

/// Set (or with `None`, remove) the value at `path`, creating objects on the way.
fn set_path(v: &mut Value, path: &[String], new: Option<Value>) {
    let Some((last, parents)) = path.split_last() else { return };
    let mut cur = v;
    for k in parents {
        if !cur.get(k).is_some_and(Value::is_object) {
            if new.is_none() {
                return;
            }
            cur[k.as_str()] = Value::Object(Map::new());
        }
        cur = &mut cur[k.as_str()];
    }
    if let Value::Object(m) = cur {
        match new {
            Some(n) => {
                m.insert(last.clone(), n);
            }
            None => {
                m.remove(last);
            }
        }
    }
}

/// The leaf paths that differ between `old` and `new`, with the new value
/// (`None`: removed).
fn diff(old: &Value, new: &Value, path: &mut Vec<String>, out: &mut Vec<(Vec<String>, Option<Value>)>) {
    match (old, new) {
        (Value::Object(a), Value::Object(b)) => {
            let keys: std::collections::BTreeSet<&String> = a.keys().chain(b.keys()).collect();
            for k in keys {
                path.push(k.clone());
                match (a.get(k), b.get(k)) {
                    (Some(x), Some(y)) => diff(x, y, path, out),
                    (_, y) => out.push((path.clone(), y.cloned())),
                }
                path.pop();
            }
        }
        _ if old != new => out.push((path.clone(), Some(new.clone()))),
        _ => {}
    }
}

/// Apply changed paths to the user file, keeping everything else in it.
fn write_changes(home: &Path, changes: &[(Vec<String>, Option<Value>)]) -> Result<()> {
    if changes.is_empty() {
        return Ok(());
    }
    let path = home.join("settings.json");
    let mut file = match std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
    {
        Some(v @ Value::Object(_)) => v,
        _ => Value::Object(Map::new()),
    };
    for (p, v) in changes {
        set_path(&mut file, p, v.clone());
    }
    let bytes = serde_json::to_vec_pretty(&file)?;
    crate::session::write_private(&path, &bytes)
        .with_context(|| format!("writing {}", path.display()))
}

fn merge(into: &mut Map<String, Value>, over: &Map<String, Value>) {
    for (k, v) in over {
        match (into.get_mut(k), v) {
            (Some(Value::Object(a)), Value::Object(b)) => merge(a, b),
            _ => {
                into.insert(k.clone(), v.clone());
            }
        }
    }
}

fn read_object(path: &Path) -> Result<Map<String, Value>, String> {
    match std::fs::read_to_string(path) {
        Ok(t) => match serde_json::from_str::<Value>(&t) {
            Ok(Value::Object(m)) => Ok(m),
            Ok(_) => Err(format!("{}: expected a JSON object", path.display())),
            Err(e) => Err(format!("{}: {e}", path.display())),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Map::new()),
        Err(e) => Err(format!("reading {}: {e}", path.display())),
    }
}

/// The user's overrides for `cwd` from `workspaces` (keys may start with `~/`).
fn workspace_overrides(user: &Map<String, Value>, cwd: &Path) -> Map<String, Value> {
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let cwd = canon(cwd);
    user.get("workspaces")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .find(|(k, _)| {
            let p = match k.strip_prefix("~/") {
                Some(rest) => dirs::home_dir().unwrap_or_default().join(rest),
                None => k.into(),
            };
            canon(&p) == cwd
        })
        .and_then(|(_, v)| v.as_object().cloned())
        .unwrap_or_default()
}

/// Load the user file with the project and workspace layers for `cwd` on
/// top, and remember the layers for [`source`] and [`update`].
/// Problems come back as warnings; a broken layer is skipped.
pub fn load_layered(home: &Path, cwd: &Path) -> (Settings, Vec<String>) {
    let mut warnings = Vec::new();
    let mut read = |path: &Path| {
        read_object(path).unwrap_or_else(|e| {
            warnings.push(format!("{e} (ignored)"));
            Map::new()
        })
    };
    let user = read(&home.join("settings.json"));
    let project_path = cwd.join(".nyq").join("settings.json");
    let mut project = read(&project_path);
    let mut workspace = workspace_overrides(&user, cwd);
    let ignored: Vec<String> = project
        .keys()
        .filter(|k| !PROJECT_KEYS.contains(&k.as_str()))
        .cloned()
        .collect();
    if !ignored.is_empty() {
        project.retain(|k, _| PROJECT_KEYS.contains(&k.as_str()));
        warnings.push(format!(
            "{}: ignored {} (only your own settings.json may set these)",
            project_path.display(),
            ignored.join(", ")
        ));
    }
    let mut merged = user.clone();
    merge(&mut merged, &project);
    merge(&mut merged, &workspace);
    let settings = serde_json::from_value(Value::Object(merged))
        .or_else(|e| {
            warnings.push(format!("project or workspace settings: {e} (using your settings.json)"));
            // The layers are unusable: none of them may be reported as the
            // source of a value or shadow a later change.
            project.clear();
            workspace.clear();
            serde_json::from_value(Value::Object(user))
        })
        .unwrap_or_else(|e| {
            warnings.push(format!("settings.json: {e} (using default settings)"));
            Settings::default()
        });
    *OVERRIDES.lock().unwrap_or_else(|e| e.into_inner()) = Some(Overrides { project, workspace });
    (settings, warnings)
}

/// The layer that sets a setting's value, when it isn't the user file.
pub fn source(id: Id) -> Option<Layer> {
    let path: Vec<String> = match id {
        Id::Model => return None,
        Id::Effort => vec!["effort".into()],
        Id::ClassifierModel => vec!["classifier_model".into()],
        Id::Fast => vec!["fast_mode".into()],
        Id::Permission => vec!["permission_mode".into()],
        Id::Sandbox => vec!["sandbox".into()],
        Id::AutoCompact => vec!["auto_compact".into()],
        Id::Sound => vec!["sound".into()],
        Id::Notifications => vec!["notifications".into()],
        Id::NotifyAfter => vec!["notify_after_secs".into()],
        Id::Segment(seg) => vec!["statusline".into(), seg.key().into()],
        Id::PromptHistory => vec!["prompt_history".into()],
        Id::AutoUpgrade => vec!["auto_upgrade".into()],
        Id::SlashCategories => vec!["slash_menu_categories".into()],
        Id::Theme => vec!["theme".into()],
        Id::CollapseToolCalls => vec!["collapse_tool_calls".into()],
        Id::StartupScrollback => vec!["startup_scrollback".into()],
    };
    overrides().layer(&path)
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

// ------------------------------------------------------------------ catalog

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Category {
    All,
    StatusLine,
    Agent,
    Notifications,
    Display,
    Advanced,
}

impl Category {
    pub const ALL: [Category; 6] = [
        Category::All,
        Category::StatusLine,
        Category::Agent,
        Category::Notifications,
        Category::Display,
        Category::Advanced,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Category::All => "All",
            Category::StatusLine => "Status line",
            Category::Agent => "Agent",
            Category::Notifications => "Notifications",
            Category::Display => "Display",
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
    ClassifierModel,
    Sandbox,
    AutoCompact,
    Sound,
    Notifications,
    NotifyAfter,
    Segment(Segment),
    PromptHistory,
    AutoUpgrade,
    SlashCategories,
    Theme,
    CollapseToolCalls,
    StartupScrollback,
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
    /// Models the current provider listed (classifier model choices).
    pub models: Vec<String>,
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
            id: Id::ClassifierModel,
            category: Category::Agent,
            label: "Classifier model",
            description:
                "Model that reviews calls in auto mode; auto picks a small one (Codex: a mini) or the session model",
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
            description: "Chime or bell: off, on (long turns, prompts) or max",
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
        id: Id::Theme,
        category: Category::Display,
        label: "Theme",
        description: "Colors for dark or light terminals; auto follows the background",
    });
    v.push(Spec {
        id: Id::CollapseToolCalls,
        category: Category::Display,
        label: "Collapse tool calls",
        description: "Show finished tool groups as one summary line (ctrl+o shows them all)",
    });
    v.push(Spec {
        id: Id::StartupScrollback,
        category: Category::Display,
        label: "Start at top of screen",
        description: "Move earlier shell output into the scrollback at start",
    });
    v.push(Spec {
        id: Id::PromptHistory,
        category: Category::Advanced,
        label: "Prompt history",
        description: "Save accepted prompts for history across sessions",
    });
    v.push(Spec {
        id: Id::AutoUpgrade,
        category: Category::Advanced,
        label: "Auto-upgrade",
        description: "Install new nyq releases in the background; ctrl+g reloads",
    });
    v.push(Spec {
        id: Id::SlashCategories,
        category: Category::Display,
        label: "Slash menu categories",
        description: "Show categories and skill sources in slash-command results",
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
        Id::ClassifierModel => s.classifier_model.clone().filter(|m| !m.trim().is_empty()).unwrap_or_else(|| "auto".into()),
        Id::Sandbox => s.sandbox.clone(),
        Id::AutoCompact if s.auto_compact == 0 => "off".into(),
        Id::AutoCompact => format!("{}%", s.auto_compact),
        Id::Sound => s.sound.clone(),
        Id::Notifications => on_off(s.notifications),
        Id::NotifyAfter => format!("{}s", s.notify_after_secs),
        Id::Segment(seg) => on_off(s.statusline.get(seg)),
        Id::PromptHistory => on_off(s.prompt_history),
        Id::AutoUpgrade => on_off(s.auto_upgrade),
        Id::SlashCategories => on_off(s.slash_menu_categories),
        Id::Theme => s.theme.clone(),
        Id::CollapseToolCalls => on_off(s.collapse_tool_calls),
        Id::StartupScrollback => on_off(s.startup_scrollback),
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
        Id::ClassifierModel => std::iter::once("auto".to_string()).chain(live.models.iter().cloned()).collect(),
        Id::Sandbox => list(&["workspace-write", "read-only", "off"]),
        Id::AutoCompact => list(&["off", "50%", "60%", "70%", "80%", "90%"]),
        Id::Sound => list(&["off", "on", "max"]),
        Id::NotifyAfter => list(&["0s", "5s", "10s", "30s", "60s", "120s"]),
        Id::Theme => list(&["auto", "dark", "light"]),
        Id::Fast
        | Id::Notifications
        | Id::Segment(_)
        | Id::PromptHistory
        | Id::AutoUpgrade
        | Id::SlashCategories
        | Id::CollapseToolCalls
        | Id::StartupScrollback => {
            list(&["off", "on"])
        }
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
        Id::ClassifierModel => s.classifier_model = Some(value.to_string()).filter(|v| v != "auto"),
        Id::Sandbox => s.sandbox = value.into(),
        Id::AutoCompact => s.auto_compact = value.trim_end_matches('%').parse().unwrap_or(0),
        Id::Sound => s.sound = value.into(),
        Id::Notifications => s.notifications = on,
        Id::NotifyAfter => s.notify_after_secs = value.trim_end_matches('s').parse().unwrap_or(10),
        Id::Segment(seg) => s.statusline.set(seg, on),
        Id::PromptHistory => s.prompt_history = on,
        Id::AutoUpgrade => s.auto_upgrade = on,
        Id::SlashCategories => s.slash_menu_categories = on,
        Id::Theme => s.theme = value.into(),
        Id::CollapseToolCalls => s.collapse_tool_calls = on,
        Id::StartupScrollback => s.startup_scrollback = on,
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

    /// Save `s` the way `update` does: only what differs from the file.
    fn save(home: &Path, s: &Settings) -> Result<()> {
        let mut changes = Vec::new();
        let old = serde_json::to_value(load(home).0)?;
        diff(&old, &serde_json::to_value(s)?, &mut Vec::new(), &mut changes);
        write_changes(home, &changes)
    }

    #[test]
    fn project_and_workspace_layers() {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let ws = cwd.path().canonicalize().unwrap();
        std::fs::write(
            home.path().join("settings.json"),
            serde_json::json!({
                "sound": "on",
                "auto_compact": 70,
                "statusline": {"tokens": false},
                "workspaces": {ws.to_string_lossy(): {"auto_compact": 50}}
            })
            .to_string(),
        )
        .unwrap();
        std::fs::create_dir_all(cwd.path().join(".nyq")).unwrap();
        std::fs::write(
            cwd.path().join(".nyq/settings.json"),
            r#"{"sound": "max", "auto_compact": 60, "statusline": {"branch": false}, "permission_mode": "yolo"}"#,
        )
        .unwrap();
        let (s, warnings) = load_layered(home.path(), cwd.path());
        assert_eq!(s.sound, "max");
        assert_eq!(s.auto_compact, 50);
        assert!(!s.statusline.tokens && !s.statusline.branch && s.statusline.model);
        // A project can't loosen permissions.
        assert_eq!(s.permission_mode, "ask");
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("permission_mode"));
        assert_eq!(source(Id::Sound), Some(Layer::Project));
        assert_eq!(source(Id::AutoCompact), Some(Layer::Workspace));
        assert_eq!(source(Id::Segment(Segment::Branch)), Some(Layer::Project));
        assert_eq!(source(Id::Segment(Segment::Tokens)), None);
        assert_eq!(source(Id::Notifications), None);
        // Saving writes only the changed key, and overridden keys are reported.
        let o = overrides();
        let mut changes = Vec::new();
        let mut next = s.clone();
        next.sound = "off".into();
        next.notifications = true;
        diff(&serde_json::to_value(&s).unwrap(), &serde_json::to_value(&next).unwrap(), &mut Vec::new(), &mut changes);
        assert_eq!(changes.len(), 2);
        write_changes(home.path(), &changes).unwrap();
        let raw: Value =
            serde_json::from_slice(&std::fs::read(home.path().join("settings.json")).unwrap()).unwrap();
        assert_eq!(raw["sound"], "off");
        assert_eq!(raw["notifications"], true);
        assert_eq!(raw["auto_compact"], 70, "layer values stay out of the user file");
        assert!(raw["statusline"].get("branch").is_none());
        assert_eq!(o.layer(&["sound".to_string()]), Some(Layer::Project));
        assert_eq!(o.layer(&["notifications".to_string()]), None);
        // A broken project file is skipped with a warning.
        std::fs::write(cwd.path().join(".nyq/settings.json"), "{").unwrap();
        let (s, warnings) = load_layered(home.path(), cwd.path());
        assert_eq!((s.sound.as_str(), s.auto_compact), ("off", 50));
        assert_eq!(warnings.len(), 1);

        // A workspace layer with an unusable value is dropped entirely: the
        // user file applies, no layer is reported as a value's source, and a
        // later change is not reverted as shadowed.
        std::fs::write(cwd.path().join(".nyq/settings.json"), r#"{"sound": "max"}"#).unwrap();
        std::fs::write(
            home.path().join("settings.json"),
            serde_json::json!({
                "sound": "on",
                "auto_compact": 70,
                "statusline": {"tokens": false},
                "workspaces": {ws.to_string_lossy(): {"sound": 5}}
            })
            .to_string(),
        )
        .unwrap();
        let (s, warnings) = load_layered(home.path(), cwd.path());
        assert_eq!(s.sound, "on");
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("project or workspace settings"), "{warnings:?}");
        assert_eq!(source(Id::Sound), None, "a dropped layer must not be a source");
        assert_eq!(source(Id::AutoCompact), None);
        install(s);
        let (effective, notice) = update(home.path(), |s| s.sound = "off".into()).unwrap();
        assert_eq!(effective.sound, "off");
        assert!(notice.is_none(), "{notice:?}");
        let raw: Value =
            serde_json::from_slice(&std::fs::read(home.path().join("settings.json")).unwrap()).unwrap();
        assert_eq!(raw["sound"], "off");
        *CURRENT.lock().unwrap() = None;
        *OVERRIDES.lock().unwrap() = None;
    }

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
        assert!(!s.statusline.tokens && s.statusline.model && s.prompt_history && s.auto_upgrade);
        std::fs::write(dir.path().join("settings.json"), r#"{"sound": 5}"#).unwrap();
        let (s, warn) = load(dir.path());
        assert!(warn.is_some());
        assert_eq!(s, Settings::default());
        assert_eq!(load(&dir.path().join("missing")).0, Settings::default());
    }

    #[test]
    fn classifier_model_setting() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Settings::default();
        let live = Live { models: vec!["gpt-mini".into(), "gpt-big".into()], ..Live::default() };
        assert_eq!(value(Id::ClassifierModel, &s, &live), "auto");
        assert_eq!(cycle(Id::ClassifierModel, &s, &live, 1).as_deref(), Some("gpt-mini"));
        assert!(apply(Id::ClassifierModel, "gpt-mini", &mut s));
        assert_eq!(value(Id::ClassifierModel, &s, &live), "gpt-mini");
        assert_eq!(cycle(Id::ClassifierModel, &s, &live, -1).as_deref(), Some("auto"));
        save(dir.path(), &s).unwrap();
        assert_eq!(load(dir.path()).0.classifier_model.as_deref(), Some("gpt-mini"));
        // Back to auto removes the key.
        assert!(apply(Id::ClassifierModel, "auto", &mut s) && s.classifier_model.is_none());
        save(dir.path(), &s).unwrap();
        assert!(!std::fs::read_to_string(dir.path().join("settings.json")).unwrap().contains("classifier_model"));
        assert_eq!(filtered(Category::Agent, "classifier model", &s, &live)[0].id, Id::ClassifierModel);
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
        // Auto-upgrade is on by default and saved as a plain flag.
        assert!(Settings::default().auto_upgrade);
        assert_eq!(value(Id::AutoUpgrade, &s, &live), "on");
        assert_eq!(cycle(Id::AutoUpgrade, &s, &live, 1).as_deref(), Some("off"));
        assert!(apply(Id::AutoUpgrade, "off", &mut t) && !t.auto_upgrade);
        assert_eq!(filtered(Category::Advanced, "upgrade", &s, &live)[0].id, Id::AutoUpgrade);
        let off: Settings = serde_json::from_str(r#"{"auto_upgrade": false}"#).unwrap();
        assert!(!off.auto_upgrade);
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
        assert_eq!(Segment::parse("title"), Some(Segment::Session));
        assert_eq!(value(Id::Theme, &s, &live), "auto");
        assert_eq!(cycle(Id::Theme, &s, &live, 1).as_deref(), Some("dark"));
        assert!(apply(Id::Theme, "light", &mut t) && t.theme == "light");
        assert!(apply(Id::CollapseToolCalls, "on", &mut t) && t.collapse_tool_calls);
        assert!(apply(Id::StartupScrollback, "on", &mut t) && t.startup_scrollback);
        assert_eq!(filtered(Category::Display, "", &s, &live).len(), 4);
        assert_eq!(
            sandbox_mode(&Settings {
                sandbox: "read-only".into(),
                ..s
            }),
            crate::sandbox::Mode::ReadOnly
        );
    }
}
