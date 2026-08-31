//! Local diagnostic collection used by `/trace`.

use crate::core::workspace::network_metrics;
use crate::core::workspace::tool_call_metrics;

pub use network_metrics::{NetworkCall, NetworkCallKind};
pub use tool_call_metrics::{ToolCallMetric, ToolCallRecord};
pub const network_ring_capacity: usize = network_metrics::ring_capacity;
pub const tool_call_ring_capacity: usize = tool_call_metrics::ring_capacity;

pub fn record_network_call(call: NetworkCall) {
    network_metrics::record(call);
}

pub fn snapshot_network_calls(out: &mut [NetworkCall]) -> usize {
    network_metrics::snapshot(out)
}

pub fn record_tool_call(call: ToolCallMetric) {
    tool_call_metrics::record(call);
}

pub fn record_tool_call_result(input: ToolCallRecord<'_>) {
    tool_call_metrics::record_result(input);
}

pub fn snapshot_tool_calls(out: &mut [ToolCallMetric]) -> usize {
    tool_call_metrics::snapshot(out)
}

pub fn reset_session() {
    network_metrics::reset();
    tool_call_metrics::reset();
}

pub fn reset_for_test() {
    reset_session();
}
