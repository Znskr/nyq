//! Named subagents: their saved state (one JSON file per subagent next to the
//! session, `<session id>.agents/<name>.json`, so resuming a session restores
//! them), the live registry the TUI draws progress lines from, and `/agents`.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Instant;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::types::{Item, Usage};

pub const MAX_NAME: usize = 40;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Working on a message (a stale `running` on disk reads as interrupted).
    Running,
    /// Finished its last message; ready for follow-ups.
    Idle,
    /// Its last message was cancelled, timed out or failed part-way.
    Interrupted,
    /// Stopped by the user or the model; `resume` reopens it.
    Stopped,
}

impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Status::Running => "running",
            Status::Idle => "idle",
            Status::Interrupted => "interrupted",
            Status::Stopped => "stopped",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Record {
    pub name: String,
    /// "explore" or "general".
    pub mode: String,
    #[serde(default)]
    pub description: String,
    pub created: DateTime<Utc>,
    pub updated: DateTime<Utc>,
    pub status: Status,
    /// Messages it has worked on.
    #[serde(default)]
    pub turns: u32,
    #[serde(default)]
    pub tool_calls: u64,
    #[serde(default)]
    pub usage: Usage,
    #[serde(default)]
    pub items: Vec<Item>,
}

impl Record {
    pub fn new(name: &str, mode: &str, description: &str) -> Self {
        let now = Utc::now();
        Self {
            name: name.into(),
            mode: mode.into(),
            description: description.into(),
            created: now,
            updated: now,
            status: Status::Idle,
            turns: 0,
            tool_calls: 0,
            usage: Usage::default(),
            items: Vec::new(),
        }
    }

    /// The status, with a `running` left behind by a dead process as interrupted.
    pub fn effective_status(&self, dir: &Path) -> Status {
        match self.status {
            Status::Running if !is_live(&key(dir, &self.name)) => Status::Interrupted,
            s => s,
        }
    }

    /// The last non-empty answer.
    pub fn last_answer(&self) -> Option<&str> {
        self.items.iter().rev().find_map(|i| match i {
            Item::Assistant { text } if !text.trim().is_empty() => Some(text.trim()),
            _ => None,
        })
    }
}

/// Where a session's subagents are kept.
pub fn store_dir(session_path: &Path) -> PathBuf {
    session_path.with_extension("agents")
}

/// Letters, digits, `-` and `_`; it is also the file name.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= MAX_NAME && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn path(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{name}.json"))
}

pub fn load(dir: &Path, name: &str) -> Result<Option<Record>> {
    if !valid_name(name) {
        bail!("invalid subagent name `{name}` (use letters, digits, '-' and '_', at most {MAX_NAME})");
    }
    let p = path(dir, name);
    match std::fs::read(&p) {
        Ok(data) => Ok(Some(serde_json::from_slice(&data).with_context(|| format!("reading {}", p.display()))?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", p.display())),
    }
}

pub fn save(dir: &Path, record: &Record) -> Result<()> {
    let p = path(dir, &record.name);
    let data = serde_json::to_vec(record)?;
    crate::session::write_private(&p, &data).with_context(|| format!("saving {}", p.display()))
}

/// Every saved subagent, oldest first.
pub fn list(dir: &Path) -> Vec<Record> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut all: Vec<Record> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .filter_map(|p| serde_json::from_slice(&std::fs::read(p).ok()?).ok())
        .collect();
    all.sort_by_key(|r: &Record| r.created);
    all
}

pub fn delete(dir: &Path, name: &str) -> Result<()> {
    if load(dir, name)?.is_none() {
        bail!("No subagent named `{name}` in this session.");
    }
    if is_live(&key(dir, name)) {
        bail!("`{name}` is working; stop it first.");
    }
    std::fs::remove_file(path(dir, name)).with_context(|| format!("deleting subagent {name}"))
}

/// Mark a subagent stopped, cancelling it if it is working.
pub fn stop(dir: &Path, name: &str) -> Result<String> {
    let Some(mut record) = load(dir, name)? else {
        bail!("No subagent named `{name}` in this session.");
    };
    let was_live = cancel_live(&key(dir, name));
    if !was_live {
        record.status = Status::Stopped;
        record.updated = Utc::now();
        save(dir, &record)?;
    }
    Ok(if was_live {
        format!("Stopping `{name}`; its current work is cancelled.")
    } else {
        format!("Stopped `{name}`. Resume it to send it more messages.")
    })
}

// ------------------------------------------------------------ live registry

struct Live {
    id: u64,
    /// `<store dir>#<name>` for named subagents.
    key: Option<String>,
    label: String,
    started: Instant,
    calls: usize,
    last: String,
    cancel: CancellationToken,
    stopped: bool,
}

static LIVE: Mutex<Vec<Live>> = Mutex::new(Vec::new());
static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn live() -> std::sync::MutexGuard<'static, Vec<Live>> {
    LIVE.lock().unwrap_or_else(|e| e.into_inner())
}

pub fn key(dir: &Path, name: &str) -> String {
    format!("{}#{name}", dir.display())
}

pub fn is_live(key: &str) -> bool {
    live().iter().any(|l| l.key.as_deref() == Some(key))
}

fn cancel_live(key: &str) -> bool {
    let mut all = live();
    let mut hit = false;
    for l in all.iter_mut().filter(|l| l.key.as_deref() == Some(key)) {
        l.stopped = true;
        l.cancel.cancel();
        hit = true;
    }
    hit
}

/// A running subagent's entry in the live registry; removed on drop.
pub struct LiveGuard(u64);

impl LiveGuard {
    /// Register a running subagent; None when `key` is already running.
    pub fn start(key: Option<String>, label: &str, cancel: CancellationToken) -> Option<Self> {
        let mut all = live();
        if key.is_some() && all.iter().any(|l| l.key == key) {
            return None;
        }
        let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        all.push(Live { id, key, label: label.into(), started: Instant::now(), calls: 0, last: String::new(), cancel, stopped: false });
        Some(Self(id))
    }

    pub fn tool(&self, tool: &str, summary: &str) {
        if let Some(l) = live().iter_mut().find(|l| l.id == self.0) {
            l.calls += 1;
            l.last = format!("{tool} {summary}").trim().to_string();
        }
    }

    /// Whether `stop` cancelled it.
    pub fn stopped(&self) -> bool {
        live().iter().any(|l| l.id == self.0 && l.stopped)
    }
}

impl Drop for LiveGuard {
    fn drop(&mut self) {
        live().retain(|l| l.id != self.0);
    }
}

/// One compact line per running subagent, for the TUI's status area.
pub fn live_lines() -> Vec<String> {
    live()
        .iter()
        .map(|l| {
            let calls = match l.calls {
                0 => "starting".to_string(),
                1 => "1 tool call".to_string(),
                n => format!("{n} tool calls"),
            };
            let last = if l.last.is_empty() { String::new() } else { format!(" · {}", l.last) };
            format!("{} · {calls}{last} ({}s)", l.label, l.started.elapsed().as_secs())
        })
        .collect()
}

// ------------------------------------------------------------ /agents

pub const USAGE: &str = "Usage: /agents [list|show NAME|stop NAME|delete NAME]";

fn ago(t: DateTime<Utc>) -> String {
    let s = (Utc::now() - t).num_seconds().max(0);
    match s {
        0..60 => format!("{s}s ago"),
        60..3600 => format!("{}m ago", s / 60),
        3600..86400 => format!("{}h ago", s / 3600),
        _ => format!("{}d ago", s / 86400),
    }
}

fn flat(s: &str, max: usize) -> String {
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.chars().count() > max { format!("{}…", s.chars().take(max - 1).collect::<String>()) } else { s }
}

pub fn list_lines(dir: &Path) -> Vec<String> {
    let all = list(dir);
    if all.is_empty() {
        return vec!["No named subagents in this session (the model creates them with subagent `name`).".into()];
    }
    let w = all.iter().map(|r| r.name.len()).max().unwrap_or(4).max(4);
    let mut lines = vec![format!("{:w$}  {:7}  {:11}  {:>5}  {:>5}  {:8}  description", "name", "mode", "status", "turns", "tools", "updated")];
    for r in &all {
        lines.push(
            format!(
                "{:w$}  {:7}  {:11}  {:>5}  {:>5}  {:8}  {}",
                r.name,
                r.mode,
                r.effective_status(dir).label(),
                r.turns,
                r.tool_calls,
                ago(r.updated),
                flat(&r.description, 60)
            )
            .trim_end()
            .to_string(),
        );
    }
    lines
}

/// A subagent's details and a digest of its transcript.
pub fn show_lines(dir: &Path, name: &str, max_items: usize) -> Result<Vec<String>> {
    let Some(r) = load(dir, name)? else {
        bail!("No subagent named `{name}` in this session.");
    };
    let mut lines = vec![
        format!("{} · {} · {}", r.name, r.mode, r.effective_status(dir).label()),
        format!(
            "  {} turns · {} tool calls · ↑{} ↓{} tokens · created {} · updated {}",
            r.turns,
            r.tool_calls,
            r.usage.input_tokens,
            r.usage.output_tokens,
            ago(r.created),
            ago(r.updated)
        ),
    ];
    if !r.description.is_empty() {
        lines.push(format!("  {}", flat(&r.description, 200)));
    }
    let digest: Vec<String> = r
        .items
        .iter()
        .filter_map(|i| match i {
            Item::User { text, .. } => Some(format!("  ▸ {}", flat(text, 160))),
            Item::Assistant { text } if !text.trim().is_empty() => Some(format!("  ◂ {}", flat(text, 160))),
            Item::ToolCall { name, arguments, .. } => Some(format!("    {name} {}", flat(arguments, 100))),
            Item::Compaction { .. } => Some("  (earlier messages compacted)".into()),
            _ => None,
        })
        .collect();
    if digest.len() > max_items {
        lines.push(format!("  … {} earlier entries", digest.len() - max_items));
    }
    lines.extend(digest.into_iter().rev().take(max_items).collect::<Vec<_>>().into_iter().rev());
    Ok(lines)
}

/// `/agents ARGS` for the session stored at `session_path`.
pub fn command(session_path: Option<&Path>, args: &[&str]) -> Result<Vec<String>> {
    let Some(session_path) = session_path else {
        bail!("No session: subagents are kept with a saved session.");
    };
    let dir = store_dir(session_path);
    Ok(match args {
        [] | ["list"] => list_lines(&dir),
        ["show" | "inspect", name] => show_lines(&dir, name, 30)?,
        ["stop", name] => vec![stop(&dir, name)?],
        ["delete" | "remove", name] => {
            delete(&dir, name)?;
            vec![format!("Deleted subagent `{name}`.")]
        }
        _ => vec![USAGE.into()],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_round_trip_and_commands() {
        let t = tempfile::tempdir().unwrap();
        let session = t.path().join("sessions/abc.jsonl");
        let dir = store_dir(&session);
        assert_eq!(dir, t.path().join("sessions/abc.agents"));
        assert!(command(Some(&session), &[]).unwrap()[0].starts_with("No named subagents"));

        let mut r = Record::new("scout", "explore", "find the parser");
        r.items = vec![Item::user("look"), Item::Assistant { text: "found it".into() }];
        r.turns = 1;
        save(&dir, &r).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join("scout.json")).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let back = load(&dir, "scout").unwrap().unwrap();
        assert_eq!(back.last_answer(), Some("found it"));
        assert!(load(&dir, "../x").is_err());
        assert!(load(&dir, "ghost").unwrap().is_none());

        let lines = command(Some(&session), &["list"]).unwrap();
        assert!(lines[1].starts_with("scout") && lines[1].contains("idle"), "{lines:?}");
        let lines = command(Some(&session), &["show", "scout"]).unwrap();
        assert!(lines.iter().any(|l| l.contains("▸ look")) && lines.iter().any(|l| l.contains("◂ found it")), "{lines:?}");

        // A `running` record from a dead process reads as interrupted.
        let mut stale = back.clone();
        stale.status = Status::Running;
        save(&dir, &stale).unwrap();
        assert_eq!(load(&dir, "scout").unwrap().unwrap().effective_status(&dir), Status::Interrupted);

        assert!(command(Some(&session), &["stop", "scout"]).unwrap()[0].starts_with("Stopped"));
        assert_eq!(load(&dir, "scout").unwrap().unwrap().status, Status::Stopped);
        assert!(command(Some(&session), &["stop", "ghost"]).is_err());
        command(Some(&session), &["delete", "scout"]).unwrap();
        assert!(list(&dir).is_empty());
        assert_eq!(command(Some(&session), &["what"]).unwrap(), [USAGE]);
        assert!(command(None, &[]).is_err());
    }

    #[test]
    fn live_registry_tracks_and_stops() {
        let cancel = CancellationToken::new();
        let k = key(Path::new("/tmp/live-test.agents"), "w");
        let guard = LiveGuard::start(Some(k.clone()), "w: task", cancel.clone()).unwrap();
        assert!(LiveGuard::start(Some(k.clone()), "again", CancellationToken::new()).is_none());
        guard.tool("read_file", "src/a.rs");
        assert!(live_lines().iter().any(|l| l.starts_with("w: task · 1 tool call · read_file src/a.rs")));
        assert!(cancel_live(&k));
        assert!(cancel.is_cancelled() && guard.stopped());
        drop(guard);
        assert!(!is_live(&k));
    }
}
