//! SuperGrok / X Premium subscription provider.
//!
//! `nyq login grok` signs in through the browser (PKCE, loopback callback on a
//! random port, or a code pasted on stdin). Tokens live in `grok-auth.json`
//! (mode 0600) and are refreshed automatically. Chat uses the Responses API on
//! the subscription proxy; the xAI API key provider is separate.

use std::{
    net::Ipv4Addr,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::Mutex,
};

use super::{responses, EventSink, Provider};
use crate::types::{Request, Response};

pub const AUTH_FILE: &str = "grok-auth.json";
pub const DEFAULT_MODEL: &str = "grok-4";

const CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
const SCOPE: &str = "openid profile email offline_access grok-cli:access api:access";
const ISSUER: &str = "https://auth.x.ai";
const TOKEN_URL: &str = "https://auth.x.ai/oauth2/token";
const USERINFO_URL: &str = "https://auth.x.ai/oauth2/userinfo";
const REVOKE_URL: &str = "https://auth.x.ai/oauth2/revoke";
const RESPONSES_URL: &str = "https://cli-chat-proxy.grok.com/v1/responses";
const MODELS_URL: &str = "https://cli-chat-proxy.grok.com/v1/models";
const MODALITIES_URL: &str = "https://api.x.ai/v1/language-models";
const PROXY_VERSION: &str = "1.0.6";
const TOKEN_AUTH: &str = "xai-grok-cli";
/// Subscription client name the chat proxy requires.
const PROXY_CLIENT_NAME: &str = match std::str::from_utf8(&[0x66, 0x78]) {
    Ok(name) => name,
    Err(_) => panic!("client name"),
};
const CORS_ORIGIN: &str = "https://accounts.x.ai";
const MAX_ACCOUNT_ID: usize = 1024;
const MAX_AUTH_BYTES: u64 = 64 * 1024;
const MAX_CODE_BYTES: usize = 4096;
const MAX_CATALOG_BYTES: usize = 1024 * 1024;
const MAX_MODELS: usize = 128;
const MAX_MODEL_ID: usize = 256;
const EXPIRY_SKEW_MS: i64 = 60_000;
const LOGIN_TIMEOUT: Duration = Duration::from_secs(5 * 60);

const TOKEN_URL_ENV: &str = "NYQ_GROK_TOKEN_URL";
const ISSUER_URL_ENV: &str = "NYQ_GROK_ISSUER_URL";
const USERINFO_URL_ENV: &str = "NYQ_GROK_USERINFO_URL";
const REVOKE_URL_ENV: &str = "NYQ_GROK_REVOKE_URL";
const RESPONSES_URL_ENV: &str = "NYQ_GROK_RESPONSES_URL";
const MODELS_URL_ENV: &str = "NYQ_GROK_MODELS_URL";
const MODALITIES_URL_ENV: &str = "NYQ_GROK_MODALITIES_URL";

#[derive(Clone)]
struct Session {
    access_token: String,
    refresh_token: String,
    expires_at_ms: i64,
    account_id: String,
}

enum Stored {
    Missing,
    Unusable,
    Ready(Session),
}

enum Callback {
    Code(String),
    Unrelated,
    StateMismatch,
    Denied,
}

enum Step {
    Continue,
    Code(String),
}

enum Fetched {
    Ok(String),
    Unauthorized,
}

pub fn auth_path(home: &Path) -> PathBuf {
    home.join(AUTH_FILE)
}

pub fn has_session(home: &Path) -> bool {
    matches!(sign_in(home), SignIn::Ready)
}

/// Whether `~/.nyq/grok-auth.json` is missing, unusable, or a usable sign-in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignIn {
    Missing,
    Unusable,
    Ready,
}

pub fn sign_in(home: &Path) -> SignIn {
    match load(home) {
        Stored::Missing => SignIn::Missing,
        Stored::Unusable => SignIn::Unusable,
        Stored::Ready(_) => SignIn::Ready,
    }
}

/// Account ids are sent as header values, so they stay printable and bounded.
pub fn valid_account_id(account_id: &str) -> bool {
    !account_id.is_empty()
        && account_id.len() <= MAX_ACCOUNT_ID
        && account_id.bytes().all(|b| (0x21..=0x7e).contains(&b))
}

pub fn refresh_deadline_ms(expires_at_ms: i64) -> i64 {
    expires_at_ms.saturating_sub(EXPIRY_SKEW_MS).max(0)
}

fn expired(expires_at_ms: i64, now_ms: i64) -> bool {
    refresh_deadline_ms(expires_at_ms) <= now_ms
}

fn load(home: &Path) -> Stored {
    let path = auth_path(home);
    let meta = match std::fs::symlink_metadata(&path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Stored::Missing,
        Err(_) => return Stored::Unusable,
    };
    if !meta.file_type().is_file() || meta.len() > MAX_AUTH_BYTES {
        return Stored::Unusable;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o077 != 0 {
            return Stored::Unusable;
        }
    }
    let Ok(bytes) = std::fs::read(&path) else {
        return Stored::Unusable;
    };
    parse_session(&bytes).map(Stored::Ready).unwrap_or(Stored::Unusable)
}

fn parse_session(bytes: &[u8]) -> Result<Session> {
    let value: Value = serde_json::from_slice(bytes)?;
    anyhow::ensure!(value["version"] == 1, "invalid Grok sign-in");
    let session = Session {
        access_token: required_string(&value, "access_token")?,
        refresh_token: required_string(&value, "refresh_token")?,
        expires_at_ms: value["expires_at_ms"].as_i64().context("invalid Grok sign-in")?,
        account_id: required_string(&value, "account_id")?,
    };
    anyhow::ensure!(valid_account_id(&session.account_id), "invalid Grok sign-in");
    Ok(session)
}

fn required_string(value: &Value, key: &str) -> Result<String> {
    value[key]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .context("invalid Grok sign-in")
}

fn encode_session(session: &Session) -> Result<Vec<u8>> {
    anyhow::ensure!(valid_account_id(&session.account_id), "invalid Grok sign-in");
    let mut bytes = serde_json::to_vec(&json!({
        "version": 1,
        "access_token": session.access_token,
        "refresh_token": session.refresh_token,
        "expires_at_ms": session.expires_at_ms,
        "account_id": session.account_id,
    }))?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn save_session(home: &Path, session: &Session) -> Result<()> {
    crate::config::write_private(&auth_path(home), &encode_session(session)?)
}

fn configured_url(var: &str, default: &str) -> Result<String> {
    match std::env::var(var) {
        Ok(value) if !value.trim().is_empty() => {
            let value = value.trim();
            anyhow::ensure!(
                loopback_http(value),
                "{var} must be an http:// loopback URL with a port"
            );
            Ok(value.to_owned())
        }
        _ => Ok(default.to_owned()),
    }
}

fn loopback_http(url: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(url) else {
        return false;
    };
    if !url.scheme().eq_ignore_ascii_case("http") || !url.username().is_empty() || url.password().is_some() || url.port().is_none() {
        return false;
    }
    matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"))
}

fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

fn form(pairs: &[(&str, &str)]) -> String {
    let mut out = String::new();
    for (i, (key, value)) in pairs.iter().enumerate() {
        if i > 0 {
            out.push('&');
        }
        percent_encode(&mut out, key);
        out.push('=');
        percent_encode(&mut out, value);
    }
    out
}

fn percent_encode(out: &mut String, value: &str) {
    const HEX: &[u8] = b"0123456789ABCDEF";
    for &byte in value.as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push('%');
            out.push(HEX[(byte >> 4) as usize] as char);
            out.push(HEX[(byte & 0x0f) as usize] as char);
        }
    }
}

fn percent_decode(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len() {
                return None;
            }
            let high = hex_digit(bytes[i + 1])?;
            let low = hex_digit(bytes[i + 2])?;
            out.push(high * 16 + low);
            i += 3;
        } else {
            out.push(if bytes[i] == b'+' { b' ' } else { bytes[i] });
            i += 1;
        }
    }
    if out.is_empty() {
        return None;
    }
    String::from_utf8(out).ok()
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn query_param(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (name, value) = pair.split_once('=')?;
        (name == key).then(|| percent_decode(value)).flatten()
    })
}

fn authorization_url(issuer: &str, redirect: &str, challenge: &str, state: &str) -> String {
    let query = form(&[
        ("response_type", "code"),
        ("client_id", CLIENT_ID),
        ("redirect_uri", redirect),
        ("scope", SCOPE),
        ("code_challenge", challenge),
        ("code_challenge_method", "S256"),
        ("state", state),
        ("referrer", PROXY_CLIENT_NAME),
    ]);
    format!("{}/oauth2/authorize?{query}", issuer.trim_end_matches('/'))
}

fn pkce_challenge(verifier: &str) -> String {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
    use sha2::{Digest, Sha256};
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

fn random_secret() -> String {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
    let mut bytes = Vec::with_capacity(32);
    bytes.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
    bytes.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
    URL_SAFE_NO_PAD.encode(bytes)
}

fn parse_callback(target: &str, expected_state: &str) -> Callback {
    if target.contains('#') || !target.starts_with("/callback?") {
        return Callback::Unrelated;
    }
    let query = &target["/callback?".len()..];
    if query_param(query, "error").is_some() {
        return match query_param(query, "state") {
            Some(state) if state == expected_state => Callback::Denied,
            Some(_) => Callback::StateMismatch,
            None => Callback::Unrelated,
        };
    }
    let (Some(code), Some(state)) = (query_param(query, "code"), query_param(query, "state")) else {
        return Callback::Unrelated;
    };
    if state != expected_state {
        Callback::StateMismatch
    } else {
        Callback::Code(code)
    }
}

fn accept_manual(raw: &str) -> Result<String> {
    let code = raw.trim();
    anyhow::ensure!(
        !code.is_empty() && code.len() <= MAX_CODE_BYTES && code.bytes().all(|b| (0x21..=0x7e).contains(&b)),
        "invalid Grok authorization code"
    );
    Ok(code.to_owned())
}

fn callback_page(title: &str, detail: &str) -> String {
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
<title>nyq</title><style>\
:root{{color-scheme:light dark}}\
body{{margin:0;min-height:100vh;display:flex;align-items:center;justify-content:center;\
background:#fff;color:#111;\
font:15px/1.6 ui-sans-serif,-apple-system,BlinkMacSystemFont,\"Segoe UI\",sans-serif}}\
@media(prefers-color-scheme:dark){{body{{background:#0b0b0c;color:#f4f4f5}}}}\
main{{text-align:center;padding:2rem;max-width:26rem}}\
h1{{margin:0 0 .5rem;font-size:1.125rem;font-weight:600;letter-spacing:-.01em}}\
p{{margin:0;font-size:.875rem;opacity:.62}}\
</style></head><body><main><h1>{title}</h1><p>{detail}</p></main></body></html>"
    )
}

fn header_value<'a>(raw: &'a str, name: &str) -> Option<&'a str> {
    raw.split("\r\n").find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.eq_ignore_ascii_case(name).then(|| value.trim()).filter(|v| !v.is_empty())
    })
}

async fn write_response(socket: &mut tokio::net::TcpStream, status: &str, cors: Option<&str>, body: &str) {
    let mut head = format!("HTTP/1.1 {status}\r\n");
    if let Some(origin) = cors {
        head.push_str(&format!("Access-Control-Allow-Origin: {origin}\r\nVary: Origin\r\n"));
    }
    head.push_str(&format!(
        "Content-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    ));
    let _ = tokio::time::timeout(Duration::from_secs(5), socket.write_all(head.as_bytes())).await;
}

async fn write_preflight(socket: &mut tokio::net::TcpStream) {
    let body = format!(
        "HTTP/1.1 204 No Content\r\n\
Access-Control-Allow-Origin: {CORS_ORIGIN}\r\n\
Access-Control-Allow-Methods: GET\r\n\
Access-Control-Allow-Private-Network: true\r\n\
Vary: Origin, Access-Control-Request-Method, Access-Control-Request-Private-Network\r\n\
Content-Length: 0\r\nConnection: close\r\n\r\n"
    );
    let _ = tokio::time::timeout(Duration::from_secs(5), socket.write_all(body.as_bytes())).await;
}

async fn read_headers(socket: &mut tokio::net::TcpStream, deadline: tokio::time::Instant) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut buf = [0u8; 1024];
    let first = deadline.min(tokio::time::Instant::now() + Duration::from_millis(250));
    let n = tokio::time::timeout_at(first, socket.read(&mut buf)).await.ok()?.ok()?;
    if n == 0 {
        return None;
    }
    bytes.extend_from_slice(&buf[..n]);
    while !bytes.windows(4).any(|w| w == b"\r\n\r\n") {
        if bytes.len() > 16 * 1024 || tokio::time::Instant::now() >= deadline {
            return None;
        }
        let n = tokio::time::timeout_at(deadline, socket.read(&mut buf)).await.ok()?.ok()?;
        if n == 0 {
            return None;
        }
        bytes.extend_from_slice(&buf[..n]);
    }
    Some(bytes)
}

async fn handle_connection(socket: &mut tokio::net::TcpStream, state: &str, deadline: tokio::time::Instant) -> Result<Step> {
    let Some(bytes) = read_headers(socket, deadline).await else {
        return Ok(Step::Continue);
    };
    let raw = String::from_utf8_lossy(&bytes);
    let mut parts = raw.lines().next().unwrap_or_default().split_whitespace();
    let method = parts.next().unwrap_or_default();
    let target = parts.next().unwrap_or_default();
    let origin = header_value(&raw, "origin");
    let cors = origin.filter(|origin| *origin == CORS_ORIGIN);
    if method.eq_ignore_ascii_case("OPTIONS") {
        let requested = header_value(&raw, "access-control-request-method");
        let callback = target == "/callback" || target.starts_with("/callback?");
        if cors.is_some() && requested.is_some_and(|m| m.eq_ignore_ascii_case("GET")) && callback {
            write_preflight(socket).await;
        } else {
            write_response(socket, "404 Not Found", None, "<!doctype html><title>Not found</title>Not found.").await;
        }
        return Ok(Step::Continue);
    }
    if !method.eq_ignore_ascii_case("GET") || (origin.is_some() && cors.is_none()) {
        write_response(socket, "404 Not Found", None, "<!doctype html><title>Not found</title>Not found.").await;
        return Ok(Step::Continue);
    }
    let cors = cors.map(|_| CORS_ORIGIN);
    match parse_callback(target, state) {
        Callback::Code(code) => {
            write_response(
                socket,
                "200 OK",
                cors,
                &callback_page("Authorization complete", "Returning you to nyq. You can close this tab."),
            )
            .await;
            Ok(Step::Code(code))
        }
        Callback::Unrelated => {
            write_response(socket, "404 Not Found", cors, "<!doctype html><title>Not found</title>Not found.").await;
            Ok(Step::Continue)
        }
        Callback::StateMismatch => {
            write_response(socket, "400 Bad Request", cors, &callback_page("Authorization failed", "Return to nyq for details.")).await;
            bail!("Grok sign-in state mismatch")
        }
        Callback::Denied => {
            write_response(socket, "400 Bad Request", cors, &callback_page("Authorization failed", "Return to nyq for details.")).await;
            bail!("Grok sign-in was denied or failed")
        }
    }
}

fn display_email(value: &Value) -> Option<&str> {
    value["email"].as_str().filter(|email| {
        !email.is_empty() && email.len() <= 320 && email.bytes().all(|b| (0x21..=0x7e).contains(&b))
    })
}

async fn post_form(client: &reqwest::Client, url: &str, body: String) -> Result<Value> {
    let response = client
        .post(url)
        .timeout(Duration::from_secs(30))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .context("Grok token request")?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    anyhow::ensure!(status.is_success(), "Grok token request failed (HTTP {status})");
    serde_json::from_str(&text).context("invalid Grok token response")
}

fn token_parts(value: &Value, require_refresh: bool) -> Result<(String, Option<String>, i64)> {
    let access = value["access_token"].as_str().filter(|s| !s.is_empty()).context("invalid Grok token response")?.to_owned();
    let refresh = match value.get("refresh_token") {
        Some(Value::String(token)) if !token.is_empty() => Some(token.clone()),
        None if !require_refresh => None,
        _ => bail!("invalid Grok token response"),
    };
    let expires_in = value["expires_in"].as_i64().filter(|n| *n > 0).context("invalid Grok token response")?;
    Ok((access, refresh, expires_in))
}

async fn fetch_account(client: &reqwest::Client, access_token: &str) -> Result<(String, Option<String>)> {
    let url = configured_url(USERINFO_URL_ENV, USERINFO_URL)?;
    let response = client
        .get(url)
        .timeout(Duration::from_secs(30))
        .bearer_auth(access_token)
        .send()
        .await
        .context("Grok account lookup")?;
    let status = response.status();
    anyhow::ensure!(status.is_success(), "Grok account lookup failed (HTTP {status})");
    let value: Value = response.json().await.context("invalid Grok account response")?;
    let account_id = value["sub"].as_str().filter(|id| valid_account_id(id)).context("invalid Grok account response")?.to_owned();
    Ok((account_id, display_email(&value).map(str::to_owned)))
}

async fn exchange_code(client: &reqwest::Client, code: &str, verifier: &str, redirect: &str) -> Result<(Session, Option<String>)> {
    let url = configured_url(TOKEN_URL_ENV, TOKEN_URL)?;
    let body = form(&[
        ("grant_type", "authorization_code"),
        ("client_id", CLIENT_ID),
        ("code", code),
        ("code_verifier", verifier),
        ("redirect_uri", redirect),
    ]);
    let value = post_form(client, &url, body).await?;
    let (access_token, refresh_token, expires_in) = token_parts(&value, true)?;
    let (account_id, email) = fetch_account(client, &access_token).await?;
    let session = Session {
        access_token,
        refresh_token: refresh_token.context("invalid Grok token response")?,
        expires_at_ms: chrono::Utc::now().timestamp_millis().saturating_add(expires_in.saturating_mul(1000)),
        account_id,
    };
    Ok((session, email))
}

async fn refresh_session(client: &reqwest::Client, session: &Session) -> Result<Session> {
    let url = configured_url(TOKEN_URL_ENV, TOKEN_URL)?;
    let body = form(&[
        ("grant_type", "refresh_token"),
        ("client_id", CLIENT_ID),
        ("refresh_token", &session.refresh_token),
    ]);
    let value = post_form(client, &url, body)
        .await
        .context("refreshing the Grok sign-in failed; run `nyq login grok` if it keeps failing")?;
    let (access_token, refresh_token, expires_in) = token_parts(&value, false)?;
    let (account_id, _) = fetch_account(client, &access_token).await?;
    anyhow::ensure!(account_id == session.account_id, "Grok account changed during refresh");
    Ok(Session {
        access_token,
        refresh_token: refresh_token.unwrap_or_else(|| session.refresh_token.clone()),
        expires_at_ms: chrono::Utc::now().timestamp_millis().saturating_add(expires_in.saturating_mul(1000)),
        account_id,
    })
}

async fn revoke_token(refresh_token: &str) -> Result<()> {
    let url = configured_url(REVOKE_URL_ENV, REVOKE_URL)?;
    let response = http_client()
        .post(url)
        .timeout(Duration::from_secs(30))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(form(&[("token", refresh_token), ("client_id", CLIENT_ID)]))
        .send()
        .await
        .context("Grok revoke request")?;
    let status = response.status();
    anyhow::ensure!(status.is_success(), "Grok revoke failed (HTTP {status})");
    Ok(())
}

fn signed_in_message(email: Option<&str>) -> String {
    match email {
        Some(email) => format!("Signed in to Grok as {email}."),
        None => "Signed in to Grok.".into(),
    }
}

/// Sign in. `notify` always receives the authorization URL. `open_browser` is
/// called unless `NYQ_NO_OPEN_BROWSER` is set (any non-empty value but `0`). `manual` resolves to a
/// pasted code, or `None` when stdin closes without one.
pub async fn login(
    home: &Path,
    notify: impl FnOnce(&str),
    open_browser: impl FnOnce(&str),
    manual: impl std::future::Future<Output = Result<Option<String>>>,
) -> Result<String> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .context("cannot bind Grok sign-in callback on 127.0.0.1")?;
    let port = listener.local_addr()?.port();
    let redirect = format!("http://127.0.0.1:{port}/callback");
    let verifier = random_secret();
    let state = random_secret();
    let issuer = configured_url(ISSUER_URL_ENV, ISSUER)?;
    let url = authorization_url(&issuer, &redirect, &pkce_challenge(&verifier), &state);
    notify(&url);
    if !std::env::var("NYQ_NO_OPEN_BROWSER").is_ok_and(|v| !v.is_empty() && v != "0") {
        open_browser(&url);
    }

    let client = http_client();
    let deadline = tokio::time::Instant::now() + LOGIN_TIMEOUT;
    tokio::pin!(manual);
    let mut manual_done = false;
    loop {
        tokio::select! {
            biased;
            code = &mut manual, if !manual_done => {
                manual_done = true;
                if let Some(code) = code? {
                    return finish(home, &client, &accept_manual(&code)?, &verifier, &redirect).await;
                }
            }
            _ = tokio::time::sleep_until(deadline) => bail!("Grok sign-in timed out after 5 minutes"),
            accepted = listener.accept() => {
                let (mut socket, _) = accepted?;
                if let Step::Code(code) = handle_connection(&mut socket, &state, deadline).await? {
                    return finish(home, &client, &code, &verifier, &redirect).await;
                }
            }
        }
    }
}

async fn finish(home: &Path, client: &reqwest::Client, code: &str, verifier: &str, redirect: &str) -> Result<String> {
    let (session, email) = exchange_code(client, code, verifier, redirect).await?;
    save_session(home, &session)?;
    Ok(signed_in_message(email.as_deref()))
}

/// One line from stdin, without blocking the browser callback. `None` on EOF.
pub async fn stdin_code() -> Result<Option<String>> {
    #[cfg(unix)]
    {
        let _nonblock = match NonblockStdin::enable() {
            Ok(guard) => guard,
            Err(_) => return Ok(None),
        };
        let mut buf = Vec::new();
        let mut closed = false;
        loop {
            match read_stdin_chunk(&mut buf, &mut closed)? {
                Some(line) => return Ok(Some(line)),
                None if closed => return Ok(None),
                None => tokio::time::sleep(Duration::from_millis(50)).await,
            }
        }
    }
    #[cfg(not(unix))]
    {
        loop {
            if !stdin_ready() {
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
            let line = tokio::task::spawn_blocking(|| {
                let mut line = String::new();
                std::io::stdin().read_line(&mut line)?;
                Ok::<_, std::io::Error>(line)
            })
            .await??;
            return Ok(Some(line));
        }
    }
}

#[cfg(unix)]
struct NonblockStdin {
    flags: libc::c_int,
}

#[cfg(unix)]
impl NonblockStdin {
    fn enable() -> std::io::Result<Self> {
        // SAFETY: STDIN_FILENO is the process standard input. Flags are restored on drop.
        let flags = unsafe { libc::fcntl(libc::STDIN_FILENO, libc::F_GETFL) };
        if flags < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: flags came from F_GETFL; O_NONBLOCK is a valid file status flag.
        if unsafe { libc::fcntl(libc::STDIN_FILENO, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Self { flags })
    }
}

#[cfg(unix)]
impl Drop for NonblockStdin {
    fn drop(&mut self) {
        // SAFETY: restoring the flags read in enable().
        unsafe { libc::fcntl(libc::STDIN_FILENO, libc::F_SETFL, self.flags) };
    }
}

#[cfg(unix)]
fn read_stdin_chunk(buf: &mut Vec<u8>, closed: &mut bool) -> Result<Option<String>> {
    if *closed {
        return Ok(None);
    }
    let mut tmp = [0u8; 512];
    loop {
        // SAFETY: tmp is a valid buffer of tmp.len() bytes; stdin is non-blocking.
        let n = unsafe { libc::read(libc::STDIN_FILENO, tmp.as_mut_ptr().cast(), tmp.len()) };
        if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            if err.kind() == std::io::ErrorKind::WouldBlock {
                return Ok(None);
            }
            return Err(err.into());
        }
        if n == 0 {
            *closed = true;
            return Ok((!buf.is_empty()).then(|| String::from_utf8_lossy(buf).into_owned()));
        }
        let chunk = &tmp[..n as usize];
        if let Some(i) = chunk.iter().position(|b| *b == b'\n') {
            buf.extend_from_slice(&chunk[..i]);
            anyhow::ensure!(buf.len() <= MAX_CODE_BYTES, "Grok authorization code is too long");
            let line = String::from_utf8_lossy(buf).into_owned();
            buf.clear();
            *closed = true;
            return Ok(Some(line));
        }
        anyhow::ensure!(buf.len() + chunk.len() <= MAX_CODE_BYTES, "Grok authorization code is too long");
        buf.extend_from_slice(chunk);
        if (n as usize) < tmp.len() {
            return Ok(None);
        }
    }
}

#[cfg(not(unix))]
fn stdin_ready() -> bool {
    const STD_INPUT_HANDLE: u32 = 0xFFFF_FFF6;
    const WAIT_OBJECT_0: u32 = 0;
    #[link(name = "kernel32")]
    extern "system" {
        fn GetStdHandle(handle: u32) -> *mut std::ffi::c_void;
        fn WaitForSingleObject(handle: *mut std::ffi::c_void, millis: u32) -> u32;
    }
    // SAFETY: STD_INPUT_HANDLE is the process standard-input handle; a zero wait does not consume input.
    unsafe { WaitForSingleObject(GetStdHandle(STD_INPUT_HANDLE), 0) == WAIT_OBJECT_0 }
}

pub async fn logout(home: &Path) -> Result<String> {
    let path = auth_path(home);
    if !path.exists() {
        return Ok("No Grok sign-in stored by nyq.".into());
    }
    let mut revocation_failed = false;
    if let Stored::Ready(session) = load(home) {
        if revoke_token(&session.refresh_token).await.is_err() {
            revocation_failed = true;
        }
    }
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok("No Grok sign-in stored by nyq.".into());
        }
        Err(e) => return Err(e).context("removing grok-auth.json"),
    }
    Ok(if revocation_failed {
        "Signed out of Grok. Revoking the login failed; the local sign-in was removed.".into()
    } else {
        "Signed out of Grok.".into()
    })
}

pub struct GrokProvider {
    home: PathBuf,
    model: String,
    effort: Option<String>,
    efforts: Vec<String>,
    context_window: Option<u64>,
    auth: std::sync::Arc<Mutex<Option<Session>>>,
    session_id: String,
    client: reqwest::Client,
}

impl GrokProvider {
    pub fn new(home: PathBuf, model: String, effort: Option<String>) -> Self {
        Self {
            home,
            model,
            effort: effort.filter(|e| e != "auto"),
            efforts: Vec::new(),
            context_window: None,
            auth: std::sync::Arc::new(Mutex::new(None)),
            session_id: uuid::Uuid::new_v4().to_string(),
            client: http_client(),
        }
    }

    pub fn with_catalog(mut self, efforts: Vec<String>, context_window: Option<u64>) -> Self {
        self.efforts = efforts;
        self.context_window = context_window;
        self
    }

    pub async fn list_models(&self) -> Result<Vec<crate::models::ModelChoice>> {
        let models_url = configured_url(MODELS_URL_ENV, MODELS_URL)?;
        let modalities_url = configured_url(MODALITIES_URL_ENV, MODALITIES_URL)?;
        let mut session = self.token(None).await?;
        let mut refreshed = false;
        loop {
            let models = match self.fetch(&session, &models_url, true).await? {
                Fetched::Unauthorized if !refreshed => {
                    refreshed = true;
                    session = self.token(Some(&session.access_token)).await?;
                    continue;
                }
                Fetched::Unauthorized => bail!("Grok model list failed (HTTP 401)"),
                Fetched::Ok(body) => body,
            };
            let modalities = match self.fetch(&session, &modalities_url, false).await? {
                Fetched::Unauthorized if !refreshed => {
                    refreshed = true;
                    session = self.token(Some(&session.access_token)).await?;
                    continue;
                }
                Fetched::Unauthorized => bail!("Grok model list failed (HTTP 401)"),
                Fetched::Ok(body) => body,
            };
            return Ok(parse_catalog(&models, &modalities)?
                .into_iter()
                .map(|model| crate::models::ModelChoice {
                    provider: "grok".into(),
                    model: model.id,
                    context_window: Some(model.context_window),
                    efforts: model.efforts,
                    price: None,
                })
                .collect());
        }
    }

    async fn fetch(&self, session: &Session, url: &str, with_account: bool) -> Result<Fetched> {
        anyhow::ensure!(valid_account_id(&session.account_id), "invalid Grok account");
        let mut request = self
            .client
            .get(url)
            .timeout(Duration::from_secs(30))
            .bearer_auth(&session.access_token)
            .header("accept", "application/json")
            .header("user-agent", crate::config::USER_AGENT);
        if with_account {
            request = request.header("X-XAI-Token-Auth", TOKEN_AUTH).header("x-userid", &session.account_id);
        }
        let response = request.send().await.context("listing Grok models")?;
        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Ok(Fetched::Unauthorized);
        }
        let status = response.status();
        let bytes = response.bytes().await.context("listing Grok models")?;
        anyhow::ensure!(bytes.len() <= MAX_CATALOG_BYTES, "Grok model catalog is too large");
        anyhow::ensure!(status.is_success(), "Grok model list failed (HTTP {status})");
        Ok(Fetched::Ok(String::from_utf8(bytes.to_vec()).context("invalid Grok model catalog")?))
    }

    fn request_body(&self, req: Request<'_>) -> Value {
        let review = req.tools.len() == 1 && req.tools[0].name == crate::classifier::TOOL_NAME;
        let empty_system = req.system.is_empty();
        let mut body = responses::body(
            &self.model,
            self.effort.as_deref(),
            req,
            responses::Options { native_web_search: false, low_verbosity: true },
        );
        if empty_system {
            body["instructions"] = json!("You are a helpful assistant.");
        }
        // Permission review must call the decision tool.
        if review {
            body["tool_choice"] = json!("required");
            body["max_output_tokens"] = json!(2048);
        }
        body
    }

    async fn token(&self, rejected: Option<&str>) -> Result<Session> {
        let now = chrono::Utc::now().timestamp_millis();
        let mut cached = self.auth.lock().await;
        if cached.is_none() {
            *cached = Some(self.load_required()?);
        }
        let current = cached.as_ref().unwrap().clone();
        if token_usable(&current, rejected, now) {
            return Ok(current);
        }
        if let Stored::Ready(fresh) = load(&self.home) {
            let changed = fresh.access_token != current.access_token || fresh.refresh_token != current.refresh_token;
            if changed && token_usable(&fresh, rejected, now) {
                *cached = Some(fresh.clone());
                return Ok(fresh);
            }
        }
        let refreshed = refresh_session(&self.client, &current).await?;
        save_session(&self.home, &refreshed)?;
        *cached = Some(refreshed.clone());
        Ok(refreshed)
    }

    fn load_required(&self) -> Result<Session> {
        match load(&self.home) {
            Stored::Ready(session) => Ok(session),
            Stored::Missing => bail!("Grok subscription needs a sign-in: run `nyq login grok`"),
            Stored::Unusable => bail!("Grok sign-in file is unusable (expected a private {}); run `nyq login grok`", AUTH_FILE),
        }
    }

    async fn send(&self, url: &str, body: &Value, session: &Session) -> Result<reqwest::Response> {
        anyhow::ensure!(valid_account_id(&session.account_id), "invalid Grok account");
        let mut request = self
            .client
            .post(url)
            .bearer_auth(&session.access_token)
            .header("accept", "text/event-stream")
            .header("X-XAI-Token-Auth", TOKEN_AUTH)
            .header("x-authenticateresponse", "authenticate-response")
            .header("x-grok-client-version", PROXY_VERSION)
            .header("x-grok-client-identifier", PROXY_CLIENT_NAME)
            .header("x-grok-model-override", &self.model)
            .header("x-grok-user-id", &session.account_id)
            .header("user-agent", crate::config::USER_AGENT);
        if !self.session_id.is_empty() {
            request = request.header("x-grok-conv-id", &self.session_id);
        }
        Ok(request.json(body).send().await?)
    }
}

fn token_usable(session: &Session, rejected: Option<&str>, now_ms: i64) -> bool {
    !expired(session.expires_at_ms, now_ms) && rejected != Some(session.access_token.as_str())
}

fn valid_model(model: &str) -> bool {
    !model.is_empty() && model.len() <= MAX_MODEL_ID && model.bytes().all(|b| b > 0x20 && b != 0x7f)
}

struct ListedModel {
    id: String,
    context_window: u64,
    /// Validated from the catalog; the picker uses context and efforts.
    #[cfg_attr(not(test), allow(dead_code))]
    max_output_tokens: u64,
    efforts: Vec<String>,
    /// Validated from the catalog; the picker uses context and efforts.
    #[cfg_attr(not(test), allow(dead_code))]
    vision: bool,
}

fn parse_catalog(subscription: &str, modalities: &str) -> Result<Vec<ListedModel>> {
    let subscription: Value = serde_json::from_str(subscription).context("invalid Grok model catalog")?;
    let modalities: Value = serde_json::from_str(modalities).context("invalid Grok model catalog")?;
    let data = subscription["data"].as_array().context("invalid Grok model catalog")?;
    let modality_models = modalities["models"].as_array().context("invalid Grok model catalog")?;
    anyhow::ensure!(data.len() <= MAX_MODELS && modality_models.len() <= MAX_MODELS, "invalid Grok model catalog");
    let mut listed = Vec::new();
    for value in data {
        let object = value.as_object().context("invalid Grok model catalog")?;
        let backend = required_field(object, "api_backend")?;
        if backend != "responses" {
            continue;
        }
        let id = required_field(object, "model")?;
        anyhow::ensure!(valid_model(id), "invalid Grok model catalog");
        let modality = find_modality(modality_models, id)?;
        if !string_array_contains(modality, "output_modalities", "text")? {
            continue;
        }
        let supports = object.get("supports_reasoning_effort").and_then(Value::as_bool).context("invalid Grok model catalog")?;
        let efforts = reasoning_efforts(object.get("reasoning_efforts"))?;
        anyhow::ensure!(supports == !efforts.is_empty(), "invalid Grok model catalog");
        listed.push(ListedModel {
            id: id.to_owned(),
            context_window: positive_u32(object.get("context_window"))?,
            max_output_tokens: optional_positive_u32(object.get("max_completion_tokens"))?,
            vision: string_array_contains(modality, "input_modalities", "image")?,
            efforts,
        });
    }
    Ok(listed)
}

fn required_field<'a>(object: &'a serde_json::Map<String, Value>, key: &str) -> Result<&'a str> {
    object.get(key).and_then(Value::as_str).filter(|s| !s.is_empty()).context("invalid Grok model catalog")
}

fn find_modality<'a>(models: &'a [Value], id: &str) -> Result<&'a serde_json::Map<String, Value>> {
    for value in models {
        let object = value.as_object().context("invalid Grok model catalog")?;
        let candidate = required_field(object, "id")?;
        anyhow::ensure!(valid_model(candidate), "invalid Grok model catalog");
        if candidate == id {
            return Ok(object);
        }
    }
    bail!("invalid Grok model catalog")
}

fn string_array_contains(object: &serde_json::Map<String, Value>, key: &str, expected: &str) -> Result<bool> {
    let items = object.get(key).and_then(Value::as_array).context("invalid Grok model catalog")?;
    anyhow::ensure!(items.len() <= 32, "invalid Grok model catalog");
    let mut found = false;
    for item in items {
        let text = item.as_str().context("invalid Grok model catalog")?;
        found |= text == expected;
    }
    Ok(found)
}

fn reasoning_efforts(value: Option<&Value>) -> Result<Vec<String>> {
    let items = value.and_then(Value::as_array).context("invalid Grok model catalog")?;
    anyhow::ensure!(items.len() <= 16, "invalid Grok model catalog");
    let mut efforts = Vec::new();
    for item in items {
        let raw = item.as_object().and_then(|o| o.get("value")).and_then(Value::as_str).context("invalid Grok model catalog")?;
        let effort = parse_effort(raw)?;
        anyhow::ensure!(!efforts.iter().any(|existing| existing == &effort), "invalid Grok model catalog");
        efforts.push(effort);
    }
    Ok(efforts)
}

fn parse_effort(raw: &str) -> Result<String> {
    anyhow::ensure!(
        !raw.eq_ignore_ascii_case("auto")
            && !raw.eq_ignore_ascii_case("adaptive")
            && !raw.eq_ignore_ascii_case("default")
            && !raw.is_empty()
            && raw.len() <= 64
            && raw.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.')),
        "invalid Grok model catalog"
    );
    Ok(raw.to_owned())
}

fn positive_u32(value: Option<&Value>) -> Result<u64> {
    value
        .and_then(Value::as_u64)
        .filter(|n| *n > 0 && *n <= u32::MAX as u64)
        .context("invalid Grok model catalog")
}

fn optional_positive_u32(value: Option<&Value>) -> Result<u64> {
    match value {
        None | Some(Value::Null) => Ok(0),
        Some(value) => positive_u32(Some(value)),
    }
}

#[async_trait::async_trait]
impl Provider for GrokProvider {
    fn name(&self) -> &str {
        "grok"
    }
    fn model(&self) -> &str {
        &self.model
    }
    fn endpoint(&self) -> Option<String> {
        configured_url(RESPONSES_URL_ENV, RESPONSES_URL).ok()
    }
    fn set_model(&mut self, model: String) {
        self.model = model;
    }
    fn effort(&self) -> Option<&str> {
        self.effort.as_deref()
    }
    fn set_effort(&mut self, effort: Option<String>) {
        self.effort = effort.filter(|e| e != "auto");
    }
    fn efforts(&self) -> Vec<String> {
        self.efforts.clone()
    }
    fn context_window(&self) -> Option<u64> {
        self.context_window
    }
    fn set_conversation(&mut self, id: &str) {
        self.session_id = id.to_owned();
    }
    fn fork(&self) -> Box<dyn Provider> {
        Box::new(Self {
            home: self.home.clone(),
            model: self.model.clone(),
            effort: self.effort.clone(),
            efforts: self.efforts.clone(),
            context_window: self.context_window,
            auth: std::sync::Arc::clone(&self.auth),
            session_id: uuid::Uuid::new_v4().to_string(),
            client: self.client.clone(),
        })
    }

    async fn stream(&self, req: Request<'_>, on_event: EventSink<'_>) -> Result<Response> {
        anyhow::ensure!(valid_model(&self.model), "invalid Grok model name");
        let url = configured_url(RESPONSES_URL_ENV, RESPONSES_URL)?;
        let body = self.request_body(req);
        let session = self.token(None).await?;
        let mut http = self.send(&url, &body, &session).await?;
        if http.status() == reqwest::StatusCode::UNAUTHORIZED {
            let session = self.token(Some(&session.access_token)).await?;
            http = self.send(&url, &body, &session).await?;
        }
        responses::read_stream(http, "Grok", on_event).await
    }
}

#[cfg(test)]
#[allow(clippy::await_holding_lock)]
mod tests {
    use super::*;
    use crate::provider::sse::tests::{events, server, ENV};
    use std::sync::atomic::{AtomicBool, Ordering};

    struct TestEnv {
        _lock: std::sync::MutexGuard<'static, ()>,
        saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
    }

    impl TestEnv {
        fn new() -> Self {
            let lock = ENV.lock().unwrap_or_else(|e| e.into_inner());
            let keys = [
                TOKEN_URL_ENV,
                ISSUER_URL_ENV,
                USERINFO_URL_ENV,
                REVOKE_URL_ENV,
                RESPONSES_URL_ENV,
                MODELS_URL_ENV,
                MODALITIES_URL_ENV,
                "NYQ_NO_OPEN_BROWSER",
                "NYQ_MODEL",
                "NYQ_PROVIDER",
                "NYQ_BASE_URL",
            ];
            let saved = keys.iter().map(|key| (*key, std::env::var_os(key))).collect();
            for key in keys {
                std::env::remove_var(key);
            }
            Self { _lock: lock, saved }
        }

        fn set(&mut self, key: &'static str, value: &str) {
            if !self.saved.iter().any(|(saved, _)| *saved == key) {
                self.saved.push((key, std::env::var_os(key)));
            }
            std::env::set_var(key, value);
        }
    }

    impl Drop for TestEnv {
        fn drop(&mut self) {
            for (key, value) in &self.saved {
                if let Some(value) = value {
                    std::env::set_var(key, value);
                } else {
                    std::env::remove_var(key);
                }
            }
        }
    }

    fn sample(expires_at_ms: i64) -> Session {
        Session {
            access_token: "access".into(),
            refresh_token: "refresh".into(),
            expires_at_ms,
            account_id: "acct_test".into(),
        }
    }

    async fn pending_code() -> Result<Option<String>> {
        std::future::pending::<()>().await;
        Ok(None)
    }

    #[test]
    fn session_round_trip_and_account_rules() {
        assert_eq!(refresh_deadline_ms(100_000), 40_000);
        assert_eq!(refresh_deadline_ms(10_000), 0);
        assert!(valid_account_id("acct_123"));
        assert!(!valid_account_id(""));
        assert!(!valid_account_id("acct\nid"));
        assert!(!valid_account_id(&"a".repeat(MAX_ACCOUNT_ID + 1)));
        let dir = tempfile::tempdir().unwrap();
        save_session(dir.path(), &sample(1234)).unwrap();
        let Stored::Ready(loaded) = load(dir.path()) else { panic!("missing session") };
        assert_eq!(loaded.access_token, "access");
        assert_eq!(loaded.refresh_token, "refresh");
        assert_eq!(loaded.account_id, "acct_test");
        assert_eq!(loaded.expires_at_ms, 1234);
        let raw = std::fs::read(auth_path(dir.path())).unwrap();
        assert!(raw.ends_with(b"\n"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(auth_path(dir.path())).unwrap().permissions().mode() & 0o777, 0o600);
            std::fs::set_permissions(auth_path(dir.path()), std::fs::Permissions::from_mode(0o644)).unwrap();
            assert!(matches!(load(dir.path()), Stored::Unusable));
            assert!(!has_session(dir.path()));
        }
        assert!(parse_session(br#"{"version":1,"access_token":"a","refresh_token":"b","expires_at_ms":1,"account_id":"acct\nid"}"#).is_err());
        assert!(parse_session(br#"{"version":2,"access_token":"a","refresh_token":"b","expires_at_ms":1,"account_id":"acct"}"#).is_err());
    }

    #[test]
    fn loopback_overrides_and_authorization_url() {
        assert!(loopback_http("http://127.0.0.1:1234/token"));
        assert!(loopback_http("http://localhost:1234/token"));
        assert!(loopback_http("http://[::1]:1234/token"));
        assert!(!loopback_http("https://127.0.0.1:1234/token"));
        assert!(!loopback_http("http://example.com:1234/token"));
        assert!(!loopback_http("http://127.0.0.1/token"));
        assert!(!loopback_http("http://user@127.0.0.1:1234/token"));
        let url = authorization_url(ISSUER, "http://127.0.0.1:1455/callback", "challenge-value", "state-value");
        assert!(url.starts_with("https://auth.x.ai/oauth2/authorize?"));
        assert!(!url.contains(' '));
        assert!(url.contains("%20"));
        assert!(!url.contains("nonce") && !url.contains("device"));
        let parsed = reqwest::Url::parse(&url).unwrap();
        let pairs: std::collections::BTreeMap<_, _> = parsed.query_pairs().into_owned().collect();
        assert_eq!(pairs["response_type"], "code");
        assert_eq!(pairs["client_id"], CLIENT_ID);
        assert_eq!(pairs["redirect_uri"], "http://127.0.0.1:1455/callback");
        assert_eq!(pairs["scope"], SCOPE);
        assert_eq!(pairs["code_challenge"], "challenge-value");
        assert_eq!(pairs["code_challenge_method"], "S256");
        assert_eq!(pairs["state"], "state-value");
        assert_eq!(pairs["referrer"], PROXY_CLIENT_NAME);
        assert!(!PROXY_CLIENT_NAME.is_empty());
        let _env = TestEnv::new();
        assert_eq!(configured_url(RESPONSES_URL_ENV, RESPONSES_URL).unwrap(), RESPONSES_URL);
        assert_eq!(configured_url(MODELS_URL_ENV, MODELS_URL).unwrap(), MODELS_URL);
        assert_eq!(configured_url(MODALITIES_URL_ENV, MODALITIES_URL).unwrap(), MODALITIES_URL);
    }

    #[test]
    fn callback_and_manual_code_rules() {
        assert!(matches!(parse_callback("/callback?code=auth%20code&state=expected", "expected"), Callback::Code(code) if code == "auth code"));
        assert!(matches!(parse_callback("/callback?code=auth&state=other", "expected"), Callback::StateMismatch));
        assert!(matches!(parse_callback("/callback?error=access_denied&state=expected", "expected"), Callback::Denied));
        assert!(matches!(parse_callback("/callback?error=access_denied&state=other", "expected"), Callback::StateMismatch));
        assert!(matches!(parse_callback("/other?code=auth&state=expected", "expected"), Callback::Unrelated));
        assert!(matches!(parse_callback("/callback?code=auth&state=expected#x", "expected"), Callback::Unrelated));
        assert_eq!(accept_manual("  code-1 \n").unwrap(), "code-1");
        assert!(accept_manual("").is_err());
        assert!(accept_manual("has space").is_err());
        assert!(accept_manual("line\nbreak").is_err());
    }

    #[test]
    fn catalog_joins_subscription_and_modalities() {
        let subscription = r#"{"data":[
            {"id":"current-a","model":"current-a","api_backend":"responses","context_window":500123,"max_completion_tokens":32768,"supports_reasoning_effort":true,"reasoning_efforts":[{"value":"xhigh"},{"value":"medium"}]},
            {"id":"current-b","model":"current-b","api_backend":"responses","context_window":480321,"supports_reasoning_effort":true,"reasoning_efforts":[{"value":"provider-next"},{"value":"low"}]},
            {"id":"chat-only","model":"chat-only","api_backend":"chat_completions","context_window":200000,"supports_reasoning_effort":false,"reasoning_efforts":[]}
        ]}"#;
        let modalities = r#"{"models":[
            {"id":"current-a","input_modalities":["text","image"],"output_modalities":["text"]},
            {"id":"current-b","input_modalities":["text"],"output_modalities":["text"]},
            {"id":"chat-only","input_modalities":["text"],"output_modalities":["text"]}
        ]}"#;
        let listed = parse_catalog(subscription, modalities).unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].id, "current-a");
        assert_eq!(listed[0].context_window, 500_123);
        assert_eq!(listed[0].max_output_tokens, 32_768);
        assert_eq!(listed[0].efforts, ["xhigh", "medium"]);
        assert!(listed[0].vision);
        assert_eq!(listed[1].id, "current-b");
        assert_eq!(listed[1].max_output_tokens, 0);
        assert_eq!(listed[1].efforts, ["provider-next", "low"]);
        assert!(!listed[1].vision);
        let modalities = r#"{"models":[{"id":"current","input_modalities":["text"],"output_modalities":["text"]}]}"#;
        let missing_window = r#"{"data":[{"model":"current","api_backend":"responses","supports_reasoning_effort":false,"reasoning_efforts":[]}]}"#;
        let inconsistent = r#"{"data":[{"model":"current","api_backend":"responses","context_window":500000,"supports_reasoning_effort":true,"reasoning_efforts":[]}]}"#;
        assert!(parse_catalog(missing_window, modalities).is_err());
        assert!(parse_catalog(inconsistent, modalities).is_err());
        let valid = r#"{"data":[{"model":"current","api_backend":"responses","context_window":500000,"supports_reasoning_effort":false,"reasoning_efforts":[]}]}"#;
        let missing = r#"{"models":[{"id":"other","input_modalities":["text"],"output_modalities":["text"]}]}"#;
        assert!(parse_catalog(valid, missing).is_err());
        let exact_id = "g".repeat(MAX_MODEL_ID);
        let exact = format!(
            r#"{{"data":[{{"model":"{exact_id}","api_backend":"responses","context_window":1000,"supports_reasoning_effort":false,"reasoning_efforts":[]}}]}}"#
        );
        let exact_modalities = format!(
            r#"{{"models":[{{"id":"{exact_id}","input_modalities":["text"],"output_modalities":["text"]}}]}}"#
        );
        assert_eq!(parse_catalog(&exact, &exact_modalities).unwrap()[0].id.len(), MAX_MODEL_ID);
        let long_id = "g".repeat(MAX_MODEL_ID + 1);
        let long = format!(
            r#"{{"data":[{{"model":"{long_id}","api_backend":"responses","context_window":1000,"supports_reasoning_effort":false,"reasoning_efforts":[]}}]}}"#
        );
        let long_modalities = format!(
            r#"{{"models":[{{"id":"{long_id}","input_modalities":["text"],"output_modalities":["text"]}}]}}"#
        );
        assert!(parse_catalog(&long, &long_modalities).is_err());
        assert!(parse_effort("auto").is_err());
        assert!(parse_effort("bad effort").is_err());
    }

    #[test]
    fn review_body_requires_the_decision_tool() {
        let provider = GrokProvider::new(PathBuf::from("."), "grok-4.20".into(), Some("high".into()));
        let spec = crate::classifier::tool_spec();
        let items = [crate::types::Item::user("Review the pending action.")];
        let body = provider.request_body(Request { system: "Be concise.", items: &items, tools: &[spec] });
        assert_eq!(body["model"], "grok-4.20");
        assert_eq!(body["tool_choice"], "required");
        assert_eq!(body["max_output_tokens"], 2048);
        assert_eq!(body["reasoning"]["effort"], "high");
        assert!(body.get("service_tier").is_none());
        assert_eq!(body["instructions"], "Be concise.");
        let empty = provider.request_body(Request { system: "", items: &[], tools: &[] });
        assert_eq!(empty["instructions"], "You are a helpful assistant.");
        assert_eq!(empty["tool_choice"], "auto");
        assert!(empty.get("service_tier").is_none());
    }

    #[tokio::test]
    async fn browser_login_saves_a_private_session() {
        let access = "access-token";
        let (base, mut requests, task) = server(vec![
            (200, json!({"access_token": access, "refresh_token": "refresh-token", "expires_in": 3600}).to_string()),
            (200, json!({"sub": "acct_test", "email": "user@example.com"}).to_string()),
        ])
        .await;
        let mut env = TestEnv::new();
        env.set(TOKEN_URL_ENV, &format!("{base}/oauth2/token"));
        env.set(USERINFO_URL_ENV, &format!("{base}/oauth2/userinfo"));
        let dir = tempfile::tempdir().unwrap();
        let challenge = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let challenge2 = std::sync::Arc::clone(&challenge);
        let before = chrono::Utc::now().timestamp_millis();
        let browser = std::sync::Arc::new(std::sync::Mutex::new(None));
        let browser2 = std::sync::Arc::clone(&browser);
        let message = login(
            dir.path(),
            |_| {},
            move |url| {
                let url = reqwest::Url::parse(url).unwrap();
                let pairs: std::collections::BTreeMap<_, _> = url.query_pairs().into_owned().collect();
                *challenge2.lock().unwrap() = pairs["code_challenge"].clone();
                let redirect = pairs["redirect_uri"].clone();
                let state = pairs["state"].clone();
                let origin = redirect.trim_end_matches("/callback").to_owned();
                let handle = tokio::spawn(async move {
                    let client = reqwest::Client::new();
                    let favicon = client.get(format!("{origin}/favicon.ico")).send().await.unwrap();
                    assert_eq!(favicon.status(), 404);
                    let preflight = client
                        .request(reqwest::Method::OPTIONS, format!("{origin}/callback"))
                        .header("origin", CORS_ORIGIN)
                        .header("access-control-request-method", "GET")
                        .header("access-control-request-private-network", "true")
                        .send()
                        .await
                        .unwrap();
                    assert_eq!(preflight.status(), 204);
                    let headers = preflight.headers().clone();
                    assert_eq!(headers.get("access-control-allow-origin").unwrap(), CORS_ORIGIN);
                    assert_eq!(headers.get("access-control-allow-private-network").unwrap(), "true");
                    let callback = format!("{redirect}?code=auth-code&state={state}");
                    let response = client.get(callback).header("origin", CORS_ORIGIN).send().await.unwrap();
                    assert_eq!(response.status(), 200);
                    let html = response.text().await.unwrap();
                    assert!(html.contains("<h1>Authorization complete</h1>"));
                    assert!(html.contains("prefers-color-scheme:dark"));
                    assert!(html.contains("nyq"));
                });
                *browser2.lock().unwrap() = Some(handle);
            },
            pending_code(),
        )
        .await
        .unwrap();
        browser.lock().unwrap().take().unwrap().await.unwrap();
        assert_eq!(message, "Signed in to Grok as user@example.com.");
        let token = requests.recv().await.unwrap();
        let userinfo = requests.recv().await.unwrap();
        task.await.unwrap();
        let form = reqwest::Url::parse(&format!("http://localhost/?{}", token.split("\r\n\r\n").nth(1).unwrap())).unwrap();
        let fields: std::collections::BTreeMap<_, _> = form.query_pairs().into_owned().collect();
        assert_eq!(fields["grant_type"], "authorization_code");
        assert_eq!(fields["client_id"], CLIENT_ID);
        assert_eq!(fields["code"], "auth-code");
        assert!(fields["redirect_uri"].starts_with("http://127.0.0.1:"));
        assert!(fields["redirect_uri"].ends_with("/callback"));
        assert_eq!(*challenge.lock().unwrap(), pkce_challenge(&fields["code_verifier"]));
        assert!(token.to_ascii_lowercase().contains("application/x-www-form-urlencoded"));
        let userinfo = userinfo.to_ascii_lowercase();
        assert!(userinfo.starts_with("get /oauth2/userinfo "));
        assert!(userinfo.contains(&format!("authorization: bearer {access}")));
        let saved: Value = serde_json::from_slice(&std::fs::read(auth_path(dir.path())).unwrap()).unwrap();
        assert_eq!(saved["version"], 1);
        assert_eq!(saved["access_token"], access);
        assert_eq!(saved["refresh_token"], "refresh-token");
        assert_eq!(saved["account_id"], "acct_test");
        assert!(saved["expires_at_ms"].as_i64().unwrap() >= before + 3_600_000);
    }

    #[tokio::test]
    async fn pasted_code_skips_the_browser_and_mismatch_fails() {
        let mut env = TestEnv::new();
        let (base, mut requests, task) = server(vec![
            (200, json!({"access_token": "a", "refresh_token": "r", "expires_in": 10}).to_string()),
            (200, json!({"sub": "acct_paste"}).to_string()),
        ])
        .await;
        env.set(TOKEN_URL_ENV, &format!("{base}/token"));
        env.set(USERINFO_URL_ENV, &format!("{base}/userinfo"));
        env.set("NYQ_NO_OPEN_BROWSER", "1");
        let dir = tempfile::tempdir().unwrap();
        let opened = AtomicBool::new(false);
        let message = login(
            dir.path(),
            |url| assert!(url.contains("/oauth2/authorize?")),
            |_| opened.store(true, Ordering::SeqCst),
            async { Ok(Some("pasted-code".into())) },
        )
        .await
        .unwrap();
        assert_eq!(message, "Signed in to Grok.");
        assert!(!opened.load(Ordering::SeqCst));
        let token = requests.recv().await.unwrap();
        let _ = requests.recv().await.unwrap();
        task.await.unwrap();
        assert!(token.contains("code=pasted-code"));
        assert!(accept_manual("bad code").is_err());
        std::env::remove_var("NYQ_NO_OPEN_BROWSER");

        let slot = std::sync::Arc::new(std::sync::Mutex::new(None));
        let slot2 = std::sync::Arc::clone(&slot);
        let err = login(
            dir.path(),
            |_| {},
            move |url| {
                let url = reqwest::Url::parse(url).unwrap();
                let pairs: std::collections::BTreeMap<_, _> = url.query_pairs().into_owned().collect();
                let redirect = pairs["redirect_uri"].clone();
                let handle = tokio::spawn(async move {
                    let response = reqwest::get(format!("{redirect}?code=x&state=nope")).await.unwrap();
                    assert_eq!(response.status(), 400);
                    assert!(response.text().await.unwrap().contains("Authorization failed"));
                });
                *slot2.lock().unwrap() = Some(handle);
            },
            pending_code(),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("state mismatch"));
        slot.lock().unwrap().take().unwrap().await.unwrap();
    }

    #[tokio::test]
    async fn logout_revokes_then_deletes() {
        let mut env = TestEnv::new();
        let (base, mut requests, task) = server(vec![(200, String::new()), (500, "no".into())]).await;
        env.set(REVOKE_URL_ENV, &format!("{base}/revoke"));
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(logout(dir.path()).await.unwrap(), "No Grok sign-in stored by nyq.");
        save_session(dir.path(), &sample(1234)).unwrap();
        assert_eq!(logout(dir.path()).await.unwrap(), "Signed out of Grok.");
        let request = requests.recv().await.unwrap();
        assert!(request.contains("token=refresh"));
        assert!(request.contains(&format!("client_id={CLIENT_ID}")));
        assert!(!auth_path(dir.path()).exists());
        save_session(dir.path(), &sample(1234)).unwrap();
        let message = logout(dir.path()).await.unwrap();
        assert!(message.contains("Revoking the login failed"));
        assert!(!auth_path(dir.path()).exists());
        let _ = requests.recv().await.unwrap();
        task.await.unwrap();
    }

    #[tokio::test]
    async fn stream_sends_subscription_headers_and_refreshes() {
        let mut env = TestEnv::new();
        let sse_body = events(&[
            json!({"type":"response.output_item.done","output_index":0,"item":{"type":"message","content":[{"type":"output_text","text":"hi"}]}}),
            json!({"type":"response.completed","response":{"usage":{"input_tokens":4,"output_tokens":1}}}),
        ]);
        let (base, mut requests, task) = server(vec![
            (401, String::new()),
            (200, json!({"access_token": "new-access", "expires_in": 3600}).to_string()),
            (200, json!({"sub": "acct_test"}).to_string()),
            (200, sse_body),
        ])
        .await;
        env.set(RESPONSES_URL_ENV, &format!("{base}/v1/responses"));
        env.set(TOKEN_URL_ENV, &format!("{base}/oauth2/token"));
        env.set(USERINFO_URL_ENV, &format!("{base}/oauth2/userinfo"));
        let dir = tempfile::tempdir().unwrap();
        let mut session = sample(chrono::Utc::now().timestamp_millis() + 3_600_000);
        session.access_token = "old-access".into();
        session.refresh_token = "keep-refresh".into();
        save_session(dir.path(), &session).unwrap();
        let mut provider = GrokProvider::new(dir.path().to_path_buf(), "grok-4".into(), Some("medium".into()));
        provider.set_conversation("conv-1");
        let response = provider.stream(Request { system: "sys", items: &[], tools: &[] }, &mut |_| {}).await.unwrap();
        assert_eq!(response.items, vec![crate::types::Item::Assistant { text: "hi".into() }]);
        let denied = requests.recv().await.unwrap();
        let refresh = requests.recv().await.unwrap();
        let _userinfo = requests.recv().await.unwrap();
        let ok = requests.recv().await.unwrap();
        task.await.unwrap();
        assert!(denied.to_ascii_lowercase().starts_with("post /v1/responses "));
        assert!(refresh.contains("grant_type=refresh_token"));
        assert!(refresh.contains("refresh_token=keep-refresh"));
        assert!(refresh.to_ascii_lowercase().contains("application/x-www-form-urlencoded"));
        let ok_l = ok.to_ascii_lowercase();
        assert!(ok_l.contains("authorization: bearer new-access"));
        assert!(ok_l.contains("x-xai-token-auth: xai-grok-cli"));
        assert!(ok_l.contains("x-authenticateresponse: authenticate-response"));
        assert!(ok_l.contains("x-grok-client-version: 1.0.6"));
        assert!(ok_l.contains(&format!("x-grok-client-identifier: {}", PROXY_CLIENT_NAME.to_ascii_lowercase())));
        assert!(ok_l.contains("x-grok-model-override: grok-4"));
        assert!(ok_l.contains("x-grok-user-id: acct_test"));
        assert!(ok_l.contains("x-grok-conv-id: conv-1"));
        let sent: Value = serde_json::from_str(ok.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(sent["model"], "grok-4");
        assert_eq!(sent["reasoning"]["effort"], "medium");
        assert!(sent.get("service_tier").is_none());
        let saved: Value = serde_json::from_slice(&std::fs::read(auth_path(dir.path())).unwrap()).unwrap();
        assert_eq!(saved["access_token"], "new-access");
        assert_eq!(saved["refresh_token"], "keep-refresh");
        assert_eq!(saved["account_id"], "acct_test");
    }

    #[tokio::test]
    async fn refresh_rejects_an_account_change_and_lists_models() {
        let mut env = TestEnv::new();
        let (base, mut requests, task) = server(vec![
            (200, json!({"access_token": "other", "expires_in": 60}).to_string()),
            (200, json!({"sub": "acct_other"}).to_string()),
            (200, json!({"data":[{"model":"current-a","api_backend":"responses","context_window":500123,"max_completion_tokens":32768,"supports_reasoning_effort":true,"reasoning_efforts":[{"value":"high"},{"value":"low"}]}]}).to_string()),
            (200, json!({"models":[{"id":"current-a","input_modalities":["text","image"],"output_modalities":["text"]}]}).to_string()),
        ])
        .await;
        env.set(TOKEN_URL_ENV, &format!("{base}/oauth2/token"));
        env.set(USERINFO_URL_ENV, &format!("{base}/oauth2/userinfo"));
        env.set(MODELS_URL_ENV, &format!("{base}/v1/models"));
        env.set(MODALITIES_URL_ENV, &format!("{base}/v1/language-models"));
        let dir = tempfile::tempdir().unwrap();
        save_session(dir.path(), &sample(0)).unwrap();
        let provider = GrokProvider::new(dir.path().to_path_buf(), DEFAULT_MODEL.into(), None);
        let err = provider.stream(Request { system: "s", items: &[], tools: &[] }, &mut |_| {}).await.unwrap_err();
        assert!(err.to_string().contains("account changed"));
        let saved: Value = serde_json::from_slice(&std::fs::read(auth_path(dir.path())).unwrap()).unwrap();
        assert_eq!(saved["access_token"], "access");
        let _ = requests.recv().await;
        let _ = requests.recv().await;

        save_session(dir.path(), &sample(chrono::Utc::now().timestamp_millis() + 3_600_000)).unwrap();
        let provider = GrokProvider::new(dir.path().to_path_buf(), DEFAULT_MODEL.into(), None);
        let choice = provider.list_models().await.unwrap();
        assert_eq!(choice.len(), 1);
        assert_eq!(choice[0].provider, "grok");
        assert_eq!(choice[0].model, "current-a");
        assert_eq!(choice[0].context_window, Some(500_123));
        assert_eq!(choice[0].efforts, ["high", "low"]);
        assert!(choice[0].price.is_none());
        let models = requests.recv().await.unwrap().to_ascii_lowercase();
        let modalities = requests.recv().await.unwrap().to_ascii_lowercase();
        task.await.unwrap();
        assert!(models.starts_with("get /v1/models "));
        assert!(models.contains("x-userid: acct_test"));
        assert!(models.contains("x-xai-token-auth: xai-grok-cli"));
        assert!(models.contains("authorization: bearer access"));
        assert!(modalities.starts_with("get /v1/language-models "));
        assert!(!modalities.contains("x-userid"));
        assert!(!modalities.contains("x-xai-token-auth"));
        assert!(modalities.contains("authorization: bearer access"));

        let built = crate::models::build(
            dir.path(),
            &crate::models::ModelChoice {
                provider: "grok".into(),
                model: "current-a".into(),
                context_window: Some(500_123),
                efforts: vec!["high".into(), "low".into()],
                price: None,
            },
            Some("low".into()),
        )
        .unwrap();
        assert_eq!((built.name(), built.model(), built.effort(), built.context_window()), ("grok", "current-a", Some("low"), Some(500_123)));
        assert_eq!(built.efforts(), ["high".to_string(), "low".to_string()]);
        assert_eq!(crate::config::provider(dir.path(), Some("grok"), None).unwrap().model(), DEFAULT_MODEL);
    }
}
