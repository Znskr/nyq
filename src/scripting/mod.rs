//! Programmatic tool calling (design §6.3, §8.3): `run_code` runs a sandboxed
//! JavaScript body with `await tools.<name>(args)`, and `workflow` adds
//! `task()`, `parallel()`, `pipeline()` and `phase()` over typed subagents.
//!
//! The script runs in an embedded QuickJS engine ([`engine`], behind the
//! `scripting` feature) with time and memory limits and no file, network or
//! process access of its own. Every tool call it makes is handed back to the
//! agent loop and goes through the full dispatch pipeline — hooks,
//! permission rules, classifier, approval, sandbox, writer lease, PostToolUse —
//! exactly like a call the model made directly (see
//! `Agent::dispatch_nested`). `Tool::run` is never called from here.
//!
//! Recording: sub-call results go back to the script, not into the
//! transcript. The `run_code`/`workflow` result lists every sub-call (tool,
//! arguments, outcome) after the script's value, so the session file keeps
//! its existing shape and older nyq versions load it unchanged.

// Without the feature only the stub `drive` and `kind_of` are reachable.
#![cfg_attr(not(feature = "scripting"), allow(dead_code))]

#[cfg(feature = "scripting")]
pub(crate) mod engine;

use std::time::Duration;

use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::agent::{Agent, Ui};
use crate::tools::ToolOutput;

pub const RUN_CODE: &str = "run_code";
pub const WORKFLOW: &str = "workflow";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Code,
    Workflow,
}

/// The script tool `name` names, if any. Always None without the feature.
pub fn kind_of(name: &str) -> Option<Kind> {
    if !cfg!(feature = "scripting") {
        return None;
    }
    match name {
        RUN_CODE => Some(Kind::Code),
        WORKFLOW => Some(Kind::Workflow),
        _ => None,
    }
}

#[derive(Clone, Debug)]
pub struct Limits {
    /// Wall-clock budget for the whole script, tool calls included.
    pub timeout: Duration,
    /// QuickJS heap limit in bytes.
    pub memory: usize,
    /// Tool calls one script may make.
    pub max_calls: usize,
}

impl Limits {
    pub fn for_kind(kind: Kind, args: &Value) -> Self {
        let (default, max, calls) = match kind {
            Kind::Code => (120, 900, 200),
            Kind::Workflow => (3600, 4 * 3600, 100),
        };
        let secs = args
            .get("timeout_secs")
            .and_then(Value::as_u64)
            .unwrap_or(default)
            .clamp(1, max);
        Self {
            timeout: Duration::from_secs(secs),
            memory: 64 * 1024 * 1024,
            max_calls: calls,
        }
    }
}

/// Calls dispatched together when the script issues several at once:
/// subagent runs at most three (the child cap), reads a few more.
const MAX_BATCH_TASKS: usize = 3;
const MAX_BATCH_READS: usize = 8;
/// Console output kept in the result.
const MAX_LOG: usize = 8 * 1024;
const MAX_RESULT: usize = 200 * 1024;

/// Tools a script may call: the agent's own, minus the script tools.
pub fn callable(agent: &Agent) -> Vec<String> {
    agent
        .tools
        .specs()
        .into_iter()
        .map(|s| s.name)
        .filter(|n| kind_of(n).is_none() && n != crate::agent::ASK_USER)
        .collect()
}

/// One sub-call, as recorded in the result.
struct SubCall {
    name: String,
    args: String,
    ok: bool,
}

/// Run a `run_code` or `workflow` call to completion. `id` is the parent
/// call's id; sub-calls get `<id>.<n>`.
#[cfg(feature = "scripting")]
pub(crate) async fn drive(
    agent: &mut Agent,
    kind: Kind,
    id: &str,
    args: &Value,
    ui: &mut dyn Ui,
    cancel: &CancellationToken,
) -> ToolOutput {
    use engine::{Event, Reply};
    let field = if kind == Kind::Code { "code" } else { "script" };
    let Some(code) = args
        .get(field)
        .or_else(|| args.get("code"))
        .and_then(Value::as_str)
        .filter(|c| !c.trim().is_empty())
    else {
        return ToolOutput::err(format!("Missing required argument `{field}`."));
    };
    let limits = Limits::for_kind(kind, args);
    let tools = callable(agent);
    if kind == Kind::Workflow && !tools.iter().any(|t| t == "subagent") {
        return ToolOutput::err("workflow needs the subagent tool, which is unavailable here.");
    }
    let mut engine = engine::start(code, kind, tools.clone(), limits.clone());
    // Sub-calls see this token: the deadline or the user's cancel stops them.
    let stop = cancel.child_token();
    let timer = {
        let stop = stop.clone();
        let timeout = limits.timeout;
        tokio::spawn(async move {
            tokio::time::sleep(timeout + Duration::from_secs(2)).await;
            stop.cancel();
        })
    };
    let mut batch: Vec<(u64, String, String)> = Vec::new();
    let mut record: Vec<SubCall> = Vec::new();
    let mut logs = String::new();
    let mut phases: Vec<String> = Vec::new();
    let mut undo = crate::undo::UndoLog::default();
    // Whether any sub-call changed the workspace.
    let mut changed = false;
    let mut images = Vec::new();
    let result = loop {
        let event = tokio::select! {
            ev = engine.events.recv() => ev,
            _ = stop.cancelled() => {
                engine.abort();
                break Err(if cancel.is_cancelled() {
                    "Cancelled by user.".to_string()
                } else {
                    format!("script timed out after {}s and was stopped", limits.timeout.as_secs())
                });
            }
        };
        match event {
            None => break Err("the script engine stopped unexpectedly".into()),
            Some(Event::Call { id: n, name, args }) => batch.push((n, name, args)),
            Some(Event::Log(line)) => {
                if logs.len() < MAX_LOG {
                    logs.push_str(&line);
                    logs.push('\n');
                }
            }
            Some(Event::Phase(name)) => {
                ui.notice(&format!("workflow phase: {name}"));
                phases.push(name);
            }
            Some(Event::Done(r)) => break r,
            Some(Event::Idle) => {
                let pending = std::mem::take(&mut batch);
                for chunk in chunks(pending) {
                    let calls: Vec<(String, String, String)> = chunk
                        .iter()
                        .map(|(n, name, args)| (format!("{id}.{n}"), name.clone(), args.clone()))
                        .collect();
                    // Script tools never nest.
                    let (nested, direct): (Vec<_>, Vec<_>) = calls
                        .iter()
                        .enumerate()
                        .partition(|(_, c)| kind_of(&c.1).is_some());
                    let direct_calls: Vec<_> = direct.iter().map(|(_, c)| (*c).clone()).collect();
                    let mut outs = agent.dispatch_nested(&direct_calls, ui, &stop).await;
                    let mut by_index: Vec<Option<ToolOutput>> = vec![None; calls.len()];
                    for ((i, _), out) in direct.iter().zip(outs.drain(..)) {
                        by_index[*i] = Some(out);
                    }
                    for (i, _) in nested {
                        by_index[i] = Some(ToolOutput::err("Scripts cannot start other scripts."));
                    }
                    for ((n, name, args), out) in chunk.into_iter().zip(by_index) {
                        let mut out =
                            out.unwrap_or_else(|| ToolOutput::err("The call did not run."));
                        // Dispatch already moved each sub-call's undo records
                        // into the session's log; its `changed` flag survives.
                        changed |= out.changed || !out.undo.is_empty();
                        undo.append(std::mem::take(&mut out.undo));
                        images.append(&mut out.images);
                        record.push(SubCall {
                            name,
                            args,
                            ok: !out.is_error,
                        });
                        engine.reply(Reply {
                            id: n,
                            ok: !out.is_error,
                            payload: out.content,
                        });
                    }
                }
            }
        }
    };
    timer.abort();
    let mut out = render(kind, result, &record, &logs, &phases);
    out.undo = undo;
    out.images = images;
    out.changed = changed || !out.undo.is_empty();
    out
}

#[cfg(not(feature = "scripting"))]
pub(crate) async fn drive(
    _agent: &mut Agent,
    _kind: Kind,
    _id: &str,
    _args: &Value,
    _ui: &mut dyn Ui,
    _cancel: &CancellationToken,
) -> ToolOutput {
    ToolOutput::err("This nyq was built without the `scripting` feature.")
}

/// Split one idle point's calls into dispatch batches: consecutive
/// subagent calls three at a time, other calls a few at a time.
fn chunks(calls: Vec<(u64, String, String)>) -> Vec<Vec<(u64, String, String)>> {
    let mut out: Vec<Vec<(u64, String, String)>> = Vec::new();
    for call in calls {
        let task = call.1 == "subagent";
        let cap = if task {
            MAX_BATCH_TASKS
        } else {
            MAX_BATCH_READS
        };
        match out.last_mut() {
            Some(last) if last.len() < cap && (last[0].1 == "subagent") == task => last.push(call),
            _ => out.push(vec![call]),
        }
    }
    out
}

fn render(
    kind: Kind,
    result: Result<String, String>,
    calls: &[SubCall],
    logs: &str,
    phases: &[String],
) -> ToolOutput {
    let failed = calls.iter().filter(|c| !c.ok).count();
    let mut text = match &result {
        Ok(json) => match serde_json::from_str::<Value>(json) {
            Ok(Value::String(s)) => s,
            Ok(v) => serde_json::to_string_pretty(&v).unwrap_or_else(|_| json.clone()),
            Err(_) => json.clone(),
        },
        Err(e) => format!("The script failed: {e}"),
    };
    if text.len() > MAX_RESULT {
        let mut end = MAX_RESULT;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push_str("\n[… result truncated]");
    }
    if !logs.is_empty() {
        text.push_str(&format!("\n\n[console]\n{}", logs.trim_end()));
    }
    if !phases.is_empty() {
        text.push_str(&format!("\n\n[phases] {}", phases.join(" → ")));
    }
    let name = if kind == Kind::Code {
        RUN_CODE
    } else {
        WORKFLOW
    };
    // The sub-call log: plain text in the tool result, so sessions keep
    // their shape (backward compatible) while the history stays auditable.
    let mut log = format!(
        "\n\n[{name}: {} tool call{}{}]",
        calls.len(),
        if calls.len() == 1 { "" } else { "s" },
        if failed > 0 {
            format!(", {failed} failed")
        } else {
            String::new()
        }
    );
    for c in calls.iter().take(50) {
        let args: String = c.args.chars().take(160).collect();
        log.push_str(&format!(
            "\n- {} {} → {}",
            c.name,
            args,
            if c.ok { "ok" } else { "error" }
        ));
    }
    if calls.len() > 50 {
        log.push_str(&format!("\n- … {} more", calls.len() - 50));
    }
    text.push_str(&log);
    let display = calls
        .iter()
        .map(|c| {
            format!(
                "{} {}{}",
                c.name,
                c.args.chars().take(100).collect::<String>(),
                if c.ok { "" } else { " (error)" }
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    ToolOutput {
        content: text,
        is_error: result.is_err(),
        display: (!display.is_empty()).then_some(display),
        ..Default::default()
    }
}

#[cfg(all(test, feature = "scripting"))]
mod tests {
    use super::*;
    use crate::agent::test_support::*;
    use crate::agent::{Approval, ApprovalRequest};
    use crate::types::{Item, StreamEvent};
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// Answers every approval with `answer` and counts the prompts.
    struct Answer(Approval, usize);
    #[async_trait::async_trait]
    impl Ui for Answer {
        fn stream(&mut self, _: &StreamEvent) {}
        fn end_message(&mut self) {}
        fn tool_start(&mut self, _: &str, _: &str) {}
        fn tool_end(&mut self, _: &str, _: &ToolOutput) {}
        async fn approve(&mut self, _: &ApprovalRequest<'_>) -> Approval {
            self.1 += 1;
            self.0
        }
    }

    /// Run one `name` call with `args` and return its result text.
    async fn run_script(
        name: &str,
        args: Value,
        mut agent: crate::agent::Agent,
        ui: &mut dyn Ui,
    ) -> (String, crate::agent::Agent) {
        let mut script = vec![reply(vec![call("s1", name, args)])];
        script.push(reply(vec![Item::Assistant {
            text: "done".into(),
        }]));
        agent.provider = Box::new(ScriptedTurns {
            left: Arc::new(std::sync::Mutex::new(script)),
        });
        agent
            .turn(Item::user("go"), ui, CancellationToken::new())
            .await
            .unwrap();
        let out = results(&agent).first().cloned().unwrap_or_default();
        (out, agent)
    }

    /// A test agent with the built-in tools (the script tools included).
    fn builtin() -> crate::agent::Agent {
        let mut agent = scripted(vec![]);
        agent.tools = crate::tools::Registry::builtin();
        agent
    }

    fn counting_agent(runs: &Arc<AtomicUsize>) -> crate::agent::Agent {
        let mut agent = builtin();
        agent.permission_mode = crate::permissions::Mode::Ask;
        agent.tools.add(Box::new(CountTool(runs.clone())));
        agent
    }

    const ASK: &str = "const a = await tools.mcp__wiki__ask({q: 1}); return {got: a};";

    /// The script's result says it changed the workspace when a sub-call
    /// did, though dispatch already moved the sub-call's undo records into
    /// the session's log (once).
    #[tokio::test]
    async fn a_script_reports_the_changes_its_sub_calls_made() {
        let dir = tempfile::tempdir().unwrap();
        let mut agent = builtin();
        agent.cwd = dir.path().to_path_buf();
        let mut ui = Answer(Approval::Yes, 0);
        let cancel = CancellationToken::new();
        let write =
            json!({"code": "await tools.write_file({path: \"c.txt\", content: \"x\"}); return 1;"});
        let out = drive(&mut agent, Kind::Code, "s1", &write, &mut ui, &cancel).await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.changed, "{}", out.content);
        assert_eq!(agent.undo.len(), 1);
        let read = json!({"code": "return (await tools.read_file({path: \"c.txt\"})).length > 0;"});
        let out = drive(&mut agent, Kind::Code, "s2", &read, &mut ui, &cancel).await;
        assert!(!out.is_error && !out.changed, "{}", out.content);
    }

    #[tokio::test]
    async fn sub_calls_go_through_approval_and_a_denial_is_a_denial() {
        let runs = Arc::new(AtomicUsize::new(0));
        let mut no = Answer(Approval::No, 0);
        let (out, _) = run_script(
            RUN_CODE,
            json!({"code": ASK}),
            counting_agent(&runs),
            &mut no,
        )
        .await;
        assert_eq!(runs.load(Ordering::SeqCst), 0, "{out}");
        assert_eq!(no.1, 1, "the sub-call was put to the user");
        assert!(out.contains("ToolError") && out.contains("denied"), "{out}");
        assert!(out.contains("[run_code: 1 tool call, 1 failed]"), "{out}");

        let mut yes = Answer(Approval::Yes, 0);
        let (out, _) = run_script(
            RUN_CODE,
            json!({"code": ASK}),
            counting_agent(&runs),
            &mut yes,
        )
        .await;
        assert_eq!(runs.load(Ordering::SeqCst), 1, "{out}");
        assert!(out.contains("\"got\": \"answer\""), "{out}");
        assert!(out.contains("- mcp__wiki__ask {\"q\":1} → ok"), "{out}");
    }

    #[tokio::test]
    async fn deny_rules_apply_to_sub_calls() {
        let runs = Arc::new(AtomicUsize::new(0));
        let mut agent = counting_agent(&runs);
        agent.permissions = Arc::new(crate::permissions::Permissions {
            deny: vec!["mcp__wiki__*".into()],
            ..Default::default()
        });
        let mut yes = Answer(Approval::Yes, 0);
        let code = "try { await tools.mcp__wiki__ask({}); return 'ran'; } catch (e) { return 'caught ' + e.name + ': ' + e.message; }";
        let (out, _) = run_script(RUN_CODE, json!({"code": code}), agent, &mut yes).await;
        assert_eq!(runs.load(Ordering::SeqCst), 0);
        assert_eq!(yes.1, 0, "a deny rule never asks");
        assert!(
            out.starts_with("caught ToolError: This call was blocked by permissions.json rule"),
            "{out}"
        );
    }

    #[tokio::test]
    async fn runaway_scripts_time_out() {
        let started = std::time::Instant::now();
        let mut ui = Answer(Approval::Yes, 0);
        let (out, _) = run_script(
            RUN_CODE,
            json!({"code": "while (true) {}", "timeout_secs": 1}),
            builtin(),
            &mut ui,
        )
        .await;
        assert!(out.contains("timed out after 1s"), "{out}");
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        let (out, _) = run_script(
            RUN_CODE,
            json!({"code": "const a = []; while (true) a.push('x'.repeat(1 << 20));"}),
            builtin(),
            &mut ui,
        )
        .await;
        assert!(out.contains("The script failed"), "{out}");
    }

    #[tokio::test]
    async fn javascript_has_no_host_access_of_its_own() {
        let mut ui = Answer(Approval::Yes, 0);
        let code = r#"
            const kinds = [typeof require, typeof fetch, typeof std, typeof os, typeof process, typeof XMLHttpRequest, typeof Deno];
            let imported = "no";
            try { await import("os"); imported = "yes"; } catch (e) { imported = "failed"; }
            console.log("hello", {x: 1});
            const names = Object.keys(tools);
            return {kinds, imported, scripts: names.filter(n => n === "run_code" || n === "workflow"), reads: names.includes("read_file")};
        "#;
        let (out, _) = run_script(RUN_CODE, json!({"code": code}), builtin(), &mut ui).await;
        let value: Value =
            serde_json::from_str(out.split("\n\n[console]").next().unwrap()).unwrap();
        assert!(
            value["kinds"]
                .as_array()
                .unwrap()
                .iter()
                .all(|k| k == "undefined"),
            "{out}"
        );
        assert_eq!(value["imported"], "failed");
        assert_eq!(value["scripts"], json!([]));
        assert_eq!(value["reads"], true);
        assert!(out.contains("[console]\nhello {\"x\":1}"), "{out}");
        let (out, _) = run_script(RUN_CODE, json!({"code": "return ("}), builtin(), &mut ui).await;
        assert!(
            out.starts_with("The script failed") && out.contains("unexpected token"),
            "{out}"
        );
    }

    #[tokio::test]
    async fn workflow_runs_typed_tasks_in_parallel() {
        let mut agent = builtin();
        agent.permission_mode = crate::permissions::Mode::Ask;
        let script = r#"
            phase("count");
            const schema = {type: "object", required: ["n"], properties: {n: {type: "integer"}}};
            const counts = await parallel([1, 2, 3].map(i => () => task("count " + i, {schema})));
            phase("sum");
            const doubled = await pipeline(counts, (c, i) => c.n * 2 + i);
            return {counts, doubled};
        "#;
        // The workflow call, three typed children that each submit (shared
        // script), the final answer.
        let child = reply(vec![call("s", crate::task::SUBMIT, json!({"n": 7}))]);
        agent.provider = Box::new(ScriptedTurns {
            left: Arc::new(std::sync::Mutex::new(vec![
                reply(vec![call("w1", WORKFLOW, json!({"script": script}))]),
                child.clone(),
                child.clone(),
                child,
                reply(vec![Item::Assistant {
                    text: "done".into(),
                }]),
            ])),
        });
        let mut ui = Answer(Approval::Yes, 0);
        agent
            .turn(Item::user("go"), &mut ui, CancellationToken::new())
            .await
            .unwrap();
        let out = results(&agent)[0].clone();
        assert!(
            out.contains("\"doubled\": [\n    14,\n    15,\n    16\n  ]"),
            "{out}"
        );
        assert!(out.contains("[phases] count → sum"), "{out}");
        assert!(out.contains("[workflow: 3 tool calls]"), "{out}");
        assert_eq!(ui.1, 0, "explore children need no approval");
    }

    #[test]
    fn batches_split_tasks_from_reads() {
        let c = |n: u64, name: &str| (n, name.to_string(), String::new());
        let got = chunks(vec![
            c(1, "subagent"),
            c(2, "subagent"),
            c(3, "subagent"),
            c(4, "subagent"),
            c(5, "grep"),
            c(6, "grep"),
        ]);
        let sizes: Vec<usize> = got.iter().map(Vec::len).collect();
        assert_eq!(sizes, [3, 1, 2]);
    }
}
