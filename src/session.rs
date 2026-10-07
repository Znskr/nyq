//! Saved conversations: one JSONL file per session under `<home>/nyq/sessions`.
//!
//! Line 1 is a header (`{"type":"session",...}`); every following line is one
//! [`Item`]. Files are append-only, so a crash can at worst truncate the last
//! line, which loading tolerates.

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::types::Item;

const INTERRUPTED: &str = "Interrupted before this tool ran.";
const TITLE_MAX: usize = 60;

#[derive(Clone, Debug)]
pub struct Meta {
    pub id: String,
    pub created: DateTime<Utc>,
    pub cwd: PathBuf,
    #[allow(dead_code)] // not shown by the current UI
    pub model: String,
    /// First user message, truncated for listing.
    pub title: String,
    pub path: PathBuf,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename = "session")]
struct Header {
    id: String,
    created: DateTime<Utc>,
    cwd: PathBuf,
    model: String,
}

pub struct Session {
    pub id: String,
    pub path: PathBuf,
    /// Header line still to be written (new sessions, before the first append).
    pending_header: Option<String>,
    file: Option<File>,
}

impl Session {
    /// New session. The file is only created on the first append so that
    /// empty sessions leave nothing behind.
    pub fn create(dir: &Path, cwd: &Path, model: &str) -> Result<Self> {
        let id = uuid::Uuid::new_v4().to_string();
        let header = Header { id: id.clone(), created: Utc::now(), cwd: cwd.to_path_buf(), model: model.to_string() };
        Ok(Self {
            path: dir.join(format!("{id}.jsonl")),
            id,
            pending_header: Some(serde_json::to_string(&header)?),
            file: None,
        })
    }

    pub fn append(&mut self, item: &Item) -> Result<()> {
        let mut line = String::new();
        if self.file.is_none() {
            if let Some(dir) = self.path.parent() {
                fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
            }
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)
                .with_context(|| format!("opening {}", self.path.display()))?;
            self.file = Some(file);
            if let Some(h) = self.pending_header.take() {
                line.push_str(&h);
                line.push('\n');
            }
        }
        line.push_str(&serde_json::to_string(item)?);
        line.push('\n');
        let file = self.file.as_mut().expect("file opened above");
        file.write_all(line.as_bytes())
            .and_then(|_| file.flush())
            .with_context(|| format!("saving session {}", self.id))
    }
}

/// All sessions, newest first.
pub fn list(dir: &Path) -> Result<Vec<Meta>> {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
    };
    let mut all: Vec<Meta> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .filter_map(|p| read_meta(&p))
        .collect();
    all.sort_by_key(|m| std::cmp::Reverse(m.created));
    Ok(all)
}

/// Header plus title (first user message); `None` if the file is unreadable.
fn read_meta(path: &Path) -> Option<Meta> {
    let mut lines = BufReader::new(File::open(path).ok()?).lines();
    let header: Header = serde_json::from_str(&lines.next()?.ok()?).ok()?;
    let title = lines
        .map_while(|l| l.ok())
        .find_map(|l| match serde_json::from_str(&l) {
            Ok(Item::User { text, .. }) => Some(title_of(&text)),
            _ => None,
        })
        .unwrap_or_else(|| "(untitled)".into());
    Some(Meta { id: header.id, created: header.created, cwd: header.cwd, model: header.model, title, path: path.into() })
}

fn title_of(text: &str) -> String {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    if line.chars().count() > TITLE_MAX {
        let cut: String = line.chars().take(TITLE_MAX - 1).collect();
        format!("{}…", cut.trim_end())
    } else {
        line.to_string()
    }
}

/// Load a session by id, unique id prefix, or `last` (newest, preferring the
/// given cwd). Returns the session reopened for appending plus its items.
pub fn load(dir: &Path, which: &str, cwd: &Path) -> Result<(Session, Meta, Vec<Item>)> {
    let all = list(dir)?;
    if all.is_empty() {
        bail!("No saved sessions in {}", dir.display());
    }
    let meta = if which == "last" {
        all.iter().find(|m| m.cwd == cwd).unwrap_or(&all[0]).clone()
    } else if let Some(m) = all.iter().find(|m| m.id == which) {
        m.clone()
    } else {
        let matches: Vec<&Meta> = all.iter().filter(|m| m.id.starts_with(which)).collect();
        match matches.as_slice() {
            [] => bail!("No session matches '{which}' (see `nyq sessions` or /sessions)."),
            [m] => (*m).clone(),
            many => bail!(
                "'{which}' is ambiguous; it matches {} sessions: {}",
                many.len(),
                many.iter().map(|m| m.id.as_str()).collect::<Vec<_>>().join(", ")
            ),
        }
    };

    let data = fs::read_to_string(&meta.path).with_context(|| format!("reading {}", meta.path.display()))?;
    let (mut items, good) = read_items(&data, &meta.path)?;
    if good < data.len() || !data.ends_with('\n') {
        // Drop a half-written final line and make sure appends start on a
        // fresh line.
        let mut f = OpenOptions::new().write(true).open(&meta.path)?;
        f.set_len(good as u64)?;
        if !data[..good].ends_with('\n') {
            f.seek(SeekFrom::End(0))?;
            f.write_all(b"\n")?;
        }
    }
    let mut session = Session { id: meta.id.clone(), path: meta.path.clone(), pending_header: None, file: None };
    for item in repairs(&items) {
        session.append(&item)?;
        items.push(item);
    }
    // Everything before the latest compaction is summarized by it.
    if let Some(i) = items.iter().rposition(|it| matches!(it, Item::Compaction { .. })) {
        items.drain(..i);
    }
    Ok((session, meta, items))
}

/// Items after the header plus the byte length of the well-formed prefix.
/// An unparsable final line (crash mid-write) is ignored; an unparsable line
/// elsewhere is an error.
fn read_items(data: &str, path: &Path) -> Result<(Vec<Item>, usize)> {
    let mut items = Vec::new();
    let mut good = 0;
    let mut lines = data.split_inclusive('\n').enumerate().peekable();
    while let Some((n, raw)) = lines.next() {
        let line = raw.trim();
        if n > 0 && !line.is_empty() {
            match serde_json::from_str::<Item>(line) {
                Ok(item) => items.push(item),
                Err(_) if lines.peek().is_none() => break,
                Err(e) => bail!("{}: line {} is corrupt: {e}", path.display(), n + 1),
            }
        }
        good += raw.len();
    }
    Ok((items, good))
}

/// Synthetic results for trailing tool calls that never got one, so the next
/// request to the model is well-formed.
fn repairs(items: &[Item]) -> Vec<Item> {
    let tail_start = items
        .iter()
        .rposition(|i| !matches!(i, Item::ToolCall { .. } | Item::ToolResult { .. }))
        .map_or(0, |p| p + 1);
    let tail = &items[tail_start..];
    let answered: HashSet<&str> = tail
        .iter()
        .filter_map(|i| match i {
            Item::ToolResult { id, .. } => Some(id.as_str()),
            _ => None,
        })
        .collect();
    tail.iter()
        .filter_map(|i| match i {
            Item::ToolCall { id, .. } if !answered.contains(id.as_str()) => {
                Some(Item::ToolResult { id: id.clone(), output: INTERRUPTED.into(), is_error: true })
            }
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(t: &str) -> Item {
        Item::user(t)
    }
    fn call(id: &str) -> Item {
        Item::ToolCall { id: id.into(), name: "bash".into(), arguments: "{}".into() }
    }

    #[test]
    fn create_is_lazy_and_roundtrips() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("sessions");
        let mut s = Session::create(&dir, Path::new("/w"), "m1").unwrap();
        assert!(!dir.exists());
        assert!(list(&dir).unwrap().is_empty());
        s.append(&user("hello there\nsecond line")).unwrap();
        s.append(&Item::Assistant { text: "hi".into() }).unwrap();
        let first = fs::read_to_string(&s.path).unwrap();
        let header: serde_json::Value = serde_json::from_str(first.lines().next().unwrap()).unwrap();
        assert_eq!(header["type"], "session");
        assert_eq!(header["id"], s.id.as_str());
        assert_eq!(header["model"], "m1");

        let metas = list(&dir).unwrap();
        assert_eq!(metas.len(), 1);
        assert_eq!(metas[0].title, "hello there");
        let (_, meta, items) = load(&dir, &s.id, Path::new("/w")).unwrap();
        assert_eq!(meta.cwd, Path::new("/w"));
        assert_eq!(items, vec![user("hello there\nsecond line"), Item::Assistant { text: "hi".into() }]);
    }

    #[test]
    fn title_truncates() {
        let t = title_of(&"x".repeat(100));
        assert_eq!(t.chars().count(), TITLE_MAX);
        assert!(t.ends_with('…'));
        assert_eq!(title_of("  \n short "), "short");
    }

    #[test]
    fn last_prefix_and_ambiguity() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let mut ids = Vec::new();
        for (cwd, text) in [("/a", "one"), ("/b", "two"), ("/c", "three")] {
            let mut s = Session::create(dir, Path::new(cwd), "m").unwrap();
            s.append(&user(text)).unwrap();
            ids.push(s.id.clone());
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        // Newest first.
        let titles: Vec<_> = list(dir).unwrap().into_iter().map(|m| m.title).collect();
        assert_eq!(titles, ["three", "two", "one"]);
        assert_eq!(load(dir, "last", Path::new("/a")).unwrap().1.title, "one");
        assert_eq!(load(dir, "last", Path::new("/zzz")).unwrap().1.title, "three");
        assert_eq!(load(dir, &ids[1][..8], Path::new("/")).unwrap().1.title, "two");
        assert!(load(dir, "", Path::new("/")).err().unwrap().to_string().contains("ambiguous"));
        assert!(load(dir, "nope", Path::new("/")).err().unwrap().to_string().contains("No session"));
        // Garbage files are skipped silently.
        fs::write(dir.join("junk.jsonl"), "not json\n").unwrap();
        assert_eq!(list(dir).unwrap().len(), 3);
    }

    #[test]
    fn truncated_tail_and_repair() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let mut s = Session::create(dir, Path::new("/w"), "m").unwrap();
        s.append(&user("run it")).unwrap();
        s.append(&call("c1")).unwrap();
        s.append(&call("c2")).unwrap();
        s.append(&Item::ToolResult { id: "c1".into(), output: "ok".into(), is_error: false }).unwrap();
        drop(s);
        let path = dir.join(fs::read_dir(dir).unwrap().next().unwrap().unwrap().file_name());
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(br#"{"type":"tool_res"#).unwrap();
        drop(f);

        let (mut s, _, items) = load(dir, "last", Path::new("/w")).unwrap();
        assert_eq!(items.len(), 5);
        assert_eq!(items[4], Item::ToolResult { id: "c2".into(), output: INTERRUPTED.into(), is_error: true });
        s.append(&Item::Assistant { text: "done".into() }).unwrap();

        // The file is consistent: reloading needs no further repair.
        let (_, _, again) = load(dir, "last", Path::new("/w")).unwrap();
        assert_eq!(again.len(), 6);
        assert_eq!(again[4], items[4]);
        assert!(repairs(&again).is_empty());
    }

    #[test]
    fn complete_last_line_without_newline() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("x.jsonl");
        let header = r#"{"type":"session","id":"abc","created":"2026-01-01T00:00:00Z","cwd":"/w","model":"m"}"#;
        fs::write(&path, format!("{header}\n{}", serde_json::to_string(&user("hi")).unwrap())).unwrap();
        let (mut s, meta, items) = load(tmp.path(), "abc", Path::new("/w")).unwrap();
        assert_eq!(meta.title, "hi");
        assert_eq!(items, vec![user("hi")]);
        s.append(&user("again")).unwrap();
        assert_eq!(load(tmp.path(), "abc", Path::new("/w")).unwrap().2.len(), 2);
    }
}
