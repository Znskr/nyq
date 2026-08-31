//! Background workspace file index for the @-mention picker.
//!
//! Each generation owns immutable path buffers plus pre-lowercased paths and a
//! per-path ASCII bitmap used to reject impossible subsequence matches before
//! scoring. Search ranks by exact/basename fit, boundary matches, prefix,
//! position, longest run, consecutive matches, and gaps.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::core::shared::{display_width, text_utils};
use crate::core::workspace::{
    pathing, unicode_simple_fold, workspace_access, workspace_files,
};

pub const max_indexed_files: usize = 100_000;
pub const max_path_len: u32 = 2048;

const MAX_SEARCH_RESULTS: usize = 64;
const NON_ASCII_MASK: u32 = 1 << 31;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CandidateKind {
    File,
    Directory,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Candidate<'a> {
    pub path: &'a [u8],
    pub kind: CandidateKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MatchSpan {
    pub byte_start: u16,
    pub byte_end: u16,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SearchResult {
    pub path: Vec<u8>,
    pub kind: CandidateKind,
    pub matched_spans: Vec<MatchSpan>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SearchError {
    NoSpaceLeft,
    InvalidIndexData,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum State {
    Idle,
    Loading,
    Ready,
    Failed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BuildError {
    LoadInProgress,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GenerationState {
    Loading,
    Ready,
    Failed,
    Canceled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LoaderOutcome {
    Ready,
    Failed,
    Canceled,
}

impl LoaderOutcome {
    fn terminal_state(self) -> GenerationState {
        match self {
            LoaderOutcome::Ready => GenerationState::Ready,
            LoaderOutcome::Failed => GenerationState::Failed,
            LoaderOutcome::Canceled => GenerationState::Canceled,
        }
    }
}

struct Generation {
    id: usize,
    paths: Vec<Vec<u8>>,
    lower: Vec<Vec<u8>>,
    kinds: Vec<CandidateKind>,
    char_masks: Vec<u32>,
    basename_starts: Vec<usize>,
    ready_count: u32,
    state: GenerationState,
}

impl Generation {
    fn new(id: usize) -> Generation {
        Generation {
            id,
            paths: Vec::new(),
            lower: Vec::new(),
            kinds: Vec::new(),
            char_masks: Vec::new(),
            basename_starts: Vec::new(),
            ready_count: 0,
            state: GenerationState::Loading,
        }
    }

    fn push(&mut self, path: &[u8], kind: CandidateKind) {
        let lower = path.iter().map(|&b| ascii_to_lower(b)).collect::<Vec<u8>>();
        let mut mask: u32 = 0;
        let mut contains_non_ascii = false;
        for &b in path {
            if b >= 0x80 {
                contains_non_ascii = true;
            }
            let lower = ascii_to_lower(b);
            if (b'a'..=b'z').contains(&lower) {
                mask |= 1u32 << (lower - b'a');
            }
        }
        if contains_non_ascii {
            mask |= NON_ASCII_MASK;
        }
        let slash = path.iter().rposition(|&b| b == b'/');
        let basename_start = slash.map_or(0, |index| index + 1);
        self.paths.push(path.to_vec());
        self.lower.push(lower);
        self.kinds.push(kind);
        self.char_masks.push(mask);
        self.basename_starts.push(basename_start);
        self.ready_count = self.paths.len() as u32;
    }
}

#[derive(Default)]
pub struct FileIndex {
    active_generation: Option<Arc<Mutex<Generation>>>,
    loading_generation: Option<Arc<Mutex<Generation>>>,
    roots: Vec<Vec<u8>>,
    pending_roots: Option<Vec<Vec<u8>>>,
    stop_requested: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    generation: usize,
    initial_failed: bool,
}

impl FileIndex {
    pub fn request_stop(&mut self) {
        self.stop_requested.store(true, Ordering::SeqCst);
    }

    pub fn deinit(&mut self) {
        self.request_stop();
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
        self.loading_generation = None;
        self.active_generation = None;
        self.roots.clear();
        self.pending_roots = None;
    }

    pub fn current_state(&self) -> State {
        if self.active_generation.is_some() {
            return State::Ready;
        }
        match &self.loading_generation {
            Some(loading) => match loading.lock().expect("generation lock poisoned").state {
                GenerationState::Loading => State::Loading,
                GenerationState::Ready => State::Ready,
                GenerationState::Failed | GenerationState::Canceled => State::Failed,
            },
            None => {
                if self.initial_failed {
                    State::Failed
                } else {
                    State::Idle
                }
            }
        }
    }

    pub fn count(&self) -> usize {
        let Some(generation) = self.searchable_generation() else {
            return 0;
        };
        let ready_count = generation
            .lock()
            .expect("generation lock poisoned")
            .ready_count as usize;
        ready_count
    }

    pub fn ensure_scope(&mut self, scope: &workspace_access::AccessScope<'_>) {
        if self.current_state() != State::Idle {
            return;
        }
        if scope.primary_directory.is_empty() {
            return;
        }
        self.roots = active_roots_alloc(scope);
        let _ = self.start_load();
    }

    pub fn start_load(&mut self) -> bool {
        if self.roots.is_empty()
            || self.thread.is_some()
            || self.loading_generation.is_some()
            || self.stop_requested.load(Ordering::SeqCst)
        {
            return false;
        }

        let generation_id = self.generation + 1;
        let loading = Arc::new(Mutex::new(Generation::new(generation_id)));
        self.loading_generation = Some(loading.clone());
        self.generation = generation_id;
        self.initial_failed = false;

        let roots = self.roots.clone();
        let stop_requested = self.stop_requested.clone();
        let loading_clone = loading.clone();
        match std::thread::Builder::new()
            .name(format!("file-index-{generation_id}"))
            .spawn(move || {
                loader_thread_main(loading_clone, roots, stop_requested);
            }) {
            Ok(handle) => {
                self.thread = Some(handle);
                true
            }
            Err(_) => {
                self.loading_generation = None;
                if self.active_generation.is_none() {
                    self.initial_failed = true;
                }
                false
            }
        }
    }

    pub fn join_thread_if_done(&mut self) -> bool {
        let Some(loading) = self.loading_generation.clone() else {
            return false;
        };
        let terminal_state = loading.lock().expect("generation lock poisoned").state;
        if terminal_state == GenerationState::Loading {
            return false;
        }

        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
        self.thread = None;

        let mut visible_changed = false;
        match terminal_state {
            GenerationState::Loading => unreachable!(),
            GenerationState::Ready => {
                let previous = self.active_generation.replace(loading);
                self.loading_generation = None;
                self.initial_failed = false;
                drop(previous);
                visible_changed = true;
            }
            GenerationState::Failed | GenerationState::Canceled => {
                self.loading_generation = None;
                if self.active_generation.is_none() && terminal_state == GenerationState::Failed {
                    self.initial_failed = true;
                    visible_changed = true;
                }
            }
        }

        if !self.stop_requested.load(Ordering::SeqCst) {
            if let Some(roots) = self.pending_roots.take() {
                self.roots = roots;
                let _ = self.start_load();
            }
        }
        visible_changed
    }

    pub fn refresh(&mut self) {
        let roots = self
            .pending_roots
            .clone()
            .unwrap_or_else(|| self.roots.clone());
        self.refresh_owned_roots(roots);
    }

    pub fn refresh_scope(&mut self, scope: &workspace_access::AccessScope<'_>) {
        self.refresh_owned_roots(active_roots_alloc(scope));
    }

    fn refresh_owned_roots(&mut self, roots: Vec<Vec<u8>>) {
        if self.stop_requested.load(Ordering::SeqCst) {
            return;
        }
        if self.thread.is_some() {
            self.replace_pending_roots(roots);
            return;
        }
        if self.loading_generation.is_some() {
            return;
        }
        self.roots = roots;
        let _ = self.start_load();
    }

    fn replace_pending_roots(&mut self, roots: Vec<Vec<u8>>) {
        if let Some(pending) = &self.pending_roots {
            if roots_equal(pending, &roots) {
                return;
            }
        }
        self.pending_roots = Some(roots);
    }

    pub fn is_current_candidate_kind(
        &self,
        path: &[u8],
        expected_kind: CandidateKind,
    ) -> bool {
        let current_roots = self.pending_roots.as_ref().unwrap_or(&self.roots);
        if !text_utils::is_terminal_safe(path) || current_roots.is_empty() {
            return false;
        }

        let (root_path, relative): (Vec<u8>, Vec<u8>) = if is_absolute(path) {
            let mut found = None;
            for root in current_roots {
                if !pathing::path_inside(root, path) {
                    continue;
                }
                if let Ok(rel) = pathing::workspace_relative_path(root, path) {
                    found = Some((root.clone(), rel));
                    break;
                }
            }
            let Some(found) = found else { return false };
            found
        } else {
            (current_roots[0].clone(), path.to_vec())
        };

        let absolute = join_bytes(&root_path, &relative);
        let Ok(meta) = fs_symlink_metadata(&absolute) else {
            return false;
        };
        candidate_kind_matches_stat(expected_kind, &meta)
    }

    pub fn build_from_raw(&mut self, raw_list: &[u8]) -> Result<(), BuildError> {
        if self.thread.is_some() || self.loading_generation.is_some() {
            return Err(BuildError::LoadInProgress);
        }
        let generation_id = self.generation + 1;
        let mut generation = Generation::new(generation_id);
        fill_generation_from_raw(&mut generation, raw_list);
        generation.state = GenerationState::Ready;
        let previous = self
            .active_generation
            .replace(Arc::new(Mutex::new(generation)));
        self.generation = generation_id;
        self.initial_failed = false;
        drop(previous);
        Ok(())
    }

    pub fn build_from_candidates(
        &mut self,
        candidates: &[Candidate<'_>],
    ) -> Result<(), BuildError> {
        if self.thread.is_some() || self.loading_generation.is_some() {
            return Err(BuildError::LoadInProgress);
        }
        let indices = candidate_indices(candidates);
        let generation_id = self.generation + 1;
        let mut generation = Generation::new(generation_id);
        for index in indices {
            let candidate = &candidates[index];
            if accepted_candidate(candidate.path, candidate.kind) {
                generation.push(candidate.path, candidate.kind);
            }
        }
        generation.state = GenerationState::Ready;
        let previous = self
            .active_generation
            .replace(Arc::new(Mutex::new(generation)));
        self.generation = generation_id;
        self.initial_failed = false;
        drop(previous);
        Ok(())
    }

    pub fn search_typed(
        &self,
        query: &[u8],
        max_results: usize,
    ) -> Result<Vec<SearchResult>, SearchError> {
        if max_results == 0 {
            return Ok(Vec::new());
        }
        let Some(generation) = self.searchable_generation() else {
            return Ok(Vec::new());
        };
        let lock = generation.lock().expect("generation lock poisoned");
        let total = lock.ready_count as usize;
        if total == 0 || query.len() > max_path_len as usize {
            return Ok(Vec::new());
        }

        if query.is_empty() {
            let result_count = total.min(max_results);
            let mut results = Vec::with_capacity(result_count);
            for index in 0..result_count {
                results.push(SearchResult {
                    path: lock.paths[index].clone(),
                    kind: lock.kinds[index],
                    matched_spans: Vec::new(),
                });
            }
            return Ok(results);
        }

        let Some(prepared) = prepare_query(query) else {
            return Ok(Vec::new());
        };
        let indices = rank_top_n(&lock, total, &prepared);
        let mut results = Vec::with_capacity(indices.len());
        for index in indices {
            let path = &lock.paths[index];
            let base_start = lock.basename_starts[index];
            let spans = reconstruct_match_spans(path, base_start, &prepared.folded)
                .ok_or(SearchError::InvalidIndexData)?;
            results.push(SearchResult {
                path: path.clone(),
                kind: lock.kinds[index],
                matched_spans: spans,
            });
        }
        Ok(results)
    }

    fn searchable_generation(&self) -> Option<Arc<Mutex<Generation>>> {
        if let Some(active) = &self.active_generation {
            return Some(active.clone());
        }
        let loading = self.loading_generation.clone()?;
        let state = loading.lock().expect("generation lock poisoned").state;
        match state {
            GenerationState::Loading | GenerationState::Ready => Some(loading),
            GenerationState::Failed | GenerationState::Canceled => None,
        }
    }
}

fn loader_thread_main(
    loading: Arc<Mutex<Generation>>,
    roots: Vec<Vec<u8>>,
    stop_requested: Arc<AtomicBool>,
) {
    let outcome = load_generation(&loading, &roots, &stop_requested);
    publish_loader_outcome_after_cleanup(&loading, outcome);
}

fn load_generation(
    loading: &Arc<Mutex<Generation>>,
    roots: &[Vec<u8>],
    stop_requested: &AtomicBool,
) -> LoaderOutcome {
    if stop_requested.load(Ordering::SeqCst) {
        return LoaderOutcome::Canceled;
    }
    let candidates = match discover_scope_candidates(roots, stop_requested) {
        Ok(candidates) => candidates,
        Err(()) => {
            if stop_requested.load(Ordering::SeqCst) {
                return LoaderOutcome::Canceled;
            }
            return LoaderOutcome::Failed;
        }
    };

    {
        let mut lock = loading.lock().expect("generation lock poisoned");
        for candidate in &candidates {
            if accepted_candidate(&candidate.path, candidate.kind) {
                lock.push(&candidate.path, candidate.kind);
            }
        }
    }

    if stop_requested.load(Ordering::SeqCst) {
        return LoaderOutcome::Canceled;
    }
    LoaderOutcome::Ready
}

fn publish_loader_outcome_after_cleanup(
    loading: &Arc<Mutex<Generation>>,
    outcome: LoaderOutcome,
) {
    loading.lock().expect("generation lock poisoned").state = outcome.terminal_state();
}

struct OwnedCandidate {
    path: Vec<u8>,
    kind: CandidateKind,
}

fn active_roots_alloc(scope: &workspace_access::AccessScope<'_>) -> Vec<Vec<u8>> {
    let mut roots: Vec<Vec<u8>> = vec![scope.primary_directory.to_vec()];
    for entry in scope.additional_directories {
        if !entry.active {
            continue;
        }
        if roots.iter().any(|root| root == &entry.path) {
            continue;
        }
        roots.push(entry.path.clone());
    }
    roots
}

fn roots_equal(a: &[Vec<u8>], b: &[Vec<u8>]) -> bool {
    a == b
}

fn fill_generation_from_raw(generation: &mut Generation, raw_list: &[u8]) {
    let separator: u8 = if raw_list.contains(&0) { 0 } else { b'\n' };
    for raw in raw_list.split(|&b| b == separator) {
        let Some(path) = accepted_raw_path(separator, raw) else {
            continue;
        };
        if accepted_candidate(path, CandidateKind::File) {
            generation.push(path, CandidateKind::File);
        }
    }
}

fn raw_path_separator(raw_list: &[u8]) -> u8 {
    if raw_list.contains(&0) {
        0
    } else {
        b'\n'
    }
}

fn accepted_raw_path(sep: u8, raw: &[u8]) -> Option<&[u8]> {
    let path = if sep == b'\n' {
        trim_end(raw, b'\r')
    } else {
        raw
    };
    if path.is_empty() || path.len() > max_path_len as usize {
        return None;
    }
    if !text_utils::is_terminal_safe(path) {
        return None;
    }
    Some(path)
}

fn accepted_candidate(path: &[u8], kind: CandidateKind) -> bool {
    if path.is_empty() || path.len() > max_path_len as usize {
        return false;
    }
    if !text_utils::is_terminal_safe(path) {
        return false;
    }
    if !is_representable(path) {
        return false;
    }
    if kind == CandidateKind::Directory && path.last() == Some(&b'/') {
        return false;
    }
    true
}

fn candidate_indices(candidates: &[Candidate<'_>]) -> Vec<usize> {
    let mut indices: Vec<usize> = Vec::new();
    let mut seen: HashMap<Vec<u8>, u8> = HashMap::new();
    for (index, candidate) in candidates.iter().enumerate() {
        if !accepted_candidate(candidate.path, candidate.kind) {
            continue;
        }
        if indices.len() >= max_indexed_files {
            break;
        }
        let kind_mask: u8 = match candidate.kind {
            CandidateKind::File => 0b01,
            CandidateKind::Directory => 0b10,
        };
        let entry = seen.entry(candidate.path.to_vec()).or_insert(0);
        if (*entry & kind_mask) != 0 {
            continue;
        }
        *entry |= kind_mask;
        indices.push(index);
    }
    indices
}

fn candidate_kind_matches_stat(expected: CandidateKind, meta: &std::fs::Metadata) -> bool {
    let ft = meta.file_type();
    match expected {
        CandidateKind::File => ft.is_file() || ft.is_symlink(),
        CandidateKind::Directory => ft.is_dir(),
    }
}

fn discover_scope_candidates(
    roots: &[Vec<u8>],
    stop_requested: &AtomicBool,
) -> Result<Vec<OwnedCandidate>, ()> {
    discover_scope_candidates_with_fallback(roots, stop_requested, false)
}

fn discover_scope_candidates_with_fallback(
    roots: &[Vec<u8>],
    stop_requested: &AtomicBool,
    force_fallback: bool,
) -> Result<Vec<OwnedCandidate>, ()> {
    if stop_requested.load(Ordering::SeqCst) {
        return Err(());
    }

    let mut candidates: Vec<OwnedCandidate> = Vec::new();
    let mut seen: HashSet<Vec<u8>> = HashSet::new();
    let mut kept: usize = 0;
    let mut succeeded: usize = 0;

    for (root_index, root_path) in roots.iter().enumerate() {
        if stop_requested.load(Ordering::SeqCst) {
            return Err(());
        }
        if kept >= max_indexed_files {
            break;
        }
        let remaining = max_indexed_files - kept;

        let file_options = workspace_files::Options {
            candidate_cap: remaining,
            force_fallback,
            include_hidden: true,
            include_untracked: true,
            sort_paths: true,
            git_worktree_is_authoritative: true,
            ..workspace_files::Options::default()
        };
        let Ok(discovered) =
            workspace_files::discover_cancellable(root_path, &file_options, stop_requested)
        else {
            continue;
        };
        succeeded += 1;

        let directory_options = workspace_files::Options {
            candidate_cap: remaining,
            force_fallback,
            include_hidden: true,
            sort_paths: true,
            git_worktree_is_authoritative: true,
            ..workspace_files::Options::default()
        };
        let directories =
            match workspace_files::discover_directories_cancellable(
                root_path,
                &directory_options,
                stop_requested,
            ) {
                Ok(result) => result.directories,
                Err(_) => Vec::new(),
            };

        if !is_directory(root_path) {
            succeeded -= 1;
            continue;
        }

        let mut file_cursor: usize = 0;
        let mut directory_cursor: usize = 0;
        while let Some(candidate) = next_sorted_discovered_candidate(
            &discovered.files,
            &directories,
            &mut file_cursor,
            &mut directory_cursor,
        ) {
            if stop_requested.load(Ordering::SeqCst) {
                return Err(());
            }
            if kept >= max_indexed_files {
                break;
            }
            if workspace_files::path_contains_ignored_dir(&[b".git".as_slice()], &candidate.path) {
                continue;
            }
            if !text_utils::is_terminal_safe(&candidate.path) {
                continue;
            }
            if candidate.kind == CandidateKind::File {
                let absolute = join_bytes(root_path, &candidate.path);
                let Ok(meta) = fs_symlink_metadata(&absolute) else {
                    continue;
                };
                let ft = meta.file_type();
                if !ft.is_file() && !ft.is_symlink() {
                    continue;
                }
            }
            if append_discovered_candidate(
                &mut candidates,
                &mut seen,
                root_path,
                root_index,
                &candidate.path,
                candidate.kind,
            ) {
                kept += 1;
            }
        }
    }

    if succeeded == 0 && !roots.is_empty() {
        return Err(());
    }
    Ok(candidates)
}

fn next_sorted_discovered_candidate(
    files: &[Vec<u8>],
    directories: &[Vec<u8>],
    file_cursor: &mut usize,
    directory_cursor: &mut usize,
) -> Option<OwnedCandidate> {
    if *file_cursor >= files.len() && *directory_cursor >= directories.len() {
        return None;
    }
    if *directory_cursor >= directories.len()
        || (*file_cursor < files.len()
            && files[*file_cursor].as_slice() <= directories[*directory_cursor].as_slice())
    {
        let path = files[*file_cursor].clone();
        *file_cursor += 1;
        Some(OwnedCandidate {
            path,
            kind: CandidateKind::File,
        })
    } else {
        let path = directories[*directory_cursor].clone();
        *directory_cursor += 1;
        Some(OwnedCandidate {
            path,
            kind: CandidateKind::Directory,
        })
    }
}

fn append_discovered_candidate(
    candidates: &mut Vec<OwnedCandidate>,
    seen: &mut HashSet<Vec<u8>>,
    root_path: &[u8],
    root_index: usize,
    relative: &[u8],
    kind: CandidateKind,
) -> bool {
    let absolute = lexical_join(root_path, relative);
    if !pathing::path_inside(root_path, &absolute) {
        return false;
    }
    let display: Vec<u8> = if root_index == 0 {
        relative.to_vec()
    } else {
        absolute.clone()
    };
    if !accepted_candidate(&display, kind) {
        return false;
    }
    if !seen.insert(absolute) {
        return false;
    }
    candidates.push(OwnedCandidate {
        path: display,
        kind,
    });
    true
}

fn is_directory(path: &[u8]) -> bool {
    fs_metadata(path).is_ok_and(|meta| meta.is_dir())
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
    !needs_quotes(path) || !path.contains(&b'"')
}

fn needs_quotes(path: &[u8]) -> bool {
    path.contains(&b' ')
}

fn ascii_to_lower(b: u8) -> u8 {
    if (b'A'..=b'Z').contains(&b) {
        b + 32
    } else {
        b
    }
}

fn alpha_mask(bytes: &[u8]) -> u32 {
    let mut mask: u32 = 0;
    for &b in bytes {
        let lower = ascii_to_lower(b);
        if (b'a'..=b'z').contains(&lower) {
            mask |= 1u32 << (lower - b'a');
        }
    }
    mask
}

fn fold_utf8(bytes: &[u8]) -> Option<Vec<u32>> {
    let text = std::str::from_utf8(bytes).ok()?;
    let mut out = Vec::new();
    for ch in text.chars() {
        out.push(unicode_simple_fold::fold(ch as u32));
    }
    Some(out)
}

struct PreparedQuery {
    folded: Vec<u32>,
    ascii: Option<Vec<u8>>,
}

fn prepare_query(query: &[u8]) -> Option<PreparedQuery> {
    let folded = fold_utf8(query)?;
    let mut ascii: Option<Vec<u8>> = Some(Vec::with_capacity(folded.len()));
    for &scalar in &folded {
        if scalar > 0x7f {
            ascii = None;
            break;
        }
        ascii.as_mut().unwrap().push(scalar as u8);
    }
    Some(PreparedQuery { folded, ascii })
}

#[derive(Clone, Copy, Debug)]
struct MatchScore {
    exact_fit: bool,
    basename_fit: bool,
    boundary_matches: usize,
    prefix: bool,
    first_position: usize,
    longest_run: usize,
    consecutive_matches: usize,
    gaps: usize,
}

#[derive(Clone, Copy, Debug, Default)]
struct SubsequenceFacts {
    boundary_matches: usize,
    prefix: bool,
    first_position: usize,
    longest_run: usize,
    consecutive_matches: usize,
    gaps: usize,
}

#[derive(Clone, Copy, Debug)]
struct RankedCandidate {
    score: MatchScore,
    index: usize,
}

fn rank_top_n(generation: &Generation, total: usize, query: &PreparedQuery) -> Vec<usize> {
    let query_ascii = query.ascii.as_ref();
    let query_mask = query_ascii.map_or(0, |ascii| alpha_mask(ascii));

    let mut ranked: Vec<RankedCandidate> = Vec::new();
    for candidate_index in 0..total {
        let path_mask = generation.char_masks[candidate_index];
        let base_start = generation.basename_starts[candidate_index];
        let score = if (path_mask & NON_ASCII_MASK) == 0 {
            let Some(ascii) = query_ascii else {
                continue;
            };
            if (path_mask & query_mask) != query_mask {
                continue;
            }
            score_ascii_match(
                &generation.paths[candidate_index],
                &generation.lower[candidate_index],
                base_start,
                ascii,
            )
        } else {
            score_folded_match(&generation.paths[candidate_index], base_start, &query.folded)
        };
        let Some(score) = score else { continue };
        ranked.push(RankedCandidate {
            score,
            index: candidate_index,
        });
    }

    ranked.sort_by(|a, b| {
        if ranked_candidate_better(generation, *a, *b) {
            std::cmp::Ordering::Less
        } else {
            std::cmp::Ordering::Greater
        }
    });
    ranked.truncate(MAX_SEARCH_RESULTS);
    ranked.into_iter().map(|candidate| candidate.index).collect()
}

fn score_ascii_match(
    path: &[u8],
    path_lower: &[u8],
    basename_start: usize,
    query: &[u8],
) -> Option<MatchScore> {
    if query.is_empty() || path.is_empty() {
        return None;
    }
    let base_start = basename_start;
    let basename = &path_lower[base_start..];
    let exact_fit = path_lower == query || basename == query;

    if let Some(facts) = score_ascii_range(path, path_lower, base_start, query) {
        return Some(match_score(exact_fit, true, facts));
    }
    let facts = score_ascii_range(path, path_lower, 0, query)?;
    Some(match_score(exact_fit, false, facts))
}

fn score_ascii_range(
    path: &[u8],
    path_lower: &[u8],
    range_start: usize,
    query: &[u8],
) -> Option<SubsequenceFacts> {
    if query.len() > path_lower.len() - range_start {
        return None;
    }

    let mut facts = SubsequenceFacts::default();
    let mut query_index = 0usize;
    let mut previous_position = 0usize;
    let mut run_length = 0usize;
    let mut position = range_start;
    while position < path_lower.len() && query_index < query.len() {
        if path_lower[position] != query[query_index] {
            position += 1;
            continue;
        }
        if query_index == 0 {
            facts.first_position = position - range_start;
            facts.prefix = position == range_start;
            run_length = 1;
        } else if position == previous_position + 1 {
            facts.consecutive_matches += 1;
            run_length += 1;
        } else {
            facts.gaps += position - previous_position - 1;
            run_length = 1;
        }
        facts.longest_run = facts.longest_run.max(run_length);
        facts.boundary_matches += usize::from(is_match_boundary(path, position));
        previous_position = position;
        query_index += 1;
        position += 1;
    }
    if query_index == query.len() {
        Some(facts)
    } else {
        None
    }
}

fn score_folded_match(
    path: &[u8],
    basename_byte_start: usize,
    query: &[u32],
) -> Option<MatchScore> {
    if query.is_empty() || path.is_empty() {
        return None;
    }
    let decoded = decode_folded_path(path, basename_byte_start)?;
    let scalars = &decoded.scalars;
    let basename_scalars = &scalars[decoded.basename_scalar_start..];
    let exact_fit = scalars == query || basename_scalars == query;

    if let Some(facts) = score_folded_range(
        path,
        scalars,
        &decoded.byte_offsets,
        decoded.basename_scalar_start,
        query,
    ) {
        return Some(match_score(exact_fit, true, facts));
    }
    let facts = score_folded_range(path, scalars, &decoded.byte_offsets, 0, query)?;
    Some(match_score(exact_fit, false, facts))
}

fn score_folded_range(
    path: &[u8],
    scalars: &[u32],
    byte_offsets: &[u16],
    range_start: usize,
    query: &[u32],
) -> Option<SubsequenceFacts> {
    if query.len() > scalars.len() - range_start {
        return None;
    }

    let mut facts = SubsequenceFacts::default();
    let mut query_index = 0usize;
    let mut previous_position = 0usize;
    let mut run_length = 0usize;
    let mut position = range_start;
    while position < scalars.len() && query_index < query.len() {
        if scalars[position] != query[query_index] {
            position += 1;
            continue;
        }
        if query_index == 0 {
            facts.first_position = position - range_start;
            facts.prefix = position == range_start;
            run_length = 1;
        } else if position == previous_position + 1 {
            facts.consecutive_matches += 1;
            run_length += 1;
        } else {
            facts.gaps += position - previous_position - 1;
            run_length = 1;
        }
        facts.longest_run = facts.longest_run.max(run_length);
        facts.boundary_matches += usize::from(is_match_boundary(path, byte_offsets[position] as usize));
        previous_position = position;
        query_index += 1;
        position += 1;
    }
    if query_index == query.len() {
        Some(facts)
    } else {
        None
    }
}

fn match_score(exact_fit: bool, basename_fit: bool, facts: SubsequenceFacts) -> MatchScore {
    MatchScore {
        exact_fit,
        basename_fit,
        boundary_matches: facts.boundary_matches,
        prefix: facts.prefix,
        first_position: facts.first_position,
        longest_run: facts.longest_run,
        consecutive_matches: facts.consecutive_matches,
        gaps: facts.gaps,
    }
}

fn is_match_boundary(path: &[u8], byte_index: usize) -> bool {
    if byte_index == 0 {
        return true;
    }
    let previous = path[byte_index - 1];
    if previous == b'/'
        || previous == b'-'
        || previous == b'_'
        || previous == b'.'
        || previous == b' '
    {
        return true;
    }
    if byte_index >= path.len() {
        return false;
    }
    let current = path[byte_index];
    (b'a'..=b'z').contains(&previous) && (b'A'..=b'Z').contains(&current)
}

fn score_better(left: MatchScore, right: MatchScore) -> bool {
    if left.exact_fit != right.exact_fit {
        return left.exact_fit;
    }
    if left.basename_fit != right.basename_fit {
        return left.basename_fit;
    }
    if left.boundary_matches != right.boundary_matches {
        return left.boundary_matches > right.boundary_matches;
    }
    if left.prefix != right.prefix {
        return left.prefix;
    }
    if left.first_position != right.first_position {
        return left.first_position < right.first_position;
    }
    if left.longest_run != right.longest_run {
        return left.longest_run > right.longest_run;
    }
    if left.consecutive_matches != right.consecutive_matches {
        return left.consecutive_matches > right.consecutive_matches;
    }
    if left.gaps != right.gaps {
        return left.gaps < right.gaps;
    }
    false
}

fn ranked_candidate_better(
    generation: &Generation,
    left: RankedCandidate,
    right: RankedCandidate,
) -> bool {
    if score_better(left.score, right.score) {
        return true;
    }
    if score_better(right.score, left.score) {
        return false;
    }
    let left_path = &generation.paths[left.index];
    let right_path = &generation.paths[right.index];
    if left_path.len() != right_path.len() {
        return left_path.len() < right_path.len();
    }
    left_path < right_path
}

struct DecodedFoldedPath {
    scalars: Vec<u32>,
    byte_offsets: Vec<u16>,
    basename_scalar_start: usize,
}

fn decode_folded_path(path: &[u8], basename_byte_start: usize) -> Option<DecodedFoldedPath> {
    let text = std::str::from_utf8(path).ok()?;
    let mut scalars = Vec::new();
    let mut byte_offsets = Vec::new();
    let mut basename_scalar_start: Option<usize> = None;
    for (byte_index, ch) in text.char_indices() {
        if byte_index == basename_byte_start {
            basename_scalar_start = Some(scalars.len());
        }
        byte_offsets.push(byte_index.min(u16::MAX as usize) as u16);
        scalars.push(unicode_simple_fold::fold(ch as u32));
    }
    if text.len() == basename_byte_start {
        basename_scalar_start = Some(scalars.len());
    }
    byte_offsets.push(text.len().min(u16::MAX as usize) as u16);
    Some(DecodedFoldedPath {
        scalars,
        byte_offsets,
        basename_scalar_start: basename_scalar_start?,
    })
}

fn reconstruct_match_spans(
    path: &[u8],
    basename_byte_start: usize,
    query: &[u32],
) -> Option<Vec<MatchSpan>> {
    let decoded = decode_folded_path(path, basename_byte_start)?;
    let matched_offsets = collect_match_offsets(
        &decoded.scalars,
        &decoded.byte_offsets,
        decoded.basename_scalar_start,
        query,
    )
    .or_else(|| collect_match_offsets(&decoded.scalars, &decoded.byte_offsets, 0, query))?;
    spans_from_matched_offsets(path, &matched_offsets)
}

fn collect_match_offsets(
    scalars: &[u32],
    byte_offsets: &[u16],
    range_start: usize,
    query: &[u32],
) -> Option<Vec<u16>> {
    if query.len() > scalars.len() - range_start {
        return None;
    }
    let mut offsets = Vec::with_capacity(query.len());
    let mut query_index = 0usize;
    let mut position = range_start;
    while position < scalars.len() && query_index < query.len() {
        if scalars[position] != query[query_index] {
            position += 1;
            continue;
        }
        offsets.push(byte_offsets[position]);
        query_index += 1;
        position += 1;
    }
    if query_index == query.len() {
        Some(offsets)
    } else {
        None
    }
}

fn spans_from_matched_offsets(path: &[u8], matched_offsets: &[u16]) -> Option<Vec<MatchSpan>> {
    let mut spans: Vec<MatchSpan> = Vec::new();
    let mut matched_index: usize = 0;
    let mut cursor: usize = 0;
    while cursor < path.len() && matched_index < matched_offsets.len() {
        let cluster_start = cursor;
        let first = display_width::display_unit_at(path, cursor);
        if first.byte_len == 0 {
            return None;
        }
        cursor += first.byte_len;
        while cursor < path.len() {
            let continuation = display_width::display_unit_at(path, cursor);
            if continuation.byte_len == 0 {
                return None;
            }
            if continuation.cell_width != 0 {
                break;
            }
            cursor += continuation.byte_len;
        }
        let cluster_end = cursor;

        let mut cluster_matched = false;
        while matched_index < matched_offsets.len()
            && (matched_offsets[matched_index] as usize) < cluster_end
        {
            if (matched_offsets[matched_index] as usize) < cluster_start {
                return None;
            }
            cluster_matched = true;
            matched_index += 1;
        }
        if !cluster_matched {
            continue;
        }

        if let Some(last) = spans.last_mut() {
            if last.byte_end as usize == cluster_start {
                last.byte_end = cluster_end.min(u16::MAX as usize) as u16;
                continue;
            }
        }
        spans.push(MatchSpan {
            byte_start: cluster_start.min(u16::MAX as usize) as u16,
            byte_end: cluster_end.min(u16::MAX as usize) as u16,
        });
    }
    if matched_index != matched_offsets.len() {
        return None;
    }
    Some(spans)
}

fn is_absolute(path: &[u8]) -> bool {
    path.first() == Some(&b'/')
}

fn lexical_join(base: &[u8], relative: &[u8]) -> Vec<u8> {
    let mut out: Vec<&[u8]> = Vec::new();
    for component in base.split(|&b| b == b'/') {
        if !component.is_empty() {
            out.push(component);
        }
    }
    for component in relative.split(|&b| b == b'/') {
        if component.is_empty() || component == b"." {
            continue;
        }
        if component == b".." {
            out.pop();
            continue;
        }
        out.push(component);
    }
    let mut result = Vec::new();
    result.push(b'/');
    for (index, component) in out.iter().enumerate() {
        if index > 0 {
            result.push(b'/');
        }
        result.extend_from_slice(component);
    }
    result
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

#[cfg(unix)]
fn path_from_bytes(bytes: &[u8]) -> Option<&std::path::Path> {
    use std::os::unix::ffi::OsStrExt;
    Some(std::path::Path::new(std::ffi::OsStr::from_bytes(bytes)))
}

#[cfg(not(unix))]
fn path_from_bytes(bytes: &[u8]) -> Option<&std::path::Path> {
    std::str::from_utf8(bytes).ok().map(std::path::Path::new)
}

#[cfg(unix)]
fn bytes_from_path(path: &std::path::Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes().to_vec()
}

#[cfg(not(unix))]
fn bytes_from_path(path: &std::path::Path) -> Vec<u8> {
    path.to_string_lossy().as_bytes().to_vec()
}

fn fs_symlink_metadata(path: &[u8]) -> std::io::Result<std::fs::Metadata> {
    std::fs::symlink_metadata(path_from_bytes(path).ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "non-utf8 path")
    })?)
}

fn fs_metadata(path: &[u8]) -> std::io::Result<std::fs::Metadata> {
    std::fs::metadata(path_from_bytes(path).ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "non-utf8 path")
    })?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_root(name: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("nyq-file-index-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn write_test_file(root: &std::path::Path, rel: &str, content: &[u8]) {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    fn realpath(root: &std::path::Path, rel: &str) -> Vec<u8> {
        std::fs::canonicalize(root.join(rel))
            .unwrap()
            .to_string_lossy()
            .as_bytes()
            .to_vec()
    }

    fn expect_valid_search_spans(result: &SearchResult) {
        let mut previous_end = 0usize;
        for span in &result.matched_spans {
            assert!(span.byte_start < span.byte_end);
            assert!(span.byte_start as usize >= previous_end);
            assert!((span.byte_end as usize) <= result.path.len());
            assert!(std::str::from_utf8(
                &result.path[span.byte_start as usize..span.byte_end as usize]
            )
            .is_ok());
            previous_end = span.byte_end as usize;
        }
    }

    fn first_search_path(raw: &[u8], query: &[u8]) -> Option<Vec<u8>> {
        let mut index = FileIndex::default();
        index.build_from_raw(raw).unwrap();
        let results = index.search_typed(query, 8).unwrap();
        results.first().map(|r| r.path.clone())
    }

    fn contains_candidate(candidates: &[OwnedCandidate], path: &[u8], kind: CandidateKind) -> bool {
        candidates
            .iter()
            .any(|candidate| candidate.kind == kind && candidate.path == path)
    }

    #[test]
    fn build_from_raw_parses_newline_separated_list() {
        let mut index = FileIndex::default();
        index
            .build_from_raw(b"src/main.zig\nsrc/core/shared/io.zig\nREADME.md\n")
            .unwrap();
        assert_eq!(3, index.count());
        let lock = index.active_generation.as_ref().unwrap().lock().unwrap();
        assert_eq!(b"src/main.zig", lock.paths[0].as_slice());
        assert_eq!(b"src/core/shared/io.zig", lock.paths[1].as_slice());
        assert_eq!(b"README.md", lock.paths[2].as_slice());
    }

    #[test]
    fn build_from_raw_parses_nul_separated_list_from_git_ls_files_z() {
        let mut index = FileIndex::default();
        index.build_from_raw(b"a/b.zig\x00CAMEL.md\x00").unwrap();
        assert_eq!(2, index.count());
        let lock = index.active_generation.as_ref().unwrap().lock().unwrap();
        assert_eq!(b"a/b.zig", lock.paths[0].as_slice());
        assert_eq!(b"CAMEL.md", lock.paths[1].as_slice());
        assert_eq!(b"camel.md", lock.lower[1].as_slice());
    }

    #[test]
    fn build_from_raw_precomputes_basename_starts() {
        let mut index = FileIndex::default();
        index.build_from_raw(b"a/b/c.txt\nroot.txt\n").unwrap();
        let lock = index.active_generation.as_ref().unwrap().lock().unwrap();
        assert_eq!(b"c.txt", &lock.paths[0][lock.basename_starts[0]..]);
        assert_eq!(b"root.txt", &lock.paths[1][lock.basename_starts[1]..]);
    }

    #[test]
    fn build_from_raw_precomputes_a_z_char_masks() {
        let mut index = FileIndex::default();
        index.build_from_raw(b"src/Main.zig\nREADME.md\n").unwrap();
        let lock = index.active_generation.as_ref().unwrap().lock().unwrap();
        assert_eq!(alpha_mask(b"src/main.zig"), lock.char_masks[0]);
        assert_eq!(alpha_mask(b"readme.md"), lock.char_masks[1]);
    }

    #[test]
    fn build_from_candidates_stores_one_mixed_typed_inventory_in_input_order() {
        let candidates = [
            Candidate { path: b"src", kind: CandidateKind::Directory },
            Candidate { path: b"src", kind: CandidateKind::Directory },
            Candidate { path: b"src", kind: CandidateKind::File },
            Candidate { path: b"src/main.zig", kind: CandidateKind::File },
            Candidate { path: b"docs", kind: CandidateKind::Directory },
        ];
        let mut index = FileIndex::default();
        index.build_from_candidates(&candidates).unwrap();
        assert_eq!(4, index.count());
        let lock = index.active_generation.as_ref().unwrap().lock().unwrap();
        assert_eq!(b"src", lock.paths[0].as_slice());
        assert_eq!(CandidateKind::Directory, lock.kinds[0]);
        assert_eq!(b"src", lock.paths[1].as_slice());
        assert_eq!(CandidateKind::File, lock.kinds[1]);
        assert_eq!(b"src/main.zig", lock.paths[2].as_slice());
        assert_eq!(CandidateKind::File, lock.kinds[2]);
        assert_eq!(b"docs", lock.paths[3].as_slice());
        assert_eq!(CandidateKind::Directory, lock.kinds[3]);
    }

    #[test]
    fn build_from_candidates_handles_empty_and_rejected_typed_candidates() {
        let mut empty = FileIndex::default();
        empty.build_from_candidates(&[]).unwrap();
        assert_eq!(0, empty.count());

        let long_path = b"a".repeat(max_path_len as usize + 1);
        let candidates = [
            Candidate { path: b"src/", kind: CandidateKind::Directory },
            Candidate { path: b"escape-\x1b[2J", kind: CandidateKind::Directory },
            Candidate { path: b"space \" dir", kind: CandidateKind::Directory },
            Candidate { path: &long_path, kind: CandidateKind::File },
            Candidate { path: b"src", kind: CandidateKind::Directory },
            Candidate { path: b"README.md", kind: CandidateKind::File },
        ];
        let mut filtered = FileIndex::default();
        filtered.build_from_candidates(&candidates).unwrap();
        assert_eq!(2, filtered.count());
        let lock = filtered.active_generation.as_ref().unwrap().lock().unwrap();
        assert_eq!(b"src", lock.paths[0].as_slice());
        assert_eq!(CandidateKind::Directory, lock.kinds[0]);
        assert_eq!(b"README.md", lock.paths[1].as_slice());
        assert_eq!(CandidateKind::File, lock.kinds[1]);
    }

    #[test]
    fn subsequence_scoring_prefers_basename_prefix_over_mid_path_match() {
        let a = score_ascii_match(b"src/main.zig", b"src/main.zig", 4, b"main").unwrap();
        let b = score_ascii_match(b"src/wasm_term_main.zig", b"src/wasm_term_main.zig", 4, b"main")
            .unwrap();
        assert!(score_better(a, b));
    }

    #[test]
    fn subsequence_scoring_prefers_earlier_match_position() {
        let early = score_ascii_match(b"src/main.zig", b"src/main.zig", 4, b"main").unwrap();
        let late = score_ascii_match(b"foo/bar/baz-main.zig", b"foo/bar/baz-main.zig", 8, b"main")
            .unwrap();
        assert!(score_better(early, late));
    }

    #[test]
    fn subsequence_scoring_returns_no_match_when_query_is_absent() {
        assert!(score_ascii_match(b"src/main.zig", b"src/main.zig", 4, b"xyz").is_none());
    }

    #[test]
    fn subsequence_scoring_accepts_longer_query_than_basename_via_full_path() {
        assert!(score_ascii_match(b"src/main.zig", b"src/main.zig", 4, b"src/main").is_some());
    }

    #[test]
    fn search_returns_best_matches_first() {
        let raw = b"src/wasm_term_main.zig\nsrc/main.zig\nREADME.md\nsrc/core/shared/io.zig\nbenchmarks/startup.sh\n";
        let results = {
            let mut index = FileIndex::default();
            index.build_from_raw(raw).unwrap();
            index.search_typed(b"main", 8).unwrap()
        };
        assert!(results.len() >= 2);
        assert_eq!(b"src/main.zig", results[0].path.as_slice());
        assert_eq!(b"src/wasm_term_main.zig", results[1].path.as_slice());
    }

    #[test]
    fn typed_search_supports_abbreviated_subsequences() {
        let candidates = [
            Candidate { path: b"src/core/workspace", kind: CandidateKind::Directory },
            Candidate { path: b"src/core/workspace/file_index.zig", kind: CandidateKind::File },
            Candidate { path: b"src/core/workspace/workspace_files.zig", kind: CandidateKind::File },
        ];
        let mut index = FileIndex::default();
        index.build_from_candidates(&candidates).unwrap();
        let results = index.search_typed(b"FIIDX", 4).unwrap();
        assert_eq!(1, results.len());
        assert_eq!(b"src/core/workspace/file_index.zig", results[0].path.as_slice());
        assert_eq!(CandidateKind::File, results[0].kind);
        expect_valid_search_spans(&results[0]);

        let mut matched_bytes = Vec::new();
        for span in &results[0].matched_spans {
            matched_bytes.extend_from_slice(
                &results[0].path[span.byte_start as usize..span.byte_end as usize],
            );
        }
        assert_eq!(b"fiidx", matched_bytes.as_slice());
    }

    #[test]
    fn ranking_signals_follow_the_accepted_descending_influence() {
        let cases: &[(&[u8], &[u8], &[u8])] = &[
            (b"other/src/core/main.zig\nsrc/core/main.zig\n", b"src/core/main.zig", b"src/core/main.zig"),
            (b"src/main.zig.backup\nsrc/main.zig\n", b"main.zig", b"src/main.zig"),
            (b"core/guide.txt\ndocs/core-guide.txt\n", b"core", b"docs/core-guide.txt"),
            (b"misc/scattered/memo.zig\nsrc/core/main.zig\n", b"scm", b"src/core/main.zig"),
            (b"src/xwidget.zig\nsrc/widgetx.zig\n", b"wid", b"src/widgetx.zig"),
            (b"x/a/b\na/x/b\n", b"ab", b"a/x/b"),
            (b"src/xxidxx.zig\nsrc/xidxxx.zig\n", b"id", b"src/xidxxx.zig"),
            (b"src/axbyc.zig\nsrc/abczz.zig\n", b"abc", b"src/abczz.zig"),
            (b"src/axxxbxc.zig\nsrc/axbxczz.zig\n", b"abc", b"src/axbxczz.zig"),
            (b"src/abc-long-name.txt\nsrc/abc.txt\n", b"abc", b"src/abc.txt"),
        ];
        for (raw, query, expected) in cases {
            assert_eq!(*expected, first_search_path(raw, query).unwrap().as_slice());
        }
    }

    #[test]
    fn ranking_is_independent_of_discovery_order_and_resolves_ties_bytewise() {
        assert_eq!(
            b"src/beta.txt".as_slice(),
            first_search_path(b"src/zeta.txt\nsrc/beta.txt\n", b"ta").unwrap().as_slice()
        );
        assert_eq!(
            b"src/beta.txt".as_slice(),
            first_search_path(b"src/beta.txt\nsrc/zeta.txt\n", b"ta").unwrap().as_slice()
        );
    }

    #[test]
    fn file_and_directory_candidates_have_no_ranking_priority() {
        let first_kinds = [
            Candidate { path: b"src/zeta.txt", kind: CandidateKind::File },
            Candidate { path: b"src/beta.txt", kind: CandidateKind::Directory },
        ];
        let reversed_kinds = [
            Candidate { path: b"src/zeta.txt", kind: CandidateKind::Directory },
            Candidate { path: b"src/beta.txt", kind: CandidateKind::File },
        ];
        for candidates in [&first_kinds[..], &reversed_kinds[..]] {
            let mut index = FileIndex::default();
            index.build_from_candidates(candidates).unwrap();
            let results = index.search_typed(b"ta", 2).unwrap();
            assert_eq!(2, results.len());
            assert_eq!(b"src/beta.txt", results[0].path.as_slice());
            assert_eq!(b"src/zeta.txt", results[1].path.as_slice());
        }
    }

    #[test]
    fn typed_empty_search_preserves_mixed_inventory_order_without_spans() {
        let candidates = [
            Candidate { path: b"src", kind: CandidateKind::Directory },
            Candidate { path: b"src/main.zig", kind: CandidateKind::File },
            Candidate { path: b"docs", kind: CandidateKind::Directory },
        ];
        let mut index = FileIndex::default();
        index.build_from_candidates(&candidates).unwrap();
        let results = index.search_typed(b"", 2).unwrap();
        assert_eq!(2, results.len());
        assert_eq!(b"src", results[0].path.as_slice());
        assert_eq!(CandidateKind::Directory, results[0].kind);
        assert_eq!(0, results[0].matched_spans.len());
        assert_eq!(b"src/main.zig", results[1].path.as_slice());
        assert_eq!(CandidateKind::File, results[1].kind);
    }

    #[test]
    fn punctuation_remains_matchable_under_case_insensitive_subsequence_search() {
        let mut index = FileIndex::default();
        index.build_from_raw(b"src/API-route.ts\nsrc/api_handler.ts\n").unwrap();
        let results = index.search_typed(b"api-rt", 4).unwrap();
        assert_eq!(1, results.len());
        assert_eq!(b"src/API-route.ts", results[0].path.as_slice());
        expect_valid_search_spans(&results[0]);
    }

    #[test]
    fn search_is_case_insensitive() {
        let mut index = FileIndex::default();
        index.build_from_raw(b"README.md\nsrc/Main.zig\n").unwrap();
        let readme = index.search_typed(b"README", 4).unwrap();
        assert!(readme.len() >= 1);
        assert_eq!(b"README.md", readme[0].path.as_slice());
        let lowercase = index.search_typed(b"readme", 4).unwrap();
        assert!(lowercase.len() >= 1);
        assert_eq!(b"README.md", lowercase[0].path.as_slice());
        let main = index.search_typed(b"main", 4).unwrap();
        assert!(main.len() >= 1);
        assert_eq!(b"src/Main.zig", main[0].path.as_slice());
    }

    #[test]
    fn search_uses_unicode_17_simple_folding_and_preserves_raw_spelling() {
        let mut index = FileIndex::default();
        index
            .build_from_raw("docs/Ärger-file.txt\x00docs/Σigma.txt\x00docs/ςigma-final.txt\x00docs/Kelvin.txt\x00docs/kettle.txt\x00".as_bytes())
            .unwrap();

        let arger = index.search_typed("ärger".as_bytes(), 8).unwrap();
        assert_eq!(1, arger.len());
        assert_eq!("docs/Ärger-file.txt".as_bytes(), arger[0].path.as_slice());

        let sigma = index.search_typed("σigma".as_bytes(), 8).unwrap();
        assert_eq!(2, sigma.len());
        assert_eq!("docs/Σigma.txt".as_bytes(), sigma[0].path.as_slice());

        let kelvin = index.search_typed(b"kelvin", 8).unwrap();
        assert!(kelvin.len() >= 1);
        assert_eq!("docs/Kelvin.txt".as_bytes(), kelvin[0].path.as_slice());

        let ascii_k = index.search_typed(b"k", 8).unwrap();
        assert!(ascii_k.len() >= 2);
        assert!(ascii_k.iter().any(|r| r.path == "docs/Kelvin.txt".as_bytes()));

        let kelvin_query = index.search_typed("K".as_bytes(), 8).unwrap();
        assert!(kelvin_query.len() >= 2);
        assert!(kelvin_query.iter().any(|r| r.path == b"docs/kettle.txt"));
    }

    #[test]
    fn unicode_search_keeps_normalization_and_full_folding_out_of_scope() {
        let mut index = FileIndex::default();
        index.build_from_raw("Ärger.txt\x00Fuß.txt\x00".as_bytes()).unwrap();
        assert_eq!(0, index.search_typed("A\u{0308}rger".as_bytes(), 4).unwrap().len());
        assert_eq!(0, index.search_typed(b"Fuss", 4).unwrap().len());
    }

    #[test]
    fn unicode_search_rejects_invalid_and_truncated_utf8_queries() {
        let mut index = FileIndex::default();
        index.build_from_raw("Ärger.txt\x00".as_bytes()).unwrap();
        assert_eq!(0, index.search_typed(b"\xff", 4).unwrap().len());
        assert_eq!(0, index.search_typed(b"\xc3", 4).unwrap().len());
    }

    #[test]
    fn typed_unicode_subsequences_preserve_simple_fold_spelling_and_scalar_spans() {
        let mut index = FileIndex::default();
        index
            .build_from_raw("docs/Ärger-Kelvin.txt\x00docs/kettle.txt\x00".as_bytes())
            .unwrap();
        let results = index.search_typed("ärgk".as_bytes(), 4).unwrap();
        assert_eq!(1, results.len());
        assert_eq!("docs/Ärger-Kelvin.txt".as_bytes(), results[0].path.as_slice());
        expect_valid_search_spans(&results[0]);

        let kelvin = index.search_typed(b"kelvin", 4).unwrap();
        assert!(kelvin.len() >= 1);
        assert_eq!("docs/Ärger-Kelvin.txt".as_bytes(), kelvin[0].path.as_slice());
        assert!(kelvin[0]
            .path
            .windows("Kelvin".as_bytes().len())
            .any(|w| w == "Kelvin".as_bytes()));
    }

    #[test]
    fn typed_spans_keep_combining_display_sequences_intact() {
        let mut index = FileIndex::default();
        index
            .build_from_raw("docs/Cafe\u{0301}-note.txt\x00".as_bytes())
            .unwrap();
        let results = index.search_typed("e\u{0301}n".as_bytes(), 2).unwrap();
        assert_eq!(1, results.len());
        assert_eq!("docs/Cafe\u{0301}-note.txt".as_bytes(), results[0].path.as_slice());
        expect_valid_search_spans(&results[0]);
        assert!(results[0].matched_spans.len() >= 1);
        let first = results[0].matched_spans[0];
        assert_eq!(
            "e\u{0301}".as_bytes(),
            &results[0].path[first.byte_start as usize..first.byte_end as usize]
        );

        let combining = index.search_typed("\u{0301}".as_bytes(), 2).unwrap();
        assert_eq!(1, combining.len());
        assert_eq!(1, combining[0].matched_spans.len());
        let span = combining[0].matched_spans[0];
        assert_eq!(
            "e\u{0301}".as_bytes(),
            &combining[0].path[span.byte_start as usize..span.byte_end as usize]
        );
    }

    #[test]
    fn typed_search_handles_repeated_query_scalars() {
        let mut index = FileIndex::default();
        index.build_from_raw(b"src/axaya.zig\x00src/alpha.zig\x00").unwrap();
        let results = index.search_typed(b"aaa", 4).unwrap();
        assert_eq!(1, results.len());
        assert_eq!(b"src/axaya.zig", results[0].path.as_slice());
        expect_valid_search_spans(&results[0]);
    }

    #[test]
    fn typed_invalid_utf8_and_no_match_queries_return_no_results() {
        let mut index = FileIndex::default();
        index.build_from_raw("docs/Ärger.txt\x00".as_bytes()).unwrap();
        assert_eq!(0, index.search_typed(b"\xff", 4).unwrap().len());
        assert_eq!(0, index.search_typed(b"\xc3", 4).unwrap().len());
        assert_eq!(0, index.search_typed(b"not-present", 4).unwrap().len());
    }

    #[test]
    fn search_returns_zero_when_index_is_not_ready() {
        let index = FileIndex::default();
        assert_eq!(0, index.search_typed(b"x", 4).unwrap().len());
    }

    #[test]
    fn search_returns_zero_when_the_index_has_failed() {
        let mut index = FileIndex::default();
        index.initial_failed = true;
        assert_eq!(0, index.search_typed(b"main", 4).unwrap().len());
    }

    #[test]
    fn search_returns_partial_results_while_ready_count_is_below_total() {
        let mut index = FileIndex::default();
        index
            .build_from_raw(b"src/wasm_term_main.zig\nsrc/main.zig\nREADME.md\nsrc/core/shared/io.zig\n")
            .unwrap();
        assert_eq!(4, index.count());

        let generation = index.active_generation.take().unwrap();
        {
            let mut lock = generation.lock().unwrap();
            lock.state = GenerationState::Loading;
            lock.ready_count = 2;
        }
        index.loading_generation = Some(generation);
        assert_eq!(2, index.count());

        let results = index.search_typed(b"main", 4).unwrap();
        assert_eq!(2, results.len());
        for result in &results {
            assert!(result.path.windows(4).any(|w| w == b"main"));
        }

        {
            let mut lock = index.loading_generation.as_ref().unwrap().lock().unwrap();
            lock.ready_count = 4;
        }
        assert_eq!(2, index.search_typed(b"main", 4).unwrap().len());

        let readme = index.search_typed(b"README", 4).unwrap();
        assert_eq!(1, readme.len());
        assert_eq!(b"README.md", readme[0].path.as_slice());
    }

    #[test]
    fn search_caps_ranked_results_at_64_even_with_larger_output() {
        let mut raw = Vec::new();
        for i in 0..80 {
            raw.extend_from_slice(format!("src/match-{i:0>3}.zig\n").as_bytes());
        }
        let mut index = FileIndex::default();
        index.build_from_raw(&raw).unwrap();
        let results = index.search_typed(b"match", 80).unwrap();
        assert_eq!(64, results.len());
        for (i, result) in results.iter().enumerate() {
            assert_eq!(
                format!("src/match-{i:0>3}.zig").as_bytes(),
                result.path.as_slice()
            );
            assert_eq!(CandidateKind::File, result.kind);
            expect_valid_search_spans(result);
        }
    }

    #[test]
    fn search_rejects_queries_longer_than_max_path_len() {
        let path = b"a".repeat(max_path_len as usize);
        let query = format!("{}b", "a".repeat(max_path_len as usize));
        let mut index = FileIndex::default();
        index.build_from_raw(&path).unwrap();
        assert_eq!(0, index.search_typed(query.as_bytes(), 1).unwrap().len());
    }

    #[test]
    fn build_from_raw_omits_unsafe_controls_and_preserves_neighboring_safe_paths() {
        let mut newline_index = FileIndex::default();
        newline_index
            .build_from_raw(b"safe-before\nnewline-path\r\nsafe-after\n")
            .unwrap();
        assert_eq!(3, newline_index.count());
        let lock = newline_index.active_generation.as_ref().unwrap().lock().unwrap();
        assert_eq!(b"newline-path", lock.paths[1].as_slice());

        let mut nul_index = FileIndex::default();
        nul_index
            .build_from_raw(b"safe.txt\x00escape-\x1b[2J.txt\x00cr-path\r\x00invalid-\xff\x00other.txt\x00")
            .unwrap();
        assert_eq!(2, nul_index.count());
        let lock = nul_index.active_generation.as_ref().unwrap().lock().unwrap();
        assert_eq!(b"safe.txt", lock.paths[0].as_slice());
        assert_eq!(b"other.txt", lock.paths[1].as_slice());
    }

    #[test]
    fn scope_discovery_emits_primary_relative_and_added_absolute_paths_in_root_order() {
        let root = temp_root("scope-discovery");
        fs_create_dir(root.join("primary"));
        fs_create_dir(root.join("shared/nested"));
        write_test_file(&root, "primary/main.zig", b"");
        write_test_file(&root, "shared/nested/lib.zig", b"");
        let primary = realpath(&root, "primary");
        let shared = realpath(&root, "shared");
        let shared_file = join_bytes(&shared, b"nested/lib.zig");
        let shared_directory = join_bytes(&shared, b"nested");
        let roots = vec![primary, shared];
        let stop_requested = AtomicBool::new(false);

        let candidates =
            discover_scope_candidates_with_fallback(&roots, &stop_requested, true).unwrap();

        let mut index = FileIndex::default();
        let borrowed: Vec<Candidate<'_>> = candidates
            .iter()
            .map(|c| Candidate { path: &c.path, kind: c.kind })
            .collect();
        index.build_from_candidates(&borrowed).unwrap();
        assert_eq!(3, index.count());
        let lock = index.active_generation.as_ref().unwrap().lock().unwrap();
        assert_eq!(b"main.zig", lock.paths[0].as_slice());
        assert_eq!(CandidateKind::File, lock.kinds[0]);
        assert_eq!(shared_directory, lock.paths[1]);
        assert_eq!(CandidateKind::Directory, lock.kinds[1]);
        assert_eq!(shared_file, lock.paths[2]);
        assert_eq!(CandidateKind::File, lock.kinds[2]);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn typed_current_candidate_validation_rejects_missing_and_changed_kinds() {
        let root = temp_root("current-kind");
        fs_create_dir(root.join("primary/folder"));
        fs_create_dir(root.join("primary/directory-to-file"));
        write_test_file(&root, "primary/file.txt", b"");
        write_test_file(&root, "primary/file-to-directory", b"");
        let primary = realpath(&root, "primary");

        let mut index = FileIndex::default();
        index.roots = vec![primary.clone()];

        assert!(index.is_current_candidate_kind(b"file.txt", CandidateKind::File));
        assert!(!index.is_current_candidate_kind(b"file.txt", CandidateKind::Directory));
        assert!(index.is_current_candidate_kind(b"folder", CandidateKind::Directory));
        assert!(!index.is_current_candidate_kind(b"folder", CandidateKind::File));
        assert!(!index.is_current_candidate_kind(b"missing", CandidateKind::File));
        assert!(!index.is_current_candidate_kind(b"unsafe\x1b", CandidateKind::File));

        std::fs::remove_file(root.join("primary/file-to-directory")).unwrap();
        fs_create_dir(root.join("primary/file-to-directory"));
        assert!(!index.is_current_candidate_kind(b"file-to-directory", CandidateKind::File));
        assert!(index.is_current_candidate_kind(b"file-to-directory", CandidateKind::Directory));

        std::fs::remove_dir_all(root.join("primary/directory-to-file")).unwrap();
        write_test_file(&root, "primary/directory-to-file", b"");
        assert!(!index.is_current_candidate_kind(b"directory-to-file", CandidateKind::Directory));
        assert!(index.is_current_candidate_kind(b"directory-to-file", CandidateKind::File));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn typed_current_candidate_validation_keeps_symlinks_as_file_references() {
        let root = temp_root("symlink-kind");
        fs_create_dir(root.join("primary/target-dir"));
        write_test_file(&root, "primary/target.txt", b"");
        std::os::unix::fs::symlink("target.txt", root.join("primary/file-link")).unwrap();
        std::os::unix::fs::symlink("target-dir", root.join("primary/directory-link")).unwrap();
        let primary = realpath(&root, "primary");

        let mut index = FileIndex::default();
        index.roots = vec![primary];

        assert!(index.is_current_candidate_kind(b"file-link", CandidateKind::File));
        assert!(!index.is_current_candidate_kind(b"file-link", CandidateKind::Directory));
        assert!(index.is_current_candidate_kind(b"directory-link", CandidateKind::File));
        assert!(!index.is_current_candidate_kind(b"directory-link", CandidateKind::Directory));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn current_candidate_rejects_an_added_root_selection_after_scope_removal() {
        let root = temp_root("added-root");
        fs_create_dir(root.join("primary"));
        fs_create_dir(root.join("shared"));
        write_test_file(&root, "shared/item.txt", b"");
        let primary = realpath(&root, "primary");
        let shared = realpath(&root, "shared");
        let shared_file = join_bytes(&shared, b"item.txt");

        let entries = [workspace_access::Entry {
            path: shared.clone(),
            saved: true,
            command_line: false,
            available: true,
            active: true,
        }];
        let scope = workspace_access::AccessScope {
            primary_directory: &primary,
            additional_directories: &entries,
        };
        let mut index = FileIndex::default();
        index.roots = active_roots_alloc(&scope);
        assert!(index.is_current_candidate_kind(&shared_file, CandidateKind::File));

        index.roots = active_roots_alloc(&workspace_access::AccessScope::primary_only(&primary));
        assert!(!index.is_current_candidate_kind(&shared_file, CandidateKind::File));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn ensure_scope_with_empty_workspace_root_stays_idle() {
        let mut index = FileIndex::default();
        index.ensure_scope(&workspace_access::AccessScope::primary_only(b""));
        assert_eq!(State::Idle, index.current_state());
        assert!(index.thread.is_none());
        assert_eq!(0, index.roots.len());
    }

    fn fs_create_dir(path: PathBuf) {
        std::fs::create_dir_all(path).unwrap();
    }
}
