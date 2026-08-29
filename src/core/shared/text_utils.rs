use super::display_width;

pub fn contains_ignore_case(haystack: &[u8], needle: &[u8]) -> bool {
    needle.is_empty()
        || (needle.len() <= haystack.len()
            && haystack
                .windows(needle.len())
                .any(|w| w.eq_ignore_ascii_case(needle)))
}

pub fn is_model_safe_text(text: &[u8]) -> bool {
    !text.contains(&0) && std::str::from_utf8(text).is_ok()
}

pub fn utf8_backward_boundary(text: &[u8], index: usize) -> usize {
    let mut n = index.min(text.len());
    while n > 0 && n < text.len() && text[n] & 0xc0 == 0x80 {
        n -= 1;
    }
    n
}

pub fn utf8_forward_boundary(text: &[u8], index: usize) -> usize {
    let mut n = index.min(text.len());
    while n < text.len() && text[n] & 0xc0 == 0x80 {
        n += 1;
    }
    n
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HeadRounding {
    Down,
    Up,
}

pub fn write_head_tail_bounded<W: std::io::Write>(
    writer: &mut W,
    text: &[u8],
    max: usize,
    marker: &[u8],
    rounding: HeadRounding,
) -> std::io::Result<()> {
    if text.len() <= max {
        return writer.write_all(text);
    }
    if max <= marker.len() {
        return writer.write_all(&marker[..max]);
    }
    let retained = max - marker.len();
    let head = match rounding {
        HeadRounding::Down => retained / 2,
        HeadRounding::Up => retained.div_ceil(2),
    };
    let tail = retained - head;
    writer.write_all(&text[..utf8_backward_boundary(text, head)])?;
    writer.write_all(marker)?;
    writer.write_all(&text[utf8_forward_boundary(text, text.len() - tail)..])
}

pub fn utf8_prefix_by_bytes(text: &[u8], max: usize) -> &[u8] {
    &text[..utf8_backward_boundary(text, max)]
}

pub fn normalize_line_endings_in_place(bytes: &mut [u8]) -> &mut [u8] {
    let (mut read, mut write) = (0, 0);
    while read < bytes.len() {
        if bytes[read] == b'\r' {
            bytes[write] = b'\n';
            write += 1;
            read += 1;
            if read < bytes.len() && bytes[read] == b'\n' {
                read += 1;
            }
        } else {
            bytes[write] = bytes[read];
            write += 1;
            read += 1;
        }
    }
    &mut bytes[..write]
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EncodedText {
    pub bytes: Vec<u8>,
    pub truncated: bool,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum PathTailError {
    PathBasenameTooLong,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EncodedPathTail {
    pub bytes: Vec<u8>,
    pub basename_start: usize,
    pub source_truncated: bool,
}

fn token(raw: &[u8], index: usize) -> (usize, Vec<u8>) {
    let b = raw[index];
    if b < 0x20 || b == 0x7f {
        return (1, format!("\\x{b:02x}").into_bytes());
    }
    if b < 0x80 {
        return (1, vec![b]);
    }
    let width = match b {
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => 1,
    };
    if index + width > raw.len() {
        return (1, format!("\\x{b:02x}").into_bytes());
    }
    let sequence = &raw[index..index + width];
    let Ok(value) = std::str::from_utf8(sequence) else {
        return (1, format!("\\x{b:02x}").into_bytes());
    };
    let cp = value.chars().next().map(u32::from).unwrap_or(0);
    if (0x80..=0x9f).contains(&cp)
        || (0x200b..=0x200f).contains(&cp)
        || (0x2028..=0x202e).contains(&cp)
        || (0x2060..=0x206f).contains(&cp)
        || cp == 0xfeff
    {
        return (width, format!("\\u{{{cp:04x}}}").into_bytes());
    }
    (width, sequence.to_vec())
}

fn encoded_token_len(encoded: &[u8]) -> usize {
    if encoded.len() >= 4
        && encoded[0] == b'\\'
        && encoded[1] == b'x'
        && encoded[2..4].iter().all(u8::is_ascii_hexdigit)
    {
        return 4;
    }
    if encoded.starts_with(b"\\u{") {
        if let Some(end) = encoded.iter().take(12).position(|&b| b == b'}') {
            if end > 3 && encoded[3..end].iter().all(u8::is_ascii_hexdigit) {
                return end + 1;
            }
        }
    }
    let b = encoded[0];
    match b {
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => 1,
    }
    .min(encoded.len())
}

pub fn encode_terminal_safe(raw: &[u8], max: usize) -> EncodedText {
    let marker = b"...";
    let marker_len = marker.len().min(max);
    let content_limit = max - marker_len;
    let (mut bytes, mut boundary, mut i, mut truncated) = (Vec::new(), 0, 0, false);
    while i < raw.len() {
        let (used, encoded) = token(raw, i);
        if bytes
            .len()
            .checked_add(encoded.len())
            .is_none_or(|n| n > max)
        {
            truncated = true;
            break;
        }
        bytes.extend_from_slice(&encoded);
        i += used;
        if bytes.len() <= content_limit {
            boundary = bytes.len();
        }
    }
    if truncated {
        bytes.truncate(boundary);
        bytes.extend_from_slice(&marker[..marker_len]);
    }
    EncodedText { bytes, truncated }
}

pub fn is_terminal_safe(raw: &[u8]) -> bool {
    encode_terminal_safe(raw, usize::MAX).bytes == raw
}

pub fn terminal_safe_visible_width(raw: &[u8]) -> usize {
    display_width::visible_width(&encode_terminal_safe(raw, usize::MAX).bytes)
}

pub fn encode_terminal_safe_path_tail(
    raw: &[u8],
    max: usize,
) -> Result<EncodedPathTail, PathTailError> {
    let basename_start_source = raw
        .iter()
        .rposition(|&b| b == b'/' || b == b'\\')
        .map_or(0, |n| n + 1);
    if basename_start_source == raw.len() {
        return Err(PathTailError::PathBasenameTooLong);
    }
    let mut encoded = Vec::new();
    let mut boundaries = vec![0];
    let mut source = 0;
    let mut basename_start = None;
    while source < raw.len() {
        if source == basename_start_source {
            basename_start = Some(encoded.len());
        }
        let (used, value) = token(raw, source);
        encoded.extend_from_slice(&value);
        source += used;
        boundaries.push(encoded.len());
    }
    let basename_start = basename_start.unwrap_or(0);
    let basename_len = encoded.len() - basename_start;
    if basename_len > max {
        return Err(PathTailError::PathBasenameTooLong);
    }
    if encoded.len() <= max {
        return Ok(EncodedPathTail {
            bytes: encoded,
            basename_start,
            source_truncated: false,
        });
    }
    let marker = "…".as_bytes();
    if marker.len() + basename_len > max {
        return Err(PathTailError::PathBasenameTooLong);
    }
    let budget = max - marker.len();
    let start = boundaries
        .into_iter()
        .find(|&b| encoded.len() - b <= budget)
        .ok_or(PathTailError::PathBasenameTooLong)?;
    if start > basename_start {
        return Err(PathTailError::PathBasenameTooLong);
    }
    let mut bytes = marker.to_vec();
    bytes.extend_from_slice(&encoded[start..]);
    Ok(EncodedPathTail {
        bytes,
        basename_start: marker.len() + basename_start - start,
        source_truncated: true,
    })
}

pub fn suffix_terminal_safe_by_width(encoded: &[u8], max: usize) -> &[u8] {
    let mut width = display_width::visible_width(encoded);
    let mut start = 0;
    while start < encoded.len() && width > max {
        let n = encoded_token_len(&encoded[start..]);
        width = width.saturating_sub(display_width::visible_width(&encoded[start..start + n]));
        start += n;
    }
    &encoded[start..]
}

pub fn prefix_terminal_safe_by_width(encoded: &[u8], max: usize) -> &[u8] {
    let (mut width, mut end) = (0, 0);
    while end < encoded.len() {
        let n = encoded_token_len(&encoded[end..]);
        let w = display_width::visible_width(&encoded[end..end + n]);
        if width + w > max {
            break;
        }
        width += w;
        end += n;
    }
    &encoded[..end]
}

pub fn clipped_label<'a>(buf: &'a mut [u8], text: &'a [u8], max: usize) -> &'a [u8] {
    if buf.is_empty() || max == 0 {
        return &[];
    }
    if display_width::visible_width(text) <= max {
        return text;
    }
    let prefix = display_width::prefix_by_width(text, max.saturating_sub(3));
    let keep = prefix.len().min(buf.len());
    if keep == 0 {
        return display_width::prefix_by_width(text, max);
    }
    buf[..keep].copy_from_slice(&prefix[..keep]);
    if keep + 3 <= buf.len() {
        buf[keep..keep + 3].copy_from_slice(b"...");
        &buf[..keep + 3]
    } else {
        &buf[..keep]
    }
}

pub fn sanitize_model_text(text: &[u8]) -> Vec<u8> {
    if is_model_safe_text(text) {
        text.to_vec()
    } else {
        format!(
            "binary or non-utf8 tool output omitted ({} bytes)",
            text.len()
        )
        .into_bytes()
    }
}

fn sensitive_key(key: &[u8]) -> bool {
    [
        b"password".as_slice(),
        b"passwd",
        b"api_key",
        b"apikey",
        b"secret",
        b"token",
        b"private_key",
        b"access_key",
    ]
    .iter()
    .any(|n| contains_ignore_case(key, n))
}
fn token_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.')
}
fn key_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

pub fn mask_secrets(text: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        if text[i..].starts_with(b"https://") {
            let auth = i + 8;
            let mut end = auth;
            while end < text.len()
                && !text[end].is_ascii_whitespace()
                && !matches!(text[end], b'/' | b'?' | b'#')
            {
                end += 1;
            }
            if let Some(at) = text[auth..end].iter().position(|&b| b == b'@') {
                if text[auth..auth + at].contains(&b':') {
                    out.extend_from_slice(b"https://[redacted]@");
                    out.extend_from_slice(&text[auth + at + 1..end]);
                    i = end;
                    continue;
                }
            }
        }
        if (i == 0 || !key_char(text[i - 1])) && key_char(text[i]) {
            let mut eq = i;
            while eq < text.len() && key_char(text[eq]) {
                eq += 1;
            }
            if eq < text.len() && text[eq] == b'=' && sensitive_key(&text[i..eq]) {
                out.extend_from_slice(&text[i..=eq]);
                let mut value = eq + 1;
                let quote = text
                    .get(value)
                    .copied()
                    .filter(|b| *b == b'\'' || *b == b'"');
                if quote.is_some() {
                    out.push(quote.unwrap());
                    value += 1;
                }
                let mut end = value;
                while end < text.len()
                    && !matches!(text[end], b'\n' | b'\r')
                    && quote.map_or(
                        !text[end].is_ascii_whitespace() && !matches!(text[end], b'\'' | b'"'),
                        |q| text[end] != q,
                    )
                {
                    end += 1;
                }
                if end > value {
                    out.extend_from_slice(b"[redacted]");
                    i = end;
                    continue;
                }
            }
        }
        let inline = [
            b"sk-".as_slice(),
            b"sk_live_",
            b"pk_live_",
            b"github_pat_",
            b"xoxb-",
            b"xoxp-",
            b"Bearer ",
        ];
        let mut matched = None;
        if i == 0 || !token_char(text[i - 1]) {
            for p in inline {
                if text[i..].starts_with(p) {
                    let mut e = i + p.len();
                    while e < text.len() && token_char(text[e]) {
                        e += 1;
                    }
                    if e - i >= 16 {
                        matched = Some(e);
                        break;
                    }
                }
            }
        }
        if matched.is_none()
            && i + 20 <= text.len()
            && [b"AKIA", b"ASIA", b"AIDA", b"AGPA", b"AROA", b"ANPA"]
                .iter()
                .any(|p| text[i..].starts_with(*p))
            && text[i..i + 20]
                .iter()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
        {
            matched = Some(i + 20);
        }
        if matched.is_none()
            && i + 40 <= text.len()
            && [b"ghp_", b"gho_", b"ghu_", b"ghs_", b"ghr_"]
                .iter()
                .any(|p| text[i..].starts_with(*p))
        {
            let mut e = i + 4;
            while e < text.len() && token_char(text[e]) {
                e += 1;
            }
            if e - i >= 40 {
                matched = Some(e);
            }
        }
        if let Some(end) = matched {
            out.extend_from_slice(b"[redacted]");
            i = end;
        } else {
            out.push(text[i]);
            i += 1;
        }
    }
    out
}

pub fn secret_may_cross_boundary(text: &[u8], retained: usize) -> bool {
    let start = text.len().saturating_sub(retained);
    let mut k = text.len();
    while k > 0 && (key_char(text[k - 1]) || matches!(text[k - 1], b'=' | b'\'' | b'"')) {
        k -= 1;
    }
    if k < start {
        let end = text[k..]
            .iter()
            .position(|&b| matches!(b, b'=' | b'\'' | b'"'))
            .map_or(text.len(), |n| k + n);
        if sensitive_key(&text[k..end]) {
            return true;
        }
    }
    text.windows(8)
        .position(|w| w == b"https://")
        .is_some_and(|p| {
            p < start
                && !text[p + 8..]
                    .iter()
                    .any(|b| b.is_ascii_whitespace() || matches!(b, b'/' | b'?' | b'#' | b'@'))
        })
}

fn percent_decode(bytes: &[u8]) -> Vec<u8> {
    let mut o = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if i + 2 < bytes.len() && bytes[i] == b'%' {
            if let (Some(a), Some(b)) = (
                (bytes[i + 1] as char).to_digit(16),
                (bytes[i + 2] as char).to_digit(16),
            ) {
                o.push((a * 16 + b) as u8);
                i += 3;
                continue;
            }
        }
        o.push(bytes[i]);
        i += 1;
    }
    o
}
pub fn redact_url_for_display(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let query = raw.iter().position(|&b| b == b'?').unwrap_or(raw.len());
    let mut prefix = raw[..query].to_vec();
    if let Some(s) = prefix.windows(3).position(|w| w == b"://") {
        let a = s + 3;
        let end = prefix[a..]
            .iter()
            .position(|b| matches!(b, b'/' | b'?' | b'#'))
            .map_or(prefix.len(), |n| a + n);
        if let Some(at) = prefix[a..end].iter().position(|&b| b == b'@') {
            prefix.splice(a..a + at, b"[redacted]".iter().copied());
        }
    }
    out.extend(prefix);
    if query == raw.len() {
        return out;
    }
    out.push(b'?');
    let fragment = raw[query + 1..]
        .iter()
        .position(|&b| b == b'#')
        .map_or(raw.len(), |n| query + 1 + n);
    let mut pos = query + 1;
    while pos < fragment {
        let end = raw[pos..fragment]
            .iter()
            .position(|&b| b == b'&')
            .map_or(fragment, |n| pos + n);
        let eq = raw[pos..end]
            .iter()
            .position(|&b| b == b'=')
            .map_or(end, |n| pos + n);
        out.extend_from_slice(&raw[pos..eq]);
        if eq < end {
            out.push(b'=');
            let key = percent_decode(&raw[pos..eq]);
            if sensitive_key(&key)
                || key.eq_ignore_ascii_case(b"sig")
                || contains_ignore_case(&key, b"signature")
                || contains_ignore_case(&key, b"credential")
                || contains_ignore_case(&key, b"authorization")
                || contains_ignore_case(&key, b"cookie")
            {
                out.extend_from_slice(b"[redacted]")
            } else {
                out.extend_from_slice(&raw[eq + 1..end])
            }
        }
        if end < fragment {
            out.push(b'&')
        }
        pos = end + 1;
    }
    out.extend_from_slice(&raw[fragment..]);
    out
}

pub fn sanitize_assistant_text(text: &[u8]) -> &[u8] {
    let trimmed = trim_ascii(text);
    let Some(nl) = trimmed.iter().position(|&b| b == b'\n') else {
        return trimmed;
    };
    let first = trim_ascii(&trimmed[..nl]);
    if contains_ignore_case(first, b"i'm fx")
        || contains_ignore_case(first, b"i am fx")
        || contains_ignore_case(first, b"local coding assistant")
    {
        trim_ascii(&trimmed[nl + 1..])
    } else {
        trimmed
    }
}
fn trim_ascii(mut s: &[u8]) -> &[u8] {
    while s.first().is_some_and(u8::is_ascii_whitespace) {
        s = &s[1..]
    }
    while s.last().is_some_and(u8::is_ascii_whitespace) {
        s = &s[..s.len() - 1]
    }
    s
}

#[derive(Default)]
pub struct IncrementalTerminalSafeEncoder {
    pending: Vec<u8>,
}
impl IncrementalTerminalSafeEncoder {
    pub fn append<W: std::io::Write>(&mut self, w: &mut W, raw: &[u8]) -> std::io::Result<()> {
        self.pending.extend_from_slice(raw);
        let keep = incomplete_utf8_tail(&self.pending);
        let end = self.pending.len() - keep;
        let encoded = encode_terminal_safe(&self.pending[..end], usize::MAX);
        w.write_all(&encoded.bytes)?;
        self.pending.drain(..end);
        Ok(())
    }
    pub fn finish<W: std::io::Write>(&mut self, w: &mut W) -> std::io::Result<()> {
        let encoded = encode_terminal_safe(&self.pending, usize::MAX);
        w.write_all(&encoded.bytes)?;
        self.pending.clear();
        Ok(())
    }
}
fn incomplete_utf8_tail(s: &[u8]) -> usize {
    if s.is_empty() {
        return 0;
    }
    let mut i = s.len() - 1;
    while i > 0 && s[i] & 0xc0 == 0x80 {
        i -= 1
    }
    let expected = match s[i] {
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => 1,
    };
    if s.len() - i < expected {
        s.len() - i
    } else {
        0
    }
}

#[derive(Default)]
pub struct IncrementalUtf8Validator {
    pending: Vec<u8>,
}
impl IncrementalUtf8Validator {
    pub fn append(&mut self, b: &[u8]) -> Result<(), std::str::Utf8Error> {
        self.pending.extend_from_slice(b);
        match std::str::from_utf8(&self.pending) {
            Ok(_) => {
                self.pending.clear();
                Ok(())
            }
            Err(e) if e.error_len().is_none() => {
                let valid = e.valid_up_to();
                self.pending.drain(..valid);
                Ok(())
            }
            Err(e) => Err(e),
        }
    }
    pub fn finish(&self) -> Result<(), std::str::Utf8Error> {
        std::str::from_utf8(&self.pending).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn basics() {
        assert!(contains_ignore_case(b"Local Coding Assistant", b"coding"));
        assert!(!is_model_safe_text(b"bad\xff"));
        assert_eq!(utf8_prefix_by_bytes("abéz".as_bytes(), 3), b"ab");
    }
    #[test]
    fn terminal() {
        let e = encode_terminal_safe(b"A\x1b\n\xff", 128);
        assert_eq!(e.bytes, b"A\\x1b\\x0a\\xff");
        assert!(!e.truncated);
        assert_eq!(encode_terminal_safe(b"abcdef", 5).bytes, b"ab...");
    }
    #[test]
    fn secrets() {
        assert_eq!(
            mask_secrets(b"AI_GATEWAY_API_KEY=abcdefghijklmnop end"),
            b"AI_GATEWAY_API_KEY=[redacted] end"
        );
        assert_eq!(
            redact_url_for_display(b"https://u:p@example.com/x?safe=ok&token=x"),
            b"https://[redacted]@example.com/x?safe=ok&token=[redacted]"
        );
    }
    #[test]
    fn lines() {
        let mut b = b"a\r\nb\rc".to_vec();
        assert_eq!(normalize_line_endings_in_place(&mut b), b"a\nb\nc");
    }
}
