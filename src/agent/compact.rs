//! Compaction and context accounting: the automatic threshold, manual and
//! automatic compaction, token estimates and usage recording.

use std::collections::HashSet;

use anyhow::Result;
use tokio_util::sync::CancellationToken;

use super::{Agent, Ui};
use crate::types::{Item, Request, StreamEvent, Usage};

/// Compact automatically once a request uses this share of the context
/// window (percent, from settings; 0 turns automatic compaction off).
static AUTO_COMPACT_PERCENT: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(80);

pub fn set_auto_compact_percent(percent: u8) {
    AUTO_COMPACT_PERCENT.store(percent.min(100), std::sync::atomic::Ordering::Relaxed);
}

/// The compaction threshold as a fraction; above 1 (never reached) when off.
fn auto_compact_at() -> f64 {
    match AUTO_COMPACT_PERCENT.load(std::sync::atomic::Ordering::Relaxed) {
        0 => f64::INFINITY,
        p => f64::from(p) / 100.0,
    }
}

pub(super) const COMPACT_PROMPT: &str = "\
Summarize this conversation so far so that work can continue from the summary alone; \
the earlier messages will be discarded. Include: the user's goals and requests (quote \
key instructions), decisions made, files read or changed (with paths) and why, commands \
run and their important results, errors and how they were resolved, the current state \
of the work, and the exact next steps. Be thorough but concise. Do not call tools.";

impl Agent {
    pub(super) fn account_usage(&mut self, usage: Option<Usage>, ui: &mut dyn Ui) {
        if let Some(u) = self.record_usage(usage) {
            ui.usage(&u);
        }
    }

    /// Count a request and record its usage (session totals and usage.jsonl).
    pub(super) fn record_usage(&mut self, usage: Option<Usage>) -> Option<Usage> {
        self.stats.requests += 1;
        let u = usage?;
        self.usage.add(&u);
        // Permissions are loaded from the active profile, including in ACP and subagents.
        if let Some(home) = self
            .permissions
            .path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
        {
            let session = self
                .usage_session_id
                .as_deref()
                .or_else(|| self.session.as_ref().map(|s| s.id.as_str()))
                .unwrap_or(self.stats.id.as_str());
            if let Err(e) = crate::usage::append(
                home,
                session,
                self.provider.name(),
                self.provider.pricing_provider(),
                self.provider.model(),
                u,
            ) {
                crate::ui::error(&format!("could not save usage: {e:#}"));
            }
        }
        Some(u)
    }

    /// Replace the transcript with a model-written summary. Returns false if
    /// cancelled (the transcript is then left unchanged).
    pub async fn compact(&mut self, ui: &mut dyn Ui, cancel: &CancellationToken) -> Result<bool> {
        if cancel.is_cancelled() {
            return Ok(false);
        }
        if self.items.is_empty() {
            return Ok(true);
        }
        let mut pending = HashSet::new();
        for item in &self.items {
            match item {
                Item::ToolCall { id, .. } => {
                    pending.insert(id);
                }
                Item::ToolResult { id, .. } => {
                    pending.remove(id);
                }
                _ => {}
            }
        }
        anyhow::ensure!(
            pending.is_empty(),
            "cannot compact with unanswered tool calls"
        );
        let mut items = self.items.clone();
        let mut prompt = COMPACT_PROMPT.to_string();
        if let Some(ext) = self.hooks.extensions() {
            for extra in ext.before_compact().await {
                prompt.push_str(&format!(
                    "\n\nAdditional instructions for this summary:\n{extra}"
                ));
            }
        }
        items.push(Item::user(prompt));
        let req = Request {
            system: &self.system,
            items: &items,
            tools: &[],
        };
        let mut ignore = |_: StreamEvent| {};
        let started = std::time::Instant::now();
        let response = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Ok(false),
            r = crate::provider::stream_with_retry(self.provider.as_ref(), req, &mut ignore) => r?,
        };
        self.stats.api_time += started.elapsed();
        self.account_usage(response.usage, ui);
        if cancel.is_cancelled() {
            return Ok(false);
        }
        if response.truncated {
            anyhow::bail!("compaction was cut off at the output limit");
        }
        anyhow::ensure!(
            !response
                .items
                .iter()
                .any(|item| matches!(item, Item::ToolCall { .. })),
            "compaction returned tool calls instead of a summary"
        );
        let summary: String = response
            .items
            .iter()
            .filter_map(|i| match i {
                Item::Assistant { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        anyhow::ensure!(
            !summary.trim().is_empty(),
            "compaction produced an empty summary"
        );
        // The summary request input is discarded. Its output remains alongside
        // the system prompt and compaction wrapper; estimate when usage is absent.
        let estimate = |text: &str| (text.len() as u64).div_ceil(4);
        self.context_tokens = estimate(&self.system)
            + estimate(&crate::types::compaction_text(""))
            + response.usage.map_or(estimate(&summary), |u| {
                u.output_tokens.max(estimate(&summary))
            });
        self.context_estimated = false;
        self.suppress_auto_compact = self.context_window > 0
            && self.context_tokens as f64 >= self.context_window as f64 * auto_compact_at();
        let before = self.items.len();
        self.items.clear();
        self.push(Item::Compaction { summary });
        self.submitted = self.items.len();
        ui.notice(&format!("Compacted {before} messages into a summary."));
        Ok(true)
    }

    /// Replace the context-window size and allow automatic compaction again.
    pub fn set_context_window(&mut self, window: u64) {
        self.context_window = window;
        self.suppress_auto_compact = false;
    }

    /// Compact only the prefix already sent to the provider. Trailing items
    /// (a new user paste, tool results, images) stay in the transcript.
    pub(super) async fn auto_compact(
        &mut self,
        ui: &mut dyn Ui,
        cancel: &CancellationToken,
    ) -> Result<bool> {
        if self.suppress_auto_compact
            || self.submitted == 0
            || self.context_window == 0
            || (self.context_tokens as f64) < (self.context_window as f64) * auto_compact_at()
        {
            return Ok(true);
        }
        let split = self.submitted.min(self.items.len());
        // One item has nothing to summarize, and an image the model has not
        // been sent yet must stay in the request that shows it.
        if self.items.len() < 2 || split == 0 || tail_has_unsent_image(&self.items[split..]) {
            return Ok(true);
        }
        let tail = self.items.split_off(split);
        ui.notice("Context is nearly full; compacting the conversation…");
        let compacted = match self.compact(ui, cancel).await {
            Ok(ok) => ok,
            Err(e) => {
                self.items.extend(tail);
                return Err(e);
            }
        };
        if !compacted {
            self.items.extend(tail);
            return Ok(false);
        }
        if !self.context_estimated {
            for item in &tail {
                self.context_tokens = self.context_tokens.saturating_add(item_tokens(item));
            }
        }
        let kept = self.items.len();
        for item in tail {
            self.push(item);
        }
        self.submitted = kept;
        if self.context_window > 0
            && self.context_tokens as f64 >= self.context_window as f64 * auto_compact_at()
        {
            self.suppress_auto_compact = true;
            ui.notice(
                "Still over the automatic compaction limit after compacting (the system prompt alone may exceed it). Automatic compaction is off for the rest of this session.",
            );
        }
        Ok(true)
    }

    pub(super) fn sync_estimate(&mut self) {
        if self.context_estimated {
            self.context_tokens = estimate_context(&self.system, &self.items);
        }
    }
}

fn text_tokens(text: &str) -> u64 {
    (text.len() as u64).div_ceil(4)
}

/// Flat cost for one image. Base64 length is not the model's token cost.
const IMAGE_TOKENS: u64 = 1_500;

fn tail_has_unsent_image(items: &[Item]) -> bool {
    items
        .iter()
        .any(|item| matches!(item, Item::User { images, .. } if !images.is_empty()))
}

pub(super) fn item_tokens(item: &Item) -> u64 {
    match item {
        Item::User { text, images } => text_tokens(text) + images.len() as u64 * IMAGE_TOKENS,
        Item::Assistant { text } => text_tokens(text),
        // `encrypted` is opaque provider state, not text the model re-reads.
        Item::Reasoning { summary, .. } => text_tokens(summary),
        Item::ToolCall {
            id,
            name,
            arguments,
        } => text_tokens(id) + text_tokens(name) + text_tokens(arguments),
        Item::ToolResult { id, output, .. } => text_tokens(id) + text_tokens(output),
        Item::Compaction { summary } => text_tokens(&crate::types::compaction_text(summary)),
    }
}

pub(super) fn estimate_context(system: &str, items: &[Item]) -> u64 {
    text_tokens(system) + items.iter().map(item_tokens).sum::<u64>()
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;
    use crate::agent::test_support::*;
    use crate::agent::{Approval, ApprovalRequest, TurnEnd};
    use crate::provider::{EventSink, Provider};
    use crate::session::Session;
    use crate::tools::ToolOutput;
    use crate::types::Response;

    #[tokio::test]
    async fn compaction_preserves_unsent_tail_on_resume_and_after_manual_compact() {
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
        agent.session = Some(Session::create(dir.path(), &agent.cwd, "fake").unwrap());
        agent.push(Item::Assistant {
            text: "old answer".into(),
        });
        agent.submitted = agent.items.len();
        let tail = Item::user("fix the failing test");
        agent.push(tail.clone());
        assert!(agent
            .auto_compact(&mut QuietUi, &CancellationToken::new())
            .await
            .unwrap());
        let id = agent.session.as_ref().unwrap().id.clone();
        let cwd = agent.cwd.clone();
        drop(agent.session.take());
        let (session, _, loaded) = crate::session::load(dir.path(), &id, &cwd).unwrap();
        assert_eq!(loaded, agent.items);
        agent.session = Some(session);
        assert_eq!(loaded.last(), Some(&tail));
        agent.submitted = 40;
        assert!(agent
            .compact(&mut QuietUi, &CancellationToken::new())
            .await
            .unwrap());
        assert_eq!(agent.submitted, 1);
        agent.push_counted(Item::user("large log".repeat(1000)));
        assert!(agent
            .auto_compact(&mut QuietUi, &CancellationToken::new())
            .await
            .unwrap());
        assert_eq!(
            agent.items.last(),
            Some(&Item::user("large log".repeat(1000)))
        );
    }

    #[tokio::test]
    async fn cancelled_compaction_preserves_transcript_and_session() {
        let cancel = CancellationToken::new();
        let mut agent = agent(
            Response {
                truncated: false,
                items: vec![Item::Assistant {
                    text: "summary".into(),
                }],
                usage: Some(Usage {
                    input_tokens: 100,
                    output_tokens: 10,
                    cached_tokens: 5,
                    ..Default::default()
                }),
            },
            Some(cancel.clone()),
        );
        let dir = tempfile::tempdir().unwrap();
        let mut session = Session::create(dir.path(), &agent.cwd, "fake").unwrap();
        session.append(&agent.items[0]).unwrap();
        let path = std::fs::read_dir(dir.path())
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let saved = std::fs::read(&path).unwrap();
        agent.session = Some(session);
        let before = agent.items.clone();
        assert!(!agent.compact(&mut QuietUi, &cancel).await.unwrap());
        assert_eq!(agent.items, before);
        assert_eq!(agent.context_tokens, 900);
        // A completed response still consumes tokens even when cancellation
        // prevents adopting its summary.
        assert_eq!(
            agent.usage,
            Usage {
                input_tokens: 100,
                output_tokens: 10,
                cached_tokens: 5,
                ..Default::default()
            }
        );
        assert_eq!(agent.stats.requests, 1);
        assert_eq!(std::fs::read(path).unwrap(), saved);
    }

    #[tokio::test]
    async fn compaction_logs_usage_once_in_active_profile() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join("models.json"),
            r#"{"fake":{"models":{"fake":{"cost":{"input":2,"output":8,"cache_read":0.5}}}}}"#,
        )
        .unwrap();
        let usage = Usage {
            input_tokens: 100,
            output_tokens: 20,
            cached_tokens: 40,
            ..Default::default()
        };
        let mut agent = agent(
            Response {
                truncated: false,
                items: vec![Item::Assistant {
                    text: "summary".into(),
                }],
                usage: Some(usage),
            },
            None,
        );
        agent.permissions = std::sync::Arc::new(crate::permissions::Permissions {
            path: home.path().join("permissions.json"),
            ..Default::default()
        });
        agent.session =
            Some(Session::create(&home.path().join("sessions"), &agent.cwd, "fake").unwrap());
        assert!(agent
            .compact(&mut QuietUi, &CancellationToken::new())
            .await
            .unwrap());
        let log = std::fs::read_to_string(home.path().join("usage.jsonl")).unwrap();
        assert_eq!(log.lines().count(), 1);
        let r: crate::usage::Record = serde_json::from_str(log.trim()).unwrap();
        assert_eq!(r.session, agent.session.as_ref().unwrap().id);
        assert_eq!((r.input, r.output, r.cached), (100, 20, 40));
        assert!((r.cost_usd.unwrap() - 0.0003).abs() < 1e-12);
        assert_eq!(agent.usage, usage);
        assert_eq!(agent.stats.requests, 1);
    }

    #[tokio::test]
    async fn compaction_accounts_usage_and_retained_context() {
        let usage = Usage {
            input_tokens: 900,
            output_tokens: 20,
            cached_tokens: 40,
            ..Default::default()
        };
        let mut agent = agent(
            Response {
                truncated: false,
                items: vec![Item::Assistant {
                    text: "summary".into(),
                }],
                usage: Some(usage),
            },
            None,
        );
        agent.suppress_auto_compact = true;
        assert!(agent
            .compact(&mut QuietUi, &CancellationToken::new())
            .await
            .unwrap());
        assert!(!agent.suppress_auto_compact);
        assert_eq!(agent.usage, usage);
        assert_eq!(
            agent.context_tokens,
            20 + 2 + (crate::types::compaction_text("").len() as u64).div_ceil(4)
        );
        assert_eq!(
            agent.items,
            vec![Item::Compaction {
                summary: "summary".into()
            }]
        );
    }

    #[tokio::test]
    async fn invalid_compaction_leaves_transcript_unchanged() {
        for items in [
            vec![],
            vec![
                Item::Assistant {
                    text: "summary".into(),
                },
                Item::ToolCall {
                    id: "new".into(),
                    name: "bash".into(),
                    arguments: "{}".into(),
                },
            ],
        ] {
            let mut agent = agent(
                Response {
                    items,
                    usage: None,
                    truncated: false,
                },
                None,
            );
            let before = agent.items.clone();
            assert!(agent
                .compact(&mut QuietUi, &CancellationToken::new())
                .await
                .is_err());
            assert_eq!(agent.items, before);
        }
        let mut agent = agent(Response::default(), None);
        agent.items.push(Item::ToolCall {
            id: "pending".into(),
            name: "bash".into(),
            arguments: "{}".into(),
        });
        assert!(agent
            .compact(&mut QuietUi, &CancellationToken::new())
            .await
            .unwrap_err()
            .to_string()
            .contains("unanswered"));
    }

    #[tokio::test]
    async fn failed_autocompact_does_not_repeat() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Arc, Mutex};

        #[derive(Clone)]
        struct Scripted {
            left: Arc<Mutex<Vec<Response>>>,
            calls: Arc<AtomicUsize>,
        }

        #[async_trait::async_trait]
        impl Provider for Scripted {
            fn name(&self) -> &str {
                "fake"
            }
            fn model(&self) -> &str {
                "fake"
            }
            fn set_model(&mut self, _: String) {}
            fn fork(&self) -> Box<dyn Provider> {
                Box::new(self.clone())
            }
            async fn stream(&self, _: Request<'_>, _: EventSink<'_>) -> Result<Response> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                let mut left = self.left.lock().unwrap();
                anyhow::ensure!(!left.is_empty(), "script exhausted");
                Ok(left.remove(0))
            }
        }

        struct NoteUi(Vec<String>);
        #[async_trait::async_trait]
        impl Ui for NoteUi {
            fn stream(&mut self, _: &StreamEvent) {}
            fn end_message(&mut self) {}
            fn tool_start(&mut self, _: &str, _: &str) {}
            fn tool_end(&mut self, _: &str, _: &ToolOutput) {}
            async fn approve(&mut self, _: &ApprovalRequest<'_>) -> Approval {
                Approval::Yes
            }
            fn notice(&mut self, msg: &str) {
                self.0.push(msg.to_string());
            }
        }

        let scripted = Scripted {
            left: Arc::new(Mutex::new(vec![
                Response {
                    truncated: false,
                    items: vec![Item::Assistant {
                        text: "summary".into(),
                    }],
                    usage: Some(Usage {
                        input_tokens: 1,
                        output_tokens: 1000,
                        cached_tokens: 0,
                        ..Default::default()
                    }),
                },
                Response {
                    truncated: false,
                    items: vec![Item::Assistant {
                        text: "answer".into(),
                    }],
                    usage: Some(Usage {
                        input_tokens: 900,
                        output_tokens: 50,
                        cached_tokens: 0,
                        ..Default::default()
                    }),
                },
                Response {
                    truncated: false,
                    items: vec![Item::Assistant {
                        text: "next".into(),
                    }],
                    usage: Some(Usage {
                        input_tokens: 900,
                        output_tokens: 10,
                        cached_tokens: 0,
                        ..Default::default()
                    }),
                },
            ])),
            calls: Arc::new(AtomicUsize::new(0)),
        };
        let calls = scripted.calls.clone();
        let mut agent = agent(Response::default(), None);
        agent.provider = Box::new(scripted);
        let mut ui = NoteUi(Vec::new());
        assert!(matches!(
            agent
                .turn(Item::user("hi"), &mut ui, CancellationToken::new())
                .await
                .unwrap(),
            TurnEnd::Done
        ));
        assert!(agent.suppress_auto_compact);
        assert!(ui
            .0
            .iter()
            .any(|m| m.contains("Automatic compaction is off")));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(matches!(
            agent
                .turn(Item::user("again"), &mut ui, CancellationToken::new())
                .await
                .unwrap(),
            TurnEnd::Done
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn unsent_user_text_reaches_the_model() {
        use std::sync::{Arc, Mutex};

        #[derive(Clone)]
        struct Rec(Arc<Mutex<bool>>);

        #[async_trait::async_trait]
        impl Provider for Rec {
            fn name(&self) -> &str {
                "fake"
            }
            fn model(&self) -> &str {
                "fake"
            }
            fn set_model(&mut self, _: String) {}
            fn fork(&self) -> Box<dyn Provider> {
                Box::new(self.clone())
            }
            async fn stream(&self, req: Request<'_>, _: EventSink<'_>) -> Result<Response> {
                let answering = req.items.last().is_none_or(
                    |item| !matches!(item, Item::User { text, .. } if text == COMPACT_PROMPT),
                );
                if answering && req.items.iter().any(|item| matches!(item, Item::User { text, .. } if text.contains("UNIQUE_PASTE"))) {
                    *self.0.lock().unwrap() = true;
                }
                Ok(Response {
                    truncated: false,
                    items: vec![Item::Assistant { text: "ok".into() }],
                    usage: Some(Usage {
                        input_tokens: 10,
                        output_tokens: 1,
                        cached_tokens: 0,
                        ..Default::default()
                    }),
                })
            }
        }

        let saw = Arc::new(Mutex::new(false));
        let mut agent = agent(Response::default(), None);
        agent.provider = Box::new(Rec(saw.clone()));
        let paste = format!("UNIQUE_PASTE {}", "line\n".repeat(50));
        let end = agent
            .turn(Item::user(paste), &mut QuietUi, CancellationToken::new())
            .await
            .unwrap();
        assert!(matches!(end, TurnEnd::Done));
        assert!(
            *saw.lock().unwrap(),
            "the unsent paste was summarized away before the model saw it"
        );
        assert!(
            agent.items.iter().any(
                |item| matches!(item, Item::User { text, .. } if text.contains("UNIQUE_PASTE"))
            ) || agent
                .items
                .iter()
                .any(|item| matches!(item, Item::Compaction { .. }))
        );
    }

    #[test]
    fn images_and_encrypted_blobs_have_a_flat_cost() {
        use crate::types::Image;
        let image = Image {
            media_type: "image/png".into(),
            data: "A".repeat(400_000),
        };
        let item = Item::User {
            text: "why".into(),
            images: vec![image.clone(), image],
        };
        let tokens = item_tokens(&item);
        assert_eq!(tokens, text_tokens("why") + 2 * IMAGE_TOKENS);
        assert!(tokens < 10_000, "{tokens}");

        let reasoning = Item::Reasoning {
            id: None,
            summary: "abcd".into(),
            encrypted: Some("z".repeat(80_000)),
        };
        assert_eq!(item_tokens(&reasoning), 1);
        let mut agent = agent(Response::default(), None);
        agent.system.clear();
        agent.items = vec![reasoning];
        agent.adopt_transcript();
        assert_eq!(agent.context_tokens, 1);
    }

    #[tokio::test]
    async fn single_oversized_message_is_not_compacted() {
        use crate::types::Image;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        struct Probe {
            saw_compact: Arc<AtomicBool>,
            saw_image: Arc<AtomicBool>,
        }

        #[async_trait::async_trait]
        impl Provider for Probe {
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
                if req
                    .items
                    .iter()
                    .any(|i| matches!(i, Item::User { text, .. } if text == COMPACT_PROMPT))
                {
                    self.saw_compact.store(true, Ordering::SeqCst);
                }
                if req
                    .items
                    .iter()
                    .any(|i| matches!(i, Item::User { images, .. } if !images.is_empty()))
                {
                    self.saw_image.store(true, Ordering::SeqCst);
                }
                Ok(Response {
                    truncated: false,
                    items: vec![Item::Assistant {
                        text: "answer".into(),
                    }],
                    usage: Some(Usage {
                        input_tokens: 10,
                        output_tokens: 1,
                        cached_tokens: 0,
                        ..Default::default()
                    }),
                })
            }
        }

        let saw_compact = Arc::new(AtomicBool::new(false));
        let saw_image = Arc::new(AtomicBool::new(false));
        let mut agent = agent(Response::default(), None);
        agent.provider = Box::new(Probe {
            saw_compact: saw_compact.clone(),
            saw_image: saw_image.clone(),
        });
        agent.items.clear();
        agent.context_tokens = 0;
        agent.context_window = 1000;
        let item = Item::User {
            text: "why".into(),
            images: vec![Image {
                media_type: "image/png".into(),
                data: "A".repeat(20_000),
            }],
        };
        agent
            .turn(item, &mut QuietUi, CancellationToken::new())
            .await
            .unwrap();
        assert!(!saw_compact.load(Ordering::SeqCst));
        assert!(saw_image.load(Ordering::SeqCst));
        assert!(agent
            .items
            .iter()
            .any(|i| matches!(i, Item::User { images, .. } if !images.is_empty())));
    }

    #[tokio::test]
    async fn viewed_image_is_not_compacted_before_the_model_sees_it() {
        use crate::tools::{Tool, ToolOutput};
        use crate::types::{Image, ToolSpec};
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        struct Probe {
            saw_compact: Arc<AtomicBool>,
            saw_image: Arc<AtomicBool>,
        }

        #[async_trait::async_trait]
        impl Provider for Probe {
            fn name(&self) -> &str {
                "fake"
            }
            fn model(&self) -> &str {
                "fake"
            }
            fn set_model(&mut self, _: String) {}
            fn fork(&self) -> Box<dyn Provider> {
                Box::new(Probe {
                    saw_compact: self.saw_compact.clone(),
                    saw_image: self.saw_image.clone(),
                })
            }
            async fn stream(&self, req: Request<'_>, _: EventSink<'_>) -> Result<Response> {
                if req
                    .items
                    .iter()
                    .any(|i| matches!(i, Item::User { text, .. } if text == COMPACT_PROMPT))
                {
                    self.saw_compact.store(true, Ordering::SeqCst);
                    return Ok(Response {
                        truncated: false,
                        items: vec![Item::Assistant {
                            text: "summary".into(),
                        }],
                        usage: None,
                    });
                }
                if req
                    .items
                    .iter()
                    .any(|i| matches!(i, Item::User { images, .. } if !images.is_empty()))
                {
                    self.saw_image.store(true, Ordering::SeqCst);
                    return Ok(Response {
                        truncated: false,
                        items: vec![Item::Assistant {
                            text: "A pixel.".into(),
                        }],
                        usage: Some(Usage {
                            input_tokens: 10,
                            output_tokens: 1,
                            cached_tokens: 0,
                            ..Default::default()
                        }),
                    });
                }
                Ok(Response {
                    truncated: false,
                    items: vec![Item::ToolCall {
                        id: "1".into(),
                        name: "view_image".into(),
                        arguments: r#"{"path":"shot.png"}"#.into(),
                    }],
                    usage: Some(Usage {
                        input_tokens: 10,
                        output_tokens: 1,
                        cached_tokens: 0,
                        ..Default::default()
                    }),
                })
            }
        }

        struct ImageTool;
        #[async_trait::async_trait]
        impl Tool for ImageTool {
            fn spec(&self) -> ToolSpec {
                ToolSpec {
                    name: "view_image".into(),
                    description: String::new(),
                    parameters: serde_json::json!({}),
                }
            }
            fn summary(&self, _: &Value) -> String {
                "shot.png".into()
            }
            async fn run(&self, _: Value, _: &crate::tools::ToolCtx) -> ToolOutput {
                ToolOutput {
                    content: "Loaded shot.png".into(),
                    images: vec![Image {
                        media_type: "image/png".into(),
                        data: "A".repeat(20_000),
                    }],
                    ..ToolOutput::default()
                }
            }
        }

        let saw_compact = Arc::new(AtomicBool::new(false));
        let saw_image = Arc::new(AtomicBool::new(false));
        let mut agent = agent(Response::default(), None);
        agent.provider = Box::new(Probe {
            saw_compact: saw_compact.clone(),
            saw_image: saw_image.clone(),
        });
        agent.tools.add(Box::new(ImageTool));
        agent.items.clear();
        agent.context_tokens = 0;
        agent.context_window = 1000;
        agent.context_estimated = false;
        agent
            .turn(
                Item::user("inspect the screenshot"),
                &mut QuietUi,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!saw_compact.load(Ordering::SeqCst));
        assert!(saw_image.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn usage_is_recorded_under_the_parent_session() {
        let home = tempfile::tempdir().unwrap();
        let mut agent = agent(Response::default(), None);
        agent.context_window = 0;
        agent.items.clear();
        agent.session = None;
        agent.usage_session_id = Some("parent-1".into());
        agent.permissions =
            std::sync::Arc::new(crate::permissions::Permissions::load(home.path(), home.path()).0);
        let stats_id = agent.stats.id.clone();
        agent.provider = Box::new(ScriptedTurns {
            left: std::sync::Arc::new(std::sync::Mutex::new(vec![Response {
                truncated: false,
                items: vec![Item::Assistant { text: "ok".into() }],
                usage: Some(Usage {
                    input_tokens: 9,
                    output_tokens: 2,
                    cached_tokens: 0,
                    ..Default::default()
                }),
            }])),
        });
        agent
            .turn(Item::user("go"), &mut QuietUi, CancellationToken::new())
            .await
            .unwrap();
        let log = std::fs::read_to_string(home.path().join("usage.jsonl")).unwrap();
        assert!(log.contains("parent-1"), "{log}");
        assert!(!log.contains(&stats_id), "{log}");
        assert!(log.contains("\"input\":9"), "{log}");
    }
}
