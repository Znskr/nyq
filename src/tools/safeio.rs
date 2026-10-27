//! Descriptor-relative file IO.
//!
//! On Linux a workspace write walks one component at a time with `openat2`
//! (`RESOLVE_BENEATH` and `RESOLVE_NO_SYMLINKS`). A symlink is resolved from
//! its directory entry and followed only when the target stays inside the
//! root, including a dangling link. A symlink that escapes the root is
//! rejected. The walk does not use `/proc`.
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
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a regular file",
        ));
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
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "path escapes the sandbox root",
                ));
            }
        }
    }
    let Some(file_name) = names.pop() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "missing file name",
        ));
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
    if let Some(parent) = target.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    let parent = target
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let Some(name) = target.file_name() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "missing file name",
        ));
    };
    replace_in_dir(parent, name, bytes)
}

fn follow_final(path: &Path) -> io::Result<PathBuf> {
    follow_final_depth(path, 0)
}

fn follow_final_depth(path: &Path, depth: usize) -> io::Result<PathBuf> {
    if depth > 40 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "too many symlinks",
        ));
    }
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            let link = fs::read_link(path)?;
            let target = if link.is_absolute() {
                link
            } else {
                path.parent().unwrap_or(Path::new(".")).join(link)
            };
            follow_final_depth(&target, depth + 1)
        }
        Ok(_) => Ok(path.to_path_buf()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let Some(name) = path.file_name() else {
                return Ok(path.to_path_buf());
            };
            match path.parent() {
                Some(parent) if !parent.as_os_str().is_empty() => Ok(fs::canonicalize(parent)
                    .unwrap_or_else(|_| parent.to_path_buf())
                    .join(name)),
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
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "not a regular file",
                    ));
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
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "refusing to follow a symlink",
                ));
            }
            if !meta.is_file() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "not a regular file",
                ));
            }
        }
        replace_in_dir(&parent, &name, bytes)
    }
}

/// Remove `rel` underneath `root` without following a symlink that leaves it.
///
/// `NotFound` means the file is already gone. A symlink is removed only when
/// it is the final component; intermediate links are followed when they stay
/// inside `root` and rejected otherwise, matching [`write_beneath`].
pub fn remove_beneath(root: &Path, rel: &Path) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        match remove_at(root, rel) {
            Err(e) if e.raw_os_error() == Some(libc::ENOSYS) => {}
            Err(e) => return Err(explain_beneath(e)),
            Ok(()) => return Ok(()),
        }
    }
    #[cfg(unix)]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let (dir, name) = descend(root, rel, false)?;
        let c = CString::new(name.as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in path"))?;
        unlink_at(&dir, &c)
    }
    #[cfg(not(unix))]
    {
        let (parent, name) = walk_beneath(root, rel, false)?;
        fs::remove_file(parent.join(name))
    }
}

/// Replace `to` with `from`, including when `to` already exists.
pub fn rename_over(from: &Path, to: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        fs::rename(from, to)
    }
    #[cfg(not(unix))]
    {
        replace_existing(from, to)
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
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "not a regular file",
            ));
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
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "refusing to follow a symlink",
            ));
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
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "refusing to follow a symlink",
                ));
            }
            Ok(meta) if meta.is_dir() => cur = next,
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "not a directory",
                ))
            }
            Err(e) if create && e.kind() == io::ErrorKind::NotFound => {
                fs::create_dir(&next)?;
                let meta = fs::symlink_metadata(&next)?;
                if meta.file_type().is_symlink() || !meta.is_dir() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "refusing to follow a symlink",
                    ));
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
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "refusing to follow a symlink",
                ));
            }
        }
        let was_readonly = fs::symlink_metadata(&dest)
            .ok()
            .is_some_and(|m| m.permissions().readonly());
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
            rename_over(&tmp, &dest)
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
    let rc = unsafe {
        MoveFileExW(
            from_w.as_ptr(),
            to_w.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if rc == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
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
    let tmp_c = CString::new(tmp.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in path"))?;
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
    let rc = unsafe {
        libc::renameat(
            dir.as_raw_fd(),
            tmp_c.as_ptr(),
            dir.as_raw_fd(),
            dest.as_ptr(),
        )
    };
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
    let rc = unsafe {
        libc::fstatat(
            dir.as_raw_fd(),
            dest.as_ptr(),
            &mut st,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if rc < 0 {
        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::NotFound || err.raw_os_error() == Some(libc::ENOENT) {
            return Ok(());
        }
        return Err(err);
    }
    let fd = file.as_raw_fd();
    // Owner first: on Linux `fchown` clears setuid/setgid even when the ids
    // do not change, so the mode has to be applied afterwards.
    // SAFETY: `fd` is the temporary file just written in this directory.
    // EPERM means the caller cannot adopt the original owner.
    let rc = unsafe { libc::fchown(fd, st.st_uid, st.st_gid) };
    if rc < 0 {
        let err = io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::EPERM) {
            return Err(err);
        }
    }
    if unsafe { libc::fchmod(fd, (st.st_mode & 0o7777) as libc::mode_t) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(unix)]
fn unlink_at(dir: &File, name: &std::ffi::CStr) -> io::Result<()> {
    use std::os::unix::io::AsRawFd;
    let rc = unsafe { libc::unlinkat(dir.as_raw_fd(), name.as_ptr(), 0) };
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(unix)]
fn open_directory(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    let canon = fs::canonicalize(path)?;
    let mut opts = OpenOptions::new();
    opts.read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC);
    opts.open(canon)
}

#[cfg(unix)]
fn open_subdir(parent: &File, name: &std::ffi::OsStr) -> io::Result<File> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::io::{AsRawFd, FromRawFd};

    let c = CString::new(name.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in path"))?;
    #[cfg(target_os = "linux")]
    {
        match openat2(
            parent.as_raw_fd(),
            &c,
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            0,
            RESOLVE_NO_SYMLINKS | RESOLVE_BENEATH,
        ) {
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

    let c = CString::new(name.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in path"))?;
    #[cfg(target_os = "linux")]
    {
        match openat2(
            parent.as_raw_fd(),
            &c,
            libc::O_RDONLY | libc::O_CLOEXEC,
            0,
            RESOLVE_NO_SYMLINKS | RESOLVE_BENEATH,
        ) {
            Err(e) if e.raw_os_error() == Some(libc::ENOSYS) => {}
            other => return map_loop(other),
        }
    }
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            c.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
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
fn openat2(
    dirfd: i32,
    path: &std::ffi::CStr,
    flags: i32,
    mode: u32,
    resolve: u64,
) -> io::Result<File> {
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
    let fd = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            dirfd,
            path.as_ptr(),
            &how,
            std::mem::size_of::<OpenHow>(),
        )
    };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        let fd = std::os::unix::io::RawFd::try_from(fd)
            .map_err(|_| io::Error::other("bad file descriptor"))?;
        // SAFETY: the syscall returned a new descriptor we own.
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

#[cfg(unix)]
fn mkdir_at(parent: &File, name: &std::ffi::OsStr) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::io::AsRawFd;
    let c = CString::new(name.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in path"))?;
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
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "path escapes the sandbox root",
                ))
            }
        }
    }
    if bytes.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "missing file name",
        ));
    }
    std::ffi::CString::new(bytes)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in path"))
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
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "refusing to follow a symlink",
                    ));
                }
                let mut out = real
                    .strip_prefix(&root_real)
                    .unwrap_or(Path::new(""))
                    .to_path_buf();
                for name in rest.iter().rev() {
                    out.push(name);
                }
                if out.as_os_str().is_empty() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "refusing to follow a symlink",
                    ));
                }
                return Ok(out);
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let Some(name) = cur.file_name() else {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "refusing to follow a symlink",
                    ));
                };
                rest.push(name.to_os_string());
                match cur.parent() {
                    Some(parent) if parent != cur => cur = parent,
                    _ => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "refusing to follow a symlink",
                        ))
                    }
                }
            }
            Err(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "refusing to follow a symlink",
                ))
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn read_resolving(root: &Path, rel: &Path, max: u64) -> io::Result<Vec<u8>> {
    match read_at(root, rel, max) {
        Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {
            read_at(root, &resolved_rel(root, rel)?, max)
        }
        other => other,
    }
}

#[cfg(target_os = "linux")]
fn read_at(root: &Path, rel: &Path, max: u64) -> io::Result<Vec<u8>> {
    let root_fd = open_directory(root)?;
    let path = rel_cstr(rel)?;
    let file = open_raw(&root_fd, &path, libc::O_RDONLY | libc::O_CLOEXEC)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a regular file",
        ));
    }
    let mut buf = Vec::new();
    file.take(max.saturating_add(1)).read_to_end(&mut buf)?;
    Ok(buf)
}

#[cfg(target_os = "linux")]
fn write_resolving(root: &Path, rel: &Path, bytes: &[u8]) -> io::Result<()> {
    match write_at(root, rel, bytes) {
        Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {
            write_at(root, &resolved_rel(root, rel)?, bytes)
        }
        other => other,
    }
}

#[cfg(target_os = "linux")]
fn write_at(root: &Path, rel: &Path, bytes: &[u8]) -> io::Result<()> {
    let root_fd = open_directory(root)?;
    let parts = path_parts(rel);
    if parts.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "missing file name",
        ));
    }
    write_parts(&root_fd, root, &parts, bytes, 0)
}

#[cfg(target_os = "linux")]
fn remove_at(root: &Path, rel: &Path) -> io::Result<()> {
    let root_fd = open_directory(root)?;
    let parts = path_parts(rel);
    if parts.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "missing file name",
        ));
    }
    remove_parts(&root_fd, root, &parts, 0)
}

#[cfg(target_os = "linux")]
enum Entry {
    Dir(File),
    File,
    Link(PathBuf),
    Missing,
}

#[cfg(target_os = "linux")]
fn classify(dir: &File, name: &std::ffi::OsStr, want_dir: bool) -> io::Result<Entry> {
    let st = match fstat_at(dir, name, libc::AT_SYMLINK_NOFOLLOW) {
        Ok(st) => st,
        Err(e) if is_enoent(&e) => return Ok(Entry::Missing),
        Err(e) => return Err(e),
    };
    let kind = st.st_mode & libc::S_IFMT;
    if kind == libc::S_IFLNK {
        return Ok(Entry::Link(read_link_at(dir, name)?));
    }
    if kind == libc::S_IFDIR {
        if !want_dir {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "not a regular file",
            ));
        }
        return Ok(Entry::Dir(open_subdir(dir, name)?));
    }
    if kind == libc::S_IFREG {
        if want_dir {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "not a directory",
            ));
        }
        let file = open_read(dir, name)?;
        let opened = fstat_fd(&file)?;
        if opened.st_ino != st.st_ino || opened.st_dev != st.st_dev {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "file changed during write",
            ));
        }
        return Ok(Entry::File);
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        if want_dir {
            "not a directory"
        } else {
            "not a regular file"
        },
    ))
}

#[cfg(target_os = "linux")]
fn write_parts(
    root_fd: &File,
    root: &Path,
    parts: &[OsString],
    bytes: &[u8],
    depth: u32,
) -> io::Result<()> {
    if depth > 32 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "refusing to follow a symlink",
        ));
    }
    let mut current = root_fd.try_clone()?;
    let mut walked = PathBuf::new();
    for (i, name) in parts.iter().enumerate() {
        let last = i + 1 == parts.len();
        match classify(&current, name, !last)? {
            Entry::Missing if !last => {
                mkdir_at(&current, name)?;
                current = open_subdir(&current, name)?;
                walked.push(name);
            }
            Entry::Missing => return replace_at(&current, name, bytes),
            Entry::Dir(dir) => {
                current = dir;
                walked.push(name);
            }
            Entry::File => return replace_at(&current, name, bytes),
            Entry::Link(target) => {
                let next = retarget(root, &walked, &target, &parts[i + 1..])?;
                return write_parts(root_fd, root, &next, bytes, depth + 1);
            }
        }
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "missing file name",
    ))
}

#[cfg(target_os = "linux")]
fn remove_parts(root_fd: &File, root: &Path, parts: &[OsString], depth: u32) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    if depth > 32 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "refusing to follow a symlink",
        ));
    }
    let mut current = root_fd.try_clone()?;
    let mut walked = PathBuf::new();
    for (i, name) in parts.iter().enumerate() {
        let last = i + 1 == parts.len();
        match classify(&current, name, !last)? {
            Entry::Missing => return Err(io::Error::new(io::ErrorKind::NotFound, "not found")),
            Entry::Dir(dir) => {
                current = dir;
                walked.push(name);
            }
            Entry::File => {
                let c = CString::new(name.as_bytes())
                    .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in path"))?;
                return unlink_at(&current, &c);
            }
            Entry::Link(target) => {
                let next = retarget(root, &walked, &target, &parts[i + 1..])?;
                return remove_parts(root_fd, root, &next, depth + 1);
            }
        }
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "missing file name",
    ))
}

#[cfg(target_os = "linux")]
fn path_parts(path: &Path) -> Vec<OsString> {
    path.components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s.to_os_string()),
            _ => None,
        })
        .collect()
}

#[cfg(target_os = "linux")]
fn retarget(
    root: &Path,
    walked: &Path,
    target: &Path,
    rest: &[OsString],
) -> io::Result<Vec<OsString>> {
    let base = if target.is_absolute() {
        absolute_under(root, target)?
    } else {
        normalize_rel(&walked.join(target))?
    };
    let mut parts = path_parts(&base);
    parts.extend(rest.iter().cloned());
    if parts.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "refusing to follow a symlink",
        ));
    }
    Ok(parts)
}

#[cfg(target_os = "linux")]
fn normalize_rel(path: &Path) -> io::Result<PathBuf> {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::Normal(s) => out.push(s),
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "refusing to follow a symlink",
                    ));
                }
            }
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "refusing to follow a symlink",
                ))
            }
        }
    }
    Ok(out)
}

/// Resolve an absolute symlink target that may not exist yet, and keep it
/// only when the existing prefix is inside `root`.
#[cfg(target_os = "linux")]
fn absolute_under(root: &Path, target: &Path) -> io::Result<PathBuf> {
    let root_real = fs::canonicalize(root)?;
    let mut cur = target.to_path_buf();
    let mut suffix = Vec::new();
    loop {
        match fs::canonicalize(&cur) {
            Ok(real) => {
                if !real.starts_with(&root_real) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "refusing to follow a symlink",
                    ));
                }
                let mut out = real
                    .strip_prefix(&root_real)
                    .unwrap_or(Path::new(""))
                    .to_path_buf();
                for name in suffix.iter().rev() {
                    out.push(name);
                }
                return Ok(out);
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let Some(name) = cur.file_name() else {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "refusing to follow a symlink",
                    ));
                };
                suffix.push(name.to_os_string());
                match cur.parent() {
                    Some(parent) if parent != cur => cur = parent.to_path_buf(),
                    _ => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "refusing to follow a symlink",
                        ))
                    }
                }
            }
            Err(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "refusing to follow a symlink",
                ))
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn read_link_at(dir: &File, name: &std::ffi::OsStr) -> io::Result<PathBuf> {
    use std::ffi::CString;
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::os::unix::io::AsRawFd;
    let c = CString::new(name.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in path"))?;
    let mut buf = vec![0u8; 256];
    loop {
        // SAFETY: `c` is NUL-terminated and `buf` is writable for `buf.len()` bytes.
        let rc = unsafe {
            libc::readlinkat(
                dir.as_raw_fd(),
                c.as_ptr(),
                buf.as_mut_ptr().cast(),
                buf.len(),
            )
        };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        let n = rc as usize;
        if n < buf.len() {
            buf.truncate(n);
            return Ok(PathBuf::from(std::ffi::OsString::from_vec(buf)));
        }
        if buf.len() >= 64 * 1024 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "symlink target too long",
            ));
        }
        buf.resize(buf.len() * 2, 0);
    }
}

#[cfg(target_os = "linux")]
fn fstat_fd(file: &File) -> io::Result<libc::stat> {
    use std::os::unix::io::AsRawFd;
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: `st` is a valid `stat` buffer and `file` is an open descriptor.
    let rc = unsafe { libc::fstat(file.as_raw_fd(), &mut st) };
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(st)
    }
}

#[cfg(target_os = "linux")]
fn fstat_at(dir: &File, name: &std::ffi::OsStr, flags: i32) -> io::Result<libc::stat> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::io::AsRawFd;
    let c = CString::new(name.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in path"))?;
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: `st` is a valid `stat` buffer and `c` is NUL-terminated.
    let rc = unsafe { libc::fstatat(dir.as_raw_fd(), c.as_ptr(), &mut st, flags) };
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(st)
    }
}

#[cfg(unix)]
fn map_loop(result: io::Result<File>) -> io::Result<File> {
    match result {
        Err(e) if e.raw_os_error() == Some(libc::ELOOP) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "refusing to follow a symlink",
        )),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn atomic_write_follows_dangling_symlink_chains() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        symlink("second", dir.path().join("first")).unwrap();
        symlink("new/target", dir.path().join("second")).unwrap();
        write_atomic(&dir.path().join("first"), b"created").unwrap();
        assert_eq!(fs::read(dir.path().join("new/target")).unwrap(), b"created");
        assert!(fs::symlink_metadata(dir.path().join("first"))
            .unwrap()
            .file_type()
            .is_symlink());
        symlink("cycle", dir.path().join("cycle")).unwrap();
        assert!(write_atomic(&dir.path().join("cycle"), b"no").is_err());
    }

    #[test]
    fn relative_under_rejects_escape() {
        let root = Path::new("/tmp/ws");
        assert_eq!(relative_under(root, "a/b").unwrap(), PathBuf::from("a/b"));
        assert_eq!(
            relative_under(root, "/tmp/ws/a").unwrap(),
            PathBuf::from("a")
        );
        assert_eq!(
            relative_under(root, "sub/../file").unwrap(),
            PathBuf::from("file")
        );
        assert!(relative_under(root, "../etc/passwd").is_none());
        assert!(relative_under(root, "a/../../etc/passwd").is_none());
        assert!(relative_under(root, "/etc/passwd").is_none());
        assert!(relative_under(root, "/tmp/ws").is_none());
    }

    #[test]
    fn capped_read_rejects_non_regular_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello").unwrap();
        assert_eq!(
            read_capped(&dir.path().join("a.txt"), 10).unwrap(),
            b"hello"
        );
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
        assert_eq!(
            std::fs::read_to_string(root.join("nested/file.txt")).unwrap(),
            "ok"
        );
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
        assert_eq!(
            std::fs::read_to_string(root.join("README.md")).unwrap(),
            "new"
        );
        assert!(std::fs::symlink_metadata(root.join("docs/README.md"))
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(
            read_beneath(&root, Path::new("docs/README.md"), 100).unwrap(),
            b"new"
        );

        let abs_target = root.join("abs-target.txt");
        std::fs::write(&abs_target, "abs").unwrap();
        std::os::unix::fs::symlink(&abs_target, root.join("abs-link")).unwrap();
        write_beneath(&root, Path::new("abs-link"), b"ABS").unwrap();
        assert_eq!(std::fs::read_to_string(&abs_target).unwrap(), "ABS");
        assert!(std::fs::symlink_metadata(root.join("abs-link"))
            .unwrap()
            .file_type()
            .is_symlink());

        std::fs::create_dir_all(root.join("node_modules/.pnpm/pkg")).unwrap();
        std::fs::create_dir_all(root.join("packages/web/node_modules")).unwrap();
        std::os::unix::fs::symlink(
            "../../../node_modules/.pnpm/pkg",
            root.join("packages/web/node_modules/@acme"),
        )
        .unwrap();
        write_beneath(
            &root,
            Path::new("packages/web/node_modules/@acme/file.txt"),
            b"ok",
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("node_modules/.pnpm/pkg/file.txt")).unwrap(),
            "ok"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn write_beneath_creates_the_target_of_a_dangling_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("ws");
        std::fs::create_dir(&root).unwrap();
        std::os::unix::fs::symlink("generated/config", root.join("alias")).unwrap();
        write_beneath(&root, Path::new("alias"), b"value").unwrap();
        assert!(std::fs::symlink_metadata(root.join("alias"))
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(
            std::fs::read_to_string(root.join("generated/config")).unwrap(),
            "value"
        );
        assert!(!root
            .join("alias")
            .metadata()
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[cfg(unix)]
    #[test]
    fn replace_keeps_setuid_bit() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("tool");
        std::fs::write(&file, b"old").unwrap();
        let mut perms = std::fs::metadata(&file).unwrap().permissions();
        perms.set_mode(0o4755);
        std::fs::set_permissions(&file, perms).unwrap();
        write_atomic(&file, b"new").unwrap();
        let got = std::fs::metadata(&file).unwrap().permissions().mode() & 0o7777;
        assert_eq!(got, 0o4755, "setuid bit cleared");
        assert_eq!(std::fs::read(&file).unwrap(), b"new");
    }
}
