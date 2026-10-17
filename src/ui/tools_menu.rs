//! `/tools` menu (with `--debug`): every tool the model may get, grouped
//! into built-in, MCP (by server) and extension tools, with whether it is
//! turned off and how its calls are approved. Typing filters, Tab and
//! Shift+Tab pick a group, Enter or → shows the description and parameters,
//! Ctrl+T turns a tool off or on, Ctrl+A always allows it (after a
//! confirmation) or takes that back, Esc closes.

use std::collections::HashSet;
use std::path::Path;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::Value;

use super::menu::{self, Badge, List, Row, View};
use super::theme::{GRAY, GREEN, RED, YELLOW};
use crate::permissions::{Mode, Permissions, ToolApproval};
use crate::types::ToolSpec;

pub const BUILTIN: &str = "Built-in";
pub const EXTENSIONS: &str = "Extensions";

pub struct Entry {
    pub spec: ToolSpec,
    /// [`BUILTIN`], `MCP <server>` or [`EXTENSIONS`].
    pub group: String,
    /// Named in the `disabled_tools` setting.
    pub disabled: bool,
    pub approval: ToolApproval,
    /// Your allow list holds exactly this name (Ctrl+A can take it back).
    pub trusted: bool,
}

/// The group a tool is listed under.
pub fn group_of(name: &str) -> String {
    if let Some(rest) = name.strip_prefix("mcp__") {
        let server = rest.split_once("__").map_or(rest, |(s, _)| s);
        return format!("MCP {server}");
    }
    if name.starts_with(crate::extensions::TOOL_PREFIX) {
        return EXTENSIONS.into();
    }
    BUILTIN.into()
}

/// Entries for every tool in `catalog` (as from `Registry::catalog`), with
/// their approval worked out by the permission rules.
pub fn entries(
    catalog: Vec<(ToolSpec, bool, bool)>,
    perms: &Permissions,
    session: &HashSet<String>,
    cwd: &Path,
    mode: Mode,
) -> Vec<Entry> {
    catalog
        .into_iter()
        .map(|(spec, disabled, needs_approval)| Entry {
            group: group_of(&spec.name),
            approval: perms.tool_approval(session, &spec.name, cwd, needs_approval, mode),
            trusted: perms.allows_exactly(&spec.name),
            disabled,
            spec,
        })
        .collect()
}

/// Group order: built-in, then MCP servers by name, then extensions.
fn group_rank(g: &str) -> (u8, &str) {
    match g {
        BUILTIN => (0, ""),
        EXTENSIONS => (2, ""),
        _ => (1, g),
    }
}

/// The badges of an entry, as text.
pub fn badges(e: &Entry) -> Vec<&'static str> {
    let mut out = Vec::new();
    if e.disabled {
        out.push("off");
    }
    out.push(e.approval.badge());
    out
}

fn badge_color(e: &Entry, text: &str) -> nu_ansi_term::Color {
    match (text, &e.approval) {
        ("off", _) | (_, ToolApproval::Denied(_)) => RED,
        (_, ToolApproval::AskRule(_) | ToolApproval::Mode(Mode::Ask | Mode::Auto)) => YELLOW,
        (_, ToolApproval::Allowed(_) | ToolApproval::Session) => GREEN,
        _ => GRAY,
    }
}

/// A JSON Schema type, readably: `string`, `array of string`, `one of a|b`.
fn type_name(schema: &Value) -> String {
    if let Some(values) = schema.get("enum").and_then(Value::as_array) {
        let names: Vec<String> = values.iter().map(|v| v.as_str().map_or_else(|| v.to_string(), String::from)).collect();
        return format!("one of {}", names.join("|"));
    }
    let base = match schema.get("type") {
        Some(Value::String(t)) => t.clone(),
        Some(Value::Array(ts)) => ts.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" or "),
        _ => match schema.get("anyOf").or_else(|| schema.get("oneOf")).and_then(Value::as_array) {
            Some(alts) => alts.iter().map(type_name).collect::<Vec<_>>().join(" or "),
            None => "any".into(),
        },
    };
    match (base.as_str(), schema.get("items")) {
        ("array", Some(items)) => format!("array of {}", type_name(items)),
        _ => base,
    }
}

/// The parameters of a tool's JSON Schema: name, type, whether required,
/// and the description indented under it.
pub fn param_lines(schema: &Value) -> Vec<String> {
    let required: Vec<&str> =
        schema.get("required").and_then(Value::as_array).map(|r| r.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
    let Some(props) = schema.get("properties").and_then(Value::as_object).filter(|p| !p.is_empty()) else {
        return vec!["  (none)".into()];
    };
    // Required parameters first, then the rest, each by name.
    let mut names: Vec<&String> = props.keys().collect();
    names.sort_by_key(|n| !required.contains(&n.as_str()));
    let mut out = Vec::new();
    for name in names {
        let p = &props[name];
        let req = if required.contains(&name.as_str()) { ", required" } else { "" };
        out.push(format!("  {name} ({}{req})", type_name(p)));
        if let Some(d) = p.get("description").and_then(Value::as_str).filter(|d| !d.trim().is_empty()) {
            out.extend(d.lines().map(|l| format!("      {}", l.trim_end())));
        }
    }
    out
}

/// The text of a tool's detail view (and `/tools info NAME`).
pub fn detail(e: &Entry) -> String {
    let state = if e.disabled { "off (disabled_tools; Ctrl+T or /tools enable turns it on)" } else { "on" };
    let mut lines = vec![
        format!("{} · {}", e.spec.name, e.group),
        format!("State: {state}"),
        format!("Approval: {} · {}", e.approval.badge(), e.approval.explain()),
        String::new(),
        e.spec.description.trim_end().to_string(),
        String::new(),
        "Parameters".into(),
    ];
    lines.extend(param_lines(&e.spec.parameters));
    lines.join("\n")
}

/// The tools as text, grouped like the menu, for `/tools list` and where no
/// menu can open (piped, `-p`, editors).
pub fn plain_lines(entries: &[Entry]) -> Vec<String> {
    let mut groups: Vec<&str> = entries.iter().map(|e| e.group.as_str()).collect();
    groups.sort_by_key(|g| group_rank(g));
    groups.dedup();
    let width = entries.iter().map(|e| e.spec.name.chars().count()).max().unwrap_or(0).min(40);
    let mut out = Vec::new();
    for g in groups {
        let rows: Vec<&Entry> = entries.iter().filter(|e| e.group == g).collect();
        out.push(format!("{g} ({})", rows.len()));
        for e in rows {
            let desc = super::render::one_line(e.spec.description.lines().next().unwrap_or(""), 90);
            out.push(format!("  {:width$}  [{}]  {desc}", e.spec.name, badges(e).join(", ")).trim_end().to_string());
        }
    }
    out.push("Use: /tools info|disable|enable|trust|untrust NAME".into());
    out
}

pub struct ToolsMenu {
    entries: Vec<Entry>,
    pub groups: Vec<String>,
    pub list: List,
    /// The tool (by name) waiting for an always-allow confirmation.
    confirm: Option<String>,
    /// A result or problem shown above the hints, and whether it is an error.
    pub notice: Option<(String, bool)>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    None,
    Close,
    /// Show this tool (by name) full screen.
    View(String),
    /// Run this command and keep the menu open (it refreshes afterwards).
    Run(String),
}

impl ToolsMenu {
    pub fn new(entries: Vec<Entry>) -> Self {
        let mut m = Self { entries: Vec::new(), groups: Vec::new(), list: List::default(), confirm: None, notice: None };
        m.set_entries(entries);
        m
    }

    /// Replace the tools (after a change), keeping the filter, group and
    /// the selected tool where it still exists.
    pub fn set_entries(&mut self, mut entries: Vec<Entry>) {
        let keep = self.current().map(|e| e.spec.name.clone());
        let group = self.list.group.and_then(|g| self.groups.get(g).cloned());
        entries.sort_by(|a, b| (group_rank(&a.group), &a.spec.name).cmp(&(group_rank(&b.group), &b.spec.name)));
        self.groups = entries.iter().map(|e| e.group.clone()).collect();
        self.groups.dedup();
        self.entries = entries;
        self.list.group = group.and_then(|g| self.groups.iter().position(|x| *x == g));
        if let Some(name) = keep {
            self.list.selected = self.rows().iter().position(|e| e.spec.name == name).unwrap_or(0);
        }
    }

    fn matching(&self) -> impl Iterator<Item = &Entry> {
        let words = self.list.words();
        self.entries.iter().filter(move |e| {
            let hay = format!("{} {} {} {}", e.spec.name, e.spec.description, e.group, badges(e).join(" "));
            menu::matches(&hay, &words)
        })
    }

    /// Entries in the chosen group matching every word of the filter (name,
    /// description, group or badge).
    pub fn rows(&self) -> Vec<&Entry> {
        let group = self.list.group.and_then(|g| self.groups.get(g));
        self.matching().filter(|e| group.is_none_or(|g| *g == e.group)).collect()
    }

    fn current(&self) -> Option<&Entry> {
        let rows = self.rows();
        rows.get(self.list.selected.min(rows.len().saturating_sub(1))).copied()
    }

    /// The entry for `name`.
    pub fn entry(&self, name: &str) -> Option<&Entry> {
        self.entries.iter().find(|e| e.spec.name == name)
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Action {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL) && !key.modifiers.contains(KeyModifiers::ALT);
        if ctrl && key.code == KeyCode::Char('c') {
            return Action::Close;
        }
        self.notice = None;
        if let Some(name) = self.confirm.take() {
            // Only a deliberate `y` trusts a tool; anything else cancels.
            return match key.code {
                KeyCode::Char('y' | 'Y') => Action::Run(format!("/tools trust {name}")),
                _ => Action::None,
            };
        }
        let count = self.rows().len();
        let current = self.current();
        match key.code {
            KeyCode::Esc => return Action::Close,
            KeyCode::Enter | KeyCode::Right => {
                if let Some(e) = current {
                    return Action::View(e.spec.name.clone());
                }
            }
            KeyCode::Char('t') if ctrl => {
                if let Some(e) = current {
                    let verb = if e.disabled { "enable" } else { "disable" };
                    return Action::Run(format!("/tools {verb} {}", e.spec.name));
                }
            }
            KeyCode::Char('a') if ctrl => {
                let Some(e) = current else { return Action::None };
                let name = &e.spec.name;
                let refuse = |msg: String| (msg, true);
                self.notice = match &e.approval {
                    _ if e.trusted => return Action::Run(format!("/tools untrust {name}")),
                    ToolApproval::Denied(rule) => Some(refuse(format!("{name} is blocked by `{rule}`; an allow rule can't override a deny."))),
                    ToolApproval::AskRule(rule) => Some(refuse(format!("{name} always asks because of `{rule}`; an allow rule can't override it."))),
                    ToolApproval::Free => Some((format!("{name} already runs without asking."), false)),
                    ToolApproval::Allowed(rule) => {
                        Some(refuse(format!("{name} is allowed by `{rule}`; change that with /allowlist.")))
                    }
                    ToolApproval::Session | ToolApproval::Mode(_) => {
                        self.confirm = Some(name.clone());
                        None
                    }
                };
            }
            _ => {
                self.list.on_key(key, count, self.groups.len());
            }
        }
        Action::None
    }

    /// The menu drawn in `width` columns and about `height` rows.
    pub fn render(&self, width: usize, height: usize) -> Vec<String> {
        let rows = self.rows();
        let all: Vec<&Entry> = self.matching().collect();
        let tabs = self.groups.iter().map(|g| (g.clone(), all.iter().filter(|e| e.group == *g).count())).collect();
        let current = self.current();
        let note = match (&self.confirm, &self.notice) {
            (Some(name), _) => Some((
                format!("Always allow {name} without asking, in every workspace (saved to permissions.json)? y allows it, any other key cancels."),
                YELLOW,
            )),
            (None, Some((n, true))) => Some((n.clone(), RED)),
            (None, Some((n, false))) => Some((n.clone(), GREEN)),
            _ => None,
        };
        let hints = match (&self.confirm, current) {
            (Some(_), _) => vec![("y", "Always allow"), ("any other key", "Cancel")],
            (None, Some(e)) => vec![
                ("Enter", "Details"),
                ("Tab", "Group"),
                ("^T", if e.disabled { "Turn on" } else { "Turn off" }),
                ("^A", if e.trusted { "Stop allowing" } else { "Always allow" }),
                ("Esc", "Close"),
            ],
            (None, None) => vec![("Tab", "Group"), ("Esc", "Close")],
        };
        let preview = current
            .map(|e| {
                let desc = e.spec.description.split_whitespace().collect::<Vec<_>>().join(" ");
                let params = e.spec.parameters.get("properties").and_then(Value::as_object).map_or(0, |p| p.len());
                vec![desc, format!("{} · {} parameter{} · {}", e.group, params, if params == 1 { "" } else { "s" }, e.approval.explain())]
            })
            .unwrap_or_default();
        let view = View {
            title: "Tools",
            tabs,
            group: self.list.group,
            total: all.len(),
            filter: &self.list.filter,
            rows: rows
                .iter()
                .map(|e| Row {
                    label: e.spec.name.clone(),
                    detail: super::render::one_line(e.spec.description.lines().next().unwrap_or(""), 300),
                    badges: badges(e).into_iter().map(|b| Badge::new(b, badge_color(e, b))).collect(),
                    dim: e.disabled,
                })
                .collect(),
            selected: self.list.selected,
            empty: vec!["No matching tools.".into()],
            preview,
            note,
            input: None,
            hints,
        };
        menu::render(&view, width, height)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::render::strip_ansi;
    use serde_json::json;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn spec(name: &str, description: &str) -> ToolSpec {
        ToolSpec {
            name: name.into(),
            description: description.into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "limit": {"type": "integer", "description": "How many"},
                    "path": {"type": "string", "description": "The file\nrelative to the workspace"},
                    "mode": {"enum": ["a", "b"]},
                    "tags": {"type": "array", "items": {"type": "string"}}
                },
                "required": ["path"]
            }),
        }
    }

    fn menu() -> ToolsMenu {
        let perms = Permissions {
            allow: vec!["mcp__gh__list_issues".into()],
            deny: vec!["mcp__gh__delete_repo".into()],
            ..Default::default()
        };
        let catalog = vec![
            (spec("read_file", "Read a file"), false, false),
            (spec("bash", "Run a shell command"), false, true),
            (spec("web_search", "Search the web"), true, true),
            (spec("mcp__gh__list_issues", "List issues"), false, true),
            (spec("mcp__gh__delete_repo", "Delete a repository"), false, true),
            (spec("mcp__db__query", "Run SQL"), false, true),
            (spec("ext__notes__save", "Save a note"), false, true),
        ];
        ToolsMenu::new(entries(catalog, &perms, &HashSet::new(), Path::new("/w"), Mode::Ask))
    }

    fn names(m: &ToolsMenu) -> Vec<&str> {
        m.rows().iter().map(|e| e.spec.name.as_str()).collect()
    }

    #[test]
    fn groups_badges_and_filters() {
        let mut m = menu();
        assert_eq!(m.groups, ["Built-in", "MCP db", "MCP gh", "Extensions"]);
        assert_eq!(names(&m)[..3], ["bash", "read_file", "web_search"]);
        let badge = |m: &ToolsMenu, n: &str| badges(m.entry(n).unwrap()).join(", ");
        assert_eq!(badge(&m, "read_file"), "auto");
        assert_eq!(badge(&m, "bash"), "asks");
        assert_eq!(badge(&m, "web_search"), "off, asks");
        assert_eq!(badge(&m, "mcp__gh__list_issues"), "always allowed");
        assert_eq!(badge(&m, "mcp__gh__delete_repo"), "denied");
        m.on_key(key(KeyCode::Tab));
        m.on_key(key(KeyCode::Tab));
        m.on_key(key(KeyCode::Tab));
        assert_eq!(names(&m), ["mcp__gh__delete_repo", "mcp__gh__list_issues"]);
        m.on_key(key(KeyCode::BackTab));
        m.on_key(key(KeyCode::BackTab));
        m.on_key(key(KeyCode::BackTab));
        for c in "off".chars() {
            m.on_key(key(KeyCode::Char(c)));
        }
        assert_eq!(names(&m), ["web_search"], "badges are searchable");
        let out = m.render(80, 40);
        assert!(out.iter().all(|r| super::super::render::visible_width(r) <= 80));
        let text = strip_ansi(&out.join("\n"));
        assert!(text.contains("[All 1]") && text.contains("Built-in 1") && text.contains("MCP gh 0"), "{text}");
        assert!(text.contains("off · asks") && text.contains("Asks before each call"), "{text}");
        assert!(text.contains("^T Turn on"), "{text}");
    }

    #[test]
    fn toggles_and_trusts_only_after_a_confirmation() {
        let mut m = menu();
        assert_eq!(m.on_key(key(KeyCode::Enter)), Action::View("bash".into()));
        assert_eq!(m.on_key(ctrl('t')), Action::Run("/tools disable bash".into()));
        assert_eq!(m.on_key(ctrl('a')), Action::None);
        assert!(strip_ansi(&m.render(80, 40).join("\n")).contains("Always allow bash"));
        // Enter does not confirm: only y does.
        assert_eq!(m.on_key(key(KeyCode::Enter)), Action::None);
        m.on_key(ctrl('a'));
        assert_eq!(m.on_key(key(KeyCode::Char('y'))), Action::Run("/tools trust bash".into()));
        m.on_key(key(KeyCode::Down));
        assert_eq!(m.rows()[m.list.selected].spec.name, "read_file");
        m.on_key(ctrl('a'));
        assert!(m.notice.as_ref().unwrap().0.contains("already runs without asking"));
        m.list.filter = "delete".into();
        m.on_key(ctrl('a'));
        assert!(m.notice.as_ref().unwrap().0.contains("can't override a deny"));
        m.list.filter = "list_issues".into();
        assert_eq!(m.on_key(ctrl('a')), Action::Run("/tools untrust mcp__gh__list_issues".into()));
        m.list.filter = "web_search".into();
        assert_eq!(m.on_key(ctrl('t')), Action::Run("/tools enable web_search".into()));
        assert_eq!(m.on_key(key(KeyCode::Esc)), Action::Close);
    }

    #[test]
    fn details_list_parameters_readably() {
        let m = menu();
        let text = detail(m.entry("bash").unwrap());
        assert!(text.starts_with("bash · Built-in\nState: on\nApproval: asks"), "{text}");
        let params = text.split("Parameters\n").nth(1).unwrap();
        assert_eq!(
            params.lines().collect::<Vec<_>>(),
            [
                "  path (string, required)",
                "      The file",
                "      relative to the workspace",
                "  limit (integer)",
                "      How many",
                "  mode (one of a|b)",
                "  tags (array of string)",
            ]
        );
        assert_eq!(param_lines(&json!({"type": "object"})), ["  (none)"]);
    }

    #[test]
    fn plain_listing_has_groups_and_badges() {
        let m = menu();
        let lines = plain_lines(&m.entries);
        assert_eq!(lines[0], "Built-in (3)");
        assert!(lines.iter().any(|l| l.contains("web_search") && l.contains("[off, asks]")), "{lines:?}");
        assert!(lines.iter().any(|l| l == "MCP gh (2)"));
        assert!(lines.iter().any(|l| l == "Extensions (1)"));
        assert!(lines.last().unwrap().contains("/tools info|disable|enable"));
    }
}
