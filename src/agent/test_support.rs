//! Fakes and builders shared by the agent tests.

use std::collections::HashSet;
use std::path::PathBuf;

use anyhow::Result;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::compact::COMPACT_PROMPT;
use super::{Agent, Approval, ApprovalRequest, Ui};
use crate::provider::{EventSink, Provider};
use crate::tools::{Registry, ToolCtx, ToolOutput};
use crate::types::{Item, Request, Response, StreamEvent, Usage};

pub(crate) struct ScriptedTurns {
    pub(crate) left: std::sync::Arc<std::sync::Mutex<Vec<crate::types::Response>>>,
}

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

/// An agent with the built-in tools, in ask mode, that never talks to a model.
pub(crate) fn test_agent() -> Agent {
    Agent {
        provider: Box::new(ScriptedTurns {
            left: Default::default(),
        }),
        tools: Registry::builtin(),
        system: String::new(),
        items: Vec::new(),
        session: None,
        usage_session_id: None,
        cwd: PathBuf::from("."),
        permission_mode: crate::permissions::Mode::Ask,
        review_cache: Default::default(),
        allowed: HashSet::new(),
        permissions: Default::default(),
        usage: Usage::default(),
        stats: Default::default(),
        context_window: 1000,
        context_tokens: 0,
        context_estimated: false,
        compaction: Default::default(),
        submitted: 0,
        hooks: Default::default(),
        sandbox: Default::default(),
        undo: Default::default(),
        taint: Default::default(),
    }
}

pub(crate) struct SummaryProvider {
    pub(crate) response: Response,
    pub(crate) cancel: Option<CancellationToken>,
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

pub(crate) fn agent(response: Response, cancel: Option<CancellationToken>) -> Agent {
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
        compaction: Default::default(),
        submitted: 1,
        hooks: Default::default(),
        sandbox: Default::default(),
        undo: Default::default(),
        taint: Default::default(),
    }
}

pub(crate) struct NoticeCatch(pub(crate) Vec<String>);
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

pub(crate) struct QuietUi;
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

/// An MCP-like tool that asks by default and counts its runs.
pub(crate) struct CountTool(pub(crate) std::sync::Arc<std::sync::atomic::AtomicUsize>);
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

pub(crate) fn calling_agent(
    runs: &std::sync::Arc<std::sync::atomic::AtomicUsize>,
    calls: usize,
) -> Agent {
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

pub(crate) fn results(agent: &Agent) -> Vec<String> {
    agent
        .items
        .iter()
        .filter_map(|i| match i {
            Item::ToolResult { output, .. } => Some(output.clone()),
            _ => None,
        })
        .collect()
}

/// A hooks configuration with one user hook, for the wiring tests.
#[cfg(unix)]
pub(crate) fn hooks_with(
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

pub(crate) fn call(id: &str, name: &str, arguments: Value) -> Item {
    Item::ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: arguments.to_string(),
    }
}

pub(crate) fn reply(items: Vec<Item>) -> Response {
    Response {
        truncated: false,
        items,
        usage: None,
    }
}

pub(crate) fn scripted(script: Vec<Response>) -> Agent {
    let mut agent = agent(Response::default(), None);
    agent.context_window = 0;
    agent.provider = Box::new(ScriptedTurns {
        left: std::sync::Arc::new(std::sync::Mutex::new(script)),
    });
    agent
}

/// A tool under any name that sleeps, tracks how many runs overlap, and
/// answers with its `n` argument (or fails when `fail` is set). Its
/// `effects` function decides the wave class the scheduler gives it.
pub(crate) struct Probe {
    pub(crate) name: &'static str,
    pub(crate) live: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    pub(crate) peak: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    pub(crate) fail: bool,
    pub(crate) delay_ms: u64,
    pub(crate) effects: fn(&Value) -> crate::tools::Effects,
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
    fn effects(&self, args: &Value) -> crate::tools::Effects {
        (self.effects)(args)
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

/// Effects for a read-only probe.
pub(crate) fn read_fx(_: &Value) -> crate::tools::Effects {
    crate::tools::Effects::reads_only()
}

/// Effects for a probe that writes its `path` argument.
pub(crate) fn write_fx(args: &Value) -> crate::tools::Effects {
    crate::tools::Effects::writes_to(vec![PathBuf::from(
        args.get("path").and_then(Value::as_str).unwrap_or_default(),
    )])
}

/// Effects for a probe that runs a process.
pub(crate) fn exec_fx(_: &Value) -> crate::tools::Effects {
    crate::tools::Effects {
        reads: false,
        writes: crate::tools::Writes::Unknown,
        exec: true,
        net: false,
    }
}

/// Effects for an undeclared (barrier) probe.
pub(crate) fn unknown_fx(_: &Value) -> crate::tools::Effects {
    crate::tools::Effects::default()
}

pub(crate) fn probe(
    name: &'static str,
    effects: fn(&Value) -> crate::tools::Effects,
    fail: bool,
) -> (Probe, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    let peak = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let tool = Probe {
        name,
        live: Default::default(),
        peak: peak.clone(),
        fail,
        delay_ms: if name == "grep" { 100 } else { 1 },
        effects,
    };
    (tool, peak)
}
