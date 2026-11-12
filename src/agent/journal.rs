//! Crash-safety records the loop writes through the session: the call journal
//! (`requested → pending → started → settled`, see `crate::session`), failed
//! model attempts, and compaction markers. Each hook is one call from the
//! loop; a failed write is reported and the turn goes on.

use super::Agent;
use crate::session::Session;
use crate::types::StreamEvent;

/// Tools that are not in the static built-in registry but only read. The
/// `skill` tool is registered per session from the skills found, so its
/// declared effects (reads only) can't be looked up by name here.
const SESSION_READS: &[&str] = &["skill"];

/// Whether `name` is a pure read (no side effects; safe to repeat), by its
/// static kind ([`crate::tools::ToolKind::is_pure_read`]): a read, a network
/// read, or session-local state. A call cut off by a crash is then safe to
/// repeat, so it is not journalled. A tool whose effects depend on its
/// arguments (`memory`: `list` only reads) counts as side-effecting.
pub(crate) fn is_pure_read(name: &str) -> bool {
    SESSION_READS.contains(&name)
        || super::builtin_tool(name).is_some_and(|t| t.kind().is_pure_read())
}

fn report(what: &str, result: anyhow::Result<()>) {
    if let Err(e) = result {
        crate::ui::error(&format!("could not save the {what}: {e:#}"));
    }
}

/// From a model stream's sink: a `Retrying` event means the request failed
/// and is sent again, so record the failed attempt.
pub(super) fn note_retry(session: &mut Option<Session>, event: &StreamEvent, partial: &str) {
    if let (Some(session), StreamEvent::Retrying(reason)) = (session.as_mut(), event) {
        report(
            "attempt record",
            session.record_attempt(reason, partial.len()),
        );
    }
}

impl Agent {
    /// Journal the side-effecting calls of a response, `(id, name, arguments)`,
    /// as `pending` once its items are saved, before any is approved or run.
    pub(super) fn journal_requested(&mut self, calls: &[(String, String, String)]) {
        let Some(session) = &mut self.session else {
            return;
        };
        let calls: Vec<(String, String)> = calls
            .iter()
            .filter(|(_, name, _)| !is_pure_read(name))
            .map(|(id, name, _)| (id.clone(), name.clone()))
            .collect();
        report("call journal", session.calls_requested(&calls));
    }

    /// The call is about to be handed to its tool: from here on its side
    /// effects may happen. Syncs the transcript, then marks it `started`.
    pub(super) fn journal_started(&mut self, id: &str, summary: &str) {
        if let Some(session) = &mut self.session {
            report("call journal", session.call_started(id, summary));
        }
    }

    /// A model request failed (after any partial output): keep an audit
    /// record. It never reaches the model.
    pub(super) fn record_failed_attempt<T>(&mut self, result: &anyhow::Result<T>, partial: &str) {
        if let (Some(session), Err(e)) = (&mut self.session, result) {
            report(
                "attempt record",
                session.record_attempt(&format!("{e:#}"), partial.len()),
            );
        }
    }

    /// Mark the start of a compaction (see `Session::compaction_start`).
    pub(super) fn compaction_started(&mut self) {
        if let Some(session) = &mut self.session {
            report("compaction marker", session.compaction_start());
        }
    }

    /// Mark a compaction as done, after its summary and kept tail are saved.
    pub(super) fn compaction_ended(&mut self) {
        if let Some(session) = &mut self.session {
            report("compaction marker", session.compaction_end());
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use anyhow::Result;
    use serde_json::{json, Value};
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::agent::test_support::*;
    use crate::provider::{EventSink, Provider};
    use crate::tools::{Tool, ToolCtx, ToolOutput};
    use crate::types::{Item, Request, Response};

    /// A side-effecting tool that records the call journal as it runs.
    struct Spy(Arc<Mutex<Vec<Value>>>);

    #[async_trait::async_trait]
    impl Tool for Spy {
        fn spec(&self) -> crate::types::ToolSpec {
            crate::types::ToolSpec {
                name: "spy".into(),
                description: String::new(),
                parameters: json!({}),
            }
        }
        fn summary(&self, args: &Value) -> String {
            format!("step {}", args["n"])
        }
        async fn run(&self, _: Value, ctx: &ToolCtx) -> ToolOutput {
            let sessions = ctx.subagent_dir.as_ref().unwrap().parent().unwrap();
            let id = ctx.session_id.as_ref().unwrap();
            let journal = std::fs::read(sessions.join(format!("{id}.calls"))).unwrap();
            self.0
                .lock()
                .unwrap()
                .push(serde_json::from_slice(&journal).unwrap());
            ToolOutput::ok("done")
        }
    }

    #[tokio::test]
    async fn calls_go_pending_started_settled_and_reads_are_not_journalled() {
        let dir = tempfile::tempdir().unwrap();
        let mut agent = scripted(vec![
            reply(vec![
                call("r1", "read_file", json!({"path": "missing.txt"})),
                call("c1", "spy", json!({"n": 1})),
                call("c2", "spy", json!({"n": 2})),
            ]),
            reply(vec![Item::Assistant { text: "ok".into() }]),
        ]);
        agent.cwd = dir.path().to_path_buf();
        let seen = Arc::new(Mutex::new(Vec::new()));
        agent.tools.add(Box::new(Spy(seen.clone())));
        agent.session = Some(
            crate::session::Session::create(&dir.path().join("sessions"), &agent.cwd, "fake")
                .unwrap(),
        );
        agent
            .turn(Item::user("go"), &mut QuietUi, CancellationToken::new())
            .await
            .unwrap();
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        let calls = &seen[0]["calls"];
        assert_eq!(calls["c1"]["state"], "started");
        assert_eq!(calls["c1"]["summary"], "step 1");
        assert_eq!(calls["c2"]["state"], "pending");
        assert!(calls.get("r1").is_none(), "pure reads are not journalled");
        // c1's result settled it before c2 started.
        let calls = &seen[1]["calls"];
        assert!(calls.get("c1").is_none());
        assert_eq!(calls["c2"]["state"], "started");
        let session = agent.session.as_ref().unwrap();
        assert!(!session.path.with_extension("calls").exists());
        assert!(session.open_calls().is_empty());
    }

    #[tokio::test]
    async fn a_failed_request_is_recorded_but_never_sent() {
        struct Fails;
        #[async_trait::async_trait]
        impl Provider for Fails {
            fn name(&self) -> &str {
                "fake"
            }
            fn model(&self) -> &str {
                "fake"
            }
            fn set_model(&mut self, _: String) {}
            fn fork(&self) -> Box<dyn Provider> {
                Box::new(Fails)
            }
            async fn stream(&self, req: Request<'_>, sink: EventSink<'_>) -> Result<Response> {
                assert!(!format!("{:?}", req.items).contains("attempt"));
                sink(StreamEvent::Text("partial".into()));
                anyhow::bail!("HTTP 502 upstream reset")
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let mut agent = scripted(Vec::new());
        agent.provider = Box::new(Fails);
        agent.session =
            Some(crate::session::Session::create(dir.path(), &agent.cwd, "fake").unwrap());
        let err = agent
            .turn(Item::user("go"), &mut QuietUi, CancellationToken::new())
            .await
            .err()
            .unwrap();
        assert!(format!("{err:#}").contains("HTTP 502"));
        let path = agent.session.as_ref().unwrap().path.clone();
        let records = crate::session::meta_records(&path);
        assert_eq!(records.len(), 1, "{records:?}");
        assert_eq!(records[0]["type"], "attempt");
        assert_eq!(records[0]["partial_text_len"], 7);
        assert!(records[0]["error"]
            .as_str()
            .unwrap()
            .contains("HTTP 502 upstream reset"));
        // The transcript has only the prompt; the attempt stays in the sidecar.
        assert_eq!(agent.items.last(), Some(&Item::user("go")));
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("attempt"));
    }

    #[test]
    fn a_retry_is_recorded_as_an_attempt() {
        let dir = tempfile::tempdir().unwrap();
        let mut session =
            Some(crate::session::Session::create(dir.path(), dir.path(), "fake").unwrap());
        session.as_mut().unwrap().append(&Item::user("go")).unwrap();
        note_retry(&mut session, &StreamEvent::Text("x".into()), "");
        note_retry(
            &mut session,
            &StreamEvent::Retrying("Retrying in 1s (1/3) · HTTP 503".into()),
            "",
        );
        note_retry(&mut None, &StreamEvent::Retrying("ignored".into()), "");
        let records = crate::session::meta_records(&session.as_ref().unwrap().path);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0]["partial_text_len"], 0);
        assert!(records[0]["error"].as_str().unwrap().contains("HTTP 503"));
    }

    #[tokio::test]
    async fn compaction_writes_start_and_end_markers() {
        let mut agent = agent(
            Response {
                truncated: false,
                items: vec![Item::Assistant {
                    text: "summary".into(),
                }],
                usage: None,
            },
            None,
        );
        let dir = tempfile::tempdir().unwrap();
        agent.session =
            Some(crate::session::Session::create(dir.path(), &agent.cwd, "fake").unwrap());
        agent.push(Item::Assistant {
            text: "old answer".into(),
        });
        assert!(agent
            .compact(&mut QuietUi, &CancellationToken::new())
            .await
            .unwrap());
        let path = agent.session.as_ref().unwrap().path.clone();
        let kinds: Vec<Value> = crate::session::meta_records(&path)
            .into_iter()
            .map(|r| json!([r["type"], r["outcome"]]))
            .collect();
        assert_eq!(
            kinds,
            vec![
                json!(["compaction_start", null]),
                json!(["compaction_end", "done"])
            ]
        );
    }
    /// A patch is journalled before it runs, so recovery never calls a
    /// half-applied one "safe to repeat"; a read is not.
    #[test]
    fn apply_patch_is_journalled() {
        let dir = tempfile::tempdir().unwrap();
        let mut agent = scripted(Vec::new());
        agent.cwd = dir.path().to_path_buf();
        agent.session = Some(
            crate::session::Session::create(&dir.path().join("sessions"), &agent.cwd, "fake")
                .unwrap(),
        );
        let patch = json!({"patch": "--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-a\n+b\n"});
        agent.journal_requested(&[
            ("p1".into(), "apply_patch".into(), patch.to_string()),
            (
                "r1".into(),
                "read_file".into(),
                json!({"path": "a.txt"}).to_string(),
            ),
        ]);
        let path = agent.session.as_ref().unwrap().path.with_extension("calls");
        let journal: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(journal["calls"]["p1"]["state"], "pending");
        assert!(journal["calls"].get("r1").is_none());
    }

    #[test]
    fn pure_reads_come_from_declared_effects() {
        for name in [
            "read_file",
            "list_dir",
            "glob",
            "grep",
            "view_image",
            "web_fetch",
            "web_search",
            crate::tools::artifacts::TOOL,
            crate::tools::session_search::TOOL,
            "skill",
            "team_task_list",
        ] {
            assert!(is_pure_read(name), "{name}");
        }
        for name in [
            "write_file",
            "edit_file",
            "multi_edit",
            "apply_patch",
            "bash",
            "memory",
            "subagent",
            "team_task_claim",
            "mcp__x__y",
        ] {
            assert!(!is_pure_read(name), "{name}");
        }
    }
}
