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
const GREP_WINDOW: usize = 1024 * 1024;
const GREP_OVERLAP: usize = 64 * 1024;

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

// ------------------------------------------------------------------------------- glob

struct GlobFiles;

#[async_trait::async_trait]
impl Tool for GlobFiles {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "glob".into(),
            description: "Find files matching a glob pattern (e.g. \"**/*.rs\", \"src/**/*.ts\"), \
                respecting .gitignore. Returns paths relative to the search root, \
                newest-modified first, at most 200 (a note is added when truncated)."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "Glob pattern, matched against paths relative to the search root."},
                    "path": {"type": "string", "description": "Directory to search (default: the workspace root)."}
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
        found.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1))); // newest first
        let total = found.len();
        let mut out: Vec<String> = found.into_iter().take(MAX_GLOB).map(|(_, p)| p).collect();
        if total > MAX_GLOB {
            out.push(format!("({} more matches)", total - MAX_GLOB));
        }
        ToolOutput::ok(out.join("\n"))
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

#[async_trait::async_trait]
impl Tool for Grep {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "grep".into(),
            description: "Search file contents with a regular expression, respecting .gitignore. \
                Returns matching lines as \"path:line: text\" (at most 300, long lines truncated) \
                or, with output=\"files\", just the paths of files containing matches. \
                Use glob to restrict which files are searched."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "Regular expression to search for."},
                    "path": {"type": "string", "description": "File or directory to search (default: the workspace root)."},
                    "glob": {"type": "string", "description": "Only search files matching this glob pattern (e.g. \"*.rs\")."},
                    "case_insensitive": {"type": "boolean", "description": "Match without regard to case (default false)."},
                    "output": {"type": "string", "enum": ["content", "files"], "description": "\"content\" returns matching lines (default); \"files\" returns the list of matching files."}
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
        let files_only = match args.get("output").and_then(Value::as_str).unwrap_or("content") {
            "content" => false,
            "files" => true,
            other => {
                return ToolOutput::err(format!("grep: output must be \"content\" or \"files\", got {other:?}."))
            }
        };
        let filter = match args.get("glob").and_then(Value::as_str) {
            Some(g) => match compile_glob(g) {
                Ok(m) => Some(m),
                Err(e) => return e,
            },
            None => None,
        };
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
        let found = tokio::task::spawn_blocking(move || collect_grep(&root, &re, filter.as_ref(), files_only, &cancel, &secrets)).await;
        let (mut matches, truncated) = match found {
            Ok(Ok(r)) => r,
            Ok(Err(msg)) => return ToolOutput::err(msg),
            Err(e) => return ToolOutput::err(format!("grep: {e}")),
        };

        if matches.is_empty() {
            return ToolOutput::ok("No matches found.");
        }
        if truncated {
            matches.push(if files_only {
                format!("(stopped after {MAX_GREP} files)")
            } else {
                format!("(stopped after {MAX_GREP} matches)")
            });
        }
        ToolOutput::ok(matches.join("\n"))
    }
}

/// Matching lines ("path:line: text") or file paths, plus whether the cap hit.
fn collect_grep(
    root: &Path,
    re: &regex::Regex,
    filter: Option<&globset::GlobMatcher>,
    files_only: bool,
    cancel: &tokio_util::sync::CancellationToken,
    secrets: &super::SecretRoots,
) -> Result<(Vec<String>, bool), String> {
    let mut matches: Vec<String> = Vec::new();
    let mut truncated = false;
    let owned = secrets.clone();

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
        match grep_file(abs, rel, re, files_only, &mut matches, cancel) {
            GrepFile::Skip => false,
            GrepFile::Continue => false,
            GrepFile::Cap => true,
        }
    };

    if root.is_file() {
        if cancel.is_cancelled() {
            return Err("Cancelled by user.".into());
        }
        let rel = Path::new(root.file_name().unwrap_or_default());
        truncated = check(rel, root);
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
                if cancel.is_cancelled() {
                    return Err("Cancelled by user.".into());
                }
                truncated = true;
                break;
            }
        }
    }
    if cancel.is_cancelled() {
        return Err("Cancelled by user.".into());
    }
    Ok((matches, truncated))
}

enum GrepFile {
    Skip,
    Continue,
    Cap,
}

/// Scan a whole regular file. A NUL drops every match from that file.
/// Lines longer than [`GREP_WINDOW`] are searched in overlapping windows so a
/// match past the first mebibyte is still found.
fn grep_file(
    path: &Path,
    rel: &Path,
    re: &regex::Regex,
    files_only: bool,
    matches: &mut Vec<String>,
    cancel: &tokio_util::sync::CancellationToken,
) -> GrepFile {
    use std::io::Read;
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
    let mut cap = false;
    let mut hit = false;
    let mut file_done = false;
    loop {
        if cancel.is_cancelled() {
            matches.truncate(start);
            return GrepFile::Skip;
        }
        let n = match reader.read(&mut chunk) {
            Ok(0) => {
                if !pending.is_empty() && !hit && !file_done && !binary {
                    cap = record_line(&pending, line_no, rel, re, files_only, matches);
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
            pending.extend_from_slice(&rest[..=i]);
            rest = &rest[i + 1..];
            if !hit && !file_done {
                let before = matches.len();
                cap = record_line(&pending, line_no, rel, re, files_only, matches) || cap;
                if files_only && matches.len() > before {
                    file_done = true;
                }
            }
            pending.clear();
            hit = false;
            line_no += 1;
            if cap || binary {
                break;
            }
        }
        if binary {
            break;
        }
        if cap {
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
        if pending.len() > GREP_WINDOW {
            if !hit && !file_done && line_matches(re, &pending) {
                cap = record_line(&pending, line_no, rel, re, files_only, matches);
                hit = true;
                if files_only {
                    file_done = true;
                }
            }
            let keep = GREP_OVERLAP.min(pending.len());
            pending.drain(..pending.len() - keep);
            if cap && read_has_nul(&mut reader) {
                binary = true;
                break;
            }
        }
    }
    if binary {
        matches.truncate(start);
        return GrepFile::Skip;
    }
    if cap { GrepFile::Cap } else { GrepFile::Continue }
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

fn record_line(
    line: &[u8],
    line_no: usize,
    rel: &Path,
    re: &regex::Regex,
    files_only: bool,
    matches: &mut Vec<String>,
) -> bool {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    if !line_matches(re, line) {
        return false;
    }
    if files_only {
        matches.push(super::slash_path(rel));
    } else {
        let text = String::from_utf8_lossy(line);
        let mut snippet = text.trim().to_string();
        if snippet.chars().count() > 300 {
            snippet = snippet.chars().take(300).collect::<String>() + "…";
        }
        matches.push(format!("{}:{line_no}: {snippet}", super::slash_path(rel)));
    }
    matches.len() >= MAX_GREP
}

fn line_matches(re: &regex::Regex, line: &[u8]) -> bool {
    if line.len() <= GREP_WINDOW {
        return re.is_match(&String::from_utf8_lossy(line));
    }
    let mut start = 0;
    while start < line.len() {
        let end = (start + GREP_WINDOW).min(line.len());
        if re.is_match(&String::from_utf8_lossy(&line[start..end])) {
            return true;
        }
        if end == line.len() {
            break;
        }
        start = end - GREP_OVERLAP;
    }
    false
}

// ------------------------------------------------------------------------------- misc

pub fn register(r: &mut Registry) {
    r.add(Box::new(GlobFiles));
    r.add(Box::new(Grep));
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(lines.last().unwrap().contains("(5 more matches)"));
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
        assert!(lines.last().unwrap().contains("stopped after 300"));

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
        assert!(lines.last().unwrap().contains("stopped after 300 files"));
    }

    #[tokio::test]
    async fn specs_are_sane() {
        let spec = GlobFiles.spec();
        assert_eq!(spec.name, "glob");
        assert_eq!(spec.parameters["required"], json!(["pattern"]));
        let spec = Grep.spec();
        assert_eq!(spec.name, "grep");
        assert_eq!(spec.parameters["properties"]["output"]["enum"], json!(["content", "files"]));
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
