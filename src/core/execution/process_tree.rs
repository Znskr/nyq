//! Command-process tracking across process groups, exec, and fork.
//!
//! The tracker guards both traversal and signaling with process identity
//! checks so a recycled PID can never be mistaken for a command descendant.
//! Linux uses `/proc/<pid>/stat` start ticks; macOS uses libproc unique ids.

#[cfg(target_os = "linux")]
const MAX_LINUX_CHILDREN_BUFFER: usize = 64 * 1024;
#[cfg(target_os = "macos")]
const MAX_DARWIN_PROCESS_FDS: usize = 10_240;
#[cfg(unix)]
const ESRCH: i32 = 3;
#[cfg(unix)]
const EPERM: i32 = 1;
#[cfg(target_os = "macos")]
const F_SETFD: i32 = 2;
#[cfg(target_os = "macos")]
const FD_CLOEXEC: i32 = 1;
#[cfg(target_os = "macos")]
const EINTR: i32 = 4;

#[cfg(unix)]
mod unix {
    unsafe extern "C" {
        pub fn getpgid(pid: i32) -> i32;
        pub fn kill(pid: i32, sig: i32) -> i32;
        pub fn pipe(fds: *mut i32) -> i32;
        pub fn fcntl(fd: i32, cmd: i32, arg: i32) -> i32;
        pub fn close(fd: i32) -> i32;
        pub fn getpid() -> i32;
    }
}

#[cfg(target_os = "macos")]
mod darwin {
    pub const PROC_PID_UNIQUE_IDENTIFIER_INFO: i32 = 17;
    pub const PROC_PID_LIST_FDS: i32 = 1;
    pub const PROC_PID_FD_PIPE_INFO: i32 = 6;
    pub const PROX_FD_TYPE_PIPE: u32 = 6;

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct ProcFdInfo {
        pub proc_fd: i32,
        pub proc_fdtype: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct ProcFileInfo {
        pub fi_openflags: u32,
        pub fi_status: u32,
        pub fi_offset: i64,
        pub fi_type: i32,
        pub fi_guardflags: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct VinfoStat {
        pub vst_dev: u32,
        pub vst_mode: u16,
        pub vst_nlink: u16,
        pub vst_ino: u64,
        pub vst_uid: u32,
        pub vst_gid: u32,
        pub vst_atime: i64,
        pub vst_atimensec: i64,
        pub vst_mtime: i64,
        pub vst_mtimensec: i64,
        pub vst_ctime: i64,
        pub vst_ctimensec: i64,
        pub vst_birthtime: i64,
        pub vst_birthtimensec: i64,
        pub vst_size: i64,
        pub vst_blocks: i64,
        pub vst_blksize: i32,
        pub vst_flags: u32,
        pub vst_gen: u32,
        pub vst_rdev: u32,
        pub vst_qspare: [i64; 2],
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct PipeInfo {
        pub pipe_stat: VinfoStat,
        pub pipe_handle: u64,
        pub pipe_peerhandle: u64,
        pub pipe_status: i32,
        pub rfu_1: i32,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct PipeFdInfo {
        pub pfi: ProcFileInfo,
        pub pipeinfo: PipeInfo,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct ProcUniqueIdentifierInfo {
        pub p_uuid: [u8; 16],
        pub p_uniqueid: u64,
        pub p_puniqueid: u64,
        pub p_idversion: i32,
        pub p_orig_ppidversion: i32,
        pub p_reserve2: u64,
        pub p_reserve3: u64,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct ProcBsdInfo {
        pub pbi_flags: u32,
        pub pbi_status: u32,
        pub pbi_xstatus: u32,
        pub pbi_pid: u32,
        pub pbi_ppid: u32,
        pub pbi_uid: u32,
        pub pbi_gid: u32,
        pub pbi_ruid: u32,
        pub pbi_rgid: u32,
        pub pbi_svuid: u32,
        pub pbi_svgid: u32,
        pub rfu_1: u32,
        pub pbi_comm: [u8; 16],
        pub pbi_name: [u8; 32],
        pub pbi_nfiles: u32,
        pub pbi_pgid: u32,
        pub pbi_pjobc: u32,
        pub e_tdev: u32,
        pub e_tpgid: u32,
        pub pbi_nice: i32,
        pub pbi_start_tvsec: u64,
        pub pbi_start_tvusec: u64,
    }

    unsafe extern "C" {
        pub fn proc_listchildpids(
            ppid: i32,
            buffer: *mut core::ffi::c_void,
            buffersize: i32,
        ) -> i32;
        pub fn proc_listallpids(buffer: *mut core::ffi::c_void, buffersize: i32) -> i32;
        pub fn proc_pidinfo(
            pid: i32,
            flavor: i32,
            arg: u64,
            buffer: *mut core::ffi::c_void,
            buffersize: i32,
        ) -> i32;
        pub fn proc_pidfdinfo(
            pid: i32,
            fd: i32,
            flavor: i32,
            buffer: *mut core::ffi::c_void,
            buffersize: i32,
        ) -> i32;
    }
}

#[derive(Debug)]
pub enum Error {
    ProcessTreeUnsupported,
    ProcessNotFound,
    ProcessIdentityUnavailable,
    FileControlFailed,
    ProcessTreeInspectionFailed,
    PermissionDenied,
    Io(std::io::Error),
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Identity {
    LinuxStartTicks(u64),
    MacosUniqueId(u64),
}

impl Identity {
    fn eql(self, other: Identity) -> bool {
        match (self, other) {
            (Identity::LinuxStartTicks(a), Identity::LinuxStartTicks(b)) => a == b,
            (Identity::MacosUniqueId(a), Identity::MacosUniqueId(b)) => a == b,
            _ => false,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct TrackedProcess {
    pid: i32,
    identity: Identity,
}

#[derive(Clone, Copy, Debug)]
struct ProcessSnapshot {
    identity: Identity,
    parent_pid: i32,
    parent_unique_id: Option<u64>,
    started_at_us: Option<u64>,
}

/// Number of delivered signals and whether any target could not be resolved.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DeliverySummary {
    pub delivered: usize,
    pub incomplete: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProcessGroupState {
    Found(i32),
    Vanished,
    Unavailable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Signal {
    Term,
    Kill,
    Int,
    Hup,
}

impl Signal {
    fn as_i32(self) -> i32 {
        match self {
            Signal::Term => 15,
            Signal::Kill => 9,
            Signal::Int => 2,
            Signal::Hup => 1,
        }
    }
}

#[cfg(target_os = "macos")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DarwinPipeIdentity {
    handle: u64,
    peer_handle: u64,
}

#[cfg(target_os = "macos")]
impl DarwinPipeIdentity {
    fn eql(self, other: DarwinPipeIdentity) -> bool {
        self.handle == other.handle && self.peer_handle == other.peer_handle
    }
}

/// Kernel-owned command membership that survives environment replacement,
/// exec, process-group changes, and forks. Only supported on macOS.
pub struct DarwinProcessWitness {
    supervisor_fd: Option<i32>,
    child_fd: Option<i32>,
    #[cfg(target_os = "macos")]
    identity: DarwinPipeIdentity,
}

impl DarwinProcessWitness {
    pub fn new() -> Result<DarwinProcessWitness> {
        #[cfg(not(target_os = "macos"))]
        {
            return Err(Error::ProcessTreeUnsupported);
        }
        #[cfg(target_os = "macos")]
        {
            let mut pipe = [0i32; 2];
            if unsafe { unix::pipe(pipe.as_mut_ptr()) } != 0 {
                return Err(Error::Io(std::io::Error::last_os_error()));
            }
            let outcome = (|| -> Result<DarwinProcessWitness> {
                set_close_on_exec(pipe[0])?;
                set_close_on_exec(pipe[1])?;
                let identity = capture_darwin_pipe_identity(unsafe { unix::getpid() }, pipe[1])?;
                Ok(DarwinProcessWitness {
                    supervisor_fd: Some(pipe[0]),
                    child_fd: Some(pipe[1]),
                    identity,
                })
            })();
            if outcome.is_err() {
                close_fd(pipe[0]);
                close_fd(pipe[1]);
            }
            outcome
        }
    }

    pub fn child_fd(&self) -> i32 {
        self.child_fd.expect("darwin witness child fd unavailable")
    }

    pub fn close_child_copy(&mut self) {
        if let Some(fd) = self.child_fd {
            close_fd(fd);
            self.child_fd = None;
        }
    }

    pub fn deinit(&mut self) {
        if let Some(fd) = self.child_fd {
            close_fd(fd);
        }
        if let Some(fd) = self.supervisor_fd {
            close_fd(fd);
        }
        self.child_fd = None;
        self.supervisor_fd = None;
    }
}

#[cfg(unix)]
fn close_fd(fd: i32) {
    let _ = unsafe { unix::close(fd) };
}

#[cfg(target_os = "macos")]
fn set_close_on_exec(fd: i32) -> Result<()> {
    loop {
        let r = unsafe { unix::fcntl(fd, F_SETFD, FD_CLOEXEC) };
        if r == 0 {
            return Ok(());
        }
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(EINTR) {
            continue;
        }
        return Err(Error::FileControlFailed);
    }
}

#[cfg(target_os = "macos")]
fn capture_darwin_pipe_identity(pid: i32, fd: i32) -> Result<DarwinPipeIdentity> {
    let mut info: darwin::PipeFdInfo = unsafe { std::mem::zeroed() };
    let read_len = unsafe {
        darwin::proc_pidfdinfo(
            pid,
            fd,
            darwin::PROC_PID_FD_PIPE_INFO,
            &mut info as *mut _ as *mut core::ffi::c_void,
            std::mem::size_of::<darwin::PipeFdInfo>() as i32,
        )
    };
    if read_len == 0 {
        return Err(Error::ProcessNotFound);
    }
    if read_len != std::mem::size_of::<darwin::PipeFdInfo>() as i32 {
        return Err(Error::ProcessIdentityUnavailable);
    }
    Ok(DarwinPipeIdentity {
        handle: info.pipeinfo.pipe_handle,
        peer_handle: info.pipeinfo.pipe_peerhandle,
    })
}

fn could_belong_by_start(
    root_started_at_us: Option<u64>,
    candidate_started_at_us: Option<u64>,
) -> bool {
    let root = match root_started_at_us {
        Some(r) => r,
        None => return true,
    };
    let candidate = match candidate_started_at_us {
        Some(c) => c,
        None => return false,
    };
    candidate >= root
}

fn darwin_start_time_us(seconds: u64, microseconds: u64) -> u64 {
    let scaled = match seconds.checked_mul(1_000_000) {
        Some(s) => s,
        None => return u64::MAX,
    };
    scaled.checked_add(microseconds).unwrap_or(u64::MAX)
}

fn should_traverse_parent(expected: Identity, actual: Identity) -> bool {
    expected.eql(actual)
}

fn snapshot_belongs_to_parent(snapshot: ProcessSnapshot, expected_parent_pid: i32) -> bool {
    snapshot.parent_pid == expected_parent_pid
}

fn should_signal_process(process_group: Option<i32>, preserved_group: Option<i32>) -> bool {
    let preserved = match preserved_group {
        Some(p) => p,
        None => return true,
    };
    let actual = match process_group {
        Some(a) => a,
        None => return false,
    };
    actual != preserved
}

fn identity_has_macos_unique_id(identity: Identity, unique_id: u64) -> bool {
    matches!(identity, Identity::MacosUniqueId(actual) if actual == unique_id)
}

fn parse_i32(text: &[u8]) -> Option<i32> {
    std::str::from_utf8(text).ok()?.parse().ok()
}

fn parse_u64(text: &[u8]) -> Option<u64> {
    std::str::from_utf8(text).ok()?.parse().ok()
}

trait SignalEffects {
    fn capture(pid: i32) -> Result<ProcessSnapshot>;
    fn process_group(pid: i32) -> ProcessGroupState;
    fn send(pid: i32, signal: Signal) -> Result<()>;
}

struct SystemSignalEffects;

impl SignalEffects for SystemSignalEffects {
    fn capture(pid: i32) -> Result<ProcessSnapshot> {
        capture_snapshot(pid)
    }

    fn process_group(pid: i32) -> ProcessGroupState {
        inspect_process_group(pid)
    }

    fn send(pid: i32, signal: Signal) -> Result<()> {
        send_signal(pid, signal)
    }
}

/// Tracks a command root and descendants across new sessions or process
/// groups. Identity checks guard both traversal and signaling against PID
/// reuse.
pub struct Tracker {
    root: Option<TrackedProcess>,
    processes: Vec<TrackedProcess>,
    #[cfg(target_os = "macos")]
    macos_child_buffer: Vec<i32>,
    #[cfg(target_os = "macos")]
    macos_pid_buffer: Vec<i32>,
    #[cfg(target_os = "macos")]
    macos_fd_buffer: Vec<darwin::ProcFdInfo>,
    #[cfg(target_os = "macos")]
    darwin_process_witness: Option<DarwinPipeIdentity>,
    #[cfg(target_os = "macos")]
    macos_root_started_at_us: Option<u64>,
}

impl Tracker {
    pub fn new() -> Tracker {
        #[cfg(target_os = "macos")]
        {
            let reported = unsafe { darwin::proc_listchildpids(0, std::ptr::null_mut(), 0) };
            let capacity = if reported > 0 {
                ((reported as usize) + 256).max(1024)
            } else {
                1024
            };
            let macos_child_buffer = vec![0i32; capacity];
            let process_count = unsafe { darwin::proc_listallpids(std::ptr::null_mut(), 0) };
            let process_capacity = if process_count > 0 {
                ((process_count as usize) + 256).max(1024)
            } else {
                1024
            };
            let macos_pid_buffer = vec![0i32; process_capacity];
            Tracker {
                root: None,
                processes: Vec::new(),
                macos_child_buffer,
                macos_pid_buffer,
                macos_fd_buffer: Vec::new(),
                darwin_process_witness: None,
                macos_root_started_at_us: None,
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            Tracker {
                root: None,
                processes: Vec::new(),
            }
        }
    }

    pub fn deinit(&mut self) {
        self.processes.clear();
        #[cfg(target_os = "macos")]
        {
            self.macos_child_buffer.clear();
            self.macos_pid_buffer.clear();
            self.macos_fd_buffer.clear();
        }
    }

    pub fn bind_process_witness(&mut self, witness: &DarwinProcessWitness) {
        #[cfg(target_os = "macos")]
        {
            self.darwin_process_witness = Some(witness.identity);
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = witness;
        }
    }

    pub fn refresh(&mut self, root_pid: i32) -> Result<()> {
        let root_snapshot: Option<ProcessSnapshot> = match capture_snapshot(root_pid) {
            Ok(snapshot) => Some(snapshot),
            Err(Error::ProcessNotFound) => None,
            Err(err) => return Err(err),
        };
        let mut traverse_root = false;
        if let Some(snapshot) = root_snapshot {
            if let Some(root) = self.root {
                traverse_root = root.pid == root_pid && root.identity.eql(snapshot.identity);
            } else {
                self.root = Some(TrackedProcess {
                    pid: root_pid,
                    identity: snapshot.identity,
                });
                #[cfg(target_os = "macos")]
                {
                    self.macos_root_started_at_us = snapshot.started_at_us;
                }
                traverse_root = true;
            }
        }
        if traverse_root {
            self.append_direct_children(self.root.expect("traversal root must exist"))?;
        }
        #[cfg(target_os = "macos")]
        {
            if root_snapshot.is_none() {
                self.refresh_lineage_processes()?;
            }
        }
        let mut parent_index = 0;
        while parent_index < self.processes.len() {
            let parent = self.processes[parent_index];
            parent_index += 1;
            let actual = match capture_snapshot(parent.pid) {
                Ok(snapshot) => snapshot,
                Err(_) => continue,
            };
            if !should_traverse_parent(parent.identity, actual.identity) {
                continue;
            }
            self.append_direct_children(parent)?;
        }
        Ok(())
    }

    pub fn refresh_additional_root(&mut self, root_pid: i32) -> Result<()> {
        let snapshot = match capture_snapshot(root_pid) {
            Ok(snapshot) => snapshot,
            Err(Error::ProcessNotFound) => return Ok(()),
            Err(err) => return Err(err),
        };
        self.append_direct_children(TrackedProcess {
            pid: root_pid,
            identity: snapshot.identity,
        })
    }

    pub fn refresh_lineage_processes(&mut self) -> Result<()> {
        #[cfg(target_os = "macos")]
        {
            let reported = unsafe { darwin::proc_listallpids(std::ptr::null_mut(), 0) };
            if reported <= 0 {
                return Ok(());
            }
            let required = (reported as usize) + 256;
            if required > self.macos_pid_buffer.len() {
                self.macos_pid_buffer.resize(required, 0);
            }
            let count = unsafe {
                darwin::proc_listallpids(
                    self.macos_pid_buffer.as_mut_ptr() as *mut core::ffi::c_void,
                    (self.macos_pid_buffer.len() * std::mem::size_of::<i32>()) as i32,
                )
            };
            if count <= 0 {
                return Ok(());
            }
            let process_count = (count as usize).min(self.macos_pid_buffer.len());
            let mut changed = true;
            while changed {
                changed = false;
                for i in 0..process_count {
                    let pid = self.macos_pid_buffer[i];
                    if pid <= 0 || pid == unsafe { unix::getpid() } {
                        continue;
                    }
                    if self.track_lineage_process(pid)? {
                        changed = true;
                    }
                }
            }
            Ok(())
        }
        #[cfg(not(target_os = "macos"))]
        {
            Ok(())
        }
    }

    pub fn signal_all(&self, signal: Signal) -> usize {
        self.signal_processes_checked(signal, None).delivered
    }

    pub fn signal_outside_process_group(&self, signal: Signal, preserved_group: i32) -> usize {
        self.signal_processes_checked(signal, Some(preserved_group))
            .delivered
    }

    pub fn signal_outside_process_group_checked(
        &self,
        signal: Signal,
        preserved_group: i32,
    ) -> DeliverySummary {
        self.signal_processes_checked(signal, Some(preserved_group))
    }

    pub fn any_alive(&self) -> bool {
        if let Some(root) = self.root {
            if let Ok(snapshot) = capture_snapshot(root.pid) {
                if root.identity.eql(snapshot.identity) {
                    return true;
                }
            }
        }
        for process in &self.processes {
            if let Ok(snapshot) = capture_snapshot(process.pid) {
                if process.identity.eql(snapshot.identity) {
                    return true;
                }
            }
        }
        false
    }

    fn signal_processes_checked(
        &self,
        signal: Signal,
        preserved_group: Option<i32>,
    ) -> DeliverySummary {
        self.signal_processes_with::<SystemSignalEffects>(signal, preserved_group)
    }

    fn signal_processes_with<E: SignalEffects>(
        &self,
        signal: Signal,
        preserved_group: Option<i32>,
    ) -> DeliverySummary {
        let mut summary = DeliverySummary::default();
        let mut index = self.processes.len();
        while index > 0 {
            index -= 1;
            self.signal_tracked_process_with::<E>(
                self.processes[index],
                signal,
                preserved_group,
                &mut summary,
            );
        }
        if let Some(root) = self.root {
            self.signal_tracked_process_with::<E>(root, signal, preserved_group, &mut summary);
        }
        summary
    }

    fn signal_tracked_process_with<E: SignalEffects>(
        &self,
        process: TrackedProcess,
        signal: Signal,
        preserved_group: Option<i32>,
        summary: &mut DeliverySummary,
    ) {
        let actual = match E::capture(process.pid) {
            Ok(snapshot) => snapshot,
            Err(Error::ProcessNotFound) => return,
            Err(_) => {
                summary.incomplete = true;
                return;
            }
        };
        if !process.identity.eql(actual.identity) {
            return;
        }
        let process_group = match E::process_group(process.pid) {
            ProcessGroupState::Found(value) => value,
            ProcessGroupState::Vanished => return,
            ProcessGroupState::Unavailable => {
                summary.incomplete = true;
                return;
            }
        };
        if !should_signal_process(Some(process_group), preserved_group) {
            return;
        }
        match E::send(process.pid, signal) {
            Ok(()) => {}
            Err(Error::ProcessNotFound) => return,
            Err(_) => {
                summary.incomplete = true;
                return;
            }
        };
        summary.delivered += 1;
    }

    fn append_direct_children(&mut self, parent: TrackedProcess) -> Result<()> {
        #[cfg(target_os = "linux")]
        {
            self.append_linux_children(parent)
        }
        #[cfg(target_os = "macos")]
        {
            self.append_macos_children(parent)
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let _ = parent;
            Err(Error::ProcessTreeUnsupported)
        }
    }

    #[cfg(target_os = "linux")]
    fn append_linux_children(&mut self, parent: TrackedProcess) -> Result<()> {
        if !self.parent_identity_matches(parent)? {
            return Ok(());
        }
        let task_path = format!("/proc/{}/task", parent.pid);
        if open_linux_proc_dir(&task_path)?.is_none() {
            return Ok(());
        }
        let entries = match std::fs::read_dir(&task_path) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(err) => return Err(Error::Io(err)),
        };
        let mut tids = Vec::new();
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => continue,
            };
            let tid = match entry.file_name().to_string_lossy().parse::<i32>() {
                Ok(tid) => tid,
                Err(_) => continue,
            };
            if tid > 0 {
                tids.push(tid);
            }
        }
        for tid in tids {
            self.append_linux_task_children(parent, tid)?;
        }
        Ok(())
    }

    #[cfg(target_os = "linux")]
    fn append_linux_task_children(&mut self, parent: TrackedProcess, tid: i32) -> Result<()> {
        let path = format!("/proc/{}/task/{}/children", parent.pid, tid);
        let mut file = match open_linux_proc_file(&path)? {
            Some(file) => file,
            None => return Ok(()),
        };
        let mut buffer = [0u8; MAX_LINUX_CHILDREN_BUFFER];
        let read_len = match read_linux_children_file(&mut file, &mut buffer) {
            Ok(len) => len,
            Err(Error::ProcessNotFound) => return Ok(()),
            Err(err) => return Err(err),
        };
        if !self.parent_identity_matches(parent)? {
            return Ok(());
        }
        for pid_text in buffer[..read_len]
            .split(|b| matches!(b, b' ' | b'\t' | b'\r' | b'\n'))
            .filter(|text| !text.is_empty())
        {
            if let Some(pid) = parse_i32(pid_text) {
                self.track_child(pid, parent.pid)?;
            }
        }
        Ok(())
    }

    #[cfg(target_os = "macos")]
    fn append_macos_children(&mut self, parent: TrackedProcess) -> Result<()> {
        if !self.parent_identity_matches(parent)? {
            return Ok(());
        }
        let count = unsafe {
            darwin::proc_listchildpids(
                parent.pid,
                self.macos_child_buffer.as_mut_ptr() as *mut core::ffi::c_void,
                (self.macos_child_buffer.len() * std::mem::size_of::<i32>()) as i32,
            )
        };
        if count <= 0 {
            return Ok(());
        }
        let child_count = (count as usize).min(self.macos_child_buffer.len());
        if !self.parent_identity_matches(parent)? {
            return Ok(());
        }
        for i in 0..child_count {
            let pid = self.macos_child_buffer[i];
            if pid > 0 {
                self.track_child(pid, parent.pid)?;
            }
        }
        Ok(())
    }

    fn parent_identity_matches(&self, parent: TrackedProcess) -> Result<bool> {
        let snapshot = match capture_snapshot(parent.pid) {
            Ok(snapshot) => snapshot,
            Err(Error::ProcessNotFound) => return Ok(false),
            Err(err) => return Err(err),
        };
        Ok(parent.identity.eql(snapshot.identity))
    }

    fn track_child(&mut self, pid: i32, expected_parent_pid: i32) -> Result<()> {
        let snapshot = match capture_snapshot(pid) {
            Ok(snapshot) => snapshot,
            Err(Error::ProcessNotFound) => return Ok(()),
            Err(err) => return Err(err),
        };
        if !snapshot_belongs_to_parent(snapshot, expected_parent_pid) {
            return Ok(());
        }
        if let Some(root) = self.root {
            if root.pid == pid && root.identity.eql(snapshot.identity) {
                return Ok(());
            }
        }
        for process in &mut self.processes {
            if process.pid != pid {
                continue;
            }
            process.identity = snapshot.identity;
            return Ok(());
        }
        self.processes.push(TrackedProcess {
            pid,
            identity: snapshot.identity,
        });
        Ok(())
    }

    #[cfg(target_os = "macos")]
    fn track_lineage_process(&mut self, pid: i32) -> Result<bool> {
        let snapshot = match capture_snapshot(pid) {
            Ok(snapshot) => snapshot,
            Err(_) => return Ok(false),
        };
        if !could_belong_by_start(self.macos_root_started_at_us, snapshot.started_at_us) {
            return Ok(false);
        }
        let parent_unique_id = match snapshot.parent_unique_id {
            Some(value) => value,
            None => return Ok(false),
        };
        if !self.contains_macos_unique_id(parent_unique_id) && !self.process_has_bound_witness(pid)? {
            return Ok(false);
        }
        if let Some(root) = self.root {
            if root.pid == pid && root.identity.eql(snapshot.identity) {
                return Ok(false);
            }
        }
        for process in &mut self.processes {
            if process.pid != pid {
                continue;
            }
            if process.identity.eql(snapshot.identity) {
                return Ok(false);
            }
            process.identity = snapshot.identity;
            return Ok(true);
        }
        self.processes.push(TrackedProcess {
            pid,
            identity: snapshot.identity,
        });
        Ok(true)
    }

    #[cfg(target_os = "macos")]
    fn process_has_bound_witness(&mut self, pid: i32) -> Result<bool> {
        let expected = match self.darwin_process_witness {
            Some(identity) => identity,
            None => return Ok(false),
        };
        if self.macos_fd_buffer.is_empty() {
            self.macos_fd_buffer =
                vec![darwin::ProcFdInfo { proc_fd: 0, proc_fdtype: 0 }; MAX_DARWIN_PROCESS_FDS];
        }
        let read_len = unsafe {
            darwin::proc_pidinfo(
                pid,
                darwin::PROC_PID_LIST_FDS,
                0,
                self.macos_fd_buffer.as_mut_ptr() as *mut core::ffi::c_void,
                (self.macos_fd_buffer.len() * std::mem::size_of::<darwin::ProcFdInfo>()) as i32,
            )
        };
        if read_len <= 0 {
            return Ok(false);
        }
        let fd_count = ((read_len as usize) / std::mem::size_of::<darwin::ProcFdInfo>())
            .min(self.macos_fd_buffer.len());
        for i in 0..fd_count {
            let fd = self.macos_fd_buffer[i];
            if fd.proc_fd < 0 || fd.proc_fdtype != darwin::PROX_FD_TYPE_PIPE {
                continue;
            }
            let actual = match capture_darwin_pipe_identity(pid, fd.proc_fd) {
                Ok(identity) => identity,
                Err(_) => continue,
            };
            if expected.eql(actual) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    #[cfg(target_os = "macos")]
    fn contains_macos_unique_id(&self, unique_id: u64) -> bool {
        if let Some(root) = self.root {
            if identity_has_macos_unique_id(root.identity, unique_id) {
                return true;
            }
        }
        self.processes
            .iter()
            .any(|process| identity_has_macos_unique_id(process.identity, unique_id))
    }
}

fn inspect_process_group(pid: i32) -> ProcessGroupState {
    #[cfg(unix)]
    {
        let process_group = unsafe { unix::getpgid(pid) };
        if process_group >= 0 {
            return ProcessGroupState::Found(process_group);
        }
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(ESRCH) {
            ProcessGroupState::Vanished
        } else {
            ProcessGroupState::Unavailable
        }
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        ProcessGroupState::Unavailable
    }
}

fn send_signal(pid: i32, signal: Signal) -> Result<()> {
    #[cfg(unix)]
    {
        let result = unsafe { unix::kill(pid, signal.as_i32()) };
        if result == 0 {
            return Ok(());
        }
        let err = std::io::Error::last_os_error();
        match err.raw_os_error() {
            Some(ESRCH) => Err(Error::ProcessNotFound),
            Some(EPERM) => Err(Error::PermissionDenied),
            _ => Err(Error::Io(err)),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (pid, signal);
        Err(Error::ProcessTreeUnsupported)
    }
}

fn capture_snapshot(pid: i32) -> Result<ProcessSnapshot> {
    #[cfg(target_os = "linux")]
    {
        capture_linux_snapshot(pid)
    }
    #[cfg(target_os = "macos")]
    {
        capture_macos_snapshot(pid)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = pid;
        Err(Error::ProcessTreeUnsupported)
    }
}

#[cfg(target_os = "linux")]
fn capture_linux_snapshot(pid: i32) -> Result<ProcessSnapshot> {
    let path = format!("/proc/{pid}/stat");
    let mut file = open_linux_proc_file(&path)?.ok_or(Error::ProcessNotFound)?;
    let mut buffer = [0u8; 4096];
    let read_len = read_linux_proc_file(&mut file, &mut buffer)?;
    let stat = &buffer[..read_len];
    let close_paren = stat
        .iter()
        .rposition(|&b| b == b')')
        .ok_or(Error::ProcessIdentityUnavailable)?;
    let mut field_number: usize = 3;
    let mut parent_pid: Option<i32> = None;
    for field in stat[close_paren + 1..]
        .split(|&b| b == b' ')
        .filter(|text| !text.is_empty())
    {
        if field_number == 4 {
            parent_pid = Some(parse_i32(field).ok_or(Error::ProcessIdentityUnavailable)?);
        }
        if field_number == 22 {
            let start_ticks = parse_u64(field).ok_or(Error::ProcessIdentityUnavailable)?;
            return Ok(ProcessSnapshot {
                identity: Identity::LinuxStartTicks(start_ticks),
                parent_pid: parent_pid.ok_or(Error::ProcessIdentityUnavailable)?,
                parent_unique_id: None,
                started_at_us: None,
            });
        }
        field_number += 1;
    }
    Err(Error::ProcessIdentityUnavailable)
}

#[cfg(target_os = "macos")]
fn capture_macos_snapshot(pid: i32) -> Result<ProcessSnapshot> {
    let mut unique: darwin::ProcUniqueIdentifierInfo = unsafe { std::mem::zeroed() };
    let unique_len = unsafe {
        darwin::proc_pidinfo(
            pid,
            darwin::PROC_PID_UNIQUE_IDENTIFIER_INFO,
            0,
            &mut unique as *mut _ as *mut core::ffi::c_void,
            std::mem::size_of::<darwin::ProcUniqueIdentifierInfo>() as i32,
        )
    };
    if unique_len == 0 {
        return Err(Error::ProcessNotFound);
    }
    if unique_len != std::mem::size_of::<darwin::ProcUniqueIdentifierInfo>() as i32 {
        return Err(Error::ProcessIdentityUnavailable);
    }
    let mut info: darwin::ProcBsdInfo = unsafe { std::mem::zeroed() };
    let read_len = unsafe {
        darwin::proc_pidinfo(
            pid,
            3,
            0,
            &mut info as *mut _ as *mut core::ffi::c_void,
            std::mem::size_of::<darwin::ProcBsdInfo>() as i32,
        )
    };
    if read_len == 0 {
        return Err(Error::ProcessNotFound);
    }
    if read_len != std::mem::size_of::<darwin::ProcBsdInfo>() as i32 {
        return Err(Error::ProcessIdentityUnavailable);
    }
    Ok(ProcessSnapshot {
        identity: Identity::MacosUniqueId(unique.p_uniqueid),
        parent_pid: info.pbi_ppid as i32,
        parent_unique_id: Some(unique.p_puniqueid),
        started_at_us: Some(darwin_start_time_us(
            info.pbi_start_tvsec,
            info.pbi_start_tvusec,
        )),
    })
}

#[cfg(target_os = "linux")]
fn open_linux_proc_dir(path: &str) -> Result<Option<std::fs::File>> {
    match std::fs::File::open(path) {
        Ok(dir) => Ok(Some(dir)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(Error::Io(err)),
    }
}

#[cfg(target_os = "linux")]
fn open_linux_proc_file(path: &str) -> Result<Option<std::fs::File>> {
    match std::fs::File::open(path) {
        Ok(file) => Ok(Some(file)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(Error::Io(err)),
    }
}

#[cfg(target_os = "linux")]
fn read_linux_children_file(file: &mut std::fs::File, buffer: &mut [u8]) -> Result<usize> {
    loop {
        match std::io::Read::read(file, buffer) {
            Ok(len) => return Ok(len),
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Err(Error::ProcessNotFound)
            }
            Err(_) => return Err(Error::ProcessTreeInspectionFailed),
        }
    }
}

#[cfg(target_os = "linux")]
fn read_linux_proc_file(file: &mut std::fs::File, buffer: &mut [u8]) -> Result<usize> {
    loop {
        match std::io::Read::read(file, buffer) {
            Ok(0) => return Err(Error::ProcessNotFound),
            Ok(len) => return Ok(len),
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Err(Error::ProcessNotFound)
            }
            Err(_) => return Err(Error::ProcessIdentityUnavailable),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_group_exclusion_preserves_the_captured_command_grace() {
        assert!(should_signal_process(Some(41), None));
        assert!(!should_signal_process(Some(41), Some(41)));
        assert!(should_signal_process(Some(42), Some(41)));
        assert!(!should_signal_process(None, Some(41)));
    }

    #[test]
    fn stale_process_identities_cannot_become_traversal_roots() {
        assert!(should_traverse_parent(
            Identity::LinuxStartTicks(41),
            Identity::LinuxStartTicks(41)
        ));
        assert!(!should_traverse_parent(
            Identity::LinuxStartTicks(41),
            Identity::LinuxStartTicks(42)
        ));
        assert!(!should_traverse_parent(
            Identity::LinuxStartTicks(41),
            Identity::MacosUniqueId(41)
        ));
    }

    #[test]
    fn child_admission_binds_the_observed_process_to_its_expected_parent() {
        let snapshot = ProcessSnapshot {
            identity: Identity::LinuxStartTicks(42),
            parent_pid: 17,
            parent_unique_id: None,
            started_at_us: None,
        };
        assert!(snapshot_belongs_to_parent(snapshot, 17));
        assert!(!snapshot_belongs_to_parent(snapshot, 18));
    }

    #[test]
    fn macos_lineage_identity_matches_only_the_same_unique_process() {
        assert!(identity_has_macos_unique_id(Identity::MacosUniqueId(42), 42));
        assert!(!identity_has_macos_unique_id(Identity::MacosUniqueId(42), 43));
        assert!(!identity_has_macos_unique_id(
            Identity::LinuxStartTicks(42),
            42
        ));
    }

    #[test]
    fn tracked_identity_distinguishes_process_instances() {
        let linux = Identity::LinuxStartTicks(42);
        assert!(linux.eql(Identity::LinuxStartTicks(42)));
        assert!(!linux.eql(Identity::LinuxStartTicks(43)));
        assert!(!linux.eql(Identity::MacosUniqueId(42)));
    }

    #[test]
    fn darwin_witness_scan_excludes_processes_older_than_command_root() {
        assert!(could_belong_by_start(None, None));
        assert!(!could_belong_by_start(Some(100), None));
        assert!(!could_belong_by_start(Some(100), Some(99)));
        assert!(could_belong_by_start(Some(100), Some(100)));
        assert!(could_belong_by_start(Some(100), Some(101)));
        assert_eq!(darwin_start_time_us(2, 3), 2_000_003);
        assert_eq!(darwin_start_time_us(u64::MAX, 1), u64::MAX);
    }

    struct FakeEffects;

    impl SignalEffects for FakeEffects {
        fn capture(pid: i32) -> Result<ProcessSnapshot> {
            match pid {
                11 => Err(Error::ProcessNotFound),
                12 => Err(Error::ProcessIdentityUnavailable),
                13 => Ok(ProcessSnapshot {
                    identity: Identity::LinuxStartTicks(113),
                    parent_pid: 1,
                    parent_unique_id: None,
                    started_at_us: None,
                }),
                _ => Ok(ProcessSnapshot {
                    identity: Identity::LinuxStartTicks(pid as u64),
                    parent_pid: 1,
                    parent_unique_id: None,
                    started_at_us: None,
                }),
            }
        }

        fn process_group(pid: i32) -> ProcessGroupState {
            match pid {
                14 => ProcessGroupState::Vanished,
                15 => ProcessGroupState::Unavailable,
                16 => ProcessGroupState::Found(41),
                _ => ProcessGroupState::Found(pid + 100),
            }
        }

        fn send(pid: i32, _: Signal) -> Result<()> {
            match pid {
                17 => Err(Error::PermissionDenied),
                18 => Err(Error::ProcessNotFound),
                _ => Ok(()),
            }
        }
    }

    #[test]
    fn checked_signal_delivery_distinguishes_vanished_stale_and_failed_targets() {
        let mut tracker = Tracker::new();
        for pid in 10..19 {
            tracker.processes.push(TrackedProcess {
                pid,
                identity: Identity::LinuxStartTicks(pid as u64),
            });
        }

        let summary = tracker.signal_processes_with::<FakeEffects>(Signal::Term, Some(41));
        assert_eq!(1, summary.delivered);
        assert!(summary.incomplete);
    }

    struct CompleteFakeEffects;

    impl SignalEffects for CompleteFakeEffects {
        fn capture(pid: i32) -> Result<ProcessSnapshot> {
            if pid == 21 {
                return Err(Error::ProcessNotFound);
            }
            Ok(ProcessSnapshot {
                identity: Identity::LinuxStartTicks(if pid == 22 { 122 } else { pid as u64 }),
                parent_pid: 1,
                parent_unique_id: None,
                started_at_us: None,
            })
        }

        fn process_group(pid: i32) -> ProcessGroupState {
            match pid {
                23 => ProcessGroupState::Vanished,
                _ => ProcessGroupState::Found(41),
            }
        }

        fn send(_: i32, _: Signal) -> Result<()> {
            Ok(())
        }
    }

    #[test]
    fn checked_signal_delivery_keeps_vanished_stale_and_excluded_targets_complete() {
        let mut tracker = Tracker::new();
        for pid in 21..25 {
            tracker.processes.push(TrackedProcess {
                pid,
                identity: Identity::LinuxStartTicks(pid as u64),
            });
        }

        let summary = tracker.signal_processes_with::<CompleteFakeEffects>(Signal::Term, Some(41));
        assert_eq!(0, summary.delivered);
        assert!(!summary.incomplete);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_proc_helpers_treat_missing_process_data_as_vanished() {
        assert!(matches!(
            open_linux_proc_dir("/proc/self/fx-process-tree-missing"),
            Ok(None)
        ));
        assert!(matches!(
            open_linux_proc_file("/proc/self/fx-process-tree-missing"),
            Ok(None)
        ));
    }
}
