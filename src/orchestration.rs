//! Durable, single-writer session inbox. The journal is authoritative; workers
//! are process-local and interrupted work is never automatically replayed.
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::HashMap,
    fs::{File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
};
use tokio::sync::Notify;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Job {
    pub id: String,
    pub name: String,
    pub args: Value,
    pub state: String,
    pub result: Option<String>,
    pub delivered: bool,
}
#[derive(Serialize, Deserialize)]
struct Entry {
    version: u8,
    sequence: u64,
    jobs: Vec<Job>,
}
pub struct Store {
    file: File,
    pub jobs: Vec<Job>,
    sequence: u64,
    pub notify: Arc<Notify>,
    pub active: std::collections::HashSet<String>,
    pub failure: Option<String>,
    pub outputs: HashMap<String, crate::tools::ToolOutput>,
}
type Shared = Arc<Mutex<Store>>;
static STORES: OnceLock<Mutex<HashMap<PathBuf, Shared>>> = OnceLock::new();
const MAX_RECORD: usize = 16 * 1024 * 1024;
impl Store {
    fn open(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join("orchestration.jsonl");
        if std::fs::symlink_metadata(dir)?.file_type().is_symlink() {
            bail!("Refusing linked orchestration directory");
        }
        if let Ok(meta) = std::fs::symlink_metadata(&path) {
            if !meta.is_file() || meta.file_type().is_symlink() {
                bail!("Journal must be a regular, unlinked file");
            }
        }
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&path)?;
        file.try_lock()
            .context("This session's orchestration is open in another process")?;
        let mut data = Vec::new();
        Read::by_ref(&mut file)
            .take(128 * 1024 * 1024 + 1)
            .read_to_end(&mut data)?;
        if data.len() > 128 * 1024 * 1024 {
            bail!("Orchestration journal exceeds 128 MiB; preserve it for repair");
        }
        let mut jobs = Vec::new();
        let mut sequence = 0;
        let mut end = 0;
        for line in data.split_inclusive(|b| *b == b'\n') {
            if line.last() != Some(&b'\n') {
                break;
            }
            if line.len() > MAX_RECORD {
                bail!("Oversized orchestration record");
            }
            let entry: Entry = serde_json::from_slice(line)
                .context("Corrupt orchestration journal; original preserved")?;
            if entry.version != 1 || entry.sequence != sequence + 1 {
                bail!("Invalid orchestration version or sequence");
            }
            sequence = entry.sequence;
            jobs = entry.jobs;
            end += line.len();
        }
        if end != data.len() {
            let backup = dir.join(format!("orchestration-tail-{}.bin", uuid::Uuid::new_v4()));
            crate::session::write_private(&backup, &data[end..])?;
            file.set_len(end as u64)?;
            file.sync_all()?;
        }
        use std::io::{Seek, SeekFrom};
        file.seek(SeekFrom::End(0))?;
        let mut store = Self {
            file,
            jobs,
            sequence,
            notify: Arc::new(Notify::new()),
            active: Default::default(),
            failure: None,
            outputs: Default::default(),
        };
        let mut next = store.jobs.clone();
        let mut changed = false;
        for job in &mut next {
            if job.state == "running" {
                job.state = "interrupted".into();
                job.result = Some("Interrupted by process exit. Inspect workspace and child transcript before sending a new task; uncertain tool effects were not replayed.".into());
                changed = true;
            }
        }
        if changed {
            let interrupted: std::collections::HashSet<_> = next
                .iter()
                .filter(|j| j.state == "interrupted")
                .map(|j| j.name.clone())
                .collect();
            for job in &mut next {
                if job.state == "queued" && interrupted.contains(&job.name) {
                    job.state = "interrupted".into();
                    job.result = Some("Follow-up held because this child's previous run was interrupted. Inspect its checkpoint and workspace, then explicitly resend this task.".into());
                }
            }
            store.commit(next)?;
        }
        // Persist file creation as well as contents.
        #[cfg(unix)]
        File::open(dir)?.sync_all()?;
        Ok(store)
    }
    pub fn commit(&mut self, jobs: Vec<Job>) -> Result<()> {
        let mut bytes = serde_json::to_vec(&Entry {
            version: 1,
            sequence: self.sequence + 1,
            jobs: jobs.clone(),
        })?;
        bytes.push(b'\n');
        if bytes.len() > MAX_RECORD {
            bail!("Orchestration record limit reached");
        }
        use std::io::{Seek, SeekFrom};
        let start = self.file.seek(SeekFrom::End(0))?;
        if start + bytes.len() as u64 > 128 * 1024 * 1024 {
            bail!("Orchestration journal limit reached; start a new session");
        }
        if let Err(e) = self
            .file
            .write_all(&bytes)
            .and_then(|_| self.file.sync_all())
        {
            // Don't append future transitions after a partial record.
            self.file
                .set_len(start)
                .context("Journal rollback failed; restart required")?;
            self.file.sync_all()?;
            return Err(e.into());
        }
        self.sequence += 1;
        self.jobs = jobs;
        self.notify.notify_one();
        Ok(())
    }
}
pub fn store(dir: &Path) -> Result<Shared> {
    let mut stores = STORES
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(s) = stores.get(dir) {
        return Ok(s.clone());
    }
    let s = Arc::new(Mutex::new(Store::open(dir)?));
    stores.insert(dir.to_owned(), s.clone());
    Ok(s)
}
pub fn enqueue(dir: &Path, name: &str, args: &Value) -> Result<String> {
    let shared = store(dir)?;
    let mut s = shared.lock().unwrap_or_else(|e| e.into_inner());
    let id = args
        .get("request_id")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    if id.is_empty()
        || id.len() > 128
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        bail!("request_id must be 1–128 letters, digits, dash, underscore or dot");
    }
    if let Some(job) = s.jobs.iter().find(|j| j.id == id) {
        if job.name != name || job.args != *args {
            bail!("request_id already belongs to different work");
        }
        return Ok(id);
    }
    if s.jobs.len() >= 1000 {
        bail!("Session orchestration limit of 1000 tasks reached");
    }
    let mut next = s.jobs.clone();
    next.push(Job {
        id: id.clone(),
        name: name.into(),
        args: args.clone(),
        state: "queued".into(),
        result: None,
        delivered: false,
    });
    s.commit(next)?;
    Ok(id)
}
pub fn cancel(dir: &Path, name: &str) -> Result<()> {
    let shared = store(dir)?;
    let mut s = shared.lock().unwrap_or_else(|e| e.into_inner());
    let mut next = s.jobs.clone();
    for j in &mut next {
        if j.name == name && matches!(j.state.as_str(), "queued" | "running") {
            j.state = "cancelled".into();
            j.result = Some("Cancelled by user".into());
            j.delivered = true;
        }
    }
    s.commit(next)
}
pub fn pending(dir: &Path) -> Result<bool> {
    Ok(store(dir)?
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .jobs
        .iter()
        .any(|j| matches!(j.state.as_str(), "queued" | "running")))
}
pub fn results(dir: &Path) -> Result<Vec<(String, String)>> {
    Ok(store(dir)?
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .jobs
        .iter()
        .filter(|j| !j.delivered && j.result.is_some())
        .map(|j| {
            (
                j.id.clone(),
                format!(
                    "[Child {} · task {} · {}]\n{}",
                    j.name,
                    j.id,
                    j.state,
                    j.result.as_deref().unwrap_or_default()
                ),
            )
        })
        .collect())
}
pub fn acknowledge(dir: &Path, id: &str) -> Result<()> {
    let shared = store(dir)?;
    let mut s = shared.lock().unwrap_or_else(|e| e.into_inner());
    let mut next = s.jobs.clone();
    if let Some(j) = next.iter_mut().find(|j| j.id == id) {
        j.delivered = true;
    }
    s.commit(next)
}
pub fn writer(cwd: &Path) -> Arc<tokio::sync::Mutex<()>> {
    static WRITERS: OnceLock<Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>> =
        OnceLock::new();
    WRITERS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(cwd.canonicalize().unwrap_or_else(|_| cwd.to_owned()))
        .or_default()
        .clone()
}

/// Child contexts inherit the parent's usage session ID, not its journal path.
pub fn enabled(session_id: &str) -> bool {
    STORES.get().is_some_and(|stores| {
        stores
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .any(|p| p.file_stem().and_then(|s| s.to_str()) == Some(session_id))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn durable_queue_and_retry() {
        let d = tempfile::tempdir().unwrap();
        let args = serde_json::json!({"task":"inspect", "request_id":"one"});
        assert_eq!(enqueue(d.path(), "reader", &args).unwrap(), "one");
        enqueue(d.path(), "reader", &args).unwrap();
        assert_eq!(store(d.path()).unwrap().lock().unwrap().jobs.len(), 1);
        assert!(enqueue(d.path(), "other", &args).is_err());
    }
    #[test]
    fn restart_marks_running_interrupted_and_preserves_queue() {
        let d = tempfile::tempdir().unwrap();
        let mut s = Store::open(d.path()).unwrap();
        let j = |state: &str| Job {
            id: state.into(),
            name: state.into(),
            args: Value::Null,
            state: state.into(),
            result: None,
            delivered: false,
        };
        s.commit(vec![j("running"), j("queued")]).unwrap();
        assert!(Store::open(d.path()).is_err());
        drop(s);
        let s = Store::open(d.path()).unwrap();
        assert_eq!(s.jobs[0].state, "interrupted");
        assert_eq!(s.jobs[1].state, "queued");
    }
    #[test]
    fn queued_followup_is_held_after_uncertain_previous_run() {
        let d = tempfile::tempdir().unwrap();
        let mut s = Store::open(d.path()).unwrap();
        let job = |state: &str| Job {
            id: state.into(),
            name: "same-child".into(),
            args: Value::Null,
            state: state.into(),
            result: None,
            delivered: false,
        };
        s.commit(vec![job("running"), job("queued")]).unwrap();
        drop(s);
        let s = Store::open(d.path()).unwrap();
        assert!(s.jobs.iter().all(|j| j.state == "interrupted"));
        assert!(s.jobs[1]
            .result
            .as_ref()
            .unwrap()
            .contains("explicitly resend"));
    }

    #[test]
    fn truncated_tail_recovers_but_complete_corruption_is_rejected() {
        let d = tempfile::tempdir().unwrap();
        let mut s = Store::open(d.path()).unwrap();
        s.commit(vec![]).unwrap();
        s.file.write_all(b"{partial").unwrap();
        drop(s);
        let mut s = Store::open(d.path()).unwrap();
        assert_eq!(s.sequence, 1);
        s.file.write_all(b"invalid\n").unwrap();
        drop(s);
        assert!(Store::open(d.path()).is_err());
    }
}
