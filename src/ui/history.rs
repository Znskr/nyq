//! Prompt history in `<home>/history`: one JSON record per line, appended as
//! each prompt is submitted (under a lock, so several nyq processes can share
//! the file) and tagged with its workspace. Older files hold one JSON string
//! (or plain text) per line; they load as prompts without a workspace.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use serde::{Deserialize, Serialize};

/// Records kept when the file is compacted.
const KEEP: usize = 1000;

#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct Record {
    text: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    cwd: String,
}

fn parse(line: &str) -> Option<Record> {
    if line.trim().is_empty() {
        return None;
    }
    match serde_json::from_str::<serde_json::Value>(line) {
        Ok(serde_json::Value::String(text)) => Some(Record {
            text,
            cwd: String::new(),
        }),
        Ok(value @ serde_json::Value::Object(_)) => serde_json::from_value(value).ok(),
        _ => Some(Record {
            text: line.to_string(),
            cwd: String::new(),
        }),
    }
}

/// Prompts for recall, oldest first: other workspaces' before this one's, so
/// Up reaches this workspace's prompts first. Repeats keep their latest place.
pub fn load(path: &Path, cwd: &str) -> Vec<String> {
    let Ok(data) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let records: Vec<Record> = data.lines().filter_map(parse).collect();
    let mut seen = std::collections::HashSet::new();
    let mut newest_first: Vec<&Record> = records
        .iter()
        .rev()
        .filter(|r| !r.text.trim().is_empty() && seen.insert(r.text.as_str()))
        .collect();
    newest_first.sort_by_key(|r| r.cwd != cwd);
    newest_first
        .into_iter()
        .map(|r| r.text.clone())
        .rev()
        .collect()
}

/// Append one prompt, compacting the file to the newest [`KEEP`] records
/// once it holds more.
pub fn append(path: &Path, text: &str, cwd: &str) -> std::io::Result<()> {
    let mut line = serde_json::to_string(&Record {
        text: text.into(),
        cwd: cwd.into(),
    })?;
    line.push('\n');
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true).read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut file = options.open(path)?;
    file.lock()?;
    let result = write_locked(&mut file, &line);
    let _ = file.unlock();
    result
}

fn write_locked(file: &mut File, line: &str) -> std::io::Result<()> {
    let mut data = String::new();
    file.seek(SeekFrom::Start(0))?;
    // A file that is not UTF-8 is left alone and appended to.
    let readable = file.read_to_string(&mut data).is_ok();
    if readable && !data.is_empty() && !data.ends_with('\n') {
        // A torn last line from a crash: start a fresh one.
        file.write_all(b"\n")?;
        data.push('\n');
    }
    file.write_all(line.as_bytes())?;
    data.push_str(line);
    let count = data.lines().filter(|l| !l.trim().is_empty()).count();
    if readable && count > KEEP {
        let kept: Vec<&str> = data
            .lines()
            .filter(|l| !l.trim().is_empty())
            .skip(count - KEEP)
            .collect();
        // Rewrite through the locked handle (it appends, so cut first).
        file.set_len(0)?;
        file.write_all((kept.join("\n") + "\n").as_bytes())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_tag_workspaces_and_recall_this_one_first() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("history");
        // An old-format file: JSON strings and plain lines.
        std::fs::write(&path, "\"old one\"\nplain\n").unwrap();
        append(&path, "here 1", "/here").unwrap();
        append(&path, "there", "/there").unwrap();
        append(&path, "multi\nline", "/here").unwrap();
        append(&path, "plain", "/there").unwrap();
        assert_eq!(
            load(&path, "/here"),
            ["old one", "there", "plain", "here 1", "multi\nline"]
        );
        assert_eq!(
            load(&path, "/there"),
            ["old one", "here 1", "multi\nline", "there", "plain"]
        );
        assert!(load(&tmp.path().join("missing"), "/").is_empty());
    }

    #[test]
    fn compacts_to_the_newest_records() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("history");
        std::fs::write(&path, "\"torn").unwrap();
        for i in 0..KEEP + 5 {
            append(&path, &format!("p{i}"), "/w").unwrap();
        }
        let data = std::fs::read_to_string(&path).unwrap();
        assert_eq!(data.lines().count(), KEEP);
        assert!(data.starts_with("{\"text\":\"p5\""), "{}", &data[..40]);
        let all = load(&path, "/w");
        assert_eq!((all.len(), all.last().unwrap().as_str()), (KEEP, "p1004"));
    }
}
