//! Vision request/result contracts (ported 1:1 from
//! `zig-reference/src/core/agent/runtime/vision_contracts.zig`).

use crate::core::mcp::json_value::{self, JsonMap, Value};
use crate::core::shared::types::{
    ChatCachePolicy, ChatMessage, ChatRole, ImageAttachment, ToolCall,
};
use crate::core::tooling::tool_result_errors::is_tool_execution_failed_output;

#[allow(non_upper_case_globals)]
const max_focus_bytes: usize = 4096;
#[allow(non_upper_case_globals)]
const max_provider_images_per_batch: usize = 8;
const MAX_PATH_BYTES: usize = 4096;
const IMAGE_PLACEHOLDER_PREFIX: &[u8] = b"[Image #";

#[allow(non_upper_case_globals)]
pub const native_route_unavailable_message: &str = "Vision is unavailable for this request.";
#[allow(non_upper_case_globals)]
pub const provider_response_format_name: &str = "fx_vision_evidence";
#[allow(non_upper_case_globals)]
pub const provider_response_format_description: &str =
    "Factual evidence extracted from the requested images.";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VisionRequestError {
    InvalidVisionJson,
    InvalidVisionRequest,
    EmptyImageIds,
    InvalidImageId,
    DuplicateImageId,
    EmptyPaths,
    InvalidImagePath,
    DuplicateImagePath,
    EmptyFocus,
    FocusTooLong,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VisionResultError {
    InvalidVisionJson,
    InvalidVisionResult,
    InvalidImageId,
    DuplicateImageId,
    UnauthorizedImageId,
    ProviderResultTooLarge,
    TooManyProviderImages,
    EmptyImageIds,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidVisionResponseImageCount;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum VisionSource {
    ImageIds(Vec<usize>),
    Paths(Vec<Vec<u8>>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VisionRequest {
    pub source: VisionSource,
    pub focus: Vec<u8>,
}

impl VisionRequest {
    pub fn image_ids(&self) -> Option<&[usize]> {
        match &self.source {
            VisionSource::ImageIds(ids) => Some(ids),
            VisionSource::Paths(_) => None,
        }
    }

    pub fn paths(&self) -> Option<&[Vec<u8>]> {
        match &self.source {
            VisionSource::ImageIds(_) => None,
            VisionSource::Paths(paths) => Some(paths),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OutputLimit {
    pub observed_bytes: usize,
    pub configured_bytes: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VisionFailure {
    ImageUnavailable,
    ProviderResponseInvalid,
    OutputLimitExceeded(OutputLimit),
    VisionUnavailable,
    MissingProviderRecord,
}

impl VisionFailure {
    pub fn code(self) -> FailureCode {
        match self {
            Self::ImageUnavailable => FailureCode::ImageUnavailable,
            Self::ProviderResponseInvalid => FailureCode::ProviderResponseInvalid,
            Self::OutputLimitExceeded(_) => FailureCode::OutputLimitExceeded,
            Self::VisionUnavailable => FailureCode::VisionUnavailable,
            Self::MissingProviderRecord => FailureCode::MissingProviderRecord,
        }
    }

    fn retryable(self) -> bool {
        !matches!(self, Self::ImageUnavailable)
    }

    fn suggestion(self) -> &'static str {
        match self {
            Self::ImageUnavailable => {
                "Explain the local image failure; do not retry the same snapshot unchanged."
            }
            Self::ProviderResponseInvalid => {
                "Try a later explicit Vision call if useful, or continue without visual claims."
            }
            Self::OutputLimitExceeded(_) => {
                "Call Vision again with a narrower focus or fewer images."
            }
            Self::VisionUnavailable => {
                "Retry later, change model or strategy, or continue without visual claims."
            }
            Self::MissingProviderRecord => {
                "Call Vision again for only this image if its evidence is still needed."
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FailureCode {
    ImageUnavailable,
    ProviderResponseInvalid,
    OutputLimitExceeded,
    VisionUnavailable,
    MissingProviderRecord,
}

impl FailureCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ImageUnavailable => "image_unavailable",
            Self::ProviderResponseInvalid => "provider_response_invalid",
            Self::OutputLimitExceeded => "output_limit_exceeded",
            Self::VisionUnavailable => "vision_unavailable",
            Self::MissingProviderRecord => "missing_provider_record",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImageEvidence {
    pub summary: Vec<u8>,
    pub visible_text: Vec<Vec<u8>>,
    pub details: Vec<Vec<u8>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ImageOutcome {
    Ok(ImageEvidence),
    Failed(VisionFailure),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VisionImageResult {
    pub image_id: usize,
    pub outcome: ImageOutcome,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VisionResult {
    pub images: Vec<VisionImageResult>,
}

/// Builds the provider-enforced JSON shape for one verified Vision batch.
pub fn build_provider_response_schema(
    image_count: usize,
) -> Result<Vec<u8>, InvalidVisionResponseImageCount> {
    if image_count == 0 || image_count > max_provider_images_per_batch {
        return Err(InvalidVisionResponseImageCount);
    }

    let mut out = Vec::new();
    out.extend_from_slice(
        br#"{"type":"object","properties":{"images":{"type":"array","minItems":"#,
    );
    out.extend_from_slice(image_count.to_string().as_bytes());
    out.extend_from_slice(br#","maxItems":"#);
    out.extend_from_slice(image_count.to_string().as_bytes());
    out.extend_from_slice(
        br#","items":{"anyOf":[{"type":"object","properties":{"image_id":{"type":"integer"},"status":{"type":"string","enum":["ok"]},"summary":{"type":"string"},"visible_text":{"type":"array","items":{"type":"string"}},"details":{"type":"array","items":{"type":"string"}}},"required":["image_id","status","summary","visible_text","details"],"additionalProperties":false},{"type":"object","properties":{"image_id":{"type":"integer"},"status":{"type":"string","enum":["failed"]},"error":{"type":"string","enum":["vision_unavailable"]}},"required":["image_id","status","error"],"additionalProperties":false}]}}},"required":["images"],"additionalProperties":false}"#,
    );
    Ok(out)
}

pub fn parse_vision_request(json: &[u8]) -> Result<VisionRequest, VisionRequestError> {
    let text = std::str::from_utf8(json).map_err(|_| VisionRequestError::InvalidVisionJson)?;
    let parsed = json_value::parse_json(text).map_err(|_| VisionRequestError::InvalidVisionJson)?;
    let object = match &parsed {
        Value::Object(object) => object,
        _ => return Err(VisionRequestError::InvalidVisionRequest),
    };
    let image_ids_value = object.get("image_ids");
    let paths_value = object.get("paths");
    if object.len() != 2 || image_ids_value.is_none() == paths_value.is_none() {
        return Err(VisionRequestError::InvalidVisionRequest);
    }

    let source = if let Some(value) = image_ids_value {
        let items = match value {
            Value::Array(items) => items,
            _ => return Err(VisionRequestError::InvalidVisionRequest),
        };
        if items.is_empty() {
            return Err(VisionRequestError::EmptyImageIds);
        }
        let mut image_ids = Vec::with_capacity(items.len());
        for item in items {
            image_ids.push(parse_image_id(item).map_err(|_| VisionRequestError::InvalidImageId)?);
        }
        validate_image_ids(&image_ids)?;
        VisionSource::ImageIds(image_ids)
    } else {
        let value = paths_value.expect("XOR check requires paths when image_ids is absent");
        let items = match value {
            Value::Array(items) => items,
            _ => return Err(VisionRequestError::InvalidVisionRequest),
        };
        if items.is_empty() {
            return Err(VisionRequestError::EmptyPaths);
        }
        let mut paths = Vec::with_capacity(items.len());
        for item in items {
            let path = match item {
                Value::String(path) => path.as_bytes(),
                _ => return Err(VisionRequestError::InvalidImagePath),
            };
            if trim_ascii_ws(path).is_empty() || path.len() > MAX_PATH_BYTES {
                return Err(VisionRequestError::InvalidImagePath);
            }
            if paths.iter().any(|previous: &Vec<u8>| previous == path) {
                return Err(VisionRequestError::DuplicateImagePath);
            }
            paths.push(path.to_vec());
        }
        VisionSource::Paths(paths)
    };

    let focus_value = object
        .get("focus")
        .ok_or(VisionRequestError::InvalidVisionRequest)?;
    let focus = match focus_value {
        Value::String(focus) => focus.as_bytes(),
        _ => return Err(VisionRequestError::InvalidVisionRequest),
    };
    if trim_ascii_ws(focus).is_empty() {
        return Err(VisionRequestError::EmptyFocus);
    }
    if focus.len() > max_focus_bytes {
        return Err(VisionRequestError::FocusTooLong);
    }

    Ok(VisionRequest {
        source,
        focus: focus.to_vec(),
    })
}

/// Parses provider JSON up to the caller-supplied effective capture bound.
pub fn parse_vision_provider_result(
    json: &[u8],
    max_result_bytes: usize,
) -> Result<VisionResult, VisionResultError> {
    if json.len() > max_result_bytes {
        return Err(VisionResultError::ProviderResultTooLarge);
    }
    let payload = single_json_fence_payload(json).unwrap_or(json);
    let text = std::str::from_utf8(payload).map_err(|_| VisionResultError::InvalidVisionJson)?;
    let parsed = json_value::parse_json(text).map_err(|_| VisionResultError::InvalidVisionJson)?;
    let object =
        exact_object(&parsed, &["images"]).ok_or(VisionResultError::InvalidVisionResult)?;
    let images_value = object
        .get("images")
        .ok_or(VisionResultError::InvalidVisionResult)?;
    let items = match images_value {
        Value::Array(items) => items,
        _ => return Err(VisionResultError::InvalidVisionResult),
    };
    if items.len() > max_provider_images_per_batch {
        return Err(VisionResultError::TooManyProviderImages);
    }
    if items.is_empty() {
        return Err(VisionResultError::InvalidVisionResult);
    }

    let mut images = Vec::with_capacity(items.len());
    for value in items {
        let image = parse_image_result(value)?;
        if images
            .iter()
            .any(|previous: &VisionImageResult| previous.image_id == image.image_id)
        {
            return Err(VisionResultError::DuplicateImageId);
        }
        images.push(image);
    }
    Ok(VisionResult { images })
}

fn single_json_fence_payload(bytes: &[u8]) -> Option<&[u8]> {
    let trimmed = trim_ascii_ws(bytes);
    const PREFIX: &[u8] = b"```json";
    if !trimmed.starts_with(PREFIX) {
        return None;
    }
    let after_prefix = &trimmed[PREFIX.len()..];
    if after_prefix.is_empty() || (after_prefix[0] != b'\n' && after_prefix[0] != b'\r') {
        return None;
    }
    const SUFFIX: &[u8] = b"```";
    if !trimmed.ends_with(SUFFIX) {
        return None;
    }
    let body = &trimmed[PREFIX.len()..trimmed.len() - SUFFIX.len()];
    Some(trim_ascii_ws(body))
}

/// Returns a deep-owned result in the exact order of `requested_ids`.
pub fn merge_vision_results(
    requested_ids: &[usize],
    provider_records: &[VisionImageResult],
) -> Result<VisionResult, VisionResultError> {
    validate_image_ids(requested_ids).map_err(|err| match err {
        VisionRequestError::EmptyImageIds => VisionResultError::EmptyImageIds,
        VisionRequestError::InvalidImageId => VisionResultError::InvalidImageId,
        VisionRequestError::DuplicateImageId => VisionResultError::DuplicateImageId,
        other => unreachable!("validate_image_ids returned {other:?}"),
    })?;
    for (index, record) in provider_records.iter().enumerate() {
        validate_result_record(record)?;
        if provider_records[..index]
            .iter()
            .any(|previous| previous.image_id == record.image_id)
        {
            return Err(VisionResultError::DuplicateImageId);
        }
        if !contains_image_id(requested_ids, record.image_id) {
            return Err(VisionResultError::UnauthorizedImageId);
        }
    }

    let mut images = Vec::with_capacity(requested_ids.len());
    for &image_id in requested_ids {
        let image = if let Some(record) = find_result_record(provider_records, image_id) {
            record.clone()
        } else {
            VisionImageResult {
                image_id,
                outcome: ImageOutcome::Failed(VisionFailure::MissingProviderRecord),
            }
        };
        images.push(image);
    }
    Ok(VisionResult { images })
}

/// Serializes a validated path-free Vision result.
pub fn stringify_vision_result(result: &VisionResult) -> Result<Vec<u8>, VisionResultError> {
    validate_vision_result(result)?;

    let mut out = b"{\"images\":[".to_vec();
    for (index, image) in result.images.iter().enumerate() {
        if index > 0 {
            out.push(b',');
        }
        out.extend_from_slice(b"{\"image_id\":");
        out.extend_from_slice(image.image_id.to_string().as_bytes());
        out.push(b',');
        match &image.outcome {
            ImageOutcome::Ok(evidence) => {
                out.extend_from_slice(b"\"status\":\"ok\",\"summary\":");
                write_json_string(&mut out, &evidence.summary);
                out.extend_from_slice(b",\"visible_text\":");
                write_json_string_array(&mut out, &evidence.visible_text);
                out.extend_from_slice(b",\"details\":");
                write_json_string_array(&mut out, &evidence.details);
            }
            ImageOutcome::Failed(failure) => {
                out.extend_from_slice(b"\"status\":\"failed\",\"error\":");
                write_failure_diagnostic(&mut out, *failure);
            }
        }
        out.push(b'}');
    }
    out.extend_from_slice(b"]}");
    Ok(out)
}

fn write_failure_diagnostic(out: &mut Vec<u8>, failure: VisionFailure) {
    out.extend_from_slice(b"{\"code\":");
    write_json_string(out, failure.code().as_str().as_bytes());
    out.extend_from_slice(b",\"message\":");
    match failure {
        VisionFailure::ImageUnavailable => {
            write_json_string(out, b"Vision could not safely load or verify this image.");
        }
        VisionFailure::ProviderResponseInvalid => {
            write_json_string(
                out,
                b"Vision received an invalid provider response after one retry.",
            );
        }
        VisionFailure::OutputLimitExceeded(limit) => {
            let message = format!(
                "Vision produced {} bytes; the configured budget is {} bytes.",
                limit.observed_bytes, limit.configured_bytes
            );
            write_json_string(out, message.as_bytes());
        }
        VisionFailure::VisionUnavailable => {
            write_json_string(out, b"Vision could not obtain a usable provider response.");
        }
        VisionFailure::MissingProviderRecord => {
            write_json_string(out, b"Vision returned no record for this image.");
        }
    }
    out.extend_from_slice(b",\"retryable\":");
    out.extend_from_slice(if failure.retryable() {
        b"true"
    } else {
        b"false"
    });
    out.extend_from_slice(b",\"suggestion\":");
    write_json_string(out, failure.suggestion().as_bytes());
    out.push(b'}');
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProjectImageReferencesError {
    InvalidImageId,
    InvalidImageCatalog,
}

/// Projects only stable attachment IDs.
pub fn project_image_references(
    catalog: &[ImageAttachment],
) -> Result<Vec<u8>, ProjectImageReferencesError> {
    if !image_attachments_sorted_by_id(catalog) {
        return Err(ProjectImageReferencesError::InvalidImageCatalog);
    }

    let mut out = b"<available_images>".to_vec();
    for attachment in catalog {
        if attachment.id == 0 {
            return Err(ProjectImageReferencesError::InvalidImageId);
        }
        out.push(b'\n');
        write_image_placeholder(&mut out, attachment.id);
    }
    if !catalog.is_empty() {
        out.push(b'\n');
    }
    out.extend_from_slice(b"</available_images>");
    Ok(out)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProjectTextOnlyError {
    MissingUserMessage,
    InvalidImageId,
    InvalidImageCatalog,
}

/// Projects an eligible text-only request without carrying filesystem-backed
/// image parts. Every authorized stable ID is attached to the current root user
/// message as a compact reference list.
pub fn project_text_only_messages(
    messages: &[ChatMessage],
    current_user_message_index: usize,
    catalog: &[ImageAttachment],
) -> Result<Vec<ChatMessage>, ProjectTextOnlyError> {
    let mut projected: Vec<ChatMessage> = messages.to_vec();
    for chat_message in &mut projected {
        chat_message.images.clear();
    }
    if catalog.is_empty() {
        return Ok(projected);
    }

    if current_user_message_index >= projected.len()
        || projected[current_user_message_index].role != ChatRole::User
        || projected[current_user_message_index].permission_feedback
    {
        return Err(ProjectTextOnlyError::MissingUserMessage);
    }
    let references = project_image_references(catalog).map_err(|err| match err {
        ProjectImageReferencesError::InvalidImageId => ProjectTextOnlyError::InvalidImageId,
        ProjectImageReferencesError::InvalidImageCatalog => {
            ProjectTextOnlyError::InvalidImageCatalog
        }
    })?;
    let content = projected[current_user_message_index]
        .content
        .as_deref()
        .unwrap_or(b"");
    projected[current_user_message_index].content = Some(if content.is_empty() {
        references
    } else {
        let mut combined = Vec::with_capacity(content.len() + 2 + references.len());
        combined.extend_from_slice(content);
        combined.extend_from_slice(b"\n\n");
        combined.extend_from_slice(&references);
        combined
    });
    Ok(projected)
}

/// Produces native-route messages with raw images. Retains only root-turn
/// Vision calls followed by contiguous structured route rejections.
/// Canonical history is unchanged.
pub fn project_native_messages(
    messages: &[ChatMessage],
    current_user_message_index: usize,
) -> Vec<ChatMessage> {
    debug_assert!(current_user_message_index < messages.len());
    debug_assert!(messages[current_user_message_index].role == ChatRole::User);

    let mut contains_vision = false;
    for chat_message in messages {
        if tool_name_bytes(chat_message) == b"vision" {
            contains_vision = true;
            break;
        }
        if chat_message
            .tool_calls
            .iter()
            .any(|call| call.name == b"vision")
        {
            contains_vision = true;
            break;
        }
    }
    if !contains_vision {
        return messages.to_vec();
    }

    let mut projected = Vec::new();
    for (message_index, chat_message) in messages.iter().enumerate() {
        if chat_message.role == ChatRole::Tool
            && tool_name_bytes(chat_message) == b"vision"
            && !(message_index >= current_user_message_index
                && is_paired_native_route_rejection_result(messages, message_index))
        {
            continue;
        }

        let mut projected_message = chat_message.clone();
        let mut vision_call_count = 0usize;
        for (call_index, call) in chat_message.tool_calls.iter().enumerate() {
            if call.name == b"vision"
                && !(message_index >= current_user_message_index
                    && native_route_rejection_result_index(messages, message_index, call_index)
                        .is_some())
            {
                vision_call_count += 1;
            }
        }
        if vision_call_count > 0 {
            let retained_count = chat_message.tool_calls.len() - vision_call_count;
            if retained_count == 0 {
                projected_message.tool_calls.clear();
                if projected_message
                    .content
                    .as_ref()
                    .map_or(true, |content| content.is_empty())
                {
                    continue;
                }
            } else {
                let mut retained = Vec::with_capacity(retained_count);
                for (call_index, call) in chat_message.tool_calls.iter().enumerate() {
                    if call.name == b"vision"
                        && !(message_index >= current_user_message_index
                            && native_route_rejection_result_index(
                                messages,
                                message_index,
                                call_index,
                            )
                            .is_some())
                    {
                        continue;
                    }
                    retained.push(call.clone());
                }
                projected_message.tool_calls = retained;
            }
        }
        projected.push(projected_message);
    }
    projected
}

fn is_native_route_rejection(message: &ChatMessage) -> bool {
    let content = content_bytes(message);
    is_tool_execution_failed_output(content)
        && contains_bytes(content, native_route_unavailable_message.as_bytes())
}

fn native_route_rejection_result_index(
    messages: &[ChatMessage],
    assistant_message_index: usize,
    call_index: usize,
) -> Option<usize> {
    if assistant_message_index >= messages.len() {
        return None;
    }
    let assistant_message = &messages[assistant_message_index];
    if assistant_message.role != ChatRole::Assistant
        || call_index >= assistant_message.tool_calls.len()
    {
        return None;
    }
    let call = &assistant_message.tool_calls[call_index];
    if call.name != b"vision" {
        return None;
    }

    let mut result_index = assistant_message_index + 1;
    while result_index < messages.len() && messages[result_index].role == ChatRole::Tool {
        let result = &messages[result_index];
        if tool_name_bytes(result) == b"vision"
            && tool_call_id_bytes(result) == call.id.as_slice()
            && is_native_route_rejection(result)
        {
            return Some(result_index);
        }
        result_index += 1;
    }
    None
}

fn is_paired_native_route_rejection_result(messages: &[ChatMessage], result_index: usize) -> bool {
    if result_index >= messages.len() || messages[result_index].role != ChatRole::Tool {
        return false;
    }

    let mut block_start = result_index;
    while block_start > 0 && messages[block_start - 1].role == ChatRole::Tool {
        block_start -= 1;
    }
    if block_start == 0 {
        return false;
    }
    let assistant_message_index = block_start - 1;
    let assistant_message = &messages[assistant_message_index];
    if assistant_message.role != ChatRole::Assistant {
        return false;
    }

    for call_index in 0..assistant_message.tool_calls.len() {
        if native_route_rejection_result_index(messages, assistant_message_index, call_index)
            == Some(result_index)
        {
            return true;
        }
    }
    false
}

pub struct VisionBatchIterator<'a> {
    image_ids: &'a [usize],
    offset: usize,
}

impl<'a> VisionBatchIterator<'a> {
    pub fn init(image_ids: &'a [usize]) -> Self {
        Self {
            image_ids,
            offset: 0,
        }
    }

    pub fn next(&mut self) -> Option<&'a [usize]> {
        if self.offset >= self.image_ids.len() {
            return None;
        }
        let remaining = self.image_ids.len() - self.offset;
        let batch_len = remaining.min(max_provider_images_per_batch);
        let batch = &self.image_ids[self.offset..self.offset + batch_len];
        self.offset += batch_len;
        Some(batch)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VisionRoute {
    NativeImages,
    TextOnly,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VisionGate {
    VisionRequired,
    Unrestricted,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VisionAttempt<'a> {
    Start,
    Valid(&'a [usize]),
    Malformed,
    Unauthorized,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PendingTransitionDisposition {
    Unchanged,
    Settled,
    RejectedMalformed,
    RejectedUnauthorized,
    Cancelled,
    NotApplicable,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingImageTransition {
    pub pending_ids: Vec<usize>,
    pub gate: VisionGate,
    pub disposition: PendingTransitionDisposition,
}

pub fn transition_pending_images(
    route: VisionRoute,
    pending_ids: &[usize],
    attempt: VisionAttempt<'_>,
) -> Result<PendingImageTransition, VisionRequestError> {
    if !pending_ids.is_empty() {
        validate_image_ids(pending_ids)?;
    }
    if route == VisionRoute::NativeImages {
        return Ok(PendingImageTransition {
            pending_ids: pending_ids.to_vec(),
            gate: VisionGate::Unrestricted,
            disposition: PendingTransitionDisposition::NotApplicable,
        });
    }

    Ok(match attempt {
        VisionAttempt::Start => {
            unchanged_pending_transition(pending_ids, PendingTransitionDisposition::Unchanged)
        }
        VisionAttempt::Malformed => unchanged_pending_transition(
            pending_ids,
            PendingTransitionDisposition::RejectedMalformed,
        ),
        VisionAttempt::Unauthorized => unchanged_pending_transition(
            pending_ids,
            PendingTransitionDisposition::RejectedUnauthorized,
        ),
        VisionAttempt::Cancelled => {
            unchanged_pending_transition(pending_ids, PendingTransitionDisposition::Cancelled)
        }
        VisionAttempt::Valid(settled_ids) => {
            validate_image_ids(settled_ids)?;
            let remaining: Vec<usize> = pending_ids
                .iter()
                .copied()
                .filter(|pending_id| !contains_image_id(settled_ids, *pending_id))
                .collect();
            if remaining.is_empty() {
                PendingImageTransition {
                    pending_ids: Vec::new(),
                    gate: VisionGate::Unrestricted,
                    disposition: PendingTransitionDisposition::Settled,
                }
            } else {
                PendingImageTransition {
                    pending_ids: remaining,
                    gate: VisionGate::VisionRequired,
                    disposition: PendingTransitionDisposition::Settled,
                }
            }
        }
    })
}

fn exact_object<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a JsonMap> {
    let object = value.as_object()?;
    if object.len() != keys.len() {
        return None;
    }
    for (key, _) in object.iter() {
        if !keys.iter().any(|known| *known == key) {
            return None;
        }
    }
    Some(object)
}

fn parse_image_id(value: &Value) -> Result<usize, VisionResultError> {
    let integer = json_integer(value).ok_or(VisionResultError::InvalidImageId)?;
    if integer <= 0 {
        return Err(VisionResultError::InvalidImageId);
    }
    usize::try_from(integer).map_err(|_| VisionResultError::InvalidImageId)
}

fn parse_image_result(value: &Value) -> Result<VisionImageResult, VisionResultError> {
    let object = match value {
        Value::Object(object) => object,
        _ => return Err(VisionResultError::InvalidVisionResult),
    };
    let status_value = object
        .get("status")
        .ok_or(VisionResultError::InvalidVisionResult)?;
    let status = match status_value {
        Value::String(status) => status.as_str(),
        _ => return Err(VisionResultError::InvalidVisionResult),
    };

    if status == "ok" {
        let object = exact_object(
            value,
            &["image_id", "status", "summary", "visible_text", "details"],
        )
        .ok_or(VisionResultError::InvalidVisionResult)?;
        return Ok(VisionImageResult {
            image_id: parse_image_id(
                object
                    .get("image_id")
                    .ok_or(VisionResultError::InvalidVisionResult)?,
            )?,
            outcome: ImageOutcome::Ok(parse_evidence(object)?),
        });
    }
    if status == "failed" {
        let object = exact_object(value, &["image_id", "status", "error"])
            .ok_or(VisionResultError::InvalidVisionResult)?;
        let failure_value = object
            .get("error")
            .ok_or(VisionResultError::InvalidVisionResult)?;
        let failure = match failure_value {
            Value::String(failure) => failure.as_str(),
            _ => return Err(VisionResultError::InvalidVisionResult),
        };
        if failure != "vision_unavailable" {
            return Err(VisionResultError::InvalidVisionResult);
        }
        return Ok(VisionImageResult {
            image_id: parse_image_id(
                object
                    .get("image_id")
                    .ok_or(VisionResultError::InvalidVisionResult)?,
            )?,
            outcome: ImageOutcome::Failed(VisionFailure::VisionUnavailable),
        });
    }
    Err(VisionResultError::InvalidVisionResult)
}

fn parse_evidence(object: &JsonMap) -> Result<ImageEvidence, VisionResultError> {
    let summary_value = object
        .get("summary")
        .ok_or(VisionResultError::InvalidVisionResult)?;
    let summary = dupe_evidence_string(summary_value, true)?;
    let visible_text = parse_evidence_list(
        object
            .get("visible_text")
            .ok_or(VisionResultError::InvalidVisionResult)?,
    )?;
    Ok(ImageEvidence {
        summary,
        visible_text,
        details: parse_evidence_list(
            object
                .get("details")
                .ok_or(VisionResultError::InvalidVisionResult)?,
        )?,
    })
}

fn parse_evidence_list(value: &Value) -> Result<Vec<Vec<u8>>, VisionResultError> {
    let items = match value {
        Value::Array(items) => items,
        _ => return Err(VisionResultError::InvalidVisionResult),
    };
    if items.is_empty() {
        return Ok(Vec::new());
    }
    let mut copied = Vec::with_capacity(items.len());
    for item in items {
        copied.push(dupe_evidence_string(item, false)?);
    }
    Ok(copied)
}

fn dupe_evidence_string(
    value: &Value,
    require_nonempty: bool,
) -> Result<Vec<u8>, VisionResultError> {
    let bytes = match value {
        Value::String(value) => value.as_bytes(),
        _ => return Err(VisionResultError::InvalidVisionResult),
    };
    if std::str::from_utf8(bytes).is_err() {
        return Err(VisionResultError::InvalidVisionResult);
    }
    if require_nonempty && trim_ascii_ws(bytes).is_empty() {
        return Err(VisionResultError::InvalidVisionResult);
    }
    Ok(bytes.to_vec())
}

fn validate_vision_result(result: &VisionResult) -> Result<(), VisionResultError> {
    for (index, image) in result.images.iter().enumerate() {
        validate_result_record(image)?;
        if result.images[..index]
            .iter()
            .any(|previous| previous.image_id == image.image_id)
        {
            return Err(VisionResultError::DuplicateImageId);
        }
    }
    Ok(())
}

fn validate_result_record(record: &VisionImageResult) -> Result<(), VisionResultError> {
    if record.image_id == 0 {
        return Err(VisionResultError::InvalidImageId);
    }
    match &record.outcome {
        ImageOutcome::Ok(evidence) => {
            validate_evidence_string(&evidence.summary, true)?;
            validate_evidence_strings(&evidence.visible_text)?;
            validate_evidence_strings(&evidence.details)?;
        }
        ImageOutcome::Failed(VisionFailure::OutputLimitExceeded(limit)) => {
            if limit.observed_bytes <= limit.configured_bytes {
                return Err(VisionResultError::InvalidVisionResult);
            }
        }
        ImageOutcome::Failed(_) => {}
    }
    Ok(())
}

fn validate_evidence_strings(items: &[Vec<u8>]) -> Result<(), VisionResultError> {
    for item in items {
        validate_evidence_string(item, false)?;
    }
    Ok(())
}

fn validate_evidence_string(value: &[u8], require_nonempty: bool) -> Result<(), VisionResultError> {
    if std::str::from_utf8(value).is_err() {
        return Err(VisionResultError::InvalidVisionResult);
    }
    if require_nonempty && trim_ascii_ws(value).is_empty() {
        return Err(VisionResultError::InvalidVisionResult);
    }
    Ok(())
}

fn find_result_record(
    records: &[VisionImageResult],
    image_id: usize,
) -> Option<&VisionImageResult> {
    records.iter().find(|record| record.image_id == image_id)
}

fn contains_image_id(image_ids: &[usize], image_id: usize) -> bool {
    image_ids.iter().any(|candidate| *candidate == image_id)
}

fn unchanged_pending_transition(
    pending_ids: &[usize],
    disposition: PendingTransitionDisposition,
) -> PendingImageTransition {
    PendingImageTransition {
        pending_ids: pending_ids.to_vec(),
        gate: if pending_ids.is_empty() {
            VisionGate::Unrestricted
        } else {
            VisionGate::VisionRequired
        },
        disposition,
    }
}

fn validate_image_ids(image_ids: &[usize]) -> Result<(), VisionRequestError> {
    if image_ids.is_empty() {
        return Err(VisionRequestError::EmptyImageIds);
    }
    for (index, &image_id) in image_ids.iter().enumerate() {
        if image_id == 0 {
            return Err(VisionRequestError::InvalidImageId);
        }
        if image_ids[..index].contains(&image_id) {
            return Err(VisionRequestError::DuplicateImageId);
        }
    }
    Ok(())
}

fn image_attachments_sorted_by_id(images: &[ImageAttachment]) -> bool {
    let mut previous: Option<usize> = None;
    for image in images {
        if let Some(id) = previous {
            if image.id <= id {
                return false;
            }
        }
        previous = Some(image.id);
    }
    true
}

fn write_image_placeholder(out: &mut Vec<u8>, id: usize) {
    out.extend_from_slice(IMAGE_PLACEHOLDER_PREFIX);
    out.extend_from_slice(id.to_string().as_bytes());
    out.push(b']');
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

fn write_json_string(out: &mut Vec<u8>, bytes: &[u8]) {
    out.push(b'"');
    for &byte in bytes {
        match byte {
            b'"' => out.extend_from_slice(b"\\\""),
            b'\\' => out.extend_from_slice(b"\\\\"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            b'\t' => out.extend_from_slice(b"\\t"),
            0x08 => out.extend_from_slice(b"\\b"),
            0x0c => out.extend_from_slice(b"\\f"),
            0x00..=0x1f => {
                const HEX: &[u8] = b"0123456789abcdef";
                out.extend_from_slice(b"\\u00");
                out.push(HEX[(byte >> 4) as usize]);
                out.push(HEX[(byte & 0x0f) as usize]);
            }
            _ => out.push(byte),
        }
    }
    out.push(b'"');
}

fn write_json_string_array(out: &mut Vec<u8>, items: &[Vec<u8>]) {
    out.push(b'[');
    for (index, item) in items.iter().enumerate() {
        if index > 0 {
            out.push(b',');
        }
        write_json_string(out, item);
    }
    out.push(b']');
}

fn trim_ascii_ws(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|b| !matches!(b, b' ' | b'\t' | b'\r' | b'\n'))
        .unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|b| !matches!(b, b' ' | b'\t' | b'\r' | b'\n'))
        .map_or(0, |i| i + 1);
    if start >= end {
        &[]
    } else {
        &bytes[start..end]
    }
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn count_bytes(haystack: &[u8], needle: &[u8]) -> usize {
    let mut count = 0;
    let mut index = 0;
    while index + needle.len() <= haystack.len() {
        if &haystack[index..index + needle.len()] == needle {
            count += 1;
            index += needle.len();
        } else {
            index += 1;
        }
    }
    count
}

fn content_bytes(message: &ChatMessage) -> &[u8] {
    message.content.as_deref().unwrap_or(b"")
}

fn tool_name_bytes(message: &ChatMessage) -> &[u8] {
    message.tool_name.as_deref().unwrap_or(b"")
}

fn tool_call_id_bytes(message: &ChatMessage) -> &[u8] {
    message.tool_call_id.as_deref().unwrap_or(b"")
}

#[cfg(test)]
mod tests {
    use super::{
        contains_bytes, count_bytes, json_integer, max_focus_bytes, max_provider_images_per_batch,
        tool_name_bytes, *,
    };
    use crate::core::mcp::json_value;
    use crate::core::tooling::tool_result_errors::{tool_execution_failure_json, ExecutionFailure};

    fn test_ok_result(image_id: usize) -> VisionImageResult {
        VisionImageResult {
            image_id,
            outcome: ImageOutcome::Ok(ImageEvidence {
                summary: b"verified image evidence".to_vec(),
                visible_text: Vec::new(),
                details: Vec::new(),
            }),
        }
    }

    fn attachment(id: usize, path: &[u8], media_type: &[u8]) -> ImageAttachment {
        ImageAttachment {
            id,
            path: path.to_vec(),
            media_type: media_type.to_vec(),
            snapshot_path: None,
            snapshot_sha256: None,
        }
    }

    fn chat_message(
        role: ChatRole,
        content: Option<&[u8]>,
        images: Vec<ImageAttachment>,
        tool_call_id: Option<&[u8]>,
        tool_name: Option<&[u8]>,
        tool_calls: Vec<ToolCall>,
        permission_feedback: bool,
    ) -> ChatMessage {
        ChatMessage {
            role,
            content: content.map(|value| value.to_vec()),
            images,
            tool_call_id: tool_call_id.map(|value| value.to_vec()),
            tool_name: tool_name.map(|value| value.to_vec()),
            tool_calls,
            provider_state_json: None,
            tool_result_status: None,
            tool_result_memory: None,
            permission_feedback,
            cache_policy: ChatCachePolicy::Default,
        }
    }

    fn utf8(bytes: &[u8]) -> &str {
        std::str::from_utf8(bytes).expect("utf8")
    }

    fn native_rejection() -> Vec<u8> {
        tool_execution_failure_json(&ExecutionFailure {
            tool_name: b"vision",
            message: native_route_unavailable_message.as_bytes(),
            details: &[],
            suggestion: Some(b"Continue using the model's native image input without Vision."),
        })
    }

    #[test]
    fn vision_request_parsing_preserves_ordered_ids_and_bounded_focus() {
        let request =
            parse_vision_request(br#"{"image_ids":[9,2,7],"focus":"Read the highlighted error"}"#)
                .unwrap();
        assert_eq!(request.image_ids().unwrap(), &[9, 2, 7]);
        assert_eq!(utf8(&request.focus), "Read the highlighted error");
    }

    #[test]
    fn vision_request_parsing_preserves_ordered_image_paths() {
        let request = parse_vision_request(
            br#"{"paths":["~/Desktop/test.png","screenshots/result.webp"],"focus":"Read the error"}"#,
        )
        .unwrap();
        let paths = request.paths().unwrap();
        assert_eq!(paths.len(), 2);
        assert_eq!(utf8(&paths[0]), "~/Desktop/test.png");
        assert_eq!(utf8(&paths[1]), "screenshots/result.webp");
        assert_eq!(utf8(&request.focus), "Read the error");
    }

    #[test]
    fn vision_request_parsing_rejects_duplicate_malformed_and_nonexact_arguments() {
        assert_eq!(
            parse_vision_request(br#"{"image_ids":[1,1],"focus":"compare"}"#),
            Err(VisionRequestError::DuplicateImageId)
        );
        assert_eq!(
            parse_vision_request(br#"{"image_ids":[0],"focus":"compare"}"#),
            Err(VisionRequestError::InvalidImageId)
        );
        assert_eq!(
            parse_vision_request(
                br#"{"image_ids":[1],"focus":"compare","path":"/tmp/private.png"}"#
            ),
            Err(VisionRequestError::InvalidVisionRequest)
        );
        assert_eq!(
            parse_vision_request(br#"{"image_ids":[1],"#),
            Err(VisionRequestError::InvalidVisionJson)
        );
    }

    #[test]
    fn vision_request_parsing_enforces_nonempty_ids_and_focus_bounds() {
        assert_eq!(
            parse_vision_request(br#"{"image_ids":[],"focus":"compare"}"#),
            Err(VisionRequestError::EmptyImageIds)
        );
        assert_eq!(
            parse_vision_request(br#"{"image_ids":[1],"focus":"  \n\t"}"#),
            Err(VisionRequestError::EmptyFocus)
        );
        let focus = "x".repeat(max_focus_bytes + 1);
        let json = format!(r#"{{"image_ids":[1],"focus":"{focus}"}}"#);
        assert_eq!(
            parse_vision_request(json.as_bytes()),
            Err(VisionRequestError::FocusTooLong)
        );
    }

    #[test]
    fn vision_request_parsing_requires_exactly_one_nonempty_source() {
        assert_eq!(
            parse_vision_request(br#"{"paths":[],"focus":"inspect"}"#),
            Err(VisionRequestError::EmptyPaths)
        );
        assert_eq!(
            parse_vision_request(br#"{"paths":["  "],"focus":"inspect"}"#),
            Err(VisionRequestError::InvalidImagePath)
        );
        assert_eq!(
            parse_vision_request(br#"{"paths":["a.png","a.png"],"focus":"inspect"}"#),
            Err(VisionRequestError::DuplicateImagePath)
        );
        assert_eq!(
            parse_vision_request(br#"{"image_ids":[1],"paths":["a.png"],"focus":"inspect"}"#),
            Err(VisionRequestError::InvalidVisionRequest)
        );
    }

    #[test]
    fn vision_provider_parsing_is_strict_and_validates_every_evidence_field() {
        let provider_json = br#"{"images":[{"image_id":3,"status":"ok","summary":"terminal screenshot","visible_text":["error: failed"],"details":["red underline"]},{"image_id":4,"status":"failed","error":"vision_unavailable"}]}"#;
        let parsed = parse_vision_provider_result(provider_json, provider_json.len()).unwrap();
        assert_eq!(parsed.images.len(), 2);
        assert_eq!(parsed.images[0].image_id, 3);
        match &parsed.images[0].outcome {
            ImageOutcome::Ok(evidence) => {
                assert_eq!(utf8(&evidence.summary), "terminal screenshot");
            }
            ImageOutcome::Failed(_) => panic!("expected ok outcome"),
        }
        match parsed.images[1].outcome {
            ImageOutcome::Failed(failure) => {
                assert_eq!(failure.code(), FailureCode::VisionUnavailable);
            }
            ImageOutcome::Ok(_) => panic!("expected failed outcome"),
        }

        let path_json = br#"{"images":[{"image_id":3,"status":"ok","summary":"ok","visible_text":[],"details":[],"path":"/tmp/private.png"}]}"#;
        assert_eq!(
            parse_vision_provider_result(path_json, path_json.len()),
            Err(VisionResultError::InvalidVisionResult)
        );
    }

    #[test]
    fn vision_provider_parsing_accepts_forty_evidence_items_within_the_total_byte_limit() {
        let mut json = String::from(
            r#"{"images":[{"image_id":3,"status":"ok","summary":"dense screenshot","visible_text":["#,
        );
        for index in 0..40 {
            if index > 0 {
                json.push(',');
            }
            json.push_str(&format!(r#""item-{index}""#));
        }
        json.push_str(r#"],"details":[]}]}"#);

        let parsed = parse_vision_provider_result(json.as_bytes(), json.len()).unwrap();
        match &parsed.images[0].outcome {
            ImageOutcome::Ok(evidence) => {
                assert_eq!(evidence.visible_text.len(), 40);
                assert_eq!(utf8(&evidence.visible_text[39]), "item-39");
            }
            ImageOutcome::Failed(_) => panic!("expected ok outcome"),
        }
    }

    #[test]
    fn vision_provider_parsing_accepts_one_long_evidence_string_within_the_total_byte_limit() {
        let long = "x".repeat(5000);
        let provider_json = format!(
            r#"{{"images":[{{"image_id":3,"status":"ok","summary":"long text","visible_text":["{long}"],"details":[]}}]}}"#
        );
        let parsed =
            parse_vision_provider_result(provider_json.as_bytes(), provider_json.len()).unwrap();
        match &parsed.images[0].outcome {
            ImageOutcome::Ok(evidence) => {
                assert_eq!(evidence.visible_text[0].len(), 5000);
            }
            ImageOutcome::Failed(_) => panic!("expected ok outcome"),
        }
    }

    #[test]
    fn vision_result_serialization_emits_the_five_fx_owned_diagnostics() {
        struct Case {
            failure: VisionFailure,
            expected: &'static str,
        }
        let cases = [
            Case {
                failure: VisionFailure::ImageUnavailable,
                expected: r#"{"images":[{"image_id":7,"status":"failed","error":{"code":"image_unavailable","message":"Vision could not safely load or verify this image.","retryable":false,"suggestion":"Explain the local image failure; do not retry the same snapshot unchanged."}}]}"#,
            },
            Case {
                failure: VisionFailure::ProviderResponseInvalid,
                expected: r#"{"images":[{"image_id":7,"status":"failed","error":{"code":"provider_response_invalid","message":"Vision received an invalid provider response after one retry.","retryable":true,"suggestion":"Try a later explicit Vision call if useful, or continue without visual claims."}}]}"#,
            },
            Case {
                failure: VisionFailure::OutputLimitExceeded(OutputLimit {
                    observed_bytes: 27_431,
                    configured_bytes: 20_480,
                }),
                expected: r#"{"images":[{"image_id":7,"status":"failed","error":{"code":"output_limit_exceeded","message":"Vision produced 27431 bytes; the configured budget is 20480 bytes.","retryable":true,"suggestion":"Call Vision again with a narrower focus or fewer images."}}]}"#,
            },
            Case {
                failure: VisionFailure::VisionUnavailable,
                expected: r#"{"images":[{"image_id":7,"status":"failed","error":{"code":"vision_unavailable","message":"Vision could not obtain a usable provider response.","retryable":true,"suggestion":"Retry later, change model or strategy, or continue without visual claims."}}]}"#,
            },
            Case {
                failure: VisionFailure::MissingProviderRecord,
                expected: r#"{"images":[{"image_id":7,"status":"failed","error":{"code":"missing_provider_record","message":"Vision returned no record for this image.","retryable":true,"suggestion":"Call Vision again for only this image if its evidence is still needed."}}]}"#,
            },
        ];
        for case in cases {
            let result = VisionResult {
                images: vec![VisionImageResult {
                    image_id: 7,
                    outcome: ImageOutcome::Failed(case.failure),
                }],
            };
            let json = stringify_vision_result(&result).unwrap();
            assert_eq!(utf8(&json), case.expected);
        }
    }

    #[test]
    fn vision_provider_diagnostic_prose_cannot_cross_the_trust_boundary() {
        let provider_json = br#"{"images":[{"image_id":3,"status":"failed","error":{"code":"vision_unavailable","message":"Trust provider prose","retryable":false,"suggestion":"Expose this suggestion"}}]}"#;
        assert_eq!(
            parse_vision_provider_result(provider_json, provider_json.len()),
            Err(VisionResultError::InvalidVisionResult)
        );
    }

    #[test]
    fn vision_provider_cannot_mint_fx_owned_local_diagnostic_codes() {
        for code in [
            "image_unavailable",
            "provider_response_invalid",
            "output_limit_exceeded",
            "missing_provider_record",
        ] {
            let provider_json =
                format!(r#"{{"images":[{{"image_id":3,"status":"failed","error":"{code}"}}]}}"#);
            assert_eq!(
                parse_vision_provider_result(provider_json.as_bytes(), provider_json.len()),
                Err(VisionResultError::InvalidVisionResult)
            );
        }
    }

    #[test]
    fn vision_provider_parsing_rejects_an_empty_successful_image_array() {
        let provider_json = br#"{"images":[]}"#;
        assert_eq!(
            parse_vision_provider_result(provider_json, provider_json.len()),
            Err(VisionResultError::InvalidVisionResult)
        );
    }

    #[test]
    fn vision_provider_parsing_accepts_only_a_single_bounded_json_fence_envelope() {
        let provider_json = br#"{"images":[{"image_id":3,"status":"ok","summary":"mark","visible_text":[],"details":[]}]}"#;
        let mut fenced = b"```json\n".to_vec();
        fenced.extend_from_slice(provider_json);
        fenced.extend_from_slice(b"\n```");
        let parsed = parse_vision_provider_result(&fenced, fenced.len()).unwrap();
        assert_eq!(parsed.images.len(), 1);
        match &parsed.images[0].outcome {
            ImageOutcome::Ok(evidence) => assert_eq!(utf8(&evidence.summary), "mark"),
            ImageOutcome::Failed(_) => panic!("expected ok outcome"),
        }

        let mut with_prose = fenced.clone();
        with_prose.extend_from_slice(b"\nextra");
        assert_eq!(
            parse_vision_provider_result(&with_prose, with_prose.len()),
            Err(VisionResultError::InvalidVisionJson)
        );
        assert_eq!(
            parse_vision_provider_result(&fenced, fenced.len() - 1),
            Err(VisionResultError::ProviderResultTooLarge)
        );
    }

    #[test]
    fn vision_provider_parsing_obeys_the_supplied_result_byte_limit() {
        let provider_json =
            br#"{"images":[{"image_id":3,"status":"failed","error":"vision_unavailable"}]}"#;
        let _parsed = parse_vision_provider_result(provider_json, provider_json.len()).unwrap();
        assert_eq!(
            parse_vision_provider_result(provider_json, provider_json.len() - 1),
            Err(VisionResultError::ProviderResultTooLarge)
        );
    }

    #[test]
    fn vision_provider_parsing_has_no_hidden_twenty_kibibyte_ceiling() {
        let evidence_chunk = "x".repeat(4096);
        let large_provider_json = format!(
            r#"{{"images":[{{"image_id":3,"status":"ok","summary":"ok","visible_text":["{0}","{0}","{0}","{0}","{0}","{0}"],"details":[]}}]}}"#,
            evidence_chunk
        );
        assert!(large_provider_json.len() > 20 * 1024);
        let parsed =
            parse_vision_provider_result(large_provider_json.as_bytes(), large_provider_json.len())
                .unwrap();
        assert_eq!(parsed.images.len(), 1);
    }

    #[test]
    fn vision_omission_preserves_sibling_evidence_and_synthesizes_an_ordered_diagnostic() {
        let provider_records = [test_ok_result(3), test_ok_result(1)];
        let merged = merge_vision_results(&[1, 2, 3], &provider_records).unwrap();
        assert_eq!(merged.images.len(), 3);
        assert_eq!(merged.images[0].image_id, 1);
        match &merged.images[0].outcome {
            ImageOutcome::Ok(evidence) => {
                assert_eq!(utf8(&evidence.summary), "verified image evidence");
            }
            ImageOutcome::Failed(_) => panic!("expected ok outcome"),
        }
        assert_eq!(merged.images[1].image_id, 2);
        match merged.images[1].outcome {
            ImageOutcome::Failed(failure) => {
                assert_eq!(failure.code(), FailureCode::MissingProviderRecord);
            }
            ImageOutcome::Ok(_) => panic!("expected failed outcome"),
        }
        assert_eq!(merged.images[2].image_id, 3);
    }

    #[test]
    fn vision_merge_preserves_nineteen_successes_and_one_failure() {
        let mut requested = [0usize; 20];
        let mut provider_records = Vec::with_capacity(20);
        for index in 0..20 {
            requested[index] = index + 1;
            provider_records.push(if index == 12 {
                VisionImageResult {
                    image_id: index + 1,
                    outcome: ImageOutcome::Failed(VisionFailure::VisionUnavailable),
                }
            } else {
                test_ok_result(index + 1)
            });
        }

        let merged = merge_vision_results(&requested, &provider_records).unwrap();
        let mut successes = 0usize;
        let mut failures = 0usize;
        for record in &merged.images {
            match record.outcome {
                ImageOutcome::Ok(_) => successes += 1,
                ImageOutcome::Failed(_) => failures += 1,
            }
        }
        assert_eq!(successes, 19);
        assert_eq!(failures, 1);
    }

    #[test]
    fn vision_batching_partitions_twenty_ids_deterministically_and_merge_keeps_cardinality() {
        let mut requested = [0usize; 20];
        let mut provider_records = Vec::with_capacity(20);
        for index in 0..20 {
            requested[index] = index + 1;
            provider_records.push(test_ok_result(index + 1));
        }

        let mut batches = VisionBatchIterator::init(&requested);
        assert_eq!(batches.next().unwrap(), &requested[0..8]);
        assert_eq!(batches.next().unwrap(), &requested[8..16]);
        assert_eq!(batches.next().unwrap(), &requested[16..20]);
        assert!(batches.next().is_none());

        let merged = merge_vision_results(&requested, &provider_records).unwrap();
        assert_eq!(merged.images.len(), requested.len());
        for (record, image_id) in merged.images.iter().zip(requested) {
            assert_eq!(record.image_id, image_id);
        }
    }

    #[test]
    fn vision_merge_rejects_duplicate_and_unauthorized_provider_ids() {
        let duplicate = [test_ok_result(1), test_ok_result(1)];
        assert_eq!(
            merge_vision_results(&[1, 2], &duplicate),
            Err(VisionResultError::DuplicateImageId)
        );

        let unauthorized = [test_ok_result(99)];
        assert_eq!(
            merge_vision_results(&[1, 2], &unauthorized),
            Err(VisionResultError::UnauthorizedImageId)
        );
    }

    #[test]
    fn vision_references_and_result_json_expose_ids_without_attachment_paths() {
        let catalog = [
            attachment(2, b"/tmp/private-two.png", b"image/png"),
            attachment(7, b"/Users/private/seven.jpg", b"image/jpeg"),
        ];
        let references = project_image_references(&catalog).unwrap();
        assert!(contains_bytes(&references, b"[Image #2]"));
        assert!(contains_bytes(&references, b"[Image #7]"));
        assert!(!contains_bytes(&references, b"/tmp/private-two.png"));
        assert!(!contains_bytes(&references, b"/Users/private/seven.jpg"));

        let result = VisionResult {
            images: vec![test_ok_result(2)],
        };
        let json = stringify_vision_result(&result).unwrap();
        assert!(contains_bytes(&json, br#""image_id":2"#));
        assert!(!contains_bytes(&json, b"path"));
    }

    #[test]
    fn text_only_message_projection_removes_raw_images_and_exposes_all_authorized_ids() {
        let catalog = [
            attachment(2, b"/tmp/private-two.png", b"image/png"),
            attachment(7, b"/Users/private/seven.jpg", b"image/jpeg"),
        ];
        let messages = [
            chat_message(
                ChatRole::System,
                Some(b"system"),
                vec![],
                None,
                None,
                vec![],
                false,
            ),
            chat_message(
                ChatRole::User,
                Some(b"inspect"),
                catalog.to_vec(),
                None,
                None,
                vec![],
                false,
            ),
        ];

        let projected = project_text_only_messages(&messages, 1, &catalog).unwrap();
        assert_eq!(projected.len(), messages.len());
        for chat_message in &projected {
            assert_eq!(chat_message.images.len(), 0);
        }
        let content = projected[1].content.as_deref().unwrap();
        assert!(contains_bytes(content, b"[Image #2]"));
        assert!(contains_bytes(content, b"[Image #7]"));
        assert!(!contains_bytes(content, b"/tmp/private-two.png"));
        assert!(!contains_bytes(content, b"/Users/private/seven.jpg"));
    }

    #[test]
    fn text_only_projection_preserves_typed_permission_feedback_byte_exact() {
        let catalog = [attachment(1, b"/tmp/private.png", b"image/png")];
        let feedback = b"Do not modify any more files.";
        let messages = [
            chat_message(
                ChatRole::User,
                Some(b"Make the requested changes."),
                catalog.to_vec(),
                None,
                None,
                vec![],
                false,
            ),
            chat_message(
                ChatRole::Assistant,
                Some(b"I need permission."),
                vec![],
                None,
                None,
                vec![],
                false,
            ),
            chat_message(
                ChatRole::Tool,
                Some(b"read result"),
                vec![],
                Some(b"read"),
                Some(b"read_file"),
                vec![],
                false,
            ),
            chat_message(
                ChatRole::User,
                Some(feedback),
                vec![],
                None,
                None,
                vec![],
                true,
            ),
        ];

        let projected = project_text_only_messages(&messages, 0, &catalog).unwrap();
        assert_eq!(projected[3].content.as_deref().unwrap(), feedback);
        assert_eq!(
            count_bytes(
                projected[0].content.as_deref().unwrap(),
                b"<available_images>"
            ),
            1
        );
        assert_eq!(
            count_bytes(
                projected[3].content.as_deref().unwrap(),
                b"<available_images>"
            ),
            0
        );
        assert_eq!(messages[3].content.as_deref().unwrap(), feedback);
        assert_eq!(
            messages[0].content.as_deref().unwrap(),
            b"Make the requested changes."
        );
    }

    #[test]
    fn native_message_projection_retains_only_the_current_turn_structured_rejection() {
        let images = [attachment(2, b"/tmp/native.png", b"image/png")];
        let historical_calls = vec![
            ToolCall::new("historical-success", "vision", "{}"),
            ToolCall::new("read-call", "read_file", "{}"),
        ];
        let rejected_call = vec![ToolCall::new("reused-call-id", "vision", "{}")];
        let reused_success_call = vec![ToolCall::new("reused-call-id", "vision", "{}")];
        let rejection = native_rejection();
        let messages = [
            chat_message(
                ChatRole::User,
                Some(b"inspect"),
                images.to_vec(),
                None,
                None,
                vec![],
                false,
            ),
            chat_message(
                ChatRole::Assistant,
                None,
                vec![],
                None,
                None,
                historical_calls,
                false,
            ),
            chat_message(
                ChatRole::Tool,
                Some(b"historical vision evidence"),
                vec![],
                Some(b"historical-success"),
                Some(b"vision"),
                vec![],
                false,
            ),
            chat_message(
                ChatRole::Tool,
                Some(b"file evidence"),
                vec![],
                Some(b"read-call"),
                Some(b"read_file"),
                vec![],
                false,
            ),
            chat_message(
                ChatRole::User,
                Some(b"recover now"),
                vec![],
                None,
                None,
                vec![],
                false,
            ),
            chat_message(
                ChatRole::Assistant,
                None,
                vec![],
                None,
                None,
                rejected_call,
                false,
            ),
            chat_message(
                ChatRole::Tool,
                Some(&rejection),
                vec![],
                Some(b"reused-call-id"),
                Some(b"vision"),
                vec![],
                false,
            ),
            chat_message(
                ChatRole::Assistant,
                Some(b"recovered"),
                vec![],
                None,
                None,
                vec![],
                false,
            ),
            chat_message(
                ChatRole::User,
                Some(b"continue later"),
                vec![],
                None,
                None,
                vec![],
                false,
            ),
            chat_message(
                ChatRole::Assistant,
                None,
                vec![],
                None,
                None,
                reused_success_call,
                false,
            ),
            chat_message(
                ChatRole::Tool,
                Some(b"current successful evidence"),
                vec![],
                Some(b"reused-call-id"),
                Some(b"vision"),
                vec![],
                false,
            ),
            chat_message(
                ChatRole::Assistant,
                Some(b"later answer"),
                vec![],
                None,
                None,
                vec![],
                false,
            ),
        ];

        let immediate = project_native_messages(&messages[..8], 4);
        assert_eq!(immediate.len(), 7);
        assert_eq!(immediate[0].images.len(), 1);
        assert_eq!(immediate[1].tool_calls.len(), 1);
        assert_eq!(immediate[1].tool_calls[0].name, b"read_file");
        assert_eq!(immediate[2].tool_name.as_deref().unwrap(), b"read_file");
        assert_eq!(immediate[4].tool_calls[0].name, b"vision");
        assert_eq!(immediate[4].tool_calls[0].id, b"reused-call-id");
        assert_eq!(immediate[5].tool_name.as_deref().unwrap(), b"vision");
        assert_eq!(
            immediate[5].tool_call_id.as_deref().unwrap(),
            b"reused-call-id"
        );
        assert_eq!(
            immediate[5].content.as_deref().unwrap(),
            rejection.as_slice()
        );

        let later = project_native_messages(&messages, 8);
        assert_eq!(later.len(), 7);
        assert_eq!(later[0].images.len(), 1);
        assert_eq!(later[1].tool_calls[0].name, b"read_file");
        assert_eq!(later[3].content.as_deref().unwrap(), b"recover now");
        assert_eq!(later[4].content.as_deref().unwrap(), b"recovered");
        assert_eq!(later[5].content.as_deref().unwrap(), b"continue later");
        assert_eq!(later[6].content.as_deref().unwrap(), b"later answer");
        for chat_message in &later {
            assert_ne!(tool_name_bytes(chat_message), b"vision");
            for call in &chat_message.tool_calls {
                assert_ne!(call.name, b"vision");
            }
        }

        let success_then_rejection_first_calls = vec![
            ToolCall::new("success-then-rejection", "vision", "{}"),
            ToolCall::new("first-read", "read_file", "{}"),
        ];
        let success_then_rejection_second_calls = vec![
            ToolCall::new("success-then-rejection", "vision", "{}"),
            ToolCall::new("second-read", "read_file", "{}"),
        ];
        let success_then_rejection_messages = [
            chat_message(
                ChatRole::User,
                Some(b"inspect two steps"),
                vec![],
                None,
                None,
                vec![],
                false,
            ),
            chat_message(
                ChatRole::Assistant,
                None,
                vec![],
                None,
                None,
                success_then_rejection_first_calls,
                false,
            ),
            chat_message(
                ChatRole::Tool,
                Some(b"successful evidence"),
                vec![],
                Some(b"success-then-rejection"),
                Some(b"vision"),
                vec![],
                false,
            ),
            chat_message(
                ChatRole::Tool,
                Some(b"first file"),
                vec![],
                Some(b"first-read"),
                Some(b"read_file"),
                vec![],
                false,
            ),
            chat_message(
                ChatRole::Assistant,
                None,
                vec![],
                None,
                None,
                success_then_rejection_second_calls,
                false,
            ),
            chat_message(
                ChatRole::Tool,
                Some(&rejection),
                vec![],
                Some(b"success-then-rejection"),
                Some(b"vision"),
                vec![],
                false,
            ),
            chat_message(
                ChatRole::Tool,
                Some(b"second file"),
                vec![],
                Some(b"second-read"),
                Some(b"read_file"),
                vec![],
                false,
            ),
            chat_message(
                ChatRole::Assistant,
                Some(b"recovered after two steps"),
                vec![],
                None,
                None,
                vec![],
                false,
            ),
        ];
        let success_then_rejection = project_native_messages(&success_then_rejection_messages, 0);
        assert_eq!(success_then_rejection.len(), 7);
        assert_eq!(success_then_rejection[1].tool_calls.len(), 1);
        assert_eq!(success_then_rejection[1].tool_calls[0].id, b"first-read");
        assert_eq!(
            success_then_rejection[2].tool_call_id.as_deref().unwrap(),
            b"first-read"
        );
        assert_eq!(success_then_rejection[3].tool_calls.len(), 2);
        assert_eq!(
            success_then_rejection[3].tool_calls[0].id,
            b"success-then-rejection"
        );
        assert_eq!(success_then_rejection[3].tool_calls[1].id, b"second-read");
        assert_eq!(
            success_then_rejection[4].tool_call_id.as_deref().unwrap(),
            b"success-then-rejection"
        );
        assert_eq!(
            success_then_rejection[4].content.as_deref().unwrap(),
            rejection.as_slice()
        );
        assert_eq!(
            success_then_rejection[5].tool_call_id.as_deref().unwrap(),
            b"second-read"
        );

        let rejection_then_success_calls =
            vec![ToolCall::new("rejection-then-success", "vision", "{}")];
        let rejection_then_success_messages = [
            chat_message(
                ChatRole::User,
                Some(b"inspect both routes"),
                vec![],
                None,
                None,
                vec![],
                false,
            ),
            chat_message(
                ChatRole::Assistant,
                None,
                vec![],
                None,
                None,
                rejection_then_success_calls.clone(),
                false,
            ),
            chat_message(
                ChatRole::Tool,
                Some(&rejection),
                vec![],
                Some(b"rejection-then-success"),
                Some(b"vision"),
                vec![],
                false,
            ),
            chat_message(
                ChatRole::Assistant,
                None,
                vec![],
                None,
                None,
                rejection_then_success_calls,
                false,
            ),
            chat_message(
                ChatRole::Tool,
                Some(b"successful evidence"),
                vec![],
                Some(b"rejection-then-success"),
                Some(b"vision"),
                vec![],
                false,
            ),
            chat_message(
                ChatRole::Assistant,
                Some(b"recovered before success"),
                vec![],
                None,
                None,
                vec![],
                false,
            ),
        ];
        let rejection_then_success = project_native_messages(&rejection_then_success_messages, 0);
        assert_eq!(rejection_then_success.len(), 4);
        assert_eq!(
            rejection_then_success[1].tool_calls[0].id,
            b"rejection-then-success"
        );
        assert_eq!(
            rejection_then_success[2].tool_call_id.as_deref().unwrap(),
            b"rejection-then-success"
        );
        assert_eq!(
            rejection_then_success[3].content.as_deref().unwrap(),
            b"recovered before success"
        );
    }

    #[test]
    fn native_message_projection_rejects_reversed_and_unmatched_vision_evidence() {
        let rejection = native_rejection();
        let before_call = vec![ToolCall::new("before-call", "vision", "{}")];
        let no_result = vec![ToolCall::new("no-result", "vision", "{}")];
        let noncontiguous = vec![ToolCall::new("noncontiguous", "vision", "{}")];
        let messages = [
            chat_message(
                ChatRole::User,
                Some(b"reject malformed evidence"),
                vec![],
                None,
                None,
                vec![],
                false,
            ),
            chat_message(
                ChatRole::Tool,
                Some(&rejection),
                vec![],
                Some(b"before-call"),
                Some(b"vision"),
                vec![],
                false,
            ),
            chat_message(
                ChatRole::Assistant,
                None,
                vec![],
                None,
                None,
                before_call,
                false,
            ),
            chat_message(
                ChatRole::Assistant,
                Some(b"before-call separator"),
                vec![],
                None,
                None,
                vec![],
                false,
            ),
            chat_message(
                ChatRole::Assistant,
                None,
                vec![],
                None,
                None,
                no_result,
                false,
            ),
            chat_message(
                ChatRole::Assistant,
                Some(b"no-result separator"),
                vec![],
                None,
                None,
                vec![],
                false,
            ),
            chat_message(
                ChatRole::Assistant,
                None,
                vec![],
                None,
                None,
                noncontiguous,
                false,
            ),
            chat_message(
                ChatRole::Assistant,
                Some(b"noncontiguous separator"),
                vec![],
                None,
                None,
                vec![],
                false,
            ),
            chat_message(
                ChatRole::Tool,
                Some(&rejection),
                vec![],
                Some(b"noncontiguous"),
                Some(b"vision"),
                vec![],
                false,
            ),
            chat_message(
                ChatRole::Tool,
                Some(&rejection),
                vec![],
                Some(b"unmatched"),
                Some(b"vision"),
                vec![],
                false,
            ),
            chat_message(
                ChatRole::Assistant,
                Some(b"finished"),
                vec![],
                None,
                None,
                vec![],
                false,
            ),
        ];

        let projected = project_native_messages(&messages, 0);
        assert_eq!(projected.len(), 5);
        assert_eq!(
            projected[0].content.as_deref().unwrap(),
            b"reject malformed evidence"
        );
        assert_eq!(
            projected[1].content.as_deref().unwrap(),
            b"before-call separator"
        );
        assert_eq!(
            projected[2].content.as_deref().unwrap(),
            b"no-result separator"
        );
        assert_eq!(
            projected[3].content.as_deref().unwrap(),
            b"noncontiguous separator"
        );
        assert_eq!(projected[4].content.as_deref().unwrap(), b"finished");
        for chat_message in &projected {
            assert_ne!(tool_name_bytes(chat_message), b"vision");
            for call in &chat_message.tool_calls {
                assert_ne!(call.name, b"vision");
            }
        }
    }

    #[test]
    fn vision_pending_transition_gates_text_routes_and_cancellation_is_neutral() {
        let initial =
            transition_pending_images(VisionRoute::TextOnly, &[1, 2, 3], VisionAttempt::Start)
                .unwrap();
        assert_eq!(initial.gate, VisionGate::VisionRequired);
        assert_eq!(initial.pending_ids, vec![1, 2, 3]);

        let partial = transition_pending_images(
            VisionRoute::TextOnly,
            &initial.pending_ids,
            VisionAttempt::Valid(&[3, 1]),
        )
        .unwrap();
        assert_eq!(partial.gate, VisionGate::VisionRequired);
        assert_eq!(partial.pending_ids, vec![2]);

        let cancelled = transition_pending_images(
            VisionRoute::TextOnly,
            &partial.pending_ids,
            VisionAttempt::Cancelled,
        )
        .unwrap();
        assert_eq!(
            cancelled.disposition,
            PendingTransitionDisposition::Cancelled
        );
        assert_eq!(cancelled.pending_ids, partial.pending_ids);

        let complete = transition_pending_images(
            VisionRoute::TextOnly,
            &partial.pending_ids,
            VisionAttempt::Valid(&[2]),
        )
        .unwrap();
        assert_eq!(complete.gate, VisionGate::Unrestricted);
        assert_eq!(complete.pending_ids.len(), 0);

        let native =
            transition_pending_images(VisionRoute::NativeImages, &[1, 2, 3], VisionAttempt::Start)
                .unwrap();
        assert_eq!(native.gate, VisionGate::Unrestricted);
        assert_eq!(
            native.disposition,
            PendingTransitionDisposition::NotApplicable
        );
        assert_eq!(native.pending_ids, vec![1, 2, 3]);
    }

    #[test]
    fn vision_malformed_and_unauthorized_attempts_settle_no_pending_ids() {
        for attempt in [VisionAttempt::Malformed, VisionAttempt::Unauthorized] {
            let transition =
                transition_pending_images(VisionRoute::TextOnly, &[4, 5], attempt).unwrap();
            assert_eq!(transition.pending_ids, vec![4, 5]);
            assert_eq!(transition.gate, VisionGate::VisionRequired);
        }
    }

    #[test]
    fn vision_provider_response_schema_matches_the_strict_parser_contract() {
        let schema = build_provider_response_schema(2).unwrap();
        let parsed = json_value::parse_json(utf8(&schema)).unwrap();
        let images = parsed
            .as_object()
            .unwrap()
            .get("properties")
            .unwrap()
            .as_object()
            .unwrap()
            .get("images")
            .unwrap()
            .as_object()
            .unwrap();
        assert_eq!(json_integer(images.get("minItems").unwrap()), Some(2));
        assert_eq!(json_integer(images.get("maxItems").unwrap()), Some(2));

        let alternatives = images
            .get("items")
            .unwrap()
            .as_object()
            .unwrap()
            .get("anyOf")
            .unwrap()
            .as_array()
            .unwrap();
        assert_eq!(alternatives.len(), 2);
        let success = alternatives[0].as_object().unwrap();
        let failure = alternatives[1].as_object().unwrap();
        assert_eq!(
            success.get("required").unwrap().as_array().unwrap().len(),
            5
        );
        assert_eq!(
            failure.get("required").unwrap().as_array().unwrap().len(),
            3
        );
        assert_eq!(
            success.get("additionalProperties").unwrap().as_bool(),
            Some(false)
        );
        assert_eq!(
            failure.get("additionalProperties").unwrap().as_bool(),
            Some(false)
        );
        let success_image_id = success
            .get("properties")
            .unwrap()
            .as_object()
            .unwrap()
            .get("image_id")
            .unwrap()
            .as_object()
            .unwrap();
        let failure_image_id = failure
            .get("properties")
            .unwrap()
            .as_object()
            .unwrap()
            .get("image_id")
            .unwrap()
            .as_object()
            .unwrap();
        assert!(success_image_id.get("enum").is_none());
        assert!(failure_image_id.get("enum").is_none());
        let failure_error = failure
            .get("properties")
            .unwrap()
            .as_object()
            .unwrap()
            .get("error")
            .unwrap()
            .as_object()
            .unwrap();
        assert_eq!(
            failure_error.get("enum").unwrap().as_array().unwrap()[0]
                .as_str()
                .unwrap(),
            "vision_unavailable"
        );

        assert_eq!(
            build_provider_response_schema(0),
            Err(InvalidVisionResponseImageCount)
        );
        assert_eq!(
            build_provider_response_schema(max_provider_images_per_batch + 1),
            Err(InvalidVisionResponseImageCount)
        );
    }
}
