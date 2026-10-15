//! Model providers. Each provider turns a [`Request`] into a streamed
//! [`Response`], reporting incremental [`StreamEvent`]s along the way.

pub mod anthropic;
pub mod codex;
pub mod grok;
pub mod hosted;
pub mod openai;
pub mod responses;
pub(crate) mod sse;

use std::time::Duration;

use crate::types::{Request, Response, StreamEvent};

pub type EventSink<'a> = &'a mut (dyn FnMut(StreamEvent) + Send);

#[async_trait::async_trait]
pub trait Provider: Send + Sync {
    /// Short provider label for the UI, e.g. "codex" or "openai".
    fn name(&self) -> &str;
    fn model(&self) -> &str;
    fn endpoint(&self) -> Option<String> { None }
    fn pricing_provider(&self) -> &str { self.name() }

    fn set_model(&mut self, model: String);
    /// Reasoning effort, if the provider supports choosing one (None = auto).
    fn effort(&self) -> Option<&str> {
        None
    }
    fn set_effort(&mut self, _effort: Option<String>) {}
    /// Supported reasoning efforts for the current model, lowest first, when
    /// the provider knows them without listing models.
    fn efforts(&self) -> Vec<String> {
        Vec::new()
    }
    /// The current model's context window, when the provider knows it.
    fn context_window(&self) -> Option<u64> {
        None
    }
    /// Fast mode (Codex: the `priority` service tier).
    fn fast(&self) -> bool {
        false
    }
    fn set_fast(&mut self, _fast: bool) {}
    /// Whether the current model offers fast mode.
    fn supports_fast(&self) -> bool {
        false
    }
    /// A stable id for the current conversation, sent where the backend uses
    /// one for routing and prompt caching (Codex `session_id`,
    /// OpenCode Go `x-opencode-session`).
    fn set_conversation(&mut self, _id: &str) {}
    /// A small model for the auto-mode permission classifier when the
    /// `classifier_model` setting is unset (None: the session model).
    fn reviewer_model(&self) -> Option<String> {
        None
    }
    /// A fresh provider with the same settings (for subagents).
    fn fork(&self) -> Box<dyn Provider>;
    /// Stream one response. Cancellation is done by dropping the future.
    async fn stream(&self, req: Request<'_>, on_event: EventSink<'_>) -> anyhow::Result<Response>;
}

/// What a provider's live model listing said.
#[derive(Clone, Debug, Default)]
pub struct Listing {
    /// Listed model ids, in listing order.
    pub models: Vec<String>,
    /// Models offering fast mode.
    pub fast: Vec<String>,
}

/// Listings per provider id, absent until the provider was listed in this
/// process.
static LISTINGS: std::sync::Mutex<std::collections::BTreeMap<String, Listing>> =
    std::sync::Mutex::new(std::collections::BTreeMap::new());

/// Record (merge in) what `provider`'s model listing said.
pub fn remember_listing(provider: &str, models: Vec<String>, fast: Vec<String>) {
    let mut all = LISTINGS.lock().unwrap_or_else(|e| e.into_inner());
    let known = all.entry(provider.to_string()).or_default();
    for (list, new) in [(&mut known.models, models), (&mut known.fast, fast)] {
        for m in new {
            if !list.contains(&m) {
                list.push(m);
            }
        }
    }
}

/// `provider`'s listing, once it has been fetched.
pub fn listing(provider: &str) -> Option<Listing> {
    LISTINGS.lock().unwrap_or_else(|e| e.into_inner()).get(provider).cloned()
}

/// Whether the listing says `model` offers fast mode (false until listed).
pub fn listed_fast(provider: &str, model: &str) -> bool {
    listing(provider).is_some_and(|l| l.fast.iter().any(|m| m == model))
}

/// Providers whose listing says which models offer fast mode.
pub fn lists_fast(provider: &str) -> bool {
    provider == "codex"
}

/// Retries after the first attempt.
const RETRIES: u32 = 3;
/// Longest wait between attempts, including a server's `Retry-After`.
const MAX_DELAY: Duration = Duration::from_secs(8);
/// A `Retry-After` longer than this is a quota window, not a blip: no retry.
const GIVE_UP_AFTER: Duration = Duration::from_secs(60);
const BACKOFF: Duration = Duration::from_millis(if cfg!(test) { 10 } else { 1_000 });

/// How long to wait before retrying after `error` (attempt 0 = first
/// retry), or None when retrying cannot help: rate limits, server errors
/// (500/502/503/504), and connection failures or timeouts are transient.
fn retry_delay(error: &anyhow::Error, attempt: u32) -> Option<Duration> {
    let backoff = (BACKOFF * 2u32.pow(attempt)).min(MAX_DELAY);
    error.chain().find_map(|cause| {
        if let Some(e) = cause.downcast_ref::<sse::HttpError>() {
            if !matches!(e.status, 429 | 500 | 502 | 503 | 504) {
                return Some(None);
            }
            return Some(match e.retry_after {
                Some(after) if after > GIVE_UP_AFTER => None,
                Some(after) => Some(after.min(MAX_DELAY)),
                None => Some(backoff),
            });
        }
        let e = cause.downcast_ref::<reqwest::Error>()?;
        Some((e.is_connect() || e.is_timeout()).then_some(backoff))
    })?
}

/// [`Provider::stream`], retried with backoff on transient failures as long
/// as nothing has been streamed yet. Each retry is announced to `on_event`
/// as [`StreamEvent::Retrying`].
pub async fn stream_with_retry(provider: &dyn Provider, req: Request<'_>, on_event: EventSink<'_>) -> anyhow::Result<Response> {
    let mut attempt = 0;
    loop {
        let mut produced = false;
        let result = {
            let mut sink = |event: StreamEvent| {
                produced = true;
                on_event(event)
            };
            provider.stream(req, &mut sink).await
        };
        let error = match result {
            Err(e) if !produced && attempt < RETRIES => e,
            result => return result,
        };
        let Some(delay) = retry_delay(&error, attempt) else {
            return Err(error);
        };
        attempt += 1;
        let reason: String = format!("{error:#}").lines().next().unwrap_or_default().chars().take(120).collect();
        on_event(StreamEvent::Retrying(format!(
            "Retrying in {:.0}s ({attempt}/{RETRIES}) · {reason}",
            delay.as_secs_f64().ceil()
        )));
        tokio::time::sleep(delay).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        provider::{openai::OpenAiProvider, sse::tests::server_with_headers},
        types::Item,
    };

    async fn run(replies: Vec<(u16, &str, &str)>) -> (anyhow::Result<Response>, Vec<StreamEvent>, usize) {
        let replies = replies.into_iter().map(|(s, h, b)| (s, h.to_string(), b.to_string())).collect();
        let (url, mut requests, task) = server_with_headers(replies).await;
        let provider = OpenAiProvider::new(url, None, "m".into());
        let mut events = Vec::new();
        let result = stream_with_retry(&provider, Request { system: "", items: &[], tools: &[] }, &mut |e| events.push(e)).await;
        task.await.unwrap();
        let mut seen = 0;
        while requests.try_recv().is_ok() {
            seen += 1;
        }
        (result, events, seen)
    }

    const OK: &str = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"},\"finish_reason\":\"stop\"}]}\n\n";

    #[tokio::test]
    async fn retries_transient_failures() {
        let (result, events, seen) = run(vec![(503, "", "busy"), (429, "Retry-After: 0\r\n", "slow down"), (200, "", OK)]).await;
        assert_eq!(result.unwrap().items, vec![Item::Assistant { text: "hi".into() }]);
        assert_eq!(seen, 3);
        let StreamEvent::Retrying(first) = &events[0] else { panic!("{events:?}") };
        assert!(first.contains("(1/3)") && first.contains("HTTP 503"), "{first}");
        assert!(matches!(&events[1], StreamEvent::Retrying(s) if s.starts_with("Retrying in 0s (2/3)")), "{events:?}");
        assert_eq!(events[2], StreamEvent::Text("hi".into()));
    }

    #[tokio::test]
    async fn gives_up() {
        // Client errors are final.
        let (result, events, seen) = run(vec![(401, "", r#"{"error":{"message":"bad key"}}"#)]).await;
        assert_eq!(result.unwrap_err().to_string(), "API access denied · HTTP 401 · bad key");
        assert!(events.is_empty() && seen == 1);
        // A long Retry-After is a quota window.
        let (result, events, _) = run(vec![(429, "Retry-After: 3600\r\n", "limit")]).await;
        assert!(result.unwrap_err().to_string().contains("429"));
        assert!(events.is_empty());
        // Four attempts at most.
        let (result, events, seen) = run(vec![(500, "", "a"), (502, "", "b"), (504, "", "c"), (503, "", "d")]).await;
        assert!(result.unwrap_err().to_string().contains("HTTP 503"));
        assert_eq!((events.len(), seen), (3, 4));
    }

    #[tokio::test]
    async fn no_retry_after_output() {
        // The stream broke after text was shown: retrying would repeat it.
        let partial = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n\n";
        let (result, events, _) = run(vec![(200, "", partial)]).await;
        assert!(result.is_err());
        assert_eq!(events, vec![StreamEvent::Text("hi".into())]);
    }

    #[tokio::test]
    async fn retries_connection_failures() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        let provider = OpenAiProvider::new(url, None, "m".into());
        let mut events = Vec::new();
        let result = stream_with_retry(&provider, Request { system: "", items: &[], tools: &[] }, &mut |e| events.push(e)).await;
        assert!(result.is_err());
        assert_eq!(events.len(), 3, "{events:?}");
    }
}
