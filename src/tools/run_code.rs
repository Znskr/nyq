//! `run_code` and `workflow`: their specs. The agent loop runs them itself
//! (`crate::scripting::drive`) so every sub-call goes through its dispatch;
//! `run` here only answers a caller outside that loop.

use serde_json::{json, Value};

use super::{Registry, Tool, ToolCtx, ToolOutput};
use crate::scripting::{RUN_CODE, WORKFLOW};
use crate::types::ToolSpec;

pub fn register(r: &mut Registry) {
    r.add(Box::new(Script(crate::scripting::Kind::Code)));
    r.add(Box::new(Script(crate::scripting::Kind::Workflow)));
}

struct Script(crate::scripting::Kind);

const RUN_CODE_DESCRIPTION: &str = "Run a short JavaScript program that calls your tools, to batch many calls without a model round trip each (e.g. grep 40 files and summarize). `code` is the body of an async function: use `await tools.<name>(args)` (tool names as you know them; each resolves to the tool's text result and rejects with a ToolError when the call fails or is denied) and `return` a JSON-serializable value. Every call goes through the normal permission checks and approvals. There is no file, network or process access from JavaScript itself, no modules, and console.log output is returned. Limits: timeout_secs (default 120), 64 MB heap, 200 tool calls.";

const WORKFLOW_DESCRIPTION: &str = "Run a multi-stage JavaScript program over subagents. `script` is the body of an async function with: `await task(goal, {schema, validators, mode, isolation, name, description})` (runs a subagent; with schema/validators it resolves to the checked JSON result, otherwise the report text; rejects when the child fails), `await parallel([() => task(...), ...])`, `await pipeline(items, (item, i) => ...)` (both run at most 3 at a time, results in order), `phase(name)` (marks progress), plus `tools.<name>(args)` as in run_code. Return a JSON-serializable value. Every call goes through the normal permission checks. Limits: timeout_secs (default 3600), 100 tool calls.";

#[async_trait::async_trait]
impl Tool for Script {
    fn spec(&self) -> ToolSpec {
        let (name, description, field, field_desc) = match self.0 {
            crate::scripting::Kind::Code => (
                RUN_CODE,
                RUN_CODE_DESCRIPTION,
                "code",
                "Body of an async function; `return` the result.",
            ),
            crate::scripting::Kind::Workflow => (
                WORKFLOW,
                WORKFLOW_DESCRIPTION,
                "script",
                "Body of an async function using task/parallel/pipeline/phase; `return` the result.",
            ),
        };
        ToolSpec {
            name: name.into(),
            description: description.into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    field: {"type": "string", "description": field_desc},
                    "description": {"type": "string", "description": "Short label shown to the user."},
                    "timeout_secs": {"type": "integer", "minimum": 1, "description": "Wall-clock limit for the whole program, tool calls included."}
                },
                "required": [field]
            }),
        }
    }

    fn summary(&self, args: &Value) -> String {
        if let Some(d) = args
            .get("description")
            .and_then(Value::as_str)
            .filter(|d| !d.trim().is_empty())
        {
            return d.trim().to_string();
        }
        let field = if self.0 == crate::scripting::Kind::Code {
            "code"
        } else {
            "script"
        };
        let code = args.get(field).and_then(Value::as_str).unwrap_or("");
        let line = code.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
        let line: String = line.trim().chars().take(60).collect();
        format!("{} lines · {line}", code.lines().count())
    }

    fn preview(&self, args: &Value, _: &ToolCtx) -> Option<String> {
        args.get("code")
            .or_else(|| args.get("script"))
            .and_then(Value::as_str)
            .map(str::to_string)
    }

    fn handles_cancel(&self) -> bool {
        true
    }

    /// Each sub-call goes through dispatch and takes the lease itself.
    fn coordinates(&self) -> bool {
        true
    }

    async fn run(&self, _: Value, _: &ToolCtx) -> ToolOutput {
        ToolOutput::err(
            "Scripts run only inside the agent loop, which dispatches their tool calls.",
        )
    }
}
