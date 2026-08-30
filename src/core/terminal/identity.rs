/// Stable local profile identity used only inside private terminal authority.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn profile_user() -> Option<Vec<u8>> {
    unsafe extern "C" { fn getuid() -> u32; }
    // SAFETY: getuid takes no arguments and has no failure mode on supported Unix hosts.
    Some(format!("uid-{}", unsafe { getuid() }).into_bytes())
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn profile_user() -> Option<Vec<u8>> { None }

#[cfg(test)] mod tests { use super::*; #[test] fn profile_identity_is_available_exactly_on_supported_terminal_hosts() { #[cfg(any(target_os="linux",target_os="macos"))] assert!(profile_user().unwrap().starts_with(b"uid-")); #[cfg(not(any(target_os="linux",target_os="macos")))] assert!(profile_user().is_none()); } }
