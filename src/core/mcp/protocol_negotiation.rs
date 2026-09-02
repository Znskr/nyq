//! Pure MCP response and stdio protocol-negotiation decisions.
//!
//! Ported 1:1 from `zig-reference/src/core/mcp/protocol_negotiation.zig`.
//! `LegacyStdioVersion::wire` returns the elicitation wire family; that module
//! is not in this batch, so the closed `Wire` enum is defined here with the
//! same variants.

use crate::core::mcp::json_value::Value;
use crate::core::mcp::mcp_contract;

#[allow(non_upper_case_globals)]
pub const legacy_protocol_version: &str = "2024-11-05";
#[allow(non_upper_case_globals)]
pub const legacy_2025_03_protocol_version: &str = "2025-03-26";
#[allow(non_upper_case_globals)]
pub const legacy_2025_06_protocol_version: &str = "2025-06-18";
#[allow(non_upper_case_globals)]
pub const legacy_2025_11_protocol_version: &str = "2025-11-25";
#[allow(non_upper_case_globals)]
pub const modern_protocol_version: &str = "2026-07-28";
#[allow(non_upper_case_globals)]
pub const unsupported_protocol_version_code: i64 = -32022;
#[allow(non_upper_case_globals)]
const missing_required_client_capability_code: i64 = -32021;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Protocol {
    Unselected,
    Legacy,
    Modern,
}

/// Matches `elicitation.Wire` variants used by stdio negotiation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Wire {
    ModernMcp,
    LegacyMcp2025_06,
    LegacyMcp2025_11,
    Acp,
}

impl Wire {
    pub fn is_legacy(self) -> bool {
        matches!(self, Self::LegacyMcp2025_06 | Self::LegacyMcp2025_11)
    }
}

#[derive(Clone, Debug)]
pub struct ProtocolError {
    pub code: i64,
    pub message: String,
    pub data: Option<Value>,
}

#[derive(Clone, Debug)]
pub enum ResponsePayload {
    Complete(Value),
    ProtocolError(ProtocolError),
}

#[derive(Clone, Debug)]
pub enum DiscoveryOutcome {
    Modern,
    LegacyFallback(LegacyStdioVersion),
    Unsupported,
    ModernProtocolError(ProtocolError),
}

/// HTTP discovery runs before protocol negotiation. Stock legacy SDK servers
/// may reject that probe with an ordinary JSON-RPC error (often `id: null`), so
/// any well-formed error falls back to legacy initialization. Only the explicit
/// modern-version retry signal keeps the connection on the modern path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HttpDiscoveryOutcome {
    Modern,
    RetryModern,
    LegacyFallback,
    Unsupported,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LegacyStdioVersion {
    V2025_11_25,
    V2025_06_18,
    V2025_03_26,
    V2024_11_05,
}

impl LegacyStdioVersion {
    pub fn string(self) -> &'static str {
        match self {
            Self::V2025_11_25 => legacy_2025_11_protocol_version,
            Self::V2025_06_18 => legacy_2025_06_protocol_version,
            Self::V2025_03_26 => legacy_2025_03_protocol_version,
            Self::V2024_11_05 => legacy_protocol_version,
        }
    }

    pub fn wire(self) -> Option<Wire> {
        match self {
            Self::V2025_11_25 => Some(Wire::LegacyMcp2025_11),
            Self::V2025_06_18 => Some(Wire::LegacyMcp2025_06),
            Self::V2025_03_26 | Self::V2024_11_05 => None,
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        if value == legacy_2025_11_protocol_version {
            Some(Self::V2025_11_25)
        } else if value == legacy_2025_06_protocol_version {
            Some(Self::V2025_06_18)
        } else if value == legacy_2025_03_protocol_version {
            Some(Self::V2025_03_26)
        } else if value == legacy_protocol_version {
            Some(Self::V2024_11_05)
        } else {
            None
        }
    }

    pub fn older(self) -> Option<Self> {
        match self {
            Self::V2025_11_25 => Some(Self::V2025_06_18),
            Self::V2025_06_18 => Some(Self::V2025_03_26),
            Self::V2025_03_26 => Some(Self::V2024_11_05),
            Self::V2024_11_05 => None,
        }
    }

    fn preference(self) -> u8 {
        match self {
            Self::V2025_11_25 => 4,
            Self::V2025_06_18 => 3,
            Self::V2025_03_26 => 2,
            Self::V2024_11_05 => 1,
        }
    }

    fn is_older_than(self, other: Self) -> bool {
        self.preference() < other.preference()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LegacyInitializeObservation {
    Accepted(LegacyStdioVersion),
    Unsupported(Option<LegacyStdioVersion>),
    ConnectionClosed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LegacyInitializeTransition {
    Accept(LegacyStdioVersion),
    Retry(LegacyStdioVersion),
    Fail,
}

pub fn decide_legacy_initialize_transition(
    offered: LegacyStdioVersion,
    observation: LegacyInitializeObservation,
) -> LegacyInitializeTransition {
    match observation {
        LegacyInitializeObservation::Accepted(negotiated) => {
            LegacyInitializeTransition::Accept(negotiated)
        }
        LegacyInitializeObservation::ConnectionClosed => {
            if let Some(version) = offered.older() {
                LegacyInitializeTransition::Retry(version)
            } else {
                LegacyInitializeTransition::Fail
            }
        }
        LegacyInitializeObservation::Unsupported(hint) => {
            if let Some(version) = hint {
                if version.is_older_than(offered) {
                    LegacyInitializeTransition::Retry(version)
                } else {
                    LegacyInitializeTransition::Fail
                }
            } else if let Some(version) = offered.older() {
                LegacyInitializeTransition::Retry(version)
            } else {
                LegacyInitializeTransition::Fail
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClassifyError {
    McpInvalidJson,
    McpInvalidProtocolError,
    McpNoResult,
    McpInvalidResult,
    McpMissingResultType,
    McpInvalidResultType,
    McpUnsupportedResultType,
    McpInvalidDiscoverResult,
    McpInitFailed,
    McpUnsupportedProtocolVersion,
}

fn json_integer(value: &Value) -> Option<i64> {
    match value {
        Value::Integer(n) => Some(*n),
        Value::NumberString(lexeme) => {
            if lexeme.contains('.') || lexeme.contains('e') || lexeme.contains('E') {
                None
            } else {
                lexeme.parse().ok()
            }
        }
        _ => None,
    }
}

pub fn classify_response_payload(
    value: &Value,
    protocol: Protocol,
) -> Result<ResponsePayload, ClassifyError> {
    mcp_contract::validate_json_rpc_response_envelope(value)
        .map_err(|_| ClassifyError::McpInvalidJson)?;
    let object = value.as_object().ok_or(ClassifyError::McpInvalidJson)?;

    if let Some(error_value) = object.get("error") {
        let error_object = error_value
            .as_object()
            .ok_or(ClassifyError::McpInvalidProtocolError)?;
        let code_value = error_object
            .get("code")
            .ok_or(ClassifyError::McpInvalidProtocolError)?;
        let message_value = error_object
            .get("message")
            .ok_or(ClassifyError::McpInvalidProtocolError)?;
        let code = json_integer(code_value).ok_or(ClassifyError::McpInvalidProtocolError)?;
        let message = message_value
            .as_str()
            .ok_or(ClassifyError::McpInvalidProtocolError)?;
        return Ok(ResponsePayload::ProtocolError(ProtocolError {
            code,
            message: message.to_string(),
            data: error_object.get("data").cloned(),
        }));
    }

    let result = object.get("result").ok_or(ClassifyError::McpNoResult)?;
    let result_object = result.as_object().ok_or(ClassifyError::McpInvalidResult)?;

    if protocol == Protocol::Modern {
        let result_type = result_object
            .get("resultType")
            .ok_or(ClassifyError::McpMissingResultType)?;
        let result_type = result_type
            .as_str()
            .ok_or(ClassifyError::McpInvalidResultType)?;
        if result_type != "complete" {
            return Err(ClassifyError::McpUnsupportedResultType);
        }
    }
    Ok(ResponsePayload::Complete(result.clone()))
}

pub fn classify_discovery_response(value: &Value) -> Result<DiscoveryOutcome, ClassifyError> {
    let object = value.as_object().ok_or(ClassifyError::McpInvalidJson)?;

    if object.get("error").is_some() {
        let payload = classify_response_payload(value, Protocol::Modern)?;
        let protocol_error = match payload {
            ResponsePayload::Complete(_) => unreachable!(),
            ResponsePayload::ProtocolError(err) => err,
        };
        if protocol_error.code == missing_required_client_capability_code {
            return Ok(DiscoveryOutcome::ModernProtocolError(protocol_error));
        }
        if protocol_error.code != unsupported_protocol_version_code {
            return Ok(DiscoveryOutcome::LegacyFallback(
                LegacyStdioVersion::V2025_11_25,
            ));
        }
        if let Some(version) = select_legacy_stdio_version(&protocol_error) {
            return Ok(DiscoveryOutcome::LegacyFallback(version));
        }
        return Ok(DiscoveryOutcome::ModernProtocolError(protocol_error));
    }

    let result = object.get("result").ok_or(ClassifyError::McpNoResult)?;
    if !result.is_object() {
        return Err(ClassifyError::McpInvalidResult);
    }

    let payload = classify_response_payload(value, Protocol::Modern)?;
    let discover = match payload {
        ResponsePayload::Complete(complete) => complete,
        ResponsePayload::ProtocolError(_) => unreachable!(),
    };
    let discover_object = discover
        .as_object()
        .ok_or(ClassifyError::McpInvalidDiscoverResult)?;
    let supported_versions = discover_object
        .get("supportedVersions")
        .ok_or(ClassifyError::McpInvalidDiscoverResult)?;
    let capabilities = discover_object
        .get("capabilities")
        .ok_or(ClassifyError::McpInvalidDiscoverResult)?;
    let versions = supported_versions
        .as_array()
        .ok_or(ClassifyError::McpInvalidDiscoverResult)?;
    if !capabilities.is_object() {
        return Err(ClassifyError::McpInvalidDiscoverResult);
    }
    let mut legacy_selection: Option<LegacyStdioVersion> = None;
    for version_value in versions {
        let version = version_value
            .as_str()
            .ok_or(ClassifyError::McpInvalidDiscoverResult)?;
        if version == modern_protocol_version {
            return Ok(DiscoveryOutcome::Modern);
        }
        if let Some(candidate) = LegacyStdioVersion::parse(version) {
            if legacy_selection.is_none_or(|current| current.is_older_than(candidate)) {
                legacy_selection = Some(candidate);
            }
        }
    }
    if let Some(version) = legacy_selection {
        return Ok(DiscoveryOutcome::LegacyFallback(version));
    }
    Ok(DiscoveryOutcome::Unsupported)
}

pub fn classify_http_discovery_response(
    value: &Value,
) -> Result<HttpDiscoveryOutcome, ClassifyError> {
    let object = value.as_object().ok_or(ClassifyError::McpInvalidJson)?;
    if object.contains("error") {
        let payload = classify_response_payload(value, Protocol::Modern)?;
        let protocol_error = match payload {
            ResponsePayload::Complete(_) => unreachable!(),
            ResponsePayload::ProtocolError(err) => err,
        };
        if protocol_error.code == unsupported_protocol_version_code
            && protocol_error_supports_version(&protocol_error, modern_protocol_version)
        {
            return Ok(HttpDiscoveryOutcome::RetryModern);
        }
        return Ok(HttpDiscoveryOutcome::LegacyFallback);
    }

    Ok(match classify_discovery_response(value)? {
        DiscoveryOutcome::Modern => HttpDiscoveryOutcome::Modern,
        DiscoveryOutcome::LegacyFallback(_) => HttpDiscoveryOutcome::LegacyFallback,
        DiscoveryOutcome::Unsupported => HttpDiscoveryOutcome::Unsupported,
        DiscoveryOutcome::ModernProtocolError(_) => unreachable!(),
    })
}

pub fn select_legacy_stdio_version(protocol_error: &ProtocolError) -> Option<LegacyStdioVersion> {
    select_legacy_stdio_version_for_request(protocol_error, modern_protocol_version)
}

pub fn select_legacy_stdio_version_for_request(
    protocol_error: &ProtocolError,
    requested_version: &str,
) -> Option<LegacyStdioVersion> {
    for version in [
        LegacyStdioVersion::V2025_11_25,
        LegacyStdioVersion::V2025_06_18,
        LegacyStdioVersion::V2025_03_26,
        LegacyStdioVersion::V2024_11_05,
    ] {
        if protocol_error_supports_version_for_request(
            protocol_error,
            requested_version,
            version.string(),
        ) {
            return Some(version);
        }
    }
    None
}

pub fn classify_legacy_initialize_response(
    value: &Value,
    offered: LegacyStdioVersion,
) -> Result<LegacyInitializeObservation, ClassifyError> {
    let payload = classify_response_payload(value, Protocol::Legacy)?;
    let result = match payload {
        ResponsePayload::Complete(complete) => complete,
        ResponsePayload::ProtocolError(protocol_error) => {
            if protocol_error.code == unsupported_protocol_version_code {
                return Ok(LegacyInitializeObservation::Unsupported(
                    select_legacy_stdio_version_for_request(&protocol_error, offered.string()),
                ));
            }
            if protocol_error.code == -32602 {
                let supported = select_legacy_stdio_version_for_request(
                    &protocol_error,
                    offered.string(),
                )
                .ok_or(ClassifyError::McpInitFailed)?;
                return Ok(LegacyInitializeObservation::Unsupported(Some(supported)));
            }
            return Err(ClassifyError::McpInitFailed);
        }
    };
    let version = result
        .as_object()
        .and_then(|object| object.get("protocolVersion"))
        .ok_or(ClassifyError::McpUnsupportedProtocolVersion)?;
    let version = version
        .as_str()
        .ok_or(ClassifyError::McpUnsupportedProtocolVersion)?;
    Ok(LegacyInitializeObservation::Accepted(
        LegacyStdioVersion::parse(version).ok_or(ClassifyError::McpUnsupportedProtocolVersion)?,
    ))
}

pub fn protocol_error_supports_version(protocol_error: &ProtocolError, version: &str) -> bool {
    protocol_error_supports_version_for_request(
        protocol_error,
        modern_protocol_version,
        version,
    )
}

pub fn protocol_error_supports_version_for_request(
    protocol_error: &ProtocolError,
    requested_version: &str,
    version: &str,
) -> bool {
    let data = match &protocol_error.data {
        Some(value) => value,
        None => return false,
    };
    let object = match data.as_object() {
        Some(object) => object,
        None => return false,
    };
    let supported = match object.get("supported") {
        Some(value) => value,
        None => return false,
    };
    let requested = match object.get("requested") {
        Some(value) => value,
        None => return false,
    };
    let supported_items = match supported.as_array() {
        Some(items) => items,
        None => return false,
    };
    let requested = match requested.as_str() {
        Some(value) => value,
        None => return false,
    };
    if requested != requested_version {
        return false;
    }
    let mut found = false;
    for supported_version in supported_items {
        let supported_version = match supported_version.as_str() {
            Some(value) => value,
            None => return false,
        };
        if supported_version == version {
            found = true;
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::mcp::json_value::parse_json;

    #[test]
    fn legacy_initialization_transitions_are_bounded_and_monotonic() {
        let cases = [
            (
                LegacyStdioVersion::V2025_11_25,
                LegacyInitializeObservation::Accepted(LegacyStdioVersion::V2025_11_25),
                LegacyInitializeTransition::Accept(LegacyStdioVersion::V2025_11_25),
            ),
            (
                LegacyStdioVersion::V2025_11_25,
                LegacyInitializeObservation::Accepted(LegacyStdioVersion::V2024_11_05),
                LegacyInitializeTransition::Accept(LegacyStdioVersion::V2024_11_05),
            ),
            (
                LegacyStdioVersion::V2025_06_18,
                LegacyInitializeObservation::Accepted(LegacyStdioVersion::V2025_11_25),
                LegacyInitializeTransition::Accept(LegacyStdioVersion::V2025_11_25),
            ),
            (
                LegacyStdioVersion::V2025_03_26,
                LegacyInitializeObservation::Accepted(LegacyStdioVersion::V2025_03_26),
                LegacyInitializeTransition::Accept(LegacyStdioVersion::V2025_03_26),
            ),
            (
                LegacyStdioVersion::V2025_11_25,
                LegacyInitializeObservation::ConnectionClosed,
                LegacyInitializeTransition::Retry(LegacyStdioVersion::V2025_06_18),
            ),
            (
                LegacyStdioVersion::V2025_06_18,
                LegacyInitializeObservation::ConnectionClosed,
                LegacyInitializeTransition::Retry(LegacyStdioVersion::V2025_03_26),
            ),
            (
                LegacyStdioVersion::V2025_03_26,
                LegacyInitializeObservation::ConnectionClosed,
                LegacyInitializeTransition::Retry(LegacyStdioVersion::V2024_11_05),
            ),
            (
                LegacyStdioVersion::V2024_11_05,
                LegacyInitializeObservation::ConnectionClosed,
                LegacyInitializeTransition::Fail,
            ),
            (
                LegacyStdioVersion::V2025_11_25,
                LegacyInitializeObservation::Unsupported(None),
                LegacyInitializeTransition::Retry(LegacyStdioVersion::V2025_06_18),
            ),
            (
                LegacyStdioVersion::V2025_06_18,
                LegacyInitializeObservation::Unsupported(None),
                LegacyInitializeTransition::Retry(LegacyStdioVersion::V2025_03_26),
            ),
            (
                LegacyStdioVersion::V2025_03_26,
                LegacyInitializeObservation::Unsupported(None),
                LegacyInitializeTransition::Retry(LegacyStdioVersion::V2024_11_05),
            ),
            (
                LegacyStdioVersion::V2024_11_05,
                LegacyInitializeObservation::Unsupported(None),
                LegacyInitializeTransition::Fail,
            ),
            (
                LegacyStdioVersion::V2025_11_25,
                LegacyInitializeObservation::Unsupported(Some(LegacyStdioVersion::V2024_11_05)),
                LegacyInitializeTransition::Retry(LegacyStdioVersion::V2024_11_05),
            ),
            (
                LegacyStdioVersion::V2025_11_25,
                LegacyInitializeObservation::Unsupported(Some(LegacyStdioVersion::V2025_11_25)),
                LegacyInitializeTransition::Fail,
            ),
            (
                LegacyStdioVersion::V2025_06_18,
                LegacyInitializeObservation::Unsupported(Some(LegacyStdioVersion::V2025_11_25)),
                LegacyInitializeTransition::Fail,
            ),
        ];
        for (offered, observation, expected) in cases {
            assert_eq!(
                expected,
                decide_legacy_initialize_transition(offered, observation)
            );
        }
    }

    #[test]
    fn mcp_legacy_stdio_negotiation_includes_2025_03_26() {
        assert_eq!(
            LegacyStdioVersion::V2025_03_26,
            LegacyStdioVersion::parse("2025-03-26").unwrap()
        );
        assert_eq!(
            LegacyStdioVersion::V2025_03_26,
            LegacyStdioVersion::V2025_06_18.older().unwrap()
        );
        assert_eq!(
            LegacyStdioVersion::V2024_11_05,
            LegacyStdioVersion::V2025_03_26.older().unwrap()
        );
    }

    #[test]
    fn method_not_found_discovery_begins_fallback_at_the_newest_legacy_version() {
        let response = parse_json(
            r#"{"jsonrpc":"2.0","id":0,"error":{"code":-32601,"message":"Method not found"}}"#,
        )
        .unwrap();
        match classify_discovery_response(&response).unwrap() {
            DiscoveryOutcome::LegacyFallback(version) => {
                assert_eq!(LegacyStdioVersion::V2025_11_25, version)
            }
            _ => panic!("TestUnexpectedResult"),
        }
    }

    #[test]
    fn ordinary_well_formed_discovery_errors_begin_legacy_fallback_independent_of_code() {
        let responses = [
            r#"{"jsonrpc":"2.0","id":0,"error":{"code":-32602,"message":"Invalid params"}}"#,
            r#"{"jsonrpc":"2.0","id":0,"error":{"code":-32000,"message":"Server not initialized"}}"#,
        ];
        for text in responses {
            let response = parse_json(text).unwrap();
            match classify_discovery_response(&response).unwrap() {
                DiscoveryOutcome::LegacyFallback(version) => {
                    assert_eq!(LegacyStdioVersion::V2025_11_25, version)
                }
                _ => panic!("TestUnexpectedResult"),
            }
        }
    }

    #[test]
    fn recognized_modern_discovery_errors_never_downgrade() {
        let responses = [
            r#"{"jsonrpc":"2.0","id":0,"error":{"code":-32021,"message":"Missing required client capability","data":{"capability":"roots"}}}"#,
            r#"{"jsonrpc":"2.0","id":0,"error":{"code":-32022,"message":"Unsupported protocol version","data":{"supported":["2026-07-28"],"requested":"2026-07-28"}}}"#,
        ];
        for text in responses {
            let response = parse_json(text).unwrap();
            match classify_discovery_response(&response).unwrap() {
                DiscoveryOutcome::ModernProtocolError(_) => {}
                _ => panic!("TestUnexpectedResult"),
            }
        }
    }

    #[test]
    fn http_discovery_state_machine_accepts_stock_sdk_null_id_errors_as_legacy_fallback() {
        let responses = [
            r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32000,"message":"Server not initialized"}}"#,
            r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32000,"message":"Bad Request: Unsupported protocol version"}}"#,
            r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32601,"message":"Method not found"}}"#,
            r#"{"jsonrpc":"2.0","error":{"code":-32004,"message":"invalid request"}}"#,
        ];
        for text in responses {
            let response = parse_json(text).unwrap();
            assert_eq!(
                HttpDiscoveryOutcome::LegacyFallback,
                classify_http_discovery_response(&response).unwrap()
            );
        }
    }

    #[test]
    fn http_discovery_retries_modern_only_when_the_server_explicitly_supports_it() {
        let response = parse_json(
            r#"{"jsonrpc":"2.0","id":0,"error":{"code":-32022,"message":"Unsupported protocol version","data":{"supported":["2026-07-28"],"requested":"2026-07-28"}}}"#,
        )
        .unwrap();
        assert_eq!(
            HttpDiscoveryOutcome::RetryModern,
            classify_http_discovery_response(&response).unwrap()
        );
    }

    #[test]
    fn discovery_selects_the_newest_legacy_version_independent_of_server_ordering() {
        let responses = [
            r#"{"jsonrpc":"2.0","id":0,"result":{"resultType":"complete","supportedVersions":["2024-11-05","2025-06-18","2025-11-25"],"capabilities":{}}}"#,
            r#"{"jsonrpc":"2.0","id":0,"result":{"resultType":"complete","supportedVersions":["2025-11-25","2025-06-18","2024-11-05"],"capabilities":{}}}"#,
        ];
        for text in responses {
            let response = parse_json(text).unwrap();
            match classify_discovery_response(&response).unwrap() {
                DiscoveryOutcome::LegacyFallback(version) => {
                    assert_eq!(LegacyStdioVersion::V2025_11_25, version)
                }
                _ => panic!("TestUnexpectedResult"),
            }
        }
    }
}
