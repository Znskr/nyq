//! The core loop: send the transcript, stream the reply, run any tool calls,
//! feed results back, repeat until the model answers without tools.

use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};

use anyhow::Result;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::permissions::Decision;
use crate::provider::Provider;
use crate::session::Session;
use crate::tools::{Registry, ToolCtx, ToolOutput};
use crate::types::{Item, Request, StreamEvent, Usage};

/// Default upper bound on model round-trips per user turn.
const MAX_STEPS: usize = 100;

/// The step limit: `NYQ_MAX_AGENT_STEPS`, else `max_agent_steps` in
/// settings, else [`MAX_STEPS`], with where it came from.
pub fn step_limit() -> (usize, &'static str) {
    let parse = |v: &str| v.trim().parse::<usize>().ok().filter(|n| *n > 0);
    if let Some(n) = std::env::var("NYQ_MAX_AGENT_STEPS").ok().as_deref().and_then(parse) {
        return (n, "NYQ_MAX_AGENT_STEPS");
    }
    match crate::settings::current().max_agent_steps.filter(|n| *n > 0) {
        Some(n) => (n, "settings"),
        None => (MAX_STEPS, "default"),
    }
}

fn max_steps() -> usize {
    step_limit().0
}

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
    async fn approve_noted(&self, origin: &str, req: &ApprovalRequest<'_>) -> (Approval, Option<String>) {
        (self.approve(origin, req).await, None)
    }
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
            description: "Ask the user up to 4 multiple-choice questions when you need decisions or missing \
                information that you cannot reasonably infer. Give each question 2-6 short, distinct options, \
                each with a one-line description of what choosing it means; the user may also type their own \
                answer or skip. Do not use it for confirmation of routine steps."
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "questions": {
                        "type": "array",
                        "description": "1-4 questions, asked in order.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "question": {"type": "string", "description": "The question, one or two sentences."},
                                "options": {
                                    "type": "array",
                                    "description": "2-6 answer choices.",
                                    "items": {
                                        "type": "object",
                                        "properties": {
                                            "label": {"type": "string", "description": "The choice, a few words."},
                                            "description": {"type": "string", "description": "What choosing it means."}
                                        },
                                        "required": ["label"]
                                    }
                                }
                            },
                            "required": ["question", "options"]
                        }
                    }
                },
                "required": ["questions"]
            }),
        }
    }
    fn summary(&self, args: &Value) -> String {
        questions(args).into_iter().next().map(|(q, _)| q).unwrap_or_default()
    }
    async fn run(&self, _args: Value, _ctx: &ToolCtx) -> ToolOutput {
        ToolOutput::err("ask_user_question is answered by the user interface")
    }
}

/// Appended to a reply the user interrupted while it streamed.
const INTERRUPTED: &str = "[interrupted by the user]";

/// A failing call repeated this many times gets told to change approach.
const REPEAT_LIMIT: usize = 3;

/// Nested instruction files are cut to this many characters.
const NESTED_INSTRUCTIONS_MAX: usize = 64 * 1024;

/// Tools that only read, so consecutive calls to them can run at once.
const PARALLEL_TOOLS: &[&str] = &[
    "read_file",
    "list_dir",
    "glob",
    "grep",
    "view_image",
    "web_fetch",
    "web_search",
    crate::tools::artifacts::TOOL,
    "skill",
];

/// Whether this call may run alongside its read-only neighbours: read tools,
/// and subagent runs (independent by contract).
fn runs_in_parallel(name: &str, arguments: &str) -> bool {
    if PARALLEL_TOOLS.contains(&name) {
        return true;
    }
    if name != "subagent" {
        return false;
    }
    let args: Value = serde_json::from_str(arguments).unwrap_or_default();
    matches!(args.get("action").and_then(Value::as_str), None | Some("run"))
}

/// An approved call, ready to run.
struct Ready {
    name: String,
    args: Value,
    ctx: ToolCtx,
    summary: String,
    /// Arguments for the PostToolUse hook, when one observes this tool.
    hook_args: Option<Value>,
    /// The note the user attached when approving, passed on with the result.
    note: Option<String>,
}

enum Prepared {
    /// Answered without running the tool.
    Done(ToolOutput),
    Run(Box<Ready>),
}

/// The transcript as sent: `note` appended to the newest item when it is a
/// user message or tool result. Earlier items are unchanged, so the cached prefix holds;
/// the stored transcript never contains the note.
fn with_trailing_note<'a>(items: &'a [Item], note: &str) -> std::borrow::Cow<'a, [Item]> {
    // Only on the newest item: a note on an older one would change the
    // prefix the provider already cached (e.g. when continuing a reply).
    let last = items.len().wrapping_sub(1);
    if note.is_empty() || !matches!(items.last(), Some(Item::User { .. } | Item::ToolResult { .. })) {
        return std::borrow::Cow::Borrowed(items);
    }
    let at = last;
    let mut out = items.to_vec();
    match &mut out[at] {
        Item::User { text, .. } => {
            text.push_str("\n\n");
            text.push_str(note);
        }
        Item::ToolResult { output, .. } => {
            output.push_str("\n\n");
            output.push_str(note);
        }
        _ => {}
    }
    std::borrow::Cow::Owned(out)
}

async fn run_ready(tools: &Registry, ready: &Ready, cancel: &CancellationToken) -> ToolOutput {
    let Some(tool) = tools.get(&ready.name) else {
        return ToolOutput::err(format!("Unknown tool `{}`.", ready.name));
    };
    if tool.handles_cancel() {
        tool.run(ready.args.clone(), &ready.ctx).await
    } else {
        tokio::select! {
            out = tool.run(ready.args.clone(), &ready.ctx) => out,
            _ = cancel.cancelled() => ToolOutput::err("Cancelled by user."),
        }
    }
}

/// The questions in an `ask_user_question` call: (question, [(label, description)]).
/// The older single `question` with plain-string options is accepted too.
fn questions(args: &Value) -> Vec<(String, Vec<(String, String)>)> {
    let option = |o: &Value| match o {
        Value::String(s) => Some((s.clone(), String::new())),
        _ => Some((
            o.get("label")?.as_str()?.to_string(),
            o.get("description").and_then(Value::as_str).unwrap_or("").to_string(),
        )),
    };
    let one = |q: &Value| {
        let text = q.get("question")?.as_str()?.trim().to_string();
        let options = q
            .get("options")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(option).collect())
            .unwrap_or_default();
        (!text.is_empty()).then_some((text, options))
    };
    match args.get("questions").and_then(Value::as_array) {
        Some(list) => list.iter().filter_map(one).take(MAX_QUESTIONS).collect(),
        None => one(args).into_iter().collect(),
    }
}

const MAX_QUESTIONS: usize = 4;

/// Put each question to the user and report the answers.
async fn ask_user(args: &Value, ui: &mut dyn Ui, cancel: &CancellationToken) -> ToolOutput {
    let asked = questions(args);
    if asked.is_empty() {
        return ToolOutput::err("ask_user_question: give `questions`, each with a `question` and `options`.");
    }
    let mut answers = Vec::new();
    for (question, options) in &asked {
        let shown: Vec<String> = options
            .iter()
            .map(|(label, about)| if about.is_empty() { label.clone() } else { format!("{label} — {about}") })
            .collect();
        let answer = tokio::select! {
            a = ui.ask(question, &shown) => a,
            _ = cancel.cancelled() => return ToolOutput::err("Cancelled by user."),
        };
        // A picked option is reported by its label.
        let answer = answer.map(|a| shown.iter().position(|s| *s == a).map_or(a, |i| options[i].0.clone()));
        answers.push((question, answer));
    }
    if answers.iter().all(|(_, a)| a.is_none()) {
        return ToolOutput::ok("The user skipped the question. Use your best judgment or ask in your reply.");
    }
    if let [(_, Some(a))] = answers.as_slice() {
        return ToolOutput::ok(format!("The user answered: {a}"));
    }
    let lines: Vec<String> = answers
        .iter()
        .map(|(q, a)| format!("- {q} → {}", a.as_deref().unwrap_or("(skipped)")))
        .collect();
    ToolOutput::ok(format!("The user answered:\n{}", lines.join("\n")))
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
    /// Session id usage rows should use when this agent has no session file
    /// (a subagent records against its parent's session).
    pub usage_session_id: Option<String>,
    pub cwd: PathBuf,
    /// How risky calls are decided. Deny rules still block in every mode.
    pub permission_mode: crate::permissions::Mode,
    /// Identical auto-mode decisions reused for the rest of this session,
    /// including a subagent's calls.
    pub review_cache: std::sync::Arc<std::sync::Mutex<HashMap<String, crate::classifier::Verdict>>>,
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
        self.usage.add(&u);
        // Permissions are loaded from the active profile, including in ACP and subagents.
        if let Some(home) = self
            .permissions
            .path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
        {
            let session = self
                .usage_session_id
                .as_deref()
                .or_else(|| self.session.as_ref().map(|s| s.id.as_str()))
                .unwrap_or(self.stats.id.as_str());
            if let Err(e) = crate::usage::append(
                home,
                session,
                self.provider.name(),
                self.provider.pricing_provider(),
                self.provider.model(),
                u,
            ) {
                crate::ui::error(&format!("could not save usage: {e:#}"));
            }
        }
        Some(u)
    }

    /// Save the model settings in use with the session, so resuming restores
    /// them. Only changes are written.
    pub fn sync_prefs(&mut self) {
        let Some(s) = &mut self.session else { return };
        let prefs = crate::session::Prefs {
            provider: self.provider.name().into(),
            model: self.provider.model().into(),
            effort: self.provider.effort().map(Into::into),
            fast: self.provider.fast(),
        };
        if let Err(e) = s.set_prefs(&prefs) {
            crate::ui::error(&format!("could not save session settings: {e:#}"));
        }
    }

    pub fn push(&mut self, item: Item) {
        self.stats.item(&item);
        self.sync_prefs();
        if let Some(s) = &mut self.session {
            if let Err(e) = s.append(&item) {
                crate::ui::error(&format!("could not save session: {e:#}"));
            }
        }
        self.items.push(item);
    }

    /// Run AttentionRequired hooks (in the background): nyq waits for the user.
    fn attention(&self, kind: &str, message: &str) {
        let session = self
            .session
            .as_ref()
            .map(|s| s.id.as_str())
            .or(self.usage_session_id.as_deref());
        self.hooks.attention(kind, message, &self.cwd, session);
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
        let Some(input) = self.extension_prompt(input, ui).await else {
            return Ok(TurnEnd::Done);
        };
        self.push_counted(input);
        self.drive(ui, cancel, false).await
    }

    /// Run extensions' `user_prompt` handlers: they may block the prompt
    /// (None), rewrite its text, or add context before it.
    async fn extension_prompt(&mut self, mut input: Item, ui: &mut dyn Ui) -> Option<Item> {
        let Some(ext) = self.hooks.extensions().cloned() else { return Some(input) };
        let Item::User { text, .. } = &mut input else { return Some(input) };
        let edit = ext.user_prompt(text).await;
        if let Some(msg) = edit.blocked {
            ui.notice(&msg);
            return None;
        }
        if let Some(prompt) = edit.prompt {
            *text = prompt;
        }
        for context in edit.context {
            ui.notice(context.lines().next().unwrap_or(""));
            self.push_counted(Item::user(context));
        }
        Some(input)
    }

    /// Steering messages from the user and from extensions (`session/send`).
    fn take_steering(&self, ui: &mut dyn Ui) -> Vec<Item> {
        let mut items = ui.take_steering();
        if let Some(ext) = self.hooks.extensions() {
            for text in ext.take_queued(true) {
                ui.notice(&format!("An extension steers: {}", text.lines().next().unwrap_or("")));
                items.push(Item::user(text));
            }
        }
        items
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
        let max_steps = max_steps();
        // A Stop hook may keep the turn going once.
        let mut stop_continued = false;
        // Failing calls by (tool, arguments), to stop the model looping on one.
        let mut failures: HashMap<(String, String), usize> = HashMap::new();
        for _ in 0..max_steps {
            self.sync_estimate();
            if !self.auto_compact(ui, &cancel).await? {
                return Ok(TurnEnd::Cancelled);
            }
            // MCP servers that connected or changed their tools since the last step.
            self.tools.refresh();
            let specs = self.advertised();
            // The system prompt stays stable between requests (so providers
            // can cache it); per-request state goes in a trailing note.
            let mut system = self.system.clone();
            if let Some(section) = crate::tools::mcp_search::notes_section(&self.tools.mcp_notes()) {
                system.push_str("\n\n");
                system.push_str(&section);
            }
            if continue_note {
                system.push_str(CONTINUE_NOTE);
            }
            let items = with_trailing_note(&self.items, &self.session_notes());
            crate::trace::log("provider", || {
                format!(
                    "request {}/{} · {} items · {} tools",
                    self.provider.name(),
                    self.provider.model(),
                    self.items.len(),
                    specs.len()
                )
            });
            let started = std::time::Instant::now();
            let mut partial = String::new();
            let result = {
                let req = Request {
                    system: &system,
                    items: &items,
                    tools: &specs,
                };
                let mut sink = |ev: StreamEvent| {
                    if let StreamEvent::Text(text) = &ev {
                        partial.push_str(text);
                    }
                    ui.stream(&ev)
                };
                let r = tokio::select! {
                    r = crate::provider::stream_with_retry(self.provider.as_ref(), req, &mut sink) => Some(r),
                    _ = cancel.cancelled() => None,
                };
                self.stats.api_time += started.elapsed();
                r
            };
            ui.end_message();
            let Some(result) = result else {
                crate::trace::log("provider", || "cancelled".into());
                // Keep what was said, so the next turn knows it was cut short.
                if !partial.trim().is_empty() {
                    self.push_counted(Item::Assistant {
                        text: format!("{}\n\n{INTERRUPTED}", partial.trim_end()),
                    });
                }
                return Ok(TurnEnd::Cancelled);
            };
            crate::trace::log("provider", || match &result {
                Ok(r) => format!(
                    "response in {}ms · {} items · usage {:?}{}",
                    started.elapsed().as_millis(),
                    r.items.len(),
                    r.usage,
                    if r.truncated { " · truncated" } else { "" }
                ),
                Err(e) => format!("error after {}ms: {e:#}", started.elapsed().as_millis()),
            });
            let response = result?;
            if response.truncated {
                ui.notice("The reply was cut off at the output limit.");
            }
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
                let mut queued = self.take_steering(ui);
                if queued.is_empty() {
                    queued.extend(ui.take_follow_up());
                }
                if queued.is_empty() {
                    if let Some(ext) = self.hooks.extensions() {
                        for text in ext.take_queued(false) {
                            ui.notice(&format!("An extension follows up: {}", text.lines().next().unwrap_or("")));
                            queued.push(Item::user(text));
                        }
                    }
                }
                if queued.is_empty() {
                    self.sync_estimate();
                    let reason = crate::hooks::stop(self, &cancel).await;
                    match reason.filter(|_| !stop_continued) {
                        Some(reason) => {
                            stop_continued = true;
                            ui.notice(&format!("A Stop hook continues the turn: {reason}"));
                            queued.push(Item::user(format!(
                                "[A Stop hook asked you to keep going before finishing]\n{reason}"
                            )));
                        }
                        None => {
                            if let Some(ext) = self.hooks.extensions() {
                                let last = self.items.iter().rev().find_map(|i| match i {
                                    Item::Assistant { text } => Some(text.as_str()),
                                    _ => None,
                                });
                                ext.turn_end(last.unwrap_or(""));
                            }
                            return Ok(TurnEnd::Done);
                        }
                    }
                }
                for item in queued {
                    self.push_counted(item);
                }
                continue;
            }

            let mut images = Vec::new();
            let mut i = 0;
            while i < calls.len() {
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
                // Consecutive read-only calls (and subagents) run together.
                let run = calls[i..]
                    .iter()
                    .take_while(|(_, name, arguments)| runs_in_parallel(name, arguments))
                    .count()
                    .max(1);
                let batch = &calls[i..i + run];
                let outs = if run > 1 {
                    self.call_parallel(batch, ui, &cancel).await
                } else {
                    let (id, name, arguments) = &batch[0];
                    vec![self.call_tool(id, name, arguments, ui, &cancel).await]
                };
                for ((id, name, arguments), mut out) in batch.iter().zip(outs) {
                    if out.is_error {
                        let n = failures
                            .entry((name.clone(), arguments.clone()))
                            .or_default();
                        *n += 1;
                        if *n >= REPEAT_LIMIT {
                            out.content.push_str(&format!(
                                "\n\n[This exact call has now failed {n} times. Do not repeat it: \
                                 change your approach, or stop and explain the blocker.]"
                            ));
                        }
                    } else {
                        self.add_nested_instructions(name, arguments, &mut out);
                    }
                    images.extend(self.record_result(id, name, out));
                }
                i += run;
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
            for item in self.take_steering(ui) {
                self.push_counted(item);
            }
        }
        // Not an error: the work so far is kept and the user can go on.
        ui.notice(&format!(
            "Stopped after {max_steps} model steps without a final answer. Send a message to continue \
             (the limit is NYQ_MAX_AGENT_STEPS or max_agent_steps in settings)."
        ));
        self.submitted = self.items.len();
        Ok(TurnEnd::Done)
    }

    /// Whether the session model cannot read images (see `tools::image::VISION`).
    fn text_only(&self) -> bool {
        crate::tools::image::text_only(self.provider.pricing_provider(), self.provider.model())
    }

    /// The tools offered on the next request: `vision` replaces `view_image`
    /// for a text-only model.
    fn advertised(&self) -> Vec<crate::types::ToolSpec> {
        let hidden = if self.text_only() { "view_image" } else { crate::tools::image::VISION };
        let mut specs = self.tools.advertised();
        specs.retain(|s| s.name != hidden);
        specs
    }

    /// Store a tool result in the transcript (credentials scrubbed, large
    /// output spilled) and return the images it carried.
    fn record_result(&mut self, id: &str, name: &str, mut out: ToolOutput) -> Vec<crate::types::Image> {
        // Credentials never reach the model or the session file.
        if let std::borrow::Cow::Owned(clean) = crate::tools::redact::scrub(&out.content) {
            out.content = clean;
        }
        // Oversized output is stored; the model gets head, tail and an id.
        let key = crate::tools::artifacts::session_key(self.session.as_ref().map(|s| s.id.as_str()));
        out.content = crate::tools::artifacts::spill(
            &crate::config::home(),
            &key,
            id,
            name,
            std::mem::take(&mut out.content),
        );
        // Usage precedes tool execution, so account for the new text
        // before deciding whether the next step needs compaction.
        // An estimate is replaced wholesale on the next sync.
        if !self.context_estimated {
            self.context_tokens = self
                .context_tokens
                .saturating_add((out.content.len() as u64).div_ceil(4));
        }
        self.push(Item::ToolResult {
            id: id.to_string(),
            output: out.content,
            is_error: out.is_error,
        });
        out.images
    }

    /// Per-request notes on the session's current state: permission mode,
    /// question UI, background processes. They go at the end of the request
    /// (see [`with_trailing_note`]), not in the system prompt, so the cached
    /// prompt prefix survives a change.
    fn session_notes(&self) -> String {
        let mode = self.permission_mode;
        let mut notes = vec![format!(
            "- Permission mode: {} ({}). A call the user or a rule denies must not be retried as is.",
            mode.as_str(),
            mode.summary()
        )];
        // The root agent of a session without ask_user_question: print mode
        // or an editor. Subagents have their own instructions.
        if self.usage_session_id.is_none() && self.tools.get(ASK_USER).is_none() {
            notes.push(
                "- There is no question UI: do not stop mid-task to wait for answers. If you are blocked, \
                 finish with a short description of the blocker and the options."
                    .into(),
            );
        }
        let procs = crate::tools::background::context_lines();
        if !procs.is_empty() {
            notes.push(format!(
                "- Background processes (reuse a matching running server instead of starting a duplicate; \
                 ended ones are no longer running):\n{}",
                procs.join("\n")
            ));
        }
        format!("[Session state]\n{}", notes.join("\n"))
    }

    /// After a successful file or search call, append any `AGENTS.md` (or
    /// `CLAUDE.md`) from workspace subdirectories it touched, once per
    /// conversation (an earlier copy in the transcript counts).
    fn add_nested_instructions(&self, name: &str, arguments: &str, out: &mut ToolOutput) {
        let file_tool = matches!(name, "read_file" | "edit_file" | "write_file");
        if !file_tool && !matches!(name, "list_dir" | "glob" | "grep") {
            return;
        }
        let args: Value = serde_json::from_str(arguments).unwrap_or_default();
        let path = args.get("path").and_then(Value::as_str).unwrap_or(".");
        let Some(full) = lexical(&self.cwd, path) else {
            return;
        };
        if !inside(&self.cwd, path) {
            return;
        }
        let dir = if file_tool || !full.is_dir() {
            full.parent().map(Path::to_path_buf).unwrap_or_default()
        } else {
            full.clone()
        };
        let Ok(rel) = dir.strip_prefix(&self.cwd) else {
            return;
        };
        let mut at = self.cwd.clone();
        for part in rel.components() {
            at.push(part);
            for file in ["AGENTS.md", "CLAUDE.md"] {
                let candidate = at.join(file);
                if candidate == full || !candidate.is_file() {
                    continue;
                }
                let shown = crate::tools::slash_path(candidate.strip_prefix(&self.cwd).unwrap_or(&candidate));
                let marker = format!("[Instructions from {shown}");
                let seen = self.items.iter().any(|item| match item {
                    Item::ToolResult { output, .. } => output.contains(&marker),
                    Item::Compaction { summary } => summary.contains(&marker),
                    _ => false,
                });
                if seen {
                    continue;
                }
                let Ok(text) = crate::tools::read_rooted_text(&self.cwd, &candidate) else {
                    continue;
                };
                let text: String = text.trim().chars().take(NESTED_INSTRUCTIONS_MAX).collect();
                if !text.is_empty() {
                    let scope = crate::tools::slash_path(at.strip_prefix(&self.cwd).unwrap_or(&at));
                    out.content.push_str(&format!(
                        "\n\n{marker}, which apply to files under {scope}/; the narrowest scope wins:]\n{text}"
                    ));
                }
            }
        }
    }

    /// Ask the session provider whether one risky call may run. `ctx` lets
    /// a file change be summarized from the tool's preview.
    async fn auto_review(
        &mut self,
        id: &str,
        name: &str,
        arguments: &str,
        args: &Value,
        ctx: &ToolCtx,
        cancel: &CancellationToken,
    ) -> crate::classifier::ReviewResult {
        let tool = self.tools.get(name);
        let file_change = is_file_tool(name).then(|| {
            let path = args.get("path").and_then(Value::as_str).unwrap_or("");
            let existed = !path.is_empty() && self.cwd.join(path).exists();
            let preview = tool.and_then(|t| t.preview(args, ctx)).unwrap_or_default();
            let content = args.get("content").and_then(Value::as_str);
            crate::classifier::file_change(path, existed, &preview, content)
        });
        let input_schema = tool
            .filter(|_| crate::tools::mcp_search::is_mcp(name))
            .map(|t| t.spec().parameters.to_string());
        let mut provider = self.provider.fork();
        if let Some(model) = crate::settings::classifier_model().or_else(|| provider.reviewer_model()) {
            provider.set_model(model);
        }
        provider.set_effort(None);
        provider.set_fast(false);
        let (user_request, prior, older_omitted) = crate::classifier::turn_context(&self.items);
        let mut action = crate::classifier::Action::new(name, arguments, &user_request);
        action.call_id = id.to_string();
        action.cwd = self.cwd.display().to_string();
        action.background = args
            .get("run_in_background")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        action.prior = prior;
        action.origin = if self.session.is_none() {
            "subagent"
        } else {
            "root"
        }
        .to_string();
        action.branch = crate::ui::statusline::git_branch(&self.cwd);
        action.older_omitted = older_omitted;
        action.file_change = file_change;
        action.input_schema = input_schema;
        let prepared = crate::classifier::prepare(&action);
        if prepared.skip.is_none() {
            let hit = self
                .review_cache
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&prepared.cache_key)
                .cloned();
            if let Some(verdict) = hit {
                return crate::classifier::ReviewResult {
                    verdict,
                    fallback: false,
                    usage: None,
                    cancelled: false,
                };
            }
        }
        let mut local = HashMap::new();
        let result = crate::classifier::review(
            provider.as_ref(),
            &action,
            &mut local,
            crate::classifier::TIMEOUT,
            cancel,
        )
        .await;
        if !result.fallback && !result.cancelled {
            if let Some(verdict) = local.remove(&prepared.cache_key) {
                self.review_cache
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(prepared.cache_key, verdict);
            }
        }
        result
    }

    async fn call_tool(
        &mut self,
        id: &str,
        name: &str,
        arguments: &str,
        ui: &mut dyn Ui,
        cancel: &CancellationToken,
    ) -> ToolOutput {
        let ready = match self.prepare(id, name, arguments, ui, cancel).await {
            Prepared::Done(out) => return out,
            Prepared::Run(ready) => ready,
        };
        ui.tool_start(name, &ready.summary);
        let before = self.undo.len();
        self.undo
            .snapshot(&self.cwd, self.sandbox.mode, name, &ready.args);
        let mut out = run_ready(&self.tools, &ready, cancel).await;
        if out.is_error && !out.changed && self.undo.len() > before {
            self.undo.discard_last();
        }
        ui.tool_end(name, &out);
        self.settle(&ready, &mut out, cancel).await;
        out
    }

    /// Run a batch of read-only calls (and subagents) concurrently. They are
    /// approved one by one first; the UI then sees each start and finish in
    /// order while all of them make progress.
    async fn call_parallel(
        &mut self,
        batch: &[(String, String, String)],
        ui: &mut dyn Ui,
        cancel: &CancellationToken,
    ) -> Vec<ToolOutput> {
        use futures::StreamExt;
        let mut prepared = Vec::new();
        for (id, name, arguments) in batch {
            prepared.push(self.prepare(id, name, arguments, ui, cancel).await);
        }
        let mut outs = Vec::new();
        {
            let tools = &self.tools;
            let mut running: futures::stream::FuturesOrdered<_> = prepared
                .iter()
                .filter_map(|p| match p {
                    Prepared::Run(ready) => Some(run_ready(tools, ready, cancel)),
                    Prepared::Done(_) => None,
                })
                .collect();
            for p in &prepared {
                outs.push(match p {
                    Prepared::Done(out) => out.clone(),
                    Prepared::Run(ready) => {
                        ui.tool_start(&ready.name, &ready.summary);
                        let out = running
                            .next()
                            .await
                            .unwrap_or_else(|| ToolOutput::err("The call did not run."));
                        ui.tool_end(&ready.name, &out);
                        out
                    }
                });
            }
        }
        for (p, out) in prepared.iter().zip(outs.iter_mut()) {
            if let Prepared::Run(ready) = p {
                self.settle(ready, out, cancel).await;
            }
        }
        outs
    }

    /// Bookkeeping after a call ran: undo snapshots, file-change stats, the
    /// approval note, the memory section, grants a subagent collected, and
    /// PostToolUse hooks.
    async fn settle(&mut self, ready: &Ready, out: &mut ToolOutput, cancel: &CancellationToken) {
        self.undo.append(std::mem::take(&mut out.undo));
        if matches!(ready.name.as_str(), "write_file" | "edit_file") && !out.is_error {
            self.stats.file_change(&ready.name, &ready.args, out.display.as_deref());
        }
        crate::trace::log("tool", || {
            format!(
                "{} {} · {} bytes",
                ready.name,
                if out.is_error { "failed" } else { "ok" },
                out.content.len()
            )
        });
        if let Some(note) = &ready.note {
            out.content
                .push_str(&format!("\n\n[The user approved this call with a note: {note}]"));
        }
        if ready.name == "memory" && !out.is_error {
            crate::tools::memory::refresh_section(&mut self.system, &crate::config::home());
        }
        // "Always" answers given for a subagent's calls belong to this session.
        self.allowed.extend(
            ready
                .ctx
                .granted
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .drain(..),
        );
        self.hooks.tool_result(&ready.name, &ready.args, out).await;
        if let Some(args) = &ready.hook_args {
            self.hooks
                .post_tool_use(&ready.name, args, out, &self.cwd, ready.ctx.session_id.as_deref(), cancel)
                .await;
        }
    }

    /// Hooks, permissions and approval for one call. `Done` carries the
    /// result of a call that was answered without running (denied, blocked,
    /// a question for the user, MCP tool search).
    async fn prepare(
        &mut self,
        id: &str,
        name: &str,
        arguments: &str,
        ui: &mut dyn Ui,
        cancel: &CancellationToken,
    ) -> Prepared {
        let meta = self.tools.is_meta(name);
        if !meta && self.tools.get(name).is_none() {
            return Prepared::Done(ToolOutput::err(format!("Unknown tool `{name}`.")));
        }
        if name == ASK_USER {
            let args: Value = serde_json::from_str(arguments).unwrap_or_default();
            if let Some((question, _)) = questions(&args).into_iter().next() {
                self.attention("question", &question);
            }
            return Prepared::Done(ask_user(&args, ui, cancel).await);
        }
        let mut args: Value = match serde_json::from_str(if arguments.trim().is_empty() {
            "{}"
        } else {
            arguments
        }) {
            Ok(v) => v,
            Err(e) => return Prepared::Done(ToolOutput::err(format!("Invalid JSON arguments: {e}"))),
        };
        let ctx = ToolCtx {
            cwd: self.cwd.clone(),
            cancel: cancel.clone(),
            sandbox: self.sandbox.clone(),
            provider: Some(std::sync::Arc::from(self.provider.fork())),
            yolo: self.permission_mode.is_yolo(),
            permission_mode: self.permission_mode,
            review_cache: std::sync::Arc::clone(&self.review_cache),
            allowed: self.allowed.clone(),
            approver: ui.approver(),
            permissions: self.permissions.clone(),
            granted: Default::default(),
            session_id: self
                .session
                .as_ref()
                .map(|s| s.id.clone())
                .or_else(|| self.usage_session_id.clone()),
            hooks: self.hooks.clone(),
            subagent_dir: self.session.as_ref().map(|s| crate::subagents::store_dir(&s.path)),
            shared_tools: self.tools.shared(),
        };
        let summary_of = |args: &Value| {
            if meta {
                args.get("query")
                    .and_then(Value::as_str)
                    .or_else(|| args.get("name").and_then(Value::as_str))
                    .unwrap_or("")
                    .to_string()
            } else {
                self.tools
                    .get(name)
                    .map(|tool| tool.summary(args))
                    .unwrap_or_default()
            }
        };
        let mut summary = summary_of(&args);
        // Reported without running: shown as a finished row.
        fn done(ui: &mut dyn Ui, name: &str, summary: &str, out: ToolOutput) -> Prepared {
            ui.tool_start(name, summary);
            ui.tool_end(name, &out);
            Prepared::Done(out)
        }

        let session_id = ctx.session_id.clone();
        let mut arguments = std::borrow::Cow::Borrowed(arguments);
        match self
            .hooks
            .pre_tool_use(name, &args, &self.cwd, session_id.as_deref(), cancel)
            .await
        {
            Err(msg) => return done(ui, name, &summary, ToolOutput::err(msg)),
            Ok(Some(updated)) => {
                // A hook rewrote the call: everything below sees the new input.
                arguments = std::borrow::Cow::Owned(updated.to_string());
                args = updated;
                summary = summary_of(&args);
            }
            Ok(None) => {}
        }
        let arguments = arguments.as_ref();

        let mut needs_approval = self
            .tools
            .get(name)
            .is_some_and(|tool| tool.needs_approval_for(&args, &self.cwd));
        if needs_approval && crate::permissions::is_read_tool(name) {
            let path = args.get("path").and_then(Value::as_str).unwrap_or(".");
            if inside_any(&self.cwd, &self.sandbox.writable, path) {
                needs_approval = false;
            }
        }
        // Shell commands: read-only pipelines run without asking (and run as
        // the rebuilt, hardened command); auto mode skips the classifier for
        // known reversible dev commands.
        let command = (name == "bash"
            && args.get("run_in_background").and_then(Value::as_bool) != Some(true))
        .then(|| args.get("command").and_then(Value::as_str).map(str::to_string))
        .flatten();
        let read_only = command.as_deref().and_then(|c| {
            crate::command_effect::read_only(c, &self.cwd, &self.sandbox.writable)
        });
        if read_only.is_some() {
            needs_approval = false;
        }
        let forced = match self.permissions.decide_in(
            &self.allowed,
            name,
            &args,
            &self.cwd,
            &self.sandbox.writable,
            needs_approval,
            self.permission_mode,
        ) {
            Decision::Run => None,
            Decision::Deny(rule) => {
                return done(
                    ui,
                    name,
                    &summary,
                    ToolOutput::err(format!(
                        "This call was blocked by permissions.json rule `{rule}` and did not run. \
                         Do not retry it; find another way or ask the user."
                    )),
                );
            }
            Decision::Ask { forced } => Some(forced),
            Decision::Classify
                if command.as_deref().is_some_and(|c| {
                    crate::command_effect::known_reversible(c, &self.cwd, &self.sandbox.writable)
                }) =>
            {
                None
            }
            Decision::Classify => {
                let review = self
                    .auto_review(id, name, arguments, &args, &ctx, cancel)
                    .await;
                if let Some(usage) = review.usage {
                    self.account_usage(Some(usage), ui);
                }
                if review.cancelled {
                    return done(ui, name, &summary, ToolOutput::err("Cancelled by user."));
                }
                match &review.verdict {
                    crate::classifier::Verdict::Allow { .. } => None,
                    crate::classifier::Verdict::Deny { .. } => {
                        let reason = review.verdict.reason();
                        return done(
                            ui,
                            name,
                            &summary,
                            ToolOutput::err(format!(
                                "The permission classifier denied this call ({reason}) and it did not run. \
                                 Do not retry it; find another way or ask the user."
                            )),
                        );
                    }
                    crate::classifier::Verdict::Ask { .. } => {
                        let reason = review.verdict.reason();
                        if review.fallback {
                            ui.notice(reason);
                        } else {
                            ui.notice(&format!("Classifier: {reason}"));
                        }
                        Some(None)
                    }
                }
            }
        };
        // Hidden MCP tools are not callable by a remembered name. Deny rules
        // and hooks above still apply; this runs only once the call is allowed.
        if let Some(msg) = self.tools.deferred_block(name) {
            return done(ui, name, &summary, ToolOutput::err(msg));
        }
        let preview = self.tools.get(name).and_then(|tool| tool.preview(&args, &ctx));
        let mut note = None;
        let asked = forced.is_some();
        if let Some(forced) = forced {
            // A call that an `ask` rule forces can't be covered by a grant.
            let rule = if forced.is_some() {
                None
            } else {
                grant_rule_in(name, &args, &self.cwd, &self.sandbox.writable)
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
            self.attention("approval", &format!("Approve {name}: {summary}"));
            let decision = tokio::select! {
                biased;
                _ = cancel.cancelled() => None,
                answer = ui.approve(&req) => Some(answer),
            };
            let Some(answer) = decision else {
                return done(ui, name, &summary, ToolOutput::err("Cancelled by user."));
            };
            note = ui.approval_note();
            match answer {
                Approval::Yes => {}
                Approval::Always => {
                    self.allowed.extend(rule);
                }
                Approval::AlwaysSave | Approval::AlwaysSaveLocal => {
                    self.allowed.extend(rule);
                    if let Some(r) = save_rule {
                        let perms = std::sync::Arc::make_mut(&mut self.permissions);
                        let saved = if answer == Approval::AlwaysSaveLocal {
                            crate::grants::save_local(perms, &r).map(|_| " for this workspace")
                        } else {
                            perms.save_allow(&r).map(|_| "")
                        };
                        match saved {
                            Ok(scope) => ui.notice(&format!(
                                "Saved `{r}` to the allow list in permissions.json{scope}."
                            )),
                            Err(e) => ui.notice(&format!(
                                "Could not save `{r}` to permissions.json: {e:#}"
                            )),
                        }
                    }
                }
                Approval::No => {
                    let out = ToolOutput::err(match &note {
                        Some(note) => format!(
                            "The user denied this tool call and said: {note}"
                        ),
                        None => "The user denied this tool call. Ask what they want instead."
                            .into(),
                    });
                    ui.tool_end(name, &out);
                    return Prepared::Done(out);
                }
            }
        }
        // The user approved the diff they saw. If the file changed while the
        // prompt was open, applying the call now would write something else.
        if asked && is_file_tool(name) && preview.is_some() {
            let now = self.tools.get(name).and_then(|tool| tool.preview(&args, &ctx));
            if now != preview {
                return done(
                    ui,
                    name,
                    &summary,
                    ToolOutput::err(
                        "The file changed after the approval preview was shown, so nothing was written. \
                         Read the file again and make a new call.",
                    ),
                );
            }
        }
        if let Some(cmd) = read_only {
            args["command"] = Value::String(cmd);
        }

        if meta {
            let hook_args = self.hooks.observes_post_tool_use(name).then(|| args.clone());
            let out = self.tools.handle_meta(name, arguments).unwrap_or_else(|| {
                ToolOutput::err(format!("`{name}` is not available."))
            });
            ui.tool_start(name, &summary);
            ui.tool_end(name, &out);
            if let Some(args) = hook_args {
                self.hooks
                    .post_tool_use(name, &args, &out, &self.cwd, session_id.as_deref(), cancel)
                    .await;
            }
            return Prepared::Done(out);
        }
        let hook_args = self.hooks.observes_post_tool_use(name).then(|| args.clone());
        Prepared::Run(Box::new(Ready {
            name: name.to_string(),
            args,
            ctx,
            summary,
            hook_args,
            note,
        }))
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
        let mut prompt = COMPACT_PROMPT.to_string();
        if let Some(ext) = self.hooks.extensions() {
            for extra in ext.before_compact().await {
                prompt.push_str(&format!("\n\nAdditional instructions for this summary:\n{extra}"));
            }
        }
        items.push(Item::user(prompt));
        let req = Request {
            system: &self.system,
            items: &items,
            tools: &[],
        };
        let mut ignore = |_: StreamEvent| {};
        let started = std::time::Instant::now();
        let response = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Ok(false),
            r = crate::provider::stream_with_retry(self.provider.as_ref(), req, &mut ignore) => r?,
        };
        self.stats.api_time += started.elapsed();
        self.account_usage(response.usage, ui);
        if cancel.is_cancelled() {
            return Ok(false);
        }
        if response.truncated {
            anyhow::bail!("compaction was cut off at the output limit");
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
        let artifact = self.session.as_ref().and_then(|session| {
            session
                .path
                .parent()
                .and_then(|dir| dir.parent())
                .map(|home| (home.to_path_buf(), session.id.clone()))
        });
        if let Some(session) = &mut self.session {
            session.clear_transcript()?;
        }
        if let Some((home, id)) = artifact {
            crate::tools::artifacts::remove_session(&home, &id);
        }
        self.items.clear();
        self.usage = Usage::default();
        let stats_id = std::mem::take(&mut self.stats.id);
        self.stats = Default::default();
        self.stats.id = stats_id;
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
        let started = std::time::Instant::now();
        let response = tokio::select! {
            biased;
            _ = cancel.cancelled() => anyhow::bail!("Interrupted."),
            r = crate::provider::stream_with_retry(self.provider.as_ref(), req, &mut ignore) => r?,
        };
        self.stats.api_time += started.elapsed();
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

    pub(crate) fn push_counted(&mut self, mut item: Item) {
        // A text-only model gets attached images as files for `vision`.
        if let Item::User { text, images } = &mut item {
            if !images.is_empty() && self.text_only() {
                let key = crate::tools::artifacts::session_key(self.session.as_ref().map(|s| s.id.as_str()));
                let note = crate::tools::image::park_images(&crate::config::home(), &key, images);
                text.push_str(&format!("\n\n{note}"));
                images.clear();
            }
        }
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
    cmd.contains('$')
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

/// The rule "always" would grant for this call (no workspace context).
#[cfg_attr(not(test), allow(dead_code))]
pub fn grant_rule(tool: &str, args: &Value) -> Option<String> {
    grant_rule_in(tool, args, &std::env::current_dir().unwrap_or_default(), &[])
}

/// The rule "always" would grant for this call, as stored in
/// `Agent::allowed`: a bash prefix, the tool, a subagent mode, a site for
/// `web_fetch`, and for paths outside the workspace that directory tree
/// (`read_file:/data/**`, `write_file:/tmp/out/**`).
pub fn grant_rule_in(tool: &str, args: &Value, cwd: &Path, extra: &[PathBuf]) -> Option<String> {
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
        return Some(match site(url) {
            Some(origin) => format!("web_fetch:{origin}/*"),
            None => format!("web_fetch:{url}"),
        });
    }
    let read = crate::permissions::is_read_tool(tool);
    if read || is_file_tool(tool) {
        let path = args
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or(if read { "." } else { "" });
        if !read && file_grant_ok_in(path, cwd, extra) {
            return Some(tool.to_string());
        }
        if path.is_empty() || (!read && path_under_git(args, cwd)) {
            return None;
        }
        let full = resolved(cwd, path)?;
        // A file grants its directory; a searched or listed directory itself.
        let dir = if matches!(tool, "read_file" | "view_image") || !read {
            full.parent()?.to_path_buf()
        } else {
            full
        };
        // Never the whole filesystem.
        dir.parent()?;
        return Some(format!(
            "{tool}:{}/**",
            dir.to_string_lossy().replace('\\', "/").trim_end_matches('/')
        ));
    }
    Some(tool.to_string())
}

/// `scheme://host[:port]` of a URL, when it has one.
fn site(url: &str) -> Option<String> {
    let origin = reqwest::Url::parse(url).ok()?.origin();
    origin.is_tuple().then(|| origin.ascii_serialization())
}

/// A URL as parsed (lowercase host, explicit path).
pub(crate) fn normalize_url(url: &str) -> Option<String> {
    reqwest::Url::parse(url).ok().map(|u| u.to_string())
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
        return match url.strip_suffix("/*") {
            Some(origin) => format!("`web_fetch` on {origin}"),
            None => format!("`web_fetch` {url}"),
        };
    }
    if let Some((name, path)) = rule.split_once(':') {
        if let Some(dir) = path.strip_suffix("/**") {
            let what = if is_file_tool(name) { "edits" } else { "reads" };
            return format!("{what} under {dir} (this session)");
        }
        return format!("`{name}` {path}");
    }
    if is_file_tool(rule) {
        return format!("{} in this workspace", rule.replace('_', " "));
    }
    format!("`{rule}`")
}

/// Whether a file tool's path (with symlinks resolved) is under a `.git`
/// directory.
pub(crate) fn path_under_git(args: &Value, cwd: &Path) -> bool {
    let path = args.get("path").and_then(Value::as_str).unwrap_or("");
    let is_git = |p: &Path| {
        p.components().any(|c| {
            c.as_os_str()
                .to_str()
                .is_some_and(|s| s.trim_end_matches(['.', ' ']).eq_ignore_ascii_case(".git"))
        })
    };
    is_git(Path::new(path)) || resolved(cwd, path).is_none_or(|p| is_git(&p))
}

/// `base.join(path)` normalized, with symlinks along its existing part
/// resolved; None if it escapes via `..`.
pub(crate) fn resolved(base: &Path, path: &str) -> Option<PathBuf> {
    let full = lexical(base, path)?;
    Some(
        match full
            .ancestors()
            .find_map(|a| std::fs::canonicalize(a).ok().map(|real| (a, real)))
        {
            Some((existing, real)) => real.join(full.strip_prefix(existing).unwrap_or(Path::new(""))),
            None => full.clone(),
        },
    )
}

/// Lexical normalization of `base.join(path)`, or None if it escapes via `..`.
pub(crate) fn lexical(base: &Path, path: &str) -> Option<PathBuf> {
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
    match full
        .ancestors()
        .find_map(|a| std::fs::canonicalize(a).ok().map(|real| (a, real)))
    {
        Some((existing, real)) => real
            .join(full.strip_prefix(existing).unwrap_or(Path::new("")))
            .starts_with(&canon),
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
    // Directory-tree grants for paths outside the workspace.
    let tree = || {
        allowed
            .iter()
            .any(|r| crate::permissions::path_rule_matches(r, tool, args, cwd, true))
    };
    if is_file_tool(tool) {
        let path = args.get("path").and_then(Value::as_str).unwrap_or("");
        return (allowed.contains(tool) && file_grant_ok_in(path, cwd, extra))
            || (tree() && !path_under_git(args, cwd));
    }
    if tool == "subagent" {
        return allowed.contains(&format!("subagent:{}", subagent_mode(args)));
    }
    if tool == "web_fetch" {
        let url = args.get("url").and_then(Value::as_str).unwrap_or("");
        let norm = normalize_url(url);
        return allowed.iter().filter_map(|r| r.strip_prefix("web_fetch:")).any(|p| {
            // A site grant must match the parsed URL, so `https://a.com/*`
            // never covers `https://a.com.evil/` or `https://a.com@evil/`.
            match &norm {
                Some(n) => crate::permissions::glob(p, n),
                None => p == url,
            }
        });
    }
    if crate::permissions::is_read_tool(tool) {
        return tree();
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
                truncated: false,
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
        let cwd = agent.cwd.clone();
        drop(agent.session.take());
        let (session, _, loaded) = crate::session::load(dir.path(), &id, &cwd).unwrap();
        assert_eq!(loaded, agent.items);
        agent.session = Some(session);
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

    #[test]
    fn sessions_save_and_restore_model_settings() {
        let dir = tempfile::tempdir().unwrap();
        let mut a = agent(Response::default(), None);
        a.session = Some(Session::create(dir.path(), dir.path(), "fake").unwrap());
        a.push(Item::user("hi"));
        let id = a.session.as_ref().unwrap().id.clone();
        let meta = crate::session::find(dir.path(), &id, dir.path()).unwrap();
        let saved = meta.prefs.clone().unwrap();
        assert_eq!((saved.provider.as_str(), saved.model.as_str(), saved.fast), ("fake", "fake", false));

        let prefs = crate::session::Prefs {
            provider: "ollama".into(),
            model: "llama3".into(),
            ..Default::default()
        };
        crate::commands::restore_prefs(dir.path(), &mut a, Some(&prefs));
        assert_eq!((a.provider.name(), a.provider.model()), ("ollama", "llama3"));
        a.sync_prefs();
        drop(a);
        let meta = crate::session::find(dir.path(), &id, dir.path()).unwrap();
        assert_eq!(meta.prefs, Some(prefs));
        // An unusable provider keeps the current model.
        let mut b = agent(Response::default(), None);
        let bad = crate::session::Prefs { provider: "nope".into(), model: "x".into(), ..Default::default() };
        crate::commands::restore_prefs(dir.path(), &mut b, Some(&bad));
        assert_eq!(b.provider.name(), "fake");
    }

    fn agent(response: Response, cancel: Option<CancellationToken>) -> Agent {
        Agent {
            provider: Box::new(SummaryProvider { response, cancel }),
            tools: Registry::default(),
            system: "system".into(),
            items: vec![Item::user("original")],
            session: None,
            usage_session_id: None,
            cwd: PathBuf::from("."),
            permission_mode: crate::permissions::Mode::Yolo,
            review_cache: Default::default(),
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

    struct NoticeCatch(Vec<String>);
    #[async_trait::async_trait]
    impl Ui for NoticeCatch {
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
                truncated: false,
                items: vec![Item::Assistant {
                    text: "summary".into(),
                }],
                usage: Some(Usage {
                    input_tokens: 100,
                    output_tokens: 10,
                    cached_tokens: 5,
                    ..Default::default()
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
        assert_eq!(
            agent.usage,
            Usage {
                input_tokens: 100,
                output_tokens: 10,
                cached_tokens: 5,
                ..Default::default()
            }
        );
        assert_eq!(agent.stats.requests, 1);
        assert_eq!(std::fs::read(path).unwrap(), saved);
    }

    #[tokio::test]
    async fn compaction_logs_usage_once_in_active_profile() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join("models.json"),
            r#"{"fake":{"models":{"fake":{"cost":{"input":2,"output":8,"cache_read":0.5}}}}}"#,
        )
        .unwrap();
        let usage = Usage {
            input_tokens: 100,
            output_tokens: 20,
            cached_tokens: 40,
            ..Default::default()
        };
        let mut agent = agent(
            Response {
                truncated: false,
                items: vec![Item::Assistant {
                    text: "summary".into(),
                }],
                usage: Some(usage),
            },
            None,
        );
        agent.permissions = std::sync::Arc::new(crate::permissions::Permissions {
            path: home.path().join("permissions.json"),
            ..Default::default()
        });
        agent.session =
            Some(Session::create(&home.path().join("sessions"), &agent.cwd, "fake").unwrap());
        assert!(agent
            .compact(&mut QuietUi, &CancellationToken::new())
            .await
            .unwrap());
        let log = std::fs::read_to_string(home.path().join("usage.jsonl")).unwrap();
        assert_eq!(log.lines().count(), 1);
        let r: crate::usage::Record = serde_json::from_str(log.trim()).unwrap();
        assert_eq!(r.session, agent.session.as_ref().unwrap().id);
        assert_eq!((r.input, r.output, r.cached), (100, 20, 40));
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
            ..Default::default()
        };
        let mut agent = agent(
            Response {
                truncated: false,
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
            let mut agent = agent(Response { items, usage: None, truncated: false }, None);
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
    fn bash_grants_reject_dynamic_extra_arguments() {
        let allowed = ["bash:git status".to_string()].into();
        for arg in ["$'--short'", "$'\\x2d\\x2dshort'", "$\"--short\"", "$", "$FLAGS", "${FLAGS}", "`echo --short`", "$(echo --short)"] {
            let cmd = format!("git status {arg}");
            assert!(!bash_matches("git status", &cmd), "{cmd}");
            assert!(!is_allowed(&allowed, "bash", &serde_json::json!({"command": cmd}), Path::new("/tmp")), "{cmd}");
            assert!(bash_prefix(&cmd).is_none(), "{cmd}");
        }
        assert!(bash_matches("git status", "git status --short"));
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
            Some("web_fetch:https://example.com/*")
        );
        assert_eq!(
            rule_label("web_fetch:https://example.com/*"),
            "`web_fetch` on https://example.com"
        );
        // A site grant covers the whole site, however the URL is spelled,
        // and nothing else.
        let site: HashSet<String> = ["web_fetch:https://example.com/*".into()].into();
        for url in ["https://example.com/a?x=1", "https://EXAMPLE.com", "https://example.com/b/c"] {
            assert!(is_allowed(&site, "web_fetch", &json!({ "url": url }), cwd), "{url}");
        }
        for url in [
            "https://example.com.evil.net/",
            "https://example.com@evil.net/",
            "http://example.com/",
            "https://example.com:8443/",
            "https://sub.example.com/",
        ] {
            assert!(!is_allowed(&site, "web_fetch", &json!({ "url": url }), cwd), "{url}");
        }
        // An exact-URL rule (as saved before) still matches only that URL.
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
                    truncated: false,
                    items: vec![Item::Assistant {
                        text: "summary".into(),
                    }],
                    usage: Some(Usage {
                        input_tokens: 1,
                        output_tokens: 1000,
                        cached_tokens: 0,
                        ..Default::default()
                    }),
                },
                Response {
                    truncated: false,
                    items: vec![Item::Assistant {
                        text: "answer".into(),
                    }],
                    usage: Some(Usage {
                        input_tokens: 900,
                        output_tokens: 50,
                        cached_tokens: 0,
                        ..Default::default()
                    }),
                },
                Response {
                    truncated: false,
                    items: vec![Item::Assistant {
                        text: "next".into(),
                    }],
                    usage: Some(Usage {
                        input_tokens: 900,
                        output_tokens: 10,
                        cached_tokens: 0,
                        ..Default::default()
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
                    truncated: false,
                    items: vec![Item::Assistant { text: "ok".into() }],
                    usage: Some(Usage {
                        input_tokens: 10,
                        output_tokens: 1,
                        cached_tokens: 0,
                        ..Default::default()
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
                        truncated: false,
                        items: vec![Item::ToolCall {
                            id: "1".into(),
                            name: "write_file".into(),
                            arguments: r#"{"path":"f.txt"}"#.into(),
                        }],
                        usage: Some(Usage {
                            input_tokens: 1,
                            output_tokens: 1,
                            cached_tokens: 0,
                            ..Default::default()
                        }),
                    },
                    Response {
                        truncated: false,
                        items: vec![Item::Assistant {
                            text: "done".into(),
                        }],
                        usage: Some(Usage {
                            input_tokens: 2,
                            output_tokens: 1,
                            cached_tokens: 0,
                            ..Default::default()
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
                truncated: false,
                items: vec![Item::ToolCall {
                    id: i.to_string(),
                    name: "mcp__wiki__ask".into(),
                    arguments: "{}".into(),
                }],
                usage: None,
            });
        }
        script.push(Response {
            truncated: false,
            items: vec![Item::Assistant {
                text: "done".into(),
            }],
            usage: None,
        });
        let mut agent = agent(Response::default(), None);
        agent.permission_mode = crate::permissions::Mode::Ask;
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
        agent.permission_mode = crate::permissions::Mode::Yolo;
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

    #[tokio::test]
    async fn auto_mode_follows_the_classifier() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        fn response(items: Vec<Item>) -> crate::types::Response {
            crate::types::Response { items, usage: None, truncated: false }
        }
        fn call() -> crate::types::Response {
            response(vec![Item::ToolCall {
                id: "1".into(),
                name: "mcp__wiki__ask".into(),
                arguments: "{}".into(),
            }])
        }
        fn decision(decision: &str, reason: &str) -> crate::types::Response {
            response(vec![Item::ToolCall {
                id: "perm".into(),
                name: crate::classifier::TOOL_NAME.into(),
                arguments: serde_json::json!({"decision": decision, "reason": reason}).to_string(),
            }])
        }
        fn done() -> crate::types::Response {
            response(vec![Item::Assistant {
                text: "done".into(),
            }])
        }
        fn scripted(
            runs: &std::sync::Arc<AtomicUsize>,
            script: Vec<crate::types::Response>,
        ) -> Agent {
            let mut agent = calling_agent(runs, 0);
            agent.permission_mode = crate::permissions::Mode::Auto;
            agent.provider = Box::new(ScriptedTurns {
                left: std::sync::Arc::new(std::sync::Mutex::new(script)),
            });
            agent
        }

        let runs = std::sync::Arc::new(AtomicUsize::new(0));
        let mut agent = scripted(
            &runs,
            vec![call(), decision("allow", "read only lookup"), done()],
        );
        let mut ui = ScriptUi {
            answer: Approval::Yes,
            asked: Vec::new(),
        };
        agent
            .turn(
                Item::user("Ship the widget to staging."),
                &mut ui,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(ui.asked.is_empty());
        assert_eq!(runs.load(Ordering::SeqCst), 1);

        let runs = std::sync::Arc::new(AtomicUsize::new(0));
        let mut agent = scripted(
            &runs,
            vec![call(), decision("deny", "touches production"), done()],
        );
        let mut ui = ScriptUi {
            answer: Approval::Yes,
            asked: Vec::new(),
        };
        agent
            .turn(
                Item::user("Ship the widget to staging."),
                &mut ui,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(ui.asked.is_empty());
        assert_eq!(runs.load(Ordering::SeqCst), 0);
        assert!(
            results(&agent)[0].contains("touches production"),
            "{:?}",
            results(&agent)
        );

        let runs = std::sync::Arc::new(AtomicUsize::new(0));
        let mut agent = scripted(
            &runs,
            vec![
                call(),
                response(vec![Item::Assistant {
                    text: "sure".into(),
                }]),
                done(),
            ],
        );
        let mut ui = ScriptUi {
            answer: Approval::Yes,
            asked: Vec::new(),
        };
        agent
            .turn(
                Item::user("Ship the widget to staging."),
                &mut ui,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(ui.asked.len(), 1);
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
                    truncated: false,
                    items: vec![Item::Assistant {
                        text: "answer".into(),
                    }],
                    usage: Some(Usage {
                        input_tokens: 10,
                        output_tokens: 1,
                        cached_tokens: 0,
                        ..Default::default()
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
                        truncated: false,
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
                        truncated: false,
                        items: vec![Item::Assistant {
                            text: "A pixel.".into(),
                        }],
                        usage: Some(Usage {
                            input_tokens: 10,
                            output_tokens: 1,
                            cached_tokens: 0,
                            ..Default::default()
                        }),
                    });
                }
                Ok(Response {
                    truncated: false,
                    items: vec![Item::ToolCall {
                        id: "1".into(),
                        name: "view_image".into(),
                        arguments: r#"{"path":"shot.png"}"#.into(),
                    }],
                    usage: Some(Usage {
                        input_tokens: 10,
                        output_tokens: 1,
                        cached_tokens: 0,
                        ..Default::default()
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

    #[test]
    fn always_outside_the_workspace_grants_the_directory_tree() {
        use serde_json::json;
        let work = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let cwd = work.path();
        let real = std::fs::canonicalize(data.path()).unwrap();
        let root = real.to_str().unwrap().replace('\\', "/");
        std::fs::create_dir_all(real.join("sub")).unwrap();
        let file = format!("{root}/notes.txt");
        let rule = grant_rule_in("read_file", &json!({ "path": file }), cwd, &[]).unwrap();
        assert_eq!(rule, format!("read_file:{root}/**"));
        assert!(rule_label(&rule).starts_with(&format!("reads under {root}")));
        let granted: HashSet<String> = [rule].into();
        let ok = |tool: &str, path: &str| is_allowed(&granted, tool, &json!({ "path": path }), cwd);
        assert!(ok("read_file", &format!("{root}/sub/deep.txt")));
        assert!(ok("list_dir", &root));
        assert!(ok("grep", &format!("{root}/sub")));
        assert!(!ok("read_file", "/etc/hosts"));
        assert!(!ok("write_file", &file), "a read grant never covers edits");

        // Edits outside the workspace: the file's directory tree, not .git.
        let rule = grant_rule_in("write_file", &json!({ "path": format!("{root}/sub/a.txt") }), cwd, &[]).unwrap();
        assert_eq!(rule, format!("write_file:{root}/sub/**"));
        let granted: HashSet<String> = [rule].into();
        let ok = |tool: &str, path: &str| is_allowed(&granted, tool, &json!({ "path": path }), cwd);
        assert!(ok("edit_file", &format!("{root}/sub/b.txt")));
        assert!(!ok("edit_file", &format!("{root}/c.txt")));
        assert!(!ok("write_file", &format!("{root}/sub/.git/config")));
        assert_eq!(
            grant_rule_in("write_file", &json!({ "path": format!("{root}/.git/x") }), cwd, &[]),
            None
        );
        // Inside the workspace a write grant stays the plain tool grant; the
        // filesystem root is never granted as a tree.
        assert_eq!(
            grant_rule_in("write_file", &json!({"path": "src/a.rs"}), cwd, &[]).as_deref(),
            Some("write_file")
        );
        #[cfg(unix)]
        assert_eq!(grant_rule_in("list_dir", &json!({"path": "/"}), cwd, &[]), None);
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
                    truncated: false,
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
        agent.stats.turns = 4;
        agent.stats.tools.insert("bash".into(), 2);
        let stats_id = agent.stats.id.clone();
        agent.clear_context().unwrap();
        assert_eq!(agent.session.as_ref().unwrap().id, id);
        assert_eq!(agent.stats.id, stats_id);
        assert_eq!(agent.stats.turns, 0);
        assert!(agent.stats.tools.is_empty());
        assert!(agent.items.is_empty());
        assert!(agent.allowed.is_empty());
        assert!(agent.undo.is_empty());
        let cwd = agent.cwd.clone();
        drop(agent.session.take());
        let (_, _, items) = crate::session::load(dir.path(), &id, &cwd).unwrap();
        assert!(items.is_empty());
    }

    /// A hooks configuration with one user hook, for the wiring tests.
    #[cfg(unix)]
    fn hooks_with(
        event: crate::hooks::Event,
        command: &str,
    ) -> std::sync::Arc<crate::hooks::Hooks> {
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
        let mut agent = calling_agent(
            &std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            0,
        );
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
    async fn a_truncated_reply_is_kept_and_announced() {
        let mut agent = agent(Response::default(), None);
        agent.context_window = 0;
        agent.items.clear();
        agent.provider = Box::new(ScriptedTurns {
            left: std::sync::Arc::new(std::sync::Mutex::new(vec![Response {
                truncated: true,
                items: vec![Item::Assistant {
                    text: "partial".into(),
                }],
                usage: None,
            }])),
        });
        let mut ui = NoticeCatch(Vec::new());
        agent
            .turn(Item::user("go"), &mut ui, CancellationToken::new())
            .await
            .unwrap();
        assert!(agent
            .items
            .iter()
            .any(|item| matches!(item, Item::Assistant { text } if text == "partial")));
        assert!(ui.0.iter().any(|m| m.contains("cut off")), "{:?}", ui.0);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // Serializes NYQ_HOME with other env-mutating tests.
    async fn memory_tool_refreshes_the_system_prompt() {
        let _lock = crate::provider::sse::tests::ENV.lock().unwrap_or_else(|e| e.into_inner());
        let home = tempfile::tempdir().unwrap();
        let old = std::env::var_os("NYQ_HOME");
        std::env::set_var("NYQ_HOME", home.path());
        let mut agent = agent(Response::default(), None);
        agent.context_window = 0;
        agent.items.clear();
        agent.system = "system".into();
        crate::tools::memory::register(&mut agent.tools);
        agent.provider = Box::new(ScriptedTurns {
            left: std::sync::Arc::new(std::sync::Mutex::new(vec![
                Response {
                    truncated: false,
                    items: vec![Item::ToolCall {
                        id: "1".into(),
                        name: "memory".into(),
                        arguments: r#"{"action":"add","fact":"the port is 7643"}"#.into(),
                    }],
                    usage: None,
                },
                Response {
                    truncated: false,
                    items: vec![Item::Assistant {
                        text: "done".into(),
                    }],
                    usage: None,
                },
            ])),
        });
        let turned = agent
            .turn(Item::user("go"), &mut QuietUi, CancellationToken::new())
            .await;
        if let Some(v) = old {
            std::env::set_var("NYQ_HOME", v);
        } else {
            std::env::remove_var("NYQ_HOME");
        }
        turned.unwrap();
        assert!(
            agent.system.contains("the port is 7643"),
            "{}",
            agent.system
        );
    }

    #[tokio::test]
    async fn usage_is_recorded_under_the_parent_session() {
        let home = tempfile::tempdir().unwrap();
        let mut agent = agent(Response::default(), None);
        agent.context_window = 0;
        agent.items.clear();
        agent.session = None;
        agent.usage_session_id = Some("parent-1".into());
        agent.permissions = std::sync::Arc::new(
            crate::permissions::Permissions::load(home.path(), home.path()).0,
        );
        let stats_id = agent.stats.id.clone();
        agent.provider = Box::new(ScriptedTurns {
            left: std::sync::Arc::new(std::sync::Mutex::new(vec![Response {
                truncated: false,
                items: vec![Item::Assistant {
                    text: "ok".into(),
                }],
                usage: Some(Usage {
                    input_tokens: 9,
                    output_tokens: 2,
                    cached_tokens: 0,
                    ..Default::default()
                }),
            }])),
        });
        agent
            .turn(Item::user("go"), &mut QuietUi, CancellationToken::new())
            .await
            .unwrap();
        let log = std::fs::read_to_string(home.path().join("usage.jsonl")).unwrap();
        assert!(log.contains("parent-1"), "{log}");
        assert!(!log.contains(&stats_id), "{log}");
        assert!(log.contains("\"input\":9"), "{log}");
    }

    #[tokio::test]
    async fn deferred_mcp_calls_honor_deny_and_stay_hidden() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct McpStub {
            name: String,
            runs: std::sync::Arc<AtomicUsize>,
        }
        #[async_trait::async_trait]
        impl crate::tools::Tool for McpStub {
            fn spec(&self) -> crate::types::ToolSpec {
                crate::types::ToolSpec {
                    name: self.name.clone(),
                    description: "d".into(),
                    parameters: serde_json::json!({}),
                }
            }
            fn needs_approval(&self, _: &serde_json::Value) -> bool {
                true
            }
            fn summary(&self, _: &serde_json::Value) -> String {
                String::new()
            }
            async fn run(&self, _: serde_json::Value, _: &ToolCtx) -> ToolOutput {
                self.runs.fetch_add(1, Ordering::SeqCst);
                ToolOutput::ok("ran")
            }
        }
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join("permissions.json"),
            r#"{"deny":["mcp_select_tool"]}"#,
        )
        .unwrap();
        let runs = std::sync::Arc::new(AtomicUsize::new(0));
        let mut agent = agent(Response::default(), None);
        agent.context_window = 0;
        agent.items.clear();
        agent.permission_mode = crate::permissions::Mode::Yolo;
        agent.permissions = std::sync::Arc::new(
            crate::permissions::Permissions::load(home.path(), home.path()).0,
        );
        for i in 0..41 {
            agent.tools.add(Box::new(McpStub {
                name: format!("mcp__s__t{i}"),
                runs: runs.clone(),
            }));
        }
        agent.provider = Box::new(ScriptedTurns {
            left: std::sync::Arc::new(std::sync::Mutex::new(vec![
                Response {
                    truncated: false,
                    items: vec![
                        Item::ToolCall {
                            id: "sel".into(),
                            name: "mcp_select_tool".into(),
                            arguments: r#"{"name":"mcp__s__t0"}"#.into(),
                        },
                        Item::ToolCall {
                            id: "call".into(),
                            name: "mcp__s__t0".into(),
                            arguments: "{}".into(),
                        },
                    ],
                    usage: None,
                },
                Response {
                    truncated: false,
                    items: vec![Item::Assistant {
                        text: "done".into(),
                    }],
                    usage: None,
                },
            ])),
        });
        agent
            .turn(Item::user("go"), &mut QuietUi, CancellationToken::new())
            .await
            .unwrap();
        let out = results(&agent);
        assert!(out[0].contains("blocked by permissions"), "{}", out[0]);
        assert!(out[1].contains("mcp_search_tools"), "{}", out[1]);
        assert_eq!(runs.load(Ordering::SeqCst), 0);
    }
    #[tokio::test]
    #[cfg(unix)]
    async fn prompt_hooks_add_context_or_block() {
        let mut agent = calling_agent(
            &std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            0,
        );
        agent.hooks = hooks_with(crate::hooks::Event::UserPromptSubmit, "echo branch: main");
        let live = CancellationToken::new();
        assert_eq!(
            crate::hooks::prompt_submit(&mut agent, "hi", &live).await,
            None
        );
        assert!(
            matches!(agent.items.last(), Some(Item::User { text, .. }) if text.ends_with("branch: main"))
        );
        agent.hooks = hooks_with(
            crate::hooks::Event::UserPromptSubmit,
            r#"echo '{"decision":"block","reason":"no"}'"#,
        );
        let n = agent.items.len();
        let msg = crate::hooks::prompt_submit(&mut agent, "hi", &live)
            .await
            .unwrap();
        assert!(msg.contains("no"), "{msg}");
        assert_eq!(agent.items.len(), n);
    }

    /// A `bash` stand-in that asks by default and records what it ran.
    struct BashStub(std::sync::Arc<std::sync::Mutex<Vec<String>>>);
    #[async_trait::async_trait]
    impl crate::tools::Tool for BashStub {
        fn spec(&self) -> crate::types::ToolSpec {
            crate::types::ToolSpec {
                name: "bash".into(),
                description: String::new(),
                parameters: serde_json::json!({}),
            }
        }
        fn needs_approval(&self, _: &Value) -> bool {
            true
        }
        fn summary(&self, args: &Value) -> String {
            args["command"].as_str().unwrap_or("").into()
        }
        async fn run(&self, args: Value, _: &ToolCtx) -> ToolOutput {
            let cmd = args["command"].as_str().unwrap_or("").to_string();
            self.0.lock().unwrap().push(cmd);
            ToolOutput::ok("ran")
        }
    }

    /// An agent whose model runs these shell commands, then answers.
    fn shell_agent(
        commands: &[&str],
        mode: crate::permissions::Mode,
    ) -> (Agent, std::sync::Arc<std::sync::Mutex<Vec<String>>>, tempfile::TempDir) {
        let ran = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let dir = tempfile::tempdir().unwrap();
        let mut script: Vec<Response> = commands
            .iter()
            .enumerate()
            .map(|(i, c)| Response {
                truncated: false,
                items: vec![Item::ToolCall {
                    id: i.to_string(),
                    name: "bash".into(),
                    arguments: serde_json::json!({ "command": c }).to_string(),
                }],
                usage: None,
            })
            .collect();
        script.push(Response {
            truncated: false,
            items: vec![Item::Assistant {
                text: "done".into(),
            }],
            usage: None,
        });
        let mut agent = agent(Response::default(), None);
        agent.cwd = dir.path().to_path_buf();
        agent.permission_mode = mode;
        agent.context_window = 0;
        agent.tools.add(Box::new(BashStub(ran.clone())));
        agent.provider = Box::new(ScriptedTurns {
            left: std::sync::Arc::new(std::sync::Mutex::new(script)),
        });
        (agent, ran, dir)
    }

    #[tokio::test]
    async fn read_only_shell_commands_run_without_asking() {
        use crate::permissions::Mode;
        let (mut agent, ran, _dir) = shell_agent(&["git status --short", "rm -rf x"], Mode::Ask);
        let mut ui = ScriptUi {
            answer: Approval::Yes,
            asked: Vec::new(),
        };
        agent
            .turn(Item::user("go"), &mut ui, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(ui.asked.len(), 1, "only rm asks");
        let ran = ran.lock().unwrap().clone();
        assert!(
            ran[0].starts_with("git --no-pager -c core.fsmonitor=false") && ran[0].ends_with("status --short"),
            "{ran:?}"
        );
        assert_eq!(ran[1], "rm -rf x");

        // Deny and ask rules still win over a read-only command.
        let (mut agent, ran, _dir) = shell_agent(&["git status", "cat a.txt"], Mode::Yolo);
        agent.permissions = std::sync::Arc::new(crate::permissions::Permissions {
            deny: vec!["bash:git".into()],
            ask: vec!["bash:cat".into()],
            ..Default::default()
        });
        let mut ui = ScriptUi {
            answer: Approval::No,
            asked: Vec::new(),
        };
        agent.permission_mode = Mode::Ask;
        agent
            .turn(Item::user("go"), &mut ui, CancellationToken::new())
            .await
            .unwrap();
        assert!(ran.lock().unwrap().is_empty());
        assert_eq!(ui.asked.len(), 1);
        assert!(results(&agent)[0].contains("blocked by permissions.json"));
    }

    #[tokio::test]
    async fn auto_mode_runs_known_dev_commands_without_the_classifier() {
        // The script has no classifier answer: consulting it would fall back
        // to asking.
        let (mut agent, ran, _dir) = shell_agent(
            &["cargo build && cargo test -p nyq 2>&1", "git fetch origin && npm ci"],
            crate::permissions::Mode::Auto,
        );
        let mut ui = ScriptUi {
            answer: Approval::No,
            asked: Vec::new(),
        };
        agent
            .turn(Item::user("Run the tests."), &mut ui, CancellationToken::new())
            .await
            .unwrap();
        assert!(ui.asked.is_empty());
        assert_eq!(
            *ran.lock().unwrap(),
            ["cargo build && cargo test -p nyq 2>&1", "git fetch origin && npm ci"]
        );
    }

    #[tokio::test]
    async fn approval_notes_reach_the_model() {
        struct NoteUi(Approval);
        #[async_trait::async_trait]
        impl Ui for NoteUi {
            fn stream(&mut self, _: &StreamEvent) {}
            fn end_message(&mut self) {}
            fn tool_start(&mut self, _: &str, _: &str) {}
            fn tool_end(&mut self, _: &str, _: &ToolOutput) {}
            async fn approve(&mut self, _: &ApprovalRequest<'_>) -> Approval {
                self.0
            }
            fn approval_note(&mut self) -> Option<String> {
                Some("use the staging wiki".into())
            }
        }
        for (answer, ran, expect) in [
            (Approval::No, 0, "The user denied this tool call and said: use the staging wiki"),
            (Approval::Yes, 1, "answer\n\n[The user approved this call with a note: use the staging wiki]"),
        ] {
            let runs = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let mut agent = calling_agent(&runs, 1);
            agent
                .turn(Item::user("go"), &mut NoteUi(answer), CancellationToken::new())
                .await
                .unwrap();
            assert_eq!(runs.load(std::sync::atomic::Ordering::SeqCst), ran);
            assert_eq!(results(&agent)[0], expect);
        }
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn stop_hook_can_continue_the_turn_once() {
        let done = |text: &str| Response {
            truncated: false,
            items: vec![Item::Assistant { text: text.into() }],
            usage: None,
        };
        let mut agent = agent(Response::default(), None);
        agent.context_window = 0;
        agent.provider = Box::new(ScriptedTurns {
            left: std::sync::Arc::new(std::sync::Mutex::new(vec![done("first"), done("second")])),
        });
        agent.hooks = hooks_with(
            crate::hooks::Event::Stop,
            r#"echo '{"decision":"block","reason":"run the tests first"}'"#,
        );
        // A third request would exhaust the script and fail the turn.
        agent
            .turn(Item::user("go"), &mut QuietUi, CancellationToken::new())
            .await
            .unwrap();
        assert!(agent.items.iter().any(
            |i| matches!(i, Item::User { text, .. } if text.contains("run the tests first"))
        ));
        assert!(matches!(agent.items.last(), Some(Item::Assistant { text }) if text == "second"));
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn pre_tool_use_hook_can_rewrite_the_input() {
        let (mut agent, ran, _dir) = shell_agent(&["rm -rf /"], crate::permissions::Mode::Ask);
        agent.hooks = hooks_with(
            crate::hooks::Event::PreToolUse,
            r#"echo '{"updatedInput":{"command":"echo safe"}}'"#,
        );
        let mut ui = ScriptUi {
            answer: Approval::No,
            asked: Vec::new(),
        };
        agent
            .turn(Item::user("go"), &mut ui, CancellationToken::new())
            .await
            .unwrap();
        // The rewritten command is read-only, so it runs without asking.
        assert!(ui.asked.is_empty());
        assert_eq!(*ran.lock().unwrap(), ["echo safe"]);
    }

    fn call(id: &str, name: &str, arguments: Value) -> Item {
        Item::ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: arguments.to_string(),
        }
    }

    fn reply(items: Vec<Item>) -> Response {
        Response {
            truncated: false,
            items,
            usage: None,
        }
    }

    fn scripted(script: Vec<Response>) -> Agent {
        let mut agent = agent(Response::default(), None);
        agent.context_window = 0;
        agent.provider = Box::new(ScriptedTurns {
            left: std::sync::Arc::new(std::sync::Mutex::new(script)),
        });
        agent
    }

    /// A tool under any name that sleeps, tracks how many runs overlap, and
    /// answers with its `n` argument (or fails when `fail` is set).
    struct Probe {
        name: &'static str,
        live: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        peak: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        fail: bool,
        delay_ms: u64,
    }

    #[async_trait::async_trait]
    impl crate::tools::Tool for Probe {
        fn spec(&self) -> crate::types::ToolSpec {
            crate::types::ToolSpec {
                name: self.name.into(),
                description: String::new(),
                parameters: serde_json::json!({}),
            }
        }
        fn summary(&self, _: &Value) -> String {
            String::new()
        }
        async fn run(&self, args: Value, _: &ToolCtx) -> ToolOutput {
            use std::sync::atomic::Ordering;
            let now = self.live.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(now, Ordering::SeqCst);
            tokio::time::sleep(std::time::Duration::from_millis(self.delay_ms)).await;
            self.live.fetch_sub(1, Ordering::SeqCst);
            if self.fail {
                return ToolOutput::err("nope");
            }
            ToolOutput::ok(format!("result {}", args["n"]))
        }
    }

    fn probe(name: &'static str, fail: bool) -> (Probe, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        let peak = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let tool = Probe {
            name,
            live: Default::default(),
            peak: peak.clone(),
            fail,
            delay_ms: if name == "grep" { 100 } else { 1 },
        };
        (tool, peak)
    }

    #[tokio::test]
    async fn consecutive_read_calls_run_together_in_order() {
        let (grep, peak) = probe("grep", false);
        let (other, _) = probe("mcp__x__write", false);
        let mut agent = scripted(vec![
            reply(vec![
                call("a", "grep", serde_json::json!({"n": 1})),
                call("b", "grep", serde_json::json!({"n": 2})),
                call("c", "grep", serde_json::json!({"n": 3})),
                call("d", "mcp__x__write", serde_json::json!({"n": 4})),
            ]),
            reply(vec![Item::Assistant { text: "done".into() }]),
        ]);
        agent.tools.add(Box::new(grep));
        agent.tools.add(Box::new(other));
        agent
            .turn(Item::user("go"), &mut QuietUi, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(peak.load(std::sync::atomic::Ordering::SeqCst), 3);
        assert_eq!(results(&agent), ["result 1", "result 2", "result 3", "result 4"]);
        assert!(runs_in_parallel("subagent", r#"{"task": "x"}"#));
        assert!(!runs_in_parallel("subagent", r#"{"action": "stop", "name": "x"}"#));
        assert!(!runs_in_parallel("bash", "{}"));
    }

    #[tokio::test]
    async fn a_call_that_keeps_failing_gets_a_nudge() {
        let (tool, _) = probe("mcp__x__go", true);
        let same = || reply(vec![call("a", "mcp__x__go", serde_json::json!({}))]);
        let mut agent = scripted(vec![same(), same(), same(), reply(vec![Item::Assistant { text: "ok".into() }])]);
        agent.tools.add(Box::new(tool));
        agent
            .turn(Item::user("go"), &mut QuietUi, CancellationToken::new())
            .await
            .unwrap();
        let results = results(&agent);
        assert!(!results[1].contains("failed"), "{}", results[1]);
        assert!(results[2].contains("has now failed 3 times"), "{}", results[2]);
    }

    #[tokio::test]
    async fn the_step_limit_ends_the_turn_with_a_notice() {
        let (tool, _) = probe("mcp__x__go", false);
        let script = (0..MAX_STEPS).map(|_| reply(vec![call("a", "mcp__x__go", serde_json::json!({}))])).collect();
        let mut agent = scripted(script);
        agent.tools.add(Box::new(tool));
        let mut ui = NoticeCatch(Vec::new());
        let end = agent.turn(Item::user("go"), &mut ui, CancellationToken::new()).await.unwrap();
        assert!(matches!(end, TurnEnd::Done));
        assert!(ui.0.iter().any(|n| n.contains("Stopped after")), "{:?}", ui.0);
    }

    #[tokio::test]
    async fn an_interrupted_reply_is_kept_with_a_marker() {
        struct Stalls;
        #[async_trait::async_trait]
        impl Provider for Stalls {
            fn name(&self) -> &str {
                "fake"
            }
            fn model(&self) -> &str {
                "fake"
            }
            fn set_model(&mut self, _: String) {}
            fn fork(&self) -> Box<dyn Provider> {
                Box::new(Stalls)
            }
            async fn stream(&self, _: Request<'_>, sink: EventSink<'_>) -> Result<Response> {
                sink(StreamEvent::Text("Half an ans".into()));
                std::future::pending().await
            }
        }
        let mut agent = scripted(Vec::new());
        agent.provider = Box::new(Stalls);
        let cancel = CancellationToken::new();
        let later = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            later.cancel();
        });
        let end = agent.turn(Item::user("go"), &mut QuietUi, cancel).await.unwrap();
        assert!(matches!(end, TurnEnd::Cancelled));
        assert_eq!(
            agent.items.last(),
            Some(&Item::Assistant {
                text: format!("Half an ans\n\n{INTERRUPTED}")
            })
        );
    }

    #[tokio::test]
    async fn an_edit_whose_file_changed_during_approval_is_refused() {
        struct Edit {
            content: std::sync::Arc<std::sync::Mutex<String>>,
            ran: std::sync::Arc<std::sync::atomic::AtomicBool>,
        }
        #[async_trait::async_trait]
        impl crate::tools::Tool for Edit {
            fn spec(&self) -> crate::types::ToolSpec {
                crate::types::ToolSpec {
                    name: "edit_file".into(),
                    description: String::new(),
                    parameters: serde_json::json!({}),
                }
            }
            fn needs_approval(&self, _: &Value) -> bool {
                true
            }
            fn summary(&self, _: &Value) -> String {
                String::new()
            }
            fn preview(&self, _: &Value, _: &ToolCtx) -> Option<String> {
                Some(self.content.lock().unwrap().clone())
            }
            async fn run(&self, _: Value, _: &ToolCtx) -> ToolOutput {
                self.ran.store(true, std::sync::atomic::Ordering::SeqCst);
                ToolOutput::ok("edited")
            }
        }
        struct Meddler(std::sync::Arc<std::sync::Mutex<String>>);
        #[async_trait::async_trait]
        impl Ui for Meddler {
            fn stream(&mut self, _: &StreamEvent) {}
            fn end_message(&mut self) {}
            fn tool_start(&mut self, _: &str, _: &str) {}
            fn tool_end(&mut self, _: &str, _: &ToolOutput) {}
            async fn approve(&mut self, _: &ApprovalRequest<'_>) -> Approval {
                *self.0.lock().unwrap() = "changed on disk".into();
                Approval::Yes
            }
        }
        let content = std::sync::Arc::new(std::sync::Mutex::new("old".to_string()));
        let ran = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut agent = scripted(vec![
            reply(vec![call("a", "edit_file", serde_json::json!({"path": "x"}))]),
            reply(vec![Item::Assistant { text: "ok".into() }]),
        ]);
        agent.permission_mode = crate::permissions::Mode::Ask;
        agent.tools.add(Box::new(Edit {
            content: content.clone(),
            ran: ran.clone(),
        }));
        agent
            .turn(Item::user("go"), &mut Meddler(content), CancellationToken::new())
            .await
            .unwrap();
        assert!(!ran.load(std::sync::atomic::Ordering::SeqCst));
        assert!(results(&agent)[0].contains("changed after the approval preview"));
    }

    #[tokio::test]
    async fn several_questions_with_described_options() {
        struct Answers(Vec<Option<String>>, Vec<Vec<String>>);
        #[async_trait::async_trait]
        impl Ui for Answers {
            fn stream(&mut self, _: &StreamEvent) {}
            fn end_message(&mut self) {}
            fn tool_start(&mut self, _: &str, _: &str) {}
            fn tool_end(&mut self, _: &str, _: &ToolOutput) {}
            async fn approve(&mut self, _: &ApprovalRequest<'_>) -> Approval {
                Approval::Yes
            }
            async fn ask(&mut self, _: &str, options: &[String]) -> Option<String> {
                self.1.push(options.to_vec());
                self.0.remove(0)
            }
        }
        let args = serde_json::json!({"questions": [
            {"question": "Which DB?", "options": [{"label": "SQLite", "description": "simplest, one file"}, {"label": "Postgres"}]},
            {"question": "Add tests?", "options": [{"label": "Yes"}, {"label": "No"}]}
        ]});
        let mut ui = Answers(vec![Some("SQLite — simplest, one file".into()), None], Vec::new());
        let out = ask_user(&args, &mut ui, &CancellationToken::new()).await;
        assert_eq!(out.content, "The user answered:\n- Which DB? → SQLite\n- Add tests? → (skipped)");
        assert_eq!(ui.1[0], ["SQLite — simplest, one file", "Postgres"]);
        let legacy = serde_json::json!({"question": "Go?", "options": ["yes", "no"]});
        let mut ui = Answers(vec![Some("typed answer".into())], Vec::new());
        let out = ask_user(&legacy, &mut ui, &CancellationToken::new()).await;
        assert_eq!(out.content, "The user answered: typed answer");
        assert!(ask_user(&serde_json::json!({}), &mut ui, &CancellationToken::new()).await.is_error);
    }

    #[tokio::test]
    async fn nested_instructions_arrive_once_with_the_first_touch() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("sub/deep")).unwrap();
        std::fs::write(dir.path().join("sub/AGENTS.md"), "Use tabs in sub.\n").unwrap();
        let (tool, _) = probe("read_file", false);
        let touch = |id: &str| reply(vec![call(id, "read_file", serde_json::json!({"path": "sub/deep/x.rs"}))]);
        let mut agent = scripted(vec![touch("a"), touch("b"), reply(vec![Item::Assistant { text: "ok".into() }])]);
        agent.cwd = dir.path().to_path_buf();
        agent.tools.add(Box::new(tool));
        agent
            .turn(Item::user("go"), &mut QuietUi, CancellationToken::new())
            .await
            .unwrap();
        let results = results(&agent);
        assert!(results[0].contains("[Instructions from sub/AGENTS.md, which apply to files under sub/"), "{}", results[0]);
        assert!(results[0].ends_with("Use tabs in sub."), "{}", results[0]);
        assert!(!results[1].contains("Instructions"), "{}", results[1]);
    }

    #[test]
    fn session_notes_describe_mode_and_question_ui() {
        let mut agent = agent(Response::default(), None);
        agent.permission_mode = crate::permissions::Mode::Ask;
        let notes = agent.session_notes();
        assert!(notes.contains("Permission mode: ask"), "{notes}");
        assert!(notes.contains("no question UI"), "{notes}");
        agent.tools.add(Box::new(AskUserTool));
        assert!(!agent.session_notes().contains("no question UI"));
    }

    #[test]
    fn a_text_only_model_gets_vision_and_parked_images() {
        struct TextOnly;
        #[async_trait::async_trait]
        impl Provider for TextOnly {
            fn name(&self) -> &str {
                "textonly-test"
            }
            fn model(&self) -> &str {
                "t1"
            }
            fn set_model(&mut self, _: String) {}
            fn fork(&self) -> Box<dyn Provider> {
                Box::new(TextOnly)
            }
            async fn stream(&self, _: Request<'_>, _: EventSink<'_>) -> Result<Response> {
                anyhow::bail!("unused")
            }
        }
        let _lock = crate::provider::sse::tests::ENV.lock().unwrap_or_else(|e| e.into_inner());
        let home = tempfile::tempdir().unwrap();
        let doc = serde_json::json!({"textonly-test": {"models": {"t1": {"modalities": {"input": ["text"], "output": ["text"]}}}}});
        std::fs::write(home.path().join("models.json"), doc.to_string()).unwrap();
        let old = std::env::var_os("NYQ_HOME");
        std::env::set_var("NYQ_HOME", home.path());
        let mut subject = agent(Response::default(), None);
        subject.tools = Registry::builtin();
        let names: Vec<String> = subject.advertised().into_iter().map(|s| s.name).collect();
        assert!(!names.contains(&"vision".to_string()) && names.contains(&"view_image".to_string()));

        let mut agent = subject;
        agent.provider = Box::new(TextOnly);
        let names: Vec<String> = agent.advertised().into_iter().map(|s| s.name).collect();
        assert!(names.contains(&"vision".to_string()) && !names.contains(&"view_image".to_string()));
        let png = crate::types::Image { media_type: "image/png".into(), data: "iVBORw0KGgo=".into() };
        agent.push_counted(Item::User { text: "look".into(), images: vec![png] });
        match old {
            Some(v) => std::env::set_var("NYQ_HOME", v),
            None => std::env::remove_var("NYQ_HOME"),
        }
        let Some(Item::User { text, images }) = agent.items.last() else { panic!() };
        assert!(images.is_empty() && text.starts_with("look\n\n[Image(s) attached: "), "{text}");
    }
}
