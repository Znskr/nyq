//! Host target classification (ported 1:1 from zig-reference/src/core/hosts/target.zig).

/// True when the running host is a WASI target (Zig `builtin.os.tag == .wasi`).
#[allow(non_upper_case_globals)]
pub const is_wasm: bool = cfg!(target_os = "wasi");

/// Minimal Rust stand-in for `std.Target.Cpu.Arch` for the variants this host
/// boundary actually distinguishes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arch {
    Wasm32,
    Wasm64,
    X86_64,
    Aarch64,
    Other,
}

/// Minimal Rust stand-in for `std.Target.Os.Tag` for the variants this host
/// boundary actually distinguishes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OsTag {
    Macos,
    Linux,
    Windows,
    Wasi,
    Emscripten,
    Freebsd,
    Other,
}

impl OsTag {
    /// Zig `@tagName` values for the OS tags this port references.
    pub fn tag_name(self) -> &'static str {
        match self {
            OsTag::Macos => "macos",
            OsTag::Linux => "linux",
            OsTag::Windows => "windows",
            OsTag::Wasi => "wasi",
            OsTag::Emscripten => "emscripten",
            OsTag::Freebsd => "freebsd",
            OsTag::Other => "other",
        }
    }
}

/// Compile-time target architecture mapped onto `Arch`.
pub fn current_arch() -> Arch {
    if cfg!(target_arch = "wasm32") {
        Arch::Wasm32
    } else if cfg!(target_arch = "wasm64") {
        Arch::Wasm64
    } else if cfg!(target_arch = "x86_64") {
        Arch::X86_64
    } else if cfg!(target_arch = "aarch64") {
        Arch::Aarch64
    } else {
        Arch::Other
    }
}

/// Compile-time target OS mapped onto `OsTag`.
pub fn current_os() -> OsTag {
    if cfg!(target_os = "macos") {
        OsTag::Macos
    } else if cfg!(target_os = "linux") {
        OsTag::Linux
    } else if cfg!(target_os = "windows") {
        OsTag::Windows
    } else if cfg!(target_os = "wasi") {
        OsTag::Wasi
    } else if cfg!(target_os = "emscripten") {
        OsTag::Emscripten
    } else if cfg!(target_os = "freebsd") {
        OsTag::Freebsd
    } else {
        OsTag::Other
    }
}
