//! Shared server-sent-events helpers for streaming providers.

use anyhow::{bail, Result};
use serde_json::Value;

pub async fn checked(response: reqwest::Response) -> Result<reqwest::Response> {
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await?;
        let excerpt: String = body.chars().take(4096).collect();
        bail!("HTTP {status}: {excerpt}");
    }
    Ok(response)
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
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (tx, rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            for (status, body) in replies {
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
                socket.write_all(format!("HTTP/1.1 {status} Test\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).as_bytes()).await.unwrap();
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
