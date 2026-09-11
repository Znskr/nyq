//! Bounded permission request/response payloads shared by prompt transports.
//!
//! Ported from `zig-reference/src/core/permissions/permission_request.zig`.
//! The Zig source separates borrowed views from allocator-owned clones so
//! requests can cross async boundaries; the Rust port keeps a single owned
//! type and preserves the capacity preflight that guards those copies.

use crate::core::output::diff::{FileChangePreview, MAX_ENCODED_LABEL_BYTES, MAX_PREVIEW_LINES};
use crate::core::shared::types::ToolPermissionDecision;
use crate::core::tooling::file_mutation_contract::{
    FileApprovalIntent, FileApprovalScope, Kind,
};

pub const max_external_scope_tail_bytes: usize =
    crate::core::tooling::file_mutation_contract::max_external_scope_tail_bytes;
pub const max_tool_arguments_preview_bytes: usize = MAX_ENCODED_LABEL_BYTES;
const MAX_EXPLANATION_BYTES: usize = 256;
const MAX_SUBAGENT_ORIGIN_BYTES: usize = 128;

pub use crate::core::output::diff::MAX_ENCODED_PATH_BYTES as max_encoded_path_bytes;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileApprovalRequest {
    pub kind: Kind,
    pub intent: FileApprovalIntent,
    pub preview: FileChangePreview,
    pub scope: FileApprovalScope,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum RequestOrigin {
    #[default]
    ActiveSession,
    Subagent(Vec<u8>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PermissionRequest {
    pub id: u64,
    pub label: Vec<u8>,
    pub origin: RequestOrigin,
    /// Optional bounded, terminal-safe text displayed as the approval reason.
    pub explanation: Option<Vec<u8>>,
    /// Optional bounded display projection of dynamic tool arguments.
    /// Renderers must still treat these bytes as untrusted terminal input.
    /// Execution must continue to use the original tool call payload.
    pub tool_arguments_preview: Option<Vec<u8>>,
    pub command: Option<Vec<u8>>,
    pub file: Option<FileApprovalRequest>,
    pub amendment_allowed: bool,
    pub confirmation_only: bool,
}

impl Default for PermissionRequest {
    fn default() -> Self {
        Self {
            id: 0,
            label: Vec::new(),
            origin: RequestOrigin::ActiveSession,
            explanation: None,
            tool_arguments_preview: None,
            command: None,
            file: None,
            amendment_allowed: true,
            confirmation_only: false,
        }
    }
}

impl PermissionRequest {
    pub fn eql(&self, other: &Self) -> bool {
        self == other
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RequestError {
    OutOfMemory,
    RequestProjectionTooLarge,
    ExplanationTooLong,
    LabelTooLong,
    ScopeProjectionTooLarge,
    SubagentOriginTooLong,
    ToolArgumentsPreviewTooLong,
    RequestIdExhausted,
    TooManyLines,
    PathTooLong,
    InvalidPathMetadata,
    LineTooLong,
    PreviewTooLarge,
    InvalidLineNumbers,
}

impl From<crate::core::output::diff::PreviewValidationError> for RequestError {
    fn from(e: crate::core::output::diff::PreviewValidationError) -> Self {
        match e {
            crate::core::output::diff::PreviewValidationError::TooManyLines => Self::TooManyLines,
            crate::core::output::diff::PreviewValidationError::PathTooLong => Self::PathTooLong,
            crate::core::output::diff::PreviewValidationError::InvalidPathMetadata => {
                Self::InvalidPathMetadata
            }
            crate::core::output::diff::PreviewValidationError::LineTooLong => Self::LineTooLong,
            crate::core::output::diff::PreviewValidationError::PreviewTooLarge => {
                Self::PreviewTooLarge
            }
            crate::core::output::diff::PreviewValidationError::InvalidLineNumbers => {
                Self::InvalidLineNumbers
            }
        }
    }
}

/// Upper bound on a fully owned file permission request, matching the Zig
/// capacity preflight that transports rely on for fixed-size staging.
pub const MAX_FILE_REQUEST_FOOTPRINT_BYTES: usize = std::mem::size_of::<PermissionRequest>()
    + MAX_ENCODED_LABEL_BYTES
    + MAX_SUBAGENT_ORIGIN_BYTES
    + MAX_EXPLANATION_BYTES
    + max_tool_arguments_preview_bytes
    + max_encoded_path_bytes
    + max_external_scope_tail_bytes
    + MAX_PREVIEW_LINES * std::mem::size_of::<crate::core::output::diff::PreviewLine>()
    + crate::core::output::diff::MAX_ENCODED_PREVIEW_BYTES;

pub fn file_request_footprint(request: &PermissionRequest) -> Result<usize, RequestError> {
    if request.label.len() > MAX_ENCODED_LABEL_BYTES {
        return Err(RequestError::LabelTooLong);
    }
    let file_request = request
        .file
        .as_ref()
        .ok_or(RequestError::RequestProjectionTooLarge)?;
    file_request.preview.validate().map_err(RequestError::from)?;

    let mut footprint = std::mem::size_of::<PermissionRequest>();
    footprint = checked_add_footprint(footprint, request.label.len())?;
    footprint = checked_add_footprint(footprint, request_origin_len(&request.origin)?)?;
    footprint = checked_add_footprint(footprint, explanation_len(request)?)?;
    footprint = checked_add_footprint(footprint, tool_arguments_preview_len(request)?)?;
    footprint = checked_add_footprint(footprint, file_request.preview.path.len())?;
    footprint = checked_add_footprint(
        footprint,
        match &file_request.scope {
            FileApprovalScope::WorkspaceFiles => 0,
            FileApprovalScope::ExternalTree(root_tail) => {
                if root_tail.len() > max_external_scope_tail_bytes {
                    return Err(RequestError::ScopeProjectionTooLarge);
                }
                root_tail.len()
            }
        },
    )?;
    footprint = checked_add_footprint(
        footprint,
        file_request
            .preview
            .lines
            .len()
            .checked_mul(std::mem::size_of::<crate::core::output::diff::PreviewLine>())
            .ok_or(RequestError::RequestProjectionTooLarge)?,
    )?;
    for line in &file_request.preview.lines {
        footprint = checked_add_footprint(footprint, line.text.len())?;
    }
    if footprint > MAX_FILE_REQUEST_FOOTPRINT_BYTES {
        return Err(RequestError::RequestProjectionTooLarge);
    }
    Ok(footprint)
}

fn checked_add_footprint(a: usize, b: usize) -> Result<usize, RequestError> {
    a.checked_add(b).ok_or(RequestError::RequestProjectionTooLarge)
}

fn explanation_len(request: &PermissionRequest) -> Result<usize, RequestError> {
    let Some(explanation) = &request.explanation else {
        return Ok(0);
    };
    if explanation.len() > MAX_EXPLANATION_BYTES {
        return Err(RequestError::ExplanationTooLong);
    }
    Ok(explanation.len())
}

fn tool_arguments_preview_len(request: &PermissionRequest) -> Result<usize, RequestError> {
    let Some(preview) = &request.tool_arguments_preview else {
        return Ok(0);
    };
    if preview.len() > max_tool_arguments_preview_bytes {
        return Err(RequestError::ToolArgumentsPreviewTooLong);
    }
    Ok(preview.len())
}

fn request_origin_len(origin: &RequestOrigin) -> Result<usize, RequestError> {
    match origin {
        RequestOrigin::ActiveSession => Ok(0),
        RequestOrigin::Subagent(child_name) => {
            if child_name.len() > MAX_SUBAGENT_ORIGIN_BYTES {
                return Err(RequestError::SubagentOriginTooLong);
            }
            Ok(child_name.len())
        }
    }
}

/// Validates every bounded component of a request before any copy is made.
pub fn preflight_request(request: &PermissionRequest) -> Result<(), RequestError> {
    if request.label.len() > MAX_ENCODED_LABEL_BYTES {
        return Err(RequestError::LabelTooLong);
    }
    if request.file.is_some() {
        file_request_footprint(request)?;
        return Ok(());
    }
    let mut footprint = request.label.len();
    footprint = checked_add_footprint(footprint, request_origin_len(&request.origin)?)?;
    footprint = checked_add_footprint(footprint, explanation_len(request)?)?;
    footprint = checked_add_footprint(footprint, tool_arguments_preview_len(request)?)?;
    if footprint > crate::core::output::diff::MAX_REQUEST_PROJECTION_BYTES {
        return Err(RequestError::RequestProjectionTooLarge);
    }
    Ok(())
}

/// Transport-confirmed permission response with optional bounded feedback.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OwnedPermissionResponse {
    pub decision: ToolPermissionDecision,
    pub feedback: Option<Vec<u8>>,
}

impl OwnedPermissionResponse {
    pub fn init(decision: ToolPermissionDecision, feedback: Option<Vec<u8>>) -> Self {
        Self { decision, feedback }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_preview() -> FileChangePreview {
        FileChangePreview {
            path: b"note.txt".to_vec(),
            path_basename_start: 5,
            path_source_truncated: false,
            lines: vec![
                crate::core::output::diff::PreviewLine {
                    op: crate::core::output::diff::PreviewOp::Deletion,
                    old_line: Some(2),
                    new_line: None,
                    text: b"beta".to_vec(),
                },
                crate::core::output::diff::PreviewLine {
                    op: crate::core::output::diff::PreviewOp::Addition,
                    old_line: None,
                    new_line: Some(2),
                    text: b"BETA".to_vec(),
                },
            ],
            additions: 1,
            deletions: 1,
            truncated: false,
        }
    }

    #[test]
    fn permission_request_exposes_amendment_capability() {
        // Compile-level contract: both request shapes carry the flag.
        let request = PermissionRequest::default();
        assert!(request.amendment_allowed);
    }

    #[test]
    fn owned_request_roundtrips_with_explanation() {
        let request = PermissionRequest {
            label: b"terminal.exec touch marker.txt".to_vec(),
            explanation: Some(
                "Auto agent couldn\u{2019}t approve because the action needs review".as_bytes().to_vec(),
            ),
            ..PermissionRequest::default()
        };
        let clone = request.clone();
        assert_eq!(request.explanation.as_deref(), clone.explanation.as_deref());
        assert!(request.eql(&clone));
    }

    #[test]
    fn owned_request_roundtrips_tool_arguments_preview() {
        let request = PermissionRequest {
            label: b"mcp_fixture_echo".to_vec(),
            tool_arguments_preview: Some(b"{\"text\":\"sentinel\"}".to_vec()),
            ..PermissionRequest::default()
        };
        let clone = request.clone();
        assert_eq!(
            request.tool_arguments_preview.as_deref(),
            clone.tool_arguments_preview.as_deref()
        );
        assert!(request.eql(&clone));
    }

    #[test]
    fn owned_request_roundtrips_subagent_origin() {
        let request = PermissionRequest {
            label: b"terminal.exec touch marker.txt".to_vec(),
            origin: RequestOrigin::Subagent(b"approval-child".to_vec()),
            ..PermissionRequest::default()
        };
        let clone = request.clone();
        match &clone.origin {
            RequestOrigin::ActiveSession => panic!("expected subagent origin"),
            RequestOrigin::Subagent(child_name) => {
                assert_eq!(child_name, b"approval-child");
            }
        }
        assert!(request.eql(&clone));
    }

    #[test]
    fn bounds_the_optional_explanation() {
        let explanation = vec![b'x'; MAX_EXPLANATION_BYTES + 1];
        let request = PermissionRequest {
            label: b"terminal.exec touch marker.txt".to_vec(),
            explanation: Some(explanation),
            ..PermissionRequest::default()
        };
        assert_eq!(preflight_request(&request), Err(RequestError::ExplanationTooLong));
    }

    #[test]
    fn bounds_the_optional_tool_arguments_preview() {
        let preview = vec![b'x'; max_tool_arguments_preview_bytes + 1];
        let request = PermissionRequest {
            label: b"mcp_fixture_echo".to_vec(),
            tool_arguments_preview: Some(preview),
            ..PermissionRequest::default()
        };
        assert_eq!(
            preflight_request(&request),
            Err(RequestError::ToolArgumentsPreviewTooLong)
        );
    }

    #[test]
    fn bounds_the_subagent_origin() {
        let child_name = vec![b'x'; 129];
        let request = PermissionRequest {
            label: b"terminal.exec touch marker.txt".to_vec(),
            origin: RequestOrigin::Subagent(child_name),
            ..PermissionRequest::default()
        };
        assert_eq!(
            preflight_request(&request),
            Err(RequestError::SubagentOriginTooLong)
        );
    }

    #[test]
    fn owned_request_deep_copies_every_field() {
        let preview = sample_preview();
        let request = PermissionRequest {
            id: 41,
            label: b"file_mutation".to_vec(),
            file: Some(FileApprovalRequest {
                kind: Kind::Edit,
                intent: FileApprovalIntent::EqualityDisclosure,
                preview,
                scope: FileApprovalScope::ExternalTree(b"/tmp/project".to_vec()),
            }),
            ..PermissionRequest::default()
        };
        let clone = request.clone();
        assert_eq!(clone.id, 41);
        assert_eq!(clone.label, b"file_mutation");
        let file = clone.file.as_ref().unwrap();
        assert_eq!(file.kind, Kind::Edit);
        assert_eq!(file.intent, FileApprovalIntent::EqualityDisclosure);
        assert!(file.preview.eql(&request.file.as_ref().unwrap().preview));
        assert_eq!(file.scope, FileApprovalScope::ExternalTree(b"/tmp/project".to_vec()));
        assert!(request.eql(&clone));
    }

    #[test]
    fn file_request_footprint_counts_root_and_owned_backing_exactly_once() {
        let request = PermissionRequest {
            id: 9,
            label: b"file".to_vec(),
            file: Some(FileApprovalRequest {
                kind: Kind::Write,
                intent: FileApprovalIntent::Mutation,
                preview: sample_preview(),
                scope: FileApprovalScope::ExternalTree(b"/tmp/project".to_vec()),
            }),
            ..PermissionRequest::default()
        };
        let expected = std::mem::size_of::<PermissionRequest>()
            + request.label.len()
            + request.file.as_ref().unwrap().preview.path.len()
            + b"/tmp/project".len()
            + request.file.as_ref().unwrap().preview.lines.len()
                * std::mem::size_of::<crate::core::output::diff::PreviewLine>()
            + request.file.as_ref().unwrap().preview.lines[0].text.len()
            + request.file.as_ref().unwrap().preview.lines[1].text.len();
        assert_eq!(file_request_footprint(&request), Ok(expected));
    }

    #[test]
    fn workspace_scope_has_no_separately_owned_backing() {
        let request = PermissionRequest {
            label: b"file".to_vec(),
            file: Some(FileApprovalRequest {
                kind: Kind::Write,
                intent: FileApprovalIntent::Mutation,
                preview: sample_preview(),
                scope: FileApprovalScope::WorkspaceFiles,
            }),
            ..PermissionRequest::default()
        };
        let expected = std::mem::size_of::<PermissionRequest>()
            + request.label.len()
            + request.file.as_ref().unwrap().preview.path.len()
            + request.file.as_ref().unwrap().preview.lines.len()
                * std::mem::size_of::<crate::core::output::diff::PreviewLine>()
            + request.file.as_ref().unwrap().preview.lines[0].text.len()
            + request.file.as_ref().unwrap().preview.lines[1].text.len();
        assert_eq!(file_request_footprint(&request), Ok(expected));
    }

    #[test]
    fn file_request_accepts_exact_component_and_aggregate_capacity() {
        let label = vec![b'l'; MAX_ENCODED_LABEL_BYTES];
        let explanation = vec![b'e'; MAX_EXPLANATION_BYTES];
        let tool_arguments_preview = vec![b'a'; max_tool_arguments_preview_bytes];
        let origin = vec![b'o'; MAX_SUBAGENT_ORIGIN_BYTES];
        let path = vec![b'p'; max_encoded_path_bytes];
        let scope = vec![b's'; max_external_scope_tail_bytes];
        let line_text = vec![b'x'; crate::core::output::diff::MAX_ENCODED_LINE_BYTES];

        let lines = vec![
            crate::core::output::diff::PreviewLine {
                op: crate::core::output::diff::PreviewOp::Deletion,
                old_line: Some(1),
                new_line: None,
                text: line_text.clone(),
            },
            crate::core::output::diff::PreviewLine {
                op: crate::core::output::diff::PreviewOp::Addition,
                old_line: None,
                new_line: Some(1),
                text: line_text.clone(),
            },
            crate::core::output::diff::PreviewLine {
                op: crate::core::output::diff::PreviewOp::Deletion,
                old_line: Some(2),
                new_line: None,
                text: line_text.clone(),
            },
            crate::core::output::diff::PreviewLine {
                op: crate::core::output::diff::PreviewOp::Addition,
                old_line: None,
                new_line: Some(2),
                text: line_text.clone(),
            },
            crate::core::output::diff::PreviewLine {
                op: crate::core::output::diff::PreviewOp::Deletion,
                old_line: Some(3),
                new_line: None,
                text: line_text.clone(),
            },
            crate::core::output::diff::PreviewLine {
                op: crate::core::output::diff::PreviewOp::Addition,
                old_line: None,
                new_line: Some(3),
                text: line_text,
            },
        ];
        let request = PermissionRequest {
            label,
            origin: RequestOrigin::Subagent(origin),
            explanation: Some(explanation),
            tool_arguments_preview: Some(tool_arguments_preview),
            file: Some(FileApprovalRequest {
                kind: Kind::Edit,
                intent: FileApprovalIntent::Mutation,
                preview: FileChangePreview {
                    path,
                    path_basename_start: max_encoded_path_bytes - 1,
                    path_source_truncated: false,
                    lines,
                    additions: 3,
                    deletions: 3,
                    truncated: false,
                },
                scope: FileApprovalScope::ExternalTree(scope),
            }),
            ..PermissionRequest::default()
        };

        assert_eq!(
            file_request_footprint(&request),
            Ok(MAX_FILE_REQUEST_FOOTPRINT_BYTES)
        );
        assert_eq!(preflight_request(&request), Ok(()));
    }

    #[test]
    fn file_request_rejects_one_byte_over_every_owned_component_boundary() {
        let over_label = vec![b'l'; MAX_ENCODED_LABEL_BYTES + 1];
        let over_path = vec![b'p'; max_encoded_path_bytes + 1];
        let over_scope = vec![b's'; max_external_scope_tail_bytes + 1];
        let over_line = vec![b'x'; crate::core::output::diff::MAX_ENCODED_LINE_BYTES + 1];

        let base = sample_preview();

        let request = PermissionRequest {
            label: over_label,
            file: Some(FileApprovalRequest {
                kind: Kind::Write,
                intent: FileApprovalIntent::Mutation,
                preview: base.clone(),
                scope: FileApprovalScope::WorkspaceFiles,
            }),
            ..PermissionRequest::default()
        };
        assert_eq!(
            file_request_footprint(&request),
            Err(RequestError::LabelTooLong)
        );

        let mut over_path_preview = base.clone();
        over_path_preview.path = over_path;
        over_path_preview.path_basename_start = max_encoded_path_bytes;
        let request = PermissionRequest {
            label: b"file".to_vec(),
            file: Some(FileApprovalRequest {
                kind: Kind::Write,
                intent: FileApprovalIntent::Mutation,
                preview: over_path_preview,
                scope: FileApprovalScope::WorkspaceFiles,
            }),
            ..PermissionRequest::default()
        };
        assert_eq!(
            file_request_footprint(&request),
            Err(RequestError::PathTooLong)
        );

        let request = PermissionRequest {
            label: b"file".to_vec(),
            file: Some(FileApprovalRequest {
                kind: Kind::Write,
                intent: FileApprovalIntent::Mutation,
                preview: base.clone(),
                scope: FileApprovalScope::ExternalTree(over_scope),
            }),
            ..PermissionRequest::default()
        };
        assert_eq!(
            file_request_footprint(&request),
            Err(RequestError::ScopeProjectionTooLarge)
        );

        let mut over_line_preview = base;
        over_line_preview.lines = vec![crate::core::output::diff::PreviewLine {
            op: crate::core::output::diff::PreviewOp::Addition,
            old_line: None,
            new_line: Some(1),
            text: over_line,
        }];
        over_line_preview.additions = 1;
        over_line_preview.deletions = 0;
        let request = PermissionRequest {
            label: b"file".to_vec(),
            file: Some(FileApprovalRequest {
                kind: Kind::Write,
                intent: FileApprovalIntent::Mutation,
                preview: over_line_preview,
                scope: FileApprovalScope::WorkspaceFiles,
            }),
            ..PermissionRequest::default()
        };
        assert_eq!(
            file_request_footprint(&request),
            Err(RequestError::LineTooLong)
        );
    }
}
