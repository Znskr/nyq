//! `nyq export`: write a saved conversation to a local file. Nothing is
//! uploaded; the only transformation is opt-in `--redact` (credential
//! scrubbing, path anonymization and image removal).

use std::io::Write;
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde_json::{json, Map, Value};

use crate::types::Item;

/// `--format`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Format {
    /// One JSON object per line: a `session` header, then every transcript
    /// item (the same shape the session file stores).
    Jsonl,
    /// ShareGPT fine-tune shape: `{"conversations":[{from,value},…]}`
    /// (`function_call`/`observation` roles carry tool calls and results;
    /// images never go into a ShareGPT export).
    Sharegpt,
}

/// `nyq export <session-id|--all> [--redact] [--format …] -o out`.
pub fn run(
    sessions_dir: &Path,
    cwd: &Path,
    which: Option<&str>,
    all: bool,
    redact: bool,
    format: Format,
    output: Option<&Path>,
) -> Result<()> {
    if all == which.is_some() {
        bail!("pass a session id (or `last`), or --all");
    }
    if all {
        let dir = output.context("--all needs -o DIR to write one file per session")?;
        export_all(sessions_dir, cwd, redact, format, dir)
    } else {
        let meta = crate::session::find(sessions_dir, which.unwrap_or("last"), cwd)?;
        let text = render(&meta, cwd, redact, format)?;
        write_out(output, meta.id.as_str(), format, &text)
    }
}

/// Every session in `dir`, one output file each under `out_dir`.
fn export_all(
    sessions_dir: &Path,
    cwd: &Path,
    redact: bool,
    format: Format,
    out_dir: &Path,
) -> Result<()> {
    let (metas, skipped) = crate::session::list_report(sessions_dir)?;
    if let Some(w) = crate::session::skipped_warning(skipped.len()) {
        eprintln!("{w}");
    }
    if metas.is_empty() {
        bail!("no saved sessions in {}", sessions_dir.display());
    }
    std::fs::create_dir_all(out_dir)
        .with_context(|| format!("creating {}", out_dir.display()))?;
    for meta in &metas {
        let text = render(meta, cwd, redact, format)?;
        write_out(Some(out_dir), meta.id.as_str(), format, &text)?;
    }
    println!("Exported {} sessions to {}", metas.len(), out_dir.display());
    Ok(())
}

/// The output for a single session: file under `-o`, or stdout
/// (`-o -`, or no `-o` for a single session).
fn write_out(output: Option<&Path>, id: &str, format: Format, text: &str) -> Result<()> {
    let stdout = || {
        let mut out = std::io::stdout().lock();
        out.write_all(text.as_bytes())?;
        out.write_all(b"\n")?;
        Ok::<(), std::io::Error>(())
    };
    match output {
        None => stdout().context("writing stdout")?,
        Some(p) if p.as_os_str() == "-" => stdout().context("writing stdout")?,
        Some(p) => {
            let path = if p.is_dir() {
                p.join(format!("{id}{}", format.extension()))
            } else {
                p.to_path_buf()
            };
            std::fs::write(&path, format!("{text}\n"))
                .with_context(|| format!("writing {}", path.display()))?;
            println!("Wrote {}", path.display());
        }
    }
    Ok(())
}

impl Format {
    fn extension(self) -> &'static str {
        match self {
            Format::Jsonl => ".jsonl",
            Format::Sharegpt => ".sharegpt.json",
        }
    }
}

/// A parsed session file: header fields plus the transcript items
/// (unreadable lines are skipped, like the loader's own tolerance).
fn read_transcript(path: &Path) -> Result<(Map<String, Value>, Vec<Item>)> {
    let data = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let mut header = Map::new();
    let mut items = Vec::new();
    for line in data.split(|b| *b == b'\n') {
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let Ok(v) = serde_json::from_slice::<Value>(line) else {
            continue;
        };
        match v.get("type").and_then(Value::as_str) {
            Some("session") => {
                if let Value::Object(m) = v {
                    header = m;
                }
            }
            // `title`/`prefs`/`permissions` are metadata, not transcript.
            Some("title") => {
                if let Some(t) = v.get("title").cloned() {
                    header.insert("title".into(), t);
                }
            }
            Some("prefs" | "permissions") => {}
            _ => {
                if let Ok(item) = serde_json::from_value::<Item>(v) {
                    items.push(item);
                }
            }
        }
    }
    Ok((header, items))
}

/// `/home/<name>` and `/Users/<name>` path prefixes become `~` — the
/// session may have been recorded on another account or machine than the
/// one exporting it.
fn anonymize_home(text: &str) -> std::borrow::Cow<'_, str> {
    static HOME: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r#"(/home|/Users)/[^/\\\s'":]+"#).unwrap());
    HOME.replace_all(text, "~")
}

/// Scrub credentials, anonymize paths (`~` for home, `<ws>` for the
/// session's workspace) and drop images.
fn redact_value(v: &mut Value, workspace: &str, home: &str) {
    match v {
        Value::String(s) => {
            let scrubbed = crate::tools::redact::scrub(s).into_owned();
            // The workspace path may live inside the home directory.
            let mut out = if workspace.is_empty() {
                scrubbed
            } else {
                scrubbed.replace(workspace, "<ws>")
            };
            if !home.is_empty() {
                out = out.replace(home, "~");
            }
            *s = anonymize_home(&out).into_owned();
        }
        Value::Array(a) => a.iter_mut().for_each(|x| redact_value(x, workspace, home)),
        Value::Object(m) => {
            m.values_mut().for_each(|x| redact_value(x, workspace, home));
            m.remove("images");
        }
        _ => {}
    }
}

/// One session rendered in `format` (redacted when asked).
fn render(meta: &crate::session::Meta, cwd: &Path, redact: bool, format: Format) -> Result<String> {
    let (header, items) = read_transcript(&meta.path)?;
    // The workspace to anonymize is the session's own, else this directory.
    let workspace = header
        .get("cwd")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| cwd.display().to_string());
    let home = dirs::home_dir().unwrap_or_default().display().to_string();
    let clean = |v: &mut Value| {
        if redact {
            redact_value(v, &workspace, &home);
        }
    };
    match format {
        Format::Jsonl => {
            let mut head = json!({
                "type": "session",
                "id": meta.id,
                "created": meta.created.to_rfc3339(),
                "cwd": workspace,
                "model": meta.model,
                "title": meta.title,
            });
            clean(&mut head);
            let mut out = serde_json::to_string(&head)?;
            for item in &items {
                let mut v = serde_json::to_value(item)?;
                clean(&mut v);
                out.push('\n');
                out.push_str(&serde_json::to_string(&v)?);
            }
            Ok(out)
        }
        Format::Sharegpt => {
            let mut conversations = Vec::new();
            for item in &items {
                // Map roles first: the "image(s) omitted" marker needs the
                // image count, which redaction drops.
                let v = serde_json::to_value(item)?;
                if let Some(mut c) = sharegpt_turn(&v) {
                    clean(&mut c);
                    conversations.push(c);
                }
            }
            let mut doc = json!({
                "id": meta.id,
                "conversations": Value::Array(conversations),
            });
            clean(&mut doc["id"]);
            Ok(serde_json::to_string(&doc)?)
        }
    }
}

/// One transcript item as a ShareGPT turn (None for reasoning, which has no
/// place in a training conversation).
fn sharegpt_turn(item: &Value) -> Option<Value> {
    let (from, value) = match item.get("type").and_then(Value::as_str) {
        Some("user") => {
            let mut text = item["text"].as_str().unwrap_or("").to_string();
            let images = item
                .get("images")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            if images > 0 {
                text.push_str(&format!("\n[{images} image(s) omitted]"));
            }
            ("human", text)
        }
        Some("assistant") => ("gpt", item["text"].as_str().unwrap_or("").to_string()),
        Some("tool_call") => (
            "function_call",
            json!({
                "name": item["name"],
                "arguments": item["arguments"],
            })
            .to_string(),
        ),
        Some("tool_result") => (
            "observation",
            item["output"].as_str().unwrap_or("").to_string(),
        ),
        Some("compaction") => (
            "system",
            item["summary"].as_str().unwrap_or("").to_string(),
        ),
        _ => return None,
    };
    Some(json!({"from": from, "value": value}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Image;
    use std::path::PathBuf;

    fn line(item: &Item) -> String {
        serde_json::to_string(item).unwrap()
    }

    /// A small session file under `dir`: header, title, three items.
    fn session_file(dir: &Path, id: &str) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join(format!("{id}.jsonl"));
        let content = format!(
            "{}\n{}\n{}\n{}\n{}\n",
            json!({"type":"session","version":1,"id":id,"created":"2026-01-01T00:00:00Z","cwd":"/tmp/ws","model":"m"}),
            json!({"type":"title","title":"demo"}),
            line(&Item::user("open /tmp/ws/secret.txt with key sk-ant-api03-abcdefghijklmnopqrstuvwxyz0123456789")),
            line(&Item::Assistant {
                text: "the file in /home/u holds ghp_abcdefghijklmnop1234567890".into()
            }),
            line(&Item::User {
                text: "look".into(),
                images: vec![Image {
                    media_type: "image/png".into(),
                    data: "QUJD".into(),
                }],
            }),
        );
        std::fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn jsonl_redacts_and_drops_images() {
        let t = tempfile::tempdir().unwrap();
        let path = session_file(t.path(), "s1");
        let meta = crate::session::Meta {
            id: "s1".into(),
            created: chrono::Utc::now(),
            updated: chrono::Utc::now(),
            cwd: PathBuf::from("/tmp/ws"),
            model: "m".into(),
            title: "demo".into(),
            preview: String::new(),
            turns: 1,
            prefs: None,
            path,
        };
        let out = render(&meta, Path::new("/tmp/ws"), true, Format::Jsonl).unwrap();
        assert!(out.contains("<ws>/secret.txt"), "{out}");
        assert!(!out.contains("sk-ant-"), "{out}");
        assert!(!out.contains("ghp_"), "{out}");
        assert!(!out.contains("QUJD"), "images must be dropped: {out}");
        assert!(!out.contains("/home/u"), "home paths anonymized: {out}");
    }

    #[test]
    fn jsonl_without_redact_keeps_everything() {
        let t = tempfile::tempdir().unwrap();
        let path = session_file(t.path(), "s1");
        let meta = crate::session::Meta {
            id: "s1".into(),
            created: chrono::Utc::now(),
            updated: chrono::Utc::now(),
            cwd: PathBuf::from("/tmp/ws"),
            model: "m".into(),
            title: "demo".into(),
            preview: String::new(),
            turns: 1,
            prefs: None,
            path,
        };
        let out = render(&meta, Path::new("/tmp/ws"), false, Format::Jsonl).unwrap();
        assert!(out.contains("QUJD"), "{out}");
        assert!(out.contains("/tmp/ws"), "{out}");
    }

    #[test]
    fn sharegpt_maps_roles() {
        let t = tempfile::tempdir().unwrap();
        let path = session_file(t.path(), "s1");
        let meta = crate::session::Meta {
            id: "s1".into(),
            created: chrono::Utc::now(),
            updated: chrono::Utc::now(),
            cwd: PathBuf::from("/tmp/ws"),
            model: "m".into(),
            title: "demo".into(),
            preview: String::new(),
            turns: 1,
            prefs: None,
            path,
        };
        let out: Value =
            serde_json::from_str(&render(&meta, Path::new("/tmp/ws"), true, Format::Sharegpt).unwrap())
                .unwrap();
        let conv = out["conversations"].as_array().unwrap();
        assert_eq!(conv[0]["from"], "human");
        assert_eq!(conv[1]["from"], "gpt");
        assert!(conv[2]["value"].as_str().unwrap().contains("omitted"));
        let text = out.to_string();
        assert!(!text.contains("ghp_"), "{text}");
    }
}
