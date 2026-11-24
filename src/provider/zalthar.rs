//! nyq Pro — Zalthar's $10/month subscription bundling nyq with a fast
//! DeepSeek Flash V4.1 model (`nyq-pro/deepseek-flash`) on the Zalthar
//! gateway (`{api}/v1`, OpenAI-compatible chat completions).
//!
//! `nyq login zalthar` signs in through the browser: a loopback callback on
//! a random 127.0.0.1 port, PKCE (S256) and a state check, then a JSON token
//! exchange at `{api}/api/cli/token` that mints an API key. The key and the
//! returned `base_url`/`default_model` are stored in `<home>/auth.json`
//! (mode 0600) like the other hosted-service keys; `nyq login zalthar
//! --api-key` pastes a console key instead, and `nyq logout zalthar` removes
//! it. `GET {api}/api/cli/whoami` reports the plan and its usage windows.
//!
//! Over a plan window's limit the gateway answers HTTP 429 with error code
//! `plan_quota_exceeded`, `Retry-After` and `x-zalthar-quota-reset`; nyq
//! surfaces the reset time instead of retrying (see `sse::checked`).
//!
//! Test and development overrides: `NYQ_ZALTHAR_APP_URL` (authorize page),
//! `NYQ_ZALTHAR_API_URL` (token exchange, whoami and the default chat base),
//! `NYQ_ZALTHAR_BASE_URL` and `NYQ_ZALTHAR_MODEL`.

use std::{net::Ipv4Addr, path::Path, time::Duration};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

use super::hosted::{self, Service};

/// The Zalthar console: browser authorization, keys and billing.
pub const APP_URL: &str = "https://app.zalthar.com";
/// The Zalthar API root: `/api/cli/*` for CLI endpoints, `/v1` for the
/// OpenAI-compatible gateway.
pub const API_URL: &str = "https://api.zalthar.com";
/// The model a nyq Pro subscription serves.
pub const DEFAULT_MODEL: &str = "nyq-pro/deepseek-flash";
/// The bundled model's context window.
pub const CONTEXT_WINDOW: u64 = 1_000_000;
/// The bundled model's output cap.
pub const MAX_OUTPUT_TOKENS: u64 = 65_536;

const CLIENT: &str = "nyq";
const CALLBACK_PATH: &str = "/callback";
const LOGIN_TIMEOUT: Duration = Duration::from_secs(5 * 60);

const APP_URL_ENV: &str = "NYQ_ZALTHAR_APP_URL";
const API_URL_ENV: &str = "NYQ_ZALTHAR_API_URL";
const BASE_URL_ENV: &str = "NYQ_ZALTHAR_BASE_URL";
const MODEL_ENV: &str = "NYQ_ZALTHAR_MODEL";

fn env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
}

/// The console origin for the authorize page (`NYQ_ZALTHAR_APP_URL`).
pub fn app_url() -> String {
    env(APP_URL_ENV).unwrap_or_else(|| APP_URL.into())
}

/// The API origin for CLI endpoints (`NYQ_ZALTHAR_API_URL`).
pub fn api_url() -> String {
    env(API_URL_ENV).unwrap_or_else(|| API_URL.into())
}

fn cli_url(api: &str, endpoint: &str) -> String {
    format!("{}/api/cli/{endpoint}", api.trim_end_matches('/'))
}

/// The whoami endpoint for a chat `base_url` (`{api}/v1` →
/// `{api}/api/cli/whoami`), used to check a key in `nyq login zalthar
/// --api-key` and by the hosted-service key check.
pub fn whoami_url(base_url: &str) -> String {
    let root = base_url.trim_end_matches('/');
    cli_url(root.strip_suffix("/v1").unwrap_or(root), "whoami")
}

/// The OpenAI-compatible chat base: `NYQ_ZALTHAR_BASE_URL`, then the
/// `base_url` the token exchange returned, then `{api}/v1`.
pub fn chat_base_url(home: &Path) -> String {
    if let Some(url) = env(BASE_URL_ENV) {
        return url.trim_end_matches('/').to_owned();
    }
    if let Some(url) = stored(home).and_then(|s| s.base_url) {
        return url;
    }
    format!("{}/v1", api_url().trim_end_matches('/'))
}

/// The model zalthar starts with: `NYQ_ZALTHAR_MODEL`, then the
/// `default_model` the token exchange returned, then [`DEFAULT_MODEL`].
pub fn default_model(home: &Path) -> String {
    env(MODEL_ENV)
        .or_else(|| stored(home).and_then(|s| s.default_model))
        .unwrap_or_else(|| DEFAULT_MODEL.into())
}

/// The account details `nyq login zalthar` stored in `<home>/auth.json`
/// under `"zalthar"` (the API key itself is read through
/// [`hosted::api_key`], honouring `ZALTHAR_API_KEY`).
#[derive(Clone, Debug, Default)]
pub struct Stored {
    #[cfg_attr(not(test), allow(dead_code))]
    pub key: String,
    pub key_id: Option<String>,
    pub org_id: Option<String>,
    pub org_name: Option<String>,
    pub base_url: Option<String>,
    pub default_model: Option<String>,
}

pub fn stored(home: &Path) -> Option<Stored> {
    let all = hosted::read_own(home).ok()?;
    let entry = all.get(Service::Zalthar.id())?;
    let key = entry["key"].as_str()?.to_owned();
    let field = |name: &str| {
        entry[name]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    Some(Stored {
        key,
        key_id: field("key_id"),
        org_id: field("org_id"),
        org_name: field("org_name"),
        base_url: field("base_url"),
        default_model: field("default_model"),
    })
}

/// The key the provider sends: `ZALTHAR_API_KEY`, else the stored one.
pub fn api_key(home: &Path) -> Option<String> {
    hosted::api_key(home, Service::Zalthar)
}

/// A name for `device_name` on the authorize page: hostname when readable.
fn device_name() -> String {
    let name = env("HOSTNAME")
        .or_else(|| env("COMPUTERNAME"))
        .or_else(|| {
            std::fs::read_to_string("/etc/hostname")
                .ok()
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "nyq".into());
    name.chars().take(64).collect()
}

/// `{app}/cli/authorize` with the client, loopback callback, state and the
/// S256 challenge.
fn authorize_url(callback: &str, state: &str, challenge: &str) -> Result<String> {
    let mut url = reqwest::Url::parse(&format!(
        "{}/cli/authorize",
        app_url().trim_end_matches('/')
    ))?;
    url.query_pairs_mut().extend_pairs([
        ("client", CLIENT),
        ("callback", callback),
        ("state", state),
        ("code_challenge", challenge),
        ("device_name", device_name().as_str()),
    ]);
    Ok(url.into())
}

enum Callback {
    Code(String),
    /// Another path (favicon, an unrelated probe): keep waiting.
    Unrelated,
    /// `error=access_denied` (or any error) on a verified state.
    Denied,
    /// Wrong or missing state, or a broken request.
    Rejected(String),
}

fn callback(target: &str, state: &str) -> Callback {
    let Ok(url) = reqwest::Url::parse(&format!("http://localhost{target}")) else {
        return Callback::Rejected("malformed callback request".into());
    };
    if url.path() != CALLBACK_PATH {
        return Callback::Unrelated;
    }
    let pairs: Vec<_> = url.query_pairs().collect();
    let field = |key: &str| -> Result<Option<String>> {
        let values: Vec<_> = pairs.iter().filter(|(k, _)| k == key).collect();
        anyhow::ensure!(values.len() <= 1, "duplicate callback parameter");
        Ok(values.first().map(|(_, v)| v.to_string()))
    };
    let read = || -> Result<Callback> {
        anyhow::ensure!(
            field("state")?.as_deref() == Some(state),
            "OAuth callback state mismatch"
        );
        if field("error")?.is_some() {
            return Ok(Callback::Denied);
        }
        let code = field("code")?
            .filter(|c| !c.is_empty())
            .context("OAuth callback missing code")?;
        Ok(Callback::Code(code))
    };
    match read() {
        Ok(result) => result,
        Err(e) => Callback::Rejected(format!("{e}")),
    }
}

/// Serve the loopback callback until a `GET /callback` arrives; browsers get
/// a small page either way.
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
            Callback::Unrelated
        };
        let (status, body) = match &result {
            Callback::Code(_) => ("200 OK", "Signed in to Zalthar — you can close this tab."),
            Callback::Unrelated => ("404 Not Found", "Waiting for sign-in."),
            Callback::Denied => (
                "400 Bad Request",
                "Sign-in was denied. Return to your terminal.",
            ),
            Callback::Rejected(_) => (
                "400 Bad Request",
                "Sign-in failed. Return to your terminal.",
            ),
        };
        let html = format!(
            "<!doctype html><meta charset=\"utf-8\"><title>Zalthar sign-in</title><p>{body}</p>"
        );
        let response = format!("HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n\r\n{html}", html.len());
        let _ = tokio::time::timeout(
            Duration::from_secs(5),
            socket.write_all(response.as_bytes()),
        )
        .await;
        match result {
            Callback::Code(code) => return Ok(code),
            Callback::Denied => {
                bail!("Zalthar sign-in was denied")
            }
            Callback::Rejected(e) => bail!("Zalthar sign-in failed: {e}"),
            Callback::Unrelated => {}
        }
    }
}

/// The token exchange result (`POST {api}/api/cli/token`).
#[derive(Debug, Deserialize)]
pub struct Account {
    pub api_key: String,
    pub key_id: String,
    pub org_id: String,
    pub org_name: String,
    pub base_url: String,
    pub default_model: String,
    #[serde(default)]
    pub plan: Option<Value>,
}

async fn exchange(api: &str, code: &str, verifier: &str) -> Result<Account> {
    let response = crate::http::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()?
        .post(cli_url(api, "token"))
        .timeout(Duration::from_secs(30))
        .header(reqwest::header::USER_AGENT, crate::config::USER_AGENT)
        .json(&json!({"code": code, "code_verifier": verifier, "client": CLIENT}))
        .send()
        .await
        .context("exchanging the Zalthar authorization code")?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        bail!(
            "Zalthar token exchange failed · {}",
            super::sse::http_error(status, &body)
        );
    }
    let account: Account = response
        .json()
        .await
        .context("invalid Zalthar token response")?;
    anyhow::ensure!(
        !account.api_key.trim().is_empty(),
        "invalid Zalthar token response"
    );
    Ok(account)
}

/// Write the `zalthar` entry of `<home>/auth.json` (mode 0600).
fn save_entry(home: &Path, entry: Value) -> Result<()> {
    let mut all = hosted::read_own(home)?;
    all.insert(Service::Zalthar.id().into(), entry);
    crate::config::write_private(&home.join("auth.json"), &serde_json::to_vec_pretty(&all)?)
        .context("saving the Zalthar sign-in")
}

/// Store the minted key and its account metadata in `<home>/auth.json`,
/// under the same entry the provider reads.
fn save(home: &Path, account: &Account) -> Result<()> {
    save_entry(
        home,
        json!({
            "type": "api",
            "key": account.api_key.trim(),
            "key_id": account.key_id,
            "org_id": account.org_id,
            "org_name": account.org_name,
            "base_url": account.base_url.trim_end_matches('/'),
            "default_model": account.default_model,
        }),
    )
}

/// "Signed in to Zalthar (Acme) · nyq Pro (active) · nyq-pro/deepseek-flash."
fn signed_in(account: &Account) -> String {
    let org = account.org_name.trim();
    let mut msg = if org.is_empty() {
        "Signed in to Zalthar.".to_string()
    } else {
        format!("Signed in to Zalthar ({org}).")
    };
    match &account.plan {
        Some(plan) => {
            let name = plan["name"].as_str().or_else(|| plan["code"].as_str());
            let status = plan["status"].as_str();
            match (name, status) {
                (Some(n), Some(s)) => msg.push_str(&format!(" Plan: {n} ({s}).")),
                (Some(n), None) => msg.push_str(&format!(" Plan: {n}.")),
                _ => {}
            }
        }
        None => msg.push_str(&format!(
            " No nyq Pro plan on this account — subscribe at {APP_URL}/billing."
        )),
    }
    let model = match account.default_model.trim() {
        "" => DEFAULT_MODEL,
        m => m,
    };
    msg.push_str(&format!(" Default model: {model}."));
    msg
}

/// `nyq login zalthar`: browser sign-in with PKCE and a loopback callback on
/// a random port. `open_browser` is called once with the authorize URL;
/// whether it launches a browser is the caller's choice (`--no-browser`,
/// `NYQ_NO_OPEN_BROWSER`). Times out after five minutes.
pub async fn login(home: &Path, open_browser: impl FnOnce(&str)) -> Result<String> {
    login_at(home, open_browser, &api_url(), LOGIN_TIMEOUT).await
}

async fn login_at(
    home: &Path,
    open_browser: impl FnOnce(&str),
    api: &str,
    timeout: Duration,
) -> Result<String> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .context("cannot listen for the Zalthar sign-in callback on 127.0.0.1")?;
    let port = listener.local_addr()?.port();
    let callback_url = format!("http://127.0.0.1:{port}{CALLBACK_PATH}");
    let verifier = crate::login::secret();
    let state = crate::login::secret();
    let url = authorize_url(&callback_url, &state, &crate::login::challenge(&verifier))?;
    open_browser(&url);
    let code = tokio::time::timeout(timeout, wait_callback(&listener, &state))
        .await
        .context("Zalthar sign-in timed out waiting for the browser callback")??;
    let account = exchange(api, &code, &verifier).await?;
    let message = signed_in(&account);
    save(home, &account)?;
    Ok(message)
}

/// `nyq login zalthar --api-key`: check a pasted key with whoami, then store
/// it with its account metadata. A refused key is not saved; a key that
/// could not be checked is saved with a warning.
pub async fn login_key(home: &Path, key: &str) -> Result<String> {
    login_key_at(home, key, &api_url()).await
}

async fn login_key_at(home: &Path, key: &str, api: &str) -> Result<String> {
    anyhow::ensure!(!key.trim().is_empty(), "the API key is empty");
    let who = match whoami_at(api, key).await {
        Ok(who) => who,
        Err(e)
            if e.chain().any(|c| {
                c.downcast_ref::<super::sse::HttpError>()
                    .is_some_and(|h| matches!(h.status, 401 | 403))
            }) =>
        {
            bail!("Zalthar rejected this API key, so it was not saved: {e:#}")
        }
        Err(e) => {
            hosted::save_key(home, Service::Zalthar, key)?;
            return Ok(format!(
                "Saved your Zalthar key in {}.\nWarning: could not check the key with Zalthar ({e:#}); saved it anyway.",
                home.join("auth.json").display()
            ));
        }
    };
    // Whoami carries no gateway details; keep the ones a browser sign-in
    // stored rather than dropping them on a key refresh.
    let prev = stored(home).unwrap_or_default();
    let mut entry = json!({
        "type": "api",
        "key": key.trim(),
        "key_id": who.key_id,
        "org_id": who.org_id,
        "org_name": who.org_name,
    });
    for (field, value) in [
        ("base_url", prev.base_url),
        ("default_model", prev.default_model),
    ] {
        if let Some(value) = value {
            entry[field] = json!(value);
        }
    }
    save_entry(home, entry)?;
    Ok(format!(
        "Signed in to Zalthar.\n{}",
        whoami_lines(&who, Some(chrono::Utc::now())).join("\n")
    ))
}

/// `msg` plus the live plan and usage windows from whoami; `msg` alone when
/// whoami cannot be reached.
pub async fn with_plan_lines(home: &Path, msg: String) -> String {
    match tokio::time::timeout(Duration::from_secs(3), whoami(home)).await {
        Ok(Ok(who)) => format!(
            "{msg}\n{}",
            whoami_lines(&who, Some(chrono::Utc::now())).join("\n")
        ),
        _ => msg,
    }
}

/// `GET {api}/api/cli/whoami` — the account and plan behind a key.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Whoami {
    pub org_id: String,
    pub org_name: String,
    pub key_id: String,
    #[serde(default)]
    pub plan: Option<Plan>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Plan {
    pub code: String,
    pub name: String,
    pub status: String,
    #[serde(default)]
    pub windows: Vec<Window>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Window {
    pub name: String,
    pub used_requests: u64,
    pub limit_requests: u64,
    pub used_tokens: u64,
    pub limit_tokens: u64,
    pub resets_at: String,
}

async fn whoami_at(api: &str, key: &str) -> Result<Whoami> {
    let response = crate::http::client()
        .get(cli_url(api, "whoami"))
        .header(reqwest::header::USER_AGENT, crate::config::USER_AGENT)
        .bearer_auth(key.trim())
        .timeout(Duration::from_secs(8))
        .send()
        .await
        .context("asking Zalthar who this key belongs to")?;
    super::sse::checked(response)
        .await?
        .json()
        .await
        .context("invalid Zalthar whoami response")
}

/// Who the configured Zalthar key belongs to (stored key or `ZALTHAR_API_KEY`).
pub async fn whoami(home: &Path) -> Result<Whoami> {
    let key = api_key(home).context("no Zalthar sign-in: run `nyq login zalthar`")?;
    whoami_at(&api_url(), &key).await
}

/// A duration as a short phrase: `4h 12m`, `45s`, `2d 3h`.
pub fn describe_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs >= 86_400 {
        format!("{}d {}h", secs / 86_400, secs % 86_400 / 3600)
    } else if secs >= 3_600 {
        format!("{}h {}m", secs / 3600, secs % 3600 / 60)
    } else if secs >= 60 {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}

/// A compact count: `600`, `4.0M`, `25M`.
fn count(n: u64) -> String {
    if n >= 1_000_000 {
        let m = n as f64 / 1_000_000.0;
        let s = format!("{m:.1}");
        format!("{}M", s.trim_end_matches(".0"))
    } else if n >= 10_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

/// One usage window as a phrase: `5h window: 12/600 requests · 1.2k/4M
/// tokens · resets 2026-11-24 18:30 UTC` (the timestamp is passed through;
/// an epoch number is shown as UTC).
pub fn describe_window(window: &Window, now: Option<chrono::DateTime<chrono::Utc>>) -> String {
    let reset = window
        .resets_at
        .trim()
        .parse::<i64>()
        .ok()
        .and_then(|s| chrono::DateTime::from_timestamp(s, 0))
        .or_else(|| {
            chrono::DateTime::parse_from_rfc3339(window.resets_at.trim())
                .ok()
                .map(|d| d.with_timezone(&chrono::Utc))
        });
    let resets = match (reset, now) {
        (Some(at), Some(now)) if at > now => {
            format!(
                "resets in {}",
                describe_duration((at - now).to_std().unwrap_or_default())
            )
        }
        (Some(at), _) => format!("resets {}", at.format("%Y-%m-%d %H:%M UTC")),
        (None, _) => format!("resets {}", window.resets_at.trim()),
    };
    format!(
        "{}: {}/{} requests · {}/{} tokens · {resets}",
        window.name,
        window.used_requests,
        count(window.limit_requests),
        count(window.used_tokens),
        count(window.limit_tokens),
    )
}

/// Human lines for `nyq status`: the org and, with a plan, its status and
/// each usage window.
pub fn whoami_lines(who: &Whoami, now: Option<chrono::DateTime<chrono::Utc>>) -> Vec<String> {
    let mut lines = vec![format!("{} ({})", who.org_name, who.org_id)];
    match &who.plan {
        Some(plan) => {
            let name = if plan.name.trim().is_empty() {
                &plan.code
            } else {
                &plan.name
            };
            lines.push(format!("  plan: {name} ({})", plan.status));
            for window in &plan.windows {
                lines.push(format!("  {}", describe_window(window, now)));
            }
        }
        None => lines.push(format!("  no plan — {APP_URL}/billing")),
    }
    lines
}

/// The `zalthar` section of `nyq status` (None when no key is configured).
/// A live whoami; an unreachable API reports the stored account instead.
pub async fn status(home: &Path) -> Option<Value> {
    api_key(home)?;
    match tokio::time::timeout(Duration::from_secs(5), whoami(home)).await {
        Ok(Ok(who)) => Some(serde_json::to_value(who).unwrap_or_default()),
        result => {
            let error = match result {
                Ok(Err(e)) => format!("{e:#}"),
                Ok(Ok(_)) => unreachable!(),
                Err(_) => "timed out".to_string(),
            };
            let mut value = json!({"error": error});
            if let Some(stored) = stored(home) {
                for (key, field) in [
                    ("key_id", stored.key_id),
                    ("org_id", stored.org_id),
                    ("org_name", stored.org_name),
                ] {
                    if let Some(v) = field {
                        value[key] = json!(v);
                    }
                }
            }
            Some(value)
        }
    }
}

/// Text rendering of the `zalthar` value [`status`] produces.
pub fn status_lines(value: &Value) -> Vec<String> {
    if let Ok(who) = serde_json::from_value::<Whoami>(value.clone()) {
        return whoami_lines(&who, Some(chrono::Utc::now()));
    }
    let mut lines = Vec::new();
    if let Some(name) = value["org_name"].as_str().filter(|n| !n.is_empty()) {
        lines.push(match value["org_id"].as_str() {
            Some(id) => format!("{name} ({id})"),
            None => name.to_string(),
        });
    }
    if let Some(error) = value["error"].as_str() {
        lines.push(format!("whoami failed: {error}"));
    }
    if lines.is_empty() {
        lines.push("signed in".into());
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::sse::tests::server;
    use serde_json::json;

    #[test]
    fn authorize_params() {
        let url = reqwest::Url::parse(
            &authorize_url("http://127.0.0.1:4321/callback", "state-x", "challenge-y").unwrap(),
        )
        .unwrap();
        assert_eq!(url.path(), "/cli/authorize");
        assert_eq!(url.host_str(), Some("app.zalthar.com"));
        let params: std::collections::BTreeMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(params["client"], "nyq");
        assert_eq!(params["callback"], "http://127.0.0.1:4321/callback");
        assert_eq!(params["state"], "state-x");
        assert_eq!(params["code_challenge"], "challenge-y");
        assert!(!params["device_name"].is_empty());
    }

    #[test]
    fn callback_params() {
        assert!(matches!(
            callback("/callback?code=c&state=s", "s"),
            Callback::Code(c) if c == "c"
        ));
        assert!(matches!(
            callback("/callback?code=c&state=wrong", "s"),
            Callback::Rejected(_)
        ));
        assert!(matches!(
            callback("/callback?error=access_denied&state=s", "s"),
            Callback::Denied
        ));
        assert!(matches!(
            callback("/callback?code=c", "s"),
            Callback::Rejected(_)
        ));
        assert!(matches!(
            callback("/callback?code=c&state=s&state=s", "s"),
            Callback::Rejected(_)
        ));
        assert!(matches!(callback("/favicon.ico", "s"), Callback::Unrelated));
    }

    #[test]
    fn duration_and_window_phrasing() {
        assert_eq!(describe_duration(Duration::from_secs(45)), "45s");
        assert_eq!(describe_duration(Duration::from_secs(12 * 60)), "12m");
        assert_eq!(
            describe_duration(Duration::from_secs(4 * 3600 + 12 * 60)),
            "4h 12m"
        );
        assert_eq!(
            describe_duration(Duration::from_secs(3 * 86_400 + 2 * 3600)),
            "3d 2h"
        );
        let window = Window {
            name: "5h".into(),
            used_requests: 12,
            limit_requests: 600,
            used_tokens: 1_234_567,
            limit_tokens: 4_000_000,
            resets_at: "2026-11-24T18:30:00Z".into(),
        };
        let text = describe_window(&window, None);
        assert_eq!(
            text,
            "5h: 12/600 requests · 1.2M/4M tokens · resets 2026-11-24 18:30 UTC"
        );
        let now = "2026-11-24T14:18:00Z".parse::<chrono::DateTime<chrono::Utc>>();
        assert!(describe_window(&window, now.ok()).contains("resets in 4h 12m"));
    }

    #[test]
    fn stored_account_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        save(
            home,
            &Account {
                api_key: "zk-test".into(),
                key_id: "key_1".into(),
                org_id: "org_1".into(),
                org_name: "Acme".into(),
                base_url: "https://api.zalthar.com/v1/".into(),
                default_model: "nyq-pro/deepseek-flash".into(),
                plan: Some(json!({"name":"nyq Pro","status":"active"})),
            },
        )
        .unwrap();
        let stored = stored(home).unwrap();
        assert_eq!(stored.org_name.as_deref(), Some("Acme"));
        assert_eq!(
            stored.base_url.as_deref(),
            Some("https://api.zalthar.com/v1")
        );
        assert_eq!(
            hosted::api_key(home, Service::Zalthar).as_deref(),
            Some("zk-test")
        );
        let signed = signed_in(&Account {
            api_key: "k".into(),
            key_id: "k".into(),
            org_id: "o".into(),
            org_name: "Acme".into(),
            base_url: "b".into(),
            default_model: "m".into(),
            plan: Some(json!({"name":"nyq Pro","status":"active"})),
        });
        assert_eq!(
            signed,
            "Signed in to Zalthar (Acme). Plan: nyq Pro (active). Default model: m."
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(home.join("auth.json"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[tokio::test]
    async fn exchange_sends_verifier_and_reads_account() {
        let (url, mut requests, task) = server(vec![(
            200,
            json!({
                "api_key":"zk-new","key_id":"key_1","org_id":"org_1","org_name":"Acme",
                "base_url":"https://api.zalthar.com/v1","default_model":"nyq-pro/deepseek-flash",
                "plan":{"code":"nyq-pro","name":"nyq Pro","status":"active"}
            })
            .to_string(),
        )])
        .await;
        let account = exchange(&url, "the-code", "the-verifier").await.unwrap();
        assert_eq!(account.api_key, "zk-new");
        assert_eq!(account.org_name, "Acme");
        let request = requests.recv().await.unwrap();
        let lower = request.to_lowercase();
        assert!(lower.starts_with("post /api/cli/token "));
        let body: Value = serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(
            body,
            json!({"code":"the-code","code_verifier":"the-verifier","client":"nyq"})
        );
        task.await.unwrap();
    }

    #[tokio::test]
    async fn exchange_errors_are_reported() {
        let (url, _requests, task) = server(vec![(
            400,
            r#"{"error":{"code":"invalid_grant","message":"expired code"}}"#.into(),
        )])
        .await;
        let err = exchange(&url, "c", "v").await.unwrap_err().to_string();
        assert!(
            err.contains("token exchange failed") && err.contains("invalid_grant"),
            "{err}"
        );
        task.await.unwrap();
    }

    #[tokio::test]
    async fn whoami_reads_plan_windows() {
        let (url, mut requests, task) = server(vec![(
            200,
            json!({
                "org_id":"org_1","org_name":"Acme","key_id":"key_1",
                "plan":{"code":"nyq-pro","name":"nyq Pro","status":"active","windows":[
                    {"name":"5h","used_requests":12,"limit_requests":600,"used_tokens":1234567,"limit_tokens":4000000,"resets_at":"2026-11-24T18:30:00Z"},
                    {"name":"week","used_requests":100,"limit_requests":4000,"used_tokens":2000000,"limit_tokens":25000000,"resets_at":"2026-11-30T00:00:00Z"}
                ]}
            })
            .to_string(),
        )])
        .await;
        let who = whoami_at(&url, "zk-test").await.unwrap();
        assert_eq!(who.org_name, "Acme");
        let request = requests.recv().await.unwrap().to_lowercase();
        assert!(
            request.starts_with("get /api/cli/whoami ")
                && request.contains("authorization: bearer zk-test"),
            "{request}"
        );
        let lines = whoami_lines(&who, None).join("\n");
        assert!(lines.contains("Acme (org_1)"), "{lines}");
        assert!(lines.contains("plan: nyq Pro (active)"), "{lines}");
        assert!(lines.contains("12/600 requests"), "{lines}");
        assert!(lines.contains("resets 2026-11-24 18:30 UTC"), "{lines}");
        task.await.unwrap();
    }

    #[tokio::test]
    async fn whoami_error_is_an_error() {
        let (url, _requests, task) = server(vec![(
            401,
            r#"{"error":{"code":"invalid_api_key","message":"bad key"}}"#.into(),
        )])
        .await;
        let err = whoami_at(&url, "zk-bad").await.unwrap_err().to_string();
        assert!(
            err.contains("401") && err.contains("invalid_api_key"),
            "{err}"
        );
        task.await.unwrap();
    }

    /// A mock Zalthar API that accepts the token exchange and drives the
    /// browser side of the flow itself.
    #[tokio::test]
    async fn full_login_flow() {
        let (api, mut requests, task) = server(vec![(
            200,
            json!({
                "api_key":"zk-minted","key_id":"key_1","org_id":"org_1","org_name":"Acme",
                "base_url":"https://api.zalthar.com/v1","default_model":"nyq-pro/deepseek-flash",
                "plan":{"name":"nyq Pro","status":"active"}
            })
            .to_string(),
        )])
        .await;
        let dir = tempfile::tempdir().unwrap();
        let challenge_seen = std::sync::Mutex::new(String::new());
        let message = login_at(
            dir.path(),
            |url| {
                let url = reqwest::Url::parse(url).unwrap();
                let params: std::collections::BTreeMap<_, _> =
                    url.query_pairs().into_owned().collect();
                assert_eq!(params["client"], "nyq");
                *challenge_seen.lock().unwrap() = params["code_challenge"].clone();
                let callback = reqwest::Url::parse(&params["callback"]).unwrap();
                let port = callback.port().unwrap();
                let target = format!(
                    "{}?code=login-code&state={}",
                    callback.path(),
                    params["state"]
                );
                tokio::spawn(async move {
                    let status = reqwest::get(format!("http://127.0.0.1:{port}/favicon.ico"))
                        .await
                        .unwrap()
                        .status();
                    assert_eq!(status, 404);
                    let html = reqwest::get(format!("http://127.0.0.1:{port}{target}"))
                        .await
                        .unwrap()
                        .text()
                        .await
                        .unwrap();
                    assert!(html.contains("you can close this tab"), "{html}");
                });
            },
            &api,
            Duration::from_secs(30),
        )
        .await
        .unwrap();
        assert_eq!(
            message,
            "Signed in to Zalthar (Acme). Plan: nyq Pro (active). Default model: nyq-pro/deepseek-flash."
        );
        let request = requests.recv().await.unwrap();
        let body: Value = serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(body["code"], "login-code");
        assert_eq!(body["client"], "nyq");
        let verifier = body["code_verifier"].as_str().unwrap();
        assert_eq!(
            *challenge_seen.lock().unwrap(),
            crate::login::challenge(verifier)
        );
        let stored = stored(dir.path()).unwrap();
        assert_eq!(stored.key, "zk-minted");
        assert_eq!(stored.org_name.as_deref(), Some("Acme"));
        assert_eq!(chat_base_url(dir.path()), "https://api.zalthar.com/v1");
        assert_eq!(default_model(dir.path()), "nyq-pro/deepseek-flash");
        task.await.unwrap();
    }

    #[tokio::test]
    async fn rejected_and_denied_callbacks_fail() {
        for (target, error) in [
            ("/callback?code=c&state=other", "state mismatch"),
            ("/callback?code=c", "state mismatch"),
            ("/callback?error=access_denied&state=expected", "denied"),
        ] {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let (result, response) = tokio::join!(
                wait_callback(&listener, "expected"),
                reqwest::get(format!("http://127.0.0.1:{port}{target}")),
            );
            assert_eq!(response.unwrap().status(), 400, "{target}");
            let err = result.unwrap_err().to_string();
            assert!(err.contains(error), "{err}");
        }
    }

    #[tokio::test]
    async fn sign_in_times_out() {
        let dir = tempfile::tempdir().unwrap();
        let err = login_at(
            dir.path(),
            |_| {},
            "http://127.0.0.1:1",
            Duration::from_millis(50),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("timed out"), "{err}");
        assert!(stored(dir.path()).is_none());
    }

    #[tokio::test]
    async fn pasted_key_stores_whoami_account() {
        let (api, mut requests, task) = server(vec![(
            200,
            json!({
                "org_id":"org_1","org_name":"Acme","key_id":"key_1",
                "plan":{"code":"nyq-pro","name":"nyq Pro","status":"active","windows":[
                    {"name":"5h","used_requests":12,"limit_requests":600,"used_tokens":1234,"limit_tokens":4000000,"resets_at":"2026-11-24T18:30:00Z"}
                ]}
            })
            .to_string(),
        )])
        .await;
        let dir = tempfile::tempdir().unwrap();
        let msg = login_key_at(dir.path(), "zk-pasted", &api).await.unwrap();
        let request = requests.recv().await.unwrap().to_lowercase();
        assert!(
            request.starts_with("get /api/cli/whoami ")
                && request.contains("authorization: bearer zk-pasted"),
            "{request}"
        );
        assert!(msg.contains("Acme (org_1)"), "{msg}");
        assert!(msg.contains("plan: nyq Pro (active)"), "{msg}");
        assert!(msg.contains("12/600 requests"), "{msg}");
        assert!(!msg.contains("zk-pasted"), "{msg}");
        let stored = stored(dir.path()).unwrap();
        assert_eq!(stored.key, "zk-pasted");
        assert_eq!(stored.org_name.as_deref(), Some("Acme"));
        task.await.unwrap();
    }

    #[tokio::test]
    async fn pasted_refused_key_is_not_saved() {
        let (api, _requests, task) = server(vec![(
            401,
            r#"{"error":{"code":"invalid_api_key","message":"bad key"}}"#.into(),
        )])
        .await;
        let dir = tempfile::tempdir().unwrap();
        let err = login_key_at(dir.path(), "zk-bad", &api)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("rejected") && err.contains("invalid_api_key"),
            "{err}"
        );
        assert!(stored(dir.path()).is_none());
        task.await.unwrap();
        // An unreachable API: the key is saved with a warning.
        let msg = login_key_at(dir.path(), "zk-offline", "http://127.0.0.1:1")
            .await
            .unwrap();
        assert!(msg.contains("Warning"), "{msg}");
        assert_eq!(stored(dir.path()).unwrap().key, "zk-offline");
    }
}
