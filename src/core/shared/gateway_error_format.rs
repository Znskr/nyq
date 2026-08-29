use super::{display_width, text_utils};

const MAX_PUBLISHED_ERROR_BYTES: usize = 1024;
const MAX_RECOVERY_DIAGNOSTIC_BYTES: usize = 600;
fn error_object(detail: &[u8]) -> Option<&[u8]> {
    let p = find(detail, b"\"error\"")?;
    let start = detail[p..].iter().position(|&b| b == b'{')? + p;
    let end = matching(detail, start, b'{', b'}')?;
    Some(&detail[start..=end])
}
fn find(h: &[u8], n: &[u8]) -> Option<usize> {
    if n.is_empty() {
        Some(0)
    } else {
        h.windows(n.len()).position(|w| w == n)
    }
}
fn matching(s: &[u8], start: usize, open: u8, close: u8) -> Option<usize> {
    let (mut d, mut quote, mut escape) = (0, false, false);
    for (i, &b) in s.iter().enumerate().skip(start) {
        if quote {
            if escape {
                escape = false
            } else if b == b'\\' {
                escape = true
            } else if b == b'"' {
                quote = false
            }
            continue;
        }
        if b == b'"' {
            quote = true
        } else if b == open {
            d += 1
        } else if b == close {
            d -= 1;
            if d == 0 {
                return Some(i);
            }
        }
    }
    None
}
fn string_field<'a>(obj: &'a [u8], key: &[u8]) -> Option<&'a [u8]> {
    let mut pat = Vec::with_capacity(key.len() + 2);
    pat.push(b'"');
    pat.extend_from_slice(key);
    pat.push(b'"');
    let p = find(obj, &pat)? + pat.len();
    let colon = obj[p..].iter().position(|&b| b == b':')? + p;
    let mut i = colon + 1;
    while obj.get(i).is_some_and(u8::is_ascii_whitespace) {
        i += 1
    }
    if obj.get(i) != Some(&b'"') {
        return None;
    }
    i += 1;
    let start = i;
    while i < obj.len() {
        if obj[i] == b'"' && obj.get(i.wrapping_sub(1)) != Some(&b'\\') {
            return Some(&obj[start..i]);
        }
        i += 1
    }
    None
}
fn nested_object<'a>(obj: &'a [u8], key: &[u8]) -> Option<&'a [u8]> {
    let mut pat = vec![b'"'];
    pat.extend(key);
    pat.push(b'"');
    let p = find(obj, &pat)? + pat.len();
    let start = obj[p..].iter().position(|&b| b == b'{')? + p;
    let end = matching(obj, start, b'{', b'}')?;
    Some(&obj[start..=end])
}
fn provider_from_message(m: &[u8]) -> Option<&[u8]> {
    let marker = b"Providers considered: ";
    let p = find(m, marker)? + marker.len();
    let end = m[p..]
        .iter()
        .position(|b| matches!(b, b'.' | b'\n' | b'\r' | b'\t'))
        .map_or(m.len(), |n| p + n);
    (end > p).then_some(&m[p..end])
}
fn safe(raw: &[u8], max: usize) -> Vec<u8> {
    text_utils::encode_terminal_safe(&text_utils::mask_secrets(raw), max).bytes
}
fn diagnostic(status: u16, detail: &[u8], title: Option<&str>, max: usize) -> Vec<u8> {
    if detail.is_empty() {
        return format!("HTTP {status}").into_bytes();
    }
    let Some(e) = error_object(detail) else {
        let mut raw = format!("HTTP {status}: ").into_bytes();
        raw.extend_from_slice(detail);
        return safe(&raw, max);
    };
    let param = nested_object(e, b"param");
    let code = string_field(e, b"code")
        .or_else(|| string_field(e, b"type"))
        .or_else(|| param.and_then(|p| string_field(p, b"name")));
    let message =
        string_field(e, b"message").or_else(|| param.and_then(|p| string_field(p, b"message")));
    let provider = string_field(e, b"provider").or_else(|| message.and_then(provider_from_message));
    let mut o = title.map_or_else(Vec::new, |t| format!("{t} · ").into_bytes());
    o.extend_from_slice(format!("HTTP {status}").as_bytes());
    if let Some(p) = provider {
        o.extend_from_slice(" · Provider: ".as_bytes());
        o.extend_from_slice(p)
    }
    if let Some(c) = code {
        o.extend_from_slice(" · ".as_bytes());
        o.extend_from_slice(c);
        if let Some(m) = message {
            if c != m {
                o.extend_from_slice(b": ");
                o.extend_from_slice(m)
            }
        }
    } else if let Some(m) = message {
        o.extend_from_slice(" · ".as_bytes());
        o.extend_from_slice(m)
    }
    safe(&o, max)
}
pub fn format_http_error_message(status: u16, detail: &[u8]) -> Vec<u8> {
    diagnostic(
        status,
        detail,
        Some(if status == 401 || status == 403 {
            "API access denied"
        } else {
            "API request failed"
        }),
        MAX_PUBLISHED_ERROR_BYTES,
    )
}
pub fn format_http_recovery_diagnostic(status: u16, detail: &[u8]) -> Vec<u8> {
    diagnostic(status, detail, None, MAX_RECOVERY_DIAGNOSTIC_BYTES)
}
fn schema_after<'a>(m: &'a [u8], marker: &[u8]) -> Option<&'a [u8]> {
    let mut i = find(m, marker)? + marker.len();
    while m
        .get(i)
        .is_some_and(|b| matches!(b, b' ' | b'\'' | b'"' | b'`'))
    {
        i += 1
    }
    let start = i;
    while m
        .get(i)
        .is_some_and(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
    {
        i += 1
    }
    (i > start).then_some(&m[start..i])
}
fn safe_token(raw: &[u8]) -> Vec<u8> {
    let mut o = Vec::new();
    let mut under = false;
    for &b in raw {
        if b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'[' | b']') {
            o.push(b);
            under = b == b'_'
        } else if !under {
            o.push(b'_');
            under = true
        }
    }
    if o.is_empty() {
        b"unknown".to_vec()
    } else {
        o
    }
}
fn param_path(e: &[u8]) -> Option<Vec<u8>> {
    let p = find(e, b"\"param\"")?;
    let colon = e[p..].iter().position(|&b| b == b':')? + p;
    let mut i = colon + 1;
    while e.get(i).is_some_and(u8::is_ascii_whitespace) {
        i += 1
    }
    if e.get(i) == Some(&b'"') {
        i += 1;
        let end = e[i..].iter().position(|&b| b == b'"')? + i;
        return Some(safe_token(&e[i..end]));
    }
    if e.get(i) == Some(&b'{') {
        let end = matching(e, i, b'{', b'}')?;
        let obj = &e[i..=end];
        return string_field(obj, b"path")
            .or_else(|| string_field(obj, b"param"))
            .map(safe_token);
    }
    if e.get(i) == Some(&b'[') {
        let end = matching(e, i, b'[', b']')?;
        let a = &e[i + 1..end];
        if let Some(pp) = find(a, b"\"path\"") {
            let rest = &a[pp..];
            if let Some(bs) = rest.iter().position(|&b| b == b'[') {
                let be = matching(rest, bs, b'[', b']')?;
                return Some(array_path(&rest[bs + 1..be]));
            }
        }
        return Some(array_path(a));
    }
    None
}
fn array_path(a: &[u8]) -> Vec<u8> {
    let mut parts = Vec::<Vec<u8>>::new();
    let mut i = 0;
    while i < a.len() {
        while i < a.len() && (a[i].is_ascii_whitespace() || a[i] == b',') {
            i += 1
        }
        if i >= a.len() {
            break;
        }
        if a[i] == b'"' {
            i += 1;
            let s = i;
            while i < a.len() && a[i] != b'"' {
                i += 1
            }
            parts.push(safe_token(&a[s..i]));
            i += 1
        } else if a[i].is_ascii_digit() || a[i] == b'-' {
            let s = i;
            i += 1;
            while i < a.len() && a[i].is_ascii_digit() {
                i += 1
            }
            parts.push(a[s..i].to_vec())
        } else {
            while i < a.len() && a[i] != b',' {
                i += 1
            }
            parts.push(b"object".to_vec())
        }
    }
    let mut o = Vec::new();
    for (n, p) in parts.iter().enumerate() {
        if n > 0 {
            o.push(b'.')
        }
        o.extend(p)
    }
    o
}
pub fn format_schema_diagnostic(detail: &[u8]) -> Option<Vec<u8>> {
    let e = error_object(detail)?;
    let message = string_field(e, b"message");
    let path = param_path(e);
    let expected = string_field(e, b"expected")
        .or_else(|| message.and_then(|m| schema_after(m, b"expected ")));
    let received = string_field(e, b"received")
        .or_else(|| message.and_then(|m| schema_after(m, b"received ")));
    if path.is_none() && expected.is_none() && received.is_none() {
        return None;
    }
    let mut o = Vec::new();
    for (label, value) in [
        (b"path=".as_slice(), path.as_deref()),
        (b"expected=", expected),
        (b"received=", received),
    ] {
        if let Some(v) = value {
            if !v.is_empty() {
                if !o.is_empty() {
                    o.push(b' ')
                }
                o.extend_from_slice(label);
                o.extend_from_slice(&safe_token(v))
            }
        }
    }
    (!o.is_empty()).then_some(o)
}
pub fn format_marked_line(marker: &[u8], message: &[u8], cols: u16) -> Vec<u8> {
    let safe_cols = usize::from(cols).max(1);
    let pw = display_width::visible_width(marker);
    let indent = vec![b' '; pw.max(1)];
    let mut rem = message;
    let mut first = true;
    let mut out = Vec::new();
    while !rem.is_empty() {
        let prefix = if first { marker } else { &indent };
        let available = safe_cols
            .saturating_sub(display_width::visible_width(prefix))
            .max(1);
        let fit = display_width::prefix_by_width(rem, available);
        let mut cut = fit.len();
        if cut < rem.len() {
            if let Some(space) = fit.iter().rposition(|b| matches!(b, b' ' | b'\t')) {
                if space > 0 {
                    cut = space
                }
            }
        }
        if cut == 0 {
            cut = super::display_width::decode_next_rune(rem, 0).len.max(1)
        }
        if !first {
            out.push(b'\n')
        }
        out.extend_from_slice(prefix);
        out.extend_from_slice(&rem[..cut]);
        rem = &rem[cut..];
        while rem.first().is_some_and(|b| matches!(b, b' ' | b'\t')) {
            rem = &rem[1..]
        }
        first = false
    }
    if message.is_empty() {
        out.extend_from_slice(marker)
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn http() {
        let d=br#"{"error":{"code":"RestrictedProvidersError","message":"demo account is restricted from provider wafer","provider":"wafer"}}"#;
        assert_eq!(format_http_error_message(403,d),"API access denied · HTTP 403 · Provider: wafer · RestrictedProvidersError: demo account is restricted from provider wafer".as_bytes())
    }
    #[test]
    fn schema() {
        let d=br#"{"error":{"message":"Invalid input: expected string, received array","param":["prompt",0,"content"]}}"#;
        assert_eq!(
            format_schema_diagnostic(d).unwrap(),
            b"path=prompt.0.content expected=string received=array"
        )
    }
    #[test]
    fn wrap() {
        let m="API access denied · HTTP 403 · Provider: wafer · demo account is restricted from provider wafer";
        assert_eq!(format_marked_line("▲ ".as_bytes(),m.as_bytes(),72),"▲ API access denied · HTTP 403 · Provider: wafer · demo account is\n  restricted from provider wafer".as_bytes())
    }
}
