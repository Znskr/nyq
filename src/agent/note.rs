//! The per-request trailing note: session state appended to the newest
//! item instead of the system prompt, so the cached prefix holds.

use super::{Agent, ASK_USER};
use crate::types::Item;

/// The transcript as sent: `note` appended to the newest item when it is a
/// user message or tool result. Earlier items are unchanged, so the cached prefix holds;
/// the stored transcript never contains the note.
pub(super) fn with_trailing_note<'a>(
    items: &'a [Item],
    note: &str,
) -> std::borrow::Cow<'a, [Item]> {
    // Only on the newest item: a note on an older one would change the
    // prefix the provider already cached (e.g. when continuing a reply).
    let last = items.len().wrapping_sub(1);
    if note.is_empty()
        || !matches!(
            items.last(),
            Some(Item::User { .. } | Item::ToolResult { .. })
        )
    {
        return std::borrow::Cow::Borrowed(items);
    }
    let at = last;
    let mut out = items.to_vec();
    match &mut out[at] {
        Item::User { text, .. } => {
            text.push_str("\n\n");
            text.push_str(note);
        }
        Item::ToolResult { output, .. } => {
            output.push_str("\n\n");
            output.push_str(note);
        }
        _ => {}
    }
    std::borrow::Cow::Owned(out)
}

impl Agent {
    /// The trailing note for the next request. Each section appears once,
    /// in this order: the task and its budget, the latest verify state, the
    /// working set, the recent intents, then the session state. It is
    /// send-only (see [`with_trailing_note`]): nothing here is stored.
    pub(super) fn trailing_note(&self) -> String {
        let (task, verify) = self.task_note_sections();
        let (workset, intents) = self.workset_sections();
        let session = self.session_notes(!workset.is_empty());
        [task, verify, workset, intents, session]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    /// The working set, rendered from disk now (never stored, so the cached
    /// prefix stays stable), and the newest intents from the session's one
    /// intent store.
    fn workset_sections(&self) -> (String, String) {
        let settings = crate::settings::current();
        let ws = crate::tools::workset::lock(&self.workset);
        let files = if settings.workset.enabled {
            ws.render(&crate::tools::workset::Budgets::from_settings(
                self.context_window,
            ))
        } else {
            String::new()
        };
        (files, ws.render_intents(settings.intents.show))
    }

    /// Per-request notes on the session's current state: permission mode,
    /// question UI, background processes. They go at the end of the request,
    /// not in the system prompt, so the cached prompt prefix survives a
    /// change.
    fn session_notes(&self, workset_shown: bool) -> String {
        let mode = self.permission_mode;
        let mut notes = vec![format!(
            "- Permission mode: {} ({}). A call the user or a rule denies must not be retried as is.",
            mode.as_str(),
            mode.summary()
        )];
        // The root agent of a session without ask_user_question: print mode
        // or an editor. Subagents have their own instructions.
        if self.usage_session_id.is_none() && self.tools.get(ASK_USER).is_none() {
            notes.push(
                "- There is no question UI: do not stop mid-task to wait for answers. If you are blocked, \
                 finish with a short description of the blocker and the options."
                    .into(),
            );
        }
        let procs = crate::tools::background::context_lines();
        if !procs.is_empty() {
            notes.push(format!(
                "- Background processes (reuse a matching running server instead of starting a duplicate; \
                 ended ones are no longer running):\n{}",
                procs.join("\n")
            ));
        }
        if workset_shown {
            notes.push(
                "- File contents in the working set are read from disk at request time and \
                 are the current state; earlier `read_file` output for these paths may be stale."
                    .into(),
            );
        }
        format!("[Session state]\n{}", notes.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use crate::agent::test_support::*;
    use crate::agent::AskUserTool;
    use crate::types::Response;

    #[test]
    fn session_notes_describe_mode_and_question_ui() {
        let mut agent = agent(Response::default(), None);
        agent.permission_mode = crate::permissions::Mode::Ask;
        let notes = agent.trailing_note();
        assert!(notes.contains("Permission mode: ask"), "{notes}");
        assert!(notes.contains("no question UI"), "{notes}");
        agent.tools.add(Box::new(AskUserTool));
        assert!(!agent.trailing_note().contains("no question UI"));
    }

    #[test]
    fn note_renders_workset_from_disk_and_keeps_the_transcript() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.rs"), "fn a() { v1 }\n").unwrap();
        let mut agent = agent(Response::default(), None);
        agent.cwd = tmp.path().to_path_buf();
        {
            let mut ws = crate::tools::workset::lock(&agent.workset);
            ws.open(
                tmp.path(),
                tmp.path().join("a.rs"),
                crate::tools::workset::Mode::Focused,
            );
            ws.record_intent("opened a.rs", "to edit it", "make the change");
        }
        let stored = agent.items.clone();
        let sent = super::with_trailing_note(&agent.items, &agent.trailing_note());
        // The stored transcript is untouched; the note is send-only.
        assert_eq!(agent.items, stored);
        let crate::types::Item::User { text, .. } = &sent[sent.len() - 1] else {
            panic!("last item should be a user message");
        };
        assert!(text.contains("[Working set]"), "{text}");
        assert!(text.contains("a.rs (focused)"), "{text}");
        assert!(text.contains("fn a() { v1 }"), "{text}");
        assert!(text.contains("[Recent intents]"), "{text}");
        assert!(text.contains("did: opened a.rs"), "{text}");
        assert!(text.contains("read from disk at request time"), "{text}");
        assert!(!serde_json::to_string(&stored)
            .unwrap()
            .contains("[Working set]"));

        // A later disk write shows up in the next note without any read call.
        std::fs::write(tmp.path().join("a.rs"), "fn a() { v2 }\n").unwrap();
        let note = agent.trailing_note();
        assert!(note.contains("fn a() { v2 }"), "{note}");
        assert!(!note.contains("v1"), "{note}");
    }
}
