//! Descriptor-relative file IO.
//!
//! Workspace writes open each path component with `openat2(RESOLVE_BENEATH |
//! RESOLVE_NO_SYMLINKS)` on Linux (and `openat` + `O_NOFOLLOW` elsewhere) so a
//! symlink swapped in after the sandbox check cannot redirect the write.
//! Bytes land in a temporary file in the destination directory and replace the
//! target only after they are synced.

use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Read at most `max + 1` bytes of a regular file.
///
/// Callers treat a longer buffer as "over the limit". Devices and other
/// non-regular files are rejected before any read, so `/dev/zero` cannot grow
/// without bound.
pub fn read_capped(path: &Path, max: u64) -> io::Result<Vec<u8>> {
    let file = File::open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a regular file"));
    }
    let mut buf = Vec::new();
    file.take(max.saturating_add(1)).read_to_end(&mut buf)?;
    Ok(buf)
}

/// `user_path` relative to `root` after resolving `.` and `..`, or None when
/// it escapes `root`.
pub fn relative_under(root: &Path, user_path: &str) -> Option<PathBuf> {
    let root = normalize(root);
    let full = if Path::new(user_path).is_absolute() {
        normalize(Path::new(user_path))
    } else {
        normalize(&root.join(user_path))
    };
    let rel = full.strip_prefix(&root).ok()?;
    if rel.as_os_str().is_empty() {
        return None;
    }
    Some(rel.to_path_buf())
}

fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => match out.components().next_back() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                Some(Component::ParentDir) => out.push(".."),
                _ => {}
            },
            other => out.push(other),
        }
    }
    out
}

fn split_rel(rel: &Path) -> io::Result<(Vec<OsString>, OsString)> {
    let mut names = Vec::new();
    for c in rel.components() {
        match c {
            Component::Normal(s) => names.push(s.to_os_string()),
            Component::CurDir => {}
            _ => {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "path escapes the sandbox root"));
            }
        }
    }
    let Some(file_name) = names.pop() else {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "missing file name"));
    };
    Ok((names, file_name))
}

/// Replace `path`, following a final symlink (unsandboxed edits). The previous
/// contents stay intact until the new bytes are durable.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    let target = follow_final(path)?;
    let parent = target.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let Some(name) = target.file_name() else {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "missing file name"));
    };
    replace_in_dir(parent, name, bytes)
}

fn follow_final(path: &Path) -> io::Result<PathBuf> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => fs::canonicalize(path),
        Ok(_) => Ok(path.to_path_buf()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let Some(name) = path.file_name() else {
                return Ok(path.to_path_buf());
            };
            match path.parent() {
                Some(parent) if !parent.as_os_str().is_empty() => {
                    Ok(fs::canonicalize(parent).unwrap_or_else(|_| parent.to_path_buf()).join(name))
                }
                _ => Ok(path.to_path_buf()),
            }
        }
        Err(e) => Err(e),
    }
}

fn tmp_name() -> String {
    let n = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tick = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!(".nyq-{}-{tick}-{n}.tmp", std::process::id())
}

/// Write `rel` underneath `root` without following symlinks.
pub fn write_beneath(root: &Path, rel: &Path, bytes: &[u8]) -> io::Result<()> {
    #[cfg(unix)]
    {
        let (dir, name) = descend(root, rel, true)?;
        match open_read(&dir, &name) {
            Ok(file) => {
                if !file.metadata()?.is_file() {
                    return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a regular file"));
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        replace_at(&dir, &name, bytes)
    }
    #[cfg(not(unix))]
    {
        let (parent, name) = walk_beneath(root, rel, true)?;
        if let Ok(meta) = fs::symlink_metadata(parent.join(&name)) {
            if meta.file_type().is_symlink() {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "refusing to follow a symlink"));
            }
            if !meta.is_file() {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a regular file"));
            }
        }
        replace_in_dir(&parent, &name, bytes)
    }
}

/// Read a regular file underneath `root` without following symlinks.
pub fn read_beneath(root: &Path, rel: &Path, max: u64) -> io::Result<Vec<u8>> {
    #[cfg(unix)]
    {
        let (dir, name) = descend(root, rel, false)?;
        let file = open_read(&dir, &name)?;
        if !file.metadata()?.is_file() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a regular file"));
        }
        let mut buf = Vec::new();
        file.take(max.saturating_add(1)).read_to_end(&mut buf)?;
        Ok(buf)
    }
    #[cfg(not(unix))]
    {
        let (parent, name) = walk_beneath(root, rel, false)?;
        let path = parent.join(name);
        let meta = fs::symlink_metadata(&path)?;
        if meta.file_type().is_symlink() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "refusing to follow a symlink"));
        }
        read_capped(&path, max)
    }
}

#[cfg(not(unix))]
fn walk_beneath(root: &Path, rel: &Path, create: bool) -> io::Result<(PathBuf, OsString)> {
    let mut cur = fs::canonicalize(root)?;
    let (dirs, file_name) = split_rel(rel)?;
    for name in dirs {
        let next = cur.join(&name);
        match fs::symlink_metadata(&next) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "refusing to follow a symlink"));
            }
            Ok(meta) if meta.is_dir() => cur = next,
            Ok(_) => return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a directory")),
            Err(e) if create && e.kind() == io::ErrorKind::NotFound => {
                fs::create_dir(&next)?;
                let meta = fs::symlink_metadata(&next)?;
                if meta.file_type().is_symlink() || !meta.is_dir() {
                    return Err(io::Error::new(io::ErrorKind::InvalidInput, "refusing to follow a symlink"));
                }
                cur = next;
            }
            Err(e) => return Err(e),
        }
    }
    Ok((cur, file_name))
}

fn replace_in_dir(dir: &Path, name: &std::ffi::OsStr, bytes: &[u8]) -> io::Result<()> {
    #[cfg(unix)]
    {
        let parent = open_directory(dir)?;
        replace_at(&parent, name, bytes)
    }
    #[cfg(not(unix))]
    {
        if let Ok(meta) = fs::symlink_metadata(dir.join(name)) {
            if meta.file_type().is_symlink() {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "refusing to follow a symlink"));
            }
        }
        let tmp = dir.join(tmp_name());
        {
            let mut file = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
            file.write_all(bytes)?;
            file.sync_all()?;
        }
        if let Err(e) = replace_existing(&tmp, &dir.join(name)) {
            let _ = fs::remove_file(&tmp);
            return Err(e);
        }
        Ok(())
    }
}

#[cfg(not(unix))]
fn replace_existing(from: &Path, to: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    let from_w: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
    let to_w: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;
    #[link(name = "kernel32")]
    extern "system" {
        fn MoveFileExW(from: *const u16, to: *const u16, flags: u32) -> i32;
    }
    let rc = unsafe { MoveFileExW(from_w.as_ptr(), to_w.as_ptr(), MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH) };
    if rc == 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

#[cfg(unix)]
fn descend(root: &Path, rel: &Path, create: bool) -> io::Result<(File, OsString)> {
    let (dirs, file_name) = split_rel(rel)?;
    let mut current = open_directory(root)?;
    for name in dirs {
        match open_subdir(&current, &name) {
            Ok(dir) => current = dir,
            Err(e) if create && e.kind() == io::ErrorKind::NotFound => {
                mkdir_at(&current, &name)?;
                current = open_subdir(&current, &name)?;
            }
            Err(e) => return Err(e),
        }
    }
    Ok((current, file_name))
}

#[cfg(unix)]
fn replace_at(dir: &File, name: &std::ffi::OsStr, bytes: &[u8]) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::io::{AsRawFd, FromRawFd};

    let tmp = tmp_name();
    let tmp_c = CString::new(tmp.as_bytes()).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in path"))?;
    let flags = libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    let fd = unsafe { libc::openat(dir.as_raw_fd(), tmp_c.as_ptr(), flags, 0o666) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `openat` succeeded and returned a fresh descriptor we own.
    let mut file = unsafe { File::from_raw_fd(fd) };
    if let Err(e) = file.write_all(bytes).and_then(|_| file.sync_all()) {
        let _ = unlink_at(dir, &tmp_c);
        return Err(e);
    }
    drop(file);
    let dest = CString::new(name.as_bytes()).map_err(|_| {
        let _ = unlink_at(dir, &tmp_c);
        io::Error::new(io::ErrorKind::InvalidInput, "NUL in path")
    })?;
    let rc = unsafe { libc::renameat(dir.as_raw_fd(), tmp_c.as_ptr(), dir.as_raw_fd(), dest.as_ptr()) };
    if rc < 0 {
        let err = io::Error::last_os_error();
        let _ = unlink_at(dir, &tmp_c);
        return Err(err);
    }
    let _ = dir.sync_all();
    Ok(())
}

#[cfg(unix)]
fn unlink_at(dir: &File, name: &std::ffi::CStr) -> io::Result<()> {
    use std::os::unix::io::AsRawFd;
    let rc = unsafe { libc::unlinkat(dir.as_raw_fd(), name.as_ptr(), 0) };
    if rc < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

#[cfg(unix)]
fn open_directory(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    let canon = fs::canonicalize(path)?;
    let mut opts = OpenOptions::new();
    opts.read(true).custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC);
    opts.open(canon)
}

#[cfg(unix)]
fn open_subdir(parent: &File, name: &std::ffi::OsStr) -> io::Result<File> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::io::{AsRawFd, FromRawFd};

    let c = CString::new(name.as_bytes()).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in path"))?;
    #[cfg(target_os = "linux")]
    {
        match openat2(parent.as_raw_fd(), &c, libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC, 0) {
            Err(e) if e.raw_os_error() == Some(libc::ENOSYS) => {}
            other => return map_loop(other),
        }
    }
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            c.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        map_loop(Err(io::Error::last_os_error()))
    } else {
        // SAFETY: `openat` succeeded and returned a fresh descriptor we own.
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

#[cfg(unix)]
fn open_read(parent: &File, name: &std::ffi::OsStr) -> io::Result<File> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::io::{AsRawFd, FromRawFd};

    let c = CString::new(name.as_bytes()).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in path"))?;
    #[cfg(target_os = "linux")]
    {
        match openat2(parent.as_raw_fd(), &c, libc::O_RDONLY | libc::O_CLOEXEC, 0) {
            Err(e) if e.raw_os_error() == Some(libc::ENOSYS) => {}
            other => return map_loop(other),
        }
    }
    let fd = unsafe { libc::openat(parent.as_raw_fd(), c.as_ptr(), libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC) };
    if fd < 0 {
        map_loop(Err(io::Error::last_os_error()))
    } else {
        // SAFETY: `openat` succeeded and returned a fresh descriptor we own.
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

#[cfg(all(unix, target_os = "linux"))]
fn openat2(dirfd: i32, path: &std::ffi::CStr, flags: i32, mode: u32) -> io::Result<File> {
    use std::os::unix::io::FromRawFd;
    #[repr(C)]
    struct OpenHow {
        flags: u64,
        mode: u64,
        resolve: u64,
    }
    const RESOLVE_NO_SYMLINKS: u64 = 0x04;
    const RESOLVE_BENEATH: u64 = 0x08;
    let how = OpenHow {
        flags: u64::from(flags as u32),
        mode: u64::from(mode),
        resolve: RESOLVE_NO_SYMLINKS | RESOLVE_BENEATH,
    };
    // SAFETY: `how` is a valid `open_how` and outlives the syscall.
    let fd = unsafe { libc::syscall(libc::SYS_openat2, dirfd, path.as_ptr(), &how, std::mem::size_of::<OpenHow>()) };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        let fd = std::os::unix::io::RawFd::try_from(fd).map_err(|_| io::Error::other("bad file descriptor"))?;
        // SAFETY: the syscall returned a new descriptor we own.
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

#[cfg(unix)]
fn mkdir_at(parent: &File, name: &std::ffi::OsStr) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::io::AsRawFd;
    let c = CString::new(name.as_bytes()).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in path"))?;
    let rc = unsafe { libc::mkdirat(parent.as_raw_fd(), c.as_ptr(), 0o777) };
    if rc == 0 {
        return Ok(());
    }
    let err = io::Error::last_os_error();
    if err.kind() == io::ErrorKind::AlreadyExists {
        open_subdir(parent, name).map(|_| ())
    } else {
        Err(err)
    }
}

#[cfg(unix)]
fn map_loop(result: io::Result<File>) -> io::Result<File> {
    match result {
        Err(e) if e.raw_os_error() == Some(libc::ELOOP) => {
            Err(io::Error::new(io::ErrorKind::InvalidInput, "refusing to follow a symlink"))
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_under_rejects_escape() {
        let root = Path::new("/tmp/ws");
        assert_eq!(relative_under(root, "a/b").unwrap(), PathBuf::from("a/b"));
        assert_eq!(relative_under(root, "/tmp/ws/a").unwrap(), PathBuf::from("a"));
        assert_eq!(relative_under(root, "sub/../file").unwrap(), PathBuf::from("file"));
        assert!(relative_under(root, "../etc/passwd").is_none());
        assert!(relative_under(root, "a/../../etc/passwd").is_none());
        assert!(relative_under(root, "/etc/passwd").is_none());
        assert!(relative_under(root, "/tmp/ws").is_none());
    }

    #[test]
    fn capped_read_rejects_non_regular_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello").unwrap();
        assert_eq!(read_capped(&dir.path().join("a.txt"), 10).unwrap(), b"hello");
        #[cfg(unix)]
        if Path::new("/dev/zero").exists() {
            let err = read_capped(Path::new("/dev/zero"), 32).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{err}");
            assert!(err.to_string().contains("regular"), "{err}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn write_beneath_does_not_follow_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside.txt");
        std::fs::write(&outside, "secret").unwrap();
        let root = dir.path().join("ws");
        std::fs::create_dir(&root).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
        let err = write_beneath(&root, Path::new("link"), b"pwned").unwrap_err();
        assert!(err.to_string().contains("symlink"), "{err}");
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "secret");
        write_beneath(&root, Path::new("nested/file.txt"), b"ok").unwrap();
        assert_eq!(std::fs::read_to_string(root.join("nested/file.txt")).unwrap(), "ok");
    }
}
