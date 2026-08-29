use super::debug_trace;
use std::{
    io,
    path::{Path, PathBuf},
};

const LINUX_SELF_EXE: &str = "/proc/self/exe";
fn test_product_exe() -> Option<PathBuf> {
    if cfg!(test) {
        std::env::var_os("FX_TEST_PRODUCT_EXE")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    } else {
        None
    }
}
pub fn path_for_reexec() -> io::Result<PathBuf> {
    if let Some(p) = test_product_exe() {
        return Ok(p);
    }
    production_path_for_reexec()
}
fn production_path_for_reexec() -> io::Result<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        Ok(PathBuf::from(LINUX_SELF_EXE))
    }
    #[cfg(not(target_os = "linux"))]
    {
        std::env::current_exe()
    }
}
pub fn path_for_peer_reexec() -> io::Result<PathBuf> {
    if let Some(p) = test_product_exe() {
        return Ok(p);
    }
    production_path_for_peer_reexec()
}
fn production_path_for_peer_reexec() -> io::Result<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        let pid = std::process::id();
        match std::env::current_exe() {
            Ok(p) if on_disk_path_is_executable(&p) => Ok(p),
            Ok(p) => {
                debug_trace::logf(
                    "self_exe",
                    format_args!(
                        "peer re-exec falling back to /proc/{pid}/exe on-disk={}",
                        p.display()
                    ),
                );
                Ok(PathBuf::from(format!("/proc/{pid}/exe")))
            }
            Err(e) => {
                debug_trace::logf(
                    "self_exe",
                    format_args!("peer re-exec falling back to /proc/{pid}/exe reason={e}"),
                );
                Ok(PathBuf::from(format!("/proc/{pid}/exe")))
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        std::env::current_exe()
    }
}
fn on_disk_path_is_executable(path: &Path) -> bool {
    path.is_absolute() && std::fs::File::open(path).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[cfg(target_os = "linux")]
    fn linux_paths() {
        assert_eq!(
            production_path_for_reexec().unwrap(),
            PathBuf::from(LINUX_SELF_EXE)
        );
        let peer = production_path_for_peer_reexec().unwrap();
        assert!(peer.is_absolute());
        assert!(on_disk_path_is_executable(&peer))
    }
    #[test]
    fn relative_rejected() {
        assert!(!on_disk_path_is_executable(Path::new("fx")))
    }
}
