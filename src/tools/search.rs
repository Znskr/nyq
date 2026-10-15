//! Workspace search tools: glob (file patterns) and grep (content regex).

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use globset::Glob;
use regex::RegexBuilder;
use serde_json::{json, Value};

use super::{Tool, ToolCtx, ToolOutput, Registry};
use crate::types::ToolSpec;

const MAX_GLOB: usize = 200;
const MAX_GREP: usize = 300;
/// Largest page `limit` the search tools accept.
const MAX_PAGE: usize = 2000;
const MAX_CONTEXT: usize = 10;
/// Files larger than this get no context lines around their matches.
const GREP_CONTEXT_FILE: u64 = 8 * 1024 * 1024;
const MAX_GREP_LINE: usize = 16 * 1024 * 1024;

/// Resolve a possibly relative path against `cwd`.
fn resolve(cwd: &Path, p: &str) -> PathBuf {
    let path = Path::new(p);
    if path.is_absolute() { path.to_path_buf() } else { cwd.join(path) }
}

/// Walk `root` respecting .gitignore (hidden files included) but never
/// descending into `.git`.
fn walk(root: &Path, secrets: &super::SecretRoots) -> ignore::Walk {
    let secrets = secrets.clone();
    ignore::WalkBuilder::new(root)
        .hidden(false)
        .require_git(false)
        .filter_entry(move |e| e.file_name() != std::ffi::OsStr::new(".git") && !secrets.blocks_path(e.path()))
        .build()
}

fn compile_glob(pattern: &str) -> Result<globset::GlobMatcher, ToolOutput> {
    Glob::new(pattern)
        .map(|g| g.compile_matcher())
        .map_err(|e| ToolOutput::err(format!("invalid glob pattern {pattern:?}: {e}")))
}

fn check_root(tool: &str, root: &Path) -> Option<ToolOutput> {
    if !root.exists() {
        Some(ToolOutput::err(format!("{tool}: no such file or directory: {}", root.display())))
    } else {
        None
    }
}

// ------------------------------------------------------------------------------- paging

/// `offset` and `limit` arguments: (offset, limit), with `limit` defaulting
/// to `default` and capped at `max`.
fn page_args(args: &Value, default: usize, max: usize) -> (usize, usize) {
    let int = |k: &str| args.get(k).and_then(|v| v.as_u64().or_else(|| v.as_f64().map(|f| f.max(0.0) as u64)));
    let offset = int("offset").unwrap_or(0) as usize;
    let limit = int("limit").map_or(default, |n| (n as usize).clamp(1, max));
    (offset, limit)
}

/// The `offset..offset+limit` page of `lines`, with a note when more follow.
/// `exact` is false when `lines` was cut short after one more than the page.
fn page(lines: Vec<String>, offset: usize, limit: usize, what: &str, exact: bool) -> String {
    let total = lines.len();
    let mut out: Vec<String> = lines.into_iter().skip(offset).take(limit).collect();
    if out.is_empty() {
        return format!("No {what} at offset {offset}.");
    }
    let end = offset + out.len();
    if end < total {
        out.push(match exact {
            true => format!("({} more {what}; continue with offset {end})", total - end),
            false => format!("(more {what}; continue with offset {end})"),
        });
    }
    out.join("\n")
}

// ------------------------------------------------------------------------------- glob

struct GlobFiles;

#[async_trait::async_trait]
impl Tool for GlobFiles {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "glob".into(),
            description: "Find files matching a glob pattern (e.g. \"**/*.rs\", \"src/**/*.ts\"), \
                respecting .gitignore. Returns paths relative to the search root, \
                newest-modified first, 200 per page (use offset to page). With mode=\"count\", \
                returns only the number of matching files."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "Glob pattern, matched against paths relative to the search root."},
                    "path": {"type": "string", "description": "Directory to search (default: the workspace root)."},
                    "mode": {"type": "string", "enum": ["list", "count"], "description": "\"list\" returns paths (default); \"count\" returns the number of matches."},
                    "offset": {"type": "integer", "description": "Skip this many paths (for paging; default 0)."},
                    "limit": {"type": "integer", "description": format!("Paths per page (default {MAX_GLOB}, max {MAX_PAGE}).")}
                },
                "required": ["pattern"]
            }),
        }
    }

    fn summary(&self, args: &Value) -> String {
        args.get("pattern").and_then(Value::as_str).unwrap_or_default().to_string()
    }

    fn needs_approval_for(&self, args: &Value, cwd: &Path) -> bool {
        super::outside_path_arg(cwd, args)
    }

    fn handles_cancel(&self) -> bool {
        true
    }

    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolOutput {
        let Some(pattern) = args.get("pattern").and_then(Value::as_str).filter(|p| !p.is_empty()) else {
            return ToolOutput::err("glob: missing required argument `pattern`.");
        };
        let count = match args.get("mode").and_then(Value::as_str).unwrap_or("list") {
            "list" => false,
            "count" => true,
            other => return ToolOutput::err(format!("glob: mode must be \"list\" or \"count\", got {other:?}.")),
        };
        let matcher = match compile_glob(pattern) {
            Ok(m) => m,
            Err(e) => return e,
        };
        let user = args.get("path").and_then(Value::as_str).unwrap_or(".");
        let root = resolve(&ctx.cwd, user);
        let secrets = super::SecretRoots::load();
        if let Some(msg) = secrets.reject(user, &root) {
            return ToolOutput::err(format!("glob: {msg}"));
        }
        if let Some(e) = check_root("glob", &root) {
            return e;
        }

        let cancel = ctx.cancel.clone();
        let found = tokio::task::spawn_blocking(move || collect_glob(&root, &matcher, &cancel, &secrets)).await;
        let mut found = match found {
            Ok(Ok(f)) => f,
            Ok(Err(msg)) => return ToolOutput::err(msg),
            Err(e) => return ToolOutput::err(format!("glob: {e}")),
        };

        if found.is_empty() {
            return ToolOutput::ok("No files matched.");
        }
        if count {
            return ToolOutput::ok(format!("{} files match.", found.len()));
        }
        found.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1))); // newest first
        let (offset, limit) = page_args(&args, MAX_GLOB, MAX_PAGE);
        ToolOutput::ok(page(found.into_iter().map(|(_, p)| p).collect(), offset, limit, "matches", true))
    }
}

/// One entry per matching file: (modified time, path relative to the walk root).
fn collect_glob(
    root: &Path,
    matcher: &globset::GlobMatcher,
    cancel: &tokio_util::sync::CancellationToken,
    secrets: &super::SecretRoots,
) -> Result<Vec<(SystemTime, String)>, String> {
    let mut found = Vec::new();
    for entry in walk(root, secrets).flatten() {
        if cancel.is_cancelled() {
            return Err("Cancelled by user.".into());
        }
        if entry.path_is_symlink() || entry.file_type().is_some_and(|t| t.is_dir()) {
            continue;
        }
        let Ok(rel) = entry.path().strip_prefix(root) else { continue };
        if rel.as_os_str().is_empty() || !matcher.is_match(rel) {
            continue;
        }
        let mtime = entry.metadata().ok().and_then(|m| m.modified().ok()).unwrap_or(SystemTime::UNIX_EPOCH);
        found.push((mtime, super::slash_path(rel)));
    }
    Ok(found)
}

// ------------------------------------------------------------------------------- grep

struct Grep;

/// What grep reports.
#[derive(Clone, Copy, PartialEq)]
enum Output {
    /// Matching lines.
    Content,
    /// Paths of files with a match.
    Files,
    /// Matching lines per file.
    Count,
}

/// One matching line (or, for `Files`, one file).
struct Hit {
    rel: String,
    line: usize,
    text: String,
}

#[async_trait::async_trait]
impl Tool for Grep {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "grep".into(),
            description: "Search file contents with a regular expression, respecting .gitignore. \
                Returns matching lines as \"path:line: text\" (300 per page, long lines truncated; \
                `context` adds surrounding lines as \"path-line- text\"), with output=\"files\" just the \
                paths of files containing matches, or with output=\"count\" the number of matching lines \
                per file. Lines over 16 MiB return an error. Page with offset/limit. Use glob to restrict which files are searched."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "Regular expression to search for."},
                    "path": {"type": "string", "description": "File or directory to search (default: the workspace root)."},
                    "glob": {"type": "string", "description": "Only search files matching this glob pattern (e.g. \"*.rs\")."},
                    "case_insensitive": {"type": "boolean", "description": "Match without regard to case (default false)."},
                    "output": {"type": "string", "enum": ["content", "files", "count"], "description": "\"content\" returns matching lines (default); \"files\" the matching files; \"count\" matching lines per file."},
                    "context": {"type": "integer", "description": format!("Lines of context before and after each match (content output; default 0, max {MAX_CONTEXT}).")},
                    "offset": {"type": "integer", "description": "Skip this many results (lines, or files) for paging (default 0)."},
                    "limit": {"type": "integer", "description": format!("Results per page (default {MAX_GREP}, max {MAX_PAGE}).")}
                },
                "required": ["pattern"]
            }),
        }
    }

    fn summary(&self, args: &Value) -> String {
        args.get("pattern").and_then(Value::as_str).unwrap_or_default().to_string()
    }

    fn needs_approval_for(&self, args: &Value, cwd: &Path) -> bool {
        super::outside_path_arg(cwd, args)
    }

    fn handles_cancel(&self) -> bool {
        true
    }

    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolOutput {
        let Some(pattern) = args.get("pattern").and_then(Value::as_str) else {
            return ToolOutput::err("grep: missing required argument `pattern`.");
        };
        let case_insensitive = args.get("case_insensitive").and_then(Value::as_bool).unwrap_or(false);
        let re = match RegexBuilder::new(pattern).case_insensitive(case_insensitive).build() {
            Ok(re) => re,
            Err(e) => return ToolOutput::err(format!("grep: invalid regex {pattern:?}: {e}")),
        };
        let output = match args.get("output").and_then(Value::as_str).unwrap_or("content") {
            "content" => Output::Content,
            "files" => Output::Files,
            "count" => Output::Count,
            other => {
                return ToolOutput::err(format!("grep: output must be \"content\", \"files\" or \"count\", got {other:?}."))
            }
        };
        let filter = match args.get("glob").and_then(Value::as_str) {
            Some(g) => match compile_glob(g) {
                Ok(m) => Some(m),
                Err(e) => return e,
            },
            None => None,
        };
        let (offset, limit) = page_args(&args, MAX_GREP, MAX_PAGE);
        let context = args.get("context").and_then(Value::as_u64).unwrap_or(0).min(MAX_CONTEXT as u64) as usize;
        let user = args.get("path").and_then(Value::as_str).unwrap_or(".");
        let root = resolve(&ctx.cwd, user);
        let secrets = super::SecretRoots::load();
        if let Some(msg) = secrets.reject(user, &root) {
            return ToolOutput::err(format!("grep: {msg}"));
        }
        if let Some(e) = check_root("grep", &root) {
            return e;
        }

        let cancel = ctx.cancel.clone();
        let cap = if output == Output::Count { usize::MAX } else { offset.saturating_add(limit).saturating_add(1) };
        let found = tokio::task::spawn_blocking(move || {
            let hits = collect_grep(&root, &re, filter.as_ref(), output, cap, &cancel, &secrets)?;
            Ok::<_, String>(render_grep(&root, hits, output, offset, limit, context))
        })
        .await;
        match found {
            Ok(Ok(text)) => ToolOutput::ok(text),
            Ok(Err(msg)) => ToolOutput::err(msg),
            Err(e) => ToolOutput::err(format!("grep: {e}")),
        }
    }
}

/// Format collected hits as one page of grep output.
fn render_grep(root: &Path, hits: Vec<Hit>, output: Output, offset: usize, limit: usize, context: usize) -> String {
    if hits.is_empty() {
        return "No matches found.".into();
    }
    match output {
        Output::Files => page(hits.into_iter().map(|h| h.rel).collect(), offset, limit, "files", false),
        Output::Count => {
            let total: usize = hits.iter().map(|h| h.line).sum();
            let files = hits.len();
            let lines = hits.into_iter().map(|h| format!("{}: {}", h.rel, h.line)).collect();
            format!("{}\n({total} matching lines in {files} files)", page(lines, offset, limit, "files", true))
        }
        Output::Content if context == 0 => {
            page(hits.into_iter().map(|h| format!("{}:{}: {}", h.rel, h.line, h.text)).collect(), offset, limit, "matches", false)
        }
        Output::Content => {
            let more = hits.len() > offset + limit;
            let shown: Vec<Hit> = hits.into_iter().skip(offset).take(limit).collect();
            if shown.is_empty() {
                return format!("No matches at offset {offset}.");
            }
            let end = offset + shown.len();
            let mut out = with_context(root, &shown, context);
            if more {
                out.push_str(&format!("\n(more matches; continue with offset {end})"));
            }
            out
        }
    }
}

/// Matches with `context` lines around them, groups separated by `--`.
fn with_context(root: &Path, hits: &[Hit], context: usize) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < hits.len() {
        let rel = &hits[i].rel;
        let j = i + hits[i..].iter().take_while(|h| &h.rel == rel).count();
        let file = if root.is_file() { root.to_path_buf() } else { root.join(rel) };
        let text = super::safeio::read_capped(&file, GREP_CONTEXT_FILE).map(|b| String::from_utf8_lossy(&b).into_owned());
        let lines: Vec<&str> = text.as_deref().map(|t| t.lines().collect()).unwrap_or_default();
        if !out.is_empty() {
            out.push("--".into());
        }
        let mut last = 0; // last line number printed for this file
        for hit in &hits[i..j] {
            let from = hit.line.saturating_sub(context).max(last + 1).max(1);
            let to = (hit.line + context).min(lines.len().max(hit.line));
            if last > 0 && from > last + 1 {
                out.push("--".into());
            }
            for n in from..=to {
                if let Some(matched) = hits[i..j].iter().find(|h| h.line == n) {
                    out.push(format!("{rel}:{n}: {}", matched.text));
                } else if let Some(line) = lines.get(n - 1) {
                    out.push(format!("{rel}-{n}- {}", snippet(line.as_bytes())));
                }
            }
            last = last.max(to);
        }
        i = j;
    }
    out.join("\n")
}

/// Matching lines or files (at most `cap`), or per-file counts in `Hit::line`.
fn collect_grep(
    root: &Path,
    re: &regex::Regex,
    filter: Option<&globset::GlobMatcher>,
    output: Output,
    cap: usize,
    cancel: &tokio_util::sync::CancellationToken,
    secrets: &super::SecretRoots,
) -> Result<Vec<Hit>, String> {
    let mut matches: Vec<Hit> = Vec::new();
    let owned = secrets.clone();
    let mut scan_error = None;

    // Search one file; returns true when the match cap was reached.
    let mut check = |rel: &Path, abs: &Path| -> bool {
        if cancel.is_cancelled() {
            return true;
        }
        if let Some(f) = filter {
            if !f.is_match(rel) {
                return false;
            }
        }
        if owned.blocks_path(abs) {
            return false;
        }
        let before = matches.len();
        let result = grep_file(abs, rel, re, output, cap, &mut matches, cancel);
        if output == Output::Count && matches.len() > before {
            let n = matches.len() - before;
            matches.truncate(before);
            matches.push(Hit { rel: super::slash_path(rel), line: n, text: String::new() });
        }
        match result {
            GrepFile::Error(message) => { scan_error = Some(message); true }
            GrepFile::Cap => true,
            _ => false,
        }
    };

    if root.is_file() {
        if cancel.is_cancelled() {
            return Err("Cancelled by user.".into());
        }
        let rel = Path::new(root.file_name().unwrap_or_default());
        check(rel, root);
    } else {
        for entry in walk(root, secrets).flatten() {
            if cancel.is_cancelled() {
                return Err("Cancelled by user.".into());
            }
            if entry.path_is_symlink() || entry.file_type().is_some_and(|t| t.is_dir()) {
                continue;
            }
            let rel = entry.path().strip_prefix(root).unwrap_or(entry.path());
            if check(rel, entry.path()) {
                break;
            }
        }
    }
    if cancel.is_cancelled() {
        return Err("Cancelled by user.".into());
    }
    if let Some(message) = scan_error { return Err(message); }
    Ok(matches)
}

enum GrepFile {
    Error(String),
    Skip,
    Continue,
    Cap,
}

/// Scan a whole regular file. A NUL drops every match from that file.
/// Match complete lines so anchors and boundaries retain their meaning.
/// Reject oversized lines explicitly rather than returning misleading results.
fn grep_file(
    path: &Path,
    rel: &Path,
    re: &regex::Regex,
    output: Output,
    cap: usize,
    matches: &mut Vec<Hit>,
    cancel: &tokio_util::sync::CancellationToken,
) -> GrepFile {
    use std::io::Read;
    let files_only = output == Output::Files;
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return GrepFile::Skip,
    };
    if !file.metadata().map(|m| m.is_file()).unwrap_or(false) {
        return GrepFile::Skip;
    }
    let start = matches.len();
    let mut reader = std::io::BufReader::new(file);
    let mut pending: Vec<u8> = Vec::new();
    let mut line_no: usize = 1;
    let mut chunk = [0u8; 64 * 1024];
    let mut binary = false;
    let mut cap_hit = false;
    let mut file_done = false;
    let record = |line: &[u8], line_no: usize, matches: &mut Vec<Hit>| record_line(line, line_no, rel, re, output, cap, matches);
    loop {
        if cancel.is_cancelled() {
            matches.truncate(start);
            return GrepFile::Skip;
        }
        let n = match reader.read(&mut chunk) {
            Ok(0) => {
                if !pending.is_empty() && !file_done && !binary {
                    cap_hit = record(&pending, line_no, matches);
                }
                break;
            }
            Ok(n) => n,
            Err(_) => {
                matches.truncate(start);
                return GrepFile::Skip;
            }
        };
        let mut rest = &chunk[..n];
        while let Some(i) = rest.iter().position(|b| *b == b'\n' || *b == 0) {
            if rest[i] == 0 {
                binary = true;
                break;
            }
            if pending.len().saturating_add(i + 1) > MAX_GREP_LINE {
                return GrepFile::Error(format!("grep: {}:{line_no}: line exceeds 16 MiB", path.display()));
            }
            pending.extend_from_slice(&rest[..=i]);
            rest = &rest[i + 1..];
            if !file_done {
                let before = matches.len();
                cap_hit = record(&pending, line_no, matches) || cap_hit;
                if files_only && matches.len() > before {
                    file_done = true;
                }
            }
            pending.clear();
            line_no += 1;
            if cap_hit || binary {
                break;
            }
        }
        if binary {
            break;
        }
        if cap_hit {
            if rest.contains(&0) || read_has_nul(&mut reader) {
                binary = true;
            }
            break;
        }
        pending.extend_from_slice(rest);
        if pending.contains(&0) {
            binary = true;
            break;
        }
        if pending.len() > MAX_GREP_LINE {
            return GrepFile::Error(format!("grep: {}:{line_no}: line exceeds 16 MiB", path.display()));
        }
    }
    if binary {
        matches.truncate(start);
        return GrepFile::Skip;
    }
    if cap_hit { GrepFile::Cap } else { GrepFile::Continue }
}

fn read_has_nul(reader: &mut impl std::io::Read) -> bool {
    let mut chunk = [0u8; 64 * 1024];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => return false,
            Ok(n) if chunk[..n].contains(&0) => return true,
            Ok(_) => {}
            Err(_) => return false,
        }
    }
}

/// A line as shown in results: trimmed and cut at 300 characters.
fn snippet(line: &[u8]) -> String {
    let text = String::from_utf8_lossy(line);
    let mut snippet = text.trim().to_string();
    if snippet.chars().count() > 300 {
        snippet = snippet.chars().take(300).collect::<String>() + "…";
    }
    snippet
}

fn record_line(
    line: &[u8],
    line_no: usize,
    rel: &Path,
    re: &regex::Regex,
    output: Output,
    cap: usize,
    matches: &mut Vec<Hit>,
) -> bool {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    if !line_matches(re, line) {
        return false;
    }
    let text = if output == Output::Content { snippet(line) } else { String::new() };
    matches.push(Hit { rel: super::slash_path(rel), line: line_no, text });
    matches.len() >= cap
}

fn line_matches(re: &regex::Regex, line: &[u8]) -> bool {
    re.is_match(&String::from_utf8_lossy(line))
}

// ------------------------------------------------------------------------------- misc

pub fn register(r: &mut Registry) {
    r.add(Box::new(GlobFiles));
    r.add(Box::new(Grep));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn context_preserves_adjacent_match_markers() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "match1\nmatch2\nafter\n").unwrap();
        let out = run(&Grep, json!({"pattern":"match", "context":1}), dir.path()).await;
        assert_eq!(out.content, "a.txt:1: match1\na.txt:2: match2\na.txt-3- after");
    }

    #[tokio::test]
    async fn long_lines_preserve_regex_anchors_and_boundaries() {
        let dir = tempfile::tempdir().unwrap();
        let text = format!("{}TARGET{}END\n", "a".repeat(1024 * 1024 - 65536), "z".repeat(200000));
        std::fs::write(dir.path().join("long.txt"), text).unwrap();
        for pattern in ["^TARGET", "a$", r"\bTARGET"] {
            let out = run(&Grep, json!({"pattern":pattern}), dir.path()).await;
            assert_eq!(out.content, "No matches found.", "{pattern}");
        }
        for pattern in ["^a+TARGETz+END$", "TARGET", "END$"] {
            let out = run(&Grep, json!({"pattern":pattern}), dir.path()).await;
            assert!(!out.is_error && out.content.contains("long.txt:1:"), "{pattern}: {}", out.content);
        }
        std::fs::write(dir.path().join("long.txt"), vec![b'a'; MAX_GREP_LINE + 1]).unwrap();
        let out = run(&Grep, json!({"pattern":"a"}), dir.path()).await;
        assert!(out.is_error && out.content.contains("line exceeds 16 MiB"));
    }


    #[cfg(unix)]
    #[tokio::test]
    async fn searches_skip_symlinked_files() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("token.txt"), "secret token").unwrap();
        std::os::unix::fs::symlink(outside.path().join("token.txt"), dir.path().join("link.txt")).unwrap();
        let grep = run(&Grep, json!({"pattern": "token"}), dir.path()).await;
        assert!(!grep.content.contains("secret"), "{}", grep.content);
        let glob = run(&GlobFiles, json!({"pattern": "**/*"}), dir.path()).await;
        assert!(!glob.content.contains("link.txt"), "{}", glob.content);
    }

    use std::time::Duration;
    use tokio_util::sync::CancellationToken;

    fn ctx(dir: &Path) -> ToolCtx {
        ToolCtx { cwd: dir.to_path_buf(), cancel: CancellationToken::new(), ..Default::default() }
    }

    async fn run(tool: &dyn Tool, args: Value, dir: &Path) -> ToolOutput {
        tool.run(args, &ctx(dir)).await
    }

    fn setup() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src/nested")).unwrap();
        std::fs::write(dir.path().join("src/old.rs"), "fn main() {}\n").unwrap();
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(dir.path().join("src/new.rs"), "fn other() {}\n").unwrap();
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(dir.path().join("src/nested/deep.rs"), "deep\n").unwrap();
        std::fs::write(dir.path().join("notes.txt"), "todo: write tests\ntodo: sleep\n").unwrap();
        std::fs::write(dir.path().join("src/ignored.rs"), "ignored\n").unwrap();
        std::fs::write(dir.path().join(".gitignore"), "ignored.rs\n").unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".git/config"), "gitdir\n").unwrap();
        std::fs::write(dir.path().join("blob.bin"), b"\0binary\n").unwrap();
        dir
    }

    #[tokio::test]
    async fn glob_newest_first_gitignore_and_dot() {
        let dir = setup();
        let out = run(&GlobFiles, json!({"pattern": "*.rs"}), dir.path()).await;
        assert!(!out.is_error, "{}", out.content);
        let lines: Vec<&str> = out.content.lines().collect();
        assert_eq!(lines[0], "src/nested/deep.rs"); // newest first
        assert!(lines.contains(&"src/new.rs"));
        assert!(lines.contains(&"src/old.rs"));
        assert!(!lines.contains(&"src/ignored.rs")); // .gitignore respected
        assert!(!lines.iter().any(|l| l.contains(".git/"))); // .git skipped
        assert!(!lines.contains(&"notes.txt"));

        let out = run(&GlobFiles, json!({"pattern": "**/*.rs", "path": "src"}), dir.path()).await;
        let lines: Vec<&str> = out.content.lines().collect();
        assert_eq!(lines[0], "nested/deep.rs");
        assert!(lines.contains(&"new.rs") && lines.contains(&"old.rs"));
        assert!(!lines.iter().any(|l| l.contains("ignored.rs")));

        let out = run(&GlobFiles, json!({"pattern": "nope-*"}), dir.path()).await;
        assert!(!out.is_error && out.content.contains("No files matched"));
        assert!(run(&GlobFiles, json!({"pattern": "["}), dir.path()).await.is_error);
        assert!(run(&GlobFiles, json!({}), dir.path()).await.is_error);
        let out = run(&GlobFiles, json!({"pattern": "*.rs", "path": "missing"}), dir.path()).await;
        assert!(out.is_error && out.content.contains("no such file"));
        assert_eq!(GlobFiles.summary(&json!({"pattern": "*.rs"})), "*.rs");
    }

    #[tokio::test]
    async fn glob_truncates_at_200() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..205 {
            std::fs::write(dir.path().join(format!("f{i:03}.txt")), b"x").unwrap();
        }
        let out = run(&GlobFiles, json!({"pattern": "*.txt"}), dir.path()).await;
        assert!(!out.is_error);
        let lines: Vec<&str> = out.content.lines().collect();
        assert_eq!(lines.len(), 201); // 200 + note
        assert!(lines.last().unwrap().contains("(5 more matches; continue with offset 200)"));

        let out = run(&GlobFiles, json!({"pattern": "*.txt", "offset": 200}), dir.path()).await;
        assert_eq!(out.content.lines().count(), 5, "{}", out.content);
        let out = run(&GlobFiles, json!({"pattern": "*.txt", "limit": 3, "offset": 1}), dir.path()).await;
        assert_eq!(out.content.lines().count(), 4, "{}", out.content);
        assert!(out.content.ends_with("(201 more matches; continue with offset 4)"), "{}", out.content);
        let out = run(&GlobFiles, json!({"pattern": "*.txt", "mode": "count"}), dir.path()).await;
        assert_eq!(out.content, "205 files match.");
        assert!(run(&GlobFiles, json!({"pattern": "*", "mode": "bogus"}), dir.path()).await.is_error);
    }

    #[tokio::test]
    async fn grep_counts_pages_and_shows_context() {
        let dir = tempfile::tempdir().unwrap();
        let body: String = (1..=10).map(|i| if i % 4 == 0 { format!("hit {i}\n") } else { format!("line {i}\n") }).collect();
        std::fs::write(dir.path().join("a.txt"), &body).unwrap();
        std::fs::write(dir.path().join("b.txt"), "hit\nhit\n").unwrap();

        let out = run(&Grep, json!({"pattern": "hit", "output": "count", "glob": "a.txt"}), dir.path()).await;
        assert_eq!(out.content, "a.txt: 2\n(2 matching lines in 1 files)");

        let out = run(&Grep, json!({"pattern": "hit", "path": "a.txt", "context": 1}), dir.path()).await;
        assert_eq!(
            out.content,
            "a.txt-3- line 3\na.txt:4: hit 4\na.txt-5- line 5\n--\na.txt-7- line 7\na.txt:8: hit 8\na.txt-9- line 9"
        );
        let out = run(&Grep, json!({"pattern": "hit", "path": "a.txt", "context": 2}), dir.path()).await;
        assert!(!out.content.contains("--"), "adjacent groups merge: {}", out.content);
        assert_eq!(out.content.lines().count(), 9, "{}", out.content);

        let out = run(&Grep, json!({"pattern": "hit", "path": "a.txt", "limit": 1}), dir.path()).await;
        assert_eq!(out.content, "a.txt:4: hit 4\n(more matches; continue with offset 1)");
        let out = run(&Grep, json!({"pattern": "hit", "path": "a.txt", "limit": 1, "offset": 1}), dir.path()).await;
        assert_eq!(out.content, "a.txt:8: hit 8");
        let out = run(&Grep, json!({"pattern": "hit", "path": "a.txt", "offset": 5}), dir.path()).await;
        assert!(out.content.contains("No matches at offset 5"), "{}", out.content);
    }

    #[tokio::test]
    async fn grep_content_files_filters() {
        let dir = setup();

        let out = run(&Grep, json!({"pattern": "todo"}), dir.path()).await;
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(out.content, "notes.txt:1: todo: write tests\nnotes.txt:2: todo: sleep");

        let out = run(&Grep, json!({"pattern": "TODO"}), dir.path()).await;
        assert!(out.content.contains("No matches"));
        let out = run(&Grep, json!({"pattern": "TODO", "case_insensitive": true}), dir.path()).await;
        assert!(out.content.contains("notes.txt:1"));

        let out = run(&Grep, json!({"pattern": "fn", "glob": "*.rs"}), dir.path()).await;
        assert!(out.content.contains("src/old.rs:1: fn main() {}"));
        assert!(out.content.contains("src/new.rs:1: fn other() {}"));
        assert!(!out.content.contains("notes.txt"));

        let out = run(&Grep, json!({"pattern": "todo", "output": "files"}), dir.path()).await;
        assert_eq!(out.content, "notes.txt");

        let out = run(&Grep, json!({"pattern": "deep", "path": "src/nested/deep.rs"}), dir.path()).await;
        assert!(out.content.contains("deep.rs:1: deep"));

        let out = run(&Grep, json!({"pattern": "binary"}), dir.path()).await;
        assert!(out.content.contains("No matches")); // binary file skipped
        let out = run(&Grep, json!({"pattern": "gitdir"}), dir.path()).await;
        assert!(out.content.contains("No matches")); // .git skipped
        assert!(run(&Grep, json!({"pattern": "("}), dir.path()).await.is_error);
        assert!(run(&Grep, json!({"pattern": "x", "output": "bogus"}), dir.path()).await.is_error);
        assert!(run(&Grep, json!({"pattern": "x", "glob": "["}), dir.path()).await.is_error);
        assert!(run(&Grep, json!({}), dir.path()).await.is_error);
        let out = run(&Grep, json!({"pattern": "x", "path": "missing"}), dir.path()).await;
        assert!(out.is_error && out.content.contains("no such file"));
        assert_eq!(Grep.summary(&json!({"pattern": "todo"})), "todo");
    }

    #[tokio::test]
    async fn grep_caps_and_truncates_lines() {
        let dir = tempfile::tempdir().unwrap();
        let body: String = (1..=400).map(|i| format!("match {i}\n")).collect();
        std::fs::write(dir.path().join("big.txt"), body).unwrap();
        std::fs::write(dir.path().join("long.txt"), format!("match {}\n", "z".repeat(400))).unwrap();

        let out = run(&Grep, json!({"pattern": "match"}), dir.path()).await;
        assert!(!out.is_error);
        let lines: Vec<&str> = out.content.lines().collect();
        assert_eq!(lines.len(), MAX_GREP + 1); // 300 + note
        assert!(lines.last().unwrap().contains("continue with offset 300"));

        let out = run(&Grep, json!({"pattern": "match", "glob": "long.txt"}), dir.path()).await;
        let line = out.content.lines().next().unwrap();
        assert_eq!(line.chars().count(), "long.txt:1: ".chars().count() + 300 + 1); // 300 + ellipsis
    }

    #[tokio::test]
    async fn grep_files_mode_caps() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..320 {
            std::fs::write(dir.path().join(format!("m{i:03}.txt")), "match\n").unwrap();
        }
        let out = run(&Grep, json!({"pattern": "match", "output": "files"}), dir.path()).await;
        assert!(!out.is_error);
        let lines: Vec<&str> = out.content.lines().collect();
        assert_eq!(lines.len(), MAX_GREP + 1);
        assert!(lines.last().unwrap().contains("(more files; continue with offset 300)"));
    }

    #[tokio::test]
    async fn specs_are_sane() {
        let spec = GlobFiles.spec();
        assert_eq!(spec.name, "glob");
        assert_eq!(spec.parameters["required"], json!(["pattern"]));
        let spec = Grep.spec();
        assert_eq!(spec.name, "grep");
        assert_eq!(spec.parameters["properties"]["output"]["enum"], json!(["content", "files", "count"]));
        assert!(!GlobFiles.needs_approval(&json!({})));
        assert!(!Grep.needs_approval(&json!({})));
    }

    #[tokio::test]
    async fn cancelled_scan_returns_without_finishing() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..40 {
            std::fs::write(dir.path().join(format!("f{i:02}.txt")), "match\n").unwrap();
        }
        let ctx = ToolCtx { cwd: dir.path().into(), cancel: CancellationToken::new(), ..Default::default() };
        ctx.cancel.cancel();
        let out = GlobFiles.run(json!({"pattern": "*.txt"}), &ctx).await;
        assert!(out.is_error && out.content.contains("Cancelled"), "{}", out.content);
        let out = Grep.run(json!({"pattern": "match"}), &ctx).await;
        assert!(out.is_error && out.content.contains("Cancelled"), "{}", out.content);
    }

    #[test]
    fn search_refuses_credential_files() {
        let _lock = crate::provider::sse::tests::ENV.lock().unwrap_or_else(|e| e.into_inner());
        let home = tempfile::tempdir().unwrap();
        let old_home = std::env::var_os("NYQ_HOME");
        let old_auth = std::env::var_os("NYQ_CODEX_AUTH");
        std::env::set_var("NYQ_HOME", home.path());
        std::env::remove_var("NYQ_CODEX_AUTH");
        std::fs::write(home.path().join("chatgpt-auth.json"), "refresh-token\n").unwrap();
        let ws = tempfile::tempdir().unwrap();
        std::fs::write(ws.path().join("ok.txt"), "hello refresh-token\n").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(home.path().join("chatgpt-auth.json"), ws.path().join("leak")).unwrap();
        let secret = home.path().to_str().unwrap().to_string();
        let ws_path = ws.path().to_path_buf();
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let out = rt.block_on(GlobFiles.run(json!({"pattern": "**/*", "path": secret}), &ctx(&ws_path)));
        assert!(out.is_error && out.content.contains("credential"), "{}", out.content);
        let out = rt.block_on(Grep.run(json!({"pattern": "refresh-token", "path": secret}), &ctx(&ws_path)));
        assert!(out.is_error && out.content.contains("credential"), "{}", out.content);
        let out = rt.block_on(Grep.run(json!({"pattern": "refresh-token"}), &ctx(&ws_path)));
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("ok.txt"), "{}", out.content);
        #[cfg(unix)]
        assert!(!out.content.contains("leak"), "{}", out.content);
        if let Some(v) = old_home { std::env::set_var("NYQ_HOME", v); } else { std::env::remove_var("NYQ_HOME"); }
        if let Some(v) = old_auth { std::env::set_var("NYQ_CODEX_AUTH", v); } else { std::env::remove_var("NYQ_CODEX_AUTH"); }
    }

    #[tokio::test]
    async fn grep_finds_a_match_past_eight_mebibytes() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let mut file = std::fs::File::create(dir.path().join("big.txt")).unwrap();
        let chunk = vec![b'a'; 1024 * 1024];
        for _ in 0..9 {
            file.write_all(&chunk).unwrap();
        }
        file.write_all(b"\nMARKER-TAIL\n").unwrap();
        let out = run(&Grep, json!({"pattern": "MARKER-TAIL", "path": "big.txt"}), dir.path()).await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("MARKER-TAIL"), "{}", out.content);
        assert!(!out.content.contains("No matches"));
    }
}
