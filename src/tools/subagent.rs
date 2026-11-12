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

/// Tools no child gets: children don't delegate further. Name-based on
/// purpose: these two start agents, which no declared effect describes
/// (the other coordination tools, e.g. the team graph, stay available).
const NO_CHILD: &[&str] = &[NAME, crate::scripting::WORKFLOW];

/// Not read-only by its declared effects, but allowed in explore children:
/// a script's sub-calls only reach the child's own (read-only) tools, so
/// what it may do depends on the child, not on the tool. Name-based for that
/// reason; everything else is kept or dropped by its declared effects.
const EXPLORE_EXTRA: &[&str] = &[crate::scripting::RUN_CODE];

const DESCRIPTION: &str = "Delegate substantial independent work. Typed children: output_schema makes the child a typed task that finishes by calling submit with a result matching that schema, and validators (build, tests, lint, cmd:<shell>, exists:<json pointer>) check each submission; failed checks go back to the child within its task budget, and the result comes back as JSON labelled as evidence. isolation \"worktree\" gives the child its own git worktree and branch from HEAD; you get a diff summary, then action merge (name) applies it as a patch (conflicts are reported, never resolved) and stop discards it. mode \"external:<name>\" runs one task on an external ACP agent configured as agents.external.<name> in the user's settings (action run only). Use action spawn with name and task to run a child independently; send queues a follow-up for that name. Up to three children execute concurrently with FIFO inboxes. Reports arrive automatically between parent model requests, and finishing waits for outstanding children: do not poll. Children receive only the supplied task, so include paths, constraints and acceptance criteria. explore (default) is read-only; general can edit, run commands and use MCP after permission checks. Parent and children share one workspace writer. model and effort override inherited settings; permission_mode can only narrow authority. request_id makes an exact spawn/send retry idempotent. inspect/list show state; stop/cancel/close cancel current and queued work; resume reopens a stopped child. Legacy run executes synchronously, optionally retaining named context. Interrupted work is not automatically replayed: inspect its checkpoint and workspace before sending a replacement task. Children cannot delegate or message peers. Avoid delegation for quick lookups.";

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
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// The requested mode: `mode`, else general for a worktree child (it exists
/// to make changes), else explore.
fn mode_of(args: &Value) -> &str {
    str_arg(args, "mode").unwrap_or(if wants_worktree(args) {
        "general"
    } else {
        "explore"
    })
}

fn is_general(args: &Value) -> bool {
    mode_of(args) == "general"
}

/// `<name>` of `mode: "external:<name>"`.
fn external_name(args: &Value) -> Option<&str> {
    mode_of(args)
        .strip_prefix("external:")
        .map(str::trim)
        .filter(|n| !n.is_empty())
}

fn wants_worktree(args: &Value) -> bool {
    str_arg(args, "isolation") == Some("worktree")
}

/// The child's name: `name`, or its alias `id`.
fn name_arg(args: &Value) -> Option<&str> {
    str_arg(args, "name").or_else(|| str_arg(args, "id"))
}

/// A fresh id for a child that wasn't given a name.
fn fresh_id(prefix: &str) -> String {
    format!(
        "{prefix}-{}",
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    )
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
fn child_tools(
    general: bool,
    shared: &[Arc<dyn Tool>],
    ledger: std::sync::Arc<crate::tools::FileLedger>,
) -> Registry {
    let mut tools = Registry::builtin();
    tools.share_file_ledger(ledger);
    let none = Value::Object(Default::default());
    let names: Vec<String> = tools.specs().into_iter().map(|s| s.name).collect();
    let keep: Vec<String> = names
        .iter()
        .filter(|n| !NO_CHILD.contains(&n.as_str()))
        .filter(|n| {
            general
                // Scripts only reach the tools the child itself has.
                || EXPLORE_EXTRA.contains(&n.as_str())
                || tools
                    .get(n)
                    .is_some_and(|t| t.is_read_only(&none))
        })
        .cloned()
        .collect();
    let drop: Vec<String> = names
        .iter()
        .filter(|n| !keep.contains(n))
        .cloned()
        .collect();
    tools.remove_names(&drop);
    for tool in shared {
        if general || tool.is_read_only(&none) {
            let _ = tools.try_insert_shared(tool.clone());
        }
    }
    tools
}

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
                    "mode": {"type": "string", "description": "explore (default; general with isolation worktree): read-only tools. general: may also edit files and run commands. external:<name>: an external ACP agent from settings (one task, action run). Fixed when a named subagent is created."},
                    "output_schema": {"type": "object", "description": "JSON Schema the child's final answer must match; the result comes back as JSON."},
                    "validators": {"type": "array", "items": {"type": "string"}, "description": "Checks on the result, in order: build, tests, lint, cmd:<shell command> (exit 0 passes), exists:<json pointer to path(s) in the result>."},
                    "isolation": {"type": "string", "enum": ["shared", "worktree"], "description": "shared (default): your workspace, writes serialized by the writer lock. worktree: a separate git worktree under .nyq/worktrees/<name>; merge it afterwards. Fixed when a named subagent is created."},
                    "id": {"type": "string", "description": "Alias of name for merge/inspect/stop."},
                    "name": {"type": "string", "description": "Keep the subagent under this name (letters, digits, - and _) so later calls can send it follow-ups."},
                    "action": {"type": "string", "enum": ["run", "spawn", "send", "list", "inspect", "stop", "cancel", "close", "resume", "merge"], "description": "spawn/send: queue task for name. run: synchronous task. list/inspect: state. stop/cancel/close: stop work (and discard an unmerged worktree). resume: reopen. merge: apply an isolated child's worktree to your workspace."},
                    "request_id": {"type":"string", "description":"Stable retry key for spawn/send; reuse only for the exact same task."},
                    "model": {"type": "string", "description": "Model for this subagent (default: yours), e.g. a faster one for simple searches."},
                    "effort": {"type": "string", "description": "Reasoning effort for this subagent (e.g. low, medium, high)."},
                    "permission_mode": {"type": "string", "enum": ["ask", "auto", "yolo"], "description": "Permission mode for this subagent; never more permissive than yours."}
                }
            }),
        }
    }

    fn needs_approval(&self, args: &Value) -> bool {
        let starts = matches!(action(args), "run" | "spawn" | "send");
        let executes = crate::tools::typed::Typed::from_args(args)
            .ok()
            .flatten()
            .is_some_and(|t| t.executes());
        (starts && (is_general(args) || external_name(args).is_some() || executes))
            || action(args) == "merge"
    }

    fn summary(&self, args: &Value) -> String {
        let label = label(args);
        let name = name_arg(args);
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

    /// The child's own calls take the writer lease; the delegating call
    /// never does.
    fn coordinates(&self) -> bool {
        true
    }

    /// A run or resume hosts a nested agent whose own calls take the
    /// workspace writer lease, so the delegating call itself stays `Pure`:
    /// holding the lease here would deadlock the child's writes. The other
    /// actions change orchestration state and run alone.
    fn concurrency(&self, args: &Value) -> super::Concurrency {
        match action(args) {
            "run" | "resume" => super::Concurrency::Pure,
            _ => super::Concurrency::Barrier,
        }
    }

    fn preview(&self, args: &Value, _: &ToolCtx) -> Option<String> {
        let task = args
            .get("task")
            .and_then(Value::as_str)
            .filter(|t| !t.trim().is_empty())?;
        let mode = mode_of(args);
        let name = str_arg(args, "name")
            .map(|n| format!(" · name: {n}"))
            .unwrap_or_default();
        let mut head = format!("mode: {mode}{name}");
        if wants_worktree(args) {
            head.push_str(" · isolation: worktree");
        }
        if let Some(ext) = external_name(args) {
            if let Ok(cfg) = crate::external_agent::config(ext) {
                head.push_str(&format!(
                    "\ncommand: {} {} (runs outside nyq's sandbox)",
                    cfg.command,
                    cfg.args.join(" ")
                ));
            }
        }
        if let Ok(Some(typed)) = crate::tools::typed::Typed::from_args(args) {
            head.push('\n');
            head.push_str(&typed.describe());
        }
        Some(format!("{head}\n{task}"))
    }

    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolOutput {
        let name = name_arg(&args);
        let dir = ctx.subagent_dir.as_deref();
        if let Err(e) = crate::tools::typed::Typed::from_args(&args) {
            return ToolOutput::err(format!("Invalid typed subagent arguments: {e}"));
        }
        if !matches!(
            str_arg(&args, "isolation"),
            None | Some("shared" | "worktree")
        ) {
            return ToolOutput::err("isolation must be \"shared\" or \"worktree\".");
        }
        if let Some(ext) = external_name(&args) {
            return external(&args, ext, ctx).await;
        }
        if !matches!(mode_of(&args), "explore" | "general") {
            return ToolOutput::err("mode must be explore, general or external:<name>.");
        }
        match (action(&args), name, dir) {
            ("run", None, Some(dir)) if wants_worktree(&args) => {
                named_run(&args, &fresh_id("wt"), dir, ctx).await
            }
            ("run", None, None) if wants_worktree(&args) => ToolOutput::err(
                "Worktree isolation is unavailable here: isolated children are kept with a saved session.",
            ),
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
            ("spawn" | "send", Some(n), Some(dir)) => {
                if str_arg(&args, "task").is_none() { return ToolOutput::err("Missing task."); }
                if ctx.provider.is_none() { return ToolOutput::err("No provider available."); }
                match crate::orchestration::enqueue(dir, n, &args) {
                    Ok(id) => { let _ = pump(ctx); ToolOutput::ok(format!("Queued child {n}, task {id}. Results are delivered automatically; keep working or finish to wait. Do not poll.")) }
                    Err(e) => ToolOutput::err(format!("{e:#}")),
                }
            }
            ("run", Some(n), Some(dir)) => named_run(&args, n, dir, ctx).await,
            ("inspect", Some(n), Some(dir)) => match subagents::show_lines(dir, n, 20) {
                Ok(lines) => ToolOutput::ok(lines.join("\n")),
                Err(e) => ToolOutput::err(format!("{e:#}")),
            },
            ("stop" | "cancel" | "close", Some(n), Some(dir)) => match crate::orchestration::cancel(dir, n).and_then(|_| subagents::stop(dir, n)) {
                Ok(msg) => ToolOutput::ok(msg),
                Err(e) => ToolOutput::err(format!("{e:#}")),
            },
            ("resume", Some(n), Some(dir)) => resume(n, dir, ctx).await,
            ("merge", Some(n), Some(dir)) => merge(n, dir, ctx).await,
            (other, _, _) => ToolOutput::err(format!("Unknown action `{other}` (run, spawn, send, list, inspect, stop, resume, merge).")),
        }
    }
}

/// A child agent with `items` as its transcript. `args` may override the
/// model, effort and (only towards stricter) permission mode.
fn child(
    ctx: &ToolCtx,
    general: bool,
    persistent: bool,
    items: Vec<Item>,
    args: &Value,
    cwd: Option<&Path>,
) -> Result<Agent, ToolOutput> {
    // A worktree child works (and is sandboxed) in its own checkout.
    let cwd = cwd
        .map(Path::to_path_buf)
        .unwrap_or_else(|| ctx.cwd.clone());
    let Some(provider) = &ctx.provider else {
        return Err(ToolOutput::err(
            "Subagents are unavailable: no model provider in this context.",
        ));
    };
    let mut provider = provider.fork();
    if let Some(model) = str_arg(args, "model") {
        provider.set_model(model.to_string());
    }
    if let Some(effort) = str_arg(args, "effort") {
        provider.set_effort(Some(effort.to_string()));
    }
    let parent_mode = if ctx.yolo {
        crate::permissions::Mode::Yolo
    } else {
        ctx.permission_mode
    };
    let permission_mode = match str_arg(args, "permission_mode") {
        None => parent_mode,
        Some(raw) => match crate::permissions::Mode::parse(raw) {
            Some(mode) if mode_rank(mode) <= mode_rank(parent_mode) => mode,
            Some(_) => parent_mode,
            None => {
                return Err(ToolOutput::err(format!(
                    "Unknown permission_mode `{raw}` (ask, auto, yolo)."
                )))
            }
        },
    };
    let context_window = crate::config::context_window_for(provider.name(), None);
    let mut system = format!(
        "{}\n\n{PREAMBLE}",
        crate::prompt::system(&cwd, &ctx.sandbox)
    );
    if persistent {
        system.push_str(&format!("\n\n{PERSISTENT}"));
    }
    Ok(Agent {
        provider,
        tools: child_tools(general, &ctx.shared_tools, ctx.file_ledger.clone()),
        system,
        items,
        session: None,
        usage_session_id: ctx.session_id.clone(),
        cwd,
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
        compaction: Default::default(),
        submitted: 0,
        // The parent's tool hooks also vet the subagent's calls.
        hooks: std::sync::Arc::new(ctx.hooks.for_subagent()),
        sandbox: ctx.sandbox.clone(),
        undo: Default::default(),
        taint: Default::default(),
        workset: Default::default(),
        task: Default::default(),
    })
}

/// How a child's turn went.
struct Outcome {
    result: anyhow::Result<TurnEnd>,
    calls: Vec<(String, String)>,
    undo: crate::undo::UndoLog,
    /// Cancelled by a `stop` (not by the parent or a timeout).
    stopped: bool,
    /// A typed child's checked result and how many submissions it made.
    typed: Option<(crate::tools::typed::Checked, u32)>,
}

/// What a typed child is checked against, and where.
struct TypedRun {
    typed: crate::tools::typed::Typed,
    /// The child's workspace (its worktree when isolated).
    root: std::path::PathBuf,
    sandbox: Arc<crate::sandbox::Policy>,
}

impl TypedRun {
    fn from_args(args: &Value, root: &Path, ctx: &ToolCtx) -> Option<Self> {
        let typed = crate::tools::typed::Typed::from_args(args).ok().flatten()?;
        Some(Self {
            typed,
            root: root.to_path_buf(),
            sandbox: ctx.sandbox.clone(),
        })
    }

    /// Make the child's next turn a typed task (W9): it finishes through
    /// `submit` with the schema, and the validators run on each submission
    /// inside the child, holding the workspace writer while commands run.
    fn arm(&self, agent: &mut Agent, goal: &str, writes: bool) {
        agent.set_task(Some(self.typed.task_spec(goal, writes)));
    }

    /// The external-agent fallback: check its final message. Validators that
    /// run commands hold the workspace writer so they see a quiescent tree.
    async fn check(
        &self,
        text: &str,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> crate::tools::typed::Checked {
        let writer = crate::orchestration::writer(&self.root);
        let _lease = if self.typed.executes() {
            tokio::select! {
                lock = writer.lock_unknown() => Some(lock),
                _ = cancel.cancelled() => None,
            }
        } else {
            None
        };
        self.typed
            .check(text, &self.root, &self.sandbox, cancel)
            .await
    }
}

/// A typed nyq child's task text: the task plus how to finish (`submit`).
fn with_instructions(task: &str, typed: Option<&TypedRun>) -> String {
    match typed {
        Some(t) => format!("{task}{}", t.typed.submit_instructions()),
        None => task.to_string(),
    }
}

fn last_answer(agent: &Agent) -> &str {
    agent
        .items
        .iter()
        .rev()
        .find_map(|i| match i {
            Item::Assistant { text } if !text.trim().is_empty() => Some(text.trim()),
            _ => None,
        })
        .unwrap_or("")
}

/// Run `fut` as team member `name` when there is a team store.
async fn as_member<F: std::future::Future>(dir: Option<&Path>, name: &str, fut: F) -> F::Output {
    match dir {
        Some(dir) => {
            crate::team::MEMBER
                .scope(
                    crate::team::Member {
                        dir: dir.to_path_buf(),
                        name: name.to_string(),
                    },
                    fut,
                )
                .await
        }
        None => fut.await,
    }
}

/// Children running at once, across sync runs, background workers and
/// external agents.
static SLOTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(3);

/// Run one turn of `agent` (a new message, or a continuation when `input` is
/// None), with a live progress line and approvals forwarded to the parent.
async fn drive_child(
    agent: &mut Agent,
    input: Option<Item>,
    ctx: &ToolCtx,
    origin: String,
    live: LiveGuard,
    cancel: tokio_util::sync::CancellationToken,
    typed: bool,
) -> Outcome {
    // Legacy synchronous children share the same execution cap.
    let _slot = tokio::select! {
        slot = SLOTS.acquire() => slot.ok(),
        _ = cancel.cancelled() => None,
    };
    let mut ui = Collector {
        calls: Vec::new(),
        approver: ctx.approver.clone(),
        origin,
        live,
        note: None,
    };
    let result = match input {
        Some(item) => {
            drive_turn(
                agent.turn(item, &mut ui, cancel.clone()),
                TIMEOUT,
                CLEANUP,
                cancel.clone(),
            )
            .await
        }
        None => {
            drive_turn(
                agent.continue_turn(&mut ui, cancel.clone()),
                TIMEOUT,
                CLEANUP,
                cancel.clone(),
            )
            .await
        }
    };
    // A typed child's turn ended through its task: an accepted `submit`,
    // or a budget or escalation that gave up (failed checks already went
    // back to it as `submit` errors).
    let checked = (typed && matches!(result, Ok(TurnEnd::Done))).then(|| {
        let outcome = agent.take_task_outcome();
        crate::tools::typed::Typed::outcome(outcome.as_ref(), agent.task_state())
    });
    if typed {
        agent.set_task(None);
    }
    if !ctx.cancel.is_cancelled() {
        let last = agent.items.iter().rev().find_map(|i| match i {
            Item::Assistant { text } => Some(text.as_str()),
            _ => None,
        });
        ctx.hooks
            .subagent_stop(
                &ui.origin,
                matches!(result, Ok(TurnEnd::Done)),
                last.unwrap_or(""),
                &ctx.cwd,
                ctx.session_id.as_deref(),
            )
            .await;
    }
    ctx.granted
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .extend(agent.allowed.difference(&ctx.allowed).cloned());
    Outcome {
        result,
        calls: std::mem::take(&mut ui.calls),
        undo: std::mem::take(&mut agent.undo),
        stopped: ui.live.stopped(),
        typed: checked,
    }
}

fn display(calls: &[(String, String)]) -> String {
    if calls.is_empty() {
        return "(no tool calls)".into();
    }
    calls
        .iter()
        .map(|(name, summary)| format!("{name} {summary}").trim_end().to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

/// The tool output for a finished turn: the report or the failure. `child`
/// is the evidence label's id; `changes` an isolated child's diff summary.
fn report(
    outcome: Outcome,
    agent: &Agent,
    footer: String,
    child: &str,
    changes: Option<String>,
) -> ToolOutput {
    let display = Some(display(&outcome.calls));
    let undo = outcome.undo;
    let fail = |msg: String| ToolOutput {
        display: display.clone(),
        undo: undo.clone(),
        ..ToolOutput::err(msg)
    };
    match outcome.result {
        Err(e) if e.to_string().contains("timed out") => {
            return fail(format!(
                "Subagent timed out after {} minutes{footer}.",
                TIMEOUT.as_secs() / 60
            ));
        }
        Err(e) => return fail(format!("Subagent failed: {e:#}{footer}")),
        Ok(TurnEnd::Cancelled) => return fail(format!("Subagent cancelled{footer}.")),
        Ok(TurnEnd::Done) => {}
    }
    let stats = format!(
        "subagent used {} tool calls, ↑{} ↓{} tokens{footer}",
        outcome.calls.len(),
        agent.usage.input_tokens,
        agent.usage.output_tokens
    );
    if let Some((checked, attempts)) = &outcome.typed {
        let mut extra = json!({ "note": stats });
        if let Some(changes) = changes {
            extra["changes"] = Value::String(changes);
        }
        let content = crate::tools::typed::render(child, checked, *attempts, extra);
        return ToolOutput {
            display,
            undo,
            is_error: checked.failure.is_some(),
            ..ToolOutput::ok(truncate(&content, MAX_REPORT * 2))
        };
    }
    let report = match last_answer(agent) {
        "" => "(the subagent produced no report)",
        text => text,
    };
    let mut content = truncate(report, MAX_REPORT);
    content.push_str(&format!(
        "\n\n({stats})\n{}",
        crate::tools::typed::evidence(child)
    ));
    if let Some(changes) = changes {
        content.push_str("\n\n");
        content.push_str(&changes);
    }
    ToolOutput {
        display,
        undo,
        ..ToolOutput::ok(content)
    }
}

async fn one_shot(args: &Value, ctx: &ToolCtx) -> ToolOutput {
    let Some(task) = args
        .get("task")
        .and_then(Value::as_str)
        .filter(|t| !t.trim().is_empty())
    else {
        return ToolOutput::err("Missing required argument `task`.");
    };
    let mut agent = match child(ctx, is_general(args), false, Vec::new(), args, None) {
        Ok(a) => a,
        Err(e) => return e,
    };
    let id = fresh_id("run");
    let typed = TypedRun::from_args(args, &ctx.cwd, ctx);
    if let Some(t) = &typed {
        t.arm(&mut agent, task, is_general(args));
    }
    let label = one_line(label(args));
    let cancel = ctx.cancel.child_token();
    let Some(live) = LiveGuard::start(None, &label, cancel.clone()) else {
        return ToolOutput::err("Subagent could not start.");
    };
    let outcome = as_member(
        ctx.subagent_dir.as_deref(),
        &id,
        drive_child(
            &mut agent,
            Some(Item::user(with_instructions(task, typed.as_ref()))),
            ctx,
            format!("subagent: {label}"),
            live,
            cancel,
            typed.is_some(),
        ),
    )
    .await;
    report(outcome, &agent, String::new(), &id, None)
}

/// Create the named subagent `name`, or send it a follow-up message.
async fn named_run(args: &Value, name: &str, dir: &Path, ctx: &ToolCtx) -> ToolOutput {
    let Some(task) = args
        .get("task")
        .and_then(Value::as_str)
        .filter(|t| !t.trim().is_empty())
    else {
        return ToolOutput::err("Missing required argument `task` (the message for the subagent).");
    };
    let record = match subagents::load(dir, name) {
        Ok(Some(r)) if r.status == Status::Stopped => {
            return ToolOutput::err(format!(
                "Subagent `{name}` is stopped; call subagent with action \"resume\" and name \"{name}\" to reopen it."
            ));
        }
        Ok(Some(r)) if wants_worktree(args) && r.isolation.is_none() => {
            return ToolOutput::err(format!(
                "Subagent `{name}` shares your workspace; isolation is fixed when a child is created. Use a new name for an isolated child."
            ));
        }
        Ok(Some(r)) => r,
        Ok(None) => {
            let mut r = Record::new(
                name,
                if is_general(args) {
                    "general"
                } else {
                    "explore"
                },
                label(args),
            );
            if wants_worktree(args) {
                match crate::worktree::create(&ctx.cwd, name).await {
                    Ok(wt) => {
                        r.isolation = Some("worktree".into());
                        r.worktree = Some(wt);
                    }
                    Err(e) => {
                        return ToolOutput::err(format!("Could not create a worktree: {e:#}"))
                    }
                }
            }
            r
        }
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
    let unfinished = record.status != Status::Idle
        && !matches!(record.items.last(), Some(Item::Assistant { .. }) | None);
    if !unfinished {
        if record.status != Status::Idle {
            record.status = Status::Idle;
            record.updated = chrono::Utc::now();
            if let Err(e) = subagents::save(dir, &record) {
                return ToolOutput::err(format!("{e:#}"));
            }
        }
        let last = record
            .last_answer()
            .map(|a| format!("\nIts last answer:\n{}", truncate(a, 2_000)))
            .unwrap_or_default();
        return ToolOutput::ok(format!(
            "Subagent `{name}` is ready for follow-up messages (call subagent with name \"{name}\" and a task).{last}"
        ));
    }
    run_named(record, None, dir, ctx, &Value::Null).await
}

/// Run a turn of a saved subagent and save it again.
pub(crate) async fn run_named(
    mut record: Record,
    input: Option<Item>,
    dir: &Path,
    ctx: &ToolCtx,
    args: &Value,
) -> ToolOutput {
    let name = record.name.clone();
    if record.isolation.is_some() && record.worktree.is_none() {
        return ToolOutput::err(format!(
            "Subagent `{name}`'s worktree was merged or removed; start a new isolated child instead."
        ));
    }
    let cwd = record
        .worktree
        .as_ref()
        .map(|w| w.path.clone())
        .unwrap_or_else(|| ctx.cwd.clone());
    let typed = TypedRun::from_args(args, &cwd, ctx);
    let goal = match &input {
        Some(Item::User { text, .. }) => text.clone(),
        _ => String::new(),
    };
    let input = match input {
        Some(Item::User { text, images }) => Some(Item::User {
            text: with_instructions(&text, typed.as_ref()),
            images,
        }),
        other => other,
    };
    let cancel = ctx.cancel.child_token();
    let live_label = if record.description.is_empty() {
        name.clone()
    } else {
        format!("{name}: {}", one_line(&record.description))
    };
    let Some(live) = LiveGuard::start(
        Some(subagents::key(dir, &name)),
        &live_label,
        cancel.clone(),
    ) else {
        return ToolOutput::err(format!(
            "Subagent `{name}` is still working on an earlier message; wait for it or stop it."
        ));
    };
    // A turn cut short earlier may have left calls without results.
    let mut items = std::mem::take(&mut record.items);
    let repairs = crate::session::repairs(&items);
    items.extend(repairs);
    let mut agent = match child(ctx, record.mode == "general", true, items, args, Some(&cwd)) {
        Ok(a) => a,
        Err(e) => return e,
    };
    if let Some(t) = &typed {
        t.arm(&mut agent, &goal, record.mode == "general");
    }
    // Saved as running with the new message, so a crash leaves it resumable.
    record.items = agent.items.clone();
    record.items.extend(input.clone());
    record.status = Status::Running;
    record.updated = chrono::Utc::now();
    if let Err(e) = subagents::save(dir, &record) {
        return ToolOutput::err(format!("{e:#}"));
    }
    let outcome = as_member(
        Some(dir),
        &name,
        crate::subagents::CHECKPOINT.scope(
            (dir.to_owned(), name.clone()),
            drive_child(
                &mut agent,
                input,
                ctx,
                format!("subagent {name}"),
                live,
                cancel,
                typed.is_some(),
            ),
        ),
    )
    .await;
    record.status = match &outcome.result {
        Ok(TurnEnd::Done) => Status::Idle,
        _ if outcome.stopped => Status::Stopped,
        _ => Status::Interrupted,
    };
    record.items = agent.items.clone();
    record.turns += 1;
    record.tool_calls += outcome.calls.len() as u64;
    record.usage.add(&agent.usage);
    record.updated = chrono::Utc::now();
    // A stopped isolated child's worktree is discarded; a finished one's
    // changes are summarized for review and merge.
    let mut changes = None;
    if record.status == Status::Stopped {
        if let Some(wt) = record.worktree.take() {
            let _ = wt.remove();
        }
    } else if let (Some(wt), Ok(TurnEnd::Done)) = (&record.worktree, &outcome.result) {
        changes = Some(wt.summary(&name).await);
    }
    let saved = subagents::save(dir, &record);
    let footer = match record.status {
        Status::Idle => {
            format!("; `{name}` keeps its context: send follow-ups with name \"{name}\"")
        }
        Status::Stopped => format!("; `{name}` was stopped"),
        _ => format!("; `{name}` can finish this with action \"resume\""),
    };
    let mut out = report(outcome, &agent, footer, &name, changes);
    if let Err(e) = saved {
        out.is_error = true;
        out.content
            .push_str(&format!("\n(could not save subagent `{name}`: {e:#})"));
    }
    out
}

/// `merge`: apply an isolated child's worktree to the workspace as a patch.
/// A patch that doesn't apply is reported and nothing is written.
async fn merge(name: &str, dir: &Path, ctx: &ToolCtx) -> ToolOutput {
    let mut record = match subagents::load(dir, name) {
        Ok(Some(r)) => r,
        Ok(None) => return ToolOutput::err(format!("No subagent named `{name}` in this session.")),
        Err(e) => return ToolOutput::err(format!("{e:#}")),
    };
    let Some(wt) = record.worktree.clone() else {
        return ToolOutput::err(format!(
            "Subagent `{name}` has no worktree to merge (it shares your workspace, or was already merged or stopped)."
        ));
    };
    if subagents::is_live(&subagents::key(dir, name)) {
        return ToolOutput::err(format!(
            "Subagent `{name}` is still working; wait for it before merging."
        ));
    }
    // The patch writes the parent's workspace: take its writer lease.
    let writer = crate::orchestration::writer(&ctx.cwd);
    let _lease = tokio::select! {
        lock = writer.lock_unknown() => lock,
        _ = ctx.cancel.cancelled() => return ToolOutput::err("Cancelled waiting for workspace writer"),
    };
    let mut undo = crate::undo::UndoLog::default();
    let merged = match wt.merge(&mut undo).await {
        Ok(m) => m,
        Err(e) => return ToolOutput::err(format!("Merge of `{name}` failed: {e:#}")),
    };
    let (text, is_error) = crate::worktree::describe(name, &merged);
    let applied = matches!(merged, crate::worktree::Merge::Applied { .. });
    if !matches!(merged, crate::worktree::Merge::Conflict { .. }) {
        record.worktree = None;
        record.status = Status::Stopped;
        record.updated = chrono::Utc::now();
        if let Err(e) = subagents::save(dir, &record) {
            return ToolOutput::err(format!("{text}\n(could not save `{name}`: {e:#})"));
        }
    }
    ToolOutput {
        content: text,
        is_error,
        changed: applied,
        undo,
        ..Default::default()
    }
}

/// `mode: "external:<name>"`: one task on an external ACP agent. With
/// worktree isolation the agent works in its own worktree, kept under a
/// generated child name for `merge`.
async fn external(args: &Value, ext: &str, ctx: &ToolCtx) -> ToolOutput {
    if action(args) != "run" {
        return ToolOutput::err(
            "External agents run a single task: use action \"run\" (they can't be spawned, sent follow-ups or resumed).",
        );
    }
    if str_arg(args, "name").is_some() {
        return ToolOutput::err("External agents are single-task and can't be named.");
    }
    let Some(task) = str_arg(args, "task") else {
        return ToolOutput::err("Missing required argument `task`.");
    };
    let cfg = match crate::external_agent::config(ext) {
        Ok(c) => c,
        Err(e) => return ToolOutput::err(format!("{e:#}")),
    };
    let label = one_line(label(args));
    let mode = format!("external:{ext}");
    // An isolated run is saved like a named child so it can be merged.
    let mut saved: Option<(Record, std::path::PathBuf)> = None;
    if wants_worktree(args) {
        let Some(dir) = &ctx.subagent_dir else {
            return ToolOutput::err(
                "Worktree isolation is unavailable here: isolated children are kept with a saved session.",
            );
        };
        let id = fresh_id("ext");
        let mut r = Record::new(&id, &mode, &label);
        match crate::worktree::create(&ctx.cwd, &id).await {
            Ok(wt) => {
                r.isolation = Some("worktree".into());
                r.worktree = Some(wt);
            }
            Err(e) => return ToolOutput::err(format!("Could not create a worktree: {e:#}")),
        }
        r.status = Status::Running;
        if let Err(e) = subagents::save(dir, &r) {
            return ToolOutput::err(format!("{e:#}"));
        }
        saved = Some((r, dir.clone()));
    }
    let id = saved
        .as_ref()
        .map(|(r, _)| r.name.clone())
        .unwrap_or_else(|| fresh_id("ext"));
    let cwd = saved
        .as_ref()
        .and_then(|(r, _)| r.worktree.as_ref())
        .map(|w| w.path.clone())
        .unwrap_or_else(|| ctx.cwd.clone());
    // An external agent has no `submit` tool: it is told to answer with
    // JSON, and its final message is checked once (the fallback).
    let typed = TypedRun::from_args(args, &cwd, ctx);
    let prompt = match &typed {
        Some(t) => format!("{task}{}", t.typed.instructions()),
        None => task.to_string(),
    };
    let cancel = ctx.cancel.child_token();
    let Some(live) = LiveGuard::start(None, &format!("{ext}: {label}"), cancel.clone()) else {
        return ToolOutput::err("Subagent could not start.");
    };
    let live = Arc::new(live);
    let observer = live.clone();
    let _slot = tokio::select! {
        slot = SLOTS.acquire() => slot.ok(),
        _ = cancel.cancelled() => None,
    };
    let result = crate::external_agent::run(
        &cfg,
        crate::external_agent::Run {
            task: &prompt,
            cwd: &cwd,
            approver: ctx.approver.clone(),
            auto_approve: ctx.yolo || ctx.permission_mode.is_yolo(),
            origin: format!("external {ext}: {label}"),
            cancel: cancel.clone(),
            timeout: TIMEOUT,
            on_tool: Some(Box::new(move |t, s| observer.tool(t, &one_line(s)))),
        },
    )
    .await;
    let display = match &result {
        Ok(o) => Some(display(&o.tool_calls)),
        Err(_) => None,
    };
    let finish = |content: String, is_error: bool| ToolOutput {
        content,
        is_error,
        display: display.clone(),
        ..Default::default()
    };
    let outcome = match result {
        Ok(o) => o,
        Err(e) => {
            if let Some((mut r, dir)) = saved {
                if let Some(wt) = r.worktree.take() {
                    let _ = wt.remove();
                }
                r.status = Status::Stopped;
                let _ = subagents::save(&dir, &r);
            }
            return finish(format!("External agent `{ext}` failed: {e:#}"), true);
        }
    };
    let mut changes = None;
    if let Some((mut r, dir)) = saved {
        r.items = vec![
            Item::user(prompt.clone()),
            Item::Assistant {
                text: outcome.text.clone(),
            },
        ];
        r.status = Status::Idle;
        r.turns = 1;
        r.tool_calls = outcome.tool_calls.len() as u64;
        r.updated = chrono::Utc::now();
        if let Some(wt) = &r.worktree {
            changes = Some(wt.summary(&r.name).await);
        }
        let _ = subagents::save(&dir, &r);
    }
    let stats = format!(
        "external agent {ext} reported {} tool calls, stop reason {}",
        outcome.tool_calls.len(),
        outcome.stop_reason
    );
    if let Some(t) = &typed {
        // A single task: no retry for external agents.
        let checked = t.check(&outcome.text, &cancel).await;
        let mut extra = json!({ "note": stats });
        if let Some(c) = changes {
            extra["changes"] = Value::String(c);
        }
        let failed = checked.failure.is_some();
        return finish(crate::tools::typed::render(&id, &checked, 1, extra), failed);
    }
    let mut content = format!(
        "{}\n\n({stats})\n{}",
        truncate(outcome.text.trim(), MAX_REPORT),
        crate::tools::typed::evidence(&id)
    );
    if let Some(c) = changes {
        content.push_str("\n\n");
        content.push_str(&c);
    }
    finish(content, false)
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
    /// The note the user attached to the last approval.
    note: Option<String>,
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
        let (answer, note) = match &self.approver {
            Some(approver) => {
                let _turn = APPROVALS.lock().await;
                approver.approve_noted(&self.origin, req).await
            }
            None => (Approval::No, None),
        };
        self.note = note;
        answer
    }
    fn approval_note(&mut self) -> Option<String> {
        self.note.take()
    }
}

/// Start up to three independent children. A name has one FIFO worker.
/// No mutex is held across model execution or an approval.
pub(crate) fn pump(ctx: &ToolCtx) -> anyhow::Result<()> {
    let Some(dir) = ctx.subagent_dir.clone() else {
        return Ok(());
    };
    let shared = crate::orchestration::store(&dir)?;
    let mut store = shared.lock().unwrap_or_else(|e| e.into_inner());
    let mut next = store.jobs.clone();
    let mut selected = Vec::new();
    let mut names = store.active.clone();
    for job in &mut next {
        if names.len() >= 3 {
            break;
        }
        if job.state == "queued" && !names.contains(&job.name) {
            names.insert(job.name.clone());
            job.state = "running".into();
            selected.push(job.clone());
        }
    }
    if selected.is_empty() {
        return Ok(());
    }
    store.commit(next)?;
    for job in &selected {
        store.active.insert(job.name.clone());
    }
    drop(store);
    for job in selected {
        let ctx = ctx.clone();
        let dir = dir.clone();
        let shared = shared.clone();
        tokio::spawn(async move {
            let cancelled = shared
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .jobs
                .iter()
                .find(|j| j.id == job.id)
                .is_none_or(|j| j.state == "cancelled");
            let output = if ctx.cancel.is_cancelled() || cancelled {
                ToolOutput::err("Interrupted before child started")
            } else {
                use futures::FutureExt;
                std::panic::AssertUnwindSafe(named_run(&job.args, &job.name, &dir, &ctx))
                    .catch_unwind()
                    .await
                    .unwrap_or_else(|_| {
                        ToolOutput::err(
                            "Child worker panicked; inspect its checkpoint before continuing",
                        )
                    })
            };
            let mut s = shared.lock().unwrap_or_else(|e| e.into_inner());
            let mut next = s.jobs.clone();
            if let Some(j) = next.iter_mut().find(|j| j.id == job.id) {
                if j.state != "cancelled" {
                    j.state = if ctx.cancel.is_cancelled() {
                        "interrupted"
                    } else if output.is_error {
                        "failed"
                    } else {
                        "succeeded"
                    }
                    .into();
                    j.result = Some(truncate(&output.content, MAX_REPORT));
                }
            }
            if let Err(e) = s.commit(next) {
                s.failure = Some(format!("Child result commit failed: {e:#}"));
            }
            s.outputs.insert(job.id.clone(), output);
            s.active.remove(&job.name);
            s.notify.notify_one();
        });
    }
    Ok(())
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
            self.seen_tools
                .lock()
                .unwrap()
                .push(req.tools.iter().map(|t| t.name.clone()).collect());
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
                ..Default::default()
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
    async fn parent_receives_completed_results_once_at_a_safe_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::default();
        *fake.script.lock().unwrap() = vec![
            resp(vec![Item::Assistant {
                text: "Acknowledged".into(),
            }]),
            resp(vec![Item::Assistant {
                text: "Next".into(),
            }]),
        ];
        let ctx = ctx(&fake, dir.path());
        let mut parent = child(&ctx, true, false, vec![], &Value::Null, None)
            .unwrap_or_else(|e| panic!("{}", e.content));
        let session = crate::session::Session::create(dir.path(), dir.path(), "fake").unwrap();
        let store_dir = subagents::store_dir(&session.path);
        parent.session = Some(session);
        let shared = crate::orchestration::store(&store_dir).unwrap();
        shared
            .lock()
            .unwrap()
            .commit(vec![crate::orchestration::Job {
                id: "finished".into(),
                name: "reader".into(),
                args: Value::Null,
                state: "succeeded".into(),
                result: Some("Evidence from child".into()),
                delivered: false,
            }])
            .unwrap();
        parent
            .turn(
                Item::user("continue"),
                &mut Collector {
                    calls: vec![],
                    approver: None,
                    origin: "test".into(),
                    live: LiveGuard::start(None, "test", Default::default()).unwrap(),
                    note: None,
                },
                Default::default(),
            )
            .await
            .unwrap();
        assert!(crate::orchestration::results(&store_dir)
            .unwrap()
            .is_empty());
        let count = |items: &[Item]| {
            items.iter().filter(|i| matches!(i, Item::User { text, .. } if text.contains("[orchestration-result:finished]"))).count()
        };
        assert_eq!(count(&parent.items), 1);
        parent
            .turn(
                Item::user("another turn"),
                &mut Collector {
                    calls: vec![],
                    approver: None,
                    origin: "test".into(),
                    live: LiveGuard::start(None, "test", Default::default()).unwrap(),
                    note: None,
                },
                Default::default(),
            )
            .await
            .unwrap();
        assert_eq!(count(&parent.items), 1);
    }

    #[tokio::test]
    async fn queued_followups_are_fifo_and_results_are_durable() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::default();
        *fake.script.lock().unwrap() = vec![
            resp(vec![Item::Assistant {
                text: "first".into(),
            }]),
            resp(vec![Item::Assistant {
                text: "second".into(),
            }]),
        ];
        let mut ctx = ctx(&fake, dir.path());
        ctx.subagent_dir = Some(dir.path().join("children"));
        for (key, task) in [("one", "first task"), ("two", "second task")] {
            let out = Subagent
                .run(
                    json!({"action":"spawn", "name":"reader", "task":task, "request_id":key}),
                    &ctx,
                )
                .await;
            assert!(!out.is_error, "{}", out.content);
        }
        let path = ctx.subagent_dir.as_ref().unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while crate::orchestration::pending(path).unwrap() {
                let shared = crate::orchestration::store(path).unwrap();
                let notify = shared.lock().unwrap().notify.clone();
                let notified = notify.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                pump(&ctx).unwrap();
                if crate::orchestration::pending(path).unwrap() {
                    notified.await;
                }
            }
        })
        .await
        .unwrap();
        let results = crate::orchestration::results(path).unwrap();
        assert_eq!(results.len(), 2);
        assert!(results[0].1.contains("first"));
        assert!(results[1].1.contains("second"));
        let record = subagents::load(path, "reader").unwrap().unwrap();
        assert_eq!(record.turns, 2);
        assert!(record
            .items
            .iter()
            .any(|i| matches!(i, Item::User { text, .. } if text == "first task")));
        for (id, _) in results {
            crate::orchestration::acknowledge(path, &id).unwrap();
        }
        assert!(crate::orchestration::results(path).unwrap().is_empty());
    }

    #[tokio::test]
    async fn cancelling_child_cancels_its_queued_followup() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake {
            hang: true,
            ..Default::default()
        };
        let mut ctx = ctx(&fake, dir.path());
        ctx.subagent_dir = Some(dir.path().join("children"));
        let path = ctx.subagent_dir.as_ref().unwrap();
        Subagent
            .run(
                json!({"action":"spawn", "name":"reader", "task":"hang"}),
                &ctx,
            )
            .await;
        Subagent
            .run(
                json!({"action":"send", "name":"reader", "task":"follow up"}),
                &ctx,
            )
            .await;
        crate::orchestration::cancel(path, "reader").unwrap();
        ctx.cancel.cancel();
        assert!(!crate::orchestration::pending(path).unwrap());
        assert!(crate::orchestration::results(path).unwrap().is_empty());
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
            ToolSpec {
                name: self.0.into(),
                description: String::new(),
                parameters: json!({}),
            }
        }
        fn summary(&self, _: &Value) -> String {
            String::new()
        }
        fn effects(&self, _: &Value) -> crate::tools::Effects {
            if self.0 == "skill" {
                crate::tools::Effects::reads_only()
            } else {
                crate::tools::Effects::default()
            }
        }
        async fn run(&self, _: Value, _: &ToolCtx) -> ToolOutput {
            ToolOutput::ok("")
        }
    }

    #[test]
    fn children_inherit_skills_mcp_and_at_most_the_parent_mode() {
        let shared: Vec<Arc<dyn Tool>> =
            vec![Arc::new(Named("skill")), Arc::new(Named("mcp__wiki__ask"))];
        let names = |general| -> Vec<String> {
            child_tools(general, &shared, Default::default())
                .specs()
                .into_iter()
                .map(|s| s.name)
                .collect()
        };
        assert!(
            names(false).contains(&"skill".into())
                && !names(false).contains(&"mcp__wiki__ask".into())
        );
        assert!(
            names(true).contains(&"skill".into()) && names(true).contains(&"mcp__wiki__ask".into())
        );

        let dir = tempfile::tempdir().unwrap();
        let mut parent = ctx(&Fake::default(), dir.path());
        parent.permission_mode = crate::permissions::Mode::Auto;
        let mode = |args: Value| {
            child(&parent, false, false, Vec::new(), &args, None).map(|a| a.permission_mode)
        };
        assert_eq!(mode(json!({})).ok(), Some(crate::permissions::Mode::Auto));
        assert_eq!(
            mode(json!({"permission_mode": "ask"})).ok(),
            Some(crate::permissions::Mode::Ask)
        );
        assert_eq!(
            mode(json!({"permission_mode": "yolo"})).ok(),
            Some(crate::permissions::Mode::Auto)
        );
        assert!(mode(json!({"permission_mode": "bogus"})).is_err());
    }

    #[test]
    fn general_mode_needs_approval_and_has_write_tools() {
        assert!(!Subagent.needs_approval(&json!({"task": "x"})));
        assert!(Subagent.needs_approval(&json!({"task": "x", "mode": "general"})));
        let names: Vec<String> = child_tools(true, &[], Default::default())
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
        ToolCtx {
            subagent_dir: Some(store.to_path_buf()),
            ..ctx(fake, dir)
        }
    }

    fn users(items: &[Item]) -> Vec<String> {
        items
            .iter()
            .filter_map(|i| match i {
                // Without the per-request session note on the newest message.
                Item::User { text, .. } => Some(
                    text.split("\n\n[Session state]")
                        .next()
                        .unwrap_or("")
                        .to_string(),
                ),
                Item::Assistant { text } => Some(format!("A:{text}")),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn named_subagent_keeps_context_across_follow_ups_and_session_resume() {
        let dir = tempfile::tempdir().unwrap();
        let session =
            crate::session::Session::create(&dir.path().join("sessions"), dir.path(), "fake")
                .unwrap();
        let store = subagents::store_dir(&session.path);
        let fake = Fake::default();
        *fake.script.lock().unwrap() = vec![
            answer("the parser is in src/parse.rs"),
            answer("it has 3 functions"),
        ];
        let c = named_ctx(&fake, dir.path(), &store);

        let out = Subagent
            .run(
                json!({"task": "find the parser", "name": "scout", "description": "Find parser"}),
                &c,
            )
            .await;
        assert!(!out.is_error, "{}", out.content);
        assert!(
            out.content.starts_with("the parser is in src/parse.rs"),
            "{}",
            out.content
        );
        assert!(
            out.content.contains("send follow-ups with name \"scout\""),
            "{}",
            out.content
        );
        let out = Subagent
            .run(json!({"task": "how many functions?", "name": "scout"}), &c)
            .await;
        assert!(
            out.content.starts_with("it has 3 functions"),
            "{}",
            out.content
        );
        let requests = fake.requests.lock().unwrap().clone();
        assert_eq!(
            users(&requests[1]),
            [
                "find the parser",
                "A:the parser is in src/parse.rs",
                "how many functions?"
            ]
        );

        let record = subagents::load(&store, "scout").unwrap().unwrap();
        assert_eq!(
            (record.status, record.turns, record.mode.as_str()),
            (Status::Idle, 2, "explore")
        );
        assert_eq!(record.description, "Find parser");
        assert_eq!(record.usage.input_tokens, 200);

        // A resumed session (a new process: fresh provider, same store) still has it.
        let fake2 = Fake::default();
        *fake2.script.lock().unwrap() = vec![answer("still remember parse.rs")];
        let out = Subagent
            .run(
                json!({"task": "where was it?", "name": "scout"}),
                &named_ctx(&fake2, dir.path(), &store),
            )
            .await;
        assert!(
            out.content.starts_with("still remember parse.rs"),
            "{}",
            out.content
        );
        let seen = fake2.requests.lock().unwrap()[0].clone();
        assert_eq!(users(&seen)[0], "find the parser");
        assert_eq!(users(&seen).len(), 5);

        let list = Subagent.run(json!({"action": "list"}), &c).await;
        assert!(
            list.content.contains("scout") && list.content.contains("idle"),
            "{}",
            list.content
        );
        let inspect = Subagent
            .run(json!({"action": "inspect", "name": "scout"}), &c)
            .await;
        assert!(
            inspect.content.contains("where was it?"),
            "{}",
            inspect.content
        );
        assert!(
            Subagent
                .run(json!({"action": "inspect"}), &c)
                .await
                .is_error
        );
        assert!(
            Subagent
                .run(json!({"task": "x", "name": "../evil"}), &c)
                .await
                .is_error
        );
        assert!(
            Subagent
                .run(json!({"task": "x", "name": "a"}), &ctx(&fake, dir.path()))
                .await
                .is_error,
            "no session store"
        );
        assert_eq!(
            Subagent.summary(&json!({"task": "go", "name": "scout"})),
            "scout: go"
        );
        assert_eq!(
            Subagent.summary(&json!({"action": "stop", "name": "scout"})),
            "stop scout"
        );
        assert!(
            !Subagent.needs_approval(&json!({"action": "stop", "name": "w", "mode": "general"}))
        );
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
            Item::ToolCall {
                id: "c1".into(),
                name: "read_file".into(),
                arguments: r#"{"path":"a.txt"}"#.into(),
            },
        ];
        subagents::save(&store, &r).unwrap();
        assert_eq!(
            subagents::load(&store, "fixer")
                .unwrap()
                .unwrap()
                .effective_status(&store),
            Status::Interrupted
        );

        let fake = Fake::default();
        *fake.script.lock().unwrap() = vec![answer("a.txt is fine"), answer("ok again")];
        let c = named_ctx(&fake, dir.path(), &store);
        let out = Subagent
            .run(json!({"action": "resume", "name": "fixer"}), &c)
            .await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.starts_with("a.txt is fine"), "{}", out.content);
        let sent = fake.requests.lock().unwrap()[0].clone();
        assert!(
            sent.iter()
                .any(|i| matches!(i, Item::ToolResult { id, is_error: true, .. } if id == "c1")),
            "the dangling call is answered: {sent:?}"
        );
        assert_eq!(
            subagents::load(&store, "fixer").unwrap().unwrap().status,
            Status::Idle
        );

        // Stopped: follow-ups are refused until it is resumed.
        subagents::stop(&store, "fixer").unwrap();
        let out = Subagent
            .run(json!({"task": "more", "name": "fixer"}), &c)
            .await;
        assert!(
            out.is_error && out.content.contains("is stopped"),
            "{}",
            out.content
        );
        let out = Subagent
            .run(json!({"action": "resume", "name": "fixer"}), &c)
            .await;
        assert!(
            out.content.contains("ready for follow-up") && out.content.contains("a.txt is fine"),
            "{}",
            out.content
        );
        let out = Subagent
            .run(json!({"task": "more", "name": "fixer"}), &c)
            .await;
        assert!(out.content.starts_with("ok again"), "{}", out.content);
        assert!(
            Subagent
                .run(json!({"action": "resume", "name": "ghost"}), &c)
                .await
                .is_error
        );
    }

    #[tokio::test]
    async fn stopping_a_working_subagent_cancels_it() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("s.agents");
        let fake = Fake {
            hang: true,
            ..Fake::default()
        };
        let c = named_ctx(&fake, dir.path(), &store);
        let task = {
            let c = c.clone();
            tokio::spawn(
                async move { Subagent.run(json!({"task": "spin", "name": "w"}), &c).await },
            )
        };
        let key = subagents::key(&store, "w");
        for _ in 0..200 {
            if subagents::is_live(&key) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(subagents::is_live(&key));
        let busy = Subagent
            .run(json!({"task": "again", "name": "w"}), &c)
            .await;
        assert!(
            busy.is_error && busy.content.contains("still working"),
            "{}",
            busy.content
        );
        let stop = Subagent
            .run(json!({"action": "stop", "name": "w"}), &c)
            .await;
        assert!(stop.content.contains("Stopping"), "{}", stop.content);
        let out = tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap();
        assert!(
            out.is_error && out.content.contains("was stopped"),
            "{}",
            out.content
        );
        let record = subagents::load(&store, "w").unwrap().unwrap();
        assert_eq!(record.status, Status::Stopped);
        assert_eq!(users(&record.items), ["spin"]);
        assert!(!subagents::is_live(&key));
    }

    fn typed_args(validators: Value) -> Value {
        json!({
            "task": "list the relevant files",
            "output_schema": {
                "type": "object",
                "required": ["files"],
                "properties": {"files": {"type": "array", "items": {"type": "string"}}}
            },
            "validators": validators
        })
    }

    fn last_user(items: &[Item]) -> String {
        users(items).last().cloned().unwrap_or_default()
    }

    fn submit(id: &str, args: Value) -> Response {
        resp(vec![Item::ToolCall {
            id: id.into(),
            name: crate::task::SUBMIT.into(),
            arguments: args.to_string(),
        }])
    }

    fn tool_results(items: &[Item]) -> Vec<String> {
        items
            .iter()
            .filter_map(|i| match i {
                Item::ToolResult { output, .. } => Some(output.clone()),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn typed_child_submits_and_returns_checked_json_evidence() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "x").unwrap();
        let fake = Fake::default();
        *fake.script.lock().unwrap() = vec![submit("s1", json!({"files": ["a.txt"]}))];
        let out = Subagent
            .run(
                typed_args(json!(["exists:/files"])),
                &ctx(&fake, dir.path()),
            )
            .await;
        assert!(!out.is_error, "{}", out.content);
        let v: Value = serde_json::from_str(&out.content).unwrap();
        assert_eq!(v["status"], "passed");
        assert_eq!(v["attempts"], 1);
        assert_eq!(v["result"]["files"][0], "a.txt");
        let child = v["child"].as_str().unwrap();
        assert!(child.starts_with("run-"));
        assert_eq!(v["evidence"], crate::tools::typed::evidence(child));
        assert_eq!(
            v["checks"],
            json!([
                {"validator": "schema", "verdict": "pass"},
                {"validator": "exists:/files", "verdict": "pass"}
            ])
        );
        // The child ran as a typed task: it was offered `submit` with the
        // parent's schema, told how to finish, and shown its task section.
        let first = fake.requests.lock().unwrap()[0].clone();
        let prompt = last_user(&first);
        assert!(prompt.contains("Required result"), "{prompt}");
        assert!(
            prompt.contains("[Task]\nYOUR CURRENT TASK: list the relevant files"),
            "{prompt}"
        );
        assert!(fake.seen_tools.lock().unwrap()[0].contains(&"submit".to_string()));
        // Executing validators make the call ask; exists alone doesn't.
        assert!(!Subagent.needs_approval(&typed_args(json!(["exists:/files"]))));
        assert!(Subagent.needs_approval(&typed_args(json!(["cmd:make check"]))));
        assert!(Subagent.needs_approval(&typed_args(json!(["tests"]))));
        let bad = Subagent
            .run(typed_args(json!(["frobnicate"])), &ctx(&fake, dir.path()))
            .await;
        assert!(bad.is_error && bad.content.contains("unknown validator"));
    }

    #[tokio::test]
    async fn typed_child_resubmits_after_a_failed_check() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "x").unwrap();
        let fake = Fake::default();
        *fake.script.lock().unwrap() = vec![
            submit("s1", json!({"files": ["ghost.rs"]})),
            submit("s2", json!({"files": ["a.txt"]})),
        ];
        let out = Subagent
            .run(
                typed_args(json!(["exists:/files"])),
                &ctx(&fake, dir.path()),
            )
            .await;
        assert!(!out.is_error, "{}", out.content);
        let v: Value = serde_json::from_str(&out.content).unwrap();
        assert_eq!(v["status"], "passed");
        assert_eq!(v["attempts"], 2);
        let requests = fake.requests.lock().unwrap().clone();
        assert_eq!(requests.len(), 2);
        // The failed check came back as the `submit` error.
        let fed_back = tool_results(&requests[1]).join("\n");
        assert!(fed_back.contains("ghost.rs"), "{fed_back}");
        assert!(
            fed_back.contains("rejected by the `exists:/files` check"),
            "{fed_back}"
        );
    }

    #[tokio::test]
    async fn typed_child_that_never_submits_fails_with_a_report() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::default();
        // Each reply without `submit` costs the syntax budget (3).
        *fake.script.lock().unwrap() = vec![
            answer("I looked around; a.txt is the one."),
            answer(r#"{"files": ["a.txt"]}"#),
            answer("done"),
        ];
        let out = Subagent
            .run(
                typed_args(json!(["exists:/files"])),
                &ctx(&fake, dir.path()),
            )
            .await;
        assert!(out.is_error, "{}", out.content);
        let v: Value = serde_json::from_str(&out.content).unwrap();
        assert_eq!(v["status"], "failed");
        assert_eq!(v["attempts"], 0);
        let why = v["failure"].as_str().unwrap();
        assert!(why.contains("syntax budget exhausted"), "{why}");
        let requests = fake.requests.lock().unwrap().clone();
        assert_eq!(requests.len(), 3, "the budget, not a retry count, ends it");
        assert!(last_user(&requests[1]).contains("You must call `submit`"));
    }

    /// Edits need a prior read in the session (the read-before-edit guard).
    fn read_call(id: &str, path: &str) -> Response {
        resp(vec![Item::ToolCall {
            id: id.into(),
            name: "read_file".into(),
            arguments: json!({"path": path}).to_string(),
        }])
    }

    fn write_call(id: &str, path: &str, content: &str) -> Response {
        resp(vec![Item::ToolCall {
            id: id.into(),
            name: "write_file".into(),
            arguments: json!({"path": path, "content": content}).to_string(),
        }])
    }

    #[tokio::test]
    async fn worktree_child_is_isolated_then_merged_or_conflicts() {
        let repo = crate::worktree::tests::repo();
        let store = tempfile::tempdir().unwrap();
        let fake = Fake::default();
        *fake.script.lock().unwrap() = vec![
            read_call("r1", "a.txt"),
            write_call("1", "a.txt", "one\nchild\nthree\n"),
            answer("edited a.txt"),
            answer("plain child done"),
            read_call("r2", "a.txt"),
            write_call("2", "a.txt", "one\nsecond\nthree\n"),
            answer("edited a.txt again"),
        ];
        let mut c = named_ctx(&fake, repo.path(), store.path());
        c.yolo = true;
        let read = |p: &str| std::fs::read_to_string(repo.path().join(p)).unwrap();

        let out = Subagent
            .run(
                json!({"task": "edit a", "name": "w1", "isolation": "worktree"}),
                &c,
            )
            .await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("+child"), "{}", out.content);
        assert!(out.content.contains("action \"merge\""));
        // The child wrote its worktree, not the parent's files.
        assert_eq!(read("a.txt"), "one\ntwo\nthree\n");
        let record = subagents::load(store.path(), "w1").unwrap().unwrap();
        assert_eq!(record.mode, "general");
        let wt = record.worktree.clone().unwrap();
        assert_eq!(
            std::fs::read_to_string(wt.path.join("a.txt")).unwrap(),
            "one\nchild\nthree\n"
        );
        // Sharing is fixed at creation.
        let again = Subagent
            .run(json!({"task": "x", "name": "plain"}), &c)
            .await;
        assert!(!again.is_error, "{}", again.content);
        let refused = Subagent
            .run(
                json!({"task": "x", "name": "plain", "isolation": "worktree"}),
                &c,
            )
            .await;
        assert!(refused.is_error && refused.content.contains("isolation is fixed"));

        let merged = Subagent
            .run(json!({"action": "merge", "id": "w1"}), &c)
            .await;
        assert!(!merged.is_error, "{}", merged.content);
        assert!(merged.changed && !merged.undo.is_empty());
        assert_eq!(read("a.txt"), "one\nchild\nthree\n");
        assert!(!wt.path.exists());
        let record = subagents::load(store.path(), "w1").unwrap().unwrap();
        assert!(record.worktree.is_none() && record.status == Status::Stopped);
        let gone = Subagent
            .run(json!({"action": "merge", "name": "w1"}), &c)
            .await;
        assert!(gone.is_error);

        // A second child started from HEAD edits the line the merge changed.
        let out = Subagent
            .run(
                json!({"task": "edit a", "name": "w2", "isolation": "worktree"}),
                &c,
            )
            .await;
        assert!(!out.is_error, "{}", out.content);
        let conflict = Subagent
            .run(json!({"action": "merge", "name": "w2"}), &c)
            .await;
        assert!(conflict.is_error, "{}", conflict.content);
        assert!(
            conflict.content.contains("a.txt") && conflict.content.contains("nothing was written")
        );
        assert_eq!(read("a.txt"), "one\nchild\nthree\n");
        let wt2 = subagents::load(store.path(), "w2")
            .unwrap()
            .unwrap()
            .worktree
            .unwrap();
        assert!(wt2.path.is_dir());
        // Stopping discards it.
        let stopped = Subagent
            .run(json!({"action": "stop", "name": "w2"}), &c)
            .await;
        assert!(!stopped.is_error, "{}", stopped.content);
        assert!(!wt2.path.exists());
        assert!(subagents::load(store.path(), "w2")
            .unwrap()
            .unwrap()
            .worktree
            .is_none());
    }
}
