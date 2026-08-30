//! Command-token classification used by command permission policy.
//!
//! These predicates decide which commands are safe to identify by their base
//! token, which wrappers are transparent for analysis, and how to derive the
//! terminal segment of a command for default shell list semantics.

use super::command_lex as lex;

const WHITESPACE: &[u8] = b" \t\r\n";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Token<'a> {
    pub text: &'a [u8],
    pub start: usize,
    pub end: usize,
}

/// Returns true when a command grant should preserve the full command text.
pub fn command_requires_literal_pattern(token: &[u8]) -> bool {
    command_hides_base_token(token)
}

fn command_hides_base_token(token: &[u8]) -> bool {
    is_shell_command(token) || is_delegating_command(token) || is_privilege_prefix(token)
}

fn is_shell_command(token: &[u8]) -> bool {
    eq(token, b"sh")
        || eq(token, b"bash")
        || eq(token, b"zsh")
        || eq(token, b"dash")
        || eq(token, b"ksh")
        || eq(token, b"csh")
        || eq(token, b"tcsh")
        || eq(token, b"fish")
        || eq(token, b"cmd")
        || eq(token, b"powershell")
        || eq(token, b"pwsh")
}

fn is_delegating_command(token: &[u8]) -> bool {
    eq(token, b"env")
        || eq(token, b"xargs")
        || eq(token, b"nice")
        || eq(token, b"stdbuf")
        || eq(token, b"unbuffer")
        || eq(token, b"nohup")
        || eq(token, b"timeout")
        || eq(token, b"time")
}

fn is_privilege_prefix(token: &[u8]) -> bool {
    eq(token, b"sudo") || eq(token, b"doas") || eq(token, b"su")
}

fn is_safe_env_command(token: &[u8]) -> bool {
    eq(token, b"env")
}

fn is_safe_priority_prefix(token: &[u8]) -> bool {
    eq(token, b"nice")
}

fn is_safe_stdio_prefix(token: &[u8]) -> bool {
    eq(token, b"stdbuf")
}

fn is_safe_passthrough_prefix(token: &[u8]) -> bool {
    eq(token, b"unbuffer") || eq(token, b"nohup") || eq(token, b"time")
}

fn is_safe_timeout_prefix(token: &[u8]) -> bool {
    eq(token, b"timeout")
}

/// Returns the first non-env command token when it is safe to identify.
pub fn base_command_token(command: &[u8]) -> Option<&[u8]> {
    if command.contains(&b'\n') {
        return None;
    }

    let mut cursor = skip_whitespace(command, 0);
    while let Some(token) = next_token(command, cursor) {
        if is_env_assignment(token.text) {
            if !lex::is_safe_env_assignment(token.text) {
                return None;
            }
            cursor = token.end;
            continue;
        }

        if command_hides_base_token(token.text) {
            return None;
        }
        if !is_accepted_command_identifier(token.text) {
            return None;
        }
        return Some(token.text);
    }
    None
}

/// Returns the command tail after peeling only safe analysis prefixes.
pub fn analysis_command_tail(command: &[u8]) -> &[u8] {
    let original = command;
    let mut cursor = skip_whitespace(command, 0);
    let mut peeled = false;

    while let Some(token) = next_token(command, cursor) {
        if is_env_assignment(token.text) {
            if !lex::is_safe_env_assignment(token.text) {
                return original;
            }
            cursor = token.end;
            peeled = true;
            continue;
        }

        if is_safe_env_command(token.text) {
            let Some(env_arg) = next_token(command, token.end) else {
                return original;
            };
            if !is_env_assignment(env_arg.text) || !lex::is_safe_env_assignment(env_arg.text) {
                return original;
            }
            cursor = env_arg.end;
            peeled = true;
            continue;
        }

        if is_safe_priority_prefix(token.text) {
            cursor = skip_nice_args(command, token.end);
            peeled = true;
            continue;
        }
        if is_safe_stdio_prefix(token.text) {
            cursor = skip_stdbuf_args(command, token.end);
            peeled = true;
            continue;
        }
        if is_safe_passthrough_prefix(token.text) {
            cursor = token.end;
            peeled = true;
            continue;
        }
        if is_safe_timeout_prefix(token.text) {
            let Some(duration) = next_token(command, token.end) else {
                return original;
            };
            if !is_timeout_duration(duration.text) {
                return original;
            }
            cursor = duration.end;
            peeled = true;
            continue;
        }

        return &command[token.start..];
    }

    if peeled {
        &command[cursor..]
    } else {
        original
    }
}

/// Returns the deterministic final command segment for default sh list semantics.
pub fn terminal_exit_segment(command: &[u8]) -> Option<&[u8]> {
    let trimmed = trim_whitespace(command);
    if trimmed.is_empty() {
        return None;
    }

    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;
    let mut segment_start: usize = 0;
    let mut i: usize = 0;

    while i < command.len() {
        let ch = command[i];
        if escaped {
            escaped = false;
            i += 1;
            continue;
        }
        if ch == b'\\' && !in_single {
            escaped = true;
            i += 1;
            continue;
        }
        if ch == b'\'' && !in_double {
            in_single = !in_single;
            i += 1;
            continue;
        }
        if ch == b'"' && !in_single {
            in_double = !in_double;
            i += 1;
            continue;
        }
        if in_single || in_double {
            i += 1;
            continue;
        }

        match ch {
            b'&' => return None,
            b'|' => {
                if i + 1 < command.len() && command[i + 1] == b'|' {
                    return None;
                }
                segment_start = i + 1;
            }
            b';' | b'\n' => segment_start = i + 1,
            _ => {}
        }
        i += 1;
    }

    if in_single || in_double {
        return None;
    }
    let segment = trim_whitespace(&command[segment_start..]);
    if segment.is_empty() {
        None
    } else {
        Some(segment)
    }
}

pub fn skip_whitespace(input: &[u8], start: usize) -> usize {
    let mut cursor = start;
    while cursor < input.len() && is_whitespace(input[cursor]) {
        cursor += 1;
    }
    cursor
}

pub fn next_token(input: &[u8], start: usize) -> Option<Token<'_>> {
    let token_start = skip_whitespace(input, start);
    if token_start >= input.len() {
        return None;
    }
    let mut token_end = token_start;
    while token_end < input.len() && !is_whitespace(input[token_end]) {
        token_end += 1;
    }
    Some(Token {
        text: &input[token_start..token_end],
        start: token_start,
        end: token_end,
    })
}

fn is_whitespace(ch: u8) -> bool {
    WHITESPACE.contains(&ch)
}

pub fn is_env_assignment(token: &[u8]) -> bool {
    let Some(eq) = token.iter().position(|&b| b == b'=') else {
        return false;
    };
    if eq == 0 {
        return false;
    }
    is_env_name(&token[..eq])
}

fn is_env_name(name: &[u8]) -> bool {
    if name.is_empty() {
        return false;
    }
    if !(name[0].is_ascii_uppercase() || name[0] == b'_') {
        return false;
    }
    name[1..]
        .iter()
        .all(|&ch| ch.is_ascii_uppercase() || ch.is_ascii_digit() || ch == b'_')
}

fn is_accepted_command_identifier(token: &[u8]) -> bool {
    if eq(token, b"[") {
        return true;
    }
    if token.is_empty() {
        return false;
    }

    let mut saw_letter = false;
    let mut prev_hyphen = false;
    for (i, &ch) in token.iter().enumerate() {
        if ch.is_ascii_lowercase() {
            saw_letter = true;
            prev_hyphen = false;
            continue;
        }
        if ch.is_ascii_digit() {
            prev_hyphen = false;
            continue;
        }
        if ch == b'-' {
            if i == 0 || i + 1 == token.len() || prev_hyphen {
                return false;
            }
            prev_hyphen = true;
            continue;
        }
        return false;
    }
    saw_letter && !prev_hyphen
}

fn skip_nice_args(command: &[u8], start: usize) -> usize {
    let Some(first) = next_token(command, start) else {
        return start;
    };
    if eq(first.text, b"-n") {
        let Some(value) = next_token(command, first.end) else {
            return start;
        };
        return value.end;
    }
    if first.text.len() > 1 && first.text[0] == b'-' && first.text[1].is_ascii_digit() {
        return first.end;
    }
    start
}

fn skip_stdbuf_args(command: &[u8], start: usize) -> usize {
    let mut cursor = start;
    while let Some(token) = next_token(command, cursor) {
        if !token.text.starts_with(b"-") {
            break;
        }
        cursor = token.end;
    }
    cursor
}

fn is_timeout_duration(token: &[u8]) -> bool {
    if token.is_empty() || !token[0].is_ascii_digit() {
        return false;
    }
    let mut saw_digit = false;
    let mut saw_dot = false;
    for (i, &ch) in token.iter().enumerate() {
        if ch.is_ascii_digit() {
            saw_digit = true;
            continue;
        }
        if ch == b'.' && !saw_dot && i + 1 < token.len() {
            saw_dot = true;
            continue;
        }
        if i + 1 == token.len() && (ch == b's' || ch == b'm' || ch == b'h' || ch == b'd') {
            return saw_digit;
        }
        return false;
    }
    saw_digit
}

fn trim_whitespace(bytes: &[u8]) -> &[u8] {
    let mut start = 0;
    let mut end = bytes.len();
    while start < end && WHITESPACE.contains(&bytes[start]) {
        start += 1;
    }
    while end > start && WHITESPACE.contains(&bytes[end - 1]) {
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

    #[test]
    fn literal_pattern_predicate_covers_delegated_and_privileged_commands() {
        assert!(command_requires_literal_pattern(b"bash"));
        assert!(command_requires_literal_pattern(b"cmd"));
        assert!(command_requires_literal_pattern(b"env"));
        assert!(command_requires_literal_pattern(b"nice"));
        assert!(command_requires_literal_pattern(b"sudo"));
        assert!(command_requires_literal_pattern(b"doas"));
        assert!(command_requires_literal_pattern(b"su"));
        assert!(!command_requires_literal_pattern(b"grep"));
    }

    #[test]
    fn analysis_command_tail_leaves_bare_command_unchanged() {
        assert_eq!(
            analysis_command_tail(b"grep foo bar.txt"),
            &b"grep foo bar.txt"[..]
        );
    }

    #[test]
    fn analysis_command_tail_starts_at_first_plain_command_token() {
        assert_eq!(analysis_command_tail(b"nice grep foo"), &b"grep foo"[..]);
    }

    #[test]
    fn known_env_prefix_is_transparent_to_command_analysis() {
        assert_eq!(
            base_command_token(b"NODE_ENV=prod grep foo"),
            Some(&b"grep"[..])
        );
        assert_eq!(
            analysis_command_tail(b"NODE_ENV=prod grep foo"),
            &b"grep foo"[..]
        );
    }

    #[test]
    fn unknown_env_prefix_fails_closed() {
        assert_eq!(base_command_token(b"MY_SECRET=x grep foo"), None);
        assert_eq!(
            analysis_command_tail(b"MY_SECRET=x grep foo"),
            &b"MY_SECRET=x grep foo"[..]
        );
    }

    #[test]
    fn shell_first_word_is_not_analyzed_as_base_command() {
        assert_eq!(base_command_token(b"bash -c 'echo hi'"), None);
        assert_eq!(
            analysis_command_tail(b"bash -c 'echo hi'"),
            &b"bash -c 'echo hi'"[..]
        );
    }

    #[test]
    fn priority_adjustment_prefix_is_transparent_to_policy_analysis() {
        assert_eq!(analysis_command_tail(b"nice grep foo"), &b"grep foo"[..]);
    }

    #[test]
    fn timeout_prefix_advances_past_its_duration_argument() {
        assert_eq!(
            analysis_command_tail(b"timeout 5 grep foo bar"),
            &b"grep foo bar"[..]
        );
    }

    #[test]
    fn env_command_is_transparent_only_with_a_known_assignment() {
        assert_eq!(
            analysis_command_tail(b"env NODE_ENV=prod grep foo"),
            &b"grep foo"[..]
        );
    }

    #[test]
    fn env_command_with_flag_fails_closed() {
        assert_eq!(
            analysis_command_tail(b"env -i grep foo"),
            &b"env -i grep foo"[..]
        );
    }

    #[test]
    fn env_command_with_unknown_assignment_fails_closed() {
        assert_eq!(
            analysis_command_tail(b"env MY_SECRET=x grep foo"),
            &b"env MY_SECRET=x grep foo"[..]
        );
    }

    #[test]
    fn env_command_with_bare_command_fails_closed() {
        assert_eq!(
            analysis_command_tail(b"env grep foo"),
            &b"env grep foo"[..]
        );
    }

    #[test]
    fn xargs_remains_the_analysis_command_because_stdin_supplies_arguments() {
        assert_eq!(
            analysis_command_tail(b"xargs grep foo"),
            &b"xargs grep foo"[..]
        );
    }

    #[test]
    fn privilege_prefixes_remain_anchored_during_generic_analysis() {
        assert_eq!(
            analysis_command_tail(b"sudo rm -rf /"),
            &b"sudo rm -rf /"[..]
        );
        assert_eq!(
            analysis_command_tail(b"doas rm -rf /"),
            &b"doas rm -rf /"[..]
        );
        assert_eq!(
            analysis_command_tail(b"su -c 'rm -rf /'"),
            &b"su -c 'rm -rf /'"[..]
        );
    }

    #[test]
    fn base_command_token_returns_normal_command_identifiers() {
        assert_eq!(base_command_token(b"python3 file.py"), Some(&b"python3"[..]));
    }

    #[test]
    fn base_command_token_rejects_unknown_env_prefix() {
        assert_eq!(base_command_token(b"MY_SECRET=x grep foo"), None);
    }

    #[test]
    fn base_command_token_rejects_unsafe_token_shapes() {
        assert_eq!(base_command_token(b"-rf /tmp"), None);
        assert_eq!(base_command_token(b"./script"), None);
        assert_eq!(base_command_token(b"123"), None);
        assert_eq!(base_command_token(b"foo.bar"), None);
    }

    #[test]
    fn base_command_token_accepts_bracket_alias_only_as_symbolic_token() {
        assert_eq!(base_command_token(b"[ -f foo ]"), Some(&b"["[..]));
        assert_eq!(base_command_token(b"] -f foo"), None);
    }

    #[test]
    fn empty_input_returns_null_or_original_slice() {
        assert_eq!(base_command_token(b" \t "), None);
        assert_eq!(analysis_command_tail(b" \t "), &b" \t "[..]);
    }

    #[test]
    fn multiline_first_word_fails_closed_while_analysis_stays_anchored() {
        assert_eq!(base_command_token(b"grep foo\nrm bar"), None);
        assert_eq!(
            analysis_command_tail(b"grep foo\nrm bar"),
            &b"grep foo\nrm bar"[..]
        );
    }

    #[test]
    fn terminal_segment_returns_command_without_operators() {
        assert_eq!(
            terminal_exit_segment(b"grep foo bar.txt"),
            Some(&b"grep foo bar.txt"[..])
        );
    }

    #[test]
    fn terminal_segment_returns_last_pipe_segment() {
        assert_eq!(
            terminal_exit_segment(b"diff a b | grep changed"),
            Some(&b"grep changed"[..])
        );
    }

    #[test]
    fn terminal_segment_returns_last_sequence_segment() {
        assert_eq!(
            terminal_exit_segment(b"ls -la; grep foo bar.txt"),
            Some(&b"grep foo bar.txt"[..])
        );
    }

    #[test]
    fn terminal_segment_returns_last_newline_segment() {
        assert_eq!(
            terminal_exit_segment(b"ls -la\ngrep foo bar.txt"),
            Some(&b"grep foo bar.txt"[..])
        );
    }

    #[test]
    fn terminal_segment_handles_mixed_pipes_and_sequences() {
        assert_eq!(
            terminal_exit_segment(b"printf a | grep b; grep c"),
            Some(&b"grep c"[..])
        );
    }

    #[test]
    fn terminal_segment_declines_short_circuit_and_background_operators() {
        assert_eq!(terminal_exit_segment(b"grep foo bar && echo done"), None);
        assert_eq!(terminal_exit_segment(b"true & grep foo bar"), None);
    }

    #[test]
    fn terminal_segment_ignores_quoted_pipe_and_rejects_unbalanced_quote() {
        assert_eq!(
            terminal_exit_segment(b"grep 'foo|bar' baz"),
            Some(&b"grep 'foo|bar' baz"[..])
        );
        assert_eq!(terminal_exit_segment(b"grep 'foo bar"), None);
    }
}
