//! Persistent permission rules: `<home>/nyq/permissions.json` holds
//! `{"allow": [...], "ask": [...], "deny": [...]}` tool patterns, and an
//! optional `<cwd>/.nyq/permissions.json` may add `ask`/`deny` rules (never
//! `allow`: a repository must not grant itself permissions).
//!
//! A pattern is a tool name with `*` wildcards (`web_fetch`, `mcp__deepwiki__*`,
//! `mcp__*`), `subagent:explore` / `subagent:general`, `web_fetch:<url>` (with
//! wildcards), `bash:<prefix>` for shell commands, or a path-scoped rule
//! (`edit_file:src/**`, `read_file:secrets/*`, `read:~/notes/**`; see
//! [`path_rule_matches`]). Precedence:
//! deny > ask > allow > session grants > the permission mode. Deny wins in
//! every mode, including yolo. `auto` classifies whatever is still undecided.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde_json::Value;

use crate::agent::{bash_matches, is_file_tool, program_base, subagent_mode};

/// Read tools ask only for paths outside the workspace; a persistent allow
/// never covers those.
const READ_TOOLS: &[&str] = &["read_file", "list_dir", "glob", "grep", "view_image"];

pub(crate) fn is_read_tool(tool: &str) -> bool {
    READ_TOOLS.contains(&tool)
}

/// How undecided risky calls are handled. Deny rules still block in every mode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    /// Ask before a call the tool marks as risky.
    #[default]
    Ask,
    /// A model classifier returns allow, ask, or deny.
    Auto,
    /// Never ask. Deny rules still block.
    Yolo,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Ask => "ask",
            Mode::Auto => "auto",
            Mode::Yolo => "yolo",
        }
    }

    pub fn summary(self) -> &'static str {
        match self {
            Mode::Ask => "ask before risky tool calls",
            Mode::Auto => "a classifier allows, asks, or denies risky tool calls",
            Mode::Yolo => "run tools without asking (deny rules still block)",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "ask" => Some(Mode::Ask),
            "auto" => Some(Mode::Auto),
            "yolo" => Some(Mode::Yolo),
            _ => None,
        }
    }

    pub fn is_yolo(self) -> bool {
        self == Mode::Yolo
    }

    pub fn code(self) -> u8 {
        match self {
            Mode::Ask => 0,
            Mode::Auto => 1,
            Mode::Yolo => 2,
        }
    }

    pub fn from_code(code: u8) -> Self {
        match code {
            1 => Mode::Auto,
            2 => Mode::Yolo,
            _ => Mode::Ask,
        }
    }
}

/// `--permission-mode` wins, then `--yolo`, then the settings value.
/// An unknown settings value falls back to ask and returns a warning.
/// An unknown flag is an error.
pub fn resolve_mode(
    flag: Option<&str>,
    yolo_flag: bool,
    settings_mode: &str,
) -> Result<(Mode, Option<String>)> {
    if let Some(raw) = flag {
        let mode = Mode::parse(raw).with_context(|| {
            format!("unknown permission mode `{raw}` (expected ask, auto or yolo)")
        })?;
        return Ok((mode, None));
    }
    if yolo_flag {
        return Ok((Mode::Yolo, None));
    }
    match Mode::parse(settings_mode) {
        Some(mode) => Ok((mode, None)),
        None => Ok((
            Mode::Ask,
            Some(format!(
                "ignoring permission_mode `{settings_mode}` in settings (expected ask, auto or yolo); using ask"
            )),
        )),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Allow,
    Ask,
    Deny,
}

impl Action {
    pub fn as_str(self) -> &'static str {
        match self {
            Action::Allow => "allow",
            Action::Ask => "ask",
            Action::Deny => "deny",
        }
    }
}

/// What the loop does with a call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    Run,
    /// Ask the user. `forced` names the `ask` rule that requires it; a
    /// session grant can't skip such a call, so "always" isn't offered.
    Ask {
        forced: Option<String>,
    },
    /// Blocked by this rule (already described for the model).
    Deny(String),
    /// Risky, and no rule or grant decided it. Only `auto` mode returns this.
    Classify,
}

#[derive(Clone, Debug, Default)]
pub struct Permissions {
    /// The user's file (`<home>/nyq/permissions.json`); empty when there is none.
    pub path: PathBuf,
    /// The project's file, when the workspace has one.
    pub project_path: Option<PathBuf>,
    pub allow: Vec<String>,
    pub ask: Vec<String>,
    pub deny: Vec<String>,
    pub project_ask: Vec<String>,
    pub project_deny: Vec<String>,
    /// Allow rules saved for this workspace only (in the user file, under
    /// `workspaces`); `workspace` is the key, empty when there is none.
    pub local: Vec<String>,
    pub workspace: String,
    /// Exact-call rules for this session (`/permissions remember`).
    pub session_rules: Vec<crate::grants::SessionRule>,
}

#[derive(Default)]
struct Rules {
    allow: Vec<String>,
    ask: Vec<String>,
    deny: Vec<String>,
}

pub fn user_path(home: &Path) -> PathBuf {
    home.join("permissions.json")
}

pub fn project_path(cwd: &Path) -> PathBuf {
    cwd.join(".nyq").join("permissions.json")
}

/// Parse a permissions file. Unknown keys are ignored.
fn parse(text: &str) -> Result<Rules, String> {
    let v: Value = serde_json::from_str(text).map_err(|e| format!("not valid JSON: {e}"))?;
    let Value::Object(map) = v else {
        return Err("expected a JSON object with \"allow\", \"ask\" and \"deny\" lists".into());
    };
    let list = |key: &str| -> Result<Vec<String>, String> {
        match map.get(key) {
            None | Some(Value::Null) => Ok(Vec::new()),
            Some(Value::Array(items)) => items
                .iter()
                .map(|i| match i.as_str().map(str::trim) {
                    Some(s) if !s.is_empty() => Ok(s.to_string()),
                    _ => Err(format!(
                        "\"{key}\" entries must be non-empty strings, got {i}"
                    )),
                })
                .collect(),
            Some(other) => Err(format!("\"{key}\" must be a list of strings, got {other}")),
        }
    };
    Ok(Rules {
        allow: list("allow")?,
        ask: list("ask")?,
        deny: list("deny")?,
    })
}

/// Read one file: `Ok(None)` when it doesn't exist.
fn read(path: &Path) -> Result<Option<Rules>, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => parse(&text).map(Some),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

impl Permissions {
    /// Load the user and project files. Problems come back as warnings (to
    /// show once); a malformed file is ignored as a whole.
    pub fn load(home: &Path, cwd: &Path) -> (Self, Vec<String>) {
        let mut perms = Permissions {
            path: user_path(home),
            ..Default::default()
        };
        let mut warnings = Vec::new();
        match read(&perms.path) {
            Ok(Some(r)) => {
                perms.allow = r.allow;
                perms.ask = r.ask;
                perms.deny = r.deny;
            }
            Ok(None) => {}
            Err(e) => warnings.push(format!("ignoring {}: {e}", perms.path.display())),
        }
        perms.workspace = crate::grants::workspace_key(cwd);
        perms.local = crate::grants::read_local(&perms.path, &perms.workspace);
        let project = project_path(cwd);
        // The workspace may be the profile directory itself.
        if project != perms.path {
            match read(&project) {
                Ok(Some(r)) => {
                    if !r.allow.is_empty() {
                        warnings.push(format!(
                            "ignoring \"allow\" in {}: a project may only add \"ask\" and \"deny\" rules",
                            project.display()
                        ));
                    }
                    perms.project_ask = r.ask;
                    perms.project_deny = r.deny;
                    perms.project_path = Some(project);
                }
                Ok(None) => {}
                Err(e) => warnings.push(format!("ignoring {}: {e}", project.display())),
            }
        }
        (perms, warnings)
    }

    /// The first rule of `action` that matches this call, with its source.
    fn find(&self, action: Action, tool: &str, args: &Value, cwd: &Path) -> Option<String> {
        let (user, project): (&[String], &[String]) = match action {
            Action::Allow => (&self.allow, &[]),
            Action::Ask => (&self.ask, &self.project_ask),
            Action::Deny => (&self.deny, &self.project_deny),
        };
        // Allow rules for bash need an exact prefix match on a plain command;
        // ask/deny rules also catch the prefix inside chains and wrappers.
        let strict = action == Action::Allow;
        let hit = |p: &&String| {
            path_rule_matches(p, tool, args, cwd, strict) || matches(p, tool, args, strict)
        };
        if let Some(r) = user.iter().find(hit) {
            return Some(format!("{} {r}", action.as_str()));
        }
        if action == Action::Allow {
            if let Some(r) = self.local.iter().find(hit) {
                return Some(format!("allow {r} (this workspace)"));
            }
        }
        project
            .iter()
            .find(hit)
            .map(|r| format!("{} {r} (project .nyq/permissions.json)", action.as_str()))
    }

    /// Decide a call. `needs_approval` is the tool's own default.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn decide(
        &self,
        session: &HashSet<String>,
        tool: &str,
        args: &Value,
        cwd: &Path,
        needs_approval: bool,
        mode: Mode,
    ) -> Decision {
        self.decide_in(session, tool, args, cwd, &[], needs_approval, mode)
    }

    /// Like [`decide`], treating `extra` directories as inside the workspace.
    #[allow(clippy::too_many_arguments)]
    pub fn decide_in(
        &self,
        session: &HashSet<String>,
        tool: &str,
        args: &Value,
        cwd: &Path,
        extra: &[PathBuf],
        needs_approval: bool,
        mode: Mode,
    ) -> Decision {
        if let Some(rule) = self.find(Action::Deny, tool, args, cwd) {
            return Decision::Deny(rule);
        }
        let exact = self.session_rules.iter().find(|r| r.matches(tool, args));
        if let Some(r) = exact.filter(|r| !r.allow) {
            return Decision::Deny(format!("deny (session rule #{})", r.id));
        }
        if let Some(rule) = self.find(Action::Ask, tool, args, cwd) {
            return if mode == Mode::Yolo {
                Decision::Run
            } else {
                Decision::Ask { forced: Some(rule) }
            };
        }
        // A path-scoped allow names its paths, so it may cover reads and
        // edits outside the workspace (never edits under `.git`). Workspace
        // local rules are allows too.
        let path_allow = || {
            self.allow
                .iter()
                .chain(self.local.iter())
                .any(|p| path_rule_matches(p, tool, args, cwd, true))
                && !(is_file_tool(tool) && crate::agent::path_under_git(args, cwd))
        };
        if exact.is_some()
            || path_allow()
            || (self.find(Action::Allow, tool, args, cwd).is_some()
                && allow_applies(tool, args, cwd, extra))
        {
            return Decision::Run;
        }
        if !needs_approval || crate::agent::is_allowed_in(session, tool, args, cwd, extra) {
            return Decision::Run;
        }
        match mode {
            Mode::Yolo => Decision::Run,
            Mode::Ask => Decision::Ask { forced: None },
            Mode::Auto => Decision::Classify,
        }
    }

    /// The action every tool of an MCP server gets from server-wide rules
    /// (`mcp__name__*`, `mcp__*`), and whether tool-specific rules exist.
    pub fn server_default(&self, prefix: &str) -> (Action, bool) {
        // A name no real tool has: only patterns that cover the whole server match.
        let probe = format!("{prefix}\u{0}");
        let hit = |rules: &[String]| rules.iter().any(|p| glob(p, &probe));
        let action = if hit(&self.deny) || hit(&self.project_deny) {
            Action::Deny
        } else if hit(&self.ask) || hit(&self.project_ask) {
            Action::Ask
        } else if hit(&self.allow) {
            Action::Allow
        } else {
            Action::Ask
        };
        let specific = [
            &self.allow,
            &self.ask,
            &self.deny,
            &self.project_ask,
            &self.project_deny,
        ]
        .iter()
        .flat_map(|r| r.iter())
        .any(|p| p.starts_with(prefix) && !glob(p, &probe));
        (action, specific)
    }

    /// Append `rule` to the user file's `allow` list (and to this copy).
    /// Other content of the file is kept; a malformed file is not overwritten.
    pub fn save_allow(&mut self, rule: &str) -> Result<()> {
        if self.path.as_os_str().is_empty() {
            bail!("no permissions file in this context");
        }
        let mut doc = match std::fs::read_to_string(&self.path) {
            Ok(text) => {
                parse(&text).map_err(|e| {
                    anyhow::anyhow!("{} is malformed ({e}); fix it first", self.path.display())
                })?;
                serde_json::from_str::<Value>(&text)?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => serde_json::json!({}),
            Err(e) => return Err(e).with_context(|| format!("reading {}", self.path.display())),
        };
        let map = doc
            .as_object_mut()
            .context("permissions file is not a JSON object")?;
        for key in ["allow", "ask", "deny"] {
            map.entry(key).or_insert_with(|| Value::Array(Vec::new()));
        }
        let allow = map
            .get_mut("allow")
            .and_then(Value::as_array_mut)
            .context("\"allow\" is not a list")?;
        if !allow.iter().any(|v| v.as_str() == Some(rule)) {
            allow.push(Value::String(rule.to_string()));
        }
        let mut text = serde_json::to_string_pretty(&doc)?;
        text.push('\n');
        crate::session::write_private(&self.path, text.as_bytes())
            .with_context(|| format!("writing {}", self.path.display()))?;
        if !self.allow.iter().any(|r| r == rule) {
            self.allow.push(rule.to_string());
        }
        Ok(())
    }

    /// Drop `rule` from the user file's `allow` list. A missing file or a
    /// rule that is only in memory is fine; a malformed file is left alone.
    /// Returns whether the in-memory list or the file changed.
    pub fn remove_allow(&mut self, rule: &str) -> Result<bool> {
        let had = self.allow.iter().any(|r| r == rule);
        self.allow.retain(|r| r != rule);
        if self.path.as_os_str().is_empty() {
            return Ok(had);
        }
        let text = match std::fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(had),
            Err(e) => return Err(e).with_context(|| format!("reading {}", self.path.display())),
        };
        parse(&text).map_err(|e| {
            anyhow::anyhow!("{} is malformed ({e}); fix it first", self.path.display())
        })?;
        let mut doc: Value = serde_json::from_str(&text)?;
        let Some(allow) = doc
            .as_object_mut()
            .and_then(|map| map.get_mut("allow"))
            .and_then(Value::as_array_mut)
        else {
            return Ok(had);
        };
        let before = allow.len();
        allow.retain(|v| v.as_str() != Some(rule));
        if allow.len() == before {
            return Ok(had);
        }
        let mut text = serde_json::to_string_pretty(&doc)?;
        text.push('\n');
        crate::session::write_private(&self.path, text.as_bytes())
            .with_context(|| format!("writing {}", self.path.display()))?;
        Ok(true)
    }

    /// Rows for `/permissions`.
    pub fn lines(&self, session: &HashSet<String>) -> Vec<String> {
        let list = |rules: &[String]| {
            if rules.is_empty() {
                "(none)".to_string()
            } else {
                rules.join("  ")
            }
        };
        let mut out = vec![
            format!(
                "Rules file   {}",
                if self.path.as_os_str().is_empty() {
                    "(none)".into()
                } else {
                    self.path.display().to_string()
                }
            ),
            format!("  deny       {}", list(&self.deny)),
            format!("  ask        {}", list(&self.ask)),
            format!("  allow      {}", list(&self.allow)),
        ];
        match &self.project_path {
            Some(p) => {
                out.push(format!("Project file {}", p.display()));
                out.push(format!("  deny       {}", list(&self.project_deny)));
                out.push(format!("  ask        {}", list(&self.project_ask)));
            }
            None => {
                out.push("Project file (none; .nyq/permissions.json may add ask/deny rules)".into())
            }
        }
        let mut grants: Vec<&str> = session.iter().map(String::as_str).collect();
        grants.sort_unstable();
        out.push(format!(
            "Session      {}",
            if grants.is_empty() {
                "(no grants)".into()
            } else {
                grants.join("  ")
            }
        ));
        out.push("Precedence: deny > ask > allow > session grants > mode (ask prompts, auto classifies, yolo runs). Deny wins in every mode. /permissions reload re-reads the files.".into());
        out
    }
}

/// Whether a persistent allow may skip the prompt for this call: never for
/// file writes outside the workspace or under `.git`, nor for reads (which
/// only ask outside the workspace). `extra` directories count as inside.
fn allow_applies(tool: &str, args: &Value, cwd: &Path, extra: &[PathBuf]) -> bool {
    if is_file_tool(tool) {
        let path = args.get("path").and_then(Value::as_str).unwrap_or("");
        return crate::agent::file_grant_ok_in(path, cwd, extra);
    }
    !READ_TOOLS.contains(&tool)
}

/// Whether the session's "always" rule for this call may also be saved.
#[cfg_attr(not(test), allow(dead_code))]
pub fn saveable(tool: &str, args: &Value, cwd: &Path) -> bool {
    saveable_in(tool, args, cwd, &[])
}

/// Like [`saveable`], treating `extra` directories as inside the workspace.
pub fn saveable_in(tool: &str, args: &Value, cwd: &Path, extra: &[PathBuf]) -> bool {
    allow_applies(tool, args, cwd, extra)
}

/// Names a call answers to: the tool, plus `subagent:<mode>` or `web_fetch:<url>`.
fn call_keys(tool: &str, args: &Value) -> Vec<String> {
    let mut keys = vec![tool.to_string()];
    match tool {
        "subagent" => keys.push(format!("subagent:{}", subagent_mode(args))),
        "web_fetch" => {
            if let Some(url) = args.get("url").and_then(Value::as_str) {
                keys.push(format!("web_fetch:{url}"));
                // Also the parsed spelling (lowercase host, explicit `/`), so
                // a site rule like `web_fetch:https://docs.rs/*` can't be
                // dodged by writing the URL differently.
                if let Some(norm) = crate::agent::normalize_url(url).filter(|n| n != url) {
                    keys.push(format!("web_fetch:{norm}"));
                }
            }
        }
        // Resource tools are not named `mcp__<server>__…`, but server-wide
        // rules (`mcp__docs__*`, `mcp__*`) still cover the server they name.
        "mcp_list_resources" | "mcp_read_resource" => {
            if let Some(server) = args.get("server").and_then(Value::as_str).filter(|s| !s.is_empty()) {
                keys.push(format!("{}{tool}", crate::mcp::tool_prefix(server)));
            }
            keys.push(format!("mcp__{tool}"));
        }
        _ => {}
    }
    keys
}

/// Whether `pattern` covers this call. `strict` selects allow semantics for
/// `bash:` rules (see [`bash_matches`]); otherwise [`bash_mentions`].
pub fn matches(pattern: &str, tool: &str, args: &Value, strict: bool) -> bool {
    if let Some(prefix) = pattern.strip_prefix("bash:") {
        if tool != "bash" {
            return false;
        }
        let cmd = args.get("command").and_then(Value::as_str).unwrap_or("");
        return if strict {
            bash_matches(prefix, cmd)
        } else {
            bash_mentions(prefix, cmd)
        };
    }
    call_keys(tool, args).iter().any(|k| glob(pattern, k))
}

/// The path category of a tool: `read` for the read tools, `edit` for
/// `write_file` / `edit_file`.
fn path_kind(tool: &str) -> Option<&'static str> {
    if is_read_tool(tool) {
        Some("read")
    } else if is_file_tool(tool) {
        Some("edit")
    } else {
        None
    }
}

/// Whether a path-scoped rule (`edit_file:src/**`, `read_file:secrets/*`,
/// `read:~/notes/**`, `edit:/tmp/**`) covers this call. The prefix names a
/// category: any read tool's rule covers every read tool, and `write_file` /
/// `edit_file` cover each other. Relative patterns are relative to the
/// workspace; `*` matches within one path segment and `**` any number of
/// segments. `strict` (allow semantics) requires the path with symlinks
/// resolved to match; otherwise either spelling does.
pub(crate) fn path_rule_matches(rule: &str, tool: &str, args: &Value, cwd: &Path, strict: bool) -> bool {
    let Some((prefix, pattern)) = rule.split_once(':') else {
        return false;
    };
    let Some(kind) = path_kind(tool) else {
        return false;
    };
    let wanted = match prefix {
        "read" | "edit" => Some(prefix),
        other => path_kind(other),
    };
    if wanted != Some(kind) || pattern.is_empty() {
        return false;
    }
    let path = match args.get("path").and_then(Value::as_str) {
        Some(p) if !p.is_empty() => p,
        _ if kind == "read" => ".",
        _ => return false,
    };
    let Some(lexical) = crate::agent::lexical(cwd, path) else {
        return false;
    };
    let resolved = crate::agent::resolved(cwd, path).unwrap_or_else(|| lexical.clone());
    let expanded = match pattern.strip_prefix("~/") {
        Some(rest) => dirs::home_dir().map(|h| slash(&h.join(rest))),
        None => Some(pattern.to_string()),
    };
    let Some(pattern) = expanded else {
        return false;
    };
    let relative = !Path::new(&pattern).is_absolute() && !pattern.starts_with('/');
    let hit = |full: &Path, root: &Path| {
        if relative {
            full.strip_prefix(root)
                .is_ok_and(|rel| path_glob(&pattern, &slash(rel)))
        } else {
            path_glob(&pattern, &slash(full))
        }
    };
    let lexical_root = crate::agent::lexical(cwd, ".").unwrap_or_else(|| cwd.to_path_buf());
    let real_root = std::fs::canonicalize(cwd).unwrap_or_else(|_| lexical_root.clone());
    let resolved_hit = hit(&resolved, &real_root);
    resolved_hit || (!strict && hit(&lexical, &lexical_root))
}

fn slash(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

/// Path globbing by segment: `*` within a segment, `**` across segments.
pub(crate) fn path_glob(pattern: &str, path: &str) -> bool {
    fn go(p: &[&str], t: &[&str]) -> bool {
        match p.split_first() {
            None => t.is_empty(),
            Some((&"**", rest)) => (0..=t.len()).any(|i| go(rest, &t[i..])),
            Some((seg, rest)) => t
                .split_first()
                .is_some_and(|(x, tail)| glob(seg, x) && go(rest, tail)),
        }
    }
    let split = |s: &str| -> Vec<String> {
        s.trim_end_matches('/')
            .split('/')
            .enumerate()
            .filter(|(i, seg)| *i == 0 || (!seg.is_empty() && *seg != "."))
            .map(|(_, seg)| seg.to_string())
            .collect()
    };
    let (p, t) = (split(pattern), split(path));
    let p: Vec<&str> = p.iter().map(String::as_str).collect();
    let t: Vec<&str> = t.iter().map(String::as_str).collect();
    go(&p, &t)
}

/// Whether the words of `prefix` appear anywhere in `cmd`, after dropping
/// quotes and backslashes and splitting on operators: so `bash:rm` catches
/// `ls; rm x`, `sh -c 'rm x'` and `/bin/rm x`. Best effort (variables and
/// aliases can still hide a command); the sandbox is the real boundary.
pub fn bash_mentions(prefix: &str, cmd: &str) -> bool {
    let want: Vec<&str> = prefix.split_whitespace().collect();
    let Some(first) = want.first() else {
        return false;
    };
    let cleaned: String = cmd
        .chars()
        .filter(|c| !matches!(c, '\'' | '"' | '\\'))
        .collect();
    let words: Vec<&str> = cleaned
        .split(|c: char| c.is_whitespace() || ";&|`()<>{}$".contains(c))
        .filter(|w| !w.is_empty())
        .collect();
    (0..words.len()).any(|i| {
        let head = words[i];
        (head == *first || (!first.contains('/') && program_base(head) == *first))
            && words.len() >= i + want.len()
            && words[i + 1..i + want.len()] == want[1..]
    })
}

/// `*` matches any run of characters; everything else is literal.
pub fn glob(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        if pi < p.len() && p[pi] == '*' {
            star = Some((pi, ti));
            pi += 1;
        } else if pi < p.len() && p[pi] == t[ti] {
            pi += 1;
            ti += 1;
        } else if let Some((sp, st)) = star {
            pi = sp + 1;
            ti = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

/// Text for `nyq permissions`. Session grants exist only inside a conversation.
pub fn format_report(perms: &Permissions, mode: Mode, warnings: &[String], json: bool) -> String {
    if json {
        let v = serde_json::json!({
            "kind": "permissions",
            "mode": mode.as_str(),
            "file": perms.path,
            "rules_file": perms.path.display().to_string(),
            "deny": perms.deny,
            "ask": perms.ask,
            "allow": perms.allow,
            "project_file": perms.project_path.as_ref().map(|p| p.display().to_string()),
            "project_deny": perms.project_deny,
            "project_ask": perms.project_ask,
            "warnings": warnings,
        });
        return serde_json::to_string_pretty(&v).unwrap_or_default();
    }
    let mut out = vec![format!("Mode         {} ({})", mode.as_str(), mode.summary())];
    out.extend(
        perms
            .lines(&Default::default())
            .into_iter()
            .filter(|l| !l.starts_with("Session")),
    );
    out.push("Session grants only exist inside a conversation; see /allowlist.".into());
    out.push(
        "Change the mode with /permissions, --permission-mode, or `permission_mode` in settings.json; --yolo skips approvals for one run."
            .into(),
    );
    out.join("\n")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrustKind {
    Command,
    Tool,
    Url,
}

impl TrustKind {
    fn label(self) -> &'static str {
        match self {
            TrustKind::Command => "command",
            TrustKind::Tool => "tool",
            TrustKind::Url => "url",
        }
    }

    pub(crate) fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "command" | "commands" => Some(TrustKind::Command),
            "tool" | "tools" => Some(TrustKind::Tool),
            "url" | "urls" => Some(TrustKind::Url),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrustEntry {
    pub kind: TrustKind,
    pub rule: String,
    pub display: String,
    pub session: bool,
    pub saved: bool,
    /// Saved for this workspace only.
    pub local: bool,
}

pub(crate) fn trust_kind(rule: &str) -> TrustKind {
    if rule.starts_with("bash:") {
        TrustKind::Command
    } else if rule.starts_with("web_fetch:") {
        TrustKind::Url
    } else {
        TrustKind::Tool
    }
}

fn trust_display(rule: &str) -> String {
    rule.strip_prefix("bash:")
        .or_else(|| rule.strip_prefix("web_fetch:"))
        .unwrap_or(rule)
        .to_string()
}

pub(crate) fn canonical_rule(kind: TrustKind, raw: &str) -> String {
    let raw = raw.trim();
    match kind {
        TrustKind::Command => format!("bash:{}", raw.strip_prefix("bash:").unwrap_or(raw)),
        TrustKind::Url => format!(
            "web_fetch:{}",
            raw.strip_prefix("web_fetch:").unwrap_or(raw)
        ),
        TrustKind::Tool => raw.to_string(),
    }
}

/// Session grants and `permissions.json` allow entries, commands then tools then URLs.
pub fn trust_entries(perms: &Permissions, session: &HashSet<String>) -> Vec<TrustEntry> {
    let mut rules: Vec<String> = perms
        .allow
        .iter()
        .chain(&perms.local)
        .cloned()
        .chain(session.iter().cloned())
        .collect();
    rules.sort();
    rules.dedup();
    let mut entries: Vec<TrustEntry> = rules
        .into_iter()
        .map(|rule| TrustEntry {
            kind: trust_kind(&rule),
            display: trust_display(&rule),
            session: session.contains(&rule),
            saved: perms.allow.iter().any(|r| r == &rule),
            local: perms.local.iter().any(|r| r == &rule),
            rule,
        })
        .collect();
    entries.sort_by(|a, b| {
        (kind_ord(a.kind), a.display.as_str()).cmp(&(kind_ord(b.kind), b.display.as_str()))
    });
    entries
}

fn kind_ord(kind: TrustKind) -> u8 {
    match kind {
        TrustKind::Command => 0,
        TrustKind::Tool => 1,
        TrustKind::Url => 2,
    }
}

fn source_label(entry: &TrustEntry) -> String {
    let sources: Vec<&str> = [(entry.session, "session"), (entry.saved, "permissions.json"), (entry.local, "this workspace")]
        .iter()
        .filter(|(on, _)| *on)
        .map(|(_, label)| *label)
        .collect();
    if sources.is_empty() {
        "unknown".into()
    } else {
        sources.join(", ")
    }
}

/// Lines for `/allowlist`.
pub fn allowlist_lines(perms: &Permissions, session: &HashSet<String>) -> Vec<String> {
    let entries = trust_entries(perms, session);
    if entries.is_empty() {
        return vec!["Nothing is trusted yet. \"always\" grants it for this session; \"always, and save\" writes permissions.json.".into()];
    }
    let mut out = vec!["Trusted commands, tools and URLs".into()];
    let mut current: Option<TrustKind> = None;
    for (index, entry) in entries.iter().enumerate() {
        if current != Some(entry.kind) {
            current = Some(entry.kind);
            out.push(format!(
                "  {}:",
                match entry.kind {
                    TrustKind::Command => "commands",
                    TrustKind::Tool => "tools",
                    TrustKind::Url => "urls",
                }
            ));
        }
        out.push(format!(
            "    {}  {}  ({})",
            index + 1,
            entry.display,
            source_label(entry)
        ));
    }
    out.push(
        "Remove one with /allowlist remove N; add with /allowlist [local|user] add command|tool|url PATTERN."
            .into(),
    );
    out
}

/// Remove one trusted command, tool, or URL from the session and the allow file.
pub fn allowlist_remove(
    perms: &mut Permissions,
    session: &mut HashSet<String>,
    args: &[&str],
) -> Result<String, String> {
    let entries = trust_entries(perms, session);
    let entry = resolve_removal(args, &entries)?;
    let rule = entry.rule.clone();
    let kind = entry.kind;
    let display = entry.display.clone();
    let sources = source_label(&entry);
    let session_hit = session.remove(&rule);
    let saved = perms.remove_allow(&rule).map_err(|e| format!("{e:#}"))?;
    let local = crate::grants::remove_local(perms, &rule).map_err(|e| format!("{e:#}"))?;
    if !session_hit && !saved && !local {
        return Err(format!("no matching {} `{display}`", kind.label()));
    }
    Ok(format!("removed {} `{display}` ({sources})", kind.label()))
}

fn resolve_removal(args: &[&str], entries: &[TrustEntry]) -> Result<TrustEntry, String> {
    let usage = "usage: /allowlist remove N, or /allowlist remove command|tool|url PATTERN";
    if args.is_empty() {
        return Err(usage.into());
    }
    if args.len() == 1 {
        if let Ok(n) = args[0].parse::<usize>() {
            if n == 0 || n > entries.len() {
                return Err(format!("no allowlist entry {n}"));
            }
            return Ok(entries[n - 1].clone());
        }
    }
    let kind = TrustKind::parse(args[0]).ok_or_else(|| usage.to_string())?;
    let raw = args[1..].join(" ");
    if raw.trim().is_empty() {
        return Err(usage.into());
    }
    let rule = canonical_rule(kind, &raw);
    if let Some(entry) = entries
        .iter()
        .find(|e| e.rule == rule || e.display == raw.trim())
    {
        if entry.kind != kind {
            return Err(format!(
                "`{}` is a {}, not a {}",
                raw.trim(),
                entry.kind.label(),
                kind.label()
            ));
        }
        return Ok(entry.clone());
    }
    Err(format!("no matching {} `{}`", kind.label(), raw.trim()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn perms(allow: &[&str], ask: &[&str], deny: &[&str]) -> Permissions {
        let v = |r: &[&str]| r.iter().map(|s| s.to_string()).collect();
        Permissions {
            allow: v(allow),
            ask: v(ask),
            deny: v(deny),
            ..Default::default()
        }
    }

    #[test]
    fn wildcards() {
        assert!(glob("mcp__deepwiki__*", "mcp__deepwiki__ask_question"));
        assert!(glob("mcp__*", "mcp__x__y"));
        assert!(glob("*", "anything"));
        assert!(glob("web_fetch", "web_fetch"));
        assert!(!glob("web_fetch", "web_fetch2"));
        assert!(!glob("mcp__deepwiki__*", "mcp__deepwiki2__x"));
        assert!(glob("mcp__*__read*", "mcp__fs__read_file"));
        assert!(!glob("mcp__*__read*", "mcp__fs__write_file"));
        assert!(glob("a*b*c", "aXbYbZc"));
        assert!(!glob("a*b*c", "aXbYbZ"));
        assert!(glob(
            "web_fetch:https://docs.rs/*",
            "web_fetch:https://docs.rs/serde"
        ));
    }

    #[test]
    fn pattern_kinds() {
        let sub = |mode: &str| json!({"task": "x", "mode": mode});
        assert!(matches(
            "subagent:general",
            "subagent",
            &sub("general"),
            false
        ));
        assert!(!matches(
            "subagent:general",
            "subagent",
            &sub("explore"),
            false
        ));
        assert!(matches(
            "subagent:explore",
            "subagent",
            &json!({"task": "x"}),
            false
        ));
        assert!(matches("subagent", "subagent", &sub("general"), false));
        assert!(matches("subagent:*", "subagent", &sub("general"), false));
        assert!(matches(
            "web_fetch",
            "web_fetch",
            &json!({"url": "https://a"}),
            false
        ));
        assert!(!matches("web_fetch", "web_search", &json!({}), false));
        let bash = |c: &str| json!({ "command": c });
        assert!(matches(
            "bash:cargo test",
            "bash",
            &bash("cargo test --lib"),
            true
        ));
        assert!(!matches(
            "bash:cargo test",
            "bash",
            &bash("cargo test && rm x"),
            true
        ));
        assert!(!matches("bash:cargo test", "write_file", &json!({}), true));
        for c in [
            "rm -rf x",
            "ls; rm x",
            "ls && /bin/rm x",
            "sh -c 'rm x'",
            "echo $(rm x)",
            "r\\m x",
            "'r'm x",
        ] {
            assert!(matches("bash:rm", "bash", &bash(c), false), "{c}");
        }
        assert!(matches(
            "bash:git push",
            "bash",
            &bash("git status && git push --force"),
            false
        ));
        assert!(!matches("bash:git push", "bash", &bash("git pull"), false));
        assert!(!matches("bash:rm", "bash", &bash("rmdir x"), false));
    }

    #[test]
    fn parsing_and_malformed_files() {
        let r =
            parse(r#"{"allow": ["web_fetch", " mcp__a__* "], "deny": ["bash:rm"], "other": 1}"#)
                .unwrap();
        assert_eq!(r.allow, ["web_fetch", "mcp__a__*"]);
        assert_eq!(r.deny, ["bash:rm"]);
        assert!(r.ask.is_empty());
        assert!(parse("[]").is_err());
        assert!(parse("{").is_err());
        assert!(parse(r#"{"allow": "web_fetch"}"#).is_err());
        assert!(parse(r#"{"allow": [1]}"#).is_err());
        assert!(parse(r#"{"allow": [""]}"#).is_err());

        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        std::fs::write(user_path(home.path()), "{ not json").unwrap();
        let (p, warnings) = Permissions::load(home.path(), cwd.path());
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(p.allow.is_empty() && p.deny.is_empty());
    }

    #[test]
    fn project_file_cannot_allow() {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(cwd.path().join(".nyq")).unwrap();
        std::fs::write(
            project_path(cwd.path()),
            r#"{"allow": ["bash:rm", "mcp__*"], "ask": ["web_fetch"], "deny": ["mcp__evil__*"]}"#,
        )
        .unwrap();
        let (p, warnings) = Permissions::load(home.path(), cwd.path());
        assert!(
            warnings.iter().any(|w| w.contains("may only add")),
            "{warnings:?}"
        );
        assert!(p.allow.is_empty());
        assert_eq!(p.project_ask, ["web_fetch"]);
        assert_eq!(p.project_deny, ["mcp__evil__*"]);
        let none = HashSet::new();
        let d = p.decide(
            &none,
            "mcp__good__x",
            &json!({}),
            cwd.path(),
            true,
            Mode::Ask,
        );
        assert_eq!(d, Decision::Ask { forced: None });
        let d = p.decide(
            &none,
            "mcp__evil__x",
            &json!({}),
            cwd.path(),
            true,
            Mode::Yolo,
        );
        assert!(matches!(d, Decision::Deny(r) if r.contains("project")));
        let d = p.decide(
            &none,
            "web_fetch",
            &json!({"url": "https://a"}),
            cwd.path(),
            false,
            Mode::Ask,
        );
        assert!(matches!(d, Decision::Ask { forced: Some(_) }));
    }

    #[test]
    fn precedence_and_yolo() {
        let cwd = tempfile::tempdir().unwrap();
        let cwd = cwd.path();
        let tool = "mcp__wiki__ask";
        let args = json!({});
        let session: HashSet<String> = [tool.to_string()].into();
        let none = HashSet::new();
        for (mode, yolo) in [(Mode::Ask, false), (Mode::Yolo, true)] {
            // deny beats everything, even yolo.
            let p = perms(&["mcp__*"], &["mcp__*"], &["mcp__wiki__*"]);
            assert!(matches!(
                p.decide(&session, tool, &args, cwd, true, mode),
                Decision::Deny(_)
            ));
            // ask beats allow and a session grant; yolo skips it.
            let p = perms(&["mcp__*"], &["mcp__wiki__*"], &[]);
            let d = p.decide(&session, tool, &args, cwd, true, mode);
            if yolo {
                assert_eq!(d, Decision::Run)
            } else {
                assert!(matches!(d, Decision::Ask { forced: Some(_) }))
            }
            // allow runs without asking.
            let p = perms(&["mcp__wiki__*"], &[], &[]);
            assert_eq!(p.decide(&none, tool, &args, cwd, true, mode), Decision::Run);
            // session grant.
            let p = perms(&[], &[], &[]);
            assert_eq!(
                p.decide(&session, tool, &args, cwd, true, mode),
                Decision::Run
            );
            // default: asks unless yolo; free tools run.
            let d = p.decide(&none, tool, &args, cwd, true, mode);
            assert_eq!(
                d,
                if yolo {
                    Decision::Run
                } else {
                    Decision::Ask { forced: None }
                }
            );
            assert_eq!(
                p.decide(&none, "subagent", &json!({"task": "x"}), cwd, false, mode),
                Decision::Run
            );
            // deny applies to tools that never ask.
            let p = perms(&[], &[], &["subagent:explore"]);
            assert!(matches!(
                p.decide(&none, "subagent", &json!({"task": "x"}), cwd, false, mode),
                Decision::Deny(_)
            ));
        }
    }

    #[test]
    fn mode_precedence_with_rules() {
        let cwd = tempfile::tempdir().unwrap();
        let cwd = cwd.path();
        let tool = "mcp__wiki__ask";
        let args = json!({});
        let none = HashSet::new();
        let session: HashSet<String> = [tool.to_string()].into();
        let denied = perms(&["mcp__*"], &[], &["mcp__wiki__*"]);
        for mode in [Mode::Ask, Mode::Auto, Mode::Yolo] {
            assert!(matches!(
                denied.decide(&session, tool, &args, cwd, true, mode),
                Decision::Deny(_)
            ));
        }
        let asked = perms(&["mcp__*"], &["mcp__wiki__ask"], &[]);
        assert!(matches!(
            asked.decide(&session, tool, &args, cwd, true, Mode::Auto),
            Decision::Ask { forced: Some(_) }
        ));
        assert_eq!(
            asked.decide(&session, tool, &args, cwd, true, Mode::Yolo),
            Decision::Run
        );
        let allowed = perms(&["mcp__wiki__*"], &[], &[]);
        assert_eq!(
            allowed.decide(&none, tool, &args, cwd, true, Mode::Auto),
            Decision::Run
        );
        let open = perms(&[], &[], &[]);
        assert_eq!(
            open.decide(&none, tool, &args, cwd, true, Mode::Ask),
            Decision::Ask { forced: None }
        );
        assert_eq!(
            open.decide(&none, tool, &args, cwd, true, Mode::Auto),
            Decision::Classify
        );
        assert_eq!(
            open.decide(&none, tool, &args, cwd, true, Mode::Yolo),
            Decision::Run
        );
        assert_eq!(
            open.decide(&session, tool, &args, cwd, true, Mode::Auto),
            Decision::Run
        );
        let (mode, warn) = resolve_mode(Some("ask"), true, "yolo").unwrap();
        assert_eq!(mode, Mode::Ask);
        assert!(warn.is_none());
        assert_eq!(resolve_mode(None, true, "ask").unwrap().0, Mode::Yolo);
        assert_eq!(resolve_mode(None, false, "auto").unwrap().0, Mode::Auto);
        let (mode, warn) = resolve_mode(None, false, "plan").unwrap();
        assert_eq!(mode, Mode::Ask);
        assert!(warn.unwrap().contains("plan"));
        assert!(resolve_mode(Some("read-only"), false, "ask").is_err());
    }

    #[test]
    fn allow_keeps_workspace_protections() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        let p = perms(&["write_file", "read_file", "*"], &[], &[]);
        let none = HashSet::new();
        let d = |tool: &str, path: &str| {
            p.decide(&none, tool, &json!({"path": path}), cwd, true, Mode::Ask)
        };
        assert_eq!(d("write_file", "src/a.rs"), Decision::Run);
        assert_eq!(
            d("write_file", ".git/hooks/pre-commit"),
            Decision::Ask { forced: None }
        );
        assert_eq!(
            d("write_file", "../outside"),
            Decision::Ask { forced: None }
        );
        assert_eq!(d("read_file", "/etc/hosts"), Decision::Ask { forced: None });
        assert!(!saveable(
            "write_file",
            &json!({"path": ".git/config"}),
            cwd
        ));
        assert!(!saveable("read_file", &json!({"path": "/etc/hosts"}), cwd));
        assert!(saveable("write_file", &json!({"path": "a.txt"}), cwd));
        assert!(saveable("bash", &json!({"command": "cargo test"}), cwd));
    }

    #[test]
    fn path_globs() {
        assert!(path_glob("src/**", "src/a/b.rs"));
        assert!(path_glob("src/**", "src"));
        assert!(path_glob("src/*.rs", "src/main.rs"));
        assert!(!path_glob("src/*.rs", "src/a/main.rs"));
        assert!(path_glob("**/*.env", "a/b/.env"));
        assert!(path_glob("/etc/**", "/etc/hosts"));
        assert!(!path_glob("/etc/**", "/etcetera/x"));
    }

    #[test]
    fn path_scoped_rules() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        std::fs::create_dir_all(cwd.join("src")).unwrap();
        std::fs::create_dir_all(cwd.join("secrets")).unwrap();
        let none = HashSet::new();
        let p = perms(
            &["edit_file:src/**"],
            &["edit:Cargo.toml"],
            &["read_file:secrets/*"],
        );
        let d = |tool: &str, path: &str| {
            p.decide(&none, tool, &json!({ "path": path }), cwd, true, Mode::Ask)
        };
        // Allow: both file tools under src, nothing else.
        assert_eq!(d("edit_file", "src/a.rs"), Decision::Run);
        assert_eq!(d("write_file", "src/new/b.rs"), Decision::Run);
        assert_eq!(d("write_file", "docs/a.md"), Decision::Ask { forced: None });
        // Ask: forced even in auto; deny: every read tool, any spelling.
        assert!(matches!(d("write_file", "Cargo.toml"), Decision::Ask { forced: Some(_) }));
        assert!(matches!(d("read_file", "secrets/key"), Decision::Deny(_)));
        assert!(matches!(d("grep", "./secrets/key"), Decision::Deny(_)));
        assert!(matches!(
            d("read_file", cwd.join("secrets/key").to_str().unwrap()),
            Decision::Deny(_)
        ));
        assert_eq!(d("read_file", "src/a.rs"), Decision::Ask { forced: None });
        // Absolute allows reach outside the workspace; never under .git.
        let outside = tempfile::tempdir().unwrap();
        let root = outside.path().to_str().unwrap();
        let p = perms(&[&format!("read:{root}/**"), &format!("edit:{root}/**")], &[], &[]);
        let d = |tool: &str, path: String| {
            p.decide(&none, tool, &json!({ "path": path }), cwd, true, Mode::Ask)
        };
        assert_eq!(d("list_dir", root.to_string()), Decision::Run);
        assert_eq!(d("write_file", format!("{root}/a.txt")), Decision::Run);
        assert_eq!(
            d("write_file", format!("{root}/.git/config")),
            Decision::Ask { forced: None }
        );
        #[cfg(unix)]
        {
            // A link out of an allowed tree is judged by where it points.
            let p = perms(&["edit:src/**"], &[], &[]);
            std::os::unix::fs::symlink(outside.path(), cwd.join("src/out")).unwrap();
            assert_eq!(
                p.decide(&none, "write_file", &json!({"path": "src/out/x"}), cwd, true, Mode::Ask),
                Decision::Ask { forced: None }
            );
        }
    }

    #[test]
    fn workspace_local_path_rules_match() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        std::fs::create_dir_all(cwd.join("src")).unwrap();
        let outside = tempfile::tempdir().unwrap();
        let root = outside.path().to_str().unwrap();
        let mut p = perms(&[], &[], &[]);
        p.local = vec!["edit_file:src/**".into(), format!("read:{root}/**")];
        let none = HashSet::new();
        let d = |tool: &str, path: &str| {
            p.decide(&none, tool, &json!({ "path": path }), cwd, true, Mode::Ask)
        };
        assert_eq!(d("edit_file", "src/a.rs"), Decision::Run);
        assert_eq!(d("write_file", "src/new/b.rs"), Decision::Run);
        assert_eq!(d("write_file", "docs/a.md"), Decision::Ask { forced: None });
        assert_eq!(
            d("write_file", "src/.git/config"),
            Decision::Ask { forced: None }
        );
        assert_eq!(d("list_dir", root), Decision::Run);
        assert_eq!(d("read_file", "src/a.rs"), Decision::Ask { forced: None });
        // A deny still wins over a workspace path allow.
        let mut denied = perms(&[], &[], &["edit_file:src/**"]);
        denied.local = vec!["edit_file:src/**".into()];
        assert!(matches!(
            denied.decide(&none, "edit_file", &json!({"path": "src/a.rs"}), cwd, true, Mode::Ask),
            Decision::Deny(_)
        ));
    }

    #[test]
    fn saving_appends_and_keeps_the_file() {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let (mut p, _) = Permissions::load(home.path(), cwd.path());
        p.save_allow("bash:cargo test").unwrap();
        p.save_allow("bash:cargo test").unwrap();
        p.save_allow("mcp__wiki__ask").unwrap();
        assert_eq!(p.allow, ["bash:cargo test", "mcp__wiki__ask"]);
        let (q, warnings) = Permissions::load(home.path(), cwd.path());
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(q.allow, p.allow);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&p.path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        // Other content survives; a malformed file is left alone.
        std::fs::write(&p.path, r#"{"deny": ["bash:rm"], "note": "mine"}"#).unwrap();
        p.save_allow("web_fetch").unwrap();
        let saved: Value =
            serde_json::from_str(&std::fs::read_to_string(&p.path).unwrap()).unwrap();
        assert_eq!(saved["note"], "mine");
        assert_eq!(saved["deny"], json!(["bash:rm"]));
        assert_eq!(saved["allow"], json!(["web_fetch"]));
        std::fs::write(&p.path, "{oops").unwrap();
        assert!(p.save_allow("x").is_err());
        assert_eq!(std::fs::read_to_string(&p.path).unwrap(), "{oops");
        assert!(Permissions::default().save_allow("x").is_err());
    }

    #[test]
    fn mcp_resource_tools_follow_server_rules() {
        let cwd = Path::new(".");
        let args = json!({"server": "docs", "uri": "mem://notes"});
        let denied = perms(&[], &[], &["mcp__docs__*"]);
        let decide = |p: &Permissions, tool: &str, args: &Value, mode: Mode| {
            p.decide(&HashSet::new(), tool, args, cwd, true, mode)
        };
        assert!(matches!(
            decide(&denied, "mcp_read_resource", &args, Mode::Yolo),
            Decision::Deny(_)
        ));
        assert!(matches!(
            decide(&denied, "mcp_list_resources", &json!({"server": "docs"}), Mode::Yolo),
            Decision::Deny(_)
        ));
        let blanket = perms(&[], &[], &["mcp__*"]);
        assert!(matches!(
            decide(&blanket, "mcp_list_resources", &json!({}), Mode::Yolo),
            Decision::Deny(_)
        ));
        assert!(matches!(
            decide(&blanket, "mcp_read_resource", &args, Mode::Ask),
            Decision::Deny(_)
        ));
        let allowed = perms(&["mcp__docs__*"], &[], &[]);
        assert_eq!(
            decide(&allowed, "mcp_read_resource", &args, Mode::Ask),
            Decision::Run
        );
        // A different server is not covered by the docs rule.
        assert_eq!(
            decide(&denied, "mcp_read_resource", &json!({"server": "other"}), Mode::Yolo),
            Decision::Run
        );
    }

    #[test]
    fn server_defaults() {
        let p = perms(&["mcp__a__*"], &["mcp__b__write"], &["mcp__*"]);
        assert_eq!(p.server_default("mcp__a__").0, Action::Deny);
        let p = perms(&["mcp__a__*"], &["mcp__b__write"], &[]);
        assert_eq!(p.server_default("mcp__a__"), (Action::Allow, false));
        assert_eq!(p.server_default("mcp__b__"), (Action::Ask, true));
        assert_eq!(p.server_default("mcp__c__"), (Action::Ask, false));
    }

    #[test]
    fn allowlist_removal_drops_session_grants_and_saved_allows() {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let (mut p, _) = Permissions::load(home.path(), cwd.path());
        p.save_allow("bash:cargo test").unwrap();
        p.save_allow("web_fetch:https://docs.rs/*").unwrap();
        p.save_allow("write_file").unwrap();
        p.deny = vec!["bash:rm".into()];
        let mut session: HashSet<String> =
            ["bash:cargo test".into(), "mcp__wiki__ask".into()].into();
        let lines = allowlist_lines(&p, &session);
        let text = lines.join("\n");
        assert!(
            text.contains("cargo test") && text.contains("session, permissions.json"),
            "{text}"
        );
        assert!(
            text.contains("https://docs.rs/*") && text.contains("mcp__wiki__ask"),
            "{text}"
        );
        assert!(!text.contains("bash:rm"), "{text}");

        let msg = allowlist_remove(&mut p, &mut session, &["command", "cargo test"]).unwrap();
        assert!(msg.contains("removed command `cargo test`"), "{msg}");
        assert!(!session.contains("bash:cargo test"));
        assert!(!p.allow.iter().any(|r| r == "bash:cargo test"));
        let saved: Value =
            serde_json::from_str(&std::fs::read_to_string(&p.path).unwrap()).unwrap();
        assert_eq!(
            saved["allow"],
            json!(["web_fetch:https://docs.rs/*", "write_file"])
        );
        assert!(p.deny.iter().any(|r| r == "bash:rm"));

        allowlist_remove(&mut p, &mut session, &["1"]).unwrap();
        assert!(!session.contains("mcp__wiki__ask"));
        let err =
            allowlist_remove(&mut p, &mut session, &["tool", "https://docs.rs/*"]).unwrap_err();
        assert!(err.contains("url") && err.contains("not a tool"), "{err}");
        allowlist_remove(&mut p, &mut session, &["url", "https://docs.rs/*"]).unwrap();
        assert!(!p.allow.iter().any(|r| r.starts_with("web_fetch:")));
        assert!(allowlist_remove(&mut p, &mut session, &["command", "missing"]).is_err());

        let report = format_report(&p, Mode::Auto, &[], false);
        assert!(report.contains("Mode         auto"), "{report}");
        assert!(report.contains("write_file"), "{report}");
        assert!(report.contains("Session grants"), "{report}");
        let json: Value =
            serde_json::from_str(&format_report(&p, Mode::Auto, &["w".into()], true)).unwrap();
        assert_eq!(json["warnings"][0], "w");
        assert_eq!(json["mode"], "auto");
        assert_eq!(json["kind"], "permissions");
    }
}
