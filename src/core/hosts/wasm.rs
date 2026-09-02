//! WebAssembly host capabilities (ported 1:1 from zig-reference/src/core/hosts/wasm.zig).

use super::target::{Arch, OsTag};

#[allow(non_upper_case_globals)]
pub const background_processes: bool = false;

pub fn is_target(arch: Arch) -> bool {
    matches!(arch, Arch::Wasm32 | Arch::Wasm64)
}

/// Returns an owned target description (`@tagName(os_tag)` in the Zig source).
pub fn operating_system_text(os_tag: OsTag) -> String {
    os_tag.tag_name().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wasm_targets_expose_no_native_process_capabilities() {
        assert!(is_target(Arch::Wasm32));
        assert!(is_target(Arch::Wasm64));
        assert!(!is_target(Arch::X86_64));
        assert!(!background_processes);
    }

    #[test]
    fn wasm_operating_system_text_does_not_require_native_discovery() {
        assert_eq!("wasi", operating_system_text(OsTag::Wasi));
        assert_eq!("emscripten", operating_system_text(OsTag::Emscripten));
    }
}
