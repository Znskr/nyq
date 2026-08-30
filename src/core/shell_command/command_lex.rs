//! Conservative shell-token lexer for captured command analysis.
//!
//! The lexer deliberately recognizes only the small set of quoting, escaping,
//! redirection, and control-operator productions that command policy needs.
//! Everything operates on raw bytes so invalid UTF-8 and embedded NUL bytes can
//! be reported as lexing errors, exactly like the Zig reference.

const WHITESPACE: &[u8] = b" \t\r\n";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LexError {
    EmbeddedNul,
    InvalidUtf8,
    MalformedEscape,
    UnbalancedQuote,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PipeSegment<'a> {
    pub text: &'a [u8],
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArgvToken<'a> {
    pub raw: &'a [u8],
    pub value: Vec<u8>,
    pub start: usize,
    pub end: usize,
    pub quoted: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Argv<'a> {
    pub tokens: Vec<ArgvToken<'a>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedirectionKind {
    ReadPath,
    WritePath,
    ReadWritePath,
    Unsupported,
}

#[derive(Debug, Default)]
struct ShellScan {
    in_single: bool,
    in_double: bool,
    escaped: bool,
}

impl ShellScan {
    /// Returns the byte at `index` when it is an unquoted, unescaped byte that
    /// should participate in top-level scanning. Quote and escape state is
    /// tracked inside the scanner.
    fn unquoted(&mut self, input: &[u8], index: usize) -> Result<Option<u8>, LexError> {
        let ch = input[index];
        if self.escaped {
            self.escaped = false;
            return Ok(None);
        }
        if ch == b'\\' && !self.in_single {
            if index + 1 >= input.len() {
                return Err(LexError::MalformedEscape);
            }
            self.escaped = true;
            return Ok(None);
        }
        if ch == b'\'' && !self.in_double {
            self.in_single = !self.in_single;
            return Ok(None);
        }
        if ch == b'"' && !self.in_single {
            self.in_double = !self.in_double;
            return Ok(None);
        }
        if self.in_single || self.in_double {
            return Ok(None);
        }
        Ok(Some(ch))
    }

    fn finish(&self) -> Result<(), LexError> {
        if self.escaped {
            return Err(LexError::MalformedEscape);
        }
        if self.in_single || self.in_double {
            return Err(LexError::UnbalancedQuote);
        }
        Ok(())
    }
}

pub fn pipe_segments(command: &[u8]) -> Result<Vec<PipeSegment<'_>>, LexError> {
    check_input(command)?;

    let mut segments: Vec<PipeSegment> = Vec::new();
    let mut scan = ShellScan::default();
    let mut segment_start: usize = 0;
    let mut i: usize = 0;
    while i < command.len() {
        let ch = match scan.unquoted(command, i)? {
            None => {
                i += 1;
                continue;
            }
            Some(ch) => ch,
        };

        if ch == b'|' {
            if i > 0 && command[i - 1] == b'>' {
                i += 1;
                continue;
            }
            if i + 1 < command.len() && command[i + 1] == b'|' {
                i += 2;
                continue;
            }
            segments.push(trim_segment(command, segment_start, i));
            segment_start = i + 1;
        }
        i += 1;
    }

    scan.finish()?;
    segments.push(trim_segment(command, segment_start, command.len()));
    Ok(segments)
}

fn trim_segment(command: &[u8], raw_start: usize, raw_end: usize) -> PipeSegment<'_> {
    let mut start = raw_start;
    let mut end = raw_end;
    while start < end && is_whitespace(command[start]) {
        start += 1;
    }
    while end > start && is_whitespace(command[end - 1]) {
        end -= 1;
    }
    PipeSegment {
        text: &command[start..end],
        start,
        end,
    }
}

/// Conservative shell-shape detector for syntax that needs a real shell parser.
/// It recognizes common unquoted compound/subshell ASCII metacharacters only;
/// it intentionally does not model every POSIX shell grammar production.
pub fn unsafe_compound_indicator(command: &[u8]) -> bool {
    if check_input(command).is_err() {
        return true;
    }

    let mut scan = ShellScan::default();
    let mut i: usize = 0;
    while i < command.len() {
        let ch = match scan.unquoted(command, i) {
            Ok(None) => {
                i += 1;
                continue;
            }
            Ok(Some(ch)) => ch,
            Err(_) => return true,
        };

        if ch == b'$' && i + 1 < command.len() && command[i + 1] == b'(' {
            return true;
        }
        if ch == b'`' {
            return true;
        }
        if ch == b'(' || ch == b')' || ch == b'{' || ch == b'}' {
            return true;
        }
        if ch == b';' || ch == b'\n' {
            return true;
        }
        if ch == b'&' {
            if i + 1 < command.len() && command[i + 1] == b'>' {
                i += 1;
                continue;
            }
            if i > 0 && command[i - 1] == b'>' {
                i += 1;
                continue;
            }
            return true;
        }
        if ch == b'|' && i + 1 < command.len() && command[i + 1] == b'|' {
            return true;
        }
        i += 1;
    }

    if scan.finish().is_err() {
        return true;
    }
    false
}

pub fn tokenize_argv(command: &[u8]) -> Result<Argv<'_>, LexError> {
    check_input(command)?;

    let mut tokens: Vec<ArgvToken> = Vec::new();
    let mut value: Vec<u8> = Vec::new();

    let mut token_start: usize = 0;
    let mut token_active = false;
    let mut token_quoted = false;
    let mut in_single = false;
    let mut in_double = false;

    let mut i: usize = 0;
    while i < command.len() {
        let ch = command[i];

        if !in_single && !in_double {
            if is_whitespace(ch) {
                if token_active {
                    finish_token(&mut tokens, &mut value, command, token_start, i, token_quoted);
                }
                token_active = false;
                token_quoted = false;
                i += 1;
                continue;
            }

            if let Some(op_len) = redirection_operator_len(command, i) {
                if token_active {
                    finish_token(&mut tokens, &mut value, command, token_start, i, token_quoted);
                }
                append_operator_token(&mut tokens, command, i, i + op_len);
                token_active = false;
                token_quoted = false;
                i += op_len;
                continue;
            }

            if let Some(op_len) = control_operator_len(command, i) {
                if token_active {
                    finish_token(&mut tokens, &mut value, command, token_start, i, token_quoted);
                }
                append_operator_token(&mut tokens, command, i, i + op_len);
                token_active = false;
                token_quoted = false;
                i += op_len;
                continue;
            }
        }

        if !token_active {
            token_start = i;
            token_active = true;
        }

        if ch == b'\'' && !in_double {
            in_single = !in_single;
            token_quoted = true;
            i += 1;
            continue;
        }
        if ch == b'"' && !in_single {
            in_double = !in_double;
            token_quoted = true;
            i += 1;
            continue;
        }
        if ch == b'\\' && !in_single {
            if i + 1 >= command.len() {
                return Err(LexError::MalformedEscape);
            }
            value.push(command[i + 1]);
            i += 2;
            continue;
        }

        value.push(ch);
        i += 1;
    }

    if in_single || in_double {
        return Err(LexError::UnbalancedQuote);
    }
    if token_active {
        finish_token(&mut tokens, &mut value, command, token_start, command.len(), token_quoted);
    }
    Ok(Argv { tokens })
}

fn finish_token<'a>(
    tokens: &mut Vec<ArgvToken<'a>>,
    value: &mut Vec<u8>,
    command: &'a [u8],
    start: usize,
    end: usize,
    quoted: bool,
) {
    let owned = value.clone();
    value.clear();
    tokens.push(ArgvToken {
        raw: &command[start..end],
        value: owned,
        start,
        end,
        quoted,
    });
}

fn append_operator_token<'a>(
    tokens: &mut Vec<ArgvToken<'a>>,
    command: &'a [u8],
    start: usize,
    end: usize,
) {
    tokens.push(ArgvToken {
        raw: &command[start..end],
        value: command[start..end].to_vec(),
        start,
        end,
        quoted: false,
    });
}

fn redirection_operator_len(input: &[u8], start: usize) -> Option<usize> {
    let ch = input[start];
    if ch == b'&' && start + 1 < input.len() && input[start + 1] == b'>' {
        if start + 2 < input.len() && input[start + 2] == b'>' {
            return Some(3);
        }
        return Some(2);
    }
    if ch == b'<' {
        if start + 2 < input.len() && input[start + 1] == b'<' && input[start + 2] == b'<' {
            return Some(3);
        }
        if start + 1 < input.len()
            && (input[start + 1] == b'<' || input[start + 1] == b'&' || input[start + 1] == b'>')
        {
            return Some(2);
        }
        return Some(1);
    }
    if ch == b'>' {
        if start + 1 < input.len()
            && (input[start + 1] == b'>' || input[start + 1] == b'|' || input[start + 1] == b'&')
        {
            return Some(2);
        }
        return Some(1);
    }
    None
}

pub fn redirection_kind(value: &[u8]) -> Option<RedirectionKind> {
    if value == b"<" {
        return Some(RedirectionKind::ReadPath);
    }
    if value == b"<>" {
        return Some(RedirectionKind::ReadWritePath);
    }
    if value == b"<<" || value == b"<<<" || value == b"<&" {
        return Some(RedirectionKind::Unsupported);
    }
    if value == b">"
        || value == b">>"
        || value == b">|"
        || value == b"&>"
        || value == b"&>>"
        || value == b">&"
    {
        return Some(RedirectionKind::WritePath);
    }
    None
}

fn control_operator_len(input: &[u8], start: usize) -> Option<usize> {
    match input[start] {
        b';' | b'(' | b')' | b'{' | b'}' => Some(1),
        b'|' => {
            if start + 1 < input.len() && input[start + 1] == b'|' {
                Some(2)
            } else {
                Some(1)
            }
        }
        b'&' => {
            if start + 1 < input.len() && input[start + 1] == b'&' {
                Some(2)
            } else {
                Some(1)
            }
        }
        _ => None,
    }
}

/// Peels a leading chain of safe environment assignments and process wrappers
/// off an argv. Returns `None` when the wrapper chain is ambiguous.
pub fn wrapper_strip_argv<'a, 'b>(argv: &'a [ArgvToken<'b>]) -> Option<&'a [ArgvToken<'b>]> {
    let mut rest = argv;
    while !rest.is_empty() {
        if is_safe_env_assignment(&rest[0].value) {
            rest = &rest[1..];
            continue;
        }

        let command = rest[0].value.as_slice();
        if eq(command, b"env") {
            rest = strip_env(rest)?;
            continue;
        }
        if eq(command, b"nice") {
            rest = strip_nice(rest)?;
            continue;
        }
        if eq(command, b"stdbuf") {
            rest = strip_stdbuf(rest)?;
            continue;
        }
        if eq(command, b"unbuffer") || eq(command, b"nohup") {
            if rest.len() < 2 {
                return None;
            }
            rest = &rest[1..];
            continue;
        }
        if eq(command, b"time") {
            rest = strip_time(rest)?;
            continue;
        }
        if eq(command, b"timeout") {
            rest = strip_timeout(rest)?;
            continue;
        }
        return Some(rest);
    }
    Some(rest)
}

fn strip_env<'a, 'b>(argv: &'a [ArgvToken<'b>]) -> Option<&'a [ArgvToken<'b>]> {
    let mut index: usize = 1;
    let mut saw_assignment = false;
    while index < argv.len() && is_safe_env_assignment(&argv[index].value) {
        saw_assignment = true;
        index += 1;
    }
    if !saw_assignment || index >= argv.len() {
        return None;
    }
    Some(&argv[index..])
}

fn strip_nice<'a, 'b>(argv: &'a [ArgvToken<'b>]) -> Option<&'a [ArgvToken<'b>]> {
    let mut index: usize = 1;
    if index < argv.len() {
        let arg = argv[index].value.as_slice();
        if eq(arg, b"-n") || eq(arg, b"--adjustment") {
            if index + 1 >= argv.len() || !is_signed_integer(&argv[index + 1].value) {
                return None;
            }
            index += 2;
        } else if arg.starts_with(b"--adjustment=") {
            if !is_signed_integer(&arg[b"--adjustment=".len()..]) {
                return None;
            }
            index += 1;
        } else if arg.len() > 1 && arg[0] == b'-' && is_signed_integer(&arg[1..]) {
            index += 1;
        }
    }
    if index >= argv.len() {
        return None;
    }
    Some(&argv[index..])
}

fn strip_stdbuf<'a, 'b>(argv: &'a [ArgvToken<'b>]) -> Option<&'a [ArgvToken<'b>]> {
    let mut index: usize = 1;
    let mut saw_option = false;
    while index < argv.len() {
        let arg = argv[index].value.as_slice();
        if !arg.starts_with(b"-") {
            break;
        }
        saw_option = true;
        if eq(arg, b"-i")
            || eq(arg, b"-o")
            || eq(arg, b"-e")
            || eq(arg, b"--input")
            || eq(arg, b"--output")
            || eq(arg, b"--error")
        {
            if index + 1 >= argv.len() || has_shell_metachar(argv[index + 1].raw) {
                return None;
            }
            index += 2;
            continue;
        }
        if arg.starts_with(b"-i")
            || arg.starts_with(b"-o")
            || arg.starts_with(b"-e")
            || arg.starts_with(b"--input=")
            || arg.starts_with(b"--output=")
            || arg.starts_with(b"--error=")
        {
            if has_shell_metachar(argv[index].raw) {
                return None;
            }
            index += 1;
            continue;
        }
        return None;
    }
    if !saw_option || index >= argv.len() {
        return None;
    }
    Some(&argv[index..])
}

fn strip_time<'a, 'b>(argv: &'a [ArgvToken<'b>]) -> Option<&'a [ArgvToken<'b>]> {
    let mut index: usize = 1;
    while index < argv.len() && eq(argv[index].value.as_slice(), b"-p") {
        index += 1;
    }
    if index >= argv.len() {
        return None;
    }
    Some(&argv[index..])
}

fn strip_timeout<'a, 'b>(argv: &'a [ArgvToken<'b>]) -> Option<&'a [ArgvToken<'b>]> {
    let mut index: usize = 1;
    while index < argv.len() {
        let arg = argv[index].value.as_slice();
        if !arg.starts_with(b"-") || eq(arg, b"-") {
            break;
        }

        if eq(arg, b"--foreground") || eq(arg, b"--preserve-status") || eq(arg, b"--verbose") || eq(arg, b"-v") {
            index += 1;
            continue;
        }
        if eq(arg, b"-k") || eq(arg, b"--kill-after") {
            if index + 1 >= argv.len()
                || !is_duration(&argv[index + 1].value)
                || has_shell_metachar(argv[index + 1].raw)
            {
                return None;
            }
            index += 2;
            continue;
        }
        if eq(arg, b"-s") || eq(arg, b"--signal") {
            if index + 1 >= argv.len()
                || !is_signal(&argv[index + 1].value)
                || has_shell_metachar(argv[index + 1].raw)
            {
                return None;
            }
            index += 2;
            continue;
        }
        if arg.starts_with(b"--kill-after=") {
            if !is_duration(&arg[b"--kill-after=".len()..]) || has_shell_metachar(argv[index].raw) {
                return None;
            }
            index += 1;
            continue;
        }
        if arg.starts_with(b"--signal=") {
            if !is_signal(&arg[b"--signal=".len()..]) || has_shell_metachar(argv[index].raw) {
                return None;
            }
            index += 1;
            continue;
        }
        return None;
    }

    if index >= argv.len()
        || !is_duration(&argv[index].value)
        || has_shell_metachar(argv[index].raw)
    {
        return None;
    }
    index += 1;
    if index >= argv.len() {
        return None;
    }
    Some(&argv[index..])
}

pub fn is_safe_env_assignment(token: &[u8]) -> bool {
    let Some(eq) = token.iter().position(|&b| b == b'=') else {
        return false;
    };
    if eq == 0 {
        return false;
    }
    is_safe_env_name(&token[..eq])
}

const SAFE_ENV_NAMES: &[&[u8]] = &[
    b"CI",
    b"NODE_ENV",
    b"BUN_ENV",
    b"DENO_ENV",
    b"PYTHONUNBUFFERED",
    b"PYTHONWARNINGS",
    b"RUST_LOG",
    b"RUST_BACKTRACE",
    b"TERM",
    b"COLORTERM",
    b"NO_COLOR",
    b"FORCE_COLOR",
    b"CLICOLOR",
    b"CLICOLOR_FORCE",
    b"LANG",
    b"LC_ALL",
    b"LC_CTYPE",
    b"LC_MESSAGES",
    b"TZ",
    b"CC",
    b"CXX",
    b"AR",
    b"AS",
    b"LD",
    b"CFLAGS",
    b"CXXFLAGS",
    b"CPPFLAGS",
    b"LDFLAGS",
    b"MAKEFLAGS",
    b"CMAKE_BUILD_TYPE",
];

fn is_safe_env_name(name: &[u8]) -> bool {
    SAFE_ENV_NAMES.contains(&name)
}

fn check_input(input: &[u8]) -> Result<(), LexError> {
    if input.contains(&0) {
        return Err(LexError::EmbeddedNul);
    }
    if std::str::from_utf8(input).is_err() {
        return Err(LexError::InvalidUtf8);
    }
    Ok(())
}

fn is_whitespace(ch: u8) -> bool {
    WHITESPACE.contains(&ch)
}

fn is_signed_integer(value: &[u8]) -> bool {
    if value.is_empty() {
        return false;
    }
    let start: usize = if value[0] == b'+' || value[0] == b'-' { 1 } else { 0 };
    if start >= value.len() {
        return false;
    }
    value[start..].iter().all(|&ch| ch.is_ascii_digit())
}

fn is_duration(value: &[u8]) -> bool {
    if value.is_empty() {
        return false;
    }
    let mut saw_digit = false;
    let mut saw_dot = false;
    for (i, &ch) in value.iter().enumerate() {
        if ch.is_ascii_digit() {
            saw_digit = true;
            continue;
        }
        if ch == b'.' && !saw_dot && i + 1 < value.len() {
            saw_dot = true;
            continue;
        }
        if i + 1 == value.len() && (ch == b's' || ch == b'm' || ch == b'h' || ch == b'd') {
            return saw_digit;
        }
        return false;
    }
    saw_digit
}

fn is_signal(value: &[u8]) -> bool {
    if value.is_empty() {
        return false;
    }
    value.iter().all(|&ch| ch.is_ascii_alphanumeric())
}

fn has_shell_metachar(value: &[u8]) -> bool {
    value.iter().any(|&ch| {
        matches!(
            ch,
            b';' | b'&'
                | b'|'
                | b'('
                | b')'
                | b'{'
                | b'}'
                | b'<'
                | b'>'
                | b'$'
                | b'`'
                | b'\''
                | b'"'
        )
    })
}

fn eq(a: &[u8], b: &[u8]) -> bool {
    a == b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipe_segments_splits_unquoted_pipes() {
        let segments = pipe_segments(b"printf a | grep a | wc -l").unwrap();
        assert_eq!(segments.len(), 3);
        assert_eq!(segments[0].text, &b"printf a"[..]);
        assert_eq!(segments[1].text, &b"grep a"[..]);
        assert_eq!(segments[2].text, &b"wc -l"[..]);
    }

    #[test]
    fn pipe_segments_ignores_quoted_pipes() {
        let segments = pipe_segments(b"printf 'a|b' | grep \"c|d\"").unwrap();
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0].text, &b"printf 'a|b'"[..]);
        assert_eq!(segments[1].text, &b"grep \"c|d\""[..]);
    }

    #[test]
    fn pipe_segments_ignores_escaped_pipes() {
        let segments = pipe_segments(b"printf a\\|b | cat").unwrap();
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0].text, &b"printf a\\|b"[..]);
    }

    #[test]
    fn pipe_segments_does_not_split_logical_or() {
        let segments = pipe_segments(b"grep x file || true").unwrap();
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].text, &b"grep x file || true"[..]);
    }

    #[test]
    fn pipe_segments_keeps_overwrite_pipe_redirection_within_its_segment() {
        let segments = pipe_segments(b"printf x >| out | cat").unwrap();
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0].text, &b"printf x >| out"[..]);
        assert_eq!(segments[1].text, &b"cat"[..]);
    }

    #[test]
    fn pipe_segments_reports_unbalanced_quotes() {
        assert_eq!(
            pipe_segments(b"printf 'x | cat"),
            Err(LexError::UnbalancedQuote)
        );
    }

    #[test]
    fn pipe_segments_reports_malformed_trailing_escapes() {
        assert_eq!(pipe_segments(b"printf x\\"), Err(LexError::MalformedEscape));
    }

    #[test]
    fn pipe_segments_rejects_embedded_nul_and_invalid_utf8() {
        assert_eq!(pipe_segments(b"cat \x00 file"), Err(LexError::EmbeddedNul));
        assert_eq!(pipe_segments(b"cat \xff"), Err(LexError::InvalidUtf8));
    }

    #[test]
    fn tokenize_argv_decodes_single_and_double_quoted_tokens() {
        let argv = tokenize_argv(b"grep 'hello world' \"file name.txt\"").unwrap();
        assert_eq!(argv.tokens.len(), 3);
        assert_eq!(argv.tokens[1].value, &b"hello world"[..]);
        assert_eq!(argv.tokens[2].value, &b"file name.txt"[..]);
        assert!(argv.tokens[1].quoted);
    }

    #[test]
    fn tokenize_argv_preserves_raw_byte_offsets() {
        let argv = tokenize_argv(b"  cat 'a b'").unwrap();
        assert_eq!(argv.tokens[0].start, 2);
        assert_eq!(argv.tokens[0].end, 5);
        assert_eq!(argv.tokens[1].raw, &b"'a b'"[..]);
    }

    #[test]
    fn tokenize_argv_handles_escaped_spaces_without_splitting() {
        let argv = tokenize_argv(b"cat file\\ name.txt").unwrap();
        assert_eq!(argv.tokens.len(), 2);
        assert_eq!(argv.tokens[1].value, &b"file name.txt"[..]);
        assert_eq!(argv.tokens[1].raw, &b"file\\ name.txt"[..]);
    }

    #[test]
    fn tokenize_argv_emits_redirection_operators_as_tokens() {
        let argv = tokenize_argv(
            b"printf ok>out.txt &>> log.txt >&err.txt < in.txt <> state.txt << EOF <<< value <& 0",
        )
        .unwrap();

        assert_eq!(argv.tokens[2].value, &b">"[..]);
        assert_eq!(argv.tokens[4].value, &b"&>>"[..]);
        assert_eq!(argv.tokens[6].value, &b">&"[..]);
        assert_eq!(argv.tokens[7].value, &b"err.txt"[..]);
        assert_eq!(argv.tokens[8].value, &b"<"[..]);
        assert_eq!(argv.tokens[10].value, &b"<>"[..]);
        assert_eq!(argv.tokens[12].value, &b"<<"[..]);
        assert_eq!(argv.tokens[14].value, &b"<<<"[..]);
        assert_eq!(argv.tokens[16].value, &b"<&"[..]);
    }

    #[test]
    fn redirection_kind_classifies_shell_redirects() {
        assert_eq!(redirection_kind(b"<"), Some(RedirectionKind::ReadPath));
        assert_eq!(
            redirection_kind(b"<>"),
            Some(RedirectionKind::ReadWritePath)
        );
        assert_eq!(redirection_kind(b">"), Some(RedirectionKind::WritePath));
        assert_eq!(redirection_kind(b"&>>"), Some(RedirectionKind::WritePath));
        assert_eq!(redirection_kind(b"<<"), Some(RedirectionKind::Unsupported));
        assert_eq!(redirection_kind(b"<<<"), Some(RedirectionKind::Unsupported));
        assert_eq!(redirection_kind(b"<&"), Some(RedirectionKind::Unsupported));
        assert_eq!(redirection_kind(b"|"), None);
    }

    #[test]
    fn tokenize_argv_distinguishes_empty_argv_from_tokenization_failure() {
        let argv = tokenize_argv(b"   \t ").unwrap();
        assert_eq!(argv.tokens.len(), 0);

        assert_eq!(
            tokenize_argv(b"\"unterminated"),
            Err(LexError::UnbalancedQuote)
        );
    }

    #[test]
    fn tokenize_argv_reports_malformed_trailing_escapes() {
        assert_eq!(tokenize_argv(b"printf x\\"), Err(LexError::MalformedEscape));
    }

    #[test]
    fn tokenize_argv_rejects_embedded_nul_and_invalid_utf8() {
        assert_eq!(
            tokenize_argv(b"cat \x00 file"),
            Err(LexError::EmbeddedNul)
        );
        assert_eq!(tokenize_argv(b"cat \xff"), Err(LexError::InvalidUtf8));
    }

    #[test]
    fn unsafe_compound_indicator_detects_subshells_and_groups() {
        assert!(unsafe_compound_indicator(b"echo $(pwd)"));
        assert!(unsafe_compound_indicator(b"(cd src; make)"));
        assert!(unsafe_compound_indicator(b"{ echo hi; }"));
        assert!(!unsafe_compound_indicator(b"grep '(' file"));
    }

    #[test]
    fn unsafe_compound_indicator_detects_logical_control_syntax() {
        assert!(unsafe_compound_indicator(b"cd src && touch x"));
        assert!(unsafe_compound_indicator(b"grep x file || true"));
        assert!(unsafe_compound_indicator(b"sleep 1 &"));
        assert!(!unsafe_compound_indicator(b"printf a | grep a"));
    }

    #[test]
    fn unsafe_compound_indicator_ignores_escaped_and_quoted_compound_characters() {
        assert!(!unsafe_compound_indicator(b"printf \\& \\| \\;"));
        assert!(!unsafe_compound_indicator(b"printf \"$(literal)\""));
    }

    #[test]
    fn wrapper_strip_argv_strips_safe_env_and_process_wrappers() {
        let argv =
            tokenize_argv(b"env NODE_ENV=prod nice -n 5 timeout --foreground 10s grep x file")
                .unwrap();

        let stripped = wrapper_strip_argv(&argv.tokens).unwrap();
        assert_eq!(stripped[0].value, &b"grep"[..]);
        assert_eq!(stripped[2].value, &b"file"[..]);
    }

    #[test]
    fn wrapper_strip_argv_declines_ambiguous_wrappers() {
        let env_argv = tokenize_argv(b"env -i grep x").unwrap();
        assert_eq!(wrapper_strip_argv(&env_argv.tokens), None);

        let timeout_argv = tokenize_argv(b"timeout --kill-after '1;rm' 10s grep x").unwrap();
        assert_eq!(wrapper_strip_argv(&timeout_argv.tokens), None);
    }
}
