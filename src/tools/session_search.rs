//! `session_search`: find text in this conversation's saved transcript,
//! including what compaction summarized away, and optionally in the other
//! sessions of the same workspace and in stored large tool results. Read-only
//! and safe to run in parallel with other reads.

use std::path::{Path, PathBuf};

use regex::{Regex, RegexBuilder};
use serde_json::{json, Value};

use super::{Registry, Tool, ToolCtx, ToolOutput};
use crate::types::{Item, ToolSpec};

pub(crate) const TOOL: &str = "session_search";
const DEFAULT_MATCHES: usize = 50;
const MAX_MATCHES: usize = 200;
/// Longest matching line shown, in characters.
const LINE_MAX: usize = 240;
/// Most other sessions searched with `all_sessions`.
const MAX_SESSIONS: usize = 50;
const OUTPUT_MAX: usize = 48 * 1024;

/// What to search: the sessions directory and the current session's id.
struct Scope {
    sessions: PathBuf,
    current: String,
}

impl Scope {
    fn of(ctx: &ToolCtx) -> Option<Self> {
        let current = ctx.session_id.clone().filter(|id| !id.is_empty())?;
        // `subagent_dir` is `<sessions>/<id>.agents` when a session is open.
        let sessions = ctx
            .subagent_dir
            .as_deref()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .unwrap_or_else(|| crate::config::home().join("sessions"));
        Some(Self { sessions, current })
    }

    fn transcript(&self, id: &str) -> PathBuf {
        self.sessions.join(format!("{id}.jsonl"))
    }

    /// `<home>/artifacts/<session>`, where spilled tool results live.
    fn artifacts(&self, id: &str) -> Option<PathBuf> {
        let home = self.sessions.parent()?;
        Some(
            home.join("artifacts")
                .join(super::artifacts::session_key(Some(id))),
        )
    }
}

struct Matcher(Regex);

impl Matcher {
    fn new(pattern: &str, regex: bool, case_sensitive: bool) -> Result<Self, String> {
        let source = if regex {
            pattern.to_string()
        } else {
            regex::escape(pattern)
        };
        RegexBuilder::new(&source)
            .case_insensitive(!case_sensitive)
            .size_limit(1 << 20)
            .build()
            .map(Self)
            .map_err(|e| format!("{TOOL}: invalid regex: {e}"))
    }
}

/// Collected hits, capped.
struct Hits {
    out: String,
    count: usize,
    limit: usize,
    truncated: bool,
}

impl Hits {
    fn full(&self) -> bool {
        self.count >= self.limit || self.out.len() >= OUTPUT_MAX
    }

    /// Add each line of `text` that matches, labelled `where_`.
    fn scan(&mut self, m: &Matcher, where_: &str, text: &str) {
        for (n, line) in text.lines().enumerate() {
            if !m.0.is_match(line) {
                continue;
            }
            if self.full() {
                self.truncated = true;
                return;
            }
            let line = crate::tools::redact::scrub(line);
            let line = line.trim();
            let shown: String = line.chars().take(LINE_MAX).collect();
            let more = if shown.len() < line.len() { "…" } else { "" };
            self.out
                .push_str(&format!("{where_}:{}: {shown}{more}\n", n + 1));
            self.count += 1;
        }
    }
}

/// The searchable text of an item and a label for it.
fn item_text(item: &Item) -> Option<(String, &str)> {
    Some(match item {
        Item::User { text, .. } => ("user".into(), text.as_str()),
        Item::Assistant { text } => ("assistant".into(), text.as_str()),
        Item::Reasoning { summary, .. } if !summary.is_empty() => {
            ("reasoning".into(), summary.as_str())
        }
        Item::ToolCall {
            name, arguments, ..
        } => (format!("call {name}"), arguments.as_str()),
        Item::ToolResult { id, output, .. } => (format!("result {id}"), output.as_str()),
        Item::Compaction { summary } => ("summary".into(), summary.as_str()),
        Item::Reasoning { .. } => return None,
    })
}

fn search_transcript(m: &Matcher, path: &Path, label: &str, hits: &mut Hits) -> bool {
    let Ok(items) = crate::session::transcript_items(path) else {
        return false;
    };
    for (i, item) in items.iter().enumerate() {
        if hits.truncated {
            break;
        }
        if let Some((kind, text)) = item_text(item) {
            hits.scan(m, &format!("{label}#{} {kind}", i + 1), text);
        }
    }
    true
}

fn search_artifacts(m: &Matcher, dir: &Path, label: &str, hits: &mut Hits) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "txt"))
        .collect();
    files.sort();
    for path in files {
        if hits.truncated {
            break;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let id = path.file_stem().unwrap_or_default().to_string_lossy();
        hits.scan(m, &format!("{label} stored {id}"), &text);
    }
}

fn short(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

struct SessionSearch;

#[async_trait::async_trait]
impl Tool for SessionSearch {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: TOOL.into(),
            description: "Search this conversation's saved transcript, including parts that were compacted into a \
                summary: user and assistant messages, tool calls (arguments) and tool results. Matches are \
                literal and case-insensitive unless `regex` or `case_sensitive` is set. With `all_sessions`, \
                also searches other saved sessions in this workspace; with `stored_results`, large tool \
                results that were stored in full (read them with read_tool_result). Each hit is \
                `<session>#<item> <kind>:<line>: text`."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "Text to find (a regular expression with `regex`)."},
                    "regex": {"type": "boolean", "description": "Treat `pattern` as a regular expression (default false)."},
                    "case_sensitive": {"type": "boolean", "description": "Match case exactly (default false)."},
                    "all_sessions": {"type": "boolean", "description": "Also search other saved sessions in this workspace (default false)."},
                    "stored_results": {"type": "boolean", "description": "Also search stored large tool results (default true)."},
                    "max_results": {"type": "integer", "description": "Most matching lines to return (default 50, max 200)."}
                },
                "required": ["pattern"]
            }),
        }
    }

    fn summary(&self, args: &Value) -> String {
        let pattern = args.get("pattern").and_then(Value::as_str).unwrap_or("");
        let all = args.get("all_sessions").and_then(Value::as_bool) == Some(true);
        format!("\"{pattern}\"{}", if all { " (all sessions)" } else { "" })
    }

    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolOutput {
        let Some(pattern) = args
            .get("pattern")
            .and_then(Value::as_str)
            .filter(|p| !p.is_empty())
        else {
            return ToolOutput::err(format!("{TOOL}: missing required argument `pattern`."));
        };
        let flag = |k: &str, default: bool| args.get(k).and_then(Value::as_bool).unwrap_or(default);
        let matcher =
            match Matcher::new(pattern, flag("regex", false), flag("case_sensitive", false)) {
                Ok(m) => m,
                Err(e) => return ToolOutput::err(e),
            };
        let Some(scope) = Scope::of(ctx) else {
            return ToolOutput::err(format!(
                "{TOOL}: this conversation is not saved, so there is nothing to search."
            ));
        };
        let limit = args
            .get("max_results")
            .and_then(Value::as_u64)
            .map_or(DEFAULT_MATCHES, |n| (n as usize).clamp(1, MAX_MATCHES));
        let stored = flag("stored_results", true);
        let all = flag("all_sessions", false);
        let cwd = ctx.cwd.clone();
        let task = tokio::task::spawn_blocking(move || {
            let mut hits = Hits {
                out: String::new(),
                count: 0,
                limit,
                truncated: false,
            };
            let mut ids = vec![scope.current.clone()];
            if all {
                if let Ok(list) = crate::session::list(&scope.sessions) {
                    ids.extend(
                        list.into_iter()
                            .filter(|m| {
                                m.id != scope.current && crate::session::same_cwd(&m.cwd, &cwd)
                            })
                            .take(MAX_SESSIONS)
                            .map(|m| m.id),
                    );
                }
            }
            let mut searched = 0;
            for id in &ids {
                if hits.truncated {
                    break;
                }
                let label = if *id == scope.current {
                    "this".to_string()
                } else {
                    short(id).to_string()
                };
                if search_transcript(&matcher, &scope.transcript(id), &label, &mut hits) {
                    searched += 1;
                }
                if stored {
                    if let Some(dir) = scope.artifacts(id) {
                        search_artifacts(&matcher, &dir, &label, &mut hits);
                    }
                }
            }
            (hits, searched)
        });
        let (hits, searched) = tokio::select! {
            r = task => match r {
                Ok(r) => r,
                Err(e) => return ToolOutput::err(format!("{TOOL}: {e}")),
            },
            _ = ctx.cancel.cancelled() => return ToolOutput::err("Cancelled by user."),
        };
        if searched == 0 {
            return ToolOutput::err(format!(
                "{TOOL}: this conversation has no saved transcript yet."
            ));
        }
        let scope = if searched == 1 {
            "this session".to_string()
        } else {
            format!("{searched} sessions")
        };
        if hits.count == 0 {
            return ToolOutput::ok(format!("No matches for {pattern:?} in {scope}."));
        }
        let mut out = format!(
            "{} match(es) for {pattern:?} in {scope}:\n{}",
            hits.count, hits.out
        );
        if hits.truncated {
            out.push_str("[more matches not shown; narrow the pattern or raise max_results]\n");
        }
        ToolOutput::ok(out.trim_end().to_string())
    }
}

pub fn register(r: &mut Registry) {
    r.add(Box::new(SessionSearch));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::Session;

    fn ctx_for(sessions: &Path, id: &str, cwd: &Path) -> ToolCtx {
        ToolCtx {
            cwd: cwd.to_path_buf(),
            session_id: Some(id.to_string()),
            subagent_dir: Some(sessions.join(format!("{id}.agents"))),
            ..Default::default()
        }
    }

    fn saved(sessions: &Path, cwd: &Path, items: &[Item]) -> String {
        let mut s = Session::create(sessions, cwd, "m").unwrap();
        for item in items {
            s.append(item).unwrap();
        }
        s.id.clone()
    }

    #[tokio::test]
    async fn finds_text_before_and_after_compaction() {
        let home = tempfile::tempdir().unwrap();
        let sessions = home.path().join("sessions");
        let cwd = home.path().join("w");
        let id = saved(
            &sessions,
            &cwd,
            &[
                Item::user("the deploy key is in vault/alpha"),
                Item::ToolCall {
                    id: "c1".into(),
                    name: "bash".into(),
                    arguments: r#"{"command":"make Alpha"}"#.into(),
                },
                Item::ToolResult {
                    id: "c1".into(),
                    output: "built alpha\nok".into(),
                    is_error: false,
                },
                Item::Compaction {
                    summary: "worked on beta".into(),
                },
                Item::Assistant {
                    text: "alpha again".into(),
                },
            ],
        );
        let tool = SessionSearch;
        let out = tool
            .run(json!({"pattern": "alpha"}), &ctx_for(&sessions, &id, &cwd))
            .await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.starts_with("4 match(es)"), "{}", out.content);
        assert!(out.content.contains("this#1 user:1:"));
        assert!(out.content.contains("this#2 call bash:1:"));
        assert!(out.content.contains("this#3 result c1:1: built alpha"));
        assert!(out.content.contains("this#5 assistant:1: alpha again"));

        let out = tool
            .run(
                json!({"pattern": "Alpha", "case_sensitive": true}),
                &ctx_for(&sessions, &id, &cwd),
            )
            .await;
        assert!(out.content.starts_with("1 match(es)"), "{}", out.content);

        let out = tool
            .run(
                json!({"pattern": "^(built|worked) .*a$", "regex": true}),
                &ctx_for(&sessions, &id, &cwd),
            )
            .await;
        assert!(out.content.starts_with("2 match(es)"), "{}", out.content);
        assert!(out.content.contains("this#4 summary:1: worked on beta"));

        let out = tool
            .run(
                json!({"pattern": "alpha", "max_results": 1}),
                &ctx_for(&sessions, &id, &cwd),
            )
            .await;
        assert!(out.content.starts_with("1 match(es)"), "{}", out.content);
        assert!(out.content.contains("more matches not shown"));

        let out = tool
            .run(
                json!({"pattern": "(", "regex": true}),
                &ctx_for(&sessions, &id, &cwd),
            )
            .await;
        assert!(out.is_error && out.content.contains("invalid regex"));
        let out = tool
            .run(json!({"pattern": "zzz"}), &ctx_for(&sessions, &id, &cwd))
            .await;
        assert!(out.content.starts_with("No matches"));
    }

    #[tokio::test]
    async fn other_sessions_and_stored_results_on_request() {
        let home = tempfile::tempdir().unwrap();
        let sessions = home.path().join("sessions");
        let cwd = home.path().join("w");
        let other = saved(&sessions, &cwd, &[Item::user("needle in the old one")]);
        let elsewhere = saved(
            &sessions,
            &home.path().join("x"),
            &[Item::user("needle elsewhere")],
        );
        let id = saved(&sessions, &cwd, &[Item::user("nothing here")]);
        let key = crate::tools::artifacts::session_key(Some(&id));
        let stored = home.path().join("artifacts").join(&key);
        std::fs::create_dir_all(&stored).unwrap();
        std::fs::write(stored.join("bash-abc.txt"), "line\nthe needle\n").unwrap();

        let tool = SessionSearch;
        let ctx = ctx_for(&sessions, &id, &cwd);
        let out = tool.run(json!({"pattern": "needle"}), &ctx).await;
        assert!(
            out.content.contains("this stored bash-abc:2: the needle"),
            "{}",
            out.content
        );
        assert!(!out.content.contains("old one"));

        let out = tool
            .run(
                json!({"pattern": "needle", "all_sessions": true, "stored_results": false}),
                &ctx,
            )
            .await;
        assert!(
            out.content
                .contains(&format!("{}#1 user:1: needle in the old one", &other[..8])),
            "{}",
            out.content
        );
        assert!(!out.content.contains(&elsewhere[..8]));
        assert!(!out.content.contains("stored"));

        let mut unsaved = ctx_for(&sessions, &id, &cwd);
        unsaved.session_id = None;
        assert!(tool.run(json!({"pattern": "x"}), &unsaved).await.is_error);
    }
}
