//! Skill metadata contract (ported 1:1 from zig-reference/src/core/skills/skill_contract.zig).

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

pub const MAX_FRONTMATTER_BYTES: usize = 64 * 1024;
pub const MAX_NAME_BYTES: usize = 256;
pub const MAX_DESCRIPTION_BYTES: usize = 4 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkillSource {
    WorkspaceFx,
    WorkspaceShared,
    WorkspaceOpencode,
    WorkspaceCodex,
    WorkspaceClaude,
    WorkspaceAgents,
    WorkspaceClaw,
    GlobalFx,
    GlobalOpencode,
    GlobalCodex,
    GlobalClaude,
    GlobalAgents,
    GlobalClaw,
}

/// One default skill root. `path` is relative to a workspace ancestor or the
/// home directory, depending on which table declares the spec.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RootSpec<'a> {
    pub source: SkillSource,
    pub path: &'a [u8],
}

/// Borrowed root policy supplied by a product capability owner.
#[derive(Clone, Copy, Debug, Default)]
pub struct RootPolicy<'a> {
    pub workspace_roots: &'a [RootSpec<'a>],
    /// Source identity for the managed install directory passed to discovery.
    /// A null source excludes that directory from the policy.
    pub managed_root_source: Option<SkillSource>,
    pub global_roots: &'a [RootSpec<'a>],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InvalidMetadataCause {
    MissingClosingDelimiter,
    MissingName,
    DuplicateRecognizedKey,
    InvalidName,
    NameTooLong,
    DescriptionTooLong,
    MalformedQuote,
    UnsupportedMultiline,
    InvalidUtf8,
    ControlByte,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MetadataStatus {
    NoFrontmatter,
    Valid,
    Invalid(InvalidMetadataCause),
}

impl MetadataStatus {
    pub fn invalid(cause: InvalidMetadataCause) -> Self {
        MetadataStatus::Invalid(cause)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockDescriptionStyle {
    FoldedClip,
    FoldedStrip,
    LiteralClip,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockDescription {
    pub style: BlockDescriptionStyle,
    pub base_indent: usize,
    pub decoded_len: usize,
}

impl BlockDescription {
    pub fn decode_into(&self, raw: &[u8], output: &mut [u8]) {
        assert_eq!(output.len(), self.decoded_len);
        let written = decode_block_description(raw, self.base_indent, self.style, Some(output));
        assert_eq!(written, output.len());
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParsedSkillFile<'a> {
    pub name: Option<&'a [u8]>,
    pub description: Option<&'a [u8]>,
    pub description_block: Option<BlockDescription>,
    pub body: &'a [u8],
    pub status: MetadataStatus,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SkillMetadata<'a> {
    pub name: &'a [u8],
    pub description: &'a [u8],
    pub description_block: Option<BlockDescription>,
}

impl SkillMetadata<'_> {
    pub fn description_len(self) -> usize {
        match self.description_block {
            Some(block) => block.decoded_len,
            None => self.description.len(),
        }
    }

    pub fn write_description(self, output: &mut [u8]) {
        assert_eq!(output.len(), self.description_len());
        match self.description_block {
            Some(block) => block.decode_into(self.description, output),
            None => output.copy_from_slice(self.description),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkillMetadataResult<'a> {
    Valid(SkillMetadata<'a>),
    Invalid(InvalidMetadataCause),
}

pub fn resolve_metadata<'a>(
    parsed: ParsedSkillFile<'a>,
    fallback_name: &'a [u8],
) -> SkillMetadataResult<'a> {
    match parsed.status {
        MetadataStatus::NoFrontmatter => {
            if let Some(cause) = invalid_skill_name_cause(fallback_name) {
                SkillMetadataResult::Invalid(cause)
            } else {
                SkillMetadataResult::Valid(SkillMetadata {
                    name: fallback_name,
                    description: b"",
                    description_block: None,
                })
            }
        }
        MetadataStatus::Valid => SkillMetadataResult::Valid(SkillMetadata {
            name: parsed.name.expect("valid metadata has a name"),
            description: parsed.description.unwrap_or(b""),
            description_block: parsed.description_block,
        }),
        MetadataStatus::Invalid(cause) => SkillMetadataResult::Invalid(cause),
    }
}

pub fn parse_skill_file<'a>(content: &'a [u8]) -> ParsedSkillFile<'a> {
    let Some(header_start) = frontmatter_header_start(content) else {
        return ParsedSkillFile {
            name: None,
            description: None,
            description_block: None,
            body: content,
            status: MetadataStatus::NoFrontmatter,
        };
    };
    let Some(closing) = find_closing_delimiter(content, header_start) else {
        return ParsedSkillFile {
            name: None,
            description: None,
            description_block: None,
            body: content,
            status: MetadataStatus::Invalid(InvalidMetadataCause::MissingClosingDelimiter),
        };
    };

    let header = &content[header_start..closing.header_end];
    let body = trim_start_crlf(&content[closing.body_start..]);

    let mut name: Option<&'a [u8]> = None;
    let mut description: Option<&'a [u8]> = None;
    let mut description_block: Option<BlockDescription> = None;
    let mut invalid_cause: Option<InvalidMetadataCause> = None;
    let mut saw_name = false;
    let mut saw_description = false;
    let mut previous_line_recognized = false;

    let mut line_offset: usize = 0;
    while let Some(line) = header_line_at(header, line_offset) {
        line_offset = line.next;
        let trimmed = trim_ascii(line.bytes, b" \t");
        if trimmed.is_empty() || trimmed[0] == b'#' {
            continue;
        }
        if previous_line_recognized && (line.bytes[0] == b' ' || line.bytes[0] == b'\t') {
            set_first_invalid_cause(&mut invalid_cause, InvalidMetadataCause::UnsupportedMultiline);
            previous_line_recognized = false;
            continue;
        }

        let Some(colon_idx) = find_byte(trimmed, b':') else {
            previous_line_recognized = false;
            continue;
        };
        let key = trim_ascii(&trimmed[0..colon_idx], b" \t");
        let raw_value = trim_ascii(&trimmed[colon_idx + 1..], b" \t");

        if key == b"name" {
            previous_line_recognized = true;
            if saw_name {
                set_first_invalid_cause(
                    &mut invalid_cause,
                    InvalidMetadataCause::DuplicateRecognizedKey,
                );
            }
            saw_name = true;
            let parsed_value = parse_recognized_value(raw_value);
            name = Some(parsed_value.value);
            if let Some(cause) = parsed_value.invalid_cause {
                set_first_invalid_cause(&mut invalid_cause, cause);
            }
        } else if key == b"description" {
            if saw_description {
                set_first_invalid_cause(
                    &mut invalid_cause,
                    InvalidMetadataCause::DuplicateRecognizedKey,
                );
            }
            saw_description = true;
            if let Some(style) = block_description_style(raw_value) {
                let parsed_block = parse_block_description(header, line_offset, style);
                description = Some(parsed_block.value);
                description_block = Some(parsed_block.block);
                line_offset = parsed_block.next_offset;
                previous_line_recognized = false;
                if let Some(cause) = parsed_block.invalid_cause {
                    set_first_invalid_cause(&mut invalid_cause, cause);
                }
            } else {
                previous_line_recognized = true;
                let parsed_value = parse_recognized_value(raw_value);
                description = Some(parsed_value.value);
                description_block = None;
                if let Some(cause) = parsed_value.invalid_cause {
                    set_first_invalid_cause(&mut invalid_cause, cause);
                }
            }
        } else {
            previous_line_recognized = false;
        }
    }

    if let Some(skill_name) = name {
        if let Some(cause) = invalid_skill_name_cause(skill_name) {
            set_first_invalid_cause(&mut invalid_cause, cause);
        }
    } else {
        set_first_invalid_cause(&mut invalid_cause, InvalidMetadataCause::MissingName);
    }
    if description_block.is_none() {
        if let Some(skill_description) = description {
            if skill_description.len() > MAX_DESCRIPTION_BYTES {
                set_first_invalid_cause(
                    &mut invalid_cause,
                    InvalidMetadataCause::DescriptionTooLong,
                );
            } else if let Some(cause) = invalid_text_cause(skill_description) {
                set_first_invalid_cause(&mut invalid_cause, cause);
            }
        }
    }
    if invalid_cause.is_some() {
        description_block = None;
    }

    ParsedSkillFile {
        name,
        description,
        description_block,
        body,
        status: match invalid_cause {
            Some(cause) => MetadataStatus::Invalid(cause),
            None => MetadataStatus::Valid,
        },
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct HeaderLine<'a> {
    start: usize,
    bytes: &'a [u8],
    next: usize,
}

fn header_line_at<'a>(header: &'a [u8], start: usize) -> Option<HeaderLine<'a>> {
    if start >= header.len() {
        return None;
    }
    let newline_offset = header[start..].iter().position(|&b| b == b'\n');
    let line_end = match newline_offset {
        Some(offset) => start + offset,
        None => header.len(),
    };
    let mut raw_line = &header[start..line_end];
    if raw_line.len() > 0 && raw_line[raw_line.len() - 1] == b'\r' {
        raw_line = &raw_line[0..raw_line.len() - 1];
    }
    Some(HeaderLine {
        start,
        bytes: raw_line,
        next: if newline_offset.is_some() {
            line_end + 1
        } else {
            line_end
        },
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ParsedBlockDescription<'a> {
    value: &'a [u8],
    block: BlockDescription,
    next_offset: usize,
    invalid_cause: Option<InvalidMetadataCause>,
}

fn parse_block_description<'a>(
    header: &'a [u8],
    start: usize,
    style: BlockDescriptionStyle,
) -> ParsedBlockDescription<'a> {
    let mut base_indent: Option<usize> = None;
    let mut invalid_cause: Option<InvalidMetadataCause> = None;
    let mut can_decode = true;
    let mut line_offset = start;
    let mut block_end = header.len();

    while let Some(line) = header_line_at(header, line_offset) {
        line_offset = line.next;
        if is_structural_blank(line.bytes) {
            continue;
        }

        let indent = leading_space_count(line.bytes);
        if indent == 0 {
            if line.bytes[0] == b'\t' {
                set_first_invalid_cause(
                    &mut invalid_cause,
                    InvalidMetadataCause::UnsupportedMultiline,
                );
                can_decode = false;
                continue;
            }
            block_end = line.start;
            line_offset = line.start;
            break;
        }
        if indent < line.bytes.len() && line.bytes[indent] == b'\t' {
            set_first_invalid_cause(
                &mut invalid_cause,
                InvalidMetadataCause::UnsupportedMultiline,
            );
            can_decode = false;
            continue;
        }

        if let Some(established_indent) = base_indent {
            if indent < established_indent {
                set_first_invalid_cause(
                    &mut invalid_cause,
                    InvalidMetadataCause::UnsupportedMultiline,
                );
                can_decode = false;
                continue;
            }
        } else {
            base_indent = Some(indent);
        }

        let content_line = &line.bytes[base_indent.unwrap()..];
        if let Some(cause) = invalid_text_cause(content_line) {
            set_first_invalid_cause(&mut invalid_cause, cause);
        }
    }

    let raw_value = &header[start..block_end];
    let decoded_len = if can_decode {
        decode_block_description(raw_value, base_indent.unwrap_or(0), style, None)
    } else {
        0
    };
    if decoded_len > MAX_DESCRIPTION_BYTES {
        set_first_invalid_cause(&mut invalid_cause, InvalidMetadataCause::DescriptionTooLong);
    }

    ParsedBlockDescription {
        value: raw_value,
        block: BlockDescription {
            style,
            base_indent: base_indent.unwrap_or(0),
            decoded_len,
        },
        next_offset: line_offset,
        invalid_cause,
    }
}

fn block_description_style(value: &[u8]) -> Option<BlockDescriptionStyle> {
    if value == b">" {
        Some(BlockDescriptionStyle::FoldedClip)
    } else if value == b">-" {
        Some(BlockDescriptionStyle::FoldedStrip)
    } else if value == b"|" {
        Some(BlockDescriptionStyle::LiteralClip)
    } else {
        None
    }
}

fn is_structural_blank(line: &[u8]) -> bool {
    trim_ascii(line, b" \t").is_empty()
}

fn leading_space_count(line: &[u8]) -> usize {
    line.iter().take_while(|&&b| b == b' ').count()
}

struct DescriptionEmitter<'a> {
    output: Option<&'a mut [u8]>,
    len: usize,
}

impl DescriptionEmitter<'_> {
    fn write(&mut self, value: &[u8]) {
        let end = self.len.saturating_add(value.len());
        if let Some(output) = self.output.as_deref_mut() {
            assert!(end <= output.len());
            output[self.len..end].copy_from_slice(value);
        }
        self.len = end;
    }
}

fn decode_block_description(
    raw: &[u8],
    base_indent: usize,
    style: BlockDescriptionStyle,
    output: Option<&mut [u8]>,
) -> usize {
    let mut last_nonblank_next: usize = 0;
    let mut line_offset: usize = 0;
    while let Some(line) = header_line_at(raw, line_offset) {
        line_offset = line.next;
        if !is_structural_blank(line.bytes) {
            last_nonblank_next = line.next;
        }
    }
    if last_nonblank_next == 0 {
        return 0;
    }

    let mut emitter = DescriptionEmitter {
        output,
        len: 0,
    };
    let mut previous_nonblank = false;
    let mut first = true;
    line_offset = 0;
    while line_offset < last_nonblank_next {
        let line = header_line_at(raw, line_offset).expect("line offset is within raw");
        line_offset = line.next;
        let nonblank = !is_structural_blank(line.bytes);
        if !first {
            let separator: &[u8] = match style {
                BlockDescriptionStyle::LiteralClip => b"\n",
                BlockDescriptionStyle::FoldedClip | BlockDescriptionStyle::FoldedStrip => {
                    if previous_nonblank && nonblank {
                        b" "
                    } else {
                        b"\n"
                    }
                }
            };
            emitter.write(separator);
        }
        if nonblank {
            emitter.write(&line.bytes[base_indent..]);
        }
        previous_nonblank = nonblank;
        first = false;
    }
    match style {
        BlockDescriptionStyle::FoldedClip | BlockDescriptionStyle::LiteralClip => {
            emitter.write(b"\n")
        }
        BlockDescriptionStyle::FoldedStrip => {}
    }
    emitter.len
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ClosingDelimiter {
    header_end: usize,
    body_start: usize,
}

pub fn frontmatter_header_start(content: &[u8]) -> Option<usize> {
    if content.starts_with(b"---\r\n") {
        return Some(5);
    }
    if content.starts_with(b"---\n") {
        return Some(4);
    }
    if content == b"---" {
        return Some(3);
    }
    None
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SkillMetadataScanner {
    offset: usize,
    line_prefix: [u8; 4],
    line_len: usize,
}

impl SkillMetadataScanner {
    fn init(header_start: usize) -> SkillMetadataScanner {
        SkillMetadataScanner {
            offset: header_start,
            line_prefix: [0; 4],
            line_len: 0,
        }
    }

    fn scan(&mut self, content: &[u8], complete: bool) -> bool {
        while self.offset < content.len() {
            let byte = content[self.offset];
            self.offset += 1;
            if byte == b'\n' {
                let closing = self.is_closing_line(true);
                self.line_len = 0;
                if closing {
                    return true;
                }
                continue;
            }
            if self.line_len < self.line_prefix.len() {
                self.line_prefix[self.line_len] = byte;
            }
            self.line_len += 1;
        }
        complete && self.is_closing_line(false)
    }

    fn is_closing_line(&self, newline_terminated: bool) -> bool {
        if self.line_len == 3 && &self.line_prefix[0..3] == b"---" {
            return true;
        }
        newline_terminated && self.line_len == 4 && &self.line_prefix[0..4] == b"---\r"
    }
}

#[derive(Debug)]
pub enum ReadMetadataError {
    Io(std::io::Error),
    UnexpectedEndOfFile,
    StreamTooLong,
}

impl From<std::io::Error> for ReadMetadataError {
    fn from(value: std::io::Error) -> Self {
        ReadMetadataError::Io(value)
    }
}

pub type ReadMetadataResult<T> = std::result::Result<T, ReadMetadataError>;

/// Reads just enough of a skill file to inspect its frontmatter. The returned
/// slice owns its bytes and is at most `MAX_FRONTMATTER_BYTES + 1` long.
pub fn read_metadata_prefix(file: &File, file_size: usize) -> ReadMetadataResult<Vec<u8>> {
    let readable_size = file_size.min(MAX_FRONTMATTER_BYTES + 1);
    let mut content: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 16 * 1024];
    let mut scanner: Option<SkillMetadataScanner> = None;
    let mut metadata_complete = false;

    while content.len() < readable_size {
        let wanted = chunk.len().min(readable_size - content.len());
        let read = read_at(file, &mut chunk[0..wanted], content.len() as u64)?;
        if read == 0 {
            return Err(ReadMetadataError::UnexpectedEndOfFile);
        }
        content.extend_from_slice(&chunk[0..read]);

        let file_complete = content.len() == file_size;
        if scanner.is_none() {
            if content.len() < 5 && !file_complete {
                continue;
            }
            let Some(header_start) = frontmatter_header_start(&content) else {
                metadata_complete = true;
                break;
            };
            scanner = Some(SkillMetadataScanner::init(header_start));
        }
        if scanner
            .as_mut()
            .expect("scanner initialized above")
            .scan(&content, file_complete)
        {
            metadata_complete = true;
            break;
        }
    }
    if !metadata_complete && content.len() < file_size {
        return Err(ReadMetadataError::StreamTooLong);
    }
    Ok(content)
}

fn read_at(file: &File, buf: &mut [u8], offset: u64) -> std::io::Result<usize> {
    let mut reader = file.try_clone()?;
    reader.seek(SeekFrom::Start(offset))?;
    reader.read(buf)
}

fn find_closing_delimiter(content: &[u8], header_start: usize) -> Option<ClosingDelimiter> {
    let mut line_start = header_start;
    while line_start <= content.len() {
        let newline_offset = content[line_start..].iter().position(|&b| b == b'\n');
        let line_end = match newline_offset {
            Some(offset) => line_start + offset,
            None => content.len(),
        };
        let raw_line = &content[line_start..line_end];
        let line = if newline_offset.is_some()
            && raw_line.len() > 0
            && raw_line[raw_line.len() - 1] == b'\r'
        {
            &raw_line[0..raw_line.len() - 1]
        } else {
            raw_line
        };

        if line == b"---" {
            return Some(ClosingDelimiter {
                header_end: line_start,
                body_start: if newline_offset.is_some() {
                    line_end + 1
                } else {
                    line_end
                },
            });
        }
        if newline_offset.is_none() {
            return None;
        }
        line_start = line_end + 1;
    }
    None
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ParsedRecognizedValue<'a> {
    value: &'a [u8],
    invalid_cause: Option<InvalidMetadataCause>,
}

fn parse_recognized_value(value: &[u8]) -> ParsedRecognizedValue<'_> {
    if value.len() > 0 && (value[0] == b'|' || value[0] == b'>') {
        return ParsedRecognizedValue {
            value,
            invalid_cause: Some(InvalidMetadataCause::UnsupportedMultiline),
        };
    }

    let starts_with_quote =
        value.len() > 0 && (value[0] == b'\'' || value[0] == b'"');
    let ends_with_quote = value.len() > 0
        && (value[value.len() - 1] == b'\'' || value[value.len() - 1] == b'"');
    if starts_with_quote || ends_with_quote {
        if value.len() >= 2 && value[0] == value[value.len() - 1] {
            return ParsedRecognizedValue {
                value: &value[1..value.len() - 1],
                invalid_cause: None,
            };
        }
        return ParsedRecognizedValue {
            value,
            invalid_cause: Some(InvalidMetadataCause::MalformedQuote),
        };
    }
    ParsedRecognizedValue {
        value,
        invalid_cause: None,
    }
}

fn set_first_invalid_cause(
    current: &mut Option<InvalidMetadataCause>,
    cause: InvalidMetadataCause,
) {
    if current.is_none() {
        *current = Some(cause);
    }
}

fn invalid_text_cause(value: &[u8]) -> Option<InvalidMetadataCause> {
    if std::str::from_utf8(value).is_err() {
        return Some(InvalidMetadataCause::InvalidUtf8);
    }
    for &byte in value {
        if byte < 0x20 || byte == 0x7f {
            return Some(InvalidMetadataCause::ControlByte);
        }
    }
    None
}

pub fn invalid_skill_name_cause(name: &[u8]) -> Option<InvalidMetadataCause> {
    if name.is_empty() {
        return Some(InvalidMetadataCause::MissingName);
    }
    if name.len() > MAX_NAME_BYTES {
        return Some(InvalidMetadataCause::NameTooLong);
    }
    if let Some(cause) = invalid_text_cause(name) {
        return Some(cause);
    }
    match validate_managed_skill_name(name) {
        Ok(()) => None,
        Err(_) => Some(InvalidMetadataCause::InvalidName),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValidateSkillNameError {
    InvalidSkillName,
}

pub fn validate_managed_skill_name(name: &[u8]) -> Result<(), ValidateSkillNameError> {
    if name.is_empty() || name.len() > MAX_NAME_BYTES {
        return Err(ValidateSkillNameError::InvalidSkillName);
    }
    if name == b"." || name == b".." {
        return Err(ValidateSkillNameError::InvalidSkillName);
    }
    if is_absolute_path(name) {
        return Err(ValidateSkillNameError::InvalidSkillName);
    }
    if name.contains(&b'/') || name.contains(&b'\\') {
        return Err(ValidateSkillNameError::InvalidSkillName);
    }
    Ok(())
}

fn is_absolute_path(name: &[u8]) -> bool {
    name.starts_with(b"/")
}

fn trim_ascii<'a>(mut value: &'a [u8], chars: &[u8]) -> &'a [u8] {
    while value.first().is_some_and(|b| chars.contains(b)) {
        value = &value[1..];
    }
    while value.last().is_some_and(|b| chars.contains(b)) {
        value = &value[..value.len() - 1];
    }
    value
}

fn trim_start_crlf(mut value: &[u8]) -> &[u8] {
    while value.first() == Some(&b'\r') || value.first() == Some(&b'\n') {
        value = &value[1..];
    }
    value
}

fn find_byte(haystack: &[u8], needle: u8) -> Option<usize> {
    haystack.iter().position(|&b| b == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expect_resolved_description(content: &[u8], expected: &[u8]) {
        let metadata = match resolve_metadata(parse_skill_file(content), b"fallback") {
            SkillMetadataResult::Valid(value) => value,
            SkillMetadataResult::Invalid(_) => panic!("expected valid metadata"),
        };
        let mut description = vec![0u8; metadata.description_len()];
        metadata.write_description(&mut description);
        assert_eq!(expected, description.as_slice());
    }

    #[test]
    fn parse_skill_file_with_full_frontmatter() {
        let content = b"---\nname: my-skill\ndescription: Helps with testing\n---\n\n# My Skill\n\nDo the thing.";
        let parsed = parse_skill_file(content);
        assert_eq!(b"my-skill", parsed.name.unwrap());
        assert_eq!(b"Helps with testing", parsed.description.unwrap());
        assert_eq!(b"# My Skill\n\nDo the thing.", parsed.body);
    }

    #[test]
    fn parse_skill_file_accepts_supported_description_block_forms() {
        let cases: [&[u8]; 3] = [
            b"---\nname: folded\ndescription: >\n  Fold this\n  onto one line.\n---\nBody",
            b"---\nname: folded-strip\ndescription: >-\n  Fold without\n  a trailing newline.\n---\nBody",
            b"---\nname: literal\ndescription: |\n  Keep this\n  on two lines.\n---\nBody",
        ];
        for content in cases {
            assert_eq!(MetadataStatus::Valid, parse_skill_file(content).status);
        }
    }

    #[test]
    fn description_blocks_preserve_folded_literal_and_trailing_newline_semantics() {
        expect_resolved_description(
            b"---\nname: folded\ndescription: >\n  Fold this\n  onto one line.\n\n  Keep this paragraph.\n---\nBody",
            b"Fold this onto one line.\n\nKeep this paragraph.\n",
        );
        expect_resolved_description(
            b"---\nname: folded-strip\ndescription: >-\n  Fold without\n  a trailing newline.\n---\nBody",
            b"Fold without a trailing newline.",
        );
        expect_resolved_description(
            b"---\nname: literal\ndescription: |\n  Keep this\n  on two lines.\n---\nBody",
            b"Keep this\non two lines.\n",
        );
        expect_resolved_description(
            b"---\r\nname: crlf\r\ndescription: |\r\n  first\r\n  second\r\n---\r\nBody",
            b"first\nsecond\n",
        );
        expect_resolved_description(b"---\nname: empty\ndescription: >\n\n---\nBody", b"");
    }

    #[test]
    fn description_blocks_return_to_top_level_metadata_and_reject_malformed_structure() {
        let valid = parse_skill_file(
            b"---\ndescription: >-\n  first\n    extra indent\nname: after-block\n---\nBody",
        );
        assert_eq!(MetadataStatus::Valid, valid.status);
        assert_eq!(b"after-block", valid.name.unwrap());
        expect_resolved_description(
            b"---\ndescription: >-\n  first\n    extra indent\nname: after-block\n---\nBody",
            b"first   extra indent",
        );

        let cases: [(&[u8], InvalidMetadataCause); 6] = [
            (
                b"---\nname: unsupported\ndescription: >+\n  value\n---\n",
                InvalidMetadataCause::UnsupportedMultiline,
            ),
            (
                b"---\nname: >\n  block names stay invalid\n---\n",
                InvalidMetadataCause::UnsupportedMultiline,
            ),
            (
                b"---\nname: tabbed\ndescription: >\n\tvalue\n---\n",
                InvalidMetadataCause::UnsupportedMultiline,
            ),
            (
                b"---\nname: shallow\ndescription: >\n   first\n  smaller indent\n---\n",
                InvalidMetadataCause::UnsupportedMultiline,
            ),
            (
                b"---\nname: invalid-utf8\ndescription: >\n  bad\xff\n---\n",
                InvalidMetadataCause::InvalidUtf8,
            ),
            (
                b"---\nname: control\ndescription: >\n  bad\x01\n---\n",
                InvalidMetadataCause::ControlByte,
            ),
        ];
        for (content, cause) in cases {
            assert_eq!(
                MetadataStatus::Invalid(cause),
                parse_skill_file(content).status
            );
        }
    }

    #[test]
    fn description_blocks_enforce_the_decoded_byte_limit() {
        let exact = b"d".repeat(MAX_DESCRIPTION_BYTES);
        let over = format!("{}d", String::from_utf8_lossy(&exact));
        let mut exact_doc = b"---\nname: exact\ndescription: >-\n  ".to_vec();
        exact_doc.extend_from_slice(&exact);
        exact_doc.extend_from_slice(b"\n---\n");
        assert_eq!(MetadataStatus::Valid, parse_skill_file(&exact_doc).status);

        let mut over_doc = b"---\nname: over\ndescription: >-\n  ".to_vec();
        over_doc.extend_from_slice(over.as_bytes());
        over_doc.extend_from_slice(b"\n---\n");
        assert_eq!(
            MetadataStatus::Invalid(InvalidMetadataCause::DescriptionTooLong),
            parse_skill_file(&over_doc).status
        );

        let clipped = b"d".repeat(MAX_DESCRIPTION_BYTES - 1);
        let mut clipped_doc = b"---\nname: clipped\ndescription: |\n  ".to_vec();
        clipped_doc.extend_from_slice(&clipped);
        clipped_doc.extend_from_slice(b"\n---\n");
        assert_eq!(MetadataStatus::Valid, parse_skill_file(&clipped_doc).status);
    }

    #[test]
    fn metadata_scanner_examines_malformed_and_late_delimiters_once() {
        let chunk_bytes = 16 * 1024;

        let malformed = vec![b'a'; 256 * 1024 + 37];
        let malformed_header = b"---\nname: linear\n";
        let mut malformed = malformed;
        malformed[0..malformed_header.len()].copy_from_slice(malformed_header);
        let mut malformed_scanner = SkillMetadataScanner::init(4);
        let mut malformed_end = 0;
        let mut malformed_found = false;
        while malformed_end < malformed.len() {
            malformed_end = (malformed_end + chunk_bytes).min(malformed.len());
            malformed_found = malformed_scanner.scan(&malformed[0..malformed_end], malformed_end == malformed.len());
        }
        assert!(!malformed_found);
        assert_eq!(malformed.len(), malformed_scanner.offset);

        let mut late = vec![b'b'; 256 * 1024 + 37];
        let late_header = b"---\nname: late\n";
        late[0..late_header.len()].copy_from_slice(late_header);
        let late_tail = b"\n---";
        let late_tail_start = late.len() - late_tail.len();
        late[late_tail_start..].copy_from_slice(late_tail);
        let mut late_scanner = SkillMetadataScanner::init(4);
        let mut late_end = 0;
        let mut late_found = false;
        while late_end < late.len() && !late_found {
            late_end = (late_end + chunk_bytes).min(late.len());
            late_found = late_scanner.scan(&late[0..late_end], late_end == late.len());
        }
        assert!(late_found);
        assert_eq!(late.len(), late_scanner.offset);
    }

    #[test]
    fn parse_skill_file_without_frontmatter_returns_full_content_as_body() {
        let content = b"# Just Markdown\n\nSome content.";
        let parsed = parse_skill_file(content);
        assert_eq!(MetadataStatus::NoFrontmatter, parsed.status);
        assert!(parsed.name.is_none());
        assert!(parsed.description.is_none());
        assert_eq!(content, parsed.body);
    }

    #[test]
    fn parse_skill_file_with_partial_frontmatter() {
        let content = b"---\nname: partial\n---\n\nBody here.";
        let parsed = parse_skill_file(content);
        assert_eq!(MetadataStatus::Valid, parsed.status);
        assert_eq!(b"partial", parsed.name.unwrap());
        assert!(parsed.description.is_none());
        assert_eq!(b"Body here.", parsed.body);
    }

    #[test]
    fn parse_skill_file_enforces_hard_metadata_field_bounds() {
        let valid_name = b"n".repeat(MAX_NAME_BYTES);
        let oversized_name = format!("{}n", String::from_utf8_lossy(&valid_name));
        let valid_description = b"d".repeat(MAX_DESCRIPTION_BYTES);
        let oversized_description = format!("{}d", String::from_utf8_lossy(&valid_description));

        let mut name_at_limit = b"---\nname: ".to_vec();
        name_at_limit.extend_from_slice(&valid_name);
        name_at_limit.extend_from_slice(b"\n---\n");
        assert_eq!(MetadataStatus::Valid, parse_skill_file(&name_at_limit).status);

        let mut name_over_limit = b"---\nname: ".to_vec();
        name_over_limit.extend_from_slice(oversized_name.as_bytes());
        name_over_limit.extend_from_slice(b"\n---\n");
        assert_eq!(
            MetadataStatus::Invalid(InvalidMetadataCause::NameTooLong),
            parse_skill_file(&name_over_limit).status
        );

        let mut description_at_limit = b"---\nname: valid\ndescription: ".to_vec();
        description_at_limit.extend_from_slice(&valid_description);
        description_at_limit.extend_from_slice(b"\n---\n");
        assert_eq!(
            MetadataStatus::Valid,
            parse_skill_file(&description_at_limit).status
        );

        let mut description_over_limit = b"---\nname: valid\ndescription: ".to_vec();
        description_over_limit.extend_from_slice(oversized_description.as_bytes());
        description_over_limit.extend_from_slice(b"\n---\n");
        assert_eq!(
            MetadataStatus::Invalid(InvalidMetadataCause::DescriptionTooLong),
            parse_skill_file(&description_over_limit).status
        );
    }

    #[test]
    fn parse_skill_file_normalizes_crlf_delimiters_and_simply_quoted_values() {
        let content = b"---\r\nname: \"windows-newline\"\r\ndescription: 'quoted description'\r\n---\r\nBody";
        let parsed = parse_skill_file(content);
        assert_eq!(MetadataStatus::Valid, parsed.status);
        assert_eq!(b"windows-newline", parsed.name.unwrap());
        assert_eq!(b"quoted description", parsed.description.unwrap());
        assert_eq!(b"Body", parsed.body);

        let mixed = parse_skill_file(b"---\r\nname: mixed-endings\ndescription: mixed\r\n---\nBody");
        assert_eq!(MetadataStatus::Valid, mixed.status);
        assert_eq!(b"mixed-endings", mixed.name.unwrap());
        assert_eq!(b"mixed", mixed.description.unwrap());
        assert_eq!(b"Body", mixed.body);
    }

    #[test]
    fn parse_skill_file_ignores_unknown_keys_and_colonless_lines() {
        let content = b"---\nignored\nname: known\nextra: value\ndescription: useful\n---\nBody";
        let parsed = parse_skill_file(content);
        assert_eq!(b"known", parsed.name.unwrap());
        assert_eq!(b"useful", parsed.description.unwrap());
        assert_eq!(b"Body", parsed.body);
    }

    #[test]
    fn parse_skill_file_removes_one_matching_pair_of_outer_quotes() {
        let content = b"---\nname: \"quoted-name\"\ndescription: 'quoted description'\n---\nBody\n";
        let parsed = parse_skill_file(content);
        assert_eq!(MetadataStatus::Valid, parsed.status);
        assert_eq!(b"quoted-name", parsed.name.unwrap());
        assert_eq!(b"quoted description", parsed.description.unwrap());
    }

    #[test]
    fn resolve_metadata_gives_discovery_and_installation_one_validity_result() {
        let legacy = resolve_metadata(parse_skill_file(b"# Legacy\n"), b"legacy");
        match legacy {
            SkillMetadataResult::Valid(value) => {
                assert_eq!(b"legacy", value.name);
                assert_eq!(b"", value.description);
            }
            SkillMetadataResult::Invalid(_) => panic!("expected valid legacy metadata"),
        }

        let valid = resolve_metadata(
            parse_skill_file(b"---\nname: review\ndescription: 'review helper'\n---\nbody"),
            b"fallback",
        );
        match valid {
            SkillMetadataResult::Valid(value) => {
                assert_eq!(b"review", value.name);
                assert_eq!(b"review helper", value.description);
            }
            SkillMetadataResult::Invalid(_) => panic!("expected valid metadata"),
        }

        let malformed = resolve_metadata(
            parse_skill_file(b"---\nname: first\nname: second\n---\nbody"),
            b"fallback",
        );
        assert_eq!(
            SkillMetadataResult::Invalid(InvalidMetadataCause::DuplicateRecognizedKey),
            malformed
        );

        let unsafe_legacy = resolve_metadata(parse_skill_file(b"# Legacy\n"), b"../unsafe");
        assert_eq!(
            SkillMetadataResult::Invalid(InvalidMetadataCause::InvalidName),
            unsafe_legacy
        );
    }

    #[test]
    fn parse_skill_file_accepts_an_exact_closing_delimiter_at_end_of_file() {
        let parsed = parse_skill_file(b"---\nname: eof-close\n---");
        assert_eq!(MetadataStatus::Valid, parsed.status);
        assert_eq!(b"eof-close", parsed.name.unwrap());
        assert_eq!(b"", parsed.body);
    }

    #[test]
    fn parse_skill_file_classifies_invalid_recognized_metadata() {
        let cases: [(&[u8], InvalidMetadataCause, Option<&[u8]>); 11] = [
            (
                b"---\nname: unclosed",
                InvalidMetadataCause::MissingClosingDelimiter,
                None,
            ),
            (
                b"---\nname: prefixed-close\n---suffix\nBody",
                InvalidMetadataCause::MissingClosingDelimiter,
                None,
            ),
            (
                b"---\nname: bare-cr-close\n---\r",
                InvalidMetadataCause::MissingClosingDelimiter,
                None,
            ),
            (
                b"---\ndescription: missing name\n---\nBody",
                InvalidMetadataCause::MissingName,
                None,
            ),
            (b"---\nname: \"\"\n---\nBody", InvalidMetadataCause::MissingName, Some(b"")),
            (
                b"---\nname: first\nname: second\n---\nBody",
                InvalidMetadataCause::DuplicateRecognizedKey,
                Some(b"second"),
            ),
            (
                b"---\nname: ../unsafe\n---\nBody",
                InvalidMetadataCause::InvalidName,
                Some(b"../unsafe"),
            ),
            (
                b"---\nname: \"unterminated\n---\nBody",
                InvalidMetadataCause::MalformedQuote,
                Some(b"\"unterminated"),
            ),
            (
                b"---\nname: multiline\ndescription: |2\n---\nBody",
                InvalidMetadataCause::UnsupportedMultiline,
                Some(b"multiline"),
            ),
            (
                b"---\nname: invalid\xff\n---\nBody",
                InvalidMetadataCause::InvalidUtf8,
                Some(b"invalid\xff"),
            ),
            (
                b"---\nname: control\x01byte\n---\nBody",
                InvalidMetadataCause::ControlByte,
                Some(b"control\x01byte"),
            ),
        ];

        for (content, cause, expected_name) in cases {
            let parsed = parse_skill_file(content);
            match parsed.status {
                MetadataStatus::Invalid(actual) => assert_eq!(cause, actual),
                _ => panic!("expected invalid metadata"),
            }
            match expected_name {
                Some(expected) => assert_eq!(expected, parsed.name.unwrap()),
                None => assert!(parsed.name.is_none()),
            }
        }
    }

    #[test]
    fn parse_skill_file_rejects_indented_continuations_for_recognized_metadata() {
        let cases: [&[u8]; 4] = [
            b"---\nname: workflow\n  continued name\ndescription: helper\n---\nBody",
            b"---\nname: workflow\ndescription: first line\n  continued description\n---\nBody",
            b"---\nname: workflow\ndescription: first line\n  continued: description\n---\nBody",
            b"---\nname: workflow\ndescription:\n  continued description\n---\nBody",
        ];
        for content in cases {
            assert_eq!(
                MetadataStatus::Invalid(InvalidMetadataCause::UnsupportedMultiline),
                parse_skill_file(content).status
            );
        }
    }

    #[test]
    fn parse_skill_file_keeps_fuzzed_borrowed_slices_inside_the_input() {
        let corpus: [&[u8]; 6] = [
            b"",
            b"plain body",
            b"---\nname: valid\n---\nbody",
            b"---\r\nname: \"valid\"\r\n---\r\nbody",
            b"---\nname: quoted\ndescription: 'single line'\n---\nbody",
            b"---\nname: block\ndescription: >-\n  first line\n  second line\n---\nbody",
        ];
        for input in corpus {
            fuzz_parse_skill_file(input);
        }

        let mut state: u64 = 0x9e3779b97f4a7c15;
        let mut inputs: Vec<Vec<u8>> = corpus.iter().map(|s| s.to_vec()).collect();
        for _ in 0..256 {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let len = (state >> 33) as usize % 4096;
            let mut buf = vec![0u8; len];
            for byte in buf.iter_mut() {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                *byte = (state >> 33) as u8;
            }
            inputs.push(buf);
        }
        for input in &inputs {
            fuzz_parse_skill_file(input);
        }
    }

    fn fuzz_parse_skill_file(input: &[u8]) {
        let parsed = parse_skill_file(input);
        expect_borrowed_slice(input, parsed.body);
        if let Some(name) = parsed.name {
            expect_borrowed_slice(input, name);
        }
        if let Some(description) = parsed.description {
            expect_borrowed_slice(input, description);
            if let Some(block) = parsed.description_block {
                assert!(block.decoded_len <= MAX_DESCRIPTION_BYTES);
                let mut decoded = vec![0u8; block.decoded_len];
                block.decode_into(description, &mut decoded);
            }
        }
    }

    fn expect_borrowed_slice(input: &[u8], borrowed: &[u8]) {
        let input_start = input.as_ptr() as usize;
        let input_end = input_start + input.len();
        let borrowed_start = borrowed.as_ptr() as usize;
        let borrowed_end = borrowed_start + borrowed.len();
        assert!(borrowed_start >= input_start);
        assert!(borrowed_end <= input_end);
    }

    #[test]
    fn validate_managed_skill_name_accepts_plain_names_and_rejects_path_shapes() {
        assert!(validate_managed_skill_name(b"review").is_ok());

        let invalid_names: [&[u8]; 6] = [
            b"",
            b".",
            b"..",
            b"nested/name",
            b"nested\\name",
            b"/absolute",
        ];
        for name in invalid_names {
            assert_eq!(
                Err(ValidateSkillNameError::InvalidSkillName),
                validate_managed_skill_name(name)
            );
        }
    }
}
