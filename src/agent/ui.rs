//! What the loop needs from a frontend: the `Ui` trait and approvals.

use crate::tools::ToolOutput;
use crate::types::{Item, StreamEvent, Usage};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Approval {
    Yes,
    /// Approve calls matching this call's rule (see [`grant_rule`](super::grant_rule)) for the
    /// rest of the session; like `Yes` when the call can't be scoped.
    Always,
    /// Like `Always`, and also append the rule to permissions.json `allow`.
    /// Only offered when the request has a `save_rule`.
    AlwaysSave,
    /// Like `AlwaysSave`, for this workspace only.
    AlwaysSaveLocal,
    No,
}

/// What the UI is asked to approve.
pub struct ApprovalRequest<'a> {
    pub tool: &'a str,
    pub summary: &'a str,
    pub preview: Option<&'a str>,
    /// What "always" would allow, e.g. "`cargo test …`"; None when the call
    /// can't be scoped to a rule (then "always" approves just this once).
    pub rule: Option<String>,
    /// The raw rule "always, and save" would write to permissions.json
    /// (e.g. `bash:cargo test`); None when saving isn't offered.
    pub save_rule: Option<String>,
}

/// Everything the loop needs from a frontend.
#[async_trait::async_trait]
pub trait Ui: Send {
    fn stream(&mut self, event: &StreamEvent);
    /// Called once the model response is complete (flush partial lines).
    fn end_message(&mut self);
    fn tool_start(&mut self, tool: &str, summary: &str);
    fn tool_end(&mut self, tool: &str, output: &ToolOutput);
    async fn approve(&mut self, req: &ApprovalRequest<'_>) -> Approval;
    /// A note the user attached to their last approval answer, passed to the
    /// model with the call's result (or its denial).
    fn approval_note(&mut self) -> Option<String> {
        None
    }
    fn usage(&mut self, _usage: &Usage) {}
    /// A short status message from the loop (e.g. "Compacting…").
    fn notice(&mut self, _msg: &str) {}
    /// Messages the user typed while the agent was working, delivered before
    /// the next model request ("steering").
    fn take_steering(&mut self) -> Vec<Item> {
        Vec::new()
    }
    /// A message queued to run once the agent would otherwise stop.
    fn take_follow_up(&mut self) -> Option<Item> {
        None
    }
    /// Put a question to the user; None if they skipped it or can't be asked.
    async fn ask(&mut self, _question: &str, _options: &[String]) -> Option<String> {
        None
    }
    /// A shareable handle that asks the same person (or editor) for approval,
    /// for work that runs outside this loop — e.g. a subagent's tool calls.
    /// None means such calls cannot be approved and must be denied.
    fn approver(&self) -> Option<std::sync::Arc<dyn Approver>> {
        None
    }
}

/// Asks for approval from outside the agent loop (see `Ui::approver`).
#[async_trait::async_trait]
pub trait Approver: Send + Sync {
    /// `origin` names who is asking, e.g. "subagent: Count Rust files".
    async fn approve(&self, origin: &str, req: &ApprovalRequest<'_>) -> Approval;
    /// Like `approve`, with the note the user attached to the answer.
    async fn approve_noted(
        &self,
        origin: &str,
        req: &ApprovalRequest<'_>,
    ) -> (Approval, Option<String>) {
        (self.approve(origin, req).await, None)
    }
}
