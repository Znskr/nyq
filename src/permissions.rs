//! Persistent permission rules: `<home>/nyq/permissions.json` holds
//! `{"allow": [...], "ask": [...], "deny": [...]}` tool patterns, and an
//! optional `<cwd>/.nyq/permissions.json` may add `ask`/`deny` rules (never
//! `allow`: a repository must not grant itself permissions).
//!
//! A pattern is a tool name with `*` wildcards (`web_fetch`, `mcp__deepwiki__*`,
//! `mcp__*`), `subagent:explore` / `subagent:general`, `web_fetch:<url>` (with
//! wildcards), or `bash:<prefix>` for shell commands. Precedence:
//! deny > ask > allow > session grants > the tool's default.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde_json::Value;

use crate::agent::{bash_matches, is_file_tool, program_base, subagent_mode};

/// Read tools ask only for paths outside the workspace; a persistent allow
/// never covers those.
const READ_TOOLS: &[&str] = &["read_file", "list_dir", "glob", "grep", "view_image"];

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
    Ask { forced: Option<String> },
    /// Blocked by this rule (already described for the model).
    Deny(String),
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
}

#[derive(Default)]
struct Rules {
    allow: Vec<String>,
    ask: Vec<String>,
    deny: Vec<String>,
}

pub fn user_path(home: &Path) -> PathBuf {
    home.join("nyq").join("permissions.json")
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
                    _ => Err(format!("\"{key}\" entries must be non-empty strings, got {i}")),
                })
                .collect(),
            Some(other) => Err(format!("\"{key}\" must be a list of strings, got {other}")),
        }
    };
    Ok(Rules { allow: list("allow")?, ask: list("ask")?, deny: list("deny")? })
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
        let mut perms = Permissions { path: user_path(home), ..Default::default() };
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
    fn find(&self, action: Action, tool: &str, args: &Value) -> Option<String> {
        let (user, project): (&[String], &[String]) = match action {
            Action::Allow => (&self.allow, &[]),
            Action::Ask => (&self.ask, &self.project_ask),
            Action::Deny => (&self.deny, &self.project_deny),
        };
        // Allow rules for bash need an exact prefix match on a plain command;
        // ask/deny rules also catch the prefix inside chains and wrappers.
        let strict = action == Action::Allow;
        if let Some(r) = user.iter().find(|p| matches(p, tool, args, strict)) {
            return Some(format!("{} {r}", action.as_str()));
        }
        project
            .iter()
            .find(|p| matches(p, tool, args, strict))
            .map(|r| format!("{} {r} (project .nyq/permissions.json)", action.as_str()))
    }

    /// Decide a call. `needs_approval` is the tool's own default.
    pub fn decide(
        &self,
        session: &HashSet<String>,
        tool: &str,
        args: &Value,
        cwd: &Path,
        needs_approval: bool,
        yolo: bool,
    ) -> Decision {
        if let Some(rule) = self.find(Action::Deny, tool, args) {
            return Decision::Deny(rule);
        }
        if let Some(rule) = self.find(Action::Ask, tool, args) {
            return if yolo { Decision::Run } else { Decision::Ask { forced: Some(rule) } };
        }
        if self.find(Action::Allow, tool, args).is_some() && allow_applies(tool, args, cwd) {
            return Decision::Run;
        }
        if !needs_approval || yolo || crate::agent::is_allowed(session, tool, args, cwd) {
            return Decision::Run;
        }
        Decision::Ask { forced: None }
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
        let specific = [&self.allow, &self.ask, &self.deny, &self.project_ask, &self.project_deny]
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
                parse(&text).map_err(|e| anyhow::anyhow!("{} is malformed ({e}); fix it first", self.path.display()))?;
                serde_json::from_str::<Value>(&text)?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => serde_json::json!({}),
            Err(e) => return Err(e).with_context(|| format!("reading {}", self.path.display())),
        };
        let map = doc.as_object_mut().context("permissions file is not a JSON object")?;
        for key in ["allow", "ask", "deny"] {
            map.entry(key).or_insert_with(|| Value::Array(Vec::new()));
        }
        let allow = map.get_mut("allow").and_then(Value::as_array_mut).context("\"allow\" is not a list")?;
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

    /// Rows for `/permissions`.
    pub fn lines(&self, session: &HashSet<String>) -> Vec<String> {
        let list = |rules: &[String]| if rules.is_empty() { "(none)".to_string() } else { rules.join("  ") };
        let mut out = vec![
            format!("Rules file   {}", if self.path.as_os_str().is_empty() { "(none)".into() } else { self.path.display().to_string() }),
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
            None => out.push("Project file (none; .nyq/permissions.json may add ask/deny rules)".into()),
        }
        let mut grants: Vec<&str> = session.iter().map(String::as_str).collect();
        grants.sort_unstable();
        out.push(format!("Session      {}", if grants.is_empty() { "(no grants)".into() } else { grants.join("  ") }));
        out.push("Precedence: deny > ask > allow > session grants > default. /permissions reload re-reads the files.".into());
        out
    }
}

/// Whether a persistent allow may skip the prompt for this call: never for
/// file writes outside the workspace or under `.git`, nor for reads (which
/// only ask outside the workspace).
fn allow_applies(tool: &str, args: &Value, cwd: &Path) -> bool {
    if is_file_tool(tool) {
        let path = args.get("path").and_then(Value::as_str).unwrap_or("");
        return crate::agent::file_grant_ok(path, cwd);
    }
    !READ_TOOLS.contains(&tool)
}

/// Whether the session's "always" rule for this call may also be saved.
pub fn saveable(tool: &str, args: &Value, cwd: &Path) -> bool {
    allow_applies(tool, args, cwd)
}

/// Names a call answers to: the tool, plus `subagent:<mode>` or `web_fetch:<url>`.
fn call_keys(tool: &str, args: &Value) -> Vec<String> {
    let mut keys = vec![tool.to_string()];
    match tool {
        "subagent" => keys.push(format!("subagent:{}", subagent_mode(args))),
        "web_fetch" => {
            if let Some(url) = args.get("url").and_then(Value::as_str) {
                keys.push(format!("web_fetch:{url}"));
            }
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
        return if strict { bash_matches(prefix, cmd) } else { bash_mentions(prefix, cmd) };
    }
    call_keys(tool, args).iter().any(|k| glob(pattern, k))
}

/// Whether the words of `prefix` appear anywhere in `cmd`, after dropping
/// quotes and backslashes and splitting on operators: so `bash:rm` catches
/// `ls; rm x`, `sh -c 'rm x'` and `/bin/rm x`. Best effort (variables and
/// aliases can still hide a command); the sandbox is the real boundary.
pub fn bash_mentions(prefix: &str, cmd: &str) -> bool {
    let want: Vec<&str> = prefix.split_whitespace().collect();
    let Some(first) = want.first() else { return false };
    let cleaned: String = cmd.chars().filter(|c| !matches!(c, '\'' | '"' | '\\')).collect();
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn perms(allow: &[&str], ask: &[&str], deny: &[&str]) -> Permissions {
        let v = |r: &[&str]| r.iter().map(|s| s.to_string()).collect();
        Permissions { allow: v(allow), ask: v(ask), deny: v(deny), ..Default::default() }
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
        assert!(glob("web_fetch:https://docs.rs/*", "web_fetch:https://docs.rs/serde"));
    }

    #[test]
    fn pattern_kinds() {
        let sub = |mode: &str| json!({"task": "x", "mode": mode});
        assert!(matches("subagent:general", "subagent", &sub("general"), false));
        assert!(!matches("subagent:general", "subagent", &sub("explore"), false));
        assert!(matches("subagent:explore", "subagent", &json!({"task": "x"}), false));
        assert!(matches("subagent", "subagent", &sub("general"), false));
        assert!(matches("subagent:*", "subagent", &sub("general"), false));
        assert!(matches("web_fetch", "web_fetch", &json!({"url": "https://a"}), false));
        assert!(!matches("web_fetch", "web_search", &json!({}), false));
        let bash = |c: &str| json!({ "command": c });
        assert!(matches("bash:cargo test", "bash", &bash("cargo test --lib"), true));
        assert!(!matches("bash:cargo test", "bash", &bash("cargo test && rm x"), true));
        assert!(!matches("bash:cargo test", "write_file", &json!({}), true));
        for c in ["rm -rf x", "ls; rm x", "ls && /bin/rm x", "sh -c 'rm x'", "echo $(rm x)", "r\\m x", "'r'm x"] {
            assert!(matches("bash:rm", "bash", &bash(c), false), "{c}");
        }
        assert!(matches("bash:git push", "bash", &bash("git status && git push --force"), false));
        assert!(!matches("bash:git push", "bash", &bash("git pull"), false));
        assert!(!matches("bash:rm", "bash", &bash("rmdir x"), false));
    }

    #[test]
    fn parsing_and_malformed_files() {
        let r = parse(r#"{"allow": ["web_fetch", " mcp__a__* "], "deny": ["bash:rm"], "other": 1}"#).unwrap();
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
        std::fs::create_dir_all(home.path().join("nyq")).unwrap();
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
        assert!(warnings.iter().any(|w| w.contains("may only add")), "{warnings:?}");
        assert!(p.allow.is_empty());
        assert_eq!(p.project_ask, ["web_fetch"]);
        assert_eq!(p.project_deny, ["mcp__evil__*"]);
        let none = HashSet::new();
        let d = p.decide(&none, "mcp__good__x", &json!({}), cwd.path(), true, false);
        assert_eq!(d, Decision::Ask { forced: None });
        let d = p.decide(&none, "mcp__evil__x", &json!({}), cwd.path(), true, true);
        assert!(matches!(d, Decision::Deny(r) if r.contains("project")));
        let d = p.decide(&none, "web_fetch", &json!({"url": "https://a"}), cwd.path(), false, false);
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
        for yolo in [false, true] {
            // deny beats everything, even yolo.
            let p = perms(&["mcp__*"], &["mcp__*"], &["mcp__wiki__*"]);
            assert!(matches!(p.decide(&session, tool, &args, cwd, true, yolo), Decision::Deny(_)));
            // ask beats allow and a session grant; yolo skips it.
            let p = perms(&["mcp__*"], &["mcp__wiki__*"], &[]);
            let d = p.decide(&session, tool, &args, cwd, true, yolo);
            if yolo { assert_eq!(d, Decision::Run) } else { assert!(matches!(d, Decision::Ask { forced: Some(_) })) }
            // allow runs without asking.
            let p = perms(&["mcp__wiki__*"], &[], &[]);
            assert_eq!(p.decide(&none, tool, &args, cwd, true, yolo), Decision::Run);
            // session grant.
            let p = perms(&[], &[], &[]);
            assert_eq!(p.decide(&session, tool, &args, cwd, true, yolo), Decision::Run);
            // default: asks unless yolo; free tools run.
            let d = p.decide(&none, tool, &args, cwd, true, yolo);
            assert_eq!(d, if yolo { Decision::Run } else { Decision::Ask { forced: None } });
            assert_eq!(p.decide(&none, "subagent", &json!({"task": "x"}), cwd, false, yolo), Decision::Run);
            // deny applies to tools that never ask.
            let p = perms(&[], &[], &["subagent:explore"]);
            assert!(matches!(p.decide(&none, "subagent", &json!({"task": "x"}), cwd, false, yolo), Decision::Deny(_)));
        }
    }

    #[test]
    fn allow_keeps_workspace_protections() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        let p = perms(&["write_file", "read_file", "*"], &[], &[]);
        let none = HashSet::new();
        let d = |tool: &str, path: &str| p.decide(&none, tool, &json!({"path": path}), cwd, true, false);
        assert_eq!(d("write_file", "src/a.rs"), Decision::Run);
        assert_eq!(d("write_file", ".git/hooks/pre-commit"), Decision::Ask { forced: None });
        assert_eq!(d("write_file", "../outside"), Decision::Ask { forced: None });
        assert_eq!(d("read_file", "/etc/hosts"), Decision::Ask { forced: None });
        assert!(!saveable("write_file", &json!({"path": ".git/config"}), cwd));
        assert!(!saveable("read_file", &json!({"path": "/etc/hosts"}), cwd));
        assert!(saveable("write_file", &json!({"path": "a.txt"}), cwd));
        assert!(saveable("bash", &json!({"command": "cargo test"}), cwd));
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
        let saved: Value = serde_json::from_str(&std::fs::read_to_string(&p.path).unwrap()).unwrap();
        assert_eq!(saved["note"], "mine");
        assert_eq!(saved["deny"], json!(["bash:rm"]));
        assert_eq!(saved["allow"], json!(["web_fetch"]));
        std::fs::write(&p.path, "{oops").unwrap();
        assert!(p.save_allow("x").is_err());
        assert_eq!(std::fs::read_to_string(&p.path).unwrap(), "{oops");
        assert!(Permissions::default().save_allow("x").is_err());
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
}
