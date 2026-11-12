//! The core loop: send the transcript, stream the reply, run any tool calls,
//! feed results back, repeat until the model answers without tools.

mod ask;
mod compact;
mod dispatch;
mod note;
mod permission_rules;
mod step;
mod task;
#[cfg(test)]
pub(crate) mod test_support;
mod ui;

// Every item that was public in the old single-file `agent.rs` keeps its
// `crate::agent::X` path. Some are only used by tests or by other
// workstreams' code, hence the `allow`.
pub use ask::{AskUserTool, ASK_USER};
pub use compact::set_auto_compact_percent;
#[allow(unused_imports)]
pub use permission_rules::{
    bash_matches, bash_prefix, grant_rule, grant_rule_in, is_allowed, is_allowed_in, rule_label,
};
#[allow(unused_imports)]
pub(crate) use permission_rules::{
    file_grant_ok, file_grant_ok_in, inside, inside_any, is_file_tool, lexical, normalize_url,
    path_under_git, program_base, resolved, subagent_mode,
};
pub use step::step_limit;
pub use task::TaskRuntime;
#[cfg(test)]
pub(crate) use test_support::test_agent;
pub use ui::{Approval, ApprovalRequest, Approver, Ui};

use compact::{estimate_context, item_tokens};

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use anyhow::Result;
use tokio_util::sync::CancellationToken;

use crate::provider::Provider;
use crate::session::Session;
use crate::tools::Registry;
use crate::types::{Item, Request, StreamEvent, Usage};

const TITLE_PROMPT: &str = "\
Write a 3 to 6 word title for this conversation. Reply with the title only: \
no quotes, no trailing punctuation, and no explanation. Do not call tools.";

pub enum TurnEnd {
    Done,
    Cancelled,
}

pub struct Agent {
    pub provider: Box<dyn Provider>,
    pub tools: Registry,
    pub system: String,
    pub items: Vec<Item>,
    pub session: Option<Session>,
    /// Session id usage rows should use when this agent has no session file
    /// (a subagent records against its parent's session).
    pub usage_session_id: Option<String>,
    pub cwd: PathBuf,
    /// How risky calls are decided. Deny rules still block in every mode.
    pub permission_mode: crate::permissions::Mode,
    /// Identical auto-mode decisions reused for the rest of this session,
    /// including a subagent's calls.
    pub review_cache: std::sync::Arc<std::sync::Mutex<HashMap<String, crate::classifier::Verdict>>>,
    /// Rules granted with "always" in this session (see [`grant_rule`]).
    pub allowed: HashSet<String>,
    /// Persistent allow/ask/deny rules (see `crate::permissions`).
    pub permissions: std::sync::Arc<crate::permissions::Permissions>,
    pub usage: Usage,
    pub stats: crate::usage::Stats,
    /// Model context window in tokens; drives automatic compaction.
    pub context_window: u64,
    /// Latest context size (input plus output), or an estimate after compaction.
    pub context_tokens: u64,
    /// `context_tokens` is a chars/4 estimate, refreshed from the transcript
    /// until a provider reports usage.
    pub context_estimated: bool,
    /// Set when compaction cannot get under the limit, so later turns don't retry.
    pub(crate) suppress_auto_compact: bool,
    /// How many leading transcript items have already been sent to the provider.
    /// Automatic compaction may summarize only that prefix, so a user message
    /// that has not been submitted yet is kept verbatim.
    pub(crate) submitted: usize,
    /// User-configured lifecycle hooks (see `crate::hooks`); empty by default.
    pub hooks: std::sync::Arc<crate::hooks::Hooks>,
    pub sandbox: std::sync::Arc<crate::sandbox::Policy>,
    /// File contents before each write/edit, for `/undo`.
    pub undo: crate::undo::UndoLog,
    /// The task engine: the active task, its budget and ladder, the
    /// background verifier (see `task.rs`). Default: plain chat.
    pub task: TaskRuntime,
}

impl Agent {
    /// Save the model settings in use with the session, so resuming restores
    /// them. Only changes are written.
    pub fn sync_prefs(&mut self) {
        let Some(s) = &mut self.session else { return };
        let prefs = crate::session::Prefs {
            provider: self.provider.name().into(),
            model: self.provider.model().into(),
            effort: self.provider.effort().map(Into::into),
            fast: self.provider.fast(),
        };
        if let Err(e) = s.set_prefs(&prefs) {
            crate::ui::error(&format!("could not save session settings: {e:#}"));
        }
    }

    pub fn push(&mut self, item: Item) {
        self.stats.item(&item);
        self.sync_prefs();
        if let Some(s) = &mut self.session {
            if let Err(e) = s.append(&item) {
                crate::ui::error(&format!("could not save session: {e:#}"));
            }
        }
        self.items.push(item);
        if let Err(e) = crate::subagents::checkpoint(&self.items) {
            crate::ui::error(&format!("Child checkpoint failed: {e:#}"));
        }
    }

    /// Run AttentionRequired hooks (in the background): nyq waits for the user.
    fn attention(&self, kind: &str, message: &str) {
        let session = self
            .session
            .as_ref()
            .map(|s| s.id.as_str())
            .or(self.usage_session_id.as_deref());
        self.hooks.attention(kind, message, &self.cwd, session);
    }

    fn bind_conversation(&mut self) {
        if let Some(s) = &self.session {
            self.provider.set_conversation(&s.id);
        }
    }

    /// Run one user turn to completion (or cancellation).
    pub async fn turn(
        &mut self,
        input: Item,
        ui: &mut dyn Ui,
        cancel: CancellationToken,
    ) -> Result<TurnEnd> {
        self.bind_conversation();
        let Some(input) = self.extension_prompt(input, ui).await else {
            return Ok(TurnEnd::Done);
        };
        self.push_counted(input);
        self.drive(ui, cancel, false).await
    }

    /// Run extensions' `user_prompt` handlers: they may block the prompt
    /// (None), rewrite its text, or add context before it.
    async fn extension_prompt(&mut self, mut input: Item, ui: &mut dyn Ui) -> Option<Item> {
        let Some(ext) = self.hooks.extensions().cloned() else {
            return Some(input);
        };
        let Item::User { text, .. } = &mut input else {
            return Some(input);
        };
        let edit = ext.user_prompt(text).await;
        if let Some(msg) = edit.blocked {
            ui.notice(&msg);
            return None;
        }
        if let Some(prompt) = edit.prompt {
            *text = prompt;
        }
        for context in edit.context {
            ui.notice(context.lines().next().unwrap_or(""));
            self.push_counted(Item::user(context));
        }
        Some(input)
    }

    /// Continue an interrupted or cut-off response. No user message is added.
    pub async fn continue_turn(
        &mut self,
        ui: &mut dyn Ui,
        cancel: CancellationToken,
    ) -> Result<TurnEnd> {
        anyhow::ensure!(!self.items.is_empty(), "Nothing to continue.");
        self.bind_conversation();
        self.drive(ui, cancel, true).await
    }

    /// Whether the session model cannot read images (see `tools::image::VISION`).
    fn text_only(&self) -> bool {
        crate::tools::image::text_only(self.provider.pricing_provider(), self.provider.model())
    }

    /// The tools offered on the next request: `vision` replaces `view_image`
    /// for a text-only model.
    fn advertised(&self) -> Vec<crate::types::ToolSpec> {
        let hidden = if self.text_only() {
            "view_image"
        } else {
            crate::tools::image::VISION
        };
        let mut specs = self.tools.advertised();
        specs.retain(|s| s.name != hidden);
        specs
    }

    /// Start a fresh conversation (keeps provider, tools and grants).
    pub fn reset(&mut self, session: Option<Session>) {
        self.items.clear();
        self.session = session;
        self.usage = Usage::default();
        self.stats = Default::default();
        self.context_tokens = 0;
        self.context_estimated = false;
        self.suppress_auto_compact = false;
        self.submitted = 0;
        self.undo = Default::default();
    }

    /// Drop the transcript in place: same session id, model, sandbox and rules.
    pub fn clear_context(&mut self) -> Result<()> {
        let artifact = self.session.as_ref().and_then(|session| {
            session
                .path
                .parent()
                .and_then(|dir| dir.parent())
                .map(|home| (home.to_path_buf(), session.id.clone()))
        });
        if let Some(session) = &mut self.session {
            session.clear_transcript()?;
        }
        if let Some((home, id)) = artifact {
            crate::tools::artifacts::remove_session(&home, &id);
        }
        self.items.clear();
        self.usage = Usage::default();
        let stats_id = std::mem::take(&mut self.stats.id);
        self.stats = Default::default();
        self.stats.id = stats_id;
        self.context_tokens = 0;
        self.context_estimated = false;
        self.suppress_auto_compact = false;
        self.submitted = 0;
        self.undo = Default::default();
        self.allowed.clear();
        Ok(())
    }

    /// Ask the model for a short title. The request is not stored in the transcript.
    pub async fn suggest_title(&mut self, cancel: &CancellationToken) -> Result<String> {
        anyhow::ensure!(!self.items.is_empty(), "Nothing to name yet.");
        if cancel.is_cancelled() {
            anyhow::bail!("Interrupted.");
        }
        self.bind_conversation();
        let mut items = self.items.clone();
        items.push(Item::user(TITLE_PROMPT));
        let specs = [];
        let req = Request {
            system: &self.system,
            items: &items,
            tools: &specs,
        };
        let mut ignore = |_: StreamEvent| {};
        let started = std::time::Instant::now();
        let response = tokio::select! {
            biased;
            _ = cancel.cancelled() => anyhow::bail!("Interrupted."),
            r = crate::provider::stream_with_retry(self.provider.as_ref(), req, &mut ignore) => r?,
        };
        self.stats.api_time += started.elapsed();
        self.record_usage(response.usage);
        let raw: String = response
            .items
            .iter()
            .filter_map(|item| match item {
                Item::Assistant { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        crate::session::generated_title(&raw)
    }

    /// After loading a transcript, estimate its size until the provider reports usage.
    pub(crate) fn adopt_transcript(&mut self) {
        if let Some(session) = &self.session {
            match self.stats.restore(session, self.permissions.path.parent()) {
                Ok(usage) => self.usage = usage,
                Err(e) => crate::ui::error(&format!("could not restore session stats: {e:#}")),
            }
        }

        self.context_estimated = true;
        self.context_tokens = estimate_context(&self.system, &self.items);
        self.submitted = self.items.len();
    }

    pub(crate) fn push_counted(&mut self, mut item: Item) {
        // A text-only model gets attached images as files for `vision`.
        if let Item::User { text, images } = &mut item {
            if !images.is_empty() && self.text_only() {
                let key = crate::tools::artifacts::session_key(
                    self.session.as_ref().map(|s| s.id.as_str()),
                );
                let note = crate::tools::image::park_images(&crate::config::home(), &key, images);
                text.push_str(&format!("\n\n{note}"));
                images.clear();
            }
        }
        if !self.context_estimated {
            self.context_tokens = self.context_tokens.saturating_add(item_tokens(&item));
        }
        self.push(item);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::test_support::*;
    use crate::provider::EventSink;
    use crate::types::Response;

    #[test]
    fn sessions_save_and_restore_model_settings() {
        let dir = tempfile::tempdir().unwrap();
        let mut a = agent(Response::default(), None);
        a.session = Some(Session::create(dir.path(), dir.path(), "fake").unwrap());
        a.push(Item::user("hi"));
        let id = a.session.as_ref().unwrap().id.clone();
        let meta = crate::session::find(dir.path(), &id, dir.path()).unwrap();
        let saved = meta.prefs.clone().unwrap();
        assert_eq!(
            (saved.provider.as_str(), saved.model.as_str(), saved.fast),
            ("fake", "fake", false)
        );

        let prefs = crate::session::Prefs {
            provider: "ollama".into(),
            model: "llama3".into(),
            ..Default::default()
        };
        crate::commands::restore_prefs(dir.path(), &mut a, Some(&prefs));
        assert_eq!(
            (a.provider.name(), a.provider.model()),
            ("ollama", "llama3")
        );
        a.sync_prefs();
        drop(a);
        let meta = crate::session::find(dir.path(), &id, dir.path()).unwrap();
        assert_eq!(meta.prefs, Some(prefs));
        // An unusable provider keeps the current model.
        let mut b = agent(Response::default(), None);
        let bad = crate::session::Prefs {
            provider: "nope".into(),
            model: "x".into(),
            ..Default::default()
        };
        crate::commands::restore_prefs(dir.path(), &mut b, Some(&bad));
        assert_eq!(b.provider.name(), "fake");
    }

    #[test]
    fn reset_drops_the_undo_log() {
        use serde_json::json;
        let dir = tempfile::tempdir().unwrap();
        let mut agent = agent(Response::default(), None);
        agent.cwd = dir.path().to_path_buf();
        std::fs::write(dir.path().join("f.txt"), "a").unwrap();
        agent.undo.snapshot(
            &agent.cwd,
            crate::sandbox::Mode::WorkspaceWrite,
            "edit_file",
            &json!({"path": "f.txt"}),
        );
        assert_eq!(agent.undo.len(), 1);
        agent.suppress_auto_compact = true;
        agent.context_estimated = true;
        agent.context_tokens = 40;
        agent.reset(None);
        assert!(agent.undo.is_empty());
        assert!(!agent.suppress_auto_compact);
        assert!(!agent.context_estimated);
        assert_eq!(agent.context_tokens, 0);

        agent.system = "abcd".into();
        agent.items = vec![Item::user("abcd")];
        agent.adopt_transcript();
        assert!(agent.context_estimated);
        assert_eq!(agent.context_tokens, 2);
        agent.set_context_window(1000);
        assert_eq!(agent.context_window, 1000);
        assert!(!agent.suppress_auto_compact);
    }

    #[test]
    fn push_keeps_the_item_when_the_session_cannot_be_saved() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("not-a-directory");
        std::fs::write(&blocker, "x").unwrap();
        let mut agent = agent(Response::default(), None);
        agent.session = Some(Session::create(&blocker, std::path::Path::new("."), "m").unwrap());
        agent.items.clear();
        agent.push(Item::user("hello"));
        assert_eq!(agent.items, vec![Item::user("hello")]);
    }

    #[tokio::test]
    async fn continue_and_rename_do_not_store_a_new_user_message() {
        struct Script {
            text: String,
            saw: std::sync::Arc<std::sync::Mutex<String>>,
        }
        #[async_trait::async_trait]
        impl Provider for Script {
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
                *self.saw.lock().unwrap() = req.system.to_string();
                assert!(req.items.iter().all(|item| !matches!(item, Item::User { text, .. } if text.contains("Continue from where"))));
                Ok(Response {
                    truncated: false,
                    items: vec![Item::Assistant {
                        text: self.text.clone(),
                    }],
                    usage: None,
                })
            }
        }
        let saw = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let mut agent = agent(Response::default(), None);
        agent.provider = Box::new(Script {
            text: "rest of the answer".into(),
            saw: saw.clone(),
        });
        agent.items = vec![Item::Assistant {
            text: "partial".into(),
        }];
        agent.system = "system".into();
        agent
            .continue_turn(&mut QuietUi, CancellationToken::new())
            .await
            .unwrap();
        assert!(saw
            .lock()
            .unwrap()
            .contains("Continue from where you stopped"));
        assert_eq!(agent.items.len(), 2);
        assert!(matches!(&agent.items[0], Item::Assistant { text } if text == "partial"));
        assert!(agent
            .items
            .iter()
            .all(|item| !matches!(item, Item::User { .. })));

        agent.items.clear();
        assert!(agent
            .continue_turn(&mut QuietUi, CancellationToken::new())
            .await
            .is_err());

        agent.provider = Box::new(Script {
            text: "\"Fix the parser bug.\"".into(),
            saw,
        });
        agent.items = vec![Item::user("the parser fails on quotes")];
        let before = agent.items.clone();
        let title = agent
            .suggest_title(&CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(title, "Fix the parser bug");
        assert_eq!(agent.items, before);
    }

    #[tokio::test]
    async fn clear_context_keeps_the_session_and_drops_the_transcript() {
        let dir = tempfile::tempdir().unwrap();
        let mut agent = agent(Response::default(), None);
        agent.cwd = dir.path().to_path_buf();
        agent.session = Some(Session::create(dir.path(), &agent.cwd, "m").unwrap());
        agent.items.clear();
        agent.push(Item::user("hello"));
        agent.allowed.insert("bash:ls".into());
        let id = agent.session.as_ref().unwrap().id.clone();
        agent.stats.turns = 4;
        agent.stats.tools.insert("bash".into(), 2);
        let stats_id = agent.stats.id.clone();
        agent.clear_context().unwrap();
        assert_eq!(agent.session.as_ref().unwrap().id, id);
        assert_eq!(agent.stats.id, stats_id);
        assert_eq!(agent.stats.turns, 0);
        assert!(agent.stats.tools.is_empty());
        assert!(agent.items.is_empty());
        assert!(agent.allowed.is_empty());
        assert!(agent.undo.is_empty());
        let cwd = agent.cwd.clone();
        drop(agent.session.take());
        let (_, _, items) = crate::session::load(dir.path(), &id, &cwd).unwrap();
        assert!(items.is_empty());
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn prompt_hooks_add_context_or_block() {
        let mut agent = calling_agent(
            &std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            0,
        );
        agent.hooks = hooks_with(crate::hooks::Event::UserPromptSubmit, "echo branch: main");
        let live = CancellationToken::new();
        assert_eq!(
            crate::hooks::prompt_submit(&mut agent, "hi", &live).await,
            None
        );
        assert!(
            matches!(agent.items.last(), Some(Item::User { text, .. }) if text.ends_with("branch: main"))
        );
        agent.hooks = hooks_with(
            crate::hooks::Event::UserPromptSubmit,
            r#"echo '{"decision":"block","reason":"no"}'"#,
        );
        let n = agent.items.len();
        let msg = crate::hooks::prompt_submit(&mut agent, "hi", &live)
            .await
            .unwrap();
        assert!(msg.contains("no"), "{msg}");
        assert_eq!(agent.items.len(), n);
    }

    #[test]
    fn a_text_only_model_gets_vision_and_parked_images() {
        struct TextOnly;
        #[async_trait::async_trait]
        impl Provider for TextOnly {
            fn name(&self) -> &str {
                "textonly-test"
            }
            fn model(&self) -> &str {
                "t1"
            }
            fn set_model(&mut self, _: String) {}
            fn fork(&self) -> Box<dyn Provider> {
                Box::new(TextOnly)
            }
            async fn stream(&self, _: Request<'_>, _: EventSink<'_>) -> Result<Response> {
                anyhow::bail!("unused")
            }
        }
        let _lock = crate::provider::sse::tests::ENV
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home = tempfile::tempdir().unwrap();
        let doc = serde_json::json!({"textonly-test": {"models": {"t1": {"modalities": {"input": ["text"], "output": ["text"]}}}}});
        std::fs::write(home.path().join("models.json"), doc.to_string()).unwrap();
        let old = std::env::var_os("NYQ_HOME");
        std::env::set_var("NYQ_HOME", home.path());
        let mut subject = agent(Response::default(), None);
        subject.tools = Registry::builtin();
        let names: Vec<String> = subject.advertised().into_iter().map(|s| s.name).collect();
        assert!(
            !names.contains(&"vision".to_string()) && names.contains(&"view_image".to_string())
        );

        let mut agent = subject;
        agent.provider = Box::new(TextOnly);
        let names: Vec<String> = agent.advertised().into_iter().map(|s| s.name).collect();
        assert!(
            names.contains(&"vision".to_string()) && !names.contains(&"view_image".to_string())
        );
        let png = crate::types::Image {
            media_type: "image/png".into(),
            data: "iVBORw0KGgo=".into(),
        };
        agent.push_counted(Item::User {
            text: "look".into(),
            images: vec![png],
        });
        match old {
            Some(v) => std::env::set_var("NYQ_HOME", v),
            None => std::env::remove_var("NYQ_HOME"),
        }
        let Some(Item::User { text, images }) = agent.items.last() else {
            panic!()
        };
        assert!(
            images.is_empty() && text.starts_with("look\n\n[Image(s) attached: "),
            "{text}"
        );
    }
}
