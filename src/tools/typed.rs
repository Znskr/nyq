//! Typed subagent results (design §4.2, §8.1): `subagent` with
//! `output_schema` and/or `validators` gets its child's final message back as
//! JSON, checked by the task engine's validators, and labelled as evidence.
//!
//! TODO(W9): once the `submit` tool and typed task results land, the child
//! should finish by calling `submit` (schema = `output_schema`) and these
//! checks should run as that task's validators. Until then the child's final
//! message is parsed as JSON, and a failing check gets one retry with the
//! problems fed back.

use std::path::Path;
use std::sync::Arc;

use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::task::{
    BuildValidator, CustomCommandValidator, Exec, ExistsValidator, LintValidator,
    SchemaValidator, ShellRunner, TestScope, TestsValidator, ValidateCx, Validator, Verdict,
};

/// Attempts per typed child: the first answer and one retry.
pub(crate) const ATTEMPTS: u32 = 2;
const MAX_VALIDATORS: usize = 8;

/// One entry of `validators`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Check {
    Build,
    Tests,
    Lint,
    /// `cmd:<shell command>`: exit code 0 passes.
    Cmd(String),
    /// `exists:<json pointer>`: the path(s) at that pointer must exist.
    Exists(String),
}

impl Check {
    fn parse(raw: &str) -> Result<Self, String> {
        let raw = raw.trim();
        Ok(match raw {
            "build" => Check::Build,
            "tests" | "test" => Check::Tests,
            "lint" => Check::Lint,
            _ => {
                if let Some(cmd) = raw.strip_prefix("cmd:") {
                    let cmd = cmd.trim();
                    if cmd.is_empty() {
                        return Err("`cmd:` needs a shell command".into());
                    }
                    Check::Cmd(cmd.to_string())
                } else if let Some(ptr) = raw.strip_prefix("exists:") {
                    let ptr = ptr.trim();
                    if !(ptr.is_empty() || ptr.starts_with('/')) {
                        return Err(format!(
                            "`exists:{ptr}` needs a JSON pointer such as `/files`"
                        ));
                    }
                    Check::Exists(ptr.to_string())
                } else {
                    return Err(format!(
                        "unknown validator `{raw}` (build, tests, lint, cmd:<sh>, exists:<pointer>)"
                    ));
                }
            }
        })
    }

    /// Whether it runs a command (and so needs approval like a shell call).
    pub(crate) fn executes(&self) -> bool {
        !matches!(self, Check::Exists(_))
    }

    fn label(&self) -> String {
        match self {
            Check::Build => "build".into(),
            Check::Tests => "tests".into(),
            Check::Lint => "lint".into(),
            Check::Cmd(c) => format!("cmd:{c}"),
            Check::Exists(p) => format!("exists:{p}"),
        }
    }

    fn validator(&self) -> Box<dyn Validator> {
        match self {
            Check::Build => Box::new(BuildValidator),
            Check::Tests => Box::new(TestsValidator::new(TestScope::All)),
            Check::Lint => Box::new(LintValidator),
            Check::Cmd(c) => Box::new(CustomCommandValidator::new(c.clone())),
            Check::Exists(p) => Box::new(ExistsValidator::in_value(p.clone())),
        }
    }
}

/// What a typed child must produce.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Typed {
    pub schema: Option<Value>,
    pub checks: Vec<Check>,
}

/// The outcome of checking one answer.
#[derive(Clone, Debug)]
pub(crate) struct Checked {
    /// The parsed answer (a JSON string when it is not JSON and no schema
    /// asks for JSON).
    pub value: Value,
    /// `{validator, verdict, feedback?}` per check that ran.
    pub checks: Vec<Value>,
    /// Feedback for the child when a check failed.
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
                            .and_then(Check::parse)
                    })
                    .collect::<Result<_, _>>()?
            }
            Some(Value::String(s)) => vec![Check::parse(s)?],
            Some(_) => return Err("`validators` must be a list of strings".into()),
        };
        if schema.is_none() && checks.is_empty() {
            return Ok(None);
        }
        Ok(Some(Self { schema, checks }))
    }

    pub(crate) fn executes(&self) -> bool {
        self.checks.iter().any(Check::executes)
    }

    /// Lines for the approval preview.
    pub(crate) fn describe(&self) -> String {
        let mut out = Vec::new();
        if self.schema.is_some() {
            out.push("output: JSON matching output_schema".to_string());
        }
        if !self.checks.is_empty() {
            let names: Vec<String> = self.checks.iter().map(Check::label).collect();
            out.push(format!("validators: {}", names.join(", ")));
        }
        out.join("\n")
    }

    /// Appended to the child's task so it knows how to finish.
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
            let names: Vec<String> = self.checks.iter().map(Check::label).collect();
            s.push_str(&format!(
                "Your result is then checked by: {}. Make sure these pass before you finish.\n",
                names.join(", ")
            ));
        }
        s
    }

    /// Check one final message. Validators run in order and stop at the
    /// first failure, with commands confined by `policy` in `root`.
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
                checks.push(json!({"validator": "json", "verdict": "fail", "feedback": e}));
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
        let exec = Exec::new(Arc::new(ShellRunner::new(policy.clone()))).with_cancel(cancel.clone());
        let cx = ValidateCx::detect(root, exec);
        let mut validators: Vec<(String, Box<dyn Validator>)> = Vec::new();
        if let Some(schema) = &self.schema {
            validators.push(("schema".into(), Box::new(SchemaValidator::new(schema.clone()))));
        }
        for c in &self.checks {
            validators.push((c.label(), c.validator()));
        }
        for (name, v) in validators {
            let verdict = v.check(&value, &cx).await;
            let (label, feedback) = match &verdict {
                Verdict::Pass => ("pass", None),
                Verdict::Fail { feedback, .. } => ("fail", Some(feedback.clone())),
                Verdict::Escalate(why) => ("escalate", Some(why.clone())),
            };
            let mut entry = json!({"validator": name, "verdict": label});
            if let Some(f) = &feedback {
                entry["feedback"] = Value::String(truncate(f, 4_000));
            }
            checks.push(entry);
            if let Some(f) = feedback {
                return Checked {
                    value,
                    checks,
                    failure: Some(format!(
                        "Your result failed the `{name}` check:\n{}\nFix the problem, then finish again with the complete result.",
                        truncate(&f, 4_000)
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
                Check::Build,
                Check::Tests,
                Check::Lint,
                Check::Cmd("make check".into()),
                Check::Exists("/files".into())
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
    async fn checks_schema_and_exists() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("real.rs"), "").unwrap();
        let t = Typed {
            schema: Some(json!({
                "type": "object",
                "required": ["files"],
                "properties": {"files": {"type": "array", "items": {"type": "string"}}}
            })),
            checks: vec![Check::Exists("/files".into())],
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
        let shape = t.check(r#"{"files":"x"}"#, dir.path(), &policy, &cancel).await;
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
    }
}
