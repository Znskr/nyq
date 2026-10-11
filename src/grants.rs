//! Session grants and rules, and the allowlist's scopes.
//!
//! - Session grants ("always" answers) and exact-call rules
//!   (`/permissions remember allow|deny TOOL ARGS-JSON`) are saved with the
//!   session and come back on resume.
//! - The allowlist has a user scope (`allow` in `<home>/permissions.json`)
//!   and a local scope for one workspace, kept in the same user file under
//!   `workspaces.<path>.allow`, so a repository can never grant itself.

use std::collections::HashSet;
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::agent::Agent;
use crate::permissions::{self, Permissions, TrustKind};
use crate::ui::{self, tui::Tui};

/// Allow or deny one exact call (tool name and arguments) for this session.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionRule {
    pub id: u32,
    pub allow: bool,
    pub tool: String,
    pub args: Value,
}

impl SessionRule {
    pub fn matches(&self, tool: &str, args: &Value) -> bool {
        self.tool == tool && &self.args == args
    }

    fn line(&self) -> String {
        format!("#{} {} {} {}", self.id, if self.allow { "allow" } else { "deny" }, self.tool, self.args)
    }
}

/// What a session file keeps.
#[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
struct Saved {
    #[serde(default)]
    grants: Vec<String>,
    #[serde(default)]
    rules: Vec<SessionRule>,
}

/// Save the session's grants and rules when they changed.
pub fn persist(agent: &mut Agent) {
    let mut grants: Vec<String> = agent.allowed.iter().cloned().collect();
    grants.sort();
    let saved = Saved { grants, rules: agent.permissions.session_rules.clone() };
    let Some(session) = agent.session.as_mut() else { return };
    if saved == Saved::default() && session.saved_permissions().is_none() {
        return;
    }
    if let Ok(state) = serde_json::to_value(&saved) {
        if let Err(e) = session.save_permissions(state) {
            ui::error(&format!("could not save session permissions: {e:#}"));
        }
    }
}

/// Take the grants and rules saved with the (just resumed) session.
pub fn restore(agent: &mut Agent) {
    let saved: Saved = agent
        .session
        .as_ref()
        .and_then(|s| s.saved_permissions())
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();
    agent.allowed = saved.grants.into_iter().collect();
    std::sync::Arc::make_mut(&mut agent.permissions).session_rules = saved.rules;
}

// ------------------------------------------------------------ local scope

/// The key a workspace's local rules are saved under.
pub fn workspace_key(cwd: &Path) -> String {
    std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf()).to_string_lossy().into_owned()
}

/// Local allow rules for `key` in the user file (none when unreadable).
pub fn read_local(path: &Path, key: &str) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(path) else { return Vec::new() };
    let Ok(doc) = serde_json::from_str::<Value>(&text) else { return Vec::new() };
    doc["workspaces"][key]["allow"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default()
}

/// Rewrite this workspace's local allow list in the user file, keeping
/// everything else. A malformed file is left alone.
fn write_local(perms: &mut Permissions, list: Vec<String>) -> Result<()> {
    if perms.path.as_os_str().is_empty() || perms.workspace.is_empty() {
        bail!("no permissions file in this context");
    }
    let mut doc = match std::fs::read_to_string(&perms.path) {
        Ok(text) => serde_json::from_str::<Value>(&text)
            .ok()
            .filter(Value::is_object)
            .with_context(|| format!("{} is malformed; fix it first", perms.path.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(e) => return Err(e).with_context(|| format!("reading {}", perms.path.display())),
    };
    let map = doc.as_object_mut().expect("checked above");
    let workspaces = map.entry("workspaces").or_insert_with(|| json!({}));
    let workspaces = workspaces.as_object_mut().context("\"workspaces\" is not an object")?;
    if list.is_empty() {
        workspaces.remove(&perms.workspace);
    } else {
        workspaces.insert(perms.workspace.clone(), json!({ "allow": list }));
    }
    if workspaces.is_empty() {
        map.remove("workspaces");
    }
    let mut text = serde_json::to_string_pretty(&doc)?;
    text.push('\n');
    crate::session::write_private(&perms.path, text.as_bytes())
        .with_context(|| format!("writing {}", perms.path.display()))?;
    perms.local = list;
    Ok(())
}

/// Allow `rule` in this workspace only.
pub fn save_local(perms: &mut Permissions, rule: &str) -> Result<()> {
    let mut list = read_local(&perms.path, &perms.workspace);
    if !list.iter().any(|r| r == rule) {
        list.push(rule.to_string());
    }
    write_local(perms, list)
}

/// Drop `rule` from this workspace's list; whether it was there.
pub fn remove_local(perms: &mut Permissions, rule: &str) -> Result<bool> {
    let had = perms.local.iter().any(|r| r == rule);
    if !had || perms.workspace.is_empty() {
        perms.local.retain(|r| r != rule);
        return Ok(had);
    }
    let list = perms.local.iter().filter(|r| *r != rule).cloned().collect();
    write_local(perms, list)?;
    Ok(true)
}

// ------------------------------------------------------------- /allowlist

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scope {
    User,
    Local,
}

const ALLOWLIST_USAGE: &str = "usage: /allowlist [view [effective|local|user]] | [local|user] add command|tool|url PATTERN | \
                               [local|user] remove N|KIND PATTERN | [local|user] reset [commands|tools|urls|all]";

fn kind_filter(arg: Option<&str>) -> Result<Option<TrustKind>, String> {
    match arg {
        None | Some("all") => Ok(None),
        Some(k) => TrustKind::parse(k).map(Some).ok_or_else(|| ALLOWLIST_USAGE.to_string()),
    }
}

fn scoped_lines(title: &str, rules: &[String]) -> Vec<String> {
    let mut out = vec![title.to_string()];
    if rules.is_empty() {
        out.push("  (none)".into());
    }
    out.extend(rules.iter().map(|r| format!("  {r}")));
    out
}

/// Run `/allowlist ARGS`; the lines to show.
pub fn allowlist(perms: &mut Permissions, session: &mut HashSet<String>, args: &[&str]) -> Result<Vec<String>, String> {
    let (scope, rest) = match args.first() {
        Some(&"local") => (Some(Scope::Local), &args[1..]),
        Some(&"user") => (Some(Scope::User), &args[1..]),
        _ => (None, args),
    };
    match (scope, rest) {
        (None, [] | ["view"] | ["view", "effective"]) => Ok(permissions::allowlist_lines(perms, session)),
        (None, ["view", "local"]) | (Some(Scope::Local), []) => {
            Ok(scoped_lines(&format!("This workspace ({})", perms.workspace), &perms.local))
        }
        (None, ["view", "user"]) | (Some(Scope::User), []) => {
            Ok(scoped_lines(&format!("Every workspace ({})", perms.path.display()), &perms.allow))
        }
        (_, ["add", kind, pattern @ ..]) if !pattern.is_empty() => {
            let kind = TrustKind::parse(kind).ok_or_else(|| ALLOWLIST_USAGE.to_string())?;
            let rule = permissions::canonical_rule(kind, &pattern.join(" "));
            let result = match scope.unwrap_or(Scope::User) {
                Scope::User => perms.save_allow(&rule).map(|_| "permissions.json"),
                Scope::Local => save_local(perms, &rule).map(|_| "this workspace"),
            };
            let place = result.map_err(|e| format!("{e:#}"))?;
            Ok(vec![format!("Trusted `{rule}` ({place}).")])
        }
        (None, ["remove", rest @ ..]) => permissions::allowlist_remove(perms, session, rest).map(|m| vec![m]),
        (Some(scope), ["remove", kind, pattern @ ..]) if !pattern.is_empty() => {
            let kind = TrustKind::parse(kind).ok_or_else(|| ALLOWLIST_USAGE.to_string())?;
            let rule = permissions::canonical_rule(kind, &pattern.join(" "));
            let removed = match scope {
                Scope::User => perms.remove_allow(&rule),
                Scope::Local => remove_local(perms, &rule),
            }
            .map_err(|e| format!("{e:#}"))?;
            if removed {
                Ok(vec![format!("Removed `{rule}`.")])
            } else {
                Err(format!("`{rule}` is not in that list"))
            }
        }
        (_, ["reset", kind @ ..]) if kind.len() <= 1 => {
            let only = kind_filter(kind.first().copied())?;
            let hit = |r: &String| only.is_none_or(|k| permissions::trust_kind(r) == k);
            let mut n = 0;
            if scope.is_none() {
                let before = session.len();
                session.retain(|r| !hit(r));
                n += before - session.len();
            }
            if scope != Some(Scope::Local) {
                for rule in perms.allow.clone().iter().filter(|r| hit(r)) {
                    n += usize::from(perms.remove_allow(rule).map_err(|e| format!("{e:#}"))?);
                }
            }
            if scope != Some(Scope::User) {
                let keep: Vec<String> = perms.local.iter().filter(|r| !hit(r)).cloned().collect();
                if keep.len() != perms.local.len() {
                    n += perms.local.len() - keep.len();
                    write_local(perms, keep).map_err(|e| format!("{e:#}"))?;
                }
            }
            Ok(vec![format!("Forgot {n} trusted entr{}.", if n == 1 { "y" } else { "ies" })])
        }
        _ => Err(ALLOWLIST_USAGE.into()),
    }
}

// ---------------------------------------------- /permissions remember|revoke

/// `/permissions remember allow|deny TOOL ARGS-JSON`, after a confirmation.
pub async fn remember(agent: &mut Agent, tui: &Tui, args: &[&str]) -> Result<()> {
    let usage = "usage: /permissions remember allow|deny TOOL ARGS-JSON, e.g. remember allow bash {\"command\":\"make\"}";
    let [decision, tool, json @ ..] = args else { bail!(usage) };
    let allow = match *decision {
        "allow" => true,
        "deny" => false,
        _ => bail!(usage),
    };
    anyhow::ensure!(!json.is_empty(), usage);
    let args: Value = serde_json::from_str(&json.join(" ")).map_err(|e| anyhow::anyhow!("ARGS-JSON is not valid JSON ({e}); {usage}"))?;
    anyhow::ensure!(args.is_object(), "ARGS-JSON must be a JSON object; {usage}");
    let rules = &agent.permissions.session_rules;
    let id = rules.iter().map(|r| r.id).max().unwrap_or(0) + 1;
    let rule = SessionRule { id, allow, tool: tool.to_string(), args };
    let options = vec!["Remember".to_string(), "Cancel".to_string()];
    let prompt = format!("{} exactly this call for the rest of this session: {} {}", if allow { "Allow" } else { "Deny" }, rule.tool, rule.args);
    if tui.pick("Remember a permission rule", &prompt, &options).await != Some(0) {
        ui::info("  Nothing remembered.");
        return Ok(());
    }
    let perms = std::sync::Arc::make_mut(&mut agent.permissions);
    perms.session_rules.retain(|r| !r.matches(&rule.tool, &rule.args));
    ui::info(&format!("  Remembered {} (saved with this session; /permissions revoke {id} drops it).", rule.line()));
    perms.session_rules.push(rule);
    persist(agent);
    Ok(())
}

/// `/permissions revoke ID`.
pub fn revoke(agent: &mut Agent, id: &str) -> Result<()> {
    let id: u32 = id.trim_start_matches('#').parse().context("usage: /permissions revoke ID")?;
    let perms = std::sync::Arc::make_mut(&mut agent.permissions);
    let before = perms.session_rules.len();
    perms.session_rules.retain(|r| r.id != id);
    anyhow::ensure!(perms.session_rules.len() < before, "no session rule #{id}");
    ui::info(&format!("  Revoked session rule #{id}."));
    persist(agent);
    Ok(())
}

/// Rows for `/permissions rules`.
pub fn rule_lines(perms: &Permissions) -> Vec<String> {
    if perms.session_rules.is_empty() {
        return vec!["Session rules (none; /permissions remember allow|deny TOOL ARGS-JSON adds one)".into()];
    }
    let mut out = vec!["Session rules".to_string()];
    out.extend(perms.session_rules.iter().map(|r| format!("  {}", r.line())));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn perms(home: &Path, cwd: &Path) -> Permissions {
        Permissions::load(home, cwd).0
    }

    #[test]
    fn local_rules_stay_with_their_workspace() {
        let home = tempfile::tempdir().unwrap();
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let mut p = perms(home.path(), a.path());
        let mut session = HashSet::new();
        let out = allowlist(&mut p, &mut session, &["local", "add", "command", "make", "test"]).unwrap();
        assert_eq!(out, ["Trusted `bash:make test` (this workspace)."]);
        allowlist(&mut p, &mut session, &["add", "tool", "mcp__wiki__ask"]).unwrap();
        let bash = json!({"command": "make test"});
        let decide = |p: &Permissions| p.decide(&HashSet::new(), "bash", &bash, a.path(), true, permissions::Mode::Ask);
        assert_eq!(decide(&p), permissions::Decision::Run);
        let reloaded = perms(home.path(), a.path());
        assert_eq!(reloaded.local, ["bash:make test"]);
        assert_eq!(reloaded.allow, ["mcp__wiki__ask"]);
        assert!(perms(home.path(), b.path()).local.is_empty(), "other workspaces don't see it");
        let lines = permissions::allowlist_lines(&reloaded, &session);
        assert!(lines.iter().any(|l| l.contains("make test  (this workspace)")), "{lines:?}");
        assert_eq!(allowlist(&mut p, &mut session, &["view", "local"]).unwrap()[1], "  bash:make test");

        session.insert("bash:ls".into());
        let out = allowlist(&mut p, &mut session, &["reset", "commands"]).unwrap();
        assert_eq!(out, ["Forgot 2 trusted entries."]);
        assert!(session.is_empty() && p.local.is_empty());
        assert_eq!(p.allow, ["mcp__wiki__ask"]);
        assert!(perms(home.path(), a.path()).local.is_empty());
        let saved: Value = serde_json::from_str(&std::fs::read_to_string(&p.path).unwrap()).unwrap();
        assert!(saved.get("workspaces").is_none(), "{saved}");
        allowlist(&mut p, &mut session, &["user", "reset"]).unwrap();
        assert!(p.allow.is_empty());
        assert!(allowlist(&mut p, &mut session, &["add", "nope", "x"]).is_err());
        assert!(allowlist(&mut p, &mut session, &["local", "remove", "command", "gone"]).is_err());
    }

    #[test]
    fn exact_session_rules_decide_before_grants_and_modes() {
        let mut p = Permissions::default();
        let args = json!({"command": "make deploy"});
        p.session_rules.push(SessionRule { id: 1, allow: false, tool: "bash".into(), args: args.clone() });
        let decide = |p: &Permissions, args: &Value, mode| p.decide(&HashSet::new(), "bash", args, Path::new("."), true, mode);
        assert!(matches!(decide(&p, &args, permissions::Mode::Yolo), permissions::Decision::Deny(r) if r.contains("#1")));
        assert_eq!(decide(&p, &json!({"command": "make"}), permissions::Mode::Ask), permissions::Decision::Ask { forced: None });
        p.session_rules[0].allow = true;
        assert_eq!(decide(&p, &args, permissions::Mode::Ask), permissions::Decision::Run);
    }

    #[test]
    fn grants_and_rules_come_back_with_the_session() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        let mut s = crate::session::Session::create(&sessions, dir.path(), "m").unwrap();
        s.append(&crate::types::Item::user("hi")).unwrap();
        let saved = Saved {
            grants: vec!["bash:cargo test".into()],
            rules: vec![SessionRule { id: 1, allow: true, tool: "bash".into(), args: json!({"command": "ls"}) }],
        };
        s.save_permissions(serde_json::to_value(&saved).unwrap()).unwrap();
        s.save_permissions(serde_json::to_value(&saved).unwrap()).unwrap();
        let id = s.id.clone();
        drop(s);
        let text = std::fs::read_to_string(sessions.join(format!("{id}.jsonl"))).unwrap();
        assert_eq!(text.matches("\"permissions\"").count(), 1, "unchanged state is not written twice");
        let (s, _, items) = crate::session::load(&sessions, &id, dir.path()).unwrap();
        assert_eq!(items.len(), 1, "the permissions line is not an item");
        let back: Saved = serde_json::from_value(s.saved_permissions().unwrap().clone()).unwrap();
        assert_eq!(back, saved);
    }
}
