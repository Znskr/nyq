//! Owned workspace access scope: primary root plus saved/command-line
//! additional roots, with staged mutation and availability refresh.

use std::fs;
use std::path::Path;

use crate::core::shared::types::ResolveMode;
use crate::core::workspace::pathing;

pub const max_additional_directories: usize = 16;
const MAX_PATH_BYTES: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    InvalidPath,
    PathNotFound,
    NotDirectory,
    UnknownAdditionalDirectory,
    PrimaryDirectory,
    TooManyDirectories,
    PathOutsideWorkspace,
    Path(pathing::PathError),
}

/// Returns an owned identity for an existing directory.
pub fn add_directory_identity_alloc(
    primary_directory: &[u8],
    input_path: &[u8],
) -> Result<Vec<u8>, Error> {
    canonical_existing_directory(primary_directory, input_path)
}

/// Returns an owned identity for a stored directory.
pub fn stored_directory_identity_alloc(
    primary_directory: &[u8],
    input_path: &[u8],
) -> Result<Vec<u8>, Error> {
    let resolved = resolve_saved_directory(primary_directory, input_path)?;
    Ok(resolved.path)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Entry {
    pub path: Vec<u8>,
    pub saved: bool,
    pub command_line: bool,
    pub available: bool,
    pub active: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SavedSource {
    pub source: Vec<u8>,
    pub identity: Vec<u8>,
    pub identity_canonical: bool,
}

#[derive(Clone, Debug)]
pub struct AccessScope<'a> {
    pub primary_directory: &'a [u8],
    pub additional_directories: &'a [Entry],
}

impl<'a> AccessScope<'a> {
    pub fn contains(&self, absolute_path: &[u8]) -> bool {
        self.root_for_path(absolute_path).is_some()
    }

    pub fn primary_only(primary_directory: &'a [u8]) -> AccessScope<'a> {
        AccessScope {
            primary_directory,
            additional_directories: &[],
        }
    }

    pub fn additional_root_for_path(&self, absolute_path: &[u8]) -> Option<&'a [u8]> {
        self.additional_directories
            .iter()
            .find(|entry| entry.active && pathing::path_inside(&entry.path, absolute_path))
            .map(|entry| entry.path.as_slice())
    }

    pub fn root_for_path(&self, absolute_path: &[u8]) -> Option<&'a [u8]> {
        if pathing::path_inside(self.primary_directory, absolute_path) {
            return Some(self.primary_directory);
        }
        self.additional_root_for_path(absolute_path)
    }

    pub fn resolve_path(&self, input_path: &[u8], mode: ResolveMode) -> Result<Vec<u8>, Error> {
        let trimmed = trim(input_path, b" \t\r\n");
        if !is_absolute(trimmed) && !trimmed.starts_with(b"~") {
            return pathing::resolve_workspace_path(self.primary_directory, input_path, mode)
                .map_err(map_path_error);
        }
        let resolved = match mode {
            ResolveMode::Existing => {
                pathing::resolve_workspace_or_external_path(self.primary_directory, input_path)
            }
            ResolveMode::Create => {
                pathing::resolve_workspace_or_external_create_path(self.primary_directory, input_path)
            }
        }
        .map_err(map_path_error)?;
        if !self.contains(&resolved) {
            return Err(Error::PathOutsideWorkspace);
        }
        Ok(resolved)
    }
}

#[derive(Debug, Default, PartialEq)]
pub struct WorkspaceAccess {
    pub entries: Vec<Entry>,
    pub saved_sources: Vec<SavedSource>,
    pub saved_suppressed: bool,
}

impl WorkspaceAccess {
    pub fn init(
        primary_directory: &[u8],
        saved_directories: &[&[u8]],
        command_line_directories: &[&[u8]],
        saved_suppressed: bool,
    ) -> Result<WorkspaceAccess, Error> {
        let mut entries: Vec<Entry> = Vec::new();
        let mut saved_sources: Vec<SavedSource> = Vec::new();

        for path in saved_directories {
            let resolved = resolve_saved_directory(primary_directory, path)?;
            append_or_merge(
                &mut entries,
                resolved.path.clone(),
                true,
                false,
                resolved.available,
            )?;
            saved_sources.push(SavedSource {
                source: path.to_vec(),
                identity: resolved.path,
                identity_canonical: resolved.available,
            });
        }
        for path in command_line_directories {
            let resolved = canonical_existing_directory(primary_directory, path)?;
            append_or_merge(&mut entries, resolved, false, true, true)?;
        }

        let mut result = WorkspaceAccess {
            entries,
            saved_sources,
            saved_suppressed,
        };
        result.recompute_active();
        Ok(result)
    }

    pub fn deinit(&mut self) {
        self.entries.clear();
        self.saved_sources.clear();
        self.saved_suppressed = false;
    }

    pub fn scope<'a>(&'a self, primary_directory: &'a [u8]) -> AccessScope<'a> {
        AccessScope {
            primary_directory,
            additional_directories: &self.entries,
        }
    }

    pub fn eql(&self, other: &WorkspaceAccess) -> bool {
        self.entries == other.entries && self.saved_suppressed == other.saved_suppressed
    }

    pub fn command_line_source_removed(&self, replacement: &WorkspaceAccess) -> bool {
        for entry in &self.entries {
            if !entry.command_line {
                continue;
            }
            let retained = replacement.entries.iter().any(|candidate| {
                candidate.command_line && candidate.path == entry.path
            });
            if !retained {
                return true;
            }
        }
        false
    }

    pub fn stage_availability_refresh(
        &self,
        primary_directory: &[u8],
    ) -> Result<Option<WorkspaceAccess>, Error> {
        let mut refreshed_sources: Vec<RefreshedSavedSource> = Vec::new();
        for source in &self.saved_sources {
            let resolved = if source.identity_canonical {
                resolve_observed_directory(primary_directory, &source.identity)?
            } else {
                resolve_saved_directory(primary_directory, &source.source)?
            };
            refreshed_sources.push(RefreshedSavedSource {
                source: source.clone(),
                identity: resolved.path,
                identity_canonical: source.identity_canonical || resolved.available,
                available: resolved.available,
            });
        }

        let mut replacement = WorkspaceAccess {
            saved_suppressed: self.saved_suppressed,
            ..WorkspaceAccess::default()
        };
        for source in &refreshed_sources {
            append_saved_source(
                &mut replacement,
                &source.source.source,
                &source.identity,
                source.identity_canonical,
            )?;
        }

        let mut appended_sources = vec![false; refreshed_sources.len()];
        for entry in &self.entries {
            for (source_index, source) in refreshed_sources.iter().enumerate() {
                if appended_sources[source_index] || source.source.identity != entry.path {
                    continue;
                }
                append_owned_entry_or_merge(
                    &mut replacement,
                    source.identity.clone(),
                    true,
                    false,
                    source.available,
                )?;
                appended_sources[source_index] = true;
            }
            if !entry.command_line {
                continue;
            }
            let resolved = resolve_observed_directory(primary_directory, &entry.path)?;
            append_owned_entry_or_merge(
                &mut replacement,
                resolved.path,
                false,
                true,
                resolved.available,
            )?;
        }
        for (source_index, source) in refreshed_sources.iter().enumerate() {
            if appended_sources[source_index] {
                continue;
            }
            append_owned_entry_or_merge(
                &mut replacement,
                source.identity.clone(),
                true,
                false,
                source.available,
            )?;
        }

        replacement.recompute_active();
        if self.refresh_state_eql(&replacement) {
            return Ok(None);
        }
        Ok(Some(replacement))
    }

    pub fn saved_directories_alloc(&self) -> Vec<Vec<u8>> {
        self.entries
            .iter()
            .filter(|entry| entry.saved)
            .map(|entry| entry.path.clone())
            .collect()
    }

    pub fn saved_source_paths_alloc(&self) -> Vec<Vec<u8>> {
        self.saved_sources
            .iter()
            .map(|source| source.source.clone())
            .collect()
    }

    pub fn stage_add_saved(
        &self,
        primary_directory: &[u8],
        input_path: &[u8],
    ) -> Result<WorkspaceAccess, Error> {
        let canonical = canonical_existing_directory(primary_directory, input_path)?;
        for entry in &self.entries {
            if entry.saved && entry.path == canonical {
                return Ok(self.clone());
            }
        }

        let mut replacement = self.clone();
        append_saved_source(&mut replacement, &canonical, &canonical, true)?;

        for entry in &mut replacement.entries {
            if entry.path != canonical {
                continue;
            }
            entry.saved = true;
            replacement.recompute_active();
            return Ok(replacement);
        }
        if replacement.entries.len() >= max_additional_directories {
            return Err(Error::TooManyDirectories);
        }
        append_entry(
            &mut replacement,
            Entry {
                path: canonical,
                saved: true,
                command_line: false,
                available: true,
                active: false,
            },
        )?;
        replacement.recompute_active();
        Ok(replacement)
    }

    pub fn stage_remove(
        &self,
        primary_directory: &[u8],
        input_path: &[u8],
    ) -> Result<WorkspaceAccess, Error> {
        let identity = self.removal_identity_alloc(primary_directory, input_path)?;
        let mut replacement = self.clone();
        remove_identity(&mut replacement, &identity)?;
        replacement.recompute_active();
        Ok(replacement)
    }

    pub fn stage_clear(&self) -> WorkspaceAccess {
        WorkspaceAccess {
            saved_suppressed: self.saved_suppressed,
            ..WorkspaceAccess::default()
        }
    }

    pub fn clone(&self) -> WorkspaceAccess {
        WorkspaceAccess {
            entries: self.entries.clone(),
            saved_sources: self.saved_sources.clone(),
            saved_suppressed: self.saved_suppressed,
        }
    }

    pub fn apply_launch(
        &mut self,
        primary_directory: &[u8],
        command_line_directories: &[&[u8]],
        saved_suppressed: bool,
    ) -> Result<(), Error> {
        let mut replacement = WorkspaceAccess {
            saved_suppressed,
            ..WorkspaceAccess::default()
        };
        for entry in &self.entries {
            if !entry.saved {
                continue;
            }
            append_entry(
                &mut replacement,
                Entry {
                    path: entry.path.clone(),
                    saved: true,
                    command_line: false,
                    available: entry.available,
                    active: false,
                },
            )?;
        }
        for source in &self.saved_sources {
            append_saved_source(
                &mut replacement,
                &source.source,
                &source.identity,
                source.identity_canonical,
            )?;
        }
        for path in command_line_directories {
            let canonical = canonical_existing_directory(primary_directory, path)?;
            append_owned_entry_or_merge(&mut replacement, canonical, false, true, true)?;
        }
        replacement.recompute_active();
        *self = replacement;
        Ok(())
    }

    pub fn command_line_directories_alloc(&self) -> Vec<Vec<u8>> {
        self.entries
            .iter()
            .filter(|entry| entry.command_line)
            .map(|entry| entry.path.clone())
            .collect()
    }

    fn removal_identity_alloc(
        &self,
        primary_directory: &[u8],
        input_path: &[u8],
    ) -> Result<Vec<u8>, Error> {
        let normalized_input = resolve_absolute_input(primary_directory, input_path)?;
        for source in &self.saved_sources {
            let normalized_source = resolve_absolute_input(primary_directory, &source.source)?;
            if normalized_source == normalized_input {
                return Ok(source.identity.clone());
            }
        }

        let resolved = resolve_saved_directory(primary_directory, input_path)?;
        for entry in &self.entries {
            if entry.path == resolved.path {
                return Ok(resolved.path);
            }
        }
        Err(Error::UnknownAdditionalDirectory)
    }

    fn recompute_active(&mut self) {
        for entry in &mut self.entries {
            let source_active = entry.command_line || (entry.saved && !self.saved_suppressed);
            entry.active = entry.available && source_active;
        }
    }

    fn refresh_state_eql(&self, other: &WorkspaceAccess) -> bool {
        if !self.eql(other) || self.saved_sources.len() != other.saved_sources.len() {
            return false;
        }
        for (left, right) in self.saved_sources.iter().zip(&other.saved_sources) {
            if left.source != right.source
                || left.identity != right.identity
                || left.identity_canonical != right.identity_canonical
            {
                return false;
            }
        }
        true
    }
}

struct SavedResolution {
    path: Vec<u8>,
    available: bool,
}

struct RefreshedSavedSource {
    source: SavedSource,
    identity: Vec<u8>,
    identity_canonical: bool,
    available: bool,
}

fn resolve_saved_directory(
    primary_directory: &[u8],
    input_path: &[u8],
) -> Result<SavedResolution, Error> {
    validate_stored_path(input_path)?;
    match canonical_existing_directory(primary_directory, input_path) {
        Ok(canonical) => Ok(SavedResolution {
            path: canonical,
            available: true,
        }),
        Err(Error::PathNotFound) | Err(Error::NotDirectory) => {
            let normalized = resolve_absolute_input(primary_directory, input_path)?;
            if normalized == primary_directory {
                return Err(Error::PrimaryDirectory);
            }
            let identity = pathing::resolve_create_target_from_nearest_existing(b"/", &normalized)
                .map_err(|_| Error::InvalidPath)?;
            Ok(SavedResolution {
                path: identity,
                available: false,
            })
        }
        Err(err) => Err(err),
    }
}

fn canonical_existing_directory(
    primary_directory: &[u8],
    input_path: &[u8],
) -> Result<Vec<u8>, Error> {
    if input_path.is_empty()
        || input_path.len() > MAX_PATH_BYTES
        || std::str::from_utf8(input_path).is_err()
        || input_path.contains(&0)
    {
        return Err(Error::InvalidPath);
    }

    let absolute = if is_absolute(input_path) {
        input_path.to_vec()
    } else {
        lexical_join(primary_directory, input_path)
    };

    let canonical = match fs::canonicalize(path_from_bytes(&absolute).ok_or(Error::InvalidPath)?) {
        Ok(path) => bytes_from_path(&path),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Err(Error::PathNotFound),
        Err(_) => return Err(Error::InvalidPath),
    };

    let stat = match fs::symlink_metadata(path_from_bytes(&canonical).ok_or(Error::InvalidPath)?) {
        Ok(meta) => meta,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Err(Error::PathNotFound),
        Err(_) => return Err(Error::InvalidPath),
    };
    if !stat.is_dir() {
        return Err(Error::NotDirectory);
    }
    if canonical == primary_directory {
        return Err(Error::PrimaryDirectory);
    }
    Ok(canonical)
}

fn resolve_observed_directory(
    primary_directory: &[u8],
    identity: &[u8],
) -> Result<SavedResolution, Error> {
    match canonical_existing_directory(primary_directory, identity) {
        Ok(canonical) => {
            if canonical == identity {
                Ok(SavedResolution {
                    path: canonical,
                    available: true,
                })
            } else {
                Ok(SavedResolution {
                    path: identity.to_vec(),
                    available: false,
                })
            }
        }
        Err(Error::PathNotFound) | Err(Error::NotDirectory) => Ok(SavedResolution {
            path: identity.to_vec(),
            available: false,
        }),
        Err(err) => Err(err),
    }
}

fn validate_stored_path(path: &[u8]) -> Result<(), Error> {
    if path.is_empty()
        || path.len() > MAX_PATH_BYTES
        || !is_absolute(path)
        || std::str::from_utf8(path).is_err()
        || path.contains(&0)
    {
        return Err(Error::InvalidPath);
    }
    Ok(())
}

fn resolve_absolute_input(
    primary_directory: &[u8],
    input_path: &[u8],
) -> Result<Vec<u8>, Error> {
    if input_path.is_empty()
        || input_path.len() > MAX_PATH_BYTES
        || std::str::from_utf8(input_path).is_err()
        || input_path.contains(&0)
    {
        return Err(Error::InvalidPath);
    }
    Ok(if is_absolute(input_path) {
        lexical_normalize(input_path)
    } else {
        lexical_join(primary_directory, input_path)
    })
}

fn append_entry(access: &mut WorkspaceAccess, entry: Entry) -> Result<(), Error> {
    if access.entries.len() >= max_additional_directories {
        return Err(Error::TooManyDirectories);
    }
    access.entries.push(entry);
    Ok(())
}

fn append_saved_source(
    access: &mut WorkspaceAccess,
    source: &[u8],
    identity: &[u8],
    identity_canonical: bool,
) -> Result<(), Error> {
    access.saved_sources.push(SavedSource {
        source: source.to_vec(),
        identity: identity.to_vec(),
        identity_canonical,
    });
    Ok(())
}

fn append_owned_entry_or_merge(
    access: &mut WorkspaceAccess,
    owned_path: Vec<u8>,
    saved: bool,
    command_line: bool,
    available: bool,
) -> Result<(), Error> {
    for entry in &mut access.entries {
        if entry.path != owned_path {
            continue;
        }
        entry.saved = entry.saved || saved;
        entry.command_line = entry.command_line || command_line;
        entry.available = entry.available || available;
        return Ok(());
    }
    if access.entries.len() >= max_additional_directories {
        return Err(Error::TooManyDirectories);
    }
    append_entry(
        access,
        Entry {
            path: owned_path,
            saved,
            command_line,
            available,
            active: false,
        },
    )
}

fn append_or_merge(
    entries: &mut Vec<Entry>,
    owned_path: Vec<u8>,
    saved: bool,
    command_line: bool,
    available: bool,
) -> Result<(), Error> {
    for entry in entries.iter_mut() {
        if entry.path != owned_path {
            continue;
        }
        entry.saved = entry.saved || saved;
        entry.command_line = entry.command_line || command_line;
        entry.available = entry.available || available;
        return Ok(());
    }
    if entries.len() >= max_additional_directories {
        return Err(Error::TooManyDirectories);
    }
    entries.push(Entry {
        path: owned_path,
        saved,
        command_line,
        available,
        active: false,
    });
    Ok(())
}

fn remove_identity(access: &mut WorkspaceAccess, identity: &[u8]) -> Result<(), Error> {
    access.entries.retain(|entry| entry.path != identity);
    access.saved_sources.retain(|source| source.identity != identity);
    Ok(())
}

fn map_path_error(err: pathing::PathError) -> Error {
    if err == pathing::PathError::PathOutsideWorkspace {
        Error::PathOutsideWorkspace
    } else {
        Error::Path(err)
    }
}

fn is_absolute(path: &[u8]) -> bool {
    path.first() == Some(&b'/')
}

fn trim<'a>(mut bytes: &'a [u8], whitespace: &[u8]) -> &'a [u8] {
    while bytes.first().is_some_and(|b| whitespace.contains(b)) {
        bytes = &bytes[1..];
    }
    while bytes.last().is_some_and(|b| whitespace.contains(b)) {
        bytes = &bytes[..bytes.len() - 1];
    }
    bytes
}

fn lexical_normalize(path: &[u8]) -> Vec<u8> {
    let absolute = is_absolute(path);
    let mut components: Vec<&[u8]> = Vec::new();
    for component in path.split(|&b| b == b'/') {
        if component.is_empty() || component == b"." {
            continue;
        }
        if component == b".." {
            components.pop();
            continue;
        }
        components.push(component);
    }
    let mut out = Vec::new();
    if absolute {
        out.push(b'/');
    }
    for (index, component) in components.iter().enumerate() {
        if index > 0 {
            out.push(b'/');
        }
        out.extend_from_slice(component);
    }
    out
}

fn lexical_join(base: &[u8], relative: &[u8]) -> Vec<u8> {
    let mut joined = Vec::with_capacity(base.len() + 1 + relative.len());
    joined.extend_from_slice(base);
    if !joined.ends_with(b"/") {
        joined.push(b'/');
    }
    joined.extend_from_slice(relative);
    lexical_normalize(&joined)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_root(name: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("nyq-workspace-access-{}-{}", name, std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn realpath(root: &Path, rel: &str) -> Vec<u8> {
        fs::canonicalize(root.join(rel))
            .unwrap()
            .to_string_lossy()
            .as_bytes()
            .to_vec()
    }

    fn join(a: &[u8], b: &[u8]) -> Vec<u8> {
        lexical_join(a, b)
    }

    fn symlink_dir(root: &Path, target: &str, link: &str) {
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, root.join(link)).unwrap();
        #[cfg(not(unix))]
        {
            let _ = (root, target, link);
        }
    }

    #[test]
    fn workspace_access_merges_source_state_without_moving_roots() {
        let root = temp_root("merge");
        fs::create_dir_all(root.join("primary")).unwrap();
        fs::create_dir_all(root.join("shared")).unwrap();
        fs::create_dir_all(root.join("docs")).unwrap();
        let primary = realpath(&root, "primary");
        let shared = realpath(&root, "shared");
        let docs = realpath(&root, "docs");

        let access = WorkspaceAccess::init(&primary, &[&shared, &docs], &[&shared], false).unwrap();

        assert_eq!(2, access.entries.len());
        assert_eq!(shared, access.entries[0].path);
        assert!(access.entries[0].saved);
        assert!(access.entries[0].command_line);
        assert!(access.entries[0].active);
        assert_eq!(docs, access.entries[1].path);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn workspace_access_retains_missing_saved_directories_as_inactive() {
        let root = temp_root("missing-inactive");
        fs::create_dir_all(root.join("primary")).unwrap();
        let primary = realpath(&root, "primary");
        let missing = join(&primary, b"../missing");

        let access = WorkspaceAccess::init(&primary, &[&missing], &[], false).unwrap();

        assert_eq!(1, access.entries.len());
        assert!(!access.entries[0].available);
        assert!(!access.entries[0].active);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn workspace_access_refreshes_availability_after_a_saved_directory_disappears_and_returns() {
        let root = temp_root("refresh-availability");
        fs::create_dir_all(root.join("primary")).unwrap();
        fs::create_dir_all(root.join("saved")).unwrap();
        let primary = realpath(&root, "primary");
        let saved = realpath(&root, "saved");

        let mut access = WorkspaceAccess::init(&primary, &[&saved], &[], false).unwrap();
        assert!(access.entries[0].available);
        assert!(access.entries[0].active);

        fs::remove_dir_all(root.join("saved")).unwrap();
        let unavailable = access.stage_availability_refresh(&primary).unwrap().unwrap();
        assert!(!unavailable.entries[0].available);
        assert!(!unavailable.entries[0].active);
        access = unavailable;

        fs::create_dir_all(root.join("saved")).unwrap();
        let available = access.stage_availability_refresh(&primary).unwrap().unwrap();
        assert!(available.entries[0].available);
        assert!(available.entries[0].active);
        access = available;
        assert!(access.stage_availability_refresh(&primary).unwrap().is_none());

        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn workspace_access_canonicalizes_a_saved_directory_restored_through_a_symlinked_ancestor() {
        let root = temp_root("symlinked-ancestor");
        fs::create_dir_all(root.join("primary")).unwrap();
        fs::create_dir_all(root.join("real-parent/shared")).unwrap();
        let primary = realpath(&root, "primary");
        let shared = realpath(&root, "real-parent/shared");
        let source = join(&primary, b"../parent-link/shared");

        let access = WorkspaceAccess::init(&primary, &[&source], &[], false).unwrap();
        assert!(!access.entries[0].available);

        symlink_dir(&root, "real-parent", "parent-link");

        let replacement = access.stage_availability_refresh(&primary).unwrap().unwrap();
        assert!(!access.entries[0].available);
        assert_eq!(source, access.entries[0].path);
        assert_eq!(shared, replacement.entries[0].path);
        assert_eq!(shared, replacement.saved_sources[0].identity);
        assert!(replacement.saved_sources[0].identity_canonical);
        assert!(replacement.scope(&primary).contains(&shared));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn workspace_access_normalizes_equivalent_missing_saved_directory_identities() {
        let root = temp_root("normalize-missing");
        fs::create_dir_all(root.join("primary")).unwrap();
        let primary = realpath(&root, "primary");
        let missing = join(&primary, b"../missing");
        let dot_alias = join(&missing, b".");
        let parent_alias = join(&missing, b"child/..");

        let access = WorkspaceAccess::init(&primary, &[&dot_alias, &parent_alias], &[], false).unwrap();

        assert_eq!(1, access.entries.len());
        assert_eq!(missing, access.entries[0].path);
        let removed = access.stage_remove(&primary, &missing).unwrap();
        assert_eq!(0, removed.entries.len());

        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn workspace_access_preserves_observed_source_identity_across_retarget_and_launch() {
        let root = temp_root("retarget");
        fs::create_dir_all(root.join("primary")).unwrap();
        fs::create_dir_all(root.join("first")).unwrap();
        fs::create_dir_all(root.join("second")).unwrap();
        symlink_dir(&root, "first", "saved-link");

        let primary = realpath(&root, "primary");
        let first = realpath(&root, "first");
        let source = join(&primary, b"../saved-link");

        let access = WorkspaceAccess::init(&primary, &[&source], &[], false).unwrap();
        assert_eq!(1, access.saved_sources.len());
        assert_eq!(source, access.saved_sources[0].source);
        assert_eq!(first, access.saved_sources[0].identity);

        fs::remove_file(root.join("saved-link")).unwrap();
        symlink_dir(&root, "second", "saved-link");

        let mut cloned = access.clone();
        cloned.apply_launch(&primary, &[], false).unwrap();
        assert_eq!(first, cloned.entries[0].path);
        assert_eq!(first, cloned.saved_sources[0].identity);

        let removed = cloned.stage_remove(&primary, &source).unwrap();
        assert_eq!(0, removed.entries.len());
        assert_eq!(0, removed.saved_sources.len());

        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn workspace_access_removes_a_disappeared_saved_source_by_its_spelling() {
        let root = temp_root("remove-by-spelling");
        fs::create_dir_all(root.join("primary")).unwrap();
        fs::create_dir_all(root.join("shared")).unwrap();
        symlink_dir(&root, "shared", "saved-link");

        let primary = realpath(&root, "primary");
        let source = join(&primary, b"../saved-link");

        let access = WorkspaceAccess::init(&primary, &[&source], &[], false).unwrap();
        fs::remove_file(root.join("saved-link")).unwrap();

        let removed = access.stage_remove(&primary, &source).unwrap();
        assert_eq!(0, removed.entries.len());

        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn workspace_access_resolves_unavailable_identities_through_existing_ancestor_links() {
        let root = temp_root("ancestor-links");
        fs::create_dir_all(root.join("primary")).unwrap();
        fs::create_dir_all(root.join("real-parent")).unwrap();
        symlink_dir(&root, "real-parent", "parent-link");

        let primary = realpath(&root, "primary");
        let real_parent = realpath(&root, "real-parent");
        let real_missing = join(&real_parent, b"missing");
        let linked_missing = join(&primary, b"../parent-link/missing");

        let access =
            WorkspaceAccess::init(&primary, &[&real_missing, &linked_missing], &[], false).unwrap();

        assert_eq!(1, access.entries.len());
        assert_eq!(real_missing, access.entries[0].path);
        assert_eq!(2, access.saved_sources.len());
        let removed = access.stage_remove(&primary, &linked_missing).unwrap();
        assert_eq!(0, removed.entries.len());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn workspace_access_suppresses_saved_roots_but_keeps_command_line_roots() {
        let root = temp_root("suppress");
        fs::create_dir_all(root.join("primary")).unwrap();
        fs::create_dir_all(root.join("saved")).unwrap();
        fs::create_dir_all(root.join("launch")).unwrap();
        let primary = realpath(&root, "primary");
        let saved = realpath(&root, "saved");
        let launch = realpath(&root, "launch");

        let access = WorkspaceAccess::init(&primary, &[&saved], &[&launch], true).unwrap();

        assert!(!access.entries[0].active);
        assert!(access.entries[1].active);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn workspace_access_rejects_the_primary_root_and_capacity_overflow() {
        let root = temp_root("reject-primary");
        fs::create_dir_all(root.join("primary")).unwrap();
        let primary = realpath(&root, "primary");

        assert_eq!(
            Err(Error::PrimaryDirectory),
            WorkspaceAccess::init(&primary, &[&primary], &[], false)
        );

        let mut paths: Vec<Vec<u8>> = Vec::new();
        for index in 0..(max_additional_directories + 1) {
            let name = format!("root-{index}");
            fs::create_dir_all(root.join(&name)).unwrap();
            paths.push(realpath(&root, &name));
        }
        let refs: Vec<&[u8]> = paths.iter().map(|p| p.as_slice()).collect();
        assert_eq!(
            Err(Error::TooManyDirectories),
            WorkspaceAccess::init(&primary, &refs, &[], false)
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn access_scope_admits_only_active_roots() {
        let entries = [
            Entry {
                path: b"/shared".to_vec(),
                saved: true,
                command_line: false,
                available: true,
                active: true,
            },
            Entry {
                path: b"/offline".to_vec(),
                saved: true,
                command_line: false,
                available: false,
                active: false,
            },
        ];
        let scope = AccessScope {
            primary_directory: b"/project",
            additional_directories: &entries,
        };

        assert!(scope.contains(b"/project/src/main.zig"));
        assert!(scope.contains(b"/shared/lib.zig"));
        assert!(!scope.contains(b"/offline/file.txt"));
        assert!(!scope.contains(b"/external/file.txt"));
    }

    #[test]
    fn workspace_access_stages_add_remove_clear_without_mutating_the_source() {
        let root = temp_root("staged");
        fs::create_dir_all(root.join("primary")).unwrap();
        fs::create_dir_all(root.join("saved")).unwrap();
        fs::create_dir_all(root.join("launch")).unwrap();
        let primary = realpath(&root, "primary");
        let saved = realpath(&root, "saved");
        let launch = realpath(&root, "launch");

        let original = WorkspaceAccess::init(&primary, &[&saved], &[&launch], false).unwrap();

        let added = original.stage_add_saved(&primary, &launch).unwrap();
        assert_eq!(2, original.entries.len());
        assert!(!original.entries[1].saved);
        assert!(added.entries[1].saved);
        assert!(added.entries[1].command_line);
        assert_eq!(2, added.saved_sources.len());

        let removed = added.stage_remove(&primary, &launch).unwrap();
        assert_eq!(1, removed.entries.len());
        assert_eq!(saved, removed.entries[0].path);
        assert_eq!(1, removed.saved_sources.len());

        let cleared = removed.stage_clear();
        assert_eq!(0, cleared.entries.len());
        assert_eq!(0, cleared.saved_sources.len());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn workspace_access_rejects_removal_of_an_unknown_existing_directory() {
        let root = temp_root("remove-unknown-existing");
        fs::create_dir_all(root.join("primary")).unwrap();
        fs::create_dir_all(root.join("saved")).unwrap();
        fs::create_dir_all(root.join("unknown")).unwrap();
        let primary = realpath(&root, "primary");
        let saved = realpath(&root, "saved");
        let unknown = realpath(&root, "unknown");

        let access = WorkspaceAccess::init(&primary, &[&saved], &[], false).unwrap();
        assert_eq!(
            Err(Error::UnknownAdditionalDirectory),
            access.stage_remove(&primary, &unknown)
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn workspace_access_rejects_removal_of_an_unknown_missing_directory() {
        let root = temp_root("remove-unknown-missing");
        fs::create_dir_all(root.join("primary")).unwrap();
        fs::create_dir_all(root.join("saved")).unwrap();
        let primary = realpath(&root, "primary");
        let saved = realpath(&root, "saved");
        let unknown = join(&primary, b"../unknown");

        let access = WorkspaceAccess::init(&primary, &[&saved], &[], false).unwrap();
        assert_eq!(
            Err(Error::UnknownAdditionalDirectory),
            access.stage_remove(&primary, &unknown)
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn workspace_access_removes_a_known_saved_directory_after_it_disappears() {
        let root = temp_root("remove-after-disappear");
        fs::create_dir_all(root.join("primary")).unwrap();
        fs::create_dir_all(root.join("saved")).unwrap();
        let primary = realpath(&root, "primary");
        let saved = realpath(&root, "saved");

        let access = WorkspaceAccess::init(&primary, &[&saved], &[], false).unwrap();
        fs::remove_dir_all(root.join("saved")).unwrap();

        let removed = access.stage_remove(&primary, &saved).unwrap();
        assert_eq!(0, removed.entries.len());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn workspace_access_removes_an_unavailable_saved_directory_with_a_trailing_separator() {
        let root = temp_root("trailing-sep");
        fs::create_dir_all(root.join("primary")).unwrap();
        fs::create_dir_all(root.join("saved")).unwrap();
        let primary = realpath(&root, "primary");
        let saved = realpath(&root, "saved");
        let input_path = format!("{}/", String::from_utf8(saved.clone()).unwrap());

        let access = WorkspaceAccess::init(&primary, &[&saved], &[], false).unwrap();
        fs::remove_dir_all(root.join("saved")).unwrap();

        let removed = access.stage_remove(&primary, input_path.as_bytes()).unwrap();
        assert_eq!(0, removed.entries.len());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn workspace_access_removes_an_unavailable_saved_directory_with_a_dot_segment() {
        let root = temp_root("dot-segment");
        fs::create_dir_all(root.join("primary")).unwrap();
        fs::create_dir_all(root.join("saved")).unwrap();
        let primary = realpath(&root, "primary");
        let saved = realpath(&root, "saved");
        let input_path = format!("{}/.", String::from_utf8(saved.clone()).unwrap());

        let access = WorkspaceAccess::init(&primary, &[&saved], &[], false).unwrap();
        fs::remove_dir_all(root.join("saved")).unwrap();

        let removed = access.stage_remove(&primary, input_path.as_bytes()).unwrap();
        assert_eq!(0, removed.entries.len());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn workspace_access_removes_an_unavailable_saved_directory_with_a_parent_segment() {
        let root = temp_root("parent-segment");
        fs::create_dir_all(root.join("primary")).unwrap();
        fs::create_dir_all(root.join("saved")).unwrap();
        let primary = realpath(&root, "primary");
        let saved = realpath(&root, "saved");
        let input_path = format!("{}/missing/..", String::from_utf8(saved.clone()).unwrap());

        let access = WorkspaceAccess::init(&primary, &[&saved], &[], false).unwrap();
        fs::remove_dir_all(root.join("saved")).unwrap();

        let removed = access.stage_remove(&primary, input_path.as_bytes()).unwrap();
        assert_eq!(0, removed.entries.len());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn workspace_access_removes_a_known_saved_directory_replaced_by_a_file() {
        let root = temp_root("replaced-by-file");
        fs::create_dir_all(root.join("primary")).unwrap();
        fs::create_dir_all(root.join("saved")).unwrap();
        let primary = realpath(&root, "primary");
        let saved = realpath(&root, "saved");

        let access = WorkspaceAccess::init(&primary, &[&saved], &[], false).unwrap();
        fs::remove_dir_all(root.join("saved")).unwrap();
        fs::write(root.join("saved"), b"file").unwrap();

        let removed = access.stage_remove(&primary, &saved).unwrap();
        assert_eq!(0, removed.entries.len());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn workspace_access_reports_when_a_staged_replacement_removes_command_line_authority() {
        let root = temp_root("command-line-authority");
        fs::create_dir_all(root.join("primary")).unwrap();
        fs::create_dir_all(root.join("saved")).unwrap();
        fs::create_dir_all(root.join("launch")).unwrap();
        let primary = realpath(&root, "primary");
        let saved = realpath(&root, "saved");
        let launch = realpath(&root, "launch");

        let saved_only = WorkspaceAccess::init(&primary, &[&saved], &[], false).unwrap();
        let saved_removed = saved_only.stage_remove(&primary, &saved).unwrap();
        assert!(!saved_only.command_line_source_removed(&saved_removed));

        let flag_only = WorkspaceAccess::init(&primary, &[], &[&launch], false).unwrap();
        let flag_removed = flag_only.stage_remove(&primary, &launch).unwrap();
        assert!(flag_only.command_line_source_removed(&flag_removed));

        let both_sources = WorkspaceAccess::init(&primary, &[&launch], &[&launch], false).unwrap();
        let both_removed = both_sources.stage_remove(&primary, &launch).unwrap();
        assert!(both_sources.command_line_source_removed(&both_removed));

        let added = flag_only.stage_add_saved(&primary, &saved).unwrap();
        assert!(!flag_only.command_line_source_removed(&added));

        let cleared = flag_only.stage_clear();
        assert!(flag_only.command_line_source_removed(&cleared));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn workspace_access_releases_partial_state_and_supports_reinit() {
        let root = temp_root("reinit");
        fs::create_dir_all(root.join("primary")).unwrap();
        fs::create_dir_all(root.join("saved")).unwrap();
        fs::create_dir_all(root.join("launch")).unwrap();
        let primary = realpath(&root, "primary");
        let saved = realpath(&root, "saved");
        let launch = realpath(&root, "launch");

        let mut access = WorkspaceAccess::init(&primary, &[&saved], &[&launch], false).unwrap();
        assert_eq!(2, access.entries.len());
        access.deinit();
        assert_eq!(0, access.entries.len());
        assert_eq!(0, access.saved_sources.len());

        let _ = fs::remove_dir_all(&root);
    }
}
