use super::contracts::ShellSpec;
pub use crate::core::execution::command_environment::Profile;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResolveError {
    MissingLoginShell,
    RelativeShellPath,
    UnsupportedShell,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Environment {
    Legacy,
    Clean(Vec<u8>),
    User(Vec<u8>),
    WorkspaceClean,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ShellKind { Bash, Zsh }

fn shell_kind(path: &[u8]) -> Option<ShellKind> {
    let basename = path.rsplit(|&b| b == b'/').next().unwrap_or(path);
    if basename == b"bash" {
        return Some(ShellKind::Bash);
    }
    if basename == b"zsh" {
        return Some(ShellKind::Zsh);
    }
    None
}

fn fallback_login_shell() -> &'static [u8] {
    if cfg!(target_os = "macos") {
        b"/bin/zsh"
    } else {
        b"/bin/bash"
    }
}

fn is_absolute(path: &[u8]) -> bool {
    !path.is_empty() && path[0] == b'/'
}

fn supported_login_shell(configured: Option<&[u8]>) -> Result<&[u8], ResolveError> {
    let path = configured.ok_or(ResolveError::MissingLoginShell)?;
    if !is_absolute(path) {
        return Err(ResolveError::RelativeShellPath);
    }
    if shell_kind(path).is_some() {
        return Ok(path);
    }
    Ok(fallback_login_shell())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Invocation {
    pub path: Vec<u8>,
    values: [Vec<u8>; 6],
    len: usize,
}

impl Invocation {
    pub fn argv(&self) -> &[Vec<u8>] {
        &self.values[..self.len]
    }

    fn append(&mut self, value: &[u8]) {
        self.values[self.len] = value.to_vec();
        self.len += 1;
    }

    pub fn set_command(&mut self, command: &[u8]) {
        self.append(b"-c");
        self.append(command);
    }
}

pub fn resolve(
    configured_login_shell: Option<&[u8]>,
    shell: ShellSpec,
) -> Result<Invocation, ResolveError> {
    let (path, clean_start) = match shell {
        ShellSpec::UserLogin => (supported_login_shell(configured_login_shell)?.to_vec(), false),
        ShellSpec::Executable { path, clean_start } => (path, clean_start),
    };
    if !is_absolute(&path) {
        return Err(ResolveError::RelativeShellPath);
    }
    let kind = shell_kind(&path).ok_or(ResolveError::UnsupportedShell)?;

    let mut result = Invocation {
        path: path.clone(),
        values: std::array::from_fn(|_| Vec::new()),
        len: 0,
    };
    result.append(&path);
    match kind {
        ShellKind::Bash => {
            if clean_start {
                result.append(b"--noprofile");
                result.append(b"--norc");
            } else {
                result.append(b"--login");
            }
            result.append(b"-i");
        }
        ShellKind::Zsh => {
            if clean_start {
                result.append(b"-f");
            } else {
                result.append(b"-l");
            }
            result.append(b"-i");
        }
    }
    Ok(result)
}

pub fn configured_login_shell_into(buffer: &mut [u8]) -> Option<&[u8]> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        use std::ffi::CStr;
        use std::os::raw::c_char;
        use std::ptr;

        unsafe extern "C" {
            fn getuid() -> u32;
            fn getpwuid_r(
                uid: u32,
                pwd: *mut u8,
                buf: *mut u8,
                buflen: usize,
                result: *mut *mut u8,
            ) -> i32;
        }

        let mut entry = [0u8; 512];
        let mut scratch = [0u8; 4096];
        let mut found: *mut u8 = ptr::null_mut();
        // SAFETY: getpwuid_r writes into the provided buffers and stores the
        // resulting pointer in `found`; all pointers remain valid for the call.
        let status = unsafe {
            getpwuid_r(
                getuid(),
                entry.as_mut_ptr(),
                scratch.as_mut_ptr(),
                scratch.len(),
                &mut found,
            )
        };
        if status != 0 || found.is_null() {
            return None;
        }

        #[cfg(target_os = "linux")]
        const SHELL_OFFSET: usize = 40;
        #[cfg(target_os = "macos")]
        const SHELL_OFFSET: usize = 56;

        // SAFETY: `found` points at the struct passwd written into `entry`; the
        // pw_shell field sits at a fixed offset for the target ABI.
        let shell_ptr =
            unsafe { ptr::read_unaligned(entry[SHELL_OFFSET..].as_ptr() as *const *const c_char) };
        if shell_ptr.is_null() {
            return None;
        }
        // SAFETY: pw_shell is a NUL-terminated C string owned by libc.
        let shell = unsafe { CStr::from_ptr(shell_ptr) }.to_bytes();
        if shell.is_empty() || shell.len() > buffer.len() {
            return None;
        }
        buffer[..shell.len()].copy_from_slice(shell);
        return Some(&buffer[..shell.len()]);
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = buffer;
        None
    }
}

pub fn environment(
    configured_login_shell: Option<&[u8]>,
    profile: Option<Profile>,
) -> Result<Environment, ResolveError> {
    let selected = profile.unwrap_or(Profile::User);
    let path = supported_login_shell(configured_login_shell)?;
    let probe_spec = match selected {
        Profile::Clean => ShellSpec::Executable { path: path.to_vec(), clean_start: true },
        Profile::User => ShellSpec::Executable { path: path.to_vec(), clean_start: false },
    };
    let _ = resolve(None, probe_spec)?;
    Ok(match selected {
        Profile::Clean => Environment::Clean(path.to_vec()),
        Profile::User => Environment::User(path.to_vec()),
    })
}

pub fn profile_shell(
    configured_login_shell: Option<&[u8]>,
    profile: Profile,
) -> Result<ShellSpec, ResolveError> {
    match profile {
        Profile::Clean => {
            let path = supported_login_shell(configured_login_shell)?;
            let _ = resolve(
                None,
                ShellSpec::Executable { path: path.to_vec(), clean_start: true },
            )?;
            Ok(ShellSpec::Executable { path: path.to_vec(), clean_start: true })
        }
        Profile::User => {
            let Some(configured) = configured_login_shell else {
                return Ok(ShellSpec::UserLogin);
            };
            let path = supported_login_shell(Some(configured))?;
            if path == configured {
                Ok(ShellSpec::UserLogin)
            } else {
                Ok(ShellSpec::Executable { path: path.to_vec(), clean_start: false })
            }
        }
    }
}

const CAPTURED_ZSH_USER_PRELUDE: &[u8] = b"\\builtin trap - TERM; ";

pub fn captured_invocation(
    environment_value: Environment,
    command: &[u8],
) -> Result<Invocation, ResolveError> {
    match environment_value {
        Environment::Legacy | Environment::WorkspaceClean => Err(ResolveError::UnsupportedShell),
        Environment::Clean(path) => {
            let mut invocation = resolve(
                None,
                ShellSpec::Executable { path: path.clone(), clean_start: true },
            )?;
            remove_interactive_flag(&mut invocation);
            invocation.set_command(command);
            Ok(invocation)
        }
        Environment::User(path) => {
            let mut invocation = resolve(Some(&path), ShellSpec::UserLogin)?;
            let basename = path.rsplit(|&b| b == b'/').next().unwrap_or(&path);
            if basename == b"bash" {
                remove_interactive_flag(&mut invocation);
                invocation.append(b"-O");
                invocation.append(b"expand_aliases");
            }
            let effective_command = if shell_kind(&path) == Some(ShellKind::Zsh) {
                let mut concat = Vec::with_capacity(CAPTURED_ZSH_USER_PRELUDE.len() + command.len());
                concat.extend_from_slice(CAPTURED_ZSH_USER_PRELUDE);
                concat.extend_from_slice(command);
                concat
            } else {
                command.to_vec()
            };
            invocation.set_command(&effective_command);
            Ok(invocation)
        }
    }
}

pub fn format_invocation_command(invocation: &Invocation) -> Vec<u8> {
    let mut output = Vec::new();
    for (index, word) in invocation.argv().iter().enumerate() {
        if index != 0 {
            output.push(b' ');
        }
        append_shell_word(&mut output, word);
    }
    output
}

fn remove_interactive_flag(invocation: &mut Invocation) {
    assert!(invocation.len > 0);
    assert_eq!(invocation.values[invocation.len - 1], b"-i");
    invocation.len -= 1;
}

pub fn build_bootstrap(
    executable: &[u8],
    control_path: &[u8],
    nonce: &[u8],
    command_path: Option<&[u8]>,
) -> Vec<u8> {
    let mut output = Vec::new();
    output.extend_from_slice(b"set +x; ");
    if let Some(path) = command_path {
        output.extend_from_slice(b"fx_terminal_command=$(< ");
        append_shell_word(&mut output, path);
        output.extend_from_slice(b") || exit 125; ");
    }
    append_marker(&mut output, executable, control_path, nonce, b"shell-ready");
    if command_path.is_some() {
        output.extend_from_slice(b" || exit 125; ");
        append_marker(&mut output, executable, control_path, nonce, b"command-started");
        output.extend_from_slice(
            b" || exit 125; builtin eval -- \"$fx_terminal_command\"; fx_terminal_status=$?; exit \"$fx_terminal_status\"\n",
        );
    } else {
        output.extend_from_slice(b" || exit 125\n");
    }
    output
}

pub fn build_source_command(bootstrap_path: &[u8]) -> Vec<u8> {
    let mut output = Vec::new();
    output.extend_from_slice(b". ");
    append_shell_word(&mut output, bootstrap_path);
    output.push(b'\n');
    output
}

fn append_marker(
    output: &mut Vec<u8>,
    executable: &[u8],
    control_path: &[u8],
    nonce: &[u8],
    event: &[u8],
) {
    append_shell_word(output, executable);
    for word in [
        b"--fx-internal-terminal-control".as_slice(),
        control_path,
        nonce,
        event,
    ] {
        output.push(b' ');
        append_shell_word(output, word);
    }
}

fn append_shell_word(output: &mut Vec<u8>, word: &[u8]) {
    output.push(b'\'');
    for &byte in word {
        if byte == b'\'' {
            output.extend_from_slice(b"'\"'\"'");
        } else {
            output.push(byte);
        }
    }
    output.push(b'\'');
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check_argv(invocation: &Invocation, expected: &[&[u8]]) {
        let actual: Vec<&[u8]> = invocation.argv().iter().map(|v| v.as_slice()).collect();
        assert_eq!(actual, expected);
    }

    #[test]
    fn resolver_builds_bash_and_zsh_interactive_argv() {
        let bash = resolve(Some(b"/bin/bash"), ShellSpec::UserLogin).unwrap();
        check_argv(&bash, &[b"/bin/bash".as_slice(), b"--login".as_slice(), b"-i".as_slice()]);

        let zsh = resolve(
            None,
            ShellSpec::Executable { path: b"/bin/zsh".to_vec(), clean_start: false },
        )
        .unwrap();
        check_argv(&zsh, &[b"/bin/zsh".as_slice(), b"-l".as_slice(), b"-i".as_slice()]);
    }

    #[test]
    fn resolver_makes_clean_startup_explicit() {
        let bash = resolve(
            None,
            ShellSpec::Executable { path: b"/usr/local/bin/bash".to_vec(), clean_start: true },
        )
        .unwrap();
        check_argv(
            &bash,
            &[
                b"/usr/local/bin/bash".as_slice(),
                b"--noprofile".as_slice(),
                b"--norc".as_slice(),
                b"-i".as_slice(),
            ],
        );

        let zsh = resolve(
            None,
            ShellSpec::Executable { path: b"/bin/zsh".to_vec(), clean_start: true },
        )
        .unwrap();
        check_argv(&zsh, &[b"/bin/zsh".as_slice(), b"-f".as_slice(), b"-i".as_slice()]);
    }

    #[test]
    fn resolver_rejects_missing_relative_and_unsupported_shells() {
        assert_eq!(resolve(None, ShellSpec::UserLogin), Err(ResolveError::MissingLoginShell));
        assert_eq!(
            resolve(None, ShellSpec::Executable { path: b"zsh".to_vec(), clean_start: false }),
            Err(ResolveError::RelativeShellPath)
        );
        assert_eq!(
            resolve(None, ShellSpec::Executable { path: b"/bin/fish".to_vec(), clean_start: false }),
            Err(ResolveError::UnsupportedShell)
        );
    }

    #[test]
    fn login_shell_resolution_falls_back_without_accepting_explicit_unsupported_shells() {
        let fallback = resolve(Some(b"/opt/homebrew/bin/fish"), ShellSpec::UserLogin).unwrap();
        assert_eq!(fallback.path, fallback_login_shell());
        #[cfg(target_os = "macos")]
        check_argv(&fallback, &[b"/bin/zsh".as_slice(), b"-l".as_slice(), b"-i".as_slice()]);
        #[cfg(not(target_os = "macos"))]
        check_argv(&fallback, &[b"/bin/bash".as_slice(), b"--login".as_slice(), b"-i".as_slice()]);

        assert_eq!(
            resolve(None, ShellSpec::Executable { path: b"/opt/homebrew/bin/fish".to_vec(), clean_start: false }),
            Err(ResolveError::UnsupportedShell)
        );
    }

    #[test]
    fn captured_profiles_use_exact_non_pty_argv() {
        let bash_clean =
            captured_invocation(Environment::Clean(b"/bin/bash".to_vec()), b"printf clean").unwrap();
        check_argv(
            &bash_clean,
            &[
                b"/bin/bash".as_slice(),
                b"--noprofile".as_slice(),
                b"--norc".as_slice(),
                b"-c".as_slice(),
                b"printf clean".as_slice(),
            ],
        );
        let bash_user =
            captured_invocation(Environment::User(b"/bin/bash".to_vec()), b"printf user").unwrap();
        check_argv(
            &bash_user,
            &[
                b"/bin/bash".as_slice(),
                b"--login".as_slice(),
                b"-O".as_slice(),
                b"expand_aliases".as_slice(),
                b"-c".as_slice(),
                b"printf user".as_slice(),
            ],
        );
        let zsh_clean =
            captured_invocation(Environment::Clean(b"/bin/zsh".to_vec()), b"printf clean").unwrap();
        check_argv(
            &zsh_clean,
            &[b"/bin/zsh".as_slice(), b"-f".as_slice(), b"-c".as_slice(), b"printf clean".as_slice()],
        );
        let zsh_user =
            captured_invocation(Environment::User(b"/bin/zsh".to_vec()), b"printf user").unwrap();
        let expected: &[&[u8]] = &[
            b"/bin/zsh".as_slice(),
            b"-l".as_slice(),
            b"-i".as_slice(),
            b"-c".as_slice(),
            b"\\builtin trap - TERM; printf user".as_slice(),
        ];
        assert_eq!(zsh_user.argv().len(), expected.len());
        for (expected_word, actual_word) in expected.iter().zip(zsh_user.argv()) {
            assert_eq!(*expected_word, actual_word.as_slice());
        }
    }

    #[test]
    fn captured_invocation_provider_projection_shell_quotes_every_argv_word() {
        let invocation = captured_invocation(
            Environment::Clean(b"/bin/zsh".to_vec()),
            b"printf '%s' ok",
        )
        .unwrap();
        let command = format_invocation_command(&invocation);
        assert_eq!(
            command,
            b"'/bin/zsh' '-f' '-c' 'printf '\"'\"'%s'\"'\"' ok'"
        );
    }

    #[test]
    fn profile_normalization_defaults_captured_and_persistent_execution_to_user() {
        assert_eq!(
            environment(Some(b"/bin/bash"), None).unwrap(),
            Environment::User(b"/bin/bash".to_vec())
        );
        assert_eq!(
            environment(Some(b"/bin/zsh"), None).unwrap(),
            Environment::User(b"/bin/zsh".to_vec())
        );
        assert_eq!(
            environment(Some(b"/bin/zsh"), Some(Profile::Clean)).unwrap(),
            Environment::Clean(b"/bin/zsh".to_vec())
        );
        assert_eq!(
            environment(Some(b"/bin/zsh"), Some(Profile::User)).unwrap(),
            Environment::User(b"/bin/zsh".to_vec())
        );
        assert_eq!(profile_shell(Some(b"/bin/zsh"), Profile::User).unwrap(), ShellSpec::UserLogin);
        assert_eq!(
            profile_shell(Some(b"/bin/zsh"), Profile::Clean).unwrap(),
            ShellSpec::Executable { path: b"/bin/zsh".to_vec(), clean_start: true }
        );
    }

    #[test]
    fn unsupported_login_shell_profiles_fall_back_for_captured_and_persistent_execution() {
        let fallback = fallback_login_shell();
        let user_environment =
            environment(Some(b"/opt/homebrew/bin/fish"), Some(Profile::User)).unwrap();
        let clean_environment =
            environment(Some(b"/opt/homebrew/bin/fish"), Some(Profile::Clean)).unwrap();
        assert_eq!(user_environment, Environment::User(fallback.to_vec()));
        assert_eq!(clean_environment, Environment::Clean(fallback.to_vec()));

        let user_invocation = captured_invocation(user_environment, b"printf user").unwrap();
        let clean_invocation = captured_invocation(clean_environment, b"printf clean").unwrap();
        assert_eq!(user_invocation.path, fallback);
        assert_eq!(clean_invocation.path, fallback);

        assert_eq!(
            profile_shell(Some(b"/opt/homebrew/bin/fish"), Profile::User).unwrap(),
            ShellSpec::Executable { path: fallback.to_vec(), clean_start: false }
        );
        assert_eq!(
            profile_shell(Some(b"/opt/homebrew/bin/fish"), Profile::Clean).unwrap(),
            ShellSpec::Executable { path: fallback.to_vec(), clean_start: true }
        );
    }

    #[test]
    fn bootstrap_quotes_private_paths_and_separates_command_completion() {
        let commandless = build_bootstrap(b"/tmp/fx'bin", b"/tmp/control", b"nonce", None);
        assert_eq!(
            commandless,
            b"set +x; '/tmp/fx'\"'\"'bin' '--fx-internal-terminal-control' '/tmp/control' 'nonce' 'shell-ready' || exit 125\n"
        );

        let command = build_bootstrap(b"/tmp/fx", b"/tmp/control", b"nonce", Some(b"/tmp/command"));
        assert!(find_subslice(&command, b"'command-started'"));
        assert!(find_subslice(&command, b"builtin eval --"));
        assert!(find_subslice(&command, b"exit \"$fx_terminal_status\""));

        let source = build_source_command(b"/tmp/bootstrap'file");
        assert_eq!(source, b". '/tmp/bootstrap'\"'\"'file'\n");
    }

    fn find_subslice(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }
}
