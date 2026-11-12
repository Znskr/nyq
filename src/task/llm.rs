//! The task engine's helper model calls: the ladder's progress check (low
//! tier) and diagnosis (high tier), and the LLM judge validator (high tier).
//!
//! Each is one tool-less request that must answer through a single forced
//! tool, the pattern `crate::classifier` uses for permission decisions. The
//! helpers see a written summary of the evidence (errors, recent actions,
//! file contents or diffs) and cannot run tools themselves, so they are
//! read-only by construction.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};
use similar::{udiff, Algorithm};
use tokio_util::sync::CancellationToken;

use super::validate::{JudgeRunner, JudgeVerdict, SchemaValidator, ValidateCx};
use crate::provider::Provider;
use crate::types::{Item, Request, ToolSpec, Usage};

pub const PROGRESS_TOOL: &str = "progress_verdict";
pub const DIAGNOSE_TOOL: &str = "diagnosis";
pub const JUDGE_TOOL: &str = "judge_verdict";

/// How long one helper call may take.
pub const TIMEOUT: Duration = Duration::from_secs(180);

/// Each error is cut to this much in a helper prompt.
const ERROR_CHARS: usize = 1500;
/// File text and diffs shown to a helper, in total.
const EVIDENCE_CHARS: usize = 48 * 1024;

/// A forced tool call's validated arguments and the request's usage.
#[derive(Debug)]
pub struct Answer {
    pub value: Value,
    pub usage: Option<Usage>,
}

/// One request offering only `spec`; the answer is its single call's
/// arguments, checked against its schema. Prose around the call is ignored.
pub async fn forced_call(
    provider: &dyn Provider,
    system: &str,
    user: &str,
    spec: &ToolSpec,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<Answer, String> {
    if cancel.is_cancelled() {
        return Err("cancelled".into());
    }
    let item = Item::user(user);
    let req = Request {
        system,
        items: std::slice::from_ref(&item),
        tools: std::slice::from_ref(spec),
    };
    let mut sink = |_: crate::types::StreamEvent| {};
    let pending = crate::provider::stream_with_retry(provider, req, &mut sink);
    tokio::pin!(pending);
    let response = tokio::select! {
        biased;
        _ = cancel.cancelled() => return Err("cancelled".into()),
        r = tokio::time::timeout(timeout, &mut pending) => match r {
            Err(_) => return Err("timed out".into()),
            Ok(Err(e)) => return Err(format!("{e:#}").lines().next().unwrap_or("").to_string()),
            Ok(Ok(r)) => r,
        },
    };
    let calls: Vec<&str> = response
        .items
        .iter()
        .filter_map(|i| match i {
            Item::ToolCall {
                name, arguments, ..
            } if *name == spec.name => Some(arguments.as_str()),
            _ => None,
        })
        .collect();
    let [args] = calls.as_slice() else {
        return Err(format!(
            "expected one `{}` call, got {}",
            spec.name,
            calls.len()
        ));
    };
    let value: Value = serde_json::from_str(args)
        .map_err(|e| format!("`{}` arguments are not JSON: {e}", spec.name))?;
    let errs = SchemaValidator::new(spec.parameters.clone()).errors(&value);
    if !errs.is_empty() {
        return Err(format!("`{}` arguments: {}", spec.name, errs.join("; ")));
    }
    Ok(Answer {
        value,
        usage: response.usage,
    })
}

fn clip(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        return s.to_string();
    }
    format!("{}\n… (truncated)", s.chars().take(max).collect::<String>())
}

fn numbered(errors: &[String]) -> String {
    errors
        .iter()
        .enumerate()
        .map(|(i, e)| format!("{}: {}", i + 1, clip(e, ERROR_CHARS)))
        .collect::<Vec<_>>()
        .join("\n\n")
}

// ---------------------------------------------------------------------------
// Progress check (ladder rung 2)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Progress {
    pub made: bool,
    pub why: String,
}

fn progress_spec() -> ToolSpec {
    ToolSpec {
        name: PROGRESS_TOOL.into(),
        description: "Report whether the agent is making progress.".into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "made": {"type": "boolean", "description": "true if the failures show progress."},
                "why": {"type": "string", "description": "One sentence on why."}
            },
            "required": ["made", "why"]
        }),
    }
}

/// promptyped `handleConsecutiveCompileFailures` step 1: do the recent
/// failures show progress (different errors, fewer errors, earlier problems
/// fixed)?
pub async fn progress_check(
    provider: &dyn Provider,
    goal: &str,
    errors: &[String],
    cancel: &CancellationToken,
) -> Result<(Progress, Option<Usage>), String> {
    let system = "You check whether an AI coding agent is making progress on a task that keeps failing. \
        Look only at the failures you are given. Progress means the errors changed in a way that shows earlier \
        problems were fixed: different errors, fewer errors, or a later stage failing. The same error again and again \
        is not progress. Answer only by calling progress_verdict.";
    let user = format!(
        "The task: {goal}\n\nThe last {} failures, oldest first:\n\n{}\n\nIs progress being made? Call {PROGRESS_TOOL}.",
        errors.len(),
        numbered(errors)
    );
    let a = forced_call(provider, system, &user, &progress_spec(), TIMEOUT, cancel).await?;
    Ok((
        Progress {
            made: a.value["made"].as_bool().unwrap_or(false),
            why: a.value["why"].as_str().unwrap_or("").trim().to_string(),
        },
        a.usage,
    ))
}

// ---------------------------------------------------------------------------
// Diagnosis (ladder rung 3)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnosis {
    pub has_solution: bool,
    pub plan: String,
}

fn diagnose_spec() -> ToolSpec {
    ToolSpec {
        name: DIAGNOSE_TOOL.into(),
        description: "Report a plan to get the agent unstuck, or that there is none.".into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "has_solution": {"type": "boolean", "description": "false if you cannot find a likely solution."},
                "plan": {"type": "string", "description": "A specific, actionable plan: which files to change and how."}
            },
            "required": ["has_solution", "plan"]
        }),
    }
}

/// What the diagnosis gets to look at.
pub struct Evidence<'a> {
    pub goal: &'a str,
    pub errors: &'a [String],
    /// Recent tool calls and results, one per line.
    pub actions: &'a str,
    /// (path, current text) of the files the task touched.
    pub files: &'a [(String, String)],
}

/// promptyped `handleConsecutiveCompileFailures` steps 2–3: a high-tier,
/// read-only look at the evidence that proposes a plan.
pub async fn diagnose(
    provider: &dyn Provider,
    ev: &Evidence<'_>,
    cancel: &CancellationToken,
) -> Result<(Diagnosis, Option<Usage>), String> {
    let system = "You are a senior engineer helping an AI coding agent that is stuck: its task keeps failing without \
        progress. You cannot run tools or change anything. Study the evidence and suggest a specific, actionable plan \
        to get past the failure. If you cannot find a likely solution, set has_solution to false. Do not write the full \
        code yourself. Answer only by calling diagnosis.";
    let mut files = String::new();
    let mut budget = EVIDENCE_CHARS;
    for (path, text) in ev.files {
        if budget == 0 {
            files.push_str("\n(more files omitted)");
            break;
        }
        let shown = clip(text, budget.min(16 * 1024));
        budget = budget.saturating_sub(shown.len());
        files.push_str(&format!("\n--- {path}\n{shown}\n"));
    }
    let user = format!(
        "The task: {}\n\nRecent failures, oldest first:\n\n{}\n\nRecent actions, oldest first:\n{}\n\nFiles the agent changed:{}\n\nCall {DIAGNOSE_TOOL}.",
        ev.goal,
        numbered(ev.errors),
        if ev.actions.trim().is_empty() { "(none)" } else { ev.actions },
        if files.is_empty() { " (none)".to_string() } else { files }
    );
    let a = forced_call(provider, system, &user, &diagnose_spec(), TIMEOUT, cancel).await?;
    Ok((
        Diagnosis {
            has_solution: a.value["has_solution"].as_bool().unwrap_or(false),
            plan: a.value["plan"].as_str().unwrap_or("").trim().to_string(),
        },
        a.usage,
    ))
}

// ---------------------------------------------------------------------------
// Judge
// ---------------------------------------------------------------------------

fn judge_spec() -> ToolSpec {
    ToolSpec {
        name: JUDGE_TOOL.into(),
        description: "Report whether the claimed change is present and complete.".into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "complete": {"type": "boolean", "description": "true if the change is present and complete."},
                "feedback": {"type": "string", "description": "When not complete: what is missing, addressed to the agent."}
            },
            "required": ["complete", "feedback"]
        }),
    }
}

/// promptyped `validateFileModifiedWithAi`'s rubric.
const JUDGE_SYSTEM: &str = "You are responsible for checking another AI agent's output. You decide whether the \
change it claims to have made was actually made, from the diffs of the files it touched. Do NOT check correctness: \
an implementation with a small bug or two should still pass, because the build and the tests catch those (but reject \
a change that does something obviously wrong). DO check that the change is really present, not just a comment saying \
it was done. For a refactoring, check that it is done completely, with nothing left to do, not partially. Doing more \
than asked is fine as long as it looks helpful towards the task. Answer only by calling judge_verdict; when the change \
is missing or incomplete, feedback must tell the agent what is missing.";

/// The before → after diff of each touched file. `originals` holds the text
/// before the task first wrote a file (None: it did not exist).
pub fn touched_diff(
    root: &Path,
    touched: &[PathBuf],
    originals: &HashMap<PathBuf, Option<String>>,
) -> String {
    let mut out = String::new();
    for rel in touched {
        if out.len() >= EVIDENCE_CHARS {
            out.push_str("\n(more files omitted)\n");
            break;
        }
        let shown = crate::tools::slash_path(rel);
        let now = std::fs::read_to_string(root.join(rel)).ok();
        let piece = match (originals.get(rel), &now) {
            (Some(before), now) => {
                let (old, new) = (
                    before.as_deref().unwrap_or(""),
                    now.as_deref().unwrap_or(""),
                );
                if old == new {
                    format!("--- {shown}: unchanged\n")
                } else {
                    let (a, b) = (
                        if before.is_some() {
                            format!("a/{shown}")
                        } else {
                            "/dev/null".into()
                        },
                        if now.is_some() {
                            format!("b/{shown}")
                        } else {
                            "/dev/null".into()
                        },
                    );
                    udiff::unified_diff(
                        Algorithm::Myers,
                        old,
                        new,
                        3,
                        Some((a.as_str(), b.as_str())),
                    )
                }
            }
            (None, Some(text)) => format!("--- {shown} (no earlier copy; current text):\n{text}\n"),
            (None, None) => format!("--- {shown}: deleted\n"),
        };
        out.push_str(&piece);
    }
    if out.is_empty() {
        "(no files were changed)".into()
    } else {
        clip(&out, EVIDENCE_CHARS)
    }
}

/// The LLM judge: a high-tier forced call over the goal, the submitted value
/// and the diff of the touched files.
pub struct LlmJudge {
    provider: Arc<dyn Provider>,
    goal: String,
    originals: HashMap<PathBuf, Option<String>>,
    cancel: CancellationToken,
    /// Usage of the judge's requests, for the caller to account.
    pub usage: Arc<Mutex<Vec<Usage>>>,
}

impl LlmJudge {
    pub fn new(
        provider: Box<dyn Provider>,
        goal: impl Into<String>,
        originals: HashMap<PathBuf, Option<String>>,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            provider: Arc::from(provider),
            goal: goal.into(),
            originals,
            cancel,
            usage: Default::default(),
        }
    }
}

#[async_trait]
impl JudgeRunner for LlmJudge {
    async fn judge(
        &self,
        rubric: &str,
        value: &Value,
        cx: &ValidateCx,
    ) -> Result<JudgeVerdict, String> {
        let touched: Vec<PathBuf> = cx
            .touched
            .iter()
            .map(|p| p.strip_prefix(&cx.root).unwrap_or(p).to_path_buf())
            .collect();
        let diff = touched_diff(&cx.root, &touched, &self.originals);
        let user = format!(
            "The task that was supposed to be done:\n{}\n\nWhat to check: {rubric}\n\nThe result the agent submitted:\n{}\n\n\
             The changes it made (diffs of the files it touched):\n{diff}\n\nCall {JUDGE_TOOL}.",
            self.goal.trim(),
            clip(&value.to_string(), 4096),
        );
        let a = forced_call(
            self.provider.as_ref(),
            JUDGE_SYSTEM,
            &user,
            &judge_spec(),
            TIMEOUT,
            &self.cancel,
        )
        .await?;
        if let Some(u) = a.usage {
            self.usage.lock().unwrap_or_else(|e| e.into_inner()).push(u);
        }
        let complete = a.value["complete"].as_bool().unwrap_or(false);
        let mut feedback = a.value["feedback"]
            .as_str()
            .unwrap_or("")
            .trim()
            .to_string();
        if !complete {
            if feedback.is_empty() {
                feedback = "The change is missing or incomplete.".into();
            }
            feedback = format!(
                "A reviewer found that the change you claim to have made is not (fully) there: {feedback}\n\
                 If the change is impossible, add a comment at the relevant place explaining exactly why, and submit again."
            );
        }
        Ok(JudgeVerdict { complete, feedback })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::test_support::{call, reply, ScriptedTurns};
    use crate::task::build::Exec;
    use crate::task::build::FakeRunner;

    fn scripted(responses: Vec<crate::types::Response>) -> ScriptedTurns {
        ScriptedTurns {
            left: Arc::new(Mutex::new(responses)),
        }
    }

    #[tokio::test]
    async fn forced_calls_parse_and_reject_bad_answers() {
        let p = scripted(vec![
            reply(vec![call(
                "1",
                PROGRESS_TOOL,
                json!({"made": true, "why": "fewer errors"}),
            )]),
            reply(vec![Item::Assistant {
                text: "no tool".into(),
            }]),
            reply(vec![call("2", PROGRESS_TOOL, json!({"made": "yes"}))]),
        ]);
        let c = CancellationToken::new();
        let (got, _) = progress_check(&p, "g", &["e1".into(), "e2".into()], &c)
            .await
            .unwrap();
        assert_eq!(
            got,
            Progress {
                made: true,
                why: "fewer errors".into()
            }
        );
        assert!(progress_check(&p, "g", &[], &c)
            .await
            .unwrap_err()
            .contains("expected one"));
        let err = progress_check(&p, "g", &[], &c).await.unwrap_err();
        assert!(
            err.contains("missing field `why`") || err.contains("expected boolean"),
            "{err}"
        );
        c.cancel();
        assert_eq!(
            progress_check(&p, "g", &[], &c).await.unwrap_err(),
            "cancelled"
        );
    }

    #[tokio::test]
    async fn diagnosis_and_judge() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "new\n").unwrap();
        let p = scripted(vec![
            reply(vec![call(
                "1",
                DIAGNOSE_TOOL,
                json!({"has_solution": true, "plan": "import x"}),
            )]),
            reply(vec![call(
                "2",
                JUDGE_TOOL,
                json!({"complete": false, "feedback": "b.txt untouched"}),
            )]),
            reply(vec![call(
                "3",
                JUDGE_TOOL,
                json!({"complete": true, "feedback": ""}),
            )]),
        ]);
        let c = CancellationToken::new();
        let files = [("a.txt".to_string(), "new".to_string())];
        let ev = Evidence {
            goal: "g",
            errors: &["e".into()],
            actions: "",
            files: &files,
        };
        let (d, _) = diagnose(&p, &ev, &c).await.unwrap();
        assert!(d.has_solution && d.plan == "import x");

        let mut originals = HashMap::new();
        originals.insert(PathBuf::from("a.txt"), Some("old\n".to_string()));
        let judge = LlmJudge::new(Box::new(p.fork_scripted()), "change a and b", originals, c);
        let cx = ValidateCx::new(dir.path(), Exec::new(FakeRunner::new()))
            .with_touched(vec![PathBuf::from("a.txt")]);
        let v = judge.judge("both files", &json!({}), &cx).await.unwrap();
        assert!(!v.complete && v.feedback.contains("b.txt untouched"));
        assert!(
            judge
                .judge("both files", &json!({}), &cx)
                .await
                .unwrap()
                .complete
        );
    }

    #[test]
    fn diffs_of_touched_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\n").unwrap();
        std::fs::write(dir.path().join("new.txt"), "fresh\n").unwrap();
        std::fs::write(dir.path().join("same.txt"), "x\n").unwrap();
        let mut originals = HashMap::new();
        originals.insert(PathBuf::from("a.txt"), Some("one\n".to_string()));
        originals.insert(PathBuf::from("new.txt"), None);
        originals.insert(PathBuf::from("same.txt"), Some("x\n".to_string()));
        let touched = ["a.txt", "new.txt", "same.txt", "unknown.txt"].map(PathBuf::from);
        let d = touched_diff(dir.path(), &touched, &originals);
        assert!(d.contains("--- a/a.txt") && d.contains("+two"), "{d}");
        assert!(d.contains("--- /dev/null") && d.contains("+fresh"), "{d}");
        assert!(d.contains("same.txt: unchanged"), "{d}");
        assert!(d.contains("unknown.txt: deleted"), "{d}");
        assert_eq!(
            touched_diff(dir.path(), &[], &originals),
            "(no files were changed)"
        );
    }

    impl ScriptedTurns {
        fn fork_scripted(&self) -> ScriptedTurns {
            ScriptedTurns {
                left: self.left.clone(),
            }
        }
    }
}
