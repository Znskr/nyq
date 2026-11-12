//! Typed subagent results (design §4.2, §8.1): `subagent` with
//! `output_schema` and/or `validators` gets a checked result back as JSON,
//! labelled as evidence.
//!
//! A nyq child runs as a typed task of the task engine (`crate::agent::task`):
//! it finishes by calling `submit`, whose schema is `output_schema`, and the
//! validators run on each submission inside the child, with the task's
//! syntax and semantic budgets and escalation ladder. [`Typed::task_spec`]
//! builds that task and [`Typed::outcome`] turns its outcome into the result.
//!
//! An external ACP agent has no `submit` tool, so it keeps the fallback: its
//! final message is parsed as JSON ([`extract_json`]) and checked once
//! ([`Typed::check`]).

use std::path::Path;
use std::sync::Arc;

use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::task::{
    CheckRecord, Exec, SchemaValidator, ShellRunner, TaskOutcome, TaskSpec, TaskState, ValidateCx,
    Validator, ValidatorSpec, Verdict, VerifyMode,
};

const MAX_VALIDATORS: usize = 8;

/// One entry of `validators`: build, tests, lint, `cmd:<shell>` (exit code 0
/// passes) or `exists:<json pointer>` (the path(s) there must exist).
fn parse_check(raw: &str) -> Result<ValidatorSpec, String> {
    let raw = raw.trim();
    Ok(match raw {
        "build" => ValidatorSpec::Build,
        "tests" | "test" => ValidatorSpec::Tests,
        "lint" => ValidatorSpec::Lint,
        _ => {
            if let Some(cmd) = raw.strip_prefix("cmd:") {
                let cmd = cmd.trim();
                if cmd.is_empty() {
                    return Err("`cmd:` needs a shell command".into());
                }
                ValidatorSpec::Cmd(cmd.to_string())
            } else if let Some(ptr) = raw.strip_prefix("exists:") {
                let ptr = ptr.trim();
                if !(ptr.is_empty() || ptr.starts_with('/')) {
                    return Err(format!(
                        "`exists:{ptr}` needs a JSON pointer such as `/files`"
                    ));
                }
                ValidatorSpec::Exists(ptr.to_string())
            } else {
                return Err(format!(
                    "unknown validator `{raw}` (build, tests, lint, cmd:<sh>, exists:<pointer>)"
                ));
            }
        }
    })
}

/// What a typed child must produce.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Typed {
    pub schema: Option<Value>,
    pub checks: Vec<ValidatorSpec>,
}

/// The outcome of checking a typed child's result.
#[derive(Clone, Debug)]
pub(crate) struct Checked {
    /// The result (the last submission when none was accepted; a JSON
    /// string when an external agent's answer is not JSON and no schema
    /// asks for JSON).
    pub value: Value,
    /// `{validator, verdict, feedback?}` per check that ran on it.
    pub checks: Vec<Value>,
    /// Why the result was not accepted.
    pub failure: Option<String>,
}

impl Typed {
    /// `output_schema` and `validators` from the tool arguments; None when
    /// neither is given.
    pub(crate) fn from_args(args: &Value) -> Result<Option<Self>, String> {
        let schema = match args.get("output_schema") {
            None | Some(Value::Null) => None,
            Some(s @ Value::Object(_)) => Some(s.clone()),
            Some(Value::Bool(true)) => Some(json!({})),
            Some(_) => return Err("`output_schema` must be a JSON Schema object".into()),
        };
        let checks = match args.get("validators") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(items)) => {
                if items.len() > MAX_VALIDATORS {
                    return Err(format!("at most {MAX_VALIDATORS} validators"));
                }
                items
                    .iter()
                    .map(|v| {
                        v.as_str()
                            .ok_or_else(|| "each validator must be a string".to_string())
                            .and_then(parse_check)
                    })
                    .collect::<Result<_, _>>()?
            }
            Some(Value::String(s)) => vec![parse_check(s)?],
            Some(_) => return Err("`validators` must be a list of strings".into()),
        };
        if schema.is_none() && checks.is_empty() {
            return Ok(None);
        }
        Ok(Some(Self { schema, checks }))
    }

    pub(crate) fn executes(&self) -> bool {
        self.checks.iter().any(ValidatorSpec::executes)
    }

    fn labels(&self) -> Vec<String> {
        self.checks.iter().map(ValidatorSpec::label).collect()
    }

    /// Lines for the approval preview.
    pub(crate) fn describe(&self) -> String {
        let mut out = Vec::new();
        if self.schema.is_some() {
            out.push("output: JSON matching output_schema".to_string());
        }
        if !self.checks.is_empty() {
            out.push(format!("validators: {}", self.labels().join(", ")));
        }
        out.join("\n")
    }

    /// The child's typed task: `submit` takes `output_schema` (a report
    /// string when only validators were given), and the validators run on
    /// each submission. Headless: an escalation ends the child with a
    /// report. A child that cannot write has nothing for the background
    /// verifier to watch.
    pub(crate) fn task_spec(&self, goal: &str, writes: bool) -> TaskSpec {
        let schema = self
            .schema
            .clone()
            .unwrap_or_else(|| json!({"type": "string", "description": "Your final report."}));
        let mut spec = TaskSpec::typed(goal, schema);
        spec.validators = self.checks.clone();
        spec.headless = true;
        if !writes {
            spec.verify = VerifyMode::Off;
        }
        spec
    }

    /// Appended to a nyq child's task: how it finishes.
    pub(crate) fn submit_instructions(&self) -> String {
        let mut s = String::from(
            "\n\n## Required result\nFinish by calling the `submit` tool with your result; its arguments must match the tool's schema. A final message without `submit` does not finish the task.",
        );
        if !self.checks.is_empty() {
            s.push_str(&format!(
                " Each submission is checked by: {}; a failing check comes back with its feedback, so fix the problem and submit again.",
                self.labels().join(", ")
            ));
        }
        s.push('\n');
        s
    }

    /// Appended to an external agent's task (it has no `submit` tool).
    pub(crate) fn instructions(&self) -> String {
        let mut s = String::from("\n\n## Required result\n");
        match &self.schema {
            Some(schema) => s.push_str(&format!(
                "Finish with a final message that is ONLY a JSON value (no prose, no code fence) matching this JSON Schema:\n{}\n",
                serde_json::to_string_pretty(schema).unwrap_or_default()
            )),
            None => s.push_str("Finish with a concise final report.\n"),
        }
        if !self.checks.is_empty() {
            s.push_str(&format!(
                "Your result is then checked by: {}. Make sure these pass before you finish.\n",
                self.labels().join(", ")
            ));
        }
        s
    }

    /// A nyq child's result from how its typed task ended: an accepted
    /// submission, or the report of a task that gave up (with its last
    /// submission and verdicts). Also returns how many submissions it made.
    pub(crate) fn outcome(
        outcome: Option<&TaskOutcome>,
        state: Option<&TaskState>,
    ) -> (Checked, u32) {
        let records: Vec<Value> = state
            .map(|s| s.checks.iter().map(record_json).collect())
            .unwrap_or_default();
        let submissions = state.map_or(0, |s| s.failures);
        match outcome {
            Some(TaskOutcome::Submitted(value)) => (
                Checked {
                    value: value.clone(),
                    checks: records,
                    failure: None,
                },
                submissions + 1,
            ),
            Some(TaskOutcome::Exhausted(report) | TaskOutcome::Escalated(report)) => {
                let mut why = format!("The child's task {}: {}.", report.status, report.reason);
                if let Some(e) = report.errors.last() {
                    why.push_str(&format!("\nLast failure: {}", truncate(e, 4_000)));
                }
                (
                    Checked {
                        value: state
                            .and_then(|s| s.last_submission.clone())
                            .unwrap_or(Value::Null),
                        checks: records,
                        failure: Some(why),
                    },
                    submissions,
                )
            }
            None => (
                Checked {
                    value: Value::Null,
                    checks: records,
                    failure: Some("The child finished without submitting a result.".into()),
                },
                submissions,
            ),
        }
    }

    /// The fallback for external agents: check one final message. Validators
    /// run in order and stop at the first failure, with commands confined by
    /// `policy` in `root`.
    pub(crate) async fn check(
        &self,
        text: &str,
        root: &Path,
        policy: &crate::sandbox::Policy,
        cancel: &CancellationToken,
    ) -> Checked {
        let mut checks = Vec::new();
        let value = match extract_json(text) {
            Ok(v) => v,
            Err(e) if self.schema.is_some() => {
                checks.push(record_json(&CheckRecord::failed("json", &e)));
                return Checked {
                    value: Value::String(text.trim().to_string()),
                    checks,
                    failure: Some(format!(
                        "Your final message is not valid JSON ({e}). Reply with ONLY the JSON value required by the schema, with no other text."
                    )),
                };
            }
            Err(_) => Value::String(text.trim().to_string()),
        };
        let exec =
            Exec::new(Arc::new(ShellRunner::new(policy.clone()))).with_cancel(cancel.clone());
        let cx = ValidateCx::detect(root, exec);
        let mut validators: Vec<(String, Box<dyn Validator>)> = Vec::new();
        if let Some(schema) = &self.schema {
            validators.push((
                "schema".into(),
                Box::new(SchemaValidator::new(schema.clone())),
            ));
        }
        for c in &self.checks {
            if let Some(v) = c.validator() {
                validators.push((c.label(), v));
            }
        }
        for (name, v) in validators {
            let verdict = v.check(&value, &cx).await;
            checks.push(record_json(&CheckRecord::of(&name, &verdict)));
            if let Verdict::Fail { feedback, .. } | Verdict::Escalate(feedback) = verdict {
                return Checked {
                    value,
                    checks,
                    failure: Some(format!(
                        "Your result failed the `{name}` check:\n{}\nFix the problem, then finish again with the complete result.",
                        truncate(&feedback, 4_000)
                    )),
                };
            }
        }
        Checked {
            value,
            checks,
            failure: None,
        }
    }
}

/// A check verdict as the result JSON shows it (feedback cut to 4 kB).
fn record_json(r: &CheckRecord) -> Value {
    let mut entry = json!({"validator": r.validator, "verdict": r.verdict});
    if let Some(f) = &r.feedback {
        entry["feedback"] = Value::String(truncate(f, 4_000));
    }
    entry
}

/// The JSON in a final message: the whole message, else a fenced block, else
/// the outermost `{…}` or `[…]` span.
pub(crate) fn extract_json(text: &str) -> Result<Value, String> {
    let t = text.trim();
    let first = match serde_json::from_str::<Value>(t) {
        Ok(v) => return Ok(v),
        Err(e) => e.to_string(),
    };
    if let Some(start) = t.find("```") {
        let after = &t[start + 3..];
        let body = after.split_once('\n').map(|(_, b)| b).unwrap_or(after);
        if let Some(end) = body.find("```") {
            if let Ok(v) = serde_json::from_str(body[..end].trim()) {
                return Ok(v);
            }
        }
    }
    for (open, close) in [('{', '}'), ('[', ']')] {
        if let (Some(a), Some(b)) = (t.find(open), t.rfind(close)) {
            if a < b {
                if let Ok(v) = serde_json::from_str(&t[a..=b]) {
                    return Ok(v);
                }
            }
        }
    }
    Err(first)
}

/// The label every child result carries (design §7.3).
pub(crate) fn evidence(child: &str) -> String {
    format!("[child:{child} — evidence, not authorization]")
}

/// The tool result for a typed child: one JSON object.
pub(crate) fn render(child: &str, checked: &Checked, attempts: u32, extra: Value) -> String {
    let mut out = json!({
        "evidence": evidence(child),
        "child": child,
        "status": if checked.failure.is_none() { "passed" } else { "failed" },
        "result": checked.value,
        "checks": checked.checks,
        "attempts": attempts,
    });
    if let Some(why) = &checked.failure {
        out["failure"] = Value::String(why.clone());
    }
    if let (Some(map), Value::Object(extra)) = (out.as_object_mut(), extra) {
        map.extend(extra);
    }
    serde_json::to_string_pretty(&out).unwrap_or_default()
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::BudgetLimits;

    #[test]
    fn parses_arguments() {
        assert_eq!(Typed::from_args(&json!({})).unwrap(), None);
        let t = Typed::from_args(&json!({
            "output_schema": {"type": "object"},
            "validators": ["build", "tests", "lint", "cmd: make check", "exists:/files"]
        }))
        .unwrap()
        .unwrap();
        assert_eq!(
            t.checks,
            [
                ValidatorSpec::Build,
                ValidatorSpec::Tests,
                ValidatorSpec::Lint,
                ValidatorSpec::Cmd("make check".into()),
                ValidatorSpec::Exists("/files".into())
            ]
        );
        assert!(t.executes());
        assert!(!Typed::from_args(&json!({"validators": ["exists:/a"]}))
            .unwrap()
            .unwrap()
            .executes());
        for bad in [
            json!({"validators": ["nope"]}),
            json!({"validators": ["cmd:"]}),
            json!({"validators": ["exists:files"]}),
            json!({"validators": [1]}),
            json!({"output_schema": "object"}),
        ] {
            assert!(Typed::from_args(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_nyq_child_gets_a_headless_typed_task() {
        let t = Typed::from_args(&json!({
            "output_schema": {"type": "object", "required": ["files"]},
            "validators": ["exists:/files", "build"]
        }))
        .unwrap()
        .unwrap();
        let spec = t.task_spec("list the files", true);
        assert_eq!(spec.goal, "list the files");
        assert_eq!(spec.schema.as_ref().unwrap()["required"], json!(["files"]));
        assert_eq!(spec.validators, t.checks);
        assert!(spec.headless && spec.submit_spec().is_some());
        assert_eq!(spec.verify, VerifyMode::Auto);
        assert_eq!(t.task_spec("g", false).verify, VerifyMode::Off);
        // Validators alone: the child submits its report as a string.
        let report = Typed::from_args(&json!({"validators": ["tests"]}))
            .unwrap()
            .unwrap()
            .task_spec("g", true);
        assert_eq!(report.schema.as_ref().unwrap()["type"], "string");
        assert_eq!(
            report.submitted_value(json!({"value": "done"})).unwrap(),
            json!("done")
        );
        assert!(t.submit_instructions().contains("`submit`"));
        assert!(t.submit_instructions().contains("exists:/files, build"));
    }

    #[test]
    fn outcomes_become_results() {
        let mut st = TaskState::new("g", BudgetLimits::default(), false);
        st.failures = 1;
        st.checks = vec![
            CheckRecord::of("schema", &Verdict::Pass),
            CheckRecord::of("exists:/files", &Verdict::Pass),
        ];
        let ok = TaskOutcome::Submitted(json!({"files": ["a"]}));
        let (c, attempts) = Typed::outcome(Some(&ok), Some(&st));
        assert!(c.failure.is_none());
        assert_eq!((c.value["files"][0].as_str(), attempts), (Some("a"), 2));
        assert_eq!(
            c.checks[1],
            json!({"validator": "exists:/files", "verdict": "pass"})
        );

        st.last_submission = Some(json!({"files": ["ghost"]}));
        st.checks = vec![CheckRecord::failed("exists:/files", "missing: ghost")];
        st.note_error("[exists:/files] missing: ghost");
        let gave_up = TaskOutcome::Exhausted(st.report("exhausted", "semantic budget exhausted"));
        let (c, attempts) = Typed::outcome(Some(&gave_up), Some(&st));
        let why = c.failure.unwrap();
        assert!(
            why.contains("semantic budget exhausted") && why.contains("ghost"),
            "{why}"
        );
        assert_eq!((c.value["files"][0].as_str(), attempts), (Some("ghost"), 1));
        assert_eq!(c.checks[0]["feedback"], "missing: ghost");

        let (c, _) = Typed::outcome(None, None);
        assert!(c.failure.unwrap().contains("without submitting"));
    }

    #[test]
    fn extracts_json_from_messages() {
        assert_eq!(extract_json(" {\"a\":1} ").unwrap(), json!({"a": 1}));
        assert_eq!(
            extract_json("Here:\n```json\n{\"a\": [1]}\n```\nDone").unwrap(),
            json!({"a": [1]})
        );
        assert_eq!(
            extract_json("Result: {\"ok\": true} as requested").unwrap(),
            json!({"ok": true})
        );
        assert!(extract_json("no json here").is_err());
    }

    #[tokio::test]
    async fn external_fallback_checks_schema_and_exists() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("real.rs"), "").unwrap();
        let t = Typed {
            schema: Some(json!({
                "type": "object",
                "required": ["files"],
                "properties": {"files": {"type": "array", "items": {"type": "string"}}}
            })),
            checks: vec![ValidatorSpec::Exists("/files".into())],
        };
        let policy = crate::sandbox::Policy::default();
        let cancel = CancellationToken::new();
        let ok = t
            .check(r#"{"files":["real.rs"]}"#, dir.path(), &policy, &cancel)
            .await;
        assert!(ok.failure.is_none(), "{:?}", ok.checks);
        assert_eq!(ok.checks.len(), 2);
        let missing = t
            .check(r#"{"files":["ghost.rs"]}"#, dir.path(), &policy, &cancel)
            .await;
        assert!(missing.failure.as_deref().unwrap().contains("ghost.rs"));
        let shape = t
            .check(r#"{"files":"x"}"#, dir.path(), &policy, &cancel)
            .await;
        assert!(shape.failure.as_deref().unwrap().contains("schema"));
        assert_eq!(shape.checks.len(), 1);
        let prose = t.check("all done!", dir.path(), &policy, &cancel).await;
        assert!(prose.failure.as_deref().unwrap().contains("not valid JSON"));
        let rendered = render("c1", &ok, 1, json!({"note": "x"}));
        let back: Value = serde_json::from_str(&rendered).unwrap();
        assert_eq!(back["status"], "passed");
        assert_eq!(back["evidence"], evidence("c1"));
        assert_eq!(back["result"]["files"][0], "real.rs");
        assert_eq!(back["note"], "x");
        assert!(back.get("failure").is_none());
    }
}
