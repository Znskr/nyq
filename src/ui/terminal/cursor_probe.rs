#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Position {
    pub row: u16,
    pub col: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResponseParseError {
    CursorPositionUnavailable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CursorProbeError {
    CursorProbeBusy,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Protocol {
    #[default]
    Private,
    AnsiTagged,
}

#[allow(non_upper_case_globals)]
pub const first_tag_column: u16 = 1;
#[allow(non_upper_case_globals)]
pub const confirmation_tag_column: u16 = 2;
const MAX_RESPONSE_BYTES: usize = 16;
const MAX_CANDIDATE_BYTES: usize = 64;
const MAX_DEFERRED_BYTES: usize = MAX_RESPONSE_BYTES + MAX_CANDIDATE_BYTES;
const MAX_FORWARD_BYTES: usize = MAX_DEFERRED_BYTES + 1;
#[allow(non_upper_case_globals)]
pub const response_idle_timeout_ms: i64 = 100;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ForwardBytes {
    pub storage: [u8; MAX_FORWARD_BYTES],
    pub len: u8,
}

impl Default for ForwardBytes {
    fn default() -> Self {
        Self {
            storage: [0; MAX_FORWARD_BYTES],
            len: 0,
        }
    }
}

impl ForwardBytes {
    pub fn slice(&self) -> &[u8] {
        &self.storage[..usize::from(self.len)]
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FeedResult {
    Pending,
    Forward(ForwardBytes),
    Position(Position),
    LateResponse,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PollResult {
    None,
    ProbeTimedOut,
    LateWindowExpired,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Mode {
    #[default]
    Idle,
    Waiting,
    DiscardingLate,
    WaitingForPasteEnd,
    DiscardingLateForPasteEnd,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CandidateStatus {
    Prefix,
    Complete(Position),
    Invalid,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Parser {
    mode: Mode,
    protocol: Protocol,
    deadline_ms: i64,
    candidate: [u8; MAX_CANDIDATE_BYTES],
    candidate_len: u8,
    pending_response: [u8; MAX_RESPONSE_BYTES],
    pending_response_len: u8,
    pending_position: Option<Position>,
    deferred: [u8; MAX_DEFERRED_BYTES],
    deferred_start: u8,
    deferred_len: u8,
    deferred_input_in_flight: bool,
}

impl Default for Parser {
    fn default() -> Self {
        Self {
            mode: Mode::Idle,
            protocol: Protocol::Private,
            deadline_ms: 0,
            candidate: [0; MAX_CANDIDATE_BYTES],
            candidate_len: 0,
            pending_response: [0; MAX_RESPONSE_BYTES],
            pending_response_len: 0,
            pending_position: None,
            deferred: [0; MAX_DEFERRED_BYTES],
            deferred_start: 0,
            deferred_len: 0,
            deferred_input_in_flight: false,
        }
    }
}

impl Parser {
    pub fn begin(&mut self, protocol: Protocol, deadline_ms: i64) -> Result<(), CursorProbeError> {
        if !self.can_begin() {
            return Err(CursorProbeError::CursorProbeBusy);
        }
        self.mode = Mode::Waiting;
        self.protocol = protocol;
        self.deadline_ms = deadline_ms;
        self.candidate_len = 0;
        self.pending_response_len = 0;
        self.pending_position = None;
        Ok(())
    }

    pub fn cancel(&mut self) {
        self.defer_probe_bytes();
        self.mode = Mode::Idle;
        self.deadline_ms = 0;
    }

    pub fn can_begin(&self) -> bool {
        self.mode == Mode::Idle
            && self.deferred_start == self.deferred_len
            && !self.deferred_input_in_flight
    }

    pub fn intercepts_input(&self) -> bool {
        self.mode != Mode::Idle
    }

    pub fn has_pending_input(&self) -> bool {
        self.candidate_len > 0
            || self.pending_response_len > 0
            || self.deferred_start < self.deferred_len
            || self.deferred_input_in_flight
    }

    pub fn note_input_activity(&mut self, now_ms: i64) {
        if matches!(self.mode, Mode::Waiting | Mode::DiscardingLate) {
            self.deadline_ms = add_millis(now_ms, response_idle_timeout_ms);
        }
    }

    pub fn suspend_for_paste(&mut self) -> bool {
        let interrupted_measurement = match self.mode {
            Mode::Idle | Mode::WaitingForPasteEnd | Mode::DiscardingLateForPasteEnd => {
                return false
            }
            Mode::Waiting => {
                self.mode = Mode::WaitingForPasteEnd;
                true
            }
            Mode::DiscardingLate => {
                self.mode = Mode::DiscardingLateForPasteEnd;
                false
            }
        };
        self.deadline_ms = 0;
        interrupted_measurement
    }

    pub fn resume_after_paste(&mut self, now_ms: i64) -> bool {
        let resumed_measurement = match self.mode {
            Mode::WaitingForPasteEnd => {
                self.mode = Mode::Waiting;
                true
            }
            Mode::DiscardingLateForPasteEnd => {
                self.mode = Mode::DiscardingLate;
                false
            }
            Mode::Idle | Mode::Waiting | Mode::DiscardingLate => return false,
        };
        self.deadline_ms = add_millis(now_ms, response_idle_timeout_ms);
        resumed_measurement
    }

    pub fn poll(&mut self, now_ms: i64) -> PollResult {
        if matches!(
            self.mode,
            Mode::Idle | Mode::WaitingForPasteEnd | Mode::DiscardingLateForPasteEnd
        ) || now_ms < self.deadline_ms
        {
            return PollResult::None;
        }
        match self.mode {
            Mode::Waiting => {
                self.defer_probe_bytes();
                self.mode = Mode::DiscardingLate;
                self.deadline_ms = add_millis(now_ms, response_idle_timeout_ms);
                PollResult::ProbeTimedOut
            }
            Mode::DiscardingLate => {
                self.defer_probe_bytes();
                self.mode = Mode::Idle;
                self.deadline_ms = 0;
                PollResult::LateWindowExpired
            }
            Mode::Idle | Mode::WaitingForPasteEnd | Mode::DiscardingLateForPasteEnd => {
                unreachable!()
            }
        }
    }

    pub fn expire(&mut self, now_ms: i64) -> bool {
        if self.mode != Mode::Waiting {
            return false;
        }
        self.defer_probe_bytes();
        self.mode = Mode::DiscardingLate;
        self.deadline_ms = add_millis(now_ms, response_idle_timeout_ms);
        true
    }

    pub fn feed(&mut self, byte: u8) -> FeedResult {
        if !self.intercepts_input() {
            return forward_byte(byte);
        }
        match self.protocol {
            Protocol::Private => self.feed_private(byte),
            Protocol::AnsiTagged => self.feed_ansi_tagged(byte),
        }
    }

    pub fn take_deferred_byte(&mut self) -> Option<u8> {
        assert!(!self.deferred_input_in_flight);
        if self.deferred_start >= self.deferred_len {
            return None;
        }
        let byte = self.deferred[usize::from(self.deferred_start)];
        self.deferred_start += 1;
        if self.deferred_start == self.deferred_len {
            self.deferred_start = 0;
            self.deferred_len = 0;
        }
        self.deferred_input_in_flight = true;
        Some(byte)
    }

    pub fn consume_deferred_input_dispatch(&mut self) -> bool {
        let in_flight = self.deferred_input_in_flight;
        self.deferred_input_in_flight = false;
        in_flight
    }

    fn feed_private(&mut self, byte: u8) -> FeedResult {
        self.feed_candidate(byte, Protocol::Private)
    }

    fn feed_ansi_tagged(&mut self, byte: u8) -> FeedResult {
        self.feed_candidate(byte, Protocol::AnsiTagged)
    }

    fn feed_candidate(&mut self, byte: u8, protocol: Protocol) -> FeedResult {
        if usize::from(self.candidate_len) == self.candidate.len() {
            return self.forward_overflow(byte);
        }
        self.candidate[usize::from(self.candidate_len)] = byte;
        self.candidate_len += 1;
        let bytes = &self.candidate[..usize::from(self.candidate_len)];
        let status = match protocol {
            Protocol::Private => classify_private_candidate(bytes),
            Protocol::AnsiTagged => classify_ansi_candidate(bytes),
        };
        match status {
            CandidateStatus::Prefix => FeedResult::Pending,
            CandidateStatus::Complete(position) => self.handle_position(position),
            CandidateStatus::Invalid => self.forward_invalid_candidate(protocol),
        }
    }

    fn handle_position(&mut self, position: Position) -> FeedResult {
        if let Some(first) = self.pending_position {
            if position.row == first.row && position.col == confirmation_tag_column {
                self.candidate_len = 0;
                return self.complete(first);
            }
            let mut forwarded = ForwardBytes::default();
            let pending_len = usize::from(self.pending_response_len);
            forwarded.storage[..pending_len].copy_from_slice(&self.pending_response[..pending_len]);
            forwarded.len = self.pending_response_len;
            self.pending_response_len = 0;
            self.pending_position = None;
            if position.col == first_tag_column {
                self.store_pending_position(position);
            } else {
                let candidate_len = usize::from(self.candidate_len);
                let start = usize::from(forwarded.len);
                forwarded.storage[start..start + candidate_len]
                    .copy_from_slice(&self.candidate[..candidate_len]);
                forwarded.len += self.candidate_len;
                self.candidate_len = 0;
            }
            return FeedResult::Forward(forwarded);
        }
        if position.col != first_tag_column {
            return self.forward_invalid_candidate(self.protocol);
        }
        self.store_pending_position(position);
        FeedResult::Pending
    }

    fn store_pending_position(&mut self, position: Position) {
        let len = usize::from(self.candidate_len);
        assert!(len <= self.pending_response.len());
        self.pending_response[..len].copy_from_slice(&self.candidate[..len]);
        self.pending_response_len = self.candidate_len;
        self.pending_position = Some(position);
        self.candidate_len = 0;
    }

    fn complete(&mut self, position: Position) -> FeedResult {
        let prior_mode = self.mode;
        self.candidate_len = 0;
        self.pending_response_len = 0;
        self.pending_position = None;
        self.mode = Mode::Idle;
        self.deadline_ms = 0;
        if matches!(prior_mode, Mode::Waiting | Mode::WaitingForPasteEnd) {
            FeedResult::Position(position)
        } else {
            FeedResult::LateResponse
        }
    }

    fn forward_invalid_candidate(&mut self, protocol: Protocol) -> FeedResult {
        let candidate_len = usize::from(self.candidate_len);
        let suffix_start = prefix_suffix_start(&self.candidate[..candidate_len], protocol);
        let forward_len = suffix_start.unwrap_or(candidate_len);
        let mut forwarded = ForwardBytes::default();
        let pending_len = usize::from(self.pending_response_len);
        if pending_len > 0 {
            forwarded.storage[..pending_len].copy_from_slice(&self.pending_response[..pending_len]);
            forwarded.len = self.pending_response_len;
            self.pending_response_len = 0;
            self.pending_position = None;
        }
        let start = usize::from(forwarded.len);
        forwarded.storage[start..start + forward_len]
            .copy_from_slice(&self.candidate[..forward_len]);
        forwarded.len += forward_len as u8;
        if let Some(start) = suffix_start {
            let retained_len = candidate_len - start;
            self.candidate.copy_within(start..candidate_len, 0);
            self.candidate_len = retained_len as u8;
        } else {
            self.candidate_len = 0;
        }
        FeedResult::Forward(forwarded)
    }

    fn forward_overflow(&mut self, byte: u8) -> FeedResult {
        let mut forwarded = ForwardBytes::default();
        let pending_len = usize::from(self.pending_response_len);
        if pending_len > 0 {
            forwarded.storage[..pending_len].copy_from_slice(&self.pending_response[..pending_len]);
            forwarded.len = self.pending_response_len;
            self.pending_response_len = 0;
            self.pending_position = None;
        }
        let candidate_len = usize::from(self.candidate_len);
        let start = usize::from(forwarded.len);
        forwarded.storage[start..start + candidate_len]
            .copy_from_slice(&self.candidate[..candidate_len]);
        forwarded.len += self.candidate_len;
        forwarded.storage[usize::from(forwarded.len)] = byte;
        forwarded.len += 1;
        self.candidate_len = 0;
        FeedResult::Forward(forwarded)
    }

    fn defer_probe_bytes(&mut self) {
        if self.pending_response_len == 0 && self.candidate_len == 0 {
            return;
        }
        assert_eq!(self.deferred_start, self.deferred_len);
        let pending_len = usize::from(self.pending_response_len);
        let candidate_len = usize::from(self.candidate_len);
        self.deferred[..pending_len].copy_from_slice(&self.pending_response[..pending_len]);
        self.deferred[pending_len..pending_len + candidate_len]
            .copy_from_slice(&self.candidate[..candidate_len]);
        self.deferred_start = 0;
        self.deferred_len = self.pending_response_len + self.candidate_len;
        self.pending_response_len = 0;
        self.pending_position = None;
        self.candidate_len = 0;
    }
}

pub fn query_bytes(protocol: Protocol) -> &'static [u8] {
    match protocol {
        Protocol::Private => b"\x1b[?2026h\x1b7\x1b[1G\x1b[?6n\x1b[2G\x1b[?6n\x1b8\x1b[?2026l",
        Protocol::AnsiTagged => b"\x1b[?2026h\x1b7\x1b[1G\x1b[6n\x1b[2G\x1b[6n\x1b8\x1b[?2026l",
    }
}

pub fn find_position_response(bytes: &[u8]) -> Option<Position> {
    let mut i = 0;
    while i + 3 <= bytes.len() {
        if bytes[i] == 0x1b && i + 2 < bytes.len() && bytes[i + 1] == b'[' {
            let mut row_end = i + 2;
            while row_end < bytes.len() && bytes[row_end].is_ascii_digit() {
                row_end += 1;
            }
            if row_end != i + 2 && row_end < bytes.len() && bytes[row_end] == b';' {
                let mut col_end = row_end + 1;
                while col_end < bytes.len() && bytes[col_end].is_ascii_digit() {
                    col_end += 1;
                }
                if col_end != row_end + 1 && col_end < bytes.len() && bytes[col_end] == b'R' {
                    if let (Some(row), Some(col)) = (
                        parse_u16(&bytes[i + 2..row_end]),
                        parse_u16(&bytes[row_end + 1..col_end]),
                    ) {
                        if row != 0 && col != 0 {
                            return Some(Position { row, col });
                        }
                    }
                }
            }
        }
        i += 1;
    }
    None
}

pub fn parse_position_response(bytes: &[u8]) -> Result<Position, ResponseParseError> {
    find_position_response(bytes).ok_or(ResponseParseError::CursorPositionUnavailable)
}

fn add_millis(now_ms: i64, duration_ms: i64) -> i64 {
    now_ms.saturating_add(duration_ms)
}

fn forward_byte(byte: u8) -> FeedResult {
    let mut forwarded = ForwardBytes::default();
    forwarded.storage[0] = byte;
    forwarded.len = 1;
    FeedResult::Forward(forwarded)
}

fn prefix_suffix_start(bytes: &[u8], protocol: Protocol) -> Option<usize> {
    (1..bytes.len()).find(|&start| {
        let status = match protocol {
            Protocol::Private => classify_private_candidate(&bytes[start..]),
            Protocol::AnsiTagged => classify_ansi_candidate(&bytes[start..]),
        };
        status == CandidateStatus::Prefix
    })
}

fn csi_parameter_start(bytes: &[u8]) -> Option<Result<usize, ()>> {
    match bytes.first() {
        None => Some(Ok(0)),
        Some(0x1b) => {
            if bytes.len() == 1 {
                None
            } else if bytes[1] != b'[' {
                Some(Err(()))
            } else {
                Some(Ok(2))
            }
        }
        Some(0x9b) => Some(Ok(1)),
        Some(_) => Some(Err(())),
    }
}

fn classify_private_candidate(bytes: &[u8]) -> CandidateStatus {
    classify_candidate(bytes, true)
}

fn classify_ansi_candidate(bytes: &[u8]) -> CandidateStatus {
    classify_candidate(bytes, false)
}

fn classify_candidate(bytes: &[u8], private: bool) -> CandidateStatus {
    if bytes.is_empty() {
        return CandidateStatus::Prefix;
    }
    let mut index = match csi_parameter_start(bytes) {
        None => return CandidateStatus::Prefix,
        Some(Err(())) => return CandidateStatus::Invalid,
        Some(Ok(index)) => index,
    };
    if index == bytes.len() {
        return CandidateStatus::Prefix;
    }
    if private {
        if bytes[index] != b'?' {
            return CandidateStatus::Invalid;
        }
        index += 1;
    }
    let row_start = index;
    while index < bytes.len() && bytes[index].is_ascii_digit() {
        index += 1;
    }
    let row_len = index - row_start;
    if row_len > 5 {
        return CandidateStatus::Invalid;
    }
    if index == bytes.len() {
        return CandidateStatus::Prefix;
    }
    if row_len == 0 || bytes[index] != b';' {
        return CandidateStatus::Invalid;
    }
    index += 1;
    let col_start = index;
    while index < bytes.len() && bytes[index].is_ascii_digit() {
        index += 1;
    }
    let col_len = index - col_start;
    if col_len > 5 {
        return CandidateStatus::Invalid;
    }
    if index == bytes.len() {
        return CandidateStatus::Prefix;
    }
    if col_len == 0 || bytes[index] != b'R' || index + 1 != bytes.len() {
        return CandidateStatus::Invalid;
    }
    let (Some(row), Some(col)) = (
        parse_u16(&bytes[row_start..row_start + row_len]),
        parse_u16(&bytes[col_start..col_start + col_len]),
    ) else {
        return CandidateStatus::Invalid;
    };
    if row == 0 || col == 0 {
        CandidateStatus::Invalid
    } else {
        CandidateStatus::Complete(Position { row, col })
    }
}

fn parse_u16(bytes: &[u8]) -> Option<u16> {
    let mut value = 0u16;
    for &byte in bytes {
        value = value
            .checked_mul(10)?
            .checked_add(u16::from(byte.checked_sub(b'0')?))?;
    }
    Some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn append_forwarded(result: FeedResult, forwarded: &mut Vec<u8>) -> Option<Position> {
        match result {
            FeedResult::Pending | FeedResult::LateResponse => None,
            FeedResult::Position(position) => Some(position),
            FeedResult::Forward(bytes) => {
                forwarded.extend_from_slice(bytes.slice());
                None
            }
        }
    }

    #[test]
    fn private_cursor_probe_preserves_arbitrary_backlog_before_the_response() {
        for backlog_size in [255, 256, 257, 5_000_000] {
            let mut probe = Parser::default();
            probe.begin(Protocol::Private, 100).unwrap();
            let mut forwarded = Vec::new();
            for _ in 0..backlog_size {
                assert_eq!(append_forwarded(probe.feed(b'x'), &mut forwarded), None);
            }
            for &byte in b"\x1b[?12;1R\x1b[?12;2R" {
                if let Some(position) = append_forwarded(probe.feed(byte), &mut forwarded) {
                    assert_eq!(position, Position { row: 12, col: 1 });
                }
            }
            assert_eq!(forwarded.len(), backlog_size);
            assert!(forwarded.iter().all(|&byte| byte == b'x'));
            assert!(probe.can_begin());
        }
    }

    #[test]
    fn private_cursor_probe_survives_a_response_split_across_reads() {
        let mut probe = Parser::default();
        probe.begin(Protocol::Private, 100).unwrap();
        let mut forwarded = Vec::new();
        for &byte in b"\x1b[?7;" {
            assert_eq!(append_forwarded(probe.feed(byte), &mut forwarded), None);
        }
        assert!(forwarded.is_empty());
        let mut position = None;
        for &byte in b"1R" {
            position = append_forwarded(probe.feed(byte), &mut forwarded).or(position);
        }
        assert!(probe.has_pending_input());
        for &byte in b"\x1b[?7;2R" {
            position = append_forwarded(probe.feed(byte), &mut forwarded).or(position);
        }
        assert_eq!(position, Some(Position { row: 7, col: 1 }));
        assert!(forwarded.is_empty());
        assert!(!probe.has_pending_input());
    }

    #[test]
    fn private_cursor_probe_forwards_modified_f3_and_consumes_only_the_private_report() {
        let mut probe = Parser::default();
        probe.begin(Protocol::Private, 100).unwrap();
        let mut forwarded = Vec::new();
        for &byte in b"\x1b[1;2R" {
            append_forwarded(probe.feed(byte), &mut forwarded);
        }
        assert_eq!(forwarded, b"\x1b[1;2R");
        let mut position = None;
        for &byte in b"\x1b[?5;1R\x1b[?5;2R" {
            position = append_forwarded(probe.feed(byte), &mut forwarded).or(position);
        }
        assert_eq!(position, Some(Position { row: 5, col: 1 }));
    }

    #[test]
    fn private_cursor_probe_timeout_replays_a_partial_candidate_and_discards_a_late_response() {
        let mut probe = Parser::default();
        probe.begin(Protocol::Private, 100).unwrap();
        for &byte in b"\x1b[?12" {
            assert_eq!(probe.feed(byte), FeedResult::Pending);
        }
        assert!(probe.has_pending_input());
        assert_eq!(probe.poll(100), PollResult::ProbeTimedOut);
        let mut replay = Vec::new();
        while let Some(byte) = probe.take_deferred_byte() {
            assert!(probe.has_pending_input());
            assert!(probe.consume_deferred_input_dispatch());
            assert!(!probe.consume_deferred_input_dispatch());
            replay.push(byte);
        }
        assert_eq!(replay, b"\x1b[?12");
        assert!(!probe.can_begin());
        assert!(!probe.has_pending_input());
        let mut saw_late_response = false;
        for &byte in b"\x1b[?8;1R\x1b[?8;2R" {
            match probe.feed(byte) {
                FeedResult::LateResponse => saw_late_response = true,
                FeedResult::Pending => {}
                result => panic!("unexpected probe result: {result:?}"),
            }
        }
        assert!(saw_late_response);
        assert!(probe.can_begin());
    }

    #[test]
    fn private_cursor_probe_supports_consecutive_measurements() {
        let mut probe = Parser::default();
        probe.begin(Protocol::Private, 100).unwrap();
        for &byte in b"\x1b[?2;1R\x1b[?2;2R" {
            let result = probe.feed(byte);
            if matches!(result, FeedResult::Position(_)) {
                assert_eq!(result, FeedResult::Position(Position { row: 2, col: 1 }));
            }
        }
        probe.begin(Protocol::Private, 200).unwrap();
        for &byte in b"\x9b?4;1R\x9b?4;2R" {
            let result = probe.feed(byte);
            if matches!(result, FeedResult::Position(_)) {
                assert_eq!(result, FeedResult::Position(Position { row: 4, col: 1 }));
            }
        }
    }

    #[test]
    fn tagged_ansi_cursor_probe_forwards_modified_f3_before_the_response() {
        let mut probe = Parser::default();
        probe.begin(Protocol::AnsiTagged, 100).unwrap();
        let mut forwarded = Vec::new();
        for &byte in b"\x1b[1;2R" {
            append_forwarded(probe.feed(byte), &mut forwarded);
        }
        assert_eq!(forwarded, b"\x1b[1;2R");
        let mut position = None;
        for &byte in b"\x1b[12;1R\x1b[12;2R" {
            position = append_forwarded(probe.feed(byte), &mut forwarded).or(position);
        }
        assert_eq!(position, Some(Position { row: 12, col: 1 }));
        assert_eq!(forwarded, b"\x1b[1;2R");
    }

    #[test]
    fn tagged_ansi_cursor_probe_forwards_every_legacy_f3_modifier_parameter() {
        for modifier in 2..=64 {
            let mut probe = Parser::default();
            probe.begin(Protocol::AnsiTagged, 100).unwrap();
            let sequence = format!("\x1b[1;{modifier}R");
            let mut forwarded = Vec::new();
            for byte in sequence.bytes() {
                append_forwarded(probe.feed(byte), &mut forwarded);
            }
            assert_eq!(forwarded, sequence.as_bytes());
        }
    }

    #[test]
    fn tagged_ansi_cursor_probe_forwards_the_modified_f3_three_and_four_pair() {
        let mut probe = Parser::default();
        probe.begin(Protocol::AnsiTagged, 100).unwrap();
        let mut forwarded = Vec::new();
        for &byte in b"\x1b[1;3R\x1b[1;4R" {
            append_forwarded(probe.feed(byte), &mut forwarded);
        }
        assert_eq!(forwarded, b"\x1b[1;3R\x1b[1;4R");
        let mut position = None;
        for &byte in b"\x1b[12;1R\x1b[12;2R" {
            position = append_forwarded(probe.feed(byte), &mut forwarded).or(position);
        }
        assert_eq!(position, Some(Position { row: 12, col: 1 }));
    }

    #[test]
    fn tagged_ansi_cursor_probe_preserves_cpr_shaped_input_after_the_terminal_response() {
        let mut probe = Parser::default();
        probe.begin(Protocol::AnsiTagged, 100).unwrap();
        let mut forwarded = Vec::new();
        let mut position = None;
        for &byte in b"\x1b[12;1R\x1b[12;2R\x1b[1;2R" {
            position = append_forwarded(probe.feed(byte), &mut forwarded).or(position);
        }
        assert_eq!(position, Some(Position { row: 12, col: 1 }));
        assert_eq!(forwarded, b"\x1b[1;2R");
    }

    #[test]
    fn tagged_ansi_cursor_probe_forwards_untagged_cursor_reports_in_order() {
        let mut probe = Parser::default();
        probe.begin(Protocol::AnsiTagged, 100).unwrap();
        let mut forwarded = Vec::new();
        for &byte in b"\x1b[1;2Rx" {
            append_forwarded(probe.feed(byte), &mut forwarded);
        }
        assert_eq!(forwarded, b"\x1b[1;2Rx");
        let mut position = None;
        for &byte in b"\x1b[7;1R\x1b[7;2R" {
            position = append_forwarded(probe.feed(byte), &mut forwarded).or(position);
        }
        assert_eq!(position, Some(Position { row: 7, col: 1 }));
        assert_eq!(forwarded, b"\x1b[1;2Rx");
    }

    #[test]
    fn tagged_ansi_cursor_probe_preserves_cpr_shaped_paste_before_the_terminal_response_pair() {
        let mut probe = Parser::default();
        probe.begin(Protocol::AnsiTagged, 100).unwrap();
        assert!(probe.suspend_for_paste());
        assert!(probe.intercepts_input());
        assert_eq!(probe.poll(1_000), PollResult::None);
        let mut forwarded = Vec::new();
        for &byte in b"\x1b[1;1R" {
            append_forwarded(probe.feed(byte), &mut forwarded);
        }
        assert!(forwarded.is_empty());
        let mut position = None;
        for &byte in b"x\x1b[12;1R\x1b[12;2R" {
            position = append_forwarded(probe.feed(byte), &mut forwarded).or(position);
        }
        assert_eq!(position, Some(Position { row: 12, col: 1 }));
        assert_eq!(forwarded, b"\x1b[1;1Rx");
        assert!(probe.can_begin());
    }

    #[test]
    fn tagged_ansi_late_response_window_suspends_for_paste_and_still_discards_the_reply() {
        let mut probe = Parser::default();
        probe.begin(Protocol::AnsiTagged, 100).unwrap();
        assert_eq!(probe.poll(100), PollResult::ProbeTimedOut);
        assert!(!probe.suspend_for_paste());
        assert!(probe.intercepts_input());
        assert_eq!(probe.poll(1_000), PollResult::None);
        let mut saw_late_response = false;
        for &byte in b"\x1b[12;1R\x1b[12;2R" {
            match probe.feed(byte) {
                FeedResult::LateResponse => saw_late_response = true,
                FeedResult::Pending => {}
                result => panic!("unexpected probe result: {result:?}"),
            }
        }
        assert!(saw_late_response);
        assert!(probe.can_begin());
    }

    #[test]
    fn tagged_ansi_query_synchronizes_transient_cursor_moves() {
        assert_eq!(
            query_bytes(Protocol::AnsiTagged),
            b"\x1b[?2026h\x1b7\x1b[1G\x1b[6n\x1b[2G\x1b[6n\x1b8\x1b[?2026l"
        );
    }

    #[test]
    fn private_query_synchronizes_transient_cursor_moves() {
        assert_eq!(
            query_bytes(Protocol::Private),
            b"\x1b[?2026h\x1b7\x1b[1G\x1b[?6n\x1b[2G\x1b[?6n\x1b8\x1b[?2026l"
        );
    }

    #[test]
    fn one_shot_cursor_response_parser_finds_a_response_with_leading_and_trailing_bytes() {
        assert_eq!(
            parse_position_response(b"noise\x1b[42;7Rtail").unwrap(),
            Position { row: 42, col: 7 }
        );
    }

    #[test]
    fn one_shot_cursor_response_parser_rejects_invalid_rows_and_columns() {
        let invalid: &[&[u8]] = &[
            b"",
            b"\x1b[0;1R",
            b"\x1b[1;0R",
            b"\x1b[;1R",
            b"\x1b[1;R",
            b"\x1b[1;1",
            b"\x1b[999999;1R",
            b"\x1b[1;999999R",
            b"\x9b1;1R",
        ];
        for &sample in invalid {
            assert_eq!(find_position_response(sample), None);
            assert_eq!(
                parse_position_response(sample),
                Err(ResponseParseError::CursorPositionUnavailable)
            );
        }
    }

    #[test]
    fn cursor_probe_timeout_measures_input_silence_rather_than_total_backlog_time() {
        let mut probe = Parser::default();
        probe.begin(Protocol::Private, 100).unwrap();
        probe.note_input_activity(90);
        assert_eq!(probe.poll(189), PollResult::None);
        assert_eq!(probe.poll(190), PollResult::ProbeTimedOut);
    }
}
