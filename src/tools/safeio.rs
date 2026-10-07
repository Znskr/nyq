//! Descriptor-relative file IO.
//!
//! On Linux a workspace write opens the whole relative path in one `openat2`
//! against the root descriptor with `RESOLVE_BENEATH` only. Symlinks that stay
//! inside the root are followed; a symlink that escapes it is rejected.
//! `RESOLVE_BENEATH` has no equivalent on macOS or other Unix, so those
//! platforms keep `openat` + `O_NOFOLLOW` and refuse every symlink. Windows
//! refuses symlinks via `symlink_metadata`. Bytes land in a temporary file in
//! the destination directory and replace the target only after they are synced.
//! An existing target keeps its permission bits and owner.

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

/// Write `rel` underneath `root`.
///
/// Linux follows symlinks that resolve inside `root` and rejects the rest.
/// Other Unix platforms and Windows refuse every symlink; see the module docs.
pub fn write_beneath(root: &Path, rel: &Path, bytes: &[u8]) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        match write_resolving(root, rel, bytes) {
            Err(e) if e.raw_os_error() == Some(libc::ENOSYS) => {}
            Err(e) => return Err(explain_beneath(e)),
            Ok(()) => return Ok(()),
        }
    }
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

/// Read a regular file underneath `root`. Symlink policy matches [`write_beneath`].
pub fn read_beneath(root: &Path, rel: &Path, max: u64) -> io::Result<Vec<u8>> {
    #[cfg(target_os = "linux")]
    {
        match read_resolving(root, rel, max) {
            Err(e) if e.raw_os_error() == Some(libc::ENOSYS) => {}
            Err(e) => return Err(explain_beneath(e)),
            Ok(buf) => return Ok(buf),
        }
    }
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
        let dest = dir.join(name);
        if let Ok(meta) = fs::symlink_metadata(&dest) {
            if meta.file_type().is_symlink() {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "refusing to follow a symlink"));
            }
        }
        let was_readonly = fs::symlink_metadata(&dest).ok().is_some_and(|m| m.permissions().readonly());
        if was_readonly {
            let mut perms = fs::metadata(&dest)?.permissions();
            // Windows only: lift the read-only attribute so the rename can replace it.
            #[allow(clippy::permissions_set_readonly_false)]
            perms.set_readonly(false);
            fs::set_permissions(&dest, perms)?;
        }
        let tmp = dir.join(tmp_name());
        let write_tmp = (|| {
            {
                let mut file = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
                file.write_all(bytes)?;
                file.sync_all()?;
            }
            if was_readonly {
                let mut perms = fs::metadata(&tmp)?.permissions();
                perms.set_readonly(true);
                fs::set_permissions(&tmp, perms)?;
            }
            replace_existing(&tmp, &dest)
        })();
        if let Err(e) = write_tmp {
            let _ = fs::remove_file(&tmp);
            if was_readonly {
                if let Ok(meta) = fs::symlink_metadata(&dest) {
                    let mut perms = meta.permissions();
                    perms.set_readonly(true);
                    let _ = fs::set_permissions(&dest, perms);
                }
            }
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
    let dest = CString::new(name.as_bytes()).map_err(|_| {
        let _ = unlink_at(dir, &tmp_c);
        io::Error::new(io::ErrorKind::InvalidInput, "NUL in path")
    })?;
    if let Err(e) = copy_owner_mode(dir, &dest, &file) {
        let _ = unlink_at(dir, &tmp_c);
        return Err(e);
    }
    if let Err(e) = file.sync_all() {
        let _ = unlink_at(dir, &tmp_c);
        return Err(e);
    }
    drop(file);
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
fn copy_owner_mode(dir: &File, dest: &std::ffi::CStr, file: &File) -> io::Result<()> {
    use std::os::unix::io::AsRawFd;
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: `st` is a valid `stat` buffer and `dest` is NUL-terminated.
    let rc = unsafe { libc::fstatat(dir.as_raw_fd(), dest.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) };
    if rc < 0 {
        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::NotFound || err.raw_os_error() == Some(libc::ENOENT) {
            return Ok(());
        }
        return Err(err);
    }
    let fd = file.as_raw_fd();
    // SAFETY: `fd` is the temporary file just written in this directory.
    if unsafe { libc::fchmod(fd, (st.st_mode & 0o7777) as libc::mode_t) } < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: same fd. EPERM means the caller cannot adopt the original owner.
    let rc = unsafe { libc::fchown(fd, st.st_uid, st.st_gid) };
    if rc < 0 {
        let err = io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::EPERM) {
            return Err(err);
        }
    }
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
        match openat2(parent.as_raw_fd(), &c, libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC, 0, RESOLVE_NO_SYMLINKS | RESOLVE_BENEATH) {
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
        match openat2(parent.as_raw_fd(), &c, libc::O_RDONLY | libc::O_CLOEXEC, 0, RESOLVE_NO_SYMLINKS | RESOLVE_BENEATH) {
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

#[cfg(target_os = "linux")]
const RESOLVE_NO_SYMLINKS: u64 = 0x04;
#[cfg(target_os = "linux")]
const RESOLVE_BENEATH: u64 = 0x08;

#[cfg(all(unix, target_os = "linux"))]
fn openat2(dirfd: i32, path: &std::ffi::CStr, flags: i32, mode: u32, resolve: u64) -> io::Result<File> {
    use std::os::unix::io::FromRawFd;
    #[repr(C)]
    struct OpenHow {
        flags: u64,
        mode: u64,
        resolve: u64,
    }
    let how = OpenHow {
        flags: u64::from(flags as u32),
        mode: u64::from(mode),
        resolve,
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

#[cfg(target_os = "linux")]
fn explain_beneath(err: io::Error) -> io::Error {
    if err.raw_os_error() == Some(libc::ENOSYS) {
        return err;
    }
    match err.raw_os_error() {
        Some(libc::EXDEV) | Some(libc::ELOOP) => {
            io::Error::new(io::ErrorKind::InvalidInput, "refusing to follow a symlink")
        }
        _ => err,
    }
}

#[cfg(target_os = "linux")]
fn is_enoent(err: &io::Error) -> bool {
    err.kind() == io::ErrorKind::NotFound || err.raw_os_error() == Some(libc::ENOENT)
}

#[cfg(target_os = "linux")]
fn rel_cstr(rel: &Path) -> io::Result<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;
    let mut bytes = Vec::new();
    let mut first = true;
    for c in rel.components() {
        match c {
            Component::Normal(s) => {
                if !first {
                    bytes.push(b'/');
                }
                first = false;
                bytes.extend_from_slice(s.as_bytes());
            }
            Component::CurDir => {}
            _ => return Err(io::Error::new(io::ErrorKind::InvalidInput, "path escapes the sandbox root")),
        }
    }
    if bytes.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "missing file name"));
    }
    std::ffi::CString::new(bytes).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in path"))
}

#[cfg(target_os = "linux")]
fn open_raw(dir: &File, path: &std::ffi::CStr, flags: i32) -> io::Result<File> {
    use std::os::unix::io::AsRawFd;
    openat2(dir.as_raw_fd(), path, flags, 0, RESOLVE_BENEATH)
}

/// `RESOLVE_BENEATH` rejects every absolute symlink, even one whose target
/// stays inside `root`. When that happens, keep the write only if the
/// canonical path is still under `root`, and retry against that real path.
#[cfg(target_os = "linux")]
fn resolved_rel(root: &Path, rel: &Path) -> io::Result<PathBuf> {
    let root_real = fs::canonicalize(root)?;
    let full = root.join(rel);
    let mut rest = Vec::new();
    let mut cur = full.as_path();
    loop {
        match fs::canonicalize(cur) {
            Ok(real) => {
                if !real.starts_with(&root_real) {
                    return Err(io::Error::new(io::ErrorKind::InvalidInput, "refusing to follow a symlink"));
                }
                let mut out = real.strip_prefix(&root_real).unwrap_or(Path::new("")).to_path_buf();
                for name in rest.iter().rev() {
                    out.push(name);
                }
                if out.as_os_str().is_empty() {
                    return Err(io::Error::new(io::ErrorKind::InvalidInput, "refusing to follow a symlink"));
                }
                return Ok(out);
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let Some(name) = cur.file_name() else {
                    return Err(io::Error::new(io::ErrorKind::InvalidInput, "refusing to follow a symlink"));
                };
                rest.push(name.to_os_string());
                match cur.parent() {
                    Some(parent) if parent != cur => cur = parent,
                    _ => return Err(io::Error::new(io::ErrorKind::InvalidInput, "refusing to follow a symlink")),
                }
            }
            Err(_) => return Err(io::Error::new(io::ErrorKind::InvalidInput, "refusing to follow a symlink")),
        }
    }
}

#[cfg(target_os = "linux")]
fn read_resolving(root: &Path, rel: &Path, max: u64) -> io::Result<Vec<u8>> {
    match read_at(root, rel, max) {
        Err(e) if e.raw_os_error() == Some(libc::EXDEV) => read_at(root, &resolved_rel(root, rel)?, max),
        other => other,
    }
}

#[cfg(target_os = "linux")]
fn read_at(root: &Path, rel: &Path, max: u64) -> io::Result<Vec<u8>> {
    let root_fd = open_directory(root)?;
    let path = rel_cstr(rel)?;
    let file = open_raw(&root_fd, &path, libc::O_RDONLY | libc::O_CLOEXEC)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a regular file"));
    }
    let mut buf = Vec::new();
    file.take(max.saturating_add(1)).read_to_end(&mut buf)?;
    Ok(buf)
}

#[cfg(target_os = "linux")]
fn write_resolving(root: &Path, rel: &Path, bytes: &[u8]) -> io::Result<()> {
    match write_at(root, rel, bytes) {
        Err(e) if e.raw_os_error() == Some(libc::EXDEV) => write_at(root, &resolved_rel(root, rel)?, bytes),
        other => other,
    }
}

#[cfg(target_os = "linux")]
fn write_at(root: &Path, rel: &Path, bytes: &[u8]) -> io::Result<()> {
    let root_fd = open_directory(root)?;
    let path = rel_cstr(rel)?;
    match open_raw(&root_fd, &path, libc::O_RDONLY | libc::O_CLOEXEC) {
        Ok(file) => {
            if !file.metadata()?.is_file() {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a regular file"));
            }
            write_existing(&root_fd, &file, bytes)
        }
        Err(e) if is_enoent(&e) => write_new(&root_fd, rel, bytes),
        Err(e) => Err(e),
    }
}

/// Replace the file `openat2` already resolved, not the symlink that named it.
#[cfg(target_os = "linux")]
fn write_existing(root_fd: &File, file: &File, bytes: &[u8]) -> io::Result<()> {
    use std::os::unix::io::AsRawFd;
    let root_path = fs::read_link(format!("/proc/self/fd/{}", root_fd.as_raw_fd()))?;
    let file_path = fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd()))?;
    if !file_path.starts_with(&root_path) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "refusing to follow a symlink"));
    }
    let Some(name) = file_path.file_name() else {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "missing file name"));
    };
    let Some(parent) = file_path.parent() else {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "missing parent directory"));
    };
    let parent_fd = if parent == root_path {
        root_fd.try_clone()?
    } else {
        let rel = parent.strip_prefix(&root_path).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "refusing to follow a symlink")
        })?;
        let path = rel_cstr(rel)?;
        open_raw(root_fd, &path, libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC)?
    };
    let opened = fstat_fd(file)?;
    let named = fstatat_follow(&parent_fd, name)?;
    if opened.st_ino != named.st_ino || opened.st_dev != named.st_dev {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "file changed during write"));
    }
    replace_at(&parent_fd, name, bytes)
}

#[cfg(target_os = "linux")]
fn write_new(root_fd: &File, rel: &Path, bytes: &[u8]) -> io::Result<()> {
    let (dirs, file_name) = split_rel(rel)?;
    let mut current = root_fd.try_clone()?;
    let mut prefix = PathBuf::new();
    for name in &dirs {
        prefix.push(name);
        let path = rel_cstr(&prefix)?;
        match open_raw(root_fd, &path, libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC) {
            Ok(dir) => current = dir,
            Err(e) if is_enoent(&e) => {
                mkdir_at(&current, name)?;
                current = open_subdir(&current, name)?;
            }
            Err(e) => return Err(e),
        }
    }
    let path = rel_cstr(rel)?;
    match open_raw(root_fd, &path, libc::O_RDONLY | libc::O_CLOEXEC) {
        Ok(file) => {
            if !file.metadata()?.is_file() {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a regular file"));
            }
            write_existing(root_fd, &file, bytes)
        }
        Err(e) if is_enoent(&e) => replace_at(&current, &file_name, bytes),
        Err(e) => Err(e),
    }
}

#[cfg(target_os = "linux")]
fn fstat_fd(file: &File) -> io::Result<libc::stat> {
    use std::os::unix::io::AsRawFd;
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: `st` is a valid `stat` buffer and `file` is an open descriptor.
    let rc = unsafe { libc::fstat(file.as_raw_fd(), &mut st) };
    if rc < 0 { Err(io::Error::last_os_error()) } else { Ok(st) }
}

#[cfg(target_os = "linux")]
fn fstatat_follow(dir: &File, name: &std::ffi::OsStr) -> io::Result<libc::stat> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::io::AsRawFd;
    let c = CString::new(name.as_bytes()).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in path"))?;
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: `st` is a valid `stat` buffer and `c` is NUL-terminated.
    let rc = unsafe { libc::fstatat(dir.as_raw_fd(), c.as_ptr(), &mut st, 0) };
    if rc < 0 { Err(io::Error::last_os_error()) } else { Ok(st) }
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

    #[cfg(unix)]
    #[test]
    fn replace_keeps_permission_bits() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("deploy.sh");
        std::fs::write(&file, b"old").unwrap();
        for (mode, body) in [(0o755, &b"new"[..]), (0o600, &b"secret"[..])] {
            let mut perms = std::fs::metadata(&file).unwrap().permissions();
            perms.set_mode(mode);
            std::fs::set_permissions(&file, perms).unwrap();
            write_atomic(&file, body).unwrap();
            let got = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
            assert_eq!(got, mode, "mode changed after replace");
            assert_eq!(std::fs::read(&file).unwrap(), body);
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn write_beneath_follows_symlinks_inside_the_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("ws");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("README.md"), "old").unwrap();
        std::fs::create_dir(root.join("docs")).unwrap();
        std::os::unix::fs::symlink("../README.md", root.join("docs/README.md")).unwrap();
        write_beneath(&root, Path::new("docs/README.md"), b"new").unwrap();
        assert_eq!(std::fs::read_to_string(root.join("README.md")).unwrap(), "new");
        assert!(std::fs::symlink_metadata(root.join("docs/README.md")).unwrap().file_type().is_symlink());
        assert_eq!(read_beneath(&root, Path::new("docs/README.md"), 100).unwrap(), b"new");

        let abs_target = root.join("abs-target.txt");
        std::fs::write(&abs_target, "abs").unwrap();
        std::os::unix::fs::symlink(&abs_target, root.join("abs-link")).unwrap();
        write_beneath(&root, Path::new("abs-link"), b"ABS").unwrap();
        assert_eq!(std::fs::read_to_string(&abs_target).unwrap(), "ABS");
        assert!(std::fs::symlink_metadata(root.join("abs-link")).unwrap().file_type().is_symlink());

        std::fs::create_dir_all(root.join("node_modules/.pnpm/pkg")).unwrap();
        std::fs::create_dir_all(root.join("packages/web/node_modules")).unwrap();
        std::os::unix::fs::symlink(
            "../../../node_modules/.pnpm/pkg",
            root.join("packages/web/node_modules/@acme"),
        )
        .unwrap();
        write_beneath(&root, Path::new("packages/web/node_modules/@acme/file.txt"), b"ok").unwrap();
        assert_eq!(std::fs::read_to_string(root.join("node_modules/.pnpm/pkg/file.txt")).unwrap(), "ok");
    }
}
