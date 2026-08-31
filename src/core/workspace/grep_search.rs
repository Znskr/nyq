//! Literal grep search over workspace files with a git-grep fast path and a
//! bounded recursive fallback.

use std::collections::HashMap;
use std::path::Path;
use std::process::{Command, Stdio};

use crate::core::shared::{debug_trace, text_utils};
use crate::core::workspace::{glob_pattern, pathing, workspace_files};

/// Maximum number of matches rendered to model-visible output.
pub const output_cap: usize = 200;

/// Maximum number of matching lines collected before grep stops scanning.
pub const collection_cap: usize = 2000;

const FILE_BYTE_CAP: usize = 50 * 1024 * 4;
const GIT_GREP_STDOUT_LIMIT: usize = 8 * 1024 * 1024;

static DEFAULT_IGNORED_NAMES: &[&[u8]] = &[
    b".git".as_slice(),
    b".zig-cache".as_slice(),
    b"zig-out".as_slice(),
    b"node_modules".as_slice(),
    b".next".as_slice(),
    b"dist".as_slice(),
    b"build".as_slice(),
    b"coverage".as_slice(),
];

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Match {
    pub absolute_path: Vec<u8>,
    pub line_number: usize,
    pub line: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TruncatedReason {
    CollectionCap,
}

#[derive(Clone, Debug, Default)]
pub struct Result {
    pub matches: Vec<Match>,
    pub truncated_reason: Option<TruncatedReason>,
    pub candidate_count: usize,
    pub candidate_cap: usize,
    pub candidate_incomplete: bool,
    pub skipped_overlong: usize,
}

#[derive(Clone, Debug, Default)]
pub struct CountResult {
    pub matching_lines: usize,
    pub matching_files: usize,
    pub candidate_count: usize,
    pub candidate_cap: usize,
    pub candidate_incomplete: bool,
    pub skipped_overlong: usize,
}

type GrepResult<T> = std::result::Result<T, GrepError>;

#[derive(Debug)]
pub enum GrepError {
    Cancelled,
    GitGrepFailed,
    GitGrepUnsupported,
    Io(std::io::Error),
}

impl From<std::io::Error> for GrepError {
    fn from(err: std::io::Error) -> Self {
        GrepError::Io(err)
    }
}

impl std::fmt::Display for GrepError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GrepError::Cancelled => write!(f, "cancelled"),
            GrepError::GitGrepFailed => write!(f, "git grep failed"),
            GrepError::GitGrepUnsupported => write!(f, "git grep unsupported"),
            GrepError::Io(err) => write!(f, "{err}"),
        }
    }
}

pub fn collect_directory_matches(
    workspace_root: &[u8],
    absolute_root: &[u8],
    pattern: &[u8],
    case_insensitive: bool,
    include: Option<&glob_pattern::Pattern<'_>>,
) -> GrepResult<Result> {
    collect_directory_matches_with_options(
        workspace_root,
        absolute_root,
        pattern,
        case_insensitive,
        DEFAULT_IGNORED_NAMES,
        include,
        &workspace_files::Options::default(),
    )
}

pub fn collect_directory_matches_with_ignored(
    workspace_root: &[u8],
    absolute_root: &[u8],
    pattern: &[u8],
    case_insensitive: bool,
    ignored_names: &'static [&'static [u8]],
    include: Option<&glob_pattern::Pattern<'_>>,
) -> GrepResult<Result> {
    collect_directory_matches_with_options(
        workspace_root,
        absolute_root,
        pattern,
        case_insensitive,
        ignored_names,
        include,
        &workspace_files::Options::default(),
    )
}

pub fn count_directory_matches_with_ignored(
    workspace_root: &[u8],
    absolute_root: &[u8],
    pattern: &[u8],
    case_insensitive: bool,
    ignored_names: &'static [&'static [u8]],
    include: Option<&glob_pattern::Pattern<'_>>,
) -> GrepResult<CountResult> {
    count_directory_matches_with_options(
        workspace_root,
        absolute_root,
        pattern,
        case_insensitive,
        ignored_names,
        include,
        &workspace_files::Options::default(),
    )
}

#[allow(clippy::too_many_arguments)]
fn collect_directory_matches_with_options(
    workspace_root: &[u8],
    absolute_root: &[u8],
    pattern: &[u8],
    case_insensitive: bool,
    ignored_names: &'static [&'static [u8]],
    include: Option<&glob_pattern::Pattern<'_>>,
    workspace_options: &workspace_files::Options,
) -> GrepResult<Result> {
    let mut matches: Vec<Match> = Vec::new();
    let mut truncated_reason;
    let mut stats = CandidateStats {
        cap: workspace_options.candidate_cap,
        ..CandidateStats::default()
    };

    if !workspace_options.force_fallback && !git_ignores_root(absolute_root) {
        let git_result = git_grep_tracked_matches(
            workspace_root,
            absolute_root,
            pattern,
            case_insensitive,
            include,
            &mut matches,
        );
        if let Ok(tracked) = git_result {
            stats.count += tracked.candidate_count;
            truncated_reason = tracked.truncated_reason;

            if truncated_reason.is_none() {
                let untracked_options = workspace_files::Options {
                    candidate_cap: workspace_options.candidate_cap,
                    git_stdout_limit: workspace_options.git_stdout_limit,
                    only_untracked: true,
                    ..workspace_files::Options::default()
                };
                match workspace_file_candidates(absolute_root, ignored_names, &untracked_options) {
                    Ok(untracked) => {
                        stats.add(&untracked.result);
                        truncated_reason = scan_candidate_list(
                            workspace_root,
                            &untracked.provider_root,
                            &untracked.result.files,
                            pattern,
                            case_insensitive,
                            include,
                            &mut matches,
                        )?;
                    }
                    Err(err) => {
                        debug_trace::logf(
                            "core",
                            format_args!(
                                "grep_files untracked fallback skipped root={} err={}",
                                lossy(absolute_root),
                                err
                            ),
                        );
                        return finish_matches(matches, truncated_reason, stats);
                    }
                }
            }
            return finish_matches(matches, truncated_reason, stats);
        }
    }

    let mut discovery_options = workspace_options.clone();
    discovery_options.include_untracked = true;
    let candidates = workspace_file_candidates(absolute_root, ignored_names, &discovery_options)?;
    stats.add(&candidates.result);
    truncated_reason = scan_candidate_list(
        workspace_root,
        &candidates.provider_root,
        &candidates.result.files,
        pattern,
        case_insensitive,
        include,
        &mut matches,
    )?;

    finish_matches(matches, truncated_reason, stats)
}

#[allow(clippy::too_many_arguments)]
fn count_directory_matches_with_options(
    workspace_root: &[u8],
    absolute_root: &[u8],
    pattern: &[u8],
    case_insensitive: bool,
    ignored_names: &'static [&'static [u8]],
    include: Option<&glob_pattern::Pattern<'_>>,
    workspace_options: &workspace_files::Options,
) -> GrepResult<CountResult> {
    let mut count_result = CountResult {
        candidate_cap: workspace_options.candidate_cap,
        ..CountResult::default()
    };
    let mut stats = CandidateStats {
        cap: workspace_options.candidate_cap,
        ..CandidateStats::default()
    };

    if !workspace_options.force_fallback && !git_ignores_root(absolute_root) {
        if let Ok(tracked) =
            git_grep_tracked_counts(workspace_root, absolute_root, pattern, case_insensitive, include)
        {
            count_result.matching_lines += tracked.matching_lines;
            count_result.matching_files += tracked.matching_files;
            stats.count += tracked.candidate_count;

            let untracked_options = workspace_files::Options {
                candidate_cap: workspace_options.candidate_cap,
                git_stdout_limit: workspace_options.git_stdout_limit,
                only_untracked: true,
                ..workspace_files::Options::default()
            };
            match workspace_file_candidates(absolute_root, ignored_names, &untracked_options) {
                Ok(untracked) => {
                    stats.add(&untracked.result);
                    count_candidate_list(
                        workspace_root,
                        &untracked.provider_root,
                        &untracked.result.files,
                        pattern,
                        case_insensitive,
                        include,
                        &mut count_result,
                    )?;
                }
                Err(_) => {
                    return Ok(finish_count(count_result, stats));
                }
            }
            return Ok(finish_count(count_result, stats));
        }
    }

    let mut discovery_options = workspace_options.clone();
    discovery_options.include_untracked = true;
    let candidates = workspace_file_candidates(absolute_root, ignored_names, &discovery_options)?;
    stats.add(&candidates.result);
    count_candidate_list(
        workspace_root,
        &candidates.provider_root,
        &candidates.result.files,
        pattern,
        case_insensitive,
        include,
        &mut count_result,
    )?;
    Ok(finish_count(count_result, stats))
}

pub fn collect_regular_file_root(
    workspace_root: &[u8],
    absolute_path: &[u8],
    pattern: &[u8],
    case_insensitive: bool,
    include: Option<&glob_pattern::Pattern<'_>>,
) -> GrepResult<Result> {
    let _ = workspace_root;
    let mut matches: Vec<Match> = Vec::new();

    if let Some(include_pattern) = include {
        if !include_pattern.matches_basename(basename(absolute_path)) {
            return finish_matches(
                matches,
                None,
                CandidateStats {
                    cap: workspace_files::default_candidate_cap,
                    ..CandidateStats::default()
                },
            );
        }
    }

    let truncated_reason = scan_file(absolute_path, pattern, case_insensitive, &mut matches)?;
    finish_matches(
        matches,
        truncated_reason,
        CandidateStats {
            count: 1,
            cap: workspace_files::default_candidate_cap,
            ..CandidateStats::default()
        },
    )
}

pub fn count_regular_file_root(
    workspace_root: &[u8],
    absolute_path: &[u8],
    pattern: &[u8],
    case_insensitive: bool,
    include: Option<&glob_pattern::Pattern<'_>>,
) -> GrepResult<CountResult> {
    let _ = workspace_root;
    if let Some(include_pattern) = include {
        if !include_pattern.matches_basename(basename(absolute_path)) {
            return Ok(finish_count(
                CountResult::default(),
                CandidateStats {
                    cap: workspace_files::default_candidate_cap,
                    ..CandidateStats::default()
                },
            ));
        }
    }

    let file_count = count_file(absolute_path, pattern, case_insensitive)?;
    Ok(finish_count(
        CountResult {
            matching_lines: file_count.matching_lines,
            matching_files: if file_count.matching_lines > 0 { 1 } else { 0 },
            ..CountResult::default()
        },
        CandidateStats {
            count: 1,
            cap: workspace_files::default_candidate_cap,
            ..CandidateStats::default()
        },
    ))
}

#[derive(Clone, Debug, Default)]
struct CandidateStats {
    count: usize,
    cap: usize,
    incomplete: bool,
    skipped_overlong: usize,
}

impl CandidateStats {
    fn add(&mut self, result: &workspace_files::Result) {
        self.count += result.files.len();
        self.cap = result.candidate_cap;
        self.incomplete = self.incomplete || result.incomplete;
        self.skipped_overlong += result.skipped_overlong;
    }
}

fn finish_matches(
    matches: Vec<Match>,
    truncated_reason: Option<TruncatedReason>,
    stats: CandidateStats,
) -> GrepResult<Result> {
    Ok(Result {
        matches,
        truncated_reason,
        candidate_count: stats.count,
        candidate_cap: stats.cap,
        candidate_incomplete: stats.incomplete,
        skipped_overlong: stats.skipped_overlong,
    })
}

fn finish_count(mut result: CountResult, stats: CandidateStats) -> CountResult {
    result.candidate_count = stats.count;
    result.candidate_cap = stats.cap;
    result.candidate_incomplete = stats.incomplete;
    result.skipped_overlong += stats.skipped_overlong;
    result
}

struct WorkspaceCandidates {
    result: workspace_files::Result,
    provider_root: Vec<u8>,
}

fn workspace_file_candidates(
    absolute_root: &[u8],
    ignored_names: &'static [&'static [u8]],
    options: &workspace_files::Options,
) -> GrepResult<WorkspaceCandidates> {
    let mut discover_options = options.clone();
    discover_options.ignored_names = ignored_names.to_vec();

    let result = workspace_files::discover(absolute_root, &discover_options)
        .map_err(map_workspace_error)?;
    Ok(WorkspaceCandidates {
        result,
        provider_root: absolute_root.to_vec(),
    })
}

fn map_workspace_error(err: workspace_files::Error) -> GrepError {
    match err {
        workspace_files::Error::Cancelled => GrepError::Cancelled,
        _ => GrepError::GitGrepFailed,
    }
}

struct CandidateFile {
    display_path: Vec<u8>,
    read_path: Vec<u8>,
}

fn resolve_candidate_file(
    workspace_root: &[u8],
    provider_root: &[u8],
    candidate: &[u8],
) -> GrepResult<Option<CandidateFile>> {
    let absolute_match = join_bytes(provider_root, candidate);
    let entry_meta = match fs_symlink_metadata(&absolute_match) {
        Ok(meta) => meta,
        Err(_) => {
            debug_trace::logf(
                "core",
                format_args!(
                    "grep_files scan skipped path={} err=stat_failed",
                    lossy(&absolute_match)
                ),
            );
            return Ok(None);
        }
    };
    let ft = entry_meta.file_type();
    if !ft.is_file() && !ft.is_symlink() {
        return Ok(None);
    }

    match resolve_directory_entry_target(workspace_root, &absolute_match, ft.is_symlink()) {
        Ok(Some(read_path)) => Ok(Some(CandidateFile {
            display_path: absolute_match,
            read_path,
        })),
        Ok(None) => Ok(None),
        Err(_) => {
            debug_trace::logf(
                "core",
                format_args!(
                    "grep_files scan skipped path={} err=resolve_failed",
                    lossy(&absolute_match)
                ),
            );
            Ok(None)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn scan_candidate_list(
    workspace_root: &[u8],
    provider_root: &[u8],
    candidates: &[Vec<u8>],
    pattern: &[u8],
    case_insensitive: bool,
    include: Option<&glob_pattern::Pattern<'_>>,
    matches: &mut Vec<Match>,
) -> GrepResult<Option<TruncatedReason>> {
    for candidate in candidates {
        if let Some(include_pattern) = include {
            if !include_pattern.matches_path(candidate) {
                continue;
            }
        }

        let Some(candidate_file) =
            resolve_candidate_file(workspace_root, provider_root, candidate)?
        else {
            continue;
        };
        match scan_file_at(
            &candidate_file.display_path,
            &candidate_file.read_path,
            pattern,
            case_insensitive,
            matches,
        ) {
            Ok(Some(reason)) => return Ok(Some(reason)),
            Ok(None) => {}
            Err(_) => continue,
        }
    }
    Ok(None)
}

#[allow(clippy::too_many_arguments)]
fn count_candidate_list(
    workspace_root: &[u8],
    provider_root: &[u8],
    candidates: &[Vec<u8>],
    pattern: &[u8],
    case_insensitive: bool,
    include: Option<&glob_pattern::Pattern<'_>>,
    count_result: &mut CountResult,
) -> GrepResult<()> {
    for candidate in candidates {
        if let Some(include_pattern) = include {
            if !include_pattern.matches_path(candidate) {
                continue;
            }
        }

        let Some(candidate_file) =
            resolve_candidate_file(workspace_root, provider_root, candidate)?
        else {
            continue;
        };
        let Ok(file_count) =
            count_file_at(&candidate_file.read_path, pattern, case_insensitive)
        else {
            continue;
        };
        if file_count.matching_lines == 0 {
            continue;
        }
        count_result.matching_files += 1;
        count_result.matching_lines += file_count.matching_lines;
    }
    Ok(())
}

pub fn git_grep_argv_for_test() -> [&'static [u8]; 9] {
    [
        b"git",
        b"--no-optional-locks",
        b"grep",
        b"-n",
        b"-I",
        b"-F",
        b"-z",
        b"-e",
        b"needle",
    ]
}

struct GitGrepMatchesResult {
    candidate_count: usize,
    truncated_reason: Option<TruncatedReason>,
}

struct FileCount {
    matching_lines: usize,
}

fn git_ignores_root(absolute_root: &[u8]) -> bool {
    let argv: [&[u8]; 6] = [
        b"git",
        b"--no-optional-locks",
        b"check-ignore",
        b"-q",
        b"--",
        b".",
    ];
    match run_command(absolute_root, &argv, 1024) {
        Ok(output) => output.code == 0,
        Err(_) => false,
    }
}

#[allow(clippy::too_many_arguments)]
fn git_grep_tracked_matches(
    workspace_root: &[u8],
    absolute_root: &[u8],
    pattern: &[u8],
    case_insensitive: bool,
    include: Option<&glob_pattern::Pattern<'_>>,
    matches: &mut Vec<Match>,
) -> GrepResult<GitGrepMatchesResult> {
    if pattern.is_empty() {
        return Err(GrepError::GitGrepUnsupported);
    }

    let mut argv: Vec<&[u8]> =
        vec![b"git", b"--no-optional-locks", b"grep", b"-n", b"-I", b"-F", b"-z"];
    if case_insensitive {
        argv.push(b"-i");
    }
    argv.push(b"-e");
    argv.push(pattern);
    argv.push(b"--");
    argv.push(safe_git_include_pathspec(include).unwrap_or(b"."));

    let output = run_command(absolute_root, &argv, GIT_GREP_STDOUT_LIMIT)
        .map_err(|_| GrepError::GitGrepFailed)?;
    if output.stdout.len() > GIT_GREP_STDOUT_LIMIT {
        return Err(GrepError::GitGrepFailed);
    }
    match output.code {
        0 => parse_git_grep_matches(
            workspace_root,
            absolute_root,
            &output.stdout,
            include,
            matches,
        ),
        1 => Ok(GitGrepMatchesResult {
            candidate_count: 0,
            truncated_reason: None,
        }),
        _ => Err(GrepError::GitGrepFailed),
    }
}

#[allow(clippy::too_many_arguments)]
fn git_grep_tracked_counts(
    workspace_root: &[u8],
    absolute_root: &[u8],
    pattern: &[u8],
    case_insensitive: bool,
    include: Option<&glob_pattern::Pattern<'_>>,
) -> GrepResult<CountResult> {
    if pattern.is_empty() {
        return Err(GrepError::GitGrepUnsupported);
    }

    let mut argv: Vec<&[u8]> =
        vec![b"git", b"--no-optional-locks", b"grep", b"--count", b"-I", b"-F", b"-z"];
    if case_insensitive {
        argv.push(b"-i");
    }
    argv.push(b"-e");
    argv.push(pattern);
    argv.push(b"--");
    argv.push(safe_git_include_pathspec(include).unwrap_or(b"."));

    let output = run_command(absolute_root, &argv, GIT_GREP_STDOUT_LIMIT)
        .map_err(|_| GrepError::GitGrepFailed)?;
    if output.stdout.len() > GIT_GREP_STDOUT_LIMIT {
        return Err(GrepError::GitGrepFailed);
    }
    match output.code {
        0 => parse_git_grep_counts(workspace_root, absolute_root, &output.stdout, include),
        1 => Ok(CountResult::default()),
        _ => Err(GrepError::GitGrepFailed),
    }
}

fn parse_git_grep_matches(
    workspace_root: &[u8],
    absolute_root: &[u8],
    raw: &[u8],
    include: Option<&glob_pattern::Pattern<'_>>,
    matches: &mut Vec<Match>,
) -> GrepResult<GitGrepMatchesResult> {
    let mut result = GitGrepMatchesResult {
        candidate_count: 0,
        truncated_reason: None,
    };
    let mut display_paths: HashMap<Vec<u8>, Vec<u8>> = HashMap::new();
    let mut skipped_paths: std::collections::HashSet<Vec<u8>> = std::collections::HashSet::new();

    let mut index: usize = 0;
    while index < raw.len() {
        let Some(path_end) = find_scalar_from(raw, index, 0) else {
            break;
        };
        let path = &raw[index..path_end];
        let line_start = path_end + 1;
        let Some(line_end) = find_scalar_from(raw, line_start, 0) else {
            break;
        };
        let content_start = line_end + 1;
        let content_end = find_scalar_from(raw, content_start, b'\n').unwrap_or(raw.len());
        index = if content_end < raw.len() {
            content_end + 1
        } else {
            raw.len()
        };

        if path.is_empty() || path.len() > workspace_files::max_relative_path_bytes {
            continue;
        }
        if let Some(include_pattern) = include {
            if !include_pattern.matches_path(path) {
                continue;
            }
        }
        if skipped_paths.contains(path) {
            continue;
        }

        let Some(line_number) = std::str::from_utf8(&raw[line_start..line_end])
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
        else {
            continue;
        };
        let line = &raw[content_start..content_end];
        if !text_utils::is_model_safe_text(line) {
            skipped_paths.insert(path.to_vec());
            continue;
        }

        let display_path = if let Some(existing) = display_paths.get(path) {
            existing.clone()
        } else {
            let Some(absolute_match) =
                validate_matched_git_file(workspace_root, absolute_root, path)?
            else {
                skipped_paths.insert(path.to_vec());
                continue;
            };
            result.candidate_count += 1;
            display_paths.insert(path.to_vec(), absolute_match.clone());
            absolute_match
        };

        if matches.len() >= collection_cap {
            result.truncated_reason = Some(TruncatedReason::CollectionCap);
            break;
        }
        append_match(matches, &display_path, line_number, line);
        if matches.len() >= collection_cap {
            result.truncated_reason = Some(TruncatedReason::CollectionCap);
            break;
        }
    }

    Ok(result)
}

fn parse_git_grep_counts(
    workspace_root: &[u8],
    absolute_root: &[u8],
    raw: &[u8],
    include: Option<&glob_pattern::Pattern<'_>>,
) -> GrepResult<CountResult> {
    let mut result = CountResult::default();
    let mut skipped_paths: std::collections::HashSet<Vec<u8>> = std::collections::HashSet::new();

    let mut index: usize = 0;
    while index < raw.len() {
        let Some(path_end) = find_scalar_from(raw, index, 0) else {
            break;
        };
        let path = &raw[index..path_end];
        let count_start = path_end + 1;
        let count_end = find_scalar_from(raw, count_start, b'\n').unwrap_or(raw.len());
        index = if count_end < raw.len() {
            count_end + 1
        } else {
            raw.len()
        };

        if path.is_empty() || path.len() > workspace_files::max_relative_path_bytes {
            continue;
        }
        if let Some(include_pattern) = include {
            if !include_pattern.matches_path(path) {
                continue;
            }
        }
        if skipped_paths.contains(path) {
            continue;
        }

        let raw_count = trim_end(&raw[count_start..count_end], b'\r');
        let Some(line_count) = std::str::from_utf8(raw_count)
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
        else {
            continue;
        };
        if line_count == 0 {
            continue;
        }
        if validate_matched_git_file(workspace_root, absolute_root, path)?.is_none() {
            skipped_paths.insert(path.to_vec());
            continue;
        }
        result.candidate_count += 1;
        result.matching_files += 1;
        result.matching_lines += line_count;
    }

    Ok(result)
}

fn validate_matched_git_file(
    workspace_root: &[u8],
    absolute_root: &[u8],
    path: &[u8],
) -> GrepResult<Option<Vec<u8>>> {
    let absolute_match = join_bytes(absolute_root, path);
    let meta = match fs_symlink_metadata(&absolute_match) {
        Ok(meta) => meta,
        Err(_) => return Ok(None),
    };
    let ft = meta.file_type();
    if !ft.is_file() && !ft.is_symlink() {
        return Ok(None);
    }
    if meta.len() > FILE_BYTE_CAP as u64 {
        log_oversized_file(&absolute_match);
        return Ok(None);
    }

    let resolved_match = if ft.is_symlink() {
        let Some(target) = resolve_directory_entry_target(workspace_root, &absolute_match, ft.is_symlink())?
        else {
            return Ok(None);
        };
        target
    } else {
        absolute_match.clone()
    };
    if read_model_safe_content(&absolute_match, &resolved_match).is_none() {
        return Ok(None);
    }
    Ok(Some(absolute_match))
}

fn safe_git_include_pathspec<'a>(
    include: Option<&'a glob_pattern::Pattern<'a>>,
) -> Option<&'a [u8]> {
    let include = include?;
    let raw = include.raw;
    if raw.is_empty() || raw.contains(&b'/') {
        return None;
    }
    for &ch in raw {
        if ch == b'\\' || ch == b'{' || ch == b'}' {
            return None;
        }
    }
    Some(raw)
}

fn find_scalar_from(haystack: &[u8], start: usize, needle: u8) -> Option<usize> {
    haystack[start..]
        .iter()
        .position(|&b| b == needle)
        .map(|offset| start + offset)
}

fn resolve_directory_entry_target(
    workspace_root: &[u8],
    absolute_path: &[u8],
    entry_is_symlink: bool,
) -> GrepResult<Option<Vec<u8>>> {
    let resolved = match std::fs::canonicalize(path_from_bytes(absolute_path).unwrap_or(Path::new(""))) {
        Ok(path) => bytes_from_path(&path),
        Err(err) => return Err(GrepError::Io(err)),
    };
    if entry_is_symlink && !pathing::path_inside(workspace_root, &resolved) {
        debug_trace::logf(
            "core",
            format_args!(
                "grep_files skipped external symlink target path={} target={}",
                lossy(absolute_path),
                lossy(&resolved)
            ),
        );
        return Ok(None);
    }
    Ok(Some(resolved))
}

fn scan_file(
    absolute_path: &[u8],
    pattern: &[u8],
    case_insensitive: bool,
    matches: &mut Vec<Match>,
) -> GrepResult<Option<TruncatedReason>> {
    scan_file_at(absolute_path, absolute_path, pattern, case_insensitive, matches)
}

fn count_file(
    absolute_path: &[u8],
    pattern: &[u8],
    case_insensitive: bool,
) -> GrepResult<FileCount> {
    count_file_at(absolute_path, pattern, case_insensitive)
}

fn count_file_at(
    read_path: &[u8],
    pattern: &[u8],
    case_insensitive: bool,
) -> GrepResult<FileCount> {
    let Some(content) = read_model_safe_content(read_path, read_path) else {
        return Ok(FileCount { matching_lines: 0 });
    };

    let mut result = FileCount { matching_lines: 0 };
    for line in content.split(|&b| b == b'\n') {
        if line_matches_pattern(line, pattern, case_insensitive) {
            result.matching_lines += 1;
        }
    }
    Ok(result)
}

fn scan_file_at(
    display_path: &[u8],
    read_path: &[u8],
    pattern: &[u8],
    case_insensitive: bool,
    matches: &mut Vec<Match>,
) -> GrepResult<Option<TruncatedReason>> {
    let Some(content) = read_model_safe_content(display_path, read_path) else {
        return Ok(None);
    };

    for (index, line) in content.split(|&b| b == b'\n').enumerate() {
        if !line_matches_pattern(line, pattern, case_insensitive) {
            continue;
        }
        if matches.len() >= collection_cap {
            return Ok(Some(TruncatedReason::CollectionCap));
        }
        append_match(matches, display_path, index + 1, line);
        if matches.len() >= collection_cap {
            return Ok(Some(TruncatedReason::CollectionCap));
        }
    }
    Ok(None)
}

fn read_model_safe_content(display_path: &[u8], read_path: &[u8]) -> Option<Vec<u8>> {
    let meta = fs_metadata(read_path).ok()?;
    if meta.len() > FILE_BYTE_CAP as u64 {
        log_oversized_file(display_path);
        return None;
    }
    let content = std::fs::read(path_from_bytes(read_path).unwrap_or(Path::new(""))).ok()?;
    if content.len() > FILE_BYTE_CAP {
        log_oversized_file(display_path);
        return None;
    }
    if !text_utils::is_model_safe_text(&content) {
        let reason = model_unsafe_reason(&content);
        debug_trace::logf(
            "core",
            format_args!(
                "grep_files skipped non-text file path={} reason={}",
                lossy(display_path),
                reason
            ),
        );
        return None;
    }
    Some(content)
}

fn append_match(
    matches: &mut Vec<Match>,
    absolute_path: &[u8],
    line_number: usize,
    line: &[u8],
) {
    matches.push(Match {
        absolute_path: absolute_path.to_vec(),
        line_number,
        line: line.to_vec(),
    });
}

fn log_oversized_file(absolute_path: &[u8]) {
    debug_trace::logf(
        "core",
        format_args!(
            "grep_files skipped oversized file path={}",
            lossy(absolute_path)
        ),
    );
}

fn model_unsafe_reason(content: &[u8]) -> &'static str {
    if content.contains(&0) {
        "contains_nul"
    } else if std::str::from_utf8(content).is_err() {
        "invalid_utf8"
    } else {
        "not_model_safe"
    }
}

fn line_matches_pattern(line: &[u8], pattern: &[u8], case_insensitive: bool) -> bool {
    if case_insensitive {
        text_utils::contains_ignore_case(line, pattern)
    } else {
        line.windows(pattern.len()).any(|w| w == pattern)
    }
}

struct CommandOutput {
    code: i32,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

fn run_command(cwd: &[u8], argv: &[&[u8]], stdout_limit: usize) -> std::io::Result<CommandOutput> {
    let mut command = Command::new(os_from_bytes(argv[0]));
    for arg in &argv[1..] {
        command.arg(os_from_bytes(arg));
    }
    command
        .current_dir(os_from_bytes(cwd))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = command.output()?;
    let mut stdout = output.stdout;
    stdout.truncate(stdout_limit);
    Ok(CommandOutput {
        code: output.status.code().unwrap_or(-1),
        stdout,
        stderr: output.stderr,
    })
}

fn find_scalar(haystack: &[u8], needle: u8) -> Option<usize> {
    haystack.iter().position(|&b| b == needle)
}

fn join_bytes(a: &[u8], b: &[u8]) -> Vec<u8> {
    if a.is_empty() {
        return b.to_vec();
    }
    let mut out = Vec::with_capacity(a.len() + 1 + b.len());
    out.extend_from_slice(a);
    out.push(b'/');
    out.extend_from_slice(b);
    out
}

fn trim_end<'a>(mut bytes: &'a [u8], pattern: u8) -> &'a [u8] {
    while bytes.last() == Some(&pattern) {
        bytes = &bytes[..bytes.len() - 1];
    }
    bytes
}

fn basename(path: &[u8]) -> &[u8] {
    let mut end = path.len();
    while end != 0 && path[end - 1] == b'/' {
        end -= 1;
    }
    if end == 0 {
        return &path[..0];
    }
    let mut start = end - 1;
    while start != 0 && path[start - 1] != b'/' {
        start -= 1;
    }
    &path[start..end]
}

fn lossy(bytes: &[u8]) -> std::borrow::Cow<'_, str> {
    String::from_utf8_lossy(bytes)
}

fn fs_symlink_metadata(path: &[u8]) -> std::io::Result<std::fs::Metadata> {
    std::fs::symlink_metadata(path_from_bytes(path).ok_or_else(invalid_input)?)
}

fn fs_metadata(path: &[u8]) -> std::io::Result<std::fs::Metadata> {
    std::fs::metadata(path_from_bytes(path).ok_or_else(invalid_input)?)
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
fn bytes_from_path(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes().to_vec()
}

#[cfg(not(unix))]
fn bytes_from_path(path: &Path) -> Vec<u8> {
    path.to_string_lossy().as_bytes().to_vec()
}

#[cfg(unix)]
fn os_from_bytes(bytes: &[u8]) -> std::ffi::OsString {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::OsStr::from_bytes(bytes).to_os_string()
}

#[cfg(not(unix))]
fn os_from_bytes(bytes: &[u8]) -> std::ffi::OsString {
    std::ffi::OsString::from(String::from_utf8_lossy(bytes).into_owned())
}

fn invalid_input() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, "non-utf8 path")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::Mutex;

    static TRACE_LOCK: Mutex<()> = Mutex::new(());

    fn temp_root(name: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("nyq-grep-search-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn write_temp_file(root: &Path, sub_path: &str, content: &[u8]) -> Vec<u8> {
        let path = root.join(sub_path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, content).unwrap();
        std::fs::canonicalize(&path)
            .unwrap()
            .to_string_lossy()
            .as_bytes()
            .to_vec()
    }

    fn workspace_root(root: &Path) -> Vec<u8> {
        std::fs::canonicalize(root)
            .unwrap()
            .to_string_lossy()
            .as_bytes()
            .to_vec()
    }

    fn git_available() -> bool {
        Command::new("git")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn run_git_for_test(cwd: &[u8], args: &[&str]) {
        let mut command = Command::new("git");
        command.args(args).current_dir(os_from_bytes(cwd));
        let output = command.output().unwrap();
        assert!(output.status.success(), "git {args:?} failed");
    }

    fn content_with_prefix(len: usize, prefix: &[u8]) -> Vec<u8> {
        let mut content = vec![b'x'; len];
        let n = prefix.len().min(content.len());
        content[..n].copy_from_slice(&prefix[..n]);
        content
    }

    fn read_trace(trace_path: &[u8]) -> Vec<u8> {
        std::fs::read(path_from_bytes(trace_path).unwrap()).unwrap()
    }

    fn trace_contains(trace: &[u8], needle: &str) -> bool {
        String::from_utf8_lossy(trace).contains(needle)
    }

    #[test]
    fn grep_search_git_grep_argv_uses_literal_fixed_string_flags() {
        let argv = git_grep_argv_for_test();
        assert_eq!(b"git", argv[0]);
        assert_eq!(b"--no-optional-locks", argv[1]);
        assert_eq!(b"grep", argv[2]);
        assert_eq!(b"-n", argv[3]);
        assert_eq!(b"-I", argv[4]);
        assert_eq!(b"-F", argv[5]);
        assert_eq!(b"-z", argv[6]);
        assert_eq!(b"-e", argv[7]);
    }

    #[test]
    fn grep_search_preserves_explicitly_requested_ignored_directory_roots() {
        let root = temp_root("ignored-dir-root");
        let workspace = workspace_root(&root);
        let ignored = write_temp_file(&root, "node_modules/pkg/ignored.txt", b"needle ignored\n");

        let absolute_root = std::fs::canonicalize(root.join("node_modules/pkg"))
            .unwrap()
            .to_string_lossy()
            .as_bytes()
            .to_vec();
        let result =
            collect_directory_matches(&workspace, &absolute_root, b"needle", false, None).unwrap();

        assert_eq!(1, result.matches.len());
        assert!(result.truncated_reason.is_none());
        assert!(result.matches[0]
            .absolute_path
            .ends_with(b"node_modules/pkg/ignored.txt"));
        assert_eq!(b"needle ignored", result.matches[0].line.as_slice());
        assert_eq!(ignored, result.matches[0].absolute_path);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn grep_search_preserves_explicitly_requested_ignored_file_roots() {
        let root = temp_root("ignored-file-root");
        let workspace = workspace_root(&root);
        let ignored = write_temp_file(&root, "node_modules/pkg/ignored.txt", b"needle ignored\n");

        let result =
            collect_regular_file_root(&workspace, &ignored, b"needle", false, None).unwrap();

        assert_eq!(1, result.matches.len());
        assert!(result.truncated_reason.is_none());
        assert!(result.matches[0].absolute_path.ends_with(b"node_modules/pkg/ignored.txt"));
        assert_eq!(b"needle ignored", result.matches[0].line.as_slice());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn grep_search_does_not_ignore_workspace_because_ignored_name_is_outside_workspace() {
        let root = temp_root("ignored-outside");
        let path = write_temp_file(&root, "build/workspace/src/file.txt", b"needle kept\n");
        let workspace = std::fs::canonicalize(root.join("build/workspace"))
            .unwrap()
            .to_string_lossy()
            .as_bytes()
            .to_vec();

        let result = collect_directory_matches(&workspace, &workspace, b"needle", false, None).unwrap();

        assert_eq!(1, result.matches.len());
        assert!(result.truncated_reason.is_none());
        assert!(result.matches[0].absolute_path.ends_with(b"src/file.txt"));
        assert_eq!(path, result.matches[0].absolute_path);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn grep_search_falls_back_to_recursive_scanner_outside_git_repositories() {
        let root = temp_root("recursive-fallback");
        let workspace = workspace_root(&root);
        let file_path = write_temp_file(&root, "src/main.zig", b"needle fallback\n");

        let result = collect_directory_matches(&workspace, &workspace, b"needle", false, None).unwrap();

        assert_eq!(1, result.matches.len());
        assert!(result.matches[0].absolute_path.ends_with(b"src/main.zig"));
        assert_eq!(b"needle fallback", result.matches[0].line.as_slice());
        assert_eq!(file_path, result.matches[0].absolute_path);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn grep_search_scans_untracked_files_after_git_grep_tracked_backend() {
        if !git_available() {
            return;
        }
        let root = temp_root("untracked-after-git");
        let workspace = workspace_root(&root);

        run_git_for_test(&workspace, &["init", "--quiet"]);
        let tracked = write_temp_file(&root, "tracked.txt", b"no match\n");
        let _ = tracked;
        run_git_for_test(&workspace, &["add", "tracked.txt"]);
        let untracked = write_temp_file(&root, "untracked.txt", b"needle untracked\n");

        let result = collect_directory_matches(&workspace, &workspace, b"needle", false, None).unwrap();

        assert_eq!(1, result.matches.len());
        assert!(result.matches[0].absolute_path.ends_with(b"untracked.txt"));
        assert_eq!(b"needle untracked", result.matches[0].line.as_slice());
        assert_eq!(untracked, result.matches[0].absolute_path);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn grep_search_git_grep_backend_skips_files_with_unsafe_bytes_outside_matched_line() {
        if !git_available() {
            return;
        }
        let root = temp_root("unsafe-git");
        let workspace = workspace_root(&root);
        run_git_for_test(&workspace, &["init", "--quiet"]);
        let unsafe_path = write_temp_file(&root, "unsafe.txt", b"needle safe\ninvalid \xff bytes\n");
        let _ = unsafe_path;
        run_git_for_test(&workspace, &["add", "unsafe.txt"]);

        let result = collect_directory_matches(&workspace, &workspace, b"needle", false, None).unwrap();
        assert_eq!(0, result.matches.len());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn grep_search_logs_skipped_non_model_safe_files() {
        let _lock = TRACE_LOCK.lock().unwrap();
        let root = temp_root("non-model-safe");
        let workspace = workspace_root(&root);
        let binary = write_temp_file(&root, "binary.txt", b"needle\x00binary\n");
        let trace_path = root.join("trace.log");
        debug_trace::reset_for_test();
        debug_trace::configure_for_test(&trace_path).unwrap();

        let result = collect_regular_file_root(&workspace, &binary, b"needle", false, None).unwrap();
        assert_eq!(0, result.matches.len());
        assert!(result.truncated_reason.is_none());

        let trace = std::fs::read(&trace_path).unwrap();
        assert!(trace_contains(&trace, "grep_files skipped non-text file path="));
        assert!(trace_contains(&trace, "binary.txt"));
        assert!(trace_contains(&trace, "reason=contains_nul"));

        debug_trace::reset_for_test();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn grep_search_logs_skipped_per_file_scan_errors_during_directory_traversal() {
        let _lock = TRACE_LOCK.lock().unwrap();
        let root = temp_root("scan-error-log");
        let workspace = workspace_root(&root);
        let good = write_temp_file(&root, "good.txt", b"needle good\n");
        let _ = good;
        std::os::unix::fs::symlink("missing-target.txt", root.join("broken.txt")).unwrap();
        let trace_path = root.join("trace.log");
        debug_trace::reset_for_test();
        debug_trace::configure_for_test(&trace_path).unwrap();

        let pattern = glob_pattern::Pattern::compile(b"*.txt").unwrap();
        let result =
            collect_directory_matches(&workspace, &workspace, b"needle", false, Some(&pattern))
                .unwrap();
        assert_eq!(1, result.matches.len());
        assert!(result.truncated_reason.is_none());

        let trace = std::fs::read(&trace_path).unwrap();
        assert!(trace_contains(&trace, "grep_files scan skipped path="));
        assert!(trace_contains(&trace, "broken.txt"));

        debug_trace::reset_for_test();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn grep_search_directory_traversal_skips_external_symlink_targets_with_trace() {
        let _lock = TRACE_LOCK.lock().unwrap();
        let workspace_tmp = temp_root("external-symlink-workspace");
        let external_tmp = temp_root("external-symlink-external");

        let workspace = workspace_root(&workspace_tmp);
        let internal_target =
            write_temp_file(&workspace_tmp, "target/internal.txt", b"needle internal\n");
        let external_target = write_temp_file(&external_tmp, "outside.txt", b"needle external\n");
        std::fs::create_dir_all(workspace_tmp.join("links")).unwrap();
        std::os::unix::fs::symlink("../target/internal.txt", workspace_tmp.join("links/internal.txt"))
            .unwrap();
        std::os::unix::fs::symlink(
            String::from_utf8(external_target.clone()).unwrap(),
            workspace_tmp.join("links/external.txt"),
        )
            .unwrap();

        let trace_path = workspace_tmp.join("trace.log");
        debug_trace::reset_for_test();
        debug_trace::configure_for_test(&trace_path).unwrap();

        let links_root = std::fs::canonicalize(workspace_tmp.join("links"))
            .unwrap()
            .to_string_lossy()
            .as_bytes()
            .to_vec();
        let result =
            collect_directory_matches(&workspace, &links_root, b"needle", false, None).unwrap();

        assert_eq!(1, result.matches.len());
        assert!(result.matches[0].absolute_path.ends_with(b"links/internal.txt"));
        assert_eq!(b"needle internal", result.matches[0].line.as_slice());

        let trace = std::fs::read(&trace_path).unwrap();
        assert!(trace_contains(&trace, "grep_files skipped external symlink target path="));
        assert!(trace_contains(&trace, "links/external.txt"));

        debug_trace::reset_for_test();
        let _ = std::fs::remove_dir_all(&workspace_tmp);
        let _ = std::fs::remove_dir_all(&external_tmp);
    }

    #[test]
    fn grep_search_scans_files_exactly_at_byte_cap() {
        let root = temp_root("exact-cap");
        let workspace = workspace_root(&root);
        let content = content_with_prefix(FILE_BYTE_CAP, b"needle\n");
        let exact = write_temp_file(&root, "exact.txt", &content);

        let result = collect_regular_file_root(&workspace, &exact, b"needle", false, None).unwrap();

        assert_eq!(1, result.matches.len());
        assert!(result.truncated_reason.is_none());
        assert!(result.matches[0].absolute_path.ends_with(b"exact.txt"));
        assert_eq!(1, result.matches[0].line_number);
        assert_eq!(b"needle", result.matches[0].line.as_slice());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn grep_search_logs_oversized_files_and_continues_directory_traversal() {
        let _lock = TRACE_LOCK.lock().unwrap();
        let root = temp_root("oversized-continue");
        let workspace = workspace_root(&root);
        let good = write_temp_file(&root, "good.txt", b"needle good\n");
        let _ = good;
        let content = content_with_prefix(FILE_BYTE_CAP + 1, b"needle hidden\n");
        let large = write_temp_file(&root, "large.txt", &content);
        let _ = large;
        let trace_path = root.join("trace.log");
        debug_trace::reset_for_test();
        debug_trace::configure_for_test(&trace_path).unwrap();

        let result = collect_directory_matches(&workspace, &workspace, b"needle", false, None).unwrap();
        assert_eq!(1, result.matches.len());
        assert!(result.truncated_reason.is_none());
        assert!(result.matches[0].absolute_path.ends_with(b"good.txt"));

        let trace = std::fs::read(&trace_path).unwrap();
        assert!(trace_contains(&trace, "grep_files skipped oversized file path="));
        assert!(trace_contains(&trace, "large.txt"));

        debug_trace::reset_for_test();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn grep_search_skips_oversized_regular_file_roots_with_trace() {
        let _lock = TRACE_LOCK.lock().unwrap();
        let root = temp_root("oversized-root");
        let workspace = workspace_root(&root);
        let content = content_with_prefix(FILE_BYTE_CAP + 1, b"needle hidden\n");
        let large = write_temp_file(&root, "large.txt", &content);
        let trace_path = root.join("trace.log");
        debug_trace::reset_for_test();
        debug_trace::configure_for_test(&trace_path).unwrap();

        let result = collect_regular_file_root(&workspace, &large, b"needle", false, None).unwrap();
        assert_eq!(0, result.matches.len());
        assert!(result.truncated_reason.is_none());

        let trace = std::fs::read(&trace_path).unwrap();
        assert!(trace_contains(&trace, "grep_files skipped oversized file path="));
        assert!(trace_contains(&trace, "large.txt"));

        debug_trace::reset_for_test();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn grep_search_finds_match_beyond_former_traversal_cap() {
        let root = temp_root("beyond-cap");
        let workspace = workspace_root(&root);
        for i in 0..2050 {
            write_temp_file(&root, &format!("many/file-{i:0>4}.txt"), b"not here\n");
        }
        let late = write_temp_file(&root, "zzzz/match.txt", b"needle late\n");

        let result = collect_directory_matches(&workspace, &workspace, b"needle", false, None).unwrap();

        assert_eq!(1, result.matches.len());
        assert!(result.truncated_reason.is_none());
        assert!(result.matches[0].absolute_path.ends_with(b"zzzz/match.txt"));
        assert_eq!(late, result.matches[0].absolute_path);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn grep_files_path_narrowing_applies_before_candidate_cap() {
        let root = temp_root("path-narrowing");
        let workspace = workspace_root(&root);
        let outside = write_temp_file(&root, "aaa/outside.txt", b"needle outside\n");
        let _ = outside;
        let target = write_temp_file(&root, "src/core/target.txt", b"needle target\n");
        let narrowed_root = std::fs::canonicalize(root.join("src/core"))
            .unwrap()
            .to_string_lossy()
            .as_bytes()
            .to_vec();

        let options = workspace_files::Options {
            candidate_cap: 1,
            force_fallback: true,
            ..workspace_files::Options::default()
        };
        let result = collect_directory_matches_with_options(
            &workspace,
            &narrowed_root,
            b"needle",
            false,
            DEFAULT_IGNORED_NAMES,
            None,
            &options,
        )
        .unwrap();

        assert_eq!(1, result.candidate_count);
        assert!(!result.candidate_incomplete);
        assert_eq!(1, result.matches.len());
        assert!(result.matches[0].absolute_path.ends_with(b"src/core/target.txt"));
        assert_eq!(b"needle target", result.matches[0].line.as_slice());
        assert_eq!(target, result.matches[0].absolute_path);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn grep_search_collection_cap_saturation_reports_metadata() {
        let root = temp_root("collection-cap");
        let workspace = workspace_root(&root);
        let mut content = Vec::new();
        for _ in 0..(collection_cap + 1) {
            content.extend_from_slice(b"needle\n");
        }
        let path = write_temp_file(&root, "many.txt", &content);

        let result = collect_regular_file_root(&workspace, &path, b"needle", false, None).unwrap();

        assert_eq!(collection_cap, result.matches.len());
        assert_eq!(Some(TruncatedReason::CollectionCap), result.truncated_reason);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn grep_search_collection_cap_takes_precedence_at_traversal_boundary() {
        let root = temp_root("collection-boundary");
        let workspace = workspace_root(&root);
        for i in 0..100 {
            write_temp_file(&root, &format!("file-{i:0>4}.txt"), b"not here\n");
        }
        let mut content = Vec::new();
        for _ in 0..(collection_cap + 1) {
            content.extend_from_slice(b"needle\n");
        }
        let match_path = write_temp_file(&root, "zzzz/many.txt", &content);
        let _ = match_path;

        let result = collect_directory_matches(&workspace, &workspace, b"needle", false, None).unwrap();

        assert_eq!(collection_cap, result.matches.len());
        assert_eq!(Some(TruncatedReason::CollectionCap), result.truncated_reason);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn grep_search_does_not_import_tool_dispatch_layer() {
        let source = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/core/workspace/grep_search.rs"
        ))
        .unwrap();
        assert!(find_scalar(&source, b't').is_some());
        assert!(!source.windows(15).any(|w| w == b"tool_dispatch.rs"));
    }
}
