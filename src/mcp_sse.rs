//! The legacy HTTP+SSE MCP transport (protocol 2024-11-05): a long-lived
//! `GET` event stream whose first `endpoint` event names the URL that client
//! messages are `POST`ed to; server messages arrive as `message` events.

use std::collections::BTreeMap;
use std::time::Duration;

use eventsource_stream::Eventsource;
use futures::StreamExt;
use rmcp::service::{RxJsonRpcMessage, TxJsonRpcMessage};
use rmcp::transport::Transport;
use rmcp::RoleClient;
use tokio::sync::mpsc;

pub struct SseTransport {
    client: reqwest::Client,
    endpoint: reqwest::Url,
    headers: reqwest::header::HeaderMap,
    incoming: mpsc::Receiver<RxJsonRpcMessage<RoleClient>>,
    reader: tokio::task::JoinHandle<()>,
}

fn io_err(msg: impl Into<String>) -> std::io::Error {
    std::io::Error::other(msg.into())
}

impl SseTransport {
    /// Open the event stream and wait (up to `timeout`) for the endpoint.
    pub async fn connect(url: &str, headers: &BTreeMap<String, String>, timeout: Duration) -> std::io::Result<Self> {
        let base = reqwest::Url::parse(url).map_err(|e| io_err(format!("invalid url: {e}")))?;
        let mut map = reqwest::header::HeaderMap::new();
        for (k, v) in headers {
            let name = reqwest::header::HeaderName::from_bytes(k.as_bytes()).map_err(|_| io_err(format!("invalid header name {k}")))?;
            let mut value = reqwest::header::HeaderValue::from_str(v).map_err(|_| io_err(format!("invalid value for header {k}")))?;
            value.set_sensitive(true);
            map.insert(name, value);
        }
        let client = reqwest::Client::new();
        let response = tokio::time::timeout(
            timeout,
            client.get(base.clone()).headers(map.clone()).header(reqwest::header::ACCEPT, "text/event-stream").send(),
        )
        .await
        .map_err(|_| io_err("the event stream did not open in time"))?
        .map_err(|e| io_err(e.to_string()))?;
        let status = response.status();
        if !status.is_success() {
            return Err(io_err(format!("the event stream returned HTTP {status}")));
        }
        let mut events = response.bytes_stream().eventsource();
        let endpoint = tokio::time::timeout(timeout, async {
            while let Some(event) = events.next().await {
                let event = event.map_err(|e| io_err(e.to_string()))?;
                if event.event == "endpoint" {
                    return base.join(event.data.trim()).map_err(|e| io_err(format!("invalid endpoint: {e}")));
                }
            }
            Err(io_err("the event stream ended before naming an endpoint"))
        })
        .await
        .map_err(|_| io_err("the server sent no endpoint event"))??;
        // Messages must go back to the server that sent the stream.
        if endpoint.origin() != base.origin() {
            return Err(io_err(format!("the endpoint {endpoint} is on another origin")));
        }
        let (tx, incoming) = mpsc::channel(64);
        let reader = tokio::spawn(async move {
            while let Some(Ok(event)) = events.next().await {
                if event.event != "message" && !event.event.is_empty() {
                    continue;
                }
                // Malformed messages are skipped; the request they answer times out.
                if let Ok(msg) = serde_json::from_str(&event.data) {
                    if tx.send(msg).await.is_err() {
                        break;
                    }
                }
            }
        });
        Ok(Self { client, endpoint, headers: map, incoming, reader })
    }
}

impl Transport<RoleClient> for SseTransport {
    type Error = std::io::Error;

    fn send(
        &mut self,
        item: TxJsonRpcMessage<RoleClient>,
    ) -> impl std::future::Future<Output = Result<(), Self::Error>> + Send + 'static {
        let request = self.client.post(self.endpoint.clone()).headers(self.headers.clone()).json(&item);
        async move {
            let response = request.send().await.map_err(|e| io_err(e.to_string()))?;
            match response.status() {
                s if s.is_success() => Ok(()),
                s => Err(io_err(format!("posting a message returned HTTP {s}"))),
            }
        }
    }

    fn receive(&mut self) -> impl std::future::Future<Output = Option<RxJsonRpcMessage<RoleClient>>> + Send {
        self.incoming.recv()
    }

    async fn close(&mut self) -> Result<(), Self::Error> {
        self.reader.abort();
        Ok(())
    }
}

impl Drop for SseTransport {
    fn drop(&mut self) {
        self.reader.abort();
    }
}
