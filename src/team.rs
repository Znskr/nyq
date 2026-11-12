//! Team task graph (design §8.2): `TaskNode`s with compare-and-set on `rev`,
//! stored in `team.json` next to `orchestration.jsonl`, guarded by a file lock.
//!
//! Every change is a read-modify-write of the whole graph under an exclusive
//! lock on `team.lock` (cross-process, and per open file so threads in one
//! process serialize too), written atomically (temp file, fsync, rename).
//! `write_scope` is advisory: overlapping scopes produce a warning, while
//! actual writes stay serialized by the per-workspace writer mutex.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Who is calling the team tools: set by the subagent runner around a
/// child's turn. Outside such a scope the caller is the parent (`parent`)
/// and the store directory comes from `ToolCtx::subagent_dir`.
#[derive(Clone, Debug)]
pub struct Member {
    /// The session's subagent store directory (holds `orchestration.jsonl`).
    pub dir: PathBuf,
    /// The child's name.
    pub name: String,
}

tokio::task_local! {
    pub static MEMBER: Member;
}

/// The caller's name outside any child scope.
pub const PARENT: &str = "parent";
pub const MAX_TASKS: usize = 1000;
const MAX_GOAL: usize = 8 * 1024;
const MAX_FILE: u64 = 32 * 1024 * 1024;
pub const STATES: &[&str] = &["open", "claimed", "done", "failed"];

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct TaskNode {
    pub id: String,
    pub goal: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<Value>,
    #[serde(default)]
    pub deps: Vec<String>,
    /// open | claimed | done | failed
    pub state: String,
    #[serde(default)]
    pub owner: Option<String>,
    #[serde(default)]
    pub write_scope: Vec<String>,
    pub rev: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default)]
    pub created_by: String,
    #[serde(default = "Utc::now")]
    pub created: DateTime<Utc>,
    #[serde(default = "Utc::now")]
    pub updated: DateTime<Utc>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Graph {
    #[serde(default = "one")]
    pub version: u8,
    /// Bumped on every committed change.
    #[serde(default)]
    pub rev: u64,
    /// The next task number (`t<n>`).
    #[serde(default)]
    pub next: u64,
    #[serde(default)]
    pub tasks: Vec<TaskNode>,
}

fn one() -> u8 {
    1
}

impl Default for Graph {
    fn default() -> Self {
        Self {
            version: 1,
            rev: 0,
            next: 0,
            tasks: Vec::new(),
        }
    }
}

impl Graph {
    pub fn get(&self, id: &str) -> Option<&TaskNode> {
        self.tasks.iter().find(|t| t.id == id)
    }

    /// Open and every dependency done.
    pub fn ready(&self, node: &TaskNode) -> bool {
        node.state == "open"
            && node
                .deps
                .iter()
                .all(|d| self.get(d).is_some_and(|t| t.state == "done"))
    }
}

fn refuse_link(path: &Path) -> Result<()> {
    if let Ok(meta) = std::fs::symlink_metadata(path) {
        if meta.file_type().is_symlink() || !meta.is_file() {
            bail!("{} must be a regular, unlinked file", path.display());
        }
    }
    Ok(())
}

fn load(path: &Path) -> Result<Graph> {
    refuse_link(path)?;
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Graph::default()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let mut data = Vec::new();
    use std::io::Read;
    file.take(MAX_FILE + 1).read_to_end(&mut data)?;
    if data.len() as u64 > MAX_FILE {
        bail!("team.json exceeds {MAX_FILE} bytes");
    }
    let graph: Graph = serde_json::from_slice(&data)
        .with_context(|| format!("corrupt {}; original preserved", path.display()))?;
    if graph.version != 1 {
        bail!("Unsupported team.json version {}", graph.version);
    }
    Ok(graph)
}

/// Run `f` on the graph under the store lock; when it returns `true` the
/// changed graph is committed (with its `rev` bumped).
pub fn with_graph<T>(dir: &Path, f: impl FnOnce(&mut Graph) -> Result<(T, bool)>) -> Result<T> {
    std::fs::create_dir_all(dir)?;
    if std::fs::symlink_metadata(dir)?.file_type().is_symlink() {
        bail!("Refusing linked team directory");
    }
    let lock_path = dir.join("team.lock");
    refuse_link(&lock_path)?;
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let lock = options.open(&lock_path)?;
    lock.lock().context("locking team.lock")?;
    let path = dir.join("team.json");
    let mut graph = load(&path)?;
    let (out, changed) = f(&mut graph)?;
    if changed {
        graph.rev += 1;
        let bytes = serde_json::to_vec_pretty(&graph)?;
        crate::session::write_private(&path, &bytes)
            .with_context(|| format!("saving {}", path.display()))?;
    }
    drop(lock);
    Ok(out)
}

pub fn list(dir: &Path) -> Result<Graph> {
    with_graph(dir, |g| Ok((std::mem::take(g), false)))
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64
}

/// The part of a scope entry before any glob character, as path components.
fn literal_prefix(scope: &str) -> Vec<&str> {
    scope
        .trim()
        .trim_start_matches("./")
        .split('/')
        .filter(|c| !c.is_empty() && *c != ".")
        .take_while(|c| !c.contains(['*', '?', '[', '{']))
        .collect()
}

/// Whether two write scopes may touch the same files: one literal prefix
/// contains the other (conservative for globs).
pub fn scopes_overlap(a: &str, b: &str) -> bool {
    let (a, b) = (literal_prefix(a), literal_prefix(b));
    let n = a.len().min(b.len());
    a[..n] == b[..n]
}

/// Warnings for `node`'s scope overlapping other claimed tasks' scopes.
fn overlap_warnings(graph: &Graph, node: &TaskNode) -> Vec<String> {
    let mut out = Vec::new();
    for other in graph
        .tasks
        .iter()
        .filter(|t| t.id != node.id && t.state == "claimed")
    {
        let hits: Vec<String> = node
            .write_scope
            .iter()
            .flat_map(|a| {
                other
                    .write_scope
                    .iter()
                    .filter(move |b| scopes_overlap(a, b))
                    .map(move |b| format!("{a} ~ {b}"))
            })
            .collect();
        if !hits.is_empty() {
            out.push(format!(
                "write_scope overlaps claimed task {} (owner {}): {}. Scopes are advisory; writes are still serialized by the per-workspace writer mutex, but coordinate to avoid clobbering each other's changes.",
                other.id,
                other.owner.as_deref().unwrap_or("?"),
                hits.join(", ")
            ));
        }
    }
    out
}

pub struct NewTask {
    pub goal: String,
    pub schema: Option<Value>,
    pub deps: Vec<String>,
    pub write_scope: Vec<String>,
}

/// Add an open task; returns it and any scope warnings.
pub fn create(dir: &Path, by: &str, new: NewTask) -> Result<(TaskNode, Vec<String>)> {
    let goal = new.goal.trim().to_string();
    if goal.is_empty() {
        bail!("goal must not be empty");
    }
    if goal.len() > MAX_GOAL {
        bail!("goal exceeds {MAX_GOAL} bytes");
    }
    if let Some(schema) = &new.schema {
        if !schema.is_object() {
            bail!("schema must be a JSON Schema object");
        }
    }
    with_graph(dir, |g| {
        if g.tasks.len() >= MAX_TASKS {
            bail!("Team task limit of {MAX_TASKS} reached");
        }
        for d in &new.deps {
            if g.get(d).is_none() {
                bail!("Unknown dependency `{d}`: create it first");
            }
        }
        g.next += 1;
        let mut id = format!("t{}", g.next);
        while g.get(&id).is_some() {
            g.next += 1;
            id = format!("t{}", g.next);
        }
        let now = Utc::now();
        let mut deps = new.deps.clone();
        deps.dedup();
        let node = TaskNode {
            id,
            goal: goal.clone(),
            schema: new.schema.clone(),
            deps,
            state: "open".into(),
            owner: None,
            write_scope: new.write_scope.clone(),
            rev: 1,
            result: None,
            created_by: by.into(),
            created: now,
            updated: now,
        };
        g.tasks.push(node.clone());
        Ok(((node, Vec::new()), true))
    })
}

fn conflict(node: &TaskNode, rev: u64) -> anyhow::Error {
    anyhow::anyhow!(
        "rev conflict on {}: you passed rev {rev}, current rev is {} (state {}, owner {}). Re-read with team_task_list and retry.",
        node.id,
        node.rev,
        node.state,
        node.owner.as_deref().unwrap_or("none")
    )
}

/// Compare-and-set claim of an open, ready task.
pub fn claim(dir: &Path, who: &str, id: &str, rev: u64) -> Result<(TaskNode, Vec<String>)> {
    if !valid_id(id) {
        bail!("Invalid task id");
    }
    with_graph(dir, |g| {
        let Some(i) = g.tasks.iter().position(|t| t.id == id) else {
            bail!("No team task `{id}`");
        };
        let node = &g.tasks[i];
        if node.rev != rev {
            return Err(conflict(node, rev));
        }
        if node.state != "open" {
            bail!(
                "Task {id} is {} (owner {}), not open",
                node.state,
                node.owner.as_deref().unwrap_or("none")
            );
        }
        if !g.ready(node) {
            let waiting: Vec<&str> = node
                .deps
                .iter()
                .filter(|d| g.get(d).is_none_or(|t| t.state != "done"))
                .map(String::as_str)
                .collect();
            bail!("Task {id} is not ready: waiting on {}", waiting.join(", "));
        }
        let node = &mut g.tasks[i];
        node.state = "claimed".into();
        node.owner = Some(who.into());
        node.rev += 1;
        node.updated = Utc::now();
        let node = node.clone();
        let warnings = overlap_warnings(g, &node);
        Ok(((node, warnings), true))
    })
}

/// Compare-and-set state change. Only the owner (or the parent) may change a
/// claimed task; `done` results are checked against the task's schema.
pub fn update(
    dir: &Path,
    who: &str,
    id: &str,
    rev: u64,
    state: &str,
    result: Option<Value>,
) -> Result<(TaskNode, Vec<String>)> {
    if !STATES.contains(&state) {
        bail!("state must be one of {}", STATES.join(", "));
    }
    with_graph(dir, |g| {
        let Some(i) = g.tasks.iter().position(|t| t.id == id) else {
            bail!("No team task `{id}`");
        };
        let node = &g.tasks[i];
        if node.rev != rev {
            return Err(conflict(node, rev));
        }
        if let Some(owner) = &node.owner {
            if owner != who && who != PARENT {
                bail!("Task {id} is owned by {owner}; only its owner or the parent may update it");
            }
        }
        if state == "claimed" && node.state == "open" && !g.ready(node) {
            bail!("Task {id} is not ready; its dependencies are not all done");
        }
        if state == "done" {
            if let Some(schema) = &node.schema {
                let Some(value) = &result else {
                    bail!("Task {id} has a schema: pass a `result` matching it");
                };
                let errors = crate::task::SchemaValidator::new(schema.clone()).errors(value);
                if !errors.is_empty() {
                    bail!(
                        "The result does not match task {id}'s schema:\n- {}",
                        errors.join("\n- ")
                    );
                }
            }
        }
        let node = &mut g.tasks[i];
        node.state = state.into();
        match state {
            "open" => node.owner = None,
            "claimed" if node.owner.is_none() => node.owner = Some(who.into()),
            _ => {}
        }
        if result.is_some() {
            node.result = result.clone();
        }
        node.rev += 1;
        node.updated = Utc::now();
        let node = node.clone();
        let warnings = if state == "claimed" {
            overlap_warnings(g, &node)
        } else {
            Vec::new()
        };
        Ok(((node, warnings), true))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn new(goal: &str, deps: &[&str], scope: &[&str]) -> NewTask {
        NewTask {
            goal: goal.into(),
            schema: None,
            deps: deps.iter().map(|s| s.to_string()).collect(),
            write_scope: scope.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn cas_conflict_on_double_claim() {
        let d = tempfile::tempdir().unwrap();
        let (t, _) = create(d.path(), "parent", new("a", &[], &[])).unwrap();
        assert_eq!((t.id.as_str(), t.rev), ("t1", 1));
        let (c, _) = claim(d.path(), "w1", "t1", 1).unwrap();
        assert_eq!((c.rev, c.owner.as_deref()), (2, Some("w1")));
        let err = claim(d.path(), "w2", "t1", 1).unwrap_err().to_string();
        assert!(
            err.contains("rev conflict") && err.contains("current rev is 2"),
            "{err}"
        );
        assert!(err.contains("owner w1"), "{err}");
    }

    #[test]
    fn not_ready_until_dependency_done() {
        let d = tempfile::tempdir().unwrap();
        create(d.path(), "parent", new("base", &[], &[])).unwrap();
        create(d.path(), "parent", new("next", &["t1"], &[])).unwrap();
        let err = claim(d.path(), "w", "t2", 1).unwrap_err().to_string();
        assert!(err.contains("not ready") && err.contains("t1"), "{err}");
        claim(d.path(), "w", "t1", 1).unwrap();
        update(d.path(), "w", "t1", 2, "done", None).unwrap();
        let g = list(d.path()).unwrap();
        assert!(g.ready(g.get("t2").unwrap()));
        claim(d.path(), "w", "t2", 1).unwrap();
    }

    #[test]
    fn unknown_dependency_rejected() {
        let d = tempfile::tempdir().unwrap();
        let err = create(d.path(), "parent", new("x", &["t9"], &[]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("Unknown dependency"), "{err}");
        assert!(list(d.path()).unwrap().tasks.is_empty());
    }

    #[test]
    fn non_owner_update_rejected_but_parent_allowed() {
        let d = tempfile::tempdir().unwrap();
        create(d.path(), "parent", new("x", &[], &[])).unwrap();
        claim(d.path(), "w1", "t1", 1).unwrap();
        let err = update(d.path(), "w2", "t1", 2, "done", None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("owned by w1"), "{err}");
        let (n, _) = update(d.path(), "parent", "t1", 2, "open", None).unwrap();
        assert_eq!(
            (n.state.as_str(), n.owner.clone(), n.rev),
            ("open", None, 3)
        );
        assert!(update(d.path(), "w1", "t1", 3, "bogus", None).is_err());
    }

    #[test]
    fn done_result_is_checked_against_schema() {
        let d = tempfile::tempdir().unwrap();
        let schema = json!({"type":"object","required":["files"],"properties":{"files":{"type":"array","items":{"type":"string"}}}});
        create(
            d.path(),
            "parent",
            NewTask {
                schema: Some(schema),
                ..new("list files", &[], &[])
            },
        )
        .unwrap();
        claim(d.path(), "w", "t1", 1).unwrap();
        let err = update(d.path(), "w", "t1", 2, "done", Some(json!({"file": 1})))
            .unwrap_err()
            .to_string();
        assert!(err.contains("files"), "{err}");
        assert!(update(d.path(), "w", "t1", 2, "done", None).is_err());
        let (n, _) = update(d.path(), "w", "t1", 2, "done", Some(json!({"files":["a"]}))).unwrap();
        assert_eq!(n.result, Some(json!({"files":["a"]})));
    }

    #[test]
    fn overlapping_write_scope_warns() {
        let d = tempfile::tempdir().unwrap();
        create(d.path(), "parent", new("a", &[], &["src/parser/**"])).unwrap();
        create(d.path(), "parent", new("b", &[], &["src/parser/lex.rs"])).unwrap();
        create(d.path(), "parent", new("c", &[], &["docs/"])).unwrap();
        let (_, w) = claim(d.path(), "w1", "t1", 1).unwrap();
        assert!(w.is_empty());
        let (_, w) = claim(d.path(), "w2", "t2", 1).unwrap();
        assert_eq!(w.len(), 1);
        assert!(
            w[0].contains("t1") && w[0].contains("writer mutex"),
            "{w:?}"
        );
        let (_, w) = claim(d.path(), "w3", "t3", 1).unwrap();
        assert!(w.is_empty());
        assert!(scopes_overlap("./src", "src/a.rs"));
        assert!(!scopes_overlap("src/a.rs", "src/b.rs"));
        assert!(scopes_overlap("**", "anything"));
    }

    #[test]
    fn persists_and_tolerates_unknown_fields() {
        let d = tempfile::tempdir().unwrap();
        create(d.path(), "parent", new("a", &[], &[])).unwrap();
        let path = d.path().join("team.json");
        let mut v: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        v["future"] = json!(true);
        v["tasks"][0]["extra"] = json!("x");
        std::fs::write(&path, serde_json::to_vec(&v).unwrap()).unwrap();
        let g = list(d.path()).unwrap();
        assert_eq!(g.tasks[0].goal, "a");
        let (t, _) = create(d.path(), "parent", new("b", &[], &[])).unwrap();
        assert_eq!(t.id, "t2");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[cfg(unix)]
    #[test]
    fn refuses_symlinked_store() {
        let d = tempfile::tempdir().unwrap();
        let other = d.path().join("elsewhere.json");
        std::fs::write(&other, "{}").unwrap();
        std::os::unix::fs::symlink(&other, d.path().join("team.json")).unwrap();
        assert!(list(d.path()).is_err());
    }

    #[test]
    fn concurrent_claims_have_one_winner() {
        let d = tempfile::tempdir().unwrap();
        create(d.path(), "parent", new("a", &[], &[])).unwrap();
        let wins: usize = std::thread::scope(|s| {
            let handles: Vec<_> = (0..8)
                .map(|i| {
                    let dir = d.path().to_path_buf();
                    s.spawn(move || claim(&dir, &format!("w{i}"), "t1", 1).is_ok() as usize)
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).sum()
        });
        assert_eq!(wins, 1);
    }
}
