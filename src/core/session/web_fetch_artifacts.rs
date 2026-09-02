//! Durable, session-scoped web-fetch artifact storage.
//!
//! Mirrors `zig-reference/src/core/session/web_fetch_artifacts.zig`.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use crate::core::shared::io as io_mod;

pub const RELATIVE_DIR: &str = "artifacts/web-fetch";
pub const DEFAULT_MAX_BYTES: usize = 50 * 1024 * 1024;
pub const DEFAULT_MAX_FILES: usize = 1024;

// Zig exposes these declarations with lower-case names.
#[allow(non_upper_case_globals)]
pub const relative_dir: &str = RELATIVE_DIR;
#[allow(non_upper_case_globals)]
pub const default_max_bytes: usize = DEFAULT_MAX_BYTES;
#[allow(non_upper_case_globals)]
pub const default_max_files: usize = DEFAULT_MAX_FILES;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Limits {
    pub max_bytes: usize,
    pub max_files: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_bytes: DEFAULT_MAX_BYTES,
            max_files: DEFAULT_MAX_FILES,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Artifact {
    pub handle: String,
    pub mime_type: String,
    pub byte_count: usize,
    pub retention_scope: &'static str,
    pub display_path: PathBuf,
}

impl Artifact {
    pub fn deinit(&mut self) {
        self.handle.clear();
        self.mime_type.clear();
        self.display_path.clear();
    }
}

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    ArtifactNotFound,
    CorruptArtifactStore,
    ArtifactQuotaExceeded,
    ArtifactFileCapExceeded,
    InvalidArtifactHandle,
    Overflow,
    StreamTooLong,
}

impl From<std::io::Error> for Error {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Default)]
struct Usage {
    used_bytes: usize,
    file_count: usize,
}

#[derive(Debug)]
pub struct Store {
    pub dir: PathBuf,
    pub store_id: PathBuf,
    pub limits: Limits,
    usage: Mutex<Usage>,
}

impl Store {
    pub fn init(session_dir: impl AsRef<Path>) -> Result<Self> {
        Self::initWithLimits(session_dir, Limits::default())
    }

    #[allow(non_snake_case)]
    pub fn initWithLimits(session_dir: impl AsRef<Path>, limits: Limits) -> Result<Self> {
        let session_dir = session_dir.as_ref();
        ensure_artifact_dir(session_dir)?;
        let dir = session_dir.join(RELATIVE_DIR);
        let mut store = Self {
            store_id: dir.clone(),
            dir,
            limits,
            usage: Mutex::new(Usage::default()),
        };
        store.reconcile()?;
        Ok(store)
    }

    pub fn deinit(&mut self) {
        self.dir.clear();
        self.store_id.clear();
        if let Ok(mut usage) = self.usage.lock() {
            *usage = Usage::default();
        }
    }

    pub fn id(&self) -> &Path {
        &self.store_id
    }

    pub fn write(&self, mime_type: &str, bytes: &[u8]) -> Result<Artifact> {
        let handle = make_handle(extensionForMime(mime_type))?;
        self.write_with_owned_handle(handle, mime_type, bytes)
    }

    pub fn read(&self, handle: &str, max_bytes: usize) -> Result<Vec<u8>> {
        if !self.contains(handle)? {
            return Err(Error::ArtifactNotFound);
        }
        let path = self.path_for_handle(handle)?;
        let mut file = match File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(Error::ArtifactNotFound)
            }
            Err(error) => return Err(Error::Io(error)),
        };
        let mut bytes = Vec::new();
        Read::by_ref(&mut file)
            .take(max_bytes.saturating_add(1) as u64)
            .read_to_end(&mut bytes)?;
        if bytes.len() > max_bytes {
            return Err(Error::StreamTooLong);
        }
        Ok(bytes)
    }

    pub fn contains(&self, handle: &str) -> Result<bool> {
        validate_handle(handle)?;
        for entry in fs::read_dir(&self.dir)? {
            let entry = entry?;
            if entry.file_name() != handle {
                continue;
            }
            let metadata = fs::symlink_metadata(entry.path())?;
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(Error::CorruptArtifactStore);
            }
            return Ok(true);
        }
        Ok(false)
    }

    fn reconcile(&mut self) -> Result<()> {
        let mut used_bytes = 0usize;
        let mut file_count = 0usize;
        for entry in fs::read_dir(&self.dir)? {
            let entry = entry?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| Error::InvalidArtifactHandle)?;
            if is_temp_name(&name) {
                fs::remove_file(entry.path())?;
                continue;
            }
            let metadata = fs::symlink_metadata(entry.path())?;
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(Error::CorruptArtifactStore);
            }
            validate_handle(&name)?;
            used_bytes = checked_add(used_bytes, metadata.len())?;
            file_count = file_count
                .checked_add(1)
                .ok_or(Error::ArtifactQuotaExceeded)?;
            if used_bytes > self.limits.max_bytes || file_count > self.limits.max_files {
                return Err(Error::ArtifactQuotaExceeded);
            }
        }
        *self.usage.lock().map_err(|_| Error::CorruptArtifactStore)? = Usage {
            used_bytes,
            file_count,
        };
        Ok(())
    }

    #[cfg(test)]
    fn write_with_handle_for_test(
        &self,
        handle: &str,
        mime_type: &str,
        bytes: &[u8],
    ) -> Result<Artifact> {
        self.write_with_owned_handle(handle.to_owned(), mime_type, bytes)
    }

    fn write_with_owned_handle(
        &self,
        handle: String,
        mime_type: &str,
        bytes: &[u8],
    ) -> Result<Artifact> {
        validate_handle(&handle)?;
        let normalized_mime = normalized_mime(mime_type);
        let path = self.path_for_handle(&handle)?;
        let display_path = path.clone();
        let temp_path = PathBuf::from(format!(
            "{}.tmp.{}",
            path.display(),
            io_mod::nano_timestamp()
        ));

        self.reserve(bytes.len())?;
        let result = (|| {
            let mut file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&temp_path)?;
            file.write_all(bytes)?;
            file.sync_all()?;
            fs::rename(&temp_path, &path)?;
            Ok(Artifact {
                handle,
                mime_type: normalized_mime,
                byte_count: bytes.len(),
                retention_scope: "session",
                display_path,
            })
        })();
        if result.is_err() {
            self.rollback(bytes.len());
            let _ = fs::remove_file(&temp_path);
        }
        result
    }

    fn reserve(&self, byte_count: usize) -> Result<()> {
        let mut usage = self.usage.lock().map_err(|_| Error::CorruptArtifactStore)?;
        if byte_count > self.limits.max_bytes
            || usage.used_bytes > self.limits.max_bytes - byte_count
        {
            return Err(Error::ArtifactQuotaExceeded);
        }
        if usage.file_count >= self.limits.max_files {
            return Err(Error::ArtifactFileCapExceeded);
        }
        usage.used_bytes += byte_count;
        usage.file_count += 1;
        Ok(())
    }

    fn rollback(&self, byte_count: usize) {
        if let Ok(mut usage) = self.usage.lock() {
            usage.used_bytes = usage.used_bytes.saturating_sub(byte_count);
            usage.file_count = usage.file_count.saturating_sub(1);
        }
    }

    fn path_for_handle(&self, handle: &str) -> Result<PathBuf> {
        validate_handle(handle)?;
        Ok(self.dir.join(handle))
    }

    #[cfg(test)]
    fn used_bytes_for_test(&self) -> usize {
        self.usage.lock().unwrap().used_bytes
    }

    #[cfg(test)]
    fn file_count_for_test(&self) -> usize {
        self.usage.lock().unwrap().file_count
    }
}

fn ensure_artifact_dir(session_dir: &Path) -> Result<()> {
    ensure_managed_dir(session_dir, "artifacts")?;
    ensure_managed_dir(&session_dir.join("artifacts"), "web-fetch")
}

fn ensure_managed_dir(parent_path: &Path, child_name: &str) -> Result<()> {
    // Zig's `openDirAbsolute` follows the supplied parent path, while the
    // managed child itself is opened with `follow_symlinks = false`.
    let parent_metadata = fs::metadata(parent_path)?;
    if !parent_metadata.is_dir() {
        return Err(Error::CorruptArtifactStore);
    }
    let child = parent_path.join(child_name);
    match fs::symlink_metadata(&child) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(Error::CorruptArtifactStore);
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => fs::create_dir(&child)?,
        Err(error) => return Err(Error::Io(error)),
    }
    set_private_dir_permissions(&child)?;
    let metadata = fs::symlink_metadata(&child)?;
    if metadata.file_type().is_symlink()
        || !metadata.is_dir()
        || !has_private_permissions(&metadata)
    {
        return Err(Error::CorruptArtifactStore);
    }
    Ok(())
}

#[cfg(unix)]
fn set_private_dir_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_private_dir_permissions(_: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn has_private_permissions(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o777 == 0o700
}

#[cfg(not(unix))]
fn has_private_permissions(_: &fs::Metadata) -> bool {
    true
}

fn checked_add(a: usize, b: u64) -> Result<usize> {
    let rhs = usize::try_from(b).map_err(|_| Error::Overflow)?;
    a.checked_add(rhs).ok_or(Error::Overflow)
}

fn normalized_mime(mime_type: &str) -> String {
    let value = trim_mime(mime_type.split(';').next().unwrap_or(""));
    if value.is_empty() {
        "application/octet-stream".to_owned()
    } else {
        value.to_ascii_lowercase()
    }
}

#[allow(non_snake_case)]
pub fn extensionForMime(mime_type: &str) -> &'static str {
    match trim_mime(mime_type.split(';').next().unwrap_or(""))
        .to_ascii_lowercase()
        .as_str()
    {
        "application/pdf" => "pdf",
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/svg+xml" => "svg",
        "application/zip" => "zip",
        _ => "bin",
    }
}

fn trim_mime(value: &str) -> &str {
    value.trim_matches([' ', '\t', '\r', '\n'])
}

static HANDLE_COUNTER: AtomicU64 = AtomicU64::new(0);

fn make_handle(extension: &str) -> Result<String> {
    let mut random_bytes = [0u8; 8];
    #[cfg(unix)]
    File::open("/dev/urandom")?.read_exact(&mut random_bytes)?;
    #[cfg(not(unix))]
    {
        let seed =
            (io_mod::nano_timestamp() as u64) ^ HANDLE_COUNTER.fetch_add(1, Ordering::Relaxed);
        random_bytes = seed.to_le_bytes();
    }
    // Retain a counter in every build to prevent dead-code differences across targets.
    let _ = HANDLE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let random_hex: String = random_bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    Ok(format!(
        "artifact-{}-{random_hex}.{extension}",
        io_mod::nano_timestamp()
    ))
}

fn is_temp_name(name: &str) -> bool {
    name.starts_with(".artifact-") || name.contains(".tmp.")
}

fn validate_handle(handle: &str) -> Result<()> {
    if handle.is_empty()
        || handle.len() > 160
        || !handle.starts_with("artifact-")
        || handle.contains("..")
        || !handle
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'.')
        || !has_allowed_extension(handle)
    {
        return Err(Error::InvalidArtifactHandle);
    }
    Ok(())
}

fn has_allowed_extension(handle: &str) -> bool {
    [
        ".pdf", ".png", ".jpg", ".gif", ".webp", ".svg", ".zip", ".bin",
    ]
    .iter()
    .any(|extension| handle.ends_with(extension))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "nyq-web-fetch-{}-{}",
                std::process::id(),
                TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn web_fetch_binary_artifact_write_is_byte_exact() {
        let tmp = TempDir::new();
        let store = Store::init(&tmp.0).unwrap();
        let bytes = b"\x00pdf bytes\xff";
        let artifact = store.write("application/pdf", bytes).unwrap();
        assert_eq!(
            bytes,
            store.read(&artifact.handle, 1024).unwrap().as_slice()
        );
    }

    #[test]
    fn web_fetch_artifact_quota_reconciles_existing_files_on_resume() {
        let tmp = TempDir::new();
        let artifact_dir = tmp.0.join(RELATIVE_DIR);
        fs::create_dir_all(&artifact_dir).unwrap();
        fs::write(artifact_dir.join("artifact-existing.bin"), b"abc").unwrap();
        let store = Store::initWithLimits(
            &tmp.0,
            Limits {
                max_bytes: 4,
                max_files: 2,
            },
        )
        .unwrap();
        assert_eq!(3, store.used_bytes_for_test());
        assert_eq!(1, store.file_count_for_test());
        assert!(matches!(
            store.write_with_handle_for_test("artifact-new.bin", "application/octet-stream", b"xy"),
            Err(Error::ArtifactQuotaExceeded)
        ));
    }

    #[test]
    fn web_fetch_concurrent_artifact_reservations_cannot_exceed_byte_or_file_caps() {
        let tmp = TempDir::new();
        let store = Arc::new(
            Store::initWithLimits(
                &tmp.0,
                Limits {
                    max_bytes: 3,
                    max_files: 1,
                },
            )
            .unwrap(),
        );
        let first = Arc::clone(&store);
        let second = Arc::clone(&store);
        let a = std::thread::spawn(move || first.reserve(2));
        let b = std::thread::spawn(move || second.reserve(2));
        let outcomes = [a.join().unwrap(), b.join().unwrap()];
        assert_eq!(1, outcomes.iter().filter(|result| result.is_ok()).count());
        assert_eq!(2, store.used_bytes_for_test());
        assert_eq!(1, store.file_count_for_test());
        store.rollback(2);
    }

    #[test]
    fn web_fetch_failed_artifact_write_rolls_back_quota_reservation() {
        let tmp = TempDir::new();
        let store = Store::initWithLimits(
            &tmp.0,
            Limits {
                max_bytes: 3,
                max_files: 1,
            },
        )
        .unwrap();
        fs::create_dir(store.dir.join("artifact-conflict.bin")).unwrap();
        assert!(store
            .write_with_handle_for_test("artifact-conflict.bin", "application/octet-stream", b"ab")
            .is_err());
        assert_eq!(0, store.used_bytes_for_test());
        assert_eq!(0, store.file_count_for_test());
        store
            .write_with_handle_for_test("artifact-ok.bin", "application/octet-stream", b"ab")
            .unwrap();
        assert_eq!(2, store.used_bytes_for_test());
        assert_eq!(1, store.file_count_for_test());
    }

    #[test]
    fn web_fetch_artifact_store_refuses_symlinks_and_partial_temp_files_deterministically() {
        let tmp = TempDir::new();
        let artifact_dir = tmp.0.join(RELATIVE_DIR);
        fs::create_dir_all(&artifact_dir).unwrap();
        let temp_path = artifact_dir.join(".artifact-leftover.tmp");
        fs::write(&temp_path, b"partial").unwrap();
        let store = Store::init(&tmp.0).unwrap();
        assert!(!store.contains("artifact-leftover.bin").unwrap());
        assert!(!temp_path.exists());

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let other = TempDir::new();
            let dir = other.0.join(RELATIVE_DIR);
            fs::create_dir_all(&dir).unwrap();
            symlink("target", dir.join("artifact-link.bin")).unwrap();
            assert!(matches!(
                Store::init(&other.0),
                Err(Error::CorruptArtifactStore)
            ));
        }
    }

    #[cfg(unix)]
    #[test]
    fn web_fetch_artifact_store_rejects_a_symlinked_store_directory() {
        use std::os::unix::fs::symlink;
        let tmp = TempDir::new();
        fs::create_dir(tmp.0.join("artifacts")).unwrap();
        fs::create_dir(tmp.0.join("outside")).unwrap();
        symlink("../outside", tmp.0.join(RELATIVE_DIR)).unwrap();
        assert!(matches!(
            Store::init(&tmp.0),
            Err(Error::CorruptArtifactStore)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn web_fetch_artifact_store_creates_private_managed_directories() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new();
        let _store = Store::init(&tmp.0).unwrap();
        assert_eq!(
            0o700,
            fs::metadata(tmp.0.join("artifacts"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777
        );
        assert_eq!(
            0o700,
            fs::metadata(tmp.0.join(RELATIVE_DIR))
                .unwrap()
                .permissions()
                .mode()
                & 0o777
        );
    }

    #[test]
    fn web_fetch_corrupt_durable_artifact_store_fails_instead_of_degrading() {
        let tmp = TempDir::new();
        let artifact_dir = tmp.0.join(RELATIVE_DIR);
        fs::create_dir_all(&artifact_dir).unwrap();
        fs::write(artifact_dir.join("not-managed.bin"), b"corrupt").unwrap();
        assert!(matches!(
            Store::init(&tmp.0),
            Err(Error::InvalidArtifactHandle)
        ));
    }
}
