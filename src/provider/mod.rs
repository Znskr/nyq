//! Model providers. Each provider turns a [`Request`] into a streamed
//! [`Response`], reporting incremental [`StreamEvent`]s along the way.

pub mod codex;
pub mod openai;
mod sse;

use crate::types::{Request, Response, StreamEvent};

pub type EventSink<'a> = &'a mut (dyn FnMut(StreamEvent) + Send);

#[async_trait::async_trait]
pub trait Provider: Send + Sync {
    /// Short provider label for the UI, e.g. "codex" or "openai".
    fn name(&self) -> &str;
    fn model(&self) -> &str;
    fn set_model(&mut self, model: String);
    /// Stream one response. Cancellation is done by dropping the future.
    async fn stream(&self, req: Request<'_>, on_event: EventSink<'_>) -> anyhow::Result<Response>;
}
