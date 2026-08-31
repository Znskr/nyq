//! In-memory ring buffer of recent tool invocations used by `/trace`.

use std::sync::{Mutex, OnceLock};

use crate::core::shared::io;

pub const max_name_len: usize = 64;
pub const max_args_len: usize = 1200;
pub const max_result_len: usize = 2000;
pub const ring_capacity: usize = 64;

#[derive(Clone, Copy)]
pub struct ToolCallMetric {
    pub started_at_ms: i64,
    pub duration_ms: u32,
    pub ok: bool,
    pub subagent_id: u64,
    pub name_buf: [u8; max_name_len],
    pub name_len: u8,
    pub args_buf: [u8; max_args_len],
    pub args_len: u16,
    pub args_total_bytes: u32,
    pub result_buf: [u8; max_result_len],
    pub result_len: u16,
    pub result_total_bytes: u32,
}

impl Default for ToolCallMetric {
    fn default() -> Self {
        ToolCallMetric {
            started_at_ms: 0,
            duration_ms: 0,
            ok: true,
            subagent_id: 0,
            name_buf: [0; max_name_len],
            name_len: 0,
            args_buf: [0; max_args_len],
            args_len: 0,
            args_total_bytes: 0,
            result_buf: [0; max_result_len],
            result_len: 0,
            result_total_bytes: 0,
        }
    }
}

impl ToolCallMetric {
    pub fn name(&self) -> &[u8] {
        &self.name_buf[..self.name_len as usize]
    }

    pub fn args(&self) -> &[u8] {
        &self.args_buf[..self.args_len as usize]
    }

    pub fn result(&self) -> &[u8] {
        &self.result_buf[..self.result_len as usize]
    }

    pub fn set_name(&mut self, value: &[u8]) {
        let n = value.len().min(max_name_len);
        self.name_buf[..n].copy_from_slice(&value[..n]);
        self.name_len = n as u8;
    }

    pub fn set_args(&mut self, value: &[u8]) {
        self.args_total_bytes = value.len().min(u32::MAX as usize) as u32;
        let n = value.len().min(max_args_len);
        self.args_buf[..n].copy_from_slice(&value[..n]);
        self.args_len = n as u16;
    }

    pub fn set_result(&mut self, value: &[u8]) {
        let normalized = content_for_display(value);
        self.result_total_bytes = normalized.len().min(u32::MAX as usize) as u32;
        let n = normalized.len().min(max_result_len);
        self.result_buf[..n].copy_from_slice(&normalized[..n]);
        self.result_len = n as u16;
    }

    pub fn set_payloads_for_name(
        &mut self,
        tool_name: &[u8],
        arguments_json: &[u8],
        model_output: &[u8],
    ) {
        if omits_payloads_for_name(tool_name) {
            self.clear_payloads();
            return;
        }
        self.set_args(arguments_json);
        self.set_result(model_output);
    }

    fn clear_payloads(&mut self) {
        self.args_buf = [0; max_args_len];
        self.args_len = 0;
        self.args_total_bytes = 0;
        self.result_buf = [0; max_result_len];
        self.result_len = 0;
        self.result_total_bytes = 0;
    }
}

pub struct ToolCallRecord<'a> {
    pub name: &'a [u8],
    pub arguments_json: &'a [u8],
    pub model_output: &'a [u8],
    pub ok: bool,
    pub started_at_ms: i64,
    pub subagent_id: u64,
}

pub fn omits_payloads_for_name(tool_name: &[u8]) -> bool {
    tool_name == b"web_fetch"
}

#[derive(Default)]
struct Ring {
    ring: Vec<ToolCallMetric>,
    head: usize,
    stored: usize,
}

impl Ring {
    fn new() -> Self {
        Ring {
            ring: vec![ToolCallMetric::default(); ring_capacity],
            head: 0,
            stored: 0,
        }
    }
}

static RING: OnceLock<Mutex<Ring>> = OnceLock::new();

fn ring() -> &'static Mutex<Ring> {
    RING.get_or_init(|| Mutex::new(Ring::new()))
}

pub fn record(call: ToolCallMetric) {
    let mut stored_call = call;
    if omits_payloads_for_name(stored_call.name()) {
        stored_call.clear_payloads();
    }

    let mut ring = ring().lock().expect("tool call metric ring lock poisoned");
    let head = ring.head;
    ring.ring[head] = stored_call;
    ring.head = (head + 1) % ring_capacity;
    if ring.stored < ring_capacity {
        ring.stored += 1;
    }
}

pub fn record_result(input: ToolCallRecord<'_>) {
    let now_ms = io::milli_timestamp();
    let elapsed = if now_ms > input.started_at_ms {
        now_ms - input.started_at_ms
    } else {
        0
    };
    let mut metric = ToolCallMetric {
        started_at_ms: input.started_at_ms,
        duration_ms: elapsed.min(u32::MAX as i64) as u32,
        ok: input.ok,
        subagent_id: input.subagent_id,
        ..ToolCallMetric::default()
    };
    metric.set_name(input.name);
    metric.set_payloads_for_name(input.name, input.arguments_json, input.model_output);
    record(metric);
}

pub fn snapshot(out: &mut [ToolCallMetric]) -> usize {
    let ring = ring().lock().expect("tool call metric ring lock poisoned");
    let n = ring.stored.min(out.len());
    for i in 0..n {
        let idx = (ring.head + ring_capacity - n + i) % ring_capacity;
        out[i] = ring.ring[idx];
    }
    n
}

pub fn reset() {
    let mut ring = ring().lock().expect("tool call metric ring lock poisoned");
    ring.head = 0;
    ring.stored = 0;
}

pub fn reset_for_test() {
    reset();
}

/// Faithful mirror of `tool_result_display.contentForDisplay` (which is not
/// yet ported). Malformed or unfamiliar output stays literal.
fn content_for_display(body: &[u8]) -> Vec<u8> {
    let mut rest = body;
    if rest.starts_with(b"<path>") {
        let Some(path_close) = find(rest, b"</path>") else {
            return body.to_vec();
        };
        rest = trim_start(&rest[path_close + b"</path>".len()..], b"\r\n");
    }
    if !rest.starts_with(b"<content>") {
        return body.to_vec();
    }
    rest = trim_start(&rest[b"<content>".len()..], b"\r\n");
    if !rest.ends_with(b"</content>") {
        return body.to_vec();
    }
    trim_end(&rest[..rest.len() - b"</content>".len()], b"\r\n").to_vec()
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn trim_start<'a>(mut bytes: &'a [u8], whitespace: &[u8]) -> &'a [u8] {
    while bytes.first().is_some_and(|b| whitespace.contains(b)) {
        bytes = &bytes[1..];
    }
    bytes
}

fn trim_end<'a>(mut bytes: &'a [u8], whitespace: &[u8]) -> &'a [u8] {
    while bytes.last().is_some_and(|b| whitespace.contains(b)) {
        bytes = &bytes[..bytes.len() - 1];
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    static TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn tool_call_ring_keeps_last_n_records_in_chronological_order() {
        let _lock = TEST_LOCK.lock().unwrap();
        reset_for_test();

        for i in 0..(ring_capacity as u32 + 3) {
            let mut call = ToolCallMetric {
                duration_ms: i,
                ..ToolCallMetric::default()
            };
            call.set_name(b"read_file");
            call.set_args(b"{}");
            call.set_result(b"ok");
            record(call);
        }

        let mut buf = vec![ToolCallMetric::default(); ring_capacity];
        let n = snapshot(&mut buf);
        assert_eq!(ring_capacity, n);
        assert_eq!(3, buf[0].duration_ms);
        assert_eq!(ring_capacity as u32 + 3 - 1, buf[ring_capacity - 1].duration_ms);

        reset_for_test();
    }

    #[test]
    fn args_and_result_are_truncated_and_total_length_tracked() {
        let _lock = TEST_LOCK.lock().unwrap();
        reset_for_test();

        let huge_len = max_args_len.max(max_result_len) + 200;
        let huge = vec![b'a'; huge_len];
        let mut call = ToolCallMetric::default();
        call.set_name(b"read_file");
        call.set_args(&huge);
        call.set_result(&huge);
        record(call);

        let mut buf = [ToolCallMetric::default(); 1];
        let n = snapshot(&mut buf);
        assert_eq!(1, n);
        assert_eq!(max_args_len as u16, buf[0].args_len);
        assert_eq!(huge_len as u32, buf[0].args_total_bytes);
        assert_eq!(max_result_len as u16, buf[0].result_len);
        assert_eq!(huge_len as u32, buf[0].result_total_bytes);

        reset_for_test();
    }

    #[test]
    fn content_for_display_unwraps_and_preserves_malformed_output() {
        assert_eq!(
            b"1\talpha\n2\tbeta".as_slice(),
            content_for_display(b"<path>README.md</path>\n<content>\n1\talpha\n2\tbeta\n</content>")
        );
        let raw = b"<path>README.md</path>\n<content>missing close";
        assert_eq!(raw.as_slice(), content_for_display(raw));
        assert_eq!(b"plain output".as_slice(), content_for_display(b"plain output"));
    }
}
