//! In-memory ring buffer of recent gateway HTTP calls. Used by /trace
//! to surface latency / error patterns without forcing a persistent trace
//! log. Process-wide and lock-protected: callers do not need to plumb the
//! buffer through their context.

use std::sync::{Mutex, OnceLock};

pub const max_model_len: usize = 64;
pub const max_error_len: usize = 48;
pub const max_stop_reason_len: usize = 48;
pub const max_gateway_schema_diagnostic_len: usize = 160;
pub const max_gateway_request_shape_len: usize = 512;
pub const ring_capacity: usize = 32;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NetworkCallKind {
    Gateway,
    WebSearch,
    WebFetchTarget,
}

impl Default for NetworkCallKind {
    fn default() -> Self {
        NetworkCallKind::Gateway
    }
}

#[derive(Clone, Copy)]
pub struct NetworkCall {
    pub kind: NetworkCallKind,
    pub started_at_ms: i64,
    pub duration_ms: u32,
    pub status: u16,
    pub response_bytes: u32,
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub is_web_search: bool,
    pub web_search_requests: u32,
    pub turn_id: u64,
    pub step_id: u64,
    pub subagent_id: u64,
    pub model_buf: [u8; max_model_len],
    pub model_len: u8,
    pub error_buf: [u8; max_error_len],
    pub error_len: u8,
    pub stop_reason_buf: [u8; max_stop_reason_len],
    pub stop_reason_len: u8,
    pub gateway_schema_diagnostic_buf: [u8; max_gateway_schema_diagnostic_len],
    pub gateway_schema_diagnostic_len: u16,
    pub gateway_request_shape_buf: [u8; max_gateway_request_shape_len],
    pub gateway_request_shape_len: u16,
}

impl Default for NetworkCall {
    fn default() -> Self {
        NetworkCall {
            kind: NetworkCallKind::Gateway,
            started_at_ms: 0,
            duration_ms: 0,
            status: 0,
            response_bytes: 0,
            input_tokens: 0,
            output_tokens: 0,
            is_web_search: false,
            web_search_requests: 0,
            turn_id: 0,
            step_id: 0,
            subagent_id: 0,
            model_buf: [0; max_model_len],
            model_len: 0,
            error_buf: [0; max_error_len],
            error_len: 0,
            stop_reason_buf: [0; max_stop_reason_len],
            stop_reason_len: 0,
            gateway_schema_diagnostic_buf: [0; max_gateway_schema_diagnostic_len],
            gateway_schema_diagnostic_len: 0,
            gateway_request_shape_buf: [0; max_gateway_request_shape_len],
            gateway_request_shape_len: 0,
        }
    }
}

impl NetworkCall {
    pub fn model(&self) -> &[u8] {
        &self.model_buf[..self.model_len as usize]
    }

    pub fn error_name(&self) -> &[u8] {
        &self.error_buf[..self.error_len as usize]
    }

    pub fn terminal_stop_reason(&self) -> &[u8] {
        &self.stop_reason_buf[..self.stop_reason_len as usize]
    }

    pub fn gateway_schema_diagnostic(&self) -> &[u8] {
        &self.gateway_schema_diagnostic_buf[..self.gateway_schema_diagnostic_len as usize]
    }

    pub fn gateway_request_shape(&self) -> &[u8] {
        &self.gateway_request_shape_buf[..self.gateway_request_shape_len as usize]
    }

    pub fn set_model(&mut self, name: &[u8]) {
        let n = name.len().min(max_model_len);
        self.model_buf[..n].copy_from_slice(&name[..n]);
        self.model_len = n as u8;
    }

    pub fn set_error(&mut self, name: &[u8]) {
        let n = name.len().min(max_error_len);
        self.error_buf[..n].copy_from_slice(&name[..n]);
        self.error_len = n as u8;
    }

    pub fn set_terminal_stop_reason(&mut self, reason: &[u8]) {
        let n = reason.len().min(max_stop_reason_len);
        self.stop_reason_buf[..n].copy_from_slice(&reason[..n]);
        self.stop_reason_len = n as u8;
    }

    pub fn set_gateway_schema_diagnostic(&mut self, diagnostic: &[u8]) {
        let n = diagnostic.len().min(max_gateway_schema_diagnostic_len);
        self.gateway_schema_diagnostic_buf[..n].copy_from_slice(&diagnostic[..n]);
        self.gateway_schema_diagnostic_len = n as u16;
    }

    pub fn set_gateway_request_shape(&mut self, shape: &[u8]) {
        let n = shape.len().min(max_gateway_request_shape_len);
        self.gateway_request_shape_buf[..n].copy_from_slice(&shape[..n]);
        self.gateway_request_shape_len = n as u16;
    }
}

#[derive(Default)]
struct Ring {
    ring: Vec<NetworkCall>,
    head: usize,
    stored: usize,
}

impl Ring {
    fn new() -> Self {
        Ring {
            ring: vec![NetworkCall::default(); ring_capacity],
            head: 0,
            stored: 0,
        }
    }
}

static RING: OnceLock<Mutex<Ring>> = OnceLock::new();

fn ring() -> &'static Mutex<Ring> {
    RING.get_or_init(|| Mutex::new(Ring::new()))
}

pub fn record(call: NetworkCall) {
    let mut ring = ring().lock().expect("network metric ring lock poisoned");
    let head = ring.head;
    ring.ring[head] = call;
    ring.head = (head + 1) % ring_capacity;
    if ring.stored < ring_capacity {
        ring.stored += 1;
    }
}

pub fn snapshot(out: &mut [NetworkCall]) -> usize {
    let ring = ring().lock().expect("network metric ring lock poisoned");
    let n = ring.stored.min(out.len());
    for i in 0..n {
        let idx = (ring.head + ring_capacity - n + i) % ring_capacity;
        out[i] = ring.ring[idx];
    }
    n
}

pub fn reset() {
    let mut ring = ring().lock().expect("network metric ring lock poisoned");
    ring.head = 0;
    ring.stored = 0;
}

pub fn reset_for_test() {
    reset();
}

#[cfg(test)]
mod tests {
    use super::*;

    // The ring buffer is process-global; Rust runs unit tests on multiple
    // threads by default, so serialize every test that touches it.
    static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn ring_keeps_last_n_records_in_chronological_order() {
        let _lock = TEST_LOCK.lock().unwrap();
        reset_for_test();

        for i in 0..(ring_capacity as u32 + 5) {
            let mut call = NetworkCall {
                duration_ms: i,
                ..NetworkCall::default()
            };
            call.set_model(b"anthropic/claude-opus-4.6");
            record(call);
        }

        let mut buf = [NetworkCall::default(); ring_capacity];
        let n = snapshot(&mut buf);
        assert_eq!(ring_capacity, n);
        assert_eq!(5, buf[0].duration_ms);
        assert_eq!(ring_capacity as u32 + 5 - 1, buf[ring_capacity - 1].duration_ms);

        reset_for_test();
    }

    #[test]
    fn snapshot_truncates_to_caller_buffer() {
        let _lock = TEST_LOCK.lock().unwrap();
        reset_for_test();

        for i in 0..10u32 {
            record(NetworkCall {
                duration_ms: i,
                ..NetworkCall::default()
            });
        }

        let mut small = [NetworkCall::default(); 4];
        let n = snapshot(&mut small);
        assert_eq!(4, n);
        assert_eq!(6, small[0].duration_ms);
        assert_eq!(9, small[3].duration_ms);

        reset_for_test();
    }

    #[test]
    fn gateway_diagnostics_default_empty_truncate_and_survive_snapshot() {
        let _lock = TEST_LOCK.lock().unwrap();
        reset_for_test();

        let empty = NetworkCall::default();
        assert_eq!(b"", empty.gateway_schema_diagnostic());
        assert_eq!(b"", empty.gateway_request_shape());

        let long_schema = [b's'; max_gateway_schema_diagnostic_len + 8];
        let long_shape = [b'r'; max_gateway_request_shape_len + 8];

        let mut call = NetworkCall {
            status: 400,
            ..NetworkCall::default()
        };
        call.set_gateway_schema_diagnostic(&long_schema);
        call.set_gateway_request_shape(&long_shape);
        record(call);

        let mut buf = [NetworkCall::default(); 1];
        let n = snapshot(&mut buf);
        assert_eq!(1, n);
        assert_eq!(
            max_gateway_schema_diagnostic_len,
            buf[0].gateway_schema_diagnostic().len()
        );
        assert_eq!(
            max_gateway_request_shape_len,
            buf[0].gateway_request_shape().len()
        );
        assert_eq!(b's', buf[0].gateway_schema_diagnostic()[0]);
        assert_eq!(b'r', buf[0].gateway_request_shape()[0]);

        reset_for_test();
    }
}
