//! Model-facing tool result sanitization and size limits.
//!
//! Ported from `zig-reference/src/core/tooling/tool_result_limits.zig`.

use crate::core::shared::{text_utils, types::ToolResultMemory};

pub const DEFAULT_MAX_TOOL_RESULT_BYTES: usize = 64 * 1024;
pub const MIN_CONFIGURED_TOOL_RESULT_BYTES: usize = 1024;
#[allow(non_upper_case_globals)]
pub const default_max_tool_result_bytes: usize = DEFAULT_MAX_TOOL_RESULT_BYTES;
#[allow(non_upper_case_globals)]
pub const min_configured_tool_result_bytes: usize = MIN_CONFIGURED_TOOL_RESULT_BYTES;

pub fn resolve_max_tool_result_bytes(setting: Option<usize>, default_value: usize) -> usize {
    setting.unwrap_or(default_value)
}

pub fn prepare_model_output(tool_name: &[u8], raw: &[u8], max_bytes: usize) -> Vec<u8> {
    let sanitized = text_utils::sanitize_model_text(raw);
    let masked = text_utils::mask_secrets(&sanitized);
    let marker = format!(
        "\n... [tool result truncated for {}: original {} bytes; cap is {} bytes]\n",
        String::from_utf8_lossy(tool_name),
        masked.len(),
        max_bytes,
    );
    truncate_text(TruncateOptions {
        text: &masked,
        max_bytes,
        marker: marker.as_bytes(),
        trace_scope: b"tool",
        trace_label: tool_name,
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedInlineResult {
    pub model_output: Vec<u8>,
    pub memory: ToolResultMemory,
}

pub fn prepare_inline_result(
    tool_name: &[u8],
    raw_output: &[u8],
    max_bytes: usize,
) -> PreparedInlineResult {
    let model_output = prepare_model_output(tool_name, raw_output, max_bytes);
    let stored_output_bytes = model_output.len();
    PreparedInlineResult {
        model_output,
        memory: ToolResultMemory {
            output_handle: None,
            preview: None,
            output_bytes: raw_output.len(),
            stored_output_bytes,
            truncated: stored_output_bytes < raw_output.len(),
            ..ToolResultMemory::default()
        },
    }
}

#[derive(Clone, Copy, Debug)]
pub struct TruncateOptions<'a> {
    pub text: &'a [u8],
    pub max_bytes: usize,
    pub marker: &'a [u8],
    pub trace_scope: &'a [u8],
    pub trace_label: &'a [u8],
}

pub fn truncate_text(opts: TruncateOptions<'_>) -> Vec<u8> {
    if opts.text.len() <= opts.max_bytes {
        return opts.text.to_vec();
    }
    let prefix_cap = opts.max_bytes.saturating_sub(opts.marker.len());
    let prefix_len = text_utils::utf8_backward_boundary(opts.text, prefix_cap);
    let mut output = Vec::with_capacity(prefix_len + opts.marker.len());
    output.extend_from_slice(&opts.text[..prefix_len]);
    output.extend_from_slice(opts.marker);
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn masks_secrets_before_applying_cap() {
        assert_eq!(
            prepare_model_output(
                b"mcp__server__tool",
                b"token=secret-value",
                default_max_tool_result_bytes
            ),
            b"token=[redacted]"
        );
        assert_eq!(
            prepare_model_output(
                b"run_command",
                b"API_KEY=\"secret-value\"",
                default_max_tool_result_bytes
            ),
            b"API_KEY=\"[redacted]\""
        );
    }
    #[test]
    fn caps_chatty_output_with_explicit_marker() {
        let output = prepare_model_output(b"grep_files", &[b'x'; 256], 128);
        assert!(output.len() <= 128);
        assert!(output.windows(b"... [tool result truncated for grep_files: original 256 bytes; cap is 128 bytes]".len()).any(|v| v == b"... [tool result truncated for grep_files: original 256 bytes; cap is 128 bytes]"));
    }
    #[test]
    fn keeps_complete_codepoints_at_the_cap() {
        let mut text = b"x".to_vec();
        for _ in 0..300 {
            text.extend_from_slice("é".as_bytes());
        }
        for cap in [128, 129] {
            let output = prepare_model_output(b"grep_files", &text, cap);
            assert!(output.len() <= cap);
            let marker = output
                .windows(b"\n... [tool result truncated".len())
                .position(|v| v == b"\n... [tool result truncated")
                .unwrap();
            assert!(std::str::from_utf8(&output[..marker]).is_ok());
            assert!(output[..marker].ends_with("é".as_bytes()));
        }
    }
    #[test]
    fn inline_result_tracks_sizes() {
        let result = prepare_inline_result(b"read_file", b"hello", 64);
        assert_eq!(result.memory.output_bytes, 5);
        assert_eq!(result.memory.stored_output_bytes, 5);
        assert!(!result.memory.truncated);
    }
}
