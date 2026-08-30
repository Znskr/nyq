//! URL admission and normalization for the `web_fetch` tool.
//!
//! Conservative port of `tools/web/url_policy.zig`. No external crate
//! dependencies are used; IP address parsing and classification are
//! hand-rolled exactly as the original Zig implementation does.

/// Maximum number of bytes accepted in a submitted URL.
pub const MAX_URL_BYTES: usize = 2000;

/// Error set produced by URL admission and normalization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    UrlTooLong,
    EmptyUrl,
    UnsupportedScheme,
    MissingHost,
    CredentialedUrl,
    SingleLabelHost,
    MalformedHost,
    MalformedLocation,
    MalformedPercentEncoding,
    PercentEncodedHost,
    UnicodeHost,
    ControlByte,
    RequestTargetWhitespace,
    InvalidPort,
    InvalidIpv4,
    ScopeIdRejected,
    NonPublicAddress,
    PortChanged,
    ProtocolChanged,
    WriteFailed,
    OutOfMemory,
}

/// URL scheme accepted by `web_fetch`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scheme {
    Http,
    Https,
}

impl Scheme {
    pub fn default_port(self) -> u16 {
        match self {
            Scheme::Http => 80,
            Scheme::Https => 443,
        }
    }

    pub fn text(self) -> &'static str {
        match self {
            Scheme::Http => "http",
            Scheme::Https => "https",
        }
    }
}

/// Minimal IP address union, mirroring `std.Io.net.IpAddress` from the
/// reference for the subset of behavior needed here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IpAddress {
    Ip4([u8; 4]),
    Ip6([u8; 16]),
}

impl IpAddress {
    pub fn parse(text: &str, _port: u16) -> Option<IpAddress> {
        if text.contains(':') {
            Some(IpAddress::Ip6(parse_ipv6(text)?))
        } else {
            Some(IpAddress::Ip4(parse_ipv4_bytes(text)?))
        }
    }
}

/// A fully validated and normalized URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedUrl {
    pub scheme: Scheme,
    pub canonical_host: String,
    pub port: u16,
    pub explicit_port: Option<u16>,
    pub path_query: String,
    pub retrieval_url: String,
    pub literal_address: Option<IpAddress>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedirectKind {
    Follow,
    CrossHost,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedirectDecision {
    pub kind: RedirectKind,
    pub target: Option<ValidatedUrl>,
    pub cross_host_url: Option<String>,
}

struct ParsedAuthority<'a> {
    host: &'a str,
    port: Option<u16>,
    is_ipv6_literal: bool,
}

struct CanonicalHost {
    host: String,
    literal_address: Option<IpAddress>,
}

enum Ipv4ParseError {
    NotIpv4,
    InvalidIpv4,
}

/// Normalize and validate a submitted URL.
pub fn normalize(raw_url: &str) -> Result<ValidatedUrl, Error> {
    let bytes = raw_url.as_bytes();
    if bytes.is_empty() {
        return Err(Error::EmptyUrl);
    }
    if bytes.len() > MAX_URL_BYTES {
        return Err(Error::UrlTooLong);
    }
    reject_control_bytes(bytes)?;
    validate_percent_encoding(bytes)?;

    let scheme_end = find_subslice(bytes, b"://").ok_or(Error::UnsupportedScheme)?;
    let raw_scheme = &raw_url[..scheme_end];
    let input_scheme = if raw_scheme.eq_ignore_ascii_case("http") {
        Scheme::Http
    } else if raw_scheme.eq_ignore_ascii_case("https") {
        Scheme::Https
    } else {
        return Err(Error::UnsupportedScheme);
    };

    let authority_start = scheme_end + 3;
    let authority_end = authority_end(bytes, authority_start);
    if authority_end == authority_start {
        return Err(Error::MissingHost);
    }

    let authority = &raw_url[authority_start..authority_end];
    if authority.contains('@') {
        return Err(Error::CredentialedUrl);
    }

    let parsed_authority = parse_authority(authority)?;
    let host_policy = canonicalize_host(parsed_authority.host, parsed_authority.is_ipv6_literal)?;

    let scheme = Scheme::Https;
    let port = normalized_port(input_scheme, parsed_authority.port);
    let explicit_port = normalized_explicit_port(scheme, port, parsed_authority.port);
    let raw_path_query = strip_fragment(&raw_url[authority_end..].as_bytes());
    let path_query = normalize_path_query(raw_path_query)?;
    let retrieval_url = format_retrieval_url(scheme, &host_policy.host, explicit_port, &path_query);

    Ok(ValidatedUrl {
        scheme,
        canonical_host: host_policy.host,
        port,
        explicit_port,
        path_query,
        retrieval_url,
        literal_address: host_policy.literal_address,
    })
}

pub fn is_public_address(address: IpAddress) -> bool {
    match address {
        IpAddress::Ip4(bytes) => is_public_ipv4(bytes),
        IpAddress::Ip6(bytes) => is_public_ipv6(bytes),
    }
}

pub fn redirect_target(current: &ValidatedUrl, location: &str) -> Result<RedirectDecision, Error> {
    if location.is_empty() {
        return Err(Error::MalformedLocation);
    }
    reject_control_bytes(location.as_bytes())?;

    let absolute = absolute_redirect_url(current, location)?;

    let target = match normalize(&absolute) {
        Ok(target) => target,
        Err(
            err @ (Error::UnsupportedScheme
            | Error::CredentialedUrl
            | Error::ScopeIdRejected
            | Error::ControlByte
            | Error::RequestTargetWhitespace
            | Error::UrlTooLong
            | Error::NonPublicAddress),
        ) => return Err(err),
        Err(_) => return Err(Error::MalformedLocation),
    };

    if target.scheme != current.scheme {
        return Err(Error::ProtocolChanged);
    }
    if target.port != current.port {
        return Err(Error::PortChanged);
    }

    if same_host_or_optional_www(&current.canonical_host, &target.canonical_host) {
        return Ok(RedirectDecision {
            kind: RedirectKind::Follow,
            target: Some(target),
            cross_host_url: None,
        });
    }

    let cross_host_url = target.retrieval_url.clone();
    Ok(RedirectDecision {
        kind: RedirectKind::CrossHost,
        target: None,
        cross_host_url: Some(cross_host_url),
    })
}

fn normalized_port(input_scheme: Scheme, explicit_port: Option<u16>) -> u16 {
    if let Some(port) = explicit_port {
        if input_scheme == Scheme::Http && port == 80 {
            return Scheme::Https.default_port();
        }
        return port;
    }
    Scheme::Https.default_port()
}

fn normalized_explicit_port(scheme: Scheme, port: u16, raw_explicit: Option<u16>) -> Option<u16> {
    if raw_explicit.is_none() {
        return None;
    }
    if port == scheme.default_port() {
        return None;
    }
    Some(port)
}

fn authority_end(url: &[u8], start: usize) -> usize {
    let mut index = start;
    while index < url.len() {
        match url[index] {
            b'/' | b'?' | b'#' => return index,
            _ => {}
        }
        index += 1;
    }
    url.len()
}

fn parse_authority(authority: &str) -> Result<ParsedAuthority<'_>, Error> {
    if authority.is_empty() {
        return Err(Error::MissingHost);
    }
    let bytes = authority.as_bytes();
    if bytes[0] == b'[' {
        let end = authority.find(']').ok_or(Error::MalformedHost)?;
        if end == 1 {
            return Err(Error::MissingHost);
        }
        let host = &authority[1..end];
        if host.contains('%') {
            return Err(Error::ScopeIdRejected);
        }
        if end + 1 == authority.len() {
            return Ok(ParsedAuthority {
                host,
                port: None,
                is_ipv6_literal: true,
            });
        }
        if authority.as_bytes()[end + 1] != b':' {
            return Err(Error::MalformedHost);
        }
        return Ok(ParsedAuthority {
            host,
            port: Some(parse_port(&authority[end + 2..])?),
            is_ipv6_literal: true,
        });
    }

    let colon = authority.rfind(':').unwrap_or(authority.len());
    if colon == 0 {
        return Err(Error::MissingHost);
    }
    let host = &authority[..colon];
    let port = if colon == authority.len() {
        None
    } else {
        Some(parse_port(&authority[colon + 1..])?)
    };
    Ok(ParsedAuthority {
        host,
        port,
        is_ipv6_literal: false,
    })
}

fn parse_port(raw: &str) -> Result<u16, Error> {
    if raw.is_empty() {
        return Err(Error::InvalidPort);
    }
    if !raw.bytes().all(|b| b.is_ascii_digit()) {
        return Err(Error::InvalidPort);
    }
    raw.parse::<u16>().map_err(|_| Error::InvalidPort)
}

fn canonicalize_host(raw_host: &str, is_ipv6_literal: bool) -> Result<CanonicalHost, Error> {
    if raw_host.is_empty() {
        return Err(Error::MissingHost);
    }
    if raw_host.contains('%') {
        return Err(Error::PercentEncodedHost);
    }

    for &byte in raw_host.as_bytes() {
        if byte < 0x20 || byte == 0x7f {
            return Err(Error::ControlByte);
        }
        if byte >= 0x80 {
            return Err(Error::UnicodeHost);
        }
        if byte == b' ' || byte == b'\\' || byte == b'/' {
            return Err(Error::MalformedHost);
        }
    }

    if is_ipv6_literal {
        let ip6 = parse_ipv6(raw_host).ok_or(Error::MalformedHost)?;
        let address = IpAddress::Ip6(ip6);
        if !is_public_address(address) {
            return Err(Error::NonPublicAddress);
        }
        let lower = lower_ascii(raw_host);
        return Ok(CanonicalHost {
            host: lower,
            literal_address: Some(address),
        });
    }

    let without_root = strip_one_root_dot(raw_host);
    if without_root.is_empty() {
        return Err(Error::MalformedHost);
    }

    match parse_strict_ipv4(without_root) {
        Ok(ip4) => {
            let address = IpAddress::Ip4(ip4);
            if !is_public_address(address) {
                return Err(Error::NonPublicAddress);
            }
            return Ok(CanonicalHost {
                host: lower_ascii(without_root),
                literal_address: Some(address),
            });
        }
        Err(Ipv4ParseError::InvalidIpv4) => return Err(Error::InvalidIpv4),
        Err(Ipv4ParseError::NotIpv4) => {}
    }

    if looks_like_ambiguous_ipv4(without_root) {
        return Err(Error::InvalidIpv4);
    }
    if is_single_label(without_root) {
        return Err(Error::SingleLabelHost);
    }

    validate_hostname_grammar(without_root)?;
    let lower = lower_ascii(without_root);
    if is_blocked_hostname(&lower) {
        return Err(Error::NonPublicAddress);
    }
    Ok(CanonicalHost {
        host: lower,
        literal_address: None,
    })
}

fn lower_ascii(value: &str) -> String {
    value.to_ascii_lowercase()
}

fn strip_one_root_dot(host: &str) -> &str {
    if host.len() > 1 && host.ends_with('.') {
        &host[..host.len() - 1]
    } else {
        host
    }
}

fn validate_hostname_grammar(host: &str) -> Result<(), Error> {
    let mut label_len: usize = 0;
    let mut previous: u8 = 0;
    for &byte in host.as_bytes() {
        if byte == b'.' {
            if label_len == 0 || label_len > 63 {
                return Err(Error::MalformedHost);
            }
            if !previous.is_ascii_alphanumeric() {
                return Err(Error::MalformedHost);
            }
            label_len = 0;
            previous = byte;
            continue;
        }
        if !byte.is_ascii_alphanumeric() && byte != b'-' {
            return Err(Error::MalformedHost);
        }
        if label_len == 0 && byte == b'-' {
            return Err(Error::MalformedHost);
        }
        label_len += 1;
        previous = byte;
    }
    if label_len == 0 || label_len > 63 {
        return Err(Error::MalformedHost);
    }
    if !previous.is_ascii_alphanumeric() {
        return Err(Error::MalformedHost);
    }
    Ok(())
}

fn is_single_label(host: &str) -> bool {
    !host.contains('.')
}

fn is_blocked_hostname(host: &str) -> bool {
    if host == "localhost"
        || host == "localhost.localdomain"
        || host == "metadata.google.internal"
        || host == "metadata.goog"
    {
        return true;
    }
    ends_on_label_boundary(host, ".localhost")
        || ends_on_label_boundary(host, ".local")
        || ends_on_label_boundary(host, ".localdomain")
        || ends_on_label_boundary(host, ".internal")
}

fn ends_on_label_boundary(host: &str, suffix: &str) -> bool {
    host.len() > suffix.len() && host.ends_with(suffix)
}

fn parse_strict_ipv4(host: &str) -> Result<[u8; 4], Ipv4ParseError> {
    let mut out = [0u8; 4];
    let mut count: usize = 0;
    for part in host.split('.') {
        if count == 4 {
            return Err(Ipv4ParseError::InvalidIpv4);
        }
        let part_bytes = part.as_bytes();
        if part_bytes.is_empty() {
            return Err(Ipv4ParseError::NotIpv4);
        }
        if part_bytes.len() > 1 && part_bytes[0] == b'0' {
            return Err(Ipv4ParseError::InvalidIpv4);
        }
        for &byte in part_bytes {
            if !byte.is_ascii_digit() {
                return Err(Ipv4ParseError::NotIpv4);
            }
        }
        out[count] = part.parse::<u8>().map_err(|_| Ipv4ParseError::InvalidIpv4)?;
        count += 1;
    }
    if count == 4 {
        return Ok(out);
    }
    if count > 1 {
        return Err(Ipv4ParseError::InvalidIpv4);
    }
    Err(Ipv4ParseError::NotIpv4)
}

fn looks_like_ambiguous_ipv4(host: &str) -> bool {
    if host.contains("0x") || host.contains("0X") {
        return true;
    }

    let mut labels: usize = 0;
    let mut numeric_labels: usize = 0;
    for label in host.split('.') {
        labels += 1;
        if label.is_empty() {
            return true;
        }
        let numeric = label.bytes().all(|b| b.is_ascii_digit());
        if numeric {
            numeric_labels += 1;
        }
    }
    if numeric_labels == labels && labels > 1 {
        return true;
    }
    if labels == 4 && numeric_labels > 0 {
        return true;
    }
    false
}

fn reject_control_bytes(value: &[u8]) -> Result<(), Error> {
    for &byte in value {
        if byte < 0x20 || byte == 0x7f {
            return Err(Error::ControlByte);
        }
    }
    Ok(())
}

fn validate_percent_encoding(value: &[u8]) -> Result<(), Error> {
    let mut index: usize = 0;
    while index < value.len() {
        if value[index] != b'%' {
            index += 1;
            continue;
        }
        if index + 2 >= value.len() {
            return Err(Error::MalformedPercentEncoding);
        }
        if !value[index + 1].is_ascii_hexdigit() || !value[index + 2].is_ascii_hexdigit() {
            return Err(Error::MalformedPercentEncoding);
        }
        index += 3;
    }
    Ok(())
}

fn strip_fragment(path_query_fragment: &[u8]) -> &[u8] {
    match find_byte(path_query_fragment, b'#') {
        Some(end) => &path_query_fragment[..end],
        None => path_query_fragment,
    }
}

fn normalize_path_query(path_query: &[u8]) -> Result<String, Error> {
    for &byte in path_query {
        if is_ascii_whitespace(byte) {
            return Err(Error::RequestTargetWhitespace);
        }
    }
    if path_query.is_empty() {
        return Ok("/".to_string());
    }
    if path_query[0] == b'?' {
        let mut out = String::from("/");
        out.push_str(std::str::from_utf8(path_query).unwrap());
        return Ok(out);
    }
    Ok(std::str::from_utf8(path_query).unwrap().to_string())
}

fn format_retrieval_url(
    scheme: Scheme,
    host: &str,
    explicit_port: Option<u16>,
    path_query: &str,
) -> String {
    let host_text = host_for_url(host);
    if let Some(port) = explicit_port {
        format!("{}://{}:{}{}", scheme.text(), host_text, port, path_query)
    } else {
        format!("{}://{}{}", scheme.text(), host_text, path_query)
    }
}

fn host_for_url(canonical_host: &str) -> String {
    if !canonical_host.contains(':') {
        canonical_host.to_string()
    } else {
        format!("[{}]", canonical_host)
    }
}

fn absolute_redirect_url(current: &ValidatedUrl, location: &str) -> Result<String, Error> {
    let without_fragment = std::str::from_utf8(strip_fragment(location.as_bytes())).unwrap();
    if without_fragment.is_empty() {
        return Ok(current.retrieval_url.clone());
    }

    if without_fragment.starts_with("http://") || without_fragment.starts_with("https://") {
        return Ok(without_fragment.to_string());
    }
    if without_fragment.starts_with("//") {
        return Ok(format!("{}:{}", current.scheme.text(), without_fragment));
    }
    if has_unsupported_absolute_scheme(without_fragment) {
        return Err(Error::UnsupportedScheme);
    }

    if without_fragment.starts_with('/') {
        return Ok(format_retrieval_url(
            current.scheme,
            &current.canonical_host,
            current.explicit_port,
            without_fragment,
        ));
    }

    let merged = merge_relative_path(&current.path_query, without_fragment);
    Ok(format_retrieval_url(
        current.scheme,
        &current.canonical_host,
        current.explicit_port,
        &merged,
    ))
}

fn has_unsupported_absolute_scheme(value: &str) -> bool {
    let colon = match value.find(':') {
        Some(colon) => colon,
        None => return false,
    };
    let slash = value.find('/').unwrap_or(value.len());
    let question = value.find('?').unwrap_or(value.len());
    colon < slash && colon < question
}

fn merge_relative_path(current_path_query: &str, relative: &str) -> String {
    let current_path_end = current_path_query.find('?').unwrap_or(current_path_query.len());
    let current_path = &current_path_query[..current_path_end];
    let base_end = current_path.rfind('/').map_or(0, |slash| slash + 1);
    let query_start = relative.find('?').unwrap_or(relative.len());
    let relative_path = &relative[..query_start];
    let relative_query = &relative[query_start..];

    let joined = format!("{}{}", &current_path[..base_end], relative_path);
    let normalized_path = remove_dot_segments(&joined);
    format!("{}{}", normalized_path, relative_query)
}

fn remove_dot_segments(path: &str) -> String {
    let mut segments: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        if segment.is_empty() || segment == "." {
            continue;
        }
        if segment == ".." {
            if !segments.is_empty() {
                segments.pop();
            }
            continue;
        }
        segments.push(segment);
    }

    let mut result = String::from("/");
    for (index, segment) in segments.iter().enumerate() {
        if index > 0 {
            result.push('/');
        }
        result.push_str(segment);
    }
    if path.ends_with('/') && !segments.is_empty() {
        result.push('/');
    }
    result
}

fn same_host_or_optional_www(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    strip_one_www(a) == strip_one_www(b)
}

fn strip_one_www(host: &str) -> &str {
    if host.starts_with("www.") && host.len() > 4 {
        &host[4..]
    } else {
        host
    }
}

fn is_public_ipv4(bytes: [u8; 4]) -> bool {
    const RANGES: &[([u8; 4], u8)] = &[
        ([0, 0, 0, 0], 8),
        ([10, 0, 0, 0], 8),
        ([100, 64, 0, 0], 10),
        ([127, 0, 0, 0], 8),
        ([169, 254, 0, 0], 16),
        ([172, 16, 0, 0], 12),
        ([192, 0, 0, 0], 24),
        ([192, 0, 2, 0], 24),
        ([192, 88, 99, 0], 24),
        ([192, 168, 0, 0], 16),
        ([198, 18, 0, 0], 15),
        ([198, 51, 100, 0], 24),
        ([203, 0, 113, 0], 24),
        ([224, 0, 0, 0], 4),
        ([240, 0, 0, 0], 4),
    ];
    for &(prefix, bits) in RANGES {
        if ipv4_in_cidr(bytes, prefix, bits) {
            return false;
        }
    }
    true
}

fn ipv4_in_cidr(bytes: [u8; 4], prefix: [u8; 4], bits: u8) -> bool {
    let value = u32::from_be_bytes(bytes);
    let prefix_value = u32::from_be_bytes(prefix);
    let mask: u32 = if bits == 0 {
        0
    } else {
        u32::MAX << (32 - bits)
    };
    (value & mask) == (prefix_value & mask)
}

fn is_public_ipv6(bytes: [u8; 16]) -> bool {
    let in_ipv6_global_unicast = (bytes[0] & 0xe0) == 0x20;
    if let Some(ip4) = ip4_from_ip6(bytes) {
        return is_public_ipv4(ip4) && in_ipv6_global_unicast;
    }

    if !in_ipv6_global_unicast {
        return false;
    }
    if bytes[0] == 0x20 && bytes[1] == 0x01 && (bytes[2] & 0xfe) == 0x00 {
        return false;
    }
    if bytes[0] == 0x20 && bytes[1] == 0x01 && bytes[2] == 0x0d && bytes[3] == 0xb8 {
        return false;
    }
    if bytes[0] == 0x20 && bytes[1] == 0x02 {
        return false;
    }
    if bytes[0] == 0x3f && (bytes[1] & 0xf0) == 0xf0 {
        return false;
    }
    true
}

fn ip4_from_ip6(bytes: [u8; 16]) -> Option<[u8; 4]> {
    if bytes[..10].iter().all(|&b| b == 0) && bytes[10] == 0xff && bytes[11] == 0xff {
        Some([bytes[12], bytes[13], bytes[14], bytes[15]])
    } else {
        None
    }
}

fn is_ascii_whitespace(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

fn find_byte(haystack: &[u8], needle: u8) -> Option<usize> {
    haystack.iter().position(|&b| b == needle)
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    haystack.windows(needle.len()).position(|window| window == needle)
}

fn parse_ipv4_bytes(text: &str) -> Option<[u8; 4]> {
    let parts: Vec<&str> = text.split('.').collect();
    if parts.len() != 4 {
        return None;
    }
    let mut out = [0u8; 4];
    for (index, part) in parts.iter().enumerate() {
        out[index] = part.parse::<u8>().ok()?;
    }
    Some(out)
}

fn parse_ipv6(s: &str) -> Option<[u8; 16]> {
    if s.is_empty() {
        return None;
    }

    let (head, v4): (&str, Option<[u8; 4]>) = if s.contains('.') {
        let colon = s.rfind(':')?;
        let v4 = parse_ipv4_bytes(&s[colon + 1..])?;
        (&s[..colon], Some(v4))
    } else {
        (s, None)
    };

    let (left, right): (&str, &str) = match head.find("::") {
        Some(pos) => (&head[..pos], &head[pos + 2..]),
        None => (head, ""),
    };

    let mut result = [0u8; 16];
    let mut index: usize = 0;

    if !left.is_empty() {
        for group in left.split(':') {
            if group.is_empty() {
                return None;
            }
            let value = u16::from_str_radix(group, 16).ok()?;
            if index + 2 > 16 {
                return None;
            }
            result[index] = (value >> 8) as u8;
            result[index + 1] = (value & 0xff) as u8;
            index += 2;
        }
    }

    let double_present = head.contains("::");

    if double_present {
        let right_groups: Vec<&str> = if right.is_empty() {
            Vec::new()
        } else {
            right.split(':').collect()
        };
        let v4_group_count = if v4.is_some() { 2 } else { 0 };
        let total_remaining = right_groups.len() + v4_group_count;
        if total_remaining > 8 {
            return None;
        }
        let start_byte = 16 - total_remaining * 2;
        if start_byte < index {
            return None;
        }
        let mut pos = start_byte;
        for group in right_groups {
            if group.is_empty() {
                return None;
            }
            let value = u16::from_str_radix(group, 16).ok()?;
            result[pos] = (value >> 8) as u8;
            result[pos + 1] = (value & 0xff) as u8;
            pos += 2;
        }
        if let Some(v4) = v4 {
            result[pos..pos + 4].copy_from_slice(&v4);
            pos += 4;
        }
        if pos != 16 {
            return None;
        }
    } else {
        let group_count = if left.is_empty() {
            0
        } else {
            left.split(':').count()
        };
        if v4.is_some() {
            if group_count != 6 {
                return None;
            }
            if index != 12 {
                return None;
            }
            result[12..16].copy_from_slice(&v4.unwrap());
        } else {
            if group_count != 8 {
                return None;
            }
            if index != 16 {
                return None;
            }
        }
    }

    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn web_fetch_rejects_non_http_schemes_credentials_single_label_hosts_and_overlong_urls_before_services() {
        assert!(matches!(
            normalize("ftp://example.com/file"),
            Err(Error::UnsupportedScheme)
        ));
        assert!(matches!(
            normalize("https://token@example.com/private"),
            Err(Error::CredentialedUrl)
        ));
        assert!(matches!(
            normalize("https://intranet/path"),
            Err(Error::SingleLabelHost)
        ));

        let overlong = "a".repeat(2001);
        assert!(matches!(normalize(&overlong), Err(Error::UrlTooLong)));
    }

    #[test]
    fn web_fetch_canonicalizes_ascii_hostname_root_dot_and_rejects_malformed_percent_encoded_and_unicode_host_bytes() {
        let normalized = normalize("http://Example.COM./docs?q=1#frag").unwrap();

        assert_eq!(normalized.scheme, Scheme::Https);
        assert_eq!(normalized.canonical_host, "example.com");
        assert_eq!(normalized.port, 443);
        assert_eq!(normalized.path_query, "/docs?q=1");
        assert_eq!(normalized.retrieval_url, "https://example.com/docs?q=1");

        assert!(matches!(
            normalize("https://example.com/%zz"),
            Err(Error::MalformedPercentEncoding)
        ));
        assert!(matches!(
            normalize("https://exa%6dple.com/"),
            Err(Error::PercentEncodedHost)
        ));
        assert!(matches!(
            normalize("https://éxample.com/"),
            Err(Error::UnicodeHost)
        ));
    }

    #[test]
    fn web_fetch_rejects_raw_spaces_in_request_targets_and_redirects() {
        assert!(matches!(
            normalize("https://example.com/a b"),
            Err(Error::RequestTargetWhitespace)
        ));
        assert!(matches!(
            normalize("https://example.com/search?q=a b"),
            Err(Error::RequestTargetWhitespace)
        ));

        let current = normalize("https://example.com/docs").unwrap();
        assert!(matches!(
            redirect_target(&current, "/a b"),
            Err(Error::RequestTargetWhitespace)
        ));
    }

    #[test]
    fn web_fetch_fixtures_every_blocked_ipv4_range_ipv6_envelope_exclusion_and_embedded_ipv4_range() {
        let blocked_ipv4 = [
            "0.1.2.3",
            "10.0.0.1",
            "100.64.0.1",
            "127.0.0.1",
            "169.254.169.254",
            "172.16.0.1",
            "192.0.0.1",
            "192.0.2.1",
            "192.88.99.1",
            "192.168.0.1",
            "198.18.0.1",
            "198.51.100.1",
            "203.0.113.1",
            "224.0.0.1",
            "240.0.0.1",
        ];
        for text in blocked_ipv4 {
            let addr = IpAddress::parse(text, 443).unwrap();
            assert!(!is_public_address(addr));
        }

        let public_ipv4 = IpAddress::parse("93.184.216.34", 443).unwrap();
        assert!(is_public_address(public_ipv4));

        let blocked_ipv6 = [
            "::1",
            "fc00::1",
            "fe80::1",
            "ff00::1",
            "2001::1",
            "2001:db8::1",
            "2002::1",
            "3fff::1",
            "::ffff:127.0.0.1",
            "::ffff:192.168.0.1",
        ];
        for text in blocked_ipv6 {
            let addr = IpAddress::parse(text, 443).unwrap();
            assert!(!is_public_address(addr));
        }

        let public_ipv6 = IpAddress::parse("2606:4700:4700::1111", 443).unwrap();
        assert!(is_public_address(public_ipv6));
    }

    #[test]
    fn web_fetch_rejects_public_and_private_ipv4_mapped_ipv6_literals() {
        let mapped = ["93.184.216.34", "127.0.0.1", "192.168.0.1"];

        for embedded in mapped {
            let literal = format!("::ffff:{}", embedded);
            let address = IpAddress::parse(&literal, 443).unwrap();
            assert!(!is_public_address(address));

            let url = format!("https://[{}]/", literal);
            assert!(matches!(normalize(&url), Err(Error::NonPublicAddress)));
        }
    }

    #[test]
    fn web_fetch_relative_redirect_resolves_before_admission_and_malformed_location_fails_closed() {
        let current = normalize("https://example.com/a/b/page").unwrap();

        let redirected = redirect_target(&current, "../next?q=1").unwrap();
        assert_eq!(redirected.kind, RedirectKind::Follow);
        assert_eq!(
            redirected.target.unwrap().retrieval_url,
            "https://example.com/a/next?q=1"
        );

        assert!(matches!(
            redirect_target(&current, "javascript:alert(1)"),
            Err(Error::UnsupportedScheme)
        ));
        assert!(matches!(
            redirect_target(&current, "https://exa mple.com/"),
            Err(Error::MalformedLocation)
        ));
    }

    #[test]
    fn web_fetch_redirect_normalization_rejects_credentials_scope_ids_control_bytes_and_port_changes() {
        let current = normalize("https://example.com/docs").unwrap();

        assert!(matches!(
            redirect_target(&current, "https://token@example.com/"),
            Err(Error::CredentialedUrl)
        ));
        assert!(matches!(
            redirect_target(&current, "https://[2606:4700:4700::1111%25en0]/"),
            Err(Error::ScopeIdRejected)
        ));
        assert!(matches!(
            redirect_target(&current, "https://example.com/a\nb"),
            Err(Error::ControlByte)
        ));
        assert!(matches!(
            redirect_target(&current, "https://example.com:8443/docs"),
            Err(Error::PortChanged)
        ));
    }

    #[test]
    fn web_fetch_scheme_relative_and_fragment_redirect_behavior_is_deterministic() {
        let current = normalize("https://example.com/docs/index.html?x=1").unwrap();

        let scheme_relative = redirect_target(&current, "//example.com/next#frag").unwrap();
        assert_eq!(scheme_relative.kind, RedirectKind::Follow);
        assert_eq!(
            scheme_relative.target.unwrap().retrieval_url,
            "https://example.com/next"
        );

        let fragment = redirect_target(&current, "#section").unwrap();
        assert_eq!(fragment.kind, RedirectKind::Follow);
        assert_eq!(
            fragment.target.unwrap().retrieval_url,
            "https://example.com/docs/index.html?x=1"
        );
    }

    #[test]
    fn web_fetch_cross_host_redirect_returns_reinvocation_result_without_following() {
        let current = normalize("https://example.com/docs").unwrap();

        let cross = redirect_target(&current, "https://example.org/docs").unwrap();
        assert_eq!(cross.kind, RedirectKind::CrossHost);
        assert_eq!(cross.cross_host_url.unwrap(), "https://example.org/docs");

        let www = redirect_target(&current, "https://www.example.com/docs").unwrap();
        assert_eq!(www.kind, RedirectKind::Follow);
        assert_eq!(www.target.unwrap().canonical_host, "www.example.com");
    }
}
