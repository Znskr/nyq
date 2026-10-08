//! Workspace file tools: read, write, edit, list.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};
use similar::udiff;
use similar::Algorithm;

use super::{Tool, ToolCtx, ToolOutput, Registry};
use crate::types::ToolSpec;

/// Resolve a possibly relative path against `cwd`.
fn resolve(cwd: &Path, p: &str) -> PathBuf {
    let path = Path::new(p);
    if path.is_absolute() { path.to_path_buf() } else { cwd.join(path) }
}

/// Path as shown to the user: relative to the process cwd when inside it.
fn display_path(path: &Path) -> String {
    let cwd = std::env::current_dir().unwrap_or_default();
    path.strip_prefix(&cwd)
        .map(|r| if r.as_os_str().is_empty() { ".".into() } else { super::slash_path(r) })
        .unwrap_or_else(|_| super::slash_path(path))
}

/// Path argument resolved against the process cwd (for `summary`, which has no ctx).
fn summary_path(args: &Value) -> String {
    let cwd = std::env::current_dir().unwrap_or_default();
    let p = args.get("path").and_then(Value::as_str).unwrap_or(".");
    display_path(&resolve(&cwd, p))
}

fn unified_diff(path: &Path, old: &str, new: &str) -> String {
    let p = display_path(path);
    let (a, b) = (format!("a/{p}"), format!("b/{p}"));
    udiff::unified_diff(Algorithm::Myers, old, new, 3, Some((a.as_str(), b.as_str())))
}

/// First `n` lines of `text`, with a truncation marker.
fn head(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = lines.iter().take(n).copied().collect::<Vec<_>>().join("\n");
    if lines.len() > n {
        out.push_str("\n…");
    }
    out
}

fn int_arg(args: &Value, name: &str) -> Option<i64> {
    args.get(name).and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)))
}

/// `edit_file` refuses files larger than this rather than rewriting a prefix.
const MAX_SCAN_BYTES: u64 = 8 * 1024 * 1024;
const MAX_LINE_BYTES: usize = 1024 * 1024;
const DISPLAY_CHARS: usize = 2000;

/// Open a regular file. Directories and devices are rejected before any byte
/// is copied (so `/dev/zero` cannot grow without bound).
fn open_regular(path: &Path) -> Result<fs::File, ToolOutput> {
    // Windows refuses to open a directory at all, so name the problem first.
    if fs::metadata(path).is_ok_and(|m| m.is_dir()) {
        return Err(ToolOutput::err(format!("read_file: {} is a directory (use list_dir).", display_path(path))));
    }
    let file = match fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(ToolOutput::err(format!("read_file: file not found: {}", display_path(path))));
        }
        Err(e) => return Err(ToolOutput::err(format!("read_file: {e}: {}", display_path(path)))),
    };
    let meta = match file.metadata() {
        Ok(m) => m,
        Err(e) => return Err(ToolOutput::err(format!("read_file: {e}: {}", display_path(path)))),
    };
    if meta.is_dir() {
        return Err(ToolOutput::err(format!("read_file: {} is a directory (use list_dir).", display_path(path))));
    }
    if !meta.is_file() {
        return Err(ToolOutput::err(format!("read_file: {} is not a regular file.", display_path(path))));
    }
    Ok(file)
}

enum ScanLine {
    Eof,
    Line(Vec<u8>),
    /// No newline within [`MAX_LINE_BYTES`]. `prefix` is that many bytes;
    /// the rest of the line has been discarded.
    TooLong(Vec<u8>),
}

fn fill_has(buf: &[u8], byte: u8) -> Option<usize> {
    buf.iter().position(|b| *b == byte)
}

/// Next logical line, bounded to [`MAX_LINE_BYTES`]. A NUL anywhere is an error.
fn next_line(reader: &mut impl std::io::BufRead, path: &Path) -> Result<ScanLine, ToolOutput> {
    let mut buf = Vec::new();
    loop {
        let available = match reader.fill_buf() {
            Ok(b) => b,
            Err(e) => return Err(ToolOutput::err(format!("read_file: {e}: {}", display_path(path)))),
        };
        if available.is_empty() {
            return Ok(if buf.is_empty() { ScanLine::Eof } else { ScanLine::Line(buf) });
        }
        let room = MAX_LINE_BYTES - buf.len();
        let scan = &available[..available.len().min(room)];
        if let Some(i) = fill_has(scan, 0) {
            if fill_has(&scan[..i], b'\n').is_none() {
                return Err(ToolOutput::err(format!("read_file: {} is a binary file.", display_path(path))));
            }
        }
        if let Some(i) = fill_has(scan, b'\n') {
            buf.extend_from_slice(&available[..=i]);
            reader.consume(i + 1);
            return Ok(ScanLine::Line(buf));
        }
        if available.len() > room {
            buf.extend_from_slice(scan);
            reader.consume(room);
            skip_rest(reader, path)?;
            return Ok(ScanLine::TooLong(buf));
        }
        let n = available.len();
        buf.extend_from_slice(available);
        reader.consume(n);
    }
}

fn skip_rest(reader: &mut impl std::io::BufRead, path: &Path) -> Result<(), ToolOutput> {
    loop {
        let available = match reader.fill_buf() {
            Ok(b) => b,
            Err(e) => return Err(ToolOutput::err(format!("read_file: {e}: {}", display_path(path)))),
        };
        if available.is_empty() {
            return Ok(());
        }
        if let Some(i) = fill_has(available, 0) {
            if fill_has(&available[..i], b'\n').is_none() {
                return Err(ToolOutput::err(format!("read_file: {} is a binary file.", display_path(path))));
            }
        }
        if let Some(i) = fill_has(available, b'\n') {
            reader.consume(i + 1);
            return Ok(());
        }
        let n = available.len();
        reader.consume(n);
    }
}

fn line_text(bytes: &[u8], path: &Path, cut: bool) -> Result<String, ToolOutput> {
    let bytes = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    let bytes = bytes.strip_suffix(b"\r").unwrap_or(bytes);
    match std::str::from_utf8(bytes) {
        Ok(t) => Ok(t.to_string()),
        Err(e) if cut && e.error_len().is_none() => match std::str::from_utf8(&bytes[..e.valid_up_to()]) {
            Ok(t) => Ok(t.to_string()),
            Err(_) => Err(ToolOutput::err(format!("read_file: {} is not UTF-8 text.", display_path(path)))),
        },
        Err(_) => Err(ToolOutput::err(format!("read_file: {} is not UTF-8 text.", display_path(path)))),
    }
}

fn push_numbered(out: &mut String, n: u64, line: &str, exceeds: bool) {
    if exceeds || line.chars().count() > DISPLAY_CHARS {
        let shown: String = line.chars().take(DISPLAY_CHARS).collect();
        let note = if exceeds { " [line exceeds 1 MiB]" } else { "" };
        out.push_str(&format!("{n:>6}\t{shown}…{note}\n"));
    } else {
        out.push_str(&format!("{n:>6}\t{line}\n"));
    }
}

// --------------------------------------------------------------------------- read_file

struct ReadFile;

#[async_trait::async_trait]
impl Tool for ReadFile {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "read_file".into(),
            description: concat!(
                "Read a text file from the workspace, returning numbered lines in the form \"     N\\tline\". ",
                "Starts at 1-based line `offset` and returns up to `limit` lines (default 2000); ",
                "long lines are truncated at 2000 chars. Use offset/limit to page through large files."
            )
            .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path, relative to the workspace root or absolute."},
                    "offset": {"type": "integer", "description": "1-based line number to start at."},
                    "limit": {"type": "integer", "description": "Maximum number of lines to return (default 2000)."}
                },
                "required": ["path"]
            }),
        }
    }

    fn summary(&self, args: &Value) -> String {
        summary_path(args)
    }

    fn needs_approval_for(&self, args: &Value, cwd: &Path) -> bool {
        super::outside_path_arg(cwd, args)
    }

    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolOutput {
        let Some(p) = args.get("path").and_then(Value::as_str) else {
            return ToolOutput::err("read_file: missing required argument `path`.");
        };
        let path = resolve(&ctx.cwd, p);
        if let Some(msg) = super::SecretRoots::load().reject(p, &path) {
            return ToolOutput::err(format!("read_file: {msg}"));
        }
        let offset = int_arg(&args, "offset").unwrap_or(1);
        let limit = int_arg(&args, "limit").unwrap_or(2000);
        if offset < 1 {
            return ToolOutput::err(format!("read_file: offset must be at least 1, got {offset}."));
        }
        if limit < 1 {
            return ToolOutput::err(format!("read_file: limit must be at least 1, got {limit}."));
        }
        let file = match open_regular(&path) {
            Ok(f) => f,
            Err(e) => return e,
        };
        let mut reader = std::io::BufReader::new(file);
        let mut out = String::new();
        let mut line_no: u64 = 0;
        let mut emitted: u64 = 0;
        let mut more: u64 = 0;
        let limit = limit as u64;
        let offset = offset as u64;
        loop {
            let scanned = match next_line(&mut reader, &path) {
                Ok(s) => s,
                Err(e) => return e,
            };
            let (bytes, exceeds) = match scanned {
                ScanLine::Eof => break,
                ScanLine::Line(b) => (b, false),
                ScanLine::TooLong(b) => (b, true),
            };
            let text = match line_text(&bytes, &path, exceeds) {
                Ok(t) => t,
                Err(e) => return e,
            };
            line_no += 1;
            if line_no < offset {
                continue;
            }
            if emitted < limit {
                push_numbered(&mut out, line_no, &text, exceeds);
                emitted += 1;
            } else {
                more += 1;
            }
        }
        if line_no == 0 {
            return ToolOutput::ok(format!("read_file: {} is empty.", display_path(&path)));
        }
        if emitted == 0 {
            return ToolOutput::err(format!(
                "read_file: offset {offset} is past the end of {} ({line_no} lines).",
                display_path(&path)
            ));
        }
        if more > 0 {
            out.push_str(&format!("({more} more lines below — use offset {} to read on)\n", offset + emitted));
        }
        ToolOutput::ok(out)
    }
}

// --------------------------------------------------------------------------- list_dir

struct ListDir;

#[async_trait::async_trait]
impl Tool for ListDir {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "list_dir".into(),
            description: "List the entries of a directory: sorted, one per line, up to 500 entries, \
                directories suffixed with '/'. Defaults to the workspace root."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Directory to list, relative to the workspace root or absolute (default: the root)."}
                }
            }),
        }
    }

    fn summary(&self, args: &Value) -> String {
        summary_path(args)
    }

    fn needs_approval_for(&self, args: &Value, cwd: &Path) -> bool {
        super::outside_path_arg(cwd, args)
    }

    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolOutput {
        let user = args.get("path").and_then(Value::as_str).unwrap_or(".");
        let path = resolve(&ctx.cwd, user);
        if let Some(msg) = super::SecretRoots::load().reject(user, &path) {
            return ToolOutput::err(format!("list_dir: {msg}"));
        }
        if !path.is_dir() {
            if !path.exists() {
                return ToolOutput::err(format!("list_dir: no such directory: {}", display_path(&path)));
            }
            return ToolOutput::err(format!("list_dir: {} is not a directory.", display_path(&path)));
        }
        let entries = match fs::read_dir(&path) {
            Ok(rd) => rd,
            Err(e) => return ToolOutput::err(format!("list_dir: {e}: {}", display_path(&path))),
        };
        let mut names: Vec<String> = entries
            .filter_map(|e| e.ok())
            .map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                if e.file_type().map(|t| t.is_dir()).unwrap_or(false) { format!("{name}/") } else { name }
            })
            .collect();
        names.sort();
        let total = names.len();
        if total == 0 {
            return ToolOutput::ok(format!("list_dir: {} is empty.", display_path(&path)));
        }
        let mut out = names.into_iter().take(500).collect::<Vec<_>>().join("\n");
        if total > 500 {
            out.push_str(&format!("\n({} more entries)", total - 500));
        }
        ToolOutput::ok(out)
    }
}

// -------------------------------------------------------------------------- write_file

/// Diff / new-file text for writing `content` to `path`. No side effects.
fn write_preview(path: &Path, content: &str) -> String {
    match super::safeio::read_capped(path, MAX_SCAN_BYTES) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            format!("new file: {}\n{}", display_path(path), head(content, 30))
        }
        Err(_) => format!("cannot preview {}", display_path(path)),
        Ok(bytes) if bytes.len() as u64 > MAX_SCAN_BYTES => {
            format!("replaces a file larger than 8 MiB: {}", display_path(path))
        }
        Ok(bytes) => {
            let len = bytes.len();
            match String::from_utf8(bytes) {
                Ok(old) => unified_diff(path, &old, content),
                Err(_) => format!("replaces binary file: {} ({len} bytes)", display_path(path)),
            }
        }
    }
}

struct WriteFile;

#[async_trait::async_trait]
impl Tool for WriteFile {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "write_file".into(),
            description: "Write text to a file, creating parent directories as needed. Overwrites \
                any existing content; the approval prompt shows a diff of old vs new."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path, relative to the workspace root or absolute."},
                    "content": {"type": "string", "description": "Full new content of the file."}
                },
                "required": ["path", "content"]
            }),
        }
    }

    fn needs_approval(&self, _args: &Value) -> bool {
        true
    }

    fn summary(&self, args: &Value) -> String {
        summary_path(args)
    }

    fn preview(&self, args: &Value, ctx: &ToolCtx) -> Option<String> {
        let p = args.get("path")?.as_str()?;
        let content = args.get("content")?.as_str()?;
        Some(write_preview(&resolve(&ctx.cwd, p), content))
    }

    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolOutput {
        let Some(p) = args.get("path").and_then(Value::as_str) else {
            return ToolOutput::err("write_file: missing required argument `path`.");
        };
        let Some(content) = args.get("content").and_then(Value::as_str) else {
            return ToolOutput::err("write_file: missing required argument `content`.");
        };
        let path = resolve(&ctx.cwd, p);
        if let Some(msg) = sandbox_denied(ctx, p, &path, "write_file") {
            return ToolOutput::err(msg);
        }
        let preview = write_preview(&path, content);
        if let Err(e) = commit_bytes(ctx, p, &path, content.as_bytes()) {
            return ToolOutput::err(format!("write_file: {e}: {}", display_path(&path)));
        }
        ToolOutput {
            content: format!("Wrote {} lines to {}.", content.lines().count(), display_path(&path)),
            display: (!preview.is_empty()).then_some(preview),
            ..ToolOutput::default()
        }
    }
}

// --------------------------------------------------------------------------- edit_file

struct EditFile;

#[async_trait::async_trait]
impl Tool for EditFile {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "edit_file".into(),
            description: "Edit a text file by replacing `old_string` with `new_string` (exact text \
                match). Fails if `old_string` is not found, or if it occurs more than once unless \
                `replace_all` is true (then all occurrences are replaced). Preserves the file's \
                existing line endings; the approval prompt shows the resulting diff."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path, relative to the workspace root or absolute."},
                    "old_string": {"type": "string", "description": "Exact text to replace (include surrounding lines to make it unique)."},
                    "new_string": {"type": "string", "description": "Replacement text."},
                    "replace_all": {"type": "boolean", "description": "Replace every occurrence (default false)."}
                },
                "required": ["path", "old_string", "new_string"]
            }),
        }
    }

    fn needs_approval(&self, _args: &Value) -> bool {
        true
    }

    fn summary(&self, args: &Value) -> String {
        summary_path(args)
    }

    fn preview(&self, args: &Value, ctx: &ToolCtx) -> Option<String> {
        let p = args.get("path")?.as_str()?;
        let path = resolve(&ctx.cwd, p);
        let (old, new) = apply_edit(ctx, p, &path, args).ok()?;
        Some(unified_diff(&path, &old, &new))
    }

    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolOutput {
        let Some(p) = args.get("path").and_then(Value::as_str) else {
            return ToolOutput::err("edit_file: missing required argument `path`.");
        };
        if args.get("old_string").and_then(Value::as_str).is_none() {
            return ToolOutput::err("edit_file: missing required argument `old_string`.");
        }
        if args.get("new_string").and_then(Value::as_str).is_none() {
            return ToolOutput::err("edit_file: missing required argument `new_string`.");
        }
        let path = resolve(&ctx.cwd, p);
        if let Some(msg) = sandbox_denied(ctx, p, &path, "edit_file") {
            return ToolOutput::err(msg);
        }
        let (old, new) = match apply_edit(ctx, p, &path, &args) {
            Ok(v) => v,
            Err(e) => return e,
        };
        let diff = unified_diff(&path, &old, &new);
        if let Err(e) = commit_bytes(ctx, p, &path, new.as_bytes()) {
            return ToolOutput::err(format!("edit_file: {e}: {}", display_path(&path)));
        }
        ToolOutput {
            content: format!("Edited {}.", display_path(&path)),
            display: (!diff.is_empty()).then_some(diff),
            ..ToolOutput::default()
        }
    }
}

/// `None` when this write is allowed by the sandbox. Read-only refuses every
/// write; workspace-write allows the workspace and any extra writable roots.
fn sandbox_denied(ctx: &ToolCtx, user_path: &str, path: &Path, tool: &str) -> Option<String> {
    let allowed = match ctx.sandbox.mode {
        crate::sandbox::Mode::Off => true,
        crate::sandbox::Mode::ReadOnly => false,
        crate::sandbox::Mode::WorkspaceWrite => {
            crate::agent::inside(&ctx.cwd, user_path)
                || ctx.sandbox.writable.iter().any(|root| {
                    crate::agent::inside(root, path.to_str().unwrap_or(""))
                })
        }
    };
    if allowed {
        return None;
    }
    Some(match ctx.sandbox.mode {
        crate::sandbox::Mode::ReadOnly => format!("{tool}: sandbox is read-only; refusing to write {}", display_path(path)),
        _ => format!(
            "{tool}: sandbox is workspace-write; refusing to write outside the workspace: {}",
            display_path(path)
        ),
    })
}

/// Root a sandboxed write must stay under, when one applies.
fn write_root<'a>(ctx: &'a ToolCtx, user_path: &str, path: &Path) -> Option<&'a Path> {
    if crate::agent::inside(&ctx.cwd, user_path) {
        return Some(ctx.cwd.as_path());
    }
    let shown = path.to_str().unwrap_or("");
    ctx.sandbox
        .writable
        .iter()
        .find(|root| crate::agent::inside(root, user_path) || crate::agent::inside(root, shown))
        .map(std::path::PathBuf::as_path)
}

fn relative_in(root: &Path, user_path: &str, path: &Path) -> std::io::Result<std::path::PathBuf> {
    super::safeio::relative_under(root, user_path)
        .or_else(|| path.to_str().and_then(|p| super::safeio::relative_under(root, p)))
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "path escapes the sandbox root"))
}

/// Replace `path`. Unsandboxed writes may follow a final symlink and then
/// replace that target atomically. Sandboxed writes never follow.
fn commit_bytes(ctx: &ToolCtx, user_path: &str, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if ctx.sandbox.mode == crate::sandbox::Mode::Off {
        return super::safeio::write_atomic(path, bytes);
    }
    let root = write_root(ctx, user_path, path).unwrap_or(ctx.cwd.as_path());
    let rel = relative_in(root, user_path, path)?;
    super::safeio::write_beneath(root, &rel, bytes)
}

fn load_edit_bytes(ctx: &ToolCtx, user_path: &str, path: &Path) -> Result<Vec<u8>, ToolOutput> {
    if path.is_dir() {
        return Err(ToolOutput::err(format!("edit_file: {} is a directory.", display_path(path))));
    }
    let loaded = if ctx.sandbox.mode == crate::sandbox::Mode::Off {
        super::safeio::read_capped(path, MAX_SCAN_BYTES)
    } else {
        let root = write_root(ctx, user_path, path).unwrap_or(ctx.cwd.as_path());
        match relative_in(root, user_path, path) {
            Ok(rel) => super::safeio::read_beneath(root, &rel, MAX_SCAN_BYTES),
            Err(e) => Err(e),
        }
    };
    match loaded {
        Ok(bytes) if bytes.len() as u64 > MAX_SCAN_BYTES => {
            Err(ToolOutput::err(format!("edit_file: {} is larger than 8 MiB.", display_path(path))))
        }
        Ok(bytes) => Ok(bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Err(ToolOutput::err(format!("edit_file: file not found: {}", display_path(path))))
        }
        Err(e) => Err(ToolOutput::err(format!("edit_file: {e}: {}", display_path(path)))),
    }
}

/// Pure version of an edit: (old content, new content). `Err` is the user-facing error.
fn apply_edit(ctx: &ToolCtx, user_path: &str, path: &Path, args: &Value) -> Result<(String, String), ToolOutput> {
    let old_string = args.get("old_string").and_then(Value::as_str).unwrap_or_default();
    let new_string = args.get("new_string").and_then(Value::as_str).unwrap_or_default();
    let replace_all = args.get("replace_all").and_then(Value::as_bool).unwrap_or(false);
    if old_string.is_empty() {
        return Err(ToolOutput::err("edit_file: old_string is empty."));
    }
    if old_string == new_string {
        return Err(ToolOutput::err("edit_file: old_string and new_string are identical."));
    }
    let bytes = load_edit_bytes(ctx, user_path, path)?;
    if bytes.contains(&0) {
        return Err(ToolOutput::err(format!("edit_file: {} is a binary file.", display_path(path))));
    }
    let content = match String::from_utf8(bytes) {
        Ok(t) => t,
        Err(_) => return Err(ToolOutput::err(format!("edit_file: {} is not UTF-8 text.", display_path(path)))),
    };

    // If every newline in the file is CRLF, accept plain-LF old/new strings and
    // rewrite them so the file's line endings are preserved.
    let crlf = content.contains("\r\n") && !content.replace("\r\n", "").contains('\n');
    let to_crlf =
        |s: &str| (crlf && s.contains('\n') && !s.contains("\r\n")).then(|| s.replace('\n', "\r\n"));
    let old = to_crlf(old_string).unwrap_or_else(|| old_string.to_string());
    let new = to_crlf(new_string).unwrap_or_else(|| new_string.to_string());

    let count = content.matches(&old).count();
    if count == 0 {
        return Err(ToolOutput::err(format!("edit_file: old_string not found in {}.", display_path(path))));
    }
    if count > 1 && !replace_all {
        return Err(ToolOutput::err(format!(
            "edit_file: old_string occurs {count} times in {}; include more surrounding lines to make it unique, or set replace_all to true.",
            display_path(path)
        )));
    }
    let new_content = if replace_all { content.replace(&old, &new) } else { content.replacen(&old, &new, 1) };
    Ok((content, new_content))
}

// ------------------------------------------------------------------------------- misc

pub fn register(r: &mut Registry) {
    r.add(Box::new(ReadFile));
    r.add(Box::new(ListDir));
    r.add(Box::new(WriteFile));
    r.add(Box::new(EditFile));
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_util::sync::CancellationToken;

    fn ctx(dir: &Path) -> ToolCtx {
        ToolCtx { cwd: dir.to_path_buf(), cancel: CancellationToken::new(), ..Default::default() }
    }

    fn run(tool: &dyn Tool, args: Value, dir: &Path) -> ToolOutput {
        futures::executor::block_on(tool.run(args, &ctx(dir)))
    }

    #[test]
    fn read_numbering_paging_and_long_lines() {
        let dir = tempfile::tempdir().unwrap();
        let long = "y".repeat(2500);
        std::fs::write(dir.path().join("f.txt"), format!("one\ntwo\n{long}\nfour\n")).unwrap();

        let out = run(&ReadFile, json!({"path": "f.txt"}), dir.path());
        assert!(!out.is_error);
        let lines: Vec<&str> = out.content.lines().collect();
        assert_eq!(lines[0], "     1\tone");
        assert_eq!(lines[1], "     2\ttwo");
        assert_eq!(lines[2].chars().count(), 6 + 1 + 2000 + 1); // prefix + 2000 + ellipsis
        assert_eq!(lines[3], "     4\tfour");

        let out = run(&ReadFile, json!({"path": "f.txt", "offset": 3, "limit": 1}), dir.path());
        assert!(out.content.starts_with("     3\t"));
        assert!(out.content.contains("(1 more lines below"));
        assert!(out.content.contains("use offset 4"));

        let out = run(&ReadFile, json!({"path": "f.txt", "offset": 99}), dir.path());
        assert!(out.is_error);
        assert!(out.content.contains("past the end") && out.content.contains("4 lines"));

        assert!(run(&ReadFile, json!({"path": "f.txt", "offset": 0}), dir.path()).is_error);
        assert!(run(&ReadFile, json!({"path": "f.txt", "limit": 0}), dir.path()).is_error);
    }

    #[test]
    fn read_errors() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("bin"), b"hi\0there").unwrap();
        std::fs::create_dir(dir.path().join("d")).unwrap();
        std::fs::write(dir.path().join("empty"), "").unwrap();

        let out = run(&ReadFile, json!({"path": "missing.txt"}), dir.path());
        assert!(out.is_error && out.content.contains("not found"));
        assert!(run(&ReadFile, json!({}), dir.path()).is_error);
        let out = run(&ReadFile, json!({"path": "bin"}), dir.path());
        assert!(out.is_error && out.content.contains("binary"));
        let out = run(&ReadFile, json!({"path": "d"}), dir.path());
        assert!(out.is_error && out.content.contains("directory"));
        let out = run(&ReadFile, json!({"path": "empty"}), dir.path());
        assert!(!out.is_error && out.content.contains("empty"));

        #[cfg(unix)]
        if std::path::Path::new("/dev/zero").exists() {
            let out = run(&ReadFile, json!({"path": "/dev/zero"}), dir.path());
            assert!(out.is_error && out.content.contains("regular"), "{}", out.content);
        }
    }

    #[test]
    fn list_dir_sorted_and_capped() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("zed")).unwrap();
        std::fs::write(dir.path().join("beta"), b"b").unwrap();
        std::fs::write(dir.path().join("alpha"), b"a").unwrap();
        let out = run(&ListDir, json!({}), dir.path());
        assert!(!out.is_error);
        assert_eq!(out.content.lines().collect::<Vec<_>>(), ["alpha", "beta", "zed/"]);

        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        for i in 0..505 {
            std::fs::write(sub.join(format!("{i:03}")), b"x").unwrap();
        }
        let out = run(&ListDir, json!({"path": "sub"}), dir.path());
        assert!(!out.is_error);
        assert_eq!(out.content.lines().count(), 501); // 500 entries + note
        assert!(out.content.contains("(5 more entries)"));

        assert!(run(&ListDir, json!({"path": "nope"}), dir.path()).is_error);
        assert!(run(&ListDir, json!({"path": "alpha"}), dir.path()).is_error);
    }

    #[test]
    fn write_creates_and_diffs() {
        let dir = tempfile::tempdir().unwrap();
        let args = json!({"path": "a/b/new.txt", "content": "one\ntwo\n"});
        let preview = WriteFile.preview(&args, &ctx(dir.path())).unwrap();
        assert!(preview.starts_with("new file: "));
        assert!(preview.ends_with("a/b/new.txt\none\ntwo"));
        assert!(!dir.path().join("a").exists()); // preview has no side effects

        let out = run(&WriteFile, args, dir.path());
        assert!(!out.is_error);
        assert!(out.content.contains("2 lines") && out.content.contains("a/b/new.txt"));
        assert_eq!(std::fs::read_to_string(dir.path().join("a/b/new.txt")).unwrap(), "one\ntwo\n");

        let args = json!({"path": "a/b/new.txt", "content": "one\ntwo\nthree\n"});
        let out = run(&WriteFile, args, dir.path());
        let display = out.display.unwrap();
        assert!(display.contains("--- a/") && display.contains("new.txt"));
        assert!(display.contains("+++ b/") && display.contains("+three"));
        assert_eq!(std::fs::read_to_string(dir.path().join("a/b/new.txt")).unwrap(), "one\ntwo\nthree\n");
        assert!(WriteFile.needs_approval(&json!({"path": "a/b/new.txt", "content": "x"})));

        let out = run(&WriteFile, json!({"path": "a/b"}), dir.path()); // path is a directory
        assert!(out.is_error);
    }

    #[test]
    fn edit_cases() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("e.txt");
        std::fs::write(&file, "alpha\nbeta\ngamma\nbeta\n").unwrap();

        let args = json!({"path": "e.txt", "old_string": "gamma", "new_string": "GAMMA"});
        let preview = EditFile.preview(&args, &ctx(dir.path())).unwrap();
        assert!(preview.contains("-gamma") && preview.contains("+GAMMA"));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "alpha\nbeta\ngamma\nbeta\n"); // no side effects

        let out = run(&EditFile, args, dir.path());
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "alpha\nbeta\nGAMMA\nbeta\n");

        let out = run(&EditFile, json!({"path": "e.txt", "old_string": "beta", "new_string": "x"}), dir.path());
        assert!(out.is_error && out.content.contains("2 times") && out.content.contains("replace_all"));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "alpha\nbeta\nGAMMA\nbeta\n");

        let out = run(
            &EditFile,
            json!({"path": "e.txt", "old_string": "beta\nGAMMA\n", "new_string": "two\nGAMMA\n"}),
            dir.path(),
        );
        assert!(!out.is_error);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "alpha\ntwo\nGAMMA\nbeta\n");

        let out = run(&EditFile, json!({"path": "e.txt", "old_string": "nope", "new_string": "x"}), dir.path());
        assert!(out.is_error && out.content.contains("not found"));
        let out =
            run(&EditFile, json!({"path": "e.txt", "old_string": "beta", "new_string": "beta"}), dir.path());
        assert!(out.is_error && out.content.contains("identical"));
        assert!(run(&EditFile, json!({"path": "e.txt", "old_string": "", "new_string": "x"}), dir.path()).is_error);
        assert!(run(&EditFile, json!({"path": "no.txt", "old_string": "a", "new_string": "b"}), dir.path())
            .is_error);

        let out = run(
            &EditFile,
            json!({"path": "e.txt", "old_string": "beta", "new_string": "BETA", "replace_all": true}),
            dir.path(),
        );
        assert!(!out.is_error);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "alpha\ntwo\nGAMMA\nBETA\n");
        assert!(EditFile.needs_approval(&json!({"path": "e.txt"})));
    }

    #[test]
    fn edit_preserves_crlf() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("c.txt"), "one\r\ntwo\r\n").unwrap();

        // Single-line match: the replacement's newlines become CRLF too.
        let out = run(
            &EditFile,
            json!({"path": "c.txt", "old_string": "two", "new_string": "TWO\nthree"}),
            dir.path(),
        );
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(std::fs::read_to_string(dir.path().join("c.txt")).unwrap(), "one\r\nTWO\r\nthree\r\n");

        // Multi-line LF search string still matches a CRLF file.
        let out = run(
            &EditFile,
            json!({"path": "c.txt", "old_string": "one\nTWO", "new_string": "uno"}),
            dir.path(),
        );
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(std::fs::read_to_string(dir.path().join("c.txt")).unwrap(), "uno\r\nthree\r\n");
    }

    #[test]
    fn summaries_and_specs() {
        let abs = std::env::current_dir().unwrap();
        let args = json!({"path": abs.join("src/main.rs").display().to_string()});
        assert_eq!(ReadFile.summary(&args), "src/main.rs");
        assert_eq!(EditFile.summary(&args), "src/main.rs");
        let args = json!({"path": "/definitely/outside.rs"});
        assert!(ReadFile.summary(&args).ends_with("/definitely/outside.rs"));

        let spec = ReadFile.spec();
        assert_eq!(spec.name, "read_file");
        assert_eq!(spec.parameters["required"][0], "path");
        let spec = EditFile.spec();
        assert_eq!(spec.parameters["required"], json!(["path", "old_string", "new_string"]));
        assert!(!ReadFile.needs_approval(&args));
        assert!(!ListDir.needs_approval(&args));
        assert!(WriteFile.needs_approval(&args));
    }

    fn ctx_mode(dir: &Path, mode: crate::sandbox::Mode) -> ToolCtx {
        ToolCtx {
            cwd: dir.to_path_buf(),
            cancel: CancellationToken::new(),
            sandbox: std::sync::Arc::new(crate::sandbox::Policy { mode, ..Default::default() }),
            ..Default::default()
        }
    }

    #[test]
    fn writes_follow_the_sandbox() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let outside_file = outside.path().join("x.txt");
        let abs = outside_file.to_str().unwrap().to_string();
        let escape_name = format!("nyq-write-escape-{}", std::process::id());
        let escape = dir.path().join("..").join(&escape_name);
        let _ = std::fs::remove_file(&escape);

        let ro = ctx_mode(dir.path(), crate::sandbox::Mode::ReadOnly);
        let out = futures::executor::block_on(WriteFile.run(json!({"path": "a.txt", "content": "hi"}), &ro));
        assert!(out.is_error && out.content.contains("read-only"), "{}", out.content);
        assert!(!dir.path().join("a.txt").exists());

        let ww = ctx_mode(dir.path(), crate::sandbox::Mode::WorkspaceWrite);
        let out = futures::executor::block_on(WriteFile.run(json!({"path": &abs, "content": "hi"}), &ww));
        assert!(out.is_error && out.content.contains("outside"), "{}", out.content);
        assert!(!outside_file.exists());

        let out = futures::executor::block_on(
            WriteFile.run(json!({"path": format!("../{escape_name}"), "content": "nope"}), &ww),
        );
        assert!(out.is_error, "{}", out.content);
        assert!(!escape.exists(), "write escaped to {}", escape.display());

        let out = futures::executor::block_on(WriteFile.run(json!({"path": "a.txt", "content": "hi"}), &ww));
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(std::fs::read_to_string(dir.path().join("a.txt")).unwrap(), "hi");

        let off = ctx_mode(dir.path(), crate::sandbox::Mode::Off);
        let out = futures::executor::block_on(WriteFile.run(json!({"path": &abs, "content": "out"}), &off));
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(std::fs::read_to_string(&outside_file).unwrap(), "out");

        std::fs::write(dir.path().join("e.txt"), "one").unwrap();
        let out = futures::executor::block_on(
            EditFile.run(json!({"path": &abs, "old_string": "out", "new_string": "OUT"}), &ww),
        );
        assert!(out.is_error && out.content.contains("outside"), "{}", out.content);
        assert_eq!(std::fs::read_to_string(&outside_file).unwrap(), "out");
        let out = futures::executor::block_on(
            EditFile.run(json!({"path": "e.txt", "old_string": "one", "new_string": "two"}), &ro),
        );
        assert!(out.is_error && out.content.contains("read-only"), "{}", out.content);
        assert_eq!(std::fs::read_to_string(dir.path().join("e.txt")).unwrap(), "one");

        let mut extra_ctx = ctx_mode(dir.path(), crate::sandbox::Mode::WorkspaceWrite);
        let extra = tempfile::tempdir().unwrap();
        extra_ctx.sandbox = std::sync::Arc::new(crate::sandbox::Policy {
            mode: crate::sandbox::Mode::WorkspaceWrite,
            writable: vec![extra.path().to_path_buf()],
            ..Default::default()
        });
        let dest = extra.path().join("ok.txt");
        let out = futures::executor::block_on(
            WriteFile.run(json!({"path": dest.to_str().unwrap(), "content": "ok"}), &extra_ctx),
        );
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "ok");

        #[cfg(unix)]
        {
            let link = dir.path().join("link");
            std::os::unix::fs::symlink(outside.path(), &link).unwrap();
            let out = futures::executor::block_on(
                WriteFile.run(json!({"path": "link/via-symlink.txt", "content": "x"}), &ww),
            );
            assert!(out.is_error, "{}", out.content);
            assert!(!outside.path().join("via-symlink.txt").exists());
        }
        let _ = std::fs::remove_file(&escape);

        #[cfg(unix)]
        {
            let target = dir.path().join("real.txt");
            std::fs::write(&target, "real").unwrap();
            let link = dir.path().join("alias");
            std::os::unix::fs::symlink(&target, &link).unwrap();
            let out = futures::executor::block_on(
                WriteFile.run(json!({"path": "alias", "content": "pwned"}), &ww),
            );
            #[cfg(target_os = "linux")]
            {
                assert!(!out.is_error, "{}", out.content);
                assert_eq!(std::fs::read_to_string(&target).unwrap(), "pwned");
                assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
                let out = futures::executor::block_on(EditFile.run(
                    json!({"path": "alias", "old_string": "pwned", "new_string": "edited"}),
                    &ww,
                ));
                assert!(!out.is_error, "{}", out.content);
                assert_eq!(std::fs::read_to_string(&target).unwrap(), "edited");
            }
            #[cfg(not(target_os = "linux"))]
            {
                assert!(out.is_error && out.content.contains("symlink"), "{}", out.content);
                assert_eq!(std::fs::read_to_string(&target).unwrap(), "real");
            }

            let out = futures::executor::block_on(
                WriteFile.run(json!({"path": "alias", "content": "followed"}), &off),
            );
            assert!(!out.is_error, "{}", out.content);
            assert_eq!(std::fs::read_to_string(&target).unwrap(), "followed");
        }
    }

    #[cfg(unix)]
    #[test]
    fn failed_write_keeps_the_original_bytes() {
        use std::os::unix::fs::PermissionsExt;
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("keep.txt");
        std::fs::write(&file, "original").unwrap();
        let mut perms = std::fs::metadata(dir.path()).unwrap().permissions();
        perms.set_mode(0o555);
        std::fs::set_permissions(dir.path(), perms).unwrap();
        let out = run(&WriteFile, json!({"path": "keep.txt", "content": "new"}), dir.path());
        let mut perms = std::fs::metadata(dir.path()).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(dir.path(), perms).unwrap();
        assert!(out.is_error, "{}", out.content);
        assert!(!out.changed);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "original");
    }

    #[test]
    fn read_pages_a_line_longer_than_one_mebibyte() {
        let dir = tempfile::tempdir().unwrap();
        let mut big = "x".repeat(MAX_LINE_BYTES + 8);
        big.push('\n');
        big.push_str("after\n");
        std::fs::write(dir.path().join("app.min.js"), &big).unwrap();
        let out = run(&ReadFile, json!({"path": "app.min.js", "limit": 1}), dir.path());
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("[line exceeds 1 MiB]"), "{}", out.content);
        assert!(out.content.contains('…'));
        let out = run(&ReadFile, json!({"path": "app.min.js", "offset": 2, "limit": 1}), dir.path());
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("after"), "{}", out.content);
    }

    #[test]
    fn read_reaches_past_eight_mebibytes() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let mut file = std::fs::File::create(dir.path().join("big.txt")).unwrap();
        let chunk = vec![b'a'; 1024 * 1024];
        for _ in 0..9 {
            file.write_all(&chunk).unwrap();
        }
        file.write_all(b"\nMARKER-TAIL\n").unwrap();
        let out = run(&ReadFile, json!({"path": "big.txt", "offset": 2, "limit": 1}), dir.path());
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("MARKER-TAIL"), "{}", out.content);
    }

    #[test]
    fn reads_outside_the_workspace_need_approval() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!ReadFile.needs_approval_for(&json!({"path": "notes.txt"}), dir.path()));
        assert!(ReadFile.needs_approval_for(&json!({"path": "../notes.txt"}), dir.path()));
        assert!(!ListDir.needs_approval_for(&json!({}), dir.path()));
    }

    #[test]
    fn reads_refuse_credential_files() {
        let _lock = crate::provider::sse::tests::ENV.lock().unwrap_or_else(|e| e.into_inner());
        let home = tempfile::tempdir().unwrap();
        let old_home = std::env::var_os("NYQ_HOME");
        let old_auth = std::env::var_os("NYQ_CODEX_AUTH");
        std::env::set_var("NYQ_HOME", home.path());
        std::env::remove_var("NYQ_CODEX_AUTH");
        std::fs::write(home.path().join("chatgpt-auth.json"), "refresh-token").unwrap();
        let ws = tempfile::tempdir().unwrap();
        std::fs::write(ws.path().join("ok.txt"), "hello\n").unwrap();
        let secret = home.path().join("chatgpt-auth.json");
        let out = run(&ReadFile, json!({"path": secret.to_str().unwrap()}), ws.path());
        assert!(out.is_error && out.content.contains("credential"), "{}", out.content);
        assert!(!out.content.contains("refresh-token"));
        let out = run(&ListDir, json!({"path": home.path().to_str().unwrap()}), ws.path());
        assert!(out.is_error && out.content.contains("credential"), "{}", out.content);
        let out = run(&ReadFile, json!({"path": "ok.txt"}), ws.path());
        assert!(!out.is_error && out.content.contains("hello"), "{}", out.content);
        let out = run(&ReadFile, json!({"path": "~/.ssh/id_rsa"}), ws.path());
        assert!(out.is_error && out.content.contains("credential"), "{}", out.content);
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&secret, ws.path().join("leak")).unwrap();
            let out = run(&ReadFile, json!({"path": "leak"}), ws.path());
            assert!(out.is_error && out.content.contains("credential"), "{}", out.content);
            assert!(!out.content.contains("refresh-token"));
        }
        if let Some(v) = old_home { std::env::set_var("NYQ_HOME", v); } else { std::env::remove_var("NYQ_HOME"); }
        if let Some(v) = old_auth { std::env::set_var("NYQ_CODEX_AUTH", v); } else { std::env::remove_var("NYQ_CODEX_AUTH"); }
    }
}
