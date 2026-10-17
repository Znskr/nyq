//! Large tool results: output over [`SPILL_AT`] bytes is stored in full under
//! `<home>/artifacts/<session>/<id>.txt` and the model gets its head and tail
//! plus the id; `read_tool_result` pages through (or searches) the stored text.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::{Registry, Tool, ToolCtx, ToolOutput};
use crate::types::ToolSpec;

/// Results larger than this are stored as artifacts (not `skill` results,
/// which `skill_chunk_bytes` already splits into parts).
pub(crate) const SPILL_AT: usize = 16 * 1024;
const HEAD: usize = 3 * 1024;
const TAIL: usize = 1024;
const READ_DEFAULT: usize = 8 * 1024;
const READ_MAX: usize = 64 * 1024;
const QUERY_MAX_MATCHES: usize = 50;
pub(crate) const TOOL: &str = "read_tool_result";

/// Artifact directory name for a session; without one (subagents, tests),
/// a per-process name.
pub(crate) fn session_key(session: Option<&str>) -> String {
    match session.map(safe).filter(|s| !s.is_empty()) {
        Some(s) => s,
        None => format!("process-{}", std::process::id()),
    }
}

fn safe(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_')).take(64).collect()
}

fn dir(home: &Path, key: &str) -> PathBuf {
    home.join("artifacts").join(key)
}

/// Delete one session's spilled tool results. A missing directory is fine.
pub(crate) fn remove_session(home: &Path, session_id: &str) {
    let key = safe(session_id);
    if key.is_empty() {
        return;
    }
    let _ = std::fs::remove_dir_all(dir(home, &key));
}

fn floor(s: &str, mut i: usize) -> usize {
    i = i.min(s.len());
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil(s: &str, mut i: usize) -> usize {
    i = i.min(s.len());
    while !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// Store `content` when it is too large and return what the model should see.
pub(crate) fn spill(home: &Path, key: &str, call_id: &str, tool: &str, content: String) -> String {
    if content.len() <= SPILL_AT || tool == TOOL || tool == "skill" {
        return content;
    }
    let mut hash = Sha256::new();
    hash.update(call_id.as_bytes());
    hash.update([0]);
    hash.update(content.as_bytes());
    let digest: String = hash.finalize().iter().take(6).map(|b| format!("{b:02x}")).collect();
    let id = format!("{}-{digest}", safe(tool).chars().take(40).collect::<String>());
    let head = floor(&content, HEAD);
    let tail = ceil(&content, content.len() - TAIL).max(head);
    let total = content.len();
    let stored = crate::session::write_private(&dir(home, key).join(format!("{id}.txt")), content.as_bytes());
    let note = match stored {
        Ok(()) => format!(
            "[Full output: {total} bytes, stored as tool result \"{id}\". Call {TOOL} with {{\"id\": \"{id}\", \"offset\": {head}}} \
             to page through it (limit up to {READ_MAX} bytes), or with \"query\" to find matching lines.]"
        ),
        Err(e) => format!("[Output was {total} bytes; the middle was dropped (could not store it: {e}).]"),
    };
    format!(
        "{}\n… [{} bytes omitted] …\n{}\n{note}",
        &content[..head],
        tail - head,
        &content[tail..]
    )
}

/// Keep a downloaded file (a PDF or image from `web_fetch`) with the session's
/// artifacts, named by its content. Returns where it was saved.
pub(crate) fn store_file(home: &Path, key: &str, ext: &str, bytes: &[u8]) -> std::io::Result<PathBuf> {
    let digest: String = Sha256::digest(bytes).iter().take(8).map(|b| format!("{b:02x}")).collect();
    let path = dir(home, key).join(format!("fetch-{digest}.{}", safe(ext)));
    crate::session::write_private(&path, bytes)?;
    Ok(path)
}

/// Start a stored result for output that is still arriving: writes `first`
/// and returns the id and the file to append the rest to.
pub(crate) fn store_open(home: &Path, key: &str, tool: &str, first: &[u8]) -> std::io::Result<(String, std::fs::File)> {
    let id = format!("{}-{}", safe(tool), &uuid::Uuid::new_v4().simple().to_string()[..12]);
    let path = dir(home, key).join(format!("{id}.txt"));
    crate::session::write_private(&path, first)?;
    let file = std::fs::OpenOptions::new().append(true).open(&path)?;
    Ok((id, file))
}

/// A byte range of a stored result, or the lines containing `query`.
pub(crate) fn read(home: &Path, key: &str, id: &str, offset: usize, limit: usize, query: Option<&str>) -> Result<String, String> {
    let id = id.trim().trim_end_matches(".txt");
    if id.is_empty() || safe(id) != id {
        return Err(format!("invalid tool result id \"{id}\""));
    }
    let path = dir(home, key).join(format!("{id}.txt"));
    let text = std::fs::read_to_string(&path).map_err(|_| {
        format!("no stored tool result \"{id}\" in this session; copy the id exactly from the truncated tool output")
    })?;
    if let Some(query) = query.map(str::trim).filter(|q| !q.is_empty()) {
        let mut out = format!("[tool result {id}: lines containing {query:?}]\n");
        let mut n = 0;
        for (i, line) in text.lines().enumerate().filter(|(_, l)| l.contains(query)) {
            if n == QUERY_MAX_MATCHES || out.len() + line.len() > READ_MAX {
                out.push_str("[more matches not shown; refine the query or page with offset]\n");
                break;
            }
            out.push_str(&format!("{}|{line}\n", i + 1));
            n += 1;
        }
        if n == 0 {
            out.push_str("(no matches)\n");
        }
        return Ok(out.trim_end().to_string());
    }
    let start = ceil(&text, offset);
    let end = floor(&text, start.saturating_add(limit.clamp(1, READ_MAX))).max(start);
    let mut out = format!("[tool result {id}: bytes {start}-{end} of {}]\n{}", text.len(), &text[start..end]);
    if end < text.len() {
        out.push_str(&format!("\n[{} more bytes; continue with offset {end}]", text.len() - end));
    }
    Ok(out)
}

struct ReadToolResult;

#[async_trait::async_trait]
impl Tool for ReadToolResult {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: TOOL.into(),
            description: "Read a large tool result that was stored because it was too long to show in full, by the id \
                given in the truncated output. Page with offset/limit (bytes) or find lines with query. Only results \
                from this session are available; it cannot read arbitrary files."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "id": {"type": "string", "description": "Stored tool result id, copied exactly from the truncated output."},
                    "offset": {"type": "integer", "description": "Byte offset to start at (default 0)."},
                    "limit": {"type": "integer", "description": "Bytes to return (default 8192, max 65536)."},
                    "query": {"type": "string", "description": "Instead of a range, return the lines containing this literal text."}
                },
                "required": ["id"]
            }),
        }
    }

    fn summary(&self, args: &Value) -> String {
        let id = args.get("id").and_then(Value::as_str).unwrap_or("");
        match (args.get("query").and_then(Value::as_str), args.get("offset").and_then(Value::as_u64)) {
            (Some(q), _) => format!("{id} \"{q}\""),
            (None, Some(o)) => format!("{id} @{o}"),
            _ => id.to_string(),
        }
    }

    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolOutput {
        let Some(id) = args.get("id").and_then(Value::as_str) else {
            return ToolOutput::err("read_tool_result: missing required argument `id`.");
        };
        let num = |k: &str| args.get(k).and_then(Value::as_u64).map(|n| n as usize);
        let key = session_key(ctx.session_id.as_deref());
        let query = args.get("query").and_then(Value::as_str);
        match read(&crate::config::home(), &key, id, num("offset").unwrap_or(0), num("limit").unwrap_or(READ_DEFAULT), query) {
            Ok(s) => ToolOutput::ok(s),
            Err(e) => ToolOutput::err(e),
        }
    }
}

pub fn register(r: &mut Registry) {
    r.add(Box::new(ReadToolResult));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn large_results_are_stored_and_paged() {
        let home = tempfile::tempdir().unwrap();
        let h = home.path();
        assert_eq!(spill(h, "s1", "call", "bash", "small".into()), "small");

        let full: String = (0..2000).map(|i| format!("line {i:04} é\n")).collect();
        assert!(full.len() > SPILL_AT);
        let shown = spill(h, "s1", "call_1", "mcp__srv__big", full.clone());
        assert!(shown.len() < HEAD + TAIL + 600, "{}", shown.len());
        assert!(shown.starts_with("line 0000"));
        assert!(shown.contains("line 1999"));
        assert!(shown.contains("bytes omitted"));
        let id = shown.split("stored as tool result \"").nth(1).unwrap().split('"').next().unwrap().to_string();
        assert!(id.starts_with("mcp__srv__big-"));
        let file = h.join("artifacts/s1").join(format!("{id}.txt"));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), full);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
        }

        // Page through the whole thing and get it back exactly.
        let mut offset = 0;
        let mut rebuilt = String::new();
        loop {
            let page = read(h, "s1", &id, offset, 5000, None).unwrap();
            let (header, rest) = page.split_once('\n').unwrap();
            let range = header.split("bytes ").nth(1).unwrap().split(' ').next().unwrap();
            let (a, b) = range.split_once('-').unwrap();
            let (a, b): (usize, usize) = (a.parse().unwrap(), b.parse().unwrap());
            assert_eq!(a, offset);
            rebuilt.push_str(&rest[..b - a]);
            if b == full.len() {
                assert!(!page.contains("more bytes"));
                break;
            }
            assert!(page.contains(&format!("continue with offset {b}")));
            offset = b;
        }
        assert_eq!(rebuilt, full);

        // An offset inside a multi-byte character snaps forward.
        let mid = full.find('é').unwrap() + 1;
        assert!(read(h, "s1", &id, mid, 10, None).unwrap().contains(&format!("bytes {}-", mid + 1)));
        let found = read(h, "s1", &id, 0, 0, Some("line 1234")).unwrap();
        assert!(found.contains("1235|line 1234 é"), "{found}");
        assert!(read(h, "s1", &id, 0, 0, Some("nope")).unwrap().contains("(no matches)"));

        assert!(read(h, "s1", "../../etc/passwd", 0, 10, None).unwrap_err().contains("invalid"));
        assert!(read(h, "other-session", &id, 0, 10, None).unwrap_err().contains("no stored tool result"));
        // The reader's own output is never stored again.
        assert_eq!(spill(h, "s1", "c", TOOL, full.clone()), full);
        assert_eq!(spill(h, "s1", "c", "skill", full.clone()), full, "skills are chunked by skill_chunk_bytes");

        let _ = spill(h, "s2", "other", "bash", full);
        remove_session(h, "s1");
        assert!(!h.join("artifacts/s1").exists());
        assert!(h.join("artifacts/s2").exists());
        remove_session(h, "../../etc");
        assert!(h.join("artifacts/s2").exists());
        assert!(h.exists());
    }
}
