//! The core loop: send the transcript, stream the reply, run any tool calls,
//! feed results back, repeat until the model answers without tools.

use std::collections::HashSet;
use std::path::PathBuf;

use anyhow::Result;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::provider::Provider;
use crate::session::Session;
use crate::tools::{Registry, ToolCtx, ToolOutput};
use crate::types::{Item, Request, StreamEvent, Usage};

/// Upper bound on model round-trips per user turn.
const MAX_STEPS: usize = 100;

/// Compact automatically once a request uses this share of the context window.
const AUTO_COMPACT_AT: f64 = 0.8;

const COMPACT_PROMPT: &str = "\
Summarize this conversation so far so that work can continue from the summary alone; \
the earlier messages will be discarded. Include: the user's goals and requests (quote \
key instructions), decisions made, files read or changed (with paths) and why, commands \
run and their important results, errors and how they were resolved, the current state \
of the work, and the exact next steps. Be thorough but concise. Do not call tools.";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Approval {
    Yes,
    /// Approve this tool for the rest of the session.
    Always,
    No,
}

/// What the UI is asked to approve.
pub struct ApprovalRequest<'a> {
    pub tool: &'a str,
    pub summary: &'a str,
    pub preview: Option<&'a str>,
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
}

pub enum TurnEnd {
    Done,
    Cancelled,
}

pub struct Agent {
    pub provider: Box<dyn Provider>,
    pub tools: Registry,
    pub system: String,
    pub items: Vec<Item>,
    pub session: Option<Session>,
    pub cwd: PathBuf,
    /// Skip all approval prompts.
    pub yolo: bool,
    /// Tools approved with "always" in this session.
    pub allowed: HashSet<String>,
    pub usage: Usage,
    /// Model context window in tokens; drives automatic compaction.
    pub context_window: u64,
    /// Latest context size (input plus output), or an estimate after compaction.
    pub context_tokens: u64,
    pub sandbox: std::sync::Arc<crate::sandbox::Policy>,
}

impl Agent {
    fn push(&mut self, item: Item) {
        if let Some(s) = &mut self.session {
            if let Err(e) = s.append(&item) {
                eprintln!("warning: could not save session: {e}");
            }
        }
        self.items.push(item);
    }

    /// Run one user turn to completion (or cancellation).
    pub async fn turn(
        &mut self,
        input: Item,
        ui: &mut dyn Ui,
        cancel: CancellationToken,
    ) -> Result<TurnEnd> {
        self.push(input);
        for _ in 0..MAX_STEPS {
            if self.context_window > 0
                && self.context_tokens as f64 >= self.context_window as f64 * AUTO_COMPACT_AT
            {
                ui.notice("Context is nearly full; compacting the conversation…");
                if !self.compact(ui, &cancel).await? {
                    return Ok(TurnEnd::Cancelled);
                }
                anyhow::ensure!(
                    (self.context_tokens as f64) < self.context_window as f64 * AUTO_COMPACT_AT,
                    "context is still over the automatic compaction limit after compacting"
                );
            }
            let specs = self.tools.specs();
            let req = Request {
                system: &self.system,
                items: &self.items,
                tools: &specs,
            };
            let result = {
                let mut sink = |ev: StreamEvent| ui.stream(&ev);
                tokio::select! {
                    r = self.provider.stream(req, &mut sink) => Some(r),
                    _ = cancel.cancelled() => None,
                }
            };
            ui.end_message();
            let Some(result) = result else {
                return Ok(TurnEnd::Cancelled);
            };
            let response = result?;
            if let Some(u) = response.usage {
                self.usage.input_tokens += u.input_tokens;
                self.usage.output_tokens += u.output_tokens;
                self.usage.cached_tokens += u.cached_tokens;
                self.context_tokens = u.input_tokens + u.output_tokens;
                ui.usage(&u);
            }

            let calls: Vec<(String, String, String)> = response
                .items
                .iter()
                .filter_map(|i| match i {
                    Item::ToolCall {
                        id,
                        name,
                        arguments,
                    } => Some((id.clone(), name.clone(), arguments.clone())),
                    _ => None,
                })
                .collect();
            for item in response.items {
                self.push(item);
            }
            if calls.is_empty() {
                // Queued input keeps the run going: steering first, then one
                // follow-up at a time.
                let mut queued = ui.take_steering();
                if queued.is_empty() {
                    queued.extend(ui.take_follow_up());
                }
                if queued.is_empty() {
                    return Ok(TurnEnd::Done);
                }
                for item in queued {
                    self.push(item);
                }
                continue;
            }

            let mut images = Vec::new();
            for (i, (id, name, arguments)) in calls.iter().enumerate() {
                if cancel.is_cancelled() {
                    // Every call needs a result or the next request is invalid.
                    for (id, ..) in &calls[i..] {
                        self.push(Item::ToolResult {
                            id: id.clone(),
                            output: "Cancelled by user.".into(),
                            is_error: true,
                        });
                    }
                    return Ok(TurnEnd::Cancelled);
                }
                let out = self.call_tool(name, arguments, ui, &cancel).await;
                // Usage precedes tool execution, so account for the new text
                // before deciding whether the next step needs compaction.
                self.context_tokens = self
                    .context_tokens
                    .saturating_add((out.content.len() as u64).div_ceil(4));
                images.extend(out.images);
                self.push(Item::ToolResult {
                    id: id.clone(),
                    output: out.content,
                    is_error: out.is_error,
                });
            }
            // Tool results are text-only in both APIs, so images a tool
            // produced are handed over in a follow-up user message.
            if !images.is_empty() {
                let text = format!(
                    "[{} image(s) returned by the tool calls above]",
                    images.len()
                );
                self.push(Item::User { text, images });
            }
            for item in ui.take_steering() {
                self.push(item);
            }
        }
        anyhow::bail!("stopped after {MAX_STEPS} steps without a final answer")
    }

    async fn call_tool(
        &mut self,
        name: &str,
        arguments: &str,
        ui: &mut dyn Ui,
        cancel: &CancellationToken,
    ) -> ToolOutput {
        let Some(tool) = self.tools.get(name) else {
            return ToolOutput::err(format!("Unknown tool `{name}`."));
        };
        let args: Value = match serde_json::from_str(if arguments.trim().is_empty() {
            "{}"
        } else {
            arguments
        }) {
            Ok(v) => v,
            Err(e) => return ToolOutput::err(format!("Invalid JSON arguments: {e}")),
        };
        let ctx = ToolCtx {
            cwd: self.cwd.clone(),
            cancel: cancel.clone(),
            sandbox: self.sandbox.clone(),
        };
        let summary = tool.summary(&args);

        if tool.needs_approval(&args) && !self.yolo && !self.allowed.contains(name) {
            let preview = tool.preview(&args, &ctx);
            let req = ApprovalRequest {
                tool: name,
                summary: &summary,
                preview: preview.as_deref(),
            };
            match ui.approve(&req).await {
                Approval::Yes => {}
                Approval::Always => {
                    self.allowed.insert(name.to_string());
                }
                Approval::No => {
                    let out = ToolOutput::err(
                        "The user denied this tool call. Ask what they want instead.",
                    );
                    ui.tool_end(name, &out);
                    return out;
                }
            }
        }

        ui.tool_start(name, &summary);
        let out = tokio::select! {
            out = tool.run(args, &ctx) => out,
            _ = cancel.cancelled() => ToolOutput::err("Cancelled by user."),
        };
        ui.tool_end(name, &out);
        out
    }

    /// Replace the transcript with a model-written summary. Returns false if
    /// cancelled (the transcript is then left unchanged).
    pub async fn compact(&mut self, ui: &mut dyn Ui, cancel: &CancellationToken) -> Result<bool> {
        if cancel.is_cancelled() {
            return Ok(false);
        }
        if self.items.is_empty() {
            return Ok(true);
        }
        let mut pending = HashSet::new();
        for item in &self.items {
            match item {
                Item::ToolCall { id, .. } => {
                    pending.insert(id);
                }
                Item::ToolResult { id, .. } => {
                    pending.remove(id);
                }
                _ => {}
            }
        }
        anyhow::ensure!(
            pending.is_empty(),
            "cannot compact with unanswered tool calls"
        );
        let mut items = self.items.clone();
        items.push(Item::user(COMPACT_PROMPT));
        let req = Request {
            system: &self.system,
            items: &items,
            tools: &[],
        };
        let mut ignore = |_: StreamEvent| {};
        let response = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Ok(false),
            r = self.provider.stream(req, &mut ignore) => r?,
        };
        if cancel.is_cancelled() {
            return Ok(false);
        }
        anyhow::ensure!(
            !response
                .items
                .iter()
                .any(|item| matches!(item, Item::ToolCall { .. })),
            "compaction returned tool calls instead of a summary"
        );
        let summary: String = response
            .items
            .iter()
            .filter_map(|i| match i {
                Item::Assistant { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        anyhow::ensure!(
            !summary.trim().is_empty(),
            "compaction produced an empty summary"
        );
        // The summary request input is discarded. Its output remains alongside
        // the system prompt and compaction wrapper; estimate when usage is absent.
        let estimate = |text: &str| (text.len() as u64).div_ceil(4);
        self.context_tokens = estimate(&self.system)
            + estimate(&crate::types::compaction_text(""))
            + response.usage.map_or(estimate(&summary), |u| {
                u.output_tokens.max(estimate(&summary))
            });
        if let Some(u) = response.usage {
            self.usage.input_tokens += u.input_tokens;
            self.usage.output_tokens += u.output_tokens;
            self.usage.cached_tokens += u.cached_tokens;
            ui.usage(&u);
        }
        let before = self.items.len();
        self.items.clear();
        self.push(Item::Compaction { summary });
        ui.notice(&format!("Compacted {before} messages into a summary."));
        Ok(true)
    }

    /// Start a fresh conversation (keeps provider, tools and grants).
    pub fn reset(&mut self, session: Option<Session>) {
        self.items.clear();
        self.session = session;
        self.usage = Usage::default();
        self.context_tokens = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::EventSink;
    use crate::types::Response;

    struct SummaryProvider {
        response: Response,
        cancel: Option<CancellationToken>,
    }

    #[async_trait::async_trait]
    impl Provider for SummaryProvider {
        fn name(&self) -> &str {
            "fake"
        }
        fn model(&self) -> &str {
            "fake"
        }
        fn set_model(&mut self, _: String) {}
        async fn stream(&self, req: Request<'_>, _: EventSink<'_>) -> Result<Response> {
            assert!(req.tools.is_empty());
            assert_eq!(req.items.last(), Some(&Item::user(COMPACT_PROMPT)));
            if let Some(cancel) = &self.cancel {
                cancel.cancel();
            }
            Ok(self.response.clone())
        }
    }

    fn agent(response: Response, cancel: Option<CancellationToken>) -> Agent {
        Agent {
            provider: Box::new(SummaryProvider { response, cancel }),
            tools: Registry::default(),
            system: "system".into(),
            items: vec![Item::user("original")],
            session: None,
            cwd: PathBuf::from("."),
            yolo: true,
            allowed: HashSet::new(),
            usage: Usage::default(),
            context_window: 1000,
            context_tokens: 900,
            sandbox: Default::default(),
        }
    }

    struct QuietUi;
    #[async_trait::async_trait]
    impl Ui for QuietUi {
        fn stream(&mut self, _: &StreamEvent) {}
        fn end_message(&mut self) {}
        fn tool_start(&mut self, _: &str, _: &str) {}
        fn tool_end(&mut self, _: &str, _: &ToolOutput) {}
        async fn approve(&mut self, _: &ApprovalRequest<'_>) -> Approval {
            Approval::Yes
        }
    }

    #[tokio::test]
    async fn cancelled_compaction_preserves_transcript_and_session() {
        let cancel = CancellationToken::new();
        let mut agent = agent(
            Response {
                items: vec![Item::Assistant {
                    text: "summary".into(),
                }],
                usage: Some(Usage {
                    input_tokens: 100,
                    output_tokens: 10,
                    cached_tokens: 5,
                }),
            },
            Some(cancel.clone()),
        );
        let dir = tempfile::tempdir().unwrap();
        let mut session = Session::create(dir.path(), &agent.cwd, "fake").unwrap();
        session.append(&agent.items[0]).unwrap();
        let path = std::fs::read_dir(dir.path())
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let saved = std::fs::read(&path).unwrap();
        agent.session = Some(session);
        let before = agent.items.clone();
        assert!(!agent.compact(&mut QuietUi, &cancel).await.unwrap());
        assert_eq!(agent.items, before);
        assert_eq!(agent.context_tokens, 900);
        assert_eq!(agent.usage, Usage::default());
        assert_eq!(std::fs::read(path).unwrap(), saved);
    }

    #[tokio::test]
    async fn compaction_accounts_usage_and_retained_context() {
        let usage = Usage {
            input_tokens: 900,
            output_tokens: 20,
            cached_tokens: 40,
        };
        let mut agent = agent(
            Response {
                items: vec![Item::Assistant {
                    text: "summary".into(),
                }],
                usage: Some(usage),
            },
            None,
        );
        assert!(agent
            .compact(&mut QuietUi, &CancellationToken::new())
            .await
            .unwrap());
        assert_eq!(agent.usage, usage);
        assert_eq!(
            agent.context_tokens,
            20 + 2 + (crate::types::compaction_text("").len() as u64).div_ceil(4)
        );
        assert_eq!(
            agent.items,
            vec![Item::Compaction {
                summary: "summary".into()
            }]
        );
    }

    #[tokio::test]
    async fn invalid_compaction_leaves_transcript_unchanged() {
        for items in [
            vec![],
            vec![
                Item::Assistant {
                    text: "summary".into(),
                },
                Item::ToolCall {
                    id: "new".into(),
                    name: "bash".into(),
                    arguments: "{}".into(),
                },
            ],
        ] {
            let mut agent = agent(Response { items, usage: None }, None);
            let before = agent.items.clone();
            assert!(agent
                .compact(&mut QuietUi, &CancellationToken::new())
                .await
                .is_err());
            assert_eq!(agent.items, before);
        }
        let mut agent = agent(Response::default(), None);
        agent.items.push(Item::ToolCall {
            id: "pending".into(),
            name: "bash".into(),
            arguments: "{}".into(),
        });
        assert!(agent
            .compact(&mut QuietUi, &CancellationToken::new())
            .await
            .unwrap_err()
            .to_string()
            .contains("unanswered"));
    }
}
