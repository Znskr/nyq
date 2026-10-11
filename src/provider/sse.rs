//! Shared server-sent-events helpers for streaming providers.

use anyhow::Result;
use serde_json::Value;

/// A non-success HTTP reply from a model endpoint.
#[derive(Debug)]
pub struct HttpError {
    pub status: u16,
    /// The `Retry-After` header, when given in seconds.
    pub retry_after: Option<std::time::Duration>,
    pub message: String,
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for HttpError {}

pub async fn checked(response: reqwest::Response) -> Result<reqwest::Response> {
    let status = response.status();
    if !status.is_success() {
        let retry_after = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map(std::time::Duration::from_secs);
        let body = response.text().await?;
        return Err(HttpError { status: status.as_u16(), retry_after, message: http_error(status, &body) }.into());
    }
    Ok(response)
}

/// "API access denied · HTTP 403 · Provider: X · code: message" from a JSON
/// error body; other bodies are shown as an excerpt after the status.
pub fn http_error(status: reqwest::StatusCode, body: &str) -> String {
    let code = status.as_u16();
    let parsed = serde_json::from_str::<Value>(body).ok();
    let Some(error) = parsed.as_ref().map(|v| &v["error"]).filter(|e| e.is_object() || e.is_string()) else {
        let excerpt: String = body.trim().chars().take(4096).collect();
        return if excerpt.is_empty() { format!("HTTP {status}") } else { format!("HTTP {status}: {excerpt}") };
    };
    let text = |v: &Value| v.as_str().map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned);
    let message = text(error).or_else(|| text(&error["message"])).or_else(|| text(&error["param"]["message"]));
    let kind = text(&error["code"]).or_else(|| text(&error["type"])).or_else(|| text(&error["param"]["name"]));
    let provider = text(&error["provider"]).or_else(|| {
        let m = message.as_deref()?;
        let rest = &m[m.find("Providers considered:")? + "Providers considered:".len()..];
        text(&Value::from(rest.split([',', ';']).next()?.trim_end_matches('.')))
    });
    let title = if matches!(code, 401 | 403) { "API access denied" } else { "API request failed" };
    let mut out = format!("{title} · HTTP {code}");
    if let Some(p) = provider {
        out.push_str(&format!(" · Provider: {p}"));
    }
    match (kind, message) {
        (Some(k), Some(m)) if k != m => out.push_str(&format!(" · {k}: {m}")),
        (Some(t), _) | (None, Some(t)) => out.push_str(&format!(" · {t}")),
        (None, None) => {}
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(1024).collect()
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
        server_with_headers(replies.into_iter().map(|(s, b)| (s, String::new(), b)).collect()).await
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
        let e = |status: u16, body: &str| super::http_error(StatusCode::from_u16(status).unwrap(), body);
        assert_eq!(
            e(403, r#"{"error":{"code":"RestrictedProvidersError","message":"demo account is restricted from provider wafer","provider":"wafer"}}"#),
            "API access denied · HTTP 403 · Provider: wafer · RestrictedProvidersError: demo account is restricted from provider wafer"
        );
        assert_eq!(
            e(403, r#"{"error":{"message":"Your team has restricted access. Providers considered: wafer","type":"no_providers_available","param":{"name":"RestrictedProvidersError"}}}"#),
            "API access denied · HTTP 403 · Provider: wafer · no_providers_available: Your team has restricted access. Providers considered: wafer"
        );
        assert_eq!(
            e(401, r#"{"error":{"code":"api_key_required","message":"Set AI_GATEWAY_API_KEY\n to use this endpoint."}}"#),
            "API access denied · HTTP 401 · api_key_required: Set AI_GATEWAY_API_KEY to use this endpoint."
        );
        assert_eq!(
            e(529, r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#),
            "API request failed · HTTP 529 · overloaded_error: Overloaded"
        );
        assert_eq!(e(400, r#"{"error":"bad model"}"#), "API request failed · HTTP 400 · bad model");
        assert_eq!(e(502, "<html>bad gateway</html>"), "HTTP 502 Bad Gateway: <html>bad gateway</html>");
        assert_eq!(e(500, ""), "HTTP 500 Internal Server Error");
        assert_eq!(e(500, r#"{"detail":"x"}"#), r#"HTTP 500 Internal Server Error: {"detail":"x"}"#);
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
