//! Policy-neutral file-mutation target proof (ported 1:1 from
//! zig-reference/src/core/tooling/file_mutation_contract.zig).

use crate::core::output::diff;
use crate::core::shared::types::{ContentHash, PermissionGrant, RuleDecision};
use std::path::{Path, MAIN_SEPARATOR};

pub const max_file_target_components: usize = 256;
pub const max_file_target_proof_bytes: usize = 32 * 1024;
pub const max_external_scope_tail_bytes: usize = 4 * 1024;
const max_path_bytes: usize = 4096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PermissionTargetKind {
    Target,
    Parent,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileTargetDisposition {
    TargetEntry,
    ExistingParent,
    CreateParent,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileKind {
    Unknown,
    BlockDevice,
    CharacterDevice,
    Directory,
    NamedPipe,
    File,
    SymLink,
    UnixDomainSocket,
    Whiteout,
    Door,
    EventPort,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FileIdentity {
    pub device: u64,
    pub inode: u64,
    pub kind: FileKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PathSpan {
    pub start: usize,
    pub end: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TraversalScope {
    Workspace,
    External,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerifiedTraversalAnchor {
    pub scope: TraversalScope,
    pub path_end: usize,
    pub identity: FileIdentity,
    pub relative_components: Vec<PathSpan>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TraversalDirectoryState {
    Existing(FileIdentity),
    Create { permission_target_index: usize },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TraversalDirectory {
    pub component_index: usize,
    pub path_end: usize,
    pub state: TraversalDirectoryState,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EvaluatedPermissionTarget {
    pub kind: PermissionTargetKind,
    pub disposition: FileTargetDisposition,
    pub path_end: usize,
    pub expected_identity: Option<FileIdentity>,
    pub rule: RuleDecision,
    pub session_grant_allowed: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResolvedPermissionTarget {
    pub kind: PermissionTargetKind,
    pub disposition: FileTargetDisposition,
    pub path_end: usize,
    pub expected_identity: Option<FileIdentity>,
}

/// Owns the policy-neutral canonical target and filesystem identity proof for
/// one decoded write_file or edit_file call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedFileMutationTargets {
    pub canonical_target_path: Vec<u8>,
    pub anchor: VerifiedTraversalAnchor,
    pub traversal_directories: Vec<TraversalDirectory>,
    pub items: Vec<ResolvedPermissionTarget>,
    pub owns_memory: bool,
}

impl ResolvedFileMutationTargets {
    pub fn deinit(&mut self) {
        if !self.owns_memory {
            return;
        }
        self.canonical_target_path.clear();
        self.anchor.relative_components.clear();
        self.traversal_directories.clear();
        self.items.clear();
        self.owns_memory = false;
    }

    pub fn relinquish(&mut self) {
        self.owns_memory = false;
    }

    pub fn anchor_path(&self) -> &[u8] {
        &self.canonical_target_path[..self.anchor.path_end]
    }

    pub fn component(&self, span: PathSpan) -> &[u8] {
        &self.canonical_target_path[span.start..span.end]
    }

    pub fn traversal_path(&self, entry: TraversalDirectory) -> &[u8] {
        &self.canonical_target_path[..entry.path_end]
    }

    pub fn permission_path(&self, item: ResolvedPermissionTarget) -> &[u8] {
        &self.canonical_target_path[..item.path_end]
    }

    pub fn proof_valid(&self) -> bool {
        let canonical = self.canonical_target_path.as_slice();
        let components = self.anchor.relative_components.as_slice();
        let items = self.items.as_slice();
        if !target_path_shape_valid(
            self.anchor.path_end,
            self.anchor.identity,
            &self.traversal_directories,
            canonical,
            components,
        ) {
            return false;
        }
        if !permission_targets_valid(
            canonical.len(),
            self.anchor.path_end,
            self.anchor.identity,
            &self.traversal_directories,
            components,
            items.iter().copied().map(evaluated_from_resolved),
        ) {
            return false;
        }
        let Some(charge) = proof_charge(
            canonical.len(),
            components.len(),
            self.traversal_directories.len(),
            items.len(),
        ) else {
            return false;
        };
        charge <= max_file_target_proof_bytes
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PolicyEvaluatedFileTargets {
    pub canonical_target_path: Vec<u8>,
    pub anchor: VerifiedTraversalAnchor,
    pub traversal_directories: Vec<TraversalDirectory>,
    pub items: Vec<EvaluatedPermissionTarget>,
    pub prompt_required: bool,
}

impl PolicyEvaluatedFileTargets {
    pub fn deinit_owned(&mut self) {
        self.canonical_target_path.clear();
        self.anchor.relative_components.clear();
        self.traversal_directories.clear();
        self.items.clear();
    }

    pub fn anchor_path(&self) -> &[u8] {
        &self.canonical_target_path[..self.anchor.path_end]
    }

    pub fn component(&self, span: PathSpan) -> &[u8] {
        &self.canonical_target_path[span.start..span.end]
    }

    pub fn traversal_path(&self, entry: TraversalDirectory) -> &[u8] {
        &self.canonical_target_path[..entry.path_end]
    }

    pub fn permission_path(&self, item: EvaluatedPermissionTarget) -> &[u8] {
        &self.canonical_target_path[..item.path_end]
    }

    pub fn authorization_eql(a: &Self, b: &Self) -> bool {
        a.canonical_target_path == b.canonical_target_path
            && a.anchor.scope == b.anchor.scope
            && a.anchor.path_end == b.anchor.path_end
            && a.anchor.identity == b.anchor.identity
            && a.anchor.relative_components == b.anchor.relative_components
            && a.traversal_directories == b.traversal_directories
            && a.items == b.items
            && a.prompt_required == b.prompt_required
    }

    pub fn proof_valid(&self) -> bool {
        let canonical = self.canonical_target_path.as_slice();
        let components = self.anchor.relative_components.as_slice();
        let items = self.items.as_slice();
        if !target_path_shape_valid(
            self.anchor.path_end,
            self.anchor.identity,
            &self.traversal_directories,
            canonical,
            components,
        ) {
            return false;
        }
        if !permission_targets_valid(
            canonical.len(),
            self.anchor.path_end,
            self.anchor.identity,
            &self.traversal_directories,
            components,
            items.iter().copied(),
        ) {
            return false;
        }
        let Some(charge) = proof_charge(
            canonical.len(),
            components.len(),
            self.traversal_directories.len(),
            items.len(),
        ) else {
            return false;
        };
        charge <= max_file_target_proof_bytes
    }
}

pub fn proof_charge(
    canonical_path_len: usize,
    component_count: usize,
    traversal_count: usize,
    item_count: usize,
) -> Option<usize> {
    let mut total = canonical_path_len;
    total = checked_proof_charge(total, component_count, std::mem::size_of::<PathSpan>())?;
    total = checked_proof_charge(
        total,
        traversal_count,
        std::mem::size_of::<TraversalDirectory>(),
    )?;
    checked_proof_charge(
        total,
        item_count,
        std::mem::size_of::<EvaluatedPermissionTarget>(),
    )
}

fn target_path_shape_valid(
    anchor_path_end: usize,
    anchor_identity: FileIdentity,
    traversal_directories: &[TraversalDirectory],
    canonical: &[u8],
    components: &[PathSpan],
) -> bool {
    if canonical.is_empty() || canonical.len() > max_path_bytes {
        return false;
    }
    if !is_absolute(canonical) {
        return false;
    }
    if anchor_path_end == 0 || anchor_path_end > canonical.len() {
        return false;
    }
    if anchor_identity.kind != FileKind::Directory {
        return false;
    }
    if components.is_empty() || components.len() > max_file_target_components {
        return false;
    }
    if traversal_directories.len() != components.len() - 1 {
        return false;
    }

    let mut expected_start = anchor_path_end;
    if expected_start < canonical.len() && is_sep(canonical[expected_start]) {
        expected_start += 1;
    }
    for component in components {
        if component.start != expected_start {
            return false;
        }
        if component.end <= component.start || component.end > canonical.len() {
            return false;
        }
        expected_start = component.end;
        if expected_start < canonical.len() && is_sep(canonical[expected_start]) {
            expected_start += 1;
        }
    }
    components[components.len() - 1].end == canonical.len()
}

fn permission_targets_valid<I>(
    canonical_len: usize,
    anchor_path_end: usize,
    anchor_identity: FileIdentity,
    traversal_directories: &[TraversalDirectory],
    components: &[PathSpan],
    items: I,
) -> bool
where
    I: IntoIterator<Item = EvaluatedPermissionTarget>,
{
    let items: Vec<EvaluatedPermissionTarget> = items.into_iter().collect();
    if components.is_empty() {
        return false;
    }
    if traversal_directories.len() != components.len() - 1 {
        return false;
    }
    if items.len() < 2 || items.len() > max_file_target_components + 1 {
        return false;
    }
    if !permission_item_matches(
        items[0],
        PermissionTargetKind::Target,
        FileTargetDisposition::TargetEntry,
        canonical_len,
        items[0].expected_identity,
    ) {
        return false;
    }

    let mut used_items = [false; max_file_target_components + 1];
    used_items[0] = true;
    used_items[1] = true;

    let mut create_seen = false;
    for (index, entry) in traversal_directories.iter().enumerate() {
        if entry.component_index != index {
            return false;
        }
        if entry.path_end != components[index].end {
            return false;
        }
        match entry.state {
            TraversalDirectoryState::Existing(identity) => {
                if create_seen || identity.kind != FileKind::Directory {
                    return false;
                }
            }
            TraversalDirectoryState::Create {
                permission_target_index,
            } => {
                create_seen = true;
                if permission_target_index >= items.len() {
                    return false;
                }
                if used_items[permission_target_index] && permission_target_index != 1 {
                    return false;
                }
                let item = items[permission_target_index];
                if !permission_item_matches(
                    item,
                    PermissionTargetKind::Parent,
                    FileTargetDisposition::CreateParent,
                    entry.path_end,
                    None,
                ) {
                    return false;
                }
                used_items[permission_target_index] = true;
            }
        }
    }

    if traversal_directories.is_empty() {
        if !permission_item_matches(
            items[1],
            PermissionTargetKind::Parent,
            FileTargetDisposition::ExistingParent,
            anchor_path_end,
            Some(anchor_identity),
        ) {
            return false;
        }
    } else {
        let parent = traversal_directories[traversal_directories.len() - 1];
        match parent.state {
            TraversalDirectoryState::Existing(identity) => {
                if !permission_item_matches(
                    items[1],
                    PermissionTargetKind::Parent,
                    FileTargetDisposition::ExistingParent,
                    parent.path_end,
                    Some(identity),
                ) {
                    return false;
                }
            }
            TraversalDirectoryState::Create {
                permission_target_index,
            } => {
                if permission_target_index != 1 {
                    return false;
                }
                if !permission_item_matches(
                    items[1],
                    PermissionTargetKind::Parent,
                    FileTargetDisposition::CreateParent,
                    parent.path_end,
                    None,
                ) {
                    return false;
                }
            }
        }
    }
    used_items[..items.len()].iter().all(|used| *used)
}

fn permission_item_matches(
    item: EvaluatedPermissionTarget,
    kind: PermissionTargetKind,
    disposition: FileTargetDisposition,
    path_end: usize,
    expected_identity: Option<FileIdentity>,
) -> bool {
    item.kind == kind
        && item.disposition == disposition
        && item.path_end == path_end
        && optional_identity_eql(item.expected_identity, expected_identity)
}

fn checked_proof_charge(base: usize, count: usize, item_size: usize) -> Option<usize> {
    let bytes = count.checked_mul(item_size)?;
    base.checked_add(bytes)
}

fn optional_identity_eql(a: Option<FileIdentity>, b: Option<FileIdentity>) -> bool {
    match (a, b) {
        (Some(actual), Some(expected)) => actual == expected,
        (None, None) => true,
        _ => false,
    }
}

fn evaluated_from_resolved(item: ResolvedPermissionTarget) -> EvaluatedPermissionTarget {
    EvaluatedPermissionTarget {
        kind: item.kind,
        disposition: item.disposition,
        path_end: item.path_end,
        expected_identity: item.expected_identity,
        rule: RuleDecision::None,
        session_grant_allowed: false,
    }
}

fn is_absolute(canonical: &[u8]) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        Path::new(std::ffi::OsStr::from_bytes(canonical)).is_absolute()
    }
    #[cfg(not(unix))]
    {
        std::str::from_utf8(canonical)
            .ok()
            .is_some_and(|text| Path::new(text).is_absolute())
    }
}

fn is_sep(byte: u8) -> bool {
    byte == b'/' || (MAIN_SEPARATOR as u8 != b'/' && byte == MAIN_SEPARATOR as u8)
}

#[derive(Debug)]
pub enum FileTargetPolicyResult {
    TargetResolutionFailure(FileTargetResolutionFailure),
    PolicyDenied { target_index: usize },
    Evaluated(PolicyEvaluatedFileTargets),
}

#[derive(Debug)]
pub enum FileTargetPreparationResult {
    TargetResolutionFailure(FileTargetResolutionFailure),
    Prepared(ResolvedFileMutationTargets),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileTargetResolutionFailure {
    PathOutsideWorkspace,
    FileNotFound,
    HomeNotSet,
    InvalidPath,
    WorkspaceUnavailable,
    TooManyPathComponents,
    PolicyProofTooLarge,
    AccessDenied,
    NotDirectory,
    SymlinkLoop,
    NameTooLong,
    BadPathName,
    PathStateChanged,
    UnsupportedPathOperation,
    FilesystemUnavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Kind {
    Write,
    Edit,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileApprovalIntent {
    Mutation,
    EqualityDisclosure,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FileApprovalScope {
    WorkspaceFiles,
    ExternalTree(Vec<u8>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileGrantOffer {
    pub grants: Vec<PermissionGrant>,
    pub scope: FileApprovalScope,
}

pub fn is_tool_name(tool_name: &[u8]) -> bool {
    tool_name == b"write_file" || tool_name == b"edit_file"
}

pub fn result_is_noop(tool_name: &[u8], output: &[u8]) -> bool {
    is_tool_name(tool_name) && output.starts_with(b"No changes to ")
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WriteInput {
    pub path: Vec<u8>,
    pub content: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EditInput {
    pub path: Vec<u8>,
    pub old_string: Vec<u8>,
    pub new_string: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FileMutationInput {
    Write(WriteInput),
    Edit(EditInput),
}

impl FileMutationInput {
    pub fn path(&self) -> &[u8] {
        match self {
            FileMutationInput::Write(input) => &input.path,
            FileMutationInput::Edit(input) => &input.path,
        }
    }

    pub fn deinit(&mut self) {
        match self {
            FileMutationInput::Write(input) => {
                input.path.clear();
                input.content.clear();
            }
            FileMutationInput::Edit(input) => {
                input.path.clear();
                input.old_string.clear();
                input.new_string.clear();
            }
        }
    }

    pub fn kind(&self) -> Kind {
        match self {
            FileMutationInput::Write(_) => Kind::Write,
            FileMutationInput::Edit(_) => Kind::Edit,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Preimage {
    Absent,
    Present {
        content: Vec<u8>,
        content_hash: ContentHash,
    },
}

pub struct PreparedFileMutation {
    pub call_id: Vec<u8>,
    pub tool_name: Vec<u8>,
    pub arguments_hash: ContentHash,
    pub target_path: Vec<u8>,
    pub display_path: Vec<u8>,
    pub policy_targets: PolicyEvaluatedFileTargets,
    pub kind: Kind,
    pub preimage: Preimage,
    pub after_content: Vec<u8>,
    pub preview: diff::FileChangePreview,
    pub review: diff::FileReview,
    pub commit_token: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

pub fn prepared_mutation_is_noop(prepared: &PreparedFileMutation) -> bool {
    match &prepared.preimage {
        Preimage::Absent => false,
        Preimage::Present { content, .. } => content == &prepared.after_content,
    }
}

pub enum PrepareResult {
    Prepared(PreparedFileMutation),
    SemanticFailure(Vec<u8>),
}

pub struct FileExecutionAuthorization {
    pub input: FileMutationInput,
    pub policy_targets: PolicyEvaluatedFileTargets,
    pub prepared: Option<PreparedFileMutation>,
    pub grant_offer: Option<FileGrantOffer>,
    pub approval_intent: Option<FileApprovalIntent>,
    pub read_disclosure_required: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_target_proof_validation_accepts_and_rejects_owned_target_shapes() {
        let canonical = if MAIN_SEPARATOR == '\\' {
            br"C:\workspace\parent\note.txt".as_slice()
        } else {
            b"/workspace/parent/note.txt".as_slice()
        };
        let anchor_end = if MAIN_SEPARATOR == '\\' {
            br"C:\workspace".len()
        } else {
            b"/workspace".len()
        };
        let parent_start = anchor_end + 1;
        let parent_end = parent_start + b"parent".len();
        let target_start = parent_end + 1;
        let target_end = target_start + b"note.txt".len();

        let anchor_identity = FileIdentity {
            device: 1,
            inode: 10,
            kind: FileKind::Directory,
        };
        let parent_identity = FileIdentity {
            device: 1,
            inode: 11,
            kind: FileKind::Directory,
        };
        let target_identity = FileIdentity {
            device: 1,
            inode: 12,
            kind: FileKind::File,
        };
        let components = vec![
            PathSpan {
                start: parent_start,
                end: parent_end,
            },
            PathSpan {
                start: target_start,
                end: target_end,
            },
        ];
        let traversal = vec![TraversalDirectory {
            component_index: 0,
            path_end: parent_end,
            state: TraversalDirectoryState::Existing(parent_identity),
        }];
        let items = vec![
            EvaluatedPermissionTarget {
                kind: PermissionTargetKind::Target,
                disposition: FileTargetDisposition::TargetEntry,
                path_end: canonical.len(),
                expected_identity: Some(target_identity),
                rule: RuleDecision::Allow,
                session_grant_allowed: false,
            },
            EvaluatedPermissionTarget {
                kind: PermissionTargetKind::Parent,
                disposition: FileTargetDisposition::ExistingParent,
                path_end: parent_end,
                expected_identity: Some(parent_identity),
                rule: RuleDecision::Allow,
                session_grant_allowed: false,
            },
        ];
        let policy = PolicyEvaluatedFileTargets {
            canonical_target_path: canonical.to_vec(),
            anchor: VerifiedTraversalAnchor {
                scope: TraversalScope::Workspace,
                path_end: anchor_end,
                identity: anchor_identity,
                relative_components: components.clone(),
            },
            traversal_directories: traversal,
            items: items.clone(),
            prompt_required: false,
        };

        assert!(policy.proof_valid());

        let mut corrupted_policy = policy.clone();
        corrupted_policy.anchor.relative_components[0].start = anchor_end;
        assert!(!corrupted_policy.proof_valid());

        let mut unused_item_policy = policy;
        unused_item_policy.items = vec![items[0], items[1], items[0]];
        assert!(!unused_item_policy.proof_valid());
    }

    #[test]
    fn file_target_proof_charge_rejects_arithmetic_overflow() {
        assert_eq!(proof_charge(usize::MAX, 1, 0, 0), None);
    }

    #[test]
    fn persisted_result_no_op_requires_a_file_mutation_tool_and_canonical_output() {
        assert!(result_is_noop(b"write_file", b"No changes to note.txt"));
        assert!(result_is_noop(b"edit_file", b"No changes to note.txt"));
        assert!(!result_is_noop(b"read_file", b"No changes to note.txt"));
        assert!(!result_is_noop(b"write_file", b"Wrote note.txt"));
    }

    #[test]
    fn file_mutation_input_owns_write_and_edit_fields() {
        let write_path = b"note.txt";
        let write_content = b"hello\n";
        let mut write_input = FileMutationInput::Write(WriteInput {
            path: write_path.to_vec(),
            content: write_content.to_vec(),
        });
        assert_eq!(write_input.path(), write_path);
        match &write_input {
            FileMutationInput::Write(input) => {
                assert_eq!(input.content, write_content);
                assert!(!std::ptr::eq(input.path.as_ptr(), write_path.as_ptr()));
                assert!(!std::ptr::eq(input.content.as_ptr(), write_content.as_ptr()));
            }
            FileMutationInput::Edit(_) => panic!("expected write"),
        }
        write_input.deinit();

        let edit_path = b"note.txt";
        let old_string = b"hello";
        let new_string = b"goodbye";
        let mut edit_input = FileMutationInput::Edit(EditInput {
            path: edit_path.to_vec(),
            old_string: old_string.to_vec(),
            new_string: new_string.to_vec(),
        });
        assert_eq!(edit_input.path(), edit_path);
        match &edit_input {
            FileMutationInput::Edit(input) => {
                assert_eq!(input.old_string, old_string);
                assert_eq!(input.new_string, new_string);
                assert!(!std::ptr::eq(input.path.as_ptr(), edit_path.as_ptr()));
                assert!(!std::ptr::eq(input.old_string.as_ptr(), old_string.as_ptr()));
                assert!(!std::ptr::eq(input.new_string.as_ptr(), new_string.as_ptr()));
            }
            FileMutationInput::Write(_) => panic!("expected edit"),
        }
        edit_input.deinit();
    }
}
