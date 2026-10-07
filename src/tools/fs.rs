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

    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolOutput {
        let Some(p) = args.get("path").and_then(Value::as_str) else {
            return ToolOutput::err("read_file: missing required argument `path`.");
        };
        let path = resolve(&ctx.cwd, p);
        let offset = int_arg(&args, "offset").unwrap_or(1);
        let limit = int_arg(&args, "limit").unwrap_or(2000);
        if offset < 1 {
            return ToolOutput::err(format!("read_file: offset must be at least 1, got {offset}."));
        }
        if limit < 1 {
            return ToolOutput::err(format!("read_file: limit must be at least 1, got {limit}."));
        }
        if path.is_dir() {
            return ToolOutput::err(format!("read_file: {} is a directory (use list_dir).", display_path(&path)));
        }
        let bytes = match fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return ToolOutput::err(format!("read_file: file not found: {}", display_path(&path)))
            }
            Err(e) => return ToolOutput::err(format!("read_file: {e}: {}", display_path(&path))),
        };
        if bytes.contains(&0) {
            return ToolOutput::err(format!("read_file: {} is a binary file.", display_path(&path)));
        }
        let text = match String::from_utf8(bytes) {
            Ok(t) => t,
            Err(_) => return ToolOutput::err(format!("read_file: {} is not UTF-8 text.", display_path(&path))),
        };

        let lines: Vec<&str> = text.lines().collect();
        let total = lines.len();
        if total == 0 {
            return ToolOutput::ok(format!("read_file: {} is empty.", display_path(&path)));
        }
        let start = offset as usize - 1;
        if start >= total {
            return ToolOutput::err(format!(
                "read_file: offset {offset} is past the end of {} ({total} lines).",
                display_path(&path)
            ));
        }
        let end = (start + limit as usize).min(total);
        let mut out = String::new();
        for (i, line) in lines[start..end].iter().enumerate() {
            let n = start + i + 1;
            if line.chars().count() > 2000 {
                out.push_str(&format!("{n:>6}\t{}…\n", line.chars().take(2000).collect::<String>()));
            } else {
                out.push_str(&format!("{n:>6}\t{line}\n"));
            }
        }
        if end < total {
            out.push_str(&format!("({} more lines below — use offset {} to read on)\n", total - end, end + 1));
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

    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolOutput {
        let path = resolve(&ctx.cwd, args.get("path").and_then(Value::as_str).unwrap_or("."));
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
    match fs::read(path) {
        Err(_) => format!("new file: {}\n{}", display_path(path), head(content, 30)),
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

        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                if let Err(e) = fs::create_dir_all(parent) {
                    return ToolOutput::err(format!("write_file: {e}: {}", display_path(parent)));
                }
            }
        }
        if let Err(e) = fs::write(&path, content) {
            return ToolOutput::err(format!("write_file: {e}: {}", display_path(&path)));
        }
        ToolOutput {
            content: format!("Wrote {} lines to {}.", content.lines().count(), display_path(&path)),
            is_error: false,
            display: (!preview.is_empty()).then_some(preview),
            images: Vec::new(),
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
        let (old, new) = apply_edit(&path, args).ok()?;
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
        let (old, new) = match apply_edit(&path, &args) {
            Ok(v) => v,
            Err(e) => return e,
        };
        let diff = unified_diff(&path, &old, &new);
        if let Err(e) = fs::write(&path, &new) {
            return ToolOutput::err(format!("edit_file: {e}: {}", display_path(&path)));
        }
        ToolOutput {
            content: format!("Edited {}.", display_path(&path)),
            is_error: false,
            display: (!diff.is_empty()).then_some(diff),
            images: Vec::new(),
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

/// Pure version of an edit: (old content, new content). `Err` is the user-facing error.
fn apply_edit(path: &Path, args: &Value) -> Result<(String, String), ToolOutput> {
    let old_string = args.get("old_string").and_then(Value::as_str).unwrap_or_default();
    let new_string = args.get("new_string").and_then(Value::as_str).unwrap_or_default();
    let replace_all = args.get("replace_all").and_then(Value::as_bool).unwrap_or(false);
    if old_string.is_empty() {
        return Err(ToolOutput::err("edit_file: old_string is empty."));
    }
    if old_string == new_string {
        return Err(ToolOutput::err("edit_file: old_string and new_string are identical."));
    }
    if path.is_dir() {
        return Err(ToolOutput::err(format!("edit_file: {} is a directory.", display_path(path))));
    }
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(ToolOutput::err(format!("edit_file: file not found: {}", display_path(path))))
        }
        Err(e) => return Err(ToolOutput::err(format!("edit_file: {e}: {}", display_path(path)))),
    };
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
    }
}
