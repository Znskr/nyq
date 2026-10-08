//! Model providers. Each provider turns a [`Request`] into a streamed
//! [`Response`], reporting incremental [`StreamEvent`]s along the way.

pub mod anthropic;
pub mod codex;
pub mod hosted;
pub mod openai;
pub mod responses;
pub(crate) mod sse;

use crate::types::{Request, Response, StreamEvent};

pub type EventSink<'a> = &'a mut (dyn FnMut(StreamEvent) + Send);

#[async_trait::async_trait]
pub trait Provider: Send + Sync {
    /// Short provider label for the UI, e.g. "codex" or "openai".
    fn name(&self) -> &str;
    fn model(&self) -> &str;
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
    /// A stable id for the current conversation, sent where the backend uses
    /// one for routing and prompt caching (Codex `session_id`,
    /// OpenCode Go `x-opencode-session`).
    fn set_conversation(&mut self, _id: &str) {}
    /// A fresh provider with the same settings (for subagents).
    fn fork(&self) -> Box<dyn Provider>;
    /// Stream one response. Cancellation is done by dropping the future.
    async fn stream(&self, req: Request<'_>, on_event: EventSink<'_>) -> anyhow::Result<Response>;
}
