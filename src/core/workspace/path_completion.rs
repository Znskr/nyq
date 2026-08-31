//! Path-shaped @-mention completion for explicit parent queries.

use std::path::Path;

use crate::core::shared::text_utils;
use crate::core::workspace::file_index::{CandidateKind, MatchSpan, SearchResult};
use crate::core::workspace::pathing;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueryMode {
    WorkspaceIndex,
    ExplicitPath,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    NoSpaceLeft,
    PathUnavailable,
}

struct ParsedQuery<'a> {
    parent: &'a [u8],
    display_prefix: &'a [u8],
    basename_prefix: &'a [u8],
}

pub fn query_mode(query: &[u8]) -> QueryMode {
    for &byte in query {
        if is_sep(byte) {
            return QueryMode::ExplicitPath;
        }
    }
    QueryMode::WorkspaceIndex
}

pub fn complete(
    workspace_root: &[u8],
    query: &[u8],
    max_results: usize,
) -> Result<Vec<SearchResult>, Error> {
    if max_results == 0 {
        return Ok(Vec::new());
    }
    let Some(parsed) = parse_explicit_query(query) else {
        return Ok(Vec::new());
    };

    let resolved = pathing::resolve_workspace_or_external_path(workspace_root, parsed.parent)
        .map_err(|_| Error::PathUnavailable)?;

    let entries = std::fs::read_dir(path_from_bytes(&resolved).ok_or(Error::PathUnavailable)?)
        .map_err(|_| Error::PathUnavailable)?;

    let mut results: Vec<SearchResult> = Vec::new();
    for entry in entries {
        let Ok(entry) = entry else { continue };
        let name = entry.file_name();
        let name = os_str_bytes(&name);
        if !starts_with_ignore_case(name, parsed.basename_prefix) {
            continue;
        }
        if !text_utils::is_terminal_safe(name) {
            continue;
        }
        let Some(kind) = candidate_kind(&entry) else {
            continue;
        };

        let mut candidate = Vec::with_capacity(parsed.display_prefix.len() + name.len());
        candidate.extend_from_slice(parsed.display_prefix);
        candidate.extend_from_slice(name);
        if !text_utils::is_terminal_safe(&candidate) {
            continue;
        }
        if !is_representable(&candidate) {
            continue;
        }
        insert_candidate(&mut results, max_results, candidate, kind);
    }

    let mut spans_used: Vec<MatchSpan> = Vec::new();
    for result in &mut results {
        if parsed.basename_prefix.is_empty() {
            result.matched_spans = Vec::new();
            continue;
        }
        result.matched_spans = vec![MatchSpan {
            byte_start: parsed.display_prefix.len().min(u16::MAX as usize) as u16,
            byte_end: (parsed.display_prefix.len() + parsed.basename_prefix.len())
                .min(u16::MAX as usize) as u16,
        }];
        spans_used.push(result.matched_spans[0]);
    }
    let _ = spans_used;
    Ok(results)
}

pub fn is_current_candidate_kind(
    workspace_root: &[u8],
    path: &[u8],
    expected_kind: CandidateKind,
) -> bool {
    if !text_utils::is_terminal_safe(path) {
        return false;
    }
    let resolved = match pathing::resolve_workspace_or_external_path(workspace_root, path) {
        Ok(resolved) => resolved,
        Err(_) => return false,
    };
    let Ok(meta) = std::fs::metadata(path_from_bytes(&resolved).unwrap_or(Path::new(""))) else {
        return false;
    };
    kind_matches_stat(expected_kind, &meta)
}

fn parse_explicit_query(query: &[u8]) -> Option<ParsedQuery<'_>> {
    let mut separator_index: Option<usize> = None;
    for (index, &byte) in query.iter().enumerate() {
        if is_sep(byte) {
            separator_index = Some(index);
        }
    }
    let separator = separator_index?;
    let parent = if separator == 0 {
        &query[0..1]
    } else {
        &query[0..separator]
    };
    Some(ParsedQuery {
        parent,
        display_prefix: &query[0..separator + 1],
        basename_prefix: &query[separator + 1..],
    })
}

fn candidate_kind(entry: &std::fs::DirEntry) -> Option<CandidateKind> {
    let file_type = entry.file_type().ok()?;
    if file_type.is_file() {
        return Some(CandidateKind::File);
    }
    if file_type.is_dir() {
        return Some(CandidateKind::Directory);
    }
    // Symlinks (and unknown kinds) are resolved through their target.
    let meta = std::fs::metadata(entry.path()).ok()?;
    let ft = meta.file_type();
    if ft.is_file() {
        Some(CandidateKind::File)
    } else if ft.is_dir() {
        Some(CandidateKind::Directory)
    } else {
        None
    }
}

fn kind_matches_stat(expected: CandidateKind, meta: &std::fs::Metadata) -> bool {
    let ft = meta.file_type();
    match expected {
        CandidateKind::File => ft.is_file(),
        CandidateKind::Directory => ft.is_dir(),
    }
}

fn insert_candidate(
    out: &mut Vec<SearchResult>,
    max_results: usize,
    path: Vec<u8>,
    kind: CandidateKind,
) {
    let mut insertion_index = 0;
    while insertion_index < out.len() && path.as_slice() > out[insertion_index].path.as_slice() {
        insertion_index += 1;
    }
    if insertion_index >= max_results {
        return;
    }
    out.insert(
        insertion_index,
        SearchResult {
            path,
            kind,
            matched_spans: Vec::new(),
        },
    );
    out.truncate(max_results);
}

fn starts_with_ignore_case(bytes: &[u8], prefix: &[u8]) -> bool {
    bytes.len() >= prefix.len() && bytes[..prefix.len()].eq_ignore_ascii_case(prefix)
}

fn is_representable(path: &[u8]) -> bool {
    if !path.is_empty() && path[0] == b'"' {
        return false;
    }
    for &b in path {
        if b == b'\t' || b == b'\n' || b == b'\r' {
            return false;
        }
    }
    !path.contains(&b' ') || !path.contains(&b'"')
}

fn is_sep(byte: u8) -> bool {
    byte == b'/' || (cfg!(windows) && byte == b'\\')
}

#[cfg(unix)]
fn path_from_bytes(bytes: &[u8]) -> Option<&Path> {
    use std::os::unix::ffi::OsStrExt;
    Some(Path::new(std::ffi::OsStr::from_bytes(bytes)))
}

#[cfg(not(unix))]
fn path_from_bytes(bytes: &[u8]) -> Option<&Path> {
    std::str::from_utf8(bytes).ok().map(Path::new)
}

#[cfg(unix)]
fn os_str_bytes(value: &std::ffi::OsStr) -> &[u8] {
    use std::os::unix::ffi::OsStrExt;
    value.as_bytes()
}

#[cfg(not(unix))]
fn os_str_bytes(value: &std::ffi::OsStr) -> &[u8] {
    let s = value.to_string_lossy();
    let s: &str = &s;
    s.as_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_root(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "nyq-path-completion-{}-{}",
            name,
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn write_test_file(root: &Path, rel: &str) {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, b"").unwrap();
    }

    fn realpath(root: &Path, rel: &str) -> Vec<u8> {
        std::fs::canonicalize(root.join(rel))
            .unwrap()
            .to_string_lossy()
            .as_bytes()
            .to_vec()
    }

    #[test]
    fn path_completion_classifies_and_splits_only_path_shaped_queries() {
        assert_eq!(QueryMode::WorkspaceIndex, query_mode(b"main"));
        assert_eq!(QueryMode::WorkspaceIndex, query_mode(b"~"));
        assert_eq!(QueryMode::ExplicitPath, query_mode(b"~/Dow"));
        assert_eq!(QueryMode::ExplicitPath, query_mode(b"/tmp/fi"));
        assert_eq!(QueryMode::ExplicitPath, query_mode(b"./src/"));
        assert_eq!(QueryMode::ExplicitPath, query_mode(b"../shared/"));
        assert_eq!(QueryMode::ExplicitPath, query_mode(b"src/core/"));

        let parsed = parse_explicit_query(b"../shared/na").unwrap();
        assert_eq!(b"../shared", parsed.parent);
        assert_eq!(b"../shared/", parsed.display_prefix);
        assert_eq!(b"na", parsed.basename_prefix);
    }

    #[test]
    fn path_completion_enumerates_immediate_entries_with_deterministic_bounded_order() {
        let root = temp_root("enumeration");
        std::fs::create_dir_all(root.join("workspace/src/empty")).unwrap();
        std::fs::create_dir_all(root.join("workspace/src/nested/deeper")).unwrap();
        write_test_file(&root, "workspace/src/zeta.txt");
        write_test_file(&root, "workspace/src/Alpha.txt");
        write_test_file(&root, "workspace/src/beta.txt");
        write_test_file(&root, "workspace/src/.hidden.txt");
        write_test_file(&root, "workspace/src/space \" file.txt");
        let workspace = realpath(&root, "workspace");

        let results = complete(&workspace, b"./src/", 3).unwrap();
        assert_eq!(3, results.len());
        assert_eq!(b"./src/.hidden.txt", results[0].path.as_slice());
        assert_eq!(b"./src/Alpha.txt", results[1].path.as_slice());
        assert_eq!(b"./src/beta.txt", results[2].path.as_slice());

        let filtered = complete(&workspace, b"src/al", 3).unwrap();
        assert_eq!(1, filtered.len());
        assert_eq!(b"src/Alpha.txt", filtered[0].path.as_slice());
        assert_eq!(CandidateKind::File, filtered[0].kind);
        assert_eq!(1, filtered[0].matched_spans.len());
        assert_eq!(b"src/".len() as u16, filtered[0].matched_spans[0].byte_start);
        assert_eq!(b"src/al".len() as u16, filtered[0].matched_spans[0].byte_end);
        assert_eq!(0, complete(&workspace, b"src/space", 3).unwrap().len());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn path_completion_resolves_parent_and_absolute_forms_without_recursive_traversal() {
        let root = temp_root("parent-absolute");
        std::fs::create_dir_all(root.join("workspace")).unwrap();
        std::fs::create_dir_all(root.join("outside/empty")).unwrap();
        write_test_file(&root, "outside/external.txt");
        let workspace = realpath(&root, "workspace");
        let outside = realpath(&root, "outside");

        let parent = complete(&workspace, b"../outside/", 8).unwrap();
        assert_eq!(2, parent.len());
        assert_eq!(b"../outside/empty", parent[0].path.as_slice());
        assert_eq!(CandidateKind::Directory, parent[0].kind);
        assert_eq!(b"../outside/external.txt", parent[1].path.as_slice());

        let absolute_query = format!("{}/ex", String::from_utf8(outside.clone()).unwrap());
        let absolute = complete(&workspace, absolute_query.as_bytes(), 8).unwrap();
        assert_eq!(1, absolute.len());
        let expected = format!("{}/external.txt", String::from_utf8(outside).unwrap());
        assert_eq!(expected.as_bytes(), absolute[0].path.as_slice());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn path_completion_follows_listed_symlinks_and_filters_unsafe_names() {
        let root = temp_root("symlinks");
        std::fs::create_dir_all(root.join("workspace/target-dir")).unwrap();
        write_test_file(&root, "workspace/target.txt");
        write_test_file(&root, "workspace/unsafe-\x1b.txt");
        std::os::unix::fs::symlink("target-dir", root.join("workspace/linked-dir")).unwrap();
        std::os::unix::fs::symlink("target.txt", root.join("workspace/linked-file")).unwrap();
        let workspace = realpath(&root, "workspace");

        let results = complete(&workspace, b"./linked", 4).unwrap();
        assert_eq!(2, results.len());
        assert_eq!(CandidateKind::Directory, results[0].kind);
        assert_eq!(CandidateKind::File, results[1].kind);
        assert_eq!(0, complete(&workspace, b"./unsafe", 4).unwrap().len());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn path_completion_reports_unavailable_parents_and_bounded_output() {
        assert_eq!(
            Err(Error::PathUnavailable),
            complete(b"/", b"/definitely/missing/fx-path/", 1)
        );
        assert_eq!(0, complete(b"/", b"not/a/path", 0).unwrap().len());
    }

    #[test]
    fn path_completion_validates_the_current_explicit_candidate_kind() {
        let root = temp_root("validate-kind");
        std::fs::create_dir_all(root.join("workspace/folder")).unwrap();
        write_test_file(&root, "workspace/file.txt");
        let workspace = realpath(&root, "workspace");

        assert!(is_current_candidate_kind(&workspace, b"./folder", CandidateKind::Directory));
        assert!(!is_current_candidate_kind(&workspace, b"./folder", CandidateKind::File));
        assert!(is_current_candidate_kind(&workspace, b"./file.txt", CandidateKind::File));
        assert!(!is_current_candidate_kind(&workspace, b"./missing", CandidateKind::File));

        let _ = std::fs::remove_dir_all(&root);
    }
}
