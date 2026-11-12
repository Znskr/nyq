//! Workspace file tools: read, write, edit, list.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};
use similar::udiff;
use similar::Algorithm;

use super::{Registry, Tool, ToolCtx, ToolOutput};
use crate::types::ToolSpec;

/// Resolve a possibly relative path against `cwd`.
fn resolve(cwd: &Path, p: &str) -> PathBuf {
    let path = Path::new(p);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

/// Path as shown to the user: relative to the process cwd when inside it.
fn display_path(path: &Path) -> String {
    let cwd = std::env::current_dir().unwrap_or_default();
    path.strip_prefix(&cwd)
        .map(|r| {
            if r.as_os_str().is_empty() {
                ".".into()
            } else {
                super::slash_path(r)
            }
        })
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
    udiff::unified_diff(
        Algorithm::Myers,
        old,
        new,
        3,
        Some((a.as_str(), b.as_str())),
    )
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
    args.get(name)
        .and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)))
}

/// `edit_file` refuses files larger than this rather than rewriting a prefix.
const MAX_SCAN_BYTES: u64 = 8 * 1024 * 1024;
const MAX_LINE_BYTES: usize = 1024 * 1024;
const DISPLAY_CHARS: usize = 2000;

// --------------------------------------------------------------- read guard

/// Per-session record of files `read_file` returned or the write tools
/// touched, each with the size and mtime seen then. Mutating tools refuse an
/// existing file that was never recorded ("read it first") or whose stamp
/// changed ("re-read"); a missing file (a creation) passes unconditionally,
/// as does the guard when the `read_before_edit` setting is off. Files the
/// model creates are recorded, so a fresh write stays editable.
#[derive(Default)]
pub(crate) struct FileLedger {
    seen: std::sync::Mutex<HashMap<PathBuf, (u64, Option<std::time::SystemTime>)>>,
}

impl FileLedger {
    /// Canonical spelling of `path` when it exists, else lexical: a read
    /// through one spelling covers edits through a symlink alias.
    fn key(path: &Path) -> PathBuf {
        std::fs::canonicalize(path).unwrap_or_else(|_| super::lexical(path))
    }

    /// Record `path`'s current stamp (a successful read or write).
    pub(crate) fn note(&self, path: &Path) {
        if let Ok(meta) = std::fs::metadata(path) {
            self.seen
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(Self::key(path), (meta.len(), meta.modified().ok()));
        }
    }

    /// Forget every record: a fresh conversation has read nothing.
    pub(crate) fn clear(&self) {
        self.seen
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }

    /// `Err` explains to the model why `path` may not be written.
    pub(crate) fn check(&self, tool: &str, path: &Path) -> Result<(), String> {
        if !crate::settings::current().read_before_edit {
            return Ok(());
        }
        let Ok(meta) = std::fs::metadata(path) else {
            return Ok(());
        };
        let seen = self.seen.lock().unwrap_or_else(|e| e.into_inner());
        match seen.get(&Self::key(path)) {
            None => Err(format!(
                "{tool}: {} has not been read in this session; read it first.",
                display_path(path)
            )),
            Some(&(len, modified))
                if len != meta.len() || modified != meta.modified().ok() =>
            {
                Err(format!(
                    "{tool}: {} changed on disk since you last read it; re-read it.",
                    display_path(path)
                ))
            }
            Some(_) => Ok(()),
        }
    }
}

/// Open a regular file. Directories and devices are rejected before any byte
/// is copied (so `/dev/zero` cannot grow without bound).
fn open_regular(path: &Path) -> Result<fs::File, ToolOutput> {
    // Windows refuses to open a directory at all, so name the problem first.
    if fs::metadata(path).is_ok_and(|m| m.is_dir()) {
        return Err(ToolOutput::err(format!(
            "read_file: {} is a directory (use list_dir).",
            display_path(path)
        )));
    }
    let mut options = fs::OpenOptions::new();
    options.read(true);
    // A FIFO must not block before we can inspect the opened descriptor.
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = match options.open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(ToolOutput::err(format!(
                "read_file: file not found: {}",
                display_path(path)
            )));
        }
        Err(e) => {
            return Err(ToolOutput::err(format!(
                "read_file: {e}: {}",
                display_path(path)
            )))
        }
    };
    let meta = match file.metadata() {
        Ok(m) => m,
        Err(e) => {
            return Err(ToolOutput::err(format!(
                "read_file: {e}: {}",
                display_path(path)
            )))
        }
    };
    if meta.is_dir() {
        return Err(ToolOutput::err(format!(
            "read_file: {} is a directory (use list_dir).",
            display_path(path)
        )));
    }
    if !meta.is_file() {
        return Err(ToolOutput::err(format!(
            "read_file: {} is not a regular file.",
            display_path(path)
        )));
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
            Err(e) => {
                return Err(ToolOutput::err(format!(
                    "read_file: {e}: {}",
                    display_path(path)
                )))
            }
        };
        if available.is_empty() {
            return Ok(if buf.is_empty() {
                ScanLine::Eof
            } else {
                ScanLine::Line(buf)
            });
        }
        let room = MAX_LINE_BYTES - buf.len();
        let scan = &available[..available.len().min(room)];
        if let Some(i) = fill_has(scan, 0) {
            if fill_has(&scan[..i], b'\n').is_none() {
                return Err(ToolOutput::err(format!(
                    "read_file: {} is a binary file.",
                    display_path(path)
                )));
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
            Err(e) => {
                return Err(ToolOutput::err(format!(
                    "read_file: {e}: {}",
                    display_path(path)
                )))
            }
        };
        if available.is_empty() {
            return Ok(());
        }
        if let Some(i) = fill_has(available, 0) {
            if fill_has(&available[..i], b'\n').is_none() {
                return Err(ToolOutput::err(format!(
                    "read_file: {} is a binary file.",
                    display_path(path)
                )));
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
        Err(e) if cut && e.error_len().is_none() => {
            match std::str::from_utf8(&bytes[..e.valid_up_to()]) {
                Ok(t) => Ok(t.to_string()),
                Err(_) => Err(ToolOutput::err(format!(
                    "read_file: {} is not UTF-8 text.",
                    display_path(path)
                ))),
            }
        }
        Err(_) => Err(ToolOutput::err(format!(
            "read_file: {} is not UTF-8 text.",
            display_path(path)
        ))),
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
            return ToolOutput::err(format!(
                "read_file: offset must be at least 1, got {offset}."
            ));
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
            ctx.file_ledger.note(&path);
            return ToolOutput::ok(format!("read_file: {} is empty.", display_path(&path)));
        }
        if emitted == 0 {
            return ToolOutput::err(format!(
                "read_file: offset {offset} is past the end of {} ({line_no} lines).",
                display_path(&path)
            ));
        }
        if more > 0 {
            out.push_str(&format!(
                "({more} more lines below — use offset {} to read on)\n",
                offset + emitted
            ));
        }
        ctx.file_ledger.note(&path);
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
            description:
                "List the entries of a directory: sorted, one per line, up to 500 entries, \
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
                return ToolOutput::err(format!(
                    "list_dir: no such directory: {}",
                    display_path(&path)
                ));
            }
            return ToolOutput::err(format!(
                "list_dir: {} is not a directory.",
                display_path(&path)
            ));
        }
        let entries = match fs::read_dir(&path) {
            Ok(rd) => rd,
            Err(e) => return ToolOutput::err(format!("list_dir: {e}: {}", display_path(&path))),
        };
        let mut names: Vec<String> = entries
            .filter_map(|e| e.ok())
            .map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    format!("{name}/")
                } else {
                    name
                }
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
        if let Err(e) = ctx.file_ledger.check("write_file", &path) {
            return ToolOutput::err(e);
        }
        let preview = write_preview(&path, content);
        if let Err(e) = commit_bytes(ctx, p, &path, content.as_bytes()) {
            return ToolOutput::err(format!("write_file: {e}: {}", display_path(&path)));
        }
        ctx.file_ledger.note(&path);
        ToolOutput {
            content: format!(
                "Wrote {} lines to {}.",
                content.lines().count(),
                display_path(&path)
            ),
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
            description:
                "Edit a text file by replacing `old_string` with `new_string` (exact text \
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
        if let Err(e) = ctx.file_ledger.check("edit_file", &path) {
            return ToolOutput::err(e);
        }
        let (old, new) = match apply_edit(ctx, p, &path, &args) {
            Ok(v) => v,
            Err(e) => return e,
        };
        let diff = unified_diff(&path, &old, &new);
        if let Err(e) = commit_bytes(ctx, p, &path, new.as_bytes()) {
            return ToolOutput::err(format!("edit_file: {e}: {}", display_path(&path)));
        }
        ctx.file_ledger.note(&path);
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
                || ctx
                    .sandbox
                    .writable
                    .iter()
                    .any(|root| crate::agent::inside(root, path.to_str().unwrap_or("")))
        }
    };
    if allowed {
        return None;
    }
    Some(match ctx.sandbox.mode {
        crate::sandbox::Mode::ReadOnly => format!(
            "{tool}: sandbox is read-only; refusing to write {}",
            display_path(path)
        ),
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
        .or_else(|| {
            path.to_str()
                .and_then(|p| super::safeio::relative_under(root, p))
        })
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "path escapes the sandbox root",
            )
        })
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

/// Remove `path` under the same sandbox rules as [`commit_bytes`].
fn commit_delete(ctx: &ToolCtx, user_path: &str, path: &Path) -> std::io::Result<()> {
    if ctx.sandbox.mode == crate::sandbox::Mode::Off {
        return fs::remove_file(path);
    }
    let root = write_root(ctx, user_path, path).unwrap_or(ctx.cwd.as_path());
    let rel = relative_in(root, user_path, path)?;
    super::safeio::remove_beneath(root, &rel)
}

fn load_edit_bytes(
    ctx: &ToolCtx,
    user_path: &str,
    path: &Path,
    tool: &str,
) -> Result<Vec<u8>, ToolOutput> {
    if path.is_dir() {
        return Err(ToolOutput::err(format!(
            "{tool}: {} is a directory.",
            display_path(path)
        )));
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
        Ok(bytes) if bytes.len() as u64 > MAX_SCAN_BYTES => Err(ToolOutput::err(format!(
            "{tool}: {} is larger than 8 MiB.",
            display_path(path)
        ))),
        Ok(bytes) => Ok(bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(ToolOutput::err(format!(
            "{tool}: file not found: {}",
            display_path(path)
        ))),
        Err(e) => Err(ToolOutput::err(format!(
            "{tool}: {e}: {}",
            display_path(path)
        ))),
    }
}

/// Read `path` for editing: directories, binaries, non-UTF-8 text and files
/// over the scan cap are refused, exactly as `edit_file` always did.
fn load_text(
    ctx: &ToolCtx,
    user_path: &str,
    path: &Path,
    tool: &str,
) -> Result<String, ToolOutput> {
    let bytes = load_edit_bytes(ctx, user_path, path, tool)?;
    if bytes.contains(&0) {
        return Err(ToolOutput::err(format!(
            "{tool}: {} is a binary file.",
            display_path(path)
        )));
    }
    String::from_utf8(bytes).map_err(|_| {
        ToolOutput::err(format!(
            "{tool}: {} is not UTF-8 text.",
            display_path(path)
        ))
    })
}

/// Replace `old_string` with `new_string` inside `content`: exact match,
/// `replace_all` semantics, the file's line endings preserved. Errors carry
/// no tool prefix; the caller names the tool (and the edit index).
fn replace_in(
    content: &str,
    path: &Path,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
) -> Result<String, String> {
    if old_string.is_empty() {
        return Err("old_string is empty.".into());
    }
    if old_string == new_string {
        return Err("old_string and new_string are identical.".into());
    }

    // If every newline in the file is CRLF, accept plain-LF old/new strings and
    // rewrite them so the file's line endings are preserved.
    let crlf = content.contains("\r\n") && !content.replace("\r\n", "").contains('\n');
    let to_crlf = |s: &str| {
        (crlf && s.contains('\n') && !s.contains("\r\n")).then(|| s.replace('\n', "\r\n"))
    };
    let old = to_crlf(old_string).unwrap_or_else(|| old_string.to_string());
    let new = to_crlf(new_string).unwrap_or_else(|| new_string.to_string());

    let count = content.matches(&old).count();
    if count == 0 {
        return Err(format!(
            "old_string not found in {}.",
            display_path(path)
        ));
    }
    if count > 1 && !replace_all {
        return Err(format!(
            "old_string occurs {count} times in {}; include more surrounding lines to make it unique, or set replace_all to true.",
            display_path(path)
        ));
    }
    Ok(if replace_all {
        content.replace(&old, &new)
    } else {
        content.replacen(&old, &new, 1)
    })
}

/// Pure version of an edit: (old content, new content). `Err` is the user-facing error.
fn apply_edit(
    ctx: &ToolCtx,
    user_path: &str,
    path: &Path,
    args: &Value,
) -> Result<(String, String), ToolOutput> {
    let old_string = args
        .get("old_string")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let new_string = args
        .get("new_string")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let replace_all = args
        .get("replace_all")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let content = load_text(ctx, user_path, path, "edit_file")?;
    let new_content = replace_in(&content, path, old_string, new_string, replace_all)
        .map_err(|e| ToolOutput::err(format!("edit_file: {e}")))?;
    Ok((content, new_content))
}

// ------------------------------------------------------------------------- multi_edit

struct MultiEdit;

/// Pure version of a multi_edit: every `edits[i]` applied to the in-memory
/// content in order. `Err` names the failing edit's index.
fn apply_multi(
    ctx: &ToolCtx,
    user_path: &str,
    path: &Path,
    args: &Value,
) -> Result<(String, String), ToolOutput> {
    let Some(edits) = args.get("edits").and_then(Value::as_array) else {
        return Err(ToolOutput::err(
            "multi_edit: missing required argument `edits` (an array).",
        ));
    };
    if edits.is_empty() {
        return Err(ToolOutput::err("multi_edit: `edits` is empty."));
    }
    let content = load_text(ctx, user_path, path, "multi_edit")?;
    let mut current = content.clone();
    for (i, edit) in edits.iter().enumerate() {
        let old_string = edit.get("old_string").and_then(Value::as_str).ok_or_else(|| {
            ToolOutput::err(format!("multi_edit: edit {i}: missing `old_string`."))
        })?;
        let new_string = edit.get("new_string").and_then(Value::as_str).ok_or_else(|| {
            ToolOutput::err(format!("multi_edit: edit {i}: missing `new_string`."))
        })?;
        let replace_all = edit
            .get("replace_all")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        current = replace_in(&current, path, old_string, new_string, replace_all)
            .map_err(|e| ToolOutput::err(format!("multi_edit: edit {i}: {e}")))?;
    }
    Ok((content, current))
}

#[async_trait::async_trait]
impl Tool for MultiEdit {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "multi_edit".into(),
            description:
                "Apply a list of `edit_file`-style replacements to one file, in order, \
                atomically: if any edit fails nothing is written and the error names the \
                failing edit's index. Each edit is `{old_string, new_string, replace_all?}` \
                with the same matching and CRLF rules as edit_file."
                    .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path, relative to the workspace root or absolute."},
                    "edits": {
                        "type": "array",
                        "description": "Replacements applied in order; all or nothing.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "old_string": {"type": "string", "description": "Exact text to replace."},
                                "new_string": {"type": "string", "description": "Replacement text."},
                                "replace_all": {"type": "boolean", "description": "Replace every occurrence (default false)."}
                            },
                            "required": ["old_string", "new_string"]
                        },
                        "minItems": 1
                    }
                },
                "required": ["path", "edits"]
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
        let (old, new) = apply_multi(ctx, p, &path, args).ok()?;
        Some(unified_diff(&path, &old, &new))
    }

    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolOutput {
        let Some(p) = args.get("path").and_then(Value::as_str) else {
            return ToolOutput::err("multi_edit: missing required argument `path`.");
        };
        let path = resolve(&ctx.cwd, p);
        if let Some(msg) = sandbox_denied(ctx, p, &path, "multi_edit") {
            return ToolOutput::err(msg);
        }
        if let Err(e) = ctx.file_ledger.check("multi_edit", &path) {
            return ToolOutput::err(e);
        }
        let (old, new) = match apply_multi(ctx, p, &path, &args) {
            Ok(v) => v,
            Err(e) => return e,
        };
        let diff = unified_diff(&path, &old, &new);
        if let Err(e) = commit_bytes(ctx, p, &path, new.as_bytes()) {
            return ToolOutput::err(format!("multi_edit: {e}: {}", display_path(&path)));
        }
        ctx.file_ledger.note(&path);
        let n = args
            .get("edits")
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        ToolOutput {
            content: format!("Applied {n} edits to {}.", display_path(&path)),
            display: (!diff.is_empty()).then_some(diff),
            ..ToolOutput::default()
        }
    }
}

// ------------------------------------------------------------------------- edit_range

struct EditRange;

/// Pure version of an edit_range: (old content, new content). `Err` is the
/// user-facing error without the tool prefix.
fn apply_range(
    content: &str,
    path: &Path,
    start_re: &regex::Regex,
    start_near: usize,
    end_re: &regex::Regex,
    end_near: Option<usize>,
    new_text: &str,
) -> Result<String, String> {
    // (byte_start, byte_end) of each line, terminator included.
    let mut bounds: Vec<(usize, usize)> = Vec::new();
    let mut pos = 0;
    for l in content.split_inclusive('\n') {
        bounds.push((pos, pos + l.len()));
        pos += l.len();
    }
    if content.is_empty() {
        return Err(format!("{} is empty.", display_path(path)));
    }
    let line = |i: usize| content[bounds[i].0..bounds[i].1].trim_end_matches(['\r', '\n']);

    let start = bounds
        .iter()
        .enumerate()
        .filter(|(i, _)| start_re.is_match(line(*i)))
        .map(|(i, _)| i)
        .min_by_key(|i| (i + 1).abs_diff(start_near))
        .ok_or_else(|| {
            format!(
                "start_regex matches no line of {}.",
                display_path(path)
            )
        })?;
    let anchor = end_near.unwrap_or(start + 1);
    let end = bounds
        .iter()
        .enumerate()
        .skip(start)
        .filter(|(i, _)| end_re.is_match(line(*i)))
        .map(|(i, _)| i)
        .min_by_key(|i| (i + 1).abs_diff(anchor))
        .ok_or_else(|| {
            format!(
                "end_regex matches no line at or after line {} of {}.",
                start + 1,
                display_path(path)
            )
        })?;

    let from = bounds[start].0;
    let to = bounds[end].1;
    let at_eof = to == content.len();
    let had_nl = content[..to].ends_with('\n');
    let crlf = content.contains("\r\n") && !content.replace("\r\n", "").contains('\n');
    let mut text = new_text.to_string();
    if crlf && text.contains('\n') && !text.contains("\r\n") {
        text = text.replace('\n', "\r\n");
    }
    if at_eof && !had_nl {
        // The replaced tail had no newline; keep the file unterminated.
        text = text.trim_end_matches(['\r', '\n']).to_string();
    } else if !text.is_empty() && !text.ends_with('\n') {
        text.push_str(if crlf { "\r\n" } else { "\n" });
    }
    Ok(format!("{}{}{}", &content[..from], text, &content[to..]))
}

#[async_trait::async_trait]
impl Tool for EditRange {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "edit_range".into(),
            description:
                "Replace a range of lines in a text file. `start_regex` picks the line \
                closest to `start_near_line`, then `end_regex` picks the match closest to \
                `end_near_line` at or after the start; the inclusive range is replaced by \
                `new_text`. Regexes match against single lines, so this stays correct when \
                line numbers drift. The approval prompt shows the resulting diff."
                    .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path, relative to the workspace root or absolute."},
                    "start_regex": {"type": "string", "description": "Matches the first line of the range (single-line regex)."},
                    "start_near_line": {"type": "integer", "description": "1-based hint for the start match (default: first match)."},
                    "end_regex": {"type": "string", "description": "Matches the last line of the range, at or after the start (single-line regex)."},
                    "end_near_line": {"type": "integer", "description": "1-based hint for the end match (default: first match after the start)."},
                    "new_text": {"type": "string", "description": "Text replacing the whole inclusive range."}
                },
                "required": ["path", "start_regex", "end_regex", "new_text"]
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
        let (old, new) = apply_range_args(ctx, p, &path, args).ok()?;
        Some(unified_diff(&path, &old, &new))
    }

    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolOutput {
        let Some(p) = args.get("path").and_then(Value::as_str) else {
            return ToolOutput::err("edit_range: missing required argument `path`.");
        };
        let path = resolve(&ctx.cwd, p);
        if let Some(msg) = sandbox_denied(ctx, p, &path, "edit_range") {
            return ToolOutput::err(msg);
        }
        if let Err(e) = ctx.file_ledger.check("edit_range", &path) {
            return ToolOutput::err(e);
        }
        let (old, new) = match apply_range_args(ctx, p, &path, &args) {
            Ok(v) => v,
            Err(e) => return e,
        };
        let diff = unified_diff(&path, &old, &new);
        if let Err(e) = commit_bytes(ctx, p, &path, new.as_bytes()) {
            return ToolOutput::err(format!("edit_range: {e}: {}", display_path(&path)));
        }
        ctx.file_ledger.note(&path);
        ToolOutput {
            content: format!("Edited {}.", display_path(&path)),
            display: (!diff.is_empty()).then_some(diff),
            ..ToolOutput::default()
        }
    }
}

/// Shared argument handling for `EditRange::preview` and `run`.
fn apply_range_args(
    ctx: &ToolCtx,
    user_path: &str,
    path: &Path,
    args: &Value,
) -> Result<(String, String), ToolOutput> {
    let missing = |name: &str| {
        ToolOutput::err(format!("edit_range: missing required argument `{name}`."))
    };
    let Some(start_src) = args.get("start_regex").and_then(Value::as_str) else {
        return Err(missing("start_regex"));
    };
    let Some(end_src) = args.get("end_regex").and_then(Value::as_str) else {
        return Err(missing("end_regex"));
    };
    let Some(new_text) = args.get("new_text").and_then(Value::as_str) else {
        return Err(missing("new_text"));
    };
    if start_src.contains('\n') || end_src.contains('\n') {
        return Err(ToolOutput::err(
            "edit_range: regexes must match single lines.",
        ));
    }
    let start_re = regex::Regex::new(start_src).map_err(|e| {
        ToolOutput::err(format!("edit_range: invalid start_regex: {e}"))
    })?;
    let end_re = regex::Regex::new(end_src).map_err(|e| {
        ToolOutput::err(format!("edit_range: invalid end_regex: {e}"))
    })?;
    let near = |name: &str| -> Result<Option<usize>, ToolOutput> {
        match int_arg(args, name) {
            None => Ok(None),
            Some(n) if n >= 1 => Ok(Some(n as usize)),
            Some(n) => Err(ToolOutput::err(format!(
                "edit_range: {name} must be at least 1, got {n}."
            ))),
        }
    };
    let start_near = near("start_near_line")?.unwrap_or(1);
    let end_near = near("end_near_line")?;
    let content = load_text(ctx, user_path, path, "edit_range")?;
    let new = apply_range(
        &content, path, &start_re, start_near, &end_re, end_near, new_text,
    )
    .map_err(|e| ToolOutput::err(format!("edit_range: {e}")))?;
    Ok((content, new))
}

// ------------------------------------------------------------------------- apply_patch

struct ApplyPatch;

/// The writes a parsed patch would make: `(user path, resolved path)` plus
/// the computed new content. Everything is validated before anything writes.
enum Planned {
    Write { bytes: Vec<u8> },
    Delete,
}

struct PlannedFile {
    user_path: String,
    path: PathBuf,
    plan: Planned,
}

/// Parse `args` and compute every file's outcome without writing. Also the
/// preview's source of truth, so what the approver sees is what runs.
fn plan_patch(
    ctx: &ToolCtx,
    args: &Value,
) -> Result<Vec<PlannedFile>, ToolOutput> {
    let Some(text) = args.get("patch").and_then(Value::as_str) else {
        return Err(ToolOutput::err(
            "apply_patch: missing required argument `patch`.",
        ));
    };
    let files = super::patch::parse(text).map_err(|e| {
        ToolOutput::err(format!("apply_patch: {e}"))
    })?;
    let mut planned = Vec::with_capacity(files.len());
    for file in &files {
        let path = resolve(&ctx.cwd, &file.path);
        if let Some(msg) = sandbox_denied(ctx, &file.path, &path, "apply_patch") {
            return Err(ToolOutput::err(msg));
        }
        match &file.change {
            super::patch::Change::Create(lines) => {
                if path.exists() {
                    return Err(ToolOutput::err(format!(
                        "apply_patch: {} already exists.",
                        display_path(&path)
                    )));
                }
                planned.push(PlannedFile {
                    user_path: file.path.clone(),
                    path,
                    plan: Planned::Write {
                        bytes: super::patch::created(lines).into_bytes(),
                    },
                });
            }
            super::patch::Change::Delete => {
                if let Err(e) = ctx.file_ledger.check("apply_patch", &path) {
                    return Err(ToolOutput::err(e));
                }
                if !path.exists() {
                    return Err(ToolOutput::err(format!(
                        "apply_patch: file not found: {}",
                        display_path(&path)
                    )));
                }
                planned.push(PlannedFile {
                    user_path: file.path.clone(),
                    path,
                    plan: Planned::Delete,
                });
            }
            super::patch::Change::Update(_) => {
                if let Err(e) = ctx.file_ledger.check("apply_patch", &path) {
                    return Err(ToolOutput::err(e));
                }
                let old = load_text(ctx, &file.path, &path, "apply_patch")?;
                let new = super::patch::apply(&file.path, &old, &file.change)
                    .map_err(|e| ToolOutput::err(format!("apply_patch: {e}")))?;
                planned.push(PlannedFile {
                    user_path: file.path.clone(),
                    path,
                    plan: Planned::Write {
                        bytes: new.into_bytes(),
                    },
                });
            }
        }
    }
    Ok(planned)
}

/// The approval preview: one section per touched file.
fn patch_preview(ctx: &ToolCtx, args: &Value) -> Option<String> {
    let planned = plan_patch(ctx, args).ok()?;
    let mut out = String::new();
    for file in &planned {
        if !out.is_empty() {
            out.push('\n');
        }
        match &file.plan {
            Planned::Delete => {
                let old = load_edit_bytes(ctx, &file.user_path, &file.path, "apply_patch")
                    .ok()
                    .and_then(|b| String::from_utf8(b).ok())
                    .unwrap_or_default();
                out.push_str(&unified_diff(&file.path, &old, ""));
            }
            Planned::Write { bytes } => {
                let new = String::from_utf8_lossy(bytes);
                match super::safeio::read_capped(&file.path, MAX_SCAN_BYTES) {
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        out.push_str(&format!(
                            "new file: {}\n{}",
                            display_path(&file.path),
                            head(&new, 30)
                        ));
                    }
                    _ => {
                        let old = super::safeio::read_capped(&file.path, MAX_SCAN_BYTES)
                            .ok()
                            .and_then(|b| String::from_utf8(b).ok())
                            .unwrap_or_default();
                        out.push_str(&unified_diff(&file.path, &old, &new));
                    }
                }
            }
        }
    }
    Some(out)
}

#[async_trait::async_trait]
impl Tool for ApplyPatch {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "apply_patch".into(),
            description:
                "Apply a patch to one or more files. Accepts a standard unified diff \
                (multi-file, tolerant of small line-offset and trailing-whitespace fuzz) \
                and the Codex format (`*** Begin Patch` with `*** Update File:` / \
                `*** Add File:` / `*** Delete File:` sections, ending `*** End Patch`). \
                Every hunk of every file is validated before anything is written; on a \
                mid-write failure earlier files are restored."
                    .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "patch": {"type": "string", "description": "The full patch text (unified diff or `*** Begin Patch` format)."}
                },
                "required": ["patch"]
            }),
        }
    }

    fn needs_approval(&self, _args: &Value) -> bool {
        true
    }

    fn summary(&self, args: &Value) -> String {
        let paths = args
            .get("patch")
            .and_then(Value::as_str)
            .map(super::patch::paths)
            .unwrap_or_default();
        match paths.len() {
            0 => String::new(),
            1 => paths[0].clone(),
            n => format!("{} (+{} more)", paths[0], n - 1),
        }
    }

    fn preview(&self, args: &Value, ctx: &ToolCtx) -> Option<String> {
        patch_preview(ctx, args)
    }

    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolOutput {
        let planned = match plan_patch(ctx, &args) {
            Ok(p) => p,
            Err(e) => return e,
        };
        let preview = patch_preview(ctx, &args).unwrap_or_default();
        // Snapshot for rollback, then write. A failure after some commits
        // restores them best-effort.
        let mut done: Vec<(PathBuf, String, Option<Vec<u8>>)> = Vec::new();
        for file in &planned {
            let before = super::safeio::read_capped(&file.path, MAX_SCAN_BYTES).ok();
            let result = match &file.plan {
                Planned::Write { bytes } => commit_bytes(ctx, &file.user_path, &file.path, bytes),
                Planned::Delete => commit_delete(ctx, &file.user_path, &file.path),
            };
            if let Err(e) = result {
                for (path, user_path, old) in done.iter().rev() {
                    match old {
                        Some(bytes) => {
                            let _ = commit_bytes(ctx, user_path, path, bytes);
                        }
                        None => {
                            let _ = commit_delete(ctx, user_path, path);
                        }
                    }
                }
                return ToolOutput {
                    content: format!(
                        "apply_patch: {e}: {} (earlier files were restored)",
                        display_path(&file.path)
                    ),
                    is_error: true,
                    changed: !done.is_empty(),
                    ..ToolOutput::default()
                };
            }
            done.push((file.path.clone(), file.user_path.clone(), before));
        }
        for file in &planned {
            ctx.file_ledger.note(&file.path);
        }
        let names = planned
            .iter()
            .map(|f| display_path(&f.path))
            .collect::<Vec<_>>()
            .join(", ");
        ToolOutput {
            content: format!("Patched {} file(s): {names}.", planned.len()),
            display: (!preview.is_empty()).then_some(preview),
            ..ToolOutput::default()
        }
    }
}

// ------------------------------------------------------------------------------- misc

pub fn register(r: &mut Registry) {
    r.add(Box::new(ReadFile));
    r.add(Box::new(ListDir));
    r.add(Box::new(WriteFile));
    r.add(Box::new(EditFile));
    r.add(Box::new(MultiEdit));
    r.add(Box::new(EditRange));
    r.add(Box::new(ApplyPatch));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn read_rejects_fifo_without_waiting_for_a_writer() {
        use std::os::unix::ffi::OsStrExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pipe");
        let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let (tx, rx) = std::sync::mpsc::channel();
        let read_path = path.clone();
        let worker = std::thread::spawn(move || {
            tx.send(open_regular(&read_path).is_err()).unwrap();
        });
        let result = rx.recv_timeout(std::time::Duration::from_secs(2));
        if result.is_err() {
            use std::os::unix::fs::OpenOptionsExt;
            let _writer = fs::OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(path);
            worker.join().unwrap();
            panic!("read_file blocked opening a FIFO");
        }
        assert!(result.unwrap());
        worker.join().unwrap();
    }

    use tokio_util::sync::CancellationToken;

    fn ctx(dir: &Path) -> ToolCtx {
        ToolCtx {
            cwd: dir.to_path_buf(),
            cancel: CancellationToken::new(),
            ..Default::default()
        }
    }

    fn run(tool: &dyn Tool, args: Value, dir: &Path) -> ToolOutput {
        futures::executor::block_on(tool.run(args, &ctx(dir)))
    }

    fn run_ctx(tool: &dyn Tool, args: Value, ctx: &ToolCtx) -> ToolOutput {
        futures::executor::block_on(tool.run(args, ctx))
    }

    /// A `read_file` that primes the ledger, as the real flow would.
    fn prime(ctx: &ToolCtx, path: &str) {
        let out = run_ctx(&ReadFile, json!({"path": path}), ctx);
        assert!(!out.is_error, "{}", out.content);
    }

    #[test]
    fn read_numbering_paging_and_long_lines() {
        let dir = tempfile::tempdir().unwrap();
        let long = "y".repeat(2500);
        std::fs::write(
            dir.path().join("f.txt"),
            format!("one\ntwo\n{long}\nfour\n"),
        )
        .unwrap();

        let out = run(&ReadFile, json!({"path": "f.txt"}), dir.path());
        assert!(!out.is_error);
        let lines: Vec<&str> = out.content.lines().collect();
        assert_eq!(lines[0], "     1\tone");
        assert_eq!(lines[1], "     2\ttwo");
        assert_eq!(lines[2].chars().count(), 6 + 1 + 2000 + 1); // prefix + 2000 + ellipsis
        assert_eq!(lines[3], "     4\tfour");

        let out = run(
            &ReadFile,
            json!({"path": "f.txt", "offset": 3, "limit": 1}),
            dir.path(),
        );
        assert!(out.content.starts_with("     3\t"));
        assert!(out.content.contains("(1 more lines below"));
        assert!(out.content.contains("use offset 4"));

        let out = run(
            &ReadFile,
            json!({"path": "f.txt", "offset": 99}),
            dir.path(),
        );
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
            assert!(
                out.is_error && out.content.contains("regular"),
                "{}",
                out.content
            );
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
        assert_eq!(
            out.content.lines().collect::<Vec<_>>(),
            ["alpha", "beta", "zed/"]
        );

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
        let c = ctx(dir.path());
        let args = json!({"path": "a/b/new.txt", "content": "one\ntwo\n"});
        let preview = WriteFile.preview(&args, &c).unwrap();
        assert!(preview.starts_with("new file: "));
        assert!(preview.ends_with("a/b/new.txt\none\ntwo"));
        assert!(!dir.path().join("a").exists()); // preview has no side effects

        let out = run_ctx(&WriteFile, args, &c);
        assert!(!out.is_error);
        assert!(out.content.contains("2 lines") && out.content.contains("a/b/new.txt"));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a/b/new.txt")).unwrap(),
            "one\ntwo\n"
        );

        // The created file was recorded, so overwriting it is allowed.
        let args = json!({"path": "a/b/new.txt", "content": "one\ntwo\nthree\n"});
        let out = run_ctx(&WriteFile, args, &c);
        let display = out.display.unwrap();
        assert!(display.contains("--- a/") && display.contains("new.txt"));
        assert!(display.contains("+++ b/") && display.contains("+three"));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a/b/new.txt")).unwrap(),
            "one\ntwo\nthree\n"
        );
        assert!(WriteFile.needs_approval(&json!({"path": "a/b/new.txt", "content": "x"})));

        let out = run(&WriteFile, json!({"path": "a/b"}), dir.path()); // path is a directory
        assert!(out.is_error);
    }

    #[test]
    fn edit_cases() {
        let dir = tempfile::tempdir().unwrap();
        let c = ctx(dir.path());
        let file = dir.path().join("e.txt");
        std::fs::write(&file, "alpha\nbeta\ngamma\nbeta\n").unwrap();
        prime(&c, "e.txt");

        let args = json!({"path": "e.txt", "old_string": "gamma", "new_string": "GAMMA"});
        let preview = EditFile.preview(&args, &c).unwrap();
        assert!(preview.contains("-gamma") && preview.contains("+GAMMA"));
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "alpha\nbeta\ngamma\nbeta\n"
        ); // no side effects

        let out = run_ctx(&EditFile, args, &c);
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "alpha\nbeta\nGAMMA\nbeta\n"
        );

        let out = run_ctx(
            &EditFile,
            json!({"path": "e.txt", "old_string": "beta", "new_string": "x"}),
            &c,
        );
        assert!(
            out.is_error && out.content.contains("2 times") && out.content.contains("replace_all")
        );
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "alpha\nbeta\nGAMMA\nbeta\n"
        );

        let out = run_ctx(
            &EditFile,
            json!({"path": "e.txt", "old_string": "beta\nGAMMA\n", "new_string": "two\nGAMMA\n"}),
            &c,
        );
        assert!(!out.is_error);
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "alpha\ntwo\nGAMMA\nbeta\n"
        );

        let out = run_ctx(
            &EditFile,
            json!({"path": "e.txt", "old_string": "nope", "new_string": "x"}),
            &c,
        );
        assert!(out.is_error && out.content.contains("not found"));
        let out = run_ctx(
            &EditFile,
            json!({"path": "e.txt", "old_string": "beta", "new_string": "beta"}),
            &c,
        );
        assert!(out.is_error && out.content.contains("identical"));
        assert!(
            run_ctx(
                &EditFile,
                json!({"path": "e.txt", "old_string": "", "new_string": "x"}),
                &c
            )
            .is_error
        );
        assert!(
            run_ctx(
                &EditFile,
                json!({"path": "no.txt", "old_string": "a", "new_string": "b"}),
                &c
            )
            .is_error
        );

        let out = run_ctx(
            &EditFile,
            json!({"path": "e.txt", "old_string": "beta", "new_string": "BETA", "replace_all": true}),
            &c,
        );
        assert!(!out.is_error);
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "alpha\ntwo\nGAMMA\nBETA\n"
        );
        assert!(EditFile.needs_approval(&json!({"path": "e.txt"})));
    }

    #[test]
    fn edit_preserves_crlf() {
        let dir = tempfile::tempdir().unwrap();
        let c = ctx(dir.path());
        std::fs::write(dir.path().join("c.txt"), "one\r\ntwo\r\n").unwrap();
        prime(&c, "c.txt");

        // Single-line match: the replacement's newlines become CRLF too.
        let out = run_ctx(
            &EditFile,
            json!({"path": "c.txt", "old_string": "two", "new_string": "TWO\nthree"}),
            &c,
        );
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("c.txt")).unwrap(),
            "one\r\nTWO\r\nthree\r\n"
        );

        // Multi-line LF search string still matches a CRLF file.
        let out = run_ctx(
            &EditFile,
            json!({"path": "c.txt", "old_string": "one\nTWO", "new_string": "uno"}),
            &c,
        );
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("c.txt")).unwrap(),
            "uno\r\nthree\r\n"
        );
    }

    #[test]
    fn multi_edit_atomic_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let c = ctx(dir.path());
        let file = dir.path().join("m.txt");
        std::fs::write(&file, "one\ntwo\nthree\n").unwrap();
        prime(&c, "m.txt");

        // A failing edit names its index and writes nothing.
        let out = run_ctx(
            &MultiEdit,
            json!({"path": "m.txt", "edits": [
                {"old_string": "one", "new_string": "ONE"},
                {"old_string": "missing", "new_string": "x"}
            ]}),
            &c,
        );
        assert!(out.is_error, "{}", out.content);
        assert!(out.content.contains("edit 1"), "{}", out.content);
        assert!(out.content.contains("not found"), "{}", out.content);
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "one\ntwo\nthree\n"
        );

        let out = run_ctx(
            &MultiEdit,
            json!({"path": "m.txt", "edits": [
                {"old_string": "one", "new_string": "ONE"},
                {"old_string": "three", "new_string": "THREE"}
            ]}),
            &c,
        );
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("2 edits"));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "ONE\ntwo\nTHREE\n");
        let diff = out.display.unwrap();
        assert!(diff.contains("-one") && diff.contains("+ONE") && diff.contains("+THREE"));

        // replace_all inside a multi_edit, and validation errors.
        let out = run_ctx(
            &MultiEdit,
            json!({"path": "m.txt", "edits": [
                {"old_string": "t", "new_string": "T", "replace_all": true}
            ]}),
            &c,
        );
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "ONE\nTwo\nTHREE\n");

        let out = run_ctx(&MultiEdit, json!({"path": "m.txt", "edits": []}), &c);
        assert!(out.is_error && out.content.contains("empty"));
        let out = run_ctx(
            &MultiEdit,
            json!({"path": "m.txt", "edits": [{"new_string": "x"}]}),
            &c,
        );
        assert!(out.is_error && out.content.contains("edit 0"), "{}", out.content);
        assert!(MultiEdit.needs_approval(&json!({"path": "m.txt"})));
    }

    #[test]
    fn multi_edit_preserves_crlf() {
        let dir = tempfile::tempdir().unwrap();
        let c = ctx(dir.path());
        std::fs::write(dir.path().join("m.txt"), "a\r\nb\r\n").unwrap();
        prime(&c, "m.txt");
        let out = run_ctx(
            &MultiEdit,
            json!({"path": "m.txt", "edits": [{"old_string": "a\nb", "new_string": "x\ny"}]}),
            &c,
        );
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("m.txt")).unwrap(),
            "x\r\ny\r\n"
        );
    }

    #[test]
    fn edit_range_selects_and_replaces() {
        let dir = tempfile::tempdir().unwrap();
        let c = ctx(dir.path());
        let file = dir.path().join("r.rs");
        std::fs::write(
            &file,
            "fn a() {\n  1\n}\n\nfn b() {\n  2\n  3\n}\n\nfn c() {\n  4\n}\n",
        )
        .unwrap();
        prime(&c, "r.rs");

        // start_regex closest to line 5 lands on `fn b`; end_regex picks the
        // first `}` at or after it.
        let args = json!({
            "path": "r.rs",
            "start_regex": "^fn ",
            "start_near_line": 5,
            "end_regex": "^\\}",
            "new_text": "fn b() {\n  REWRITTEN\n}"
        });
        let preview = EditRange.preview(&args, &c).unwrap();
        assert!(preview.contains("-  2") && preview.contains("+  REWRITTEN"), "{preview}");
        let out = run_ctx(&EditRange, args, &c);
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "fn a() {\n  1\n}\n\nfn b() {\n  REWRITTEN\n}\n\nfn c() {\n  4\n}\n"
        );

        // end_near_line steers the end match.
        let out = run_ctx(
            &EditRange,
            json!({
                "path": "r.rs",
                "start_regex": "^fn ",
                "start_near_line": 1,
                "end_regex": "^\\}",
                "end_near_line": 9,
                "new_text": "gone"
            }),
            &c,
        );
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "gone\n\nfn c() {\n  4\n}\n"
        );

        let out = run_ctx(
            &EditRange,
            json!({"path": "r.rs", "start_regex": "nope", "end_regex": "x", "new_text": "y"}),
            &c,
        );
        assert!(out.is_error && out.content.contains("no line"), "{}", out.content);
        let out = run_ctx(
            &EditRange,
            json!({"path": "r.rs", "start_regex": "^fn c", "end_regex": "^zzz", "new_text": "y"}),
            &c,
        );
        assert!(out.is_error && out.content.contains("end_regex"), "{}", out.content);
        let out = run_ctx(
            &EditRange,
            json!({"path": "r.rs", "start_regex": "[", "end_regex": "x", "new_text": "y"}),
            &c,
        );
        assert!(out.is_error && out.content.contains("invalid start_regex"));
        assert!(EditRange.needs_approval(&json!({"path": "r.rs"})));
    }

    #[test]
    fn apply_patch_unified_multi_file() {
        let dir = tempfile::tempdir().unwrap();
        let c = ctx(dir.path());
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\nthree\n").unwrap();
        std::fs::write(dir.path().join("d.txt"), "bye\n").unwrap();
        prime(&c, "a.txt");
        prime(&c, "d.txt");

        let patch = "\
diff --git a/a.txt b/a.txt
--- a/a.txt
+++ b/a.txt
@@ -1,3 +1,3 @@
 one
-two
+TWO
 three
--- /dev/null
+++ b/new.txt
@@ -0,0 +1 @@
+made
--- a/d.txt
+++ /dev/null
@@ -1 +0,0 @@
-bye
";
        let args = json!({"patch": patch});
        let preview = ApplyPatch.preview(&args, &c).unwrap();
        assert!(preview.contains("-two") && preview.contains("+TWO"), "{preview}");
        assert!(preview.contains("new file:") && preview.contains("new.txt"));
        let out = run_ctx(&ApplyPatch, args.clone(), &c);
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "one\nTWO\nthree\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("new.txt")).unwrap(),
            "made\n"
        );
        assert!(!dir.path().join("d.txt").exists());
        assert!(ApplyPatch.needs_approval(&args));
    }

    #[test]
    fn apply_patch_codex_format() {
        let dir = tempfile::tempdir().unwrap();
        let c = ctx(dir.path());
        std::fs::write(
            dir.path().join("src/lib.rs"),
            "fn main() {\n    old();\n}\n",
        )
        .unwrap_or_else(|_| {
            std::fs::create_dir_all(dir.path().join("src")).unwrap();
            std::fs::write(dir.path().join("src/lib.rs"), "fn main() {\n    old();\n}\n").unwrap();
        });
        prime(&c, "src/lib.rs");
        let patch = "\
*** Begin Patch
*** Update File: src/lib.rs
 fn main() {
-    old();
+    new();
 }
*** Add File: notes.md
+hello
*** End Patch
";
        let out = run_ctx(&ApplyPatch, json!({"patch": patch}), &c);
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("src/lib.rs")).unwrap(),
            "fn main() {\n    new();\n}\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("notes.md")).unwrap(),
            "hello\n"
        );
    }

    #[test]
    fn apply_patch_validates_everything_before_writing() {
        let dir = tempfile::tempdir().unwrap();
        let c = ctx(dir.path());
        std::fs::write(dir.path().join("a.txt"), "keep me\n").unwrap();
        std::fs::write(dir.path().join("b.txt"), "second\n").unwrap();
        prime(&c, "a.txt");
        prime(&c, "b.txt");
        // a.txt would patch cleanly, but b.txt's hunk cannot match: nothing
        // may be written.
        let patch = "\
--- a/a.txt
+++ b/a.txt
@@ -1 +1 @@
-keep me
+changed
--- a/b.txt
+++ b/b.txt
@@ -1 +1 @@
-no such line
+changed
";
        let out = run_ctx(&ApplyPatch, json!({"patch": patch}), &c);
        assert!(out.is_error, "{}", out.content);
        assert!(out.content.contains("hunk") && out.content.contains("context"), "{}", out.content);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "keep me\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("b.txt")).unwrap(),
            "second\n"
        );

        let out = run_ctx(&ApplyPatch, json!({"patch": "garbage"}), &c);
        assert!(out.is_error);
        let out = run_ctx(&ApplyPatch, json!({}), &c);
        assert!(out.is_error);
    }

    #[test]
    fn read_before_edit_guard() {
        let dir = tempfile::tempdir().unwrap();
        let c = ctx(dir.path());
        let file = dir.path().join("g.txt");
        std::fs::write(&file, "original content\n").unwrap();

        // Never read in this session.
        let out = run_ctx(
            &EditFile,
            json!({"path": "g.txt", "old_string": "original", "new_string": "x"}),
            &c,
        );
        assert!(out.is_error && out.content.contains("read it first"), "{}", out.content);
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "original content\n"
        );

        // After a read the edit works and re-stamps the record.
        prime(&c, "g.txt");
        let out = run_ctx(
            &EditFile,
            json!({"path": "g.txt", "old_string": "original", "new_string": "updated and longer"}),
            &c,
        );
        assert!(!out.is_error, "{}", out.content);

        // An external change (different size) is caught.
        std::fs::write(&file, "outside!\n").unwrap();
        let out = run_ctx(
            &EditFile,
            json!({"path": "g.txt", "old_string": "outside", "new_string": "y"}),
            &c,
        );
        assert!(
            out.is_error && out.content.contains("changed on disk") && out.content.contains("re-read"),
            "{}",
            out.content
        );
        prime(&c, "g.txt");
        let out = run_ctx(
            &EditFile,
            json!({"path": "g.txt", "old_string": "outside", "new_string": "y"}),
            &c,
        );
        assert!(!out.is_error, "{}", out.content);

        // The other mutating tools enforce it too; creations do not.
        std::fs::write(dir.path().join("u.txt"), "unread\n").unwrap();
        let out = run_ctx(
            &MultiEdit,
            json!({"path": "u.txt", "edits": [{"old_string": "u", "new_string": "x"}]}),
            &c,
        );
        assert!(out.is_error && out.content.contains("read it first"), "{}", out.content);
        let out = run_ctx(
            &ApplyPatch,
            json!({"patch": "--- a/u.txt\n+++ b/u.txt\n@@ -1 +1 @@\n-unread\n+x\n"}),
            &c,
        );
        assert!(out.is_error && out.content.contains("read it first"), "{}", out.content);
        let out = run_ctx(
            &WriteFile,
            json!({"path": "u.txt", "content": "over\n"}),
            &c,
        );
        assert!(out.is_error && out.content.contains("read it first"), "{}", out.content);
        let out = run_ctx(
            &WriteFile,
            json!({"path": "brand-new.txt", "content": "made\n"}),
            &c,
        );
        assert!(!out.is_error, "{}", out.content);
        // A file the model just wrote stays editable.
        let out = run_ctx(
            &EditFile,
            json!({"path": "brand-new.txt", "old_string": "made", "new_string": "made2"}),
            &c,
        );
        assert!(!out.is_error, "{}", out.content);
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
        assert_eq!(
            spec.parameters["required"],
            json!(["path", "old_string", "new_string"])
        );
        assert!(!ReadFile.needs_approval(&args));
        assert!(!ListDir.needs_approval(&args));
        assert!(WriteFile.needs_approval(&args));
    }

    fn ctx_mode(dir: &Path, mode: crate::sandbox::Mode) -> ToolCtx {
        ToolCtx {
            cwd: dir.to_path_buf(),
            cancel: CancellationToken::new(),
            sandbox: std::sync::Arc::new(crate::sandbox::Policy {
                mode,
                ..Default::default()
            }),
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
        let out = futures::executor::block_on(
            WriteFile.run(json!({"path": "a.txt", "content": "hi"}), &ro),
        );
        assert!(
            out.is_error && out.content.contains("read-only"),
            "{}",
            out.content
        );
        assert!(!dir.path().join("a.txt").exists());

        let ww = ctx_mode(dir.path(), crate::sandbox::Mode::WorkspaceWrite);
        let out =
            futures::executor::block_on(WriteFile.run(json!({"path": &abs, "content": "hi"}), &ww));
        assert!(
            out.is_error && out.content.contains("outside"),
            "{}",
            out.content
        );
        assert!(!outside_file.exists());

        let out = futures::executor::block_on(WriteFile.run(
            json!({"path": format!("../{escape_name}"), "content": "nope"}),
            &ww,
        ));
        assert!(out.is_error, "{}", out.content);
        assert!(!escape.exists(), "write escaped to {}", escape.display());

        let out = futures::executor::block_on(
            WriteFile.run(json!({"path": "a.txt", "content": "hi"}), &ww),
        );
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "hi"
        );

        let off = ctx_mode(dir.path(), crate::sandbox::Mode::Off);
        let out = futures::executor::block_on(
            WriteFile.run(json!({"path": &abs, "content": "out"}), &off),
        );
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(std::fs::read_to_string(&outside_file).unwrap(), "out");

        std::fs::write(dir.path().join("e.txt"), "one").unwrap();
        prime(&ww, "e.txt");
        let out = futures::executor::block_on(EditFile.run(
            json!({"path": &abs, "old_string": "out", "new_string": "OUT"}),
            &ww,
        ));
        assert!(
            out.is_error && out.content.contains("outside"),
            "{}",
            out.content
        );
        assert_eq!(std::fs::read_to_string(&outside_file).unwrap(), "out");
        let out = futures::executor::block_on(EditFile.run(
            json!({"path": "e.txt", "old_string": "one", "new_string": "two"}),
            &ro,
        ));
        assert!(
            out.is_error && out.content.contains("read-only"),
            "{}",
            out.content
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("e.txt")).unwrap(),
            "one"
        );

        let mut extra_ctx = ctx_mode(dir.path(), crate::sandbox::Mode::WorkspaceWrite);
        let extra = tempfile::tempdir().unwrap();
        extra_ctx.sandbox = std::sync::Arc::new(crate::sandbox::Policy {
            mode: crate::sandbox::Mode::WorkspaceWrite,
            writable: vec![extra.path().to_path_buf()],
            ..Default::default()
        });
        let dest = extra.path().join("ok.txt");
        let out = futures::executor::block_on(WriteFile.run(
            json!({"path": dest.to_str().unwrap(), "content": "ok"}),
            &extra_ctx,
        ));
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
            prime(&ww, "real.txt");
            let out = futures::executor::block_on(
                WriteFile.run(json!({"path": "alias", "content": "pwned"}), &ww),
            );
            #[cfg(target_os = "linux")]
            {
                assert!(!out.is_error, "{}", out.content);
                assert_eq!(std::fs::read_to_string(&target).unwrap(), "pwned");
                assert!(std::fs::symlink_metadata(&link)
                    .unwrap()
                    .file_type()
                    .is_symlink());
                let out = futures::executor::block_on(EditFile.run(
                    json!({"path": "alias", "old_string": "pwned", "new_string": "edited"}),
                    &ww,
                ));
                assert!(!out.is_error, "{}", out.content);
                assert_eq!(std::fs::read_to_string(&target).unwrap(), "edited");
            }
            #[cfg(not(target_os = "linux"))]
            {
                assert!(
                    out.is_error && out.content.contains("symlink"),
                    "{}",
                    out.content
                );
                assert_eq!(std::fs::read_to_string(&target).unwrap(), "real");
            }

            prime(&off, "real.txt");
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
        let c = ctx(dir.path());
        let file = dir.path().join("keep.txt");
        std::fs::write(&file, "original").unwrap();
        prime(&c, "keep.txt");
        let mut perms = std::fs::metadata(dir.path()).unwrap().permissions();
        perms.set_mode(0o555);
        std::fs::set_permissions(dir.path(), perms).unwrap();
        let out = run_ctx(&WriteFile, json!({"path": "keep.txt", "content": "new"}), &c);
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
        let out = run(
            &ReadFile,
            json!({"path": "app.min.js", "limit": 1}),
            dir.path(),
        );
        assert!(!out.is_error, "{}", out.content);
        assert!(
            out.content.contains("[line exceeds 1 MiB]"),
            "{}",
            out.content
        );
        assert!(out.content.contains('…'));
        let out = run(
            &ReadFile,
            json!({"path": "app.min.js", "offset": 2, "limit": 1}),
            dir.path(),
        );
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
        let out = run(
            &ReadFile,
            json!({"path": "big.txt", "offset": 2, "limit": 1}),
            dir.path(),
        );
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
        let _lock = crate::provider::sse::tests::ENV
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home = tempfile::tempdir().unwrap();
        let old_home = std::env::var_os("NYQ_HOME");
        let old_auth = std::env::var_os("NYQ_CODEX_AUTH");
        std::env::set_var("NYQ_HOME", home.path());
        std::env::remove_var("NYQ_CODEX_AUTH");
        std::fs::write(home.path().join("chatgpt-auth.json"), "refresh-token").unwrap();
        let ws = tempfile::tempdir().unwrap();
        std::fs::write(ws.path().join("ok.txt"), "hello\n").unwrap();
        let secret = home.path().join("chatgpt-auth.json");
        let out = run(
            &ReadFile,
            json!({"path": secret.to_str().unwrap()}),
            ws.path(),
        );
        assert!(
            out.is_error && out.content.contains("credential"),
            "{}",
            out.content
        );
        assert!(!out.content.contains("refresh-token"));
        let out = run(
            &ListDir,
            json!({"path": home.path().to_str().unwrap()}),
            ws.path(),
        );
        assert!(
            out.is_error && out.content.contains("credential"),
            "{}",
            out.content
        );
        let out = run(&ReadFile, json!({"path": "ok.txt"}), ws.path());
        assert!(
            !out.is_error && out.content.contains("hello"),
            "{}",
            out.content
        );
        let out = run(&ReadFile, json!({"path": "~/.ssh/id_rsa"}), ws.path());
        assert!(
            out.is_error && out.content.contains("credential"),
            "{}",
            out.content
        );
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&secret, ws.path().join("leak")).unwrap();
            let out = run(&ReadFile, json!({"path": "leak"}), ws.path());
            assert!(
                out.is_error && out.content.contains("credential"),
                "{}",
                out.content
            );
            assert!(!out.content.contains("refresh-token"));
        }
        if let Some(v) = old_home {
            std::env::set_var("NYQ_HOME", v);
        } else {
            std::env::remove_var("NYQ_HOME");
        }
        if let Some(v) = old_auth {
            std::env::set_var("NYQ_CODEX_AUTH", v);
        } else {
            std::env::remove_var("NYQ_CODEX_AUTH");
        }
    }
}
