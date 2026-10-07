//! `subagent`: delegate a self-contained task to a fresh agent with its own
//! context; only its final report comes back to the parent model.

use std::collections::HashSet;
use std::time::Duration;

use serde_json::{json, Value};

use super::{Registry, Tool, ToolCtx, ToolOutput};
use crate::agent::{Agent, Approval, ApprovalRequest, TurnEnd, Ui};
use crate::types::{Item, StreamEvent, ToolSpec, Usage};

const NAME: &str = "subagent";
const TIMEOUT: Duration = Duration::from_secs(15 * 60);
const MAX_REPORT: usize = 20_000;
/// Tools an explore subagent may use (those present in the builtins).
const READ_ONLY: &[&str] = &[
    "read_file", "list_dir", "glob", "grep", "view_image", "web_fetch", "web_search",
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

/// The child's tools: builtins (minus mutating ones for explore), never itself.
fn child_tools(general: bool) -> Registry {
    let mut tools = Registry::builtin();
    let names: Vec<String> = tools.specs().into_iter().map(|s| s.name).collect();
    let keep: Vec<&str> = if general {
        names.iter().map(String::as_str).filter(|n| *n != NAME).collect()
    } else {
        READ_ONLY.iter().copied().filter(|n| names.iter().any(|m| m == n)).collect()
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
        let label = args
            .get("description")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .or_else(|| args.get("task").and_then(Value::as_str).and_then(|t| t.lines().next()))
            .unwrap_or("")
            .trim();
        if is_general(args) { format!("{label} (general)") } else { label.to_string() }
    }

    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolOutput {
        let Some(task) = args.get("task").and_then(Value::as_str).filter(|t| !t.trim().is_empty()) else {
            return ToolOutput::err("Missing required argument `task`.");
        };
        let Some(provider) = &ctx.provider else {
            return ToolOutput::err("Subagents are unavailable: no model provider in this context.");
        };
        let provider = provider.fork();
        let context_window = crate::config::context_window_for(provider.name(), None);
        let mut agent = Agent {
            provider,
            tools: child_tools(is_general(&args)),
            system: format!("{}\n\n{PREAMBLE}", crate::prompt::system(&ctx.cwd)),
            items: Vec::new(),
            session: None,
            cwd: ctx.cwd.clone(),
            // Explore tools need no approval; for general mode the parent's
            // approval of this call covers the subagent's actions.
            yolo: true,
            allowed: HashSet::new(),
            usage: Usage::default(),
            context_window,
            context_tokens: 0,
            sandbox: ctx.sandbox.clone(),
            undo: Default::default(),
        };
        let mut ui = Collector::default();
        let cancel = ctx.cancel.child_token();
        let result = tokio::time::timeout(TIMEOUT, agent.turn(Item::user(task), &mut ui, cancel)).await;
        let display = Some(ui.display());
        let fail = |msg: String| ToolOutput { display: display.clone(), ..ToolOutput::err(msg) };
        match result {
            Err(_) => return fail(format!("Subagent timed out after {} minutes.", TIMEOUT.as_secs() / 60)),
            Ok(Err(e)) => return fail(format!("Subagent failed: {e:#}")),
            Ok(Ok(TurnEnd::Cancelled)) => return fail("Subagent cancelled.".into()),
            Ok(Ok(TurnEnd::Done)) => {}
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
        ToolOutput { display, ..ToolOutput::ok(content) }
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
    format!("{}\n[… report truncated, {} more bytes]", &s[..end], s.len() - end)
}

/// Headless UI for the child agent: records tool calls, approves everything.
#[derive(Default)]
struct Collector {
    calls: Vec<(String, String)>,
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
    async fn approve(&mut self, _: &ApprovalRequest<'_>) -> Approval {
        Approval::Yes
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
            if self.hang {
                std::future::pending::<()>().await;
            }
            let mut script = self.script.lock().unwrap();
            anyhow::ensure!(!script.is_empty(), "script exhausted");
            Ok(script.remove(0))
        }
    }

    fn resp(items: Vec<Item>) -> Response {
        Response { items, usage: Some(Usage { input_tokens: 100, output_tokens: 10, cached_tokens: 0 }) }
    }

    fn ctx(fake: &Fake, dir: &std::path::Path) -> ToolCtx {
        ToolCtx { cwd: dir.to_path_buf(), provider: Some(Arc::new(fake.clone())), ..ToolCtx::default() }
    }

    #[tokio::test]
    async fn subagent_runs_tools_and_returns_report() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello").unwrap();
        let fake = Fake::default();
        *fake.script.lock().unwrap() = vec![
            resp(vec![Item::ToolCall { id: "1".into(), name: "read_file".into(), arguments: r#"{"path":"a.txt"}"#.into() }]),
            resp(vec![Item::Assistant { text: "Found hello in a.txt:1".into() }]),
        ];
        let out = Subagent.run(json!({"task": "look at a.txt", "description": "read a"}), &ctx(&fake, dir.path())).await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.starts_with("Found hello in a.txt:1"));
        assert!(out.content.contains("(subagent used 1 tool calls, ↑200 ↓20 tokens)"), "{}", out.content);
        assert!(out.display.unwrap().contains("read_file a.txt"));

        let seen = fake.seen_tools.lock().unwrap();
        let tools = &seen[0];
        assert!(tools.contains(&"read_file".to_string()) && tools.contains(&"grep".to_string()));
        for banned in ["write_file", "edit_file", "bash", "subagent"] {
            assert!(!tools.contains(&banned.to_string()), "explore offered {banned}");
        }
    }

    #[test]
    fn general_mode_needs_approval_and_has_write_tools() {
        assert!(!Subagent.needs_approval(&json!({"task": "x"})));
        assert!(Subagent.needs_approval(&json!({"task": "x", "mode": "general"})));
        let names: Vec<String> = child_tools(true).specs().into_iter().map(|s| s.name).collect();
        assert!(names.contains(&"bash".to_string()) && names.contains(&"write_file".to_string()));
        assert!(!names.contains(&"subagent".to_string()));
        assert_eq!(Subagent.summary(&json!({"task": "first\nsecond"})), "first");
    }

    #[tokio::test]
    async fn subagent_without_provider_errors() {
        let out = Subagent.run(json!({"task": "x"}), &ToolCtx::default()).await;
        assert!(out.is_error);
    }

    #[tokio::test]
    async fn subagent_honors_cancellation() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake { hang: true, ..Fake::default() };
        let ctx = ctx(&fake, dir.path());
        let cancel = ctx.cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            cancel.cancel();
        });
        let out = tokio::time::timeout(Duration::from_secs(5), Subagent.run(json!({"task": "x"}), &ctx))
            .await
            .expect("subagent did not stop on cancel");
        assert!(out.is_error && out.content.contains("cancelled"));
    }
}
