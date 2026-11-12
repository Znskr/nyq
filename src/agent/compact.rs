//! Compaction and context accounting, cheapest tier first.
//!
//! Automatic compaction starts once the context passes
//! [`compact_threshold`], or right after the provider rejects a request as
//! too long for the context window (the step loop then retries once):
//!
//! 1. **Prune** (no model call). Older tool results, superseded reads, old
//!    errors and old images are shortened in the *request*, not in the
//!    stored transcript: [`CompactState`] keeps a frozen overlay of
//!    replacements that [`project`] applies whenever a request is built.
//! 2. **Summarize the older span.** The newest ~16% of `W − O` stays
//!    verbatim; everything before it becomes one [`Item::Compaction`]. The
//!    summary request replays the request prefix and appends the
//!    instruction, so it is mostly a prompt-cache hit.
//! 3. **Summarize again with a smaller verbatim tail**, down to none. If
//!    the context is still too large, say so and carry on. Compaction never
//!    switches itself off: the next step that has something new to
//!    summarize tries again.
//!
//! Why a projection and not a persisted prune: the session file stays the
//! full record (search, export and `/rewind` see real outputs), and nothing
//! new has to be replayed on load. The overlay changes only when tier 1
//! runs, which is only above the threshold, and its cut-off moves in
//! chunks of [`PRUNE_CHUNK_TURNS`] turns, so between runs every request
//! repeats the same pruned prefix byte for byte. On resume the overlay is
//! rebuilt by the same deterministic rules the first time pressure returns.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::hash::{Hash, Hasher};
use std::path::Path;

use anyhow::Result;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::{Agent, Ui};
use crate::types::{Item, Request, StreamEvent, ToolSpec, Usage};
use crate::usage::tokens::{Estimator, Family};

/// Compact automatically once a request uses this share of the context
/// window (percent, from settings; 0 turns automatic compaction off).
static AUTO_COMPACT_PERCENT: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(80);

pub fn set_auto_compact_percent(percent: u8) {
    AUTO_COMPACT_PERCENT.store(percent.min(100), std::sync::atomic::Ordering::Relaxed);
}

fn auto_compact_percent() -> u8 {
    AUTO_COMPACT_PERCENT.load(std::sync::atomic::Ordering::Relaxed)
}

/// Room kept free beyond the output cap (dsh's `B`).
const HEADROOM: u64 = 65_536;

/// The token count at which automatic compaction starts:
/// `min(percent × W, W − O − 64Ki)`, or just the percentage when the window
/// is too small for the headroom. None when automatic compaction is off.
pub(super) fn compact_threshold(window: u64, output: u64, percent: u8) -> Option<u64> {
    if window == 0 || percent == 0 {
        return None;
    }
    let share = window.saturating_mul(u64::from(percent.min(100))) / 100;
    match window.checked_sub(output.saturating_add(HEADROOM)) {
        Some(room) if room > 0 => Some(share.min(room)),
        _ => Some(share),
    }
}

/// Share of `W − O` a summary keeps verbatim, then the smaller retries.
const KEEP_STEPS: [f64; 3] = [0.16, 0.04, 0.0];
/// Model turns at the end that tier 1 never touches.
const PRUNE_KEEP_TURNS: usize = 6;
/// Tier 1's cut-off advances this many turns at a time, so the pruned
/// prefix changes in steps rather than one item per request.
const PRUNE_CHUNK_TURNS: usize = 4;
/// Older tool results above this many tokens are cut to a head and tail.
const PRUNE_RESULT_TOKENS: u64 = 1_024;
const PRUNE_HEAD: usize = 800;
const PRUNE_TAIL: usize = 400;
/// Older error results keep this many characters (promptyped).
const PRUNE_ERROR_CHARS: usize = 200;
/// Newest images that stay in the request.
const PRUNE_KEEP_IMAGES: usize = 2;

pub(super) const COMPACT_PROMPT: &str = "\
Summarize this conversation so far so that work can continue from your summary alone: the \
messages above will be replaced by it, and any newer messages are kept verbatim after it. Write a \
checkpoint with exactly these sections, in order, as terse bullets; write \"(none)\" for an \
empty section:

## Goals
The user's goals and requests, quoting key instructions and corrections verbatim.
## Decisions
Decisions made and why, constraints, and the user's preferences.
## Intent log
What was attempted, in order, and what came of each attempt, including dead ends to avoid.
## Open questions
Unanswered questions, pending work and blockers.
## Files touched
Exact paths read, created or changed, and what changed in each.
## Build and test state
The latest build, test and lint commands with their results, and the exact errors still open.
## Next step
The exact next action.

Preserve exact paths, commands, identifiers and error messages. If the conversation starts with \
an earlier summary, merge it: keep what is still true and drop what is stale. Do not mention \
this request. Do not call tools.";

/// What one compaction attempt achieved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Reduced {
    Cancelled,
    /// Nothing could be pruned or summarized.
    Unchanged,
    Smaller,
}

/// Bracket around a transcript replacement. W5 records these in the
/// session (`compaction_start` before the summary is appended,
/// `compaction_end` after the verbatim tail is re-appended) so a crash in
/// between is detectable on load; until then they are only traced.
#[derive(Clone, Copy, Debug)]
pub(super) enum CompactionPhase {
    /// About to replace `summarized` items with a summary and re-append
    /// `kept` items after it.
    Start {
        summarized: usize,
        kept: usize,
    },
    End,
}

/// Per-session compaction state: tier 1's overlay and the token estimator.
#[derive(Clone, Debug, Default)]
pub(crate) struct CompactState {
    /// Request-time replacements for stored items, by index.
    overlay: BTreeMap<usize, Pruned>,
    estimator: Estimator,
}

#[derive(Clone, Debug)]
struct Pruned {
    /// Fingerprint of the stored item this replaces; a transcript replaced
    /// behind the agent's back (resume, rewind) no longer matches.
    mark: u64,
    item: Item,
}

impl CompactState {
    /// Forget the overlay (the transcript was replaced).
    pub(crate) fn clear(&mut self) {
        self.overlay.clear();
    }

    /// After a summary replaced `items[..split]`: entries for the kept tail
    /// move to their new positions behind the summary item.
    fn rebase(&mut self, split: usize) {
        self.overlay = std::mem::take(&mut self.overlay)
            .into_iter()
            .filter(|(i, _)| *i >= split)
            .map(|(i, p)| (i - split + 1, p))
            .collect();
    }
}

fn mark(item: &Item) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    std::mem::discriminant(item).hash(&mut h);
    let mut text = |s: &str| {
        s.len().hash(&mut h);
        s.as_bytes()[..s.len().min(64)].hash(&mut h);
    };
    match item {
        Item::User { text: t, images } => {
            text(t);
            images.len().hash(&mut h);
            for image in images {
                image.data.len().hash(&mut h);
            }
        }
        Item::ToolResult {
            id,
            output,
            is_error,
        } => {
            text(id);
            text(output);
            is_error.hash(&mut h);
        }
        Item::Assistant { text: t } | Item::Compaction { summary: t } => text(t),
        Item::Reasoning { summary, .. } => text(summary),
        Item::ToolCall { id, arguments, .. } => {
            text(id);
            text(arguments);
        }
    }
    h.finish()
}

/// The transcript as requests see it: stored items with tier 1's
/// replacements applied.
pub(super) fn project<'a>(items: &'a [Item], state: &CompactState) -> Cow<'a, [Item]> {
    let mut out: Option<Vec<Item>> = None;
    for (&i, pruned) in &state.overlay {
        if items.get(i).is_some_and(|item| mark(item) == pruned.mark) {
            out.get_or_insert_with(|| items.to_vec())[i] = pruned.item.clone();
        }
    }
    out.map_or(Cow::Borrowed(items), Cow::Owned)
}

fn raw_item(e: &Estimator, item: &Item) -> f64 {
    match item {
        Item::User { text, images } => {
            e.raw_text(text) + images.iter().map(|i| e.raw_image(i)).sum::<f64>()
        }
        Item::Assistant { text } => e.raw_text(text),
        // `encrypted` is opaque provider state, not text the model re-reads.
        Item::Reasoning { summary, .. } => e.raw_text(summary),
        Item::ToolCall {
            id,
            name,
            arguments,
        } => e.raw_text(id) + e.raw_text(name) + e.raw_text(arguments),
        Item::ToolResult { id, output, .. } => e.raw_text(id) + e.raw_text(output),
        Item::Compaction { summary } => e.raw_text(&crate::types::compaction_text(summary)),
    }
}

pub(super) fn item_tokens(e: &Estimator, item: &Item) -> u64 {
    e.tokens(raw_item(e, item))
}

fn raw_specs(e: &Estimator, specs: &[ToolSpec]) -> f64 {
    specs
        .iter()
        .map(|s| {
            e.raw_text(&s.name) + e.raw_text(&s.description) + e.raw_text(&s.parameters.to_string())
        })
        .sum()
}

/// Uncalibrated tokens for a whole request.
fn raw_request(e: &Estimator, system: &str, items: &[Item], specs: &[ToolSpec]) -> f64 {
    e.raw_text(system) + items.iter().map(|i| raw_item(e, i)).sum::<f64>() + raw_specs(e, specs)
}

/// Whether `error` says the request did not fit the model's context window.
pub(super) fn is_context_overflow(error: &anyhow::Error) -> bool {
    const SIGNS: [&str; 12] = [
        "context_length_exceeded",
        "maximum context length",
        "context length",
        "context window",
        "context size",
        "prompt is too long",
        "maximum prompt length",
        "input is too long",
        "input token count",
        "exceeds the maximum number of tokens",
        "too large for model",
        "request_too_large",
    ];
    let mut rate_limited = false;
    let found = error.chain().any(|cause| {
        if let Some(e) = cause.downcast_ref::<crate::provider::sse::HttpError>() {
            // A rate limit can say "request too large" about tokens per minute.
            if matches!(e.status, 401 | 403 | 429) {
                rate_limited = true;
                return false;
            }
            if e.status == 413 {
                return true;
            }
        }
        let text = cause.to_string().to_ascii_lowercase();
        SIGNS.iter().any(|sign| text.contains(sign))
    });
    found && !rate_limited
}

/// Start of the first model turn tier 1 must leave alone, or 0. A turn is
/// one model response (reasoning, text, calls) with the results it got.
fn prune_cut(items: &[Item], keep: usize, chunk: usize) -> usize {
    let model = |i: &Item| {
        matches!(
            i,
            Item::Assistant { .. } | Item::Reasoning { .. } | Item::ToolCall { .. }
        )
    };
    let starts: Vec<usize> = (0..items.len())
        .filter(|&i| model(&items[i]) && (i == 0 || !model(&items[i - 1])))
        .collect();
    if starts.len() <= keep {
        return 0;
    }
    let eligible = (starts.len() - keep) / chunk.max(1) * chunk.max(1);
    if eligible == 0 {
        return 0;
    }
    starts[eligible]
}

fn floor_char(s: &str, mut i: usize) -> usize {
    i = i.min(s.len());
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil_char(s: &str, mut i: usize) -> usize {
    i = i.min(s.len());
    while !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// The `read_tool_result` id a spilled output already points to.
fn stored_id(output: &str) -> Option<&str> {
    const SIGN: &str = "tool result \"";
    let start = output.rfind(SIGN)? + SIGN.len();
    let len = output[start..].find('"')?;
    Some(&output[start..start + len]).filter(|id| !id.is_empty())
}

/// Where tier 1 keeps what it takes out of the request.
struct Store<'a> {
    home: &'a Path,
    key: &'a str,
}

impl Store<'_> {
    fn result(&self, call_id: &str, tool: &str, content: &str) -> Option<String> {
        crate::tools::artifacts::store_result(self.home, self.key, call_id, tool, content).ok()
    }

    fn image(&self, image: &crate::types::Image) -> Option<String> {
        use base64::Engine;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&image.data)
            .ok()?;
        let ext = image.media_type.rsplit('/').next().unwrap_or("png");
        let path = crate::tools::artifacts::store_file(self.home, self.key, ext, &bytes).ok()?;
        Some(path.display().to_string())
    }
}

/// A large older result cut to its head and tail, with where to find the rest.
fn shorten_result(call_id: &str, tool: &str, output: &str, store: &Store) -> String {
    let (body, id) = match stored_id(output) {
        // Already spilled: keep pointing at the stored copy.
        Some(id) => (
            output
                .rfind("\n[Full output:")
                .map_or(output, |end| &output[..end]),
            Some(id.to_string()),
        ),
        None if tool == crate::tools::artifacts::TOOL => (output, None),
        None => (output, store.result(call_id, tool, output)),
    };
    let head = floor_char(body, PRUNE_HEAD);
    let tail = ceil_char(body, body.len().saturating_sub(PRUNE_TAIL)).max(head);
    let note = match id {
        Some(id) => format!(
            "[Older output shortened. The full text is stored as tool result \"{id}\": call {} with {{\"id\": \"{id}\"}} (offset/limit or query) to read it.]",
            crate::tools::artifacts::TOOL
        ),
        None if tool == crate::tools::artifacts::TOOL => {
            "[Older page shortened; read it again to see all of it.]".into()
        }
        None => "[Older output shortened; the middle could not be kept.]".into(),
    };
    format!(
        "{}\n… [{} bytes omitted] …\n{}\n{note}",
        &body[..head],
        tail - head,
        &body[tail..]
    )
}

/// A `read_file` call's path and line range.
fn read_target(args: &Value) -> Option<(String, Option<u64>, Option<u64>)> {
    let path = args.get("path")?.as_str()?.to_string();
    let num = |k: &str| args.get(k).and_then(Value::as_u64);
    Some((path, num("offset").filter(|n| *n > 1), num("limit")))
}

/// Tier 1 replacements for `items[..cut]`, computed from the stored items
/// (`items` is the already-submitted prefix). Every rule rewrites one item
/// in place, so a call and its result always stay together.
fn plan_prune(items: &[Item], cut: usize, e: &Estimator, store: &Store) -> Vec<(usize, Item)> {
    let mut calls: HashMap<&str, (&str, Value)> = HashMap::new();
    for item in items {
        if let Item::ToolCall {
            id,
            name,
            arguments,
        } = item
        {
            let args = serde_json::from_str(arguments).unwrap_or(Value::Null);
            calls.insert(id.as_str(), (name.as_str(), args));
        }
    }
    let call = |id: &str| calls.get(id).map(|(n, a)| (*n, a));

    let newest_error = items
        .iter()
        .rposition(|i| matches!(i, Item::ToolResult { is_error: true, .. }));

    // Reads with a later read of the same file that covers them: a whole-file
    // read, or the same range again.
    let mut superseded = vec![false; items.len()];
    type Range = (Option<u64>, Option<u64>);
    let mut later: HashMap<String, Vec<Range>> = HashMap::new();
    for (i, item) in items.iter().enumerate().rev() {
        let Item::ToolResult {
            id,
            is_error: false,
            ..
        } = item
        else {
            continue;
        };
        let Some(("read_file", args)) = call(id) else {
            continue;
        };
        let Some((path, offset, limit)) = read_target(args) else {
            continue;
        };
        let seen = later.entry(path).or_default();
        superseded[i] = seen
            .iter()
            .any(|r| *r == (None, None) || *r == (offset, limit));
        seen.push((offset, limit));
    }

    // Images, newest first: the first PRUNE_KEEP_IMAGES stay.
    let mut image_rank = vec![0usize; items.len()];
    let mut newer = 0;
    for (i, item) in items.iter().enumerate().rev() {
        if let Item::User { images, .. } = item {
            image_rank[i] = newer;
            newer += images.len();
        }
    }

    let mut out = Vec::new();
    for (i, item) in items.iter().enumerate().take(cut) {
        let replacement = match item {
            Item::ToolResult {
                id,
                output,
                is_error: true,
            } if Some(i) != newest_error && output.chars().count() > PRUNE_ERROR_CHARS => {
                let keep = output
                    .char_indices()
                    .nth(PRUNE_ERROR_CHARS)
                    .map_or(output.len(), |(at, _)| at);
                Some(Item::ToolResult {
                    id: id.clone(),
                    output: format!("{}… [older error shortened]", &output[..keep]),
                    is_error: true,
                })
            }
            Item::ToolResult {
                id,
                is_error: false,
                ..
            } if superseded[i] => {
                let path = call(id)
                    .and_then(|(_, a)| read_target(a))
                    .map(|t| t.0)
                    .unwrap_or_default();
                Some(Item::ToolResult {
                    id: id.clone(),
                    output: format!("[superseded by a later read of {path}]"),
                    is_error: false,
                })
            }
            Item::ToolResult {
                id,
                output,
                is_error: false,
            } if e.text(output) > PRUNE_RESULT_TOKENS => {
                let tool = call(id).map_or("tool", |(n, _)| n);
                Some(Item::ToolResult {
                    id: id.clone(),
                    output: shorten_result(id, tool, output, store),
                    is_error: false,
                })
            }
            Item::User { text, images } if image_rank[i] + images.len() > PRUNE_KEEP_IMAGES => {
                // Keep this item's newest images while the budget lasts.
                let kept = PRUNE_KEEP_IMAGES.saturating_sub(image_rank[i]);
                let (old, new) = images.split_at(images.len() - kept);
                let mut text = text.clone();
                for image in old {
                    let size = crate::usage::tokens::image_size(&image.data)
                        .map(|(w, h)| format!(", {w}×{h}"))
                        .unwrap_or_default();
                    let saved = store
                        .image(image)
                        .map(|p| format!("; saved at {p}"))
                        .unwrap_or_default();
                    text.push_str(&format!(
                        "\n[Older image removed from context ({}{size}){saved}.]",
                        image.media_type
                    ));
                }
                Some(Item::User {
                    text,
                    images: new.to_vec(),
                })
            }
            _ => None,
        };
        if let Some(r) = replacement {
            out.push((i, r));
        }
    }
    out
}

/// Whether the transcript may be cut before `items[s]`: no call before the
/// cut has its result after it, and reasoning stays with what follows it.
fn boundaries(items: &[Item]) -> Vec<bool> {
    let mut result_at: HashMap<&str, usize> = HashMap::new();
    for (i, item) in items.iter().enumerate() {
        if let Item::ToolResult { id, .. } = item {
            result_at.entry(id.as_str()).or_insert(i);
        }
    }
    let mut ok = vec![false; items.len() + 1];
    // The furthest result index of any call seen so far; a call without a
    // result blocks every later cut.
    let mut open_until = 0usize;
    for s in 1..=items.len() {
        if let Item::ToolCall { id, .. } = &items[s - 1] {
            let r = result_at.get(id.as_str()).copied().unwrap_or(items.len());
            open_until = open_until.max(r);
        }
        ok[s] = open_until < s && !matches!(items[s - 1], Item::Reasoning { .. });
    }
    ok
}

/// A request must replace at least this many tokens beyond an earlier
/// summary to be worth re-summarizing.
fn min_span(window: u64) -> u64 {
    (window / 50).clamp(1, 2_048)
}

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

    fn family(&self) -> Family {
        Family::detect(self.provider.name(), self.provider.model())
    }

    /// The session's estimator for the current model.
    pub(super) fn estimator(&self) -> Estimator {
        let family = self.family();
        if self.compaction.estimator.family() == family {
            self.compaction.estimator.clone()
        } else {
            Estimator::new(family)
        }
    }

    pub(super) fn text_tokens(&self, text: &str) -> u64 {
        self.estimator().text(text)
    }

    pub(super) fn item_tokens(&self, item: &Item) -> u64 {
        item_tokens(&self.estimator(), item)
    }

    /// Uncalibrated size of a request, to calibrate on its reported usage.
    pub(super) fn raw_request(&self, system: &str, items: &[Item], specs: &[ToolSpec]) -> f64 {
        raw_request(&self.estimator(), system, items, specs)
    }

    /// Fit the estimator to a provider's report for a request whose
    /// uncalibrated size was `raw`.
    pub(super) fn calibrate(&mut self, raw: f64, input_tokens: u64) {
        let family = self.family();
        self.compaction.estimator.set_family(family);
        self.compaction.estimator.calibrate(raw, input_tokens);
        crate::trace::log("compaction", || {
            format!(
                "estimate {raw:.0} raw tokens, reported {input_tokens}; factor {:.3}",
                self.compaction.estimator.factor()
            )
        });
    }

    /// The transcript as the next request sends it (before the trailing note).
    pub(super) fn projected(&self) -> Cow<'_, [Item]> {
        project(&self.items, &self.compaction)
    }

    /// Estimated size of the next request: system prompt, tools and the
    /// projected transcript.
    pub(super) fn estimate_request(&self) -> u64 {
        let e = self.estimator();
        let items = self.projected();
        e.tokens(e.raw_text(&self.request_system(false)) + raw_specs(&e, &self.advertised()))
            + items.iter().map(|i| item_tokens(&e, i)).sum::<u64>()
    }

    pub(super) fn sync_estimate(&mut self) {
        if self.context_estimated {
            self.context_tokens = self.estimate_request();
        }
    }

    fn output_cap(&self) -> u64 {
        self.provider.max_output_tokens().unwrap_or(0)
    }

    /// The automatic compaction threshold for the current model, if on.
    pub(super) fn compact_threshold(&self) -> Option<u64> {
        compact_threshold(
            self.context_window,
            self.output_cap(),
            auto_compact_percent(),
        )
    }

    /// Hook points for the session's compaction records (see
    /// [`CompactionPhase`]).
    fn compaction_phase(&mut self, phase: CompactionPhase) {
        // W5: `compaction_start` / `compaction_end` session records go here.
        crate::trace::log("compaction", || match phase {
            CompactionPhase::Start { summarized, kept } => {
                format!("start: {summarized} items summarized, {kept} kept")
            }
            CompactionPhase::End => "end".into(),
        });
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
        anyhow::ensure!(
            boundaries(&self.items)[self.items.len()],
            "cannot compact with unanswered tool calls"
        );
        let before = self.items.len();
        let done = self.summarize_span(self.items.len(), ui, cancel).await?;
        if done {
            ui.notice(&format!("Compacted {before} messages into a summary."));
        }
        Ok(done)
    }

    /// Replace the context-window size.
    pub fn set_context_window(&mut self, window: u64) {
        self.context_window = window;
    }

    /// Replace `items[..split]` with a summary and keep the rest verbatim.
    /// The request replays the projected prefix the provider has cached,
    /// then asks for the summary. Returns false if cancelled (nothing
    /// changes then).
    async fn summarize_span(
        &mut self,
        split: usize,
        ui: &mut dyn Ui,
        cancel: &CancellationToken,
    ) -> Result<bool> {
        let mut prompt = COMPACT_PROMPT.to_string();
        if let Some(ext) = self.hooks.extensions() {
            for extra in ext.before_compact().await {
                prompt.push_str(&format!(
                    "\n\nAdditional instructions for this summary:\n{extra}"
                ));
            }
        }
        let mut items = self.projected()[..split].to_vec();
        items.push(Item::user(prompt));
        let system = self.request_system(false);
        let raw = self.raw_request(&system, &items, &[]);
        let req = Request {
            system: &system,
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
        if let Some(u) = response.usage {
            self.calibrate(raw, u.input_tokens);
        }
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

        let tail = self.items.split_off(split);
        self.compaction_phase(CompactionPhase::Start {
            summarized: split,
            kept: tail.len(),
        });
        let submitted = self.submitted;
        self.items.clear();
        self.compaction.rebase(split);
        self.push(Item::Compaction { summary });
        for item in tail {
            self.push(item);
        }
        self.compaction_phase(CompactionPhase::End);
        self.submitted = (1 + submitted.saturating_sub(split)).min(self.items.len());
        // The summary request's input is discarded: estimate what remains
        // until the next response reports usage.
        self.context_estimated = true;
        self.sync_estimate();
        Ok(true)
    }

    /// Tier 1: prune the request in place of the stored transcript.
    /// Returns the estimated tokens saved. `force` (after an overflow)
    /// spares only the newest turn.
    fn prune(&mut self, force: bool) -> u64 {
        let limit = self.submitted.min(self.items.len());
        let (keep, chunk) = if force {
            (1, 1)
        } else {
            (PRUNE_KEEP_TURNS, PRUNE_CHUNK_TURNS)
        };
        let cut = prune_cut(&self.items[..limit], keep, chunk);
        if cut == 0 {
            return 0;
        }
        let e = self.estimator();
        let home = crate::config::home();
        let key =
            crate::tools::artifacts::session_key(self.session.as_ref().map(|s| s.id.as_str()));
        let store = Store {
            home: &home,
            key: &key,
        };
        let plan = plan_prune(&self.items[..limit], cut, &e, &store);
        let mut saved = 0u64;
        for (i, item) in plan {
            let stored = &self.items[i];
            let mark = mark(stored);
            let current = match self.compaction.overlay.get(&i) {
                Some(p) if p.mark == mark => &p.item,
                _ => stored,
            };
            if *current == item {
                continue;
            }
            let (was, now) = (item_tokens(&e, current), item_tokens(&e, &item));
            if now >= was {
                continue;
            }
            saved += was - now;
            self.compaction.overlay.insert(i, Pruned { mark, item });
        }
        saved
    }

    /// Where to split for a summary that keeps about `keep` tokens of the
    /// newest items verbatim, or None when there is nothing worth
    /// summarizing.
    fn summary_split(&self, keep: u64) -> Option<usize> {
        let limit = self.submitted.min(self.items.len());
        // One item has nothing to summarize, and an image the model has not
        // been sent yet must stay in the request that shows it.
        if self.items.len() < 2
            || limit == 0
            || self.items[limit..]
                .iter()
                .any(|i| matches!(i, Item::User { images, .. } if !images.is_empty()))
        {
            return None;
        }
        let e = self.estimator();
        let items = self.projected();
        let costs: Vec<u64> = items.iter().map(|i| item_tokens(&e, i)).collect();
        // The smallest cut whose suffix fits in `keep`.
        let mut suffix: u64 = costs[limit..].iter().sum();
        let mut ideal = limit;
        while ideal > 0 && suffix + costs[ideal - 1] <= keep {
            ideal -= 1;
            suffix += costs[ideal];
        }
        let lo = usize::from(matches!(items[0], Item::Compaction { .. }));
        let ok = boundaries(&items);
        let split = (ideal.max(lo + 1)..=limit)
            .find(|&s| ok[s])
            .or_else(|| (lo + 1..ideal.max(lo + 1)).rev().find(|&s| ok[s]))?;
        // Re-summarizing a summary plus a few items costs a request per step
        // while the context stays full; wait until there is more.
        let fresh: u64 = costs[lo..split].iter().sum();
        (lo == 0 || fresh >= min_span(self.context_window)).then_some(split)
    }

    /// A split that summarizes about the older half of what `split` would,
    /// for when the summary request itself does not fit.
    fn smaller_split(&self, split: usize) -> Option<usize> {
        let e = self.estimator();
        let items = self.projected();
        let lo = usize::from(matches!(items[0], Item::Compaction { .. }));
        let total: u64 = items[lo..split].iter().map(|i| item_tokens(&e, i)).sum();
        let ok = boundaries(&items);
        let mut sum = 0;
        let mut best = None;
        for s in lo + 1..split {
            sum += item_tokens(&e, &items[s - 1]);
            if sum > total / 2 {
                break;
            }
            if ok[s] {
                best = Some(s);
            }
        }
        best
    }

    /// Summarize at `split`; if the summary request itself overflows,
    /// summarize a smaller older span instead.
    async fn summarize_fitting(
        &mut self,
        mut split: usize,
        ui: &mut dyn Ui,
        cancel: &CancellationToken,
    ) -> Result<bool> {
        loop {
            match self.summarize_span(split, ui, cancel).await {
                Err(e) if is_context_overflow(&e) => match self.smaller_split(split) {
                    Some(smaller) => split = smaller,
                    None => return Err(e),
                },
                other => return other,
            }
        }
    }

    /// Bring the context under `threshold`, cheapest tier first.
    async fn reduce(
        &mut self,
        threshold: u64,
        force: bool,
        ui: &mut dyn Ui,
        cancel: &CancellationToken,
    ) -> Result<Reduced> {
        let mut changed = false;
        let saved = self.prune(force);
        if saved > 0 {
            changed = true;
            self.context_tokens = self.context_tokens.saturating_sub(saved);
            ui.notice(&format!(
                "Context is nearly full; pruned older tool output from the request (about {saved} tokens)."
            ));
            if self.context_tokens < threshold {
                return Ok(Reduced::Smaller);
            }
        }
        let span = self.context_window.saturating_sub(self.output_cap());
        let mut summarized = false;
        for share in KEEP_STEPS {
            let Some(split) = self.summary_split((span as f64 * share) as u64) else {
                continue;
            };
            if !summarized {
                ui.notice("Context is nearly full; summarizing older messages…");
            }
            if !self.summarize_fitting(split, ui, cancel).await? {
                return Ok(Reduced::Cancelled);
            }
            summarized = true;
            changed = true;
            if self.context_tokens < threshold {
                return Ok(Reduced::Smaller);
            }
        }
        if summarized {
            ui.notice(
                "The context is still above the compaction threshold after summarizing: the system prompt, \
                 tools or newest messages alone are near the limit. Continuing; compaction runs again as \
                 the conversation grows.",
            );
        }
        Ok(if changed {
            Reduced::Smaller
        } else {
            Reduced::Unchanged
        })
    }

    /// Compact the prefix already sent to the provider when the context is
    /// over the threshold. Trailing items (a new user paste, tool results,
    /// images) stay in the transcript. Returns false if cancelled.
    pub(super) async fn auto_compact(
        &mut self,
        ui: &mut dyn Ui,
        cancel: &CancellationToken,
    ) -> Result<bool> {
        let Some(threshold) = self.compact_threshold() else {
            return Ok(true);
        };
        if self.submitted == 0 || self.context_tokens < threshold {
            return Ok(true);
        }
        Ok(self.reduce(threshold, false, ui, cancel).await? != Reduced::Cancelled)
    }

    /// After the provider rejected a request (uncalibrated size `raw`) as
    /// too long: learn from it and compact even below the threshold.
    pub(super) async fn recover_overflow(
        &mut self,
        raw: f64,
        ui: &mut dyn Ui,
        cancel: &CancellationToken,
    ) -> Result<Reduced> {
        let Some(threshold) = self.compact_threshold() else {
            return Ok(Reduced::Unchanged);
        };
        let family = self.family();
        self.compaction.estimator.set_family(family);
        self.compaction
            .estimator
            .overflowed(raw, self.context_window);
        self.context_tokens = self.context_tokens.max(self.context_window);
        ui.notice("The request did not fit the model's context window; compacting and retrying.");
        self.reduce(threshold, true, ui, cancel).await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::agent::test_support::*;
    use crate::agent::TurnEnd;
    use crate::provider::{EventSink, Provider};
    use crate::session::Session;
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
        assert!(agent
            .compact(&mut QuietUi, &CancellationToken::new())
            .await
            .unwrap());
        assert_eq!(agent.usage, usage);
        // What remains is estimated: the system prompt and the summary.
        let e = agent.estimator();
        assert!(agent.context_estimated);
        assert_eq!(
            agent.context_tokens,
            e.text("system")
                + item_tokens(
                    &e,
                    &Item::Compaction {
                        summary: "summary".into()
                    }
                )
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

    type Reply = dyn Fn(&Seen, usize) -> Result<Response> + Send + Sync;

    /// A provider that answers with `reply` and records every request.
    #[derive(Clone)]
    struct Fake {
        log: Arc<Mutex<Vec<Seen>>>,
        reply: Arc<Reply>,
        max_output: Option<u64>,
    }

    #[derive(Clone, Debug)]
    struct Seen {
        system: String,
        items: Vec<Item>,
        tools: Vec<ToolSpec>,
    }

    impl Seen {
        fn is_summary(&self) -> bool {
            matches!(self.items.last(), Some(Item::User { text, .. }) if text.starts_with(COMPACT_PROMPT))
        }
    }

    #[async_trait::async_trait]
    impl Provider for Fake {
        fn name(&self) -> &str {
            "fake"
        }
        fn model(&self) -> &str {
            "fake"
        }
        fn set_model(&mut self, _: String) {}
        fn max_output_tokens(&self) -> Option<u64> {
            self.max_output
        }
        fn fork(&self) -> Box<dyn Provider> {
            Box::new(self.clone())
        }
        async fn stream(&self, req: Request<'_>, _: EventSink<'_>) -> Result<Response> {
            let seen = Seen {
                system: req.system.to_string(),
                items: req.items.to_vec(),
                tools: req.tools.to_vec(),
            };
            let n = {
                let mut log = self.log.lock().unwrap();
                log.push(seen.clone());
                log.len() - 1
            };
            (self.reply)(&seen, n)
        }
    }

    fn fake(
        reply: impl Fn(&Seen, usize) -> Result<Response> + Send + Sync + 'static,
    ) -> (Box<dyn Provider>, Arc<Mutex<Vec<Seen>>>) {
        let log = Arc::new(Mutex::new(Vec::new()));
        let provider = Fake {
            log: log.clone(),
            reply: Arc::new(reply),
            max_output: None,
        };
        (Box::new(provider), log)
    }

    fn says(text: &str, input: u64) -> Response {
        Response {
            truncated: false,
            items: vec![Item::Assistant { text: text.into() }],
            usage: Some(Usage {
                input_tokens: input,
                output_tokens: 10,
                ..Default::default()
            }),
        }
    }

    fn result(id: &str, output: impl Into<String>, is_error: bool) -> Item {
        Item::ToolResult {
            id: id.into(),
            output: output.into(),
            is_error,
        }
    }

    fn png(w: u32, h: u32) -> crate::types::Image {
        use base64::Engine;
        let mut bytes = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        bytes.extend(w.to_be_bytes());
        bytes.extend(h.to_be_bytes());
        bytes.extend([8, 6, 0, 0, 0]);
        crate::types::Image {
            media_type: "image/png".into(),
            data: base64::engine::general_purpose::STANDARD.encode(bytes),
        }
    }

    /// `n` model turns, each a bash call and its `output(i)` result.
    fn turns(n: usize, output: impl Fn(usize) -> String) -> Vec<Item> {
        let mut items = vec![Item::user("build the parser")];
        for i in 0..n {
            items.push(call(
                &format!("c{i}"),
                "bash",
                serde_json::json!({"command": i}),
            ));
            items.push(result(&format!("c{i}"), output(i), false));
        }
        items
    }

    #[test]
    fn threshold_is_the_smaller_of_the_percentage_and_the_headroom() {
        // min(0.8 × 200k, 200k − 32k − 64Ki)
        assert_eq!(compact_threshold(200_000, 32_000, 80), Some(102_464));
        // A large window: the percentage is smaller.
        assert_eq!(compact_threshold(1_000_000, 64_000, 80), Some(800_000));
        // The headroom does not fit: fall back to the percentage.
        assert_eq!(compact_threshold(32_768, 0, 80), Some(26_214));
        assert_eq!(compact_threshold(65_536, 0, 80), Some(52_428));
        assert_eq!(compact_threshold(100_000, 40_000, 80), Some(80_000));
        assert_eq!(compact_threshold(0, 0, 80), None);
        assert_eq!(compact_threshold(200_000, 0, 0), None);

        // The agent uses the provider's output cap.
        let mut agent = agent(Response::default(), None);
        agent.provider = Box::new(Fake {
            log: Default::default(),
            reply: Arc::new(|_, _| anyhow::bail!("unused")),
            max_output: Some(32_000),
        });
        agent.context_window = 200_000;
        assert_eq!(agent.compact_threshold(), Some(102_464));
        agent.context_window = 0;
        assert_eq!(agent.compact_threshold(), None);
    }

    #[test]
    fn prune_cut_moves_in_chunks_and_spares_the_newest_turns() {
        let cut = |n| {
            prune_cut(
                &turns(n, |_| "x".into()),
                PRUNE_KEEP_TURNS,
                PRUNE_CHUNK_TURNS,
            )
        };
        // Turn i starts at item 1 + 2i.
        assert_eq!(cut(6), 0);
        assert_eq!(cut(9), 0);
        assert_eq!(cut(10), 1 + 2 * 4);
        assert_eq!(cut(13), 1 + 2 * 4);
        assert_eq!(cut(14), 1 + 2 * 8);
        // Text, reasoning and parallel calls of one response are one turn.
        let items = vec![
            Item::user("go"),
            Item::Reasoning {
                id: None,
                summary: "think".into(),
                encrypted: None,
            },
            call("a", "bash", serde_json::json!({})),
            call("b", "bash", serde_json::json!({})),
            result("a", "1", false),
            result("b", "2", false),
            Item::Assistant {
                text: "done".into(),
            },
        ];
        assert_eq!(prune_cut(&items, 1, 1), 6);
    }

    #[test]
    fn tier1_rules_shorten_results_reads_errors_and_images_in_place() {
        let home = tempfile::tempdir().unwrap();
        let store = Store {
            home: home.path(),
            key: "s1",
        };
        let log: String = (0..400)
            .map(|i| format!("line {i} of the build log\n"))
            .collect();
        let error = format!("error[E0308]: mismatched types {}", "detail ".repeat(80));
        let spilled = format!(
            "{}\n[Full output: 99999 bytes, stored as tool result \"bash-0123abcd\". Call read_tool_result …]",
            "y ".repeat(3_000)
        );
        let items = vec![
            Item::user("go"),
            call("r1", "read_file", serde_json::json!({"path": "a.rs"})),
            result("r1", "fn a() {} // v1", false),
            call(
                "r2",
                "read_file",
                serde_json::json!({"path": "b.rs", "offset": 10, "limit": 5}),
            ),
            result("r2", "b lines 10-14", false),
            call("b1", "bash", serde_json::json!({"command": "make"})),
            result("b1", log.clone(), false),
            call("b2", "bash", serde_json::json!({"command": "cargo test"})),
            result("b2", error.clone(), true),
            call("b3", "bash", serde_json::json!({"command": "big"})),
            result("b3", spilled, false),
            call("r3", "read_file", serde_json::json!({"path": "a.rs"})),
            result("r3", "fn a() {} // v2", false),
            call(
                "r4",
                "read_file",
                serde_json::json!({"path": "b.rs", "offset": 30}),
            ),
            result("r4", "b lines 30+", false),
            Item::User {
                text: "screenshots".into(),
                images: vec![png(800, 600), png(10, 10), png(20, 20)],
            },
            call("b4", "bash", serde_json::json!({"command": "cargo test"})),
            result("b4", error.clone(), true),
        ];
        let e = Estimator::default();
        let plan: HashMap<usize, Item> = plan_prune(&items, items.len(), &e, &store)
            .into_iter()
            .collect();

        // A whole-file read is superseded by a later read of the same file;
        // a different range is not.
        assert_eq!(
            plan.get(&2),
            Some(&result("r1", "[superseded by a later read of a.rs]", false))
        );
        assert!(!plan.contains_key(&4) && !plan.contains_key(&12) && !plan.contains_key(&14));
        // A large result keeps its head and tail and points at a stored copy.
        let Some(Item::ToolResult { id, output, .. }) = plan.get(&6) else {
            panic!("{plan:?}")
        };
        assert_eq!(id, "b1");
        assert!(output.starts_with("line 0 of the build log"), "{output}");
        assert!(output.contains("line 399 of the build log"), "{output}");
        let stored = stored_id(output).unwrap();
        let back =
            crate::tools::artifacts::read(home.path(), "s1", stored, 0, 64 * 1024, None).unwrap();
        assert!(back.contains(&log), "{back}");
        assert!(e.text(output) < e.text(&log) / 4);
        // Older errors keep 200 characters; the newest stays whole.
        let Some(Item::ToolResult {
            output, is_error, ..
        }) = plan.get(&8)
        else {
            panic!()
        };
        assert!(*is_error && output.ends_with("… [older error shortened]"));
        assert_eq!(
            output.chars().count(),
            200 + "… [older error shortened]".chars().count()
        );
        assert!(!plan.contains_key(&17));
        // An output that was spilled already keeps pointing at its copy.
        let Some(Item::ToolResult { output, .. }) = plan.get(&10) else {
            panic!()
        };
        assert_eq!(stored_id(output), Some("bash-0123abcd"));
        // Only the newest two images stay; the oldest becomes a placeholder.
        let Some(Item::User { text, images }) = plan.get(&15) else {
            panic!()
        };
        assert_eq!(images, &vec![png(10, 10), png(20, 20)]);
        assert!(
            text.contains("[Older image removed from context (image/png, 800×600); saved at "),
            "{text}"
        );
        // Calls are never touched and results keep their call ids.
        for (i, replacement) in &plan {
            match (&items[*i], replacement) {
                (Item::ToolResult { id: a, .. }, Item::ToolResult { id: b, .. }) => {
                    assert_eq!(a, b)
                }
                (Item::User { .. }, Item::User { .. }) => {}
                other => panic!("{other:?}"),
            }
        }
        // Nothing before the cut means nothing changes.
        assert!(plan_prune(&items, 0, &e, &store).is_empty());
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // Serializes NYQ_HOME with other env-mutating tests.
    async fn tier1_prunes_a_projection_and_stops_without_a_summary() {
        let _lock = crate::provider::sse::tests::ENV
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home = tempfile::tempdir().unwrap();
        let old = std::env::var_os("NYQ_HOME");
        std::env::set_var("NYQ_HOME", home.path());

        let (provider, log) = fake(|_, _| Ok(says("done", 1_000)));
        let mut agent = agent(Response::default(), None);
        agent.provider = provider;
        agent.session =
            Some(Session::create(&home.path().join("sessions"), &agent.cwd, "fake").unwrap());
        let big = |i: usize| format!("{i}: {}", "warning: unused variable `x`\n".repeat(700));
        agent.items.clear();
        for item in turns(12, big) {
            agent.push(item);
        }
        agent.submitted = agent.items.len();
        agent.context_window = 100_000;
        let threshold = agent.compact_threshold().unwrap();
        assert_eq!(threshold, 34_464);
        agent.context_tokens = 40_000;
        agent.context_estimated = false;
        let stored = agent.items.clone();
        let path = agent.session.as_ref().unwrap().path.clone();
        let file = std::fs::read(&path).unwrap();

        let mut ui = NoticeCatch(Vec::new());
        assert!(agent
            .auto_compact(&mut ui, &CancellationToken::new())
            .await
            .unwrap());
        // No model call, and the stored history is untouched.
        assert!(log.lock().unwrap().is_empty());
        assert_eq!(agent.items, stored);
        assert_eq!(std::fs::read(&path).unwrap(), file);
        assert!(agent.context_tokens < threshold, "{}", agent.context_tokens);
        assert!(
            ui.0.iter().any(|n| n.contains("pruned older tool output")),
            "{:?}",
            ui.0
        );
        // The oldest 4 turns (12 − 6 rounded down to a chunk) are shortened.
        let projected = agent.projected().into_owned();
        let changed: Vec<usize> = (0..stored.len())
            .filter(|&i| projected[i] != stored[i])
            .collect();
        assert_eq!(changed, vec![2, 4, 6, 8]);

        // Requests send the projection, and the prefix stays byte-stable
        // from one request to the next.
        for text in ["next", "again"] {
            agent
                .turn(Item::user(text), &mut QuietUi, CancellationToken::new())
                .await
                .unwrap();
        }
        let log = log.lock().unwrap();
        assert_eq!(log.len(), 2);
        assert_eq!(log[0].items[..stored.len()], projected[..]);
        let first = log[0].items.len() - 1;
        assert_eq!(log[1].items[..first], log[0].items[..first]);
        drop(log);

        // A transcript replaced behind the agent's back drops stale entries.
        agent.items[2] = result("c0", "different", false);
        assert_eq!(agent.projected()[2], agent.items[2]);
        match old {
            Some(v) => std::env::set_var("NYQ_HOME", v),
            None => std::env::remove_var("NYQ_HOME"),
        }
    }

    #[tokio::test]
    async fn tier2_summarizes_the_older_span_and_keeps_a_verbatim_tail() {
        let (provider, log) = fake(|seen, _| {
            assert!(seen.is_summary());
            Ok(says("SUMMARY", 5_000))
        });
        let dir = tempfile::tempdir().unwrap();
        let mut agent = agent(Response::default(), None);
        agent.provider = provider;
        agent.session = Some(Session::create(dir.path(), &agent.cwd, "fake").unwrap());
        agent.items.clear();
        // Results under the pruning size, all different: tier 1 has nothing.
        let medium = |i: usize| format!("{i} {}", "compiled crate ok\n".repeat(60));
        for item in turns(10, medium) {
            agent.push(item);
        }
        agent.push(Item::Assistant {
            text: "all built".into(),
        });
        agent.submitted = agent.items.len();
        agent.push(Item::user("now run the tests"));
        agent.context_window = 10_000;
        agent.context_tokens = 9_000;
        let before = agent.items.clone();

        assert!(agent
            .auto_compact(&mut QuietUi, &CancellationToken::new())
            .await
            .unwrap());
        let log = log.lock().unwrap();
        assert_eq!(log.len(), 1, "one summary brings it under the threshold");
        let seen = &log[0];
        assert!(seen.tools.is_empty());
        assert_eq!(seen.system, agent.request_system(false));
        // The request replays the stored prefix up to the split.
        let split = seen.items.len() - 1;
        assert_eq!(seen.items[..split], before[..split]);
        for section in [
            "## Goals",
            "## Decisions",
            "## Intent log",
            "## Open questions",
            "## Files touched",
            "## Build and test state",
        ] {
            assert!(COMPACT_PROMPT.contains(section), "{section}");
        }
        // Summary, then the newest items verbatim, starting at a turn.
        assert_eq!(
            agent.items[0],
            Item::Compaction {
                summary: "SUMMARY".into()
            }
        );
        assert_eq!(agent.items[1..], before[split..]);
        assert!(matches!(
            agent.items[1],
            Item::ToolCall { .. } | Item::Assistant { .. }
        ));
        assert!(agent.items.len() > 3, "a verbatim tail is kept");
        // Measured as at the split, before the summary's usage calibrated.
        let e = Estimator::default();
        let kept: u64 = before[split..].iter().map(|i| item_tokens(&e, i)).sum();
        assert!(kept <= 1_600, "{kept}");
        // The unsent message is still unsent.
        assert_eq!(agent.submitted, agent.items.len() - 1);
        assert!(agent.context_tokens < 8_000);

        // Reloading keeps the summary and the verbatim tail.
        let id = agent.session.as_ref().unwrap().id.clone();
        let cwd = agent.cwd.clone();
        drop(agent.session.take());
        let (_, _, loaded) = crate::session::load(dir.path(), &id, &cwd).unwrap();
        assert_eq!(loaded, agent.items);
    }

    #[tokio::test]
    async fn context_overflow_compacts_and_retries_once() {
        let overflow = || -> anyhow::Error {
            crate::provider::sse::HttpError {
                status: 400,
                retry_after: None,
                message: "API request failed · HTTP 400 · invalid_request_error: prompt is too long: 10500 tokens > 10000 maximum".into(),
            }
            .into()
        };
        let (provider, log) = fake(move |seen, n| match (seen.is_summary(), n) {
            (true, _) => Ok(says("SUMMARY", 3_000)),
            (false, 0) => Err(overflow()),
            (false, _) => Ok(says("done", 3_000)),
        });
        let mut agent = agent(Response::default(), None);
        agent.provider = provider;
        agent.items.clear();
        let text = |i: usize| format!("{i} {}", "test output line\n".repeat(30));
        for item in turns(4, text) {
            agent.push(item);
        }
        agent.push(Item::Assistant {
            text: "built".into(),
        });
        agent.submitted = agent.items.len();
        agent.context_window = 10_000;
        agent.context_tokens = 500;
        let mut ui = NoticeCatch(Vec::new());
        let end = agent
            .turn(Item::user("run it"), &mut ui, CancellationToken::new())
            .await
            .unwrap();
        assert!(matches!(end, TurnEnd::Done));
        let kinds: Vec<bool> = log.lock().unwrap().iter().map(Seen::is_summary).collect();
        assert_eq!(kinds, vec![false, true, false]);
        assert!(matches!(agent.items[0], Item::Compaction { .. }));
        assert!(agent.items.contains(&Item::user("run it")));
        assert_eq!(
            agent.items.last(),
            Some(&Item::Assistant {
                text: "done".into()
            })
        );
        assert!(ui.0.iter().any(|n| n.contains("did not fit")), "{:?}", ui.0);
        // The overflow taught the estimator that its counts were low.
        assert!(agent.estimator().factor() > 1.0);

        // Overflowing again right after the retry is an error, not a loop.
        let (provider, log) = fake(move |seen, _| {
            if seen.is_summary() {
                Ok(says("SUMMARY", 3_000))
            } else {
                Err(overflow())
            }
        });
        agent.provider = provider;
        let err = agent
            .turn(Item::user("again"), &mut QuietUi, CancellationToken::new())
            .await
            .err()
            .expect("the overflow error");
        assert!(format!("{err:#}").contains("prompt is too long"));
        assert!(log.lock().unwrap().len() <= 3);

        // With nothing sent before, there is nothing to compact: the error stands.
        let (provider, log) = fake(move |_, _| Err(overflow()));
        let mut fresh = agent_with(provider);
        let err = fresh
            .turn(
                Item::user("huge paste"),
                &mut QuietUi,
                CancellationToken::new(),
            )
            .await
            .err()
            .expect("the overflow error");
        assert!(format!("{err:#}").contains("prompt is too long"));
        assert_eq!(log.lock().unwrap().len(), 1);
    }

    fn agent_with(provider: Box<dyn Provider>) -> Agent {
        let mut a = agent(Response::default(), None);
        a.provider = provider;
        a.items.clear();
        a.submitted = 0;
        a.context_tokens = 0;
        a
    }

    #[test]
    fn overflow_errors_are_recognized() {
        let http = |status, message: &str| -> anyhow::Error {
            crate::provider::sse::HttpError {
                status,
                retry_after: None,
                message: message.into(),
            }
            .into()
        };
        for yes in [
            http(400, "This model's maximum context length is 128000 tokens. However, you requested 130000 tokens"),
            http(400, "API request failed · HTTP 400 · context_length_exceeded: Your input exceeds the context window of this model."),
            http(413, "API request failed · HTTP 413 · request_too_large"),
            anyhow::anyhow!("The input token count (1200000) exceeds the maximum number of tokens allowed (1048576)."),
            anyhow::anyhow!("prompt is too long: 210000 tokens > 200000 maximum").context("streaming"),
        ] {
            assert!(is_context_overflow(&yes), "{yes:#}");
        }
        for no in [
            http(
                429,
                "Request too large for gpt-4o on tokens per min (TPM): context length",
            ),
            http(400, "invalid tool schema"),
            anyhow::anyhow!("connection reset"),
        ] {
            assert!(!is_context_overflow(&no), "{no:#}");
        }
    }

    #[tokio::test]
    async fn compaction_never_latches_off() {
        let (provider, log) = fake(|seen, _| {
            Ok(if seen.is_summary() {
                says("summary", 3_000)
            } else {
                says("answer", 3_000)
            })
        });
        let mut agent = agent(Response::default(), None);
        agent.provider = provider;
        // A system prompt that alone is over the threshold (800 of 1000).
        agent.system = "word ".repeat(1_000);
        let mut ui = NoticeCatch(Vec::new());
        let summaries = |log: &Arc<Mutex<Vec<Seen>>>| {
            log.lock()
                .unwrap()
                .iter()
                .filter(|s| s.is_summary())
                .count()
        };

        agent
            .turn(Item::user("hi"), &mut ui, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(summaries(&log), 1);
        assert!(
            ui.0.iter()
                .any(|n| n.contains("still above the compaction threshold")),
            "{:?}",
            ui.0
        );
        // Still over, but only a summary and a few words are left: no request
        // is spent re-summarizing them.
        agent
            .turn(Item::user("again"), &mut ui, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(summaries(&log), 1);
        assert_eq!(log.lock().unwrap().len(), 3);
        // New material arrives; once it has been sent, compaction runs again.
        agent
            .turn(
                Item::user("detail ".repeat(150)),
                &mut ui,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(summaries(&log), 1, "an unsent message is never summarized");
        agent
            .turn(Item::user("more"), &mut ui, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(summaries(&log), 2);
        assert!(matches!(agent.items[0], Item::Compaction { .. }));
    }

    #[test]
    fn summaries_never_split_a_call_from_its_result() {
        let reasoning = || Item::Reasoning {
            id: None,
            summary: "plan".into(),
            encrypted: Some("blob".into()),
        };
        let items = vec![
            Item::user("go"),
            reasoning(),
            call("a", "bash", serde_json::json!({"n": 1})),
            call("b", "bash", serde_json::json!({"n": 2})),
            result("a", "one ".repeat(50), false),
            result("b", "two ".repeat(50), false),
            Item::Assistant {
                text: "ran both".into(),
            },
            Item::user("next"),
            reasoning(),
            Item::Assistant {
                text: "looking".into(),
            },
            call("c", "read_file", serde_json::json!({"path": "x"})),
            result("c", "three ".repeat(50), false),
            Item::Assistant {
                text: "done".into(),
            },
        ];
        let ok = boundaries(&items);
        // Inside the parallel calls, between results, or after reasoning: no.
        for s in [2, 3, 4, 5, 9, 11] {
            assert!(!ok[s], "{s}");
        }
        for s in [1, 6, 7, 8, 10, 12, 13] {
            assert!(ok[s], "{s}");
        }
        let mut agent = agent(Response::default(), None);
        agent.items = items.clone();
        agent.submitted = items.len();
        let e = agent.estimator();
        let total: u64 = items.iter().map(|i| item_tokens(&e, i)).sum();
        for keep in 0..=total + 1 {
            let Some(s) = agent.summary_split(keep) else {
                continue;
            };
            assert!(ok[s], "keep {keep} split {s}");
            let before: Vec<&str> = items[..s]
                .iter()
                .filter_map(|i| match i {
                    Item::ToolCall { id, .. } => Some(id.as_str()),
                    _ => None,
                })
                .collect();
            for item in &items[s..] {
                if let Item::ToolResult { id, .. } = item {
                    assert!(!before.contains(&id.as_str()), "keep {keep} split {s}");
                }
            }
        }
        // A call still waiting for its result blocks manual compaction too.
        assert!(!boundaries(&items[..4])[4]);
    }

    #[tokio::test]
    async fn the_estimate_is_calibrated_on_reported_usage() {
        let (provider, log) = fake(|_, _| Ok(says("ok", 5_000)));
        let mut agent = agent_with(provider);
        agent.context_window = 0;
        let message = "word ".repeat(2_000);
        let before = agent.text_tokens(&message);
        agent
            .turn(
                Item::user(message.clone()),
                &mut QuietUi,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let seen = log.lock().unwrap()[0].clone();
        let raw = raw_request(
            &Estimator::default(),
            &seen.system,
            &seen.items,
            &seen.tools,
        );
        let factor = agent.estimator().factor();
        assert!((factor - 5_000.0 / raw).abs() < 1e-9, "{factor} vs {raw}");
        assert!(factor > 1.5);
        // Later estimates use it.
        let after = agent.text_tokens(&message);
        assert!(after as f64 > before as f64 * 1.5, "{before} → {after}");
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
        let e = Estimator::default();
        let tokens = item_tokens(&e, &item);
        assert_eq!(
            tokens,
            e.text("why") + 2 * crate::usage::tokens::IMAGE_FALLBACK
        );
        assert!(tokens < 10_000, "{tokens}");

        let reasoning = Item::Reasoning {
            id: None,
            summary: "abcd".into(),
            encrypted: Some("z".repeat(80_000)),
        };
        assert_eq!(item_tokens(&e, &reasoning), 1);
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
