//! Signing in: the ChatGPT browser flow for the Codex provider, the Grok
//! subscription browser flow, and API keys for hosted services
//! (`nyq login anthropic|opencode-go|opencode|xai`).
use std::{io::Write, path::Path, time::Duration};

use anyhow::{Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use reqwest::Url;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

use crate::provider::hosted::{self, Service};

const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
/// Callback ports registered for the client, in order of preference.
const CALLBACK_PORTS: [u16; 2] = [1455, 1457];

fn redirect(port: u16) -> String {
    format!("http://localhost:{port}/auth/callback")
}

fn secret() -> String {
    let mut bytes = Vec::with_capacity(32);
    bytes.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
    bytes.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
    URL_SAFE_NO_PAD.encode(bytes)
}

// SHA-256 for PKCE, kept here because the crate has no direct hash dependency.
fn challenge(verifier: &str) -> String {
    use sha2::{Digest, Sha256};
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

fn authorize(issuer: &str, redirect_uri: &str, state: &str, verifier: &str) -> Result<Url> {
    let mut url = Url::parse(&format!("{}/oauth/authorize", issuer.trim_end_matches('/')))?;
    url.query_pairs_mut().extend_pairs([
        ("response_type", "code"),
        ("client_id", CLIENT_ID),
        ("redirect_uri", redirect_uri),
        ("scope", "openid profile email offline_access"),
        ("code_challenge", &challenge(verifier)),
        ("code_challenge_method", "S256"),
        ("id_token_add_organizations", "true"),
        ("codex_cli_simplified_flow", "true"),
        ("state", state),
        ("originator", "nyq"),
    ]);
    Ok(url)
}

fn callback(target: &str, state: &str) -> Result<Option<String>> {
    anyhow::ensure!(target.starts_with('/'), "invalid callback target");
    let url = Url::parse(&format!("http://localhost{target}"))?;
    if url.path() != "/auth/callback" {
        return Ok(None);
    }
    let pairs: Vec<_> = url.query_pairs().collect();
    let field = |key: &str| -> Result<Option<&str>> {
        let values: Vec<_> = pairs.iter().filter(|(k, _)| k == key).collect();
        anyhow::ensure!(values.len() <= 1, "duplicate callback parameter");
        Ok(values.first().map(|(_, v)| v.as_ref()))
    };
    anyhow::ensure!(
        field("state")? == Some(state),
        "OAuth callback state mismatch"
    );
    anyhow::ensure!(
        field("error")?.is_none(),
        "ChatGPT sign-in was denied or failed"
    );
    let code = field("code")?
        .filter(|c| !c.is_empty())
        .context("OAuth callback missing code")?;
    Ok(Some(code.to_owned()))
}

async fn wait_callback(listener: &TcpListener, state: &str) -> Result<String> {
    loop {
        let (mut socket, _) = listener.accept().await?;
        let request = tokio::time::timeout(Duration::from_secs(5), async {
            let mut bytes = Vec::new();
            loop {
                let byte = socket.read_u8().await?;
                bytes.push(byte);
                anyhow::ensure!(bytes.len() <= 8192, "callback request too large");
                if bytes.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            Ok::<_, anyhow::Error>(bytes)
        })
        .await;
        let Ok(Ok(bytes)) = request else {
            continue;
        };
        let request = String::from_utf8_lossy(&bytes);
        let mut parts = request
            .lines()
            .next()
            .unwrap_or_default()
            .split_whitespace();
        let result = if parts.next() == Some("GET") {
            callback(parts.next().unwrap_or_default(), state)
        } else {
            Ok(None)
        };
        let (status, body) = match &result {
            Ok(Some(_)) => ("200 OK", "Signed in — you can close this tab"),
            Ok(None) => ("404 Not Found", "Waiting for sign-in"),
            Err(_) => (
                "400 Bad Request",
                "Sign-in failed. Return to your terminal.",
            ),
        };
        let html = format!(
            "<!doctype html><meta charset=\"utf-8\"><title>ChatGPT sign-in</title><p>{body}</p>"
        );
        let response = format!("HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n\r\n{html}", html.len());
        let _ = tokio::time::timeout(
            Duration::from_secs(5),
            socket.write_all(response.as_bytes()),
        )
        .await;
        if let Some(code) = result? {
            return Ok(code);
        }
    }
}

fn claims(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?).ok()
}

#[derive(Deserialize)]
struct Tokens {
    access_token: String,
    refresh_token: String,
    id_token: String,
    expires_in: i64,
}

/// Sign in through a browser and atomically save `home/chatgpt-auth.json`.
#[allow(dead_code)]
pub async fn login(home: &Path, open_browser: impl Fn(&str)) -> Result<String> {
    let issuer =
        std::env::var("NYQ_CODEX_ISSUER").unwrap_or_else(|_| "https://auth.openai.com".into());
    login_with_issuer(home, open_browser, &issuer).await
}

/// Listen on the first free callback port.
async fn bind_callback(ports: &[u16]) -> Result<(TcpListener, u16)> {
    for &port in ports {
        if let Ok(listener) = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await {
            return Ok((listener, port));
        }
    }
    let list = ports
        .iter()
        .map(u16::to_string)
        .collect::<Vec<_>>()
        .join(" and ");
    anyhow::bail!("cannot listen for the ChatGPT sign-in callback on 127.0.0.1 ports {list}; close any other sign-in using them and retry")
}

async fn login_with_issuer(
    home: &Path,
    open_browser: impl Fn(&str),
    issuer: &str,
) -> Result<String> {
    let (listener, port) = bind_callback(&CALLBACK_PORTS).await?;
    let redirect_uri = redirect(port);
    let verifier = secret();
    let state = secret();
    let url = authorize(issuer, &redirect_uri, &state, &verifier)?;
    open_browser(url.as_str());
    let code = tokio::time::timeout(Duration::from_secs(300), wait_callback(&listener, &state))
        .await
        .context("ChatGPT sign-in timed out after 5 minutes")??;
    let mut form = Url::parse("http://localhost/")?;
    form.query_pairs_mut().extend_pairs([
        ("grant_type", "authorization_code"),
        ("client_id", CLIENT_ID),
        ("code", &code),
        ("redirect_uri", &redirect_uri),
        ("code_verifier", &verifier),
    ]);

    let response = crate::http::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()?
        .post(format!("{}/oauth/token", issuer.trim_end_matches('/')))
        .timeout(Duration::from_secs(30))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(form.query().unwrap_or_default().to_owned())
        .send()
        .await
        .context("exchanging ChatGPT authorization code")?;
    anyhow::ensure!(
        response.status().is_success(),
        "ChatGPT token exchange failed (HTTP {})",
        response.status()
    );
    let tokens: Tokens = response
        .json()
        .await
        .context("invalid ChatGPT token response")?;
    anyhow::ensure!(
        !tokens.access_token.is_empty()
            && !tokens.refresh_token.is_empty()
            && tokens.expires_in > 0,
        "invalid ChatGPT token response"
    );
    let access = claims(&tokens.access_token).context("invalid ChatGPT access token claims")?;
    let id = claims(&tokens.id_token);
    let account = access["https://api.openai.com/auth"]["chatgpt_account_id"]
        .as_str()
        .filter(|s| !s.is_empty())
        .context("ChatGPT token missing account ID")?;
    let auth = json!({"version":1,"access_token":tokens.access_token,"refresh_token":tokens.refresh_token,
        "expires_at_ms":chrono::Utc::now().timestamp_millis().saturating_add(tokens.expires_in.saturating_mul(1000)),"account_id":account});
    save(home, &auth)?;
    Ok(match id.as_ref().and_then(|v| v["email"].as_str()) {
        Some(email) => format!("Signed in to ChatGPT as {email}."),
        None => "Signed in to ChatGPT.".into(),
    })
}

fn save(home: &Path, auth: &Value) -> Result<()> {
    std::fs::create_dir_all(home)?;
    let temp = home.join(format!(".chatgpt-auth-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temp)?;
        file.write_all(&serde_json::to_vec(auth)?)?;
        file.sync_all()?;
        crate::tools::safeio::rename_over(&temp, &home.join("chatgpt-auth.json"))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result.context("saving ChatGPT sign-in")
}

/// Whether sign-in flows must not launch a browser: `NYQ_NO_OPEN_BROWSER`
/// (any non-empty value but `0`) is set, or stdin is not a terminal. Sign-in
/// URLs are printed either way.
pub fn browser_disabled() -> bool {
    use std::io::IsTerminal;
    std::env::var("NYQ_NO_OPEN_BROWSER").is_ok_and(|v| !v.is_empty() && v != "0")
        || !std::io::stdin().is_terminal()
}

/// What to print before a terminal sign-in opens `url`.
pub fn sign_in_prompt(url: &str) -> String {
    if browser_disabled() {
        format!(
            "Open this URL in a browser to sign in:
  {url}"
        )
    } else {
        format!(
            "Opening your browser to sign in. If it doesn't open, visit:
  {url}"
        )
    }
}

/// Open the system browser (unless [`browser_disabled`]); launch failures
/// are intentionally ignored.
#[allow(dead_code)]
pub fn open_browser(url: &str) {
    if browser_disabled() {
        return;
    }
    #[cfg(target_os = "windows")]
    {
        // `cmd /c start` splits on `&`, which OAuth URLs contain.
        open_browser_windows(url);
    }
    #[cfg(not(target_os = "windows"))]
    {
        let mut command;
        #[cfg(target_os = "macos")]
        {
            command = std::process::Command::new("open");
            command.arg(url);
        }
        #[cfg(not(target_os = "macos"))]
        {
            command = std::process::Command::new("xdg-open");
            command.arg(url);
        }
        if let Ok(mut child) = command
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            let _ = std::thread::Builder::new()
                .name("browser-launch".into())
                .spawn(move || {
                    let _ = child.wait();
                });
        }
    }
}

#[cfg(target_os = "windows")]
fn open_browser_windows(url: &str) {
    use std::os::windows::ffi::OsStrExt;
    let file: Vec<u16> = std::ffi::OsStr::new(url)
        .encode_wide()
        .chain(Some(0))
        .collect();
    let op: Vec<u16> = std::ffi::OsStr::new("open")
        .encode_wide()
        .chain(Some(0))
        .collect();
    #[link(name = "shell32")]
    extern "system" {
        fn ShellExecuteW(
            hwnd: *mut std::ffi::c_void,
            operation: *const u16,
            file: *const u16,
            params: *const u16,
            dir: *const u16,
            show: i32,
        ) -> isize;
    }
    // SAFETY: both strings are NUL-terminated UTF-16. A result ≤ 32 is a
    // launch failure, which is ignored the same way a missing `open` is.
    unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            op.as_ptr(),
            file.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            1,
        );
    }
}

/// `nyq login <service>`: read an API key from the terminal (hidden) or
/// stdin and store it in `<home>/auth.json`.
pub async fn login_key(home: &Path, service: Service) -> Result<String> {
    eprintln!("{} and press Enter:", key_prompt(service));
    let key = read_secret()?;
    store_key(home, service, &key).await
}

/// "Paste your xAI API key from https://console.x.ai"; for an
/// Anthropic-compatible endpoint, the endpoint instead.
pub fn key_prompt(service: Service) -> String {
    let base = service.base_url();
    if service == Service::Anthropic && base != hosted::ANTHROPIC_URL {
        return format!("Paste the API key for the Anthropic-compatible endpoint {base}");
    }
    format!(
        "Paste your {} API key from {}",
        service.title(),
        key_site(service)
    )
}

/// Where to get an API key for `service`.
pub fn key_site(service: Service) -> &'static str {
    match service {
        Service::OpencodeGo | Service::Opencode => "https://opencode.ai/auth",
        Service::Xai => "https://console.x.ai",
        Service::Anthropic => "https://console.anthropic.com/settings/keys",
    }
}

/// Check `key` with `service` and save it, noting when an environment key
/// takes precedence. A rejected key is not saved; one that could not be
/// checked is saved with a warning.
pub async fn store_key(home: &Path, service: Service, key: &str) -> Result<String> {
    store_key_at(home, service, key, &service.base_url()).await
}

async fn store_key_at(home: &Path, service: Service, key: &str, base_url: &str) -> Result<String> {
    anyhow::ensure!(!key.trim().is_empty(), "the API key is empty");
    let warning = match hosted::check_key(service, base_url, key).await {
        hosted::KeyCheck::Accepted => None,
        hosted::KeyCheck::Refused(e) => anyhow::bail!(
            "{} rejected this API key, so it was not saved: {e}",
            service.title()
        ),
        hosted::KeyCheck::Unknown(e) => Some(format!(
            "\nWarning: could not check the key with {} ({e}); saved it anyway.",
            service.title()
        )),
    };
    hosted::save_key(home, service, key)?;
    let mut msg = format!(
        "Saved your {} key in {}.",
        service.title(),
        home.join("auth.json").display()
    );
    msg.extend(warning);
    if hosted::api_key(home, service).as_deref() != Some(key.trim()) {
        let other = match service {
            Service::OpencodeGo | Service::Opencode => {
                "OPENCODE_API_KEY or the OpenCode CLI's login"
            }
            Service::Anthropic => "ANTHROPIC_API_KEY or ANTHROPIC_AUTH_TOKEN",
            Service::Xai => service.key_env(),
        };
        msg.push_str(&format!("\nNote: {other} is set and takes precedence."));
    }
    Ok(msg)
}

/// Browser sign-in for a Grok subscription. `manual` is a pasted code, or
/// `None` when stdin closes without one. The URL is always printed;
/// `NYQ_NO_OPEN_BROWSER` skips opening it.
pub async fn login_grok(
    home: &Path,
    manual: impl std::future::Future<Output = Result<Option<String>>>,
) -> Result<String> {
    crate::provider::grok::login(
        home,
        |url| {
            crate::ui::info(&format!(
                "Sign in to Grok in your browser (waiting up to 5 minutes):\n  {url}"
            ));
        },
        open_browser,
        manual,
    )
    .await
}

/// `nyq logout <provider>`: forget nyq's stored credentials.
pub async fn logout(home: &Path, provider: &str) -> Result<String> {
    if provider.eq_ignore_ascii_case("grok") {
        return crate::provider::grok::logout(home).await;
    }
    if provider == "codex" {
        let path = home.join("chatgpt-auth.json");
        return Ok(match std::fs::remove_file(&path) {
            Ok(()) => "Signed out of ChatGPT.".into(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                "No ChatGPT sign-in stored by nyq.".into()
            }
            Err(e) => return Err(e).context("removing chatgpt-auth.json"),
        });
    }
    let service = Service::parse(provider)
        .with_context(|| format!("unknown provider `{provider}` (expected codex, grok, anthropic, opencode-go, opencode or xai)"))?;
    Ok(if hosted::remove_key(home, service)? {
        format!("Removed the stored {} key.", service.title())
    } else {
        format!("No {} key stored by nyq.", service.title())
    })
}

/// `nyq logout` without a provider: forget every sign-in nyq stored.
pub async fn logout_all(home: &Path) -> Result<String> {
    let mut done = Vec::new();
    if home.join("chatgpt-auth.json").is_file() {
        done.push(logout(home, "codex").await?);
    }
    if crate::provider::grok::auth_path(home).is_file() {
        done.push(crate::provider::grok::logout(home).await?);
    }
    for service in Service::ALL {
        if hosted::remove_key(home, service)? {
            done.push(format!("Removed the stored {} key.", service.title()));
        }
    }
    Ok(if done.is_empty() {
        "No sign-ins stored by nyq.".into()
    } else {
        done.join("\n")
    })
}

fn read_secret() -> Result<String> {
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() {
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        return Ok(line.trim().to_owned());
    }
    use crossterm::event::{read, Event, KeyCode, KeyEventKind, KeyModifiers};
    crossterm::terminal::enable_raw_mode()?;
    let result = (|| -> Result<String> {
        let mut key = String::new();
        loop {
            match read()? {
                Event::Paste(text) => key.push_str(&text),
                Event::Key(k) if k.kind != KeyEventKind::Release => match k.code {
                    KeyCode::Enter => return Ok(key),
                    KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                        anyhow::bail!("cancelled")
                    }
                    KeyCode::Esc => anyhow::bail!("cancelled"),
                    KeyCode::Backspace => {
                        key.pop();
                    }
                    KeyCode::Char(c) => key.push(c),
                    _ => {}
                },
                _ => {}
            }
        }
    })();
    let _ = crossterm::terminal::disable_raw_mode();
    eprintln!();
    Ok(result?.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn logout_all_forgets_every_stored_sign_in() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        assert_eq!(
            logout_all(home).await.unwrap(),
            "No sign-ins stored by nyq."
        );
        std::fs::write(home.join("chatgpt-auth.json"), "{}").unwrap();
        hosted::save_key(home, Service::Xai, "xai-key").unwrap();
        let msg = logout_all(home).await.unwrap();
        assert!(msg.contains("Signed out of ChatGPT."), "{msg}");
        assert!(msg.contains(Service::Xai.title()), "{msg}");
        assert!(!home.join("chatgpt-auth.json").exists());
        assert_eq!(
            logout_all(home).await.unwrap(),
            "No sign-ins stored by nyq."
        );
    }

    #[test]
    fn pkce() {
        let verifier = secret();
        assert!((43..=128).contains(&verifier.len()));
        assert!(verifier
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-._~".contains(&b)));
        assert_ne!(verifier, secret());
        assert_eq!(
            challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn authorize_params() {
        let url = authorize(
            "https://auth.openai.com/",
            &redirect(1455),
            "state",
            "verifier",
        )
        .unwrap();
        assert_eq!(url.path(), "/oauth/authorize");
        let params: std::collections::BTreeMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(params.len(), 10);
        for (k, v) in [
            ("response_type", "code"),
            ("client_id", CLIENT_ID),
            ("redirect_uri", "http://localhost:1455/auth/callback"),
            ("scope", "openid profile email offline_access"),
            ("code_challenge_method", "S256"),
            ("id_token_add_organizations", "true"),
            ("codex_cli_simplified_flow", "true"),
            ("state", "state"),
            ("originator", "nyq"),
        ] {
            assert_eq!(params[k], v);
        }
        assert_eq!(params["code_challenge"], challenge("verifier"));
        assert_eq!(url.host_str(), Some("auth.openai.com"));
    }

    #[test]
    fn callback_params() {
        assert_eq!(
            callback("/auth/callback?code=a%2Bb&state=s", "s").unwrap(),
            Some("a+b".into())
        );
        assert!(callback("/auth/callback?code=c&state=wrong", "s")
            .unwrap_err()
            .to_string()
            .contains("state mismatch"));
        assert!(callback("/auth/callback?error=access_denied&state=s", "s").is_err());
        assert!(callback("/auth/callback?code=c", "s").is_err());
        assert!(callback("/auth/callback?code=c&state=s&state=s", "s").is_err());
        assert_eq!(callback("/favicon.ico", "s").unwrap(), None);
    }

    #[tokio::test]
    async fn callback_port_falls_back() {
        let free = || std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let busy = free();
        let spare = free().local_addr().unwrap().port();
        let ports = [busy.local_addr().unwrap().port(), spare];
        let (_listener, port) = bind_callback(&ports).await.unwrap();
        assert_eq!(port, spare);
        let err = bind_callback(&ports).await.unwrap_err().to_string();
        assert!(
            err.contains(&format!("ports {} and {spare}", ports[0])),
            "{err}"
        );
    }

    #[tokio::test]
    async fn keys_are_checked_before_saving() {
        use crate::provider::sse::tests::server;
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let stored = || {
            hosted::read_own(home)
                .unwrap()
                .get("xai")
                .map(|v| v["key"].clone())
        };
        let (url, mut requests, task) = server(vec![
            (
                401,
                r#"{"error":{"code":"invalid_api_key","message":"Incorrect API key"}}"#.into(),
            ),
            (200, r#"{"data":[]}"#.into()),
            (
                402,
                r#"{"error":{"code":"payment_required","message":"Add a payment method"}}"#.into(),
            ),
        ])
        .await;
        let err = store_key_at(home, Service::Xai, "bad", &url)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("rejected")
                && err.contains("HTTP 401 · invalid_api_key: Incorrect API key"),
            "{err}"
        );
        assert_eq!(stored(), None);
        let request = requests.recv().await.unwrap().to_lowercase();
        assert!(
            request.starts_with("get /models ") && request.contains("authorization: bearer bad"),
            "{request}"
        );
        let msg = store_key_at(home, Service::Xai, " good ", &url)
            .await
            .unwrap();
        assert!(
            msg.starts_with("Saved your xAI key") && !msg.contains("Warning"),
            "{msg}"
        );
        assert_eq!(stored(), Some(json!("good")));
        // A status that says nothing about the key: saved with a warning.
        let msg = store_key_at(home, Service::Xai, "maybe", &url)
            .await
            .unwrap();
        assert!(
            msg.contains("Warning: could not check the key") && msg.contains("HTTP 402"),
            "{msg}"
        );
        assert_eq!(stored(), Some(json!("maybe")));
        task.await.unwrap();
        // Unreachable: saved with a warning.
        let msg = store_key_at(home, Service::Xai, "offline", "http://127.0.0.1:1")
            .await
            .unwrap();
        assert!(msg.contains("Warning"), "{msg}");
        assert_eq!(stored(), Some(json!("offline")));
        assert!(store_key_at(home, Service::Xai, " ", &url).await.is_err());
    }

    #[tokio::test]
    async fn anthropic_keys_are_checked_with_anthropic_headers() {
        use crate::provider::sse::tests::server;
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let (url, mut requests, task) = server(vec![
            (401, r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#.into()),
            (200, r#"{"data":[]}"#.into()),
        ])
        .await;
        let base = format!("{url}/v1");
        let err = store_key_at(home, Service::Anthropic, "sk-ant-bad", &base)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("Anthropic API rejected this API key")
                && err.contains("invalid x-api-key"),
            "{err}"
        );
        let request = requests.recv().await.unwrap().to_lowercase();
        assert!(request.starts_with("get /v1/models "), "{request}");
        for header in ["x-api-key: sk-ant-bad", "anthropic-version: 2023-06-01"] {
            assert!(request.contains(header), "{header}");
        }
        assert!(!request.contains("authorization"));
        let msg = store_key_at(home, Service::Anthropic, "sk-ant-good", &base)
            .await
            .unwrap();
        assert!(msg.starts_with("Saved your Anthropic API key"), "{msg}");
        assert_eq!(
            hosted::read_own(home).unwrap()["anthropic"]["key"],
            json!("sk-ant-good")
        );
        task.await.unwrap();
        assert!(logout(home, "claude")
            .await
            .unwrap()
            .contains("Anthropic API"));
        assert!(hosted::read_own(home).unwrap().get("anthropic").is_none());
    }

    #[test]
    fn atomic_auth_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("nested");
        save(&home, &json!({"version":1,"access_token":"old"})).unwrap();
        let expected = json!({"version":1,"access_token":"new","refresh_token":"refresh", "expires_at_ms":123,"account_id":"account"});
        save(&home, &expected).unwrap();
        let path = home.join("chatgpt-auth.json");
        assert_eq!(
            serde_json::from_slice::<Value>(&std::fs::read(&path).unwrap()).unwrap(),
            expected
        );
        assert_eq!(std::fs::read_dir(home).unwrap().count(), 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    fn jwt(value: Value) -> String {
        format!(
            "e30.{}.signature",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&value).unwrap())
        )
    }

    #[tokio::test]
    async fn full_flow() {
        let endpoint = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let issuer = format!("http://{}", endpoint.local_addr().unwrap());
        let access =
            jwt(json!({"https://api.openai.com/auth":{"chatgpt_account_id":"account-test"}}));
        let expected_access = access.clone();
        let server = tokio::spawn(async move {
            let (mut socket, _) = endpoint.accept().await.unwrap();
            let mut bytes = Vec::new();
            while !bytes.ends_with(b"\r\n\r\n") {
                bytes.push(socket.read_u8().await.unwrap());
            }
            let headers = String::from_utf8(bytes).unwrap();
            assert!(headers.starts_with("POST /oauth/token HTTP/1.1"));
            assert!(headers
                .to_ascii_lowercase()
                .contains("application/x-www-form-urlencoded"));
            let len: usize = headers
                .lines()
                .find_map(|line| {
                    let (key, val) = line.split_once(':')?;
                    key.eq_ignore_ascii_case("content-length")
                        .then(|| val.trim().parse().unwrap())
                })
                .unwrap();
            let mut body = vec![0; len];
            socket.read_exact(&mut body).await.unwrap();
            let form = Url::parse(&format!(
                "http://localhost/?{}",
                String::from_utf8(body).unwrap()
            ))
            .unwrap();
            let fields: std::collections::BTreeMap<_, _> =
                form.query_pairs().into_owned().collect();
            assert_eq!(fields["grant_type"], "authorization_code");
            assert_eq!(fields["code"], "fake-code");
            assert_eq!(fields["client_id"], CLIENT_ID);
            assert!(CALLBACK_PORTS
                .map(redirect)
                .contains(&fields["redirect_uri"]));
            let body = json!({"access_token":access,"refresh_token":"refresh-test",
                "id_token":jwt(json!({"email":"test@example.com"})),"expires_in":3600})
            .to_string();
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            fields["code_verifier"].clone()
        });
        let dir = tempfile::tempdir().unwrap();
        let challenge_seen = std::sync::Mutex::new(String::new());
        let before = chrono::Utc::now().timestamp_millis();
        let message = login_with_issuer(
            dir.path(),
            |url| {
                let url = Url::parse(url).unwrap();
                let fields: std::collections::BTreeMap<_, _> =
                    url.query_pairs().into_owned().collect();
                *challenge_seen.lock().unwrap() = fields["code_challenge"].clone();
                let mut callback = Url::parse(&fields["redirect_uri"]).unwrap();
                callback.set_host(Some("127.0.0.1")).unwrap();
                callback
                    .query_pairs_mut()
                    .extend_pairs([("code", "fake-code"), ("state", fields["state"].as_str())]);
                let favicon = callback.join("/favicon.ico").unwrap();
                tokio::spawn(async move {
                    // Browsers may request unrelated resources before the callback.
                    assert_eq!(reqwest::get(favicon).await.unwrap().status(), 404);
                    let html = reqwest::get(callback).await.unwrap().text().await.unwrap();
                    assert!(html.contains("Signed in — you can close this tab"));
                });
            },
            &issuer,
        )
        .await
        .unwrap();
        assert!(message.contains("test@example.com"));
        let verifier = server.await.unwrap();
        assert_eq!(*challenge_seen.lock().unwrap(), challenge(&verifier));
        let path = dir.path().join("chatgpt-auth.json");
        let saved: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved.as_object().unwrap().len(), 5);
        assert_eq!(saved["version"], 1);
        assert_eq!(saved["access_token"], expected_access);
        assert_eq!(saved["refresh_token"], "refresh-test");
        assert_eq!(saved["account_id"], "account-test");
        assert!(saved["expires_at_ms"].as_i64().unwrap() >= before + 3_600_000);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}
