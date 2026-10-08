//! `subagent`: delegate a self-contained task to a fresh agent with its own
//! context; only its final report comes back to the parent model.

use std::time::Duration;

use serde_json::{json, Value};

use super::{Registry, Tool, ToolCtx, ToolOutput};
use std::sync::Arc;

use crate::agent::{Agent, Approval, ApprovalRequest, Approver, TurnEnd, Ui};
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
independent subagent calls can run in parallel. The subagent sees NOTHING of this conversation, \
so `task` must contain complete instructions: the goal, relevant paths and names, constraints, \
and what to report back. Modes: \"explore\" (default) gets read-only tools (read, list, glob, \
grep, view images) and needs no approval; \"general\" may also edit files and run commands. \
Don't use it for a quick lookup you can do with one or two direct tool calls.";

const PREAMBLE: &str = "\
You are a subagent: another agent delegated the task below to you. You have none of its \
conversation, only this task. Work on it autonomously with your tools; you cannot ask the user \
questions. When done, finish with a concise report of your findings, including file paths and \
line numbers where relevant. The report is all the delegating agent will see.";

pub fn register(r: &mut Registry) {
    r.add(Box::new(Subagent));
}

struct Subagent;

fn is_general(args: &Value) -> bool {
    args.get("mode").and_then(Value::as_str) == Some("general")
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

/// The child's tools: builtins (minus mutating ones for explore), never itself.
fn child_tools(general: bool) -> Registry {
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
    tools
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
                    "task": {"type": "string", "description": "Complete, self-contained instructions. The subagent sees nothing else."},
                    "description": {"type": "string", "description": "Short 3-6 word label for the task, shown to the user."},
                    "mode": {"type": "string", "enum": ["explore", "general"], "description": "explore (default): read-only tools. general: may also edit files and run commands."}
                },
                "required": ["task"]
            }),
        }
    }

    fn needs_approval(&self, args: &Value) -> bool {
        is_general(args)
    }

    fn summary(&self, args: &Value) -> String {
        let label = label(args);
        if is_general(args) {
            format!("{label} (general)")
        } else {
            label.to_string()
        }
    }

    fn handles_cancel(&self) -> bool {
        true
    }

    fn preview(&self, args: &Value, _: &ToolCtx) -> Option<String> {
        let task = args
            .get("task")
            .and_then(Value::as_str)
            .filter(|t| !t.trim().is_empty())?;
        let mode = if is_general(args) {
            "general"
        } else {
            "explore"
        };
        Some(format!("mode: {mode}\n{task}"))
    }

    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolOutput {
        let Some(task) = args
            .get("task")
            .and_then(Value::as_str)
            .filter(|t| !t.trim().is_empty())
        else {
            return ToolOutput::err("Missing required argument `task`.");
        };
        let Some(provider) = &ctx.provider else {
            return ToolOutput::err(
                "Subagents are unavailable: no model provider in this context.",
            );
        };
        let provider = provider.fork();
        let context_window = crate::config::context_window_for(provider.name(), None);
        let mut agent = Agent {
            provider,
            tools: child_tools(is_general(&args)),
            system: format!(
                "{}\n\n{PREAMBLE}",
                crate::prompt::system(&ctx.cwd, &ctx.sandbox)
            ),
            items: Vec::new(),
            session: None,
            cwd: ctx.cwd.clone(),
            // Inherit the parent's mode, "always" grants and rules.
            // Calls that still need approval are forwarded to the parent's
            // approver, or denied when there is none: approving this call
            // does not auto-approve them. A yolo flag set on the context
            // wins so older callers that only set `yolo` still skip prompts.
            permission_mode: if ctx.yolo {
                crate::permissions::Mode::Yolo
            } else {
                ctx.permission_mode
            },
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
        };
        let origin = format!("subagent: {}", one_line(label(&args)));
        let mut ui = Collector {
            calls: Vec::new(),
            approver: ctx.approver.clone(),
            origin,
        };
        let cancel = ctx.cancel.child_token();
        let result = drive_turn(
            agent.turn(Item::user(task), &mut ui, cancel.clone()),
            TIMEOUT,
            CLEANUP,
            cancel,
        )
        .await;
        let display = Some(ui.display());
        let undo = std::mem::take(&mut agent.undo);
        ctx.granted
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .extend(agent.allowed.difference(&ctx.allowed).cloned());
        let fail = |msg: String| ToolOutput {
            display: display.clone(),
            undo: undo.clone(),
            ..ToolOutput::err(msg)
        };
        match result {
            Err(e) if e.to_string().contains("timed out") => {
                return fail(format!(
                    "Subagent timed out after {} minutes.",
                    TIMEOUT.as_secs() / 60
                ));
            }
            Err(e) => return fail(format!("Subagent failed: {e:#}")),
            Ok(TurnEnd::Cancelled) => return fail("Subagent cancelled.".into()),
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
            "\n\n(subagent used {} tool calls, ↑{} ↓{} tokens)",
            ui.calls.len(),
            agent.usage.input_tokens,
            agent.usage.output_tokens
        ));
        ToolOutput {
            display,
            undo,
            ..ToolOutput::ok(content)
        }
    }
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

/// Headless UI for the child agent: records tool calls. Approvals go to the
/// parent's approver (the user) when there is one; otherwise they are denied.
struct Collector {
    calls: Vec<(String, String)>,
    approver: Option<Arc<dyn Approver>>,
    /// Who is asking, e.g. "subagent: Count files".
    origin: String,
}

impl Collector {
    fn display(&self) -> String {
        if self.calls.is_empty() {
            return "(no tool calls)".into();
        }
        self.calls
            .iter()
            .map(|(name, summary)| format!("{name} {summary}").trim_end().to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[async_trait::async_trait]
impl Ui for Collector {
    fn stream(&mut self, _: &StreamEvent) {}
    fn end_message(&mut self) {}
    fn tool_start(&mut self, tool: &str, summary: &str) {
        self.calls.push((tool.to_string(), summary.to_string()));
    }
    fn tool_end(&mut self, _: &str, _: &ToolOutput) {}
    async fn approve(&mut self, req: &ApprovalRequest<'_>) -> Approval {
        match &self.approver {
            Some(approver) => approver.approve(&self.origin, req).await,
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

    #[test]
    fn general_mode_needs_approval_and_has_write_tools() {
        assert!(!Subagent.needs_approval(&json!({"task": "x"})));
        assert!(Subagent.needs_approval(&json!({"task": "x", "mode": "general"})));
        let names: Vec<String> = child_tools(true)
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
}
