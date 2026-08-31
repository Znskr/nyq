//! Workspace path resolution and bounded file-target traversal.
//!
//! The bounded resolver canonicalizes a target path without heap allocation:
//! callers provide two fixed path scratch buffers plus a component-span scratch
//! buffer. Intermediate symlinks are followed (with a bounded expansion count)
//! while the final component is preserved so create-mode callers can create
//! through a dangling final symlink when it stays inside the workspace.

use std::fs;
use std::path::Path;

use crate::core::shared::io;
use crate::core::shared::types::ResolveMode;

const PATH_ENTRY_WHITESPACE: &[u8] = b" \t\r\n";
const MAX_SYMBOLIC_LINK_EXPANSIONS: usize = 40;

/// Faithful mirror of the small `FileIdentity` struct from
/// `tooling/file_mutation_contract.zig` (which is not yet ported).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FileIdentity {
    pub device: u64,
    pub inode: u64,
    pub kind: FileKind,
}

/// Faithful mirror of `std.Io.File.Kind` for the kinds this module observes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileKind {
    Directory,
    File,
    SymLink,
    Unknown,
}

/// Faithful mirror of the `std.Io.File.Stat` fields consumed by `fileIdentity`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FileStat {
    pub inode: u64,
    pub kind: FileKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileIdentityError {
    Unexpected,
    SystemResources,
    ProcessFdQuotaExceeded,
    SystemFdQuotaExceeded,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PathError {
    PathOutsideWorkspace,
    FileNotFound,
    HomeNotSet,
    InvalidPath,
    WorkspaceUnavailable,
    TooManyPathComponents,
    AccessDenied,
    PermissionDenied,
    NotDir,
    SymLinkLoop,
    NameTooLong,
    BadPathName,
    NotLink,
    IsDir,
    PathAlreadyExists,
    OperationUnsupported,
    UnsupportedReparsePointType,
    NoDevice,
    NetworkNotFound,
    UnrecognizedVolume,
    InputOutput,
    FileSystem,
    FileTooBig,
    NoSpaceLeft,
    ReadOnlyFileSystem,
    DeviceBusy,
    FileBusy,
    PipeBusy,
    FileLocksUnsupported,
    WouldBlock,
    AntivirusInterference,
    Streaming,
    Canceled,
    Unexpected,
    SystemResources,
    ProcessFdQuotaExceeded,
    SystemFdQuotaExceeded,
}

/// Byte span of one path component inside a bounded scratch path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BoundedFileTargetComponent {
    pub start: usize,
    pub end: usize,
}

/// Result of a bounded file-target resolution.
#[derive(Debug, PartialEq)]
pub struct BoundedFileTargetResolution<'a, 'b> {
    pub canonical_target_path: &'a [u8],
    pub anchor_path_end: usize,
    pub anchor_is_external: bool,
    pub relative_components: &'b [BoundedFileTargetComponent],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PathScope {
    WorkspaceOnly,
    WorkspaceOrExternal,
    WorkspaceOrExternalCreate,
}

struct ResolvedInput {
    absolute: Vec<u8>,
    external_intent: bool,
}

enum ExternalPathInput<'a> {
    Absolute(&'a [u8]),
    HomeRelative(&'a [u8]),
    WorkspaceRelative(&'a [u8]),
}

pub fn file_identity(device: u64, stat: FileStat) -> FileIdentity {
    FileIdentity {
        device,
        inode: stat.inode,
        kind: stat.kind,
    }
}

pub fn directory_identity(path: &[u8]) -> Result<FileIdentity, FileIdentityError> {
    let meta = fs::symlink_metadata(path_from_bytes(path).ok_or(FileIdentityError::Unexpected)?)
        .map_err(|_| FileIdentityError::Unexpected)?;
    Ok(file_identity(device(&meta), stat_from_metadata(&meta)))
}

pub fn descriptor_device(path: &[u8]) -> Result<u64, FileIdentityError> {
    let meta = fs::metadata(path_from_bytes(path).ok_or(FileIdentityError::Unexpected)?)
        .map_err(|_| FileIdentityError::Unexpected)?;
    Ok(device(&meta))
}

pub fn directory_entry_device(dir: &[u8], component: &[u8]) -> Result<u64, FileIdentityError> {
    descriptor_device(&join_bytes(dir, component))
}

fn stat_from_metadata(meta: &fs::Metadata) -> FileStat {
    FileStat {
        inode: ino(meta),
        kind: kind_from_metadata(meta),
    }
}

fn kind_from_metadata(meta: &fs::Metadata) -> FileKind {
    let ft = meta.file_type();
    if ft.is_dir() {
        FileKind::Directory
    } else if ft.is_file() {
        FileKind::File
    } else if ft.is_symlink() {
        FileKind::SymLink
    } else {
        FileKind::Unknown
    }
}

#[cfg(unix)]
fn device(meta: &fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    meta.dev()
}

#[cfg(not(unix))]
fn device(_: &fs::Metadata) -> u64 {
    0
}

#[cfg(unix)]
fn ino(meta: &fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    meta.ino()
}

#[cfg(not(unix))]
fn ino(_: &fs::Metadata) -> u64 {
    0
}

pub fn resolve_workspace_path(
    workspace_root: &[u8],
    input_path: &[u8],
    mode: ResolveMode,
) -> Result<Vec<u8>, PathError> {
    resolve_path(workspace_root, input_path, None, mode, PathScope::WorkspaceOnly)
}

pub fn resolve_workspace_or_external_path(
    workspace_root: &[u8],
    input_path: &[u8],
) -> Result<Vec<u8>, PathError> {
    resolve_workspace_or_external_path_with_home(
        workspace_root,
        input_path,
        io::getenv(b"HOME").as_deref(),
        ResolveMode::Existing,
    )
}

pub fn resolve_workspace_or_external_create_path(
    workspace_root: &[u8],
    input_path: &[u8],
) -> Result<Vec<u8>, PathError> {
    resolve_workspace_or_external_path_with_home(
        workspace_root,
        input_path,
        io::getenv(b"HOME").as_deref(),
        ResolveMode::Create,
    )
}

pub fn resolve_file_mutation_target_bounded<'a, 'b>(
    workspace_root: &[u8],
    input_path: &[u8],
    mode: ResolveMode,
    primary_path_scratch: &'a mut [u8],
    secondary_path_scratch: &'a mut [u8],
    component_scratch: &'b mut [BoundedFileTargetComponent],
) -> Result<BoundedFileTargetResolution<'a, 'b>, PathError> {
    let cleaned = trim(input_path, PATH_ENTRY_WHITESPACE);
    if cleaned.is_empty() {
        return Err(PathError::InvalidPath);
    }

    let (initial_len, external_intent) =
        resolve_bounded_file_target_input(workspace_root, cleaned, secondary_path_scratch)?;
    resolve_bounded_absolute_file_target(
        workspace_root,
        initial_len,
        external_intent,
        mode,
        primary_path_scratch,
        secondary_path_scratch,
        component_scratch,
    )
}

fn resolve_workspace_or_external_path_with_home(
    workspace_root: &[u8],
    input_path: &[u8],
    home_dir: Option<&[u8]>,
    mode: ResolveMode,
) -> Result<Vec<u8>, PathError> {
    let scope = match mode {
        ResolveMode::Existing => PathScope::WorkspaceOrExternal,
        ResolveMode::Create => PathScope::WorkspaceOrExternalCreate,
    };
    resolve_path(workspace_root, input_path, home_dir, mode, scope)
}

fn resolve_path(
    workspace_root: &[u8],
    input_path: &[u8],
    home_dir: Option<&[u8]>,
    mode: ResolveMode,
    scope: PathScope,
) -> Result<Vec<u8>, PathError> {
    let cleaned = trim(input_path, PATH_ENTRY_WHITESPACE);
    if cleaned.is_empty() {
        return Err(PathError::InvalidPath);
    }

    let input = resolve_input(workspace_root, cleaned, home_dir, scope)?;
    let resolved = match mode {
        ResolveMode::Existing => canonicalize_bytes(&input.absolute)?,
        ResolveMode::Create => {
            if scope == PathScope::WorkspaceOrExternalCreate && input.external_intent {
                resolve_absolute_create_target_path(&input.absolute, 0)?
            } else {
                resolve_create_target_path(workspace_root, &input.absolute)?
            }
        }
    };

    if !input.external_intent {
        ensure_path_inside_workspace(workspace_root, &resolved)?;
    }
    Ok(resolved)
}

fn resolve_input(
    workspace_root: &[u8],
    cleaned: &[u8],
    home_dir: Option<&[u8]>,
    scope: PathScope,
) -> Result<ResolvedInput, PathError> {
    if scope == PathScope::WorkspaceOnly {
        return Ok(ResolvedInput {
            absolute: if is_absolute(cleaned) {
                lexical_normalize_absolute(cleaned)
            } else {
                lexical_join(workspace_root, cleaned)
            },
            external_intent: false,
        });
    }

    match classify_external_path_input(cleaned)? {
        ExternalPathInput::Absolute(absolute) => Ok(ResolvedInput {
            absolute: lexical_normalize_absolute(absolute),
            external_intent: true,
        }),
        ExternalPathInput::HomeRelative(relative) => {
            let home = home_dir.ok_or(PathError::HomeNotSet)?;
            if home.is_empty() || !is_absolute(home) {
                return Err(PathError::InvalidPath);
            }
            let expanded = if relative.is_empty() {
                home.to_vec()
            } else {
                join_bytes(home, relative)
            };
            Ok(ResolvedInput {
                absolute: lexical_normalize_absolute(&expanded),
                external_intent: true,
            })
        }
        ExternalPathInput::WorkspaceRelative(relative) => {
            let absolute = lexical_join(workspace_root, relative);
            Ok(ResolvedInput {
                external_intent: !path_inside(workspace_root, &absolute),
                absolute,
            })
        }
    }
}

fn classify_external_path_input(cleaned: &[u8]) -> Result<ExternalPathInput<'_>, PathError> {
    if cleaned.is_empty() {
        return Err(PathError::InvalidPath);
    }
    if is_absolute(cleaned) {
        return Ok(ExternalPathInput::Absolute(cleaned));
    }
    if cleaned == b"~" {
        return Ok(ExternalPathInput::HomeRelative(b""));
    }
    if let Some(rest) = cleaned.strip_prefix(b"~/") {
        return Ok(ExternalPathInput::HomeRelative(rest));
    }
    if cleaned[0] == b'~' {
        return Err(PathError::InvalidPath);
    }
    Ok(ExternalPathInput::WorkspaceRelative(cleaned))
}

fn resolve_bounded_file_target_input(
    workspace_root: &[u8],
    cleaned: &[u8],
    scratch: &mut [u8],
) -> Result<(usize, bool), PathError> {
    match classify_external_path_input(cleaned)? {
        ExternalPathInput::Absolute(absolute) => {
            let len = normalize_absolute_path_into(scratch, absolute)?;
            Ok((len, true))
        }
        ExternalPathInput::HomeRelative(relative) => {
            let home = io::getenv(b"HOME").ok_or(PathError::HomeNotSet)?;
            if home.is_empty() || !is_absolute(&home) {
                return Err(PathError::InvalidPath);
            }
            let len = normalize_base_relative_path_into(scratch, &home, relative)?;
            Ok((len, true))
        }
        ExternalPathInput::WorkspaceRelative(relative) => {
            if workspace_root.is_empty() || !is_absolute(workspace_root) {
                return Err(PathError::WorkspaceUnavailable);
            }
            let len = normalize_base_relative_path_into(scratch, workspace_root, relative)?;
            Ok((len, !path_inside(workspace_root, &scratch[..len])))
        }
    }
}

enum BoundedTraversalStep {
    RestartPathLen(usize),
    Complete {
        path_len: usize,
        anchor_path_end: usize,
        component_count: usize,
    },
}

#[allow(clippy::too_many_arguments)]
fn resolve_bounded_absolute_file_target<'a, 'b>(
    workspace_root: &[u8],
    initial_absolute_len: usize,
    external_intent: bool,
    mode: ResolveMode,
    primary_path_scratch: &'a mut [u8],
    secondary_path_scratch: &'a mut [u8],
    component_scratch: &'b mut [BoundedFileTargetComponent],
) -> Result<BoundedFileTargetResolution<'a, 'b>, PathError> {
    if primary_path_scratch.is_empty() || secondary_path_scratch.is_empty() {
        return Err(PathError::InvalidPath);
    }

    // Index 0 is the secondary scratch (initial pending path); index 1 is the
    // primary scratch (where the final canonical path must land).
    let mut buffers: [&'a mut [u8]; 2] = [secondary_path_scratch, primary_path_scratch];
    let mut pending_index = 0usize;
    let mut output_index = 1usize;
    let mut pending_path_len = initial_absolute_len;
    let mut symbolic_link_expansions: usize = 0;

    loop {
        let step = {
            let (pending, output) = if pending_index == 0 {
                let (a, b) = buffers.split_at_mut(1);
                (&mut a[0], &mut b[0])
            } else {
                let (a, b) = buffers.split_at_mut(1);
                (&mut b[0], &mut a[0])
            };
            traverse_bounded_absolute_file_target(
                workspace_root,
                pending,
                pending_path_len,
                mode,
                output,
                component_scratch,
                &mut symbolic_link_expansions,
            )?
        };

        match step {
            BoundedTraversalStep::RestartPathLen(restart_path_len) => {
                if !external_intent
                    && !path_inside(
                        workspace_root,
                        &buffers[output_index][..restart_path_len],
                    )
                {
                    return Err(PathError::PathOutsideWorkspace);
                }
                std::mem::swap(&mut pending_index, &mut output_index);
                pending_path_len = restart_path_len;
            }
            BoundedTraversalStep::Complete {
                path_len,
                anchor_path_end,
                component_count,
            } => {
                if !external_intent
                    && !path_inside(workspace_root, &buffers[output_index][..path_len])
                {
                    return Err(PathError::PathOutsideWorkspace);
                }

                if output_index != 1 {
                    if path_len > buffers[1].len() {
                        return Err(PathError::InvalidPath);
                    }
                    let (secondary, primary) = buffers.split_at_mut(1);
                    primary[0][..path_len].copy_from_slice(&secondary[0][..path_len]);
                }

                let canonical = &buffers[1][..path_len];
                return Ok(bounded_resolution(
                    workspace_root,
                    canonical,
                    anchor_path_end,
                    &component_scratch[..component_count],
                ));
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn traverse_bounded_absolute_file_target(
    workspace_root: &[u8],
    pending_scratch: &mut [u8],
    pending_path_len: usize,
    mode: ResolveMode,
    path_scratch: &mut [u8],
    component_scratch: &mut [BoundedFileTargetComponent],
    symbolic_link_expansions: &mut usize,
) -> Result<BoundedTraversalStep, PathError> {
    if pending_path_len == 0 || pending_path_len > pending_scratch.len() || path_scratch.is_empty() {
        return Err(PathError::InvalidPath);
    }

    let absolute = pending_scratch[..pending_path_len].to_vec();
    path_scratch[0] = b'/';
    let mut path_len: usize = 1;
    let mut component_count: usize = 0;
    let workspace_target = path_inside(workspace_root, &absolute);
    let mut workspace_anchor_end: Option<usize> =
        if workspace_target && workspace_root == b"/" {
            Some(1)
        } else {
            None
        };

    let mut iter = PathComponentIterator::init(&absolute);
    if !iter.has_next() {
        return Ok(BoundedTraversalStep::Complete {
            path_len,
            anchor_path_end: path_len,
            component_count: 0,
        });
    }

    let mut current_dir: Vec<u8> = vec![b'/'];

    while let Some(component) = iter.next() {
        let parent_path_end = path_len;
        let span = append_bounded_path_component(path_scratch, &mut path_len, component)?;
        let is_final = !iter.has_next();

        if is_final {
            let final_path = join_bytes(&current_dir, component);
            match fs::symlink_metadata(path_from_bytes(&final_path).ok_or(PathError::InvalidPath)?) {
                Ok(_) => {
                    if workspace_target && &path_scratch[..path_len] == workspace_root {
                        return Ok(BoundedTraversalStep::Complete {
                            path_len,
                            anchor_path_end: path_len,
                            component_count: 0,
                        });
                    }
                    append_bounded_relative_component(component_scratch, &mut component_count, span)?;
                    return Ok(BoundedTraversalStep::Complete {
                        path_len,
                        anchor_path_end: if workspace_target {
                            workspace_anchor_end.ok_or(PathError::PathOutsideWorkspace)?
                        } else {
                            parent_path_end
                        },
                        component_count,
                    });
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    if mode == ResolveMode::Existing {
                        return Err(PathError::FileNotFound);
                    }
                    append_bounded_relative_component(component_scratch, &mut component_count, span)?;
                    return Ok(BoundedTraversalStep::Complete {
                        path_len,
                        anchor_path_end: if workspace_target {
                            workspace_anchor_end.ok_or(PathError::PathOutsideWorkspace)?
                        } else {
                            parent_path_end
                        },
                        component_count,
                    });
                }
                Err(err) => return Err(map_io_error(err)),
            }
        }

        let child_path = join_bytes(&current_dir, component);
        match fs::symlink_metadata(path_from_bytes(&child_path).ok_or(PathError::InvalidPath)?) {
            Ok(meta) => {
                let ft = meta.file_type();
                if ft.is_symlink() {
                    let restart = resolve_bounded_intermediate_symlink(
                        &current_dir,
                        component,
                        parent_path_end,
                        iter.index,
                        pending_scratch,
                        pending_path_len,
                        path_scratch,
                        symbolic_link_expansions,
                    )?;
                    return Ok(BoundedTraversalStep::RestartPathLen(restart));
                } else if ft.is_dir() {
                    current_dir = child_path;
                    if workspace_target {
                        if workspace_anchor_end.is_none() {
                            if &path_scratch[..path_len] == workspace_root {
                                workspace_anchor_end = Some(path_len);
                            }
                        } else {
                            append_bounded_relative_component(
                                component_scratch,
                                &mut component_count,
                                span,
                            )?;
                        }
                    }
                } else {
                    return Err(PathError::NotDir);
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                if mode == ResolveMode::Existing {
                    return Err(PathError::FileNotFound);
                }
                append_bounded_relative_component(component_scratch, &mut component_count, span)?;
                while let Some(missing_component) = iter.next() {
                    let missing_span =
                        append_bounded_path_component(path_scratch, &mut path_len, missing_component)?;
                    append_bounded_relative_component(
                        component_scratch,
                        &mut component_count,
                        missing_span,
                    )?;
                }
                return Ok(BoundedTraversalStep::Complete {
                    path_len,
                    anchor_path_end: if workspace_target {
                        workspace_anchor_end.ok_or(PathError::PathOutsideWorkspace)?
                    } else {
                        parent_path_end
                    },
                    component_count,
                });
            }
            Err(err) => return Err(map_io_error(err)),
        }
    }

    Err(PathError::InvalidPath)
}

#[allow(clippy::too_many_arguments)]
fn resolve_bounded_intermediate_symlink(
    parent_dir: &[u8],
    component: &[u8],
    parent_path_end: usize,
    suffix_offset: usize,
    pending_scratch: &mut [u8],
    pending_path_len: usize,
    output_scratch: &mut [u8],
    symbolic_link_expansions: &mut usize,
) -> Result<usize, PathError> {
    if *symbolic_link_expansions >= MAX_SYMBOLIC_LINK_EXPANSIONS {
        return Err(PathError::SymLinkLoop);
    }
    *symbolic_link_expansions += 1;

    if suffix_offset > pending_path_len {
        return Err(PathError::InvalidPath);
    }
    let suffix_len = pending_path_len - suffix_offset;
    if suffix_len > pending_scratch.len() {
        return Err(PathError::InvalidPath);
    }
    let suffix_start = pending_scratch.len() - suffix_len;
    pending_scratch.copy_within(suffix_offset..pending_path_len, suffix_start);

    let link_path = join_bytes(parent_dir, component);
    let link_target = fs::read_link(path_from_bytes(&link_path).ok_or(PathError::InvalidPath)?)
        .map_err(map_read_link_error)?;
    let link_target = bytes_from_path(&link_target);
    if link_target.is_empty() {
        return Err(PathError::InvalidPath);
    }

    let mut resolved_len: usize = if is_absolute(&link_target) {
        if output_scratch.is_empty() {
            return Err(PathError::InvalidPath);
        }
        output_scratch[0] = b'/';
        1
    } else {
        parent_path_end
    };
    if resolved_len > output_scratch.len() {
        return Err(PathError::InvalidPath);
    }
    normalize_relative_path_part_into(output_scratch, &mut resolved_len, &link_target)?;
    normalize_relative_path_part_into(
        output_scratch,
        &mut resolved_len,
        &pending_scratch[suffix_start..suffix_start + suffix_len],
    )?;
    Ok(resolved_len)
}

#[derive(Clone, Copy)]
struct PathComponentIterator<'a> {
    path: &'a [u8],
    index: usize,
}

impl<'a> PathComponentIterator<'a> {
    fn init(path: &'a [u8]) -> Self {
        Self {
            path,
            index: if !path.is_empty() && path[0] == b'/' { 1 } else { 0 },
        }
    }

    fn next(&mut self) -> Option<&'a [u8]> {
        while self.index < self.path.len() && self.path[self.index] == b'/' {
            self.index += 1;
        }
        if self.index >= self.path.len() {
            return None;
        }
        let start = self.index;
        while self.index < self.path.len() && self.path[self.index] != b'/' {
            self.index += 1;
        }
        let end = self.index;
        if self.index < self.path.len() {
            self.index += 1;
        }
        Some(&self.path[start..end])
    }

    fn has_next(&self) -> bool {
        let mut copy = *self;
        copy.next().is_some()
    }
}

fn bounded_resolution<'a, 'b>(
    workspace_root: &[u8],
    canonical_target_path: &'a [u8],
    anchor_path_end: usize,
    relative_components: &'b [BoundedFileTargetComponent],
) -> BoundedFileTargetResolution<'a, 'b> {
    BoundedFileTargetResolution {
        canonical_target_path,
        anchor_path_end,
        anchor_is_external: !path_inside(workspace_root, &canonical_target_path[..anchor_path_end]),
        relative_components,
    }
}

fn normalize_absolute_path_into(
    scratch: &mut [u8],
    raw_path: &[u8],
) -> Result<usize, PathError> {
    if !is_absolute(raw_path) {
        return Err(PathError::InvalidPath);
    }
    if scratch.is_empty() {
        return Err(PathError::InvalidPath);
    }
    scratch[0] = b'/';
    let mut len: usize = 1;
    normalize_relative_path_part_into(scratch, &mut len, raw_path)?;
    Ok(len)
}

fn normalize_base_relative_path_into(
    scratch: &mut [u8],
    base_abs: &[u8],
    relative_path: &[u8],
) -> Result<usize, PathError> {
    if !is_absolute(base_abs) {
        return Err(PathError::InvalidPath);
    }
    if scratch.is_empty() {
        return Err(PathError::InvalidPath);
    }
    scratch[0] = b'/';
    let mut len: usize = 1;
    normalize_relative_path_part_into(scratch, &mut len, base_abs)?;
    normalize_relative_path_part_into(scratch, &mut len, relative_path)?;
    Ok(len)
}

fn normalize_relative_path_part_into(
    scratch: &mut [u8],
    path_len: &mut usize,
    raw_path: &[u8],
) -> Result<(), PathError> {
    let mut index: usize = 0;
    while index < raw_path.len() {
        while index < raw_path.len() && raw_path[index] == b'/' {
            index += 1;
        }
        if index >= raw_path.len() {
            return Ok(());
        }
        let start = index;
        while index < raw_path.len() && raw_path[index] != b'/' {
            if raw_path[index] == 0 {
                return Err(PathError::InvalidPath);
            }
            index += 1;
        }
        let component = &raw_path[start..index];
        if component.is_empty() || component == b"." {
            continue;
        } else if component == b".." {
            pop_normalized_path_component(scratch, path_len);
        } else {
            append_bounded_path_component(scratch, path_len, component)?;
        }
    }
    Ok(())
}

fn pop_normalized_path_component(path: &[u8], path_len: &mut usize) {
    if *path_len <= 1 {
        return;
    }
    let mut index = *path_len - 1;
    while index > 0 && path[index] != b'/' {
        index -= 1;
    }
    *path_len = if index == 0 { 1 } else { index };
}

fn append_bounded_path_component(
    scratch: &mut [u8],
    path_len: &mut usize,
    component: &[u8],
) -> Result<BoundedFileTargetComponent, PathError> {
    if component.is_empty() || component == b"." || component == b".." {
        return Err(PathError::InvalidPath);
    }

    let needs_separator = *path_len > 1;
    let required_len = *path_len + component.len() + usize::from(needs_separator);
    if required_len > scratch.len() {
        return Err(PathError::InvalidPath);
    }

    if needs_separator {
        scratch[*path_len] = b'/';
        *path_len += 1;
    }

    let start = *path_len;
    scratch[start..start + component.len()].copy_from_slice(component);
    *path_len += component.len();
    Ok(BoundedFileTargetComponent {
        start,
        end: *path_len,
    })
}

fn append_bounded_relative_component(
    component_scratch: &mut [BoundedFileTargetComponent],
    component_count: &mut usize,
    span: BoundedFileTargetComponent,
) -> Result<(), PathError> {
    if *component_count >= component_scratch.len() {
        return Err(PathError::TooManyPathComponents);
    }
    component_scratch[*component_count] = span;
    *component_count += 1;
    Ok(())
}

fn map_stat_file_error(err: std::io::Error) -> PathError {
    use std::io::ErrorKind::*;
    match err.kind() {
        NotFound => PathError::FileNotFound,
        PermissionDenied => PathError::PermissionDenied,
        NotADirectory => PathError::NotDir,
        AlreadyExists => PathError::PathAlreadyExists,
        _ => PathError::Unexpected,
    }
}

fn map_read_link_error(err: std::io::Error) -> PathError {
    use std::io::ErrorKind::*;
    match err.kind() {
        NotFound => PathError::FileNotFound,
        PermissionDenied => PathError::PermissionDenied,
        NotADirectory => PathError::NotDir,
        InvalidInput => PathError::NotLink,
        _ => PathError::Unexpected,
    }
}

fn map_io_error(err: std::io::Error) -> PathError {
    use std::io::ErrorKind::*;
    match err.kind() {
        NotFound => PathError::FileNotFound,
        PermissionDenied => PathError::PermissionDenied,
        NotADirectory => PathError::NotDir,
        AlreadyExists => PathError::PathAlreadyExists,
        InvalidInput => PathError::InvalidPath,
        TimedOut | WouldBlock => PathError::WouldBlock,
        _ => PathError::Unexpected,
    }
}

pub fn resolve_workspace_path_entry(
    workspace_root: &[u8],
    input_path: &[u8],
) -> Result<Vec<u8>, PathError> {
    let parts = resolve_workspace_path_entry_parts(workspace_root, input_path)?;
    let resolved_parent =
        resolve_workspace_path(workspace_root, &parts.parent, ResolveMode::Existing)?;
    Ok(join_bytes(&resolved_parent, &parts.basename))
}

pub fn resolve_workspace_path_entry_existing(
    workspace_root: &[u8],
    input_path: &[u8],
) -> Result<Vec<u8>, PathError> {
    let target = resolve_workspace_path_entry(workspace_root, input_path)?;
    fs::symlink_metadata(path_from_bytes(&target).ok_or(PathError::InvalidPath)?)
        .map_err(map_stat_file_error)?;
    Ok(target)
}

pub fn resolve_workspace_path_entry_create(
    workspace_root: &[u8],
    input_path: &[u8],
) -> Result<Vec<u8>, PathError> {
    let parts = resolve_workspace_path_entry_parts(workspace_root, input_path)?;
    let resolved_parent =
        resolve_workspace_path_entry_create_parent(workspace_root, &parts.parent)?;
    Ok(join_bytes(&resolved_parent, &parts.basename))
}

pub fn resolve_create_target_path(
    workspace_root: &[u8],
    absolute: &[u8],
) -> Result<Vec<u8>, PathError> {
    match canonicalize_bytes(absolute) {
        Ok(existing) => Ok(existing),
        Err(PathError::FileNotFound) => {
            resolve_create_target_from_nearest_existing(workspace_root, absolute)
        }
        Err(err) => Err(err),
    }
}

pub fn resolve_create_target_from_nearest_existing(
    workspace_root: &[u8],
    absolute: &[u8],
) -> Result<Vec<u8>, PathError> {
    ensure_create_target_symlinks_inside_workspace(workspace_root, absolute, 0)?;

    let mut missing_components: Vec<Vec<u8>> = Vec::new();
    let mut current = absolute.to_vec();
    loop {
        match canonicalize_bytes(&current) {
            Ok(existing) => {
                ensure_path_inside_workspace(workspace_root, &existing)?;
                let mut resolved = existing;
                for i in (0..missing_components.len()).rev() {
                    resolved = join_bytes(&resolved, &missing_components[i]);
                }
                return Ok(resolved);
            }
            Err(PathError::FileNotFound) => {
                let basename = basename(&current);
                if basename.is_empty() {
                    return Err(PathError::FileNotFound);
                }
                missing_components.push(basename.to_vec());
                let parent = dirname(&current).ok_or(PathError::FileNotFound)?;
                if parent == current {
                    return Err(PathError::FileNotFound);
                }
                current = parent.to_vec();
            }
            Err(err) => return Err(err),
        }
    }
}

fn resolve_absolute_create_target_path(
    absolute: &[u8],
    depth: usize,
) -> Result<Vec<u8>, PathError> {
    if depth > 16 {
        return Err(PathError::PathOutsideWorkspace);
    }
    match canonicalize_bytes(absolute) {
        Ok(existing) => Ok(existing),
        Err(PathError::FileNotFound) => {
            resolve_absolute_create_target_from_nearest_existing(absolute, depth)
        }
        Err(err) => Err(err),
    }
}

fn resolve_absolute_create_target_from_nearest_existing(
    absolute: &[u8],
    depth: usize,
) -> Result<Vec<u8>, PathError> {
    let mut missing_components: Vec<Vec<u8>> = Vec::new();
    let mut current = absolute.to_vec();
    loop {
        match canonicalize_bytes(&current) {
            Ok(existing) => {
                let mut resolved = existing;
                for i in (0..missing_components.len()).rev() {
                    resolved = join_bytes(&resolved, &missing_components[i]);
                }
                return Ok(resolved);
            }
            Err(PathError::FileNotFound) => {
                if let Some(target) = symlink_target_absolute(&current)? {
                    let mut resolved_target = target;
                    for i in (0..missing_components.len()).rev() {
                        resolved_target = join_bytes(&resolved_target, &missing_components[i]);
                    }
                    return resolve_absolute_create_target_path(&resolved_target, depth + 1);
                }

                let basename = basename(&current);
                if basename.is_empty() {
                    return Err(PathError::FileNotFound);
                }
                missing_components.push(basename.to_vec());
                let parent = dirname(&current).ok_or(PathError::FileNotFound)?;
                if parent == current {
                    return Err(PathError::FileNotFound);
                }
                current = parent.to_vec();
            }
            Err(err) => return Err(err),
        }
    }
}

fn ensure_create_target_symlinks_inside_workspace(
    workspace_root: &[u8],
    absolute: &[u8],
    depth: usize,
) -> Result<(), PathError> {
    if depth > 16 {
        return Err(PathError::PathOutsideWorkspace);
    }
    let mut current = absolute.to_vec();
    loop {
        match canonicalize_bytes(&current) {
            Ok(existing) => {
                ensure_path_inside_workspace(workspace_root, &existing)?;
                return Ok(());
            }
            Err(PathError::FileNotFound) => {
                if let Some(target) = symlink_target_absolute(&current)? {
                    ensure_create_target_symlinks_inside_workspace(
                        workspace_root,
                        &target,
                        depth + 1,
                    )?;
                }
                let basename = basename(&current);
                if basename.is_empty() {
                    return Err(PathError::FileNotFound);
                }
                let parent = dirname(&current).ok_or(PathError::FileNotFound)?;
                if parent == current {
                    return Err(PathError::FileNotFound);
                }
                current = parent.to_vec();
            }
            Err(err) => return Err(err),
        }
    }
}

fn symlink_target_absolute(path: &[u8]) -> Result<Option<Vec<u8>>, PathError> {
    match fs::symlink_metadata(path_from_bytes(path).ok_or(PathError::InvalidPath)?) {
        Ok(meta) if !meta.file_type().is_symlink() => Ok(None),
        Ok(_) => {
            let target = fs::read_link(path_from_bytes(path).ok_or(PathError::InvalidPath)?)
                .map_err(map_read_link_error)?;
            let target = bytes_from_path(&target);
            if is_absolute(&target) {
                Ok(Some(lexical_normalize_absolute(&target)))
            } else {
                let parent = dirname(path).ok_or(PathError::PathOutsideWorkspace)?;
                Ok(Some(lexical_join(parent, &target)))
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(map_io_error(err)),
    }
}

pub fn ensure_path_inside_workspace(
    workspace_root: &[u8],
    absolute: &[u8],
) -> Result<(), PathError> {
    if workspace_root.is_empty() {
        return Err(PathError::WorkspaceUnavailable);
    }
    if !path_inside(workspace_root, absolute) {
        return Err(PathError::PathOutsideWorkspace);
    }
    Ok(())
}

pub fn workspace_relative_path(
    workspace_root: &[u8],
    absolute: &[u8],
) -> Result<Vec<u8>, PathError> {
    if !path_inside(workspace_root, absolute) {
        return Ok(absolute.to_vec());
    }
    relative_path(workspace_root, absolute).or_else(|_| Ok(absolute.to_vec()))
}

pub fn ensure_parent_directories(path_abs: &[u8]) -> Result<(), PathError> {
    let Some(parent) = dirname(path_abs) else {
        return Ok(());
    };
    let relative_to_root = trim_start(parent, b'/');
    if relative_to_root.is_empty() {
        return Ok(());
    }
    fs::create_dir_all(path_from_bytes(parent).ok_or(PathError::InvalidPath)?)
        .map_err(map_io_error)?;
    Ok(())
}

struct PathEntryParts {
    parent: Vec<u8>,
    basename: Vec<u8>,
}

fn resolve_workspace_path_entry_parts(
    workspace_root: &[u8],
    input_path: &[u8],
) -> Result<PathEntryParts, PathError> {
    let cleaned = trim(input_path, PATH_ENTRY_WHITESPACE);
    if cleaned.is_empty() {
        return Err(PathError::InvalidPath);
    }
    if invalid_path_entry_basename(basename(cleaned)) {
        return Err(PathError::InvalidPath);
    }

    let absolute = if is_absolute(cleaned) {
        lexical_normalize_absolute(cleaned)
    } else {
        lexical_join(workspace_root, cleaned)
    };

    let parent = dirname(&absolute).ok_or(PathError::PathOutsideWorkspace)?;
    let basename = basename(&absolute);
    if invalid_path_entry_basename(basename) {
        return Err(PathError::InvalidPath);
    }

    Ok(PathEntryParts {
        parent: parent.to_vec(),
        basename: basename.to_vec(),
    })
}

fn resolve_workspace_path_entry_create_parent(
    workspace_root: &[u8],
    parent: &[u8],
) -> Result<Vec<u8>, PathError> {
    let resolved_parent = match canonicalize_bytes(parent) {
        Ok(existing) => existing,
        Err(PathError::FileNotFound) => {
            resolve_create_target_from_nearest_existing(workspace_root, parent)?
        }
        Err(err) => return Err(err),
    };

    ensure_path_inside_workspace(workspace_root, &resolved_parent)?;
    match fs::metadata(path_from_bytes(&resolved_parent).ok_or(PathError::InvalidPath)?) {
        Ok(meta) => {
            if !meta.is_dir() {
                return Err(PathError::NotDir);
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(map_io_error(err)),
    }
    Ok(resolved_parent)
}

fn invalid_path_entry_basename(basename: &[u8]) -> bool {
    basename.is_empty() || basename == b"." || basename == b".."
}

pub fn path_inside(root: &[u8], candidate: &[u8]) -> bool {
    if root == candidate {
        return true;
    }
    if !candidate.starts_with(root) {
        return false;
    }
    if root.is_empty() {
        return false;
    }
    if root[root.len() - 1] == b'/' {
        return true;
    }
    candidate.len() > root.len() && candidate[root.len()] == b'/'
}

fn lexical_normalize_absolute(raw: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; raw.len() + 1];
    out[0] = b'/';
    let mut len: usize = 1;
    let _ = normalize_relative_path_part_into(&mut out, &mut len, raw);
    out.truncate(len);
    out
}

fn lexical_join(base_abs: &[u8], relative: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; base_abs.len() + relative.len() + 1];
    out[0] = b'/';
    let mut len: usize = 1;
    let _ = normalize_relative_path_part_into(&mut out, &mut len, base_abs);
    let _ = normalize_relative_path_part_into(&mut out, &mut len, relative);
    out.truncate(len);
    out
}

fn canonicalize_bytes(path: &[u8]) -> Result<Vec<u8>, PathError> {
    let p = path_from_bytes(path).ok_or(PathError::InvalidPath)?;
    fs::canonicalize(p)
        .map(|p| bytes_from_path(&p))
        .map_err(map_io_error)
}

fn relative_path(from: &[u8], to: &[u8]) -> Result<Vec<u8>, ()> {
    let from_components: Vec<&[u8]> = from
        .split(|&b| b == b'/')
        .filter(|c| !c.is_empty())
        .collect();
    let to_components: Vec<&[u8]> = to
        .split(|&b| b == b'/')
        .filter(|c| !c.is_empty())
        .collect();

    let mut common = 0usize;
    while common < from_components.len()
        && common < to_components.len()
        && from_components[common] == to_components[common]
    {
        common += 1;
    }

    let mut out = Vec::new();
    for _ in common..from_components.len() {
        if !out.is_empty() {
            out.push(b'/');
        }
        out.extend_from_slice(b"..");
    }
    for component in &to_components[common..] {
        if !out.is_empty() {
            out.push(b'/');
        }
        out.extend_from_slice(component);
    }
    Ok(out)
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

fn trim_start<'a>(mut bytes: &'a [u8], pattern: u8) -> &'a [u8] {
    while bytes.first() == Some(&pattern) {
        bytes = &bytes[1..];
    }
    bytes
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

fn basename(path: &[u8]) -> &[u8] {
    if path.is_empty() {
        return path;
    }
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

fn dirname(path: &[u8]) -> Option<&[u8]> {
    if path.is_empty() {
        return None;
    }
    let mut end = path.len() - 1;
    while end != 0 && path[end] == b'/' {
        end -= 1;
    }
    while end != 0 && path[end] != b'/' {
        end -= 1;
    }
    if end == 0 {
        return if path[0] == b'/' { Some(b"/") } else { None };
    }
    Some(&path[..end])
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
        let path = std::env::temp_dir().join(format!("nyq-pathing-{}-{}", name, std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn write_test_file(root: &Path, rel: &str, content: &[u8]) {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, content).unwrap();
    }

    fn realpath(root: &Path, rel: &str) -> Vec<u8> {
        fs::canonicalize(root.join(rel)).unwrap().to_string_lossy().as_bytes().to_vec()
    }

    fn join_str(a: &[u8], b: &str) -> Vec<u8> {
        join_bytes(a, b.as_bytes())
    }

    fn path_buf(v: &[u8]) -> PathBuf {
        PathBuf::from(String::from_utf8(v.to_vec()).unwrap())
    }

    fn resolve_bounded<'a, 'b>(
        workspace: &[u8],
        input: &[u8],
        mode: ResolveMode,
        primary: &'a mut [u8],
        secondary: &'a mut [u8],
        components: &'b mut [BoundedFileTargetComponent],
    ) -> Result<BoundedFileTargetResolution<'a, 'b>, PathError> {
        resolve_file_mutation_target_bounded(workspace, input, mode, primary, secondary, components)
    }

    #[test]
    fn path_inside_preserves_exact_child_empty_root_and_prefix_semantics() {
        assert!(path_inside(b"/workspace", b"/workspace"));
        assert!(path_inside(b"/workspace", b"/workspace/file.txt"));
        assert!(path_inside(b"/workspace/", b"/workspace/file.txt"));
        assert!(path_inside(b"", b""));
        assert!(!path_inside(b"", b"/workspace"));
        assert!(!path_inside(b"/workspace", b"/workspace-evil/file.txt"));
    }

    #[test]
    fn bounded_resolver_anchors_existing_workspace_target_at_workspace_root() {
        let root = temp_root("anchored");
        fs::create_dir_all(root.join("workspace/src")).unwrap();
        write_test_file(&root, "workspace/src/file.txt", b"inside");
        let workspace = realpath(&root, "workspace");
        let expected = join_str(&workspace, "src/file.txt");

        let mut primary = [0u8; 4096];
        let mut secondary = [0u8; 4096];
        let mut components = [BoundedFileTargetComponent { start: 0, end: 0 }; 8];

        let resolved = resolve_bounded(
            &workspace,
            b"src/file.txt",
            ResolveMode::Existing,
            &mut primary,
            &mut secondary,
            &mut components,
        )
        .unwrap();

        assert_eq!(expected, resolved.canonical_target_path);
        assert_eq!(workspace.len(), resolved.anchor_path_end);
        assert!(!resolved.anchor_is_external);
        assert_eq!(2, resolved.relative_components.len());
        let span = resolved.relative_components[0];
        assert_eq!(b"src", &resolved.canonical_target_path[span.start..span.end]);
        let span = resolved.relative_components[1];
        assert_eq!(b"file.txt", &resolved.canonical_target_path[span.start..span.end]);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn bounded_resolver_anchors_existing_explicit_external_target_at_parent() {
        let root = temp_root("external-anchor");
        fs::create_dir_all(root.join("workspace")).unwrap();
        fs::create_dir_all(root.join("external")).unwrap();
        write_test_file(&root, "external/file.txt", b"outside");
        let workspace = realpath(&root, "workspace");
        let external_parent = realpath(&root, "external");
        let external = realpath(&root, "external/file.txt");

        let mut primary = [0u8; 4096];
        let mut secondary = [0u8; 4096];
        let mut components = [BoundedFileTargetComponent { start: 0, end: 0 }; 8];
        let resolved = resolve_bounded(
            &workspace,
            &external,
            ResolveMode::Existing,
            &mut primary,
            &mut secondary,
            &mut components,
        )
        .unwrap();

        assert_eq!(external, resolved.canonical_target_path);
        assert_eq!(external_parent.len(), resolved.anchor_path_end);
        assert!(resolved.anchor_is_external);
        assert_eq!(1, resolved.relative_components.len());
        let span = resolved.relative_components[0];
        assert_eq!(b"file.txt", &resolved.canonical_target_path[span.start..span.end]);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn bounded_resolver_resolves_missing_external_final_target_from_existing_parent() {
        let root = temp_root("external-missing");
        fs::create_dir_all(root.join("workspace")).unwrap();
        fs::create_dir_all(root.join("external")).unwrap();
        let workspace = realpath(&root, "workspace");
        let external = realpath(&root, "external");
        let expected = join_str(&external, "missing.txt");

        let mut primary = [0u8; 4096];
        let mut secondary = [0u8; 4096];
        let mut components = [BoundedFileTargetComponent { start: 0, end: 0 }; 8];
        let resolved = resolve_bounded(
            &workspace,
            &expected,
            ResolveMode::Create,
            &mut primary,
            &mut secondary,
            &mut components,
        )
        .unwrap();

        assert_eq!(expected, resolved.canonical_target_path);
        assert_eq!(external.len(), resolved.anchor_path_end);
        assert!(resolved.anchor_is_external);
        assert_eq!(1, resolved.relative_components.len());
        assert!(fs::symlink_metadata(String::from_utf8(expected.clone()).unwrap()).is_err());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn bounded_resolver_rejects_component_scratch_overflow() {
        let root = temp_root("component-overflow");
        fs::create_dir_all(root.join("workspace")).unwrap();
        let workspace = realpath(&root, "workspace");

        let mut primary = [0u8; 4096];
        let mut secondary = [0u8; 4096];
        let mut components = [BoundedFileTargetComponent { start: 0, end: 0 }; 2];
        let result = resolve_bounded(
            &workspace,
            b"missing/parent/file.txt",
            ResolveMode::Create,
            &mut primary,
            &mut secondary,
            &mut components,
        );
        assert_eq!(Err(PathError::TooManyPathComponents), result);
        assert!(fs::symlink_metadata(root.join("workspace/missing")).is_err());

        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn bounded_resolver_resolves_contained_intermediate_symlinks_and_preserves_final_symlink_entry() {
        let root = temp_root("symlinks");
        fs::create_dir_all(root.join("workspace/real")).unwrap();
        fs::create_dir_all(root.join("external")).unwrap();
        write_test_file(&root, "workspace/real/target.txt", b"inside");
        write_test_file(&root, "external/target.txt", b"outside");

        let workspace = realpath(&root, "workspace");
        let external = realpath(&root, "external");
        let expected_contained = join_str(&workspace, "real/target.txt");
        let external_file = join_str(&external, "target.txt");
        let expected_missing = join_str(&workspace, "real/missing-parent/target.txt");
        let expected_final_link = join_str(&workspace, "final-link.txt");

        std::os::unix::fs::symlink("real", root.join("workspace/contained-dir-link")).unwrap();
        std::os::unix::fs::symlink("real/missing-parent", root.join("workspace/missing-dir-link"))
            .unwrap();
        std::os::unix::fs::symlink(path_buf(&external), root.join("workspace/external-dir-link")).unwrap();
        std::os::unix::fs::symlink(path_buf(&external_file), root.join("workspace/final-link.txt")).unwrap();

        let mut primary = [0u8; 4096];
        let mut secondary = [0u8; 4096];
        let mut components = [BoundedFileTargetComponent { start: 0, end: 0 }; 8];

        let contained = resolve_bounded(
            &workspace,
            b"contained-dir-link/target.txt",
            ResolveMode::Existing,
            &mut primary,
            &mut secondary,
            &mut components,
        )
        .unwrap();
        assert_eq!(expected_contained, contained.canonical_target_path);
        assert_eq!(workspace.len(), contained.anchor_path_end);
        assert!(!contained.anchor_is_external);
        assert_eq!(2, contained.relative_components.len());

        let missing = resolve_bounded(
            &workspace,
            b"missing-dir-link/target.txt",
            ResolveMode::Create,
            &mut primary,
            &mut secondary,
            &mut components,
        )
        .unwrap();
        assert_eq!(expected_missing, missing.canonical_target_path);
        assert_eq!(workspace.len(), missing.anchor_path_end);
        assert!(!missing.anchor_is_external);
        assert_eq!(3, missing.relative_components.len());

        assert_eq!(
            Err(PathError::PathOutsideWorkspace),
            resolve_bounded(
                &workspace,
                b"external-dir-link/target.txt",
                ResolveMode::Existing,
                &mut primary,
                &mut secondary,
                &mut components,
            )
        );

        let resolved = resolve_bounded(
            &workspace,
            b"final-link.txt",
            ResolveMode::Existing,
            &mut primary,
            &mut secondary,
            &mut components,
        )
        .unwrap();
        assert_eq!(expected_final_link, resolved.canonical_target_path);
        assert_ne!(external_file, resolved.canonical_target_path);
        assert_eq!(workspace.len(), resolved.anchor_path_end);
        assert!(!resolved.anchor_is_external);
        assert_eq!(1, resolved.relative_components.len());

        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn bounded_resolver_reports_intermediate_symlink_loops() {
        let root = temp_root("symlink-loop");
        fs::create_dir_all(root.join("workspace")).unwrap();
        let workspace = realpath(&root, "workspace");
        std::os::unix::fs::symlink("loop-b", root.join("workspace/loop-a")).unwrap();
        std::os::unix::fs::symlink("loop-a", root.join("workspace/loop-b")).unwrap();

        let mut primary = [0u8; 4096];
        let mut secondary = [0u8; 4096];
        let mut components = [BoundedFileTargetComponent { start: 0, end: 0 }; 8];
        assert_eq!(
            Err(PathError::SymLinkLoop),
            resolve_bounded(
                &workspace,
                b"loop-a/target.txt",
                ResolveMode::Existing,
                &mut primary,
                &mut secondary,
                &mut components,
            )
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn bounded_resolver_does_not_allocate_while_resolving_from_fixed_scratch() {
        let root = temp_root("no-alloc");
        fs::create_dir_all(root.join("workspace")).unwrap();
        let workspace = realpath(&root, "workspace");

        let mut primary = [0u8; 4096];
        let mut secondary = [0u8; 4096];
        let mut components = [BoundedFileTargetComponent { start: 0, end: 0 }; 4];
        let resolved = resolve_bounded(
            &workspace,
            b"scratch-only.txt",
            ResolveMode::Create,
            &mut primary,
            &mut secondary,
            &mut components,
        )
        .unwrap();

        assert_eq!(1, resolved.relative_components.len());
        let span = resolved.relative_components[0];
        assert_eq!(b"scratch-only.txt", &resolved.canonical_target_path[span.start..span.end]);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn bounded_resolver_read_only_create_mode_does_not_create_target_or_parents() {
        let root = temp_root("read-only-create");
        fs::create_dir_all(root.join("workspace")).unwrap();
        fs::create_dir_all(root.join("external")).unwrap();
        let workspace = realpath(&root, "workspace");
        let external = realpath(&root, "external");
        let expected = join_str(&external, "new-parent/target.txt");

        let mut primary = [0u8; 4096];
        let mut secondary = [0u8; 4096];
        let mut components = [BoundedFileTargetComponent { start: 0, end: 0 }; 8];
        let resolved = resolve_bounded(
            &workspace,
            &expected,
            ResolveMode::Create,
            &mut primary,
            &mut secondary,
            &mut components,
        )
        .unwrap();

        assert_eq!(expected, resolved.canonical_target_path);
        assert_eq!(external.len(), resolved.anchor_path_end);
        assert_eq!(2, resolved.relative_components.len());
        assert!(fs::symlink_metadata(root.join("external/new-parent")).is_err());
        assert!(fs::symlink_metadata(String::from_utf8(expected).unwrap()).is_err());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn ensure_path_inside_workspace_allows_exact_root_and_child_paths() {
        assert!(ensure_path_inside_workspace(b"/home/user/project", b"/home/user/project").is_ok());
        assert!(ensure_path_inside_workspace(b"/home/user/project", b"/home/user/project/src/main.zig").is_ok());
    }

    #[test]
    fn ensure_path_inside_workspace_preserves_root_with_trailing_separator_behavior() {
        assert!(ensure_path_inside_workspace(b"/home/user/project/", b"/home/user/project/src/main.zig").is_ok());
        assert_eq!(
            Err(PathError::PathOutsideWorkspace),
            ensure_path_inside_workspace(b"/home/user/project/", b"/home/user/project-other/src/main.zig")
        );
    }

    #[test]
    fn ensure_path_inside_workspace_rejects_outside_paths_and_prefix_collisions() {
        assert_eq!(
            Err(PathError::PathOutsideWorkspace),
            ensure_path_inside_workspace(b"/home/user/project", b"/home/user/other/file.txt")
        );
        assert_eq!(
            Err(PathError::PathOutsideWorkspace),
            ensure_path_inside_workspace(b"/home/user/proj", b"/home/user/project/file.txt")
        );
    }

    #[test]
    fn ensure_path_inside_workspace_rejects_empty_workspace_roots() {
        assert_eq!(
            Err(PathError::WorkspaceUnavailable),
            ensure_path_inside_workspace(b"", b"/some/path")
        );
    }

    #[test]
    fn workspace_relative_path_returns_relative_paths_inside_workspace_and_absolute_paths_outside() {
        let relative = workspace_relative_path(b"/home/user/project", b"/home/user/project/src/main.zig").unwrap();
        assert_eq!(b"src/main.zig", relative.as_slice());
        let outside = workspace_relative_path(b"/home/user/project", b"/etc/passwd").unwrap();
        assert_eq!(b"/etc/passwd", outside.as_slice());
    }

    #[test]
    fn resolve_workspace_path_rejects_empty_and_whitespace_only_inputs() {
        assert_eq!(
            Err(PathError::InvalidPath),
            resolve_workspace_path(b"/tmp/ws", b"", ResolveMode::Create)
        );
        assert_eq!(
            Err(PathError::InvalidPath),
            resolve_workspace_path(b"/tmp/ws", b"   \t\n  ", ResolveMode::Create)
        );
    }

    #[test]
    fn resolve_workspace_path_rejects_absolute_paths_outside_the_workspace() {
        let root = temp_root("outside-abs");
        fs::create_dir_all(root.join("workspace")).unwrap();
        fs::create_dir_all(root.join("external")).unwrap();
        write_test_file(&root, "external/file.txt", b"outside");
        let workspace = realpath(&root, "workspace");
        let outside = realpath(&root, "external/file.txt");

        assert_eq!(
            Err(PathError::PathOutsideWorkspace),
            resolve_workspace_path(&workspace, &outside, ResolveMode::Existing)
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn resolve_workspace_or_external_path_allows_explicit_absolute_existing_paths_outside_the_workspace() {
        let root = temp_root("external-ok");
        fs::create_dir_all(root.join("workspace")).unwrap();
        fs::create_dir_all(root.join("external")).unwrap();
        write_test_file(&root, "external/file.txt", b"outside");
        let workspace = realpath(&root, "workspace");
        let outside = realpath(&root, "external/file.txt");

        let resolved =
            resolve_workspace_or_external_path_with_home(&workspace, &outside, None, ResolveMode::Existing)
                .unwrap();
        assert_eq!(outside, resolved);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn external_resolver_expands_home_and_canonicalizes_relative_and_absolute_aliases() {
        let root = temp_root("home-alias");
        fs::create_dir_all(root.join("workspace")).unwrap();
        fs::create_dir_all(root.join("home")).unwrap();
        fs::create_dir_all(root.join("external")).unwrap();
        write_test_file(&root, "home/fx-path-fixture.txt", b"home");
        write_test_file(&root, "external/fx-path-fixture.txt", b"external");
        let workspace = realpath(&root, "workspace");
        let home = realpath(&root, "home");
        let home_file = realpath(&root, "home/fx-path-fixture.txt");
        let external_file = realpath(&root, "external/fx-path-fixture.txt");

        let from_home = resolve_workspace_or_external_path_with_home(
            &workspace,
            b"~/fx-path-fixture.txt",
            Some(&home),
            ResolveMode::Existing,
        )
        .unwrap();
        let from_relative = resolve_workspace_or_external_path_with_home(
            &workspace,
            b"../external/fx-path-fixture.txt",
            Some(&home),
            ResolveMode::Existing,
        )
        .unwrap();
        let from_absolute = resolve_workspace_or_external_path_with_home(
            &workspace,
            &external_file,
            Some(&home),
            ResolveMode::Existing,
        )
        .unwrap();

        assert_eq!(home_file, from_home);
        assert_eq!(external_file, from_relative);
        assert_eq!(external_file, from_absolute);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn external_resolver_supports_create_targets_below_home_and_outside_workspace() {
        let root = temp_root("external-create");
        fs::create_dir_all(root.join("workspace")).unwrap();
        fs::create_dir_all(root.join("home")).unwrap();
        fs::create_dir_all(root.join("external")).unwrap();
        let workspace = realpath(&root, "workspace");
        let home = realpath(&root, "home");
        let external = realpath(&root, "external");
        let external_target = join_str(&external, "new/nested.txt");
        let home_target = join_str(&home, "new/nested.txt");

        let from_relative = resolve_workspace_or_external_path_with_home(
            &workspace,
            b"../external/new/nested.txt",
            Some(&home),
            ResolveMode::Create,
        )
        .unwrap();
        let from_absolute = resolve_workspace_or_external_path_with_home(
            &workspace,
            &external_target,
            Some(&home),
            ResolveMode::Create,
        )
        .unwrap();
        let from_home = resolve_workspace_or_external_path_with_home(
            &workspace,
            b"~/new/nested.txt",
            Some(&home),
            ResolveMode::Create,
        )
        .unwrap();

        assert_eq!(external_target, from_relative);
        assert_eq!(external_target, from_absolute);
        assert_eq!(home_target, from_home);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn external_resolver_handles_exact_home_root_and_normalized_home_escapes() {
        let root = temp_root("home-escapes");
        fs::create_dir_all(root.join("workspace")).unwrap();
        fs::create_dir_all(root.join("home")).unwrap();
        fs::create_dir_all(root.join("external")).unwrap();
        write_test_file(&root, "home/fx-path-fixture.txt", b"home");
        write_test_file(&root, "external/fx-path-fixture.txt", b"external");
        let workspace = realpath(&root, "workspace");
        let home = realpath(&root, "home");
        let home_file = realpath(&root, "home/fx-path-fixture.txt");
        let external_file = realpath(&root, "external/fx-path-fixture.txt");

        let exact_home =
            resolve_workspace_or_external_path_with_home(&workspace, b"~", Some(&home), ResolveMode::Existing)
                .unwrap();
        let slash_home =
            resolve_workspace_or_external_path_with_home(&workspace, b"~/", Some(&home), ResolveMode::Existing)
                .unwrap();
        let escaped_home = resolve_workspace_or_external_path_with_home(
            &workspace,
            b"~/../external/fx-path-fixture.txt",
            Some(&home),
            ResolveMode::Existing,
        )
        .unwrap();
        let redundant_separator = resolve_workspace_or_external_path_with_home(
            &workspace,
            b"~//fx-path-fixture.txt",
            Some(&home),
            ResolveMode::Existing,
        )
        .unwrap();
        let filesystem_root =
            resolve_workspace_or_external_path_with_home(&workspace, b"~", Some(b"/"), ResolveMode::Existing)
                .unwrap();

        assert_eq!(home, exact_home);
        assert_eq!(home, slash_home);
        assert_eq!(external_file, escaped_home);
        assert_eq!(home_file, redundant_separator);
        assert_eq!(b"/".as_slice(), filesystem_root.as_slice());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn external_resolver_rejects_invalid_home_inputs_and_unsupported_tilde_forms() {
        assert_eq!(
            Err(PathError::HomeNotSet),
            resolve_workspace_or_external_path_with_home(b"/tmp/workspace", b"~", None, ResolveMode::Existing)
        );
        assert_eq!(
            Err(PathError::InvalidPath),
            resolve_workspace_or_external_path_with_home(b"/tmp/workspace", b"~", Some(b""), ResolveMode::Existing)
        );
        assert_eq!(
            Err(PathError::InvalidPath),
            resolve_workspace_or_external_path_with_home(
                b"/tmp/workspace",
                b"~",
                Some(b"relative/home"),
                ResolveMode::Existing
            )
        );
        assert_eq!(
            Err(PathError::HomeNotSet),
            resolve_workspace_or_external_path_with_home(
                b"/tmp/workspace",
                b"~/file.txt",
                None,
                ResolveMode::Existing
            )
        );
        assert_eq!(
            Err(PathError::HomeNotSet),
            resolve_workspace_or_external_path_with_home(
                b"/tmp/workspace",
                b"~//file.txt",
                None,
                ResolveMode::Existing
            )
        );

        let invalid_paths: &[&[u8]] = &[
            b"~other",
            b"~other/file.txt",
            b"~notes.txt",
            b"~+",
            b"~-",
            b"~~",
            b"~\\file.txt",
            b"~ user/file.txt",
            b"",
            b" \t\r\n ",
        ];
        for input_path in invalid_paths {
            assert_eq!(
                Err(PathError::InvalidPath),
                resolve_workspace_or_external_path_with_home(
                    b"/tmp/workspace",
                    input_path,
                    Some(b"/tmp/home"),
                    ResolveMode::Existing
                )
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn external_resolver_follows_home_symlinks_but_rejects_workspace_relative_symlink_escapes() {
        let root = temp_root("symlink-escape");
        fs::create_dir_all(root.join("workspace")).unwrap();
        fs::create_dir_all(root.join("home")).unwrap();
        fs::create_dir_all(root.join("external")).unwrap();
        write_test_file(&root, "external/fx-path-fixture.txt", b"external");
        let workspace = realpath(&root, "workspace");
        let home = realpath(&root, "home");
        let external = realpath(&root, "external");
        let external_file = realpath(&root, "external/fx-path-fixture.txt");

        std::os::unix::fs::symlink(path_buf(&external), root.join("home/external-link")).unwrap();
        std::os::unix::fs::symlink(path_buf(&external), root.join("workspace/external-link")).unwrap();

        let from_home_link = resolve_workspace_or_external_path_with_home(
            &workspace,
            b"~/external-link/fx-path-fixture.txt",
            Some(&home),
            ResolveMode::Existing,
        )
        .unwrap();
        assert_eq!(external_file, from_home_link);

        assert_eq!(
            Err(PathError::PathOutsideWorkspace),
            resolve_workspace_or_external_path_with_home(
                &workspace,
                b"external-link/fx-path-fixture.txt",
                Some(&home),
                ResolveMode::Existing
            )
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn external_resolver_preserves_workspace_intent_for_lexical_reentry() {
        let root = temp_root("lexical-reentry");
        fs::create_dir_all(root.join("workspace")).unwrap();
        fs::create_dir_all(root.join("external")).unwrap();
        write_test_file(&root, "workspace/inside.txt", b"inside");
        let workspace = realpath(&root, "workspace");
        let inside_file = realpath(&root, "workspace/inside.txt");
        let create_target = join_str(&workspace, "new/nested.txt");

        let existing = resolve_workspace_or_external_path_with_home(
            &workspace,
            b"../workspace/inside.txt",
            None,
            ResolveMode::Existing,
        )
        .unwrap();
        let create = resolve_workspace_or_external_path_with_home(
            &workspace,
            b"../workspace/new/nested.txt",
            None,
            ResolveMode::Create,
        )
        .unwrap();

        assert_eq!(inside_file, existing);
        assert_eq!(create_target, create);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn external_resolver_normalizes_aliases_without_shell_expansion() {
        let root = temp_root("no-shell-expansion");
        fs::create_dir_all(root.join("workspace/$HOME")).unwrap();
        fs::create_dir_all(root.join("workspace/*")).unwrap();
        fs::create_dir_all(root.join("home")).unwrap();
        fs::create_dir_all(root.join("external")).unwrap();
        write_test_file(&root, "workspace/inside.txt", b"inside");
        write_test_file(&root, "workspace/$HOME/literal.txt", b"literal-home");
        write_test_file(&root, "workspace/*/literal.txt", b"literal-star");
        write_test_file(&root, "home/home.txt", b"home");
        write_test_file(&root, "external/outside.txt", b"outside");

        let workspace = realpath(&root, "workspace");
        let home = realpath(&root, "home");
        let home_with_separator = format!("{}/", String::from_utf8(home.clone()).unwrap());
        let inside = realpath(&root, "workspace/inside.txt");
        let literal_home = realpath(&root, "workspace/$HOME/literal.txt");
        let literal_star = realpath(&root, "workspace/*/literal.txt");
        let home_file = realpath(&root, "home/home.txt");
        let external_file = realpath(&root, "external/outside.txt");

        assert_eq!(
            inside,
            resolve_workspace_or_external_path_with_home(
                &workspace,
                b" ./nested/../inside.txt ",
                Some(&home),
                ResolveMode::Existing
            )
            .unwrap()
        );
        assert_eq!(
            external_file,
            resolve_workspace_or_external_path_with_home(
                &workspace,
                b"../external/./nested/../outside.txt",
                Some(&home),
                ResolveMode::Existing
            )
            .unwrap()
        );
        assert_eq!(
            home_file,
            resolve_workspace_or_external_path_with_home(
                &workspace,
                b"~///./home.txt",
                Some(home_with_separator.as_bytes()),
                ResolveMode::Existing
            )
            .unwrap()
        );
        assert_eq!(
            literal_home,
            resolve_workspace_or_external_path_with_home(
                &workspace,
                b"$HOME/literal.txt",
                Some(&home),
                ResolveMode::Existing
            )
            .unwrap()
        );
        assert_eq!(
            literal_star,
            resolve_workspace_or_external_path_with_home(
                &workspace,
                b"*/literal.txt",
                Some(&home),
                ResolveMode::Existing
            )
            .unwrap()
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn external_resolver_accepts_deep_lexical_traversal_to_filesystem_root() {
        let mut input = Vec::new();
        for _ in 0..128 {
            input.extend_from_slice(b"../");
        }
        let resolved = resolve_workspace_or_external_path_with_home(
            b"/tmp/fx/deep/workspace",
            &input,
            None,
            ResolveMode::Existing,
        )
        .unwrap();
        assert_eq!(b"/".as_slice(), resolved.as_slice());
    }

    #[cfg(unix)]
    #[test]
    fn external_resolver_canonicalizes_a_symlinked_home_root() {
        let root = temp_root("symlinked-home");
        fs::create_dir_all(root.join("workspace")).unwrap();
        fs::create_dir_all(root.join("home")).unwrap();
        write_test_file(&root, "home/file.txt", b"home");
        let home = realpath(&root, "home");
        std::os::unix::fs::symlink(path_buf(&home), root.join("home-link")).unwrap();

        let workspace = realpath(&root, "workspace");
        let root_real = realpath(&root, ".");
        let home_link = join_str(&root_real, "home-link");
        let home_file = realpath(&root, "home/file.txt");

        let resolved = resolve_workspace_or_external_path_with_home(
            &workspace,
            b"~/file.txt",
            Some(&home_link),
            ResolveMode::Existing,
        )
        .unwrap();
        assert_eq!(home_file, resolved);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn workspace_only_resolver_keeps_tilde_literal_and_rejects_relative_escapes() {
        let root = temp_root("workspace-tilde");
        fs::create_dir_all(root.join("workspace/~")).unwrap();
        fs::create_dir_all(root.join("external")).unwrap();
        write_test_file(&root, "workspace/~/literal.txt", b"literal");
        write_test_file(&root, "external/outside.txt", b"outside");
        let workspace = realpath(&root, "workspace");
        let literal = realpath(&root, "workspace/~/literal.txt");

        assert_eq!(
            literal,
            resolve_workspace_path(&workspace, b"~/literal.txt", ResolveMode::Existing).unwrap()
        );
        assert_eq!(
            Err(PathError::PathOutsideWorkspace),
            resolve_workspace_path(&workspace, b"../external/outside.txt", ResolveMode::Existing)
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn external_create_resolver_handles_absolute_outside_and_workspace_targets() {
        let root = temp_root("external-create-abs");
        fs::create_dir_all(root.join("workspace")).unwrap();
        fs::create_dir_all(root.join("external")).unwrap();
        write_test_file(&root, "external/existing.txt", b"outside");
        let workspace = realpath(&root, "workspace");
        let external = realpath(&root, "external");
        let existing_outside = realpath(&root, "external/existing.txt");
        let missing_outside = join_str(&external, "missing.txt");
        let missing_inside = join_str(&workspace, "missing/new.txt");

        assert_eq!(
            existing_outside,
            resolve_workspace_or_external_path_with_home(
                &workspace,
                &existing_outside,
                None,
                ResolveMode::Create
            )
            .unwrap()
        );
        assert_eq!(
            missing_outside,
            resolve_workspace_or_external_path_with_home(
                &workspace,
                &missing_outside,
                None,
                ResolveMode::Create
            )
            .unwrap()
        );
        assert_eq!(
            missing_inside,
            resolve_workspace_or_external_path_with_home(
                &workspace,
                &missing_inside,
                None,
                ResolveMode::Create
            )
            .unwrap()
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn resolve_workspace_or_external_create_path_allows_missing_external_targets() {
        let root = temp_root("external-create-missing");
        fs::create_dir_all(root.join("workspace")).unwrap();
        fs::create_dir_all(root.join("external")).unwrap();
        let workspace = realpath(&root, "workspace");
        let external = realpath(&root, "external");
        let missing_external = join_str(&external, "missing/new.txt");

        assert_eq!(
            missing_external,
            resolve_workspace_or_external_path_with_home(
                &workspace,
                &missing_external,
                None,
                ResolveMode::Create
            )
            .unwrap()
        );

        let relative_escape = resolve_workspace_or_external_path_with_home(
            &workspace,
            b"../external/escape.txt",
            None,
            ResolveMode::Create,
        )
        .unwrap();
        let expected_escape = join_str(&external, "escape.txt");
        assert_eq!(expected_escape, relative_escape);

        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn resolve_workspace_or_external_create_path_resolves_dangling_symlink_targets_outside_workspace() {
        let root = temp_root("dangling-external");
        fs::create_dir_all(root.join("workspace")).unwrap();
        fs::create_dir_all(root.join("allowed")).unwrap();
        fs::create_dir_all(root.join("actual")).unwrap();
        let workspace = realpath(&root, "workspace");
        let actual_dir = realpath(&root, "actual");
        let actual_missing = join_str(&actual_dir, "missing.txt");
        std::os::unix::fs::symlink(path_buf(&actual_missing), root.join("allowed/link.txt")).unwrap();

        let link_path = realpath(&root, "allowed");
        let requested = join_str(&link_path, "link.txt");
        let resolved = resolve_workspace_or_external_path_with_home(
            &workspace,
            &requested,
            None,
            ResolveMode::Create,
        )
        .unwrap();
        assert_eq!(actual_missing, resolved);

        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn resolve_workspace_path_create_rejects_dangling_final_symlink_escaping_workspace() {
        let root = temp_root("dangling-final");
        fs::create_dir_all(root.join("workspace")).unwrap();
        fs::create_dir_all(root.join("external")).unwrap();
        let workspace = realpath(&root, "workspace");
        let external = realpath(&root, "external");
        let external_missing = join_str(&external, "missing.txt");
        std::os::unix::fs::symlink(path_buf(&external_missing), root.join("workspace/outside-link")).unwrap();

        assert_eq!(
            Err(PathError::PathOutsideWorkspace),
            resolve_workspace_path(&workspace, b"outside-link", ResolveMode::Create)
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn resolve_workspace_path_create_rejects_nested_dangling_symlink_escape() {
        let root = temp_root("nested-dangling");
        fs::create_dir_all(root.join("workspace")).unwrap();
        fs::create_dir_all(root.join("external")).unwrap();
        let workspace = realpath(&root, "workspace");
        let external = realpath(&root, "external");
        std::os::unix::fs::symlink(path_buf(&external), root.join("workspace/inner")).unwrap();
        std::os::unix::fs::symlink("inner/missing.txt", root.join("workspace/outer-link")).unwrap();

        assert_eq!(
            Err(PathError::PathOutsideWorkspace),
            resolve_workspace_path(&workspace, b"outer-link", ResolveMode::Create)
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn resolve_workspace_path_create_preserves_dangling_symlink_targeting_workspace() {
        let root = temp_root("dangling-inside");
        fs::create_dir_all(root.join("workspace")).unwrap();
        let workspace = realpath(&root, "workspace");
        std::os::unix::fs::symlink("missing-inside.txt", root.join("workspace/inside-link")).unwrap();

        let resolved = resolve_workspace_path(&workspace, b"inside-link", ResolveMode::Create).unwrap();
        let expected = join_str(&workspace, "inside-link");
        assert_eq!(expected, resolved);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn resolve_workspace_path_entry_rejects_empty_dot_dotdot_and_collapsed_final_components() {
        let root = temp_root("entry-invalid");
        fs::create_dir_all(root.join("workspace/dir")).unwrap();
        let workspace = realpath(&root, "workspace");

        let invalid_paths: &[&[u8]] = &[b"", b"  \t", b".", b"..", b"/", b"dir/.", b"dir/.."];
        for input_path in invalid_paths {
            assert_eq!(
                Err(PathError::InvalidPath),
                resolve_workspace_path_entry(&workspace, input_path)
            );
        }

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn resolve_workspace_path_entry_rejects_absolute_paths_outside_the_workspace() {
        let root = temp_root("entry-outside");
        fs::create_dir_all(root.join("workspace")).unwrap();
        fs::create_dir_all(root.join("external")).unwrap();
        let workspace = realpath(&root, "workspace");
        let external = realpath(&root, "external");
        let outside_path = join_str(&external, "outside.txt");

        assert_eq!(
            Err(PathError::PathOutsideWorkspace),
            resolve_workspace_path_entry(&workspace, &outside_path)
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn resolve_workspace_path_entry_create_uses_nearest_existing_parent_for_missing_nested_paths() {
        let root = temp_root("entry-create");
        fs::create_dir_all(root.join("workspace")).unwrap();
        let workspace = realpath(&root, "workspace");

        let resolved =
            resolve_workspace_path_entry_create(&workspace, b"missing/deeper/final.txt").unwrap();
        let expected = join_str(&workspace, "missing/deeper/final.txt");

        assert!(ensure_path_inside_workspace(&workspace, &resolved).is_ok());
        assert_eq!(expected, resolved);

        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn resolve_workspace_path_entry_existing_preserves_the_final_symlink_entry() {
        let root = temp_root("entry-symlink");
        fs::create_dir_all(root.join("workspace")).unwrap();
        write_test_file(&root, "workspace/target.txt", b"target\n");
        std::os::unix::fs::symlink("target.txt", root.join("workspace/link.txt")).unwrap();
        let workspace = realpath(&root, "workspace");
        let expected_link = join_str(&workspace, "link.txt");
        let followed_target = fs::canonicalize(path_buf(&expected_link))
            .unwrap()
            .to_string_lossy()
            .as_bytes()
            .to_vec();

        let resolved = resolve_workspace_path_entry_existing(&workspace, b"link.txt").unwrap();
        assert_eq!(expected_link, resolved);
        assert_ne!(followed_target, resolved);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn ensure_parent_directories_creates_missing_absolute_parent_directories() {
        let root = temp_root("ensure-parents");
        let root_real = realpath(&root, ".");
        let file_path = join_str(&root_real, "a/b/created.txt");
        let parent_path = join_str(&root_real, "a/b");

        ensure_parent_directories(&file_path).unwrap();
        assert!(fs::metadata(String::from_utf8(parent_path).unwrap()).unwrap().is_dir());

        let _ = fs::remove_dir_all(&root);
    }
}
