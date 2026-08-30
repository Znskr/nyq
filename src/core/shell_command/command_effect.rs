//! Command-effect classification and direct read-only command planning.
//!
//! The planner recognizes a small, reviewed set of ordinary development
//! commands that can be executed directly with hardened argv. Everything else
//! returns a stable approval reason so the command stays on the normal review
//! path.

use super::command_lex;

const MAX_COMMAND_BYTES: usize = 8 * 1024;
const MAX_LS_OPERANDS: usize = 64;
const MAX_GIT_PATHSPECS: usize = 64;
const MAX_LINE_COUNT: usize = 10_000;
const MAX_GIT_LOG_COUNT: usize = 1_000;
#[allow(non_upper_case_globals)]
pub const max_direct_pipeline_stages: usize = 8;

/// Target operating system tag used by the planner. Only macOS and Linux are
/// supported for direct read-only execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetOs {
    Macos,
    Linux,
    Windows,
    Other,
}

/// Returns true only for a small parsed set of ordinary development commands
/// whose effects are expected and recoverable in auto mode. Unknown syntax,
/// dynamic shell expansion, publication, deletion, and wrappers remain on the
/// normal review path.
pub fn known_reversible_auto_command(command: &[u8], background: bool) -> bool {
    if background {
        return false;
    }
    let trimmed = trim_ascii(command, b" \t");
    if trimmed.is_empty()
        || trimmed.len() > MAX_COMMAND_BYTES
        || trimmed.contains(&b'\n')
        || trimmed.contains(&b'\r')
    {
        return false;
    }

    let argv = match command_lex::tokenize_argv(trimmed) {
        Ok(argv) => argv,
        Err(_) => return false,
    };
    if argv.tokens.is_empty() {
        return false;
    }

    let mut stage_start: usize = 0;
    let mut index: usize = 0;
    while index <= argv.tokens.len() {
        let at_end = index == argv.tokens.len();
        if !at_end && argv.tokens[index].value.as_slice() != b"&&" {
            if is_unsupported_auto_control_operator(&argv.tokens[index]) {
                return false;
            }
            index += 1;
            continue;
        }
        if !reversible_auto_stage(&argv.tokens[stage_start..index]) {
            return false;
        }
        stage_start = index + 1;
        index += 1;
    }
    true
}

fn is_unsupported_auto_operator(token: &command_lex::ArgvToken) -> bool {
    is_unsupported_auto_control_operator(token)
        || (!token.quoted && command_lex::redirection_kind(&token.value).is_some())
}

fn is_unsupported_auto_control_operator(token: &command_lex::ArgvToken) -> bool {
    if token.quoted {
        return false;
    }
    let value = token.value.as_slice();
    if value == b"&&" {
        return false;
    }
    eq(value, b"|")
        || eq(value, b"||")
        || eq(value, b"&")
        || eq(value, b";")
        || eq(value, b"(")
        || eq(value, b")")
        || eq(value, b"{")
        || eq(value, b"}")
}

fn reversible_auto_stage(raw_tokens: &[command_lex::ArgvToken]) -> bool {
    let mut tokens = raw_tokens;
    if tokens.len() >= 3
        && !tokens[tokens.len() - 3].quoted
        && !tokens[tokens.len() - 2].quoted
        && !tokens[tokens.len() - 1].quoted
        && tokens[tokens.len() - 3].value.as_slice() == b"2"
        && tokens[tokens.len() - 2].value.as_slice() == b">&"
        && tokens[tokens.len() - 1].value.as_slice() == b"1"
    {
        tokens = &tokens[..tokens.len() - 3];
    }
    if tokens.is_empty() {
        return false;
    }
    for token in tokens {
        if is_unsupported_auto_operator(token) || token_has_dynamic_shell_syntax(token.raw) {
            return false;
        }
    }

    let executable = tokens[0].value.as_slice();
    let words = &tokens[1..];
    if eq(executable, b"node") {
        return words.len() == 1 && is_version_flag(&words[0].value);
    }
    if eq(executable, b"which") {
        return !words.is_empty() && all_operands(words);
    }
    if eq(executable, b"git") {
        return reversible_git(words);
    }
    if eq(executable, b"npm") {
        return reversible_npm(words);
    }
    if eq(executable, b"bun") || eq(executable, b"pnpm") || eq(executable, b"yarn") {
        return reversible_package_runner(words);
    }
    if eq(executable, b"zig") {
        return !words.is_empty() && words[0].value.as_slice() == b"build";
    }
    false
}

fn token_has_dynamic_shell_syntax(raw: &[u8]) -> bool {
    raw.iter().any(|&byte| {
        matches!(
            byte,
            b'$' | b'`'
                | b'*'
                | b'?'
                | b'['
                | b'~'
                | b'\n'
                | b'\r'
                | b';'
                | b'('
                | b')'
                | b'{'
                | b'}'
        )
    })
}

fn is_version_flag(value: &[u8]) -> bool {
    eq(value, b"-v") || eq(value, b"--version")
}

fn all_operands(tokens: &[command_lex::ArgvToken]) -> bool {
    for token in tokens {
        if token.value.is_empty() || token.value[0] == b'-' {
            return false;
        }
    }
    true
}

fn reversible_git(words: &[command_lex::ArgvToken]) -> bool {
    if words.is_empty() {
        return false;
    }
    let subcommand = words[0].value.as_slice();
    if eq(subcommand, b"status") {
        return true;
    }
    if eq(subcommand, b"remote") {
        return words.len() == 2 && words[1].value.as_slice() == b"-v";
    }
    if eq(subcommand, b"worktree") {
        return words.len() >= 2 && words[1].value.as_slice() == b"list";
    }
    if eq(subcommand, b"fetch") {
        for word in &words[1..] {
            let value = word.value.as_slice();
            if eq(value, b"--prune") || eq(value, b"-p") || eq(value, b"--prune-tags") {
                return false;
            }
        }
        return true;
    }
    false
}

fn reversible_npm(words: &[command_lex::ArgvToken]) -> bool {
    if words.len() == 1 && is_version_flag(&words[0].value) {
        return true;
    }
    if words.is_empty() {
        return false;
    }
    let subcommand = words[0].value.as_slice();
    if eq(subcommand, b"install")
        || eq(subcommand, b"i")
        || eq(subcommand, b"ci")
        || eq(subcommand, b"test")
    {
        return !has_global_package_flag(&words[1..]);
    }
    eq(subcommand, b"run") && words.len() >= 2
}

fn reversible_package_runner(words: &[command_lex::ArgvToken]) -> bool {
    if words.len() == 1 && is_version_flag(&words[0].value) {
        return true;
    }
    if words.is_empty() {
        return false;
    }
    let subcommand = words[0].value.as_slice();
    if eq(subcommand, b"install") || eq(subcommand, b"test") {
        return !has_global_package_flag(&words[1..]);
    }
    eq(subcommand, b"run") && words.len() >= 2
}

fn has_global_package_flag(words: &[command_lex::ArgvToken]) -> bool {
    for word in words {
        let value = word.value.as_slice();
        if eq(value, b"-g")
            || eq(value, b"--global")
            || eq(value, b"--location")
            || eq(value, b"--location=global")
            || eq(value, b"--prefix")
            || value.starts_with(b"--prefix=")
        {
            return true;
        }
    }
    false
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalReason {
    FilesystemWrite,
    NetworkAccess,
    ProcessOrSystem,
    BackgroundProcess,
    DynamicShell,
    UnsupportedShell,
    UnsupportedPlatform,
    UnsupportedInputRedirect,
    UnsupportedArgument,
    CommandOwnedInput,
    UnknownCommand,
    PlanningFailure,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvironmentProfile {
    BasicReadOnly,
    GitReadOnly,
}

pub struct DirectStage {
    pub executable: &'static [u8],
    pub argv: Vec<Vec<u8>>,
    pub environment_profile: EnvironmentProfile,
}

pub struct DirectReadOnlyPlan {
    pub command: Vec<u8>,
    pub cwd: Vec<u8>,
    pub stages: Vec<DirectStage>,
}

pub enum Admission {
    DirectReadOnly(DirectReadOnlyPlan),
    ApprovalRequired(ApprovalReason),
}

pub fn plan(
    command: &[u8],
    resolved_cwd: &[u8],
    background: bool,
    target_os: TargetOs,
) -> Admission {
    if background {
        return Admission::ApprovalRequired(ApprovalReason::BackgroundProcess);
    }
    if target_os != TargetOs::Macos && target_os != TargetOs::Linux {
        return Admission::ApprovalRequired(ApprovalReason::UnsupportedPlatform);
    }

    let trimmed = trim_ascii(command, b" \t\n");
    if trimmed.is_empty() {
        return Admission::ApprovalRequired(ApprovalReason::UnknownCommand);
    }
    if trimmed.len() > MAX_COMMAND_BYTES {
        return Admission::ApprovalRequired(ApprovalReason::UnsupportedArgument);
    }
    if let Some(reason) = validate_shell_shape(trimmed) {
        return Admission::ApprovalRequired(reason);
    }

    let segments = match command_lex::pipe_segments(trimmed) {
        Ok(segments) => segments,
        Err(_) => return Admission::ApprovalRequired(ApprovalReason::UnsupportedShell),
    };
    if segments.is_empty() {
        return Admission::ApprovalRequired(ApprovalReason::UnknownCommand);
    }
    if segments.len() > max_direct_pipeline_stages {
        return Admission::ApprovalRequired(ApprovalReason::ProcessOrSystem);
    }

    let mut planned: Vec<TemporaryStage> = Vec::new();
    for segment in &segments {
        if segment.text.is_empty() {
            return Admission::ApprovalRequired(ApprovalReason::UnsupportedShell);
        }

        let argv = match command_lex::tokenize_argv(segment.text) {
            Ok(argv) => argv,
            Err(_) => return Admission::ApprovalRequired(ApprovalReason::UnsupportedShell),
        };
        match parse_stage(&argv.tokens, target_os) {
            StageAdmission::ApprovalRequired(reason) => {
                return Admission::ApprovalRequired(reason)
            }
            StageAdmission::Direct(stage) => planned.push(stage),
        }
    }

    own_plan(command, resolved_cwd, planned)
}

struct TemporaryStage {
    executable: &'static [u8],
    argv: Vec<Vec<u8>>,
    environment_profile: EnvironmentProfile,
}

enum StageAdmission {
    Direct(TemporaryStage),
    ApprovalRequired(ApprovalReason),
}

fn validate_shell_shape(command: &[u8]) -> Option<ApprovalReason> {
    if std::str::from_utf8(command).is_err() || command.contains(&0) {
        return Some(ApprovalReason::UnsupportedShell);
    }
    if command.contains(&b'\r') {
        return Some(ApprovalReason::UnsupportedShell);
    }

    let mut in_single = false;
    let mut in_double = false;
    let mut i: usize = 0;
    while i < command.len() {
        let ch = command[i];

        if in_single {
            if ch == b'\'' {
                in_single = false;
            }
            i += 1;
            continue;
        }
        if in_double {
            if ch == b'"' {
                in_double = false;
                i += 1;
                continue;
            }
            if ch == b'$' || ch == b'`' {
                return Some(ApprovalReason::DynamicShell);
            }
            if ch == b'\\' || ch == b'\n' {
                return Some(ApprovalReason::UnsupportedShell);
            }
            i += 1;
            continue;
        }

        match ch {
            b'\'' => in_single = true,
            b'"' => in_double = true,
            b'$' | b'`' | b'*' | b'?' | b'[' | b'~' => {
                return Some(ApprovalReason::DynamicShell)
            }
            b'\\' | b'#' | b';' | b'\n' | b'(' | b')' | b'{' | b'}' => {
                return Some(ApprovalReason::UnsupportedShell)
            }
            b'&' => {
                if i + 1 < command.len() && command[i + 1] == b'>' {
                    return Some(ApprovalReason::FilesystemWrite);
                }
                return Some(ApprovalReason::UnsupportedShell);
            }
            b'|' => {
                if i + 1 < command.len() && command[i + 1] == b'|' {
                    return Some(ApprovalReason::UnsupportedShell);
                }
            }
            b'>' => return Some(ApprovalReason::FilesystemWrite),
            b'<' => {
                if i + 1 < command.len() {
                    match command[i + 1] {
                        b'(' => return Some(ApprovalReason::DynamicShell),
                        b'>' => return Some(ApprovalReason::FilesystemWrite),
                        b'<' | b'&' => return Some(ApprovalReason::UnsupportedShell),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
        i += 1;
    }

    if in_single || in_double {
        Some(ApprovalReason::UnsupportedShell)
    } else {
        None
    }
}

fn parse_stage(tokens: &[command_lex::ArgvToken], target_os: TargetOs) -> StageAdmission {
    let mut words: Vec<Vec<u8>> = Vec::new();
    let mut index: usize = 0;
    while index < tokens.len() {
        let token = &tokens[index];
        if !token.quoted && token.value.as_slice() == b"<" {
            return StageAdmission::ApprovalRequired(ApprovalReason::UnsupportedInputRedirect);
        }
        if is_operator_token(token) {
            return StageAdmission::ApprovalRequired(ApprovalReason::UnsupportedShell);
        }
        words.push(token.value.clone());
        index += 1;
    }

    if words.is_empty() {
        return StageAdmission::ApprovalRequired(ApprovalReason::UnknownCommand);
    }
    if words[0].contains(&b'=') {
        return StageAdmission::ApprovalRequired(ApprovalReason::DynamicShell);
    }

    match plan_family(&words, target_os) {
        StageAdmission::ApprovalRequired(reason) => {
            StageAdmission::ApprovalRequired(reason)
        }
        StageAdmission::Direct(stage) => StageAdmission::Direct(TemporaryStage {
            executable: stage.executable,
            argv: stage.argv,
            environment_profile: stage.environment_profile,
        }),
    }
}

fn is_operator_token(token: &command_lex::ArgvToken) -> bool {
    if token.quoted {
        return false;
    }
    let value = token.value.as_slice();
    eq(value, b"|")
        || eq(value, b"||")
        || eq(value, b"&")
        || eq(value, b"&&")
        || eq(value, b";")
        || command_lex::redirection_kind(value).is_some()
}

fn plan_family(words: &[Vec<u8>], target_os: TargetOs) -> StageAdmission {
    let command = words[0].as_slice();
    if is_filesystem_mutation(command) {
        return StageAdmission::ApprovalRequired(ApprovalReason::FilesystemWrite);
    }
    if is_network_command(command) {
        return StageAdmission::ApprovalRequired(ApprovalReason::NetworkAccess);
    }
    if is_process_or_system_command(command) {
        return StageAdmission::ApprovalRequired(ApprovalReason::ProcessOrSystem);
    }
    if eq(command, b"printf") {
        return plan_printf(words, target_os);
    }
    if eq(command, b"pwd") {
        return plan_pwd(words);
    }
    if eq(command, b"ls") {
        return plan_ls(words, target_os);
    }
    if eq(command, b"wc") {
        return plan_wc(words);
    }
    if eq(command, b"cat") {
        return plan_cat(words);
    }
    if eq(command, b"head") {
        return plan_head_or_tail(words, b"/usr/bin/head");
    }
    if eq(command, b"tail") {
        return plan_head_or_tail(words, b"/usr/bin/tail");
    }
    if eq(command, b"grep") {
        return plan_grep(words);
    }
    if eq(command, b"git") {
        return plan_git(words);
    }
    StageAdmission::ApprovalRequired(ApprovalReason::UnknownCommand)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PrintfFormatLanguage {
    PortableLiteralNewlinePercentString,
}

struct PrintfPolicy {
    executable: &'static [u8],
    #[allow(dead_code)]
    format_language: PrintfFormatLanguage,
}

fn plan_printf(words: &[Vec<u8>], target_os: TargetOs) -> StageAdmission {
    let policy = printf_policy(target_os);
    if words.len() < 2 {
        return StageAdmission::ApprovalRequired(ApprovalReason::UnsupportedArgument);
    }
    let format = words[1].as_slice();
    let Some(conversions) = printf_string_conversion_count(format) else {
        return StageAdmission::ApprovalRequired(ApprovalReason::UnsupportedArgument);
    };
    if words.len() - 2 != conversions {
        return StageAdmission::ApprovalRequired(ApprovalReason::UnsupportedArgument);
    }

    let mut argv: Vec<Vec<u8>> = Vec::new();
    argv.push(policy.executable.to_vec());
    argv.extend_from_slice(&words[1..]);
    StageAdmission::Direct(TemporaryStage {
        executable: policy.executable,
        argv,
        environment_profile: EnvironmentProfile::BasicReadOnly,
    })
}

fn printf_policy(target_os: TargetOs) -> PrintfPolicy {
    match target_os {
        TargetOs::Macos | TargetOs::Linux => PrintfPolicy {
            executable: b"/usr/bin/printf",
            format_language: PrintfFormatLanguage::PortableLiteralNewlinePercentString,
        },
        _ => unreachable!(),
    }
}

fn printf_string_conversion_count(format: &[u8]) -> Option<usize> {
    if format.is_empty() || format[0] == b'-' {
        return None;
    }

    let mut count: usize = 0;
    let mut index: usize = 0;
    while index < format.len() {
        let ch = format[index];
        if ch < 0x20 || ch > 0x7e {
            return None;
        }
        if ch == b'\\' {
            if index + 1 >= format.len() || format[index + 1] != b'n' {
                return None;
            }
            index += 2;
            continue;
        }
        if ch == b'%' {
            if index + 1 >= format.len() {
                return None;
            }
            match format[index + 1] {
                b'%' => {}
                b's' => count += 1,
                _ => return None,
            }
            index += 2;
            continue;
        }
        index += 1;
    }
    Some(count)
}

fn plan_pwd(words: &[Vec<u8>]) -> StageAdmission {
    if words.len() > 2 || (words.len() == 2 && words[1].as_slice() != b"-P") {
        return StageAdmission::ApprovalRequired(ApprovalReason::UnsupportedArgument);
    }
    StageAdmission::Direct(TemporaryStage {
        executable: b"/bin/pwd",
        argv: vec![b"/bin/pwd".to_vec(), b"-P".to_vec()],
        environment_profile: EnvironmentProfile::BasicReadOnly,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LsSymlinkSemantics {
    PlatformDefault,
}

struct LsPolicy {
    executable: &'static [u8],
    forced_argv: &'static [&'static [u8]],
    #[allow(dead_code)]
    symlink_semantics: LsSymlinkSemantics,
}

fn plan_ls(words: &[Vec<u8>], target_os: TargetOs) -> StageAdmission {
    let policy = ls_policy(target_os);
    let mut argv: Vec<Vec<u8>> = Vec::new();
    argv.push(policy.executable.to_vec());
    for forced in policy.forced_argv {
        argv.push(forced.to_vec());
    }

    let mut operands: Vec<Vec<u8>> = Vec::new();
    let mut parsing_options = true;
    let mut explicit_separator = false;
    for word in &words[1..] {
        if parsing_options && word.as_slice() == b"--" {
            parsing_options = false;
            explicit_separator = true;
            continue;
        }
        if parsing_options && word.len() > 1 && word[0] == b'-' {
            for &flag in &word[1..] {
                let canonical: &[u8] = match flag {
                    b'1' => b"-1",
                    b'a' => b"-a",
                    b'A' => b"-A",
                    b'd' => b"-d",
                    b'F' => b"-F",
                    b'l' | b'n' => b"-n",
                    b'p' => b"-p",
                    _ => {
                        return StageAdmission::ApprovalRequired(
                            ApprovalReason::UnsupportedArgument,
                        )
                    }
                };
                argv.push(canonical.to_vec());
            }
            continue;
        }
        parsing_options = false;
        if !explicit_separator && word.len() > 1 && word[0] == b'-' {
            return StageAdmission::ApprovalRequired(ApprovalReason::UnsupportedArgument);
        }
        operands.push(word.clone());
    }

    if operands.len() > MAX_LS_OPERANDS {
        return StageAdmission::ApprovalRequired(ApprovalReason::UnsupportedArgument);
    }
    argv.push(b"--".to_vec());
    argv.extend(operands);
    StageAdmission::Direct(TemporaryStage {
        executable: policy.executable,
        argv,
        environment_profile: EnvironmentProfile::BasicReadOnly,
    })
}

fn ls_policy(target_os: TargetOs) -> LsPolicy {
    match target_os {
        TargetOs::Macos | TargetOs::Linux => LsPolicy {
            executable: b"/bin/ls",
            forced_argv: &[b"-q"],
            symlink_semantics: LsSymlinkSemantics::PlatformDefault,
        },
        _ => unreachable!(),
    }
}

fn plan_wc(words: &[Vec<u8>]) -> StageAdmission {
    let mut argv: Vec<Vec<u8>> = vec![b"/usr/bin/wc".to_vec()];
    for word in &words[1..] {
        if word.len() < 2 || word[0] != b'-' {
            return StageAdmission::ApprovalRequired(ApprovalReason::UnsupportedArgument);
        }
        for &flag in &word[1..] {
            let canonical: &[u8] = match flag {
                b'L' => b"-L",
                b'c' => b"-c",
                b'l' => b"-l",
                b'm' => b"-m",
                b'w' => b"-w",
                _ => {
                    return StageAdmission::ApprovalRequired(ApprovalReason::UnsupportedArgument)
                }
            };
            argv.push(canonical.to_vec());
        }
    }
    StageAdmission::Direct(TemporaryStage {
        executable: b"/usr/bin/wc",
        argv,
        environment_profile: EnvironmentProfile::BasicReadOnly,
    })
}

fn plan_cat(words: &[Vec<u8>]) -> StageAdmission {
    if words.len() != 1 {
        return StageAdmission::ApprovalRequired(ApprovalReason::UnsupportedArgument);
    }
    StageAdmission::Direct(TemporaryStage {
        executable: b"/bin/cat",
        argv: vec![b"/bin/cat".to_vec()],
        environment_profile: EnvironmentProfile::BasicReadOnly,
    })
}

fn plan_head_or_tail(words: &[Vec<u8>], executable: &'static [u8]) -> StageAdmission {
    let mut index: usize = 1;
    let mut count: &[u8] = b"10";
    if index < words.len() && words[index].as_slice() == b"-n" {
        if index + 1 >= words.len() || !bounded_decimal(&words[index + 1], MAX_LINE_COUNT) {
            return StageAdmission::ApprovalRequired(ApprovalReason::UnsupportedArgument);
        }
        count = &words[index + 1];
        index += 2;
    }

    if index != words.len() {
        return StageAdmission::ApprovalRequired(ApprovalReason::UnsupportedArgument);
    }

    StageAdmission::Direct(TemporaryStage {
        executable,
        argv: vec![executable.to_vec(), b"-n".to_vec(), count.to_vec()],
        environment_profile: EnvironmentProfile::BasicReadOnly,
    })
}

fn plan_grep(words: &[Vec<u8>]) -> StageAdmission {
    let mut argv: Vec<Vec<u8>> = vec![b"/usr/bin/grep".to_vec()];

    let mut index: usize = 1;
    while index < words.len() {
        let word = &words[index];
        if word.as_slice() == b"--" {
            index += 1;
            break;
        }
        if word.len() <= 1 || word[0] != b'-' {
            break;
        }
        for &flag in &word[1..] {
            let canonical: &[u8] = match flag {
                b'E' => b"-E",
                b'F' => b"-F",
                b'i' => b"-i",
                b'n' => b"-n",
                b'v' => b"-v",
                _ => {
                    return StageAdmission::ApprovalRequired(ApprovalReason::UnsupportedArgument)
                }
            };
            argv.push(canonical.to_vec());
        }
        index += 1;
    }
    if index >= words.len() {
        return StageAdmission::ApprovalRequired(ApprovalReason::UnsupportedArgument);
    }
    if words.len() - index != 1 {
        return StageAdmission::ApprovalRequired(ApprovalReason::UnsupportedArgument);
    }
    argv.push(b"--".to_vec());
    argv.push(words[index].clone());
    StageAdmission::Direct(TemporaryStage {
        executable: b"/usr/bin/grep",
        argv,
        environment_profile: EnvironmentProfile::BasicReadOnly,
    })
}

fn plan_git(words: &[Vec<u8>]) -> StageAdmission {
    if words.len() < 2 {
        return StageAdmission::ApprovalRequired(ApprovalReason::CommandOwnedInput);
    }
    if words[1].as_slice() == b"status" {
        return plan_git_status(&words[2..]);
    }
    if words[1].as_slice() == b"diff" {
        return plan_git_diff(&words[2..]);
    }
    if words[1].as_slice() == b"log" {
        return plan_git_log(&words[2..]);
    }
    StageAdmission::ApprovalRequired(ApprovalReason::CommandOwnedInput)
}

fn append_git_prelude(argv: &mut Vec<Vec<u8>>) {
    argv.extend([
        b"/usr/bin/git".to_vec(),
        b"--no-pager".to_vec(),
        b"-c".to_vec(),
        b"core.hooksPath=/dev/null".to_vec(),
        b"-c".to_vec(),
        b"core.fsmonitor=false".to_vec(),
    ]);
}

fn direct_git(argv: Vec<Vec<u8>>) -> StageAdmission {
    StageAdmission::Direct(TemporaryStage {
        executable: b"/usr/bin/git",
        argv,
        environment_profile: EnvironmentProfile::GitReadOnly,
    })
}

fn plan_git_status(arguments: &[Vec<u8>]) -> StageAdmission {
    let mut argv: Vec<Vec<u8>> = Vec::new();
    append_git_prelude(&mut argv);
    argv.extend([b"status".to_vec(), b"--ignore-submodules=all".to_vec()]);
    for argument in arguments {
        let arg = argument.as_slice();
        let canonical: &[u8] = if eq(arg, b"-s") {
            b"--short"
        } else if eq(arg, b"-b") {
            b"--branch"
        } else if eq(arg, b"--short")
            || eq(arg, b"--branch")
            || eq(arg, b"--porcelain")
            || eq(arg, b"--porcelain=v1")
            || eq(arg, b"--untracked-files=no")
            || eq(arg, b"--untracked-files=normal")
            || eq(arg, b"--untracked-files=all")
        {
            arg
        } else {
            return StageAdmission::ApprovalRequired(ApprovalReason::UnsupportedArgument);
        };
        argv.push(canonical.to_vec());
    }
    direct_git(argv)
}

fn plan_git_diff(arguments: &[Vec<u8>]) -> StageAdmission {
    let mut argv: Vec<Vec<u8>> = Vec::new();
    append_git_prelude(&mut argv);
    argv.extend([
        b"diff".to_vec(),
        b"--no-ext-diff".to_vec(),
        b"--no-textconv".to_vec(),
        b"--color=never".to_vec(),
    ]);

    let mut index: usize = 0;
    while index < arguments.len() && arguments[index].as_slice() != b"--" {
        let argument = arguments[index].as_slice();
        let accepted = eq(argument, b"--cached")
            || eq(argument, b"--staged")
            || eq(argument, b"--stat")
            || eq(argument, b"--shortstat")
            || eq(argument, b"--numstat")
            || eq(argument, b"--name-only")
            || eq(argument, b"--name-status")
            || eq(argument, b"--check")
            || eq(argument, b"--no-renames");
        if !accepted {
            return StageAdmission::ApprovalRequired(ApprovalReason::UnsupportedArgument);
        }
        argv.push(argument.to_vec());
        index += 1;
    }
    let operands: &[Vec<u8>] = if index < arguments.len() {
        &arguments[index + 1..]
    } else {
        &[]
    };
    if operands.len() > MAX_GIT_PATHSPECS {
        return StageAdmission::ApprovalRequired(ApprovalReason::UnsupportedArgument);
    }
    argv.push(b"--".to_vec());
    argv.extend(operands.iter().cloned());
    direct_git(argv)
}

fn plan_git_log(arguments: &[Vec<u8>]) -> StageAdmission {
    let mut argv: Vec<Vec<u8>> = Vec::new();
    append_git_prelude(&mut argv);
    argv.extend([
        b"log".to_vec(),
        b"--no-ext-diff".to_vec(),
        b"--no-textconv".to_vec(),
        b"--color=never".to_vec(),
        b"--max-count=100".to_vec(),
    ]);

    let mut index: usize = 0;
    while index < arguments.len() && arguments[index].as_slice() != b"--" {
        let argument = arguments[index].as_slice();
        if eq(argument, b"-n") {
            if index + 1 >= arguments.len()
                || !bounded_decimal(&arguments[index + 1], MAX_GIT_LOG_COUNT)
            {
                return StageAdmission::ApprovalRequired(ApprovalReason::UnsupportedArgument);
            }
            argv.push(arguments[index].clone());
            argv.push(arguments[index + 1].clone());
            index += 2;
            continue;
        }
        if argument.starts_with(b"--max-count=") {
            if !bounded_decimal(&argument[b"--max-count=".len()..], MAX_GIT_LOG_COUNT) {
                return StageAdmission::ApprovalRequired(ApprovalReason::UnsupportedArgument);
            }
            argv.push(argument.to_vec());
            index += 1;
            continue;
        }
        let accepted = eq(argument, b"--oneline")
            || eq(argument, b"--stat")
            || eq(argument, b"--shortstat")
            || eq(argument, b"--name-only")
            || eq(argument, b"--name-status")
            || eq(argument, b"--no-merges")
            || eq(argument, b"--decorate")
            || eq(argument, b"--decorate=short")
            || eq(argument, b"--decorate=no");
        if !accepted {
            return StageAdmission::ApprovalRequired(ApprovalReason::UnsupportedArgument);
        }
        argv.push(argument.to_vec());
        index += 1;
    }
    let operands: &[Vec<u8>] = if index < arguments.len() {
        &arguments[index + 1..]
    } else {
        &[]
    };
    if operands.len() > MAX_GIT_PATHSPECS {
        return StageAdmission::ApprovalRequired(ApprovalReason::UnsupportedArgument);
    }
    argv.push(b"--".to_vec());
    argv.extend(operands.iter().cloned());
    direct_git(argv)
}

fn bounded_decimal(value: &[u8], maximum: usize) -> bool {
    if value.is_empty() {
        return false;
    }
    if !value.iter().all(|&byte| byte.is_ascii_digit()) {
        return false;
    }
    match std::str::from_utf8(value).unwrap().parse::<usize>() {
        Ok(parsed) => parsed <= maximum,
        Err(_) => false,
    }
}

fn is_filesystem_mutation(command: &[u8]) -> bool {
    eq(command, b"touch")
        || eq(command, b"rm")
        || eq(command, b"mkdir")
        || eq(command, b"rmdir")
        || eq(command, b"mv")
        || eq(command, b"cp")
        || eq(command, b"install")
        || eq(command, b"chmod")
        || eq(command, b"chown")
        || eq(command, b"ln")
}

fn is_network_command(command: &[u8]) -> bool {
    eq(command, b"curl")
        || eq(command, b"wget")
        || eq(command, b"ssh")
        || eq(command, b"scp")
        || eq(command, b"nc")
}

fn is_process_or_system_command(command: &[u8]) -> bool {
    eq(command, b"sh")
        || eq(command, b"bash")
        || eq(command, b"zsh")
        || eq(command, b"dash")
        || eq(command, b"env")
        || eq(command, b"xargs")
        || eq(command, b"sudo")
        || eq(command, b"doas")
        || eq(command, b"kill")
        || eq(command, b"pkill")
        || eq(command, b"nohup")
}

fn own_plan(
    command: &[u8],
    resolved_cwd: &[u8],
    temporary: Vec<TemporaryStage>,
) -> Admission {
    let stages: Vec<DirectStage> = temporary
        .into_iter()
        .map(|stage| DirectStage {
            executable: stage.executable,
            argv: stage.argv,
            environment_profile: stage.environment_profile,
        })
        .collect();
    Admission::DirectReadOnly(DirectReadOnlyPlan {
        command: command.to_vec(),
        cwd: resolved_cwd.to_vec(),
        stages,
    })
}

fn trim_ascii<'a>(bytes: &'a [u8], set: &[u8]) -> &'a [u8] {
    let mut start = 0;
    let mut end = bytes.len();
    while start < end && set.contains(&bytes[start]) {
        start += 1;
    }
    while end > start && set.contains(&bytes[end - 1]) {
        end -= 1;
    }
    &bytes[start..end]
}

fn eq(a: &[u8], b: &[u8]) -> bool {
    a == b
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expect_direct(command: &[u8], target_os: TargetOs) -> Admission {
        let admission = plan(command, b"/workspace", false, target_os);
        if let Admission::ApprovalRequired(reason) = &admission {
            panic!(
                "expected direct plan for {:?}, got {:?}",
                String::from_utf8_lossy(command),
                reason
            );
        }
        admission
    }

    fn expect_approval(
        command: &[u8],
        background: bool,
        target_os: TargetOs,
        expected: ApprovalReason,
    ) {
        let admission = plan(command, b"/workspace", background, target_os);
        match admission {
            Admission::DirectReadOnly(_) => panic!("expected approval"),
            Admission::ApprovalRequired(reason) => assert_eq!(reason, expected),
        }
    }

    fn expect_argv(expected: &[&[u8]], actual: &[Vec<u8>]) {
        assert_eq!(expected.len(), actual.len());
        for (expected_arg, actual_arg) in expected.iter().zip(actual.iter()) {
            assert_eq!(*expected_arg, actual_arg.as_slice());
        }
    }

    fn append_shell_single_quoted(list: &mut Vec<u8>, value: &[u8]) {
        assert!(!value.contains(&b'\''));
        list.push(b'\'');
        list.extend_from_slice(value);
        list.push(b'\'');
    }

    fn build_printf_command(format: &[u8], data: &[&[u8]]) -> Vec<u8> {
        let mut command: Vec<u8> = Vec::new();
        command.extend_from_slice(b"printf ");
        append_shell_single_quoted(&mut command, format);
        for value in data {
            command.push(b' ');
            append_shell_single_quoted(&mut command, value);
        }
        command
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn host_target_os() -> TargetOs {
        if cfg!(target_os = "macos") {
            TargetOs::Macos
        } else {
            TargetOs::Linux
        }
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn run_argv(argv: &[Vec<u8>]) -> (i32, Vec<u8>, Vec<u8>) {
        use std::os::unix::ffi::OsStrExt;
        let mut cmd = std::process::Command::new(std::ffi::OsStr::from_bytes(&argv[0]));
        for arg in &argv[1..] {
            cmd.arg(std::ffi::OsStr::from_bytes(arg));
        }
        let output = cmd.output().expect("failed to spawn command");
        (
            output.status.code().unwrap_or(-1),
            output.stdout,
            output.stderr,
        )
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn run_argv_env(argv: &[Vec<u8>], cwd: &std::path::Path) -> (i32, Vec<u8>, Vec<u8>) {
        use std::os::unix::ffi::OsStrExt;
        let mut cmd = std::process::Command::new(std::ffi::OsStr::from_bytes(&argv[0]));
        for arg in &argv[1..] {
            cmd.arg(std::ffi::OsStr::from_bytes(arg));
        }
        cmd.current_dir(cwd)
            .env("PATH", "/usr/bin:/bin")
            .env("LC_ALL", "C")
            .env("LANG", "C");
        let output = cmd.output().expect("failed to spawn command");
        (
            output.status.code().unwrap_or(-1),
            output.stdout,
            output.stderr,
        )
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn expect_native_printf_equivalent(command: &[u8], target_os: TargetOs) {
        let admission = expect_direct(command, target_os);
        let argv = match &admission {
            Admission::DirectReadOnly(plan) => plan.stages[0].argv.clone(),
            Admission::ApprovalRequired(_) => unreachable!(),
        };
        drop(admission);

        let shell_argv: Vec<Vec<u8>> =
            vec![b"/bin/sh".to_vec(), b"-lc".to_vec(), command.to_vec()];
        let shell_result = run_argv(&shell_argv);
        let direct_result = run_argv(&argv);
        assert_eq!(
            shell_result, direct_result,
            "native printf mismatch for {:?}",
            String::from_utf8_lossy(command)
        );
    }

    #[test]
    fn known_reversible_auto_commands_exclude_destructive_and_hidden_effects() {
        for command in [
            &b"node -v"[..],
            b"node -v && npm -v",
            b"which node npm && node -v",
            b"git status --short --branch",
            b"git remote -v",
            b"git worktree list --porcelain",
            b"git fetch origin main",
            b"npm install 2>&1",
            b"npm run dev",
            b"zig build test",
            b"bun test",
        ] {
            assert!(
                known_reversible_auto_command(command, false),
                "should be reversible: {:?}",
                String::from_utf8_lossy(command)
            );
        }
        for command in [
            &b"rm -rf ."[..],
            b"git rm -r netlify",
            b"git push origin HEAD",
            b"cat ~/.ssh/id_rsa",
            b"sh -c 'npm install'",
            b"npm exec -- rm -rf .",
            b"npm install --prefix=/tmp/global",
            b"npm install --location global",
            b"git fetch --prune origin",
        ] {
            assert!(
                !known_reversible_auto_command(command, false),
                "should not be reversible: {:?}",
                String::from_utf8_lossy(command)
            );
        }
        assert!(!known_reversible_auto_command(b"npm run dev", true));
    }

    #[test]
    fn planner_canonicalizes_pwd_to_a_fixed_physical_path() {
        let admission = plan(b"pwd", b"/workspace", false, TargetOs::Macos);
        match admission {
            Admission::DirectReadOnly(direct) => {
                assert_eq!(direct.cwd, &b"/workspace"[..]);
                assert_eq!(direct.stages.len(), 1);
                assert_eq!(direct.stages[0].executable, &b"/bin/pwd"[..]);
                assert_eq!(direct.stages[0].argv.len(), 2);
                assert_eq!(direct.stages[0].argv[0], &b"/bin/pwd"[..]);
                assert_eq!(direct.stages[0].argv[1], &b"-P"[..]);
                assert_eq!(
                    direct.stages[0].environment_profile,
                    EnvironmentProfile::BasicReadOnly
                );
            }
            Admission::ApprovalRequired(reason) => {
                panic!("unexpected approval reason: {:?}", reason);
            }
        }
    }

    #[test]
    fn planner_admits_the_reviewed_direct_command_matrix() {
        let cases: &[&[u8]] = &[
            b"printf 'hello\\n'",
            b"printf x",
            b"printf '%%'",
            b"printf '%s' '--help'",
            b"printf '%s%s' one two",
            b"printf '$(touch x)'",
            b"pwd",
            b"pwd -P",
            b"ls",
            b"ls src",
            b"ls -la src",
            b"ls -- -option-looking-path",
            b"wc",
            b"wc -Lclmw",
            b"cat",
            b"head",
            b"head -n 20",
            b"tail -n 20",
            b"grep -n TODO",
            b"grep -Fi -- '-needle'",
            b"git status",
            b"git status --short --branch",
            b"git diff --stat",
            b"git diff --name-only -- src",
            b"git log --oneline -n 5",
            b"git log --max-count=20 -- src",
            b"printf x | wc -c",
            b"git status --short | wc -l",
        ];

        for command in cases {
            let admission = expect_direct(command, TargetOs::Macos);
            drop(admission);
        }

        expect_approval(
            b"wc -c < input.txt",
            false,
            TargetOs::Linux,
            ApprovalReason::UnsupportedInputRedirect,
        );
    }

    #[test]
    fn planner_returns_stable_reasons_for_approval_bearing_forms() {
        #[derive(Clone, Copy)]
        struct Case {
            command: &'static [u8],
            reason: ApprovalReason,
            target_os: TargetOs,
        }

        fn case(command: &'static [u8], reason: ApprovalReason) -> Case {
            Case {
                command,
                reason,
                target_os: TargetOs::Macos,
            }
        }

        fn case_linux(command: &'static [u8], reason: ApprovalReason) -> Case {
            Case {
                command,
                reason,
                target_os: TargetOs::Linux,
            }
        }

        let cases = [
            case(b"touch created.txt", ApprovalReason::FilesystemWrite),
            case(b"printf x>created.txt", ApprovalReason::FilesystemWrite),
            case(b"printf x >> created.txt", ApprovalReason::FilesystemWrite),
            case(b"printf x 2>errors.txt", ApprovalReason::FilesystemWrite),
            case(b"cat <> state.txt", ApprovalReason::FilesystemWrite),
            case(b"rm file", ApprovalReason::FilesystemWrite),
            case(
                b"curl https://example.com",
                ApprovalReason::NetworkAccess,
            ),
            case(
                b"printf \"$(touch created.txt)\"",
                ApprovalReason::DynamicShell,
            ),
            case(
                b"printf `touch created.txt`",
                ApprovalReason::DynamicShell,
            ),
            case(b"cat <(touch created.txt)", ApprovalReason::DynamicShell),
            case(
                b"git status && touch created.txt",
                ApprovalReason::UnsupportedShell,
            ),
            case(
                b"printf x | tee created.txt",
                ApprovalReason::UnknownCommand,
            ),
            case_linux(
                b"printf x | wc -c < input.txt",
                ApprovalReason::UnsupportedInputRedirect,
            ),
            case(b"sh -c 'printf x'", ApprovalReason::ProcessOrSystem),
            case(b"PATH=/tmp ls", ApprovalReason::DynamicShell),
            case(b"pwd -L", ApprovalReason::UnsupportedArgument),
            case(b"pwd extra", ApprovalReason::UnsupportedArgument),
            case(b"ls -R", ApprovalReason::UnsupportedArgument),
            case(b"ls -L linked-dir", ApprovalReason::UnsupportedArgument),
            case(b"ls --color=always", ApprovalReason::UnsupportedArgument),
            case(b"ls -e", ApprovalReason::UnsupportedArgument),
            case(b"wc -c input.txt", ApprovalReason::UnsupportedArgument),
            case(b"wc -c /dev/null", ApprovalReason::UnsupportedArgument),
            case(b"wc -c fifo", ApprovalReason::UnsupportedArgument),
            case(b"wc --files0-from=paths", ApprovalReason::UnsupportedArgument),
            case(b"git -C other status", ApprovalReason::CommandOwnedInput),
            case(b"git push origin HEAD", ApprovalReason::CommandOwnedInput),
            case(b"git status --ignored", ApprovalReason::UnsupportedArgument),
            case(
                b"git status --short -- src",
                ApprovalReason::UnsupportedArgument,
            ),
            case(b"git diff --ext-diff", ApprovalReason::UnsupportedArgument),
            case(b"git diff HEAD", ApprovalReason::UnsupportedArgument),
            case(b"git log -p", ApprovalReason::UnsupportedArgument),
            case(b"git log -n 1001", ApprovalReason::UnsupportedArgument),
            case(
                b"git log --format='%x00'",
                ApprovalReason::UnsupportedArgument,
            ),
            case(b"cat -n README.md", ApprovalReason::UnsupportedArgument),
            case(b"cat README.md", ApprovalReason::UnsupportedArgument),
            case(
                b"cat /tmp/.ssh/id_ed25519",
                ApprovalReason::UnsupportedArgument,
            ),
            case(b"head README.md", ApprovalReason::UnsupportedArgument),
            case(b"head -n 20 README.md", ApprovalReason::UnsupportedArgument),
            case(b"head -n -1 README.md", ApprovalReason::UnsupportedArgument),
            case(b"head -n 10001 README.md", ApprovalReason::UnsupportedArgument),
            case(b"tail app.log", ApprovalReason::UnsupportedArgument),
            case(b"tail -f app.log", ApprovalReason::UnsupportedArgument),
            case(b"grep TODO src", ApprovalReason::UnsupportedArgument),
            case(b"grep token /tmp/.env", ApprovalReason::UnsupportedArgument),
            case(b"grep -r TODO src", ApprovalReason::UnsupportedArgument),
            case(b"grep -e TODO src", ApprovalReason::UnsupportedArgument),
            case(b"grep -n", ApprovalReason::UnsupportedArgument),
            case(b"unknown-reader file", ApprovalReason::UnknownCommand),
            case(
                b"wc -c < input.txt",
                ApprovalReason::UnsupportedInputRedirect,
            ),
            case_linux(
                b"wc -c < input.txt",
                ApprovalReason::UnsupportedInputRedirect,
            ),
            case(b"printf \"$HOME\"", ApprovalReason::DynamicShell),
            case(b"printf \"$x\"", ApprovalReason::DynamicShell),
            case(b"printf \"a\\q\"", ApprovalReason::UnsupportedShell),
            case(b"printf $'x'", ApprovalReason::DynamicShell),
            case(b"printf x # comment", ApprovalReason::UnsupportedShell),
            case(b"printf", ApprovalReason::UnsupportedArgument),
            case(b"printf ''", ApprovalReason::UnsupportedArgument),
            case(b"printf '' x", ApprovalReason::UnsupportedArgument),
            case(b"printf --help", ApprovalReason::UnsupportedArgument),
            case(b"printf --", ApprovalReason::UnsupportedArgument),
            case(b"printf %q x", ApprovalReason::UnsupportedArgument),
            case(b"printf %b '\\x41'", ApprovalReason::UnsupportedArgument),
            case(b"printf %d 1.5", ApprovalReason::UnsupportedArgument),
            case(b"printf '%100s' x", ApprovalReason::UnsupportedArgument),
            case(b"printf 'hello\\n' extra", ApprovalReason::UnsupportedArgument),
            case(b"printf %% extra", ApprovalReason::UnsupportedArgument),
            case(b"printf '%s'", ApprovalReason::UnsupportedArgument),
            case(b"printf '%s%s' one", ApprovalReason::UnsupportedArgument),
            case(b"printf x\\ y", ApprovalReason::UnsupportedShell),
            case(b"printf x \\\n y", ApprovalReason::UnsupportedShell),
            case_linux(
                b"wc -c < first.txt < second.txt",
                ApprovalReason::UnsupportedInputRedirect,
            ),
            case_linux(
                b"pwd < input.txt",
                ApprovalReason::UnsupportedInputRedirect,
            ),
            case_linux(
                b"printf x < input.txt",
                ApprovalReason::UnsupportedInputRedirect,
            ),
            case_linux(
                b"ls < input.txt",
                ApprovalReason::UnsupportedInputRedirect,
            ),
            case(b"printf x\r|wc -c", ApprovalReason::UnsupportedShell),
            case(b"ls alpha\rbeta", ApprovalReason::UnsupportedShell),
            case(b"printf 'x\ry'", ApprovalReason::UnsupportedShell),
            case(b"printf *.txt", ApprovalReason::DynamicShell),
            case(b"printf ~", ApprovalReason::DynamicShell),
            case(b"printf 'unterminated", ApprovalReason::UnsupportedShell),
            case(b"", ApprovalReason::UnknownCommand),
        ];

        for case in cases {
            expect_approval(case.command, false, case.target_os, case.reason);
        }
    }

    #[test]
    fn planner_preserves_quoted_shell_metacharacters_as_literal_argv() {
        let cases: &[(&[u8], &[u8])] = &[
            (b"printf '%s' '<'", b"<"),
            (b"printf '%s' '>'", b">"),
            (b"printf '%s' '|'", b"|"),
            (b"printf '%s' ';'", b";"),
            (b"printf '%s' '&'", b"&"),
        ];
        for &(command, expected) in cases {
            let admission = expect_direct(command, TargetOs::Linux);
            match &admission {
                Admission::DirectReadOnly(plan) => {
                    assert_eq!(plan.stages[0].argv[2], expected);
                }
                Admission::ApprovalRequired(_) => unreachable!(),
            }
        }

        expect_approval(
            b"wc -c '<' /etc/passwd",
            false,
            TargetOs::Linux,
            ApprovalReason::UnsupportedArgument,
        );
    }

    #[test]
    fn planner_enforces_background_and_platform_boundaries() {
        expect_approval(b"pwd", true, TargetOs::Macos, ApprovalReason::BackgroundProcess);
        expect_approval(
            b"pwd",
            false,
            TargetOs::Windows,
            ApprovalReason::UnsupportedPlatform,
        );
    }

    #[test]
    fn planner_bounds_direct_pipelines_at_eight_stages() {
        let accepted =
            b"printf x | wc -c | wc -c | wc -c | wc -c | wc -c | wc -c | wc -c";
        let direct = expect_direct(accepted, TargetOs::Linux);
        drop(direct);

        let rejected = b"printf x | wc -c | wc -c | wc -c | wc -c | wc -c | wc -c | wc -c | wc -c";
        expect_approval(rejected, false, TargetOs::Linux, ApprovalReason::ProcessOrSystem);
    }

    #[test]
    fn planner_canonicalizes_target_policies_and_per_stage_environments() {
        let admission = expect_direct(b"ls -la src | wc -c", TargetOs::Linux);
        match &admission {
            Admission::DirectReadOnly(direct) => {
                assert_eq!(direct.stages.len(), 2);
                assert_eq!(direct.stages[0].executable, &b"/bin/ls"[..]);
                assert_eq!(direct.stages[0].argv[1], &b"-q"[..]);
                assert_eq!(direct.stages[0].argv[2], &b"-n"[..]);
                assert_eq!(direct.stages[0].argv[3], &b"-a"[..]);
                assert_eq!(direct.stages[0].argv[4], &b"--"[..]);
                assert_eq!(direct.stages[0].argv[5], &b"src"[..]);
                assert_eq!(
                    direct.stages[0].environment_profile,
                    EnvironmentProfile::BasicReadOnly
                );

                assert_eq!(direct.stages[1].executable, &b"/usr/bin/wc"[..]);
                assert_eq!(direct.stages[1].argv[1], &b"-c"[..]);
                assert_eq!(
                    direct.stages[1].environment_profile,
                    EnvironmentProfile::BasicReadOnly
                );
            }
            Admission::ApprovalRequired(_) => unreachable!(),
        }
    }

    #[test]
    fn planner_pins_read_only_git_inspection_to_hardened_argv_and_environment() {
        let admission = expect_direct(b"git diff --name-only -- src", TargetOs::Linux);
        match &admission {
            Admission::DirectReadOnly(direct) => {
                let stage = &direct.stages[0];
                assert_eq!(stage.environment_profile, EnvironmentProfile::GitReadOnly);
                assert_eq!(stage.executable, &b"/usr/bin/git"[..]);
                let expected: &[&[u8]] = &[
                    b"/usr/bin/git",
                    b"--no-pager",
                    b"-c",
                    b"core.hooksPath=/dev/null",
                    b"-c",
                    b"core.fsmonitor=false",
                    b"diff",
                    b"--no-ext-diff",
                    b"--no-textconv",
                    b"--color=never",
                    b"--name-only",
                    b"--",
                    b"src",
                ];
                expect_argv(expected, &stage.argv);
            }
            Admission::ApprovalRequired(_) => unreachable!(),
        }
    }

    #[test]
    fn planner_limits_bounded_readers_to_pipeline_input() {
        let cases: &[(&[u8], &[&[u8]])] = &[
            (b"cat", &[b"/bin/cat"]),
            (b"head -n 25", &[b"/usr/bin/head", b"-n", b"25"]),
            (b"tail", &[b"/usr/bin/tail", b"-n", b"10"]),
            (
                b"grep -Fin needle",
                &[b"/usr/bin/grep", b"-F", b"-i", b"-n", b"--", b"needle"],
            ),
        ];
        for &(command, expected) in cases {
            let admission = expect_direct(command, TargetOs::Macos);
            match &admission {
                Admission::DirectReadOnly(direct) => {
                    let stage = &direct.stages[0];
                    assert_eq!(stage.environment_profile, EnvironmentProfile::BasicReadOnly);
                    expect_argv(expected, &stage.argv);
                }
                Admission::ApprovalRequired(_) => unreachable!(),
            }
        }
    }

    #[test]
    fn planner_target_policies_use_reviewed_absolute_executables_and_argv() {
        for target_os in [TargetOs::Macos, TargetOs::Linux] {
            let printf_policy = printf_policy(target_os);
            assert_eq!(printf_policy.executable, &b"/usr/bin/printf"[..]);
            assert_eq!(
                printf_policy.format_language,
                PrintfFormatLanguage::PortableLiteralNewlinePercentString
            );

            let ls_policy = ls_policy(target_os);
            assert_eq!(ls_policy.executable, &b"/bin/ls"[..]);
            assert_eq!(ls_policy.forced_argv.len(), 1);
            assert_eq!(ls_policy.forced_argv[0], &b"-q"[..]);
            assert_eq!(ls_policy.symlink_semantics, LsSymlinkSemantics::PlatformDefault);

            let commands: &[&[u8]] = &[b"printf x", b"pwd", b"ls -1aAdFlnp", b"wc -Lclmw"];
            for command in commands {
                let admission = expect_direct(command, target_os);
                match &admission {
                    Admission::DirectReadOnly(direct) => {
                        for stage in &direct.stages {
                            assert!(stage.executable[0] == b'/');
                            assert_eq!(stage.executable, stage.argv[0].as_slice());
                            assert_eq!(stage.environment_profile, EnvironmentProfile::BasicReadOnly);
                            for arg in &stage.argv {
                                assert_ne!(arg.as_slice(), b"sh");
                                assert_ne!(arg.as_slice(), b"bash");
                                assert_ne!(arg.as_slice(), b"cmd");
                                assert_ne!(arg.as_slice(), b"just-bash");
                                assert!(
                                    arg.as_slice() != b"-P" || stage.executable == b"/bin/pwd"
                                );
                            }
                        }
                    }
                    Admission::ApprovalRequired(_) => unreachable!(),
                }
            }
        }
    }

    #[test]
    fn planner_accepts_every_reviewed_wc_flag_and_rejects_every_operand() {
        let accepted: &[&[u8]] = &[
            b"wc",
            b"wc -L",
            b"wc -c",
            b"wc -l",
            b"wc -m",
            b"wc -w",
            b"wc -Lclmw",
            b"wc -L -c -l -m -w",
        ];
        for command in accepted {
            let admission = expect_direct(command, TargetOs::Linux);
            drop(admission);
        }

        let operands: &[&[u8]] = &[
            b"wc file",
            b"wc -c file",
            b"wc /dev/null",
            b"wc fifo",
            b"wc --",
            b"wc --bytes",
        ];
        for command in operands {
            expect_approval(command, false, TargetOs::Linux, ApprovalReason::UnsupportedArgument);
        }
    }

    #[test]
    fn planner_implements_the_total_portable_printf_grammar() {
        let accepted: &[&[u8]] = &[
            b"printf x",
            b"printf 'hello world'",
            b"printf 'hello\\n'",
            b"printf '%%%%'",
            b"printf '%s' ''",
            b"printf '%s' '-leading'",
            b"printf '%s' '100%'",
            b"printf '%s' '\\x41'",
            "printf '%s' 'caf\u{e9}'".as_bytes(),
            b"printf '%s%s' one two",
            b"printf 'a\\n%%:%s' value",
            b"printf 'a'\"b\"",
        ];
        for command in accepted {
            let admission = expect_direct(command, TargetOs::Macos);
            drop(admission);
        }

        let rejected: &[&[u8]] = &[
            b"printf ''",
            b"printf '-x'",
            b"printf '%s'",
            b"printf x extra",
            b"printf '%s' one two",
            b"printf '%s%s' one",
            b"printf '%b' x",
            b"printf '%q' x",
            b"printf '%d' 1",
            b"printf '%-s' x",
            b"printf '%1s' x",
            b"printf '%.1s' x",
            b"printf '%1$s' x",
            b"printf '\\t'",
            b"printf '\\c'",
            b"printf '\\101'",
            b"printf '\\x41'",
            b"printf '%'",
            b"printf '\\'",
        ];
        for command in rejected {
            expect_approval(
                command,
                false,
                TargetOs::Macos,
                ApprovalReason::UnsupportedArgument,
            );
        }
    }

    #[test]
    fn planner_generated_printf_grammar_accepts_only_exact_productions_and_data_counts() {
        let data_classes: &[&[u8]] = &[
            b"",
            b"-leading",
            b"100%",
            b"\\x41",
            b"printable ASCII",
            "caf\u{e9}".as_bytes(),
        ];
        let zero_data_formats: &[&[u8]] = &[
            b"x",
            b"hello world",
            b"hello\\n",
            b"%%",
            b"literal\\npercent:%%",
        ];
        for format in zero_data_formats {
            let command = build_printf_command(format, &[]);
            let admission = expect_direct(&command, TargetOs::Macos);
            drop(admission);

            let surplus = build_printf_command(format, &[data_classes[0]]);
            expect_approval(
                &surplus,
                false,
                TargetOs::Macos,
                ApprovalReason::UnsupportedArgument,
            );
        }

        let one_data_formats: &[&[u8]] = &[b"%s", b"prefix:%s", b"%s:suffix", b"a\\n%%:%s"];
        for format in one_data_formats {
            for data in data_classes {
                let command = build_printf_command(format, &[data]);
                let admission = expect_direct(&command, TargetOs::Linux);
                drop(admission);
            }

            let missing = build_printf_command(format, &[]);
            expect_approval(
                &missing,
                false,
                TargetOs::Linux,
                ApprovalReason::UnsupportedArgument,
            );
            let surplus = build_printf_command(format, &[data_classes[0], data_classes[1]]);
            expect_approval(
                &surplus,
                false,
                TargetOs::Linux,
                ApprovalReason::UnsupportedArgument,
            );
        }

        for first in data_classes {
            for second in data_classes {
                let command = build_printf_command(b"%s:%s", &[first, second]);
                let admission = expect_direct(&command, TargetOs::Macos);
                drop(admission);
            }
        }

        for format in [
            &b""[..],
            b"-leading",
            b"%b",
            b"%q",
            b"%d",
            b"%-s",
            b"%1s",
            b"%.1s",
            b"%1$s",
            b"\\t",
            b"\\c",
            b"\\101",
            b"\\x41",
            b"%",
            b"\\",
        ] {
            let command = build_printf_command(format, &[b"x"]);
            expect_approval(
                &command,
                false,
                TargetOs::Macos,
                ApprovalReason::UnsupportedArgument,
            );
        }
    }

    #[test]
    fn planner_preserves_accepted_printf_argv_exactly() {
        let admission = expect_direct(
            "printf '%s%s' '-x' 'caf\u{e9}'".as_bytes(),
            TargetOs::Macos,
        );
        match &admission {
            Admission::DirectReadOnly(direct) => {
                let argv = &direct.stages[0].argv;
                assert_eq!(argv.len(), 4);
                assert_eq!(argv[0], &b"/usr/bin/printf"[..]);
                assert_eq!(argv[1], &b"%s%s"[..]);
                assert_eq!(argv[2], &b"-x"[..]);
                assert_eq!(argv[3], "caf\u{e9}".as_bytes());
            }
            Admission::ApprovalRequired(_) => unreachable!(),
        }
    }

    #[test]
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn planner_native_printf_forms_match_shell_visible_execution() {
        let target_os = host_target_os();
        let commands: &[&[u8]] = &[
            b"printf x",
            b"printf 'hello\\n'",
            b"printf '%%'",
            b"printf '%s' '--help'",
            b"printf '%s%s' one two",
            "printf '%s' 'caf\u{e9}'".as_bytes(),
        ];
        for command in commands {
            expect_native_printf_equivalent(command, target_os);
        }
    }

    #[test]
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn planner_generated_native_printf_matrix_matches_shell_bytes_status_and_stderr() {
        let target_os = host_target_os();
        let data_classes: &[&[u8]] = &[
            b"",
            b"-leading",
            b"100%",
            b"\\x41",
            b"printable ASCII",
            "caf\u{e9}".as_bytes(),
        ];
        for format in [
            &b"x"[..],
            b"hello world",
            b"hello\\n",
            b"%%",
            b"literal\\npercent:%%",
        ] {
            let command = build_printf_command(format, &[]);
            expect_native_printf_equivalent(&command, target_os);
        }
        for format in [&b"%s"[..], b"prefix:%s", b"%s:suffix", b"a\\n%%:%s"] {
            for data in data_classes {
                let command = build_printf_command(format, &[data]);
                expect_native_printf_equivalent(&command, target_os);
            }
        }
        for first in data_classes {
            for second in data_classes {
                let command = build_printf_command(b"%s:%s", &[first, second]);
                expect_native_printf_equivalent(&command, target_os);
            }
        }
    }

    #[test]
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn planner_native_ls_policy_preserves_reviewed_target_behavior() {
        use std::os::unix::ffi::OsStrExt;
        use std::sync::atomic::{AtomicUsize, Ordering};

        static COUNTER: AtomicUsize = AtomicUsize::new(0);

        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }

        let target_os = host_target_os();
        let unique = COUNTER.fetch_add(1, Ordering::SeqCst);
        let tmp = std::env::temp_dir().join(format!(
            "fx-ls-policy-{}-{}",
            std::process::id(),
            unique
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        let _cleanup = Cleanup(tmp.clone());

        std::fs::write(tmp.join("file"), b"file").unwrap();
        std::fs::create_dir_all(tmp.join("real-dir")).unwrap();
        std::fs::write(tmp.join("real-dir/inside"), b"inside").unwrap();
        std::fs::write(tmp.join("-option-looking"), b"").unwrap();
        std::fs::write(tmp.join("\u{1b}hostile"), b"").unwrap();
        std::os::unix::fs::symlink("file", tmp.join("file-link")).unwrap();
        std::os::unix::fs::symlink("real-dir", tmp.join("dir-link")).unwrap();

        let cwd = std::fs::canonicalize(&tmp).unwrap();
        let cwd_bytes = cwd.as_os_str().as_bytes().to_vec();
        assert!(!cwd_bytes.is_empty());

        struct Case {
            command: &'static [u8],
            expected_stdout: Option<&'static [u8]>,
            succeeds: bool,
        }

        let cases = [
            Case {
                command: b"ls real-dir",
                expected_stdout: Some(b"inside\n"),
                succeeds: true,
            },
            Case {
                command: b"ls file-link",
                expected_stdout: Some(b"file-link\n"),
                succeeds: true,
            },
            Case {
                command: b"ls dir-link",
                expected_stdout: Some(b"inside\n"),
                succeeds: true,
            },
            Case {
                command: b"ls -d dir-link",
                expected_stdout: Some(b"dir-link\n"),
                succeeds: true,
            },
            Case {
                command: b"ls -F file-link dir-link",
                expected_stdout: Some(b"dir-link@\nfile-link@\n"),
                succeeds: true,
            },
            Case {
                command: b"ls -- -option-looking",
                expected_stdout: Some(b"-option-looking\n"),
                succeeds: true,
            },
            Case {
                command: b"ls missing",
                expected_stdout: None,
                succeeds: false,
            },
        ];
        for case in cases {
            let admission = expect_direct(case.command, target_os);
            let stage = match &admission {
                Admission::DirectReadOnly(plan) => &plan.stages[0],
                Admission::ApprovalRequired(_) => unreachable!(),
            };
            let (code, stdout, stderr) = run_argv_env(&stage.argv, &cwd);
            let succeeded = code == 0;
            assert_eq!(case.succeeds, succeeded, "command: {:?}", String::from_utf8_lossy(case.command));
            if let Some(expected) = case.expected_stdout {
                assert_eq!(stdout, expected);
                assert!(stderr.is_empty());
            } else {
                assert!(!stderr.is_empty());
            }
        }

        let hostile_failure = expect_direct(b"ls '\x1bmissing'", target_os);
        let stage = match &hostile_failure {
            Admission::DirectReadOnly(plan) => &plan.stages[0],
            Admission::ApprovalRequired(_) => unreachable!(),
        };
        let (_, stdout, stderr) = run_argv_env(&stage.argv, &cwd);
        assert!(stdout.is_empty());
        assert!(!stderr.is_empty());
        assert!(!stderr.contains(&0x1b));

        let bare = expect_direct(b"ls", target_os);
        let stage = match &bare {
            Admission::DirectReadOnly(plan) => &plan.stages[0],
            Admission::ApprovalRequired(_) => unreachable!(),
        };
        let (_, stdout, stderr) = run_argv_env(&stage.argv, &cwd);
        assert!(stderr.is_empty());
        assert!(find_subslice(&stdout, b"?hostile\n"));
        assert!(!stdout.contains(&0x1b));
        for entry in [
            &b"-option-looking\n"[..],
            b"dir-link\n",
            b"file\n",
            b"file-link\n",
            b"real-dir\n",
        ] {
            assert!(find_subslice(&stdout, entry), "missing entry {:?}", String::from_utf8_lossy(entry));
        }

        let numeric = expect_direct(b"ls -l file", target_os);
        let stage = match &numeric {
            Admission::DirectReadOnly(plan) => &plan.stages[0],
            Admission::ApprovalRequired(_) => unreachable!(),
        };
        let expected_numeric_argv: &[&[u8]] = &[b"/bin/ls", b"-q", b"-n", b"--", b"file"];
        expect_argv(expected_numeric_argv, &stage.argv);
        let (_, stdout, stderr) = run_argv_env(&stage.argv, &cwd);
        assert!(stderr.is_empty());
        assert!(stdout.ends_with(b" file\n"));
        let text = String::from_utf8(stdout).unwrap();
        let mut fields = text.split(' ');
        let _ = fields.next().unwrap();
        let _ = fields.next().unwrap();
        let owner = fields.next().unwrap();
        let group = fields.next().unwrap();
        owner.parse::<u64>().unwrap();
        group.parse::<u64>().unwrap();
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn find_subslice(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|window| window == needle)
    }

    #[test]
    fn planner_bounds_ls_operands_at_sixty_four() {
        let mut accepted: Vec<u8> = Vec::new();
        accepted.extend_from_slice(b"ls");
        for index in 0..64 {
            accepted.extend_from_slice(format!(" path-{}", index).as_bytes());
        }

        let direct = expect_direct(&accepted, TargetOs::Linux);
        drop(direct);

        accepted.extend_from_slice(b" overflow");
        expect_approval(
            &accepted,
            false,
            TargetOs::Linux,
            ApprovalReason::UnsupportedArgument,
        );
    }
}
