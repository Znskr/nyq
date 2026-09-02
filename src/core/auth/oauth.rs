//! OAuth device-flow protocol: discovery, device authorization, polling,
//! refresh, and revocation.
//!
//! Ported 1:1 from `zig-reference/src/core/auth/oauth.zig`. Zig uses
//! `std.json.Value`; the Rust port reuses `core::mcp::json_value` (the shared
//! zero-dependency JSON parser). All wire strings stay byte-oriented
//! (`&[u8]` / `Vec<u8>`) to match the Zig originals.

use crate::core::auth::oauth_transport::{Disposition, Method, Provider, Request};
use crate::core::auth::secret;
use crate::core::mcp::json_value::{self, JsonMap, Value};
use std::sync::atomic::AtomicBool;

// Sign in with Vercel supports exactly openid, email, profile and offline_access,
// and silently filters anything else. fx only needs identity plus a refresh token,
// so it asks for those two and nothing more. An earlier `use:ai-gateway` entry was
// never a real scope: it was dropped on every grant and bought nothing.
#[allow(non_upper_case_globals)]
pub const default_scope: &str = "openid offline_access";

/// Returns the first requested scope the issuer did not grant. A silently reduced
/// grant is otherwise invisible until some later request fails with a 401.
pub fn missing_granted_scope<'a>(requested: &'a [u8], granted: &[u8]) -> Option<&'a [u8]> {
    for scope in requested.split(|&b| b == b' ').filter(|s| !s.is_empty()) {
        let found = granted
            .split(|&b| b == b' ')
            .any(|candidate| candidate == scope);
        if !found {
            return Some(scope);
        }
    }
    None
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OAuthError {
    InvalidIssuer,
    InvalidOAuthResponse,
    AuthorizationPending,
    SlowDown,
    AccessDenied,
    ExpiredToken,
    InvalidClient,
    OAuthRequestFailed,
}

#[derive(Debug, PartialEq)]
pub struct Metadata {
    pub issuer: Vec<u8>,
    pub device_authorization_endpoint: Vec<u8>,
    pub token_endpoint: Vec<u8>,
    pub revocation_endpoint: Option<Vec<u8>>,
}

impl Metadata {
    pub fn deinit(&mut self) {
        self.issuer = Vec::new();
        self.device_authorization_endpoint = Vec::new();
        self.token_endpoint = Vec::new();
        self.revocation_endpoint = None;
    }
}

#[derive(Debug, PartialEq)]
pub struct DeviceAuthorization {
    pub device_code: Vec<u8>,
    pub user_code: Vec<u8>,
    pub verification_uri: Vec<u8>,
    pub verification_uri_complete: Option<Vec<u8>>,
    pub expires_in: i64,
    pub interval: i64,
}

impl DeviceAuthorization {
    pub fn deinit(&mut self) {
        secret::zero_and_free(std::mem::take(&mut self.device_code));
        self.user_code = Vec::new();
        self.verification_uri = Vec::new();
        self.verification_uri_complete = None;
    }
}

#[derive(Debug, PartialEq)]
pub struct TokenSet {
    pub access_token: Vec<u8>,
    pub refresh_token: Option<Vec<u8>>,
    pub expires_in: i64,
    pub scope: Vec<u8>,
    pub token_type: Vec<u8>,
}

impl TokenSet {
    pub fn deinit(&mut self) {
        secret::zero_and_free(std::mem::take(&mut self.access_token));
        if let Some(value) = self.refresh_token.take() {
            secret::zero_and_free(value);
        }
        self.scope = Vec::new();
        self.token_type = Vec::new();
    }
}

pub enum PollResult {
    Pending,
    SlowDown,
    Success(TokenSet),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenTypeHint {
    AccessToken,
    RefreshToken,
}

impl TokenTypeHint {
    pub fn tag_name(self) -> &'static str {
        match self {
            Self::AccessToken => "access_token",
            Self::RefreshToken => "refresh_token",
        }
    }
}

pub fn expiry_timestamp_ms(now_ms: i64, expires_in_seconds: i64) -> Result<i64, OAuthError> {
    if expires_in_seconds <= 0 {
        return Err(OAuthError::InvalidOAuthResponse);
    }
    let duration_ms = expires_in_seconds
        .checked_mul(1000)
        .ok_or(OAuthError::InvalidOAuthResponse)?;
    now_ms
        .checked_add(duration_ms)
        .ok_or(OAuthError::InvalidOAuthResponse)
}

pub fn discover(
    transport: Provider,
    issuer_url: &[u8],
) -> Result<Metadata, OAuthError> {
    let mut url = issuer_url.to_vec();
    url.extend_from_slice(b"/.well-known/openid-configuration");
    let bytes = fetch_json(transport, Method::Get, &url, None, None, None)?;
    let metadata = parse_metadata(&bytes)?;
    if metadata.issuer != issuer_url {
        return Err(OAuthError::InvalidIssuer);
    }
    Ok(metadata)
}

pub fn request_device_authorization(
    transport: Provider,
    metadata: &Metadata,
    client_id: &[u8],
) -> Result<DeviceAuthorization, OAuthError> {
    let mut form = FormBody::default();
    form.append(b"client_id", client_id);
    form.append(b"scope", default_scope.as_bytes());
    let bytes = fetch_json(
        transport,
        Method::PostForm,
        &metadata.device_authorization_endpoint,
        Some(&form.written),
        None,
        None,
    )?;
    let result = parse_device_authorization(&bytes);
    secret::zero_and_free(bytes);
    result
}

pub fn poll_device_token_bounded(
    transport: Provider,
    metadata: &Metadata,
    client_id: &[u8],
    device_code: &[u8],
    cancel_flag: Option<&AtomicBool>,
    deadline: Option<i64>,
) -> Result<PollResult, OAuthError> {
    let mut form = FormBody::default();
    form.append(b"client_id", client_id);
    form.append(b"grant_type", b"urn:ietf:params:oauth:grant-type:device_code");
    form.append(b"device_code", device_code);

    let bytes = match fetch_json(
        transport,
        Method::PostForm,
        &metadata.token_endpoint,
        Some(&form.written),
        cancel_flag,
        deadline,
    ) {
        Err(OAuthError::AuthorizationPending) => return Ok(PollResult::Pending),
        Err(OAuthError::SlowDown) => return Ok(PollResult::SlowDown),
        Err(err) => return Err(err),
        Ok(bytes) => bytes,
    };
    let result = parse_token_set(&bytes);
    secret::zero_and_free(bytes);
    result.map(PollResult::Success)
}

pub fn refresh_token(
    transport: Provider,
    metadata: &Metadata,
    client_id: &[u8],
    refresh_token_value: &[u8],
) -> Result<TokenSet, OAuthError> {
    let mut form = FormBody::default();
    form.append(b"client_id", client_id);
    form.append(b"grant_type", b"refresh_token");
    form.append(b"refresh_token", refresh_token_value);
    let bytes = fetch_json(
        transport,
        Method::PostForm,
        &metadata.token_endpoint,
        Some(&form.written),
        None,
        None,
    )?;
    let result = parse_token_set(&bytes);
    secret::zero_and_free(bytes);
    result
}

pub fn revoke_token(
    transport: Provider,
    endpoint: &[u8],
    client_id: &[u8],
    token: &[u8],
    token_type_hint: TokenTypeHint,
) -> Result<(), OAuthError> {
    let mut form = FormBody::default();
    form.append(b"client_id", client_id);
    form.append(b"token", token);
    form.append(b"token_type_hint", token_type_hint.tag_name().as_bytes());
    let bytes = fetch_json(
        transport,
        Method::PostForm,
        endpoint,
        Some(&form.written),
        None,
        None,
    )?;
    secret::zero_and_free(bytes);
    Ok(())
}

pub fn parse_metadata(bytes: &[u8]) -> Result<Metadata, OAuthError> {
    let parsed = json_object(bytes)?;
    let object = parsed.as_object().ok_or(OAuthError::InvalidOAuthResponse)?;
    Ok(Metadata {
        issuer: dupe_required_string(object, "issuer")?,
        device_authorization_endpoint: dupe_required_string(object, "device_authorization_endpoint")?,
        token_endpoint: dupe_required_string(object, "token_endpoint")?,
        revocation_endpoint: dupe_optional_string(object, "revocation_endpoint")?,
    })
}

pub fn parse_device_authorization(bytes: &[u8]) -> Result<DeviceAuthorization, OAuthError> {
    let parsed = json_object(bytes)?;
    let object = parsed.as_object().ok_or(OAuthError::InvalidOAuthResponse)?;
    let expires_in = required_integer(object, "expires_in")?;
    let interval = required_integer(object, "interval").unwrap_or(5);
    Ok(DeviceAuthorization {
        device_code: dupe_required_string(object, "device_code")?,
        user_code: dupe_required_string(object, "user_code")?,
        verification_uri: dupe_required_string(object, "verification_uri")?,
        verification_uri_complete: dupe_optional_string(object, "verification_uri_complete")?,
        expires_in,
        interval,
    })
}

pub fn parse_token_set(bytes: &[u8]) -> Result<TokenSet, OAuthError> {
    let parsed = json_object(bytes)?;
    let object = parsed.as_object().ok_or(OAuthError::InvalidOAuthResponse)?;
    let token_type = dupe_required_string(object, "token_type")?;
    if !token_type.eq_ignore_ascii_case(b"Bearer") {
        return Err(OAuthError::InvalidOAuthResponse);
    }
    let scope = dupe_optional_string(object, "scope")?.unwrap_or_default();
    let access_token = dupe_required_string(object, "access_token")?;
    let refresh_token = dupe_optional_string(object, "refresh_token")?;
    let expires_in = required_integer(object, "expires_in")?;
    Ok(TokenSet {
        access_token,
        refresh_token,
        expires_in,
        scope,
        token_type,
    })
}

fn json_object(bytes: &[u8]) -> Result<Value, OAuthError> {
    let text = std::str::from_utf8(bytes).map_err(|_| OAuthError::InvalidOAuthResponse)?;
    let parsed = json_value::parse_json(text).map_err(|_| OAuthError::InvalidOAuthResponse)?;
    if parsed.is_object() {
        Ok(parsed)
    } else {
        Err(OAuthError::InvalidOAuthResponse)
    }
}

fn fetch_json(
    transport: Provider,
    method: Method,
    url: &[u8],
    payload: Option<&[u8]>,
    cancel_flag: Option<&AtomicBool>,
    deadline: Option<i64>,
) -> Result<Vec<u8>, OAuthError> {
    let mut response = transport
        .execute(Request {
            method,
            payload,
            url,
            authorization: None,
            cancel_flag,
            deadline,
        })
        .map_err(|_| OAuthError::OAuthRequestFailed)?;

    if response.disposition == Disposition::Accepted {
        return Ok(response.take_body());
    }
    map_oauth_http_error(&response.body)?;
    Err(OAuthError::OAuthRequestFailed)
}

fn map_oauth_http_error(body: &[u8]) -> Result<(), OAuthError> {
    let text = std::str::from_utf8(body).map_err(|_| OAuthError::OAuthRequestFailed)?;
    let parsed = json_value::parse_json(text).map_err(|_| OAuthError::OAuthRequestFailed)?;
    let object = parsed
        .as_object()
        .ok_or(OAuthError::OAuthRequestFailed)?;
    let value = object.get("error").ok_or(OAuthError::OAuthRequestFailed)?;
    let error = value.as_str().ok_or(OAuthError::OAuthRequestFailed)?;
    match error {
        "authorization_pending" => Err(OAuthError::AuthorizationPending),
        "slow_down" => Err(OAuthError::SlowDown),
        "access_denied" => Err(OAuthError::AccessDenied),
        "expired_token" => Err(OAuthError::ExpiredToken),
        "invalid_client" => Err(OAuthError::InvalidClient),
        _ => Err(OAuthError::OAuthRequestFailed),
    }
}

#[derive(Default)]
struct FormBody {
    written: Vec<u8>,
}

impl FormBody {
    fn append(&mut self, key: &[u8], value: &[u8]) {
        if !self.written.is_empty() {
            self.written.push(b'&');
        }
        percent_encode(&mut self.written, key);
        self.written.push(b'=');
        percent_encode(&mut self.written, value);
    }
}

fn percent_encode(out: &mut Vec<u8>, value: &[u8]) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    for &byte in value {
        let safe = byte.is_ascii_alphanumeric()
            || byte == b'-'
            || byte == b'_'
            || byte == b'.'
            || byte == b'~';
        if safe {
            out.push(byte);
        } else {
            out.push(b'%');
            out.push(HEX[(byte >> 4) as usize]);
            out.push(HEX[(byte & 0x0f) as usize]);
        }
    }
}

fn dupe_required_string(object: &JsonMap, key: &str) -> Result<Vec<u8>, OAuthError> {
    let value = object.get(key).ok_or(OAuthError::InvalidOAuthResponse)?;
    let string = value.as_str().ok_or(OAuthError::InvalidOAuthResponse)?;
    if string.is_empty() {
        return Err(OAuthError::InvalidOAuthResponse);
    }
    Ok(string.as_bytes().to_vec())
}

fn dupe_optional_string(object: &JsonMap, key: &str) -> Result<Option<Vec<u8>>, OAuthError> {
    let value = match object.get(key) {
        Some(value) => value,
        None => return Ok(None),
    };
    if value.is_null() {
        return Ok(None);
    }
    let string = value.as_str().ok_or(OAuthError::InvalidOAuthResponse)?;
    if string.is_empty() {
        return Err(OAuthError::InvalidOAuthResponse);
    }
    Ok(Some(string.as_bytes().to_vec()))
}

fn required_integer(object: &JsonMap, key: &str) -> Result<i64, OAuthError> {
    match object.get(key) {
        Some(Value::Integer(value)) => Ok(*value),
        Some(Value::NumberString(lexeme)) => {
            lexeme.parse::<i64>().map_err(|_| OAuthError::InvalidOAuthResponse)
        }
        _ => Err(OAuthError::InvalidOAuthResponse),
    }
}

// --- test probes ---------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::auth::oauth_transport::{Disposition, Response};
    use std::cell::Cell;
    use std::rc::Rc;

    struct TransportProbe {
        expected_method: Method,
        expected_url: Vec<u8>,
        expected_payload: Option<Vec<u8>>,
        response_disposition: Disposition,
        response_body: Vec<u8>,
        expected_cancel_flag: Option<*const AtomicBool>,
        expect_deadline: bool,
        matched: Rc<Cell<bool>>,
    }

    impl TransportProbe {
        fn provider(&self) -> Provider {
            let matched = Rc::clone(&self.matched);
            let expected_method = self.expected_method;
            let expected_url = self.expected_url.clone();
            let expected_payload = self.expected_payload.clone();
            let response_disposition = self.response_disposition;
            let response_body = self.response_body.clone();
            let expected_cancel_flag = self.expected_cancel_flag;
            let expect_deadline = self.expect_deadline;
            Provider::new(Box::new(move |request: Request<'_>| {
                let cancel_matches = match (request.cancel_flag, expected_cancel_flag) {
                    (Some(actual), Some(expected)) => actual as *const AtomicBool == expected,
                    (None, None) => true,
                    _ => false,
                };
                let payload_matches = match (request.payload, expected_payload.as_deref()) {
                    (Some(actual), Some(expected)) => actual == expected,
                    (None, None) => true,
                    _ => false,
                };
                matched.set(
                    request.method == expected_method
                        && request.url == expected_url.as_slice()
                        && payload_matches
                        && cancel_matches
                        && (request.deadline.is_some() == expect_deadline),
                );
                Ok(Response {
                    disposition: response_disposition,
                    body: response_body.clone(),
                })
            }))
        }
    }

    fn probe_with(
        expected_method: Method,
        expected_url: &[u8],
        response_body: &[u8],
    ) -> TransportProbe {
        TransportProbe {
            expected_method,
            expected_url: expected_url.to_vec(),
            expected_payload: None,
            response_disposition: Disposition::Accepted,
            response_body: response_body.to_vec(),
            expected_cancel_flag: None,
            expect_deadline: false,
            matched: Rc::new(Cell::new(false)),
        }
    }

    #[test]
    fn oauth_discovery_maps_protocol_input_through_the_injected_transport() {
        let issuer = b"https://vercel.test";
        let mut expected_url = issuer.to_vec();
        expected_url.extend_from_slice(b"/.well-known/openid-configuration");
        let mut probe = probe_with(
            Method::Get,
            &expected_url,
            br#"{"issuer":"https://vercel.test","device_authorization_endpoint":"https://vercel.test/device","token_endpoint":"https://vercel.test/token"}"#,
        );

        let mut metadata = discover(probe.provider(), issuer).unwrap();

        assert!(probe.matched.get());
        assert_eq!(b"https://vercel.test/token", metadata.token_endpoint.as_slice());
        metadata.deinit();
    }

    #[test]
    fn oauth_device_authorization_owns_form_mapping_while_transport_owns_execution() {
        let mut probe = probe_with(
            Method::PostForm,
            b"https://vercel.test/device",
            br#"{"device_code":"device","user_code":"CODE","verification_uri":"https://vercel.test/verify","expires_in":600,"interval":5}"#,
        );
        probe.expected_payload = Some(b"client_id=client%20id&scope=openid%20offline_access".to_vec());
        let metadata = Metadata {
            issuer: b"https://vercel.test".to_vec(),
            device_authorization_endpoint: b"https://vercel.test/device".to_vec(),
            token_endpoint: b"https://vercel.test/token".to_vec(),
            revocation_endpoint: None,
        };

        let mut device =
            request_device_authorization(probe.provider(), &metadata, b"client id").unwrap();

        assert!(probe.matched.get());
        assert_eq!(b"device", device.device_code.as_slice());
        device.deinit();
    }

    #[test]
    fn oauth_polling_forwards_bounds_and_preserves_pending_responses() {
        let cancel_flag = AtomicBool::new(false);
        let deadline = Some(1_000_000);
        let mut probe = probe_with(
            Method::PostForm,
            b"https://vercel.test/token",
            br#"{"error":"authorization_pending"}"#,
        );
        probe.expected_payload = Some(
            b"client_id=client&grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code&device_code=device"
                .to_vec(),
        );
        probe.response_disposition = Disposition::Rejected;
        probe.expected_cancel_flag = Some(&cancel_flag as *const AtomicBool);
        probe.expect_deadline = true;
        let metadata = Metadata {
            issuer: b"https://vercel.test".to_vec(),
            device_authorization_endpoint: b"https://vercel.test/device".to_vec(),
            token_endpoint: b"https://vercel.test/token".to_vec(),
            revocation_endpoint: None,
        };

        let result = poll_device_token_bounded(
            probe.provider(),
            &metadata,
            b"client",
            b"device",
            Some(&cancel_flag),
            deadline,
        )
        .unwrap();

        assert!(probe.matched.get());
        assert!(matches!(result, PollResult::Pending));
    }

    #[test]
    fn a_reduced_grant_names_the_scope_the_issuer_withheld() {
        assert_eq!(
            None,
            missing_granted_scope(b"openid offline_access", b"openid offline_access")
        );
        assert_eq!(
            None,
            missing_granted_scope(b"openid", b"openid email profile")
        );
        assert_eq!(
            Some(&b"offline_access"[..]),
            missing_granted_scope(b"openid offline_access", b"openid")
        );
        assert_eq!(
            Some(&b"use:ai-gateway"[..]),
            missing_granted_scope(
                b"openid offline_access use:ai-gateway",
                b"openid offline_access"
            )
        );
        assert_eq!(None, missing_granted_scope(b"", b"openid"));
    }

    #[test]
    fn oauth_parses_metadata() {
        let mut metadata = parse_metadata(
            br#"{"issuer":"https://vercel.com","device_authorization_endpoint":"https://vercel.com/device","token_endpoint":"https://vercel.com/token","revocation_endpoint":"https://vercel.com/revoke"}"#,
        )
        .unwrap();
        assert_eq!(b"https://vercel.com", metadata.issuer.as_slice());
        assert_eq!(
            b"https://vercel.com/device",
            metadata.device_authorization_endpoint.as_slice()
        );
        metadata.deinit();
    }

    #[test]
    fn oauth_maps_provider_errors() {
        assert_eq!(
            Err(OAuthError::AuthorizationPending),
            map_oauth_http_error(br#"{"error":"authorization_pending"}"#)
        );
        assert_eq!(
            Err(OAuthError::SlowDown),
            map_oauth_http_error(br#"{"error":"slow_down"}"#)
        );
        assert_eq!(
            Err(OAuthError::AccessDenied),
            map_oauth_http_error(br#"{"error":"access_denied"}"#)
        );
        assert_eq!(
            Err(OAuthError::ExpiredToken),
            map_oauth_http_error(br#"{"error":"expired_token"}"#)
        );
        assert_eq!(
            Err(OAuthError::OAuthRequestFailed),
            map_oauth_http_error(br#"{"error":"invalid_request"}"#)
        );
        assert_eq!(
            Err(OAuthError::OAuthRequestFailed),
            map_oauth_http_error(br#"{"error":42}"#)
        );
        assert_eq!(
            Err(OAuthError::InvalidClient),
            map_oauth_http_error(br#"{"error":"invalid_client"}"#)
        );
    }

    #[test]
    fn oauth_parses_token_set() {
        let mut token = parse_token_set(
            br#"{"access_token":"access","refresh_token":"refresh","expires_in":3600,"scope":"openid offline_access","token_type":"Bearer"}"#,
        )
        .unwrap();
        assert_eq!(b"access", token.access_token.as_slice());
        assert_eq!(Some(b"refresh".to_vec()), token.refresh_token);
        assert_eq!(3600, token.expires_in);
        token.deinit();
    }

    #[test]
    fn oauth_parsers_clean_up_allocation_failures() {
        let mut metadata = parse_metadata(
            br#"{"issuer":"https://vercel.com","device_authorization_endpoint":"https://vercel.com/device","token_endpoint":"https://vercel.com/token","revocation_endpoint":"https://vercel.com/revoke"}"#,
        )
        .unwrap();
        metadata.deinit();

        let mut device = parse_device_authorization(
            br#"{"device_code":"device","user_code":"user","verification_uri":"https://vercel.com/verify","verification_uri_complete":"https://vercel.com/verify?code=user","expires_in":600,"interval":5}"#,
        )
        .unwrap();
        device.deinit();

        let mut token = parse_token_set(
            br#"{"access_token":"access","refresh_token":"refresh","expires_in":3600,"scope":"openid offline_access","token_type":"Bearer"}"#,
        )
        .unwrap();
        token.deinit();
    }

    #[test]
    fn oauth_expiry_timestamps_reject_invalid_durations() {
        assert_eq!(Ok(11_000), expiry_timestamp_ms(1_000, 10));
        assert_eq!(
            Err(OAuthError::InvalidOAuthResponse),
            expiry_timestamp_ms(1_000, 0)
        );
        assert_eq!(
            Err(OAuthError::InvalidOAuthResponse),
            expiry_timestamp_ms(1_000, -1)
        );
        assert_eq!(
            Err(OAuthError::InvalidOAuthResponse),
            expiry_timestamp_ms(1_000, i64::MAX)
        );
        assert_eq!(
            Err(OAuthError::InvalidOAuthResponse),
            expiry_timestamp_ms(i64::MAX, 1)
        );
    }

    #[test]
    fn oauth_parsers_reject_non_object_json() {
        assert_eq!(
            Err(OAuthError::InvalidOAuthResponse),
            parse_metadata(b"[]")
        );
        assert_eq!(
            Err(OAuthError::InvalidOAuthResponse),
            parse_device_authorization(b"null")
        );
        assert_eq!(
            Err(OAuthError::InvalidOAuthResponse),
            parse_token_set(b"\"token\"")
        );
        assert_eq!(Err(OAuthError::OAuthRequestFailed), map_oauth_http_error(b"42"));
    }
}
