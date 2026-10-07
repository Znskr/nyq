//! Bounded, in-memory snapshots of this session's file mutations.

use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::Value;

const MAX_BYTES: usize = 10 * 1024 * 1024;
const MAX_ENTRIES: usize = 100;

enum Before {
    Bytes(Vec<u8>),
    Missing,
    Unavailable(String),
}

struct Entry {
    path: PathBuf,
    cwd: PathBuf,
    before: Before,
    missing_dirs: Vec<PathBuf>,
}

#[derive(Default)]
pub struct UndoLog {
    entries: VecDeque<Entry>,
}

impl UndoLog {
    /// Capture before running a file tool; discard the entry if the tool fails.
    pub fn snapshot(&mut self, cwd: &Path, tool: &str, args: &Value) {
        if !matches!(tool, "write_file" | "edit_file") {
            return;
        }
        let Some(path) = args["path"].as_str().filter(|p| !p.is_empty()) else {
            return;
        };
        let cwd = std::path::absolute(cwd).unwrap_or_else(|_| cwd.to_path_buf());
        let path = cwd.join(path);
        let before = match File::open(&path) {
            Ok(file) => {
                let mut bytes = Vec::new();
                match file.take(MAX_BYTES as u64 + 1).read_to_end(&mut bytes) {
                    Ok(_) if bytes.len() <= MAX_BYTES => Before::Bytes(bytes),
                    Ok(_) => Before::Unavailable("file exceeded the 10 MB snapshot limit".into()),
                    Err(e) => Before::Unavailable(format!("could not read the original file: {e}")),
                }
            }
            Err(e) if e.kind() == ErrorKind::NotFound => Before::Missing,
            Err(e) => Before::Unavailable(format!("could not read the original file: {e}")),
        };
        let mut missing_dirs = Vec::new();
        if matches!(before, Before::Missing) {
            let mut parent = path.parent();
            while let Some(dir) = parent {
                if dir.exists() || dir == cwd {
                    break;
                }
                missing_dirs.push(dir.to_path_buf());
                parent = dir.parent();
            }
        }
        if self.entries.len() == MAX_ENTRIES {
            self.entries.pop_front();
        }
        self.entries.push_back(Entry {
            path,
            cwd,
            before,
            missing_dirs,
        });
    }

    pub fn discard_last(&mut self) {
        self.entries.pop_back();
    }

    /// Restore unconditionally, including over subsequent external edits.
    /// Failed filesystem restores remain available for retry. Unavailable
    /// snapshots are consumed so an earlier mutation can still be undone.
    pub fn undo(&mut self) -> Result<String> {
        let entry = self.entries.back().context("Nothing to undo.")?;
        let display = entry
            .path
            .strip_prefix(&entry.cwd)
            .unwrap_or(&entry.path)
            .display();
        let message = match &entry.before {
            Before::Bytes(bytes) => {
                fs::write(&entry.path, bytes)
                    .with_context(|| format!("Could not restore {display}"))?;
                format!("Restored {display}")
            }
            Before::Missing => {
                match fs::remove_file(&entry.path) {
                    Ok(()) => {}
                    Err(e) if e.kind() == ErrorKind::NotFound => {}
                    Err(e) => return Err(e).with_context(|| format!("Could not remove {display}")),
                }
                // Only remove parents absent at snapshot time, and only if
                // their real location is strictly inside the workspace.
                if let Ok(root) = entry.cwd.canonicalize() {
                    for dir in &entry.missing_dirs {
                        let Ok(real) = dir.canonicalize() else { break };
                        if real == root || !real.starts_with(&root) || fs::remove_dir(dir).is_err()
                        {
                            break;
                        }
                    }
                }
                format!("Removed new file {display}")
            }
            Before::Unavailable(reason) => {
                let message = format!("Cannot undo {display}: {reason}.");
                self.entries.pop_back();
                anyhow::bail!(message);
            }
        };
        self.entries.pop_back();
        Ok(message)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    #[test]
    fn undo_edit_restores_bytes_even_after_external_changes() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("file");
        fs::write(&path, [0, 255, 42]).unwrap();
        let mut log = UndoLog::default();
        log.snapshot(dir.path(), "edit_file", &json!({"path": "file"}));
        fs::write(&path, "external change").unwrap();
        assert_eq!(log.undo().unwrap(), "Restored file");
        assert_eq!(fs::read(path).unwrap(), [0, 255, 42]);
        assert!(log.is_empty());
        assert_eq!(log.undo().unwrap_err().to_string(), "Nothing to undo.");
    }

    #[test]
    fn new_file_removes_created_parents_only() {
        let dir = tempdir().unwrap();
        fs::create_dir(dir.path().join("existing")).unwrap();
        let path = dir.path().join("existing/new/nested/file");
        let mut log = UndoLog::default();
        log.snapshot(dir.path(), "write_file", &json!({"path": path}));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "new").unwrap();
        assert_eq!(
            log.undo().unwrap(),
            "Removed new file existing/new/nested/file"
        );
        assert!(!dir.path().join("existing/new").exists());
        assert!(dir.path().join("existing").is_dir());
    }

    #[test]
    fn lifo_and_discard() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("file");
        let mut log = UndoLog::default();
        for content in ["first", "second", "third"] {
            log.snapshot(dir.path(), "write_file", &json!({"path": "file"}));
            fs::write(&path, content).unwrap();
        }
        log.discard_last();
        assert_eq!(log.len(), 2);
        log.undo().unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "first");
        log.undo().unwrap();
        assert!(!path.exists());
        log.discard_last();
        assert!(log.is_empty());
    }

    #[test]
    fn bounded_history_drops_oldest() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("file");
        let mut log = UndoLog::default();
        fs::write(&path, "0").unwrap();
        for i in 1..=101 {
            log.snapshot(dir.path(), "edit_file", &json!({"path": "file"}));
            fs::write(&path, i.to_string()).unwrap();
        }
        assert_eq!(log.len(), MAX_ENTRIES);
        for _ in 0..MAX_ENTRIES {
            log.undo().unwrap();
        }
        assert_eq!(fs::read_to_string(path).unwrap(), "1");
    }

    #[test]
    fn byte_limit_and_unavailable_entry() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("large");
        let file = File::create(&path).unwrap();
        file.set_len(MAX_BYTES as u64).unwrap();
        let mut log = UndoLog::default();
        log.snapshot(dir.path(), "edit_file", &json!({"path": "large"}));
        fs::write(&path, "small").unwrap();
        log.undo().unwrap();
        assert_eq!(fs::metadata(&path).unwrap().len(), MAX_BYTES as u64);
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(MAX_BYTES as u64 + 1)
            .unwrap();
        log.snapshot(dir.path(), "write_file", &json!({"path": "large"}));
        fs::write(&path, "replacement").unwrap();
        assert!(log.undo().unwrap_err().to_string().contains("10 MB"));
        assert_eq!(fs::read_to_string(path).unwrap(), "replacement");
        assert!(log.is_empty());
    }

    #[test]
    fn ignores_other_tools_and_invalid_paths() {
        let dir = tempdir().unwrap();
        let mut log = UndoLog::default();
        for tool in ["bash", "read_file", "list_dir"] {
            log.snapshot(dir.path(), tool, &json!({"path": "file"}));
        }
        for args in [json!({}), json!({"path": 4}), json!({"path": ""})] {
            log.snapshot(dir.path(), "write_file", &args);
        }
        assert!(log.is_empty());
    }

    #[test]
    fn keeps_nonempty_parents_and_parents_outside_cwd() {
        let dir = tempdir().unwrap();
        let cwd = dir.path().join("workspace");
        fs::create_dir(&cwd).unwrap();
        let mut log = UndoLog::default();
        for relative in ["new/file", "../outside/file"] {
            let path = cwd.join(relative);
            log.snapshot(&cwd, "write_file", &json!({"path": relative}));
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, "new").unwrap();
            fs::write(path.parent().unwrap().join("keep"), "keep").unwrap();
            log.undo().unwrap();
            assert!(path.parent().unwrap().is_dir());
        }
        let path = dir.path().join("new-outside/another");
        log.snapshot(&cwd, "write_file", &json!({"path": path}));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "new").unwrap();
        log.undo().unwrap();
        assert!(dir.path().join("new-outside").is_dir());
    }
}
