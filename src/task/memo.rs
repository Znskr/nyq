//! Memoised task results (design §4.5).
//!
//! A result is stored at `.nyq/cache/<key>.json`, where the key is
//! `sha256(goal + inputs + hash of the listed files)`. Because the files hash is
//! part of the key, a changed file can never return a stale result. The entry
//! also remembers which `(goal, inputs)` slot it belongs to, so storing a new
//! result evicts the old ones for that slot.

use std::io;
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
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
        match std::fs::read(root.join(rel)) {
            Ok(bytes) => {
                h.update(b"file:");
                h.update(Sha256::digest(&bytes));
            }
            Err(_) => h.update(b"missing"),
        }
        h.update([0]);
    }
    hex(&h.finalize())
}

/// Identifies one memoised result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemoKey {
    key: String,
    slot: String,
    files_hash: String,
}

impl MemoKey {
    /// `inputs` is hashed by its compact JSON text, keys sorted. `files` are the files the
    /// result depends on, relative to `root` or absolute.
    pub fn new(root: &Path, goal: &str, inputs: &Value, files: &[PathBuf]) -> Self {
        let files_hash = hash_files(root, files);
        let mut slot = Sha256::new();
        slot.update(goal.as_bytes());
        slot.update([0]);
        slot.update(super::cycle::canonical(inputs).as_bytes());
        let slot = hex(&slot.finalize());
        let mut key = Sha256::new();
        key.update(slot.as_bytes());
        key.update([0]);
        key.update(files_hash.as_bytes());
        Self {
            key: hex(&key.finalize()),
            slot,
            files_hash,
        }
    }

    /// The hex digest used as the file name.
    pub fn as_str(&self) -> &str {
        &self.key
    }

    pub fn files_hash(&self) -> &str {
        &self.files_hash
    }
}

#[derive(Serialize, Deserialize)]
struct Entry<T> {
    version: u32,
    slot: String,
    files_hash: String,
    value: T,
}

/// The cache directory of one workspace.
#[derive(Clone, Debug)]
pub struct MemoStore {
    dir: PathBuf,
}

impl MemoStore {
    /// `root/.nyq/cache`
    pub fn new(root: &Path) -> Self {
        Self {
            dir: root.join(".nyq").join("cache"),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn path(&self, key: &MemoKey) -> PathBuf {
        self.dir.join(format!("{}.json", key.key))
    }

    /// The stored result, if there is one for exactly this goal, input and
    /// file contents. A corrupt or foreign file reads as a miss.
    pub fn get<T: DeserializeOwned>(&self, key: &MemoKey) -> Option<T> {
        let text = std::fs::read_to_string(self.path(key)).ok()?;
        let entry: Entry<T> = serde_json::from_str(&text).ok()?;
        (entry.version == 1 && entry.slot == key.slot && entry.files_hash == key.files_hash)
            .then_some(entry.value)
    }

    /// Store a result and evict older results for the same goal and inputs
    /// (their files hash differs, so they are stale).
    pub fn put<T: Serialize>(&self, key: &MemoKey, value: &T) -> io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let entry = Entry {
            version: 1,
            slot: key.slot.clone(),
            files_hash: key.files_hash.clone(),
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

    #[test]
    fn miss_then_hit() {
        let (_d, root) = setup();
        let store = MemoStore::new(&root);
        let key = MemoKey::new(&root, "plan it", &json!({"n": 1}), &[PathBuf::from("a.rs")]);
        assert_eq!(store.get::<Value>(&key), None);
        store.put(&key, &json!({"steps": ["x", "y"]})).unwrap();
        assert_eq!(store.get::<Value>(&key), Some(json!({"steps": ["x", "y"]})));
        assert!(root
            .join(".nyq/cache")
            .join(format!("{}.json", key.as_str()))
            .is_file());
    }

    #[test]
    fn typed_round_trip() {
        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct Plan {
            steps: Vec<String>,
        }
        let (_d, root) = setup();
        let store = MemoStore::new(&root);
        let key = MemoKey::new(&root, "g", &json!(null), &[]);
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
        let store = MemoStore::new(&root);
        let files = [PathBuf::from("a.rs")];
        let k1 = MemoKey::new(&root, "g", &json!({}), &files);
        store.put(&k1, &json!("old")).unwrap();

        std::fs::write(root.join("a.rs"), "fn a() { changed }").unwrap();
        let k2 = MemoKey::new(&root, "g", &json!({}), &files);
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
        let k1 = MemoKey::new(&root, "g", &json!({}), &files);
        std::fs::write(root.join("b.rs"), "changed").unwrap();
        assert_eq!(k1, MemoKey::new(&root, "g", &json!({}), &files));
    }

    #[test]
    fn goal_and_inputs_are_part_of_the_key() {
        let (_d, root) = setup();
        let base = MemoKey::new(&root, "g", &json!({"a": 1}), &[]);
        assert_ne!(base, MemoKey::new(&root, "h", &json!({"a": 1}), &[]));
        assert_ne!(base, MemoKey::new(&root, "g", &json!({"a": 2}), &[]));
        assert_eq!(base, MemoKey::new(&root, "g", &json!({"a": 1}), &[]));
    }

    #[test]
    fn input_key_order_does_not_matter() {
        let (_d, root) = setup();
        let a = MemoKey::new(&root, "g", &json!({"x": 1, "y": {"p": 1, "q": 2}}), &[]);
        let b = MemoKey::new(&root, "g", &json!({"y": {"q": 2, "p": 1}, "x": 1}), &[]);
        assert_eq!(a, b);
    }

    #[test]
    fn other_slots_are_not_evicted() {
        let (_d, root) = setup();
        let store = MemoStore::new(&root);
        let ka = MemoKey::new(&root, "goal a", &json!({}), &[]);
        let kb = MemoKey::new(&root, "goal b", &json!({}), &[]);
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
        let store = MemoStore::new(&root);
        let key = MemoKey::new(&root, "g", &json!({}), &[]);
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
        let store = MemoStore::new(&root);
        let key = MemoKey::new(&root, "g", &json!({}), &[]);
        store.put(&key, &json!(1)).unwrap();
        store.invalidate(&key);
        assert_eq!(store.get::<Value>(&key), None);
        store.invalidate(&key); // no panic when already gone
    }
}
