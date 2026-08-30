//! Captured-command startup semantics bound into permission admission.

const PERMISSION_IDENTITY_PREFIX: &[u8] = b"@fx-terminal-env:";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    Clean,
    User,
}

/// Captured-command startup semantics bound into permission admission.
///
/// The shell path is part of explicit native profiles so execution cannot
/// silently switch environments after permission has been granted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Environment<'a> {
    Legacy,
    Clean(&'a [u8]),
    User(&'a [u8]),
    WorkspaceClean,
}

impl<'a> Environment<'a> {
    pub fn eql(self, other: Environment<'a>) -> bool {
        match (self, other) {
            (Environment::Legacy, Environment::Legacy) => true,
            (Environment::WorkspaceClean, Environment::WorkspaceClean) => true,
            (Environment::Clean(path), Environment::Clean(other_path)) => path == other_path,
            (Environment::User(path), Environment::User(other_path)) => path == other_path,
            _ => false,
        }
    }

    pub fn requires_shell_route(self) -> bool {
        matches!(self, Environment::Clean(_) | Environment::User(_))
    }
}

pub fn is_explicit_permission_command_identity(value: &[u8]) -> bool {
    value.starts_with(PERMISSION_IDENTITY_PREFIX)
}

/// Binds shell startup to a retained command grant. Legacy callers keep the
/// plain-command identity; terminal profile calls bind the selected shell.
pub fn permission_command_identity(environment: Environment, command: &[u8]) -> Vec<u8> {
    match environment {
        Environment::Legacy | Environment::WorkspaceClean => command.to_vec(),
        Environment::Clean(path) => format_permission_command_identity(b"clean", path, command),
        Environment::User(path) => format_permission_command_identity(b"user", path, command),
    }
}

fn format_permission_command_identity(
    profile: &[u8],
    shell_path: &[u8],
    command: &[u8],
) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();
    out.extend_from_slice(PERMISSION_IDENTITY_PREFIX);
    out.extend_from_slice(profile);
    out.push(b':');
    out.extend_from_slice(shell_path.len().to_string().as_bytes());
    out.push(b':');
    out.extend_from_slice(shell_path);
    out.extend_from_slice(b"::");
    out.extend_from_slice(command);
    out
}

/// Removes the opaque environment prefix only for configured permission-rule
/// matching and human-facing policy display. Session grants retain it.
pub fn command_from_permission_identity(identity: &[u8]) -> &[u8] {
    if !is_explicit_permission_command_identity(identity) {
        return identity;
    }
    let mut rest = &identity[PERMISSION_IDENTITY_PREFIX.len()..];
    let Some(profile_end) = rest.iter().position(|&b| b == b':') else {
        return identity;
    };
    rest = &rest[profile_end + 1..];
    let Some(length_end) = rest.iter().position(|&b| b == b':') else {
        return identity;
    };
    let Some(shell_len) = std::str::from_utf8(&rest[..length_end])
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
    else {
        return identity;
    };
    rest = &rest[length_end + 1..];
    if shell_len > rest.len() || rest.len() - shell_len < 2 {
        return identity;
    }
    if &rest[shell_len..shell_len + 2] != b"::" {
        return identity;
    }
    &rest[shell_len + 2..]
}

pub fn format_approval_command(environment: Environment, command: &[u8]) -> Vec<u8> {
    match environment {
        Environment::Legacy => {
            let mut out = b"# terminal.exec profile=omitted (legacy)\n".to_vec();
            out.extend_from_slice(command);
            out
        }
        Environment::WorkspaceClean => {
            let mut out = b"# terminal.exec profile=clean workspace=root-fixed\n".to_vec();
            out.extend_from_slice(command);
            out
        }
        Environment::Clean(path) => {
            let mut out = b"# terminal.exec profile=clean shell=".to_vec();
            out.extend_from_slice(path);
            out.push(b'\n');
            out.extend_from_slice(command);
            out
        }
        Environment::User(path) => {
            let mut out = b"# terminal.exec profile=user shell=".to_vec();
            out.extend_from_slice(path);
            out.push(b'\n');
            out.extend_from_slice(command);
            out
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Host {
    Native,
    WorkspaceClean,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_environment_equality_includes_explicit_shell_identity() {
        assert!(Environment::Legacy.eql(Environment::Legacy));
        assert!(Environment::WorkspaceClean.eql(Environment::WorkspaceClean));
        assert!(
            Environment::Clean(b"/bin/zsh").eql(Environment::Clean(b"/bin/zsh"))
        );
        assert!(
            !Environment::Clean(b"/bin/zsh").eql(Environment::Clean(b"/bin/bash"))
        );
        assert!(!Environment::Clean(b"/bin/zsh").eql(Environment::User(b"/bin/zsh")));
    }

    #[test]
    fn explicit_profiles_require_their_selected_shell_route() {
        assert!(!Environment::Legacy.requires_shell_route());
        assert!(Environment::Clean(b"/bin/zsh").requires_shell_route());
        assert!(Environment::User(b"/bin/zsh").requires_shell_route());
        assert!(!Environment::WorkspaceClean.requires_shell_route());
    }

    #[test]
    fn explicit_permission_identities_bind_profile_and_exact_shell() {
        let command: &[u8] = b"printf 'a::b'";
        let clean = permission_command_identity(Environment::Clean(b"/bin/zsh"), command);
        let user = permission_command_identity(Environment::User(b"/bin/zsh"), command);
        let other_shell =
            permission_command_identity(Environment::Clean(b"/opt/bin::zsh"), command);

        assert_ne!(clean, user);
        assert_ne!(clean, other_shell);
        assert_eq!(command_from_permission_identity(&clean), command);
        assert_eq!(command_from_permission_identity(&other_shell), command);

        let legacy = permission_command_identity(Environment::Legacy, command);
        assert_eq!(legacy, command);
        assert_eq!(command_from_permission_identity(&legacy), command);
    }
}
