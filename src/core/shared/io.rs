use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    sync::{Mutex, OnceLock, RwLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub type RawEnviron = Vec<Vec<u8>>;
#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    InvalidPath,
    DurablePathUnsafe,
    StreamTooLong,
    EnvironmentUnavailable,
    HandlePathUnavailable,
    PrivateStatePermissionsUnsupported,
    DurableReplacePreRenameFailed,
    DurableReplacePostRenameFailed,
    LockBusy,
    LockUnsupported,
    Cancelled,
    OperationUnsupported,
}
impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}
pub type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Debug)]
enum Environment {
    Map(HashMap<Vec<u8>, Vec<u8>>),
    Block(Vec<Vec<u8>>),
    Raw(RawEnviron),
}
static ENVIRON: OnceLock<RwLock<Option<Environment>>> = OnceLock::new();
fn env_state() -> &'static RwLock<Option<Environment>> {
    ENVIRON.get_or_init(|| RwLock::new(None))
}
pub fn set_io<T>(_: T) {}
pub fn get_io() {}
pub fn set_environ_map(map: &HashMap<Vec<u8>, Vec<u8>>) {
    *env_state().write().expect("environment lock poisoned") = Some(Environment::Map(map.clone()))
}
pub fn set_environ_block(block: Vec<Vec<u8>>) {
    *env_state().write().expect("environment lock poisoned") = Some(Environment::Block(block))
}
pub fn set_raw_environ(raw: RawEnviron) {
    *env_state().write().expect("environment lock poisoned") = Some(Environment::Raw(raw))
}
fn split_entry(e: &[u8]) -> Option<(&[u8], &[u8])> {
    let n = e.iter().position(|&b| b == b'=')?;
    Some((&e[..n], &e[n + 1..]))
}
pub fn getenv(key: &[u8]) -> Option<Vec<u8>> {
    match env_state().read().ok()?.as_ref()? {
        Environment::Map(m) => m.get(key).cloned(),
        Environment::Block(v) | Environment::Raw(v) => v
            .iter()
            .filter_map(|e| split_entry(e))
            .find_map(|(k, v)| (k == key).then(|| v.to_vec())),
    }
}
pub fn environ_map() -> Option<HashMap<Vec<u8>, Vec<u8>>> {
    match env_state().read().ok()?.as_ref()? {
        Environment::Map(m) => Some(m.clone()),
        _ => None,
    }
}
pub fn clone_environ_map() -> Result<HashMap<Vec<u8>, Vec<u8>>> {
    match env_state()
        .read()
        .map_err(|_| Error::EnvironmentUnavailable)?
        .as_ref()
        .ok_or(Error::EnvironmentUnavailable)?
    {
        Environment::Map(m) => Ok(m.clone()),
        Environment::Block(v) | Environment::Raw(v) => Ok(v
            .iter()
            .filter_map(|e| split_entry(e))
            .map(|(k, v)| (k.to_vec(), v.to_vec()))
            .collect()),
    }
}
pub fn e2e_fail_if_durable_mutation_attempted() {
    if getenv(b"FX_E2E_FAIL_ON_DURABLE_MUTATION").as_deref() == Some(b"1") {
        std::process::exit(86)
    }
}

pub fn open_dir_absolute_no_follow(path: impl AsRef<Path>) -> Result<File> {
    let p = path.as_ref();
    if !p.is_absolute() {
        return Err(Error::InvalidPath);
    }
    for c in p.components() {
        if matches!(c, Component::CurDir | Component::ParentDir) {
            return Err(Error::InvalidPath);
        }
    }
    let meta = fs::symlink_metadata(p)?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return Err(Error::DurablePathUnsafe);
    }
    Ok(File::open(p)?)
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FinalSymlinkPolicy {
    NoFollow,
    Follow,
}
fn regular(path: &Path, follow: bool, hardlinks: bool, write: bool) -> Result<File> {
    let m = if follow {
        fs::metadata(path)
    } else {
        fs::symlink_metadata(path)
    }?;
    if !m.is_file() || (!hardlinks && nlink(&m) != 1) {
        return Err(Error::DurablePathUnsafe);
    }
    let mut o = OpenOptions::new();
    o.read(true).write(write);
    let f = o.open(path)?;
    let m = f.metadata()?;
    if !m.is_file() || (!hardlinks && nlink(&m) > 1) || (!hardlinks && write && nlink(&m) != 1) {
        return Err(Error::DurablePathUnsafe);
    }
    Ok(f)
}
pub fn open_existing_regular_file(path: impl AsRef<Path>, write: bool) -> Result<File> {
    regular(path.as_ref(), false, false, write)
}
pub fn open_existing_read_only_regular_file(
    path: impl AsRef<Path>,
    policy: FinalSymlinkPolicy,
) -> Result<File> {
    regular(
        path.as_ref(),
        policy == FinalSymlinkPolicy::Follow,
        true,
        false,
    )
}
pub fn verify_opened_regular_file(file: &File, write: bool) -> Result<()> {
    let m = file.metadata()?;
    if !m.is_file() || nlink(&m) > 1 || (write && nlink(&m) != 1) {
        Err(Error::DurablePathUnsafe)
    } else {
        Ok(())
    }
}
#[cfg(unix)]
fn nlink(m: &fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    m.nlink()
}
#[cfg(not(unix))]
fn nlink(_: &fs::Metadata) -> u64 {
    1
}
pub fn read_file_to_end(file: &mut File, max: usize) -> Result<Vec<u8>> {
    let mut take = file.take(max.saturating_add(1) as u64);
    let mut out = Vec::new();
    take.read_to_end(&mut out)?;
    if out.len() >= max {
        return Err(Error::StreamTooLong);
    }
    Ok(out)
}
pub fn read_file_to_end_z(file: &mut File, max: usize) -> Result<Vec<u8>> {
    let mut v = read_file_to_end(file, max)?;
    v.push(0);
    Ok(v)
}
pub fn milli_timestamp() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}
pub fn nano_timestamp() -> i128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as i128
}
fn permissions(path: &Path) -> Option<fs::Permissions> {
    fs::metadata(path)
        .ok()
        .filter(|m| m.is_file())
        .map(|m| m.permissions())
}
pub fn write_file_atomic(path: impl AsRef<Path>, text: &[u8]) -> Result<()> {
    e2e_fail_if_durable_mutation_attempted();
    let path = path.as_ref();
    if permissions(path).is_some_and(|p| p.readonly()) {
        return Err(Error::Io(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "read-only destination",
        )));
    }
    let temp = PathBuf::from(format!("{}.tmp.{}", path.display(), nano_timestamp()));
    let result = (|| {
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        if let Some(p) = permissions(path) {
            f.set_permissions(p)?
        }
        f.write_all(text)?;
        f.sync_all()?;
        fs::rename(&temp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}
pub fn copy_file_atomic(source: impl AsRef<Path>, dest: impl AsRef<Path>) -> Result<()> {
    let mut s = File::open(source.as_ref())?;
    if !s.metadata()?.is_file() {
        return Err(Error::DurablePathUnsafe);
    }
    let mut data = Vec::new();
    s.read_to_end(&mut data)?;
    write_file_atomic(dest, &data)
}

#[derive(Debug)]
pub struct VerifiedDir {
    pub path: PathBuf,
    pub dir: File,
}
impl VerifiedDir {
    pub fn close(self) {
        drop(self)
    }
}
fn valid_leaf(name: &str) -> Result<()> {
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\']) {
        Err(Error::DurablePathUnsafe)
    } else {
        Ok(())
    }
}
#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}
#[cfg(not(unix))]
fn set_mode(_: &Path, _: u32) -> std::io::Result<()> {
    Ok(())
}
pub fn open_or_create_verified_private_dir(
    parent: impl AsRef<Path>,
    name: &str,
) -> Result<VerifiedDir> {
    e2e_fail_if_durable_mutation_attempted();
    valid_leaf(name)?;
    let path = parent.as_ref().join(name);
    match fs::symlink_metadata(&path) {
        Ok(m) if m.file_type().is_symlink() || !m.is_dir() => return Err(Error::DurablePathUnsafe),
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Tolerate a concurrent creator, matching the Zig reference
            // (error.PathAlreadyExists => {} in openOrCreateVerifiedPrivateChild).
            if let Err(e) = fs::create_dir(&path) {
                if e.kind() != std::io::ErrorKind::AlreadyExists {
                    return Err(e.into());
                }
            }
        }
        Err(e) => return Err(e.into()),
    }
    set_mode(&path, 0o700).map_err(|_| Error::PrivateStatePermissionsUnsupported)?;
    Ok(VerifiedDir {
        dir: File::open(&path)?,
        path,
    })
}
pub fn open_or_create_verified_private_dir_from_dir(
    parent: impl AsRef<Path>,
    name: &str,
) -> Result<VerifiedDir> {
    open_or_create_verified_private_dir(parent, name)
}
pub fn sync_verified_dir(dir: &VerifiedDir) -> Result<()> {
    dir.dir.sync_all().map_err(Error::Io)
}
pub trait DurableOps {
    fn sync_file(&self, file: &File) -> std::io::Result<()> {
        file.sync_all()
    }
    fn sync_dir(&self, dir: &File) -> std::io::Result<()> {
        dir.sync_all()
    }
}
impl DurableOps for () {}
pub fn durable_replace_verified(dir: &VerifiedDir, name: &str, bytes: &[u8]) -> Result<()> {
    durable_replace_verified_with_ops(dir, name, bytes, &())
}
pub fn durable_replace_verified_with_ops(
    dir: &VerifiedDir,
    name: &str,
    bytes: &[u8],
    ops: &impl DurableOps,
) -> Result<()> {
    e2e_fail_if_durable_mutation_attempted();
    valid_leaf(name)?;
    let target = dir.path.join(name);
    if let Ok(m) = fs::symlink_metadata(&target) {
        if !m.is_file() || m.file_type().is_symlink() || nlink(&m) != 1 {
            return Err(Error::DurablePathUnsafe);
        }
        if m.permissions().readonly() {
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "read-only target",
            )));
        }
    }
    let temp = dir.path.join(format!(".{name}.tmp.{}", nano_timestamp()));
    let outcome = (|| {
        let mut f = OpenOptions::new()
            .create_new(true)
            .write(true)
            .read(true)
            .open(&temp)
            .map_err(|_| Error::DurableReplacePreRenameFailed)?;
        set_mode(&temp, 0o600).map_err(|_| Error::PrivateStatePermissionsUnsupported)?;
        f.write_all(bytes)
            .map_err(|_| Error::DurableReplacePreRenameFailed)?;
        ops.sync_file(&f)
            .map_err(|_| Error::DurableReplacePreRenameFailed)?;
        fs::rename(&temp, &target).map_err(|_| Error::DurableReplacePreRenameFailed)?;
        let m = fs::symlink_metadata(&target).map_err(|_| Error::DurableReplacePostRenameFailed)?;
        if !m.is_file() || nlink(&m) != 1 {
            return Err(Error::DurableReplacePostRenameFailed);
        }
        ops.sync_dir(&dir.dir)
            .map_err(|_| Error::DurableReplacePostRenameFailed)?;
        Ok(())
    })();
    if outcome.is_err() {
        let _ = fs::remove_file(temp);
    }
    outcome
}

static LOCKS: OnceLock<Mutex<HashMap<PathBuf, bool>>> = OnceLock::new();
fn locks() -> &'static Mutex<HashMap<PathBuf, bool>> {
    LOCKS.get_or_init(|| Mutex::new(HashMap::new()))
}
#[derive(Debug)]
pub struct TimedAdvisoryLock {
    path: PathBuf,
    pub file: File,
}
impl TimedAdvisoryLock {
    pub fn release(self) {
        if let Ok(mut m) = locks().lock() {
            m.remove(&self.path);
        }
    }
}
impl Drop for TimedAdvisoryLock {
    fn drop(&mut self) {
        if let Ok(mut m) = locks().lock() {
            m.remove(&self.path);
        }
    }
}
pub fn acquire_timed_advisory_lock(
    dir: &VerifiedDir,
    name: &str,
    deadline_ms: u64,
) -> Result<TimedAdvisoryLock> {
    acquire_timed_advisory_lock_cancellable(dir, name, deadline_ms, None)
}
pub fn acquire_timed_advisory_lock_cancellable(
    dir: &VerifiedDir,
    name: &str,
    deadline_ms: u64,
    cancel: Option<&std::sync::atomic::AtomicBool>,
) -> Result<TimedAdvisoryLock> {
    valid_leaf(name)?;
    let path = dir.path.join(name);
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)?;
    set_mode(&path, 0o600).map_err(|_| Error::PrivateStatePermissionsUnsupported)?;
    let start = milli_timestamp();
    loop {
        if cancel.is_some_and(|c| c.load(std::sync::atomic::Ordering::Acquire)) {
            return Err(Error::Cancelled);
        }
        {
            let mut m = locks().lock().map_err(|_| Error::LockUnsupported)?;
            if !m.get(&path).copied().unwrap_or(false) {
                m.insert(path.clone(), true);
                return Ok(TimedAdvisoryLock { path, file });
            }
        }
        if milli_timestamp() - start >= deadline_ms as i64 {
            return Err(Error::LockBusy);
        }
        std::thread::sleep(Duration::from_millis(10.min(deadline_ms)))
    }
}
pub fn sleep(ns: u64) {
    std::thread::sleep(Duration::from_nanos(ns))
}
pub fn make_dir_recursive(path: impl AsRef<Path>) -> Result<()> {
    fs::create_dir_all(path).map_err(Error::Io)
}
pub fn realpath_alloc(path: impl AsRef<Path>) -> Result<PathBuf> {
    fs::canonicalize(path).map_err(Error::Io)
}
pub fn opened_file_path_alloc(file: &File) -> Result<PathBuf> {
    let m = file.metadata()?;
    if !m.is_file() || nlink(&m) == 0 {
        return Err(Error::HandlePathUnavailable);
    }
    #[cfg(target_os = "linux")]
    {
        {
            use std::os::fd::AsRawFd;
            let p = fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd()))
                .map_err(|_| Error::HandlePathUnavailable)?;
            if !p.is_absolute() || p.as_os_str().to_string_lossy().ends_with(" (deleted)") {
                return Err(Error::HandlePathUnavailable);
            }
            Ok(p)
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        Err(Error::HandlePathUnavailable)
    }
}
pub fn dir_realpath_alloc(dir: &VerifiedDir, sub_path: impl AsRef<Path>) -> Result<PathBuf> {
    realpath_alloc(dir.path.join(sub_path))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn temp() -> PathBuf {
        let p = std::env::temp_dir().join(format!("nyq-io-{}", nano_timestamp()));
        fs::create_dir(&p).unwrap();
        p
    }
    #[test]
    fn environment() {
        set_environ_block(vec![b"A=one".to_vec()]);
        assert_eq!(getenv(b"A"), Some(b"one".to_vec()));
        assert_eq!(
            clone_environ_map().unwrap().get(b"A".as_slice()),
            Some(&b"one".to_vec())
        )
    }
    #[test]
    fn atomic() {
        let p = temp();
        let f = p.join("x");
        write_file_atomic(&f, b"one").unwrap();
        write_file_atomic(&f, b"two").unwrap();
        assert_eq!(fs::read(&f).unwrap(), b"two");
        fs::remove_dir_all(p).unwrap()
    }
    #[test]
    fn capped_read() {
        let p = temp();
        let f = p.join("x");
        fs::write(&f, b"abc").unwrap();
        let mut h = File::open(f).unwrap();
        assert!(matches!(
            read_file_to_end(&mut h, 3),
            Err(Error::StreamTooLong)
        ));
        fs::remove_dir_all(p).unwrap()
    }
}
