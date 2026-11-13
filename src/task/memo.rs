//! Memoised task results (design §4.5).
//!
//! A result is stored under the user's state directory, at
//! `~/.nyq/cache/<workspace>/<key>.json` (`<workspace>` a hash of the
//! workspace path), never inside the workspace: a repository cannot plant
//! one. The key is `sha256(goal + inputs + workspace fingerprint)`, where the
//! inputs hold the schema, the checks and the model, and the fingerprint is
//! a hash of the `--memo-file` files when some are listed, else of the git
//! state (HEAD's tree, `git status`, the staged and unstaged diffs and the
//! untracked files' contents). With neither there is no key, and nothing is
//! memoised. Because the fingerprint is part of the key, a changed workspace
//! never returns a stale result; a hit is still re-checked by the task's
//! validators before it is used (`crate::task::headless::memo_lookup`). The
//! entry also remembers which `(goal, inputs)` slot it belongs to, so storing
//! a new result evicts the old ones for that slot.
//!
//! An entry also keeps [`Evidence`] from the run that produced it, for the
//! checks a replay cannot redo from the workspace alone:
//!
//! - the files the task touched, with their content hashes when the result
//!   was accepted. `touched:` and the judge read that set; a hit restores it
//!   only when every file still has its hash, and is a miss otherwise.
//! - the judges that passed, each under a key of its rubric, the value, the
//!   workspace fingerprint, the judge model and the touched files' hashes.
//!   A hit whose judge has the same key reuses the pass instead of calling
//!   the model; any difference re-judges.
//!
//! The key is the workspace fingerprint taken when the task *started*, so a
//! task that writes files hits only once the workspace is back in that state
//! (with the git fingerprint), or as long as the `--memo-file` files are
//! unchanged.

use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// The entry format; entries of another version read as misses.
const VERSION: u32 = 2;

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Hash a file's contents into `h` without loading it whole; a symlink
/// hashes as its target path and anything else that is not a regular file
/// (a FIFO, a device) by its kind, so hashing never blocks or runs forever.
fn hash_file(h: &mut Sha256, path: &Path) {
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => {
            h.update(b"link:");
            if let Ok(target) = std::fs::read_link(path) {
                h.update(target.to_string_lossy().as_bytes());
            }
        }
        Ok(m) if m.is_file() => {
            let mut file_hash = Sha256::new();
            let ok = std::fs::File::open(path).and_then(|mut f| {
                let mut buf = vec![0u8; 64 * 1024];
                loop {
                    let n = f.read(&mut buf)?;
                    if n == 0 {
                        return Ok(());
                    }
                    file_hash.update(&buf[..n]);
                }
            });
            match ok {
                Ok(()) => {
                    h.update(b"file:");
                    h.update(file_hash.finalize());
                }
                Err(_) => h.update(b"unreadable"),
            }
        }
        Ok(_) => h.update(b"special"),
        Err(_) => h.update(b"missing"),
    }
}

/// The hash of one file's contents (as [`hash_files`] counts them: a missing
/// file, a symlink or a special file each hash differently from any regular
/// file). `path` is relative to `root` or absolute.
pub fn hash_file_at(root: &Path, path: &Path) -> String {
    let mut h = Sha256::new();
    hash_file(&mut h, &root.join(path));
    hex(&h.finalize())
}

/// A hash over the listed files: their relative paths and contents. A missing
/// file hashes as missing, so creating or deleting one changes the hash.
pub fn hash_files(root: &Path, files: &[PathBuf]) -> String {
    let mut rels: Vec<PathBuf> = files
        .iter()
        .map(|f| f.strip_prefix(root).unwrap_or(f).to_path_buf())
        .collect();
    rels.sort();
    rels.dedup();
    let mut h = Sha256::new();
    for rel in &rels {
        h.update(rel.to_string_lossy().as_bytes());
        h.update([0]);
        hash_file(&mut h, &root.join(rel));
        h.update([0]);
    }
    hex(&h.finalize())
}

/// Run git in `dir` for its output; None when it fails (not a repository,
/// no git). External diff drivers, text conversion and fsmonitor hooks are
/// off: computing a key never runs repository-configured programs.
fn git(dir: &Path, args: &[&str]) -> Option<Vec<u8>> {
    let out = std::process::Command::new("git")
        .arg("--no-optional-locks")
        .arg("-C")
        .arg(dir)
        .args(["-c", "core.fsmonitor=false", "-c", "core.quotepath=off"])
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    out.status.success().then_some(out.stdout)
}

/// The git state of the repository holding `root`: HEAD's tree, the
/// porcelain status, the staged and unstaged diffs, and the contents of the
/// untracked files. None outside a git repository.
pub fn git_fingerprint(root: &Path) -> Option<String> {
    let top = git(root, &["rev-parse", "--show-toplevel"])?;
    let top = PathBuf::from(String::from_utf8_lossy(&top).trim());
    let mut h = Sha256::new();
    // An unborn HEAD (no commit yet) has no tree; the rest still counts.
    h.update(b"tree:");
    h.update(git(&top, &["rev-parse", "--verify", "--quiet", "HEAD^{tree}"]).unwrap_or_default());
    h.update([0]);
    let status = git(
        &top,
        &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
    )?;
    h.update(b"status:");
    h.update(&status);
    h.update([0]);
    for cached in [true, false] {
        let mut args = vec![
            "diff",
            "--binary",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
        ];
        if cached {
            args.push("--cached");
        }
        h.update(b"diff:");
        h.update(git(&top, &args)?);
        h.update([0]);
    }
    // The status names untracked files; their contents count too.
    for entry in status.split(|b| *b == 0) {
        if let Some(rel) = entry.strip_prefix(b"?? ") {
            let rel = String::from_utf8_lossy(rel);
            h.update(b"untracked:");
            h.update(rel.as_bytes());
            h.update([0]);
            hash_file(&mut h, &top.join(&*rel));
            h.update([0]);
        }
    }
    Some(hex(&h.finalize()))
}

/// What a memoised result depends on in the workspace: the listed files
/// when there are any, else the git state; None when neither is available.
pub fn fingerprint(root: &Path, files: &[PathBuf]) -> Option<String> {
    if !files.is_empty() {
        return Some(format!("files:{}", hash_files(root, files)));
    }
    git_fingerprint(root).map(|g| format!("git:{g}"))
}

/// Identifies one memoised result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemoKey {
    key: String,
    slot: String,
    fingerprint: String,
}

impl MemoKey {
    /// `inputs` is hashed by its compact JSON text, keys sorted;
    /// `fingerprint` is the workspace's (see [`fingerprint`]).
    pub fn new(goal: &str, inputs: &Value, fingerprint: &str) -> Self {
        let mut slot = Sha256::new();
        slot.update(goal.as_bytes());
        slot.update([0]);
        slot.update(super::cycle::canonical(inputs).as_bytes());
        let slot = hex(&slot.finalize());
        let mut key = Sha256::new();
        key.update(slot.as_bytes());
        key.update([0]);
        key.update(fingerprint.as_bytes());
        Self {
            key: hex(&key.finalize()),
            slot,
            fingerprint: fingerprint.to_string(),
        }
    }

    /// The hex digest used as the file name.
    pub fn as_str(&self) -> &str {
        &self.key
    }

    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }
}

/// A file the task touched and its content hash when the result was
/// accepted.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TouchedFile {
    /// Relative to the workspace, or absolute under another writable root.
    pub path: PathBuf,
    /// [`hash_file_at`].
    pub hash: String,
}

/// A judge that passed the result: its rubric, model and [`judge_key`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JudgePass {
    pub rubric: String,
    pub model: String,
    pub key: String,
}

/// What a memoised result was accepted on, beyond the key (see the module
/// docs). Entries written before it existed read as empty: no touched
/// files, no judge passes, so `touched:` misses and judges run again.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evidence {
    #[serde(default)]
    pub touched: Vec<TouchedFile>,
    #[serde(default)]
    pub judges: Vec<JudgePass>,
}

/// The cache key of a judge's pass. It covers every input the judge sees on
/// a replay: the rubric, the value, the workspace fingerprint (the goal is
/// in the memo slot), the judge model, and the touched files with their
/// hashes (the judge reads their contents). Two equal keys mean the judge
/// would be asked the same question about the same files.
pub fn judge_key(
    rubric: &str,
    value: &Value,
    fingerprint: &str,
    model: &str,
    touched: &[TouchedFile],
) -> String {
    let mut h = Sha256::new();
    for part in [rubric, &super::cycle::canonical(value), fingerprint, model] {
        h.update(part.len().to_le_bytes());
        h.update(part.as_bytes());
    }
    let mut files: Vec<&TouchedFile> = touched.iter().collect();
    files.sort_by(|a, b| a.path.cmp(&b.path));
    for f in files {
        let path = f.path.to_string_lossy();
        h.update(path.len().to_le_bytes());
        h.update(path.as_bytes());
        h.update(f.hash.as_bytes());
    }
    hex(&h.finalize())
}

impl Evidence {
    /// The touched files and their current hashes (duplicates dropped).
    pub fn capture(root: &Path, touched: &[PathBuf]) -> Self {
        let mut paths: Vec<&PathBuf> = touched.iter().collect();
        paths.sort();
        paths.dedup();
        Self {
            touched: paths
                .into_iter()
                .map(|p| TouchedFile {
                    path: p.clone(),
                    hash: hash_file_at(root, p),
                })
                .collect(),
            judges: Vec::new(),
        }
    }

    /// Record that the judge `model` passed `value` under `rubric`.
    pub fn add_judge(&mut self, rubric: &str, model: &str, value: &Value, fingerprint: &str) {
        let key = judge_key(rubric, value, fingerprint, model, &self.touched);
        if !self.judges.iter().any(|j| j.key == key) {
            self.judges.push(JudgePass {
                rubric: rubric.to_string(),
                model: model.to_string(),
                key,
            });
        }
    }

    /// The touched paths, when every file still has the hash it had when
    /// the result was accepted; None when one changed (or appeared, or went
    /// away).
    pub fn intact(&self, root: &Path) -> Option<Vec<PathBuf>> {
        self.touched
            .iter()
            .all(|f| hash_file_at(root, &f.path) == f.hash)
            .then(|| self.touched.iter().map(|f| f.path.clone()).collect())
    }

    /// Whether the judge `model` already passed `value` under `rubric`, with
    /// this fingerprint and these touched files.
    pub fn judge_passed(
        &self,
        rubric: &str,
        model: &str,
        value: &Value,
        fingerprint: &str,
    ) -> bool {
        let key = judge_key(rubric, value, fingerprint, model, &self.touched);
        self.judges.iter().any(|j| j.key == key)
    }
}

/// What a `--memo` entry keeps from the run that produced its result: the
/// touched files with their hashes when it was accepted, and the judges
/// that passed it (rubric and model). [`MemoEvidence::seal`] turns it into
/// the stored [`Evidence`] once the memo key is known.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MemoEvidence {
    /// The touched files and their hashes ([`Evidence::capture`]).
    pub touched: Evidence,
    /// `(rubric, model)` of each judge that passed.
    pub judges: Vec<(String, String)>,
}

impl MemoEvidence {
    /// The stored evidence for `value` under the memo key's `fingerprint`.
    pub fn seal(self, value: &Value, fingerprint: &str) -> Evidence {
        let mut evidence = self.touched;
        for (rubric, model) in &self.judges {
            evidence.add_judge(rubric, model, value, fingerprint);
        }
        evidence
    }
}

/// A `--memo` hit being re-checked: the evidence its entry kept and the
/// workspace fingerprint of its key.
pub struct Replay<'a> {
    pub evidence: &'a Evidence,
    pub fingerprint: &'a str,
}

#[derive(Serialize, Deserialize)]
struct Entry<T> {
    version: u32,
    slot: String,
    fingerprint: String,
    value: T,
    #[serde(default)]
    evidence: Evidence,
}

/// The cache directory of one workspace, under the user's state directory.
#[derive(Clone, Debug)]
pub struct MemoStore {
    dir: PathBuf,
}

impl MemoStore {
    /// `home/cache/<hash of the workspace path>`; `home` is nyq's state
    /// directory (`crate::config::home()`). Nothing under the workspace
    /// itself is ever read.
    pub fn for_workspace(home: &Path, root: &Path) -> Self {
        let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        let id = hex(&Sha256::digest(root.to_string_lossy().as_bytes()));
        Self {
            dir: home.join("cache").join(&id[..32]),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn path(&self, key: &MemoKey) -> PathBuf {
        self.dir.join(format!("{}.json", key.key))
    }

    /// The stored result, if there is one for exactly this goal, input and
    /// workspace state. A corrupt or foreign file reads as a miss.
    pub fn get<T: DeserializeOwned>(&self, key: &MemoKey) -> Option<T> {
        self.get_with(key).map(|(value, _)| value)
    }

    /// [`MemoStore::get`], with the evidence the result was accepted on.
    pub fn get_with<T: DeserializeOwned>(&self, key: &MemoKey) -> Option<(T, Evidence)> {
        let text = std::fs::read_to_string(self.path(key)).ok()?;
        let entry: Entry<T> = serde_json::from_str(&text).ok()?;
        (entry.version == VERSION && entry.slot == key.slot && entry.fingerprint == key.fingerprint)
            .then_some((entry.value, entry.evidence))
    }

    /// Store a result and evict older results for the same goal and inputs
    /// (their fingerprint differs, so they are stale).
    pub fn put<T: Serialize>(&self, key: &MemoKey, value: &T) -> io::Result<()> {
        self.put_with(key, value, &Evidence::default())
    }

    /// [`MemoStore::put`], with the evidence the result was accepted on.
    pub fn put_with<T: Serialize>(
        &self,
        key: &MemoKey,
        value: &T,
        evidence: &Evidence,
    ) -> io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let entry = Entry {
            version: VERSION,
            slot: key.slot.clone(),
            fingerprint: key.fingerprint.clone(),
            value,
            evidence: evidence.clone(),
        };
        let text = serde_json::to_string(&entry).map_err(io::Error::other)?;
        let path = self.path(key);
        let tmp = self.dir.join(format!("{}.tmp", key.key));
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, &path)?;
        self.evict_stale(key);
        Ok(())
    }

    pub fn invalidate(&self, key: &MemoKey) {
        let _ = std::fs::remove_file(self.path(key));
    }

    fn evict_stale(&self, key: &MemoKey) {
        let Ok(rd) = std::fs::read_dir(&self.dir) else {
            return;
        };
        let own = self.path(key);
        for e in rd.flatten() {
            let p = e.path();
            if p == own || p.extension().is_none_or(|x| x != "json") {
                continue;
            }
            let slot = std::fs::read_to_string(&p)
                .ok()
                .and_then(|t| serde_json::from_str::<Value>(&t).ok())
                .and_then(|v| v.get("slot").and_then(Value::as_str).map(str::to_owned));
            if slot.as_deref() == Some(key.slot.as_str()) {
                let _ = std::fs::remove_file(&p);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn setup() -> (tempfile::TempDir, PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().to_path_buf();
        std::fs::write(root.join("a.rs"), "fn a() {}").unwrap();
        std::fs::write(root.join("b.rs"), "fn b() {}").unwrap();
        (d, root)
    }

    /// A key over the listed files.
    fn key(root: &Path, goal: &str, inputs: Value, files: &[PathBuf]) -> MemoKey {
        MemoKey::new(goal, &inputs, &fingerprint(root, files).unwrap())
    }

    #[test]
    fn miss_then_hit_under_the_state_dir() {
        let (_d, root) = setup();
        let home = tempfile::tempdir().unwrap();
        let store = MemoStore::for_workspace(home.path(), &root);
        let key = key(&root, "plan it", json!({"n": 1}), &[PathBuf::from("a.rs")]);
        assert_eq!(store.get::<Value>(&key), None);
        store.put(&key, &json!({"steps": ["x", "y"]})).unwrap();
        assert_eq!(store.get::<Value>(&key), Some(json!({"steps": ["x", "y"]})));
        let file = store.dir().join(format!("{}.json", key.as_str()));
        assert!(file.is_file());
        assert!(file.starts_with(home.path().join("cache")));
        // Nothing is written inside the workspace.
        assert!(!root.join(".nyq").exists());
        // Another workspace has its own directory.
        let other = tempfile::tempdir().unwrap();
        assert_ne!(
            MemoStore::for_workspace(home.path(), other.path()).dir(),
            store.dir()
        );
    }

    /// The review's attack: a pull request adds `.nyq/cache/<key>.json` with
    /// a well-formed entry for the exact key. The workspace cache is never
    /// read, so it is a miss.
    #[test]
    fn a_result_planted_in_the_workspace_is_ignored() {
        let (_d, root) = setup();
        let home = tempfile::tempdir().unwrap();
        let key = key(&root, "is it safe?", json!({}), &[PathBuf::from("a.rs")]);
        let planted = json!({
            "version": VERSION,
            "slot": key.slot,
            "fingerprint": key.fingerprint,
            "value": {"safe": true},
        });
        for dir in [root.join(".nyq/cache"), root.join(".nyq/cache").join("x")] {
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join(format!("{}.json", key.as_str())),
                planted.to_string(),
            )
            .unwrap();
        }
        let store = MemoStore::for_workspace(home.path(), &root);
        assert_eq!(store.get::<Value>(&key), None);
    }

    #[test]
    fn typed_round_trip() {
        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct Plan {
            steps: Vec<String>,
        }
        let (_d, root) = setup();
        let home = tempfile::tempdir().unwrap();
        let store = MemoStore::for_workspace(home.path(), &root);
        let key = key(&root, "g", json!(null), &[PathBuf::from("a.rs")]);
        let plan = Plan {
            steps: vec!["one".into()],
        };
        store.put(&key, &plan).unwrap();
        assert_eq!(store.get::<Plan>(&key), Some(plan));
        // The wrong type reads as a miss rather than an error.
        assert_eq!(store.get::<Vec<u8>>(&key), None);
    }

    #[test]
    fn changing_a_listed_file_invalidates() {
        let (_d, root) = setup();
        let home = tempfile::tempdir().unwrap();
        let store = MemoStore::for_workspace(home.path(), &root);
        let files = [PathBuf::from("a.rs")];
        let k1 = key(&root, "g", json!({}), &files);
        store.put(&k1, &json!("old")).unwrap();

        std::fs::write(root.join("a.rs"), "fn a() { changed }").unwrap();
        let k2 = key(&root, "g", json!({}), &files);
        assert_ne!(k1, k2);
        assert_eq!(
            store.get::<Value>(&k2),
            None,
            "stale result is not returned"
        );

        store.put(&k2, &json!("new")).unwrap();
        assert_eq!(store.get::<Value>(&k2), Some(json!("new")));
        assert_eq!(store.get::<Value>(&k1), None, "the old entry was evicted");
        let n = std::fs::read_dir(store.dir()).unwrap().count();
        assert_eq!(n, 1);
    }

    #[test]
    fn unlisted_files_do_not_matter() {
        let (_d, root) = setup();
        let files = [PathBuf::from("a.rs")];
        let k1 = key(&root, "g", json!({}), &files);
        std::fs::write(root.join("b.rs"), "changed").unwrap();
        assert_eq!(k1, key(&root, "g", json!({}), &files));
    }

    #[test]
    fn goal_and_inputs_are_part_of_the_key() {
        let base = MemoKey::new("g", &json!({"a": 1}), "f");
        assert_ne!(base, MemoKey::new("h", &json!({"a": 1}), "f"));
        assert_ne!(base, MemoKey::new("g", &json!({"a": 2}), "f"));
        assert_ne!(base, MemoKey::new("g", &json!({"a": 1}), "other"));
        assert_eq!(base, MemoKey::new("g", &json!({"a": 1}), "f"));
    }

    #[test]
    fn input_key_order_does_not_matter() {
        let a = MemoKey::new("g", &json!({"x": 1, "y": {"p": 1, "q": 2}}), "f");
        let b = MemoKey::new("g", &json!({"y": {"q": 2, "p": 1}, "x": 1}), "f");
        assert_eq!(a, b);
    }

    #[test]
    fn other_slots_are_not_evicted() {
        let (_d, root) = setup();
        let home = tempfile::tempdir().unwrap();
        let store = MemoStore::for_workspace(home.path(), &root);
        let ka = MemoKey::new("goal a", &json!({}), "f");
        let kb = MemoKey::new("goal b", &json!({}), "f");
        store.put(&ka, &json!(1)).unwrap();
        store.put(&kb, &json!(2)).unwrap();
        assert_eq!(store.get::<Value>(&ka), Some(json!(1)));
        assert_eq!(store.get::<Value>(&kb), Some(json!(2)));
    }

    #[test]
    fn creating_or_deleting_a_listed_file_changes_the_hash() {
        let (_d, root) = setup();
        let files = [PathBuf::from("c.rs")];
        let missing = hash_files(&root, &files);
        std::fs::write(root.join("c.rs"), "").unwrap();
        let empty = hash_files(&root, &files);
        assert_ne!(missing, empty);
        std::fs::remove_file(root.join("c.rs")).unwrap();
        assert_eq!(hash_files(&root, &files), missing);
    }

    #[test]
    fn absolute_and_relative_paths_hash_alike_and_order_is_irrelevant() {
        let (_d, root) = setup();
        let rel = hash_files(&root, &[PathBuf::from("a.rs"), PathBuf::from("b.rs")]);
        let abs = hash_files(&root, &[root.join("b.rs"), root.join("a.rs")]);
        assert_eq!(rel, abs);
    }

    #[test]
    fn corrupt_entry_is_a_miss() {
        let (_d, root) = setup();
        let home = tempfile::tempdir().unwrap();
        let store = MemoStore::for_workspace(home.path(), &root);
        let key = MemoKey::new("g", &json!({}), "f");
        std::fs::create_dir_all(store.dir()).unwrap();
        std::fs::write(
            store.dir().join(format!("{}.json", key.as_str())),
            "{not json",
        )
        .unwrap();
        assert_eq!(store.get::<Value>(&key), None);
        // And it can be overwritten.
        store.put(&key, &json!(3)).unwrap();
        assert_eq!(store.get::<Value>(&key), Some(json!(3)));
    }

    #[test]
    fn invalidate_removes_the_entry() {
        let (_d, root) = setup();
        let home = tempfile::tempdir().unwrap();
        let store = MemoStore::for_workspace(home.path(), &root);
        let key = MemoKey::new("g", &json!({}), "f");
        store.put(&key, &json!(1)).unwrap();
        store.invalidate(&key);
        assert_eq!(store.get::<Value>(&key), None);
        store.invalidate(&key); // no panic when already gone
    }

    #[test]
    fn evidence_round_trips_and_older_entries_read_as_empty() {
        let (_d, root) = setup();
        let home = tempfile::tempdir().unwrap();
        let store = MemoStore::for_workspace(home.path(), &root);
        let key = MemoKey::new("g", &json!({}), "f");
        let mut evidence = Evidence::capture(
            &root,
            &[
                PathBuf::from("b.rs"),
                PathBuf::from("a.rs"),
                PathBuf::from("a.rs"),
            ],
        );
        assert_eq!(
            evidence
                .touched
                .iter()
                .map(|t| t.path.clone())
                .collect::<Vec<_>>(),
            [PathBuf::from("a.rs"), PathBuf::from("b.rs")],
            "sorted, without duplicates"
        );
        evidence.add_judge("r", "p/m", &json!(1), "f");
        evidence.add_judge("r", "p/m", &json!(1), "f");
        assert_eq!(evidence.judges.len(), 1, "a pass is recorded once");
        store.put_with(&key, &json!(1), &evidence).unwrap();
        assert_eq!(
            store.get_with::<Value>(&key),
            Some((json!(1), evidence.clone()))
        );
        assert_eq!(store.get::<Value>(&key), Some(json!(1)));
        // An entry an older nyq wrote has no evidence: it reads as empty.
        let old = json!({
            "version": VERSION,
            "slot": key.slot,
            "fingerprint": key.fingerprint,
            "value": 2,
        });
        std::fs::write(
            store.dir().join(format!("{}.json", key.as_str())),
            old.to_string(),
        )
        .unwrap();
        assert_eq!(
            store.get_with::<Value>(&key),
            Some((json!(2), Evidence::default()))
        );
    }

    #[test]
    fn touched_files_are_intact_only_with_their_recorded_hashes() {
        let (_d, root) = setup();
        let files = [PathBuf::from("a.rs"), root.join("new.rs")];
        let evidence = Evidence::capture(&root, &files);
        assert_eq!(
            evidence.intact(&root),
            Some(vec![root.join("new.rs"), PathBuf::from("a.rs")]),
            "sorted: an absolute path (under another root) comes first"
        );
        // A file that was missing and now exists is not intact.
        std::fs::write(root.join("new.rs"), "").unwrap();
        assert_eq!(evidence.intact(&root), None);
        std::fs::remove_file(root.join("new.rs")).unwrap();
        assert!(evidence.intact(&root).is_some());
        // Nor is an edited or a deleted one.
        std::fs::write(root.join("a.rs"), "fn a() { edited }").unwrap();
        assert_eq!(evidence.intact(&root), None);
        std::fs::remove_file(root.join("a.rs")).unwrap();
        assert_eq!(evidence.intact(&root), None);
        // Nothing touched is trivially intact.
        assert_eq!(Evidence::default().intact(&root), Some(vec![]));
        // Unrelated files do not matter.
        let a = Evidence::capture(&root, &[PathBuf::from("b.rs")]);
        std::fs::write(root.join("c.rs"), "x").unwrap();
        assert!(a.intact(&root).is_some());
    }

    #[test]
    fn a_judge_key_covers_every_input() {
        let t = |path: &str, hash: &str| TouchedFile {
            path: PathBuf::from(path),
            hash: hash.into(),
        };
        let touched = vec![t("a", "1"), t("b", "2")];
        let base = judge_key("r", &json!({"x": 1}), "f", "p/m", &touched);
        assert_eq!(base, judge_key("r", &json!({"x": 1}), "f", "p/m", &touched));
        // Order of the touched files and of the value's keys is irrelevant.
        let swapped = vec![t("b", "2"), t("a", "1")];
        assert_eq!(base, judge_key("r", &json!({"x": 1}), "f", "p/m", &swapped));
        for other in [
            judge_key("r2", &json!({"x": 1}), "f", "p/m", &touched),
            judge_key("r", &json!({"x": 2}), "f", "p/m", &touched),
            judge_key("r", &json!({"x": 1}), "f2", "p/m", &touched),
            judge_key("r", &json!({"x": 1}), "f", "p/m2", &touched),
            judge_key("r", &json!({"x": 1}), "f", "p/m", &[t("a", "1")]),
            judge_key(
                "r",
                &json!({"x": 1}),
                "f",
                "p/m",
                &[t("a", "1"), t("b", "3")],
            ),
            judge_key(
                "r",
                &json!({"x": 1}),
                "f",
                "p/m",
                &[t("a", "1"), t("c", "2")],
            ),
            // Parts are length-prefixed: shifting text between them differs.
            judge_key("rf", &json!({"x": 1}), "", "p/m", &touched),
        ] {
            assert_ne!(base, other);
        }
        let mut e = Evidence {
            touched,
            judges: Vec::new(),
        };
        e.add_judge("r", "p/m", &json!({"x": 1}), "f");
        assert!(e.judge_passed("r", "p/m", &json!({"x": 1}), "f"));
        assert!(!e.judge_passed("r", "p/other", &json!({"x": 1}), "f"));
        assert!(!e.judge_passed("r", "p/m", &json!({"x": 2}), "f"));
        // Sealing pending evidence adds each judge under the key's
        // fingerprint.
        let sealed = MemoEvidence {
            touched: Evidence::default(),
            judges: vec![("r".into(), "p/m".into())],
        }
        .seal(&json!(7), "fp");
        assert!(sealed.judge_passed("r", "p/m", &json!(7), "fp"));
        assert!(!sealed.judge_passed("r", "p/m", &json!(7), "other"));
    }

    #[test]
    fn without_files_the_git_state_is_the_fingerprint() {
        // Outside git, with nothing listed, there is no fingerprint.
        let (_d, plain) = setup();
        assert_eq!(fingerprint(&plain, &[]), None);

        let repo = crate::worktree::tests::repo();
        let root = repo.path();
        let base = fingerprint(root, &[]).expect("a git fingerprint");
        assert!(base.starts_with("git:"));
        assert_eq!(fingerprint(root, &[]).unwrap(), base, "stable");
        // An unstaged edit, a staged one, a new untracked file and a change
        // to its contents each change it.
        std::fs::write(root.join("a.txt"), "edited\n").unwrap();
        let edited = fingerprint(root, &[]).unwrap();
        assert_ne!(edited, base);
        let git = |args: &[&str]| {
            assert!(std::process::Command::new("git")
                .arg("-C")
                .arg(root)
                .args(args)
                .output()
                .unwrap()
                .status
                .success());
        };
        git(&["add", "a.txt"]);
        let staged = fingerprint(root, &[]).unwrap();
        assert_ne!(staged, base);
        std::fs::write(root.join("new.txt"), "one").unwrap();
        let untracked = fingerprint(root, &[]).unwrap();
        assert_ne!(untracked, staged);
        std::fs::write(root.join("new.txt"), "two").unwrap();
        assert_ne!(fingerprint(root, &[]).unwrap(), untracked);
        // Back to the committed state: the original fingerprint.
        std::fs::remove_file(root.join("new.txt")).unwrap();
        git(&["checkout", "HEAD", "--", "a.txt"]);
        assert_eq!(fingerprint(root, &[]).unwrap(), base);
        // Listed files win over the git state.
        assert!(fingerprint(root, &[PathBuf::from("a.txt")])
            .unwrap()
            .starts_with("files:"));
    }
}
