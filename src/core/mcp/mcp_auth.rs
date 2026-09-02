//! MCP OAuth policy, metadata parsers, and credential types.
//!
//! Ported 1:1 from `zig-reference/src/core/mcp/mcp_auth.zig`. Live HTTP
//! authorize/refresh/revoke against a remote issuer uses Zig `std.http.Client`
//! and `operation_control` select; those entry points are omitted here because
//! their dependencies are not in this batch. Every unit test that does not
//! require Zig's fuzzer is ported.

use crate::core::mcp::json_value::{self, Value};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

#[allow(non_upper_case_globals)]
pub const max_document_bytes: usize = 256 * 1024;
#[allow(non_upper_case_globals)]
pub const expiry_skew_ms: i64 = 60 * 1000;
#[allow(non_upper_case_globals)]
pub const max_scope_reauthorizations: u8 = 2;
const MAX_SCOPE_TOKENS: usize = 64;
const MAX_SCOPE_TOKEN_BYTES: usize = 256;
const INTERACTIVE_CALLBACK_TIMEOUT_MS: u64 = 5 * 60 * 1000;
const INTERACTIVE_CALLBACK_POLL_MS: u64 = 50;

#[derive(Clone, Copy, Debug)]
pub struct CancellationSources<'a> {
    pub caller: Option<&'a AtomicBool>,
    pub runtime: Option<&'a AtomicBool>,
}

impl CancellationSources<'_> {
    pub fn cancelled(self) -> bool {
        self.caller
            .is_some_and(|flag| flag.load(Ordering::Acquire))
            || self
                .runtime
                .is_some_and(|flag| flag.load(Ordering::Acquire))
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Challenge {
    pub resource_metadata: Option<String>,
    pub scope: Option<String>,
    pub insufficient_scope: bool,
}

impl Challenge {
    pub fn deinit(&mut self) {
        *self = Self::default();
    }
}

#[derive(Clone, Debug, Default)]
pub struct ClientConfig<'a> {
    pub resource: Option<&'a str>,
    pub issuer: Option<&'a str>,
    pub client_id: Option<&'a str>,
    pub client_secret: Option<&'a str>,
    pub client_metadata_url: Option<&'a str>,
    pub scopes: &'a [&'a str],
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Credentials {
    pub endpoint: String,
    pub resource: String,
    pub issuer: String,
    pub client_id: String,
    pub client_secret: Option<String>,
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub scope: String,
    pub token_type: String,
    pub token_endpoint_auth_method: String,
    pub expires_at_ms: i64,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub revocation_endpoint: Option<String>,
}

impl Credentials {
    pub fn deinit(&mut self) {
        zero_string(&mut self.access_token);
        if let Some(value) = &mut self.client_secret {
            zero_string(value);
        }
        if let Some(value) = &mut self.refresh_token {
            zero_string(value);
        }
        *self = Self {
            endpoint: String::new(),
            resource: String::new(),
            issuer: String::new(),
            client_id: String::new(),
            client_secret: None,
            access_token: String::new(),
            refresh_token: None,
            scope: String::new(),
            token_type: String::new(),
            token_endpoint_auth_method: String::new(),
            expires_at_ms: 0,
            authorization_endpoint: String::new(),
            token_endpoint: String::new(),
            revocation_endpoint: None,
        };
    }

    pub fn needs_refresh(&self, now_ms: i64) -> bool {
        self.expires_at_ms.saturating_sub(expiry_skew_ms) <= now_ms
    }

    pub fn clone(&self) -> Self {
        Self {
            endpoint: self.endpoint.clone(),
            resource: self.resource.clone(),
            issuer: self.issuer.clone(),
            client_id: self.client_id.clone(),
            client_secret: self.client_secret.clone(),
            access_token: self.access_token.clone(),
            refresh_token: self.refresh_token.clone(),
            scope: self.scope.clone(),
            token_type: self.token_type.clone(),
            token_endpoint_auth_method: self.token_endpoint_auth_method.clone(),
            expires_at_ms: self.expires_at_ms,
            authorization_endpoint: self.authorization_endpoint.clone(),
            token_endpoint: self.token_endpoint.clone(),
            revocation_endpoint: self.revocation_endpoint.clone(),
        }
    }
}

fn zero_string(value: &mut String) {
    let mut bytes = std::mem::take(value).into_bytes();
    bytes.fill(0);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IssuerMismatchSource {
    AuthorizationMetadata,
    AuthorizationResponse,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IssuerMismatch {
    pub source: IssuerMismatchSource,
    pub expected: String,
    pub returned: String,
}

impl IssuerMismatch {
    pub fn init(
        source: IssuerMismatchSource,
        expected: &str,
        returned: &str,
    ) -> Self {
        Self {
            source,
            expected: expected.to_string(),
            returned: returned.to_string(),
        }
    }

    pub fn deinit(&mut self) {
        self.expected.clear();
        self.returned.clear();
    }
}

#[derive(Clone, Debug)]
pub enum AuthorizationResult {
    Credentials(Credentials),
    IssuerMismatch(IssuerMismatch),
}

#[derive(Clone, Debug)]
pub enum AuthenticationResult {
    Authenticated { repaired_entries: usize },
    IssuerMismatch(IssuerMismatch),
}

impl AuthenticationResult {
    pub fn deinit(&mut self) {
        if let Self::IssuerMismatch(mismatch) = self {
            mismatch.deinit();
        }
    }
}

#[derive(Clone, Debug)]
pub struct ResourceMetadata {
    pub resource: String,
    pub authorization_servers: Vec<String>,
    pub scopes_supported: Vec<String>,
}

impl ResourceMetadata {
    pub fn deinit(&mut self) {
        self.resource.clear();
        self.authorization_servers.clear();
        self.scopes_supported.clear();
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthorizationMetadata {
    pub issuer: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub registration_endpoint: Option<String>,
    pub revocation_endpoint: Option<String>,
    pub scopes_supported: Vec<String>,
    pub grant_types_supported: Vec<String>,
    pub token_endpoint_auth_methods_supported: Vec<String>,
    pub code_challenge_methods_supported: Vec<String>,
    pub client_id_metadata_document_supported: bool,
    pub authorization_response_iss_parameter_supported: bool,
}

impl AuthorizationMetadata {
    pub fn deinit(&mut self) {
        *self = Self {
            issuer: String::new(),
            authorization_endpoint: String::new(),
            token_endpoint: String::new(),
            registration_endpoint: None,
            revocation_endpoint: None,
            scopes_supported: Vec::new(),
            grant_types_supported: Vec::new(),
            token_endpoint_auth_methods_supported: Vec::new(),
            code_challenge_methods_supported: Vec::new(),
            client_id_metadata_document_supported: false,
            authorization_response_iss_parameter_supported: false,
        };
    }

    pub fn supports(self: &Self, field: AuthMethodField) -> bool {
        match field {
            AuthMethodField::S256 => contains(&self.code_challenge_methods_supported, "S256"),
            AuthMethodField::RefreshToken => {
                contains(&self.grant_types_supported, "refresh_token")
            }
            AuthMethodField::ClientSecretBasic => {
                contains(&self.token_endpoint_auth_methods_supported, "client_secret_basic")
            }
            AuthMethodField::ClientSecretPost => {
                contains(&self.token_endpoint_auth_methods_supported, "client_secret_post")
            }
            AuthMethodField::None => contains(&self.token_endpoint_auth_methods_supported, "none"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuthMethodField {
    S256,
    RefreshToken,
    ClientSecretBasic,
    ClientSecretPost,
    None,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthorizationResponse {
    pub code: String,
    pub state: String,
    pub issuer: Option<String>,
}

impl AuthorizationResponse {
    pub fn deinit(&mut self) {
        zero_string(&mut self.code);
        zero_string(&mut self.state);
        self.issuer = None;
    }
}

#[derive(Clone, Debug)]
pub struct HttpResponseHead {
    pub headers: Vec<(String, String)>,
}

impl HttpResponseHead {
    pub fn parse(raw: &str) -> Result<Self, AuthError> {
        let mut lines = raw.split("\r\n");
        lines.next().ok_or(AuthError::InvalidOAuthResponse)?;
        let mut headers = Vec::new();
        for line in lines {
            if line.is_empty() {
                break;
            }
            let (name, value) = line
                .split_once(':')
                .ok_or(AuthError::InvalidOAuthResponse)?;
            headers.push((name.to_string(), value.trim().to_string()));
        }
        Ok(Self { headers })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuthError {
    InvalidAuthenticateChallenge,
    InvalidMcpAuthEndpoint,
    InsecureMcpAuthEndpoint,
    InvalidAuthorizationIssuer,
    InvalidProtectedResourceMetadata,
    McpAuthResourceMismatch,
    InvalidAuthorizationMetadata,
    AuthorizationMetadataIssuerMismatch,
    MissingMetadataField,
    InvalidMetadataField,
    InvalidMetadataUrl,
    InvalidOAuthScope,
    TooManyOAuthScopes,
    OAuthStateMismatch,
    AuthorizationResponseIssuerMissing,
    AuthorizationResponseIssuerMismatch,
    InvalidAuthorizationRedirect,
    MissingQueryParameter,
    InvalidPercentEncoding,
    InvalidClientMetadataUrl,
    InvalidOAuthResponseContentType,
    InvalidOAuthResponse,
    UnsupportedTokenEndpointAuthenticationMethod,
    InvalidAuthorizationCallback,
    Cancelled,
    McpAuthorizationCallbackTimedOut,
    ClientRegistrationUnavailable,
    ClientRegistrationFailed,
}

/// Returns all WWW-Authenticate field values as one owned challenge list.
pub fn collect_authenticate_header(head: &HttpResponseHead) -> Option<String> {
    let mut out = String::new();
    let mut found = false;
    for (name, value) in &head.headers {
        if !name.eq_ignore_ascii_case("www-authenticate") {
            continue;
        }
        let value = value.trim_matches(|c: char| matches!(c, ' ' | '\t' | '\r' | '\n'));
        if found {
            out.push_str(", ");
        }
        out.push_str(value);
        found = true;
    }
    if found {
        Some(out)
    } else {
        None
    }
}

pub fn parse_challenge(value: &str) -> Result<Challenge, AuthError> {
    let mut result = Challenge::default();
    let mut cursor = match find_bearer_parameters(value) {
        Some(index) => index,
        None => return Ok(result),
    };
    let bytes = value.as_bytes();
    while cursor < bytes.len() {
        while cursor < bytes.len()
            && matches!(bytes[cursor], b' ' | b'\t' | b',')
        {
            cursor += 1;
        }
        let key_start = cursor;
        while cursor < bytes.len()
            && (bytes[cursor].is_ascii_alphanumeric()
                || bytes[cursor] == b'_'
                || bytes[cursor] == b'-')
        {
            cursor += 1;
        }
        if cursor == key_start {
            break;
        }
        let key = &value[key_start..cursor];
        while cursor < bytes.len() && matches!(bytes[cursor], b' ' | b'\t') {
            cursor += 1;
        }
        if cursor >= bytes.len() || bytes[cursor] != b'=' {
            break;
        }
        cursor += 1;
        while cursor < bytes.len() && matches!(bytes[cursor], b' ' | b'\t') {
            cursor += 1;
        }
        let parsed = parse_challenge_value(value, &mut cursor)?;
        if key.eq_ignore_ascii_case("resource_metadata") {
            result.resource_metadata = Some(parsed);
        } else if key.eq_ignore_ascii_case("scope") {
            result.scope = Some(parsed);
        } else {
            if key.eq_ignore_ascii_case("error") && parsed == "insufficient_scope" {
                result.insufficient_scope = true;
            }
        }
    }
    Ok(result)
}

fn find_bearer_parameters(value: &str) -> Option<usize> {
    let scheme = "bearer";
    if value.len() < scheme.len() {
        return None;
    }
    let bytes = value.as_bytes();
    for index in 0..=bytes.len() - scheme.len() {
        if !value[index..index + scheme.len()].eq_ignore_ascii_case(scheme) {
            continue;
        }
        if index > 0 && !matches!(bytes[index - 1], b',' | b' ' | b'\t') {
            continue;
        }
        let after = index + scheme.len();
        if after < bytes.len() && !matches!(bytes[after], b' ' | b'\t') {
            continue;
        }
        return Some(after);
    }
    None
}

fn parse_challenge_value(input: &str, cursor: &mut usize) -> Result<String, AuthError> {
    let bytes = input.as_bytes();
    if *cursor >= bytes.len() {
        return Err(AuthError::InvalidAuthenticateChallenge);
    }
    if bytes[*cursor] != b'"' {
        let start = *cursor;
        while *cursor < bytes.len() && bytes[*cursor] != b',' && bytes[*cursor] != b' ' {
            *cursor += 1;
        }
        return Ok(input[start..*cursor].to_string());
    }
    *cursor += 1;
    let mut out = String::new();
    while *cursor < bytes.len() {
        let byte = bytes[*cursor];
        *cursor += 1;
        if byte == b'"' {
            return Ok(out);
        }
        if byte == b'\\' {
            if *cursor >= bytes.len() {
                return Err(AuthError::InvalidAuthenticateChallenge);
            }
            out.push(bytes[*cursor] as char);
            *cursor += 1;
        } else {
            out.push(byte as char);
        }
    }
    Err(AuthError::InvalidAuthenticateChallenge)
}

struct ParsedUri<'a> {
    scheme: &'a str,
    user: Option<&'a str>,
    password: Option<&'a str>,
    host: Option<&'a str>,
    port: Option<u16>,
    path: &'a str,
    query: Option<&'a str>,
    fragment: Option<&'a str>,
}

fn parse_uri(input: &str) -> Result<ParsedUri<'_>, AuthError> {
    let scheme_end = input.find("://").ok_or(AuthError::InvalidMcpAuthEndpoint)?;
    let scheme = &input[..scheme_end];
    if scheme.is_empty() {
        return Err(AuthError::InvalidMcpAuthEndpoint);
    }
    let rest = &input[scheme_end + 3..];
    let (rest, fragment) = match rest.find('#') {
        Some(index) => (&rest[..index], Some(&rest[index + 1..])),
        None => (rest, None),
    };
    let (authority_and_path, query) = match rest.find('?') {
        Some(index) => (&rest[..index], Some(&rest[index + 1..])),
        None => (rest, None),
    };
    let (authority, path) = match authority_and_path.find('/') {
        Some(index) => (&authority_and_path[..index], &authority_and_path[index..]),
        None => (authority_and_path, ""),
    };
    let (user, password, hostport) = if let Some(at) = authority.rfind('@') {
        let userinfo = &authority[..at];
        let hostport = &authority[at + 1..];
        if let Some(colon) = userinfo.find(':') {
            (
                Some(&userinfo[..colon]),
                Some(&userinfo[colon + 1..]),
                hostport,
            )
        } else {
            (Some(userinfo), None, hostport)
        }
    } else {
        (None, None, authority)
    };
    let (host, port) = if hostport.starts_with('[') {
        let end = hostport.find(']').ok_or(AuthError::InvalidMcpAuthEndpoint)?;
        let host = &hostport[..=end];
        let after = &hostport[end + 1..];
        if after.is_empty() {
            (Some(host), None)
        } else {
            let port = after
                .strip_prefix(':')
                .ok_or(AuthError::InvalidMcpAuthEndpoint)?;
            (
                Some(host),
                Some(port.parse().map_err(|_| AuthError::InvalidMcpAuthEndpoint)?),
            )
        }
    } else if let Some(colon) = hostport.rfind(':') {
        let host = &hostport[..colon];
        let port_str = &hostport[colon + 1..];
        if !port_str.is_empty() && port_str.bytes().all(|b| b.is_ascii_digit()) {
            (
                Some(host),
                Some(
                    port_str
                        .parse()
                        .map_err(|_| AuthError::InvalidMcpAuthEndpoint)?,
                ),
            )
        } else {
            (Some(hostport), None)
        }
    } else {
        (Some(hostport), None)
    };
    if host.map(str::is_empty).unwrap_or(true) {
        return Err(AuthError::InvalidMcpAuthEndpoint);
    }
    Ok(ParsedUri {
        scheme,
        user,
        password,
        host,
        port,
        path,
        query,
        fragment,
    })
}

pub fn canonical_resource(endpoint: &str) -> Result<String, AuthError> {
    let uri = parse_uri(endpoint)?;
    if uri.user.is_some() || uri.password.is_some() || uri.fragment.is_some() {
        return Err(AuthError::InvalidMcpAuthEndpoint);
    }
    if !is_secure_or_loopback(&uri) {
        return Err(AuthError::InsecureMcpAuthEndpoint);
    }
    let origin = origin_alloc(&uri)?;
    let path = raw_path(&uri);
    Ok(format!("{origin}{path}"))
}

pub fn is_loopback_endpoint(endpoint: &str) -> bool {
    parse_uri(endpoint)
        .ok()
        .is_some_and(|uri| is_loopback_uri(&uri))
}

fn is_loopback_uri(uri: &ParsedUri<'_>) -> bool {
    uri.scheme.eq_ignore_ascii_case("http") && uri.port.is_some() && is_loopback_host(uri)
}

fn is_loopback_host(uri: &ParsedUri<'_>) -> bool {
    let host = match uri.host {
        Some(host) => host,
        None => return false,
    };
    host == "127.0.0.1" || host.eq_ignore_ascii_case("localhost") || host == "[::1]"
}

pub fn protected_resource_metadata_urls(resource: &str) -> Result<Vec<String>, AuthError> {
    let uri = parse_uri(resource)?;
    let origin = origin_alloc(&uri)?;
    let path = raw_path(&uri);
    let mut urls = Vec::new();
    if path != "/" {
        let suffix = if path.starts_with('/') {
            path.to_string()
        } else {
            format!("/{path}")
        };
        urls.push(format!(
            "{origin}/.well-known/oauth-protected-resource{suffix}"
        ));
    }
    urls.push(format!("{origin}/.well-known/oauth-protected-resource"));
    Ok(urls)
}

pub fn authorization_metadata_urls(issuer: &str) -> Result<Vec<String>, AuthError> {
    let uri = parse_uri(issuer).map_err(|_| AuthError::InvalidAuthorizationIssuer)?;
    if !is_secure_or_loopback(&uri) || uri.query.is_some() || uri.fragment.is_some() {
        return Err(AuthError::InvalidAuthorizationIssuer);
    }
    let origin = origin_alloc(&uri).map_err(|_| AuthError::InvalidAuthorizationIssuer)?;
    let path = raw_path(&uri);
    let trimmed_path = path.trim_matches('/');
    let mut urls = Vec::new();
    if trimmed_path.is_empty() {
        urls.push(format!("{origin}/.well-known/oauth-authorization-server"));
        urls.push(format!("{origin}/.well-known/openid-configuration"));
    } else {
        urls.push(format!(
            "{origin}/.well-known/oauth-authorization-server/{trimmed_path}"
        ));
        urls.push(format!(
            "{origin}/.well-known/openid-configuration/{trimmed_path}"
        ));
        urls.push(format!("{issuer}/.well-known/openid-configuration"));
    }
    Ok(urls)
}

pub fn parse_resource_metadata(
    bytes: &str,
    expected_resource: &str,
) -> Result<ResourceMetadata, AuthError> {
    let parsed = json_value::parse_json(bytes).map_err(|_| AuthError::InvalidProtectedResourceMetadata)?;
    let object = parsed
        .as_object()
        .ok_or(AuthError::InvalidProtectedResourceMetadata)?;
    let resource = required_string(object, "resource")?;
    let canonical = canonical_resource(resource)?;
    if !resource_covers_endpoint(&canonical, expected_resource) {
        return Err(AuthError::McpAuthResourceMismatch);
    }
    let authorization_servers = dupe_required_string_array(object, "authorization_servers")?;
    if authorization_servers.is_empty() {
        return Err(AuthError::InvalidProtectedResourceMetadata);
    }
    let scopes_supported = dupe_optional_string_array(object, "scopes_supported")?;
    Ok(ResourceMetadata {
        resource: canonical,
        authorization_servers,
        scopes_supported,
    })
}

pub fn parse_authorization_metadata(
    bytes: &str,
    expected_issuer: &str,
) -> Result<AuthorizationMetadata, AuthError> {
    match parse_authorization_metadata_outcome(bytes, expected_issuer)? {
        AuthorizationMetadataOutcome::Metadata(metadata) => Ok(metadata),
        AuthorizationMetadataOutcome::IssuerMismatch(_) => {
            Err(AuthError::AuthorizationMetadataIssuerMismatch)
        }
    }
}

enum AuthorizationMetadataOutcome {
    Metadata(AuthorizationMetadata),
    IssuerMismatch(IssuerMismatch),
}

fn parse_authorization_metadata_outcome(
    bytes: &str,
    expected_issuer: &str,
) -> Result<AuthorizationMetadataOutcome, AuthError> {
    let parsed =
        json_value::parse_json(bytes).map_err(|_| AuthError::InvalidAuthorizationMetadata)?;
    let object = parsed
        .as_object()
        .ok_or(AuthError::InvalidAuthorizationMetadata)?;
    let issuer = required_string(object, "issuer")?;
    if issuer != expected_issuer {
        return Ok(AuthorizationMetadataOutcome::IssuerMismatch(
            IssuerMismatch::init(
                IssuerMismatchSource::AuthorizationMetadata,
                expected_issuer,
                issuer,
            ),
        ));
    }
    let authorization_endpoint = dupe_required_url(object, "authorization_endpoint")?;
    let token_endpoint = dupe_required_url(object, "token_endpoint")?;
    let registration_endpoint = dupe_optional_url(object, "registration_endpoint")?;
    let revocation_endpoint = dupe_optional_url(object, "revocation_endpoint")?;
    let scopes_supported = dupe_optional_string_array(object, "scopes_supported")?;
    let grant_types_supported = dupe_optional_string_array(object, "grant_types_supported")?;
    let token_endpoint_auth_methods_supported = dupe_optional_string_array_default(
        object,
        "token_endpoint_auth_methods_supported",
        "client_secret_basic",
    )?;
    let code_challenge_methods_supported =
        dupe_optional_string_array(object, "code_challenge_methods_supported")?;
    Ok(AuthorizationMetadataOutcome::Metadata(AuthorizationMetadata {
        issuer: issuer.to_string(),
        authorization_endpoint,
        token_endpoint,
        registration_endpoint,
        revocation_endpoint,
        scopes_supported,
        grant_types_supported,
        token_endpoint_auth_methods_supported,
        code_challenge_methods_supported,
        client_id_metadata_document_supported: optional_bool(
            object,
            "client_id_metadata_document_supported",
        )
        .unwrap_or(false),
        authorization_response_iss_parameter_supported: optional_bool(
            object,
            "authorization_response_iss_parameter_supported",
        )
        .unwrap_or(false),
    }))
}

pub fn requested_scope(
    configured: &[&str],
    challenge_scope: Option<&str>,
    metadata_scopes: &[&str],
    previous_scope: Option<&str>,
    request_offline_access: bool,
) -> Result<Option<String>, AuthError> {
    let mut tokens: Vec<String> = Vec::new();
    append_scope_tokens(&mut tokens, previous_scope)?;
    if let Some(scope) = challenge_scope {
        append_scope_tokens(&mut tokens, Some(scope))?;
    } else if !configured.is_empty() {
        for scope in configured {
            append_scope_tokens(&mut tokens, Some(scope))?;
        }
    } else {
        for scope in metadata_scopes {
            append_scope_tokens(&mut tokens, Some(scope))?;
        }
    }
    if request_offline_access {
        append_unique(&mut tokens, "offline_access")?;
    }
    if tokens.is_empty() {
        return Ok(None);
    }
    Ok(Some(tokens.join(" ")))
}

pub fn validate_authorization_response(
    expected_state: &str,
    expected_issuer: &str,
    issuer_parameter_required: bool,
    response: &AuthorizationResponse,
) -> Result<(), AuthError> {
    if expected_state != response.state {
        return Err(AuthError::OAuthStateMismatch);
    }
    if issuer_parameter_required && response.issuer.is_none() {
        return Err(AuthError::AuthorizationResponseIssuerMissing);
    }
    if let Some(issuer) = &response.issuer {
        if expected_issuer != issuer {
            return Err(AuthError::AuthorizationResponseIssuerMismatch);
        }
    }
    Ok(())
}

pub fn parse_authorization_redirect(location: &str) -> Result<AuthorizationResponse, AuthError> {
    let question = location
        .find('?')
        .ok_or(AuthError::InvalidAuthorizationRedirect)?;
    let fragment = location[question + 1..]
        .find('#')
        .map(|index| question + 1 + index)
        .unwrap_or(location.len());
    let query = &location[question + 1..fragment];
    let code = query_value(query, "code")?;
    let state = query_value(query, "state")?;
    let issuer = match query_value(query, "iss") {
        Ok(value) => Some(value),
        Err(AuthError::MissingQueryParameter) => None,
        Err(err) => return Err(err),
    };
    Ok(AuthorizationResponse {
        code,
        state,
        issuer,
    })
}

fn encode_base64_url_no_pad(output: &mut [u8], input: &[u8]) -> usize {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let complete_groups = input.len() / 3;
    let remainder = input.len() % 3;
    let tail_len = match remainder {
        0 => 0,
        1 => 2,
        2 => 3,
        _ => unreachable!(),
    };
    let encoded_len = complete_groups * 4 + tail_len;
    let mut input_index = 0;
    let mut output_index = 0;
    while input_index + 3 <= input.len() {
        let value = (u32::from(input[input_index]) << 16)
            | (u32::from(input[input_index + 1]) << 8)
            | u32::from(input[input_index + 2]);
        output[output_index] = ALPHABET[((value >> 18) & 0x3f) as usize];
        output[output_index + 1] = ALPHABET[((value >> 12) & 0x3f) as usize];
        output[output_index + 2] = ALPHABET[((value >> 6) & 0x3f) as usize];
        output[output_index + 3] = ALPHABET[(value & 0x3f) as usize];
        output_index += 4;
        input_index += 3;
    }
    if remainder != 0 {
        let value = u32::from(input[input_index]) << 16
            | if remainder == 2 {
                u32::from(input[input_index + 1]) << 8
            } else {
                0
            };
        output[output_index] = ALPHABET[((value >> 18) & 0x3f) as usize];
        output[output_index + 1] = ALPHABET[((value >> 12) & 0x3f) as usize];
        if remainder == 2 {
            output[output_index + 2] = ALPHABET[((value >> 6) & 0x3f) as usize];
        }
    }
    encoded_len
}

pub struct Pkce<'a> {
    pub verifier: &'a str,
    pub challenge: &'a str,
}

pub fn generate_pkce<'a>(
    verifier_buf: &'a mut [u8; 64],
    challenge_buf: &'a mut [u8; 43],
    mut fill: impl FnMut(&mut [u8]),
) -> Pkce<'a> {
    let mut entropy = [0u8; 48];
    fill(&mut entropy);
    let verifier_len = encode_base64_url_no_pad(verifier_buf, &entropy);
    let digest = sha256(&verifier_buf[..verifier_len]);
    let challenge_len = encode_base64_url_no_pad(challenge_buf, &digest);
    Pkce {
        verifier: std::str::from_utf8(&verifier_buf[..verifier_len]).unwrap(),
        challenge: std::str::from_utf8(&challenge_buf[..challenge_len]).unwrap(),
    }
}

fn sha256(data: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());
    for chunk in msg.chunks_exact(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh) =
            (h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]);
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }
    let mut digest = [0u8; 32];
    for (i, word) in h.iter().enumerate() {
        digest[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    digest
}

pub fn bearer_header_alloc(access_token: &str) -> String {
    format!("Bearer {access_token}")
}

pub fn validate_client_metadata_url(url: &str) -> Result<(), AuthError> {
    let uri = parse_uri(url).map_err(|_| AuthError::InvalidClientMetadataUrl)?;
    if !uri.scheme.eq_ignore_ascii_case("https")
        || uri.host.is_none()
        || uri.user.is_some()
        || uri.password.is_some()
        || uri.fragment.is_some()
    {
        return Err(AuthError::InvalidClientMetadataUrl);
    }
    let path = if uri.path.is_empty() { "/" } else { uri.path };
    if path.trim_matches('/').is_empty() {
        return Err(AuthError::InvalidClientMetadataUrl);
    }
    Ok(())
}

fn validate_json_content_type(content_type: Option<&str>) -> Result<(), AuthError> {
    let value = content_type.ok_or(AuthError::InvalidOAuthResponseContentType)?;
    let separator = value.find(';').unwrap_or(value.len());
    let media_type = value[..separator].trim_matches(|c: char| matches!(c, ' ' | '\t'));
    if media_type.eq_ignore_ascii_case("application/json") {
        Ok(())
    } else {
        Err(AuthError::InvalidOAuthResponseContentType)
    }
}

fn is_secure_or_loopback(uri: &ParsedUri<'_>) -> bool {
    uri.scheme.eq_ignore_ascii_case("https") || is_loopback_uri(uri)
}

fn validate_oauth_url_for_resource(candidate: &str, resource: &str) -> Result<(), AuthError> {
    let candidate_uri = parse_uri(candidate)?;
    if candidate_uri.user.is_some()
        || candidate_uri.password.is_some()
        || candidate_uri.fragment.is_some()
    {
        return Err(AuthError::InsecureMcpAuthEndpoint);
    }
    if candidate_uri.scheme.eq_ignore_ascii_case("https") {
        return Ok(());
    }
    let resource_uri = parse_uri(resource)?;
    if is_loopback_uri(&candidate_uri) && is_loopback_host(&resource_uri) {
        return Ok(());
    }
    Err(AuthError::InsecureMcpAuthEndpoint)
}

fn origin_alloc(uri: &ParsedUri<'_>) -> Result<String, AuthError> {
    let host = uri.host.ok_or(AuthError::InvalidMcpAuthEndpoint)?;
    let scheme = uri.scheme.to_ascii_lowercase();
    let canonical_host = host.to_ascii_lowercase();
    let default_port = (uri.scheme.eq_ignore_ascii_case("https") && uri.port == Some(443))
        || (uri.scheme.eq_ignore_ascii_case("http") && uri.port == Some(80));
    if let Some(port) = uri.port {
        if !default_port {
            return Ok(format!("{scheme}://{canonical_host}:{port}"));
        }
    }
    Ok(format!("{scheme}://{canonical_host}"))
}

fn raw_path<'a>(uri: &'a ParsedUri<'_>) -> &'a str {
    if uri.path.is_empty() {
        "/"
    } else {
        uri.path
    }
}

fn required_string<'a>(
    object: &'a json_value::JsonMap,
    key: &str,
) -> Result<&'a str, AuthError> {
    let value = object.get(key).ok_or(AuthError::MissingMetadataField)?;
    let text = value.as_str().ok_or(AuthError::InvalidMetadataField)?;
    if text.is_empty() {
        return Err(AuthError::InvalidMetadataField);
    }
    Ok(text)
}

fn dupe_required_url(object: &json_value::JsonMap, key: &str) -> Result<String, AuthError> {
    let value = required_string(object, key).map_err(|err| match err {
        AuthError::MissingMetadataField | AuthError::InvalidMetadataField => {
            AuthError::InvalidMetadataUrl
        }
        other => other,
    })?;
    let uri = parse_uri(value).map_err(|_| AuthError::InvalidMetadataUrl)?;
    if !is_secure_or_loopback(&uri) || uri.fragment.is_some() {
        return Err(AuthError::InvalidMetadataUrl);
    }
    Ok(value.to_string())
}

fn dupe_optional_url(object: &json_value::JsonMap, key: &str) -> Result<Option<String>, AuthError> {
    let value = match object.get(key) {
        Some(value) => value,
        None => return Ok(None),
    };
    if value.is_null() {
        return Ok(None);
    }
    let text = value.as_str().ok_or(AuthError::InvalidMetadataUrl)?;
    if text.is_empty() {
        return Err(AuthError::InvalidMetadataUrl);
    }
    let uri = parse_uri(text).map_err(|_| AuthError::InvalidMetadataUrl)?;
    if !is_secure_or_loopback(&uri) || uri.fragment.is_some() {
        return Err(AuthError::InvalidMetadataUrl);
    }
    Ok(Some(text.to_string()))
}

fn optional_bool(object: &json_value::JsonMap, key: &str) -> Option<bool> {
    object.get(key).and_then(Value::as_bool)
}

fn dupe_required_string_array(
    object: &json_value::JsonMap,
    key: &str,
) -> Result<Vec<String>, AuthError> {
    let value = object.get(key).ok_or(AuthError::MissingMetadataField)?;
    dupe_string_array(value)
}

fn dupe_optional_string_array(
    object: &json_value::JsonMap,
    key: &str,
) -> Result<Vec<String>, AuthError> {
    match object.get(key) {
        Some(value) => dupe_string_array(value),
        None => Ok(Vec::new()),
    }
}

fn dupe_optional_string_array_default(
    object: &json_value::JsonMap,
    key: &str,
    default_value: &str,
) -> Result<Vec<String>, AuthError> {
    match object.get(key) {
        Some(value) => dupe_string_array(value),
        None => Ok(vec![default_value.to_string()]),
    }
}

fn dupe_string_array(value: &Value) -> Result<Vec<String>, AuthError> {
    let items = value.as_array().ok_or(AuthError::InvalidMetadataField)?;
    if items.is_empty() {
        return Ok(Vec::new());
    }
    let mut result = Vec::with_capacity(items.len());
    for item in items {
        let text = item.as_str().ok_or(AuthError::InvalidMetadataField)?;
        if text.is_empty() {
            return Err(AuthError::InvalidMetadataField);
        }
        result.push(text.to_string());
    }
    Ok(result)
}

fn contains(values: &[String], needle: &str) -> bool {
    values.iter().any(|value| value == needle)
}

fn resource_covers_endpoint(resource: &str, endpoint: &str) -> bool {
    if resource == endpoint {
        return true;
    }
    if !endpoint.starts_with(resource) {
        return false;
    }
    resource.is_empty()
        || resource.as_bytes().last() == Some(&b'/')
        || (endpoint.len() > resource.len() && endpoint.as_bytes()[resource.len()] == b'/')
}

fn append_scope_tokens(tokens: &mut Vec<String>, maybe_scope: Option<&str>) -> Result<(), AuthError> {
    let scope = match maybe_scope {
        Some(value) => value,
        None => return Ok(()),
    };
    for token in scope.split(|c: char| matches!(c, ' ' | '\t' | '\r' | '\n')) {
        if !token.is_empty() {
            append_unique(tokens, token)?;
        }
    }
    Ok(())
}

fn append_unique(tokens: &mut Vec<String>, value: &str) -> Result<(), AuthError> {
    if value.is_empty() {
        return Ok(());
    }
    if value.len() > MAX_SCOPE_TOKEN_BYTES {
        return Err(AuthError::InvalidOAuthScope);
    }
    for byte in value.bytes() {
        if byte == 0x21 || (0x23..=0x5b).contains(&byte) || (0x5d..=0x7e).contains(&byte) {
            continue;
        }
        return Err(AuthError::InvalidOAuthScope);
    }
    if tokens.iter().any(|existing| existing == value) {
        return Ok(());
    }
    if tokens.len() >= MAX_SCOPE_TOKENS {
        return Err(AuthError::TooManyOAuthScopes);
    }
    tokens.push(value.to_string());
    Ok(())
}

fn query_value(query: &str, key: &str) -> Result<String, AuthError> {
    for pair in query.split('&') {
        let Some((name, value)) = pair.split_once('=') else {
            continue;
        };
        if name == key {
            return percent_decode(value);
        }
    }
    Err(AuthError::MissingQueryParameter)
}

fn percent_decode(value: &str) -> Result<String, AuthError> {
    let bytes = value.as_bytes();
    let mut out = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len() {
                return Err(AuthError::InvalidPercentEncoding);
            }
            let high = hex_digit(bytes[index + 1]).ok_or(AuthError::InvalidPercentEncoding)?;
            let low = hex_digit(bytes[index + 2]).ok_or(AuthError::InvalidPercentEncoding)?;
            out.push((high << 4) | low);
            index += 3;
        } else {
            out.push(if bytes[index] == b'+' { b' ' } else { bytes[index] });
            index += 1;
        }
    }
    String::from_utf8(out).map_err(|_| AuthError::InvalidPercentEncoding)
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn check_authorization_cancellation(cancellation: CancellationSources<'_>) -> Result<(), AuthError> {
    if cancellation.cancelled() {
        Err(AuthError::Cancelled)
    } else {
        Ok(())
    }
}

fn wait_for_interactive_callback(
    _listener: &TcpListener,
    cancellation: CancellationSources<'_>,
) -> Result<(), AuthError> {
    let started = Instant::now();
    while started.elapsed().as_millis() as u64 <= INTERACTIVE_CALLBACK_TIMEOUT_MS {
        check_authorization_cancellation(cancellation)?;
        thread::sleep(Duration::from_millis(INTERACTIVE_CALLBACK_POLL_MS));
    }
    Err(AuthError::McpAuthorizationCallbackTimedOut)
}

fn validate_redirect_target(location: &str, redirect_uri: &str) -> Result<(), AuthError> {
    let query = location
        .find('?')
        .or_else(|| location.find('#'))
        .unwrap_or(location.len());
    if &location[..query] == redirect_uri {
        Ok(())
    } else {
        Err(AuthError::InvalidAuthorizationCallback)
    }
}

struct ClientRegistration {
    client_id: String,
    client_secret: Option<String>,
    token_endpoint_auth_method: String,
}

fn resolve_client_registration(
    metadata: &AuthorizationMetadata,
    _resource: &str,
    config: ClientConfig<'_>,
    _redirect_uri: &str,
) -> Result<ClientRegistration, AuthError> {
    if let Some(client_id) = config.client_id {
        let auth_method = choose_token_endpoint_auth_method(metadata, config.client_secret.is_some())?;
        return Ok(ClientRegistration {
            client_id: client_id.to_string(),
            client_secret: config.client_secret.map(str::to_string),
            token_endpoint_auth_method: auth_method.to_string(),
        });
    }
    Err(AuthError::ClientRegistrationUnavailable)
}

fn choose_token_endpoint_auth_method(
    metadata: &AuthorizationMetadata,
    has_secret: bool,
) -> Result<&'static str, AuthError> {
    if has_secret && metadata.supports(AuthMethodField::ClientSecretBasic) {
        return Ok("client_secret_basic");
    }
    if has_secret && metadata.supports(AuthMethodField::ClientSecretPost) {
        return Ok("client_secret_post");
    }
    if metadata.supports(AuthMethodField::None) {
        return Ok("none");
    }
    if metadata.supports(AuthMethodField::ClientSecretBasic) {
        return Ok("client_secret_basic");
    }
    if metadata.supports(AuthMethodField::ClientSecretPost) {
        return Ok("client_secret_post");
    }
    Err(AuthError::UnsupportedTokenEndpointAuthenticationMethod)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_base64url_encoding_covers_complete_and_partial_groups() {
        let cases: &[(&[u8], &str)] = &[
            (b"", ""),
            (b"f", "Zg"),
            (b"fo", "Zm8"),
            (b"foo", "Zm9v"),
            (&[0xfb, 0xef, 0xff], "--__"),
        ];
        for &(input, expected) in cases {
            let mut output = [0u8; 64];
            let len = encode_base64_url_no_pad(&mut output, input);
            assert_eq!(expected, std::str::from_utf8(&output[..len]).unwrap());
        }
    }

    #[test]
    fn challenge_parser_retains_only_non_secret_authorization_directions() {
        let challenge = parse_challenge(
            r#"BEARER realm="mcp", error="insufficient_scope", scope="tools.read tools.call", resource_metadata="https://api.example/.well-known/oauth-protected-resource/mcp""#,
        )
        .unwrap();
        assert!(challenge.insufficient_scope);
        assert_eq!("tools.read tools.call", challenge.scope.as_deref().unwrap());
        assert_eq!(
            "https://api.example/.well-known/oauth-protected-resource/mcp",
            challenge.resource_metadata.as_deref().unwrap()
        );
    }

    #[test]
    fn authentication_header_collection_preserves_multiple_challenges() {
        let head = HttpResponseHead::parse(
            "HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"legacy\"\r\nWWW-Authenticate: Bearer scope=\"tools.call\"\r\n\r\n",
        )
        .unwrap();
        let value = collect_authenticate_header(&head).unwrap();
        assert_eq!(
            r#"Basic realm="legacy", Bearer scope="tools.call""#,
            value
        );
        let challenge = parse_challenge(&value).unwrap();
        assert_eq!("tools.call", challenge.scope.as_deref().unwrap());
    }

    #[test]
    fn canonical_resource_normalizes_origin_identity_and_removes_query() {
        let canonical = canonical_resource("HTTPS://API.Example.COM:443?tenant=one").unwrap();
        assert_eq!("https://api.example.com/", canonical);
    }

    #[test]
    fn oauth_url_policy_confines_loopback_http_to_local_mcp_resources() {
        validate_oauth_url_for_resource(
            "https://login.example.com/oauth",
            "https://api.example.com/mcp",
        )
        .unwrap();
        validate_oauth_url_for_resource(
            "http://127.0.0.1:4321/oauth",
            "http://localhost:9876/mcp",
        )
        .unwrap();
        assert_eq!(
            Err(AuthError::InsecureMcpAuthEndpoint),
            validate_oauth_url_for_resource(
                "http://127.0.0.1:4321/oauth",
                "https://api.example.com/mcp",
            )
        );
        assert_eq!(
            Err(AuthError::InsecureMcpAuthEndpoint),
            validate_oauth_url_for_resource(
                "https://user@login.example.com/oauth",
                "https://api.example.com/mcp",
            )
        );
    }

    #[test]
    fn metadata_discovery_paths_preserve_issuer_and_resource_paths() {
        let resource_urls =
            protected_resource_metadata_urls("https://api.example.com/tenant/mcp").unwrap();
        assert_eq!(
            "https://api.example.com/.well-known/oauth-protected-resource/tenant/mcp",
            resource_urls[0]
        );
        assert_eq!(
            "https://api.example.com/.well-known/oauth-protected-resource",
            resource_urls[1]
        );

        let issuer_urls = authorization_metadata_urls("https://login.example.com/tenant").unwrap();
        assert_eq!(
            "https://login.example.com/.well-known/oauth-authorization-server/tenant",
            issuer_urls[0]
        );
        assert_eq!(
            "https://login.example.com/.well-known/openid-configuration/tenant",
            issuer_urls[1]
        );
        assert_eq!(
            "https://login.example.com/tenant/.well-known/openid-configuration",
            issuer_urls[2]
        );
    }

    #[test]
    fn authorization_metadata_defaults_omitted_token_endpoint_authentication_to_client_secret_basic()
    {
        let metadata = parse_authorization_metadata(
            r#"{"issuer":"https://login.example.com","authorization_endpoint":"https://login.example.com/authorize","token_endpoint":"https://login.example.com/token"}"#,
            "https://login.example.com",
        )
        .unwrap();
        assert_eq!(
            "client_secret_basic",
            choose_token_endpoint_auth_method(&metadata, false).unwrap()
        );
    }

    #[test]
    fn authorization_metadata_mismatch_retains_exact_issuer_values_and_fails_closed() {
        let bytes = r#"{"issuer":"https://login.example.com","authorization_endpoint":"https://login.example.com/authorize","token_endpoint":"https://login.example.com/token"}"#;
        let outcome =
            parse_authorization_metadata_outcome(bytes, "https://login.example.com/").unwrap();
        match outcome {
            AuthorizationMetadataOutcome::Metadata(_) => panic!("TestUnexpectedResult"),
            AuthorizationMetadataOutcome::IssuerMismatch(mismatch) => {
                assert_eq!(
                    IssuerMismatchSource::AuthorizationMetadata,
                    mismatch.source
                );
                assert_eq!("https://login.example.com/", mismatch.expected);
                assert_eq!("https://login.example.com", mismatch.returned);
            }
        }
        assert_eq!(
            Err(AuthError::AuthorizationMetadataIssuerMismatch),
            parse_authorization_metadata(bytes, "https://login.example.com/")
        );
    }

    #[test]
    fn client_registration_rejects_unsupported_only_token_endpoint_authentication_metadata() {
        let metadata = parse_authorization_metadata(
            r#"{"issuer":"https://login.example.com","authorization_endpoint":"https://login.example.com/authorize","token_endpoint":"https://login.example.com/token","token_endpoint_auth_methods_supported":["private_key_jwt"]}"#,
            "https://login.example.com",
        )
        .unwrap();
        let result = resolve_client_registration(
            &metadata,
            "https://api.example.com/mcp",
            ClientConfig {
                client_id: Some("configured-client"),
                ..ClientConfig::default()
            },
            "http://127.0.0.1:4321/callback",
        );
        assert_eq!(
            Err(AuthError::UnsupportedTokenEndpointAuthenticationMethod),
            result.map(|_| ())
        );
    }

    #[test]
    fn oauth_json_content_type_accepts_parameters_and_rejects_other_media() {
        validate_json_content_type(Some("application/json")).unwrap();
        validate_json_content_type(Some("Application/JSON; charset=utf-8")).unwrap();
        assert_eq!(
            Err(AuthError::InvalidOAuthResponseContentType),
            validate_json_content_type(None)
        );
        assert_eq!(
            Err(AuthError::InvalidOAuthResponseContentType),
            validate_json_content_type(Some("text/plain"))
        );
    }

    #[test]
    fn scope_policy_unions_prior_and_challenged_scopes_without_duplicates() {
        let scope = requested_scope(
            &[],
            Some("tools.call tools.admin"),
            &[],
            Some("tools.read tools.call"),
            true,
        )
        .unwrap()
        .unwrap();
        assert_eq!("tools.read tools.call tools.admin offline_access", scope);
    }

    #[test]
    fn authorization_response_uses_exact_state_and_issuer_comparison() {
        let response = parse_authorization_redirect(
            "http://127.0.0.1:3000/callback?code=abc&state=state-1&iss=https%3A%2F%2Flogin.example.com",
        )
        .unwrap();
        validate_authorization_response(
            "state-1",
            "https://login.example.com",
            true,
            &response,
        )
        .unwrap();
        assert_eq!(
            Err(AuthError::AuthorizationResponseIssuerMismatch),
            validate_authorization_response(
                "state-1",
                "https://login.example.com/",
                true,
                &response,
            )
        );
    }

    #[test]
    fn authorization_redirect_target_must_match_the_registered_callback() {
        validate_redirect_target(
            "http://localhost:3000/callback?code=one&state=two",
            "http://localhost:3000/callback",
        )
        .unwrap();
        assert_eq!(
            Err(AuthError::InvalidAuthorizationCallback),
            validate_redirect_target(
                "https://attacker.example/callback?code=one&state=two",
                "http://localhost:3000/callback",
            )
        );
    }

    #[test]
    fn interactive_callback_wait_observes_caller_and_lifecycle_cancellation() {
        use std::sync::Arc;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        for source in ["caller", "runtime"] {
            let caller = Arc::new(AtomicBool::new(false));
            let runtime = Arc::new(AtomicBool::new(false));
            let flag = if source == "caller" {
                Arc::clone(&caller)
            } else {
                Arc::clone(&runtime)
            };
            let handle = thread::spawn(move || {
                thread::sleep(Duration::from_millis(20));
                flag.store(true, Ordering::Release);
            });
            let started = Instant::now();
            assert_eq!(
                Err(AuthError::Cancelled),
                wait_for_interactive_callback(
                    &listener,
                    CancellationSources {
                        caller: Some(caller.as_ref()),
                        runtime: Some(runtime.as_ref()),
                    }
                )
            );
            handle.join().unwrap();
            assert!(started.elapsed().as_millis() < 1_000);
        }
    }
}
