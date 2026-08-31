//! Stable structured error payloads returned by tools.
//!
//! Ported from `zig-reference/src/core/tooling/tool_result_errors.zig`.

use crate::core::mcp::json_value::{self, Value};
use crate::core::shared::{text_utils, types::ToolPermissionDenialReason};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DetailValue<'a> {
    String(&'a [u8]),
    Integer(i64),
    Unsigned(u64),
    Boolean(bool),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Detail<'a> {
    pub name: &'a [u8],
    pub value: DetailValue<'a>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutionFailure<'a> {
    pub tool_name: &'a [u8],
    pub message: &'a [u8],
    pub details: &'a [Detail<'a>],
    pub suggestion: Option<&'a [u8]>,
}

pub type TerminalActionFieldConflict<'a> = [&'a [u8]; 2];

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TerminalActionFieldCorrection<'a> {
    pub action: &'a [u8],
    pub invalid_fields: &'a [&'a [u8]],
    pub missing_fields: &'a [&'a [u8]],
    pub allowed_fields: &'a [&'a [u8]],
    pub conflicts: &'a [TerminalActionFieldConflict<'a>],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TerminalActionFieldCorrectionView {
    pub invalid_field_count: usize,
}

const TERMINAL_ACTION_FIELD_ERROR_CODE: &[u8] = b"invalid_action_fields";

fn push_json_string(out: &mut Vec<u8>, bytes: &[u8]) {
    out.push(b'"');
    for &byte in bytes {
        match byte {
            b'"' => out.extend_from_slice(b"\\\""),
            b'\\' => out.extend_from_slice(b"\\\\"),
            b'\x08' => out.extend_from_slice(b"\\b"),
            b'\x0c' => out.extend_from_slice(b"\\f"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            b'\t' => out.extend_from_slice(b"\\t"),
            0..=0x1f => out.extend_from_slice(format!("\\u{byte:04x}").as_bytes()),
            _ => out.push(byte),
        }
    }
    out.push(b'"');
}

fn push_masked_json_string(out: &mut Vec<u8>, bytes: &[u8]) {
    push_json_string(out, &text_utils::mask_secrets(bytes));
}

fn push_string_array(out: &mut Vec<u8>, values: &[&[u8]]) {
    out.push(b'[');
    for (index, value) in values.iter().enumerate() {
        if index != 0 {
            out.push(b',');
        }
        push_json_string(out, value);
    }
    out.push(b']');
}

pub fn terminal_action_field_correction_json(
    correction: &TerminalActionFieldCorrection<'_>,
) -> Vec<u8> {
    let mut out = b"{\"error\":{\"code\":\"invalid_action_fields\",\"action\":".to_vec();
    push_json_string(&mut out, correction.action);
    out.extend_from_slice(b",\"invalid_fields\":");
    push_string_array(&mut out, correction.invalid_fields);
    out.extend_from_slice(b",\"missing_fields\":");
    push_string_array(&mut out, correction.missing_fields);
    out.extend_from_slice(b",\"allowed_fields\":");
    push_string_array(&mut out, correction.allowed_fields);
    out.extend_from_slice(b",\"conflicts\":[");
    for (index, pair) in correction.conflicts.iter().enumerate() {
        if index != 0 {
            out.push(b',');
        }
        push_string_array(&mut out, pair);
    }
    out.extend_from_slice(b"]}}");
    out
}

pub fn inspect_terminal_action_field_correction(
    output: &[u8],
) -> Option<TerminalActionFieldCorrectionView> {
    let text = std::str::from_utf8(output).ok()?;
    let root = json_value::parse_json(text).ok()?.as_object()?.clone();
    let error = root.get("error")?.as_object()?;
    if error.get("code")?.as_str()?.as_bytes() != TERMINAL_ACTION_FIELD_ERROR_CODE {
        return None;
    }
    error.get("action")?.as_str()?;
    let invalid_fields = error.get("invalid_fields")?.as_array()?;
    Some(TerminalActionFieldCorrectionView {
        invalid_field_count: invalid_fields.len(),
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolOutputClassification {
    StructuredToolExecutionFailed,
    ActiveLegacyToolFailure,
    AdapterSemanticFailure,
    NonFailure,
}

const ADAPTER_SEMANTIC_FAILURE_PREFIXES: [&[u8]; 5] = [
    b"Unsupported tool:",
    b"read_file failed:",
    b"edit_file failed:",
    b"open_url not supported",
    b"failed to open ",
];

fn reason_name(reason: ToolPermissionDenialReason) -> &'static [u8] {
    match reason {
        ToolPermissionDenialReason::UserDenied => b"user_denied",
        ToolPermissionDenialReason::AutoDenied => b"auto_denied",
        ToolPermissionDenialReason::ReviewCaution => b"review_caution",
        ToolPermissionDenialReason::ReviewUnavailable => b"review_unavailable",
        ToolPermissionDenialReason::PolicyDenied => b"policy_denied",
        ToolPermissionDenialReason::PermissionRequired => b"permission_required",
    }
}

fn parse_reason(raw: &str) -> Option<ToolPermissionDenialReason> {
    Some(match raw {
        "user_denied" => ToolPermissionDenialReason::UserDenied,
        "auto_denied" => ToolPermissionDenialReason::AutoDenied,
        "review_caution" => ToolPermissionDenialReason::ReviewCaution,
        "review_unavailable" => ToolPermissionDenialReason::ReviewUnavailable,
        "policy_denied" => ToolPermissionDenialReason::PolicyDenied,
        "permission_required" => ToolPermissionDenialReason::PermissionRequired,
        _ => return None,
    })
}

fn is_network_tool(tool_name: &[u8]) -> bool {
    tool_name == b"web_search"
}

fn permission_denied_message(
    tool_name: &[u8],
    reason: ToolPermissionDenialReason,
) -> &'static [u8] {
    match reason {
        ToolPermissionDenialReason::UserDenied => b"Permission denied by user",
        ToolPermissionDenialReason::AutoDenied => b"Blocked by automatic safety policy",
        ToolPermissionDenialReason::ReviewCaution => b"Action held after safety review",
        ToolPermissionDenialReason::ReviewUnavailable => {
            b"Safety reviewer unavailable; action held"
        }
        ToolPermissionDenialReason::PolicyDenied if is_network_tool(tool_name) => {
            b"Network or browser access was denied by configured policy"
        }
        ToolPermissionDenialReason::PolicyDenied => b"Tool access was denied by configured policy",
        ToolPermissionDenialReason::PermissionRequired
            if matches!(tool_name, b"run_command" | b"terminal") =>
        {
            b"Shell command approval is required before this tool can run"
        }
        ToolPermissionDenialReason::PermissionRequired if is_network_tool(tool_name) => {
            b"Network or browser approval is required before this tool can run"
        }
        ToolPermissionDenialReason::PermissionRequired => {
            b"Tool approval is required before this tool can run"
        }
    }
}

fn permission_denied_suggestion(reason: ToolPermissionDenialReason) -> &'static [u8] {
    match reason {
        ToolPermissionDenialReason::UserDenied => b"The tool did not run. Do not retry unchanged; explain the denial or use a safer allowed alternative.",
        ToolPermissionDenialReason::AutoDenied => b"The tool did not run. This is a legacy automatic denial; choose a materially different safe action or explain the blocker.",
        ToolPermissionDenialReason::ReviewCaution => b"The action did not run. Use the review advice to choose a materially different safe action, or explain why no safe path remains.",
        ToolPermissionDenialReason::ReviewUnavailable => b"The action did not run because safety review was unavailable. Continue with a different safe action or retry later.",
        ToolPermissionDenialReason::PolicyDenied => b"The tool did not run. Do not retry unchanged; explain the configured policy blocker or use an allowed alternative.",
        ToolPermissionDenialReason::PermissionRequired => b"The tool did not run. Noninteractive mode cannot show an approval prompt. Rerun interactively to approve, or configure a narrow permission rule before retrying.",
    }
}

pub fn tool_permission_denied_json(
    tool_name: &[u8],
    reason: ToolPermissionDenialReason,
) -> Vec<u8> {
    assert!(!matches!(
        reason,
        ToolPermissionDenialReason::ReviewCaution | ToolPermissionDenialReason::ReviewUnavailable
    ));
    let mut out = b"{\"error\":{\"type\":\"tool_permission_denied\",\"tool_name\":".to_vec();
    push_json_string(&mut out, tool_name);
    out.extend_from_slice(b",\"message\":");
    push_masked_json_string(&mut out, permission_denied_message(tool_name, reason));
    out.extend_from_slice(b",\"reason\":");
    push_json_string(&mut out, reason_name(reason));
    out.extend_from_slice(b",\"denied\":true,\"suggestion\":");
    push_masked_json_string(&mut out, permission_denied_suggestion(reason));
    out.extend_from_slice(b"}}");
    out
}

pub fn tool_review_held_json(
    tool_name: &[u8],
    reason: ToolPermissionDenialReason,
    advice: Option<&[u8]>,
) -> Vec<u8> {
    assert!(matches!(
        reason,
        ToolPermissionDenialReason::ReviewCaution | ToolPermissionDenialReason::ReviewUnavailable
    ));
    let mut out = b"{\"error\":{\"type\":\"tool_review_held\",\"tool_name\":".to_vec();
    push_json_string(&mut out, tool_name);
    out.extend_from_slice(b",\"message\":");
    push_masked_json_string(&mut out, permission_denied_message(tool_name, reason));
    out.extend_from_slice(b",\"reason\":");
    push_json_string(&mut out, reason_name(reason));
    out.extend_from_slice(b",\"held\":true");
    if let Some(value) = advice.filter(|v| !v.is_empty()) {
        out.extend_from_slice(b",\"advice\":");
        push_masked_json_string(&mut out, value);
    }
    out.extend_from_slice(b",\"suggestion\":");
    push_masked_json_string(&mut out, permission_denied_suggestion(reason));
    out.extend_from_slice(b"}}");
    out
}

fn error_object(output: &[u8]) -> Option<json_value::JsonMap> {
    let text = std::str::from_utf8(output).ok()?;
    let root = json_value::parse_json(text).ok()?;
    root.as_object()?.get("error")?.as_object().cloned()
}

fn is_tool_error_output_type(output: &[u8], expected: &str) -> bool {
    error_object(output)
        .and_then(|o| o.get("type").and_then(Value::as_str).map(|v| v == expected))
        .unwrap_or(false)
}

pub fn is_tool_permission_denied_output(output: &[u8]) -> bool {
    is_tool_error_output_type(output, "tool_permission_denied")
}
pub fn is_tool_review_held_output(output: &[u8]) -> bool {
    is_tool_error_output_type(output, "tool_review_held")
}

pub fn tool_permission_denial_reason(output: &[u8]) -> Option<ToolPermissionDenialReason> {
    let error = error_object(output)?;
    let review_held = match error.get("type")?.as_str()? {
        "tool_review_held" => true,
        "tool_permission_denied" => false,
        _ => return None,
    };
    let reason = parse_reason(error.get("reason")?.as_str()?)?;
    let review_reason = matches!(
        reason,
        ToolPermissionDenialReason::ReviewCaution | ToolPermissionDenialReason::ReviewUnavailable
    );
    (review_held == review_reason).then_some(reason)
}

pub fn tool_execution_failure_json(failure: &ExecutionFailure<'_>) -> Vec<u8> {
    let mut out = b"{\"error\":{\"type\":\"tool_execution_failed\",\"tool_name\":".to_vec();
    push_masked_json_string(&mut out, failure.tool_name);
    out.extend_from_slice(b",\"message\":");
    push_masked_json_string(&mut out, failure.message);
    if !failure.details.is_empty() {
        out.extend_from_slice(b",\"details\":{");
        for (index, detail) in failure.details.iter().enumerate() {
            if index != 0 {
                out.push(b',');
            }
            push_json_string(&mut out, detail.name);
            out.push(b':');
            match detail.value {
                DetailValue::String(value) => push_masked_json_string(&mut out, value),
                DetailValue::Integer(value) => out.extend_from_slice(value.to_string().as_bytes()),
                DetailValue::Unsigned(value) => out.extend_from_slice(value.to_string().as_bytes()),
                DetailValue::Boolean(value) => {
                    out.extend_from_slice(if value { b"true" } else { b"false" })
                }
            }
        }
        out.push(b'}');
    }
    if let Some(suggestion) = failure.suggestion {
        out.extend_from_slice(b",\"suggestion\":");
        push_masked_json_string(&mut out, suggestion);
    }
    out.extend_from_slice(b"}}");
    out
}

pub fn execution_error_message(error_name: &[u8]) -> Option<&'static [u8]> {
    match error_name {
        b"McpInputTimedOut" => Some(b"MCP elicitation timed out while user input was pending"),
        b"McpAuthorityChanged" => Some(b"MCP configuration or authority changed before execution"),
        _ => None,
    }
}

pub fn execution_error_suggestion(error_name: &[u8]) -> Option<&'static [u8]> {
    match error_name {
        b"McpInputTimedOut" => Some(b"Tell the user the form timed out with input pending, then retry only if they want to complete it again."),
        b"McpAuthorityChanged" => Some(b"Retry the tool on the next model step so fx can validate it against the current MCP runtime."),
        _ => None,
    }
}

pub fn format_tool_execution_error_json(tool_name: &[u8], error_name: &[u8]) -> Vec<u8> {
    let detail = [Detail {
        name: b"error",
        value: DetailValue::String(error_name),
    }];
    tool_execution_failure_json(&ExecutionFailure {
        tool_name,
        message: execution_error_message(error_name).unwrap_or(b"Tool execution failed"),
        details: &detail,
        suggestion: execution_error_suggestion(error_name),
    })
}

pub fn is_filesystem_access_denied(error_name: &[u8]) -> bool {
    matches!(error_name, b"AccessDenied" | b"PermissionDenied")
}

pub fn filesystem_access_denied_json(tool_name: &[u8], path: &[u8], error_name: &[u8]) -> Vec<u8> {
    let details = [
        Detail {
            name: b"path",
            value: DetailValue::String(path),
        },
        Detail {
            name: b"error",
            value: DetailValue::String(error_name),
        },
    ];
    tool_execution_failure_json(&ExecutionFailure {
        tool_name,
        message: b"Operating system denied filesystem access",
        details: &details,
        suggestion: Some(filesystem_access_denied_suggestion()),
    })
}

fn filesystem_access_denied_suggestion() -> &'static [u8] {
    if cfg!(target_os = "macos") {
        b"Do not retry this path unchanged or propose a symlink. fx permissions cannot override the operating system. If the path is in a protected folder such as Desktop, Documents, or Downloads, ask the user to grant the terminal app Files and Folders or Full Disk Access. Otherwise, ask the user to correct OS filesystem permissions or move/copy the project to an accessible location."
    } else {
        b"Do not retry this path unchanged or propose a symlink. fx permissions cannot override the operating system. Ask the user to correct OS filesystem permissions or move/copy the project to an accessible location."
    }
}

pub fn malformed_tool_arguments_json(tool_name: &[u8]) -> Vec<u8> {
    tool_execution_failure_json(&ExecutionFailure {
        tool_name,
        message: b"Tool arguments were not valid JSON.",
        details: &[],
        suggestion: Some(
            b"Reissue the tool call with complete valid JSON arguments matching the tool schema.",
        ),
    })
}
pub fn pre_tool_use_blocked_json(tool_name: &[u8], reason: &[u8]) -> Vec<u8> {
    tool_execution_failure_json(&ExecutionFailure { tool_name, message: reason, details: &[], suggestion: Some(b"Do not retry the same tool call unchanged. Adjust the request or use an allowed alternative.") })
}
pub fn pre_tool_use_failed_closed_json(tool_name: &[u8]) -> Vec<u8> {
    tool_execution_failure_json(&ExecutionFailure {
        tool_name,
        message: b"Tool use was blocked because a pre-execution lifecycle check failed.",
        details: &[],
        suggestion: Some(b"Do not retry unchanged. Adjust the request or try a safer alternative."),
    })
}
pub fn is_tool_execution_failed_output(output: &[u8]) -> bool {
    is_tool_error_output_type(output, "tool_execution_failed")
}
pub fn is_tool_output_error(output: &[u8]) -> bool {
    matches!(
        classify_tool_output(output),
        ToolOutputClassification::StructuredToolExecutionFailed
            | ToolOutputClassification::ActiveLegacyToolFailure
    )
}
pub fn classify_tool_output(output: &[u8]) -> ToolOutputClassification {
    if is_tool_execution_failed_output(output) {
        return ToolOutputClassification::StructuredToolExecutionFailed;
    }
    if output.starts_with(b"Tool ") && output.windows(b"failed:".len()).any(|v| v == b"failed:") {
        return ToolOutputClassification::ActiveLegacyToolFailure;
    }
    if ADAPTER_SEMANTIC_FAILURE_PREFIXES
        .iter()
        .any(|prefix| output.starts_with(prefix))
    {
        return ToolOutputClassification::AdapterSemanticFailure;
    }
    ToolOutputClassification::NonFailure
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn correction_round_trip_and_validation() {
        let correction = TerminalActionFieldCorrection {
            action: b"poll",
            invalid_fields: &[b"command"],
            missing_fields: &[b"session_id"],
            allowed_fields: &[b"action", b"session_id"],
            conflicts: &[[b"command", b"session_id"]],
        };
        let payload = terminal_action_field_correction_json(&correction);
        assert_eq!(
            inspect_terminal_action_field_correction(&payload),
            Some(TerminalActionFieldCorrectionView {
                invalid_field_count: 1
            })
        );
        assert_eq!(inspect_terminal_action_field_correction(b"{}"), None);
    }

    #[test]
    fn tool_permission_denied_json_carries_stable_fields_only() {
        let payload =
            tool_permission_denied_json(b"run_command", ToolPermissionDenialReason::UserDenied);
        let text = String::from_utf8(payload).unwrap();
        assert!(text.contains("\"type\":\"tool_permission_denied\""));
        assert!(text.contains("\"tool_name\":\"run_command\""));
        assert!(text.contains("\"reason\":\"user_denied\""));
        assert!(text.contains("\"denied\":true"));
        assert!(!text.contains("approval_request_id"));
    }

    #[test]
    fn permission_payloads_preserve_reason_categories_and_mask_advice() {
        let denied =
            tool_permission_denied_json(b"run_command", ToolPermissionDenialReason::UserDenied);
        assert!(is_tool_permission_denied_output(&denied));
        assert_eq!(
            tool_permission_denial_reason(&denied),
            Some(ToolPermissionDenialReason::UserDenied)
        );
        let held = tool_review_held_json(
            b"terminal",
            ToolPermissionDenialReason::ReviewCaution,
            Some(b"API_KEY=super-secret"),
        );
        assert!(is_tool_review_held_output(&held));
        assert!(held
            .windows(b"API_KEY=[redacted]".len())
            .any(|v| v == b"API_KEY=[redacted]"));
        assert!(!held
            .windows(b"super-secret".len())
            .any(|v| v == b"super-secret"));
        assert_eq!(
            tool_permission_denial_reason(
                b"{\"error\":{\"type\":\"tool_review_held\",\"reason\":\"auto_denied\"}}"
            ),
            None
        );
    }

    #[test]
    fn permission_denied_json_explains_policy_and_headless_blockers() {
        let policy = String::from_utf8(tool_permission_denied_json(
            b"web_search",
            ToolPermissionDenialReason::PolicyDenied,
        ))
        .unwrap();
        assert!(policy.contains("Network or browser access was denied by configured policy"));
        let shell = String::from_utf8(tool_permission_denied_json(
            b"run_command",
            ToolPermissionDenialReason::PermissionRequired,
        ))
        .unwrap();
        assert!(shell.contains("Shell command approval is required"));
        assert!(shell.contains("Noninteractive mode cannot show an approval prompt"));
    }

    #[test]
    fn execution_payloads_mask_details_and_explain_runtime_errors() {
        let details = [
            Detail {
                name: b"stderr",
                value: DetailValue::String(b"bad AKIA0123456789ABCDEF"),
            },
            Detail {
                name: b"exit_code",
                value: DetailValue::Integer(1),
            },
        ];
        let payload = tool_execution_failure_json(&ExecutionFailure {
            tool_name: b"run_command",
            message: b"failed",
            details: &details,
            suggestion: None,
        });
        assert!(is_tool_execution_failed_output(&payload));
        assert!(payload
            .windows(b"bad [redacted]".len())
            .any(|v| v == b"bad [redacted]"));
        assert!(!payload
            .windows(b"AKIA0123456789ABCDEF".len())
            .any(|v| v == b"AKIA0123456789ABCDEF"));
        let timeout = format_tool_execution_error_json(b"mcp", b"McpInputTimedOut");
        assert!(timeout
            .windows(b"user input was pending".len())
            .any(|v| v == b"user input was pending"));
    }

    #[test]
    fn tool_execution_error_json_carries_runtime_error_name() {
        let payload = format_tool_execution_error_json(b"read_file", b"FileNotFound");
        assert!(payload
            .windows(b"\"error\":\"FileNotFound\"".len())
            .any(|v| v == b"\"error\":\"FileNotFound\""));
        assert!(payload
            .windows(b"Tool execution failed".len())
            .any(|v| v == b"Tool execution failed"));
    }

    #[test]
    fn mcp_input_timeout_tells_model_user_input_was_pending() {
        let payload = format_tool_execution_error_json(b"mcp_server_tool", b"McpInputTimedOut");
        assert!(payload
            .windows(b"form timed out with input pending".len())
            .any(|v| v == b"form timed out with input pending"));
    }

    #[test]
    fn mcp_authority_change_tells_model_to_retry_next_step() {
        let payload = format_tool_execution_error_json(b"mcp_server_tool", b"McpAuthorityChanged");
        assert!(payload
            .windows(b"next model step".len())
            .any(|v| v == b"next model step"));
    }

    #[test]
    fn filesystem_and_argument_failures_include_recovery() {
        assert!(is_filesystem_access_denied(b"AccessDenied"));
        assert!(is_filesystem_access_denied(b"PermissionDenied"));
        let payload =
            filesystem_access_denied_json(b"glob_files", b"/tmp/blocked", b"PermissionDenied");
        assert!(payload
            .windows(b"Do not retry".len())
            .any(|v| v == b"Do not retry"));
        assert!(payload.windows(b"symlink".len()).any(|v| v == b"symlink"));
        let malformed = malformed_tool_arguments_json(b"ask_user_question");
        assert!(malformed
            .windows(b"tool schema".len())
            .any(|v| v == b"tool schema"));
    }

    #[test]
    fn output_classification_preserves_structured_legacy_adapter_and_success() {
        assert_eq!(
            classify_tool_output(b"{\"error\":{\"type\":\"tool_execution_failed\"}}"),
            ToolOutputClassification::StructuredToolExecutionFailed
        );
        assert_eq!(
            classify_tool_output(b"Tool read_file failed: missing"),
            ToolOutputClassification::ActiveLegacyToolFailure
        );
        assert_eq!(
            classify_tool_output(b"read_file failed: missing"),
            ToolOutputClassification::AdapterSemanticFailure
        );
        assert_eq!(
            classify_tool_output(b"Tool result: success"),
            ToolOutputClassification::NonFailure
        );
        assert!(!is_tool_output_error(b"open_url not supported on this OS"));
    }

    #[test]
    fn is_tool_output_error_recognizes_active_failure_shape() {
        assert!(is_tool_output_error(
            b"Tool read_file failed: file not found"
        ));
        assert!(is_tool_output_error(
            b"{\"error\":{\"type\":\"tool_execution_failed\"}}"
        ));
        assert!(!is_tool_output_error(b"file content here"));
    }

    #[test]
    fn malformed_tool_arguments_requests_schema_valid_retry() {
        let payload = malformed_tool_arguments_json(b"ask_user_question");
        assert!(payload
            .windows(b"valid JSON".len())
            .any(|v| v == b"valid JSON"));
        assert!(payload
            .windows(b"tool schema".len())
            .any(|v| v == b"tool schema"));
    }
}
