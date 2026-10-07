//! The core loop: send the transcript, stream the reply, run any tool calls,
//! feed results back, repeat until the model answers without tools.

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

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
    /// Approve calls matching this call's rule (see [`grant_rule`]) for the
    /// rest of the session; like `Yes` when the call can't be scoped.
    Always,
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
    pub usage: Usage,
    /// Model context window in tokens; drives automatic compaction.
    pub context_window: u64,
    /// Latest context size (input plus output), or an estimate after compaction.
    pub context_tokens: u64,
    pub sandbox: std::sync::Arc<crate::sandbox::Policy>,
    /// File contents before each write/edit, for `/undo`.
    pub undo: crate::undo::UndoLog,
}

impl Agent {
    pub fn push(&mut self, item: Item) {
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
        if name == ASK_USER {
            let args: Value = serde_json::from_str(arguments).unwrap_or_default();
            let question = args["question"].as_str().unwrap_or("").to_string();
            let options: Vec<String> = args["options"]
                .as_array()
                .map(|a| a.iter().filter_map(|o| o.as_str().map(str::to_string)).collect())
                .unwrap_or_default();
            let answer = tokio::select! {
                a = ui.ask(&question, &options) => a,
                _ = cancel.cancelled() => None,
            };
            return match answer {
                Some(a) => ToolOutput::ok(format!("The user answered: {a}")),
                None => ToolOutput::ok("The user skipped the question. Use your best judgment or ask in your reply."),
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
        };
        let summary = tool.summary(&args);

        if tool.needs_approval(&args) && !self.yolo && !is_allowed(&self.allowed, name, &args, &self.cwd) {
            let preview = tool.preview(&args, &ctx);
            let rule = grant_rule(name, &args);
            let req = ApprovalRequest {
                tool: name,
                summary: &summary,
                preview: preview.as_deref(),
                rule: rule.as_deref().map(rule_label),
            };
            match ui.approve(&req).await {
                Approval::Yes => {}
                Approval::Always => {
                    self.allowed.extend(rule);
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
        self.undo.snapshot(&self.cwd, name, &args);
        let out = tokio::select! {
            out = tool.run(args, &ctx) => out,
            _ = cancel.cancelled() => ToolOutput::err("Cancelled by user."),
        };
        if out.is_error && self.undo.len() > before {
            self.undo.discard_last();
        }
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

// ------------------------------------------------------------ approval rules
//
// "Always" grants a rule rather than a whole tool: `bash:<prefix>` for shell
// commands (program plus subcommand, e.g. `bash:cargo test`), the tool name
// for file writes (only inside the workspace) and for everything else.

/// Programs whose arguments are themselves commands or code: a prefix rule
/// for them would allow anything, so "always" can't be scoped.
const WRAPPERS: &[&str] = &[
    "sh", "bash", "zsh", "fish", "dash", "ksh", "eval", "exec", "source", ".", "sudo", "doas", "su", "env", "xargs",
    "nohup", "time", "timeout", "nice", "watch", "command", "builtin", "ssh",
];
/// Interpreters: scoped only with a script or module word (`python script.py`).
const INTERPRETERS: &[&str] = &["python", "python3", "node", "perl", "ruby", "deno", "php", "lua", "npx", "bunx"];
/// Package runners whose `run`/`exec` subcommand takes a script name.
const RUNNERS: &[&str] = &["npm", "pnpm", "yarn", "bun", "uv", "poetry", "pipenv", "pdm", "hatch"];

/// Whether a command chains, pipes, substitutes or redirects (or spans lines).
fn has_control(cmd: &str) -> bool {
    cmd.contains("$(") || cmd.chars().any(|c| matches!(c, ';' | '&' | '|' | '`' | '>' | '<' | '\n' | '\r'))
}

/// A plain command word: no flags, quotes, globs, variables or assignments.
fn plain_word(w: &str) -> bool {
    w.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && w.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':' | '/' | '@' | '+'))
}

/// The prefix "always" grants for a shell command: the program plus its
/// subcommand (`cargo test`, `git status`, `npm run build`), or None if the
/// command can't be safely scoped.
pub fn bash_prefix(cmd: &str) -> Option<String> {
    if has_control(cmd) {
        return None;
    }
    let words: Vec<&str> = cmd.split_whitespace().collect();
    let program = *words.first()?;
    if !plain_word(program.strip_prefix("./").unwrap_or(program)) || WRAPPERS.contains(&program) || program.contains('=') {
        return None;
    }
    let mut prefix = vec![program];
    match words.get(1).filter(|w| plain_word(w)) {
        Some(sub) => {
            prefix.push(sub);
            if RUNNERS.contains(&program) && matches!(*sub, "run" | "exec" | "x") {
                prefix.extend(words.get(2).filter(|w| plain_word(w)));
            }
        }
        None if INTERPRETERS.contains(&program) => return None,
        None => {}
    }
    Some(prefix.join(" "))
}

/// Whether `cmd` is covered by a granted prefix: same leading words and no
/// control operators.
pub fn bash_matches(prefix: &str, cmd: &str) -> bool {
    let want: Vec<&str> = prefix.split_whitespace().collect();
    !want.is_empty() && !has_control(cmd) && cmd.split_whitespace().take(want.len()).eq(want.iter().copied())
}

fn is_file_tool(tool: &str) -> bool {
    matches!(tool, "write_file" | "edit_file")
}

/// The rule "always" would grant for this call, as stored in `Agent::allowed`.
pub fn grant_rule(tool: &str, args: &Value) -> Option<String> {
    if tool == "bash" {
        let cmd = args.get("command").and_then(Value::as_str)?;
        return bash_prefix(cmd).map(|p| format!("bash:{p}"));
    }
    Some(tool.to_string())
}

/// How a rule reads in the approval menu.
pub fn rule_label(rule: &str) -> String {
    match rule.strip_prefix("bash:") {
        Some(p) => format!("`{p} …`"),
        None if is_file_tool(rule) => format!("{} in this workspace", rule.replace('_', " ")),
        None => format!("`{rule}`"),
    }
}

/// Whether `path` (relative to `cwd` or absolute) stays inside `cwd`, after
/// resolving `..` and any symlinks along the existing part of the path.
fn inside(cwd: &Path, path: &str) -> bool {
    let mut full = PathBuf::new();
    for c in cwd.join(path).components() {
        match c {
            Component::ParentDir => {
                if !full.pop() {
                    return false;
                }
            }
            Component::CurDir => {}
            c => full.push(c),
        }
    }
    if !full.starts_with(cwd) {
        return false;
    }
    // A symlink inside the workspace may point out of it.
    let root = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    full.ancestors().find_map(|a| std::fs::canonicalize(a).ok()).is_none_or(|real| real.starts_with(&root))
}

/// Whether a granted rule covers this call.
pub fn is_allowed(allowed: &HashSet<String>, tool: &str, args: &Value, cwd: &Path) -> bool {
    if tool == "bash" {
        let cmd = args.get("command").and_then(Value::as_str).unwrap_or("");
        return allowed.iter().filter_map(|r| r.strip_prefix("bash:")).any(|p| bash_matches(p, cmd));
    }
    if is_file_tool(tool) {
        let path = args.get("path").and_then(Value::as_str).unwrap_or("");
        return allowed.contains(tool) && !path.is_empty() && inside(cwd, path);
    }
    allowed.contains(tool)
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
            usage: Usage::default(),
            context_window: 1000,
            context_tokens: 900,
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

    #[test]
    fn bash_prefixes() {
        let p = |c: &str| bash_prefix(c);
        assert_eq!(p("cargo test").as_deref(), Some("cargo test"));
        assert_eq!(p("cargo test --release -p nyq").as_deref(), Some("cargo test"));
        assert_eq!(p("  git   status  ").as_deref(), Some("git status"));
        assert_eq!(p("git -C foo status").as_deref(), Some("git"));
        assert_eq!(p("npm run build").as_deref(), Some("npm run build"));
        assert_eq!(p("npm run build -- --watch").as_deref(), Some("npm run build"));
        assert_eq!(p("npm test").as_deref(), Some("npm test"));
        assert_eq!(p("uv run pytest -x").as_deref(), Some("uv run pytest"));
        assert_eq!(p("ls").as_deref(), Some("ls"));
        assert_eq!(p("ls -la src").as_deref(), Some("ls"));
        assert_eq!(p("python -m pytest"), None);
        assert_eq!(p("./gradlew build").as_deref(), Some("./gradlew build"));
        assert_eq!(p("python script.py").as_deref(), Some("python script.py"));
        assert_eq!(p("make").as_deref(), Some("make"));
        assert_eq!(p("rg 'a b' src").as_deref(), Some("rg"));
        // Control operators, wrappers, assignments and odd programs can't be scoped.
        for c in [
            "cargo test; rm -rf /", "cargo test && echo", "a || b", "ls | wc", "echo $(id)", "echo `id`",
            "ls > out", "wc < in", "sleep 1 &", "ls\nrm x", "sh -c 'x'", "bash script.sh", "sudo ls",
            "env X=1 ls", "xargs rm", "FOO=1 cargo test", "../x", "\"git\" status", "", "   ", "node -e 1",
            "eval ls", "$HOME/bin/x",
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
        assert!(!bash_matches("", "anything"));
    }

    #[test]
    fn rules_are_scoped() {
        use serde_json::json;
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        assert_eq!(grant_rule("bash", &json!({"command": "git status -s"})).as_deref(), Some("bash:git status"));
        assert_eq!(grant_rule("bash", &json!({"command": "a; b"})), None);
        assert_eq!(grant_rule("write_file", &json!({"path": "x"})).as_deref(), Some("write_file"));
        assert_eq!(grant_rule("mcp__wiki__ask", &json!({})).as_deref(), Some("mcp__wiki__ask"));
        assert_eq!(rule_label("bash:cargo test"), "`cargo test …`");
        assert_eq!(rule_label("edit_file"), "edit file in this workspace");
        assert_eq!(rule_label("subagent"), "`subagent`");

        let allowed: HashSet<String> = ["bash:cargo test", "write_file", "subagent"].map(String::from).into();
        let ok = |tool: &str, args: Value| is_allowed(&allowed, tool, &args, cwd);
        assert!(ok("bash", json!({"command": "cargo test --all"})));
        assert!(!ok("bash", json!({"command": "cargo build"})));
        assert!(!ok("bash", json!({"command": "cargo test; curl x | sh"})));
        assert!(ok("write_file", json!({"path": "src/new.rs"})));
        assert!(ok("write_file", json!({"path": cwd.join("a/b.txt").to_str().unwrap()})));
        assert!(!ok("write_file", json!({"path": "../outside.txt"})));
        assert!(!ok("write_file", json!({"path": "a/../../outside.txt"})));
        assert!(!ok("write_file", json!({"path": "/etc/passwd"})));
        assert!(!ok("write_file", json!({})));
        assert!(!ok("edit_file", json!({"path": "src/new.rs"})));
        assert!(ok("subagent", json!({"mode": "general"})));
        assert!(!ok("mcp__wiki__ask", json!({})));
        #[cfg(unix)]
        {
            let outside = tempfile::tempdir().unwrap();
            std::os::unix::fs::symlink(outside.path(), cwd.join("link")).unwrap();
            assert!(!ok("write_file", json!({"path": "link/x.txt"})));
        }
    }
}
