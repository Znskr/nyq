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

#[derive(Serialize, Deserialize)]
struct Entry<T> {
    version: u32,
    slot: String,
    fingerprint: String,
    value: T,
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
        let text = std::fs::read_to_string(self.path(key)).ok()?;
        let entry: Entry<T> = serde_json::from_str(&text).ok()?;
        (entry.version == VERSION && entry.slot == key.slot && entry.fingerprint == key.fingerprint)
            .then_some(entry.value)
    }

    /// Store a result and evict older results for the same goal and inputs
    /// (their fingerprint differs, so they are stale).
    pub fn put<T: Serialize>(&self, key: &MemoKey, value: &T) -> io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let entry = Entry {
            version: VERSION,
            slot: key.slot.clone(),
            fingerprint: key.fingerprint.clone(),
            value,
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
