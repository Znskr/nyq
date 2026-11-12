//! Parsing and applying file patches: standard unified diffs (multi-file)
//! and the Codex `*** Begin Patch` format. Everything here is pure: the
//! caller loads the touched files, applies the parsed changes in memory,
//! then writes. Line-ending and sandbox handling live with the caller.

/// One changed line inside a [`Hunk`].
#[derive(Clone, Debug, PartialEq)]
pub struct Line {
    pub kind: Kind,
    pub text: String,
    /// A `\ No newline at end of file` marker followed this line. Diff
    /// generators only emit it for the file's last line.
    pub no_nl: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Context,
    Remove,
    Add,
}

#[derive(Clone, Debug, Default)]
pub struct Hunk {
    /// 0-based line the hunk claims to start at (from `@@ -l`); Codex
    /// sections carry no hint.
    pub hint: Option<usize>,
    /// Codex `*** End of File`: the hunk must match anchored at the end.
    pub eof: bool,
    pub lines: Vec<Line>,
}

#[derive(Clone, Debug)]
pub enum Change {
    /// Apply `hunks` to the existing file.
    Update(Vec<Hunk>),
    /// Create the file with these lines (a trailing newline is added).
    Create(Vec<String>),
    /// Delete the file.
    Delete,
}

#[derive(Clone, Debug)]
pub struct FilePatch {
    pub path: String,
    pub change: Change,
}

impl Line {
    fn new(kind: Kind, text: &str) -> Self {
        Self {
            kind,
            text: text.trim_end_matches('\r').to_string(),
            no_nl: false,
        }
    }
}

/// Parse `text` as a patch. A `*** Begin Patch` marker selects the Codex
/// format; anything else is read as a unified diff.
pub fn parse(text: &str) -> Result<Vec<FilePatch>, String> {
    if text.lines().any(|l| l.trim() == "*** Begin Patch") {
        parse_codex(text)
    } else {
        parse_unified(text)
    }
}

/// The files a patch touches, in patch order, without validating hunks.
/// Cheap enough for undo snapshots and summaries.
pub fn paths(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |p: String| {
        if !p.is_empty() && p != "/dev/null" && !out.contains(&p) {
            out.push(p);
        }
    };
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        for marker in ["*** Update File:", "*** Add File:", "*** Delete File:"] {
            if let Some(rest) = line.strip_prefix(marker) {
                push(rest.trim().to_string());
            }
        }
        for marker in ["--- ", "+++ "] {
            if let Some(rest) = line.strip_prefix(marker) {
                push(strip_path(rest));
            }
        }
    }
    out
}

/// A `---`/`+++` header path: up to a tab, `a/` or `b/` stripped, quoted
/// spellings unwrapped.
fn strip_path(rest: &str) -> String {
    let raw = rest.split('\t').next().unwrap_or("").trim_end();
    let raw = raw
        .strip_prefix('"')
        .and_then(|r| r.strip_suffix('"'))
        .unwrap_or(raw);
    raw.strip_prefix("a/")
        .or_else(|| raw.strip_prefix("b/"))
        .unwrap_or(raw)
        .to_string()
}

// ------------------------------------------------------------------ unified

/// Both `(start, count)` pairs of an `@@` header.
fn hunk_counts(header: &str) -> Option<(usize, usize, usize, usize)> {
    let mut t = header.split_whitespace();
    if t.next() != Some("@@") {
        return None;
    }
    let pair = |want: char, t: &mut std::str::SplitWhitespace| -> Option<(usize, usize)> {
        let tok = t.find(|tok| tok.starts_with(want))?;
        let mut parts = tok[1..].split(',');
        let start: usize = parts.next()?.parse().ok()?;
        let count: usize = match parts.next() {
            Some(p) => p.parse().ok()?,
            None => 1,
        };
        Some((start, count))
    };
    let (os, oc) = pair('-', &mut t)?;
    let (ns, nc) = pair('+', &mut t)?;
    Some((os, oc, ns, nc))
}

fn parse_unified(text: &str) -> Result<Vec<FilePatch>, String> {
    let lines: Vec<&str> = text.lines().collect();
    let mut files = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let Some(old_raw) = lines[i].strip_prefix("--- ") else {
            i += 1;
            continue;
        };
        let old_path = strip_path(old_raw);
        i += 1;
        let Some(new_raw) = lines.get(i).and_then(|l| l.strip_prefix("+++ ")) else {
            return Err(format!("unified diff: `--- {old_path}` is not followed by a `+++` line"));
        };
        let new_path = strip_path(new_raw);
        i += 1;
        let mut hunks = Vec::new();
        while i < lines.len() && lines[i].starts_with("@@") {
            let Some((os, oc, _ns, nc)) = hunk_counts(lines[i]) else {
                return Err(format!("unified diff: malformed hunk header `{}`", lines[i]));
            };
            // `-a,0` means insert after line a; `-a,n` starts at line a.
            let hint = if oc == 0 { os } else { os.saturating_sub(1) };
            i += 1;
            let mut hunk = Hunk {
                hint: Some(hint),
                ..Hunk::default()
            };
            let mut need_old = oc;
            let mut need_new = nc;
            while i < lines.len() && (need_old > 0 || need_new > 0) {
                let l = lines[i];
                let (kind, body) = if let Some(b) = l.strip_prefix(' ') {
                    (Kind::Context, b)
                } else if let Some(b) = l.strip_prefix('-') {
                    (Kind::Remove, b)
                } else if let Some(b) = l.strip_prefix('+') {
                    (Kind::Add, b)
                } else if l.starts_with('\\') {
                    if let Some(last) = hunk.lines.last_mut() {
                        last.no_nl = true;
                    }
                    i += 1;
                    continue;
                } else if l.is_empty() {
                    // Bare empty line: a context line whose leading space
                    // was stripped by whoever produced the patch.
                    (Kind::Context, "")
                } else {
                    return Err(format!(
                        "unified diff: hunk for {new_path} ends early: `{l}`"
                    ));
                };
                match kind {
                    Kind::Add => {
                        need_new = need_new
                            .checked_sub(1)
                            .ok_or("unified diff: hunk has more `+` lines than declared")?;
                    }
                    _ => {
                        need_old = need_old
                            .checked_sub(1)
                            .ok_or("unified diff: hunk has more context/`-` lines than declared")?;
                        if kind == Kind::Context {
                            need_new = need_new
                                .checked_sub(1)
                                .ok_or("unified diff: hunk has more context lines than declared")?;
                        }
                    }
                }
                hunk.lines.push(Line::new(kind, body));
                i += 1;
            }
            if need_old > 0 || need_new > 0 {
                return Err(format!("unified diff: hunk for {new_path} ends early"));
            }
            // A `\ No newline` marker may follow the last body line.
            while i < lines.len() && lines[i].starts_with('\\') {
                if let Some(last) = hunk.lines.last_mut() {
                    last.no_nl = true;
                }
                i += 1;
            }
            hunks.push(hunk);
        }
        let (path, change) = if old_path == "/dev/null" {
            // Creation: every hunk line must be an add.
            let mut added = Vec::new();
            for line in hunks.iter().flat_map(|h| &h.lines) {
                if line.kind != Kind::Add {
                    return Err(format!(
                        "unified diff: new file {new_path} has context or removals"
                    ));
                }
                added.push(line.text.clone());
            }
            (new_path, Change::Create(added))
        } else if new_path == "/dev/null" {
            (old_path, Change::Delete)
        } else {
            if hunks.is_empty() {
                return Err(format!("unified diff: {new_path} has no hunks"));
            }
            (new_path, Change::Update(hunks))
        };
        files.push(FilePatch { path, change });
    }
    if files.is_empty() {
        return Err("no unified diff hunks or `*** Begin Patch` markers found".into());
    }
    Ok(files)
}

// -------------------------------------------------------------------- codex

fn parse_codex(text: &str) -> Result<Vec<FilePatch>, String> {
    let lines: Vec<&str> = text.lines().collect();
    let mut files = Vec::new();
    let mut i = 0;
    while i < lines.len() && lines[i].trim() != "*** Begin Patch" {
        i += 1;
    }
    i += 1;
    let mut saw_end = false;
    while i < lines.len() {
        let line = lines[i].trim_end_matches('\r');
        if line.trim() == "*** End Patch" {
            saw_end = true;
            break;
        }
        if let Some(rest) = line.strip_prefix("*** Update File:") {
            let path = rest.trim().to_string();
            if path.is_empty() {
                return Err("apply_patch: `*** Update File:` needs a path".into());
            }
            i += 1;
            let mut hunks = vec![Hunk::default()];
            while i < lines.len() && !lines[i].starts_with("*** ") {
                let l = lines[i];
                if let Some(ctx) = l.strip_prefix("@@") {
                    // `@@` starts a new section; text after it is a context
                    // line the model expects at that point.
                    if !hunks.last().is_some_and(|h| h.lines.is_empty()) {
                        hunks.push(Hunk::default());
                    }
                    if !ctx.trim().is_empty() {
                        hunks
                            .last_mut()
                            .unwrap()
                            .lines
                            .push(Line::new(Kind::Context, ctx.trim()));
                    }
                    i += 1;
                    continue;
                }
                let (kind, body) = if let Some(b) = l.strip_prefix(' ') {
                    (Kind::Context, b)
                } else if let Some(b) = l.strip_prefix('-') {
                    (Kind::Remove, b)
                } else if let Some(b) = l.strip_prefix('+') {
                    (Kind::Add, b)
                } else if l.is_empty() {
                    (Kind::Context, "")
                } else {
                    return Err(format!(
                        "apply_patch: expected ` `/`-`/`+` in {path}: `{l}`"
                    ));
                };
                hunks.last_mut().unwrap().lines.push(Line::new(kind, body));
                i += 1;
            }
            if i < lines.len() && lines[i].trim_end_matches('\r') == "*** End of File" {
                if let Some(last) = hunks.last_mut().filter(|h| !h.lines.is_empty()) {
                    last.eof = true;
                }
                i += 1;
            }
            hunks.retain(|h| !h.lines.is_empty());
            if hunks.is_empty() {
                return Err(format!("apply_patch: {path} has no changes"));
            }
            files.push(FilePatch {
                path,
                change: Change::Update(hunks),
            });
            continue;
        }
        if let Some(rest) = line.strip_prefix("*** Add File:") {
            let path = rest.trim().to_string();
            if path.is_empty() {
                return Err("apply_patch: `*** Add File:` needs a path".into());
            }
            i += 1;
            let mut added = Vec::new();
            while i < lines.len() && !lines[i].starts_with("*** ") {
                let l = lines[i];
                if l.is_empty() {
                    added.push(String::new());
                } else if let Some(b) = l.strip_prefix('+') {
                    added.push(b.trim_end_matches('\r').to_string());
                } else {
                    return Err(format!(
                        "apply_patch: expected `+` lines for new file {path}: `{l}`"
                    ));
                }
                i += 1;
            }
            files.push(FilePatch {
                path,
                change: Change::Create(added),
            });
            continue;
        }
        if let Some(rest) = line.strip_prefix("*** Delete File:") {
            let path = rest.trim().to_string();
            if path.is_empty() {
                return Err("apply_patch: `*** Delete File:` needs a path".into());
            }
            files.push(FilePatch {
                path,
                change: Change::Delete,
            });
            i += 1;
            continue;
        }
        if line.trim_start().starts_with("*** Move to:") {
            return Err("apply_patch: `*** Move to:` (renames) is not supported".into());
        }
        if line.trim().is_empty() {
            i += 1;
            continue;
        }
        return Err(format!("apply_patch: unexpected line `{line}`"));
    }
    if !saw_end {
        return Err("apply_patch: missing `*** End Patch`".into());
    }
    if files.is_empty() {
        return Err("apply_patch: no file changes found".into());
    }
    Ok(files)
}

// ------------------------------------------------------------------- apply

/// Whether every newline in `s` is a CRLF (the `edit_file` rule).
fn is_crlf(s: &str) -> bool {
    s.contains("\r\n") && !s.replace("\r\n", "").contains('\n')
}

/// The (context + remove) and (context + add) halves of a hunk.
fn sides(hunk: &Hunk) -> (Vec<&Line>, Vec<&Line>) {
    let mut want = Vec::new();
    let mut repl = Vec::new();
    for line in &hunk.lines {
        match line.kind {
            Kind::Context => {
                want.push(line);
                repl.push(line);
            }
            Kind::Remove => want.push(line),
            Kind::Add => repl.push(line),
        }
    }
    (want, repl)
}

fn matches_at(lines: &[String], pos: usize, want: &[&Line], rtrim: bool) -> bool {
    if pos + want.len() > lines.len() {
        return false;
    }
    let eq = |a: &str, b: &str| {
        if rtrim {
            a.trim_end() == b.trim_end()
        } else {
            a == b
        }
    };
    lines[pos..pos + want.len()]
        .iter()
        .zip(want)
        .all(|(have, want)| eq(have, &want.text))
}

/// The position where `want` begins, preferring the hunk's hinted line and
/// tolerating offsets and trailing whitespace. `end_anchored` candidates
/// must run to the last line.
fn find(
    path: &str,
    hunk_no: usize,
    lines: &[String],
    want: &[&Line],
    hint: usize,
    end_anchored: bool,
    has_nl: bool,
) -> Result<usize, String> {
    let n = want.len();
    let span = if end_anchored {
        lines.len().saturating_sub(n)..=lines.len().saturating_sub(n)
    } else {
        0..=lines.len().saturating_sub(n)
    };
    // A trailing `\ No newline` marker pins the match to a newline-less EOF.
    let eof_ok = |pos: usize| {
        want.last().map(|l| l.no_nl) != Some(true)
            || (pos + n == lines.len() && !has_nl)
    };
    for rtrim in [false, true] {
        let mut best: Option<usize> = None;
        for pos in span.clone() {
            if eof_ok(pos) && matches_at(lines, pos, want, rtrim) {
                let better = best.is_none_or(|b| pos.abs_diff(hint) < b.abs_diff(hint));
                if better {
                    best = Some(pos);
                }
            }
        }
        if let Some(pos) = best {
            return Ok(pos);
        }
    }
    let first = want
        .first()
        .map(|l| l.text.as_str())
        .unwrap_or("")
        .trim_end();
    Err(format!(
        "hunk {hunk_no} of {path}: context not found (near line {}, starting `{first}`)",
        hint + 1
    ))
}

/// Apply `hunks` to `old`, returning the new content. CRLF files stay CRLF.
pub fn apply(path: &str, old: &str, change: &Change) -> Result<String, String> {
    let crlf = is_crlf(old);
    let norm = if crlf {
        old.replace("\r\n", "\n")
    } else {
        old.to_string()
    };
    let applied = match change {
        Change::Create(_) => {
            return Err(format!("{path}: cannot create an existing file"));
        }
        Change::Delete => String::new(),
        Change::Update(hunks) => apply_update(path, &norm, hunks)?,
    };
    Ok(if crlf && !applied.is_empty() {
        applied.replace('\n', "\r\n")
    } else {
        applied
    })
}

/// The contents a `Create` change produces.
pub fn created(lines: &[String]) -> String {
    if lines.is_empty() {
        String::new()
    } else {
        format!("{}\n", lines.join("\n"))
    }
}

fn apply_update(path: &str, norm: &str, hunks: &[Hunk]) -> Result<String, String> {
    if hunks.is_empty() {
        return Err(format!("{path}: patch has no hunks"));
    }
    let mut has_nl = norm.is_empty() || norm.ends_with('\n');
    let mut lines: Vec<String> = norm
        .split_terminator('\n')
        .map(|l| l.trim_end_matches('\r').to_string())
        .collect();
    // `split_terminator` drops the marker on a trailing "\n"; a file that does
    // not end with one keeps its last line. Both forms land here correctly.
    let mut shift: isize = 0;
    for (no, hunk) in hunks.iter().enumerate() {
        let (want, repl) = sides(hunk);
        let hint = hunk
            .hint
            .map(|h| (h as isize + shift).clamp(0, lines.len() as isize) as usize);
        if want.is_empty() {
            // Pure insertion: at the hint when given, else EOF.
            let pos = if hunk.eof {
                lines.len()
            } else {
                hint.unwrap_or(lines.len())
            };
            let at_eof = pos == lines.len();
            lines.splice(pos..pos, repl.iter().map(|l| l.text.clone()));
            if at_eof {
                has_nl = repl.last().map(|l| !l.no_nl).unwrap_or(has_nl);
            }
            shift += repl.len() as isize;
            continue;
        }
        let pos = find(
            path,
            no + 1,
            &lines,
            &want,
            hint.unwrap_or(0),
            hunk.eof,
            has_nl,
        )?;
        let at_eof = pos + want.len() == lines.len();
        lines.splice(
            pos..pos + want.len(),
            repl.iter().map(|l| l.text.clone()),
        );
        if at_eof {
            has_nl = repl.last().map(|l| !l.no_nl).unwrap_or(has_nl);
        }
        shift += repl.len() as isize - want.len() as isize;
    }
    let mut out = lines.join("\n");
    if has_nl && !out.is_empty() {
        out.push('\n');
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply_str(old: &str, change: &Change) -> Result<String, String> {
        apply("f.txt", old, change)
    }

    #[test]
    fn unified_single_file() {
        let patch = "\
--- a/f.txt
+++ b/f.txt
@@ -1,3 +1,3 @@
 one
-two
+TWO
 three
";
        let files = parse(patch).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "f.txt");
        let out = apply("f.txt", "one\ntwo\nthree\n", &files[0].change).unwrap();
        assert_eq!(out, "one\nTWO\nthree\n");
    }

    #[test]
    fn unified_multi_file_and_create_delete() {
        let patch = "\
diff --git a/a.txt b/a.txt
index 111..222 100644
--- a/a.txt
+++ b/a.txt
@@ -1 +1 @@
-old
+new
--- a/gone.txt
+++ /dev/null
@@ -1,2 +0,0 @@
-bye
-now
--- /dev/null
+++ b/made.txt
@@ -0,0 +1,2 @@
+hello
+world
";
        let files = parse(patch).unwrap();
        assert_eq!(files.len(), 3);
        assert_eq!(files[0].path, "a.txt");
        assert_eq!(apply_str("old\n", &files[0].change).unwrap(), "new\n");
        assert!(matches!(files[1].change, Change::Delete));
        assert_eq!(files[1].path, "gone.txt");
        let Change::Create(lines) = &files[2].change else {
            panic!("expected create");
        };
        assert_eq!(created(lines), "hello\nworld\n");
    }

    #[test]
    fn hunks_apply_at_fuzzed_offsets() {
        // The file gained a line before the hunk; the -5 hint is stale.
        let patch = "\
--- a/f.txt
+++ b/f.txt
@@ -5,3 +5,3 @@
 a
-b
+B
 c
";
        let files = parse(patch).unwrap();
        let out = apply_str("x\ny\na\nb\nc\nz\n", &files[0].change).unwrap();
        assert_eq!(out, "x\ny\na\nB\nc\nz\n");
    }

    #[test]
    fn hunks_match_with_trailing_whitespace_fuzz() {
        let patch = "\
--- a/f.txt
+++ b/f.txt
@@ -1,2 +1,2 @@
-a
+b
 keep
";
        let files = parse(patch).unwrap();
        let out = apply_str("a   \nkeep\n", &files[0].change).unwrap();
        assert_eq!(out, "b\nkeep\n");
    }

    #[test]
    fn hunks_apply_sequentially_with_shift() {
        let patch = "\
--- a/f.txt
+++ b/f.txt
@@ -1,2 +1,3 @@
 a
+extra
 b
@@ -4,2 +5,2 @@
 d
-e
+E
";
        let files = parse(patch).unwrap();
        let out = apply_str("a\nb\nc\nd\ne\n", &files[0].change).unwrap();
        assert_eq!(out, "a\nextra\nb\nc\nd\nE\n");
    }

    #[test]
    fn no_newline_markers() {
        // Removing the file's last, unterminated line.
        let patch = "\
--- a/f.txt
+++ b/f.txt
@@ -1,2 +1,1 @@
 keep
-tail
\\ No newline at end of file
";
        let files = parse(patch).unwrap();
        let out = apply_str("keep\ntail", &files[0].change).unwrap();
        assert_eq!(out, "keep\n");

        // Adding a line after an unterminated tail.
        let patch = "\
--- a/f.txt
+++ b/f.txt
@@ -1,2 +1,3 @@
 keep
-tail
\\ No newline at end of file
+tail
+more
\\ No newline at end of file
";
        let files = parse(patch).unwrap();
        let out = apply_str("keep\ntail", &files[0].change).unwrap();
        assert_eq!(out, "keep\ntail\nmore");
    }

    #[test]
    fn codex_update_add_delete() {
        let patch = "\
*** Begin Patch
*** Update File: src/a.rs
 fn main() {
-    old();
+    new();
 }
*** Add File: notes.md
+hello
+world
*** Delete File: dead.rs
*** End Patch
";
        let files = parse(patch).unwrap();
        assert_eq!(files.len(), 3);
        assert_eq!(files[0].path, "src/a.rs");
        let out = apply_str("fn main() {\n    old();\n}\n", &files[0].change).unwrap();
        assert_eq!(out, "fn main() {\n    new();\n}\n");
        let Change::Create(lines) = &files[1].change else {
            panic!("expected create");
        };
        assert_eq!(created(lines), "hello\nworld\n");
        assert!(matches!(files[2].change, Change::Delete));
    }

    #[test]
    fn codex_sections_and_end_of_file() {
        let patch = "\
*** Begin Patch
*** Update File: f.txt
@@
 head
-mid
+MID
@@
 foot
-tail
+TAIL
*** End of File
*** End Patch
";
        let files = parse(patch).unwrap();
        let Change::Update(hunks) = &files[0].change else {
            panic!("expected update");
        };
        assert_eq!(hunks.len(), 2);
        assert!(hunks[1].eof);
        let out = apply_str("head\nmid\nx\nfoot\ntail\n", &files[0].change).unwrap();
        assert_eq!(out, "head\nMID\nx\nfoot\nTAIL\n");
    }

    #[test]
    fn codex_missing_end_is_an_error() {
        let err = parse("*** Begin Patch\n*** Delete File: a\n").unwrap_err();
        assert!(err.contains("End Patch"), "{err}");
    }

    #[test]
    fn parse_errors() {
        assert!(parse("not a patch").is_err());
        assert!(parse("--- a/f.txt\n").is_err());
        assert!(
            parse("--- a/f.txt\n+++ b/f.txt\n@@ -1 +1 @@\n-x\n").is_err() // declared counts short
                || parse("--- a/f.txt\n+++ b/f.txt\n@@ -1,1 +1,1 @@\n-x\n").is_ok()
        );
        let p = parse("*** Begin Patch\n*** Move to: b\n*** End Patch\n");
        assert!(p.is_err());
    }

    #[test]
    fn context_not_found_reports_hunk() {
        let patch = "--- a/f.txt\n+++ b/f.txt\n@@ -1 +1 @@\n-nope\n+y\n";
        let files = parse(patch).unwrap();
        let err = apply_str("something\nelse\n", &files[0].change).unwrap_err();
        assert!(err.contains("hunk 1") && err.contains("context not found"), "{err}");
    }

    #[test]
    fn paths_scan() {
        let unified = "--- a/x.rs\n+++ b/x.rs\n@@ -1 +1 @@\n-a\n+b\n--- a/y.rs\n+++ /dev/null\n@@ -1 +0,0 @@\n-z\n";
        assert_eq!(paths(unified), ["x.rs", "y.rs"]);
        let codex = "*** Begin Patch\n*** Update File: u.rs\n*** Add File: n.rs\n*** Delete File: d.rs\n*** End Patch\n";
        assert_eq!(paths(codex), ["u.rs", "n.rs", "d.rs"]);
    }

    #[test]
    fn crlf_files_stay_crlf() {
        let patch = "--- a/f.txt\n+++ b/f.txt\n@@ -1,2 +1,2 @@\n a\n-b\n+B\n";
        let files = parse(patch).unwrap();
        let out = apply_str("a\r\nb\r\n", &files[0].change).unwrap();
        assert_eq!(out, "a\r\nB\r\n");
    }
}
