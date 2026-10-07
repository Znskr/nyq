//! Browser sign-in for the ChatGPT-subscription Codex provider.
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

const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const REDIRECT: &str = "http://localhost:1455/auth/callback";

fn secret() -> String {
    let mut bytes = Vec::with_capacity(32);
    bytes.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
    bytes.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
    URL_SAFE_NO_PAD.encode(bytes)
}

// SHA-256 for PKCE, kept here because the crate has no direct hash dependency.
fn challenge(verifier: &str) -> String {
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
    let mut data = verifier.as_bytes().to_vec();
    let bits = (data.len() as u64) * 8;
    data.push(0x80);
    while data.len() % 64 != 56 {
        data.push(0);
    }
    data.extend_from_slice(&bits.to_be_bytes());
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    for block in data.chunks_exact(64) {
        let mut w = [0u32; 64];
        for (i, word) in block.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes(word.try_into().unwrap());
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add((e & f) ^ (!e & g))
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let t2 = s0.wrapping_add((a & b) ^ (a & c) ^ (b & c));
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (x, y) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *x = x.wrapping_add(y);
        }
    }
    URL_SAFE_NO_PAD.encode(h.into_iter().flat_map(u32::to_be_bytes).collect::<Vec<_>>())
}

fn authorize(issuer: &str, state: &str, verifier: &str) -> Result<Url> {
    let mut url = Url::parse(&format!("{}/oauth/authorize", issuer.trim_end_matches('/')))?;
    url.query_pairs_mut().extend_pairs([
        ("response_type", "code"),
        ("client_id", CLIENT_ID),
        ("redirect_uri", REDIRECT),
        ("scope", "openid profile email offline_access"),
        ("code_challenge", &challenge(verifier)),
        ("code_challenge_method", "S256"),
        ("id_token_add_organizations", "true"),
        ("codex_cli_simplified_flow", "true"),
        ("state", state),
        ("originator", "fx"),
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

async fn login_with_issuer(
    home: &Path,
    open_browser: impl Fn(&str),
    issuer: &str,
) -> Result<String> {
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 1455)).await
        .context("cannot bind ChatGPT sign-in callback on 127.0.0.1:1455; close any other sign-in using this port and retry")?;
    let verifier = secret();
    let state = secret();
    let url = authorize(issuer, &state, &verifier)?;
    open_browser(url.as_str());
    let code = tokio::time::timeout(Duration::from_secs(300), wait_callback(&listener, &state))
        .await
        .context("ChatGPT sign-in timed out after 5 minutes")??;
    let mut form = Url::parse("http://localhost/")?;
    form.query_pairs_mut().extend_pairs([
        ("grant_type", "authorization_code"),
        ("client_id", CLIENT_ID),
        ("code", &code),
        ("redirect_uri", REDIRECT),
        ("code_verifier", &verifier),
    ]);

    let response = reqwest::Client::builder()
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
        std::fs::rename(&temp, home.join("chatgpt-auth.json"))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result.context("saving ChatGPT sign-in")
}

/// Open the system browser; launch failures are intentionally ignored.
#[allow(dead_code)]
pub fn open_browser(url: &str) {
    let mut command;
    #[cfg(target_os = "windows")]
    {
        command = std::process::Command::new("cmd");
        command.args(["/c", "start", "", url]);
    }
    #[cfg(target_os = "macos")]
    {
        command = std::process::Command::new("open");
        command.arg(url);
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
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

#[cfg(test)]
mod tests {
    use super::*;

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
        let url = authorize("https://auth.openai.com/", "state", "verifier").unwrap();
        assert_eq!(url.path(), "/oauth/authorize");
        let params: std::collections::BTreeMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(params.len(), 10);
        for (k, v) in [
            ("response_type", "code"),
            ("client_id", CLIENT_ID),
            ("redirect_uri", REDIRECT),
            ("scope", "openid profile email offline_access"),
            ("code_challenge_method", "S256"),
            ("id_token_add_organizations", "true"),
            ("codex_cli_simplified_flow", "true"),
            ("state", "state"),
            ("originator", "fx"),
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
            assert_eq!(fields["redirect_uri"], REDIRECT);
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
                let mut callback = Url::parse(REDIRECT).unwrap();
                callback.set_host(Some("127.0.0.1")).unwrap();
                callback
                    .query_pairs_mut()
                    .extend_pairs([("code", "fake-code"), ("state", fields["state"].as_str())]);
                tokio::spawn(async move {
                    // Browsers may request unrelated resources before the callback.
                    assert_eq!(
                        reqwest::get("http://127.0.0.1:1455/favicon.ico")
                            .await
                            .unwrap()
                            .status(),
                        404
                    );
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
