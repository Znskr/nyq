//! Undo stack for workspace file mutations.
//!
//! Every captured operation records a full preimage so undo can restore a
//! file atomically without destroying what is already there until the
//! replacement is durable.

use std::fs;
use std::path::Path;

use crate::core::shared::{debug_trace, io as shared_io};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OperationKind {
    Write,
    Edit,
}

impl OperationKind {
    fn label(self) -> &'static str {
        match self {
            OperationKind::Write => "write",
            OperationKind::Edit => "edit",
        }
    }
}

pub struct FileOperation {
    pub kind: OperationKind,
    pub path: Vec<u8>,
    pub previous_content: Option<Vec<u8>>,
    pub timestamp_ms: i64,
}

pub enum UndoResult {
    Restored(Vec<u8>),
    Deleted(Vec<u8>),
    /// The operation was consumed but could not be reversed, either because
    /// its preimage was never captured or because restoring it failed. Undo
    /// must not guess, and must not report this as a restore.
    Unavailable(Vec<u8>),
    Empty,
}

pub struct ChangeTracker {
    stack: Vec<FileOperation>,
}

impl ChangeTracker {
    pub const max_stack_size: usize = 100;

    pub fn init() -> ChangeTracker {
        ChangeTracker { stack: Vec::new() }
    }

    pub fn deinit(&mut self) {
        self.stack.clear();
    }

    pub fn clear(&mut self) {
        self.stack.clear();
    }

    pub fn push_operation(&mut self, op: FileOperation) {
        if self.stack.len() >= Self::max_stack_size {
            self.stack.remove(0);
        }
        self.stack.push(op);
    }

    pub fn undo_last(&mut self) -> UndoResult {
        let op = match self.stack.pop() {
            Some(op) => op,
            None => return UndoResult::Empty,
        };
        let FileOperation {
            kind,
            path,
            previous_content,
            timestamp_ms: _,
        } = op;

        match kind {
            OperationKind::Write | OperationKind::Edit => {
                if let Some(content) = previous_content {
                    if let Err(err) = restore_content(&path, &content) {
                        trace_unavailable(kind, &path, b"restore_content", error_label(&err));
                        return UndoResult::Unavailable(path);
                    }
                    return UndoResult::Restored(path);
                }

                match delete_created(&path) {
                    Ok(()) | Err(DeleteError::NotFound) => UndoResult::Deleted(path),
                    Err(DeleteError::Other(err)) => {
                        trace_unavailable(kind, &path, b"delete_created", io_error_name(&err));
                        UndoResult::Unavailable(path)
                    }
                }
            }
        }
    }
}

enum DeleteError {
    NotFound,
    Other(std::io::Error),
}

fn delete_created(path: &[u8]) -> Result<(), DeleteError> {
    let path = path_from_bytes(path).ok_or_else(|| {
        DeleteError::Other(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "non-utf8 path",
        ))
    })?;
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Err(DeleteError::NotFound),
        Err(err) => Err(DeleteError::Other(err)),
    }
}

/// Restores `content` at `absolute_path` without destroying what is already
/// there until the replacement is durable. A failed restore leaves the current
/// file untouched and is reported to the caller rather than swallowed.
fn restore_content(absolute_path: &[u8], content: &[u8]) -> shared_io::Result<()> {
    let path = path_from_bytes(absolute_path).ok_or(shared_io::Error::InvalidPath)?;
    shared_io::write_file_atomic(path, content)
}

fn path_from_bytes(bytes: &[u8]) -> Option<&Path> {
    std::str::from_utf8(bytes).ok().map(Path::new)
}

fn trace_unavailable(kind: OperationKind, path: &[u8], stage: &[u8], err_name: &str) {
    let mut path_buf = [0u8; 256];
    let preview = debug_trace::terminal_preview(&mut path_buf, path);
    debug_trace::eventf(
        "undo",
        "undo_unavailable",
        debug_trace::TraceContext::default(),
        format_args!(
            "kind={} path={} stage={} error={}",
            kind.label(),
            String::from_utf8_lossy(preview),
            String::from_utf8_lossy(stage),
            err_name
        ),
    );
}

fn error_label(err: &shared_io::Error) -> &'static str {
    match err {
        shared_io::Error::Io(inner) => io_error_name(inner),
        shared_io::Error::InvalidPath => "invalid_path",
        shared_io::Error::DurablePathUnsafe => "durable_path_unsafe",
        shared_io::Error::StreamTooLong => "stream_too_long",
        shared_io::Error::EnvironmentUnavailable => "environment_unavailable",
        shared_io::Error::HandlePathUnavailable => "handle_path_unavailable",
        shared_io::Error::PrivateStatePermissionsUnsupported => {
            "private_state_permissions_unsupported"
        }
        shared_io::Error::DurableReplacePreRenameFailed => "durable_replace_pre_rename_failed",
        shared_io::Error::DurableReplacePostRenameFailed => "durable_replace_post_rename_failed",
        shared_io::Error::LockBusy => "lock_busy",
        shared_io::Error::LockUnsupported => "lock_unsupported",
        shared_io::Error::Cancelled => "cancelled",
        shared_io::Error::OperationUnsupported => "operation_unsupported",
    }
}

fn io_error_name(err: &std::io::Error) -> &'static str {
    use std::io::ErrorKind;
    match err.kind() {
        ErrorKind::NotFound => "file_not_found",
        ErrorKind::PermissionDenied => "permission_denied",
        ErrorKind::AlreadyExists => "already_exists",
        ErrorKind::InvalidInput => "invalid_input",
        _ => "io",
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod file_limit_ffi {
    pub const RLIMIT_FSIZE: u32 = 1;
    pub const SIG_XFSZ: i32 = 25;
    pub const SIG_IGN: usize = 1;
    pub const SIG_DFL: usize = 0;

    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default)]
    pub struct Rlimit {
        pub cur: u64,
        pub max: u64,
    }

    #[cfg(target_os = "linux")]
    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default)]
    pub struct Sigaction {
        pub sa_handler: usize,
        pub sa_mask: [u64; 16],
        pub sa_flags: i32,
        pub sa_restorer: usize,
    }

    #[cfg(target_os = "macos")]
    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default)]
    pub struct Sigaction {
        pub sa_handler: usize,
        pub sa_mask: u32,
        pub sa_flags: i32,
    }

    extern "C" {
        pub fn getrlimit(resource: u32, rlim: *mut Rlimit) -> i32;
        pub fn setrlimit(resource: u32, rlim: *const Rlimit) -> i32;
        pub fn sigaction(signum: i32, act: *const Sigaction, oldact: *mut Sigaction) -> i32;
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[derive(Debug)]
struct FileSizeLimitGuard {
    saved_limit: file_limit_ffi::Rlimit,
    saved_action: file_limit_ffi::Sigaction,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl FileSizeLimitGuard {
    fn restore_inner(&self) {
        unsafe {
            file_limit_ffi::setrlimit(file_limit_ffi::RLIMIT_FSIZE, &self.saved_limit);
            file_limit_ffi::sigaction(
                file_limit_ffi::SIG_XFSZ,
                &self.saved_action,
                std::ptr::null_mut(),
            );
        }
    }

    fn restore(self) {
        self.restore_inner();
        std::mem::forget(self);
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl Drop for FileSizeLimitGuard {
    fn drop(&mut self) {
        self.restore_inner();
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[derive(Debug, Eq, PartialEq)]
struct InjectedSetupFailure;

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn sigaction_ignore() -> file_limit_ffi::Sigaction {
    let mut action = file_limit_ffi::Sigaction::default();
    action.sa_handler = file_limit_ffi::SIG_IGN;
    action
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn sigaction_default() -> file_limit_ffi::Sigaction {
    let mut action = file_limit_ffi::Sigaction::default();
    action.sa_handler = file_limit_ffi::SIG_DFL;
    action
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn get_rlimit() -> file_limit_ffi::Rlimit {
    let mut limit = file_limit_ffi::Rlimit::default();
    let ret = unsafe { file_limit_ffi::getrlimit(file_limit_ffi::RLIMIT_FSIZE, &mut limit) };
    assert_eq!(0, ret, "getrlimit failed");
    limit
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn set_rlimit(cur: u64, max: u64) {
    let limit = file_limit_ffi::Rlimit { cur, max };
    let ret = unsafe { file_limit_ffi::setrlimit(file_limit_ffi::RLIMIT_FSIZE, &limit) };
    assert_eq!(0, ret, "setrlimit failed");
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn limit_file_size_for_test(
    bytes: u64,
    fail_after_signal: bool,
) -> Result<FileSizeLimitGuard, InjectedSetupFailure> {
    let mut saved_action = file_limit_ffi::Sigaction::default();
    unsafe {
        file_limit_ffi::sigaction(
            file_limit_ffi::SIG_XFSZ,
            &sigaction_ignore(),
            &mut saved_action,
        );
    }
    if fail_after_signal {
        unsafe {
            file_limit_ffi::sigaction(
                file_limit_ffi::SIG_XFSZ,
                &saved_action,
                std::ptr::null_mut(),
            );
        }
        return Err(InjectedSetupFailure);
    }

    let saved_limit = get_rlimit();
    set_rlimit(bytes, saved_limit.max);
    Ok(FileSizeLimitGuard {
        saved_limit,
        saved_action,
    })
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn expect_signal_handler_equal(
    expected: &file_limit_ffi::Sigaction,
    actual: &file_limit_ffi::Sigaction,
) {
    assert_eq!(expected.sa_handler, actual.sa_handler);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    // RLIMIT_FSIZE and the SIGXFSZ disposition are process-wide. Rust runs
    // unit tests on multiple threads by default, so every test that touches
    // either must serialize against this lock to avoid racing the other tests.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    static PROCESS_LIMIT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn temp_dir(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("nyq-change-{}-{}", name, std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn write_absolute(path: &Path, content: &[u8]) {
        fs::write(path, content).unwrap();
    }

    fn read_absolute(path: &Path) -> Vec<u8> {
        fs::read(path).unwrap()
    }

    fn expect_missing(path: &Path) {
        match fs::symlink_metadata(path) {
            Ok(_) => panic!("file still exists: {}", path.display()),
            Err(_) => {}
        }
    }

    #[test]
    fn undo_last_returns_empty_on_an_initially_empty_stack() {
        let mut tracker = ChangeTracker::init();
        assert!(matches!(tracker.undo_last(), UndoResult::Empty));
        tracker.deinit();
    }

    #[test]
    fn clear_releases_operations_and_leaves_the_tracker_empty() {
        let mut tracker = ChangeTracker::init();

        tracker.push_operation(FileOperation {
            kind: OperationKind::Edit,
            path: b"/workspace/a.txt".to_vec(),
            previous_content: Some(b"before".to_vec()),
            timestamp_ms: 1,
        });

        tracker.clear();

        assert_eq!(0, tracker.stack.len());
        assert!(matches!(tracker.undo_last(), UndoResult::Empty));

        tracker.push_operation(FileOperation {
            kind: OperationKind::Write,
            path: b"/workspace/reused.txt".to_vec(),
            previous_content: None,
            timestamp_ms: 2,
        });
        assert_eq!(1, tracker.stack.len());
        assert_eq!(b"/workspace/reused.txt", tracker.stack[0].path.as_slice());

        tracker.deinit();
    }

    #[test]
    fn push_operation_evicts_the_oldest_operation_with_stable_ordering() {
        let mut tracker = ChangeTracker::init();

        for i in 0..(ChangeTracker::max_stack_size + 1) {
            tracker.push_operation(FileOperation {
                kind: OperationKind::Write,
                path: format!("/tracked/file-{i}").into_bytes(),
                previous_content: None,
                timestamp_ms: i as i64,
            });
        }

        assert_eq!(ChangeTracker::max_stack_size, tracker.stack.len());
        assert_eq!(b"/tracked/file-1", tracker.stack[0].path.as_slice());
        assert_eq!(
            format!("/tracked/file-{}", ChangeTracker::max_stack_size).as_bytes(),
            tracker.stack[tracker.stack.len() - 1].path.as_slice()
        );

        tracker.deinit();
    }

    #[test]
    fn undo_last_restores_previous_content_for_write_and_edit_operations() {
        let dir = temp_dir("restore");
        let path = dir.join("restore.txt");
        write_absolute(&path, b"modified");

        let mut tracker = ChangeTracker::init();
        tracker.push_operation(FileOperation {
            kind: OperationKind::Write,
            path: path.to_str().unwrap().as_bytes().to_vec(),
            previous_content: Some(b"original".to_vec()),
            timestamp_ms: 1,
        });

        match tracker.undo_last() {
            UndoResult::Restored(restored_path) => {
                assert_eq!(path.to_str().unwrap().as_bytes(), restored_path.as_slice());
            }
            _ => panic!("expected restore"),
        }

        let content = read_absolute(&path);
        assert_eq!(b"original", content.as_slice());

        tracker.deinit();
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn undo_last_deletes_new_write_and_edit_operations() {
        let dir = temp_dir("new");
        let path = dir.join("new.txt");
        write_absolute(&path, b"new content");

        let mut tracker = ChangeTracker::init();
        tracker.push_operation(FileOperation {
            kind: OperationKind::Edit,
            path: path.to_str().unwrap().as_bytes().to_vec(),
            previous_content: None,
            timestamp_ms: 1,
        });

        match tracker.undo_last() {
            UndoResult::Deleted(deleted_path) => {
                assert_eq!(path.to_str().unwrap().as_bytes(), deleted_path.as_slice());
            }
            _ => panic!("expected delete"),
        }

        expect_missing(&path);

        tracker.deinit();
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn undo_last_reports_deleted_for_a_new_write_when_the_file_is_already_absent() {
        let dir = temp_dir("already-absent");
        let path = dir.join("already-absent.txt");

        let mut tracker = ChangeTracker::init();
        tracker.push_operation(FileOperation {
            kind: OperationKind::Write,
            path: path.to_str().unwrap().as_bytes().to_vec(),
            previous_content: None,
            timestamp_ms: 1,
        });

        match tracker.undo_last() {
            UndoResult::Deleted(deleted_path) => {
                assert_eq!(path.to_str().unwrap().as_bytes(), deleted_path.as_slice());
            }
            _ => panic!("expected delete"),
        }

        expect_missing(&path);

        tracker.deinit();
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn undo_last_pops_before_filesystem_restore_failures_reports_them_and_does_not_create_parents() {
        let dir = temp_dir("missing-parent");
        let path = dir.join("missing-parent/file.txt");

        let mut tracker = ChangeTracker::init();
        tracker.push_operation(FileOperation {
            kind: OperationKind::Write,
            path: path.to_str().unwrap().as_bytes().to_vec(),
            previous_content: Some(b"content".to_vec()),
            timestamp_ms: 1,
        });

        match tracker.undo_last() {
            UndoResult::Unavailable(_) => {}
            _ => panic!("expected unavailable"),
        }
        assert_eq!(0, tracker.stack.len());
        expect_missing(&path);
        // The stack is empty now, which is a different answer from a failed restore.
        assert!(matches!(tracker.undo_last(), UndoResult::Empty));

        tracker.deinit();
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn undo_restore_cannot_be_redirected_by_a_symlink_introduced_after_capture() {
        let dir = temp_dir("symlink");
        let recorded_path = dir.join("recorded.txt");
        let redirect_target = dir.join("redirect-target.txt");
        write_absolute(&recorded_path, b"current bytes");
        write_absolute(&redirect_target, b"must stay untouched");

        let mut tracker = ChangeTracker::init();
        tracker.push_operation(FileOperation {
            kind: OperationKind::Edit,
            path: recorded_path.to_str().unwrap().as_bytes().to_vec(),
            previous_content: Some(b"preimage bytes".to_vec()),
            timestamp_ms: 1,
        });

        fs::remove_file(&recorded_path).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&redirect_target, &recorded_path).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_file(&redirect_target, &recorded_path).unwrap();

        match tracker.undo_last() {
            UndoResult::Restored(_) => {}
            _ => panic!("expected restore"),
        }

        let restored_meta = fs::symlink_metadata(&recorded_path).unwrap();
        assert!(restored_meta.is_file());
        let restored = read_absolute(&recorded_path);
        assert_eq!(b"preimage bytes", restored.as_slice());
        let untouched = read_absolute(&redirect_target);
        assert_eq!(b"must stay untouched", untouched.as_slice());

        tracker.deinit();
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn file_size_limit_guard_restores_sigxfsz_after_normal_use() {
        let _process_lock = PROCESS_LIMIT_LOCK.lock().unwrap();
        let mut original = file_limit_ffi::Sigaction::default();
        unsafe {
            file_limit_ffi::sigaction(
                file_limit_ffi::SIG_XFSZ,
                &sigaction_default(),
                &mut original,
            );
        }

        let mut expected = file_limit_ffi::Sigaction::default();
        unsafe {
            file_limit_ffi::sigaction(
                file_limit_ffi::SIG_XFSZ,
                std::ptr::null(),
                &mut expected,
            );
        }

        let guard = limit_file_size_for_test(4096, false).unwrap();
        guard.restore();

        let mut actual = file_limit_ffi::Sigaction::default();
        unsafe {
            file_limit_ffi::sigaction(file_limit_ffi::SIG_XFSZ, std::ptr::null(), &mut actual);
        }
        expect_signal_handler_equal(&expected, &actual);

        unsafe {
            file_limit_ffi::sigaction(file_limit_ffi::SIG_XFSZ, &original, std::ptr::null_mut());
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn file_size_limit_setup_restores_sigxfsz_on_failure() {
        let _process_lock = PROCESS_LIMIT_LOCK.lock().unwrap();
        let mut original = file_limit_ffi::Sigaction::default();
        unsafe {
            file_limit_ffi::sigaction(
                file_limit_ffi::SIG_XFSZ,
                &sigaction_default(),
                &mut original,
            );
        }

        let mut expected = file_limit_ffi::Sigaction::default();
        unsafe {
            file_limit_ffi::sigaction(
                file_limit_ffi::SIG_XFSZ,
                std::ptr::null(),
                &mut expected,
            );
        }

        let err = limit_file_size_for_test(4096, true).unwrap_err();
        assert_eq!(InjectedSetupFailure, err);

        let mut actual = file_limit_ffi::Sigaction::default();
        unsafe {
            file_limit_ffi::sigaction(file_limit_ffi::SIG_XFSZ, std::ptr::null(), &mut actual);
        }
        expect_signal_handler_equal(&expected, &actual);

        unsafe {
            file_limit_ffi::sigaction(file_limit_ffi::SIG_XFSZ, &original, std::ptr::null_mut());
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn undo_last_reports_unavailable_when_a_new_file_cannot_be_deleted() {
        let dir = temp_dir("locked-delete");
        fs::create_dir_all(dir.join("locked")).unwrap();
        let path = dir.join("locked/new.txt");
        write_absolute(&path, b"new content");

        set_mode(&dir.join("locked"), 0o500);

        let mut tracker = ChangeTracker::init();
        tracker.push_operation(FileOperation {
            kind: OperationKind::Write,
            path: path.to_str().unwrap().as_bytes().to_vec(),
            previous_content: None,
            timestamp_ms: 1,
        });

        match tracker.undo_last() {
            UndoResult::Unavailable(reported_path) => {
                assert_eq!(path.to_str().unwrap().as_bytes(), reported_path.as_slice());
            }
            _ => panic!("expected unavailable"),
        }
        assert_eq!(0, tracker.stack.len());

        let survived = read_absolute(&path);
        assert_eq!(b"new content", survived.as_slice());

        set_mode(&dir.join("locked"), 0o700);
        tracker.deinit();
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn undo_last_leaves_the_original_file_intact_when_the_restore_write_fails() {
        let _process_lock = PROCESS_LIMIT_LOCK.lock().unwrap();
        let dir = temp_dir("restore-fail");
        let path = dir.join("restore-target.txt");

        let current = b"bytes the user still has on disk";
        write_absolute(&path, current);

        let preimage = vec![b'R'; 64 * 1024];

        let mut tracker = ChangeTracker::init();
        tracker.push_operation(FileOperation {
            kind: OperationKind::Write,
            path: path.to_str().unwrap().as_bytes().to_vec(),
            previous_content: Some(preimage),
            timestamp_ms: 1,
        });

        // Fail every write past 4 KiB, without the file-size signal killing
        // the test process.
        let file_size_guard = limit_file_size_for_test(4096, false).unwrap();

        match tracker.undo_last() {
            UndoResult::Unavailable(_) => {}
            _ => panic!("expected unavailable"),
        }

        let survived = read_absolute(&path);
        assert_eq!(current, survived.as_slice());

        file_size_guard.restore();
        tracker.deinit();
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn undo_refuses_a_file_whose_directory_denies_writes_and_leaves_it_intact() {
        let dir = temp_dir("locked-restore");
        fs::create_dir_all(dir.join("locked")).unwrap();
        let path = dir.join("locked/file.txt");
        write_absolute(&path, b"current bytes");

        set_mode(&dir.join("locked"), 0o500);

        let mut tracker = ChangeTracker::init();
        tracker.push_operation(FileOperation {
            kind: OperationKind::Edit,
            path: path.to_str().unwrap().as_bytes().to_vec(),
            previous_content: Some(b"preimage bytes".to_vec()),
            timestamp_ms: 1,
        });

        // The file cannot be replaced atomically here, and a direct overwrite
        // could leave it half replaced, so undo reports that it could not act.
        match tracker.undo_last() {
            UndoResult::Unavailable(_) => {}
            _ => panic!("expected unavailable"),
        }
        let survived = read_absolute(&path);
        assert_eq!(b"current bytes", survived.as_slice());

        set_mode(&dir.join("locked"), 0o700);
        tracker.deinit();
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn a_locked_directory_plus_a_failing_write_never_leaves_a_half_replaced_file() {
        let _process_lock = PROCESS_LIMIT_LOCK.lock().unwrap();
        let dir = temp_dir("half-replaced");
        fs::create_dir_all(dir.join("locked")).unwrap();
        let path = dir.join("locked/file.txt");

        let current = b"the bytes the user has right now";
        write_absolute(&path, current);

        // Large enough that any direct overwrite would be cut short by the
        // limit below, which is what would leave a prefix of preimage bytes
        // behind.
        let preimage = vec![b'R'; 64 * 1024];

        let mut tracker = ChangeTracker::init();
        tracker.push_operation(FileOperation {
            kind: OperationKind::Edit,
            path: path.to_str().unwrap().as_bytes().to_vec(),
            previous_content: Some(preimage),
            timestamp_ms: 1,
        });

        set_mode(&dir.join("locked"), 0o500);
        let file_size_guard = limit_file_size_for_test(4096, false).unwrap();

        match tracker.undo_last() {
            UndoResult::Unavailable(_) => {}
            _ => panic!("expected unavailable"),
        }

        // Not shortened, not partly rewritten: exactly the bytes that were there.
        let survived = read_absolute(&path);
        assert_eq!(current, survived.as_slice());

        file_size_guard.restore();
        set_mode(&dir.join("locked"), 0o700);
        tracker.deinit();
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    fn set_mode(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }
}
