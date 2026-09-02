//! Host boundary contracts (ported 1:1 from zig-reference/src/core/hosts/host.zig).
//!
//! Zig expresses each host hook as a struct holding an opaque `?*anyopaque`
//! context plus function pointers. The Rust port keeps the same shape but
//! models the context as an address-sized `usize` token (the convention used
//! by the rest of this crate) so callers and tests never need `unsafe` to
//! round-trip it. Zig's `std.mem.Allocator` parameter is dropped because Rust's
//! global allocator is infallible; owned results are plain `Vec<u8>`.

use super::{target, wasm};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminalSupport {
    Unsupported,
    Supported,
}

impl TerminalSupport {
    pub fn is_supported(self) -> bool {
        self == TerminalSupport::Supported
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capabilities {
    pub background_processes: bool,
    pub url_open: bool,
    pub native_url_open: bool,
    pub terminal: TerminalSupport,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UrlOpenError {
    /// API-only variant. Zig's `UrlOpenError` is `std.mem.Allocator.Error`;
    /// Rust's global allocator is infallible, so this is never produced.
    OutOfMemory,
}

#[derive(Clone, Copy)]
pub struct UrlOpener {
    pub context: usize,
    pub open_fn: fn(usize, &[u8]) -> Result<bool, UrlOpenError>,
}

impl UrlOpener {
    /// Borrows `url` for this call. The caller retains ownership. Returns
    /// `false` when the host cannot launch the URL so callers can keep a
    /// manual fallback available.
    pub fn open(self, url: &[u8]) -> Result<bool, UrlOpenError> {
        (self.open_fn)(self.context, url)
    }
}

#[allow(non_upper_case_globals)]
pub const unavailable_url_opener: UrlOpener = UrlOpener {
    context: 0,
    open_fn: unavailable_url_open,
};

fn unavailable_url_open(_: usize, _: &[u8]) -> Result<bool, UrlOpenError> {
    Ok(false)
}

#[derive(Clone, Copy)]
pub struct TerminalTitle {
    pub context: usize,
    pub set_fn: fn(usize, &[u8]),
    pub clear_fn: fn(usize),
}

impl TerminalTitle {
    /// Borrows `label` for this call. Callers resolve what the label says; the
    /// provider only renders it. Terminal write failures are intentionally
    /// non-fatal because the title is presentation metadata.
    pub fn set(self, label: &[u8]) {
        (self.set_fn)(self.context, label);
    }

    pub fn clear(self) {
        (self.clear_fn)(self.context);
    }
}

#[allow(non_upper_case_globals)]
pub const unavailable_terminal_title: TerminalTitle = TerminalTitle {
    context: 0,
    set_fn: ignore_terminal_title_set,
    clear_fn: ignore_terminal_title_clear,
};

fn ignore_terminal_title_set(_: usize, _: &[u8]) {}

fn ignore_terminal_title_clear(_: usize) {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SecretStoreLoadError {
    StoredKeyInsecure,
    StoredKeyUnreadable,
    /// API-only variant. Zig folds `std.mem.Allocator.Error` into this error
    /// set; Rust's global allocator is infallible, so this is never produced.
    OutOfMemory,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SecretStoreWriteError {
    StoredKeyWriteFailed,
    /// API-only variant. Zig folds `std.mem.Allocator.Error` into this error
    /// set; Rust's global allocator is infallible, so this is never produced.
    OutOfMemory,
}

#[derive(Clone, Copy)]
pub struct SecretStore {
    pub context: usize,
    pub backend_label: &'static str,
    pub is_disabled_fn: fn(usize) -> bool,
    pub load_fn: fn(usize) -> Result<Option<Vec<u8>>, SecretStoreLoadError>,
    pub store_fn: fn(usize, &[u8]) -> Result<(), SecretStoreWriteError>,
    pub store_interactive_fn: fn(usize) -> Result<bool, SecretStoreWriteError>,
}

impl SecretStore {
    pub fn is_disabled(self) -> bool {
        (self.is_disabled_fn)(self.context)
    }

    /// Returns an owned secret, or `None` when none is stored.
    pub fn load(self) -> Result<Option<Vec<u8>>, SecretStoreLoadError> {
        (self.load_fn)(self.context)
    }

    /// Borrows `value` for this call. The caller retains ownership.
    pub fn store(self, value: &[u8]) -> Result<(), SecretStoreWriteError> {
        (self.store_fn)(self.context, value)
    }

    /// Lets the host collect and store a secret without exposing its bytes to
    /// Core. Returns false when the host has no interactive secret prompt.
    pub fn store_interactive(self) -> Result<bool, SecretStoreWriteError> {
        (self.store_interactive_fn)(self.context)
    }
}

#[allow(non_upper_case_globals)]
pub const unavailable_secret_store: SecretStore = SecretStore {
    context: 0,
    backend_label: "configured credential store",
    is_disabled_fn: unavailable_secret_store_is_disabled,
    load_fn: unavailable_secret_store_load,
    store_fn: unavailable_secret_store_write,
    store_interactive_fn: unavailable_secret_store_interactive_write,
};

fn unavailable_secret_store_is_disabled(_: usize) -> bool {
    false
}

fn unavailable_secret_store_load(_: usize) -> Result<Option<Vec<u8>>, SecretStoreLoadError> {
    Ok(None)
}

fn unavailable_secret_store_write(_: usize, _: &[u8]) -> Result<(), SecretStoreWriteError> {
    Err(SecretStoreWriteError::StoredKeyWriteFailed)
}

fn unavailable_secret_store_interactive_write(_: usize) -> Result<bool, SecretStoreWriteError> {
    Ok(false)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClipboardError {
    CopyFailed,
}

#[allow(non_upper_case_globals)]
pub const unavailable_clipboard: Clipboard = Clipboard {
    context: 0,
    copy_fn: copy_unavailable,
    copy_file_fn: copy_file_unavailable,
};

fn copy_unavailable(_: usize, _: &[u8]) -> Result<bool, ClipboardError> {
    Ok(false)
}

fn copy_file_unavailable(_: usize, _: &[u8]) -> Result<bool, ClipboardError> {
    Ok(false)
}

#[derive(Clone, Copy)]
pub struct Clipboard {
    pub context: usize,
    pub copy_fn: fn(usize, &[u8]) -> Result<bool, ClipboardError>,
    pub copy_file_fn: fn(usize, &[u8]) -> Result<bool, ClipboardError>,
}

impl Clipboard {
    /// Borrows `text` for this call. The caller retains ownership. Returns
    /// `false` when the host has no clipboard implementation.
    pub fn copy(self, text: &[u8]) -> Result<bool, ClipboardError> {
        (self.copy_fn)(self.context, text)
    }

    /// Borrows `path` for this call. The caller retains ownership. Returns
    /// `false` when the host cannot publish file references to its clipboard.
    pub fn copy_file(self, path: &[u8]) -> Result<bool, ClipboardError> {
        (self.copy_file_fn)(self.context, path)
    }
}

pub fn current() -> Capabilities {
    capabilities_for_target(target::current_arch(), target::current_os())
}

pub fn capabilities_for_target(arch: target::Arch, os_tag: target::OsTag) -> Capabilities {
    if wasm::is_target(arch) {
        return Capabilities {
            background_processes: wasm::background_processes,
            url_open: false,
            native_url_open: false,
            terminal: terminal_support_for_os(os_tag),
        };
    }
    native_for_os(os_tag)
}

pub fn terminal_support_for_os(os_tag: target::OsTag) -> TerminalSupport {
    if matches!(os_tag, target::OsTag::Macos | target::OsTag::Linux) {
        TerminalSupport::Supported
    } else {
        TerminalSupport::Unsupported
    }
}

pub fn native_for_os(os_tag: target::OsTag) -> Capabilities {
    Capabilities {
        background_processes: !matches!(os_tag, target::OsTag::Windows | target::OsTag::Wasi),
        url_open: matches!(os_tag, target::OsTag::Macos | target::OsTag::Linux),
        native_url_open: matches!(os_tag, target::OsTag::Macos),
        terminal: terminal_support_for_os(os_tag),
    }
}

/// Returns an owned description of the current operating system.
pub fn operating_system_text() -> String {
    if wasm::is_target(target::current_arch()) {
        return wasm::operating_system_text(target::current_os());
    }
    if matches!(target::current_os(), target::OsTag::Windows | target::OsTag::Wasi) {
        return target::current_os().tag_name().to_string();
    }
    match uname_parts() {
        Some((sysname, release)) => {
            if release.is_empty() {
                sysname
            } else {
                format!("{sysname} {release}")
            }
        }
        None => std::env::consts::OS.to_string(),
    }
}

#[cfg(target_os = "linux")]
fn uname_parts() -> Option<(String, String)> {
    // `/proc/sys/kernel/ostype` + `osrelease` report the same values as the
    // `uname(2)` syscall that Zig's `std.posix.uname()` reads.
    let sysname = cstr_text(&std::fs::read("/proc/sys/kernel/ostype").ok()?);
    let release = cstr_text(&std::fs::read("/proc/sys/kernel/osrelease").ok()?);
    Some((sysname, release))
}

#[cfg(target_os = "macos")]
fn uname_parts() -> Option<(String, String)> {
    // Darwin `struct utsname` uses 256-byte fixed buffers.
    #[repr(C)]
    struct Utsname {
        sysname: [u8; 256],
        nodename: [u8; 256],
        release: [u8; 256],
        version: [u8; 256],
        machine: [u8; 256],
    }
    unsafe extern "C" {
        fn uname(buf: *mut Utsname) -> i32;
    }
    let mut uts: Utsname = unsafe { std::mem::zeroed() };
    if unsafe { uname(&mut uts) } != 0 {
        return None;
    }
    Some((cstr_text(&uts.sysname), cstr_text(&uts.release)))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn uname_parts() -> Option<(String, String)> {
    None
}

fn cstr_text(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    thread_local! {
        static CAPTURE: RefCell<Capture> = RefCell::new(Capture::default());
    }

    struct Capture {
        label: [u8; 64],
        label_len: usize,
        clear_count: usize,
    }

    impl Default for Capture {
        fn default() -> Self {
            Capture {
                label: [0u8; 64],
                label_len: 0,
                clear_count: 0,
            }
        }
    }

    impl Capture {
        fn set(&mut self, label: &[u8]) {
            self.label_len = label.len().min(self.label.len());
            self.label[..self.label_len].copy_from_slice(&label[..self.label_len]);
        }

        fn clear(&mut self) {
            self.clear_count += 1;
        }

        fn label_text(&self) -> &[u8] {
            &self.label[..self.label_len]
        }
    }

    fn capture_set(_: usize, label: &[u8]) {
        CAPTURE.with(|c| c.borrow_mut().set(label));
    }

    fn capture_clear(_: usize) {
        CAPTURE.with(|c| c.borrow_mut().clear());
    }

    #[test]
    fn terminal_title_forwards_borrowed_label_bytes_and_clear() {
        CAPTURE.with(|c| *c.borrow_mut() = Capture::default());
        let terminal_title = TerminalTitle {
            context: 0,
            set_fn: capture_set,
            clear_fn: capture_clear,
        };

        terminal_title.set(b"session name");
        terminal_title.clear();

        assert_eq!(
            b"session name".as_slice(),
            CAPTURE.with(|c| c.borrow().label_text().to_vec())
        );
        assert_eq!(1, CAPTURE.with(|c| c.borrow().clear_count));
    }

    #[test]
    fn unavailable_terminal_title_accepts_set_and_clear() {
        unavailable_terminal_title.set(b"provider/model");
        unavailable_terminal_title.clear();
    }

    #[test]
    fn native_host_capabilities_expose_process_and_url_support() {
        let macos = native_for_os(target::OsTag::Macos);
        assert!(macos.background_processes);
        assert!(macos.url_open);
        assert!(macos.native_url_open);
        assert_eq!(TerminalSupport::Supported, macos.terminal);

        let linux = native_for_os(target::OsTag::Linux);
        assert!(linux.background_processes);
        assert!(linux.url_open);
        assert!(!linux.native_url_open);
        assert_eq!(TerminalSupport::Supported, linux.terminal);

        let windows = native_for_os(target::OsTag::Windows);
        assert!(!windows.background_processes);
        assert!(!windows.url_open);
        assert!(!windows.native_url_open);
        assert_eq!(TerminalSupport::Unsupported, windows.terminal);

        let wasi = native_for_os(target::OsTag::Wasi);
        assert!(!wasi.background_processes);
        assert!(!wasi.url_open);
        assert!(!wasi.native_url_open);
        assert_eq!(TerminalSupport::Unsupported, wasi.terminal);

        assert_eq!(
            TerminalSupport::Unsupported,
            terminal_support_for_os(target::OsTag::Freebsd)
        );
    }

    #[test]
    fn host_boundary_routes_webassembly_targets_to_wasm_capabilities() {
        let emscripten = capabilities_for_target(target::Arch::Wasm32, target::OsTag::Emscripten);
        assert!(!emscripten.background_processes);
        assert!(!emscripten.url_open);
        assert!(!emscripten.native_url_open);
        assert_eq!(TerminalSupport::Unsupported, emscripten.terminal);

        let wasi = capabilities_for_target(target::Arch::Wasm64, target::OsTag::Wasi);
        assert!(!wasi.background_processes);
        assert!(!wasi.url_open);
        assert!(!wasi.native_url_open);
        assert_eq!(TerminalSupport::Unsupported, wasi.terminal);
    }

    #[test]
    fn unavailable_url_opener_keeps_the_manual_fallback_available() {
        assert!(!unavailable_url_opener.open(b"https://example.test").unwrap());
    }

    #[test]
    fn unavailable_secret_store_reports_absence_and_refuses_writes() {
        assert!(!unavailable_secret_store.is_disabled());
        assert!(unavailable_secret_store.load().unwrap().is_none());
        assert_eq!(
            Err(SecretStoreWriteError::StoredKeyWriteFailed),
            unavailable_secret_store.store(b"secret")
        );
        assert!(!unavailable_secret_store.store_interactive().unwrap());
    }

    #[test]
    fn unavailable_clipboard_rejects_text_and_file_references() {
        assert!(!unavailable_clipboard.copy(b"text").unwrap());
        assert!(!unavailable_clipboard.copy_file(b"/tmp/report.md").unwrap());
    }

    #[test]
    fn current_host_describes_its_operating_system() {
        let text = operating_system_text();
        assert!(text.len() > 0);
        if cfg!(any(target_os = "windows", target_os = "wasi")) {
            assert_eq!(target::current_os().tag_name(), text);
        }
    }
}
