//! `subagent`: delegate a self-contained task to a fresh agent with its own
//! context; only its final report comes back to the parent model. With a
//! `name`, the subagent persists for the session (see `crate::subagents`):
//! later calls with that name send it follow-up messages.

use std::path::Path;
use std::time::Duration;

use serde_json::{json, Value};

use super::{Registry, Tool, ToolCtx, ToolOutput};
use std::sync::Arc;

use crate::agent::{Agent, Approval, ApprovalRequest, Approver, TurnEnd, Ui};
use crate::subagents::{self, LiveGuard, Record, Status};
use crate::types::{Item, StreamEvent, ToolSpec, Usage};

const NAME: &str = "subagent";
const TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// How long a timed-out subagent gets to kill its tools before we drop it.
const CLEANUP: Duration = Duration::from_secs(5);
const MAX_REPORT: usize = 20_000;
/// Tools an explore subagent may use (those present in the builtins).
const READ_ONLY: &[&str] = &[
    "read_file",
    "list_dir",
    "glob",
    "grep",
    "view_image",
    "web_fetch",
    "web_search",
];

const DESCRIPTION: &str = "\
Delegate a self-contained task to a subagent: a fresh agent with its own context that works \
on its own and returns only its final report. Use it for broad codebase searches, independent \
research, or investigations that would otherwise fill your context with file dumps; several \
subagent calls in one response run in parallel, so only batch independent tasks. The subagent \
sees NOTHING of this conversation, so `task` must contain complete instructions: the goal, \
relevant paths and names, constraints, and what to report back. Modes: \"explore\" (default) gets \
read-only tools (read, list, glob, grep, view images, skills) and needs no approval; \"general\" \
may also edit files, run commands and use MCP tools. `model`, `effort` and `permission_mode` \
override yours for this subagent (the permission mode can only be stricter). \
Without `name` the subagent is one-shot. Give a `name` to keep it for this session: the first \
call creates it, and later calls with the same name send `task` to it as a follow-up message, \
with everything it already learned still in its context. `action` manages named subagents: \
\"list\", \"inspect\" (status and recent transcript), \"stop\", and \"resume\" (reopen a stopped \
one, or finish an interrupted message). \
Don't use it for a quick lookup you can do with one or two direct tool calls.";

const PREAMBLE: &str = "\
You are a subagent: another agent delegated the task below to you. You have none of its \
conversation, only this task. Work on it autonomously with your tools; you cannot ask the user \
questions. When done, finish with a concise report of your findings, including file paths and \
line numbers where relevant. The report is all the delegating agent will see.";

const PERSISTENT: &str = "\
The delegating agent may send you follow-up messages later; keep what you learn in mind and \
answer each message with a self-contained report.";

pub fn register(r: &mut Registry) {
    r.add(Box::new(Subagent));
}

struct Subagent;

fn str_arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty())
}

fn is_general(args: &Value) -> bool {
    args.get("mode").and_then(Value::as_str) == Some("general")
}

fn action(args: &Value) -> &str {
    str_arg(args, "action").unwrap_or("run")
}

/// The short label: `description`, else the task's first line.
fn label(args: &Value) -> &str {
    args.get("description")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            args.get("task")
                .and_then(Value::as_str)
                .and_then(|t| t.lines().next())
        })
        .unwrap_or("")
        .trim()
}

/// The child's tools: builtins (minus mutating ones for explore), never
/// itself, plus the session's skills and (for general) MCP tools.
fn child_tools(general: bool, shared: &[Arc<dyn Tool>]) -> Registry {
    let mut tools = Registry::builtin();
    let names: Vec<String> = tools.specs().into_iter().map(|s| s.name).collect();
    let keep: Vec<&str> = if general {
        names
            .iter()
            .map(String::as_str)
            .filter(|n| *n != NAME)
            .collect()
    } else {
        READ_ONLY
            .iter()
            .copied()
            .filter(|n| names.iter().any(|m| m == n))
            .collect()
    };
    for name in &names {
        // remove_prefix is the only removal API; don't let it take a kept tool.
        if !keep.contains(&name.as_str()) && !keep.iter().any(|k| k.starts_with(name.as_str())) {
            tools.remove_prefix(name);
        }
    }
    for tool in shared {
        let name = tool.spec().name;
        if general || READ_ONLY_SHARED.contains(&name.as_str()) {
            let _ = tools.try_insert_shared(tool.clone());
        }
    }
    tools
}

/// Inherited session tools an explore subagent may use.
const READ_ONLY_SHARED: &[&str] = &["skill", "mcp_list_resources", "mcp_read_resource"];

/// One approval prompt at a time: parallel subagents may ask together.
static APPROVALS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// How permissive a mode is; a subagent can't exceed its parent.
fn mode_rank(mode: crate::permissions::Mode) -> u8 {
    match mode {
        crate::permissions::Mode::Ask => 0,
        crate::permissions::Mode::Auto => 1,
        crate::permissions::Mode::Yolo => 2,
    }
}

#[async_trait::async_trait]
impl Tool for Subagent {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: NAME.into(),
            description: DESCRIPTION.into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "task": {"type": "string", "description": "Complete, self-contained instructions (or, for an existing named subagent, the follow-up message). The subagent sees nothing else."},
                    "description": {"type": "string", "description": "Short 3-6 word label for the task, shown to the user."},
                    "mode": {"type": "string", "enum": ["explore", "general"], "description": "explore (default): read-only tools. general: may also edit files and run commands. Fixed when a named subagent is created."},
                    "name": {"type": "string", "description": "Keep the subagent under this name (letters, digits, - and _) so later calls can send it follow-ups."},
                    "action": {"type": "string", "enum": ["run", "list", "inspect", "stop", "resume"], "description": "run (default): run `task`. list: named subagents. inspect/stop/resume: act on `name`."},
                    "model": {"type": "string", "description": "Model for this subagent (default: yours), e.g. a faster one for simple searches."},
                    "effort": {"type": "string", "description": "Reasoning effort for this subagent (e.g. low, medium, high)."},
                    "permission_mode": {"type": "string", "enum": ["ask", "auto", "yolo"], "description": "Permission mode for this subagent; never more permissive than yours."}
                }
            }),
        }
    }

    fn needs_approval(&self, args: &Value) -> bool {
        is_general(args) && action(args) == "run"
    }

    fn summary(&self, args: &Value) -> String {
        let label = label(args);
        let name = str_arg(args, "name");
        match (action(args), name) {
            ("run", None) if is_general(args) => format!("{label} (general)"),
            ("run", None) => label.to_string(),
            ("run", Some(n)) if is_general(args) => format!("{n}: {label} (general)"),
            ("run", Some(n)) => format!("{n}: {label}"),
            (a, Some(n)) => format!("{a} {n}"),
            (a, None) => a.to_string(),
        }
    }

    fn handles_cancel(&self) -> bool {
        true
    }

    fn preview(&self, args: &Value, _: &ToolCtx) -> Option<String> {
        let task = args.get("task").and_then(Value::as_str).filter(|t| !t.trim().is_empty())?;
        let mode = if is_general(args) { "general" } else { "explore" };
        let name = str_arg(args, "name").map(|n| format!(" · name: {n}")).unwrap_or_default();
        Some(format!("mode: {mode}{name}\n{task}"))
    }

    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolOutput {
        let name = str_arg(&args, "name");
        let dir = ctx.subagent_dir.as_deref();
        match (action(&args), name, dir) {
            ("run", None, _) => one_shot(&args, ctx).await,
            ("list", _, Some(dir)) => ToolOutput::ok(subagents::list_lines(dir).join("\n")),
            ("list", _, None) | (_, Some(_), None) => {
                ToolOutput::err("Named subagents are unavailable here: they are kept with a saved session.")
            }
            (_, None, _) => ToolOutput::err("This action needs the subagent's `name`."),
            (_, Some(n), _) if !subagents::valid_name(n) => ToolOutput::err(format!(
                "Invalid subagent name `{n}`: use letters, digits, '-' and '_' (at most {} characters).",
                subagents::MAX_NAME
            )),
            ("run", Some(n), Some(dir)) => named_run(&args, n, dir, ctx).await,
            ("inspect", Some(n), Some(dir)) => match subagents::show_lines(dir, n, 20) {
                Ok(lines) => ToolOutput::ok(lines.join("\n")),
                Err(e) => ToolOutput::err(format!("{e:#}")),
            },
            ("stop", Some(n), Some(dir)) => match subagents::stop(dir, n) {
                Ok(msg) => ToolOutput::ok(msg),
                Err(e) => ToolOutput::err(format!("{e:#}")),
            },
            ("resume", Some(n), Some(dir)) => resume(n, dir, ctx).await,
            (other, _, _) => ToolOutput::err(format!("Unknown action `{other}` (run, list, inspect, stop, resume).")),
        }
    }
}

/// A child agent with `items` as its transcript. `args` may override the
/// model, effort and (only towards stricter) permission mode.
fn child(ctx: &ToolCtx, general: bool, persistent: bool, items: Vec<Item>, args: &Value) -> Result<Agent, ToolOutput> {
    let Some(provider) = &ctx.provider else {
        return Err(ToolOutput::err("Subagents are unavailable: no model provider in this context."));
    };
    let mut provider = provider.fork();
    if let Some(model) = str_arg(args, "model") {
        provider.set_model(model.to_string());
    }
    if let Some(effort) = str_arg(args, "effort") {
        provider.set_effort(Some(effort.to_string()));
    }
    let parent_mode = if ctx.yolo { crate::permissions::Mode::Yolo } else { ctx.permission_mode };
    let permission_mode = match str_arg(args, "permission_mode") {
        None => parent_mode,
        Some(raw) => match crate::permissions::Mode::parse(raw) {
            Some(mode) if mode_rank(mode) <= mode_rank(parent_mode) => mode,
            Some(_) => parent_mode,
            None => return Err(ToolOutput::err(format!("Unknown permission_mode `{raw}` (ask, auto, yolo)."))),
        },
    };
    let context_window = crate::config::context_window_for(provider.name(), None);
    let mut system = format!("{}\n\n{PREAMBLE}", crate::prompt::system(&ctx.cwd, &ctx.sandbox));
    if persistent {
        system.push_str(&format!("\n\n{PERSISTENT}"));
    }
    Ok(Agent {
        provider,
        tools: child_tools(general, &ctx.shared_tools),
        system,
        items,
        session: None,
        usage_session_id: ctx.session_id.clone(),
        cwd: ctx.cwd.clone(),
        // Inherit the parent's mode (or a stricter one), "always" grants
        // and rules. Calls that still need approval are forwarded to the
        // parent's approver, or denied when there is none: approving this
        // call does not auto-approve them. A yolo flag set on the context
        // wins so older callers that only set `yolo` still skip prompts.
        permission_mode,
        review_cache: std::sync::Arc::clone(&ctx.review_cache),
        allowed: ctx.allowed.clone(),
        permissions: ctx.permissions.clone(),
        usage: Usage::default(),
        stats: Default::default(),
        context_window,
        context_tokens: 0,
        context_estimated: false,
        suppress_auto_compact: false,
        submitted: 0,
        // The parent's tool hooks also vet the subagent's calls.
        hooks: std::sync::Arc::new(ctx.hooks.for_subagent()),
        sandbox: ctx.sandbox.clone(),
        undo: Default::default(),
    })
}

/// How a child's turn went.
struct Outcome {
    result: anyhow::Result<TurnEnd>,
    calls: Vec<(String, String)>,
    undo: crate::undo::UndoLog,
    /// Cancelled by a `stop` (not by the parent or a timeout).
    stopped: bool,
}

/// Run one turn of `agent` (a new message, or a continuation when `input` is
/// None), with a live progress line and approvals forwarded to the parent.
async fn drive_child(
    agent: &mut Agent,
    input: Option<Item>,
    ctx: &ToolCtx,
    origin: String,
    live: LiveGuard,
    cancel: tokio_util::sync::CancellationToken,
) -> Outcome {
    let mut ui = Collector { calls: Vec::new(), approver: ctx.approver.clone(), origin, live };
    let result = match input {
        Some(item) => drive_turn(agent.turn(item, &mut ui, cancel.clone()), TIMEOUT, CLEANUP, cancel).await,
        None => drive_turn(agent.continue_turn(&mut ui, cancel.clone()), TIMEOUT, CLEANUP, cancel).await,
    };
    ctx.granted
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .extend(agent.allowed.difference(&ctx.allowed).cloned());
    Outcome { result, calls: std::mem::take(&mut ui.calls), undo: std::mem::take(&mut agent.undo), stopped: ui.live.stopped() }
}

fn display(calls: &[(String, String)]) -> String {
    if calls.is_empty() {
        return "(no tool calls)".into();
    }
    calls.iter().map(|(name, summary)| format!("{name} {summary}").trim_end().to_string()).collect::<Vec<_>>().join("\n")
}

/// The tool output for a finished turn: the report or the failure.
fn report(outcome: Outcome, agent: &Agent, footer: String) -> ToolOutput {
    let display = Some(display(&outcome.calls));
    let undo = outcome.undo;
    let fail = |msg: String| ToolOutput { display: display.clone(), undo: undo.clone(), ..ToolOutput::err(msg) };
    match outcome.result {
        Err(e) if e.to_string().contains("timed out") => {
            return fail(format!("Subagent timed out after {} minutes{footer}.", TIMEOUT.as_secs() / 60));
        }
        Err(e) => return fail(format!("Subagent failed: {e:#}{footer}")),
        Ok(TurnEnd::Cancelled) => return fail(format!("Subagent cancelled{footer}.")),
        Ok(TurnEnd::Done) => {}
    }
    let report = agent
        .items
        .iter()
        .rev()
        .find_map(|i| match i {
            Item::Assistant { text } if !text.trim().is_empty() => Some(text.trim()),
            _ => None,
        })
        .unwrap_or("(the subagent produced no report)");
    let mut content = truncate(report, MAX_REPORT);
    content.push_str(&format!(
        "\n\n(subagent used {} tool calls, ↑{} ↓{} tokens{footer})",
        outcome.calls.len(),
        agent.usage.input_tokens,
        agent.usage.output_tokens
    ));
    ToolOutput { display, undo, ..ToolOutput::ok(content) }
}

async fn one_shot(args: &Value, ctx: &ToolCtx) -> ToolOutput {
    let Some(task) = args.get("task").and_then(Value::as_str).filter(|t| !t.trim().is_empty()) else {
        return ToolOutput::err("Missing required argument `task`.");
    };
    let mut agent = match child(ctx, is_general(args), false, Vec::new(), args) {
        Ok(a) => a,
        Err(e) => return e,
    };
    let label = one_line(label(args));
    let cancel = ctx.cancel.child_token();
    let Some(live) = LiveGuard::start(None, &label, cancel.clone()) else {
        return ToolOutput::err("Subagent could not start.");
    };
    let outcome = drive_child(&mut agent, Some(Item::user(task)), ctx, format!("subagent: {label}"), live, cancel).await;
    report(outcome, &agent, String::new())
}

/// Create the named subagent `name`, or send it a follow-up message.
async fn named_run(args: &Value, name: &str, dir: &Path, ctx: &ToolCtx) -> ToolOutput {
    let Some(task) = args.get("task").and_then(Value::as_str).filter(|t| !t.trim().is_empty()) else {
        return ToolOutput::err("Missing required argument `task` (the message for the subagent).");
    };
    let record = match subagents::load(dir, name) {
        Ok(Some(r)) if r.status == Status::Stopped => {
            return ToolOutput::err(format!(
                "Subagent `{name}` is stopped; call subagent with action \"resume\" and name \"{name}\" to reopen it."
            ));
        }
        Ok(Some(r)) => r,
        Ok(None) => Record::new(name, if is_general(args) { "general" } else { "explore" }, label(args)),
        Err(e) => return ToolOutput::err(format!("{e:#}")),
    };
    run_named(record, Some(Item::user(task)), dir, ctx, args).await
}

/// `resume`: reopen a stopped subagent, and finish a message it was
/// interrupted in.
async fn resume(name: &str, dir: &Path, ctx: &ToolCtx) -> ToolOutput {
    let mut record = match subagents::load(dir, name) {
        Ok(Some(r)) => r,
        Ok(None) => return ToolOutput::err(format!("No subagent named `{name}` in this session.")),
        Err(e) => return ToolOutput::err(format!("{e:#}")),
    };
    if subagents::is_live(&subagents::key(dir, name)) {
        return ToolOutput::err(format!("Subagent `{name}` is already working."));
    }
    let unfinished = record.status != Status::Idle && !matches!(record.items.last(), Some(Item::Assistant { .. }) | None);
    if !unfinished {
        if record.status != Status::Idle {
            record.status = Status::Idle;
            record.updated = chrono::Utc::now();
            if let Err(e) = subagents::save(dir, &record) {
                return ToolOutput::err(format!("{e:#}"));
            }
        }
        let last = record.last_answer().map(|a| format!("\nIts last answer:\n{}", truncate(a, 2_000))).unwrap_or_default();
        return ToolOutput::ok(format!(
            "Subagent `{name}` is ready for follow-up messages (call subagent with name \"{name}\" and a task).{last}"
        ));
    }
    run_named(record, None, dir, ctx, &Value::Null).await
}

/// Run a turn of a saved subagent and save it again.
async fn run_named(mut record: Record, input: Option<Item>, dir: &Path, ctx: &ToolCtx, args: &Value) -> ToolOutput {
    let name = record.name.clone();
    let cancel = ctx.cancel.child_token();
    let live_label = if record.description.is_empty() { name.clone() } else { format!("{name}: {}", one_line(&record.description)) };
    let Some(live) = LiveGuard::start(Some(subagents::key(dir, &name)), &live_label, cancel.clone()) else {
        return ToolOutput::err(format!("Subagent `{name}` is still working on an earlier message; wait for it or stop it."));
    };
    // A turn cut short earlier may have left calls without results.
    let mut items = std::mem::take(&mut record.items);
    let repairs = crate::session::repairs(&items);
    items.extend(repairs);
    let mut agent = match child(ctx, record.mode == "general", true, items, args) {
        Ok(a) => a,
        Err(e) => return e,
    };
    // Saved as running with the new message, so a crash leaves it resumable.
    record.items = agent.items.clone();
    record.items.extend(input.clone());
    record.status = Status::Running;
    record.updated = chrono::Utc::now();
    if let Err(e) = subagents::save(dir, &record) {
        return ToolOutput::err(format!("{e:#}"));
    }
    let outcome = drive_child(&mut agent, input, ctx, format!("subagent {name}"), live, cancel).await;
    record.status = match &outcome.result {
        Ok(TurnEnd::Done) => Status::Idle,
        _ if outcome.stopped => Status::Stopped,
        _ => Status::Interrupted,
    };
    record.items = agent.items.clone();
    record.turns += 1;
    record.tool_calls += outcome.calls.len() as u64;
    record.usage.input_tokens += agent.usage.input_tokens;
    record.usage.output_tokens += agent.usage.output_tokens;
    record.usage.cached_tokens += agent.usage.cached_tokens;
    record.updated = chrono::Utc::now();
    let saved = subagents::save(dir, &record);
    let footer = match record.status {
        Status::Idle => format!("; `{name}` keeps its context: send follow-ups with name \"{name}\""),
        Status::Stopped => format!("; `{name}` was stopped"),
        _ => format!("; `{name}` can finish this with action \"resume\""),
    };
    let mut out = report(outcome, &agent, footer);
    if let Err(e) = saved {
        out.content.push_str(&format!("\n(could not save subagent `{name}`: {e:#})"));
    }
    out
}

async fn drive_turn(
    turn: impl std::future::Future<Output = anyhow::Result<TurnEnd>>,
    timeout: Duration,
    cleanup: Duration,
    cancel: tokio_util::sync::CancellationToken,
) -> anyhow::Result<TurnEnd> {
    tokio::pin!(turn);
    tokio::select! {
        r = &mut turn => r,
        _ = tokio::time::sleep(timeout) => {
            cancel.cancel();
            match tokio::time::timeout(cleanup, turn).await {
                Ok(Ok(TurnEnd::Cancelled)) | Err(_) => anyhow::bail!("timed out"),
                Ok(other) => other,
            }
        }
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}\n[… report truncated, {} more bytes]",
        &s[..end],
        s.len() - end
    )
}

fn one_line(s: &str) -> String {
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.chars().count() > 60 {
        format!("{}…", s.chars().take(59).collect::<String>())
    } else {
        s
    }
}

/// Headless UI for the child agent: records tool calls and updates its live
/// line. Approvals go to the parent's approver (the user) when there is one;
/// otherwise they are denied.
struct Collector {
    calls: Vec<(String, String)>,
    approver: Option<Arc<dyn Approver>>,
    /// Who is asking, e.g. "subagent: Count files".
    origin: String,
    live: LiveGuard,
}

#[async_trait::async_trait]
impl Ui for Collector {
    fn stream(&mut self, _: &StreamEvent) {}
    fn end_message(&mut self) {}
    fn tool_start(&mut self, tool: &str, summary: &str) {
        self.live.tool(tool, &one_line(summary));
        self.calls.push((tool.to_string(), summary.to_string()));
    }
    fn tool_end(&mut self, _: &str, _: &ToolOutput) {}
    async fn approve(&mut self, req: &ApprovalRequest<'_>) -> Approval {
        match &self.approver {
            Some(approver) => {
                let _turn = APPROVALS.lock().await;
                approver.approve(&self.origin, req).await
            }
            None => Approval::No,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{EventSink, Provider};
    use crate::types::{Request, Response};
    use std::sync::{Arc, Mutex};

    /// Scripted responses shared across forks; records each request's tools.
    #[derive(Clone, Default)]
    struct Fake {
        script: Arc<Mutex<Vec<Response>>>,
        seen_tools: Arc<Mutex<Vec<Vec<String>>>>,
        /// Tool results the child model was shown.
        seen_results: Arc<Mutex<Vec<String>>>,
        /// Every request's transcript.
        requests: Arc<Mutex<Vec<Vec<Item>>>>,
        hang: bool,
    }

    #[async_trait::async_trait]
    impl Provider for Fake {
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
        async fn stream(&self, req: Request<'_>, _: EventSink<'_>) -> anyhow::Result<Response> {
            self.seen_tools.lock().unwrap().push(req.tools.iter().map(|t| t.name.clone()).collect());
            self.requests.lock().unwrap().push(req.items.to_vec());
            *self.seen_results.lock().unwrap() = req
                .items
                .iter()
                .filter_map(|i| match i {
                    Item::ToolResult { output, .. } => Some(output.clone()),
                    _ => None,
                })
                .collect();
            if self.hang {
                std::future::pending::<()>().await;
            }
            let mut script = self.script.lock().unwrap();
            anyhow::ensure!(!script.is_empty(), "script exhausted");
            Ok(script.remove(0))
        }
    }

    fn resp(items: Vec<Item>) -> Response {
        Response {
            truncated: false,
            items,
            usage: Some(Usage {
                input_tokens: 100,
                output_tokens: 10,
                cached_tokens: 0,
            }),
        }
    }

    fn ctx(fake: &Fake, dir: &std::path::Path) -> ToolCtx {
        ToolCtx {
            cwd: dir.to_path_buf(),
            provider: Some(Arc::new(fake.clone())),
            ..ToolCtx::default()
        }
    }

    #[tokio::test]
    async fn subagent_runs_tools_and_returns_report() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello").unwrap();
        let fake = Fake::default();
        *fake.script.lock().unwrap() = vec![
            resp(vec![Item::ToolCall {
                id: "1".into(),
                name: "read_file".into(),
                arguments: r#"{"path":"a.txt"}"#.into(),
            }]),
            resp(vec![Item::Assistant {
                text: "Found hello in a.txt:1".into(),
            }]),
        ];
        let out = Subagent
            .run(
                json!({"task": "look at a.txt", "description": "read a"}),
                &ctx(&fake, dir.path()),
            )
            .await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.starts_with("Found hello in a.txt:1"));
        assert!(
            out.content
                .contains("(subagent used 1 tool calls, ↑200 ↓20 tokens)"),
            "{}",
            out.content
        );
        assert!(out.display.unwrap().contains("read_file a.txt"));

        let seen = fake.seen_tools.lock().unwrap();
        let tools = &seen[0];
        assert!(tools.contains(&"read_file".to_string()) && tools.contains(&"grep".to_string()));
        for banned in ["write_file", "edit_file", "bash", "subagent"] {
            assert!(
                !tools.contains(&banned.to_string()),
                "explore offered {banned}"
            );
        }
    }

    /// A stand-in session tool (skill or MCP) under `name`.
    struct Named(&'static str);
    #[async_trait::async_trait]
    impl Tool for Named {
        fn spec(&self) -> ToolSpec {
            ToolSpec { name: self.0.into(), description: String::new(), parameters: json!({}) }
        }
        fn summary(&self, _: &Value) -> String {
            String::new()
        }
        async fn run(&self, _: Value, _: &ToolCtx) -> ToolOutput {
            ToolOutput::ok("")
        }
    }

    #[test]
    fn children_inherit_skills_mcp_and_at_most_the_parent_mode() {
        let shared: Vec<Arc<dyn Tool>> = vec![Arc::new(Named("skill")), Arc::new(Named("mcp__wiki__ask"))];
        let names = |general| -> Vec<String> { child_tools(general, &shared).specs().into_iter().map(|s| s.name).collect() };
        assert!(names(false).contains(&"skill".into()) && !names(false).contains(&"mcp__wiki__ask".into()));
        assert!(names(true).contains(&"skill".into()) && names(true).contains(&"mcp__wiki__ask".into()));

        let dir = tempfile::tempdir().unwrap();
        let mut parent = ctx(&Fake::default(), dir.path());
        parent.permission_mode = crate::permissions::Mode::Auto;
        let mode = |args: Value| child(&parent, false, false, Vec::new(), &args).map(|a| a.permission_mode);
        assert_eq!(mode(json!({})).ok(), Some(crate::permissions::Mode::Auto));
        assert_eq!(mode(json!({"permission_mode": "ask"})).ok(), Some(crate::permissions::Mode::Ask));
        assert_eq!(mode(json!({"permission_mode": "yolo"})).ok(), Some(crate::permissions::Mode::Auto));
        assert!(mode(json!({"permission_mode": "bogus"})).is_err());
    }

    #[test]
    fn general_mode_needs_approval_and_has_write_tools() {
        assert!(!Subagent.needs_approval(&json!({"task": "x"})));
        assert!(Subagent.needs_approval(&json!({"task": "x", "mode": "general"})));
        let names: Vec<String> = child_tools(true, &[])
            .specs()
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert!(names.contains(&"bash".to_string()) && names.contains(&"write_file".to_string()));
        assert!(!names.contains(&"subagent".to_string()));
        assert_eq!(Subagent.summary(&json!({"task": "first\nsecond"})), "first");
        let preview = Subagent
            .preview(
                &json!({"task": "rm -rf /", "description": "check the test names", "mode": "general"}),
                &ToolCtx::default(),
            )
            .unwrap();
        assert!(
            preview.contains("rm -rf /") && preview.contains("mode: general"),
            "{preview}"
        );
        assert_eq!(
            Subagent.summary(&json!({"task": "rm -rf /", "description": "check the test names", "mode": "general"})),
            "check the test names (general)"
        );
    }

    #[tokio::test]
    async fn general_subagent_does_not_auto_approve_writes() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::default();
        *fake.script.lock().unwrap() = vec![
            resp(vec![Item::ToolCall {
                id: "1".into(),
                name: "write_file".into(),
                arguments: r#"{"path":"out.txt","content":"x"}"#.into(),
            }]),
            resp(vec![Item::Assistant {
                text: "done".into(),
            }]),
        ];
        let out = Subagent
            .run(
                json!({"task": "write out.txt", "mode": "general"}),
                &ctx(&fake, dir.path()),
            )
            .await;
        assert!(!out.is_error, "{}", out.content);
        assert!(!dir.path().join("out.txt").exists());
        assert!(out.content.starts_with("done"), "{}", out.content);
    }

    #[tokio::test]
    async fn general_subagent_inherits_yolo_and_grants() {
        let dir = tempfile::tempdir().unwrap();
        let write = || {
            resp(vec![Item::ToolCall {
                id: "1".into(),
                name: "write_file".into(),
                arguments: r#"{"path":"out.txt","content":"x"}"#.into(),
            }])
        };
        let done = || {
            resp(vec![Item::Assistant {
                text: "wrote".into(),
            }])
        };

        let fake = Fake::default();
        *fake.script.lock().unwrap() = vec![write(), done()];
        let mut c = ctx(&fake, dir.path());
        c.yolo = true;
        let out = Subagent
            .run(json!({"task": "write", "mode": "general"}), &c)
            .await;
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("out.txt")).unwrap(),
            "x"
        );
        let mut parent_undo = crate::undo::UndoLog::default();
        parent_undo.append(out.undo);
        assert_eq!(parent_undo.len(), 1);
        parent_undo.undo().unwrap();
        assert!(!dir.path().join("out.txt").exists());

        let fake = Fake::default();
        *fake.script.lock().unwrap() = vec![write(), done()];
        let mut c = ctx(&fake, dir.path());
        c.allowed.insert("write_file".into());
        let out = Subagent
            .run(json!({"task": "write", "mode": "general"}), &c)
            .await;
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("out.txt")).unwrap(),
            "x"
        );
    }

    /// Answers every request with `answer` and records what it was asked.
    struct FakeApprover {
        answer: Approval,
        asked: Mutex<Vec<(String, String, Option<String>)>>,
    }

    #[async_trait::async_trait]
    impl Approver for FakeApprover {
        async fn approve(&self, origin: &str, req: &ApprovalRequest<'_>) -> Approval {
            self.asked.lock().unwrap().push((
                origin.to_string(),
                req.summary.to_string(),
                req.save_rule.clone(),
            ));
            self.answer
        }
    }

    /// Run a general subagent whose child calls `bash touch ran.txt`.
    async fn run_touch(
        answer: Approval,
        permissions: crate::permissions::Permissions,
    ) -> (
        tempfile::TempDir,
        ToolOutput,
        Arc<FakeApprover>,
        Fake,
        Vec<String>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::default();
        *fake.script.lock().unwrap() = vec![
            resp(vec![Item::ToolCall {
                id: "1".into(),
                name: "bash".into(),
                arguments: r#"{"command":"touch ran.txt"}"#.into(),
            }]),
            resp(vec![Item::Assistant {
                text: "finished".into(),
            }]),
        ];
        let approver = Arc::new(FakeApprover {
            answer,
            asked: Mutex::new(Vec::new()),
        });
        let mut c = ctx(&fake, dir.path());
        c.sandbox = Arc::new(crate::sandbox::Policy {
            mode: crate::sandbox::Mode::Off,
            ..Default::default()
        });
        c.approver = Some(approver.clone());
        c.permissions = Arc::new(permissions);
        let out = Subagent
            .run(
                json!({"task": "touch it", "description": "Touch a file", "mode": "general"}),
                &c,
            )
            .await;
        let granted = c.granted.lock().unwrap().clone();
        (dir, out, approver, fake, granted)
    }

    #[tokio::test]
    async fn child_calls_are_forwarded_to_the_parent_approver() {
        let (dir, out, approver, _, granted) = run_touch(Approval::Yes, Default::default()).await;
        assert!(!out.is_error, "{}", out.content);
        assert!(dir.path().join("ran.txt").exists());
        let asked = approver.asked.lock().unwrap().clone();
        assert_eq!(asked.len(), 1);
        assert_eq!(asked[0].0, "subagent: Touch a file");
        assert_eq!(asked[0].1, "touch ran.txt");
        assert_eq!(asked[0].2.as_deref(), Some("bash:touch ran.txt"));
        assert!(granted.is_empty());

        let (dir, out, approver, fake, _) = run_touch(Approval::No, Default::default()).await;
        assert!(!out.is_error, "{}", out.content);
        assert!(!dir.path().join("ran.txt").exists());
        assert_eq!(approver.asked.lock().unwrap().len(), 1);
        let results = fake.seen_results.lock().unwrap().clone();
        assert!(results.iter().any(|r| r.contains("denied")), "{results:?}");

        // "Always" for a child's call is handed back to the parent's session.
        let (dir, out, _, _, granted) = run_touch(Approval::Always, Default::default()).await;
        assert!(!out.is_error && dir.path().join("ran.txt").exists());
        assert_eq!(granted, ["bash:touch ran.txt"]);
    }

    #[tokio::test]
    async fn deny_rules_block_child_calls_without_asking() {
        let perms = crate::permissions::Permissions {
            deny: vec!["bash:touch".into()],
            ..Default::default()
        };
        let (dir, out, approver, fake, _) = run_touch(Approval::Yes, perms).await;
        assert!(!out.is_error, "{}", out.content);
        assert!(!dir.path().join("ran.txt").exists());
        assert!(approver.asked.lock().unwrap().is_empty());
        let results = fake.seen_results.lock().unwrap();
        assert!(
            results
                .iter()
                .any(|r| r.contains("blocked by permissions.json rule `deny bash:touch`")),
            "{results:?}"
        );
    }

    #[tokio::test]
    async fn subagent_without_provider_errors() {
        let out = Subagent
            .run(json!({"task": "x"}), &ToolCtx::default())
            .await;
        assert!(out.is_error);
    }

    #[tokio::test]
    async fn subagent_honors_cancellation() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake {
            hang: true,
            ..Fake::default()
        };
        let ctx = ctx(&fake, dir.path());
        let cancel = ctx.cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            cancel.cancel();
        });
        let out = tokio::time::timeout(
            Duration::from_secs(5),
            Subagent.run(json!({"task": "x"}), &ctx),
        )
        .await
        .expect("subagent did not stop on cancel");
        assert!(out.is_error && out.content.contains("cancelled"));
    }

    #[tokio::test]
    async fn timeout_cancels_the_child_before_reporting() {
        let cancel = tokio_util::sync::CancellationToken::new();
        let token = cancel.clone();
        let turn = async move {
            token.cancelled().await;
            Ok(TurnEnd::Cancelled)
        };
        let err = match drive_turn(
            turn,
            Duration::from_millis(30),
            Duration::from_secs(2),
            cancel,
        )
        .await
        {
            Err(err) => err,
            Ok(_) => panic!("expected a timeout"),
        };
        assert!(err.to_string().contains("timed out"), "{err}");
    }

    fn answer(text: &str) -> Response {
        resp(vec![Item::Assistant { text: text.into() }])
    }

    fn named_ctx(fake: &Fake, dir: &std::path::Path, store: &std::path::Path) -> ToolCtx {
        ToolCtx { subagent_dir: Some(store.to_path_buf()), ..ctx(fake, dir) }
    }

    fn users(items: &[Item]) -> Vec<String> {
        items
            .iter()
            .filter_map(|i| match i {
                Item::User { text, .. } => Some(text.clone()),
                Item::Assistant { text } => Some(format!("A:{text}")),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn named_subagent_keeps_context_across_follow_ups_and_session_resume() {
        let dir = tempfile::tempdir().unwrap();
        let session = crate::session::Session::create(&dir.path().join("sessions"), dir.path(), "fake").unwrap();
        let store = subagents::store_dir(&session.path);
        let fake = Fake::default();
        *fake.script.lock().unwrap() = vec![answer("the parser is in src/parse.rs"), answer("it has 3 functions")];
        let c = named_ctx(&fake, dir.path(), &store);

        let out = Subagent.run(json!({"task": "find the parser", "name": "scout", "description": "Find parser"}), &c).await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.starts_with("the parser is in src/parse.rs"), "{}", out.content);
        assert!(out.content.contains("send follow-ups with name \"scout\""), "{}", out.content);
        let out = Subagent.run(json!({"task": "how many functions?", "name": "scout"}), &c).await;
        assert!(out.content.starts_with("it has 3 functions"), "{}", out.content);
        let requests = fake.requests.lock().unwrap().clone();
        assert_eq!(users(&requests[1]), ["find the parser", "A:the parser is in src/parse.rs", "how many functions?"]);

        let record = subagents::load(&store, "scout").unwrap().unwrap();
        assert_eq!((record.status, record.turns, record.mode.as_str()), (Status::Idle, 2, "explore"));
        assert_eq!(record.description, "Find parser");
        assert_eq!(record.usage.input_tokens, 200);

        // A resumed session (a new process: fresh provider, same store) still has it.
        let fake2 = Fake::default();
        *fake2.script.lock().unwrap() = vec![answer("still remember parse.rs")];
        let out = Subagent.run(json!({"task": "where was it?", "name": "scout"}), &named_ctx(&fake2, dir.path(), &store)).await;
        assert!(out.content.starts_with("still remember parse.rs"), "{}", out.content);
        let seen = fake2.requests.lock().unwrap()[0].clone();
        assert_eq!(users(&seen)[0], "find the parser");
        assert_eq!(users(&seen).len(), 5);

        let list = Subagent.run(json!({"action": "list"}), &c).await;
        assert!(list.content.contains("scout") && list.content.contains("idle"), "{}", list.content);
        let inspect = Subagent.run(json!({"action": "inspect", "name": "scout"}), &c).await;
        assert!(inspect.content.contains("where was it?"), "{}", inspect.content);
        assert!(Subagent.run(json!({"action": "inspect"}), &c).await.is_error);
        assert!(Subagent.run(json!({"task": "x", "name": "../evil"}), &c).await.is_error);
        assert!(Subagent.run(json!({"task": "x", "name": "a"}), &ctx(&fake, dir.path())).await.is_error, "no session store");
        assert_eq!(Subagent.summary(&json!({"task": "go", "name": "scout"})), "scout: go");
        assert_eq!(Subagent.summary(&json!({"action": "stop", "name": "scout"})), "stop scout");
        assert!(!Subagent.needs_approval(&json!({"action": "stop", "name": "w", "mode": "general"})));
        assert!(Subagent.needs_approval(&json!({"task": "x", "name": "w", "mode": "general"})));
    }

    #[tokio::test]
    async fn interrupted_and_stopped_subagents_resume() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("s.agents");
        // A process died mid-message: saved as running, a call without a result.
        let mut r = Record::new("fixer", "explore", "fix it");
        r.status = Status::Running;
        r.items = vec![
            Item::user("check a.txt"),
            Item::ToolCall { id: "c1".into(), name: "read_file".into(), arguments: r#"{"path":"a.txt"}"#.into() },
        ];
        subagents::save(&store, &r).unwrap();
        assert_eq!(subagents::load(&store, "fixer").unwrap().unwrap().effective_status(&store), Status::Interrupted);

        let fake = Fake::default();
        *fake.script.lock().unwrap() = vec![answer("a.txt is fine"), answer("ok again")];
        let c = named_ctx(&fake, dir.path(), &store);
        let out = Subagent.run(json!({"action": "resume", "name": "fixer"}), &c).await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.starts_with("a.txt is fine"), "{}", out.content);
        let sent = fake.requests.lock().unwrap()[0].clone();
        assert!(
            sent.iter().any(|i| matches!(i, Item::ToolResult { id, is_error: true, .. } if id == "c1")),
            "the dangling call is answered: {sent:?}"
        );
        assert_eq!(subagents::load(&store, "fixer").unwrap().unwrap().status, Status::Idle);

        // Stopped: follow-ups are refused until it is resumed.
        subagents::stop(&store, "fixer").unwrap();
        let out = Subagent.run(json!({"task": "more", "name": "fixer"}), &c).await;
        assert!(out.is_error && out.content.contains("is stopped"), "{}", out.content);
        let out = Subagent.run(json!({"action": "resume", "name": "fixer"}), &c).await;
        assert!(out.content.contains("ready for follow-up") && out.content.contains("a.txt is fine"), "{}", out.content);
        let out = Subagent.run(json!({"task": "more", "name": "fixer"}), &c).await;
        assert!(out.content.starts_with("ok again"), "{}", out.content);
        assert!(Subagent.run(json!({"action": "resume", "name": "ghost"}), &c).await.is_error);
    }

    #[tokio::test]
    async fn stopping_a_working_subagent_cancels_it() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("s.agents");
        let fake = Fake { hang: true, ..Fake::default() };
        let c = named_ctx(&fake, dir.path(), &store);
        let task = {
            let c = c.clone();
            tokio::spawn(async move { Subagent.run(json!({"task": "spin", "name": "w"}), &c).await })
        };
        let key = subagents::key(&store, "w");
        for _ in 0..200 {
            if subagents::is_live(&key) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(subagents::is_live(&key));
        let busy = Subagent.run(json!({"task": "again", "name": "w"}), &c).await;
        assert!(busy.is_error && busy.content.contains("still working"), "{}", busy.content);
        let stop = Subagent.run(json!({"action": "stop", "name": "w"}), &c).await;
        assert!(stop.content.contains("Stopping"), "{}", stop.content);
        let out = tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
        assert!(out.is_error && out.content.contains("was stopped"), "{}", out.content);
        let record = subagents::load(&store, "w").unwrap().unwrap();
        assert_eq!(record.status, Status::Stopped);
        assert_eq!(users(&record.items), ["spin"]);
        assert!(!subagents::is_live(&key));
    }
}
