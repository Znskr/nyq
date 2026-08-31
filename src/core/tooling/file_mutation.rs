//! File mutation prepare/apply (ported 1:1 from
//! zig-reference/src/core/tooling/file_mutation.zig).

use crate::core::output::diff::{
    self, BoundedPreviewError, FileChangePreview, FileReview, FileReviewError, PreviewLine,
    PreviewOp, MAX_REQUEST_PROJECTION_BYTES,
};
use crate::core::shared::io;
use crate::core::shared::text_utils::{self, PathTailError};
use crate::core::shared::types::{ResolveMode, ToolCall};
use crate::core::tooling::file_mutation_contract::{
    self, FileExecutionAuthorization, FileIdentity, FileKind, FileMutationInput, Kind,
    PolicyEvaluatedFileTargets, Preimage, PrepareResult, PreparedFileMutation,
    TraversalDirectoryState,
};
use crate::core::workspace::pathing::{self, PathError};
use crate::core::workspace::read_tracker;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

pub const TARGET_MISMATCH_MESSAGE: &[u8] =
    b"file mutation preparation failed: approved target no longer matches the call";
pub const IDENTITY_CHANGED_MESSAGE: &[u8] =
    b"file mutation preparation failed: approved filesystem identity changed";
const preview_too_large_message: &[u8] =
    b"file mutation preparation failed: diff preview exceeds preparation limits";
const total_content_too_large_message: &[u8] =
    b"file mutation preparation failed: content proof exceeds the 8 MiB preparation limit";

const max_content_bytes: usize = 4 * 1024 * 1024;
const max_total_content_bytes: usize = 8 * 1024 * 1024;
const max_path_bytes: usize = 4096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ParentCleanupResidueReason {
    NotEmpty,
    IdentityChanged,
    RemoveFailed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CreatedParentResidue {
    pub component_index: usize,
    pub path_end: usize,
    pub permission_target_index: usize,
    pub created_identity: FileIdentity,
    pub observed_identity: Option<FileIdentity>,
    pub reason: ParentCleanupResidueReason,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApplyRejectionReason {
    BindingMismatch,
    StalePreimage,
    Cancelled,
    TraversalChanged,
    StagedSourceChanged,
    IoFailure,
}

#[derive(Debug)]
pub struct ApplyRejection {
    pub reason: ApplyRejectionReason,
    pub created_parent_residue: Vec<CreatedParentResidue>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChangeTrackerHandoff {
    pub kind: Kind,
    pub raw_path: Vec<u8>,
    pub previous_content: Option<Vec<u8>>,
    pub committed_at_ms: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FullViewSnapshot {
    pub after_content: Vec<u8>,
    pub lifecycle_id: crate::core::shared::types::ToolLifecycleId,
}

pub struct CommittedFileHandoff {
    pub preview: FileChangePreview,
    pub tracker: ChangeTrackerHandoff,
    pub full_view: Option<FullViewSnapshot>,
}

impl CommittedFileHandoff {
    pub fn init(preview: FileChangePreview, tracker: ChangeTrackerHandoff) -> Self {
        Self {
            preview,
            tracker,
            full_view: None,
        }
    }

    pub fn deinit(&mut self) {
        self.preview.lines.clear();
        self.preview.path.clear();
        self.tracker.raw_path.clear();
        self.tracker.previous_content = None;
        self.full_view = None;
    }

    pub fn attach_full_view(
        &mut self,
        after_content: &[u8],
        lifecycle_id: crate::core::shared::types::ToolLifecycleId,
    ) {
        self.full_view = Some(FullViewSnapshot {
            after_content: after_content.to_vec(),
            lifecycle_id: crate::core::shared::types::ToolLifecycleId {
                turn_id: lifecycle_id.turn_id,
                call_id: lifecycle_id.call_id.clone(),
            },
        });
    }
}

pub enum ApplyResult {
    Rejected(ApplyRejection),
    Committed(CommittedFileHandoff),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileMutationPrepareError {
    OutOfMemory,
    Canceled,
    Unexpected,
    SystemResources,
    ProcessFdQuotaExceeded,
    SystemFdQuotaExceeded,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileMutationApplyError {
    OutOfMemory,
    ResultContractViolation,
}

static TARGET_RESOLUTION_CHECK_COUNT: AtomicUsize = AtomicUsize::new(0);

pub fn reset_target_resolution_check_count_for_test() {
    TARGET_RESOLUTION_CHECK_COUNT.store(0, Ordering::SeqCst);
}

pub fn target_resolution_check_count_for_test() -> usize {
    TARGET_RESOLUTION_CHECK_COUNT.load(Ordering::SeqCst)
}

pub fn prepare(
    call: &ToolCall,
    input: &FileMutationInput,
    policy_targets: PolicyEvaluatedFileTargets,
) -> Result<PrepareResult, FileMutationPrepareError> {
    prepare_internal(call, input, policy_targets, true)
}

pub fn prepare_resolved_target(
    call: &ToolCall,
    input: &FileMutationInput,
    policy_targets: PolicyEvaluatedFileTargets,
) -> Result<PrepareResult, FileMutationPrepareError> {
    prepare_internal(call, input, policy_targets, false)
}

fn prepare_internal(
    call: &ToolCall,
    input: &FileMutationInput,
    policy_targets: PolicyEvaluatedFileTargets,
    verify_target_resolution: bool,
) -> Result<PrepareResult, FileMutationPrepareError> {
    if policy_identity_binding_mismatch(&policy_targets) {
        return Ok(PrepareResult::SemanticFailure(
            IDENTITY_CHANGED_MESSAGE.to_vec(),
        ));
    }
    if !policy_targets.proof_valid() {
        return Ok(PrepareResult::SemanticFailure(
            TARGET_MISMATCH_MESSAGE.to_vec(),
        ));
    }
    if verify_target_resolution && !resolved_target_matches(input, &policy_targets)? {
        return Ok(PrepareResult::SemanticFailure(
            TARGET_MISMATCH_MESSAGE.to_vec(),
        ));
    }

    let preimage = match verify_and_read_preimage(input, &policy_targets)? {
        PreimageResult::Preimage(preimage) => preimage,
        PreimageResult::SemanticFailure(message) => {
            return Ok(PrepareResult::SemanticFailure(message));
        }
    };
    let after_content = match derive_postimage(input, &preimage)? {
        PostimageResult::Content(content) => content,
        PostimageResult::SemanticFailure(message) => {
            return Ok(PrepareResult::SemanticFailure(message));
        }
    };
    let before_content: &[u8] = match &preimage {
        Preimage::Absent => b"",
        Preimage::Present { content, .. } => content,
    };
    let total_content_bytes = match before_content.len().checked_add(after_content.len()) {
        Some(total) => total,
        None => {
            return Ok(PrepareResult::SemanticFailure(
                total_content_too_large_message.to_vec(),
            ));
        }
    };
    if total_content_bytes > max_total_content_bytes {
        return Ok(PrepareResult::SemanticFailure(
            total_content_too_large_message.to_vec(),
        ));
    }

    let approval_path = match policy_targets.anchor.scope {
        file_mutation_contract::TraversalScope::External => {
            policy_targets.canonical_target_path.as_slice()
        }
        file_mutation_contract::TraversalScope::Workspace => input.path(),
    };
    let encoded_path = match text_utils::encode_terminal_safe_path_tail(
        approval_path,
        diff::MAX_ENCODED_PATH_BYTES,
    ) {
        Ok(encoded) => encoded,
        Err(PathTailError::PathBasenameTooLong) => {
            return Ok(PrepareResult::SemanticFailure(
                preview_too_large_message.to_vec(),
            ));
        }
    };
    let preview = match build_preview(&encoded_path, &preimage, before_content, &after_content)? {
        PreviewResult::Preview(preview) => preview,
        PreviewResult::SemanticFailure(message) => {
            return Ok(PrepareResult::SemanticFailure(message));
        }
    };
    let review = match FileReview::init(before_content, &after_content) {
        Ok(review) => review,
        Err(FileReviewError::OutOfMemory) => return Err(FileMutationPrepareError::OutOfMemory),
        Err(FileReviewError::ArithmeticOverflow) => {
            return Ok(PrepareResult::SemanticFailure(
                preview_too_large_message.to_vec(),
            ));
        }
    };

    Ok(PrepareResult::Prepared(PreparedFileMutation {
        call_id: call.id.clone(),
        tool_name: call.name.clone(),
        arguments_hash: read_tracker::content_hash(&call.arguments_json),
        target_path: policy_targets.canonical_target_path.clone(),
        display_path: encoded_path.bytes,
        policy_targets,
        kind: input.kind(),
        preimage,
        after_content,
        preview,
        review,
        commit_token: Arc::new(AtomicBool::new(false)),
    }))
}

enum PreimageResult {
    Preimage(Preimage),
    SemanticFailure(Vec<u8>),
}

enum PostimageResult {
    Content(Vec<u8>),
    SemanticFailure(Vec<u8>),
}

enum PreviewResult {
    Preview(FileChangePreview),
    SemanticFailure(Vec<u8>),
}

pub(crate) struct ApplyTestControls<'a> {
    after_write_chunk: Option<&'a mut dyn FnMut(usize) -> Result<(), ApplyRejectionReason>>,
    after_stage: Option<&'a mut dyn FnMut(&Path, &[u8]) -> Result<(), ApplyRejectionReason>>,
    after_final_preimage_read:
        Option<&'a mut dyn FnMut(&Path, &[u8]) -> Result<(), ApplyRejectionReason>>,
    after_final_validation: Option<&'a mut dyn FnMut() -> Result<(), ApplyRejectionReason>>,
}

impl Default for ApplyTestControls<'static> {
    fn default() -> Self {
        Self {
            after_write_chunk: None,
            after_stage: None,
            after_final_preimage_read: None,
            after_final_validation: None,
        }
    }
}

/// Open directory descriptor used by apply so rename/open follow the inode,
/// matching Zig `std.Io.Dir` rather than a pathname that can be retargeted.
struct DirFd {
    file: File,
    path: PathBuf,
}

#[cfg(unix)]
mod posix {
    use std::ffi::c_char;

    pub const O_RDONLY: i32 = 0;
    pub const O_RDWR: i32 = 2;
    pub const O_CREAT: i32 = 0o100;
    pub const O_EXCL: i32 = 0o200;
    pub const O_DIRECTORY: i32 = 0o200000;
    pub const O_NOFOLLOW: i32 = 0o400000;
    pub const O_CLOEXEC: i32 = 0o2000000;
    pub const EINTR: i32 = 4;

    unsafe extern "C" {
        pub fn openat(dirfd: i32, pathname: *const c_char, flags: i32, mode: u32) -> i32;
        pub fn renameat(
            olddirfd: i32,
            oldpath: *const c_char,
            newdirfd: i32,
            newpath: *const c_char,
        ) -> i32;
        pub fn unlinkat(dirfd: i32, pathname: *const c_char, flags: i32) -> i32;
        pub fn mkdirat(dirfd: i32, pathname: *const c_char, mode: u32) -> i32;
    }
}

impl DirFd {
    fn open_absolute(path: &Path) -> std::io::Result<Self> {
        let mut opts = OpenOptions::new();
        opts.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.custom_flags(posix::O_DIRECTORY | posix::O_NOFOLLOW | posix::O_CLOEXEC);
        }
        Ok(Self {
            file: opts.open(path)?,
            path: path.to_path_buf(),
        })
    }

    fn try_clone(&self) -> std::io::Result<Self> {
        Ok(Self {
            file: self.file.try_clone()?,
            path: self.path.clone(),
        })
    }

    fn open_child(&self, name: &[u8]) -> std::io::Result<Self> {
        #[cfg(unix)]
        {
            let fd = posix_openat(
                self,
                name,
                posix::O_RDONLY | posix::O_DIRECTORY | posix::O_NOFOLLOW | posix::O_CLOEXEC,
                0,
            )?;
            use std::os::unix::io::FromRawFd;
            return Ok(Self {
                file: unsafe { File::from_raw_fd(fd) },
                path: join_dir(&self.path, name),
            });
        }
        #[cfg(not(unix))]
        {
            Self::open_absolute(&join_dir(&self.path, name))
        }
    }

    fn mkdir_child(&self, name: &[u8]) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            let c_path = posix_c_name(name)?;
            posix_retry(|| unsafe { posix::mkdirat(self.raw_fd(), c_path.as_ptr(), 0o777) })?;
            return Ok(());
        }
        #[cfg(not(unix))]
        {
            fs::create_dir(join_dir(&self.path, name))
        }
    }

    fn open_child_file(&self, name: &[u8]) -> std::io::Result<File> {
        #[cfg(unix)]
        {
            let fd = posix_openat(
                self,
                name,
                posix::O_RDONLY | posix::O_NOFOLLOW | posix::O_CLOEXEC,
                0,
            )?;
            use std::os::unix::io::FromRawFd;
            return Ok(unsafe { File::from_raw_fd(fd) });
        }
        #[cfg(not(unix))]
        {
            File::open(join_dir(&self.path, name))
        }
    }

    fn create_exclusive_child(&self, name: &[u8], mode: Option<u32>) -> std::io::Result<File> {
        #[cfg(unix)]
        {
            let fd = posix_openat(
                self,
                name,
                posix::O_RDWR
                    | posix::O_CREAT
                    | posix::O_EXCL
                    | posix::O_NOFOLLOW
                    | posix::O_CLOEXEC,
                mode.unwrap_or(0o666),
            )?;
            use std::os::unix::io::FromRawFd;
            return Ok(unsafe { File::from_raw_fd(fd) });
        }
        #[cfg(not(unix))]
        {
            create_exclusive_file(&join_dir(&self.path, name), mode)
        }
    }

    fn rename_child(&self, from: &[u8], to: &[u8]) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            let old = posix_c_name(from)?;
            let new = posix_c_name(to)?;
            posix_retry(|| unsafe {
                posix::renameat(self.raw_fd(), old.as_ptr(), self.raw_fd(), new.as_ptr())
            })?;
            return Ok(());
        }
        #[cfg(not(unix))]
        {
            fs::rename(join_dir(&self.path, from), join_dir(&self.path, to))
        }
    }

    fn unlink_child(&self, name: &[u8]) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            let c_path = posix_c_name(name)?;
            posix_retry(|| unsafe { posix::unlinkat(self.raw_fd(), c_path.as_ptr(), 0) })?;
            return Ok(());
        }
        #[cfg(not(unix))]
        {
            fs::remove_file(join_dir(&self.path, name))
        }
    }

    fn identity(&self) -> std::io::Result<FileIdentity> {
        Ok(identity_from_metadata(&self.file.metadata()?))
    }

    #[cfg(unix)]
    fn raw_fd(&self) -> i32 {
        use std::os::unix::io::AsRawFd;
        self.file.as_raw_fd()
    }
}

#[cfg(unix)]
fn posix_c_name(name: &[u8]) -> std::io::Result<std::ffi::CString> {
    std::ffi::CString::new(name).map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))
}

#[cfg(unix)]
fn posix_retry(mut f: impl FnMut() -> i32) -> std::io::Result<i32> {
    loop {
        let rc = f();
        if rc >= 0 {
            return Ok(rc);
        }
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(posix::EINTR) {
            continue;
        }
        return Err(err);
    }
}

#[cfg(unix)]
fn posix_openat(dir: &DirFd, name: &[u8], flags: i32, mode: u32) -> std::io::Result<i32> {
    let c_path = posix_c_name(name)?;
    posix_retry(|| unsafe { posix::openat(dir.raw_fd(), c_path.as_ptr(), flags, mode) })
}

struct CreatedParentRecord {
    component_index: usize,
    path_end: usize,
    permission_target_index: usize,
    identity: FileIdentity,
}

enum PreimageValidation {
    Valid(Option<u32>),
    Stale,
    IoFailure,
}

enum TraversalValidation {
    Parent(DirFd),
    Changed,
    IoFailure,
}

enum StagedSourceValidation {
    Valid,
    Changed,
    IoFailure,
}

enum TargetEntryValidation {
    Valid,
    Changed,
    IoFailure,
}

struct ApplyResources {
    policy: PolicyEvaluatedFileTargets,
    created: Vec<CreatedParentRecord>,
    realized: Vec<FileIdentity>,
    residue_storage: Vec<CreatedParentResidue>,
    parent: Option<DirFd>,
    temp_name: Option<Vec<u8>>,
    temp_identity: Option<FileIdentity>,
}

impl ApplyResources {
    fn reject(&mut self, reason: ApplyRejectionReason) -> ApplyResult {
        self.cleanup_temp();
        self.parent = None;
        let residues = self.cleanup_created_parents();
        ApplyResult::Rejected(ApplyRejection {
            reason,
            created_parent_residue: residues,
        })
    }

    fn cleanup_temp(&mut self) {
        let Some(parent) = self.parent.as_ref() else {
            return;
        };
        let Some(temp_name) = self.temp_name.as_deref() else {
            return;
        };
        let Some(expected_identity) = self.temp_identity else {
            return;
        };
        let Some(actual_identity) = observed_file_identity(parent, temp_name) else {
            return;
        };
        if !identity_eql(actual_identity, expected_identity) {
            return;
        }
        let _ = parent.unlink_child(temp_name);
        self.temp_name = None;
        self.temp_identity = None;
    }

    fn cleanup_created_parents(&mut self) -> Vec<CreatedParentResidue> {
        let mut residues = Vec::new();
        for created in self.created.iter().rev() {
            if let Some(residue) =
                cleanup_created_parent(&self.policy, &self.realized, created)
            {
                residues.push(residue);
            }
        }
        residues
    }
}

pub fn apply(
    call: &ToolCall,
    authorization: FileExecutionAuthorization,
    cancel_flag: &AtomicBool,
) -> Result<ApplyResult, FileMutationApplyError> {
    apply_with_test_controls(call, authorization, cancel_flag, ApplyTestControls::default())
}

pub(crate) fn apply_with_test_controls(
    call: &ToolCall,
    mut authorization: FileExecutionAuthorization,
    cancel_flag: &AtomicBool,
    mut test_controls: ApplyTestControls<'_>,
) -> Result<ApplyResult, FileMutationApplyError> {
    let prepared = match authorization.prepared.take() {
        Some(prepared) => prepared,
        None => return Ok(rejected_without_residue(ApplyRejectionReason::BindingMismatch)),
    };
    if !apply_binding_valid(call, &authorization.policy_targets, &prepared) {
        return Ok(rejected_without_residue(ApplyRejectionReason::BindingMismatch));
    }
    if prepared.commit_token.load(Ordering::SeqCst) {
        return Err(FileMutationApplyError::ResultContractViolation);
    }
    if cancel_flag.load(Ordering::SeqCst) {
        return Ok(rejected_without_residue(ApplyRejectionReason::Cancelled));
    }

    let previous_content = match &prepared.preimage {
        Preimage::Absent => None,
        Preimage::Present { content, .. } => Some(content.as_slice()),
    };
    let mut handoff = dupe_committed_file_handoff(
        &prepared.preview,
        prepared.kind,
        &prepared.target_path,
        previous_content,
    );
    let mut resources = ApplyResources {
        policy: prepared.policy_targets.clone(),
        created: Vec::new(),
        realized: Vec::new(),
        residue_storage: Vec::new(),
        parent: None,
        temp_name: None,
        temp_identity: None,
    };

    io::e2e_fail_if_durable_mutation_attempted();
    if let Some(reason) = realize_traversal(&mut resources) {
        return Ok(resources.reject(reason));
    }
    let parent = match resources.parent.as_ref().expect("parent realized").try_clone() {
        Ok(dir) => dir,
        Err(_) => return Ok(resources.reject(ApplyRejectionReason::IoFailure)),
    };
    let last_span = *prepared
        .policy_targets
        .anchor
        .relative_components
        .last()
        .expect("target component");
    let target_name = prepared.policy_targets.component(last_span).to_vec();

    let destination_permissions = match validate_preimage(
        &parent,
        &target_name,
        &prepared,
        None,
        true,
    ) {
        PreimageValidation::Valid(permissions) => permissions,
        PreimageValidation::Stale => return Ok(resources.reject(ApplyRejectionReason::StalePreimage)),
        PreimageValidation::IoFailure => return Ok(resources.reject(ApplyRejectionReason::IoFailure)),
    };

    let mut random_bytes = [0u8; 16];
    if fill_random(&mut random_bytes).is_err() {
        return Ok(resources.reject(ApplyRejectionReason::IoFailure));
    }
    let suffix = bytes_to_hex_lower(&random_bytes);
    let mut temp_name = b".fx-stage-".to_vec();
    temp_name.extend_from_slice(&suffix);

    let mut stage = match parent.create_exclusive_child(&temp_name, destination_permissions) {
        Ok(file) => file,
        Err(_) => return Ok(resources.reject(ApplyRejectionReason::IoFailure)),
    };
    resources.temp_name = Some(temp_name.clone());
    let initial_stage_meta = match stage.metadata() {
        Ok(meta) => meta,
        Err(_) => return Ok(resources.reject(ApplyRejectionReason::IoFailure)),
    };
    if !initial_stage_meta.is_file() || nlink(&initial_stage_meta) != 1 {
        return Ok(resources.reject(ApplyRejectionReason::StagedSourceChanged));
    }
    resources.temp_identity = Some(identity_from_metadata(&initial_stage_meta));
    if let Some(mode) = destination_permissions {
        set_mode(&stage, mode).ok();
    }
    if let Err(reason) = write_staged_content(
        &mut stage,
        &prepared.after_content,
        cancel_flag,
        &mut test_controls,
    ) {
        return Ok(resources.reject(reason));
    }
    if stage.sync_all().is_err() {
        return Ok(resources.reject(ApplyRejectionReason::IoFailure));
    }
    if cancel_flag.load(Ordering::SeqCst) {
        return Ok(resources.reject(ApplyRejectionReason::Cancelled));
    }
    if let Some(after_stage) = test_controls.after_stage.as_mut() {
        if after_stage(&parent.path, &temp_name).is_err() {
            return Ok(resources.reject(ApplyRejectionReason::IoFailure));
        }
    }
    if !apply_binding_valid(call, &authorization.policy_targets, &prepared) {
        return Ok(resources.reject(ApplyRejectionReason::BindingMismatch));
    }
    if cancel_flag.load(Ordering::SeqCst) {
        return Ok(resources.reject(ApplyRejectionReason::Cancelled));
    }

    match validate_preimage(
        &parent,
        &target_name,
        &prepared,
        destination_permissions,
        false,
    ) {
        PreimageValidation::Valid(_) => {}
        PreimageValidation::Stale => return Ok(resources.reject(ApplyRejectionReason::StalePreimage)),
        PreimageValidation::IoFailure => return Ok(resources.reject(ApplyRejectionReason::IoFailure)),
    }
    if matches!(prepared.preimage, Preimage::Present { .. }) {
        if let Some(after_read) = test_controls.after_final_preimage_read.as_mut() {
            if after_read(&parent.path, &target_name).is_err() {
                return Ok(resources.reject(ApplyRejectionReason::IoFailure));
            }
        }
        match validate_target_entry(
            &parent,
            &target_name,
            prepared.policy_targets.items[0].expected_identity,
        ) {
            TargetEntryValidation::Valid => {}
            TargetEntryValidation::Changed => {
                return Ok(resources.reject(ApplyRejectionReason::StalePreimage));
            }
            TargetEntryValidation::IoFailure => {
                return Ok(resources.reject(ApplyRejectionReason::IoFailure));
            }
        }
    }
    match validate_staged_source(
        &parent,
        &temp_name,
        &mut stage,
        resources.temp_identity.unwrap(),
        &prepared.after_content,
    ) {
        StagedSourceValidation::Valid => {}
        StagedSourceValidation::Changed => {
            return Ok(resources.reject(ApplyRejectionReason::StagedSourceChanged));
        }
        StagedSourceValidation::IoFailure => {
            return Ok(resources.reject(ApplyRejectionReason::IoFailure));
        }
    }
    if cancel_flag.load(Ordering::SeqCst) {
        return Ok(resources.reject(ApplyRejectionReason::Cancelled));
    }

    let commit_parent = match revalidate_traversal(&resources) {
        TraversalValidation::Parent(dir) => dir,
        TraversalValidation::Changed => {
            return Ok(resources.reject(ApplyRejectionReason::TraversalChanged));
        }
        TraversalValidation::IoFailure => {
            return Ok(resources.reject(ApplyRejectionReason::IoFailure));
        }
    };
    match validate_target_entry(
        &commit_parent,
        &target_name,
        prepared.policy_targets.items[0].expected_identity,
    ) {
        TargetEntryValidation::Valid => {}
        TargetEntryValidation::Changed => {
            return Ok(resources.reject(ApplyRejectionReason::StalePreimage));
        }
        TargetEntryValidation::IoFailure => {
            return Ok(resources.reject(ApplyRejectionReason::IoFailure));
        }
    }
    match validate_staged_source_identity(
        &commit_parent,
        &temp_name,
        &mut stage,
        resources.temp_identity.unwrap(),
        prepared.after_content.len() as u64,
    ) {
        StagedSourceValidation::Valid => {}
        StagedSourceValidation::Changed => {
            return Ok(resources.reject(ApplyRejectionReason::StagedSourceChanged));
        }
        StagedSourceValidation::IoFailure => {
            return Ok(resources.reject(ApplyRejectionReason::IoFailure));
        }
    }
    if cancel_flag.load(Ordering::SeqCst) {
        return Ok(resources.reject(ApplyRejectionReason::Cancelled));
    }
    if let Some(after_final_validation) = test_controls.after_final_validation.as_mut() {
        if after_final_validation().is_err() {
            return Ok(resources.reject(ApplyRejectionReason::IoFailure));
        }
    }
    if prepared
        .commit_token
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        resources.cleanup_temp();
        resources.parent = None;
        let _ = resources.cleanup_created_parents();
        return Err(FileMutationApplyError::ResultContractViolation);
    }

    if commit_parent
        .rename_child(&temp_name, &target_name)
        .is_err()
    {
        return Ok(resources.reject(ApplyRejectionReason::IoFailure));
    }
    resources.temp_name = None;
    resources.temp_identity = None;
    resources.parent = None;

    handoff.tracker.committed_at_ms = io::milli_timestamp();
    Ok(ApplyResult::Committed(handoff))
}

fn dupe_committed_file_handoff(
    preview: &FileChangePreview,
    kind: Kind,
    raw_path: &[u8],
    previous_content: Option<&[u8]>,
) -> CommittedFileHandoff {
    CommittedFileHandoff::init(
        FileChangePreview {
            path: preview.path.clone(),
            path_basename_start: 0,
            path_source_truncated: false,
            lines: preview
                .lines
                .iter()
                .map(|line| PreviewLine {
                    op: line.op,
                    old_line: line.old_line,
                    new_line: line.new_line,
                    text: line.text.clone(),
                })
                .collect(),
            additions: preview.additions,
            deletions: preview.deletions,
            truncated: preview.truncated,
        },
        ChangeTrackerHandoff {
            kind,
            raw_path: raw_path.to_vec(),
            previous_content: previous_content.map(|content| content.to_vec()),
            committed_at_ms: 0,
        },
    )
}

fn write_staged_content(
    stage: &mut File,
    content: &[u8],
    cancel_flag: &AtomicBool,
    test_controls: &mut ApplyTestControls<'_>,
) -> Result<(), ApplyRejectionReason> {
    const CHUNK_BYTES: usize = 64 * 1024;
    let mut offset = 0usize;
    while offset < content.len() {
        if cancel_flag.load(Ordering::SeqCst) {
            return Err(ApplyRejectionReason::Cancelled);
        }
        let end = (offset + CHUNK_BYTES).min(content.len());
        if stage.write_all(&content[offset..end]).is_err() {
            return Err(ApplyRejectionReason::IoFailure);
        }
        offset = end;
        if let Some(after_write_chunk) = test_controls.after_write_chunk.as_mut() {
            after_write_chunk(offset)?;
        }
    }
    if cancel_flag.load(Ordering::SeqCst) {
        return Err(ApplyRejectionReason::Cancelled);
    }
    Ok(())
}

fn rejected_without_residue(reason: ApplyRejectionReason) -> ApplyResult {
    ApplyResult::Rejected(ApplyRejection {
        reason,
        created_parent_residue: Vec::new(),
    })
}

fn validate_staged_source(
    parent: &DirFd,
    temp_name: &[u8],
    stage: &mut File,
    expected_identity: FileIdentity,
    expected_content: &[u8],
) -> StagedSourceValidation {
    match validate_staged_source_identity(
        parent,
        temp_name,
        stage,
        expected_identity,
        expected_content.len() as u64,
    ) {
        StagedSourceValidation::Valid => {}
        other => return other,
    }
    if stage.seek(SeekFrom::Start(0)).is_err() {
        return StagedSourceValidation::IoFailure;
    }
    let mut hasher_input = Vec::new();
    if stage.read_to_end(&mut hasher_input).is_err() {
        return StagedSourceValidation::IoFailure;
    }
    if read_tracker::content_hash(&hasher_input) != read_tracker::content_hash(expected_content) {
        return StagedSourceValidation::Changed;
    }
    StagedSourceValidation::Valid
}

fn validate_staged_source_identity(
    parent: &DirFd,
    temp_name: &[u8],
    stage: &File,
    expected_identity: FileIdentity,
    expected_size: u64,
) -> StagedSourceValidation {
    let Ok(stat) = stage.metadata() else {
        return StagedSourceValidation::IoFailure;
    };
    if !stat.is_file() || nlink(&stat) != 1 || stat.len() != expected_size {
        return StagedSourceValidation::Changed;
    }
    let handle_identity = identity_from_metadata(&stat);
    if !identity_eql(handle_identity, expected_identity) {
        return StagedSourceValidation::Changed;
    }
    let Some(named_identity) = observed_file_identity(parent, temp_name) else {
        return StagedSourceValidation::Changed;
    };
    if !identity_eql(named_identity, expected_identity) {
        return StagedSourceValidation::Changed;
    }
    StagedSourceValidation::Valid
}

fn apply_binding_valid(
    call: &ToolCall,
    policy: &PolicyEvaluatedFileTargets,
    prepared: &PreparedFileMutation,
) -> bool {
    if call.id != prepared.call_id || call.name != prepared.tool_name {
        return false;
    }
    let expected_kind = if call.name == b"write_file" {
        Kind::Write
    } else if call.name == b"edit_file" {
        Kind::Edit
    } else {
        return false;
    };
    if prepared.kind != expected_kind {
        return false;
    }
    if read_tracker::content_hash(&call.arguments_json) != prepared.arguments_hash {
        return false;
    }
    if !PolicyEvaluatedFileTargets::authorization_eql(policy, &prepared.policy_targets) {
        return false;
    }
    if !policy.proof_valid() {
        return false;
    }
    if prepared.target_path != policy.canonical_target_path {
        return false;
    }
    if prepared.display_path != prepared.preview.path {
        return false;
    }
    if prepared.preview.validate().is_err() {
        return false;
    }
    if prepared.after_content.len() > max_content_bytes {
        return false;
    }
    match &prepared.preimage {
        Preimage::Absent => {}
        Preimage::Present {
            content,
            content_hash,
        } => {
            if content.len() > max_content_bytes {
                return false;
            }
            if read_tracker::content_hash(content) != *content_hash {
                return false;
            }
        }
    }
    true
}

fn realize_traversal(resources: &mut ApplyResources) -> Option<ApplyRejectionReason> {
    let mut current = match DirFd::open_absolute(&path_from_bytes(resources.policy.anchor_path())) {
        Ok(dir) => dir,
        Err(err) => {
            return Some(if err.kind() == std::io::ErrorKind::NotFound {
                ApplyRejectionReason::TraversalChanged
            } else {
                ApplyRejectionReason::IoFailure
            })
        }
    };
    let Ok(anchor_identity) = current.identity() else {
        return Some(ApplyRejectionReason::IoFailure);
    };
    if !identity_eql(anchor_identity, resources.policy.anchor.identity) {
        return Some(ApplyRejectionReason::TraversalChanged);
    }

    for (index, entry) in resources.policy.traversal_directories.clone().iter().enumerate() {
        let component = resources
            .policy
            .component(resources.policy.anchor.relative_components[entry.component_index])
            .to_vec();
        match entry.state {
            TraversalDirectoryState::Existing(expected_identity) => {
                let next = match current.open_child(&component) {
                    Ok(dir) => dir,
                    Err(err) => {
                        return Some(if err.kind() == std::io::ErrorKind::NotFound {
                            ApplyRejectionReason::TraversalChanged
                        } else {
                            ApplyRejectionReason::IoFailure
                        })
                    }
                };
                let Ok(actual) = next.identity() else {
                    return Some(ApplyRejectionReason::IoFailure);
                };
                if !identity_eql(actual, expected_identity) {
                    return Some(ApplyRejectionReason::TraversalChanged);
                }
                resources.realized.push(actual);
                current = next;
            }
            TraversalDirectoryState::Create {
                permission_target_index,
            } => {
                if let Err(err) = current.mkdir_child(&component) {
                    return Some(if err.kind() == std::io::ErrorKind::AlreadyExists {
                        ApplyRejectionReason::TraversalChanged
                    } else {
                        ApplyRejectionReason::IoFailure
                    });
                }
                let next = match current.open_child(&component) {
                    Ok(dir) => dir,
                    Err(_) => {
                        let _ = fs::remove_dir(join_dir(&current.path, &component));
                        return Some(ApplyRejectionReason::IoFailure);
                    }
                };
                let Ok(actual) = next.identity() else {
                    let _ = fs::remove_dir(&next.path);
                    return Some(ApplyRejectionReason::IoFailure);
                };
                resources.realized.push(actual);
                resources.created.push(CreatedParentRecord {
                    component_index: entry.component_index,
                    path_end: entry.path_end,
                    permission_target_index,
                    identity: actual,
                });
                let _ = index;
                current = next;
            }
        }
    }
    resources.parent = Some(current);
    None
}

fn revalidate_traversal(resources: &ApplyResources) -> TraversalValidation {
    let mut current = match DirFd::open_absolute(&path_from_bytes(resources.policy.anchor_path())) {
        Ok(dir) => dir,
        Err(err) => {
            return if err.kind() == std::io::ErrorKind::NotFound {
                TraversalValidation::Changed
            } else {
                TraversalValidation::IoFailure
            }
        }
    };
    let Ok(anchor_identity) = current.identity() else {
        return TraversalValidation::IoFailure;
    };
    if !identity_eql(anchor_identity, resources.policy.anchor.identity) {
        return TraversalValidation::Changed;
    }
    for (index, entry) in resources.policy.traversal_directories.iter().enumerate() {
        let component = resources.policy.component(
            resources.policy.anchor.relative_components[entry.component_index],
        );
        let next = match current.open_child(component) {
            Ok(dir) => dir,
            Err(err) => {
                return if err.kind() == std::io::ErrorKind::NotFound {
                    TraversalValidation::Changed
                } else {
                    TraversalValidation::IoFailure
                }
            }
        };
        let Ok(actual) = next.identity() else {
            return TraversalValidation::IoFailure;
        };
        if !identity_eql(actual, resources.realized[index]) {
            return TraversalValidation::Changed;
        }
        current = next;
    }
    TraversalValidation::Parent(current)
}

fn validate_preimage(
    parent: &DirFd,
    target_name: &[u8],
    prepared: &PreparedFileMutation,
    expected_permissions: Option<u32>,
    recheck_target: bool,
) -> PreimageValidation {
    match &prepared.preimage {
        Preimage::Absent => match parent.open_child_file(target_name) {
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                PreimageValidation::Valid(None)
            }
            Err(_) => PreimageValidation::IoFailure,
            Ok(_) => PreimageValidation::Stale,
        },
        Preimage::Present { content, content_hash } => {
            let mut file = match parent.open_child_file(target_name) {
                Ok(file) => file,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    return PreimageValidation::Stale;
                }
                Err(_) => return PreimageValidation::IoFailure,
            };
            let Ok(stat) = file.metadata() else {
                return PreimageValidation::IoFailure;
            };
            if !stat.is_file() || stat.len() != content.len() as u64 {
                return PreimageValidation::Stale;
            }
            let actual_identity = identity_from_metadata(&stat);
            let Some(expected_identity) = prepared.policy_targets.items[0].expected_identity else {
                return PreimageValidation::Stale;
            };
            if !identity_eql(actual_identity, expected_identity) {
                return PreimageValidation::Stale;
            }
            let mode = unix_mode(&stat);
            if mode & 0o222 == 0 {
                return PreimageValidation::IoFailure;
            }
            if let Some(expected) = expected_permissions {
                if mode != expected {
                    return PreimageValidation::Stale;
                }
            }
            let mut read_buf = Vec::new();
            if file.read_to_end(&mut read_buf).is_err() {
                return PreimageValidation::IoFailure;
            }
            if read_tracker::content_hash(&read_buf) != *content_hash {
                return PreimageValidation::Stale;
            }
            if recheck_target {
                match validate_target_entry(parent, target_name, Some(actual_identity)) {
                    TargetEntryValidation::Valid => {}
                    TargetEntryValidation::Changed => return PreimageValidation::Stale,
                    TargetEntryValidation::IoFailure => return PreimageValidation::IoFailure,
                }
            }
            PreimageValidation::Valid(Some(mode))
        }
    }
}

fn validate_target_entry(
    parent: &DirFd,
    name: &[u8],
    expected_identity: Option<FileIdentity>,
) -> TargetEntryValidation {
    if expected_identity.is_none() {
        return match parent.open_child_file(name) {
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => TargetEntryValidation::Valid,
            Err(_) => TargetEntryValidation::IoFailure,
            Ok(_) => TargetEntryValidation::Changed,
        };
    }
    let Ok(file) = parent.open_child_file(name) else {
        return if parent
            .open_child_file(name)
            .err()
            .is_some_and(|err| err.kind() == std::io::ErrorKind::NotFound)
            || path_missing(&join_dir(&parent.path, name))
        {
            TargetEntryValidation::Changed
        } else {
            TargetEntryValidation::IoFailure
        };
    };
    let Ok(stat) = file.metadata() else {
        return TargetEntryValidation::IoFailure;
    };
    if !stat.is_file() {
        return TargetEntryValidation::Changed;
    }
    let actual = identity_from_metadata(&stat);
    if identity_eql(actual, expected_identity.unwrap()) {
        TargetEntryValidation::Valid
    } else {
        TargetEntryValidation::Changed
    }
}

fn observed_file_identity(parent: &DirFd, name: &[u8]) -> Option<FileIdentity> {
    let file = parent.open_child_file(name).ok()?;
    let stat = file.metadata().ok()?;
    if !stat.is_file() || nlink(&stat) != 1 {
        return None;
    }
    Some(identity_from_metadata(&stat))
}

fn cleanup_created_parent(
    policy: &PolicyEvaluatedFileTargets,
    realized: &[FileIdentity],
    created: &CreatedParentRecord,
) -> Option<CreatedParentResidue> {
    let mut current = path_from_bytes(policy.anchor_path());
    let Ok(anchor_identity) = pathing::directory_identity(policy.anchor_path()) else {
        return Some(created_parent_residue(created, None, ParentCleanupResidueReason::IdentityChanged));
    };
    if !identity_eql(to_contract_identity(anchor_identity), policy.anchor.identity) {
        return Some(created_parent_residue(created, None, ParentCleanupResidueReason::IdentityChanged));
    }
    for (index, entry) in policy.traversal_directories[..created.component_index]
        .iter()
        .enumerate()
    {
        let component = policy.component(policy.anchor.relative_components[entry.component_index]);
        let next = join_dir(&current, component);
        let Ok(actual) = pathing::directory_identity(&path_to_bytes(&next)) else {
            return Some(created_parent_residue(
                created,
                None,
                ParentCleanupResidueReason::IdentityChanged,
            ));
        };
        if !identity_eql(to_contract_identity(actual), realized[index]) {
            return Some(created_parent_residue(
                created,
                None,
                ParentCleanupResidueReason::IdentityChanged,
            ));
        }
        current = next;
    }
    let component = policy.component(policy.anchor.relative_components[created.component_index]);
    let child = join_dir(&current, component);
    if path_missing(&child) {
        return None;
    }
    let Ok(observed) = pathing::directory_identity(&path_to_bytes(&child)) else {
        return Some(created_parent_residue(
            created,
            None,
            ParentCleanupResidueReason::IdentityChanged,
        ));
    };
    let observed = to_contract_identity(observed);
    if !identity_eql(observed, created.identity) {
        return Some(created_parent_residue(
            created,
            Some(observed),
            ParentCleanupResidueReason::IdentityChanged,
        ));
    }
    match fs::remove_dir(&child) {
        Ok(()) => None,
        Err(err) if err.kind() == std::io::ErrorKind::DirectoryNotEmpty => Some(
            created_parent_residue(created, Some(observed), ParentCleanupResidueReason::NotEmpty),
        ),
        Err(_) => Some(created_parent_residue(
            created,
            Some(observed),
            ParentCleanupResidueReason::RemoveFailed,
        )),
    }
}

fn created_parent_residue(
    created: &CreatedParentRecord,
    observed_identity: Option<FileIdentity>,
    reason: ParentCleanupResidueReason,
) -> CreatedParentResidue {
    CreatedParentResidue {
        component_index: created.component_index,
        path_end: created.path_end,
        permission_target_index: created.permission_target_index,
        created_identity: created.identity,
        observed_identity,
        reason,
    }
}

fn policy_identity_binding_mismatch(policy: &PolicyEvaluatedFileTargets) -> bool {
    if policy.items.len() < 2 {
        return false;
    }
    let parent_item = policy.items[1];
    if parent_item.kind != file_mutation_contract::PermissionTargetKind::Parent {
        return false;
    }
    if policy.traversal_directories.is_empty() {
        if parent_item.disposition != file_mutation_contract::FileTargetDisposition::ExistingParent
            || parent_item.path_end != policy.anchor.path_end
        {
            return false;
        }
        return !optional_identity_eql(
            parent_item.expected_identity,
            Some(policy.anchor.identity),
        );
    }
    let parent = policy.traversal_directories[policy.traversal_directories.len() - 1];
    match parent.state {
        TraversalDirectoryState::Existing(identity) => {
            parent_item.disposition
                == file_mutation_contract::FileTargetDisposition::ExistingParent
                && parent_item.path_end == parent.path_end
                && !optional_identity_eql(parent_item.expected_identity, Some(identity))
        }
        TraversalDirectoryState::Create { .. } => {
            parent_item.disposition == file_mutation_contract::FileTargetDisposition::CreateParent
                && parent_item.path_end == parent.path_end
                && parent_item.expected_identity.is_some()
        }
    }
}

fn resolved_target_matches(
    input: &FileMutationInput,
    policy: &PolicyEvaluatedFileTargets,
) -> Result<bool, FileMutationPrepareError> {
    TARGET_RESOLUTION_CHECK_COUNT.fetch_add(1, Ordering::SeqCst);
    let mut primary = vec![0u8; max_path_bytes];
    let mut secondary = vec![0u8; max_path_bytes];
    let mut component_scratch =
        vec![pathing::BoundedFileTargetComponent { start: 0, end: 0 }; file_mutation_contract::max_file_target_components];
    let resolved = match pathing::resolve_file_mutation_target_bounded(
        policy.anchor_path(),
        input.path(),
        match input {
            FileMutationInput::Write(_) => ResolveMode::Create,
            FileMutationInput::Edit(_) => ResolveMode::Existing,
        },
        &mut primary,
        &mut secondary,
        &mut component_scratch,
    ) {
        Ok(resolved) => resolved,
        Err(err) => {
            if let Some(operational) = operational_path_error(err) {
                return Err(operational);
            }
            return Ok(false);
        }
    };
    if resolved.canonical_target_path != policy.canonical_target_path.as_slice() {
        return Ok(false);
    }
    if resolved.anchor_path_end != policy.anchor.path_end {
        return Ok(false);
    }
    if resolved.relative_components.len() != policy.anchor.relative_components.len() {
        return Ok(false);
    }
    for (actual, expected) in resolved
        .relative_components
        .iter()
        .zip(policy.anchor.relative_components.iter())
    {
        if actual.start != expected.start || actual.end != expected.end {
            return Ok(false);
        }
    }
    Ok(true)
}

fn verify_and_read_preimage(
    input: &FileMutationInput,
    policy: &PolicyEvaluatedFileTargets,
) -> Result<PreimageResult, FileMutationPrepareError> {
    let mut current = path_from_bytes(policy.anchor_path());
    let Ok(anchor_identity) = pathing::directory_identity(policy.anchor_path()) else {
        return Ok(PreimageResult::SemanticFailure(
            IDENTITY_CHANGED_MESSAGE.to_vec(),
        ));
    };
    if !identity_eql(to_contract_identity(anchor_identity), policy.anchor.identity) {
        return Ok(PreimageResult::SemanticFailure(
            IDENTITY_CHANGED_MESSAGE.to_vec(),
        ));
    }

    let mut missing_parent_seen = false;
    for entry in &policy.traversal_directories {
        let component = policy.component(
            policy.anchor.relative_components[entry.component_index],
        );
        match entry.state {
            TraversalDirectoryState::Existing(expected_identity) => {
                if missing_parent_seen {
                    return Ok(PreimageResult::SemanticFailure(
                        IDENTITY_CHANGED_MESSAGE.to_vec(),
                    ));
                }
                let next = join_dir(&current, component);
                let Ok(actual) = pathing::directory_identity(&path_to_bytes(&next)) else {
                    return Ok(PreimageResult::SemanticFailure(
                        IDENTITY_CHANGED_MESSAGE.to_vec(),
                    ));
                };
                if !identity_eql(to_contract_identity(actual), expected_identity) {
                    return Ok(PreimageResult::SemanticFailure(
                        IDENTITY_CHANGED_MESSAGE.to_vec(),
                    ));
                }
                current = next;
            }
            TraversalDirectoryState::Create { .. } => {
                if missing_parent_seen {
                    continue;
                }
                let next = join_dir(&current, component);
                match fs::symlink_metadata(&next) {
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                        missing_parent_seen = true;
                        continue;
                    }
                    Err(_) => {
                        return Ok(PreimageResult::SemanticFailure(
                            IDENTITY_CHANGED_MESSAGE.to_vec(),
                        ));
                    }
                    Ok(_) => {
                        return Ok(PreimageResult::SemanticFailure(
                            IDENTITY_CHANGED_MESSAGE.to_vec(),
                        ));
                    }
                }
            }
        }
    }

    let target_span = *policy.anchor.relative_components.last().unwrap();
    let target_name = policy.component(target_span);
    let expected_target_identity = policy.items[0].expected_identity;
    if missing_parent_seen {
        if expected_target_identity.is_some() || !matches!(input, FileMutationInput::Write(_)) {
            return Ok(PreimageResult::SemanticFailure(
                IDENTITY_CHANGED_MESSAGE.to_vec(),
            ));
        }
        return Ok(PreimageResult::Preimage(Preimage::Absent));
    }

    let target_path = join_dir(&current, target_name);
    let path_stat = match fs::symlink_metadata(&target_path) {
        Ok(stat) => stat,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            if expected_target_identity.is_some() || !matches!(input, FileMutationInput::Write(_)) {
                return Ok(PreimageResult::SemanticFailure(
                    IDENTITY_CHANGED_MESSAGE.to_vec(),
                ));
            }
            return Ok(PreimageResult::Preimage(Preimage::Absent));
        }
        Err(_) => {
            return Ok(PreimageResult::SemanticFailure(
                IDENTITY_CHANGED_MESSAGE.to_vec(),
            ));
        }
    };
    let Some(expected_identity) = expected_target_identity else {
        return Ok(PreimageResult::SemanticFailure(
            IDENTITY_CHANGED_MESSAGE.to_vec(),
        ));
    };
    if !path_stat.is_file() || expected_identity.kind != FileKind::File {
        return Ok(PreimageResult::SemanticFailure(
            b"file mutation preparation failed: target is not a regular file".to_vec(),
        ));
    }
    let Ok(mut file) = open_nofollow(&target_path, false) else {
        return Ok(PreimageResult::SemanticFailure(
            IDENTITY_CHANGED_MESSAGE.to_vec(),
        ));
    };
    let Ok(file_stat) = file.metadata() else {
        return Ok(PreimageResult::SemanticFailure(
            IDENTITY_CHANGED_MESSAGE.to_vec(),
        ));
    };
    let actual_identity = identity_from_metadata(&file_stat);
    if !identity_eql(actual_identity, expected_identity) {
        return Ok(PreimageResult::SemanticFailure(
            IDENTITY_CHANGED_MESSAGE.to_vec(),
        ));
    }
    let mut content = Vec::new();
    match file.read_to_end(&mut content) {
        Ok(_) => {}
        Err(_) => {
            return Ok(PreimageResult::SemanticFailure(
                b"file mutation preparation failed: unable to read the approved preimage".to_vec(),
            ));
        }
    }
    if content.len() > max_content_bytes {
        return Ok(PreimageResult::SemanticFailure(
            b"file mutation preparation failed: preimage exceeds the 4 MiB preparation limit"
                .to_vec(),
        ));
    }
    let content_hash = read_tracker::content_hash(&content);
    Ok(PreimageResult::Preimage(Preimage::Present {
        content,
        content_hash,
    }))
}

fn derive_postimage(
    input: &FileMutationInput,
    preimage: &Preimage,
) -> Result<PostimageResult, FileMutationPrepareError> {
    match input {
        FileMutationInput::Write(write) => Ok(PostimageResult::Content(write.content.clone())),
        FileMutationInput::Edit(edit) => {
            if edit.old_string == edit.new_string {
                return Ok(PostimageResult::SemanticFailure(
                    b"edit_file failed: old_string and new_string are identical".to_vec(),
                ));
            }
            let before = match preimage {
                Preimage::Absent => {
                    return Ok(PostimageResult::SemanticFailure(
                        IDENTITY_CHANGED_MESSAGE.to_vec(),
                    ));
                }
                Preimage::Present { content, .. } => content.as_slice(),
            };
            let occurrence_count = count_occurrences(before, &edit.old_string);
            if occurrence_count == 0 {
                return Ok(PostimageResult::SemanticFailure(
                    b"edit_file failed: old_string not found in file".to_vec(),
                ));
            }
            if occurrence_count > 1 {
                return Ok(PostimageResult::SemanticFailure(format!(
                    "edit_file failed: old_string is not unique (found {occurrence_count} occurrences), provide more context"
                ).into_bytes()));
            }
            let match_start = find_bytes(before, &edit.old_string).unwrap();
            let suffix_start = match_start + edit.old_string.len();
            let after_len = match match_start
                .checked_add(edit.new_string.len())
                .and_then(|n| n.checked_add(before.len() - suffix_start))
            {
                Some(len) => len,
                None => {
                    return Ok(PostimageResult::SemanticFailure(
                        b"edit_file failed: postimage exceeds the 4 MiB preparation limit".to_vec(),
                    ));
                }
            };
            if after_len > max_content_bytes {
                return Ok(PostimageResult::SemanticFailure(
                    b"edit_file failed: postimage exceeds the 4 MiB preparation limit".to_vec(),
                ));
            }
            let mut after = Vec::with_capacity(after_len);
            after.extend_from_slice(&before[..match_start]);
            after.extend_from_slice(&edit.new_string);
            after.extend_from_slice(&before[suffix_start..]);
            Ok(PostimageResult::Content(after))
        }
    }
}

fn count_occurrences(haystack: &[u8], needle: &[u8]) -> usize {
    if needle.is_empty() {
        return 0;
    }
    let mut count = 0usize;
    let mut index = 0usize;
    while let Some(relative) = find_bytes(&haystack[index..], needle) {
        count += 1;
        index += relative + needle.len();
    }
    count
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|window| window == needle)
}

fn build_preview(
    display_path: &text_utils::EncodedPathTail,
    preimage: &Preimage,
    before_content: &[u8],
    after_content: &[u8],
) -> Result<PreviewResult, FileMutationPrepareError> {
    let mut projection = match diff::build_bounded_preview(
        before_content,
        after_content,
        diff::PreviewOptions::default(),
    ) {
        Ok(projection) => projection,
        Err(BoundedPreviewError::OutOfMemory) => {
            return Err(FileMutationPrepareError::OutOfMemory);
        }
        Err(
            BoundedPreviewError::InvalidPreviewOptions
            | BoundedPreviewError::PreviewBudgetExceeded
            | BoundedPreviewError::ArithmeticOverflow,
        ) => {
            return Ok(PreviewResult::SemanticFailure(
                preview_too_large_message.to_vec(),
            ));
        }
    };

    let lines = if projection.additions == 0 && projection.deletions == 0 {
        projection.deinit();
        let notice = match preimage {
            Preimage::Absent if after_content.is_empty() => b"empty file".as_slice(),
            _ => b"no content changes".as_slice(),
        };
        vec![PreviewLine {
            op: PreviewOp::Notice,
            old_line: None,
            new_line: None,
            text: notice.to_vec(),
        }]
    } else {
        projection
            .lines
            .drain(..)
            .map(|source| PreviewLine {
                op: source.op,
                old_line: source.old_line,
                new_line: source.new_line,
                text: source.text,
            })
            .collect()
    };

    let preview = FileChangePreview {
        path: display_path.bytes.clone(),
        path_basename_start: display_path.basename_start,
        path_source_truncated: display_path.source_truncated,
        lines,
        additions: projection.additions,
        deletions: projection.deletions,
        truncated: projection.truncated,
    };
    if preview.validate().is_err() {
        return Ok(PreviewResult::SemanticFailure(
            preview_too_large_message.to_vec(),
        ));
    }
    let mut projection_charge = display_path.bytes.len();
    let Some(charged) = checked_charge(
        projection_charge,
        preview.lines.len(),
        std::mem::size_of::<PreviewLine>(),
    ) else {
        return Ok(PreviewResult::SemanticFailure(
            preview_too_large_message.to_vec(),
        ));
    };
    projection_charge = charged;
    for line in &preview.lines {
        projection_charge = match projection_charge.checked_add(line.text.len()) {
            Some(total) => total,
            None => {
                return Ok(PreviewResult::SemanticFailure(
                    preview_too_large_message.to_vec(),
                ));
            }
        };
    }
    if projection_charge > MAX_REQUEST_PROJECTION_BYTES {
        return Ok(PreviewResult::SemanticFailure(
            preview_too_large_message.to_vec(),
        ));
    }
    Ok(PreviewResult::Preview(preview))
}

fn checked_charge(base: usize, count: usize, item_size: usize) -> Option<usize> {
    count.checked_mul(item_size)?.checked_add(base)
}

fn identity_eql(a: FileIdentity, b: FileIdentity) -> bool {
    a.device == b.device && a.inode == b.inode && a.kind == b.kind
}

fn optional_identity_eql(a: Option<FileIdentity>, b: Option<FileIdentity>) -> bool {
    match (a, b) {
        (Some(actual), Some(expected)) => identity_eql(actual, expected),
        (None, None) => true,
        _ => false,
    }
}

fn operational_path_error(err: PathError) -> Option<FileMutationPrepareError> {
    match err {
        PathError::Canceled => Some(FileMutationPrepareError::Canceled),
        PathError::Unexpected => Some(FileMutationPrepareError::Unexpected),
        PathError::SystemResources => Some(FileMutationPrepareError::SystemResources),
        PathError::ProcessFdQuotaExceeded => Some(FileMutationPrepareError::ProcessFdQuotaExceeded),
        PathError::SystemFdQuotaExceeded => Some(FileMutationPrepareError::SystemFdQuotaExceeded),
        _ => None,
    }
}

fn to_contract_identity(id: pathing::FileIdentity) -> FileIdentity {
    FileIdentity {
        device: id.device,
        inode: id.inode,
        kind: match id.kind {
            pathing::FileKind::Directory => FileKind::Directory,
            pathing::FileKind::File => FileKind::File,
            pathing::FileKind::SymLink => FileKind::SymLink,
            pathing::FileKind::Unknown => FileKind::Unknown,
        },
    }
}

fn identity_from_metadata(meta: &fs::Metadata) -> FileIdentity {
    FileIdentity {
        device: unix_dev(meta),
        inode: unix_ino(meta),
        kind: if meta.file_type().is_dir() {
            FileKind::Directory
        } else if meta.file_type().is_file() {
            FileKind::File
        } else if meta.file_type().is_symlink() {
            FileKind::SymLink
        } else {
            FileKind::Unknown
        },
    }
}

fn path_from_bytes(bytes: &[u8]) -> PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        PathBuf::from(std::ffi::OsStr::from_bytes(bytes))
    }
    #[cfg(not(unix))]
    {
        PathBuf::from(String::from_utf8_lossy(bytes).as_ref())
    }
}

fn path_to_bytes(path: &Path) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        path.as_os_str().as_bytes().to_vec()
    }
    #[cfg(not(unix))]
    {
        path.to_string_lossy().as_bytes().to_vec()
    }
}

fn join_dir(parent: &Path, name: &[u8]) -> PathBuf {
    parent.join(path_from_bytes(name))
}

fn path_missing(path: &Path) -> bool {
    matches!(
        fs::symlink_metadata(path),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound
    )
}

fn fill_random(out: &mut [u8]) -> std::io::Result<()> {
    File::open("/dev/urandom")?.read_exact(out)
}

fn bytes_to_hex_lower(bytes: &[u8]) -> Vec<u8> {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = Vec::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        out.push(HEX[(byte >> 4) as usize]);
        out.push(HEX[(byte & 0xf) as usize]);
    }
    out
}

fn open_nofollow(path: &Path, write: bool) -> std::io::Result<File> {
    let mut opts = OpenOptions::new();
    opts.read(true);
    if write {
        opts.write(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(0o400000); // O_NOFOLLOW on Linux
    }
    opts.open(path)
}

fn create_exclusive_file(path: &Path, mode: Option<u32>) -> std::io::Result<File> {
    let mut opts = OpenOptions::new();
    opts.read(true).write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(mode.unwrap_or(0o666));
    }
    let _ = mode;
    opts.open(path)
}

fn set_mode(file: &File, mode: u32) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = file.metadata()?.permissions();
        perms.set_mode(mode);
        file.set_permissions(perms)
    }
    #[cfg(not(unix))]
    {
        let _ = (file, mode);
        Ok(())
    }
}

fn nlink(meta: &fs::Metadata) -> u64 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        meta.nlink()
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        1
    }
}

fn unix_mode(meta: &fs::Metadata) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode()
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        0o666
    }
}

fn unix_dev(meta: &fs::Metadata) -> u64 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        meta.dev()
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        0
    }
}

fn unix_ino(meta: &fs::Metadata) -> u64 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        meta.ino()
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::mcp::json_value::{self, Value};
    use crate::core::shared::types::RuleDecision;
    use crate::core::tooling::file_mutation_contract::{
        EditInput, EvaluatedPermissionTarget, FileTargetDisposition, PathSpan,
        PermissionTargetKind, TraversalDirectory, TraversalScope, WriteInput,
    };
    use std::sync::atomic::AtomicBool;
    use std::time::{SystemTime, UNIX_EPOCH};
    use std::os::unix::ffi::OsStrExt;

    fn write_json_string(out: &mut Vec<u8>, bytes: &[u8]) {
        out.push(b'"');
        for &byte in bytes {
            match byte {
                b'"' => out.extend_from_slice(br#"\""#),
                b'\\' => out.extend_from_slice(br#"\\"#),
                b'\n' => out.extend_from_slice(br#"\n"#),
                b'\r' => out.extend_from_slice(br#"\r"#),
                b'\t' => out.extend_from_slice(br#"\t"#),
                byte if byte < 0x20 => {
                    out.extend_from_slice(format!("\\u{byte:04x}").as_bytes());
                }
                _ => out.push(byte),
            }
        }
        out.push(b'"');
    }

    fn write_arguments_json(path: &[u8], content: &[u8]) -> Vec<u8> {
        let mut out = b"{\"path\":".to_vec();
        write_json_string(&mut out, path);
        out.extend_from_slice(b",\"content\":");
        write_json_string(&mut out, content);
        out.push(b'}');
        out
    }

    fn edit_arguments_json(path: &[u8], old_string: &[u8], new_string: &[u8]) -> Vec<u8> {
        let mut out = b"{\"path\":".to_vec();
        write_json_string(&mut out, path);
        out.extend_from_slice(b",\"old_string\":");
        write_json_string(&mut out, old_string);
        out.extend_from_slice(b",\"new_string\":");
        write_json_string(&mut out, new_string);
        out.push(b'}');
        out
    }

    fn tmp_root() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("nyq-file-mutation-{nanos}"));
        fs::create_dir_all(&path).unwrap();
        fs::canonicalize(&path).unwrap()
    }

    fn workspace_root(tmp: &Path) -> Vec<u8> {
        path_to_bytes(tmp)
    }

    fn create_file(tmp: &Path, sub_path: &str, content: &[u8]) {
        let path = tmp.join(sub_path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, content).unwrap();
    }

    fn evaluate_policy(workspace: &[u8], input: &FileMutationInput) -> PolicyEvaluatedFileTargets {
        let mut primary = vec![0u8; max_path_bytes];
        let mut secondary = vec![0u8; max_path_bytes];
        let mut component_scratch = vec![
            pathing::BoundedFileTargetComponent { start: 0, end: 0 };
            file_mutation_contract::max_file_target_components
        ];
        let resolved = pathing::resolve_file_mutation_target_bounded(
            workspace,
            input.path(),
            match input {
                FileMutationInput::Write(_) => ResolveMode::Create,
                FileMutationInput::Edit(_) => ResolveMode::Existing,
            },
            &mut primary,
            &mut secondary,
            &mut component_scratch,
        )
        .expect("resolve file mutation target");
        let canonical = resolved.canonical_target_path.to_vec();
        let rel: Vec<PathSpan> = resolved
            .relative_components
            .iter()
            .map(|component| PathSpan {
                start: component.start,
                end: component.end,
            })
            .collect();
        let anchor_path_end = resolved.anchor_path_end;
        let anchor_is_external = resolved.anchor_is_external;
        let anchor_identity = to_contract_identity(
            pathing::directory_identity(&canonical[..anchor_path_end]).unwrap(),
        );

        let mut current = path_from_bytes(&canonical[..anchor_path_end]);
        let mut traversal = Vec::new();
        let mut missing_parent_seen = false;
        for (index, span) in rel.iter().take(rel.len().saturating_sub(1)).enumerate() {
            let component = &canonical[span.start..span.end];
            let next = join_dir(&current, component);
            if missing_parent_seen {
                traversal.push(TraversalDirectory {
                    component_index: index,
                    path_end: span.end,
                    state: TraversalDirectoryState::Create {
                        permission_target_index: 0,
                    },
                });
                continue;
            }
            match fs::symlink_metadata(&next) {
                Ok(_) => {
                    let identity = to_contract_identity(
                        pathing::directory_identity(&path_to_bytes(&next)).unwrap(),
                    );
                    traversal.push(TraversalDirectory {
                        component_index: index,
                        path_end: span.end,
                        state: TraversalDirectoryState::Existing(identity),
                    });
                    current = next;
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    missing_parent_seen = true;
                    traversal.push(TraversalDirectory {
                        component_index: index,
                        path_end: span.end,
                        state: TraversalDirectoryState::Create {
                            permission_target_index: 0,
                        },
                    });
                }
                Err(err) => panic!("stat parent: {err}"),
            }
        }

        let mut target_identity = None;
        if !missing_parent_seen {
            let target_span = *rel.last().unwrap();
            let target_name = &canonical[target_span.start..target_span.end];
            let target_path = join_dir(&current, target_name);
            if let Ok(meta) = fs::symlink_metadata(&target_path) {
                if meta.is_file() {
                    target_identity = Some(identity_from_metadata(&meta));
                }
            }
        }

        let mut items = Vec::new();
        items.push(EvaluatedPermissionTarget {
            kind: PermissionTargetKind::Target,
            disposition: FileTargetDisposition::TargetEntry,
            path_end: canonical.len(),
            expected_identity: target_identity,
            rule: RuleDecision::None,
            session_grant_allowed: false,
        });
        if traversal.is_empty() {
            items.push(EvaluatedPermissionTarget {
                kind: PermissionTargetKind::Parent,
                disposition: FileTargetDisposition::ExistingParent,
                path_end: anchor_path_end,
                expected_identity: Some(anchor_identity),
                rule: RuleDecision::None,
                session_grant_allowed: false,
            });
        } else {
            let parent_index = traversal.len() - 1;
            match traversal[parent_index].state {
                TraversalDirectoryState::Existing(identity) => {
                    items.push(EvaluatedPermissionTarget {
                        kind: PermissionTargetKind::Parent,
                        disposition: FileTargetDisposition::ExistingParent,
                        path_end: traversal[parent_index].path_end,
                        expected_identity: Some(identity),
                        rule: RuleDecision::None,
                        session_grant_allowed: false,
                    });
                }
                TraversalDirectoryState::Create { .. } => {
                    items.push(EvaluatedPermissionTarget {
                        kind: PermissionTargetKind::Parent,
                        disposition: FileTargetDisposition::CreateParent,
                        path_end: traversal[parent_index].path_end,
                        expected_identity: None,
                        rule: RuleDecision::None,
                        session_grant_allowed: false,
                    });
                    traversal[parent_index].state = TraversalDirectoryState::Create {
                        permission_target_index: 1,
                    };
                }
            }
            let mut parent_index = if traversal.len() > 1 {
                traversal.len() - 1
            } else {
                0
            };
            while parent_index > 0 {
                parent_index -= 1;
                if let TraversalDirectoryState::Create { .. } = traversal[parent_index].state {
                    let item_count = items.len();
                    traversal[parent_index].state = TraversalDirectoryState::Create {
                        permission_target_index: item_count,
                    };
                    items.push(EvaluatedPermissionTarget {
                        kind: PermissionTargetKind::Parent,
                        disposition: FileTargetDisposition::CreateParent,
                        path_end: traversal[parent_index].path_end,
                        expected_identity: None,
                        rule: RuleDecision::None,
                        session_grant_allowed: false,
                    });
                }
            }
        }

        PolicyEvaluatedFileTargets {
            canonical_target_path: canonical,
            anchor: file_mutation_contract::VerifiedTraversalAnchor {
                scope: if anchor_is_external {
                    TraversalScope::External
                } else {
                    TraversalScope::Workspace
                },
                path_end: anchor_path_end,
                identity: anchor_identity,
                relative_components: rel,
            },
            traversal_directories: traversal,
            items,
            prompt_required: true,
        }
    }

    fn decode_test_mutation_input(call: &ToolCall) -> FileMutationInput {
        let text = std::str::from_utf8(&call.arguments_json).unwrap();
        let parsed = json_value::parse_json(text).unwrap();
        let object = parsed.as_object().unwrap();
        let path = match object.get("path") {
            Some(Value::String(value)) => value.as_bytes().to_vec(),
            _ => panic!("path"),
        };
        if call.name == b"write_file" {
            let content = match object.get("content") {
                Some(Value::String(value)) => value.as_bytes().to_vec(),
                _ => panic!("content"),
            };
            FileMutationInput::Write(WriteInput { path, content })
        } else {
            let old_string = match object.get("old_string") {
                Some(Value::String(value)) => value.as_bytes().to_vec(),
                _ => panic!("old_string"),
            };
            let new_string = match object.get("new_string") {
                Some(Value::String(value)) => value.as_bytes().to_vec(),
                _ => panic!("new_string"),
            };
            FileMutationInput::Edit(EditInput {
                path,
                old_string,
                new_string,
            })
        }
    }

    fn expect_prepared(call: &ToolCall, policy: PolicyEvaluatedFileTargets) -> PreparedFileMutation {
        let input = decode_test_mutation_input(call);
        match prepare(call, &input, policy).unwrap() {
            PrepareResult::Prepared(prepared) => prepared,
            PrepareResult::SemanticFailure(message) => {
                panic!(
                    "unexpected semantic failure: {}",
                    String::from_utf8_lossy(&message)
                )
            }
        }
    }

    fn expect_semantic_failure(call: &ToolCall, policy: PolicyEvaluatedFileTargets) -> Vec<u8> {
        let input = decode_test_mutation_input(call);
        match prepare(call, &input, policy).unwrap() {
            PrepareResult::Prepared(_) => panic!("unexpected prepared mutation"),
            PrepareResult::SemanticFailure(message) => message,
        }
    }

    struct TestPreparedMutation {
        input: FileMutationInput,
        policy: PolicyEvaluatedFileTargets,
        prepared: PreparedFileMutation,
    }

    fn prepare_for_apply(workspace: &[u8], call: &ToolCall) -> TestPreparedMutation {
        let input = decode_test_mutation_input(call);
        let policy = evaluate_policy(workspace, &input);
        let prepared = match prepare(call, &input, policy.clone()).unwrap() {
            PrepareResult::Prepared(prepared) => prepared,
            PrepareResult::SemanticFailure(message) => {
                panic!(
                    "unexpected semantic failure: {}",
                    String::from_utf8_lossy(&message)
                )
            }
        };
        TestPreparedMutation {
            input,
            policy,
            prepared,
        }
    }

    fn execution_authorization(prepared: TestPreparedMutation) -> FileExecutionAuthorization {
        FileExecutionAuthorization {
            input: prepared.input,
            policy_targets: prepared.policy,
            prepared: Some(prepared.prepared),
            grant_offer: None,
            approval_intent: None,
            read_disclosure_required: false,
        }
    }

    fn expect_apply_rejected(result: ApplyResult, reason: ApplyRejectionReason) -> ApplyRejection {
        match result {
            ApplyResult::Rejected(rejected) => {
                assert_eq!(rejected.reason, reason);
                rejected
            }
            ApplyResult::Committed(_) => panic!("unexpected committed mutation"),
        }
    }

    fn read_test_file(tmp: &Path, sub_path: &str) -> Vec<u8> {
        fs::read(tmp.join(sub_path)).unwrap()
    }

    #[test]
    fn prepare_derives_a_missing_write_without_creating_the_target() {
        let tmp = tmp_root();
        let root = workspace_root(&tmp);
        let arguments = write_arguments_json(b"missing/parents/new.txt", b"hello\n");
        let call = ToolCall::new("write-missing", "write_file", arguments);
        let input = FileMutationInput::Write(WriteInput {
            path: b"missing/parents/new.txt".to_vec(),
            content: b"hello\n".to_vec(),
        });
        let policy = evaluate_policy(&root, &input);
        let prepared = match prepare(&call, &input, policy.clone()).unwrap() {
            PrepareResult::Prepared(prepared) => prepared,
            PrepareResult::SemanticFailure(message) => {
                panic!("{}", String::from_utf8_lossy(&message))
            }
        };
        assert_eq!(prepared.kind, Kind::Write);
        assert!(matches!(prepared.preimage, Preimage::Absent));
        assert_eq!(prepared.after_content, b"hello\n");
        assert_eq!(prepared.display_path, b"missing/parents/new.txt");
        assert_eq!(prepared.target_path, policy.canonical_target_path);
        assert!(PolicyEvaluatedFileTargets::authorization_eql(
            &policy,
            &prepared.policy_targets
        ));
        assert_eq!(prepared.preview.additions, 1);
        assert_eq!(prepared.preview.deletions, 0);
        assert!(!tmp.join("missing").exists());
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn prepare_derives_an_existing_write_from_the_exact_reviewed_preimage() {
        let tmp = tmp_root();
        create_file(&tmp, "note.txt", b"old\n");
        let root = workspace_root(&tmp);
        let call = ToolCall::new(
            "write-existing",
            "write_file",
            write_arguments_json(b"note.txt", b"new\n"),
        );
        let prepared = expect_prepared(&call, evaluate_policy(&root, &decode_test_mutation_input(&call)));
        match &prepared.preimage {
            Preimage::Present {
                content,
                content_hash,
            } => {
                assert_eq!(content, b"old\n");
                assert_eq!(content_hash, &read_tracker::content_hash(b"old\n"));
            }
            Preimage::Absent => panic!("expected present preimage"),
        }
        assert_eq!(prepared.after_content, b"new\n");
        assert_eq!(prepared.preview.additions, 1);
        assert_eq!(prepared.preview.deletions, 1);
        assert_eq!(fs::read(tmp.join("note.txt")).unwrap(), b"old\n");
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn prepare_computes_one_exact_edit_occurrence() {
        let tmp = tmp_root();
        create_file(&tmp, "note.txt", b"alpha\nbeta\ngamma\n");
        let root = workspace_root(&tmp);
        let call = ToolCall::new(
            "edit-exact",
            "edit_file",
            edit_arguments_json(b"note.txt", b"beta", b"BETA"),
        );
        let prepared = expect_prepared(&call, evaluate_policy(&root, &decode_test_mutation_input(&call)));
        assert_eq!(prepared.kind, Kind::Edit);
        match &prepared.preimage {
            Preimage::Present { content, .. } => {
                assert_eq!(content, b"alpha\nbeta\ngamma\n");
            }
            Preimage::Absent => panic!("expected present"),
        }
        assert_eq!(prepared.after_content, b"alpha\nBETA\ngamma\n");
        assert_eq!(prepared.preview.additions, 1);
        assert_eq!(prepared.preview.deletions, 1);
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn prepare_preserves_exact_edit_semantic_failures() {
        let tmp = tmp_root();
        create_file(&tmp, "note.txt", b"same twice same\n");
        let root = workspace_root(&tmp);

        let identical_call = ToolCall::new(
            "edit-identical",
            "edit_file",
            edit_arguments_json(b"note.txt", b"same", b"same"),
        );
        assert_eq!(
            expect_semantic_failure(
                &identical_call,
                evaluate_policy(&root, &decode_test_mutation_input(&identical_call))
            ),
            b"edit_file failed: old_string and new_string are identical"
        );

        let missing_call = ToolCall::new(
            "edit-missing",
            "edit_file",
            edit_arguments_json(b"note.txt", b"missing", b"new"),
        );
        assert_eq!(
            expect_semantic_failure(
                &missing_call,
                evaluate_policy(&root, &decode_test_mutation_input(&missing_call))
            ),
            b"edit_file failed: old_string not found in file"
        );

        let duplicate_call = ToolCall::new(
            "edit-duplicate",
            "edit_file",
            edit_arguments_json(b"note.txt", b"same", b"new"),
        );
        assert_eq!(
            expect_semantic_failure(
                &duplicate_call,
                evaluate_policy(&root, &decode_test_mutation_input(&duplicate_call))
            ),
            b"edit_file failed: old_string is not unique (found 2 occurrences), provide more context"
        );
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn prepare_emits_explicit_empty_and_no_change_notices() {
        let tmp = tmp_root();
        create_file(&tmp, "same.txt", b"same\n");
        let root = workspace_root(&tmp);

        let empty_call = ToolCall::new(
            "write-empty",
            "write_file",
            write_arguments_json(b"empty.txt", b""),
        );
        let empty = expect_prepared(
            &empty_call,
            evaluate_policy(&root, &decode_test_mutation_input(&empty_call)),
        );
        assert_eq!(empty.preview.additions, 0);
        assert_eq!(empty.preview.deletions, 0);
        assert_eq!(empty.preview.lines.len(), 1);
        assert_eq!(empty.preview.lines[0].op, PreviewOp::Notice);
        assert_eq!(empty.preview.lines[0].text, b"empty file");

        let same_call = ToolCall::new(
            "write-same",
            "write_file",
            write_arguments_json(b"same.txt", b"same\n"),
        );
        let same = expect_prepared(
            &same_call,
            evaluate_policy(&root, &decode_test_mutation_input(&same_call)),
        );
        assert_eq!(same.preview.additions, 0);
        assert_eq!(same.preview.deletions, 0);
        assert_eq!(same.preview.lines[0].text, b"no content changes");
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn prepare_hashes_exact_raw_json_bytes() {
        let tmp = tmp_root();
        create_file(&tmp, "note.txt", b"same");
        let root = workspace_root(&tmp);
        let call_a = ToolCall::new(
            "hash-a",
            "write_file",
            br#"{"path":"note.txt","content":"same"}"#.to_vec(),
        );
        let call_b = ToolCall::new(
            "hash-b",
            "write_file",
            br#"{ "content": "same", "path": "note.txt" }"#.to_vec(),
        );
        let prepared_a = expect_prepared(
            &call_a,
            evaluate_policy(&root, &decode_test_mutation_input(&call_a)),
        );
        let prepared_b = expect_prepared(
            &call_b,
            evaluate_policy(&root, &decode_test_mutation_input(&call_b)),
        );
        assert_eq!(
            prepared_a.arguments_hash,
            read_tracker::content_hash(&call_a.arguments_json)
        );
        assert_ne!(prepared_a.arguments_hash, prepared_b.arguments_hash);
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn prepare_rejects_call_target_and_authority_identity_mismatches() {
        let tmp = tmp_root();
        create_file(&tmp, "a.txt", b"a");
        create_file(&tmp, "b.txt", b"b");
        let root = workspace_root(&tmp);
        let call_a = ToolCall::new(
            "target-a",
            "write_file",
            write_arguments_json(b"a.txt", b"new"),
        );
        let policy_a = evaluate_policy(&root, &decode_test_mutation_input(&call_a));
        let call_b = ToolCall::new(
            "target-b",
            "write_file",
            write_arguments_json(b"b.txt", b"new"),
        );
        assert_eq!(
            expect_semantic_failure(&call_b, policy_a.clone()),
            TARGET_MISMATCH_MESSAGE
        );

        let mut changed_anchor = policy_a.clone();
        changed_anchor.anchor.identity.inode = changed_anchor.anchor.identity.inode.wrapping_add(1);
        assert_eq!(
            expect_semantic_failure(&call_a, changed_anchor),
            IDENTITY_CHANGED_MESSAGE
        );

        create_file(&tmp, "replacement.txt", b"replacement");
        fs::rename(tmp.join("replacement.txt"), tmp.join("a.txt")).unwrap();
        assert_eq!(
            expect_semantic_failure(&call_a, policy_a),
            IDENTITY_CHANGED_MESSAGE
        );
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn prepare_rejects_a_corrupted_permission_target_proof() {
        let tmp = tmp_root();
        let root = workspace_root(&tmp);
        let call = ToolCall::new(
            "corrupted-proof-prepare",
            "write_file",
            write_arguments_json(b"new.txt", b"new"),
        );
        let mut policy = evaluate_policy(&root, &decode_test_mutation_input(&call));
        policy.anchor.relative_components[0].start = policy.anchor.path_end;
        assert_eq!(
            expect_semantic_failure(&call, policy),
            TARGET_MISMATCH_MESSAGE
        );
        assert!(!tmp.join("new.txt").exists());
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn prepare_retains_full_review_separately_from_the_bounded_approval_preview() {
        let tmp = tmp_root();
        let root = workspace_root(&tmp);
        let mut content = Vec::new();
        for line_number in 1..31 {
            content.extend_from_slice(format!("line {line_number}\n").as_bytes());
        }
        let call = ToolCall::new(
            "write-full-review",
            "write_file",
            write_arguments_json(b"full-review.txt", &content),
        );
        let prepared = expect_prepared(
            &call,
            evaluate_policy(&root, &decode_test_mutation_input(&call)),
        );
        assert!(prepared.preview.lines.len() <= diff::MAX_PREVIEW_LINES);
        let viewport = prepared.review.viewport(24, 6);
        assert_eq!(viewport.total_rows, 30);
        assert_eq!(viewport.lines[0].text, b"line 25");
        assert_eq!(viewport.lines[5].text, b"line 30");
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn apply_atomically_installs_reviewed_bytes_and_returns_typed_commit_metadata() {
        let tmp = tmp_root();
        let root = workspace_root(&tmp);
        let call = ToolCall::new(
            "apply-write",
            "write_file",
            write_arguments_json(b"nested/new.txt", b"reviewed\n"),
        );
        let prepared = prepare_for_apply(&root, &call);
        let target_path = prepared.prepared.target_path.clone();
        let kind = prepared.prepared.kind;
        let additions = prepared.prepared.preview.additions;
        let cancel_flag = AtomicBool::new(false);
        let result = apply(&call, execution_authorization(prepared), &cancel_flag).unwrap();
        let committed = match result {
            ApplyResult::Committed(handoff) => handoff,
            ApplyResult::Rejected(_) => panic!("unexpected rejected mutation"),
        };
        assert_eq!(committed.tracker.raw_path, target_path);
        assert_eq!(committed.tracker.kind, kind);
        assert!(committed.tracker.previous_content.is_none());
        assert_eq!(committed.preview.additions, additions);
        assert_eq!(read_test_file(&tmp, "nested/new.txt"), b"reviewed\n");
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn apply_rejects_binding_mismatches_before_allocation_or_filesystem_access() {
        let tmp = tmp_root();
        let root = workspace_root(&tmp);
        let call = ToolCall::new(
            "binding",
            "write_file",
            write_arguments_json(b"new.txt", b"new"),
        );
        let prepared = prepare_for_apply(&root, &call);
        let cancel_flag = AtomicBool::new(false);
        let authorization = FileExecutionAuthorization {
            input: prepared.input,
            policy_targets: prepared.policy,
            prepared: None,
            grant_offer: None,
            approval_intent: None,
            read_disclosure_required: false,
        };
        let rejected = expect_apply_rejected(
            apply(&call, authorization, &cancel_flag).unwrap(),
            ApplyRejectionReason::BindingMismatch,
        );
        assert!(rejected.created_parent_residue.is_empty());
        assert!(!tmp.join("new.txt").exists());
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn apply_rejects_stale_absent_targets_and_stale_existing_preimages() {
        let tmp = tmp_root();
        create_file(&tmp, "existing.txt", b"old");
        let root = workspace_root(&tmp);
        let missing_call = ToolCall::new(
            "stale-absent",
            "write_file",
            write_arguments_json(b"missing.txt", b"new"),
        );
        let missing_prepared = prepare_for_apply(&root, &missing_call);
        create_file(&tmp, "missing.txt", b"raced");
        let cancel_flag = AtomicBool::new(false);
        let _ = expect_apply_rejected(
            apply(
                &missing_call,
                execution_authorization(missing_prepared),
                &cancel_flag,
            )
            .unwrap(),
            ApplyRejectionReason::StalePreimage,
        );
        assert_eq!(read_test_file(&tmp, "missing.txt"), b"raced");

        let existing_call = ToolCall::new(
            "stale-existing",
            "write_file",
            write_arguments_json(b"existing.txt", b"new"),
        );
        let existing_prepared = prepare_for_apply(&root, &existing_call);
        fs::write(tmp.join("existing.txt"), b"changed").unwrap();
        let _ = expect_apply_rejected(
            apply(
                &existing_call,
                execution_authorization(existing_prepared),
                &cancel_flag,
            )
            .unwrap(),
            ApplyRejectionReason::StalePreimage,
        );
        assert_eq!(read_test_file(&tmp, "existing.txt"), b"changed");
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn apply_rejects_replaced_anchors_and_existing_parents() {
        let tmp = tmp_root();
        fs::create_dir_all(tmp.join("workspace/parent")).unwrap();
        create_file(&tmp, "workspace/parent/note.txt", b"old");
        let workspace = path_to_bytes(&fs::canonicalize(tmp.join("workspace")).unwrap());
        let call = ToolCall::new(
            "replace-anchor",
            "write_file",
            write_arguments_json(b"parent/note.txt", b"new"),
        );
        let prepared = prepare_for_apply(&workspace, &call);
        fs::rename(tmp.join("workspace"), tmp.join("workspace-old")).unwrap();
        fs::create_dir_all(tmp.join("workspace")).unwrap();
        let cancel_flag = AtomicBool::new(false);
        let _ = expect_apply_rejected(
            apply(&call, execution_authorization(prepared), &cancel_flag).unwrap(),
            ApplyRejectionReason::TraversalChanged,
        );
        assert_eq!(read_test_file(&tmp, "workspace-old/parent/note.txt"), b"old");

        fs::rename(tmp.join("workspace"), tmp.join("workspace-empty")).unwrap();
        fs::rename(tmp.join("workspace-old"), tmp.join("workspace")).unwrap();
        let parent_call = ToolCall::new(
            "replace-parent",
            "write_file",
            write_arguments_json(b"parent/note.txt", b"new"),
        );
        let parent_prepared = prepare_for_apply(&workspace, &parent_call);
        fs::rename(
            tmp.join("workspace/parent"),
            tmp.join("workspace/parent-old"),
        )
        .unwrap();
        fs::create_dir_all(tmp.join("workspace/parent")).unwrap();
        let _ = expect_apply_rejected(
            apply(
                &parent_call,
                execution_authorization(parent_prepared),
                &cancel_flag,
            )
            .unwrap(),
            ApplyRejectionReason::TraversalChanged,
        );
        assert_eq!(
            read_test_file(&tmp, "workspace/parent-old/note.txt"),
            b"old"
        );
        let _ = fs::remove_dir_all(&tmp);
    }

    fn expect_terminal_safe(text: &[u8]) {
        assert!(std::str::from_utf8(text).is_ok());
        assert!(!text.contains(&0x1b));
        assert!(!text.contains(&b'\n'));
        assert!(!text.contains(&b'\r'));
    }

    fn expect_no_stage_files(root: &[u8]) {
        let root_path = path_from_bytes(root);
        for entry in walkdir(&root_path) {
            let name = entry.file_name().to_string_lossy().into_owned();
            assert!(
                !name.contains(".fx-stage-"),
                "unexpected stage file {name}"
            );
        }
    }

    fn walkdir(root: &Path) -> Vec<std::fs::DirEntry> {
        let mut out = Vec::new();
        fn rec(path: &Path, out: &mut Vec<std::fs::DirEntry>) {
            let Ok(entries) = fs::read_dir(path) else {
                return;
            };
            for entry in entries.flatten() {
                let ty = entry.file_type().ok();
                if ty.as_ref().is_some_and(|kind| kind.is_dir()) {
                    rec(&entry.path(), out);
                }
                out.push(entry);
            }
        }
        rec(root, &mut out);
        out
    }

    fn share_prepared(src: &PreparedFileMutation) -> PreparedFileMutation {
        let before = match &src.preimage {
            Preimage::Absent => b"".as_slice(),
            Preimage::Present { content, .. } => content.as_slice(),
        };
        PreparedFileMutation {
            call_id: src.call_id.clone(),
            tool_name: src.tool_name.clone(),
            arguments_hash: src.arguments_hash,
            target_path: src.target_path.clone(),
            display_path: src.display_path.clone(),
            policy_targets: src.policy_targets.clone(),
            kind: src.kind,
            preimage: src.preimage.clone(),
            after_content: src.after_content.clone(),
            preview: src.preview.clone(),
            review: FileReview::init(before, &src.after_content).unwrap(),
            commit_token: Arc::clone(&src.commit_token),
        }
    }

    fn apply_controls(
        call: &ToolCall,
        prepared: TestPreparedMutation,
        cancel_flag: &AtomicBool,
        controls: ApplyTestControls<'_>,
    ) -> ApplyResult {
        apply_with_test_controls(
            call,
            execution_authorization(prepared),
            cancel_flag,
            controls,
        )
        .unwrap()
    }

    #[test]
    fn prepare_shows_the_canonical_external_target_when_a_symlink_redirects_outside_the_workspace() {
        let tmp = tmp_root();
        fs::create_dir_all(tmp.join("workspace")).unwrap();
        fs::create_dir_all(tmp.join("outside")).unwrap();
        if std::os::unix::fs::symlink("../outside", tmp.join("workspace/link")).is_err() {
            let _ = fs::remove_dir_all(&tmp);
            return;
        }
        let workspace = path_to_bytes(&fs::canonicalize(tmp.join("workspace")).unwrap());
        let misleading = {
            let mut path = workspace.clone();
            if !path.ends_with(&[b'/']) {
                path.push(b'/');
            }
            path.extend_from_slice(b"link/new.txt");
            path
        };
        let call = ToolCall::new(
            "write-symlink-redirect",
            "write_file",
            write_arguments_json(&misleading, b"hello\n"),
        );
        let prepared = expect_prepared(
            &call,
            evaluate_policy(&workspace, &decode_test_mutation_input(&call)),
        );
        assert_eq!(
            prepared.policy_targets.anchor.scope,
            TraversalScope::External
        );
        assert_eq!(prepared.display_path, prepared.policy_targets.canonical_target_path);
        assert_eq!(prepared.display_path, prepared.preview.path);
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn prepare_terminal_encodes_hostile_path_and_preview_content() {
        let tmp = tmp_root();
        let root = workspace_root(&tmp);
        let hostile_path = b"name\x1b[31m\nfile.txt";
        let hostile_content = b"line\x1b[31mred\x1b[0m\n";
        let call = ToolCall::new(
            "write-hostile",
            "write_file",
            write_arguments_json(hostile_path, hostile_content),
        );
        let prepared = expect_prepared(
            &call,
            evaluate_policy(&root, &decode_test_mutation_input(&call)),
        );
        expect_terminal_safe(&prepared.display_path);
        assert_ne!(prepared.display_path, hostile_path);
        for line in &prepared.preview.lines {
            expect_terminal_safe(&line.text);
        }
        assert!(!tmp.join("name\x1b[31m\nfile.txt").exists());
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn prepare_enforces_independent_preimage_and_postimage_caps() {
        let tmp = tmp_root();
        let root = workspace_root(&tmp);
        let oversized = vec![b'x'; max_content_bytes + 1];
        create_file(&tmp, "oversized.txt", &oversized);
        let preimage_call = ToolCall::new(
            "oversized-preimage",
            "write_file",
            write_arguments_json(b"oversized.txt", b"new"),
        );
        assert_eq!(
            expect_semantic_failure(
                &preimage_call,
                evaluate_policy(&root, &decode_test_mutation_input(&preimage_call))
            ),
            b"file mutation preparation failed: preimage exceeds the 4 MiB preparation limit"
        );

        let mut maximum = vec![b'x'; max_content_bytes];
        maximum[0] = b'a';
        create_file(&tmp, "maximum.txt", &maximum);
        let postimage_call = ToolCall::new(
            "oversized-postimage",
            "edit_file",
            edit_arguments_json(b"maximum.txt", b"a", b"aa"),
        );
        assert_eq!(
            expect_semantic_failure(
                &postimage_call,
                evaluate_policy(&root, &decode_test_mutation_input(&postimage_call))
            ),
            b"edit_file failed: postimage exceeds the 4 MiB preparation limit"
        );
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn prepare_rejects_an_intermediate_directory_retarget() {
        let tmp = tmp_root();
        fs::create_dir_all(tmp.join("approved")).unwrap();
        fs::create_dir_all(tmp.join("other")).unwrap();
        create_file(&tmp, "approved/note.txt", b"approved");
        create_file(&tmp, "other/note.txt", b"other");
        let root = workspace_root(&tmp);
        let call = ToolCall::new(
            "retarget",
            "edit_file",
            edit_arguments_json(b"approved/note.txt", b"approved", b"new"),
        );
        let policy = evaluate_policy(&root, &decode_test_mutation_input(&call));
        fs::rename(tmp.join("approved"), tmp.join("approved-old")).unwrap();
        std::os::unix::fs::symlink("other", tmp.join("approved")).unwrap();
        assert_eq!(
            expect_semantic_failure(&call, policy),
            TARGET_MISMATCH_MESSAGE
        );
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn apply_installs_reviewed_bytes_beneath_an_authorized_external_anchor() {
        let tmp = tmp_root();
        fs::create_dir_all(tmp.join("workspace")).unwrap();
        fs::create_dir_all(tmp.join("external")).unwrap();
        let workspace = path_to_bytes(&fs::canonicalize(tmp.join("workspace")).unwrap());
        let external = path_to_bytes(&fs::canonicalize(tmp.join("external")).unwrap());
        let mut target_path = external.clone();
        if !target_path.ends_with(&[b'/']) {
            target_path.push(b'/');
        }
        target_path.extend_from_slice(b"nested/new.txt");
        let call = ToolCall::new(
            "apply-external",
            "write_file",
            write_arguments_json(&target_path, b"reviewed\n"),
        );
        let prepared = prepare_for_apply(&workspace, &call);
        assert_eq!(prepared.policy.anchor.scope, TraversalScope::External);
        let cancel_flag = AtomicBool::new(false);
        let result = apply(&call, execution_authorization(prepared), &cancel_flag).unwrap();
        assert!(matches!(result, ApplyResult::Committed(_)));
        assert_eq!(read_test_file(&tmp, "external/nested/new.txt"), b"reviewed\n");
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn apply_rejects_a_corrupted_prepared_policy_proof_before_mutation() {
        let tmp = tmp_root();
        let root = workspace_root(&tmp);
        let call = ToolCall::new(
            "corrupted-proof-apply",
            "write_file",
            write_arguments_json(b"new.txt", b"new"),
        );
        let mut prepared = prepare_for_apply(&root, &call);
        prepared.policy.anchor.relative_components[0].start = prepared.policy.anchor.path_end;
        prepared.prepared.policy_targets.anchor.relative_components[0].start =
            prepared.prepared.policy_targets.anchor.path_end;
        let cancel_flag = AtomicBool::new(false);
        let rejected = expect_apply_rejected(
            apply(&call, execution_authorization(prepared), &cancel_flag).unwrap(),
            ApplyRejectionReason::BindingMismatch,
        );
        assert!(rejected.created_parent_residue.is_empty());
        assert!(!tmp.join("new.txt").exists());
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn apply_rejects_replaced_staged_sources_without_deleting_foreign_replacements() {
        let tmp = tmp_root();
        let root = workspace_root(&tmp);
        let call = ToolCall::new(
            "replace-stage",
            "write_file",
            write_arguments_json(b"new.txt", b"reviewed"),
        );
        let prepared = prepare_for_apply(&root, &call);
        let cancel_flag = AtomicBool::new(false);
        let mut after_stage = |parent: &Path, temp_name: &[u8]| {
            let stage = join_dir(parent, temp_name);
            fs::rename(&stage, parent.join(".removed-stage")).unwrap();
            fs::remove_file(parent.join(".removed-stage")).unwrap();
            fs::write(join_dir(parent, temp_name), b"foreign").unwrap();
            Ok(())
        };
        let _ = expect_apply_rejected(
            apply_controls(
                &call,
                prepared,
                &cancel_flag,
                ApplyTestControls {
                    after_write_chunk: None,
                    after_stage: Some(&mut after_stage),
                    after_final_preimage_read: None,
                    after_final_validation: None,
                },
            ),
            ApplyRejectionReason::StagedSourceChanged,
        );
        assert!(!tmp.join("new.txt").exists());
        let mut found_foreign = false;
        for entry in fs::read_dir(&tmp).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name();
            if name.to_string_lossy().starts_with(".fx-stage-") {
                assert_eq!(fs::read(entry.path()).unwrap(), b"foreign");
                found_foreign = true;
            }
        }
        assert!(found_foreign);
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn apply_rejects_in_place_staged_content_changes_before_rename() {
        let tmp = tmp_root();
        let root = workspace_root(&tmp);
        let call = ToolCall::new(
            "mutate-stage",
            "write_file",
            write_arguments_json(b"new.txt", b"reviewed"),
        );
        let prepared = prepare_for_apply(&root, &call);
        let cancel_flag = AtomicBool::new(false);
        let mut after_stage = |parent: &Path, temp_name: &[u8]| {
            fs::write(
                join_dir(parent, temp_name),
                b"tampered",
            )
            .unwrap();
            Ok(())
        };
        let _ = expect_apply_rejected(
            apply_controls(
                &call,
                prepared,
                &cancel_flag,
                ApplyTestControls {
                    after_write_chunk: None,
                    after_stage: Some(&mut after_stage),
                    after_final_preimage_read: None,
                    after_final_validation: None,
                },
            ),
            ApplyRejectionReason::StagedSourceChanged,
        );
        assert!(!tmp.join("new.txt").exists());
        expect_no_stage_files(&root);
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn apply_rejects_a_target_pathname_replaced_during_final_preimage_validation() {
        let tmp = tmp_root();
        create_file(&tmp, "existing.txt", b"old");
        let root = workspace_root(&tmp);
        let call = ToolCall::new(
            "replace-target-during-read",
            "write_file",
            write_arguments_json(b"existing.txt", b"new"),
        );
        let prepared = prepare_for_apply(&root, &call);
        let cancel_flag = AtomicBool::new(false);
        let mut after_read = |parent: &Path, target_name: &[u8]| {
            let target = join_dir(parent, target_name);
            fs::rename(&target, parent.join("reviewed-target")).unwrap();
            fs::write(&target, b"old").unwrap();
            Ok(())
        };
        let _ = expect_apply_rejected(
            apply_controls(
                &call,
                prepared,
                &cancel_flag,
                ApplyTestControls {
                    after_write_chunk: None,
                    after_stage: None,
                    after_final_preimage_read: Some(&mut after_read),
                    after_final_validation: None,
                },
            ),
            ApplyRejectionReason::StalePreimage,
        );
        assert_eq!(read_test_file(&tmp, "existing.txt"), b"old");
        assert_eq!(read_test_file(&tmp, "reviewed-target"), b"old");
        expect_no_stage_files(&root);
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn apply_rejects_a_parent_moved_during_final_preimage_validation() {
        let tmp = tmp_root();
        fs::create_dir_all(tmp.join("parent")).unwrap();
        create_file(&tmp, "parent/existing.txt", b"old");
        let root = workspace_root(&tmp);
        let call = ToolCall::new(
            "move-parent-during-final-read",
            "write_file",
            write_arguments_json(b"parent/existing.txt", b"new"),
        );
        let prepared = prepare_for_apply(&root, &call);
        let cancel_flag = AtomicBool::new(false);
        let tmp_for_hook = tmp.clone();
        let mut reached = false;
        let mut after_read = |_parent: &Path, _target_name: &[u8]| {
            reached = true;
            fs::rename(tmp_for_hook.join("parent"), tmp_for_hook.join("moved-parent")).unwrap();
            fs::create_dir_all(tmp_for_hook.join("parent")).unwrap();
            fs::write(tmp_for_hook.join("parent/existing.txt"), b"replacement").unwrap();
            Ok(())
        };
        let _ = expect_apply_rejected(
            apply_controls(
                &call,
                prepared,
                &cancel_flag,
                ApplyTestControls {
                    after_write_chunk: None,
                    after_stage: None,
                    after_final_preimage_read: Some(&mut after_read),
                    after_final_validation: None,
                },
            ),
            ApplyRejectionReason::TraversalChanged,
        );
        assert!(reached);
        assert_eq!(read_test_file(&tmp, "parent/existing.txt"), b"replacement");
        assert_eq!(read_test_file(&tmp, "moved-parent/existing.txt"), b"old");
        expect_no_stage_files(&root);
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn apply_commits_through_a_parent_moved_after_final_validation() {
        let tmp = tmp_root();
        fs::create_dir_all(tmp.join("parent")).unwrap();
        create_file(&tmp, "parent/existing.txt", b"old");
        let root = workspace_root(&tmp);
        let call = ToolCall::new(
            "move-parent-after-final-validation",
            "write_file",
            write_arguments_json(b"parent/existing.txt", b"new"),
        );
        let prepared = prepare_for_apply(&root, &call);
        let cancel_flag = AtomicBool::new(false);
        let tmp_for_hook = tmp.clone();
        let mut reached = false;
        let mut after_final = || {
            reached = true;
            fs::rename(tmp_for_hook.join("parent"), tmp_for_hook.join("moved-parent")).unwrap();
            fs::create_dir_all(tmp_for_hook.join("parent")).unwrap();
            fs::write(tmp_for_hook.join("parent/existing.txt"), b"replacement").unwrap();
            Ok(())
        };
        let result = apply_controls(
            &call,
            prepared,
            &cancel_flag,
            ApplyTestControls {
                after_write_chunk: None,
                after_stage: None,
                after_final_preimage_read: None,
                after_final_validation: Some(&mut after_final),
            },
        );
        let committed = match result {
            ApplyResult::Committed(handoff) => handoff,
            ApplyResult::Rejected(_) => panic!("unexpected rejected mutation"),
        };
        assert!(reached);
        assert_eq!(committed.preview.path, b"parent/existing.txt");
        assert_eq!(read_test_file(&tmp, "parent/existing.txt"), b"replacement");
        assert_eq!(read_test_file(&tmp, "moved-parent/existing.txt"), b"new");
        expect_no_stage_files(&root);
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn apply_observes_cancellation_between_bounded_stage_writes() {
        let tmp = tmp_root();
        let root = workspace_root(&tmp);
        let content = vec![b'x'; 128 * 1024];
        let call = ToolCall::new(
            "cancel-stage-chunk",
            "write_file",
            write_arguments_json(b"one/two/new.txt", &content),
        );
        let prepared = prepare_for_apply(&root, &call);
        let cancel_flag = AtomicBool::new(false);
        let mut chunk_count = 0usize;
        let mut after_chunk = |_offset: usize| {
            chunk_count += 1;
            if chunk_count == 1 {
                cancel_flag.store(true, Ordering::SeqCst);
            }
            Ok(())
        };
        let rejected = expect_apply_rejected(
            apply_controls(
                &call,
                prepared,
                &cancel_flag,
                ApplyTestControls {
                    after_write_chunk: Some(&mut after_chunk),
                    after_stage: None,
                    after_final_preimage_read: None,
                    after_final_validation: None,
                },
            ),
            ApplyRejectionReason::Cancelled,
        );
        assert_eq!(chunk_count, 1);
        assert!(rejected.created_parent_residue.is_empty());
        assert!(!tmp.join("one").exists());
        expect_no_stage_files(&root);
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn apply_rejects_replacement_of_transaction_created_parent_directories() {
        for (id, replacement) in [
            ("replace-created-shallow", "shallow"),
            ("replace-created-deep", "deep"),
        ] {
            let tmp = tmp_root();
            let root = workspace_root(&tmp);
            let call = ToolCall::new(
                id,
                "write_file",
                write_arguments_json(b"one/two/new.txt", b"reviewed"),
            );
            let prepared = prepare_for_apply(&root, &call);
            let cancel_flag = AtomicBool::new(false);
            let tmp_for_hook = tmp.clone();
            let mut after_stage = |_parent: &Path, _temp_name: &[u8]| {
                match replacement {
                    "shallow" => {
                        fs::rename(tmp_for_hook.join("one"), tmp_for_hook.join("one-original"))
                            .unwrap();
                        fs::create_dir_all(tmp_for_hook.join("one")).unwrap();
                    }
                    _ => {
                        fs::rename(
                            tmp_for_hook.join("one/two"),
                            tmp_for_hook.join("one/two-original"),
                        )
                        .unwrap();
                        fs::create_dir_all(tmp_for_hook.join("one/two")).unwrap();
                    }
                }
                Ok(())
            };
            let rejected = expect_apply_rejected(
                apply_controls(
                    &call,
                    prepared,
                    &cancel_flag,
                    ApplyTestControls {
                        after_write_chunk: None,
                        after_stage: Some(&mut after_stage),
                        after_final_preimage_read: None,
                        after_final_validation: None,
                    },
                ),
                ApplyRejectionReason::TraversalChanged,
            );
            assert_eq!(rejected.created_parent_residue.len(), 2);
            assert!(
                rejected.created_parent_residue[0].component_index
                    > rejected.created_parent_residue[1].component_index
            );
            assert!(!tmp.join("one/two/new.txt").exists());
            let moved_target = if replacement == "shallow" {
                tmp.join("one-original/two/new.txt")
            } else {
                tmp.join("one/two-original/new.txt")
            };
            assert!(!moved_target.exists());
            expect_no_stage_files(&root);
            let _ = fs::remove_dir_all(&tmp);
        }
    }

    #[test]
    fn apply_cancellation_removes_its_temp_and_newly_created_parents() {
        let tmp = tmp_root();
        create_file(&tmp, "keep.txt", b"keep");
        let root = workspace_root(&tmp);
        let call = ToolCall::new(
            "cancel-clean",
            "write_file",
            write_arguments_json(b"one/two/new.txt", b"reviewed"),
        );
        let prepared = prepare_for_apply(&root, &call);
        let cancel_flag = AtomicBool::new(false);
        let mut after_stage = |_parent: &Path, _temp_name: &[u8]| {
            cancel_flag.store(true, Ordering::SeqCst);
            Ok(())
        };
        let rejected = expect_apply_rejected(
            apply_controls(
                &call,
                prepared,
                &cancel_flag,
                ApplyTestControls {
                    after_write_chunk: None,
                    after_stage: Some(&mut after_stage),
                    after_final_preimage_read: None,
                    after_final_validation: None,
                },
            ),
            ApplyRejectionReason::Cancelled,
        );
        assert!(rejected.created_parent_residue.is_empty());
        assert_eq!(read_test_file(&tmp, "keep.txt"), b"keep");
        assert!(!tmp.join("one").exists());
        expect_no_stage_files(&root);
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn apply_returns_deepest_first_bounded_residue_when_created_parents_are_not_empty() {
        let tmp = tmp_root();
        let root = workspace_root(&tmp);
        let call = ToolCall::new(
            "cancel-residue",
            "write_file",
            write_arguments_json(b"one/two/new.txt", b"reviewed"),
        );
        let prepared = prepare_for_apply(&root, &call);
        let cancel_flag = AtomicBool::new(false);
        let mut after_stage = |parent: &Path, _temp_name: &[u8]| {
            fs::write(parent.join("blocker"), b"").unwrap();
            cancel_flag.store(true, Ordering::SeqCst);
            Ok(())
        };
        let rejected = expect_apply_rejected(
            apply_controls(
                &call,
                prepared,
                &cancel_flag,
                ApplyTestControls {
                    after_write_chunk: None,
                    after_stage: Some(&mut after_stage),
                    after_final_preimage_read: None,
                    after_final_validation: None,
                },
            ),
            ApplyRejectionReason::Cancelled,
        );
        assert_eq!(rejected.created_parent_residue.len(), 2);
        assert!(
            rejected.created_parent_residue[0].component_index
                > rejected.created_parent_residue[1].component_index
        );
        for residue in &rejected.created_parent_residue {
            assert_eq!(residue.reason, ParentCleanupResidueReason::NotEmpty);
            assert!(residue.observed_identity.is_some());
        }
        assert_eq!(read_test_file(&tmp, "one/two/blocker"), b"");
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn apply_preserves_the_existing_destination_mode() {
        let tmp = tmp_root();
        create_file(&tmp, "mode.txt", b"old");
        {
            use std::os::unix::fs::PermissionsExt;
            let path = tmp.join("mode.txt");
            let mut perms = fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o640);
            fs::set_permissions(&path, perms).unwrap();
        }
        let root = workspace_root(&tmp);
        let call = ToolCall::new(
            "mode",
            "write_file",
            write_arguments_json(b"mode.txt", b"new"),
        );
        let prepared = prepare_for_apply(&root, &call);
        let cancel_flag = AtomicBool::new(false);
        let result = apply(&call, execution_authorization(prepared), &cancel_flag).unwrap();
        assert!(matches!(result, ApplyResult::Committed(_)));
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(tmp.join("mode.txt")).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o640);
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn approved_mutation_cannot_apply_twice() {
        let tmp = tmp_root();
        let root = workspace_root(&tmp);
        let call = ToolCall::new(
            "single-use-proof",
            "write_file",
            write_arguments_json(b"new.txt", b"reviewed"),
        );
        let prepared = prepare_for_apply(&root, &call);
        let second = share_prepared(&prepared.prepared);
        let input2 = prepared.input.clone();
        let policy2 = prepared.policy.clone();
        let cancel_flag = AtomicBool::new(false);
        let first = apply(&call, execution_authorization(prepared), &cancel_flag).unwrap();
        assert!(matches!(first, ApplyResult::Committed(_)));
        let second_auth = FileExecutionAuthorization {
            input: input2,
            policy_targets: policy2,
            prepared: Some(second),
            grant_offer: None,
            approval_intent: None,
            read_disclosure_required: false,
        };
        match apply(&call, second_auth, &cancel_flag) {
            Err(err) => assert_eq!(err, FileMutationApplyError::ResultContractViolation),
            Ok(_) => panic!("expected result contract violation"),
        }
        assert_eq!(read_test_file(&tmp, "new.txt"), b"reviewed");
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn apply_commit_handoff_remains_valid_after_the_call_owner_is_destroyed() {
        let tmp = tmp_root();
        let root = workspace_root(&tmp);
        let call = ToolCall::new(
            "result-lifetime",
            "write_file",
            write_arguments_json(b"new.txt", b"reviewed"),
        );
        let prepared = prepare_for_apply(&root, &call);
        let cancel_flag = AtomicBool::new(false);
        let result = apply(&call, execution_authorization(prepared), &cancel_flag).unwrap();
        drop(call);
        let committed = match result {
            ApplyResult::Committed(handoff) => handoff,
            ApplyResult::Rejected(_) => panic!("unexpected rejected mutation"),
        };
        assert_eq!(committed.preview.path, b"new.txt");
        assert!(committed.tracker.raw_path.ends_with(b"new.txt"));
        assert_eq!(committed.tracker.kind, Kind::Write);
        let _ = fs::remove_dir_all(&tmp);
    }
}
