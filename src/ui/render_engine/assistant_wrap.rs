#![allow(non_snake_case, non_upper_case_globals)]
use super::build_checkpoint::{self, BuildCheckpoint, InputPending};
use crate::core::shared::display_width;
use crate::ui::assistant::pacer::SgrState;

/// A pathological combining-mark run gets a physical row boundary at this
/// byte count. The source bytes remain exact while every retained row stays
/// bounded for compact and full-view projection.
pub const literal_command_zero_width_row_byte_limit: usize = 256;

#[derive(Clone, Copy)]
struct WordBreak<'a> {
    start: usize,
    end: usize,
    sgr: SgrState,
    hyperlink: Option<&'a [u8]>,
    has_preceding_word: bool,
}

/// The last two space runs on the current row, kept as reflow points for
/// overflow wrapping and orphan avoidance.
#[derive(Clone, Copy, Default)]
struct WordBreaks<'a> {
    last: Option<WordBreak<'a>>,
    previous: Option<WordBreak<'a>>,
}

impl<'a> WordBreaks<'a> {
    /// Extends `last` when adjacent; otherwise demotes it to `previous`.
    fn record(&mut self, word_break: WordBreak<'a>) {
        if let Some(last) = &mut self.last {
            if last.end == word_break.start {
                last.end = word_break.end;
                return;
            }
            self.previous = Some(*last);
        }
        self.last = Some(word_break);
    }
}

#[derive(Clone, Copy)]
enum Osc8Update<'a> {
    Open(&'a [u8]),
    Close,
}

#[derive(Clone, Copy)]
enum AssistantContinuation {
    ListIndent(usize),
    Blockquote { indent: usize, depth: usize },
}

/// Reflow assistant text to fit within `cols` while preserving inline
/// SGR attributes and OSC 8 links across wrap boundaries. Paragraphs wrap at whitespace
/// and move a preceding word down when that avoids a single-word tail.
/// Pre-existing hard newlines in `text` are honoured. The returned slice is
/// owned by the caller.
pub fn wrapAssistantText(text: &[u8], cols: u16) -> Vec<u8> {
    wrapAssistantTextWithBaseGutter(text, cols, 0, false, false, None, None)
        .expect("null checkpoint cannot be input-pending")
}

#[allow(clippy::too_many_arguments)]
fn wrapAssistantTextWithBaseGutter<'a>(
    text: &'a [u8],
    cols: u16,
    base_gutter: u16,
    replace_wide_runes_at_one_content_col: bool,
    literal_command: bool,
    mut checkpoint: Option<&mut BuildCheckpoint>,
    mut finalized_prefix_bytes: Option<&mut usize>,
) -> Result<Vec<u8>, InputPending> {
    let mut out: Vec<u8> = Vec::new();
    let mut finalized_source_end: usize = 0;
    if let Some(bytes) = finalized_prefix_bytes.as_deref_mut() {
        *bytes = 0;
        if let Some(newline) = text.iter().rposition(|&b| b == b'\n') {
            finalized_source_end = newline + 1;
        }
    }

    if cols == 0 {
        return Ok(out);
    }
    if text.is_empty() {
        if literal_command {
            appendAssistantRowStart(&mut out, base_gutter, SgrState::default(), None, true);
        }
        return Ok(out);
    }

    let mut sgr: SgrState = SgrState::default();
    let mut hyperlink: Option<&[u8]> = None;
    let content_cols = cols.saturating_sub(base_gutter);
    let mut col: u16 = base_gutter + 1;
    let mut continuation = if literal_command {
        None
    } else {
        assistantContinuation(text, 0)
    };
    let mut word_breaks: WordBreaks = WordBreaks::default();
    let mut has_word_on_row = false;
    let mut row_has_content = false;
    let mut zero_width_run_bytes: usize = 0;
    let mut i: usize = 0;
    while i < text.len() {
        build_checkpoint::tick(checkpoint.as_deref_mut())?;
        if !row_has_content && !literal_command {
            if let Some(marker_end) = infeasibleMarkerEnd(text, i, base_gutter, cols) {
                i = marker_end;
                continue;
            }
        }

        let ch = text[i];

        if ch == 0x1b {
            let end = display_width::ansi_sequence_end(text, i);
            if end <= i {
                // Preserve an incomplete trailing escape without stalling the loop.
                out.extend_from_slice(&text[i..]);
                break;
            }
            let seq = &text[i..end];
            sgr.apply(seq);
            if let Some(update) = osc8Update(seq) {
                match update {
                    Osc8Update::Open(open) => hyperlink = Some(open),
                    Osc8Update::Close => hyperlink = None,
                }
            }
            if base_gutter == 0 || row_has_content {
                out.extend_from_slice(seq);
            }
            i = end;
            continue;
        }

        if ch == b'\r' || ch == b'\n' {
            if literal_command && !row_has_content {
                appendAssistantRowStart(&mut out, base_gutter, sgr, hyperlink, true);
            }
            if base_gutter > 0 && row_has_content {
                closeAssistantRowStyles(&mut out, sgr, hyperlink);
            }
            out.push(ch);
            col = base_gutter + 1;
            i += 1;
            if let Some(bytes) = finalized_prefix_bytes.as_deref_mut() {
                if i == finalized_source_end {
                    *bytes = out.len();
                }
            }
            continuation = if literal_command {
                None
            } else {
                assistantContinuation(text, i)
            };
            word_breaks = WordBreaks::default();
            has_word_on_row = false;
            row_has_content = false;
            zero_width_run_bytes = 0;
            continue;
        }
        if ch == b'\t' {
            if base_gutter > 0 && !row_has_content {
                appendAssistantRowStart(&mut out, base_gutter, sgr, hyperlink, literal_command);
                row_has_content = true;
            }
            let next_col = display_width::next_tab_stop_column(col, cols);
            append_ntimes(&mut out, b' ', (next_col.saturating_sub(col)) as usize);
            col = col.max(next_col);
            i += 1;
            zero_width_run_bytes = 0;
            continue;
        }
        if ch < 32 {
            if base_gutter == 0 || row_has_content {
                out.push(ch);
            }
            i += 1;
            continue;
        }

        let unit = display_width::display_unit_at(text, i);
        let w_usize = unit.cell_width;
        if w_usize == 0 {
            if literal_command
                && zero_width_run_bytes.saturating_add(unit.byte_len)
                    > literal_command_zero_width_row_byte_limit
            {
                col = appendAssistantContinuation(
                    &mut out,
                    sgr,
                    hyperlink,
                    continuation,
                    cols,
                    base_gutter,
                    1,
                    true,
                );
                word_breaks = WordBreaks::default();
                has_word_on_row = false;
                row_has_content = true;
                zero_width_run_bytes = 0;
            }
            if base_gutter > 0 && !row_has_content {
                appendAssistantRowStart(&mut out, base_gutter, sgr, hyperlink, literal_command);
                row_has_content = true;
            }
            out.extend_from_slice(&text[i..i + unit.byte_len]);
            if literal_command {
                zero_width_run_bytes += unit.byte_len;
            }
            i += unit.byte_len;
            continue;
        } else if literal_command {
            zero_width_run_bytes = 0;
        }
        let replace_wide_unit =
            replace_wide_runes_at_one_content_col && content_cols == 1 && w_usize == 2;
        let w: u16 = if replace_wide_unit {
            1
        } else {
            w_usize as u16
        };

        if ch == b' ' && display_width::should_wrap_at(col, w, cols) {
            let mut moved_for_orphan = false;
            if let Some(word_break) = word_breaks.last {
                let next_word_start = i + unit.byte_len;
                if remainingWordWidth(text, next_word_start) > 0
                    && isFinalWordOnLine(text, next_word_start)
                    && wordBreakFits(
                        &out[word_break.end..],
                        text,
                        next_word_start,
                        continuation,
                        cols,
                        base_gutter,
                    )
                {
                    let next_width = firstVisibleWidth(&text[next_word_start..]).unwrap_or(w);
                    if let Some(reflowed_col) = reflowAtWordBreak(
                        &mut out,
                        word_break,
                        continuation,
                        cols,
                        base_gutter,
                        next_width,
                        literal_command,
                    ) {
                        col = reflowed_col;
                        word_breaks = WordBreaks::default();
                        has_word_on_row = true;
                        row_has_content = true;
                        moved_for_orphan = true;
                    }
                }
            }
            if !moved_for_orphan {
                col = appendAssistantContinuation(
                    &mut out,
                    sgr,
                    hyperlink,
                    continuation,
                    cols,
                    base_gutter,
                    w,
                    literal_command,
                );
                word_breaks = WordBreaks::default();
                has_word_on_row = false;
                row_has_content = true;
                i += unit.byte_len;
                continue;
            }
        }

        if display_width::should_wrap_at(col, w, cols) {
            if let Some(word_break) = word_breaks.last {
                let mut break_to_use = word_break;
                if let Some(previous) = word_breaks.previous {
                    if previous.has_preceding_word
                        && isFinalWordOnLine(text, i)
                        && wordBreakFits(
                            &out[previous.end..],
                            text,
                            i,
                            continuation,
                            cols,
                            base_gutter,
                        )
                    {
                        break_to_use = previous;
                    }
                }

                if let Some(reflowed_col) = reflowAtWordBreak(
                    &mut out,
                    break_to_use,
                    continuation,
                    cols,
                    base_gutter,
                    w,
                    literal_command,
                ) {
                    col = reflowed_col;
                    word_breaks = WordBreaks::default();
                    row_has_content = true;
                }
            }
        }

        if display_width::should_wrap_at(col, w, cols) {
            col = appendAssistantContinuation(
                &mut out,
                sgr,
                hyperlink,
                continuation,
                cols,
                base_gutter,
                w,
                literal_command,
            );
            word_breaks = WordBreaks::default();
            has_word_on_row = false;
            row_has_content = true;
        }

        if base_gutter > 0 && !row_has_content {
            appendAssistantRowStart(&mut out, base_gutter, sgr, hyperlink, literal_command);
            row_has_content = true;
        }

        let space_start = out.len();
        if replace_wide_unit {
            out.push(b'?');
        } else {
            out.extend_from_slice(&text[i..i + unit.byte_len]);
        }
        col += w;
        if ch == b' '
            && has_word_on_row
            && !isContinuationMarkerSeparator(col - 1, continuation, base_gutter)
        {
            word_breaks.record(WordBreak {
                start: space_start,
                end: out.len(),
                sgr,
                hyperlink,
                has_preceding_word: word_breaks.last.is_some(),
            });
        } else if ch != b' ' {
            has_word_on_row = true;
        }
        i += unit.byte_len;
    }

    if literal_command && !row_has_content {
        appendAssistantRowStart(&mut out, base_gutter, sgr, hyperlink, true);
    }

    Ok(out)
}

fn closeAssistantRowStyles(out: &mut Vec<u8>, sgr: SgrState, hyperlink: Option<&[u8]>) {
    if sgr.isActive() {
        out.extend_from_slice(b"\x1b[0m");
    }
    if hyperlink.is_some() {
        out.extend_from_slice(b"\x1b]8;;\x1b\\");
    }
}

fn appendAssistantRowStart(
    out: &mut Vec<u8>,
    base_gutter: u16,
    sgr: SgrState,
    hyperlink: Option<&[u8]>,
    literal_command: bool,
) {
    if literal_command && base_gutter >= 2 {
        out.extend_from_slice("│ ".as_bytes());
        append_ntimes(out, b' ', (base_gutter - 2) as usize);
    } else {
        append_ntimes(out, b' ', base_gutter as usize);
    }
    if sgr.isActive() {
        let mut reopen = [0u8; 64];
        let n = sgr.writeOpens(&mut reopen);
        out.extend_from_slice(&reopen[..n]);
    }
    if let Some(open) = hyperlink {
        out.extend_from_slice(open);
    }
}

pub fn gutterWidth(cols: u16) -> u16 {
    if cols >= 3 {
        2
    } else if cols == 2 {
        1
    } else {
        0
    }
}

pub fn prefixStructuralRows(bytes: &[u8], gutter: u16) -> Vec<u8> {
    if gutter == 0 || bytes.is_empty() {
        return bytes.to_vec();
    }

    let mut out: Vec<u8> = Vec::new();
    let mut line_start: usize = 0;
    while line_start < bytes.len() {
        let newline = bytes[line_start..]
            .iter()
            .position(|&b| b == b'\n')
            .map(|p| line_start + p);
        let line_end = newline.unwrap_or(bytes.len());
        if firstVisibleWidth(&bytes[line_start..line_end]).is_some() {
            append_ntimes(&mut out, b' ', gutter as usize);
        }
        out.extend_from_slice(&bytes[line_start..line_end]);
        if newline.is_some() {
            out.push(b'\n');
        }
        line_start = line_end + 1;
    }
    out
}

pub fn wrapTranscriptAssistantTextInterruptible(
    text: &[u8],
    cols: u16,
    checkpoint: Option<&mut BuildCheckpoint>,
) -> Result<Vec<u8>, InputPending> {
    wrapAssistantTextWithBaseGutter(
        text,
        cols,
        gutterWidth(cols),
        true,
        false,
        checkpoint,
        None,
    )
}

/// `bytes` is owned by the caller. `finalized_prefix_bytes` ends after the
/// last source hard newline and excludes soft wraps from the unfinished line.
pub struct TranscriptAssistantWrap {
    pub bytes: Vec<u8>,
    pub finalized_prefix_bytes: usize,
}

pub fn wrapTranscriptAssistantTextWithFinalityInterruptible(
    text: &[u8],
    cols: u16,
    checkpoint: Option<&mut BuildCheckpoint>,
) -> Result<TranscriptAssistantWrap, InputPending> {
    let mut finalized_prefix_bytes: usize = 0;
    let bytes = wrapAssistantTextWithBaseGutter(
        text,
        cols,
        gutterWidth(cols),
        true,
        false,
        checkpoint,
        Some(&mut finalized_prefix_bytes),
    )?;
    Ok(TranscriptAssistantWrap {
        bytes,
        finalized_prefix_bytes,
    })
}

/// Reflow canonical command output as literal terminal text. Every physical
/// row owns its gutter; Markdown continuation markers are deliberately not
/// interpreted, and tabs advance to the terminal's absolute eight-column
/// stops. The returned slice is owned by the caller.
pub fn wrapLiteralCommandOutput(text: &[u8], cols: u16) -> Vec<u8> {
    if cols == 0 {
        return Vec::new();
    }
    let gutter = if cols >= 3 { 2u16 } else { 0u16 };
    wrapAssistantTextWithBaseGutter(text, cols, gutter, true, true, None, None)
        .expect("null checkpoint cannot be input-pending")
}

/// Reflows literal tool detail with the three-cell rail used by Ctrl-O.
pub fn wrapLiteralToolOutput(text: &[u8], cols: u16) -> Vec<u8> {
    wrapLiteralToolOutputInterruptible(text, cols, None).expect("null checkpoint cannot be input-pending")
}

pub fn wrapLiteralToolOutputInterruptible(
    text: &[u8],
    cols: u16,
    checkpoint: Option<&mut BuildCheckpoint>,
) -> Result<Vec<u8>, InputPending> {
    if cols == 0 {
        return Ok(Vec::new());
    }
    let gutter = if cols >= 4 {
        3u16
    } else if cols >= 3 {
        2u16
    } else {
        0u16
    };
    wrapAssistantTextWithBaseGutter(text, cols, gutter, true, true, checkpoint, None)
}

pub struct LiteralCommandOutputPrefix {
    pub bytes: Vec<u8>,
    pub has_more: bool,
}

impl LiteralCommandOutputPrefix {
    pub fn deinit(&mut self) {
        self.bytes.clear();
    }
}

/// Bounded compact projection. It retains three rows of lookahead so the
/// assistant wrapper's word/orphan decision for the admitted rows is stable,
/// but never scans or allocates in proportion to an oversized record.
pub fn wrapLiteralCommandOutputPrefix(
    text: &[u8],
    cols: u16,
    row_limit: usize,
) -> LiteralCommandOutputPrefix {
    if row_limit == 0 || cols == 0 {
        return LiteralCommandOutputPrefix {
            bytes: Vec::new(),
            has_more: !text.is_empty(),
        };
    }

    let lookahead_rows = row_limit.saturating_add(3);
    let source_end = literalLookaheadEnd(text, cols, lookahead_rows);
    let wrapped = wrapLiteralCommandOutput(&text[..source_end], cols);
    let prefix = literalPhysicalRowPrefix(&wrapped, row_limit);
    LiteralCommandOutputPrefix {
        bytes: prefix.to_vec(),
        has_more: source_end < text.len() || literalHardLineCount(&wrapped) > row_limit,
    }
}

fn literalLookaheadEnd(text: &[u8], cols: u16, row_limit: usize) -> usize {
    let gutter: u16 = if cols >= 3 { 2 } else { 0 };
    let mut col: u16 = gutter + 1;
    let mut rows: usize = 1;
    let mut zero_width_run_bytes: usize = 0;
    let mut index: usize = 0;
    while index < text.len() && rows <= row_limit {
        let byte = text[index];
        if byte == b'\r' || byte == b'\n' {
            rows += 1;
            col = gutter + 1;
            zero_width_run_bytes = 0;
            index += 1;
            continue;
        }
        if byte == b'\t' {
            col = display_width::next_tab_stop_column(col, cols);
            zero_width_run_bytes = 0;
            index += 1;
            continue;
        }
        let unit = display_width::display_unit_at(text, index);
        let width_usize = unit.cell_width;
        if width_usize == 0 {
            if zero_width_run_bytes.saturating_add(unit.byte_len)
                > literal_command_zero_width_row_byte_limit
            {
                rows += 1;
                col = gutter + 1;
                zero_width_run_bytes = 0;
                if rows > row_limit {
                    break;
                }
            }
            zero_width_run_bytes += unit.byte_len;
            index += unit.byte_len;
            continue;
        }
        zero_width_run_bytes = 0;
        let width: u16 = width_usize as u16;
        if display_width::should_wrap_at(col, width, cols) {
            rows += 1;
            col = gutter + 1;
            if rows > row_limit {
                break;
            }
        }
        col = col.saturating_add(width);
        index += unit.byte_len;
    }
    index
}

fn literalHardLineCount(bytes: &[u8]) -> usize {
    if bytes.is_empty() {
        return 0;
    }
    bytes.iter().filter(|&&b| b == b'\n').count() + 1
}

fn literalPhysicalRowPrefix(bytes: &[u8], row_count: usize) -> &[u8] {
    if row_count == 0 {
        return &bytes[..0];
    }
    let mut rows: usize = 1;
    for (index, &byte) in bytes.iter().enumerate() {
        if byte != b'\n' {
            continue;
        }
        if rows == row_count {
            return &bytes[..index];
        }
        rows += 1;
    }
    bytes
}

#[allow(clippy::too_many_arguments)]
fn appendAssistantContinuation(
    out: &mut Vec<u8>,
    sgr: SgrState,
    hyperlink: Option<&[u8]>,
    continuation: Option<AssistantContinuation>,
    cols: u16,
    base_gutter: u16,
    next_width: u16,
    literal_command: bool,
) -> u16 {
    closeAssistantRowStyles(out, sgr, hyperlink);
    out.push(b'\n');

    if literal_command && base_gutter >= 2 {
        out.extend_from_slice("│ ".as_bytes());
        append_ntimes(out, b' ', (base_gutter - 2) as usize);
    } else {
        append_ntimes(out, b' ', base_gutter as usize);
    }
    let mut col: u16 = base_gutter + 1;
    if let Some(prefix) = continuation {
        let indent = continuationWidth(prefix);
        let width: usize = cols as usize;
        let total_indent = base_gutter as usize + indent;
        if total_indent < width && (next_width as usize) <= width - total_indent {
            match prefix {
                AssistantContinuation::ListIndent(_) => append_ntimes(out, b' ', indent),
                AssistantContinuation::Blockquote {
                    indent: bq_indent,
                    depth,
                } => {
                    append_ntimes(out, b' ', bq_indent);
                    for _ in 0..depth {
                        out.extend_from_slice(b"\x1b[2m\xe2\x94\x82 \x1b[22m");
                    }
                }
            }
            col = (total_indent + 1) as u16;
        }
    }
    if sgr.isActive() {
        let mut reopen = [0u8; 64];
        let n = sgr.writeOpens(&mut reopen);
        out.extend_from_slice(&reopen[..n]);
    }
    if let Some(open) = hyperlink {
        out.extend_from_slice(open);
    }
    col
}

#[allow(clippy::too_many_arguments)]
fn reflowAtWordBreak<'a>(
    out: &mut Vec<u8>,
    word_break: WordBreak<'a>,
    continuation: Option<AssistantContinuation>,
    cols: u16,
    base_gutter: u16,
    next_width: u16,
    literal_command: bool,
) -> Option<u16> {
    let suffix = &out[word_break.end..];
    let suffix_width = display_width::visible_width_ignoring_ansi(suffix);
    let first_width = firstVisibleWidth(suffix).unwrap_or(next_width);
    let local_indent = continuation.map_or(0, continuationWidth);
    let indent = base_gutter as usize + local_indent;
    if indent >= cols as usize
        || suffix_width + next_width as usize > cols as usize - indent
    {
        return None;
    }

    let mut replacement: Vec<u8> = Vec::new();
    let col = appendAssistantContinuation(
        &mut replacement,
        word_break.sgr,
        word_break.hyperlink,
        continuation,
        cols,
        base_gutter,
        first_width,
        literal_command,
    );
    out.splice(word_break.start..word_break.end, replacement);
    Some(col + suffix_width as u16)
}

fn osc8Update<'a>(seq: &'a [u8]) -> Option<Osc8Update<'a>> {
    if !seq.starts_with(b"\x1b]8;") {
        return None;
    }
    let terminator_len = if !seq.is_empty() && seq[seq.len() - 1] == 0x07 {
        1
    } else if seq.len() >= 2 && seq[seq.len() - 2] == 0x1b && seq[seq.len() - 1] == b'\\' {
        2
    } else {
        return None;
    };
    let payload = &seq[4..seq.len() - terminator_len];
    let separator = payload.iter().position(|&b| b == b';')?;
    if separator + 1 == payload.len() {
        Some(Osc8Update::Close)
    } else {
        Some(Osc8Update::Open(seq))
    }
}

fn firstVisibleWidth(text: &[u8]) -> Option<u16> {
    let mut i: usize = 0;
    while i < text.len() {
        if text[i] == 0x1b {
            let end = display_width::ansi_sequence_end(text, i);
            if end <= i {
                return None;
            }
            i = end;
            continue;
        }
        if text[i] < 32 {
            return None;
        }
        let unit = display_width::display_unit_at(text, i);
        if unit.cell_width > 0 {
            return Some(unit.cell_width as u16);
        }
        i += unit.byte_len;
    }
    None
}

/// Returns the end of a row-leading footnote or definition marker that cannot
/// fit within `cols` together with the next visible rune.
fn infeasibleMarkerEnd(text: &[u8], start: usize, base_gutter: u16, cols: u16) -> Option<usize> {
    let marker: MarkerPrefix = if let Some(prefix) = footnotePrefix(text, start) {
        prefix
    } else if let Some(end) = definitionPrefixEnd(text, start) {
        MarkerPrefix { end, indent: 2 }
    } else {
        return None;
    };
    let next_width = firstVisibleWidth(&text[marker.end..]).unwrap_or(1);
    if base_gutter as usize + marker.indent + next_width as usize <= cols as usize {
        return None;
    }
    Some(marker.end)
}

fn isContinuationMarkerSeparator(
    col_before_space: u16,
    continuation: Option<AssistantContinuation>,
    base_gutter: u16,
) -> bool {
    let Some(prefix) = continuation else { return false };
    col_before_space as usize == base_gutter as usize + continuationWidth(prefix)
}

fn wordBreakFits(
    suffix: &[u8],
    text: &[u8],
    word_start: usize,
    continuation: Option<AssistantContinuation>,
    cols: u16,
    base_gutter: u16,
) -> bool {
    let indent = base_gutter as usize + continuation.map_or(0, continuationWidth);
    if indent >= cols as usize {
        return false;
    }
    let available = cols as usize - indent;
    display_width::visible_width_ignoring_ansi(suffix) + remainingWordWidth(text, word_start)
        <= available
}

fn remainingWordWidth(text: &[u8], start: usize) -> usize {
    let mut width: usize = 0;
    let mut i = start;
    while i < text.len() {
        if text[i] == 0x1b {
            let end = display_width::ansi_sequence_end(text, i);
            if end <= i {
                break;
            }
            i = end;
            continue;
        }
        if text[i] == b' ' || text[i] == b'\r' || text[i] == b'\n' {
            break;
        }
        let unit = display_width::display_unit_at(text, i);
        width += unit.cell_width;
        i += unit.byte_len;
    }
    width
}

fn isFinalWordOnLine(text: &[u8], start: usize) -> bool {
    let mut i = start;
    let mut passed_current_word = false;
    while i < text.len() {
        if text[i] == 0x1b {
            let end = display_width::ansi_sequence_end(text, i);
            if end <= i {
                return true;
            }
            i = end;
            continue;
        }
        if text[i] == b'\r' || text[i] == b'\n' {
            return true;
        }
        if text[i] == b' ' {
            passed_current_word = true;
            i += 1;
            continue;
        }
        if passed_current_word {
            return false;
        }
        let rune = display_width::decode_next_rune(text, i);
        i += rune.len;
    }
    true
}

fn continuationWidth(continuation: AssistantContinuation) -> usize {
    match continuation {
        AssistantContinuation::ListIndent(indent) => indent,
        AssistantContinuation::Blockquote { indent, depth } => indent + 2 * depth,
    }
}

/// Recognizes MarkdownProcessor's list, definition, and blockquote prefixes, including
/// pacer-inserted dim reassertions between visible marker characters.
fn assistantContinuation(text: &[u8], line_start: usize) -> Option<AssistantContinuation> {
    if let Some(indent) = listContinuationIndentWidth(text, line_start) {
        return Some(AssistantContinuation::ListIndent(indent));
    }
    if definitionPrefixEnd(text, line_start).is_some() {
        return Some(AssistantContinuation::ListIndent(2));
    }
    if let Some(prefix) = footnotePrefix(text, line_start) {
        return Some(AssistantContinuation::ListIndent(prefix.indent));
    }
    if let Some(indent) = footnoteContinuationIndent(text, line_start) {
        return Some(AssistantContinuation::ListIndent(indent));
    }

    let mut i = line_start;
    while i < text.len() && text[i] == b' ' {
        i += 1;
    }
    let indent = i - line_start;
    let mut depth: usize = 0;
    while let Some(end) = blockquoteMarkerEnd(text, i) {
        depth += 1;
        i = end;
    }
    if depth == 0 {
        return None;
    }
    Some(AssistantContinuation::Blockquote { indent, depth })
}

fn definitionPrefixEnd(text: &[u8], start: usize) -> Option<usize> {
    let dim_open = b"\x1b[2m";
    let dim_close = b"\x1b[22m";

    let mut i = start;
    if !starts_at(text, i, dim_open) {
        return None;
    }
    i += dim_open.len();
    i = skipPacerDimReassertions(text, i);
    if i >= text.len() || text[i] != b' ' {
        return None;
    }
    i += 1;
    i = skipPacerDimReassertions(text, i);
    if i >= text.len() || text[i] != b' ' {
        return None;
    }
    i += 1;
    i = skipPacerDimReassertions(text, i);
    if !starts_at(text, i, dim_close) {
        return None;
    }
    Some(i + dim_close.len())
}

#[derive(Clone, Copy)]
struct MarkerPrefix {
    end: usize,
    indent: usize,
}

fn footnotePrefix(text: &[u8], start: usize) -> Option<MarkerPrefix> {
    let mut i = footnoteDimOpenEnd(text, start)?;
    i = skipPacerDimReassertions(text, i);
    if i >= text.len() || text[i] != b'[' {
        return None;
    }
    i += 1;
    i = skipPacerDimReassertions(text, i);

    let mut digits: usize = 0;
    while i < text.len() && (b'0'..=b'9').contains(&text[i]) {
        digits += 1;
        i += 1;
        i = skipPacerDimReassertions(text, i);
    }
    if digits == 0 || i >= text.len() || text[i] != b']' {
        return None;
    }
    i += 1;
    i = skipPacerDimReassertions(text, i);
    if i >= text.len() || text[i] != b' ' {
        return None;
    }
    i += 1;
    let close_end = footnotePrefixBoundaryEnd(text, i)?;
    Some(MarkerPrefix {
        end: close_end,
        indent: digits + 3,
    })
}

fn footnoteContinuationIndent(text: &[u8], start: usize) -> Option<usize> {
    let mut i = footnoteDimOpenEnd(text, start)?;

    let mut indent: usize = 0;
    loop {
        i = skipPacerDimReassertions(text, i);
        if i >= text.len() || text[i] != b' ' {
            break;
        }
        indent += 1;
        i += 1;
    }
    if indent < 4 {
        return None;
    }
    footnotePrefixBoundaryEnd(text, i)?;
    Some(indent)
}

fn footnotePrefixBoundaryEnd(text: &[u8], start: usize) -> Option<usize> {
    if let Some(end) = dimCloseEnd(text, start) {
        return Some(end);
    }
    let reassert = b"\x1b[0m\x1b[2m";
    if starts_at(text, start, reassert) {
        return Some(start + reassert.len());
    }
    None
}

fn footnoteDimOpenEnd(text: &[u8], start: usize) -> Option<usize> {
    let reset = b"\x1b[0m";
    let dim_open = b"\x1b[2m";

    let mut i = start;
    if starts_at(text, i, reset) {
        i += reset.len();
    }
    if !starts_at(text, i, dim_open) {
        return None;
    }
    while starts_at(text, i, dim_open) {
        i += dim_open.len();
    }
    Some(i)
}

fn dimCloseEnd(text: &[u8], start: usize) -> Option<usize> {
    if starts_at(text, start, b"\x1b[22m") {
        return Some(start + b"\x1b[22m".len());
    }
    if starts_at(text, start, b"\x1b[0m") {
        return Some(start + b"\x1b[0m".len());
    }
    None
}

fn blockquoteMarkerEnd(text: &[u8], start: usize) -> Option<usize> {
    let dim_open = b"\x1b[2m";
    let dim_close = b"\x1b[22m";
    let vertical_rule = b"\xe2\x94\x82";

    let mut i = start;
    if !starts_at(text, i, dim_open) {
        return None;
    }
    i += dim_open.len();
    i = skipPacerDimReassertions(text, i);
    if !starts_at(text, i, vertical_rule) {
        return None;
    }
    i += vertical_rule.len();
    i = skipPacerDimReassertions(text, i);
    if i >= text.len() || text[i] != b' ' {
        return None;
    }
    i += 1;
    i = skipPacerDimReassertions(text, i);
    if !starts_at(text, i, dim_close) {
        return None;
    }
    Some(i + dim_close.len())
}

/// Recognizes MarkdownProcessor's list prefix and pacer dim reassertions.
fn listContinuationIndentWidth(text: &[u8], line_start: usize) -> Option<usize> {
    let dim_open = b"\x1b[2m";
    let dim_close = b"\x1b[22m";
    let bullet = b"\xe2\x80\xa2";

    let mut i = line_start;
    while i < text.len() && text[i] == b' ' {
        i += 1;
    }
    let indent = i - line_start;
    if let Some(task_width) = taskMarkerWidth(text, i) {
        return Some(indent + task_width);
    }
    if !starts_at(text, i, dim_open) {
        return None;
    }
    i += dim_open.len();
    i = skipPacerDimReassertions(text, i);

    if let Some(task_width) = dimTaskMarkerWidth(text, i) {
        return Some(indent + task_width);
    }

    if starts_at(text, i, bullet) {
        i += bullet.len();
        i = skipPacerDimReassertions(text, i);
        if i >= text.len() || text[i] != b' ' {
            return None;
        }
        i += 1;
        i = skipPacerDimReassertions(text, i);
        if !starts_at(text, i, dim_close) {
            return None;
        }
        return Some(indent + 2);
    }

    let mut marker_width: usize = 0;
    while i < text.len() && (b'0'..=b'9').contains(&text[i]) {
        i += 1;
        marker_width += 1;
        i = skipPacerDimReassertions(text, i);
    }
    if marker_width == 0 || i >= text.len() || text[i] != b'.' {
        return None;
    }
    i += 1;
    marker_width += 1;
    i = skipPacerDimReassertions(text, i);
    if !starts_at(text, i, dim_close) {
        return None;
    }
    i += dim_close.len();
    if i >= text.len() || text[i] != b' ' {
        return None;
    }
    i += 1;

    if let Some(task_width) = taskMarkerWidth(text, i) {
        return Some(indent + marker_width + 1 + task_width);
    }

    Some(indent + marker_width + 1)
}

fn dimTaskMarkerWidth(text: &[u8], start: usize) -> Option<usize> {
    let dim_close = b"\x1b[22m";
    let pending = b"\xe2\x98\x90";

    let mut i = skipPacerDimReassertions(text, start);
    if !starts_at(text, i, pending) {
        return None;
    }
    i += pending.len();
    i = skipPacerDimReassertions(text, i);

    let mut width: usize = 1;
    if i < text.len() && text[i] == b' ' {
        i += 1;
        width += 1;
        i = skipPacerDimReassertions(text, i);
    }
    if !starts_at(text, i, dim_close) {
        return None;
    }
    Some(width)
}

fn taskMarkerWidth(text: &[u8], start: usize) -> Option<usize> {
    let dim_open = b"\x1b[2m";
    let completed_opens: &[&[u8]] = &[b"\x1b[38;5;252m", b"\x1b[38;5;238m"];
    let completed_close = b"\x1b[39m";
    let completed = b"\xe2\x9c\x93";

    if starts_at(text, start, dim_open) {
        return dimTaskMarkerWidth(text, start + dim_open.len());
    }

    let mut i = start;
    let mut open_len: Option<usize> = None;
    for open in completed_opens {
        if starts_at(text, i, open) {
            open_len = Some(open.len());
            break;
        }
    }
    let open_len = open_len?;
    i += open_len;
    if !starts_at(text, i, completed) {
        return None;
    }
    i += completed.len();
    if !starts_at(text, i, completed_close) {
        return None;
    }
    i += completed_close.len();

    let mut width: usize = 1;
    if i < text.len() && text[i] == b' ' {
        width += 1;
    }
    Some(width)
}

fn skipPacerDimReassertions(text: &[u8], start: usize) -> usize {
    let reassert = b"\x1b[0m\x1b[2m";
    let mut i = start;
    while starts_at(text, i, reassert) {
        i += reassert.len();
    }
    i
}

fn append_ntimes(out: &mut Vec<u8>, byte: u8, n: usize) {
    out.extend(std::iter::repeat(byte).take(n));
}

fn starts_at(haystack: &[u8], start: usize, needle: &[u8]) -> bool {
    haystack.len() >= start + needle.len() && &haystack[start..start + needle.len()] == needle
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        if needle.is_empty() {
            return true;
        }
        haystack.windows(needle.len()).any(|w| w == needle)
    }

    #[test]
    fn wraps_plain_ascii_at_cols() {
        let out = wrapAssistantText(b"abcdefghij", 5);
        assert_eq!(b"abcde\nfghij", out.as_slice());
    }

    #[test]
    fn keeps_paragraph_words_together_and_avoids_a_final_orphan() {
        let out = wrapAssistantText(b"alpha beta gamma delta", 16);
        assert_eq!(b"alpha beta\ngamma delta", out.as_slice());
    }

    #[test]
    fn avoids_an_orphan_after_a_fitting_separator() {
        let out = wrapAssistantText(b"aaaa bbbb cccc dddd", 16);
        assert_eq!(b"aaaa bbbb\ncccc dddd", out.as_slice());
    }

    #[test]
    fn keeps_word_aware_list_continuations_aligned() {
        let input = "\x1b[2m• \x1b[22malpha beta gamma delta";
        let out = wrapAssistantText(input.as_bytes(), 18);
        assert_eq!(
            "\x1b[2m• \x1b[22malpha beta\n  gamma delta".as_bytes(),
            out.as_slice()
        );
    }

    #[test]
    fn repeats_every_styled_nested_blockquote_rule_on_word_aware_continuations() {
        let input = "\x1b[2m│ \x1b[22m\x1b[2m│ \x1b[22m\x1b[1malpha beta gamma delta\x1b[22m";
        let out = wrapAssistantText(input.as_bytes(), 18);
        assert_eq!(
            "\x1b[2m│ \x1b[22m\x1b[2m│ \x1b[22m\x1b[1malpha beta\x1b[0m\n\x1b[2m│ \x1b[22m\x1b[2m│ \x1b[22m\x1b[1mgamma delta\x1b[22m".as_bytes(),
            out.as_slice()
        );
    }

    #[test]
    fn repeats_paced_nested_blockquote_rules() {
        let paced_rule = "\x1b[2m│\x1b[0m\x1b[2m \x1b[0m\x1b[2m\x1b[22m";
        let input = format!("{}{}abcdefghijkl", paced_rule, paced_rule);
        let out = wrapAssistantText(input.as_bytes(), 9);
        assert_eq!(
            format!(
                "{}{}abcde\n\x1b[2m│ \x1b[22m\x1b[2m│ \x1b[22mfghij\n\x1b[2m│ \x1b[22m\x1b[2m│ \x1b[22mkl",
                paced_rule, paced_rule
            )
            .as_bytes(),
            out.as_slice()
        );
    }

    #[test]
    fn preserves_bold_sgr_across_wrap_boundary() {
        let input = "\x1b[1mabcdef\x1b[22m";
        let out = wrapAssistantText(input.as_bytes(), 3);
        assert_eq!("\x1b[1mabc\x1b[0m\n\x1b[1mdef\x1b[22m".as_bytes(), out.as_slice());
    }

    #[test]
    fn preserves_inline_code_foreground_across_wrap_boundary() {
        let cases: &[(&[u8], &[u8])] = &[
            (
                "\x1b[38;5;245mabcdef\x1b[39m".as_bytes(),
                "\x1b[38;5;245mabc\x1b[0m\n\x1b[38;5;245mdef\x1b[39m".as_bytes(),
            ),
            (
                "\x1b[38;5;247mabcdef\x1b[39m".as_bytes(),
                "\x1b[38;5;247mabc\x1b[0m\n\x1b[38;5;247mdef\x1b[39m".as_bytes(),
            ),
        ];

        for &(input, expected) in cases {
            let out = wrapAssistantText(input, 3);
            assert_eq!(expected, out.as_slice());
        }
    }

    #[test]
    fn preserves_heading_underline_across_wrap_boundary() {
        let input = "\x1b[1m\x1b[4mabcdef\x1b[24m\x1b[22m";
        let out = wrapAssistantText(input.as_bytes(), 3);
        assert_eq!(
            "\x1b[1m\x1b[4mabc\x1b[0m\n\x1b[1m\x1b[4mdef\x1b[24m\x1b[22m".as_bytes(),
            out.as_slice()
        );
    }

    #[test]
    fn preserves_an_underlined_osc8_link_across_a_wrap() {
        let input = "\x1b]8;id=fx-1;https://example.com\x1b\\\x1b[4mabcdef\x1b[24m\x1b]8;;\x1b\\ tail";
        let out = wrapAssistantText(input.as_bytes(), 3);
        assert_eq!(
            "\x1b]8;id=fx-1;https://example.com\x1b\\\x1b[4mabc\x1b[0m\x1b]8;;\x1b\\\n\x1b[4m\x1b]8;id=fx-1;https://example.com\x1b\\def\x1b[24m\x1b]8;;\x1b\\\ntai\nl".as_bytes(),
            out.as_slice()
        );
    }

    #[test]
    fn reopens_an_osc8_link_after_a_word_balanced_wrap() {
        let input = "\x1b]8;id=fx-1;https://example.com\x1b\\\x1b[4malpha beta\x1b[24m\x1b]8;;\x1b\\";
        let out = wrapAssistantText(input.as_bytes(), 6);
        assert_eq!(
            "\x1b]8;id=fx-1;https://example.com\x1b\\\x1b[4malpha\x1b[0m\x1b]8;;\x1b\\\n\x1b[4m\x1b]8;id=fx-1;https://example.com\x1b\\beta\x1b[24m\x1b]8;;\x1b\\".as_bytes(),
            out.as_slice()
        );
    }

    #[test]
    fn honours_hard_newlines_without_extra_reset() {
        let out = wrapAssistantText(b"ab\ncd", 10);
        assert_eq!(b"ab\ncd", out.as_slice());
    }

    #[test]
    fn cols_zero_returns_empty() {
        let out = wrapAssistantText(b"foo", 0);
        assert_eq!(b"", out.as_slice());
    }

    #[test]
    fn empty_text_returns_empty() {
        let out = wrapAssistantText(b"", 80);
        assert_eq!(b"", out.as_slice());
    }

    #[test]
    fn passes_non_sgr_ansi_through_without_counting_for_width() {
        let input = "a\x1b[Ab";
        let out = wrapAssistantText(input.as_bytes(), 2);
        assert_eq!("a\x1b[Ab".as_bytes(), out.as_slice());
    }

    #[test]
    fn indents_unordered_list_continuations() {
        let input = "\x1b[2m• \x1b[22mabcdefghijkl";
        let out = wrapAssistantText(input.as_bytes(), 7);
        assert_eq!(
            "\x1b[2m• \x1b[22mabcde\n  fghij\n  kl".as_bytes(),
            out.as_slice()
        );
    }

    #[test]
    fn recognizes_paced_list_markers() {
        let unordered = wrapAssistantText(
            "\x1b[2m•\x1b[0m\x1b[2m \x1b[0m\x1b[2m\x1b[22mabcdefghij".as_bytes(),
            7,
        );
        assert_eq!(
            "\x1b[2m•\x1b[0m\x1b[2m \x1b[0m\x1b[2m\x1b[22mabcde\n  fghij".as_bytes(),
            unordered.as_slice()
        );

        let ordered = wrapAssistantText(
            "\x1b[2m1\x1b[0m\x1b[2m2\x1b[0m\x1b[2m.\x1b[0m\x1b[2m\x1b[22m abcdefgh".as_bytes(),
            10,
        );
        assert_eq!(
            "\x1b[2m1\x1b[0m\x1b[2m2\x1b[0m\x1b[2m.\x1b[0m\x1b[2m\x1b[22m abcdef\n    gh".as_bytes(),
            ordered.as_slice()
        );

        let blockquote = wrapAssistantText(
            "\x1b[2m│\x1b[0m\x1b[2m \x1b[0m\x1b[2m\x1b[22mabcdefghij".as_bytes(),
            7,
        );
        assert_eq!(
            "\x1b[2m│\x1b[0m\x1b[2m \x1b[0m\x1b[2m\x1b[22mabcde\n\x1b[2m│ \x1b[22mfghij".as_bytes(),
            blockquote.as_slice()
        );
    }

    #[test]
    fn indents_ordered_and_nested_list_continuations() {
        let ordered = wrapAssistantText("  \x1b[2m12.\x1b[22m abcdefghij".as_bytes(), 10);
        assert_eq!(
            "  \x1b[2m12.\x1b[22m abcd\n      efgh\n      ij".as_bytes(),
            ordered.as_slice()
        );

        let nested = wrapAssistantText("  \x1b[2m• \x1b[22mabcdefgh".as_bytes(), 8);
        assert_eq!(
            "  \x1b[2m• \x1b[22mabcd\n    efgh".as_bytes(),
            nested.as_slice()
        );
    }

    #[test]
    fn indents_dim_definition_continuations() {
        let out = wrapAssistantText("\x1b[2m  \x1b[22malpha beta gamma delta".as_bytes(), 14);
        assert_eq!(
            "\x1b[2m  \x1b[22malpha beta\n  gamma delta".as_bytes(),
            out.as_slice()
        );
    }

    #[test]
    fn recognizes_paced_dim_definition_prefixes() {
        let out = wrapAssistantText(
            "\x1b[2m \x1b[0m\x1b[2m \x1b[0m\x1b[2m\x1b[22malpha beta gamma delta".as_bytes(),
            14,
        );
        assert_eq!(
            "\x1b[2m \x1b[0m\x1b[2m \x1b[0m\x1b[2m\x1b[22malpha beta\n  gamma delta".as_bytes(),
            out.as_slice()
        );
    }

    #[test]
    fn indents_dim_footnote_continuations() {
        let out = wrapAssistantText("\x1b[2m[1] \x1b[22mabcdefghijkl".as_bytes(), 8);
        assert_eq!(
            "\x1b[2m[1] \x1b[22mabcd\n    efgh\n    ijkl".as_bytes(),
            out.as_slice()
        );
    }

    #[test]
    fn recognizes_reset_closed_dim_footnote_prefixes() {
        let out = wrapAssistantText("\x1b[2m[1] \x1b[0mabcdefghijkl".as_bytes(), 8);
        assert_eq!(
            "\x1b[2m[1] \x1b[0mabcd\n    efgh\n    ijkl".as_bytes(),
            out.as_slice()
        );
    }

    #[test]
    fn recognizes_paced_dim_footnote_prefixes() {
        let out = wrapAssistantText(
            "\x1b[2m[1] \x1b[0m\x1b[2mabcdefghijkl\x1b[22m".as_bytes(),
            8,
        );
        assert_eq!(
            "\x1b[2m[1] \x1b[0m\x1b[2mabcd\x1b[0m\n    \x1b[2mefgh\x1b[0m\n    \x1b[2mijkl\x1b[22m".as_bytes(),
            out.as_slice()
        );
    }

    #[test]
    fn recognizes_a_leading_paced_footnote_reassertion() {
        let out = wrapAssistantText(
            "\x1b[0m\x1b[2m[2] \x1b[0mThis definition appears before its reference.".as_bytes(),
            32,
        );
        assert!(contains(&out, "\n    before its reference.".as_bytes()));
    }

    #[test]
    fn transcript_wrap_retains_reset_closed_footnote_indentation_after_prose() {
        let out = wrapTranscriptAssistantTextInterruptible(
            "Forward reference.\n\n\x1b[2m[2] \x1b[0mThis definition appears before its reference and ends with FOOTNOTE_DONE.".as_bytes(),
            32,
            None,
        )
        .unwrap();
        assert!(contains(&out, "\n      before its reference".as_bytes()));
    }

    #[test]
    fn transcript_wrap_retains_footnote_indentation_through_character_paced_markers() {
        let input = "Forward reference\n\n\x1b[2m[\x1b[0m\x1b[2m1\x1b[0m\x1b[2m]\x1b[0m\x1b[2m \x1b[0m\x1b[2m\x1b[22mThe deferred body wraps through a narrow terminal.";
        assert!(footnotePrefix(input.as_bytes(), "Forward reference\n\n".len()).is_some());
        assert!(assistantContinuation(input.as_bytes(), "Forward reference\n\n".len()).is_some());
        let out = wrapTranscriptAssistantTextInterruptible(input.as_bytes(), 32, None).unwrap();
        assert!(contains(&out, "\n      through a narrow terminal.".as_bytes()));
    }

    #[test]
    fn elides_infeasible_dim_footnote_markers_at_narrow_widths() {
        let ascii = "\x1b[2m[1] \x1b[22malphabet";
        let mut cols: u16 = 1;
        while cols <= 7 {
            let out = wrapAssistantText(ascii.as_bytes(), cols);
            for line in out.split(|&b| b == b'\n') {
                if line.is_empty() {
                    continue;
                }
                assert!(display_width::visible_width_ignoring_ansi(line) <= cols as usize);
                assert!(display_width::visible_width_ignoring_ansi(line) > 0);
            }
            if cols <= 4 {
                assert!(!contains(&out, "[1]".as_bytes()));
            } else {
                assert!(out.starts_with("\x1b[2m[1] \x1b[22m".as_bytes()));
            }
            cols += 1;
        }

        let wide = "\x1b[2m[1] \x1b[22m漢字abc";
        let mut cols: u16 = 2;
        while cols <= 7 {
            let out = wrapAssistantText(wide.as_bytes(), cols);
            for line in out.split(|&b| b == b'\n') {
                if line.is_empty() {
                    continue;
                }
                assert!(display_width::visible_width_ignoring_ansi(line) <= cols as usize);
                assert!(display_width::visible_width_ignoring_ansi(line) > 0);
            }
            cols += 1;
        }
    }

    #[test]
    fn reopens_a_definition_link_after_a_wrap() {
        let link_open = "\x1b]8;id=fx-definition;https://example.com\x1b\\";
        let link_close = "\x1b]8;;\x1b\\";
        let input = format!("\x1b[2m  \x1b[22m{link_open}\x1b[4malpha beta\x1b[24m{link_close}");
        let out = wrapAssistantText(input.as_bytes(), 8);

        assert!(out.starts_with(
            format!("\x1b[2m  \x1b[22m{link_open}\x1b[4malpha").as_bytes()
        ));
        assert!(contains(
            &out,
            format!("\x1b[0m{link_close}\n  \x1b[4m{link_open}beta\x1b[24m{link_close}").as_bytes()
        ));
    }

    #[test]
    fn indents_pending_and_completed_task_list_continuations() {
        let pending = wrapAssistantText(
            "\x1b[2m☐ \x1b[22malpha beta gamma delta".as_bytes(),
            18,
        );
        assert_eq!(
            "\x1b[2m☐ \x1b[22malpha beta\n  gamma delta".as_bytes(),
            pending.as_slice()
        );

        let completed = wrapAssistantText(
            "\x1b[38;5;252m✓\x1b[39m alpha beta gamma delta".as_bytes(),
            18,
        );
        assert_eq!(
            "\x1b[38;5;252m✓\x1b[39m alpha beta\n  gamma delta".as_bytes(),
            completed.as_slice()
        );

        let ordered = wrapAssistantText(
            "\x1b[2m12.\x1b[22m \x1b[2m☐ \x1b[22malpha beta gamma".as_bytes(),
            20,
        );
        assert_eq!(
            "\x1b[2m12.\x1b[22m \x1b[2m☐ \x1b[22malpha\n      beta gamma".as_bytes(),
            ordered.as_slice()
        );

        let nested = wrapAssistantText("  \x1b[38;5;252m✓\x1b[39m abcdefghij".as_bytes(), 10);
        assert_eq!(
            "  \x1b[38;5;252m✓\x1b[39m abcdef\n    ghij".as_bytes(),
            nested.as_slice()
        );
    }

    #[test]
    fn keeps_narrow_task_list_rows_within_terminal_width() {
        let task_rows: &[&[u8]] = &[
            "\x1b[2m☐ \x1b[22malphabet".as_bytes(),
            "\x1b[38;5;252m✓\x1b[39m alphabet".as_bytes(),
        ];

        for &task_row in task_rows {
            let mut cols: u16 = 1;
            while cols <= 5 {
                let out = wrapAssistantText(task_row, cols);
                for line in out.split(|&b| b == b'\n') {
                    assert!(display_width::visible_width_ignoring_ansi(line) <= cols as usize);
                }
                cols += 1;
            }
        }
    }

    #[test]
    fn keeps_styled_list_continuations_unstyled_at_indentation() {
        let input = "\x1b[2m• \x1b[22m\x1b[1mabcdefghij\x1b[22m";
        let out = wrapAssistantText(input.as_bytes(), 7);
        assert_eq!(
            "\x1b[2m• \x1b[22m\x1b[1mabcde\x1b[0m\n  \x1b[1mfghij\x1b[22m".as_bytes(),
            out.as_slice()
        );
    }

    #[test]
    fn keeps_narrow_list_rows_within_terminal_width() {
        let single_cell = "\x1b[2m• \x1b[22mabcdefghijkl";
        let two_cell = "\x1b[2m• \x1b[22m漢字abcdef";

        let mut cols: u16 = 1;
        while cols <= 5 {
            let out = wrapAssistantText(single_cell.as_bytes(), cols);
            for line in out.split(|&b| b == b'\n') {
                assert!(display_width::visible_width_ignoring_ansi(line) <= cols as usize);
            }
            cols += 1;
        }

        let mut cols: u16 = 2;
        while cols <= 5 {
            let out = wrapAssistantText(two_cell.as_bytes(), cols);
            for line in out.split(|&b| b == b'\n') {
                assert!(display_width::visible_width_ignoring_ansi(line) <= cols as usize);
            }
            cols += 1;
        }
    }

    #[test]
    fn keeps_unicode_display_units_intact_at_row_boundaries() {
        let cases: &[&str] = &[
            "\u{2600}\u{FE0E}",
            "\u{1F44D}\u{1F3FD}",
            "\u{1F1FA}\u{1F1F8}",
            "#\u{FE0F}\u{20E3}",
            "\u{1F3F4}\u{E0067}\u{E0062}\u{E0065}\u{E006E}\u{E0067}\u{E007F}",
            "\u{1F469}\u{200D}\u{1F4BB}",
        ];

        for unit in cases {
            let input = format!("a{}b", unit);
            let out = wrapAssistantText(input.as_bytes(), 3);

            assert!(contains(&out, unit.as_bytes()));
            for line in out.split(|&b| b == b'\n') {
                assert!(display_width::visible_width_ignoring_ansi(line) <= 3);
            }
        }
    }

    #[test]
    fn keeps_narrow_blockquote_rows_within_terminal_width() {
        let prefix = "\x1b[2m│ \x1b[22m\x1b[2m│ \x1b[22m";
        let single_cell = format!("{}abcdefghijkl", prefix);
        let two_cell = format!("{}漢字abcdef", prefix);

        let mut cols: u16 = 1;
        while cols <= 7 {
            let out = wrapAssistantText(single_cell.as_bytes(), cols);
            for line in out.split(|&b| b == b'\n') {
                assert!(display_width::visible_width_ignoring_ansi(line) <= cols as usize);
            }
            cols += 1;
        }

        let mut cols: u16 = 2;
        while cols <= 7 {
            let out = wrapAssistantText(two_cell.as_bytes(), cols);
            for line in out.split(|&b| b == b'\n') {
                assert!(display_width::visible_width_ignoring_ansi(line) <= cols as usize);
            }
            cols += 1;
        }
    }

    #[test]
    fn literal_command_output_repeats_gutters_and_uses_absolute_tab_stops() {
        let out = wrapLiteralCommandOutput("paragraph words\n\n\tvalue".as_bytes(), 16);

        assert_eq!(
            "│ paragraph\n│ words\n│ \n│       value".as_bytes(),
            out.as_slice()
        );
    }

    #[test]
    fn literal_tool_output_repeats_the_tool_detail_rail_on_physical_rows() {
        let out = wrapLiteralToolOutput("alpha beta gamma delta".as_bytes(), 16);

        assert_eq!("│  alpha beta\n│  gamma delta".as_bytes(), out.as_slice());
    }

    #[test]
    fn literal_command_output_prefix_bounds_oversized_record_work() {
        let input = vec![b'x'; 1024 * 1024];

        let mut prefix = wrapLiteralCommandOutputPrefix(&input, 20, 5);
        assert!(prefix.has_more);
        assert_eq!(5, prefix.bytes.iter().filter(|&&b| b == b'\n').count() + 1);
        assert!(prefix.bytes.len() < 256);
        prefix.deinit();
    }

    #[test]
    fn literal_command_output_preserves_and_rows_a_pathological_zero_width_run() {
        let mut input: Vec<u8> = Vec::new();
        input.push(b'a');
        let combining_count = literal_command_zero_width_row_byte_limit + 512;
        for _ in 0..combining_count {
            input.extend_from_slice(b"\xcc\x81");
        }

        let out = wrapLiteralCommandOutput(&input, 80);
        assert_eq!(
            combining_count,
            out.windows(2).filter(|w| *w == b"\xcc\x81").count()
        );
        assert!(out.iter().filter(|&&b| b == b'\n').count() > 1);
    }
}
