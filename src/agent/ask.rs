//! `ask_user_question`: a tool the loop answers through `Ui::ask`.

use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::Ui;
use crate::tools::{ToolCtx, ToolOutput};

/// `ask_user_question`: the loop answers it through `Ui::ask` instead of
/// running it, so it is only offered when a person is at the terminal.
pub struct AskUserTool;

pub const ASK_USER: &str = "ask_user_question";

#[async_trait::async_trait]
impl crate::tools::Tool for AskUserTool {
    fn spec(&self) -> crate::types::ToolSpec {
        crate::types::ToolSpec {
            name: ASK_USER.into(),
            description: "Ask the user up to 4 multiple-choice questions when you need decisions or missing \
                information that you cannot reasonably infer. Give each question 2-6 short, distinct options, \
                each with a one-line description of what choosing it means; the user may also type their own \
                answer or skip. Do not use it for confirmation of routine steps."
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "questions": {
                        "type": "array",
                        "description": "1-4 questions, asked in order.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "question": {"type": "string", "description": "The question, one or two sentences."},
                                "options": {
                                    "type": "array",
                                    "description": "2-6 answer choices.",
                                    "items": {
                                        "type": "object",
                                        "properties": {
                                            "label": {"type": "string", "description": "The choice, a few words."},
                                            "description": {"type": "string", "description": "What choosing it means."}
                                        },
                                        "required": ["label"]
                                    }
                                }
                            },
                            "required": ["question", "options"]
                        }
                    }
                },
                "required": ["questions"]
            }),
        }
    }
    fn summary(&self, args: &Value) -> String {
        questions(args)
            .into_iter()
            .next()
            .map(|(q, _)| q)
            .unwrap_or_default()
    }
    async fn run(&self, _args: Value, _ctx: &ToolCtx) -> ToolOutput {
        ToolOutput::err("ask_user_question is answered by the user interface")
    }
}

/// The questions in an `ask_user_question` call: (question, [(label, description)]).
/// The older single `question` with plain-string options is accepted too.
pub(super) fn questions(args: &Value) -> Vec<(String, Vec<(String, String)>)> {
    let option = |o: &Value| match o {
        Value::String(s) => Some((s.clone(), String::new())),
        _ => Some((
            o.get("label")?.as_str()?.to_string(),
            o.get("description")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        )),
    };
    let one = |q: &Value| {
        let text = q.get("question")?.as_str()?.trim().to_string();
        let options = q
            .get("options")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(option).collect())
            .unwrap_or_default();
        (!text.is_empty()).then_some((text, options))
    };
    match args.get("questions").and_then(Value::as_array) {
        Some(list) => list.iter().filter_map(one).take(MAX_QUESTIONS).collect(),
        None => one(args).into_iter().collect(),
    }
}

const MAX_QUESTIONS: usize = 4;

/// Put each question to the user and report the answers.
pub(super) async fn ask_user(
    args: &Value,
    ui: &mut dyn Ui,
    cancel: &CancellationToken,
) -> ToolOutput {
    let asked = questions(args);
    if asked.is_empty() {
        return ToolOutput::err(
            "ask_user_question: give `questions`, each with a `question` and `options`.",
        );
    }
    let mut answers = Vec::new();
    for (question, options) in &asked {
        let shown: Vec<String> = options
            .iter()
            .map(|(label, about)| {
                if about.is_empty() {
                    label.clone()
                } else {
                    format!("{label} — {about}")
                }
            })
            .collect();
        let answer = tokio::select! {
            a = ui.ask(question, &shown) => a,
            _ = cancel.cancelled() => return ToolOutput::err("Cancelled by user."),
        };
        // A picked option is reported by its label.
        let answer = answer.map(|a| {
            shown
                .iter()
                .position(|s| *s == a)
                .map_or(a, |i| options[i].0.clone())
        });
        answers.push((question, answer));
    }
    if answers.iter().all(|(_, a)| a.is_none()) {
        return ToolOutput::ok(
            "The user skipped the question. Use your best judgment or ask in your reply.",
        );
    }
    if let [(_, Some(a))] = answers.as_slice() {
        return ToolOutput::ok(format!("The user answered: {a}"));
    }
    let lines: Vec<String> = answers
        .iter()
        .map(|(q, a)| format!("- {q} → {}", a.as_deref().unwrap_or("(skipped)")))
        .collect();
    ToolOutput::ok(format!("The user answered:\n{}", lines.join("\n")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{Approval, ApprovalRequest};
    use crate::types::StreamEvent;

    #[tokio::test]
    async fn several_questions_with_described_options() {
        struct Answers(Vec<Option<String>>, Vec<Vec<String>>);
        #[async_trait::async_trait]
        impl Ui for Answers {
            fn stream(&mut self, _: &StreamEvent) {}
            fn end_message(&mut self) {}
            fn tool_start(&mut self, _: &str, _: &str) {}
            fn tool_end(&mut self, _: &str, _: &ToolOutput) {}
            async fn approve(&mut self, _: &ApprovalRequest<'_>) -> Approval {
                Approval::Yes
            }
            async fn ask(&mut self, _: &str, options: &[String]) -> Option<String> {
                self.1.push(options.to_vec());
                self.0.remove(0)
            }
        }
        let args = serde_json::json!({"questions": [
            {"question": "Which DB?", "options": [{"label": "SQLite", "description": "simplest, one file"}, {"label": "Postgres"}]},
            {"question": "Add tests?", "options": [{"label": "Yes"}, {"label": "No"}]}
        ]});
        let mut ui = Answers(
            vec![Some("SQLite — simplest, one file".into()), None],
            Vec::new(),
        );
        let out = ask_user(&args, &mut ui, &CancellationToken::new()).await;
        assert_eq!(
            out.content,
            "The user answered:\n- Which DB? → SQLite\n- Add tests? → (skipped)"
        );
        assert_eq!(ui.1[0], ["SQLite — simplest, one file", "Postgres"]);
        let legacy = serde_json::json!({"question": "Go?", "options": ["yes", "no"]});
        let mut ui = Answers(vec![Some("typed answer".into())], Vec::new());
        let out = ask_user(&legacy, &mut ui, &CancellationToken::new()).await;
        assert_eq!(out.content, "The user answered: typed answer");
        assert!(
            ask_user(&serde_json::json!({}), &mut ui, &CancellationToken::new())
                .await
                .is_error
        );
    }
}
