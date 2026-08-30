//! Session display metadata (title, preview, origin) derivation and sidecar
//! codec.
//!
//! Mirrors `zig-reference/src/core/session/session_display_metadata.zig`.

use std::fs;

use crate::core::shared::{debug_trace, io, types};
use crate::core::mcp::json_value::{self, Value};

pub const SIDECAR_FILE: &str = "display.json";
pub const MAX_SIDECAR_BYTES: usize = 16 * 1024;
const MAX_TITLE_WORDS: usize = 8;
pub const MAX_TITLE_BYTES: usize = 240;
const MAX_PREVIEW_LINES: usize = 2;
const MAX_PREVIEW_BYTES: usize = 240;
pub const FALLBACK_TITLE: &str = "Untitled session";
pub const IMAGE_TITLE: &str = "Image session";

#[derive(Clone, Debug, PartialEq)]
pub struct DisplayMetadata {
    pub present: bool,
    pub title: Vec<u8>,
    pub preview: Option<Vec<u8>>,
    pub origin_workspace_root: Option<Vec<u8>>,
}

impl DisplayMetadata {
    pub fn deinit(&mut self) {
        self.title.clear();
        self.preview = None;
        self.origin_workspace_root = None;
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidDisplayMetadata;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DisplayMetadataTooLarge;

pub fn missing_fallback() -> DisplayMetadata {
    DisplayMetadata {
        present: false,
        title: FALLBACK_TITLE.as_bytes().to_vec(),
        preview: None,
        origin_workspace_root: None,
    }
}

fn missing_fallback_present() -> DisplayMetadata {
    DisplayMetadata {
        present: true,
        title: FALLBACK_TITLE.as_bytes().to_vec(),
        preview: None,
        origin_workspace_root: None,
    }
}

/// Derives display metadata from the first prompt-bearing history turn.
pub fn derive_from_history(history: &[types::HistoryTurn]) -> DisplayMetadata {
    let candidate = match first_prompt_candidate(history) {
        Some(candidate) => candidate,
        None => {
            return if history.is_empty() {
                missing_fallback()
            } else {
                missing_fallback_present()
            }
        }
    };
    match candidate {
        PromptCandidate::Text(prompt) => {
            if std::str::from_utf8(prompt).is_err() {
                return missing_fallback_present();
            }
            let first_line = match first_non_empty_line(prompt) {
                Some(line) => line,
                None => return missing_fallback_present(),
            };
            let title = capped_title(first_line);
            let preview = bounded_preview(prompt);
            DisplayMetadata {
                present: true,
                title,
                preview,
                origin_workspace_root: None,
            }
        }
        PromptCandidate::ImageOnly => DisplayMetadata {
            present: true,
            title: IMAGE_TITLE.as_bytes().to_vec(),
            preview: None,
            origin_workspace_root: None,
        },
    }
}

enum PromptCandidate<'a> {
    Text(&'a [u8]),
    ImageOnly,
}

fn first_prompt_candidate(history: &[types::HistoryTurn]) -> Option<PromptCandidate<'_>> {
    for turn in history {
        let user = match turn {
            types::HistoryTurn::Assistant(entry) => &entry.user,
            types::HistoryTurn::BackgroundCommand(entry) => &entry.user,
            types::HistoryTurn::Interrupted(entry) => &entry.user,
            types::HistoryTurn::CompactedSummary(_) => continue,
        };
        if let Some(candidate) = prompt_candidate_from_user(user) {
            return Some(candidate);
        }
    }
    None
}

fn prompt_candidate_from_user(user: &types::UserTurn) -> Option<PromptCandidate<'_>> {
    let trimmed = trim_ascii(&user.text, b" \t\r\n");
    if !trimmed.is_empty() && !is_slash_command_only(trimmed) {
        return Some(PromptCandidate::Text(&user.text));
    }
    if !user.images.is_empty() {
        return Some(PromptCandidate::ImageOnly);
    }
    None
}

fn is_slash_command_only(trimmed: &[u8]) -> bool {
    !trimmed.is_empty() && trimmed[0] == b'/' && !trimmed.contains(&b'\n')
}

fn first_non_empty_line(text: &[u8]) -> Option<&[u8]> {
    for line in text.split(|&b| b == b'\n') {
        let trimmed = trim_ascii(line, b" \t\r");
        if !trimmed.is_empty() {
            return Some(trimmed);
        }
    }
    None
}

fn capped_title(line: &[u8]) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();
    let mut words = 0usize;
    let mut index = 0usize;
    while index < line.len() && words < MAX_TITLE_WORDS {
        while index < line.len() && line[index].is_ascii_whitespace() {
            index += 1;
        }
        let start = index;
        while index < line.len() && !line[index].is_ascii_whitespace() {
            index += 1;
        }
        if index == start {
            break;
        }
        let separator = if words > 0 { 1 } else { 0 };
        let remaining = MAX_TITLE_BYTES - (out.len() + separator).min(MAX_TITLE_BYTES);
        let take = valid_utf8_prefix_len(&line[start..index], remaining);
        if take == 0 {
            break;
        }
        if words > 0 {
            out.push(b' ');
        }
        out.extend_from_slice(&line[start..start + take]);
        words += 1;
    }

    if out.is_empty() {
        FALLBACK_TITLE.as_bytes().to_vec()
    } else {
        out
    }
}

fn bounded_preview(text: &[u8]) -> Option<Vec<u8>> {
    let mut out: Vec<u8> = Vec::new();
    let mut line_count = 0usize;
    for line in text.split(|&b| b == b'\n') {
        if line_count == MAX_PREVIEW_LINES {
            break;
        }
        let trimmed = trim_ascii(line, b" \t\r");
        if trimmed.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push(b'\n');
        }
        let remaining = MAX_PREVIEW_BYTES - out.len().min(MAX_PREVIEW_BYTES);
        if remaining == 0 {
            break;
        }
        let take = valid_utf8_prefix_len(trimmed, remaining);
        out.extend_from_slice(&trimmed[..take]);
        line_count += 1;
        if take < trimmed.len() || out.len() >= MAX_PREVIEW_BYTES {
            break;
        }
    }

    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

fn valid_utf8_prefix_len(text: &[u8], max_bytes: usize) -> usize {
    let mut end = text.len().min(max_bytes);
    while end > 0 && std::str::from_utf8(&text[..end]).is_err() {
        end -= 1;
    }
    end
}

fn trim_ascii<'a>(mut text: &'a [u8], chars: &[u8]) -> &'a [u8] {
    while text.first().is_some_and(|b| chars.contains(b)) {
        text = &text[1..];
    }
    while text.last().is_some_and(|b| chars.contains(b)) {
        text = &text[..text.len() - 1];
    }
    text
}

/// Encodes display metadata as the `display.json` sidecar payload.
pub fn encode_sidecar(metadata: &DisplayMetadata) -> Result<Vec<u8>, DisplayMetadataTooLarge> {
    let mut out = Vec::new();
    out.extend_from_slice(b"{\"schema_version\":1,\"title\":");
    write_json_string(&mut out, &metadata.title);
    out.extend_from_slice(b",\"preview\":");
    match &metadata.preview {
        Some(preview) => write_json_string(&mut out, preview),
        None => out.extend_from_slice(b"null"),
    }
    out.extend_from_slice(b",\"origin_workspace_root\":");
    match &metadata.origin_workspace_root {
        Some(root) => write_json_string(&mut out, root),
        None => out.extend_from_slice(b"null"),
    }
    out.push(b'}');

    if out.len() > MAX_SIDECAR_BYTES {
        return Err(DisplayMetadataTooLarge);
    }
    Ok(out)
}

/// Decodes a `display.json` sidecar payload.
pub fn decode_sidecar(bytes: &[u8]) -> Result<DisplayMetadata, InvalidDisplayMetadata> {
    if bytes.is_empty() || bytes.len() > MAX_SIDECAR_BYTES {
        return Err(InvalidDisplayMetadata);
    }
    let text = std::str::from_utf8(bytes).map_err(|_| InvalidDisplayMetadata)?;
    let parsed = json_value::parse_json(text).map_err(|_| InvalidDisplayMetadata)?;
    let root = parsed.as_object().ok_or(InvalidDisplayMetadata)?;
    let schema = root.get("schema_version").ok_or(InvalidDisplayMetadata)?;
    if !is_integer_one(schema) {
        return Err(InvalidDisplayMetadata);
    }

    let title = required_string_dup(root.get("title"))?;
    if title.is_empty() {
        return Err(InvalidDisplayMetadata);
    }
    let preview = optional_string_dup(root.get("preview"))?;
    let origin = optional_string_dup(root.get("origin_workspace_root"))?;

    Ok(DisplayMetadata {
        present: true,
        title,
        preview,
        origin_workspace_root: origin,
    })
}

fn is_integer_one(value: &Value) -> bool {
    match value {
        Value::Integer(n) => *n == 1,
        Value::NumberString(text) => text == "1",
        _ => false,
    }
}

/// Decodes a sidecar, falling back to a missing-fallback metadata on any
/// malformed input.
pub fn decode_sidecar_or_fallback(bytes: &[u8]) -> DisplayMetadata {
    match decode_sidecar(bytes) {
        Ok(metadata) => metadata,
        Err(_) => {
            debug_trace::logf("session", format_args!("event=display_sidecar_invalid"));
            missing_fallback()
        }
    }
}

/// Reads and decodes a session's `display.json`, falling back when the file is
/// absent or malformed.
pub fn read_sidecar_or_fallback(session_dir: &io::VerifiedDir) -> DisplayMetadata {
    let path = session_dir.path.join(SIDECAR_FILE);
    let meta = match fs::symlink_metadata(&path) {
        Ok(meta) => meta,
        Err(_) => return missing_fallback(),
    };
    if meta.file_type().is_symlink() || !meta.is_file() {
        return missing_fallback();
    }
    let mut file = match fs::File::open(&path) {
        Ok(file) => file,
        Err(_) => return missing_fallback(),
    };
    match io::read_file_to_end(&mut file, MAX_SIDECAR_BYTES) {
        Ok(bytes) => decode_sidecar_or_fallback(&bytes),
        Err(_) => missing_fallback(),
    }
}

/// Encodes and atomically writes a session's `display.json`.
pub fn write_sidecar(
    session_dir: &io::VerifiedDir,
    metadata: &DisplayMetadata,
) -> Result<(), DisplayMetadataTooLarge> {
    let bytes = encode_sidecar(metadata)?;
    io::durable_replace_verified(session_dir, SIDECAR_FILE, &bytes)
        .map_err(|_| DisplayMetadataTooLarge)
}

fn required_string_dup(maybe_value: Option<&Value>) -> Result<Vec<u8>, InvalidDisplayMetadata> {
    let value = maybe_value.ok_or(InvalidDisplayMetadata)?;
    match value {
        Value::String(text) => Ok(text.as_bytes().to_vec()),
        _ => Err(InvalidDisplayMetadata),
    }
}

fn optional_string_dup(
    maybe_value: Option<&Value>,
) -> Result<Option<Vec<u8>>, InvalidDisplayMetadata> {
    let value = maybe_value.ok_or(InvalidDisplayMetadata)?;
    match value {
        Value::Null => Ok(None),
        Value::String(text) => Ok(Some(text.as_bytes().to_vec())),
        _ => Err(InvalidDisplayMetadata),
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
            0x00..=0x1f => out.extend_from_slice(format!("\\u{byte:04x}").as_bytes()),
            _ => out.push(byte),
        }
    }
    out.push(b'"');
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_assistant_turn(text: &str) -> types::HistoryTurn {
        make_assistant_turn_with_images(text, vec![])
    }

    fn make_assistant_turn_with_images(
        text: &str,
        images: Vec<types::ImageAttachment>,
    ) -> types::HistoryTurn {
        types::HistoryTurn::Assistant(types::AssistantHistoryTurn {
            user: types::UserTurn {
                text: text.as_bytes().to_vec(),
                images,
                work_id: None,
            },
            assistant: b"reply".to_vec(),
            execution: types::ExecutionMemory::default(),
        })
    }

    #[test]
    fn display_metadata_derives_capped_title_from_first_prompt_line_and_bounded_preview() {
        let history = [make_assistant_turn(
            "one two three four five six seven eight nine ten\nsecond line kept\nthird line dropped",
        )];

        let metadata = derive_from_history(&history);

        assert!(metadata.present);
        assert_eq!(b"one two three four five six seven eight", metadata.title.as_slice());
        assert_eq!(
            b"one two three four five six seven eight nine ten\nsecond line kept",
            metadata.preview.unwrap().as_slice()
        );
    }

    #[test]
    fn display_metadata_byte_caps_a_single_token_title() {
        let long_token = "x".repeat(512);
        let history = [make_assistant_turn(&long_token)];

        let metadata = derive_from_history(&history);

        assert!(metadata.present);
        assert_eq!(MAX_TITLE_BYTES, metadata.title.len());
    }

    #[test]
    fn display_metadata_falls_back_for_empty_or_non_text_initial_history() {
        let no_history = derive_from_history(&[]);
        assert!(!no_history.present);
        assert_eq!(b"Untitled session", no_history.title.as_slice());

        let empty_history = [make_assistant_turn("   \n\t")];
        let metadata = derive_from_history(&empty_history);

        assert!(metadata.present);
        assert_eq!(b"Untitled session", metadata.title.as_slice());
        assert!(metadata.preview.is_none());
    }

    #[test]
    fn display_metadata_skips_slash_only_turns_and_handles_image_only_sessions() {
        let slash_then_prompt = [
            make_assistant_turn("/resume"),
            make_assistant_turn("real prompt title"),
        ];
        let metadata = derive_from_history(&slash_then_prompt);
        assert!(metadata.present);
        assert_eq!(b"real prompt title", metadata.title.as_slice());

        let image = types::ImageAttachment {
            id: 0,
            path: b"/tmp/image.png".to_vec(),
            media_type: b"image/png".to_vec(),
            snapshot_path: None,
            snapshot_sha256: None,
        };
        let image_only = [make_assistant_turn_with_images("", vec![image])];
        let image_metadata = derive_from_history(&image_only);
        assert!(image_metadata.present);
        assert_eq!(b"Image session", image_metadata.title.as_slice());
        assert!(image_metadata.preview.is_none());
    }

    #[test]
    fn display_metadata_sidecar_round_trips_and_invalid_sidecar_falls_back() {
        let source = DisplayMetadata {
            present: true,
            title: b"Plan release notes".to_vec(),
            preview: Some(b"Plan release notes\nship checklist".to_vec()),
            origin_workspace_root: Some(b"/tmp/origin".to_vec()),
        };

        let bytes = encode_sidecar(&source).unwrap();
        let decoded = decode_sidecar(&bytes).unwrap();

        assert!(decoded.present);
        assert_eq!(b"Plan release notes", decoded.title.as_slice());
        assert_eq!(
            b"Plan release notes\nship checklist",
            decoded.preview.unwrap().as_slice()
        );
        assert_eq!(b"/tmp/origin", decoded.origin_workspace_root.unwrap().as_slice());

        let fallback = decode_sidecar_or_fallback(b"{\"schema_version\":1,\"title\":42}");
        assert!(!fallback.present);
        assert_eq!(b"Untitled session", fallback.title.as_slice());
        assert!(fallback.preview.is_none());
    }
}
