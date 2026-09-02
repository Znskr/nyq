#![allow(non_snake_case)]
use std::ffi::c_void;

use crate::core::shared::{debug_trace, display_width};
use crate::core::shared::types::{
    FinishedPrompt, HistoryTurn, TurnPresentationOutcome, TurnTokenProgress,
};

const NANOS_PER_MS: i128 = 1_000_000;

pub type EmitFn = fn(*mut c_void, &[u8]) -> Result<(), ()>;
pub type FinishFn = fn(*mut c_void, FinishedPrompt) -> Result<(), ()>;
pub type AppendHistoryFn = fn(*mut c_void, &FinishedPrompt) -> Result<DeferredFinishCommit, ()>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeferredFinishCommit {
    Uncommitted,
    Committed,
}

#[derive(Clone, Copy)]
pub struct DeferredPresentationCallbacks {
    pub ctx: *mut c_void,
    pub append_history: AppendHistoryFn,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FlushResult {
    Drained,
    Blocked,
}

#[derive(Clone, Copy)]
pub struct TickCallbacks {
    pub emit_ctx: *mut c_void,
    pub emit_fn: EmitFn,
    pub finish_ctx: *mut c_void,
    pub finish_fn: FinishFn,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum CodeForeground {
    #[default]
    None,
    Dark,
    Light,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SgrState {
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub strike: bool,
    pub code_fg: CodeForeground,
}

impl SgrState {
    pub fn isActive(self) -> bool {
        self.bold
            || self.dim
            || self.italic
            || self.underline
            || self.strike
            || self.code_fg != CodeForeground::None
    }

    /// Update based on a complete ANSI sequence (including `\x1b[` prefix and
    /// terminator). Unrecognized sequences are ignored, so non-SGR escapes
    /// (cursor moves, erasures, etc.) do not confuse the tracker.
    pub fn apply(&mut self, seq: &[u8]) {
        if seq.len() < 3 {
            return;
        }
        if seq[0] != 0x1b || seq[1] != b'[' {
            return;
        }
        if seq[seq.len() - 1] != b'm' {
            return;
        }
        let body = &seq[2..seq.len() - 1];

        if body.is_empty() || body == b"0" {
            *self = Self::default();
            return;
        }

        if body == b"1" {
            self.bold = true;
        } else if body == b"2" {
            self.dim = true;
        } else if body == b"3" {
            self.italic = true;
        } else if body == b"4" {
            self.underline = true;
        } else if body == b"9" {
            self.strike = true;
        } else if body == b"22" {
            self.bold = false;
            self.dim = false;
        } else if body == b"23" {
            self.italic = false;
        } else if body == b"24" {
            self.underline = false;
        } else if body == b"29" {
            self.strike = false;
        } else if body == b"39" {
            self.code_fg = CodeForeground::None;
        } else if body == b"38;5;245" {
            self.code_fg = CodeForeground::Dark;
        } else if body == b"38;5;247" {
            self.code_fg = CodeForeground::Light;
        }
    }

    /// Serialize open codes for the currently-active attributes into `buf`.
    /// Returns the number of bytes written (always fits in 31 bytes: five
    /// 4-byte attribute opens and the 11-byte code-foreground open).
    pub fn writeOpens(&self, buf: &mut [u8]) -> usize {
        let mut n: usize = 0;
        if self.bold {
            n = append_into(buf, n, b"\x1b[1m");
        }
        if self.dim {
            n = append_into(buf, n, b"\x1b[2m");
        }
        if self.italic {
            n = append_into(buf, n, b"\x1b[3m");
        }
        if self.underline {
            n = append_into(buf, n, b"\x1b[4m");
        }
        if self.strike {
            n = append_into(buf, n, b"\x1b[9m");
        }
        match self.code_fg {
            CodeForeground::None => {}
            CodeForeground::Dark => n = append_into(buf, n, b"\x1b[38;5;245m"),
            CodeForeground::Light => n = append_into(buf, n, b"\x1b[38;5;247m"),
        }
        n
    }
}

fn append_into(dst: &mut [u8], pos: usize, src: &[u8]) -> usize {
    if pos + src.len() <= dst.len() {
        dst[pos..pos + src.len()].copy_from_slice(src);
        pos + src.len()
    } else {
        pos
    }
}

#[derive(Default)]
pub struct AssistantPacer {
    pending: Vec<u8>,
    deferred_turn: Option<FinishedPrompt>,
    deferred_started_ns: Option<i128>,
    /// Tracks which SGR attributes the terminal should have active at the
    /// start of the next emit. Other frame-work (footer, body renders)
    /// between ticks can emit their own `\x1b[0m`, so state is restored
    /// by prefixing each emit with `\x1b[0m` + the active opens.
    sgr: SgrState,
}

impl AssistantPacer {
    pub fn deinit(&mut self) {
        self.pending.clear();
        self.deferred_turn = None;
        self.deferred_started_ns = None;
    }

    pub fn hasPending(&self) -> bool {
        !self.pending.is_empty() || self.deferred_turn.is_some()
    }

    pub fn hasCompletedAssistantPresentationTail(&self) -> bool {
        let finished = match &self.deferred_turn {
            Some(f) => f,
            None => return false,
        };
        finished.terminal_outcome == Some(TurnPresentationOutcome::Completed)
            && finished.summary.is_some()
            && matches!(finished.turn, HistoryTurn::Assistant(_))
    }

    pub fn completedAssistantPresentationTokenProgress(&self) -> Option<TurnTokenProgress> {
        if !self.hasCompletedAssistantPresentationTail() {
            return None;
        }
        Some(self.deferred_turn.as_ref()?.summary?.token_progress)
    }

    pub fn enqueue(&mut self, text: &[u8]) {
        if text.is_empty() {
            return;
        }
        self.pending.extend_from_slice(text);
    }

    pub fn pause(&mut self, _now_ns: i128) {}

    pub fn rethemeInlineCode(&mut self, light: bool) {
        let from: &[u8] = if light {
            b"\x1b[38;5;245m"
        } else {
            b"\x1b[38;5;247m"
        };
        let to: &[u8] = if light {
            b"\x1b[38;5;247m"
        } else {
            b"\x1b[38;5;245m"
        };

        let mut index: usize = 0;
        while index + from.len() <= self.pending.len() {
            if self.pending[index..].starts_with(from) {
                self.pending[index..index + to.len()].copy_from_slice(to);
                index += to.len();
            } else {
                index += 1;
            }
        }

        if self.sgr.code_fg != CodeForeground::None {
            self.sgr.code_fg = if light {
                CodeForeground::Light
            } else {
                CodeForeground::Dark
            };
        }
    }

    pub fn deferFinish(&mut self, finished: FinishedPrompt) -> bool {
        if self.pending.is_empty() {
            return false;
        }
        self.deferred_turn = Some(finished);
        self.deferred_started_ns = None;
        true
    }

    pub fn clear(&mut self) {
        self.pending.clear();
        self.deferred_turn = None;
        self.deferred_started_ns = None;
        self.sgr = SgrState::default();
    }

    pub fn discardDeferredPresentationForVisualEpoch(
        &mut self,
        callbacks: DeferredPresentationCallbacks,
    ) -> Result<bool, ()> {
        let finished = match self.deferred_turn.as_ref() {
            Some(f) => f,
            None => return Ok(true),
        };
        if (callbacks.append_history)(callbacks.ctx, finished)? == DeferredFinishCommit::Uncommitted {
            return Ok(false);
        }

        self.deferred_turn = None;
        self.pending.clear();
        self.deferred_started_ns = None;
        self.sgr = SgrState::default();
        Ok(true)
    }

    pub fn tick(&mut self, now_ns: i128, cb: TickCallbacks) -> Result<(), ()> {
        if self.pending.is_empty() {
            self.fireDeferredFinish(now_ns, cb)?;
            return Ok(());
        }

        if self.deferred_turn.is_some() && self.deferred_started_ns.is_none() {
            self.deferred_started_ns = Some(now_ns);
        }

        if self.emitPendingBlock(cb)? == FlushResult::Drained {
            self.fireDeferredFinish(now_ns, cb)?;
        }
        Ok(())
    }

    pub fn flushPresentationAtBoundary(
        &mut self,
        now_ns: i128,
        cb: TickCallbacks,
    ) -> Result<(), ()> {
        if !self.pending.is_empty() {
            match self.emitPendingBlock(cb)? {
                FlushResult::Drained => {}
                FlushResult::Blocked => self.neutralizeIncompleteTail(cb)?,
            }
        }
        self.fireDeferredFinish(now_ns, cb)
    }

    fn emitPendingBlock(&mut self, cb: TickCallbacks) -> Result<FlushResult, ()> {
        self.emitCompletePrefix(cb)?;
        if self.pending.is_empty() {
            Ok(FlushResult::Drained)
        } else {
            Ok(FlushResult::Blocked)
        }
    }

    fn neutralizeIncompleteTail(&mut self, cb: TickCallbacks) -> Result<(), ()> {
        let tail = self.pending.as_slice();
        assert!(!tail.is_empty());
        if tail[0] == 0x1b {
            let kind: &str = if tail.len() == 1 {
                "escape"
            } else if tail[1] == b'[' {
                "csi"
            } else if tail[1] == b']' {
                "osc"
            } else {
                "escape"
            };
            (cb.emit_fn)(cb.emit_ctx, b"\xef\xbf\xbd\x1b[0m")?;
            debug_trace::logf(
                "ui_activity",
                format_args!(
                    "neutralized incomplete assistant control tail kind={} bytes={}",
                    kind,
                    tail.len()
                ),
            );
        } else {
            (cb.emit_fn)(cb.emit_ctx, b"\xef\xbf\xbd\x1b[0m")?;
            debug_trace::logf(
                "ui_activity",
                format_args!(
                    "neutralized incomplete assistant utf8 tail bytes={}",
                    tail.len()
                ),
            );
        }

        self.pending.clear();
        self.sgr = SgrState::default();
        Ok(())
    }

    fn fireDeferredFinish(&mut self, now_ns: i128, cb: TickCallbacks) -> Result<(), ()> {
        if let Some(mut finished) = self.deferred_turn.take() {
            let started_ns = self.deferred_started_ns;
            self.deferred_started_ns = None;
            if let Some(summary) = finished.summary.as_mut() {
                let started = started_ns.unwrap_or(now_ns);
                if now_ns > started {
                    summary.turn_duration_ms +=
                        ((now_ns - started) / NANOS_PER_MS) as u64;
                }
            }
            (cb.finish_fn)(cb.finish_ctx, finished)
        } else {
            Ok(())
        }
    }

    fn emitCompletePrefix(&mut self, cb: TickCallbacks) -> Result<(), ()> {
        let i = self.emittablePrefixLen();
        if i == 0 {
            return Ok(());
        }

        // Restore tracked SGR state because other renderers may reset it between ticks.
        if self.sgr.isActive() {
            let mut prefix_buf = [0u8; 48];
            let reset = b"\x1b[0m";
            prefix_buf[..reset.len()].copy_from_slice(reset);
            let opens_len = self.sgr.writeOpens(&mut prefix_buf[reset.len()..]);
            (cb.emit_fn)(cb.emit_ctx, &prefix_buf[..reset.len() + opens_len])?;
        }

        let emitted: Vec<u8> = self.pending[..i].to_vec();
        (cb.emit_fn)(cb.emit_ctx, &emitted)?;
        self.updateSgrFromEmitted(&emitted);
        self.consume(i);
        Ok(())
    }

    fn emittablePrefixLen(&self) -> usize {
        let mut i: usize = 0;
        while i < self.pending.len() {
            let first = self.pending[i];
            if first == 0x1b {
                // Keep ANSI sequences atomic and hold incomplete tails for a later tick.
                let end = display_width::ansi_sequence_end(&self.pending, i);
                if end > i {
                    if !ansiSequenceComplete(&self.pending, i, end) {
                        break;
                    }
                    i = end;
                    continue;
                }
            }
            let len = match utf8_byte_sequence_length(first) {
                Ok(len) => len,
                Err(()) => 1,
            };
            if i + len > self.pending.len() {
                break;
            }
            i += len;
        }
        i
    }

    fn updateSgrFromEmitted(&mut self, bytes: &[u8]) {
        let mut idx: usize = 0;
        while idx < bytes.len() {
            if bytes[idx] == 0x1b {
                let end = display_width::ansi_sequence_end(bytes, idx);
                if end > idx {
                    self.sgr.apply(&bytes[idx..end]);
                }
                idx = if end > idx { end } else { idx + 1 };
                continue;
            }
            idx += 1;
        }
    }

    fn consume(&mut self, byte_count: usize) {
        self.pending.drain(..byte_count);
    }
}

fn ansiSequenceComplete(bytes: &[u8], start: usize, end: usize) -> bool {
    if end <= start + 1 {
        return false;
    }
    if start + 1 >= bytes.len() {
        return false;
    }
    let kind = bytes[start + 1];
    if kind == b'[' {
        if end <= start + 2 {
            return false;
        }
        let last = bytes[end - 1];
        return (b'@'..=b'~').contains(&last);
    }
    if kind == b']' {
        if end <= start + 2 {
            return false;
        }
        let last = bytes[end - 1];
        if last == 0x07 {
            return true;
        }
        if end >= start + 4 && bytes[end - 2] == 0x1b && last == b'\\' {
            return true;
        }
        return false;
    }
    end == start + 2
}

fn utf8_byte_sequence_length(first: u8) -> Result<usize, ()> {
    match first {
        0x00..=0x7f => Ok(1),
        0xc0..=0xdf => Ok(2),
        0xe0..=0xef => Ok(3),
        0xf0..=0xf7 => Ok(4),
        _ => Err(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::shared::types::{
        AssistantHistoryTurn, BackgroundCommandHistoryTurn, ExecutionMemory, FinishedPromptProjection,
        TurnSummary, UserTurn,
    };

    struct TestCapture {
        emitted: Vec<u8>,
        finish_count: usize,
        finish_summary: Option<TurnSummary>,
    }

    fn emit(ctx: *mut c_void, text: &[u8]) -> Result<(), ()> {
        let cap = unsafe { &mut *(ctx as *mut TestCapture) };
        cap.emitted.extend_from_slice(text);
        Ok(())
    }

    fn finish(ctx: *mut c_void, finished: FinishedPrompt) -> Result<(), ()> {
        let cap = unsafe { &mut *(ctx as *mut TestCapture) };
        cap.finish_count += 1;
        cap.finish_summary = finished.summary;
        Ok(())
    }

    impl TestCapture {
        fn new() -> Self {
            Self {
                emitted: Vec::new(),
                finish_count: 0,
                finish_summary: None,
            }
        }

        fn callbacks(&mut self) -> TickCallbacks {
            TickCallbacks {
                emit_ctx: self as *mut TestCapture as *mut c_void,
                emit_fn: emit,
                finish_ctx: self as *mut TestCapture as *mut c_void,
                finish_fn: finish,
            }
        }
    }

    fn makeAssistantTurn() -> HistoryTurn {
        HistoryTurn::Assistant(AssistantHistoryTurn {
            user: UserTurn {
                text: b"u".to_vec(),
                images: Vec::new(),
                work_id: None,
            },
            assistant: b"a".to_vec(),
            execution: ExecutionMemory::default(),
        })
    }

    fn makeBackgroundTurn() -> HistoryTurn {
        HistoryTurn::BackgroundCommand(BackgroundCommandHistoryTurn {
            user: UserTurn {
                text: b"u".to_vec(),
                images: Vec::new(),
                work_id: None,
            },
            assistant: None,
            execution: ExecutionMemory::default(),
            log_path: b"/tmp/fx-background.log".to_vec(),
            expect_url: false,
            url: None,
            background_record_id: None,
        })
    }

    fn finished(turn: &HistoryTurn) -> FinishedPrompt {
        FinishedPrompt {
            turn: turn.clone(),
            summary: None,
            terminal_projection: FinishedPromptProjection::default(),
            terminal_outcome: None,
        }
    }

    #[test]
    fn tick_emits_one_queued_rendered_block_atomically() {
        let mut pacer = AssistantPacer::default();
        let mut cap = TestCapture::new();

        let paragraph = "A complete rendered paragraph.\n\n";
        pacer.enqueue(paragraph.as_bytes());

        pacer.tick(0, cap.callbacks()).unwrap();

        assert_eq!(paragraph.as_bytes(), cap.emitted);
        assert!(!pacer.hasPending());
    }

    #[test]
    fn presentation_boundary_emits_the_queued_block_before_continuing() {
        let mut pacer = AssistantPacer::default();
        let mut cap = TestCapture::new();

        let paragraph = "Explanation before a tool.\n";
        pacer.enqueue(paragraph.as_bytes());

        pacer.flushPresentationAtBoundary(0, cap.callbacks()).unwrap();
        assert_eq!(paragraph.as_bytes(), cap.emitted);
        assert!(!pacer.hasPending());
    }

    #[test]
    fn tick_emits_a_complete_utf8_block() {
        let mut pacer = AssistantPacer::default();
        let mut cap = TestCapture::new();

        pacer.enqueue("áñ".as_bytes());

        pacer.tick(0, cap.callbacks()).unwrap();
        assert_eq!("áñ".as_bytes(), cap.emitted);
        assert!(!pacer.hasPending());
    }

    #[test]
    fn partial_utf8_at_buffer_tail_is_held_until_rest_arrives() {
        let mut pacer = AssistantPacer::default();
        let mut cap = TestCapture::new();

        pacer.enqueue(&[0xc3]);

        pacer.tick(0, cap.callbacks()).unwrap();
        assert_eq!(0, cap.emitted.len());
        assert_eq!(1, pacer.pending.len());

        pacer.enqueue(&[0xa1]);
        pacer.tick(1, cap.callbacks()).unwrap();
        assert_eq!("á".as_bytes(), cap.emitted);
    }

    #[test]
    fn large_rendered_block_emits_in_one_tick() {
        let mut pacer = AssistantPacer::default();
        let mut cap = TestCapture::new();

        let big = vec![b'x'; 1000];
        pacer.enqueue(&big);

        pacer.tick(0, cap.callbacks()).unwrap();
        assert_eq!(&big[..], &cap.emitted[..]);
        assert!(!pacer.hasPending());
    }

    #[test]
    fn defer_finish_returns_false_when_buffer_empty() {
        let mut pacer = AssistantPacer::default();

        let turn = makeAssistantTurn();

        let deferred = pacer.deferFinish(finished(&turn));
        assert!(!deferred);
        assert!(pacer.deferred_turn.is_none());
    }

    #[test]
    fn presentation_boundary_neutralizes_only_the_incomplete_ansi_or_utf8_tail() {
        {
            let mut pacer = AssistantPacer::default();
            let mut cap = TestCapture::new();

            pacer.enqueue(b"visible\x1b[1");
            pacer.tick(0, cap.callbacks()).unwrap();
            pacer.tick(100 * NANOS_PER_MS, cap.callbacks()).unwrap();
            assert_eq!(b"visible", cap.emitted.as_slice());
            pacer
                .flushPresentationAtBoundary(100 * NANOS_PER_MS, cap.callbacks())
                .unwrap();

            assert_eq!(b"visible\xef\xbf\xbd\x1b[0m", cap.emitted.as_slice());
            assert!(!pacer.hasPending());
            assert!(!pacer.sgr.isActive());
        }

        {
            let mut pacer = AssistantPacer::default();
            let mut cap = TestCapture::new();

            pacer.enqueue(&[0xc3]);
            pacer.flushPresentationAtBoundary(0, cap.callbacks()).unwrap();

            assert_eq!(b"\xef\xbf\xbd\x1b[0m", cap.emitted.as_slice());
            assert!(!pacer.hasPending());
        }

        {
            let mut pacer = AssistantPacer::default();
            let mut cap = TestCapture::new();

            pacer.enqueue(b"visible\x1b]52;c;unsafe");
            pacer.tick(0, cap.callbacks()).unwrap();
            pacer.tick(100 * NANOS_PER_MS, cap.callbacks()).unwrap();
            pacer
                .flushPresentationAtBoundary(100 * NANOS_PER_MS, cap.callbacks())
                .unwrap();

            assert_eq!(b"visible\xef\xbf\xbd\x1b[0m", cap.emitted.as_slice());
            assert!(!contains(&cap.emitted, b"52;c;unsafe"));
            assert!(!pacer.hasPending());
        }
    }

    #[test]
    fn presentation_boundary_preserves_callback_errors_and_pending_bytes() {
        let mut pacer = AssistantPacer::default();
        pacer.enqueue(b"\x1b[1");

        fn failing_emit(_: *mut c_void, _: &[u8]) -> Result<(), ()> {
            Err(())
        }
        fn failing_finish(_: *mut c_void, _: FinishedPrompt) -> Result<(), ()> {
            Ok(())
        }
        let mut ctx: u8 = 0;
        let callbacks = TickCallbacks {
            emit_ctx: &mut ctx as *mut u8 as *mut c_void,
            emit_fn: failing_emit,
            finish_ctx: &mut ctx as *mut u8 as *mut c_void,
            finish_fn: failing_finish,
        };

        assert!(pacer.flushPresentationAtBoundary(0, callbacks).is_err());
        assert_eq!(b"\x1b[1", pacer.pending.as_slice());
    }

    #[test]
    fn defer_finish_defers_and_fires_when_buffer_drains() {
        let mut pacer = AssistantPacer::default();
        let mut cap = TestCapture::new();

        pacer.enqueue(b"hi");

        let turn = makeAssistantTurn();

        let deferred = pacer.deferFinish(finished(&turn));
        assert!(deferred);

        pacer.tick(0, cap.callbacks()).unwrap();
        assert_eq!(b"hi", cap.emitted.as_slice());
        assert_eq!(1, cap.finish_count);
        assert!(pacer.deferred_turn.is_none());
    }

    #[test]
    fn defer_finish_carries_summary_only_after_pending_text_drains() {
        let mut pacer = AssistantPacer::default();
        let mut cap = TestCapture::new();

        pacer.enqueue(b"hi");

        let turn = makeAssistantTurn();
        let summary = TurnSummary {
            thinking_duration_ms: 15_000,
            turn_duration_ms: 130_000,
            token_progress: TurnTokenProgress {
                input_tokens: 10_000,
                output_tokens: 5_000,
                input_exact: false,
                output_exact: false,
            },
        };

        let mut f = finished(&turn);
        f.summary = Some(summary);
        let deferred = pacer.deferFinish(f);
        assert!(deferred);

        pacer.tick(0, cap.callbacks()).unwrap();
        assert_eq!(b"hi", cap.emitted.as_slice());
        assert_eq!(1, cap.finish_count);
        assert_eq!(5_000, cap.finish_summary.unwrap().token_progress.output_tokens);
        assert_eq!(130_000, cap.finish_summary.unwrap().turn_duration_ms);
    }

    #[test]
    fn presentation_boundary_emits_text_before_deferred_finish() {
        let mut pacer = AssistantPacer::default();
        let mut cap = TestCapture::new();

        pacer.enqueue(b"tail");
        let turn = makeAssistantTurn();
        let summary = TurnSummary {
            thinking_duration_ms: 1_000,
            turn_duration_ms: 2_000,
            token_progress: TurnTokenProgress {
                input_tokens: 0,
                output_tokens: 3,
                input_exact: false,
                output_exact: false,
            },
        };

        let mut f = finished(&turn);
        f.summary = Some(summary);
        f.terminal_outcome = Some(TurnPresentationOutcome::Completed);
        assert!(pacer.deferFinish(f));

        pacer.flushPresentationAtBoundary(0, cap.callbacks()).unwrap();
        assert_eq!(b"tail", cap.emitted.as_slice());
        assert_eq!(1, cap.finish_count);
        assert_eq!(3, cap.finish_summary.unwrap().token_progress.output_tokens);
        assert!(!pacer.hasPending());
    }

    #[test]
    fn completed_assistant_summary_is_the_only_deferred_presentation_tail() {
        let summary = TurnSummary {
            thinking_duration_ms: 1_000,
            turn_duration_ms: 2_000,
            token_progress: TurnTokenProgress {
                input_tokens: 50_000,
                output_tokens: 20_000,
                input_exact: false,
                output_exact: false,
            },
        };

        {
            let mut pacer = AssistantPacer::default();
            pacer.enqueue(b"tail");
            let turn = makeAssistantTurn();
            let mut f = finished(&turn);
            f.summary = Some(summary);
            f.terminal_outcome = Some(TurnPresentationOutcome::Completed);
            assert!(pacer.deferFinish(f));
            assert!(pacer.hasCompletedAssistantPresentationTail());
            assert_eq!(
                summary.token_progress,
                pacer.completedAssistantPresentationTokenProgress().unwrap()
            );
        }

        {
            let mut pacer = AssistantPacer::default();
            pacer.enqueue(b"error bytes");
            assert!(!pacer.hasCompletedAssistantPresentationTail());
        }

        {
            let mut pacer = AssistantPacer::default();
            pacer.enqueue(b"tail");
            let turn = makeAssistantTurn();
            let mut f = finished(&turn);
            f.terminal_outcome = Some(TurnPresentationOutcome::Completed);
            assert!(pacer.deferFinish(f));
            assert!(!pacer.hasCompletedAssistantPresentationTail());
        }

        {
            let mut pacer = AssistantPacer::default();
            pacer.enqueue(b"tail");
            let turn = makeAssistantTurn();
            let mut f = finished(&turn);
            f.summary = Some(summary);
            f.terminal_outcome = Some(TurnPresentationOutcome::Failed);
            assert!(pacer.deferFinish(f));
            assert!(!pacer.hasCompletedAssistantPresentationTail());
        }

        {
            let mut pacer = AssistantPacer::default();
            pacer.enqueue(b"tail");
            let turn = makeBackgroundTurn();
            let mut f = finished(&turn);
            f.summary = Some(summary);
            f.terminal_outcome = Some(TurnPresentationOutcome::Completed);
            assert!(pacer.deferFinish(f));
            assert!(!pacer.hasCompletedAssistantPresentationTail());
        }

        {
            let mut pacer = AssistantPacer::default();
            pacer.enqueue(b"tail");
            let turn = makeAssistantTurn();
            let mut f = finished(&turn);
            f.summary = Some(summary);
            f.terminal_outcome = Some(TurnPresentationOutcome::Interrupted);
            assert!(pacer.deferFinish(f));
            assert!(!pacer.hasCompletedAssistantPresentationTail());
        }
    }

    #[test]
    fn clear_drops_pending_and_deferred_turn() {
        let mut pacer = AssistantPacer::default();

        pacer.enqueue(b"lots of text");
        let turn = makeAssistantTurn();
        let _ = pacer.deferFinish(finished(&turn));

        assert!(pacer.hasPending());
        pacer.clear();
        assert!(!pacer.hasPending());
    }

    #[test]
    fn visual_epoch_discard_retains_uncommitted_deferred_finish_and_drops_committed_presentation() {
        let mut pacer = AssistantPacer::default();
        pacer.enqueue(b"stale presentation");
        let turn = makeAssistantTurn();
        assert!(pacer.deferFinish(finished(&turn)));

        struct Capture {
            outcome: DeferredFinishCommit,
            calls: usize,
        }

        fn append(ctx: *mut c_void, _: &FinishedPrompt) -> Result<DeferredFinishCommit, ()> {
            let cap = unsafe { &mut *(ctx as *mut Capture) };
            cap.calls += 1;
            Ok(cap.outcome)
        }

        let mut capture = Capture {
            outcome: DeferredFinishCommit::Uncommitted,
            calls: 0,
        };
        let callbacks = DeferredPresentationCallbacks {
            ctx: &mut capture as *mut Capture as *mut c_void,
            append_history: append,
        };

        assert!(!pacer
            .discardDeferredPresentationForVisualEpoch(callbacks)
            .unwrap());
        assert!(pacer.hasPending());
        assert_eq!(1, capture.calls);

        capture.outcome = DeferredFinishCommit::Committed;
        assert!(pacer
            .discardDeferredPresentationForVisualEpoch(callbacks)
            .unwrap());
        assert!(!pacer.hasPending());
        assert_eq!(2, capture.calls);
    }

    #[test]
    fn ansi_styled_block_emits_atomically() {
        let mut pacer = AssistantPacer::default();
        let mut cap = TestCapture::new();

        pacer.enqueue(b"\x1b[1mhi\x1b[22m!");

        pacer.tick(0, cap.callbacks()).unwrap();
        assert_eq!(b"\x1b[1mhi\x1b[22m!", cap.emitted.as_slice());
        assert_eq!(0, pacer.pending.len());
    }

    #[test]
    fn incomplete_ansi_sequence_at_tail_is_held_until_completion_arrives() {
        let mut pacer = AssistantPacer::default();
        let mut cap = TestCapture::new();

        pacer.enqueue(b"x\x1b[1");
        pacer.tick(0, cap.callbacks()).unwrap();
        pacer.tick(3_000_000, cap.callbacks()).unwrap();
        assert_eq!(b"x", cap.emitted.as_slice());
        assert_eq!(3, pacer.pending.len());

        pacer.enqueue(b"my");
        pacer.tick(100_000_000, cap.callbacks()).unwrap();
        assert_eq!(b"x\x1b[1my", cap.emitted.as_slice());
    }

    #[test]
    fn code_style_is_restored_across_rendered_blocks() {
        let mut pacer = AssistantPacer::default();
        let mut cap = TestCapture::new();

        pacer.enqueue(b"\x1b[38;5;245mcode");
        pacer.tick(0, cap.callbacks()).unwrap();
        pacer.enqueue(b"\x1b[39m done");
        pacer.tick(1, cap.callbacks()).unwrap();

        assert!(contains(
            &cap.emitted,
            b"code\x1b[0m\x1b[38;5;245m\x1b[39m done"
        ));
        assert!(contains(&cap.emitted, b"\x1b[39m"));
    }

    #[test]
    fn light_code_style_is_restored_across_rendered_blocks() {
        let mut pacer = AssistantPacer::default();
        let mut cap = TestCapture::new();

        pacer.enqueue(b"\x1b[38;5;247mcode");
        pacer.tick(0, cap.callbacks()).unwrap();
        pacer.enqueue(b"\x1b[39m done");
        pacer.tick(1, cap.callbacks()).unwrap();

        assert!(contains(
            &cap.emitted,
            b"code\x1b[0m\x1b[38;5;247m\x1b[39m done"
        ));
        assert!(contains(&cap.emitted, b"\x1b[39m"));
    }

    #[test]
    fn theme_change_retints_active_code_before_the_next_block() {
        let mut pacer = AssistantPacer::default();
        let mut cap = TestCapture::new();

        pacer.enqueue(b"\x1b[38;5;245mcode");
        pacer.tick(0, cap.callbacks()).unwrap();
        let emitted_before_theme_change = cap.emitted.len();

        pacer.rethemeInlineCode(true);
        pacer.enqueue(b"\x1b[39m");
        pacer.tick(1, cap.callbacks()).unwrap();

        let after_theme_change = &cap.emitted[emitted_before_theme_change..];
        assert!(contains(after_theme_change, b"\x1b[0m\x1b[38;5;247m"));
        assert!(!contains(after_theme_change, b"\x1b[38;5;245m"));
    }

    #[test]
    fn theme_change_retints_a_queued_inline_code_opener_before_it_is_emitted() {
        let mut pacer = AssistantPacer::default();
        let mut cap = TestCapture::new();

        pacer.enqueue(b"x\x1b[38;5;247mcode\x1b[39m");
        pacer.rethemeInlineCode(false);
        pacer.tick(0, cap.callbacks()).unwrap();

        assert!(contains(&cap.emitted, b"\x1b[38;5;245m"));
        assert!(!contains(&cap.emitted, b"\x1b[38;5;247m"));
    }

    #[test]
    fn underline_style_is_restored_across_rendered_blocks() {
        let mut pacer = AssistantPacer::default();
        let mut cap = TestCapture::new();

        pacer.enqueue(b"\x1b[4mHeading");
        pacer.tick(0, cap.callbacks()).unwrap();
        pacer.enqueue(b"\x1b[24m");
        pacer.tick(1, cap.callbacks()).unwrap();

        assert!(contains(&cap.emitted, b"Heading\x1b[0m\x1b[4m\x1b[24m"));

        let mut sgr = SgrState::default();
        sgr.apply(b"\x1b[4m");
        assert!(sgr.isActive());
        let mut opens = [0u8; 16];
        let opens_len = sgr.writeOpens(&mut opens);
        assert_eq!(b"\x1b[4m", &opens[..opens_len]);
        sgr.apply(b"\x1b[24m");
        assert!(!sgr.isActive());
    }

    #[test]
    fn no_sgr_prefix_emitted_when_state_is_clean() {
        let mut pacer = AssistantPacer::default();
        let mut cap = TestCapture::new();

        pacer.enqueue(b"hello");
        pacer.tick(0, cap.callbacks()).unwrap();
        pacer.tick(100_000_000, cap.callbacks()).unwrap();
        assert_eq!(b"hello", cap.emitted.as_slice());
    }

    #[test]
    fn sgr_state_clears_after_closing_code() {
        let mut pacer = AssistantPacer::default();
        let mut cap = TestCapture::new();

        pacer.enqueue(b"\x1b[1mA\x1b[22mB");
        pacer.tick(0, cap.callbacks()).unwrap();
        assert!(!pacer.sgr.bold);
    }

    #[test]
    fn tick_on_empty_pacer_without_deferred_finish_is_a_no_op() {
        let mut pacer = AssistantPacer::default();
        let mut cap = TestCapture::new();

        pacer.tick(0, cap.callbacks()).unwrap();
        pacer.tick(1_000_000_000, cap.callbacks()).unwrap();
        assert_eq!(0, cap.emitted.len());
        assert_eq!(0, cap.finish_count);
    }

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        if needle.is_empty() {
            return true;
        }
        haystack.windows(needle.len()).any(|w| w == needle)
    }
}
