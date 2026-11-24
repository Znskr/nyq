//! Shared server-sent-events helpers for streaming providers.

use anyhow::Result;
use serde_json::Value;

/// A non-success HTTP reply from a model endpoint.
#[derive(Debug)]
pub struct HttpError {
    pub status: u16,
    /// The `Retry-After` header, when given in seconds.
    pub retry_after: Option<std::time::Duration>,
    /// A subscription-plan quota window (Zalthar `plan_quota_exceeded`):
    /// retrying before the reset cannot help, whatever `Retry-After` says.
    pub plan_quota: bool,
    pub message: String,
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for HttpError {}

/// A terminal provider reply carrying no usable content; retried like a 5xx.
#[derive(Debug)]
pub struct EmptyStream {
    pub message: String,
}

impl std::fmt::Display for EmptyStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for EmptyStream {}

pub async fn checked(response: reqwest::Response) -> Result<reqwest::Response> {
    let status = response.status();
    if !status.is_success() {
        let retry_after = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map(std::time::Duration::from_secs);
        let quota_reset = response
            .headers()
            .get("x-zalthar-quota-reset")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let body = response.text().await?;
        let (plan_quota, message) =
            match quota_message(status, &body, quota_reset.as_deref(), retry_after) {
                Some(message) => (true, message),
                None => (false, http_error(status, &body)),
            };
        return Err(HttpError {
            status: status.as_u16(),
            retry_after,
            plan_quota,
            message,
        }
        .into());
    }
    Ok(response)
}

/// The reset time of a `plan_quota_exceeded` reply: the
/// `x-zalthar-quota-reset` header (epoch seconds or RFC 3339) or
/// `Retry-After`, phrased relative to now.
fn quota_reset(reset: Option<&str>, retry_after: Option<std::time::Duration>) -> Option<String> {
    let at = reset.and_then(|v| {
        let v = v.trim();
        v.parse::<i64>()
            .ok()
            .and_then(|s| chrono::DateTime::from_timestamp(s, 0))
            .map(|d| d.with_timezone(&chrono::Utc))
            .or_else(|| {
                chrono::DateTime::parse_from_rfc3339(v)
                    .ok()
                    .map(|d| d.with_timezone(&chrono::Utc))
            })
    });
    if let Some(at) = at {
        let now = chrono::Utc::now();
        return Some(if at > now {
            format!(
                "in {}",
                crate::provider::zalthar::describe_duration(
                    (at - now).to_std().unwrap_or_default()
                )
            )
        } else {
            format!("at {}", at.format("%Y-%m-%d %H:%M UTC"))
        });
    }
    retry_after.map(|d| format!("in {}", crate::provider::zalthar::describe_duration(d)))
}

/// "… · plan quota resets in 4h 12m" for a `plan_quota_exceeded` body; None
/// for any other error.
fn quota_message(
    status: reqwest::StatusCode,
    body: &str,
    reset: Option<&str>,
    retry_after: Option<std::time::Duration>,
) -> Option<String> {
    let parsed = serde_json::from_str::<Value>(body).ok()?;
    if parsed["error"]["code"].as_str() != Some("plan_quota_exceeded") {
        return None;
    }
    Some(match quota_reset(reset, retry_after) {
        Some(at) => format!("{} · plan quota resets {at}", http_error(status, body)),
        None => format!(
            "{} · plan quota resets at an unknown time",
            http_error(status, body)
        ),
    })
}

/// "API access denied · HTTP 403 · Provider: X · code: message" from a JSON
/// error body; other bodies are shown as an excerpt after the status.
pub fn http_error(status: reqwest::StatusCode, body: &str) -> String {
    let code = status.as_u16();
    let parsed = serde_json::from_str::<Value>(body).ok();
    let Some(error) = parsed
        .as_ref()
        .map(|v| &v["error"])
        .filter(|e| e.is_object() || e.is_string())
    else {
        let excerpt: String = body.trim().chars().take(4096).collect();
        return if excerpt.is_empty() {
            format!("HTTP {status}")
        } else {
            format!("HTTP {status}: {excerpt}")
        };
    };
    let text = |v: &Value| {
        v.as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    let message = text(error).or_else(|| text(&error["message"]));
    let kind = text(&error["code"]).or_else(|| text(&error["type"]));
    let provider = text(&error["provider"]);
    let title = if matches!(code, 401 | 403) {
        "API access denied"
    } else {
        "API request failed"
    };
    let mut out = format!("{title} · HTTP {code}");
    if let Some(p) = provider {
        out.push_str(&format!(" · Provider: {p}"));
    }
    match (kind, message) {
        (Some(k), Some(m)) if k != m => out.push_str(&format!(" · {k}: {m}")),
        (Some(t), _) | (None, Some(t)) => out.push_str(&format!(" · {t}")),
        (None, None) => {}
    }
    out.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(1024)
        .collect()
}

pub fn string(value: &Value, key: &str) -> String {
    value[key].as_str().unwrap_or_default().to_owned()
}

pub fn failure(value: &Value) -> String {
    value
        .pointer("/response/error/message")
        .or_else(|| value.pointer("/error/message"))
        .or_else(|| value.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("provider returned an error")
        .to_owned()
}

#[cfg(test)]
pub mod tests {
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        sync::mpsc,
    };

    pub static ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

    pub struct Env;
    impl Env {
        pub fn set(key: &str, value: &str) {
            std::env::set_var(key, value);
        }
    }
    impl Drop for Env {
        fn drop(&mut self) {
            std::env::remove_var("NYQ_CODEX_URL");
            std::env::remove_var("NYQ_CODEX_TOKEN_URL");
        }
    }

    pub fn events(values: &[serde_json::Value]) -> String {
        values
            .iter()
            .map(|v| format!("data: {v}\r\n\r\n"))
            .collect()
    }

    pub async fn server(
        replies: Vec<(u16, String)>,
    ) -> (
        String,
        mpsc::UnboundedReceiver<String>,
        tokio::task::JoinHandle<()>,
    ) {
        server_with_headers(
            replies
                .into_iter()
                .map(|(s, b)| (s, String::new(), b))
                .collect(),
        )
        .await
    }

    /// Like [`server`], with extra raw header lines (`"Name: value\r\n"`) per reply.
    pub async fn server_with_headers(
        replies: Vec<(u16, String, String)>,
    ) -> (
        String,
        mpsc::UnboundedReceiver<String>,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (tx, rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            for (status, headers, body) in replies {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                loop {
                    let mut buf = [0; 2048];
                    let n = socket.read(&mut buf).await.unwrap();
                    assert!(n > 0);
                    bytes.extend_from_slice(&buf[..n]);
                    if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                        let header = String::from_utf8_lossy(&bytes[..end]);
                        let length = header
                            .lines()
                            .find_map(|line| {
                                line.split_once(':')
                                    .filter(|(key, _)| key.eq_ignore_ascii_case("content-length"))
                                    .map(|(_, value)| value.trim().parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        if bytes.len() >= end + 4 + length {
                            break;
                        }
                    }
                }
                tx.send(String::from_utf8(bytes).unwrap()).unwrap();
                socket.write_all(format!("HTTP/1.1 {status} Test\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n{headers}Connection: close\r\n\r\n", body.len()).as_bytes()).await.unwrap();
                // Split through SSE framing and UTF-8 boundaries, as real transports can.
                for chunk in body.as_bytes().chunks(7) {
                    socket.write_all(chunk).await.unwrap();
                    tokio::task::yield_now().await;
                }
            }
        });
        (url, rx, task)
    }
}

#[cfg(test)]
mod parsing_tests {
    use eventsource_stream::Eventsource;
    use futures::StreamExt;
    use reqwest::StatusCode;

    #[test]
    fn http_errors() {
        let e = |status: u16, body: &str| {
            super::http_error(StatusCode::from_u16(status).unwrap(), body)
        };
        assert_eq!(
            e(403, r#"{"error":{"code":"model_not_allowed","message":"this key may not use the model","provider":"anthropic"}}"#),
            "API access denied · HTTP 403 · Provider: anthropic · model_not_allowed: this key may not use the model"
        );
        assert_eq!(
            e(401, r#"{"error":{"code":"api_key_required","message":"Set OPENCODE_API_KEY\n to use this endpoint."}}"#),
            "API access denied · HTTP 401 · api_key_required: Set OPENCODE_API_KEY to use this endpoint."
        );
        assert_eq!(
            e(
                529,
                r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#
            ),
            "API request failed · HTTP 529 · overloaded_error: Overloaded"
        );
        assert_eq!(
            e(400, r#"{"error":"bad model"}"#),
            "API request failed · HTTP 400 · bad model"
        );
        assert_eq!(
            e(502, "<html>bad gateway</html>"),
            "HTTP 502 Bad Gateway: <html>bad gateway</html>"
        );
        assert_eq!(e(500, ""), "HTTP 500 Internal Server Error");
        assert_eq!(
            e(500, r#"{"detail":"x"}"#),
            r#"HTTP 500 Internal Server Error: {"detail":"x"}"#
        );
    }

    #[tokio::test]
    async fn fragmented_sse() {
        let wire = ": keepalive\r\nevent: response.output_text.delta\r\ndata: {\"delta\":\"héllo\"}\r\n\r\ndata: [DONE]\n\n";
        let chunks = wire
            .as_bytes()
            .chunks(1)
            .map(|chunk| Ok::<_, std::io::Error>(chunk.to_vec()));
        let mut events = futures::stream::iter(chunks).eventsource();
        let event = events.next().await.unwrap().unwrap();
        assert_eq!(event.event, "response.output_text.delta");
        assert_eq!(event.data, "{\"delta\":\"héllo\"}");
        assert_eq!(events.next().await.unwrap().unwrap().data, "[DONE]");
        assert!(events.next().await.is_none());
    }
}
