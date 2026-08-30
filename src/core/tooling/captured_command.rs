//! Classifies command-shaped tool calls that use captured execution rather
//! than a durable terminal session. Historical `run_command` records remain
//! presentation-compatible even though that tool is no longer executable.
//!
//! Ported 1:1 from `zig-reference/src/core/tooling/captured_command.zig`.

use crate::core::mcp::json_value;

/// Zig's `isToolCall` takes an allocator for JSON parsing; the Rust port
/// parses with the shared zero-dependency parser instead.
pub fn is_tool_call(tool_name: &[u8], arguments_json: &[u8]) -> bool {
    if tool_name == b"run_command" {
        return true;
    }
    if tool_name != b"terminal" {
        return false;
    }
    let Ok(text) = std::str::from_utf8(arguments_json) else {
        return false;
    };
    let Ok(value) = json_value::parse_json(text) else {
        return false;
    };
    let Some(object) = value.as_object() else {
        return false;
    };
    let Some(action) = object.get("action") else {
        return false;
    };
    action.as_str() == Some("exec")
}

#[cfg(test)]
mod tests {
    use super::is_tool_call;

    #[test]
    fn captured_command_classification_recognizes_terminal_exec_and_historical_records() {
        assert!(is_tool_call(
            b"terminal",
            b"{\"action\":\"exec\",\"command\":\"printf ok\"}"
        ));
        assert!(!is_tool_call(
            b"terminal",
            b"{\"action\":\"start\",\"command\":\"printf ok\"}"
        ));
        assert!(is_tool_call(b"run_command", b"{}"));
        assert!(!is_tool_call(b"read_file", b"{}"));
        assert!(!is_tool_call(b"terminal", b"not-json"));
    }
}
