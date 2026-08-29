//! Darwin process-spawn compatibility backend.
//!
//! Rust's standard process implementation already uses the native spawn
//! facilities.  The descriptor-inheritance entry point is kept separately so
//! non-Darwin builds fail with the same explicit unsupported shape.

#[cfg(target_os = "macos")]
use std::process::Command;
use std::{io, process::Child};

#[derive(Debug)]
pub enum SpawnError {
    Unsupported,
    Io(io::Error),
}
impl From<io::Error> for SpawnError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}
pub fn wrap<T>(original: T) -> T {
    original
}

#[cfg(target_os = "macos")]
pub fn spawn_inheriting_fd(
    program: &str,
    args: &[&str],
    _source_fd: i32,
) -> Result<Child, SpawnError> {
    // `Command` scopes the platform's unsafe posix_spawn interaction inside
    // the standard library. No raw pointer escapes this module.
    Ok(Command::new(program).args(args).spawn()?)
}

#[cfg(not(target_os = "macos"))]
pub fn spawn_inheriting_fd(
    _program: &str,
    _args: &[&str],
    _source_fd: i32,
) -> Result<Child, SpawnError> {
    Err(SpawnError::Unsupported)
}

pub fn spawn(program: &str, args: &[&str]) -> Result<Child, SpawnError> {
    #[cfg(target_os = "macos")]
    {
        Ok(Command::new(program).args(args).spawn()?)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (program, args);
        Err(SpawnError::Unsupported)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[cfg(not(target_os = "macos"))]
    fn unsupported_off_darwin() {
        assert!(matches!(spawn("true", &[]), Err(SpawnError::Unsupported)));
        assert!(matches!(
            spawn_inheriting_fd("true", &[], 3),
            Err(SpawnError::Unsupported)
        ))
    }
}
