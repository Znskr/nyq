use crate::core::shared::debug_trace;

const PASTE_END_MARKER: &[u8] = b"\x1b[201~";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Owner {
    #[default]
    None,
    Composer,
    DecisionPrompt,
    QuestionFreeform,
    ApprovalAmendment,
    AuthCode,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InputLimits {
    pub composer_bytes: usize,
    pub decision_bytes: usize,
}

impl Default for InputLimits {
    fn default() -> Self {
        InputLimits {
            composer_bytes: 8 * 1024 * 1024,
            decision_bytes: 4096,
        }
    }
}

impl InputLimits {
    pub fn single(max_bytes: usize) -> InputLimits {
        InputLimits {
            composer_bytes: max_bytes,
            decision_bytes: max_bytes,
        }
    }

    pub fn for_owner(self, owner: Owner) -> usize {
        match owner {
            Owner::Composer => self.composer_bytes,
            Owner::None
            | Owner::DecisionPrompt
            | Owner::QuestionFreeform
            | Owner::ApprovalAmendment
            | Owner::AuthCode => self.decision_bytes,
        }
    }
}

pub const default_input_limits: InputLimits = InputLimits {
    composer_bytes: 8 * 1024 * 1024,
    decision_bytes: 4096,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FinalizeFailure {
    pub owner: Owner,
    pub error_name: &'static str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResetReason {
    DecisionPromptActive,
    SessionReset,
    Shutdown,
    FinalizeFailed(FinalizeFailure),
    InputLimit(Owner),
    InvalidUtf8,
    NewPasteStaleState,
    UnsafeSuffix,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BoundaryState {
    #[default]
    Capturing,
    EndCandidate,
    Unsafe,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Settlement {
    #[default]
    None,
    Finish,
    Reject,
}

#[derive(Debug)]
pub struct State {
    pub owner: Owner,
    pub boundary: BoundaryState,
    pub unsafe_suffix_bytes: usize,
    pub end_match_len: usize,
    pub end_candidate_buffer_len: usize,
    pub end_candidate_overflow_bytes: usize,
    pub overflow_bytes: usize,
    pub buffer_limit: usize,
    pub decision_bytes: usize,
    pub buffer: Vec<u8>,
}

impl Default for State {
    fn default() -> Self {
        State {
            owner: Owner::None,
            boundary: BoundaryState::Capturing,
            unsafe_suffix_bytes: 0,
            end_match_len: 0,
            end_candidate_buffer_len: 0,
            end_candidate_overflow_bytes: 0,
            overflow_bytes: 0,
            buffer_limit: usize::MAX,
            decision_bytes: 0,
            buffer: Vec::new(),
        }
    }
}

impl State {
    pub fn deinit(&mut self) {
        self.reset_with_trace(ResetReason::Shutdown);
        self.buffer = Vec::new();
    }

    pub fn active(&self) -> bool {
        self.owner != Owner::None
    }

    pub fn begin(&mut self, owner: Owner, max_buffer_len: usize) {
        self.reset_with_trace(ResetReason::NewPasteStaleState);
        self.owner = owner;
        self.buffer_limit = max_buffer_len;
    }

    pub fn consume_byte(&mut self, byte: u8) {
        match self.boundary {
            BoundaryState::Capturing => {}
            BoundaryState::EndCandidate => {
                self.boundary = BoundaryState::Unsafe;
                self.unsafe_suffix_bytes = self.unsafe_suffix_bytes.saturating_add(1);
                return;
            }
            BoundaryState::Unsafe => {
                self.unsafe_suffix_bytes = self.unsafe_suffix_bytes.saturating_add(1);
                return;
            }
        }

        match self.owner {
            Owner::None => return,
            Owner::DecisionPrompt => {
                self.decision_bytes = self.decision_bytes.saturating_add(1);
            }
            Owner::Composer
            | Owner::QuestionFreeform
            | Owner::ApprovalAmendment
            | Owner::AuthCode => {
                if byte == 0x1b {
                    self.end_candidate_buffer_len = self.buffer.len();
                    self.end_candidate_overflow_bytes = self.overflow_bytes;
                }
                let captured_byte: u8 =
                    if self.owner == Owner::QuestionFreeform && byte == b'\t' {
                        b' '
                    } else {
                        byte
                    };
                let is_printable = captured_byte >= 32 && captured_byte != 127;
                let accepts_byte = match self.owner {
                    Owner::Composer => {
                        captured_byte == b'\r'
                            || captured_byte == b'\n'
                            || captured_byte == b'\t'
                            || is_printable
                    }
                    Owner::QuestionFreeform => {
                        captured_byte == b'\r' || captured_byte == b'\n' || is_printable
                    }
                    Owner::ApprovalAmendment => {
                        captured_byte == b'\r'
                            || captured_byte == b'\n'
                            || captured_byte == b'\t'
                            || is_printable
                    }
                    Owner::AuthCode => is_printable,
                    Owner::None | Owner::DecisionPrompt => false,
                };
                if accepts_byte {
                    if self.buffer.len() < self.buffer_limit {
                        self.buffer.push(captured_byte);
                    } else {
                        self.overflow_bytes = self.overflow_bytes.saturating_add(1);
                    }
                }
            }
        }

        if !self.advance_end_matcher(byte) {
            return;
        }

        match self.owner {
            Owner::None => {}
            Owner::DecisionPrompt => {
                self.decision_bytes = self.decision_bytes.saturating_sub(PASTE_END_MARKER.len());
            }
            Owner::Composer
            | Owner::QuestionFreeform
            | Owner::ApprovalAmendment
            | Owner::AuthCode => {
                self.buffer
                    .truncate(self.buffer.len().min(self.end_candidate_buffer_len));
                self.overflow_bytes = self.end_candidate_overflow_bytes;
            }
        }
        self.end_match_len = 0;
        self.end_candidate_buffer_len = 0;
        self.end_candidate_overflow_bytes = 0;
        self.boundary = BoundaryState::EndCandidate;
    }

    pub fn settle_delivery_epoch(&self) -> Settlement {
        if !self.active() {
            return Settlement::None;
        }
        match self.boundary {
            BoundaryState::Capturing => Settlement::None,
            BoundaryState::EndCandidate => Settlement::Finish,
            BoundaryState::Unsafe => {
                if self.confirmed_overflow_bytes() > 0 {
                    Settlement::Finish
                } else {
                    Settlement::Reject
                }
            }
        }
    }

    pub fn byte_count(&self) -> usize {
        match self.owner {
            Owner::None => 0,
            Owner::DecisionPrompt => self.decision_bytes,
            Owner::Composer
            | Owner::QuestionFreeform
            | Owner::ApprovalAmendment
            | Owner::AuthCode => self.buffer.len(),
        }
    }

    pub fn attempted_bytes(&self) -> usize {
        self.buffer.len().saturating_add(self.overflow_bytes)
    }

    pub fn observed_bytes(&self) -> usize {
        self.attempted_bytes()
            .saturating_add(self.decision_bytes)
            .saturating_add(self.unsafe_suffix_bytes)
    }

    pub fn confirmed_overflow_bytes(&self) -> usize {
        if self.end_match_len > 0 {
            self.end_candidate_overflow_bytes
        } else {
            self.overflow_bytes
        }
    }

    /// Ends framing while retaining captured bytes for synchronous handling.
    pub fn begin_handling(&mut self) {
        self.owner = Owner::None;
        self.boundary = BoundaryState::Capturing;
        self.unsafe_suffix_bytes = 0;
        self.end_match_len = 0;
        self.end_candidate_buffer_len = 0;
        self.end_candidate_overflow_bytes = 0;
    }

    /// Stops retaining payload bytes while preserving end-marker detection.
    pub fn continue_discarding(&mut self) {
        self.owner = Owner::DecisionPrompt;
        self.decision_bytes = 0;
    }

    /// Clears a capture after its bytes have been handled successfully.
    pub fn finish_handled(&mut self) {
        self.clear_retaining_capacity();
    }

    pub fn finish_secret_handled(&mut self) {
        self.clear_retaining_capacity();
        let capacity = self.buffer.capacity();
        if capacity > 0 {
            self.buffer.resize(capacity, 0);
            self.buffer.clear();
        }
    }

    pub fn reset_secret_with_trace(&mut self, reason: ResetReason) {
        debug_assert_eq!(self.owner, Owner::AuthCode);
        self.reset_with_trace(reason);
    }

    /// Logs any discarded capture before returning to the inactive state.
    pub fn reset_with_trace(&mut self, reason: ResetReason) {
        let clear_secret = self.owner == Owner::AuthCode;
        match reason {
            ResetReason::FinalizeFailed(failure) => {
                if !self.buffer.is_empty() {
                    debug_trace::logf(
                        "input",
                        format_args!(
                            "{} paste dropped bytes={} reason=finalize_failed error={}",
                            owner_label(failure.owner),
                            self.buffer.len(),
                            failure.error_name,
                        ),
                    );
                }
            }
            ResetReason::InputLimit(owner) => {
                let dropped_bytes = if owner == Owner::Composer {
                    self.attempted_bytes()
                } else {
                    self.buffer.len()
                };
                debug_trace::logf(
                    "input",
                    format_args!(
                        "{} paste dropped bytes={} reason=input_limit",
                        owner_label(owner),
                        dropped_bytes,
                    ),
                );
            }
            ResetReason::InvalidUtf8 => {
                if !self.buffer.is_empty() {
                    debug_trace::logf(
                        "input",
                        format_args!(
                            "{} paste dropped bytes={} reason=invalid_utf8",
                            owner_label(self.owner),
                            self.buffer.len(),
                        ),
                    );
                }
            }
            ResetReason::NewPasteStaleState => {
                if self.decision_bytes > 0 {
                    debug_trace::logf(
                        "input",
                        format_args!(
                            "decision prompt paste dropped bytes={} reason=new_paste_stale_state",
                            self.decision_bytes,
                        ),
                    );
                }
                if !self.buffer.is_empty() {
                    debug_trace::logf(
                        "input",
                        format_args!(
                            "{} paste dropped bytes={} reason=new_paste_stale_state",
                            owner_label(self.owner),
                            self.buffer.len(),
                        ),
                    );
                }
            }
            ResetReason::UnsafeSuffix => {
                debug_trace::logf(
                    "input",
                    format_args!(
                        "{} paste dropped bytes={} reason=unsafe_suffix",
                        owner_label(self.owner),
                        self.observed_bytes(),
                    ),
                );
            }
            ResetReason::DecisionPromptActive
            | ResetReason::SessionReset
            | ResetReason::Shutdown => match self.owner {
                Owner::DecisionPrompt => {
                    debug_trace::logf(
                        "input",
                        format_args!(
                            "decision prompt paste dropped bytes={} reason={}",
                            self.decision_bytes,
                            reset_reason_name(reason),
                        ),
                    );
                }
                Owner::Composer
                | Owner::QuestionFreeform
                | Owner::ApprovalAmendment
                | Owner::AuthCode => {
                    if !self.buffer.is_empty() {
                        debug_trace::logf(
                            "input",
                            format_args!(
                                "{} paste dropped bytes={} reason={}",
                                owner_label(self.owner),
                                self.buffer.len(),
                                reset_reason_name(reason),
                            ),
                        );
                    }
                }
                Owner::None => {}
            },
        }

        self.clear_retaining_capacity();
        if clear_secret && self.buffer.capacity() > 0 {
            let capacity = self.buffer.capacity();
            self.buffer.resize(capacity, 0);
            self.buffer.clear();
        }
    }

    fn clear_retaining_capacity(&mut self) {
        self.owner = Owner::None;
        self.boundary = BoundaryState::Capturing;
        self.unsafe_suffix_bytes = 0;
        self.end_match_len = 0;
        self.end_candidate_buffer_len = 0;
        self.end_candidate_overflow_bytes = 0;
        self.overflow_bytes = 0;
        self.buffer_limit = usize::MAX;
        self.decision_bytes = 0;
        self.buffer.clear();
    }

    fn advance_end_matcher(&mut self, byte: u8) -> bool {
        if byte == 0x1b {
            self.end_match_len = 1;
            return false;
        }
        if self.end_match_len == 0 {
            return false;
        }
        if byte != PASTE_END_MARKER[self.end_match_len] {
            self.end_match_len = 0;
            return false;
        }

        self.end_match_len += 1;
        self.end_match_len == PASTE_END_MARKER.len()
    }
}

fn reset_reason_name(reason: ResetReason) -> &'static str {
    match reason {
        ResetReason::DecisionPromptActive => "decision_prompt_active",
        ResetReason::SessionReset => "session_reset",
        ResetReason::Shutdown => "shutdown",
        ResetReason::FinalizeFailed(_) => "finalize_failed",
        ResetReason::InputLimit(_) => "input_limit",
        ResetReason::InvalidUtf8 => "invalid_utf8",
        ResetReason::NewPasteStaleState => "new_paste_stale_state",
        ResetReason::UnsafeSuffix => "unsafe_suffix",
    }
}

fn owner_label(owner: Owner) -> &'static str {
    match owner {
        Owner::None | Owner::Composer => "composer",
        Owner::DecisionPrompt => "decision prompt",
        Owner::QuestionFreeform => "question freeform",
        Owner::ApprovalAmendment => "approval amendment",
        Owner::AuthCode => "authorization code",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static TRACE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        if needle.is_empty() {
            return true;
        }
        haystack.windows(needle.len()).any(|w| w == needle)
    }

    fn count_occurrences(haystack: &[u8], needle: &[u8]) -> usize {
        if needle.is_empty() {
            return 0;
        }
        haystack.windows(needle.len()).filter(|w| *w == needle).count()
    }

    #[test]
    fn authorization_code_paste_zeroes_retained_capacity_after_handling_and_reset() {
        let mut state = State::default();

        state.begin(Owner::AuthCode, 64);
        for &byte in b"temporary-code" {
            state.consume_byte(byte);
        }
        assert!(state.buffer.capacity() > 0);
        assert_eq!(b't', state.buffer[0]);
        state.begin_handling();
        state.finish_secret_handled();
        // `finish_secret_handled` zeroes the retained capacity (via
        // resize+clear); safe Rust cannot observe the wiped region, so assert
        // the emptied buffer and inactive framing here.
        assert!(state.buffer.is_empty());

        state.begin(Owner::AuthCode, 64);
        for &byte in b"second-code" {
            state.consume_byte(byte);
        }
        state.reset_secret_with_trace(ResetReason::SessionReset);
        assert!(state.buffer.is_empty());
    }

    #[test]
    fn decision_paste_counts_content_bytes_and_exact_end_marker_only() {
        let mut state = State::default();
        state.begin(Owner::DecisionPrompt, usize::MAX);

        for &byte in b"abc" {
            state.consume_byte(byte);
        }
        for &byte in &PASTE_END_MARKER[..PASTE_END_MARKER.len() - 1] {
            state.consume_byte(byte);
        }
        state.consume_byte(PASTE_END_MARKER[PASTE_END_MARKER.len() - 1]);
        assert_eq!(3, state.decision_bytes);
        assert_eq!(Owner::DecisionPrompt, state.owner);
        assert_eq!(BoundaryState::EndCandidate, state.boundary);
        assert_eq!(Settlement::Finish, state.settle_delivery_epoch());
    }

    #[test]
    fn decision_paste_treats_incomplete_and_parameterized_end_candidates_as_content() {
        let mut state = State::default();
        state.begin(Owner::DecisionPrompt, usize::MAX);

        let payload = b"\x1b[20\x1b[201;1~x";
        for &byte in payload {
            state.consume_byte(byte);
        }
        for &byte in PASTE_END_MARKER {
            state.consume_byte(byte);
        }
        assert_eq!(payload.len(), state.decision_bytes);
        assert_eq!(Settlement::Finish, state.settle_delivery_epoch());
    }

    #[test]
    fn composer_paste_preserves_safe_bytes_and_tracks_bounded_overflow() {
        let mut state = State::default();
        state.begin(Owner::Composer, 3);

        for &byte in b"abcd\x01" {
            state.consume_byte(byte);
        }
        for &byte in PASTE_END_MARKER {
            state.consume_byte(byte);
        }
        assert_eq!(b"abc", state.buffer.as_slice());
        assert_eq!(1, state.overflow_bytes);
        assert_eq!(4, state.attempted_bytes());
    }

    #[test]
    fn composer_overflow_excludes_a_possible_exact_end_marker() {
        let mut exact = State::default();
        exact.begin(Owner::Composer, 1);

        exact.consume_byte(b'x');
        for &byte in &PASTE_END_MARKER[..PASTE_END_MARKER.len() - 1] {
            exact.consume_byte(byte);
            assert_eq!(0, exact.confirmed_overflow_bytes());
        }
        exact.consume_byte(PASTE_END_MARKER[PASTE_END_MARKER.len() - 1]);
        assert_eq!(0, exact.confirmed_overflow_bytes());

        let mut mismatch = State::default();
        mismatch.begin(Owner::Composer, 1);

        mismatch.consume_byte(b'x');
        for &byte in b"\x1b[20x" {
            mismatch.consume_byte(byte);
        }
        assert!(mismatch.confirmed_overflow_bytes() > 0);
    }

    #[test]
    fn composer_overflow_rejection_takes_precedence_over_an_unsafe_suffix() {
        let mut state = State::default();
        state.begin(Owner::Composer, 1);

        for &byte in b"xy" {
            state.consume_byte(byte);
        }
        for &byte in PASTE_END_MARKER {
            state.consume_byte(byte);
        }
        state.consume_byte(b'z');

        assert_eq!(BoundaryState::Unsafe, state.boundary);
        assert!(state.confirmed_overflow_bytes() > 0);
        assert_eq!(Settlement::Finish, state.settle_delivery_epoch());
    }

    #[test]
    fn discarding_remainder_preserves_buffered_bytes_and_end_marker_progress() {
        let mut state = State::default();
        state.begin(Owner::Composer, usize::MAX);

        state.consume_byte(b'x');
        state.consume_byte(0x1b);
        state.continue_discarding();
        for &byte in &PASTE_END_MARKER[1..] {
            state.consume_byte(byte);
        }

        assert_eq!(Settlement::Finish, state.settle_delivery_epoch());
        assert_eq!(b"x", state.buffer.as_slice());
        assert_eq!(0, state.decision_bytes);
    }

    #[test]
    fn begin_handling_makes_framing_inactive_and_retains_captured_bytes() {
        let mut state = State::default();
        state.begin(Owner::Composer, usize::MAX);
        state.buffer.extend_from_slice(b"captured");
        state.end_match_len = 2;

        state.begin_handling();

        assert!(!state.active());
        assert_eq!(b"captured", state.buffer.as_slice());
        assert_eq!(0, state.end_match_len);
    }

    #[test]
    fn question_and_approval_paste_keep_their_owner_specific_safe_bytes() {
        let mut question = State::default();
        question.begin(Owner::QuestionFreeform, usize::MAX);
        for &byte in b"one\r\ttwo\x03".iter().chain(PASTE_END_MARKER.iter()) {
            question.consume_byte(byte);
        }
        assert_eq!(b"one\r two", question.buffer.as_slice());

        let mut amendment = State::default();
        amendment.begin(Owner::ApprovalAmendment, usize::MAX);
        let mut payload: Vec<u8> = Vec::new();
        payload.extend_from_slice(b"pasted amendment marker ");
        payload.extend_from_slice(b"\\x1b[200~");
        payload.extend_from_slice(b"\x1b[200~");
        payload.extend_from_slice(b"\x1b[201;1~");
        payload.extend_from_slice(b"\t\r\x03");
        for &byte in payload.iter().chain(PASTE_END_MARKER.iter()) {
            amendment.consume_byte(byte);
        }
        assert_eq!(
            "pasted amendment marker \\x1b[200~[200~[201;1~\t\r".as_bytes(),
            amendment.buffer.as_slice(),
        );
    }

    #[test]
    fn capturing_remains_active_when_a_delivery_epoch_ends_before_the_marker() {
        let mut state = State::default();
        state.begin(Owner::Composer, usize::MAX);
        state.consume_byte(b'x');

        assert!(state.active());
        assert_eq!(BoundaryState::Capturing, state.boundary);
        assert_eq!(Settlement::None, state.settle_delivery_epoch());
    }

    #[test]
    fn end_candidate_stays_paste_owned_until_delivery_epoch_settlement() {
        let mut state = State::default();
        state.begin(Owner::Composer, usize::MAX);
        for &byte in b"safe".iter().chain(PASTE_END_MARKER.iter()) {
            state.consume_byte(byte);
        }

        assert!(state.active());
        assert_eq!(b"safe", state.buffer.as_slice());
        assert_eq!(BoundaryState::EndCandidate, state.boundary);
        assert_eq!(Settlement::Finish, state.settle_delivery_epoch());
    }

    #[test]
    fn any_same_epoch_suffix_makes_the_candidate_sticky_unsafe() {
        let mut state = State::default();
        state.begin(Owner::Composer, usize::MAX);
        for &byte in b"safe".iter().chain(PASTE_END_MARKER.iter()) {
            state.consume_byte(byte);
        }
        state.consume_byte(b'\r');
        for &byte in PASTE_END_MARKER {
            state.consume_byte(byte);
        }

        assert!(state.active());
        assert_eq!(b"safe", state.buffer.as_slice());
        assert_eq!(BoundaryState::Unsafe, state.boundary);
        assert_eq!(1 + PASTE_END_MARKER.len(), state.unsafe_suffix_bytes);
        assert_eq!(Settlement::Reject, state.settle_delivery_epoch());
    }

    #[test]
    fn traced_reset_reports_dropped_captures_and_clears_only_paste_state() {
        let _lock = TRACE_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!(
            "nyq-paste-{}",
            crate::core::shared::io::nano_timestamp()
        ));
        std::fs::create_dir(&dir).unwrap();
        let root = std::fs::canonicalize(&dir).unwrap();
        let trace_path = root.join("paste-reset.log");

        debug_trace::reset_for_test();
        debug_trace::configure_for_test_with_scopes(&trace_path, "input").unwrap();

        let mut decision = State::default();
        decision.begin(Owner::DecisionPrompt, usize::MAX);
        decision.reset_with_trace(ResetReason::SessionReset);

        let mut composer = State::default();
        composer.begin(Owner::Composer, usize::MAX);
        composer.buffer.extend_from_slice(b"buffered");
        composer.reset_with_trace(ResetReason::Shutdown);

        let mut stale = State::default();
        stale.decision_bytes = 3;
        stale.buffer.extend_from_slice(b"old");
        stale.reset_with_trace(ResetReason::NewPasteStaleState);

        let mut failed = State::default();
        failed.buffer.extend_from_slice(b"pending");
        failed.reset_with_trace(ResetReason::FinalizeFailed(FinalizeFailure {
            owner: Owner::Composer,
            error_name: "OutOfMemory",
        }));

        debug_trace::shutdown();
        let trace = std::fs::read(&trace_path).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();

        assert!(contains(
            &trace,
            b"decision prompt paste dropped bytes=0 reason=session_reset"
        ));
        assert!(contains(&trace, b"composer paste dropped bytes=8 reason=shutdown"));
        assert!(contains(
            &trace,
            b"decision prompt paste dropped bytes=3 reason=new_paste_stale_state"
        ));
        assert!(contains(
            &trace,
            b"composer paste dropped bytes=3 reason=new_paste_stale_state"
        ));
        assert!(contains(
            &trace,
            b"composer paste dropped bytes=7 reason=finalize_failed error=OutOfMemory"
        ));
    }

    #[test]
    fn deinit_traces_an_active_capture_once() {
        let _lock = TRACE_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!(
            "nyq-paste-{}",
            crate::core::shared::io::nano_timestamp()
        ));
        std::fs::create_dir(&dir).unwrap();
        let root = std::fs::canonicalize(&dir).unwrap();
        let trace_path = root.join("paste-deinit.log");

        debug_trace::reset_for_test();
        debug_trace::configure_for_test_with_scopes(&trace_path, "input").unwrap();

        {
            let mut decision = State::default();
            decision.begin(Owner::DecisionPrompt, usize::MAX);
            decision.deinit();
        }

        {
            let mut composer = State::default();
            composer.begin(Owner::Composer, usize::MAX);
            composer.buffer.extend_from_slice(b"pending");
            composer.deinit();
        }

        debug_trace::shutdown();
        let trace = std::fs::read(&trace_path).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();

        assert_eq!(
            1,
            count_occurrences(
                &trace,
                b"decision prompt paste dropped bytes=0 reason=shutdown"
            )
        );
        assert_eq!(
            1,
            count_occurrences(&trace, b"composer paste dropped bytes=7 reason=shutdown")
        );
    }
}
