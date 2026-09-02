use crate::core::shared::entity_spans::Span;

/// Pasted text stays visible inline until it exceeds the threshold; oversized
/// blocks collapse to placeholders and expand again before submit.
pub const large_paste_char_threshold: usize = 1000;

/// A block of pasted text replaced in the input by a short
/// `[Pasted text #N, M lines]` placeholder. The containing list owns
/// `text` and releases it through `clear_blocks` or `deinit_blocks`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PastedBlock {
    pub id: usize,
    pub text: Vec<u8>,
    pub line_count: usize,
    pub span: Span,
}

/// Releases every backing text allocation and the list allocation.
pub fn deinit_blocks(blocks: &mut Vec<PastedBlock>) {
    blocks.clear();
}

/// Releases every backing text allocation while retaining the list capacity.
pub fn clear_blocks(blocks: &mut Vec<PastedBlock>) {
    blocks.clear();
}

pub fn count_lines(bytes: &[u8]) -> usize {
    if bytes.is_empty() {
        return 0;
    }
    let mut lines: usize = 1;
    for &b in bytes {
        if b == b'\n' {
            lines += 1;
        }
    }
    // A trailing newline shouldn't inflate the count.
    if bytes[bytes.len() - 1] == b'\n' && lines > 1 {
        lines -= 1;
    }
    lines
}

pub fn count_codepoints(bytes: &[u8]) -> usize {
    let mut count: usize = 0;
    let mut i: usize = 0;
    while i < bytes.len() {
        let width = utf8_byte_sequence_length(bytes[i]).unwrap_or(1);
        i += width.min(bytes.len() - i);
        count += 1;
    }
    count
}

fn utf8_byte_sequence_length(first: u8) -> Option<usize> {
    match first {
        0x00..=0x7f => Some(1),
        0xc0..=0xdf => Some(2),
        0xe0..=0xef => Some(3),
        0xf0..=0xf7 => Some(4),
        _ => None,
    }
}

pub fn should_use_placeholder(bytes: &[u8]) -> bool {
    count_codepoints(bytes) > large_paste_char_threshold
}

/// Write the placeholder that stands in for a pasted block inside the input.
pub fn format_placeholder(id: usize, line_count: usize) -> Vec<u8> {
    let noun = if line_count == 1 { "line" } else { "lines" };
    format!("[Pasted text #{id}, {line_count} {noun}]").into_bytes()
}

/// Replace every registered pasted-text span with its matching block content.
/// When nothing matches, `owned` is false and `text` is a copy of the input.
pub struct ExpandResult {
    pub text: Vec<u8>,
    pub owned: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InvalidPasteRange;

pub fn expand(text: &[u8], blocks: &[PastedBlock]) -> ExpandResult {
    if blocks.is_empty() {
        return ExpandResult {
            text: text.to_vec(),
            owned: false,
        };
    }

    let mut out: Vec<u8> = Vec::new();
    let mut i: usize = 0;
    let mut expanded_any = false;
    while i < text.len() {
        if let Some(block) = registered_block_starting_at(text, blocks, i) {
            out.extend_from_slice(&block.text);
            i = block.span.raw_end;
            expanded_any = true;
            continue;
        }
        out.push(text[i]);
        i += 1;
    }

    if !expanded_any {
        return ExpandResult {
            text: text.to_vec(),
            owned: false,
        };
    }
    ExpandResult {
        text: out,
        owned: true,
    }
}

/// Expands registered paste spans fully contained in `[range_start, range_end)`.
/// The returned text contains only that range.
pub fn expand_range(
    text: &[u8],
    blocks: &[PastedBlock],
    range_start: usize,
    range_end: usize,
) -> Result<ExpandResult, InvalidPasteRange> {
    if range_start > range_end || range_end > text.len() {
        return Err(InvalidPasteRange);
    }
    if blocks.is_empty() {
        return Ok(ExpandResult {
            text: text[range_start..range_end].to_vec(),
            owned: false,
        });
    }

    let mut out: Vec<u8> = Vec::new();
    let mut expanded_any = false;
    let mut i = range_start;
    while i < range_end {
        if let Some(block) = registered_block_starting_at(text, blocks, i) {
            if block.span.raw_end <= range_end {
                out.extend_from_slice(&block.text);
                i = block.span.raw_end;
                expanded_any = true;
                continue;
            }
        }
        out.push(text[i]);
        i += 1;
    }

    if !expanded_any {
        return Ok(ExpandResult {
            text: text[range_start..range_end].to_vec(),
            owned: false,
        });
    }
    Ok(ExpandResult {
        text: out,
        owned: true,
    })
}

/// Returns the byte length after registered paste placeholders are expanded.
/// Null means the requested range is invalid or its length overflowed.
pub fn expanded_range_len(
    text: &[u8],
    blocks: &[PastedBlock],
    range_start: usize,
    range_end: usize,
) -> Option<usize> {
    if range_start > range_end || range_end > text.len() {
        return None;
    }
    if blocks.is_empty() {
        return Some(range_end - range_start);
    }

    let mut expanded_len: usize = 0;
    let mut i = range_start;
    while i < range_end {
        if let Some(block) = registered_block_starting_at(text, blocks, i) {
            if block.span.raw_end <= range_end {
                expanded_len = expanded_len.checked_add(block.text.len())?;
                i = block.span.raw_end;
                continue;
            }
        }
        expanded_len = expanded_len.checked_add(1)?;
        i += 1;
    }
    Some(expanded_len)
}

pub fn expanded_len(text: &[u8], blocks: &[PastedBlock]) -> Option<usize> {
    let mut expanded_len = text.len();
    for block in blocks {
        if !is_registered_block(text, block) {
            continue;
        }
        let placeholder_len = block.span.raw_end - block.span.raw_start;
        if block.text.len() >= placeholder_len {
            expanded_len = expanded_len.checked_add(block.text.len() - placeholder_len)?;
        } else {
            expanded_len = expanded_len.checked_sub(placeholder_len - block.text.len())?;
        }
    }
    Some(expanded_len)
}

fn registered_block_starting_at<'a>(
    text: &[u8],
    blocks: &'a [PastedBlock],
    raw_start: usize,
) -> Option<&'a PastedBlock> {
    for block in blocks {
        if block.span.raw_start < raw_start {
            continue;
        }
        if block.span.raw_start > raw_start {
            return None;
        }
        if !is_registered_block(text, block) {
            return None;
        }
        return Some(block);
    }
    None
}

fn is_registered_block(text: &[u8], block: &PastedBlock) -> bool {
    if !block.span.is_valid(text.len()) {
        return false;
    }
    let canonical = format_placeholder(block.id, block.line_count);
    &text[block.span.raw_start..block.span.raw_end] == canonical.as_slice()
}

pub fn registered_placeholder_span_starting_at(
    text: &[u8],
    raw_start: usize,
    blocks: &[PastedBlock],
) -> Option<PlaceholderSpan> {
    let block = registered_block_starting_at(text, blocks, raw_start)?;
    Some(PlaceholderSpan {
        start: block.span.raw_start,
        end: block.span.raw_end,
        id: block.id,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlaceholderSpan {
    pub start: usize,
    pub end: usize,
    pub id: usize,
}

/// Maps a byte offset in placeholder-bearing input into the expanded text.
/// Returns null when the offset lands inside a placeholder span.
pub fn project_offset_through_expansion(
    raw_input: &[u8],
    pasted_blocks: &[PastedBlock],
    raw_offset: usize,
) -> Option<usize> {
    if raw_offset > raw_input.len() {
        return None;
    }
    let mut input_offset: usize = 0;
    let mut output_offset: usize = 0;
    while input_offset < raw_offset {
        if let Some(block) = registered_block_starting_at(raw_input, pasted_blocks, input_offset) {
            if raw_offset < block.span.raw_end {
                return None;
            }
            output_offset = output_offset.checked_add(block.text.len())?;
            input_offset = block.span.raw_end;
            continue;
        }
        input_offset += 1;
        output_offset = output_offset.checked_add(1)?;
    }
    Some(output_offset)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn count_lines_counts_lines_with_and_without_trailing_newline() {
        assert_eq!(0, count_lines("".as_bytes()));
        assert_eq!(1, count_lines("foo".as_bytes()));
        assert_eq!(1, count_lines("foo\n".as_bytes()));
        assert_eq!(2, count_lines("foo\nbar".as_bytes()));
        assert_eq!(2, count_lines("foo\nbar\n".as_bytes()));
        assert_eq!(3, count_lines("a\nb\nc".as_bytes()));
    }

    #[test]
    fn format_placeholder_singular_and_plural() {
        assert_eq!(
            "[Pasted text #1, 1 line]".as_bytes(),
            format_placeholder(1, 1).as_slice()
        );
        assert_eq!(
            "[Pasted text #42, 17 lines]".as_bytes(),
            format_placeholder(42, 17).as_slice()
        );
    }

    #[test]
    fn should_use_placeholder_matches_large_paste_threshold() {
        assert!(!should_use_placeholder("hello".as_bytes()));
        assert!(!should_use_placeholder("x".repeat(large_paste_char_threshold).as_bytes()));
        assert!(should_use_placeholder(
            "x".repeat(large_paste_char_threshold + 1).as_bytes()
        ));
        assert!(!should_use_placeholder(
            "é".repeat(large_paste_char_threshold).as_bytes()
        ));
        assert!(should_use_placeholder(
            "é".repeat(large_paste_char_threshold + 1).as_bytes()
        ));
    }

    #[test]
    fn expand_substitutes_placeholder_for_real_text() {
        let input = "prefix [Pasted text #1, 2 lines] suffix";
        let start = "prefix ".len();
        let mut blocks: Vec<PastedBlock> = Vec::new();
        blocks.push(PastedBlock {
            id: 1,
            text: b"hello\nworld".to_vec(),
            line_count: 2,
            span: Span {
                raw_start: start,
                raw_end: start + "[Pasted text #1, 2 lines]".len(),
            },
        });

        let result = expand(input.as_bytes(), &blocks);
        assert_eq!("prefix hello\nworld suffix".as_bytes(), result.text.as_slice());
        assert!(result.owned);
    }

    #[test]
    fn expand_with_no_placeholder_returns_original_slice_unowned() {
        let blocks: Vec<PastedBlock> = Vec::new();

        let result = expand("nothing to replace".as_bytes(), &blocks);
        assert!(!result.owned);
        assert_eq!("nothing to replace".as_bytes(), result.text.as_slice());
    }

    #[test]
    fn expand_preserves_unknown_placeholder_ids_verbatim() {
        let mut blocks: Vec<PastedBlock> = Vec::new();
        blocks.push(PastedBlock {
            id: 1,
            text: b"X".to_vec(),
            line_count: 1,
            span: Span {
                raw_start: "before [Pasted text #9, 4 lines] ".len(),
                raw_end: "before [Pasted text #9, 4 lines] [Pasted text #1, 1 line]".len(),
            },
        });

        let result = expand(
            "before [Pasted text #9, 4 lines] [Pasted text #1, 1 line]".as_bytes(),
            &blocks,
        );
        assert_eq!(
            "before [Pasted text #9, 4 lines] X".as_bytes(),
            result.text.as_slice()
        );
    }

    #[test]
    fn expanded_lengths_count_registered_backing_text_without_allocating() {
        let placeholder = "[Pasted text #1, 1 line]";
        let input = format!("ab{placeholder}cd");
        let mut blocks: Vec<PastedBlock> = Vec::new();
        blocks.push(PastedBlock {
            id: 1,
            text: b"expanded".to_vec(),
            line_count: 1,
            span: Span {
                raw_start: "ab".len(),
                raw_end: "ab".len() + placeholder.len(),
            },
        });

        assert_eq!(
            Some("abexpandedcd".len()),
            expanded_len(input.as_bytes(), &blocks)
        );
        assert_eq!(
            Some("expanded".len()),
            expanded_range_len(
                input.as_bytes(),
                &blocks,
                "ab".len(),
                "ab".len() + placeholder.len(),
            )
        );
    }

    #[test]
    fn expand_ignores_typed_lookalikes_with_a_registered_id() {
        let registered = "[Pasted text #1, 1 line]";
        let input = format!("{registered} typed [Pasted text #1, 999 lines]");
        let mut blocks: Vec<PastedBlock> = Vec::new();
        blocks.push(PastedBlock {
            id: 1,
            text: b"original".to_vec(),
            line_count: 1,
            span: Span {
                raw_start: 0,
                raw_end: registered.len(),
            },
        });

        let result = expand(input.as_bytes(), &blocks);
        assert_eq!(
            "original typed [Pasted text #1, 999 lines]".as_bytes(),
            result.text.as_slice()
        );
    }
}
