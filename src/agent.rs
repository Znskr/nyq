//! The core loop: send the transcript, stream the reply, run any tool calls,
//! feed results back, repeat until the model answers without tools.

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

use anyhow::Result;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::permissions::Decision;
use crate::provider::Provider;
use crate::session::Session;
use crate::tools::{Registry, ToolCtx, ToolOutput};
use crate::types::{Item, Request, StreamEvent, Usage};

/// Upper bound on model round-trips per user turn.
const MAX_STEPS: usize = 100;

/// Compact automatically once a request uses this share of the context
/// window (percent, from settings; 0 turns automatic compaction off).
static AUTO_COMPACT_PERCENT: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(80);

pub fn set_auto_compact_percent(percent: u8) {
    AUTO_COMPACT_PERCENT.store(percent.min(100), std::sync::atomic::Ordering::Relaxed);
}

/// The compaction threshold as a fraction; above 1 (never reached) when off.
fn auto_compact_at() -> f64 {
    match AUTO_COMPACT_PERCENT.load(std::sync::atomic::Ordering::Relaxed) {
        0 => f64::INFINITY,
        p => f64::from(p) / 100.0,
    }
}

const COMPACT_PROMPT: &str = "\
Summarize this conversation so far so that work can continue from the summary alone; \
the earlier messages will be discarded. Include: the user's goals and requests (quote \
key instructions), decisions made, files read or changed (with paths) and why, commands \
run and their important results, errors and how they were resolved, the current state \
of the work, and the exact next steps. Be thorough but concise. Do not call tools.";

const CONTINUE_NOTE: &str = "\n\nThe previous response was interrupted or cut off. Continue from where you stopped. Do not repeat text you already wrote.";

const TITLE_PROMPT: &str = "\
Write a 3 to 6 word title for this conversation. Reply with the title only: \
no quotes, no trailing punctuation, and no explanation. Do not call tools.";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Approval {
    Yes,
    /// Approve calls matching this call's rule (see [`grant_rule`]) for the
    /// rest of the session; like `Yes` when the call can't be scoped.
    Always,
    /// Like `Always`, and also append the rule to permissions.json `allow`.
    /// Only offered when the request has a `save_rule`.
    AlwaysSave,
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
}

/// `ask_user_question`: the loop answers it through `Ui::ask` instead of
/// running it, so it is only offered when a person is at the terminal.
pub struct AskUserTool;

pub const ASK_USER: &str = "ask_user_question";

#[async_trait::async_trait]
impl crate::tools::Tool for AskUserTool {
    fn spec(&self) -> crate::types::ToolSpec {
        crate::types::ToolSpec {
            name: ASK_USER.into(),
            description: "Ask the user a multiple-choice question when you need a decision or missing \
                information that you cannot reasonably infer. Offer 2-5 short, distinct options; the user \
                may also type their own answer or skip. Do not use it for confirmation of routine steps."
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "question": {"type": "string", "description": "The question, one or two sentences."},
                    "options": {"type": "array", "items": {"type": "string"}, "description": "2-5 answer choices."}
                },
                "required": ["question", "options"]
            }),
        }
    }
    fn summary(&self, args: &Value) -> String {
        args["question"].as_str().unwrap_or("").to_string()
    }
    async fn run(&self, _args: Value, _ctx: &ToolCtx) -> ToolOutput {
        ToolOutput::err("ask_user_question is answered by the user interface")
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
    /// Rules granted with "always" in this session (see [`grant_rule`]).
    pub allowed: HashSet<String>,
    /// Persistent allow/ask/deny rules (see `crate::permissions`).
    pub permissions: std::sync::Arc<crate::permissions::Permissions>,
    pub usage: Usage,
    pub stats: crate::usage::Stats,
    /// Model context window in tokens; drives automatic compaction.
    pub context_window: u64,
    /// Latest context size (input plus output), or an estimate after compaction.
    pub context_tokens: u64,
    /// `context_tokens` is a chars/4 estimate, refreshed from the transcript
    /// until a provider reports usage.
    pub context_estimated: bool,
    /// Set when compaction cannot get under the limit, so later turns don't retry.
    pub(crate) suppress_auto_compact: bool,
    /// How many leading transcript items have already been sent to the provider.
    /// Automatic compaction may summarize only that prefix, so a user message
    /// that has not been submitted yet is kept verbatim.
    pub(crate) submitted: usize,
    /// User-configured lifecycle hooks (see `crate::hooks`); empty by default.
    pub hooks: std::sync::Arc<crate::hooks::Hooks>,
    pub sandbox: std::sync::Arc<crate::sandbox::Policy>,
    /// File contents before each write/edit, for `/undo`.
    pub undo: crate::undo::UndoLog,
}

impl Agent {
    fn account_usage(&mut self, usage: Option<Usage>, ui: &mut dyn Ui) {
        if let Some(u) = self.record_usage(usage) {
            ui.usage(&u);
        }
    }

    /// Count a request and record its usage (session totals and usage.jsonl).
    fn record_usage(&mut self, usage: Option<Usage>) -> Option<Usage> {
        self.stats.requests += 1;
        let u = usage?;
        self.usage.input_tokens += u.input_tokens;
        self.usage.output_tokens += u.output_tokens;
        self.usage.cached_tokens += u.cached_tokens;
        // Permissions are loaded from the active profile, including in ACP and subagents.
        if let Some(home) = self.permissions.path.parent().filter(|p| !p.as_os_str().is_empty()) {
            let session = self.session.as_ref().map_or(self.stats.id.as_str(), |s| s.id.as_str());
            if let Err(e) = crate::usage::append(home, session, self.provider.name(), self.provider.pricing_provider(), self.provider.model(), u) {
                crate::ui::error(&format!("could not save usage: {e:#}"));
            }
        }
        Some(u)
    }

    pub fn push(&mut self, item: Item) {
        self.stats.item(&item);
        if let Some(s) = &mut self.session {
            if let Err(e) = s.append(&item) {
                crate::ui::error(&format!("could not save session: {e:#}"));
            }
        }
        self.items.push(item);
    }

    fn bind_conversation(&mut self) {
        if let Some(s) = &self.session {
            self.provider.set_conversation(&s.id);
        }
    }

    /// Run one user turn to completion (or cancellation).
    pub async fn turn(
        &mut self,
        input: Item,
        ui: &mut dyn Ui,
        cancel: CancellationToken,
    ) -> Result<TurnEnd> {
        self.bind_conversation();
        self.push_counted(input);
        self.drive(ui, cancel, false).await
    }

    /// Continue an interrupted or cut-off response. No user message is added.
    pub async fn continue_turn(
        &mut self,
        ui: &mut dyn Ui,
        cancel: CancellationToken,
    ) -> Result<TurnEnd> {
        anyhow::ensure!(!self.items.is_empty(), "Nothing to continue.");
        self.bind_conversation();
        self.drive(ui, cancel, true).await
    }

    async fn drive(
        &mut self,
        ui: &mut dyn Ui,
        cancel: CancellationToken,
        continue_note: bool,
    ) -> Result<TurnEnd> {
        for _ in 0..MAX_STEPS {
            self.sync_estimate();
            if !self.auto_compact(ui, &cancel).await? {
                return Ok(TurnEnd::Cancelled);
            }
            let specs = self.tools.advertised();
            let note = continue_note.then(|| format!("{}{CONTINUE_NOTE}", self.system));
            let result = {
                let system = note.as_deref().unwrap_or(self.system.as_str());
                let req = Request {
                    system,
                    items: &self.items,
                    tools: &specs,
                };
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
            self.account_usage(response.usage, ui);
            if let Some(u) = response.usage {
                self.context_tokens = u.input_tokens + u.output_tokens;
                self.context_estimated = false;
            } else {
                self.context_estimated = true;
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
                // This response has been shown to the model. A later tool
                // result is submitted only once it has been recorded, so a
                // compaction cannot split a call from its result.
                self.submitted = self.items.len();
                // Queued input keeps the run going: steering first, then one
                // follow-up at a time.
                let mut queued = ui.take_steering();
                if queued.is_empty() {
                    queued.extend(ui.take_follow_up());
                }
                if queued.is_empty() {
                    self.sync_estimate();
                    crate::hooks::stop(self, &cancel).await;
                    return Ok(TurnEnd::Done);
                }
                for item in queued {
                    self.push_counted(item);
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
                let mut out = self.call_tool(name, arguments, ui, &cancel).await;
                // Oversized output is stored; the model gets head, tail and an id.
                let key = crate::tools::artifacts::session_key(self.session.as_ref().map(|s| s.id.as_str()));
                out.content = crate::tools::artifacts::spill(&crate::config::home(), &key, id, name, std::mem::take(&mut out.content));
                // Usage precedes tool execution, so account for the new text
                // before deciding whether the next step needs compaction.
                // An estimate is replaced wholesale on the next sync.
                if !self.context_estimated {
                    self.context_tokens = self
                        .context_tokens
                        .saturating_add((out.content.len() as u64).div_ceil(4));
                }
                images.extend(out.images);
                self.push(Item::ToolResult {
                    id: id.clone(),
                    output: out.content,
                    is_error: out.is_error,
                });
            }
            self.submitted = self.items.len();
            // Tool results are text-only in both APIs, so images a tool
            // produced are handed over in a follow-up user message.
            if !images.is_empty() {
                let text = format!(
                    "[{} image(s) returned by the tool calls above]",
                    images.len()
                );
                self.push_counted(Item::User { text, images });
            }
            for item in ui.take_steering() {
                self.push_counted(item);
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
        if let Some(out) = self.tools.handle_meta(name, arguments) {
            let args: Value = serde_json::from_str(arguments).unwrap_or_default();
            ui.tool_start(name, args["query"].as_str().or(args["name"].as_str()).unwrap_or(""));
            ui.tool_end(name, &out);
            return out;
        }
        let Some(tool) = self.tools.get(name) else {
            return ToolOutput::err(format!("Unknown tool `{name}`."));
        };
        if name == ASK_USER {
            let args: Value = serde_json::from_str(arguments).unwrap_or_default();
            let question = args["question"].as_str().unwrap_or("").to_string();
            let options: Vec<String> = args["options"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|o| o.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            let answer = tokio::select! {
                a = ui.ask(&question, &options) => a,
                _ = cancel.cancelled() => None,
            };
            return match answer {
                Some(a) => ToolOutput::ok(format!("The user answered: {a}")),
                None => ToolOutput::ok(
                    "The user skipped the question. Use your best judgment or ask in your reply.",
                ),
            };
        }
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
            provider: Some(std::sync::Arc::from(self.provider.fork())),
            yolo: self.yolo,
            allowed: self.allowed.clone(),
            approver: ui.approver(),
            permissions: self.permissions.clone(),
            granted: Default::default(),
            session_id: self.session.as_ref().map(|s| s.id.clone()),
            hooks: self.hooks.clone(),
            subagent_dir: self.session.as_ref().map(|s| crate::subagents::store_dir(&s.path)),
        };
        let summary = tool.summary(&args);

        let session_id = ctx.session_id.clone();
        if let Some(msg) = self.hooks.pre_tool_use(name, &args, &self.cwd, session_id.as_deref(), cancel).await {
            ui.tool_start(name, &summary);
            let out = ToolOutput::err(msg);
            ui.tool_end(name, &out);
            return out;
        }

        let mut needs_approval = tool.needs_approval_for(&args, &self.cwd);
        if needs_approval && crate::permissions::is_read_tool(name) {
            let path = args.get("path").and_then(Value::as_str).unwrap_or(".");
            if inside_any(&self.cwd, &self.sandbox.writable, path) {
                needs_approval = false;
            }
        }
        let forced = match self.permissions.decide_in(
            &self.allowed,
            name,
            &args,
            &self.cwd,
            &self.sandbox.writable,
            needs_approval,
            self.yolo,
        ) {
            Decision::Run => None,
            Decision::Deny(rule) => {
                ui.tool_start(name, &summary);
                let out = ToolOutput::err(format!(
                    "This call was blocked by permissions.json rule `{rule}` and did not run. \
                     Do not retry it; find another way or ask the user."
                ));
                ui.tool_end(name, &out);
                return out;
            }
            Decision::Ask { forced } => Some(forced),
        };
        if let Some(forced) = forced {
            let preview = tool.preview(&args, &ctx);
            // A call that an `ask` rule forces can't be covered by a grant.
            let rule = if forced.is_some() {
                None
            } else {
                grant_rule(name, &args)
            };
            let save_rule = rule.clone().filter(|_| {
                crate::permissions::saveable_in(name, &args, &self.cwd, &self.sandbox.writable)
            });
            let req = ApprovalRequest {
                tool: name,
                summary: &summary,
                preview: preview.as_deref(),
                rule: rule.as_deref().map(rule_label),
                save_rule: save_rule.clone(),
            };
            let decision = tokio::select! {
                biased;
                _ = cancel.cancelled() => None,
                answer = ui.approve(&req) => Some(answer),
            };
            let Some(answer) = decision else {
                ui.tool_start(name, &summary);
                let out = ToolOutput::err("Cancelled by user.");
                ui.tool_end(name, &out);
                return out;
            };
            match answer {
                Approval::Yes => {}
                Approval::Always => {
                    self.allowed.extend(rule);
                }
                Approval::AlwaysSave => {
                    self.allowed.extend(rule);
                    if let Some(r) = save_rule {
                        match std::sync::Arc::make_mut(&mut self.permissions).save_allow(&r) {
                            Ok(()) => ui.notice(&format!(
                                "Saved `{r}` to the allow list in permissions.json."
                            )),
                            Err(e) => ui.notice(&format!(
                                "Could not save `{r}` to permissions.json: {e:#}"
                            )),
                        }
                    }
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
        let before = self.undo.len();
        self.undo
            .snapshot(&self.cwd, self.sandbox.mode, name, &args);
        let hook_args = self.hooks.observes_post_tool_use(name).then(|| args.clone());
        let mut out = if tool.handles_cancel() {
            tool.run(args, &ctx).await
        } else {
            tokio::select! {
                out = tool.run(args, &ctx) => out,
                _ = cancel.cancelled() => ToolOutput::err("Cancelled by user."),
            }
        };
        if out.is_error && !out.changed && self.undo.len() > before {
            self.undo.discard_last();
        }
        self.undo.append(std::mem::take(&mut out.undo));
        // "Always" answers given for a subagent's calls belong to this session.
        self.allowed.extend(
            ctx.granted
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .drain(..),
        );
        ui.tool_end(name, &out);
        if let Some(args) = hook_args {
            self.hooks.post_tool_use(name, &args, &out, &self.cwd, session_id.as_deref(), cancel).await;
        }
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
        self.account_usage(response.usage, ui);
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
        self.context_estimated = false;
        self.suppress_auto_compact = self.context_window > 0
            && self.context_tokens as f64 >= self.context_window as f64 * auto_compact_at();
        let before = self.items.len();
        self.items.clear();
        self.push(Item::Compaction { summary });
        self.submitted = self.items.len();
        ui.notice(&format!("Compacted {before} messages into a summary."));
        Ok(true)
    }

    /// Start a fresh conversation (keeps provider, tools and grants).
    pub fn reset(&mut self, session: Option<Session>) {
        self.items.clear();
        self.session = session;
        self.usage = Usage::default();
        self.stats = Default::default();
        self.context_tokens = 0;
        self.context_estimated = false;
        self.suppress_auto_compact = false;
        self.submitted = 0;
        self.undo = Default::default();
    }

    /// Drop the transcript in place: same session id, model, sandbox and rules.
    pub fn clear_context(&mut self) -> Result<()> {
        if let Some(session) = &mut self.session {
            session.clear_transcript()?;
        }
        self.items.clear();
        self.usage = Usage::default();
        self.context_tokens = 0;
        self.context_estimated = false;
        self.suppress_auto_compact = false;
        self.submitted = 0;
        self.undo = Default::default();
        self.allowed.clear();
        Ok(())
    }

    /// Ask the model for a short title. The request is not stored in the transcript.
    pub async fn suggest_title(&mut self, cancel: &CancellationToken) -> Result<String> {
        anyhow::ensure!(!self.items.is_empty(), "Nothing to name yet.");
        if cancel.is_cancelled() {
            anyhow::bail!("Interrupted.");
        }
        self.bind_conversation();
        let mut items = self.items.clone();
        items.push(Item::user(TITLE_PROMPT));
        let specs = [];
        let req = Request {
            system: &self.system,
            items: &items,
            tools: &specs,
        };
        let mut ignore = |_: StreamEvent| {};
        let response = tokio::select! {
            biased;
            _ = cancel.cancelled() => anyhow::bail!("Interrupted."),
            r = self.provider.stream(req, &mut ignore) => r?,
        };
        self.record_usage(response.usage);
        let raw: String = response
            .items
            .iter()
            .filter_map(|item| match item {
                Item::Assistant { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        crate::session::generated_title(&raw)
    }

    /// Replace the context-window size and allow automatic compaction again.
    pub fn set_context_window(&mut self, window: u64) {
        self.context_window = window;
        self.suppress_auto_compact = false;
    }

    /// After loading a transcript, estimate its size until the provider reports usage.
    pub(crate) fn adopt_transcript(&mut self) {
        if let Some(session) = &self.session {
            match self.stats.restore(session, self.permissions.path.parent()) {
                Ok(usage) => self.usage = usage,
                Err(e) => crate::ui::error(&format!("could not restore session stats: {e:#}")),
            }
        }

        self.context_estimated = true;
        self.context_tokens = estimate_context(&self.system, &self.items);
        self.submitted = self.items.len();
    }

    /// Compact only the prefix already sent to the provider. Trailing items
    /// (a new user paste, tool results, images) stay in the transcript.
    async fn auto_compact(&mut self, ui: &mut dyn Ui, cancel: &CancellationToken) -> Result<bool> {
        if self.suppress_auto_compact
            || self.submitted == 0
            || self.context_window == 0
            || (self.context_tokens as f64) < (self.context_window as f64) * auto_compact_at()
        {
            return Ok(true);
        }
        let split = self.submitted.min(self.items.len());
        // One item has nothing to summarize, and an image the model has not
        // been sent yet must stay in the request that shows it.
        if self.items.len() < 2 || split == 0 || tail_has_unsent_image(&self.items[split..]) {
            return Ok(true);
        }
        let tail = self.items.split_off(split);
        ui.notice("Context is nearly full; compacting the conversation…");
        let compacted = match self.compact(ui, cancel).await {
            Ok(ok) => ok,
            Err(e) => {
                self.items.extend(tail);
                return Err(e);
            }
        };
        if !compacted {
            self.items.extend(tail);
            return Ok(false);
        }
        if !self.context_estimated {
            for item in &tail {
                self.context_tokens = self.context_tokens.saturating_add(item_tokens(item));
            }
        }
        let kept = self.items.len();
        for item in tail {
            self.push(item);
        }
        self.submitted = kept;
        if self.context_window > 0
            && self.context_tokens as f64 >= self.context_window as f64 * auto_compact_at()
        {
            self.suppress_auto_compact = true;
            ui.notice(
                "Still over the automatic compaction limit after compacting (the system prompt alone may exceed it). Automatic compaction is off for the rest of this session.",
            );
        }
        Ok(true)
    }

    pub(crate) fn push_counted(&mut self, item: Item) {
        if !self.context_estimated {
            self.context_tokens = self.context_tokens.saturating_add(item_tokens(&item));
        }
        self.push(item);
    }

    fn sync_estimate(&mut self) {
        if self.context_estimated {
            self.context_tokens = estimate_context(&self.system, &self.items);
        }
    }
}

fn text_tokens(text: &str) -> u64 {
    (text.len() as u64).div_ceil(4)
}

/// Flat cost for one image. Base64 length is not the model's token cost.
const IMAGE_TOKENS: u64 = 1_500;

fn tail_has_unsent_image(items: &[Item]) -> bool {
    items
        .iter()
        .any(|item| matches!(item, Item::User { images, .. } if !images.is_empty()))
}

fn item_tokens(item: &Item) -> u64 {
    match item {
        Item::User { text, images } => text_tokens(text) + images.len() as u64 * IMAGE_TOKENS,
        Item::Assistant { text } => text_tokens(text),
        // `encrypted` is opaque provider state, not text the model re-reads.
        Item::Reasoning { summary, .. } => text_tokens(summary),
        Item::ToolCall {
            id,
            name,
            arguments,
        } => text_tokens(id) + text_tokens(name) + text_tokens(arguments),
        Item::ToolResult { id, output, .. } => text_tokens(id) + text_tokens(output),
        Item::Compaction { summary } => text_tokens(&crate::types::compaction_text(summary)),
    }
}

fn estimate_context(system: &str, items: &[Item]) -> u64 {
    text_tokens(system) + items.iter().map(item_tokens).sum::<u64>()
}

// ------------------------------------------------------------ approval rules
//
// "Always" grants a rule rather than a whole tool: `bash:<prefix>` for shell
// commands (program plus subcommand, e.g. `bash:cargo test`), the tool name
// for file writes (only inside the workspace) and for everything else.

/// Programs whose arguments are themselves commands or code: a prefix rule
/// for them would allow anything, so "always" can't be scoped.
const WRAPPERS: &[&str] = &[
    "sh", "bash", "zsh", "fish", "dash", "ksh", "eval", "exec", "source", ".", "sudo", "doas",
    "su", "env", "xargs", "nohup", "time", "timeout", "nice", "watch", "command", "builtin", "ssh",
];
/// Interpreters: scoped only with a script or module word (`python script.py`).
const INTERPRETERS: &[&str] = &[
    "python", "python3", "node", "perl", "ruby", "deno", "php", "lua", "npx", "bunx",
];
/// Package runners whose `run`/`exec` subcommand takes a script name.
const RUNNERS: &[&str] = &[
    "npm", "pnpm", "yarn", "bun", "uv", "poetry", "pipenv", "pdm", "hatch",
];
/// Clients whose later arguments can start another transfer or change the
/// destination. A prefix grant would approve a different operation.
const NETWORK_CLIENTS: &[&str] = &[
    "curl", "wget", "aria2c", "scp", "rsync", "sftp", "nc", "ncat", "socat", "ftp", "rclone",
];

/// Whether a command chains, pipes, substitutes or redirects (or spans lines).
fn has_control(cmd: &str) -> bool {
    cmd.contains("$(")
        || cmd
            .chars()
            .any(|c| matches!(c, ';' | '&' | '|' | '`' | '>' | '<' | '\n' | '\r'))
}

/// A plain command word: no flags, quotes, globs, variables or assignments.
fn plain_word(w: &str) -> bool {
    w.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && w.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':' | '/' | '@' | '+')
        })
}

/// A leading flag (`-la`, `--force`), not a lone `-` (often stdin).
fn is_flag(w: &str) -> bool {
    w.starts_with('-') && w.len() > 1
}

/// Last path component, so `./sh` and `bin/bash` match wrappers by name.
pub(crate) fn program_base(program: &str) -> &str {
    program.rsplit(['/', '\\']).next().unwrap_or(program)
}

/// `Some(true)`: the next plain word is an operand and belongs in the grant.
/// `Some(false)`: a boolean flag, or a self-contained `--name=value`.
/// `None`: the option cannot be scoped (the grant would be once-only).
fn flag_operand(program: &str, flag: &str) -> Option<bool> {
    if let Some((name, value)) = flag.split_once('=') {
        if name.starts_with("--") && name.len() > 2 && plain_word(value) {
            return Some(false);
        }
        return None;
    }
    let valued = |flags: &[&str]| flags.contains(&flag).then_some(true);
    let boolean = |flags: &[&str]| flags.contains(&flag).then_some(false);
    match program_base(program) {
        "git" => valued(&[
            "-C",
            "-c",
            "--git-dir",
            "--work-tree",
            "--namespace",
            "--super-prefix",
            "--config-env",
        ])
        .or_else(|| {
            boolean(&[
                "--bare",
                "--no-pager",
                "--paginate",
                "--no-replace-objects",
                "--literal-pathspecs",
                "--glob-pathspecs",
                "--noglob-pathspecs",
                "--icase-pathspecs",
            ])
        }),
        "cargo" => valued(&[
            "--manifest-path",
            "-p",
            "--package",
            "--target",
            "--target-dir",
            "--features",
            "--jobs",
            "-j",
            "--color",
            "--config",
            "--profile",
            "--message-format",
            "--out-dir",
            "--artifact-dir",
            "-Z",
        ])
        .or_else(|| {
            boolean(&[
                "--release",
                "--offline",
                "--locked",
                "--frozen",
                "--all-features",
                "--no-default-features",
                "--workspace",
                "--all",
                "--lib",
                "--bins",
                "--tests",
                "--examples",
                "--benches",
                "--doc",
                "--quiet",
                "-q",
                "--verbose",
                "-v",
            ])
        }),
        "curl" => valued(&[
            "-o",
            "-d",
            "-H",
            "-X",
            "-u",
            "--url",
            "--output",
            "--data",
            "--header",
            "--request",
            "--user",
            "-A",
            "--user-agent",
            "-e",
            "--referer",
            "-b",
            "--cookie",
            "-F",
            "--form",
            "-T",
            "--upload-file",
            "--data-binary",
            "--data-raw",
            "-m",
            "--max-time",
            "--connect-timeout",
            "-w",
            "--write-out",
            "--proxy",
            "-x",
            "--cacert",
            "--cert",
            "--key",
        ])
        .or_else(|| {
            boolean(&[
                "-s",
                "-S",
                "-L",
                "-f",
                "-k",
                "-I",
                "-v",
                "-i",
                "-G",
                "--silent",
                "--show-error",
                "--location",
                "--fail",
                "--insecure",
                "--head",
                "--verbose",
                "--include",
                "--get",
                "--compressed",
            ])
        }),
        _ => None,
    }
}

/// The prefix "always" grants for a shell command: the program, known leading
/// options together with their operands (`git -C foo`, `curl -o file`), and
/// the following subcommand (`git -C foo status`). Unknown options are not
/// scoped. Wrappers and interpreters are matched on the program name with any
/// directory stripped (`./sh`, `bin/bash`, `./python`).
pub fn bash_prefix(cmd: &str) -> Option<String> {
    if has_control(cmd) {
        return None;
    }
    let words: Vec<&str> = cmd.split_whitespace().collect();
    let program = *words.first()?;
    let base = program_base(program);
    if !plain_word(program.strip_prefix("./").unwrap_or(program))
        || WRAPPERS.contains(&base)
        || NETWORK_CLIENTS.contains(&base)
        || program.contains('=')
    {
        return None;
    }
    if INTERPRETERS.contains(&base) && words.get(1).is_some_and(|w| is_flag(w)) {
        return None;
    }
    let mut prefix = vec![program];
    let mut i = 1;
    while words.get(i).is_some_and(|w| is_flag(w)) {
        match flag_operand(program, words[i]) {
            Some(true) => {
                let operand = words.get(i + 1).copied().filter(|w| plain_word(w))?;
                prefix.push(words[i]);
                prefix.push(operand);
                i += 2;
            }
            Some(false) => {
                prefix.push(words[i]);
                i += 1;
            }
            None => return None,
        }
    }
    if INTERPRETERS.contains(&base) {
        prefix.push(words.get(i).filter(|w| plain_word(w))?);
        return Some(prefix.join(" "));
    }
    if let Some(sub) = words.get(i) {
        if plain_word(sub) {
            prefix.push(sub);
            if RUNNERS.contains(&base) && matches!(*sub, "run" | "exec" | "x") {
                prefix.extend(words.get(i + 1).filter(|w| plain_word(w)));
            }
        } else {
            // A quoted, escaped, or otherwise unparsed operand must not fall
            // back to a grant of the bare program (`rm 'file'` is not `rm`).
            return None;
        }
    }
    Some(prefix.join(" "))
}

/// Whether `cmd` is covered by a granted prefix: same leading words and no
/// control operators.
pub fn bash_matches(prefix: &str, cmd: &str) -> bool {
    let want: Vec<&str> = prefix.split_whitespace().collect();
    !want.is_empty()
        && !has_control(cmd)
        && cmd
            .split_whitespace()
            .take(want.len())
            .eq(want.iter().copied())
}

pub(crate) fn is_file_tool(tool: &str) -> bool {
    matches!(tool, "write_file" | "edit_file")
}

/// A subagent call's mode: `general` or (by default) `explore`.
pub(crate) fn subagent_mode(args: &Value) -> &'static str {
    if args.get("mode").and_then(Value::as_str) == Some("general") {
        "general"
    } else {
        "explore"
    }
}

/// The rule "always" would grant for this call, as stored in `Agent::allowed`.
pub fn grant_rule(tool: &str, args: &Value) -> Option<String> {
    if tool == "bash" {
        let cmd = args.get("command").and_then(Value::as_str)?;
        return bash_prefix(cmd).map(|p| format!("bash:{p}"));
    }
    if tool == "subagent" {
        return Some(format!("subagent:{}", subagent_mode(args)));
    }
    if tool == "web_fetch" {
        let url = args
            .get("url")
            .and_then(Value::as_str)
            .filter(|u| !u.is_empty())?;
        return Some(format!("web_fetch:{url}"));
    }
    if matches!(
        tool,
        "read_file" | "list_dir" | "glob" | "grep" | "view_image"
    ) {
        let path = args.get("path").and_then(Value::as_str).unwrap_or(".");
        return Some(format!("{tool}:{path}"));
    }
    Some(tool.to_string())
}

/// How a rule reads in the approval menu.
pub fn rule_label(rule: &str) -> String {
    if let Some(p) = rule.strip_prefix("bash:") {
        return format!("`{p} …`");
    }
    if let Some(mode) = rule.strip_prefix("subagent:") {
        return format!("`subagent` ({mode})");
    }
    if let Some(url) = rule.strip_prefix("web_fetch:") {
        return format!("`web_fetch` {url}");
    }
    for name in ["read_file", "list_dir", "glob", "grep", "view_image"] {
        if let Some(path) = rule.strip_prefix(&format!("{name}:")) {
            return format!("`{name}` {path}");
        }
    }
    if is_file_tool(rule) {
        return format!("{} in this workspace", rule.replace('_', " "));
    }
    format!("`{rule}`")
}

/// Lexical normalization of `base.join(path)`, or None if it escapes via `..`.
fn lexical(base: &Path, path: &str) -> Option<PathBuf> {
    let mut full = PathBuf::new();
    for c in base.join(path).components() {
        match c {
            Component::ParentDir => {
                if !full.pop() {
                    return None;
                }
            }
            Component::CurDir => {}
            c => full.push(c),
        }
    }
    Some(full)
}

/// Whether `full` stays inside `root`, including through symlinks on the existing prefix.
fn covers(root: &Path, full: &Path) -> bool {
    let canon = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    // Resolve the existing prefix (symlinks, and spellings such as macOS's
    // /var vs /private/var), keep the not-yet-existing tail, then compare.
    match full.ancestors().find_map(|a| std::fs::canonicalize(a).ok().map(|real| (a, real))) {
        Some((existing, real)) => real.join(full.strip_prefix(existing).unwrap_or(Path::new(""))).starts_with(&canon),
        None => full.starts_with(root),
    }
}

/// Whether `path` (relative to `root` or absolute) stays inside `root`, after
/// resolving `..` and any symlinks along the existing part of the path.
pub(crate) fn inside(root: &Path, path: &str) -> bool {
    lexical(root, path).is_some_and(|full| covers(root, &full))
}

/// Whether `path`, resolved against `primary`, stays inside `primary` or any
/// additional directory. Relative paths still start at `primary`.
pub(crate) fn inside_any(primary: &Path, extra: &[PathBuf], path: &str) -> bool {
    let Some(full) = lexical(primary, path) else {
        return false;
    };
    covers(primary, &full) || extra.iter().any(|root| covers(root, &full))
}

/// Whether a grant for a file tool may cover writing `path`: non-empty,
/// inside the workspace, and nowhere under (or linked into) `.git`.
pub(crate) fn file_grant_ok(path: &str, cwd: &Path) -> bool {
    // Hooks and config under .git run later with full permissions: always ask.
    let git_component = |p: &Path| {
        p.components().any(|c| {
            c.as_os_str()
                .to_str()
                .is_some_and(|s| s.trim_end_matches(['.', ' ']).eq_ignore_ascii_case(".git"))
        })
    };
    let resolved = cwd.join(path);
    // Follow links by hand too: canonicalize fails on a dangling link
    // (e.g. `notes.txt -> .git/commondir`), which would otherwise slip by.
    let link_into_git = |mut p: PathBuf| {
        for _ in 0..8 {
            let Ok(target) = std::fs::read_link(&p) else {
                return false;
            };
            if git_component(&target) {
                return true;
            }
            p = p.parent().map_or(target.clone(), |dir| dir.join(&target));
        }
        true // too many hops: treat as suspicious
    };
    let in_git = git_component(Path::new(path))
        || resolved
            .ancestors()
            .take_while(|a| a.starts_with(cwd))
            .any(|a| link_into_git(a.to_path_buf()))
        || resolved
            .ancestors()
            .find_map(|a| std::fs::canonicalize(a).ok())
            .is_some_and(|real| {
                // Only what lies below the workspace counts; a workspace that
                // itself sits under a directory named `.git` is fine.
                let root = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
                git_component(real.strip_prefix(&root).unwrap_or(&real))
            });
    !path.is_empty() && !in_git && inside(cwd, path)
}

/// Like [`file_grant_ok`], also accepting paths inside `extra` directories.
pub(crate) fn file_grant_ok_in(path: &str, cwd: &Path, extra: &[PathBuf]) -> bool {
    if path.is_empty() {
        return false;
    }
    if file_grant_ok(path, cwd) {
        return true;
    }
    let Some(full) = lexical(cwd, path) else {
        return false;
    };
    extra.iter().any(|root| {
        full.strip_prefix(root).is_ok_and(|rel| {
            if rel.as_os_str().is_empty() {
                return false;
            }
            file_grant_ok(rel.to_string_lossy().as_ref(), root)
        })
    })
}

/// Whether a granted rule covers this call.
#[cfg_attr(not(test), allow(dead_code))]
pub fn is_allowed(allowed: &HashSet<String>, tool: &str, args: &Value, cwd: &Path) -> bool {
    is_allowed_in(allowed, tool, args, cwd, &[])
}

/// Like [`is_allowed`], treating `extra` directories as inside the workspace.
pub fn is_allowed_in(
    allowed: &HashSet<String>,
    tool: &str,
    args: &Value,
    cwd: &Path,
    extra: &[PathBuf],
) -> bool {
    if tool == "bash" {
        let cmd = args.get("command").and_then(Value::as_str).unwrap_or("");
        return allowed
            .iter()
            .filter_map(|r| r.strip_prefix("bash:"))
            .any(|p| bash_matches(p, cmd));
    }
    if is_file_tool(tool) {
        let path = args.get("path").and_then(Value::as_str).unwrap_or("");
        return allowed.contains(tool) && file_grant_ok_in(path, cwd, extra);
    }
    if tool == "subagent" {
        return allowed.contains(&format!("subagent:{}", subagent_mode(args)));
    }
    if tool == "web_fetch" {
        let url = args.get("url").and_then(Value::as_str).unwrap_or("");
        return allowed.contains(&format!("web_fetch:{url}"));
    }
    if matches!(
        tool,
        "read_file" | "list_dir" | "glob" | "grep" | "view_image"
    ) {
        let path = args.get("path").and_then(Value::as_str).unwrap_or(".");
        return allowed.contains(&format!("{tool}:{path}"));
    }
    allowed.contains(tool)
}

#[cfg(test)]
struct ScriptedTurns {
    left: std::sync::Arc<std::sync::Mutex<Vec<crate::types::Response>>>,
}

#[cfg(test)]
#[async_trait::async_trait]
impl Provider for ScriptedTurns {
    fn name(&self) -> &str {
        "fake"
    }
    fn model(&self) -> &str {
        "fake"
    }
    fn set_model(&mut self, _: String) {}
    fn fork(&self) -> Box<dyn Provider> {
        Box::new(ScriptedTurns {
            left: std::sync::Arc::clone(&self.left),
        })
    }
    async fn stream(
        &self,
        _: Request<'_>,
        _: crate::provider::EventSink<'_>,
    ) -> Result<crate::types::Response> {
        let mut left = self.left.lock().unwrap();
        anyhow::ensure!(!left.is_empty(), "script exhausted");
        Ok(left.remove(0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::EventSink;
    use crate::types::Response;

    #[tokio::test]
    async fn compaction_preserves_unsent_tail_on_resume_and_after_manual_compact() {
        let mut agent = agent(
            Response {
                items: vec![Item::Assistant {
                    text: "summary".into(),
                }],
                usage: None,
            },
            None,
        );
        let dir = tempfile::tempdir().unwrap();
        agent.session = Some(Session::create(dir.path(), &agent.cwd, "fake").unwrap());
        agent.push(Item::Assistant {
            text: "old answer".into(),
        });
        agent.submitted = agent.items.len();
        let tail = Item::user("fix the failing test");
        agent.push(tail.clone());
        assert!(agent
            .auto_compact(&mut QuietUi, &CancellationToken::new())
            .await
            .unwrap());
        let id = agent.session.as_ref().unwrap().id.clone();
        let (_, _, loaded) = crate::session::load(dir.path(), &id, &agent.cwd).unwrap();
        assert_eq!(loaded, agent.items);
        assert_eq!(loaded.last(), Some(&tail));
        agent.submitted = 40;
        assert!(agent
            .compact(&mut QuietUi, &CancellationToken::new())
            .await
            .unwrap());
        assert_eq!(agent.submitted, 1);
        agent.push_counted(Item::user("large log".repeat(1000)));
        assert!(agent
            .auto_compact(&mut QuietUi, &CancellationToken::new())
            .await
            .unwrap());
        assert_eq!(
            agent.items.last(),
            Some(&Item::user("large log".repeat(1000)))
        );
    }

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
        fn fork(&self) -> Box<dyn Provider> {
            unimplemented!()
        }
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
            permissions: Default::default(),
            usage: Usage::default(),
            stats: Default::default(),
            context_window: 1000,
            context_tokens: 900,
            context_estimated: false,
            suppress_auto_compact: false,
            submitted: 1,
            hooks: Default::default(),
            sandbox: Default::default(),
            undo: Default::default(),
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
        // A completed response still consumes tokens even when cancellation
        // prevents adopting its summary.
        assert_eq!(agent.usage, Usage { input_tokens: 100, output_tokens: 10, cached_tokens: 5 });
        assert_eq!(agent.stats.requests, 1);
        assert_eq!(std::fs::read(path).unwrap(), saved);
    }

    #[tokio::test]
    async fn compaction_logs_usage_once_in_active_profile() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(home.path().join("models.json"), r#"{"fake":{"models":{"fake":{"cost":{"input":2,"output":8,"cache_read":0.5}}}}}"#).unwrap();
        let usage = Usage { input_tokens: 100, output_tokens: 20, cached_tokens: 40 };
        let mut agent = agent(Response { items: vec![Item::Assistant { text: "summary".into() }], usage: Some(usage) }, None);
        agent.permissions = std::sync::Arc::new(crate::permissions::Permissions { path: home.path().join("permissions.json"), ..Default::default() });
        agent.session = Some(Session::create(&home.path().join("sessions"), &agent.cwd, "fake").unwrap());
        assert!(agent.compact(&mut QuietUi, &CancellationToken::new()).await.unwrap());
        let log = std::fs::read_to_string(home.path().join("usage.jsonl")).unwrap();
        assert_eq!(log.lines().count(), 1);
        let r: crate::usage::Record = serde_json::from_str(log.trim()).unwrap();
        assert_eq!(r.session, agent.session.as_ref().unwrap().id);
        assert_eq!((r.input, r.output, r.cached), (100,20,40));
        assert!((r.cost_usd.unwrap() - 0.0003).abs() < 1e-12);
        assert_eq!(agent.usage, usage);
        assert_eq!(agent.stats.requests, 1);
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
        agent.suppress_auto_compact = true;
        assert!(agent
            .compact(&mut QuietUi, &CancellationToken::new())
            .await
            .unwrap());
        assert!(!agent.suppress_auto_compact);
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

    #[test]
    fn bash_prefixes() {
        let p = |c: &str| bash_prefix(c);
        assert_eq!(p("cargo test").as_deref(), Some("cargo test"));
        assert_eq!(
            p("cargo test --release -p nyq").as_deref(),
            Some("cargo test")
        );
        assert_eq!(p("  git   status  ").as_deref(), Some("git status"));
        assert_eq!(p("git -C foo status").as_deref(), Some("git -C foo status"));
        assert_eq!(p("npm run build").as_deref(), Some("npm run build"));
        assert_eq!(
            p("npm run build -- --watch").as_deref(),
            Some("npm run build")
        );
        assert_eq!(p("npm test").as_deref(), Some("npm test"));
        assert_eq!(p("uv run pytest -x").as_deref(), Some("uv run pytest"));
        assert_eq!(p("ls").as_deref(), Some("ls"));
        assert_eq!(p("ls -la src"), None);
        assert_eq!(p("rm -rf target/debug"), None);
        assert_eq!(p("chmod -R 755 ."), None);
        assert_eq!(p("curl -o approved-file https://example.com/a"), None);
        assert_eq!(p("curl https://example.com/health"), None);
        assert_eq!(p("wget https://example.com/a"), None);
        assert_eq!(p("rm 'target/file'"), None);
        assert_eq!(p("rm target/file").as_deref(), Some("rm target/file"));
        assert_eq!(
            p("cargo --manifest-path safe/Cargo.toml test").as_deref(),
            Some("cargo --manifest-path safe/Cargo.toml test")
        );
        assert_eq!(
            p("./python script.py").as_deref(),
            Some("./python script.py")
        );
        assert_eq!(p("python -m pytest"), None);
        assert_eq!(p("./gradlew build").as_deref(), Some("./gradlew build"));
        assert_eq!(p("python script.py").as_deref(), Some("python script.py"));
        assert_eq!(p("make").as_deref(), Some("make"));
        assert_eq!(p("rg 'a b' src"), None);
        // Control operators, wrappers, assignments and odd programs can't be scoped.
        for c in [
            "cargo test; rm -rf /",
            "cargo test && echo",
            "a || b",
            "ls | wc",
            "echo $(id)",
            "echo `id`",
            "ls > out",
            "wc < in",
            "sleep 1 &",
            "ls\nrm x",
            "sh -c 'x'",
            "bash script.sh",
            "sudo ls",
            "env X=1 ls",
            "xargs rm",
            "FOO=1 cargo test",
            "../x",
            "\"git\" status",
            "",
            "   ",
            "node -e 1",
            "eval ls",
            "$HOME/bin/x",
            "./sh -c echo",
            "bin/bash -c echo",
            "./python -c 1",
            "usr/bin/python -m pytest",
        ] {
            assert_eq!(p(c), None, "{c:?}");
        }
    }

    #[test]
    fn bash_matching() {
        assert!(bash_matches("cargo test", "cargo test"));
        assert!(bash_matches("cargo test", "cargo   test --lib agent"));
        assert!(!bash_matches("cargo test", "cargo testx"));
        assert!(!bash_matches("cargo test", "cargo build"));
        assert!(!bash_matches("cargo test", "cargo"));
        assert!(!bash_matches("cargo test", "cargo test && rm -rf ~"));
        assert!(!bash_matches("cargo test", "cargo test | tee log"));
        assert!(!bash_matches("cargo test", "cargo test > log"));
        assert!(!bash_matches("cargo test", "cargo test $(evil)"));
        assert!(!bash_matches("cargo test", "cargo test\nrm x"));
        assert!(bash_matches("ls", "ls -la"));
        assert!(!bash_matches("ls", "lsof"));
        assert!(bash_matches("rm -rf", "rm -rf dir"));
        assert!(!bash_matches("rm -rf", "rm dir"));
        assert!(bash_matches("ls -la", "ls -la src"));
        assert!(!bash_matches("ls -la", "ls src"));
        assert!(!bash_matches("git -C", "git status"));
        assert!(bash_matches("git -C", "git -C elsewhere reset --hard"));
        assert!(!bash_matches(
            "git -C safe status",
            "git -C elsewhere reset --hard"
        ));
        assert!(!bash_matches("", "anything"));
    }

    #[test]
    fn rules_are_scoped() {
        use serde_json::json;
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        assert_eq!(
            grant_rule("bash", &json!({"command": "git status -s"})).as_deref(),
            Some("bash:git status")
        );
        assert_eq!(grant_rule("bash", &json!({"command": "a; b"})), None);
        assert_eq!(
            grant_rule("write_file", &json!({"path": "x"})).as_deref(),
            Some("write_file")
        );
        let write: HashSet<String> = ["write_file".into()].into();
        assert!(is_allowed(
            &write,
            "write_file",
            &json!({"path": "src/x.rs"}),
            cwd
        ));
        assert!(!is_allowed(
            &write,
            "write_file",
            &json!({"path": ".git/hooks/pre-commit"}),
            cwd
        ));
        assert!(!is_allowed(
            &write,
            "edit_file",
            &json!({"path": "./.git/config"}),
            cwd
        ));
        assert!(!is_allowed(
            &write,
            "write_file",
            &json!({"path": "sub/.git/config"}),
            cwd
        ));
        #[cfg(unix)]
        {
            // A dangling link into .git must not ride on an "always" grant.
            std::os::unix::fs::symlink(".git/commondir", cwd.join("notes.txt")).unwrap();
            assert!(!is_allowed(
                &write,
                "write_file",
                &json!({"path": "notes.txt"}),
                cwd
            ));
            std::os::unix::fs::symlink("src", cwd.join("code")).unwrap();
            assert!(is_allowed(
                &write,
                "write_file",
                &json!({"path": "code/x.rs"}),
                cwd
            ));
        }
        for path in [".GIT/config", ".git. /hooks/new"] {
            assert!(!is_allowed(
                &write,
                "write_file",
                &json!({"path": path}),
                cwd
            ));
        }
        #[cfg(unix)]
        {
            std::fs::create_dir(cwd.join(".git")).unwrap();
            std::os::unix::fs::symlink(".git", cwd.join("docs")).unwrap();
            for path in ["docs/config", "docs/hooks/new"] {
                assert!(!is_allowed(
                    &write,
                    "write_file",
                    &json!({"path": path}),
                    cwd
                ));
            }
        }
        assert_eq!(
            grant_rule("mcp__wiki__ask", &json!({})).as_deref(),
            Some("mcp__wiki__ask")
        );
        assert_eq!(
            grant_rule("web_fetch", &json!({"url": "https://example.com/a"})).as_deref(),
            Some("web_fetch:https://example.com/a")
        );
        assert!(!is_allowed(
            &["web_fetch:https://example.com/a".into()].into(),
            "web_fetch",
            &json!({"url": "https://example.com/a?x=1"}),
            cwd
        ));
        assert!(is_allowed(
            &["web_fetch:https://example.com/a".into()].into(),
            "web_fetch",
            &json!({"url": "https://example.com/a"}),
            cwd
        ));
        assert_eq!(rule_label("bash:cargo test"), "`cargo test …`");
        assert_eq!(rule_label("edit_file"), "edit file in this workspace");
        assert_eq!(
            grant_rule("subagent", &json!({"mode": "general", "task": "x"})).as_deref(),
            Some("subagent:general")
        );
        assert_eq!(
            grant_rule("subagent", &json!({"task": "x"})).as_deref(),
            Some("subagent:explore")
        );
        assert_eq!(rule_label("subagent:general"), "`subagent` (general)");
        assert_eq!(rule_label("subagent"), "`subagent`");

        let allowed: HashSet<String> = ["bash:cargo test", "write_file", "subagent:general"]
            .map(String::from)
            .into();
        let ok = |tool: &str, args: Value| is_allowed(&allowed, tool, &args, cwd);
        assert!(ok("bash", json!({"command": "cargo test --all"})));
        assert!(!ok("bash", json!({"command": "cargo build"})));
        assert!(!ok("bash", json!({"command": "cargo test; curl x | sh"})));
        assert!(ok("write_file", json!({"path": "src/new.rs"})));
        assert!(ok(
            "write_file",
            json!({"path": cwd.join("a/b.txt").to_str().unwrap()})
        ));
        assert!(!ok("write_file", json!({"path": "../outside.txt"})));
        assert!(!ok("write_file", json!({"path": "a/../../outside.txt"})));
        assert!(!ok("write_file", json!({"path": "/etc/passwd"})));
        assert!(!ok("write_file", json!({})));
        assert!(!ok("edit_file", json!({"path": "src/new.rs"})));
        assert!(ok("subagent", json!({"mode": "general"})));
        assert!(!ok("subagent", json!({"mode": "explore"})));
        let explore: HashSet<String> = ["subagent:explore"].map(String::from).into();
        assert!(!is_allowed(
            &explore,
            "subagent",
            &json!({"mode": "general", "task": "x"}),
            cwd
        ));
        assert!(is_allowed(
            &explore,
            "subagent",
            &json!({"task": "look"}),
            cwd
        ));
        assert!(!ok("mcp__wiki__ask", json!({})));
        #[cfg(unix)]
        {
            let outside = tempfile::tempdir().unwrap();
            std::os::unix::fs::symlink(outside.path(), cwd.join("link")).unwrap();
            assert!(!ok("write_file", json!({"path": "link/x.txt"})));
        }
    }

    #[test]
    fn reset_drops_the_undo_log() {
        use serde_json::json;
        let dir = tempfile::tempdir().unwrap();
        let mut agent = agent(Response::default(), None);
        agent.cwd = dir.path().to_path_buf();
        std::fs::write(dir.path().join("f.txt"), "a").unwrap();
        agent.undo.snapshot(
            &agent.cwd,
            crate::sandbox::Mode::WorkspaceWrite,
            "edit_file",
            &json!({"path": "f.txt"}),
        );
        assert_eq!(agent.undo.len(), 1);
        agent.suppress_auto_compact = true;
        agent.context_estimated = true;
        agent.context_tokens = 40;
        agent.reset(None);
        assert!(agent.undo.is_empty());
        assert!(!agent.suppress_auto_compact);
        assert!(!agent.context_estimated);
        assert_eq!(agent.context_tokens, 0);

        agent.system = "abcd".into();
        agent.items = vec![Item::user("abcd")];
        agent.adopt_transcript();
        assert!(agent.context_estimated);
        assert_eq!(agent.context_tokens, 2);
        agent.set_context_window(1000);
        assert_eq!(agent.context_window, 1000);
        assert!(!agent.suppress_auto_compact);
    }

    #[test]
    fn push_keeps_the_item_when_the_session_cannot_be_saved() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("not-a-directory");
        std::fs::write(&blocker, "x").unwrap();
        let mut agent = agent(Response::default(), None);
        agent.session = Some(Session::create(&blocker, std::path::Path::new("."), "m").unwrap());
        agent.items.clear();
        agent.push(Item::user("hello"));
        assert_eq!(agent.items, vec![Item::user("hello")]);
    }

    #[tokio::test]
    async fn failed_autocompact_does_not_repeat() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Arc, Mutex};

        #[derive(Clone)]
        struct Scripted {
            left: Arc<Mutex<Vec<Response>>>,
            calls: Arc<AtomicUsize>,
        }

        #[async_trait::async_trait]
        impl Provider for Scripted {
            fn name(&self) -> &str {
                "fake"
            }
            fn model(&self) -> &str {
                "fake"
            }
            fn set_model(&mut self, _: String) {}
            fn fork(&self) -> Box<dyn Provider> {
                Box::new(self.clone())
            }
            async fn stream(&self, _: Request<'_>, _: EventSink<'_>) -> Result<Response> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                let mut left = self.left.lock().unwrap();
                anyhow::ensure!(!left.is_empty(), "script exhausted");
                Ok(left.remove(0))
            }
        }

        struct NoteUi(Vec<String>);
        #[async_trait::async_trait]
        impl Ui for NoteUi {
            fn stream(&mut self, _: &StreamEvent) {}
            fn end_message(&mut self) {}
            fn tool_start(&mut self, _: &str, _: &str) {}
            fn tool_end(&mut self, _: &str, _: &ToolOutput) {}
            async fn approve(&mut self, _: &ApprovalRequest<'_>) -> Approval {
                Approval::Yes
            }
            fn notice(&mut self, msg: &str) {
                self.0.push(msg.to_string());
            }
        }

        let scripted = Scripted {
            left: Arc::new(Mutex::new(vec![
                Response {
                    items: vec![Item::Assistant {
                        text: "summary".into(),
                    }],
                    usage: Some(Usage {
                        input_tokens: 1,
                        output_tokens: 1000,
                        cached_tokens: 0,
                    }),
                },
                Response {
                    items: vec![Item::Assistant {
                        text: "answer".into(),
                    }],
                    usage: Some(Usage {
                        input_tokens: 900,
                        output_tokens: 50,
                        cached_tokens: 0,
                    }),
                },
                Response {
                    items: vec![Item::Assistant {
                        text: "next".into(),
                    }],
                    usage: Some(Usage {
                        input_tokens: 900,
                        output_tokens: 10,
                        cached_tokens: 0,
                    }),
                },
            ])),
            calls: Arc::new(AtomicUsize::new(0)),
        };
        let calls = scripted.calls.clone();
        let mut agent = agent(Response::default(), None);
        agent.provider = Box::new(scripted);
        let mut ui = NoteUi(Vec::new());
        assert!(matches!(
            agent
                .turn(Item::user("hi"), &mut ui, CancellationToken::new())
                .await
                .unwrap(),
            TurnEnd::Done
        ));
        assert!(agent.suppress_auto_compact);
        assert!(ui
            .0
            .iter()
            .any(|m| m.contains("Automatic compaction is off")));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(matches!(
            agent
                .turn(Item::user("again"), &mut ui, CancellationToken::new())
                .await
                .unwrap(),
            TurnEnd::Done
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn unsent_user_text_reaches_the_model() {
        use std::sync::{Arc, Mutex};

        #[derive(Clone)]
        struct Rec(Arc<Mutex<bool>>);

        #[async_trait::async_trait]
        impl Provider for Rec {
            fn name(&self) -> &str {
                "fake"
            }
            fn model(&self) -> &str {
                "fake"
            }
            fn set_model(&mut self, _: String) {}
            fn fork(&self) -> Box<dyn Provider> {
                Box::new(self.clone())
            }
            async fn stream(&self, req: Request<'_>, _: EventSink<'_>) -> Result<Response> {
                let answering = req.items.last().is_none_or(
                    |item| !matches!(item, Item::User { text, .. } if text == COMPACT_PROMPT),
                );
                if answering && req.items.iter().any(|item| matches!(item, Item::User { text, .. } if text.contains("UNIQUE_PASTE"))) {
                    *self.0.lock().unwrap() = true;
                }
                Ok(Response {
                    items: vec![Item::Assistant { text: "ok".into() }],
                    usage: Some(Usage {
                        input_tokens: 10,
                        output_tokens: 1,
                        cached_tokens: 0,
                    }),
                })
            }
        }

        let saw = Arc::new(Mutex::new(false));
        let mut agent = agent(Response::default(), None);
        agent.provider = Box::new(Rec(saw.clone()));
        let paste = format!("UNIQUE_PASTE {}", "line\n".repeat(50));
        let end = agent
            .turn(Item::user(paste), &mut QuietUi, CancellationToken::new())
            .await
            .unwrap();
        assert!(matches!(end, TurnEnd::Done));
        assert!(
            *saw.lock().unwrap(),
            "the unsent paste was summarized away before the model saw it"
        );
        assert!(
            agent.items.iter().any(
                |item| matches!(item, Item::User { text, .. } if text.contains("UNIQUE_PASTE"))
            ) || agent
                .items
                .iter()
                .any(|item| matches!(item, Item::Compaction { .. }))
        );
    }

    #[tokio::test]
    async fn failed_write_keeps_undo_only_when_it_changed_the_file() {
        struct MutTool {
            changed: bool,
        }
        #[async_trait::async_trait]
        impl crate::tools::Tool for MutTool {
            fn spec(&self) -> crate::types::ToolSpec {
                crate::types::ToolSpec {
                    name: "write_file".into(),
                    description: String::new(),
                    parameters: serde_json::json!({}),
                }
            }
            fn summary(&self, _: &Value) -> String {
                "f.txt".into()
            }
            async fn run(&self, _: Value, _: &ToolCtx) -> ToolOutput {
                ToolOutput {
                    content: "nope".into(),
                    is_error: true,
                    changed: self.changed,
                    ..ToolOutput::default()
                }
            }
        }

        let script = |changed: bool| {
            let mut agent = agent(Response::default(), None);
            agent.context_window = 0;
            agent.context_tokens = 0;
            agent.tools.add(Box::new(MutTool { changed }));
            agent.provider = Box::new(ScriptedTurns {
                left: std::sync::Arc::new(std::sync::Mutex::new(vec![
                    Response {
                        items: vec![Item::ToolCall {
                            id: "1".into(),
                            name: "write_file".into(),
                            arguments: r#"{"path":"f.txt"}"#.into(),
                        }],
                        usage: Some(Usage {
                            input_tokens: 1,
                            output_tokens: 1,
                            cached_tokens: 0,
                        }),
                    },
                    Response {
                        items: vec![Item::Assistant {
                            text: "done".into(),
                        }],
                        usage: Some(Usage {
                            input_tokens: 2,
                            output_tokens: 1,
                            cached_tokens: 0,
                        }),
                    },
                ])),
            });
            agent
        };

        let mut kept = script(true);
        kept.turn(Item::user("go"), &mut QuietUi, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(kept.undo.len(), 1);

        let mut dropped = script(false);
        dropped
            .turn(Item::user("go"), &mut QuietUi, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(dropped.undo.len(), 0);
    }

    /// An MCP-like tool that asks by default and counts its runs.
    struct CountTool(std::sync::Arc<std::sync::atomic::AtomicUsize>);
    #[async_trait::async_trait]
    impl crate::tools::Tool for CountTool {
        fn spec(&self) -> crate::types::ToolSpec {
            crate::types::ToolSpec {
                name: "mcp__wiki__ask".into(),
                description: String::new(),
                parameters: serde_json::json!({}),
            }
        }
        fn needs_approval(&self, _: &Value) -> bool {
            true
        }
        fn summary(&self, _: &Value) -> String {
            "q".into()
        }
        async fn run(&self, _: Value, _: &ToolCtx) -> ToolOutput {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            ToolOutput::ok("answer")
        }
    }

    /// Answers approvals with `answer`, recording (rule, save_rule) per request.
    struct ScriptUi {
        answer: Approval,
        asked: Vec<(Option<String>, Option<String>)>,
    }
    #[async_trait::async_trait]
    impl Ui for ScriptUi {
        fn stream(&mut self, _: &StreamEvent) {}
        fn end_message(&mut self) {}
        fn tool_start(&mut self, _: &str, _: &str) {}
        fn tool_end(&mut self, _: &str, _: &ToolOutput) {}
        async fn approve(&mut self, req: &ApprovalRequest<'_>) -> Approval {
            self.asked.push((req.rule.clone(), req.save_rule.clone()));
            self.answer
        }
    }

    fn calling_agent(runs: &std::sync::Arc<std::sync::atomic::AtomicUsize>, calls: usize) -> Agent {
        let mut script = Vec::new();
        for i in 0..calls {
            script.push(Response {
                items: vec![Item::ToolCall {
                    id: i.to_string(),
                    name: "mcp__wiki__ask".into(),
                    arguments: "{}".into(),
                }],
                usage: None,
            });
        }
        script.push(Response {
            items: vec![Item::Assistant {
                text: "done".into(),
            }],
            usage: None,
        });
        let mut agent = agent(Response::default(), None);
        agent.yolo = false;
        agent.context_window = 0;
        agent.tools.add(Box::new(CountTool(runs.clone())));
        agent.provider = Box::new(ScriptedTurns {
            left: std::sync::Arc::new(std::sync::Mutex::new(script)),
        });
        agent
    }

    fn results(agent: &Agent) -> Vec<String> {
        agent
            .items
            .iter()
            .filter_map(|i| match i {
                Item::ToolResult { output, .. } => Some(output.clone()),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn always_save_grants_and_writes_permissions_json() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let home = tempfile::tempdir().unwrap();
        let runs = std::sync::Arc::new(AtomicUsize::new(0));
        let mut agent = calling_agent(&runs, 2);
        agent.permissions =
            std::sync::Arc::new(crate::permissions::Permissions::load(home.path(), home.path()).0);
        let mut ui = ScriptUi {
            answer: Approval::AlwaysSave,
            asked: Vec::new(),
        };
        agent
            .turn(Item::user("go"), &mut ui, CancellationToken::new())
            .await
            .unwrap();
        // Asked once; the second call rides on the grant.
        assert_eq!(
            ui.asked,
            vec![(
                Some("`mcp__wiki__ask`".into()),
                Some("mcp__wiki__ask".into())
            )]
        );
        assert_eq!(runs.load(Ordering::SeqCst), 2);
        assert!(agent.allowed.contains("mcp__wiki__ask"));
        assert_eq!(agent.permissions.allow, ["mcp__wiki__ask"]);
        let (reloaded, _) = crate::permissions::Permissions::load(home.path(), home.path());
        assert_eq!(reloaded.allow, ["mcp__wiki__ask"]);
    }

    #[tokio::test]
    async fn deny_and_ask_rules_shape_the_loop() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let rules = |allow: &[&str], ask: &[&str], deny: &[&str]| {
            let v = |r: &[&str]| r.iter().map(|s| s.to_string()).collect();
            std::sync::Arc::new(crate::permissions::Permissions {
                allow: v(allow),
                ask: v(ask),
                deny: v(deny),
                ..Default::default()
            })
        };
        // deny: never asks or runs, even with --yolo.
        let runs = std::sync::Arc::new(AtomicUsize::new(0));
        let mut agent = calling_agent(&runs, 1);
        agent.yolo = true;
        agent.permissions = rules(&["mcp__*"], &[], &["mcp__wiki__*"]);
        let mut ui = ScriptUi {
            answer: Approval::Yes,
            asked: Vec::new(),
        };
        agent
            .turn(Item::user("go"), &mut ui, CancellationToken::new())
            .await
            .unwrap();
        assert!(ui.asked.is_empty());
        assert_eq!(runs.load(Ordering::SeqCst), 0);
        assert!(
            results(&agent)[0].contains("blocked by permissions.json rule `deny mcp__wiki__*`"),
            "{:?}",
            results(&agent)
        );

        // ask: asks every time despite a session grant, and offers no "always".
        let runs = std::sync::Arc::new(AtomicUsize::new(0));
        let mut agent = calling_agent(&runs, 2);
        agent.allowed.insert("mcp__wiki__ask".into());
        agent.permissions = rules(&["mcp__*"], &["mcp__wiki__ask"], &[]);
        let mut ui = ScriptUi {
            answer: Approval::Always,
            asked: Vec::new(),
        };
        agent
            .turn(Item::user("go"), &mut ui, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(ui.asked, vec![(None, None), (None, None)]);
        assert_eq!(runs.load(Ordering::SeqCst), 2);

        // allow: runs without asking.
        let runs = std::sync::Arc::new(AtomicUsize::new(0));
        let mut agent = calling_agent(&runs, 1);
        agent.permissions = rules(&["mcp__wiki__*"], &[], &[]);
        let mut ui = ScriptUi {
            answer: Approval::No,
            asked: Vec::new(),
        };
        agent
            .turn(Item::user("go"), &mut ui, CancellationToken::new())
            .await
            .unwrap();
        assert!(ui.asked.is_empty());
        assert_eq!(runs.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn images_and_encrypted_blobs_have_a_flat_cost() {
        use crate::types::Image;
        let image = Image {
            media_type: "image/png".into(),
            data: "A".repeat(400_000),
        };
        let item = Item::User {
            text: "why".into(),
            images: vec![image.clone(), image],
        };
        let tokens = item_tokens(&item);
        assert_eq!(tokens, text_tokens("why") + 2 * IMAGE_TOKENS);
        assert!(tokens < 10_000, "{tokens}");

        let reasoning = Item::Reasoning {
            id: None,
            summary: "abcd".into(),
            encrypted: Some("z".repeat(80_000)),
        };
        assert_eq!(item_tokens(&reasoning), 1);
        let mut agent = agent(Response::default(), None);
        agent.system.clear();
        agent.items = vec![reasoning];
        agent.adopt_transcript();
        assert_eq!(agent.context_tokens, 1);
    }

    #[tokio::test]
    async fn single_oversized_message_is_not_compacted() {
        use crate::types::Image;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        struct Probe {
            saw_compact: Arc<AtomicBool>,
            saw_image: Arc<AtomicBool>,
        }

        #[async_trait::async_trait]
        impl Provider for Probe {
            fn name(&self) -> &str {
                "fake"
            }
            fn model(&self) -> &str {
                "fake"
            }
            fn set_model(&mut self, _: String) {}
            fn fork(&self) -> Box<dyn Provider> {
                unimplemented!()
            }
            async fn stream(&self, req: Request<'_>, _: EventSink<'_>) -> Result<Response> {
                if req
                    .items
                    .iter()
                    .any(|i| matches!(i, Item::User { text, .. } if text == COMPACT_PROMPT))
                {
                    self.saw_compact.store(true, Ordering::SeqCst);
                }
                if req
                    .items
                    .iter()
                    .any(|i| matches!(i, Item::User { images, .. } if !images.is_empty()))
                {
                    self.saw_image.store(true, Ordering::SeqCst);
                }
                Ok(Response {
                    items: vec![Item::Assistant {
                        text: "answer".into(),
                    }],
                    usage: Some(Usage {
                        input_tokens: 10,
                        output_tokens: 1,
                        cached_tokens: 0,
                    }),
                })
            }
        }

        let saw_compact = Arc::new(AtomicBool::new(false));
        let saw_image = Arc::new(AtomicBool::new(false));
        let mut agent = agent(Response::default(), None);
        agent.provider = Box::new(Probe {
            saw_compact: saw_compact.clone(),
            saw_image: saw_image.clone(),
        });
        agent.items.clear();
        agent.context_tokens = 0;
        agent.context_window = 1000;
        let item = Item::User {
            text: "why".into(),
            images: vec![Image {
                media_type: "image/png".into(),
                data: "A".repeat(20_000),
            }],
        };
        agent
            .turn(item, &mut QuietUi, CancellationToken::new())
            .await
            .unwrap();
        assert!(!saw_compact.load(Ordering::SeqCst));
        assert!(saw_image.load(Ordering::SeqCst));
        assert!(agent
            .items
            .iter()
            .any(|i| matches!(i, Item::User { images, .. } if !images.is_empty())));
    }

    #[tokio::test]
    async fn viewed_image_is_not_compacted_before_the_model_sees_it() {
        use crate::tools::{Tool, ToolOutput};
        use crate::types::{Image, ToolSpec};
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        struct Probe {
            saw_compact: Arc<AtomicBool>,
            saw_image: Arc<AtomicBool>,
        }

        #[async_trait::async_trait]
        impl Provider for Probe {
            fn name(&self) -> &str {
                "fake"
            }
            fn model(&self) -> &str {
                "fake"
            }
            fn set_model(&mut self, _: String) {}
            fn fork(&self) -> Box<dyn Provider> {
                Box::new(Probe {
                    saw_compact: self.saw_compact.clone(),
                    saw_image: self.saw_image.clone(),
                })
            }
            async fn stream(&self, req: Request<'_>, _: EventSink<'_>) -> Result<Response> {
                if req
                    .items
                    .iter()
                    .any(|i| matches!(i, Item::User { text, .. } if text == COMPACT_PROMPT))
                {
                    self.saw_compact.store(true, Ordering::SeqCst);
                    return Ok(Response {
                        items: vec![Item::Assistant {
                            text: "summary".into(),
                        }],
                        usage: None,
                    });
                }
                if req
                    .items
                    .iter()
                    .any(|i| matches!(i, Item::User { images, .. } if !images.is_empty()))
                {
                    self.saw_image.store(true, Ordering::SeqCst);
                    return Ok(Response {
                        items: vec![Item::Assistant {
                            text: "A pixel.".into(),
                        }],
                        usage: Some(Usage {
                            input_tokens: 10,
                            output_tokens: 1,
                            cached_tokens: 0,
                        }),
                    });
                }
                Ok(Response {
                    items: vec![Item::ToolCall {
                        id: "1".into(),
                        name: "view_image".into(),
                        arguments: r#"{"path":"shot.png"}"#.into(),
                    }],
                    usage: Some(Usage {
                        input_tokens: 10,
                        output_tokens: 1,
                        cached_tokens: 0,
                    }),
                })
            }
        }

        struct ImageTool;
        #[async_trait::async_trait]
        impl Tool for ImageTool {
            fn spec(&self) -> ToolSpec {
                ToolSpec {
                    name: "view_image".into(),
                    description: String::new(),
                    parameters: serde_json::json!({}),
                }
            }
            fn summary(&self, _: &Value) -> String {
                "shot.png".into()
            }
            async fn run(&self, _: Value, _: &crate::tools::ToolCtx) -> ToolOutput {
                ToolOutput {
                    content: "Loaded shot.png".into(),
                    images: vec![Image {
                        media_type: "image/png".into(),
                        data: "A".repeat(20_000),
                    }],
                    ..ToolOutput::default()
                }
            }
        }

        let saw_compact = Arc::new(AtomicBool::new(false));
        let saw_image = Arc::new(AtomicBool::new(false));
        let mut agent = agent(Response::default(), None);
        agent.provider = Box::new(Probe {
            saw_compact: saw_compact.clone(),
            saw_image: saw_image.clone(),
        });
        agent.tools.add(Box::new(ImageTool));
        agent.items.clear();
        agent.context_tokens = 0;
        agent.context_window = 1000;
        agent.context_estimated = false;
        agent
            .turn(
                Item::user("inspect the screenshot"),
                &mut QuietUi,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!saw_compact.load(Ordering::SeqCst));
        assert!(saw_image.load(Ordering::SeqCst));
    }

    #[test]
    fn additional_directories_count_as_inside_for_reads_and_write_grants() {
        use serde_json::json;
        let root = tempfile::tempdir().unwrap();
        let primary = root.path().join("primary");
        let extra = root.path().join("extra");
        std::fs::create_dir(&primary).unwrap();
        std::fs::create_dir(&extra).unwrap();
        std::fs::create_dir(extra.join(".git")).unwrap();
        let note = extra.join("note.txt");
        std::fs::write(&note, "x").unwrap();
        let note = note.to_str().unwrap();
        assert!(inside(&primary, "note.txt"));
        assert!(inside_any(
            &primary,
            std::slice::from_ref(&extra),
            "../extra/note.txt"
        ));
        assert!(!inside(&primary, "../extra/note.txt"));
        assert!(!inside_any(
            &primary,
            std::slice::from_ref(&extra),
            "../missing/note.txt"
        ));
        let mut allowed = HashSet::new();
        allowed.insert("write_file".into());
        assert!(is_allowed_in(
            &allowed,
            "write_file",
            &json!({"path": note}),
            &primary,
            std::slice::from_ref(&extra)
        ));
        assert!(!is_allowed(
            &allowed,
            "write_file",
            &json!({"path": note}),
            &primary
        ));
        let git = extra.join(".git").join("config");
        assert!(!file_grant_ok_in(git.to_str().unwrap(), &primary, &[extra]));
    }

    #[tokio::test]
    async fn continue_and_rename_do_not_store_a_new_user_message() {
        struct Script {
            text: String,
            saw: std::sync::Arc<std::sync::Mutex<String>>,
        }
        #[async_trait::async_trait]
        impl Provider for Script {
            fn name(&self) -> &str {
                "fake"
            }
            fn model(&self) -> &str {
                "fake"
            }
            fn set_model(&mut self, _: String) {}
            fn fork(&self) -> Box<dyn Provider> {
                unimplemented!()
            }
            async fn stream(&self, req: Request<'_>, _: EventSink<'_>) -> Result<Response> {
                *self.saw.lock().unwrap() = req.system.to_string();
                assert!(req.items.iter().all(|item| !matches!(item, Item::User { text, .. } if text.contains("Continue from where"))));
                Ok(Response {
                    items: vec![Item::Assistant {
                        text: self.text.clone(),
                    }],
                    usage: None,
                })
            }
        }
        let saw = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let mut agent = agent(Response::default(), None);
        agent.provider = Box::new(Script {
            text: "rest of the answer".into(),
            saw: saw.clone(),
        });
        agent.items = vec![Item::Assistant {
            text: "partial".into(),
        }];
        agent.system = "system".into();
        agent
            .continue_turn(&mut QuietUi, CancellationToken::new())
            .await
            .unwrap();
        assert!(saw
            .lock()
            .unwrap()
            .contains("Continue from where you stopped"));
        assert_eq!(agent.items.len(), 2);
        assert!(matches!(&agent.items[0], Item::Assistant { text } if text == "partial"));
        assert!(agent
            .items
            .iter()
            .all(|item| !matches!(item, Item::User { .. })));

        agent.items.clear();
        assert!(agent
            .continue_turn(&mut QuietUi, CancellationToken::new())
            .await
            .is_err());

        agent.provider = Box::new(Script {
            text: "\"Fix the parser bug.\"".into(),
            saw,
        });
        agent.items = vec![Item::user("the parser fails on quotes")];
        let before = agent.items.clone();
        let title = agent
            .suggest_title(&CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(title, "Fix the parser bug");
        assert_eq!(agent.items, before);
    }

    #[tokio::test]
    async fn clear_context_keeps_the_session_and_drops_the_transcript() {
        let dir = tempfile::tempdir().unwrap();
        let mut agent = agent(Response::default(), None);
        agent.cwd = dir.path().to_path_buf();
        agent.session = Some(Session::create(dir.path(), &agent.cwd, "m").unwrap());
        agent.items.clear();
        agent.push(Item::user("hello"));
        agent.allowed.insert("bash:ls".into());
        let id = agent.session.as_ref().unwrap().id.clone();
        agent.clear_context().unwrap();
        assert_eq!(agent.session.as_ref().unwrap().id, id);
        assert!(agent.items.is_empty());
        assert!(agent.allowed.is_empty());
        assert!(agent.undo.is_empty());
        let (_, _, items) = crate::session::load(dir.path(), &id, &agent.cwd).unwrap();
        assert!(items.is_empty());
    }

    /// A hooks configuration with one user hook, for the wiring tests.
    #[cfg(unix)]
    fn hooks_with(event: crate::hooks::Event, command: &str) -> std::sync::Arc<crate::hooks::Hooks> {
        std::sync::Arc::new(crate::hooks::Hooks::from_user_hook(crate::hooks::Hook {
            event,
            matcher: None,
            command: command.to_string(),
            timeout_secs: 5,
        }))
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn pre_tool_use_hook_blocks_the_call() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let runs = std::sync::Arc::new(AtomicUsize::new(0));
        let mut blocked = calling_agent(&runs, 1);
        blocked.hooks = hooks_with(
            crate::hooks::Event::PreToolUse,
            r#"echo '{"decision":"block","reason":"not on my watch"}'"#,
        );
        blocked
            .turn(Item::user("go"), &mut QuietUi, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(runs.load(Ordering::SeqCst), 0);
        let out = &results(&blocked)[0];
        assert!(out.contains("PreToolUse hook"), "{out}");
        assert!(out.contains("not on my watch"), "{out}");
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn stop_hook_runs_after_the_turn() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("stopped");
        let mut agent = calling_agent(&std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)), 0);
        agent.hooks = hooks_with(
            crate::hooks::Event::Stop,
            &format!("echo done >> {}", marker.display()),
        );
        agent
            .turn(Item::user("go"), &mut QuietUi, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), "done\n");
    }
    #[tokio::test]
    #[cfg(unix)]
    async fn prompt_hooks_add_context_or_block() {
        let mut agent = calling_agent(&std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)), 0);
        agent.hooks = hooks_with(crate::hooks::Event::UserPromptSubmit, "echo branch: main");
        let live = CancellationToken::new();
        assert_eq!(crate::hooks::prompt_submit(&mut agent, "hi", &live).await, None);
        assert!(matches!(agent.items.last(), Some(Item::User { text, .. }) if text.ends_with("branch: main")));
        agent.hooks = hooks_with(crate::hooks::Event::UserPromptSubmit, r#"echo '{"decision":"block","reason":"no"}'"#);
        let n = agent.items.len();
        let msg = crate::hooks::prompt_submit(&mut agent, "hi", &live).await.unwrap();
        assert!(msg.contains("no"), "{msg}");
        assert_eq!(agent.items.len(), n);
    }
}
