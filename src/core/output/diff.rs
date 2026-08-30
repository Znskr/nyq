//! File diff projection (ported 1:1 from
//! zig-reference/src/core/output/diff.zig).

use crate::core::shared::{debug_trace, text_utils, types};

pub const MAX_PREVIEW_LINES: usize = 6;
pub const MAX_ENCODED_PATH_BYTES: usize = 4 * 1024;
pub const MAX_ENCODED_LABEL_BYTES: usize = 4 * 1024;
pub const MAX_ENCODED_LINE_BYTES: usize = 4 * 1024;
pub const MAX_ENCODED_PREVIEW_BYTES: usize = 24 * 1024;
pub const MAX_REQUEST_PROJECTION_BYTES: usize = 32 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LineOp {
    Equal,
    Add,
    Remove,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EditKind {
    Edited,
    Added,
}

pub const DIFF_BLOCK_START_PREFIX: &[u8] = b"\x1b]9050;";
pub const DIFF_BLOCK_END_PREFIX: &[u8] = b"\x1b]9051;";
const OSC_BELL: u8 = 0x07;
const TRAILING_NEWLINE_ADDED_MARKER: &[u8] = b"(trailing newline added)";
const TRAILING_NEWLINE_REMOVED_MARKER: &[u8] = b"(trailing newline removed)";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiffEntry {
    pub id: u32,
    pub full: Option<FullDiff>,
}

impl DiffEntry {
    pub fn deinit(&mut self) {
        self.full = None;
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FullDiff {
    pub content: Vec<u8>,
    pub lifecycle_id: types::ToolLifecycleId,
}

impl FullDiff {
    pub fn deinit(&mut self) {
        self.content.clear();
        self.lifecycle_id.call_id.clear();
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiffEntryPayload {
    pub preview: Vec<u8>,
    pub additions: u32,
    pub deletions: u32,
    pub full: Option<FullDiff>,
}

pub fn free_diff_entry_payload(payload: DiffEntryPayload) {
    drop(payload);
}

#[test]
fn diff_entry_payload_has_one_canonical_preview() {
    let payload = DiffEntryPayload {
        preview: b"preview".to_vec(),
        additions: 0,
        deletions: 0,
        full: None,
    };
    assert_eq!(payload.preview, b"preview");
}

pub fn wrap_with_markers(id: u32, content: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(DIFF_BLOCK_START_PREFIX);
    out.extend_from_slice(id.to_string().as_bytes());
    out.push(OSC_BELL);
    out.extend_from_slice(content);
    out.extend_from_slice(DIFF_BLOCK_END_PREFIX);
    out.extend_from_slice(id.to_string().as_bytes());
    out.push(OSC_BELL);
    out
}

pub fn marked_diff_block_id(bytes: &[u8]) -> Option<u32> {
    if !bytes.starts_with(DIFF_BLOCK_START_PREFIX) {
        return None;
    }
    let id_start = DIFF_BLOCK_START_PREFIX.len();
    let bell_pos = bytes[id_start..]
        .iter()
        .position(|&b| b == OSC_BELL)?
        + id_start;
    let id_digits = &bytes[id_start..bell_pos];
    let id: u32 = std::str::from_utf8(id_digits).ok()?.parse().ok()?;

    let mut end_marker = Vec::new();
    end_marker.extend_from_slice(DIFF_BLOCK_END_PREFIX);
    end_marker.extend_from_slice(id_digits);
    end_marker.push(OSC_BELL);
    if !bytes.ends_with(&end_marker) {
        return None;
    }
    Some(id)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiffLine {
    pub op: LineOp,
    pub old_num: Option<u32>,
    pub new_num: Option<u32>,
    pub text: Vec<u8>,
}

#[derive(Clone, Copy, Debug)]
pub struct PreviewOptions {
    pub max_lines: usize,
    pub max_indexed_lines: usize,
    pub max_matrix_cells: usize,
    pub max_encoded_line_bytes: usize,
    pub max_encoded_preview_bytes: usize,
}

impl Default for PreviewOptions {
    fn default() -> Self {
        Self {
            max_lines: MAX_PREVIEW_LINES,
            max_indexed_lines: CANONICAL_MAX_INDEXED_LINES,
            max_matrix_cells: CANONICAL_MAX_MATRIX_CELLS,
            max_encoded_line_bytes: MAX_ENCODED_LINE_BYTES,
            max_encoded_preview_bytes: MAX_ENCODED_PREVIEW_BYTES,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PreviewOp {
    Context,
    Addition,
    Deletion,
    Elision,
    Notice,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreviewLine {
    pub op: PreviewOp,
    pub old_line: Option<u32>,
    pub new_line: Option<u32>,
    pub text: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PreviewValidationError {
    TooManyLines,
    PathTooLong,
    InvalidPathMetadata,
    LineTooLong,
    PreviewTooLarge,
    InvalidLineNumbers,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileChangePreview {
    pub path: Vec<u8>,
    pub path_basename_start: usize,
    pub path_source_truncated: bool,
    pub lines: Vec<PreviewLine>,
    pub additions: usize,
    pub deletions: usize,
    pub truncated: bool,
}

impl FileChangePreview {
    pub fn validate(&self) -> Result<(), PreviewValidationError> {
        if self.path.len() > MAX_ENCODED_PATH_BYTES {
            return Err(PreviewValidationError::PathTooLong);
        }
        if self.path.is_empty() || self.path_basename_start >= self.path.len() {
            return Err(PreviewValidationError::InvalidPathMetadata);
        }
        if self.lines.len() > MAX_PREVIEW_LINES {
            return Err(PreviewValidationError::TooManyLines);
        }

        let mut encoded_bytes: usize = 0;
        let mut addition_rows: usize = 0;
        let mut deletion_rows: usize = 0;
        let mut elision_rows: usize = 0;
        let mut notice_rows: usize = 0;
        for line in &self.lines {
            if line.text.len() > MAX_ENCODED_LINE_BYTES {
                return Err(PreviewValidationError::LineTooLong);
            }
            encoded_bytes = encoded_bytes
                .checked_add(line.text.len())
                .ok_or(PreviewValidationError::PreviewTooLarge)?;
            if encoded_bytes > MAX_ENCODED_PREVIEW_BYTES {
                return Err(PreviewValidationError::PreviewTooLarge);
            }

            let valid_numbers = match line.op {
                PreviewOp::Context => {
                    line.old_line.is_some_and(|v| v > 0)
                        && line.new_line.is_some_and(|v| v > 0)
                }
                PreviewOp::Addition => {
                    line.old_line.is_none() && line.new_line.is_some_and(|v| v > 0)
                }
                PreviewOp::Deletion => {
                    line.old_line.is_some_and(|v| v > 0) && line.new_line.is_none()
                }
                PreviewOp::Elision | PreviewOp::Notice => {
                    line.old_line.is_none() && line.new_line.is_none()
                }
            };
            if !valid_numbers {
                return Err(PreviewValidationError::InvalidLineNumbers);
            }

            match line.op {
                PreviewOp::Context => {}
                PreviewOp::Addition => addition_rows += 1,
                PreviewOp::Deletion => deletion_rows += 1,
                PreviewOp::Elision => elision_rows += 1,
                PreviewOp::Notice => notice_rows += 1,
            }
        }

        if elision_rows > 1 {
            return Err(PreviewValidationError::InvalidLineNumbers);
        }
        if self.additions == 0 && self.deletions == 0 {
            if self.lines.len() != 1 || notice_rows != 1 {
                return Err(PreviewValidationError::InvalidLineNumbers);
            }
            return Ok(());
        }
        if notice_rows != 0 || addition_rows + deletion_rows == 0 {
            return Err(PreviewValidationError::InvalidLineNumbers);
        }
        if addition_rows > self.additions || deletion_rows > self.deletions {
            return Err(PreviewValidationError::InvalidLineNumbers);
        }
        Ok(())
    }

    pub fn eql(&self, other: &Self) -> bool {
        self == other
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectedLine {
    pub op: PreviewOp,
    pub old_line: Option<u32>,
    pub new_line: Option<u32>,
    pub text: Vec<u8>,
}

#[derive(Clone, Debug)]
pub struct BoundedDiffProjection {
    pub lines: Vec<ProjectedLine>,
    pub additions: usize,
    pub deletions: usize,
    pub truncated: bool,
}

impl BoundedDiffProjection {
    pub fn deinit(&mut self) {
        self.lines.clear();
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileChangeFullView {
    pub after_content: Vec<u8>,
    pub lifecycle_id: types::ToolLifecycleId,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileReviewError {
    OutOfMemory,
    ArithmeticOverflow,
}

enum ReviewMode {
    Computed(Vec<DiffLine>),
    Fallback(FallbackReview),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewLine {
    pub op: PreviewOp,
    pub old_line: Option<u32>,
    pub new_line: Option<u32>,
    pub text: Vec<u8>,
    pub elision_count: usize,
}

#[derive(Debug)]
pub struct ReviewViewport {
    pub lines: Vec<ReviewLine>,
    pub total_rows: usize,
}

impl ReviewViewport {
    pub fn deinit(&mut self) {
        self.lines.clear();
    }
}

pub struct FileReview {
    mode: ReviewMode,
    additions: usize,
    deletions: usize,
}

impl FileReview {
    pub fn init(
        old_text: &[u8],
        new_text: &[u8],
    ) -> Result<FileReview, FileReviewError> {
        let old_marker = trailing_newline_marker(old_text, new_text, true);
        let new_marker = trailing_newline_marker(old_text, new_text, false);
        let old_line_count = checked_review_add(
            count_text_lines(old_text),
            old_marker.is_some() as usize,
        )?;
        let new_line_count = checked_review_add(
            count_text_lines(new_text),
            new_marker.is_some() as usize,
        )?;
        let indexed_lines = checked_review_add(old_line_count, new_line_count)?;
        let matrix_cells = checked_review_mul(
            checked_review_add(old_line_count, 1)?,
            checked_review_add(new_line_count, 1)?,
        )?;

        if indexed_lines <= CANONICAL_MAX_INDEXED_LINES
            && matrix_cells <= CANONICAL_MAX_MATRIX_CELLS
        {
            let lines = compute(old_text, new_text);
            let mut additions: usize = 0;
            let mut deletions: usize = 0;
            for line in &lines {
                match line.op {
                    LineOp::Equal => {}
                    LineOp::Add => additions += 1,
                    LineOp::Remove => deletions += 1,
                }
            }
            return Ok(FileReview {
                mode: ReviewMode::Computed(lines),
                additions,
                deletions,
            });
        }

        let fallback = FallbackReview::init(
            old_text,
            new_text,
            old_marker,
            new_marker,
        )?;
        let additions = fallback.additions;
        let deletions = fallback.deletions;
        Ok(FileReview {
            mode: ReviewMode::Fallback(fallback),
            additions,
            deletions,
        })
    }

    pub fn deinit(&mut self) {
        self.mode = ReviewMode::Computed(Vec::new());
        self.additions = 0;
        self.deletions = 0;
    }

    pub fn row_count(&self) -> usize {
        let changed = self.additions + self.deletions;
        if changed == 0 {
            return 1;
        }
        match &self.mode {
            ReviewMode::Computed(source) => count_computed_review_rows(source),
            ReviewMode::Fallback(source) => count_fallback_review_rows(source),
        }
    }

    pub fn viewport(
        &self,
        start_row: usize,
        max_rows: usize,
    ) -> ReviewViewport {
        let total_rows = self.row_count();
        let start = start_row.min(total_rows);
        let count = max_rows.min(total_rows - start);
        let mut lines = Vec::with_capacity(count);

        if self.additions == 0 && self.deletions == 0 {
            if count == 1 {
                lines.push(ReviewLine {
                    op: PreviewOp::Notice,
                    old_line: None,
                    new_line: None,
                    text: b"No content changes".to_vec(),
                    elision_count: 0,
                });
            }
            return ReviewViewport { lines, total_rows };
        }

        match &self.mode {
            ReviewMode::Computed(source) => {
                fill_computed_review_viewport(
                    &mut lines,
                    source,
                    start,
                    count,
                );
            }
            ReviewMode::Fallback(source) => {
                fill_fallback_review_viewport(
                    &mut lines,
                    source,
                    start,
                    count,
                );
            }
        }
        ReviewViewport { lines, total_rows }
    }
}

struct FallbackReview {
    old_text: Vec<u8>,
    new_text: Vec<u8>,
    old_line_starts: Vec<usize>,
    new_line_starts: Vec<usize>,
    prefix_count: usize,
    suffix_count: usize,
    old_middle_count: usize,
    new_middle_count: usize,
    old_marker: Option<&'static [u8]>,
    new_marker: Option<&'static [u8]>,
    additions: usize,
    deletions: usize,
}

impl FallbackReview {
    fn init(
        old_text: &[u8],
        new_text: &[u8],
        old_marker: Option<&'static [u8]>,
        new_marker: Option<&'static [u8]>,
    ) -> Result<FallbackReview, FileReviewError> {
        let old_line_starts = index_line_starts(old_text);
        let new_line_starts = index_line_starts(new_text);
        let old_normal_count = old_line_starts.len();
        let new_normal_count = new_line_starts.len();
        let comparable_count = old_normal_count.min(new_normal_count);

        let mut prefix_count: usize = 0;
        while prefix_count < comparable_count {
            let old_line =
                indexed_text_line(old_text, &old_line_starts, prefix_count);
            let new_line =
                indexed_text_line(new_text, &new_line_starts, prefix_count);
            if old_line.text != new_line.text {
                break;
            }
            prefix_count += 1;
        }

        let mut suffix_count: usize = 0;
        while prefix_count + suffix_count < comparable_count {
            let old_line = indexed_text_line(
                old_text,
                &old_line_starts,
                old_normal_count - suffix_count - 1,
            );
            let new_line = indexed_text_line(
                new_text,
                &new_line_starts,
                new_normal_count - suffix_count - 1,
            );
            if old_line.text != new_line.text {
                break;
            }
            suffix_count += 1;
        }

        let old_middle_count = old_normal_count - prefix_count - suffix_count;
        let new_middle_count = new_normal_count - prefix_count - suffix_count;
        Ok(FallbackReview {
            old_text: old_text.to_vec(),
            new_text: new_text.to_vec(),
            old_line_starts,
            new_line_starts,
            prefix_count,
            suffix_count,
            old_middle_count,
            new_middle_count,
            old_marker,
            new_marker,
            additions: checked_review_add(
                new_middle_count,
                new_marker.is_some() as usize,
            )?,
            deletions: checked_review_add(
                old_middle_count,
                old_marker.is_some() as usize,
            )?,
        })
    }
}

fn checked_review_add(a: usize, b: usize) -> Result<usize, FileReviewError> {
    a.checked_add(b).ok_or(FileReviewError::ArithmeticOverflow)
}

fn checked_review_mul(a: usize, b: usize) -> Result<usize, FileReviewError> {
    a.checked_mul(b).ok_or(FileReviewError::ArithmeticOverflow)
}

const REVIEW_CONTEXT_LINES: usize = 5;

trait ReviewVisitor {
    fn append(&mut self, line: ReviewLine);
    fn is_full(&self) -> bool;
}

#[derive(Default)]
struct ReviewRowCounter {
    count: usize,
}

impl ReviewVisitor for ReviewRowCounter {
    fn append(&mut self, _line: ReviewLine) {
        self.count += 1;
    }

    fn is_full(&self) -> bool {
        false
    }
}

struct ReviewViewportWriter<'a> {
    destination: &'a mut Vec<ReviewLine>,
    start_row: usize,
    capacity: usize,
    ordinal: usize,
    written: usize,
}

impl ReviewVisitor for ReviewViewportWriter<'_> {
    fn append(&mut self, line: ReviewLine) {
        if self.ordinal >= self.start_row && self.written < self.capacity {
            self.destination.push(line);
            self.written += 1;
        }
        self.ordinal += 1;
    }

    fn is_full(&self) -> bool {
        self.written == self.capacity
    }
}

fn count_computed_review_rows(source: &[DiffLine]) -> usize {
    let mut counter = ReviewRowCounter::default();
    visit_computed_review_rows(source, &mut counter);
    counter.count
}

fn count_fallback_review_rows(source: &FallbackReview) -> usize {
    equal_run_row_count(source.prefix_count)
        + source.deletions
        + source.additions
        + equal_run_row_count(source.suffix_count)
}

fn equal_run_row_count(count: usize) -> usize {
    count.min(REVIEW_CONTEXT_LINES) + usize::from(count > REVIEW_CONTEXT_LINES)
}

fn visit_computed_review_rows<V: ReviewVisitor>(
    source: &[DiffLine],
    visitor: &mut V,
) {
    let mut source_index: usize = 0;
    while source_index < source.len() {
        let line = &source[source_index];
        match line.op {
            LineOp::Equal => {
                let run_start = source_index;
                while source_index < source.len()
                    && source[source_index].op == LineOp::Equal
                {
                    source_index += 1;
                }
                append_computed_equal_run(
                    source,
                    run_start,
                    source_index,
                    visitor,
                );
            }
            LineOp::Add => {
                visitor.append(ReviewLine {
                    op: PreviewOp::Addition,
                    old_line: None,
                    new_line: line.new_num,
                    text: line.text.clone(),
                    elision_count: 0,
                });
                source_index += 1;
            }
            LineOp::Remove => {
                visitor.append(ReviewLine {
                    op: PreviewOp::Deletion,
                    old_line: line.old_num,
                    new_line: None,
                    text: line.text.clone(),
                    elision_count: 0,
                });
                source_index += 1;
            }
        }
        if visitor.is_full() {
            return;
        }
    }
}

fn append_computed_equal_run<V: ReviewVisitor>(
    source: &[DiffLine],
    run_start: usize,
    run_end: usize,
    visitor: &mut V,
) {
    if run_start == 0 {
        let context_start =
            run_start.max(run_end.saturating_sub(REVIEW_CONTEXT_LINES));
        append_equal_run_elision(visitor, context_start - run_start);
        append_computed_context_range(
            source,
            context_start,
            run_end,
            visitor,
        );
        return;
    }

    if run_end == source.len() {
        let context_end =
            run_end.min(run_start + REVIEW_CONTEXT_LINES);
        append_computed_context_range(source, run_start, context_end, visitor);
        append_equal_run_elision(visitor, run_end - context_end);
        return;
    }

    let first_context_end = run_end.min(run_start + REVIEW_CONTEXT_LINES);
    let last_context_start = first_context_end
        .max(run_end.saturating_sub(REVIEW_CONTEXT_LINES));
    append_computed_context_range(
        source,
        run_start,
        first_context_end,
        visitor,
    );
    append_equal_run_elision(
        visitor,
        last_context_start - first_context_end,
    );
    append_computed_context_range(
        source,
        last_context_start,
        run_end,
        visitor,
    );
}

fn append_computed_context_range<V: ReviewVisitor>(
    source: &[DiffLine],
    start: usize,
    end: usize,
    visitor: &mut V,
) {
    for line in &source[start..end] {
        debug_assert_eq!(line.op, LineOp::Equal);
        visitor.append(ReviewLine {
            op: PreviewOp::Context,
            old_line: line.old_num,
            new_line: line.new_num,
            text: line.text.clone(),
            elision_count: 0,
        });
    }
}

fn append_equal_run_elision<V: ReviewVisitor>(
    visitor: &mut V,
    count: usize,
) {
    if count == 0 {
        return;
    }
    visitor.append(ReviewLine {
        op: PreviewOp::Elision,
        old_line: None,
        new_line: None,
        text: Vec::new(),
        elision_count: count,
    });
}

fn fill_computed_review_viewport(
    destination: &mut Vec<ReviewLine>,
    source: &[DiffLine],
    start_row: usize,
    count: usize,
) {
    if count == 0 {
        return;
    }
    let mut writer = ReviewViewportWriter {
        destination,
        start_row,
        capacity: count,
        ordinal: 0,
        written: 0,
    };
    visit_computed_review_rows(source, &mut writer);
    debug_assert_eq!(writer.written, count);
}

fn fill_fallback_review_viewport(
    destination: &mut Vec<ReviewLine>,
    source: &FallbackReview,
    start_row: usize,
    count: usize,
) {
    for offset in 0..count {
        destination.push(fallback_review_line_at(
            source,
            start_row + offset,
        ));
    }
}

fn fallback_review_line_at(
    source: &FallbackReview,
    ordinal: usize,
) -> ReviewLine {
    let mut section_start: usize = 0;
    let prefix_context_count =
        source.prefix_count.min(REVIEW_CONTEXT_LINES);
    let prefix_elision_count = source.prefix_count - prefix_context_count;
    if prefix_elision_count > 0 {
        if ordinal == section_start {
            return ReviewLine {
                op: PreviewOp::Elision,
                old_line: None,
                new_line: None,
                text: Vec::new(),
                elision_count: prefix_elision_count,
            };
        }
        section_start += 1;
    }

    if ordinal < section_start + prefix_context_count {
        let line_index =
            prefix_elision_count + ordinal - section_start;
        let line = indexed_text_line(
            &source.old_text,
            &source.old_line_starts,
            line_index,
        );
        return ReviewLine {
            op: PreviewOp::Context,
            old_line: projected_line_number(line.number),
            new_line: projected_line_number(line.number),
            text: line.text.to_vec(),
            elision_count: 0,
        };
    }
    section_start += prefix_context_count;

    if ordinal < section_start + source.old_middle_count {
        let line_index = source.prefix_count + ordinal - section_start;
        let line = indexed_text_line(
            &source.old_text,
            &source.old_line_starts,
            line_index,
        );
        return ReviewLine {
            op: PreviewOp::Deletion,
            old_line: projected_line_number(line.number),
            new_line: None,
            text: line.text.to_vec(),
            elision_count: 0,
        };
    }
    section_start += source.old_middle_count;

    if ordinal < section_start + source.new_middle_count {
        let line_index = source.prefix_count + ordinal - section_start;
        let line = indexed_text_line(
            &source.new_text,
            &source.new_line_starts,
            line_index,
        );
        return ReviewLine {
            op: PreviewOp::Addition,
            old_line: None,
            new_line: projected_line_number(line.number),
            text: line.text.to_vec(),
            elision_count: 0,
        };
    }
    section_start += source.new_middle_count;

    if let Some(marker) = source.old_marker {
        if ordinal == section_start {
            return ReviewLine {
                op: PreviewOp::Deletion,
                old_line: projected_line_number(
                    source.old_line_starts.len() + 1,
                ),
                new_line: None,
                text: marker.to_vec(),
                elision_count: 0,
            };
        }
        section_start += 1;
    }
    if let Some(marker) = source.new_marker {
        if ordinal == section_start {
            return ReviewLine {
                op: PreviewOp::Addition,
                old_line: None,
                new_line: projected_line_number(
                    source.new_line_starts.len() + 1,
                ),
                text: marker.to_vec(),
                elision_count: 0,
            };
        }
        section_start += 1;
    }

    let suffix_context_count =
        source.suffix_count.min(REVIEW_CONTEXT_LINES);
    if ordinal < section_start + suffix_context_count {
        let suffix_offset = ordinal - section_start;
        let old_line_index = source.prefix_count
            + source.old_middle_count
            + suffix_offset;
        let new_line_index = source.prefix_count
            + source.new_middle_count
            + suffix_offset;
        let old_line = indexed_text_line(
            &source.old_text,
            &source.old_line_starts,
            old_line_index,
        );
        let new_line = indexed_text_line(
            &source.new_text,
            &source.new_line_starts,
            new_line_index,
        );
        return ReviewLine {
            op: PreviewOp::Context,
            old_line: projected_line_number(old_line.number),
            new_line: projected_line_number(new_line.number),
            text: old_line.text.to_vec(),
            elision_count: 0,
        };
    }
    section_start += suffix_context_count;

    let suffix_elision_count = source.suffix_count - suffix_context_count;
    if suffix_elision_count > 0 && ordinal == section_start {
        return ReviewLine {
            op: PreviewOp::Elision,
            old_line: None,
            new_line: None,
            text: Vec::new(),
            elision_count: suffix_elision_count,
        };
    }
    unreachable!();
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BoundedPreviewError {
    OutOfMemory,
    InvalidPreviewOptions,
    PreviewBudgetExceeded,
    ArithmeticOverflow,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Stats {
    pub additions: u32,
    pub deletions: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FormatStyles {
    pub added_fg: &'static [u8],
    pub removed_fg: &'static [u8],
    pub context_fg: &'static [u8],
    pub added_marker_fg: &'static [u8],
    pub removed_marker_fg: &'static [u8],
    pub reset: &'static [u8],
}

impl Default for FormatStyles {
    fn default() -> Self {
        Self {
            added_fg: b"\x1b[38;5;252m",
            removed_fg: b"\x1b[38;5;252m",
            context_fg: b"\x1b[38;5;245m",
            added_marker_fg: b"",
            removed_marker_fg: b"",
            reset: b"\x1b[0m",
        }
    }
}

pub fn compute(old_text: &[u8], new_text: &[u8]) -> Vec<DiffLine> {
    let mut old_lines = append_lines(old_text);
    let mut new_lines = append_lines(new_text);
    append_trailing_newline_marker(
        &mut old_lines,
        &mut new_lines,
        old_text,
        new_text,
    );

    let old_len = old_lines.len();
    let new_len = new_lines.len();
    let stride = new_len + 1;

    let mut table = vec![0u32; (old_len + 1) * stride];

    for old_index in 1..=old_len {
        for new_index in 1..=new_len {
            if old_lines[old_index - 1] == new_lines[new_index - 1] {
                table[old_index * stride + new_index] =
                    table[(old_index - 1) * stride + new_index - 1] + 1;
            } else {
                let left = table[old_index * stride + new_index - 1];
                let up = table[(old_index - 1) * stride + new_index];
                table[old_index * stride + new_index] = left.max(up);
            }
        }
    }

    let mut result: Vec<DiffLine> = Vec::new();
    let mut old_cursor = old_len;
    let mut new_cursor = new_len;
    while old_cursor > 0 || new_cursor > 0 {
        if old_cursor > 0
            && new_cursor > 0
            && old_lines[old_cursor - 1] == new_lines[new_cursor - 1]
        {
            result.push(DiffLine {
                op: LineOp::Equal,
                old_num: Some(old_cursor as u32),
                new_num: Some(new_cursor as u32),
                text: old_lines[old_cursor - 1].to_vec(),
            });
            old_cursor -= 1;
            new_cursor -= 1;
        } else if new_cursor > 0
            && (old_cursor == 0
                || table[old_cursor * stride + new_cursor - 1]
                    >= table[(old_cursor - 1) * stride + new_cursor])
        {
            result.push(DiffLine {
                op: LineOp::Add,
                old_num: None,
                new_num: Some(new_cursor as u32),
                text: new_lines[new_cursor - 1].to_vec(),
            });
            new_cursor -= 1;
        } else {
            result.push(DiffLine {
                op: LineOp::Remove,
                old_num: Some(old_cursor as u32),
                new_num: None,
                text: old_lines[old_cursor - 1].to_vec(),
            });
            old_cursor -= 1;
        }
    }

    result.reverse();
    result
}

pub fn build_bounded_preview(
    old_text: &[u8],
    new_text: &[u8],
    options: PreviewOptions,
) -> Result<BoundedDiffProjection, BoundedPreviewError> {
    validate_preview_options(options)?;

    let old_marker = trailing_newline_marker(old_text, new_text, true);
    let new_marker = trailing_newline_marker(old_text, new_text, false);
    let old_line_count = checked_add(
        count_text_lines(old_text),
        old_marker.is_some() as usize,
    )?;
    let new_line_count = checked_add(
        count_text_lines(new_text),
        new_marker.is_some() as usize,
    )?;
    if preview_uses_compute(old_line_count, new_line_count, options)? {
        return build_computed_preview(old_text, new_text, options);
    }
    build_fallback_preview(
        old_text,
        new_text,
        old_marker,
        new_marker,
        options,
    )
}

pub fn format_file_change_payload(
    preview: FileChangePreview,
    previous_content: Option<&[u8]>,
    full_view: Option<FileChangeFullView>,
    styles: FormatStyles,
) -> DiffEntryPayload {
    let additions =
        u32::try_from(preview.additions).expect("diff stat overflow");
    let deletions =
        u32::try_from(preview.deletions).expect("diff stat overflow");
    let formatted_preview = format_file_change_preview(&preview, styles);
    let full = match format_file_change_full_diff(
        previous_content,
        full_view,
        styles,
    ) {
        Ok(full) => full,
        Err(err) => {
            debug_trace::logf(
                "tool",
                format_args!(
                    "committed file full diff formatting unavailable err={err:?}"
                ),
            );
            None
        }
    };
    DiffEntryPayload {
        preview: formatted_preview,
        additions,
        deletions,
        full,
    }
}

pub fn format_persisted_file_change_payload(
    presentation: &types::CommittedFilePresentation,
    styles: FormatStyles,
) -> DiffEntryPayload {
    let mut lines: Vec<PreviewLine> =
        Vec::with_capacity(presentation.lines.len());
    for line in &presentation.lines {
        lines.push(PreviewLine {
            op: match line.kind {
                types::CommittedFilePresentationLineKind::Context => {
                    PreviewOp::Context
                }
                types::CommittedFilePresentationLineKind::Addition => {
                    PreviewOp::Addition
                }
                types::CommittedFilePresentationLineKind::Deletion => {
                    PreviewOp::Deletion
                }
                types::CommittedFilePresentationLineKind::Elision => {
                    PreviewOp::Elision
                }
                types::CommittedFilePresentationLineKind::Notice => {
                    PreviewOp::Notice
                }
            },
            old_line: line.old_line,
            new_line: line.new_line,
            text: line.text.clone(),
        });
    }

    let full_view = if let Some(after_content) = &presentation.after_content {
        presentation
            .lifecycle_id
            .as_ref()
            .map(|lifecycle_id| FileChangeFullView {
                after_content: after_content.clone(),
                lifecycle_id: lifecycle_id.clone(),
            })
    } else {
        None
    };

    format_file_change_payload(
        FileChangePreview {
            path: presentation.path.clone(),
            path_basename_start: 0,
            path_source_truncated: false,
            lines,
            additions: presentation.additions,
            deletions: presentation.deletions,
            truncated: presentation.truncated,
        },
        presentation.previous_content.as_deref(),
        full_view,
        styles,
    )
}

pub fn format_file_change_full_diff(
    previous_content: Option<&[u8]>,
    full_view: Option<FileChangeFullView>,
    styles: FormatStyles,
) -> Result<Option<FullDiff>, FileReviewError> {
    let Some(snapshot) = full_view else {
        return Ok(None);
    };
    let review =
        FileReview::init(previous_content.unwrap_or(b""), &snapshot.after_content)?;
    let viewport = review.viewport(0, review.row_count());
    let content = format_file_change_review_lines(&viewport.lines, styles);
    let call_id = snapshot.lifecycle_id.call_id.clone();
    Ok(Some(FullDiff {
        content,
        lifecycle_id: types::ToolLifecycleId {
            turn_id: snapshot.lifecycle_id.turn_id,
            call_id,
        },
    }))
}

fn format_file_change_review_lines(
    lines: &[ReviewLine],
    styles: FormatStyles,
) -> Vec<u8> {
    let mut out = Vec::new();

    let mut max_line_number: u32 = 1;
    for line in lines {
        if let Some(line_number) = line.new_line.or(line.old_line) {
            max_line_number = max_line_number.max(line_number);
        }
    }
    let gutter_width = decimal_digits(max_line_number);

    for line in lines {
        let elision_text;
        let text: &[u8] = if line.op == PreviewOp::Elision {
            elision_text = format!("{} unchanged lines ⋯", line.elision_count);
            elision_text.as_bytes()
        } else {
            &line.text
        };
        append_diff_render_line(
            &mut out,
            DiffRenderLine {
                op: line.op,
                line_number: line.new_line.or(line.old_line),
                gutter_width,
                text,
            },
            styles,
        );
    }

    out
}

pub fn format_file_change_preview(
    preview: &FileChangePreview,
    styles: FormatStyles,
) -> Vec<u8> {
    let mut out = Vec::new();

    let mut max_line_number: u32 = 1;
    for line in &preview.lines {
        if let Some(line_number) = line.new_line.or(line.old_line) {
            max_line_number = max_line_number.max(line_number);
        }
    }
    let gutter_width = decimal_digits(max_line_number);

    for line in &preview.lines {
        append_diff_render_line(
            &mut out,
            DiffRenderLine {
                op: line.op,
                line_number: line.new_line.or(line.old_line),
                gutter_width,
                text: &line.text,
            },
            styles,
        );
    }

    out
}

struct DiffRenderLine<'a> {
    op: PreviewOp,
    line_number: Option<u32>,
    gutter_width: usize,
    text: &'a [u8],
}

fn append_diff_render_line(
    out: &mut Vec<u8>,
    line: DiffRenderLine<'_>,
    styles: FormatStyles,
) {
    let line_style = preview_line_style(line.op, styles);
    let marker_style = preview_marker_style(line.op, styles);
    let accent = !marker_style.is_empty();

    out.extend_from_slice(line_style);
    out.extend_from_slice("  │ ".as_bytes());
    if accent {
        out.extend_from_slice(marker_style);
    }
    append_diff_line_number(out, line.line_number, line.gutter_width);
    out.push(b' ');
    out.push(preview_line_sign(line.op));
    if accent {
        out.extend_from_slice(line_style);
    }
    out.push(b' ');
    out.extend_from_slice(line.text);
    out.extend_from_slice(styles.reset);
    out.push(b'\n');
}

fn append_diff_elision_line(
    out: &mut Vec<u8>,
    gutter_width: usize,
    styles: FormatStyles,
) {
    append_diff_render_line(
        out,
        DiffRenderLine {
            op: PreviewOp::Elision,
            line_number: None,
            gutter_width,
            text: "⋯".as_bytes(),
        },
        styles,
    );
}

fn preview_line_style(op: PreviewOp, styles: FormatStyles) -> &'static [u8] {
    match op {
        PreviewOp::Addition => styles.added_fg,
        PreviewOp::Deletion => styles.removed_fg,
        PreviewOp::Context | PreviewOp::Elision | PreviewOp::Notice => {
            styles.context_fg
        }
    }
}

fn preview_marker_style(
    op: PreviewOp,
    styles: FormatStyles,
) -> &'static [u8] {
    match op {
        PreviewOp::Addition => styles.added_marker_fg,
        PreviewOp::Deletion => styles.removed_marker_fg,
        PreviewOp::Context | PreviewOp::Elision | PreviewOp::Notice => b"",
    }
}

fn preview_line_sign(op: PreviewOp) -> u8 {
    match op {
        PreviewOp::Addition => b'+',
        PreviewOp::Deletion => b'-',
        PreviewOp::Context | PreviewOp::Elision | PreviewOp::Notice => b' ',
    }
}

fn append_diff_line_number(
    out: &mut Vec<u8>,
    line_number: Option<u32>,
    width: usize,
) {
    let number = match line_number {
        Some(value) => value.to_string(),
        None => String::new(),
    };
    for _ in 0..width.saturating_sub(number.len()) {
        out.push(b' ');
    }
    out.extend_from_slice(number.as_bytes());
}

fn decimal_digits(value: u32) -> usize {
    let mut remaining = value;
    let mut digits: usize = 1;
    while remaining >= 10 {
        digits += 1;
        remaining /= 10;
    }
    digits
}

fn diff_preview_op(op: LineOp) -> PreviewOp {
    match op {
        LineOp::Equal => PreviewOp::Context,
        LineOp::Add => PreviewOp::Addition,
        LineOp::Remove => PreviewOp::Deletion,
    }
}

pub fn count_stats(diff: &[DiffLine]) -> Stats {
    let mut stats = Stats::default();
    for line in diff {
        match line.op {
            LineOp::Equal => {}
            LineOp::Add => stats.additions += 1,
            LineOp::Remove => stats.deletions += 1,
        }
    }
    stats
}

pub const CONTEXT_LINES: usize = 2;
const MAX_DISPLAY_LINES: usize = 6;

pub fn format_unified(
    diff: &[DiffLine],
    styles: FormatStyles,
    path: Option<&[u8]>,
    kind: EditKind,
) -> Vec<u8> {
    let _ = (path, kind);
    let mut out = Vec::new();

    let mut include = vec![false; diff.len()];

    for (index, line) in diff.iter().enumerate() {
        if line.op == LineOp::Equal {
            continue;
        }
        let start = index.saturating_sub(CONTEXT_LINES);
        let end = (index + CONTEXT_LINES + 1).min(diff.len());
        for include_index in start..end {
            include[include_index] = true;
        }
    }

    let mut max_num: u32 = 1;
    for (index, line) in diff.iter().enumerate() {
        if !include[index] {
            continue;
        }
        let line_num = line.new_num.or(line.old_num).unwrap_or(0);
        if line_num > max_num {
            max_num = line_num;
        }
    }
    let gutter = decimal_digits(max_num);

    let mut prev_included = false;
    let mut emitted_any = false;
    let mut emitted_lines: usize = 0;
    for (index, line) in diff.iter().enumerate() {
        if !include[index] {
            prev_included = false;
            continue;
        }
        if emitted_lines >= MAX_DISPLAY_LINES {
            append_diff_elision_line(&mut out, gutter, styles);
            break;
        }
        if !prev_included && emitted_any {
            append_diff_elision_line(&mut out, gutter, styles);
        }

        append_diff_render_line(
            &mut out,
            DiffRenderLine {
                op: diff_preview_op(line.op),
                line_number: line.new_num.or(line.old_num),
                gutter_width: gutter,
                text: &line.text,
            },
            styles,
        );

        prev_included = true;
        emitted_any = true;
        emitted_lines += 1;
    }

    out
}

const CANONICAL_MAX_INDEXED_LINES: usize = 16_384;
const CANONICAL_MAX_MATRIX_CELLS: usize = 1_000_000;

#[derive(Clone, Debug)]
struct SelectedLine {
    op: PreviewOp,
    old_line: Option<u32>,
    new_line: Option<u32>,
    text: Vec<u8>,
}

impl Default for SelectedLine {
    fn default() -> Self {
        Self {
            op: PreviewOp::Notice,
            old_line: None,
            new_line: None,
            text: Vec::new(),
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct SelectionPlan {
    output_count: usize,
    head_count: usize,
    tail_count: usize,
    truncated: bool,
}

#[derive(Clone, Copy)]
struct TextLine<'a> {
    number: usize,
    text: &'a [u8],
}

struct ForwardLineIterator<'a> {
    text: &'a [u8],
    cursor: usize,
    next_number: usize,
}

impl<'a> ForwardLineIterator<'a> {
    fn new(text: &'a [u8]) -> Self {
        Self {
            text,
            cursor: 0,
            next_number: 1,
        }
    }

    fn next(&mut self) -> Option<TextLine<'a>> {
        if self.cursor >= self.text.len() {
            return None;
        }

        let start = self.cursor;
        let newline = self.text[self.cursor..]
            .iter()
            .position(|&b| b == b'\n');
        let end = newline.map_or(self.text.len(), |n| self.cursor + n);
        self.cursor = newline.map_or(self.text.len(), |n| self.cursor + n + 1);

        let result = TextLine {
            number: self.next_number,
            text: &self.text[start..end],
        };
        self.next_number = self.next_number.wrapping_add(1);
        Some(result)
    }
}

struct ReverseLineIterator<'a> {
    text: &'a [u8],
    end: usize,
    next_number: usize,
}

impl<'a> ReverseLineIterator<'a> {
    fn new(text: &'a [u8], line_count: usize) -> Self {
        Self {
            text,
            end: text.len(),
            next_number: line_count,
        }
    }

    fn next(&mut self) -> Option<TextLine<'a>> {
        if self.end == 0 {
            return None;
        }

        let logical_end = if self.text[self.end - 1] == b'\n' {
            self.end - 1
        } else {
            self.end
        };
        let mut start = logical_end;
        while start > 0 && self.text[start - 1] != b'\n' {
            start -= 1;
        }
        let result = TextLine {
            number: self.next_number,
            text: &self.text[start..logical_end],
        };
        self.end = start;
        self.next_number -= 1;
        Some(result)
    }
}

fn validate_preview_options(
    options: PreviewOptions,
) -> Result<(), BoundedPreviewError> {
    if options.max_lines == 0
        || options.max_lines > MAX_PREVIEW_LINES
        || options.max_indexed_lines == 0
        || options.max_indexed_lines > CANONICAL_MAX_INDEXED_LINES
        || options.max_matrix_cells == 0
        || options.max_matrix_cells > CANONICAL_MAX_MATRIX_CELLS
        || options.max_encoded_line_bytes == 0
        || options.max_encoded_line_bytes > MAX_ENCODED_LINE_BYTES
        || options.max_encoded_preview_bytes == 0
        || options.max_encoded_preview_bytes > MAX_ENCODED_PREVIEW_BYTES
    {
        return Err(BoundedPreviewError::InvalidPreviewOptions);
    }
    Ok(())
}

fn preview_uses_compute(
    old_line_count: usize,
    new_line_count: usize,
    options: PreviewOptions,
) -> Result<bool, BoundedPreviewError> {
    validate_preview_options(options)?;

    let indexed_lines = checked_add(old_line_count, new_line_count)?;
    if indexed_lines > options.max_indexed_lines {
        return Ok(false);
    }
    Ok(checked_mul(
        checked_add(old_line_count, 1)?,
        checked_add(new_line_count, 1)?,
    )? <= options.max_matrix_cells)
}

fn build_computed_preview(
    old_text: &[u8],
    new_text: &[u8],
    options: PreviewOptions,
) -> Result<BoundedDiffProjection, BoundedPreviewError> {
    let full_diff = compute(old_text, new_text);

    let mut additions: usize = 0;
    let mut deletions: usize = 0;
    for line in &full_diff {
        match line.op {
            LineOp::Equal => {}
            LineOp::Add => additions = checked_add(additions, 1)?,
            LineOp::Remove => deletions = checked_add(deletions, 1)?,
        }
    }

    let total_changed = checked_add(additions, deletions)?;
    let mut selected: [SelectedLine; MAX_PREVIEW_LINES] =
        std::array::from_fn(|_| SelectedLine::default());
    let selection = select_computed_lines(
        &full_diff,
        total_changed,
        options.max_lines,
        &mut selected,
    );

    materialize_projection(
        &selected[..selection.output_count],
        additions,
        deletions,
        selection.truncated,
        options,
    )
}

#[derive(Clone, Copy, Debug)]
struct ComputedSelection {
    output_count: usize,
    truncated: bool,
}

fn select_computed_lines(
    full_diff: &[DiffLine],
    total_changed: usize,
    max_lines: usize,
    selected: &mut [SelectedLine; MAX_PREVIEW_LINES],
) -> ComputedSelection {
    if total_changed == 0 {
        selected[0] = SelectedLine {
            op: PreviewOp::Notice,
            old_line: None,
            new_line: None,
            text: b"No changes".to_vec(),
        };
        return ComputedSelection {
            output_count: 1,
            truncated: false,
        };
    }
    if total_changed > max_lines {
        let plan = selection_plan(total_changed, max_lines);
        initialize_selection(selected, plan);
        let mut ordinal: usize = 0;
        for line in full_diff {
            let Some(projected) = selected_line_from_diff(line) else {
                continue;
            };
            if let Some(slot) =
                selection_slot(ordinal, total_changed, plan)
            {
                selected[slot] = projected;
            }
            ordinal += 1;
        }
        return ComputedSelection {
            output_count: plan.output_count,
            truncated: true,
        };
    }

    let mut first_changed: Option<usize> = None;
    let mut last_changed: usize = 0;
    let mut hunk_count: usize = 0;
    let mut in_hunk = false;
    for (index, line) in full_diff.iter().enumerate() {
        if line.op == LineOp::Equal {
            in_hunk = false;
            continue;
        }
        if !in_hunk {
            hunk_count += 1;
            in_hunk = true;
        }
        if first_changed.is_none() {
            first_changed = Some(index);
        }
        last_changed = index;
    }

    let first = first_changed.unwrap();
    let before = if first > 0 && full_diff[first - 1].op == LineOp::Equal {
        Some(first - 1)
    } else {
        None
    };
    let after = if last_changed + 1 < full_diff.len()
        && full_diff[last_changed + 1].op == LineOp::Equal
    {
        Some(last_changed + 1)
    } else {
        None
    };
    let context_slots = max_lines - total_changed;
    let include_both = hunk_count == 1
        && before.is_some()
        && after.is_some()
        && context_slots >= 2;
    let include_before = include_both
        || (hunk_count == 1
            && before.is_some()
            && after.is_none()
            && context_slots >= 1);
    let include_after = include_both
        || (hunk_count == 1
            && before.is_none()
            && after.is_some()
            && context_slots >= 1);

    let mut output_count: usize = 0;
    if include_before {
        selected[output_count] =
            selected_context_line(&full_diff[before.unwrap()]);
        output_count += 1;
    }
    for line in full_diff {
        let Some(projected) = selected_line_from_diff(line) else {
            continue;
        };
        selected[output_count] = projected;
        output_count += 1;
    }
    if include_after {
        selected[output_count] =
            selected_context_line(&full_diff[after.unwrap()]);
        output_count += 1;
    }
    ComputedSelection {
        output_count,
        truncated: false,
    }
}

fn selected_line_from_diff(line: &DiffLine) -> Option<SelectedLine> {
    match line.op {
        LineOp::Equal => None,
        LineOp::Add => Some(SelectedLine {
            op: PreviewOp::Addition,
            old_line: None,
            new_line: line.new_num,
            text: line.text.clone(),
        }),
        LineOp::Remove => Some(SelectedLine {
            op: PreviewOp::Deletion,
            old_line: line.old_num,
            new_line: None,
            text: line.text.clone(),
        }),
    }
}

fn selected_context_line(line: &DiffLine) -> SelectedLine {
    debug_assert_eq!(line.op, LineOp::Equal);
    SelectedLine {
        op: PreviewOp::Context,
        old_line: line.old_num,
        new_line: line.new_num,
        text: line.text.clone(),
    }
}

fn build_fallback_preview(
    old_text: &[u8],
    new_text: &[u8],
    old_marker: Option<&'static [u8]>,
    new_marker: Option<&'static [u8]>,
    options: PreviewOptions,
) -> Result<BoundedDiffProjection, BoundedPreviewError> {
    let old_normal_count = count_text_lines(old_text);
    let new_normal_count = count_text_lines(new_text);
    let comparable_count = old_normal_count.min(new_normal_count);

    let mut prefix_count: usize = 0;
    let mut old_forward = ForwardLineIterator::new(old_text);
    let mut new_forward = ForwardLineIterator::new(new_text);
    while prefix_count < comparable_count {
        let old_line = old_forward.next().unwrap();
        let new_line = new_forward.next().unwrap();
        if old_line.text != new_line.text {
            break;
        }
        prefix_count += 1;
    }

    let mut suffix_count: usize = 0;
    let mut old_reverse = ReverseLineIterator::new(old_text, old_normal_count);
    let mut new_reverse = ReverseLineIterator::new(new_text, new_normal_count);
    while prefix_count + suffix_count < comparable_count {
        let old_line = old_reverse.next().unwrap();
        let new_line = new_reverse.next().unwrap();
        if old_line.text != new_line.text {
            break;
        }
        suffix_count += 1;
    }

    let old_middle_count = old_normal_count - prefix_count - suffix_count;
    let new_middle_count = new_normal_count - prefix_count - suffix_count;
    let deletions = checked_add(
        old_middle_count,
        old_marker.is_some() as usize,
    )?;
    let additions = checked_add(
        new_middle_count,
        new_marker.is_some() as usize,
    )?;
    let total_changed = checked_add(additions, deletions)?;
    let plan = selection_plan(total_changed, options.max_lines);
    let mut selected: [SelectedLine; MAX_PREVIEW_LINES] =
        std::array::from_fn(|_| SelectedLine::default());
    initialize_selection(&mut selected, plan);

    let mut ordinal: usize = 0;
    let mut old_forward = ForwardLineIterator::new(old_text);
    for _ in 0..prefix_count {
        old_forward.next();
    }
    for _ in 0..old_middle_count {
        let line = old_forward.next().unwrap();
        if let Some(slot) = selection_slot(ordinal, total_changed, plan) {
            selected[slot] = SelectedLine {
                op: PreviewOp::Deletion,
                old_line: projected_line_number(line.number),
                new_line: None,
                text: line.text.to_vec(),
            };
        }
        ordinal += 1;
    }

    let mut new_forward = ForwardLineIterator::new(new_text);
    for _ in 0..prefix_count {
        new_forward.next();
    }
    for _ in 0..new_middle_count {
        let line = new_forward.next().unwrap();
        if let Some(slot) = selection_slot(ordinal, total_changed, plan) {
            selected[slot] = SelectedLine {
                op: PreviewOp::Addition,
                old_line: None,
                new_line: projected_line_number(line.number),
                text: line.text.to_vec(),
            };
        }
        ordinal += 1;
    }

    if let Some(marker) = old_marker {
        if let Some(slot) = selection_slot(ordinal, total_changed, plan) {
            selected[slot] = SelectedLine {
                op: PreviewOp::Deletion,
                old_line: projected_line_number(
                    checked_add(old_normal_count, 1)?,
                ),
                new_line: None,
                text: marker.to_vec(),
            };
        }
        ordinal += 1;
    }
    if let Some(marker) = new_marker {
        if let Some(slot) = selection_slot(ordinal, total_changed, plan) {
            selected[slot] = SelectedLine {
                op: PreviewOp::Addition,
                old_line: None,
                new_line: projected_line_number(
                    checked_add(new_normal_count, 1)?,
                ),
                text: marker.to_vec(),
            };
        }
    }

    materialize_projection(
        &selected[..plan.output_count],
        additions,
        deletions,
        plan.truncated,
        options,
    )
}

fn materialize_projection(
    selected: &[SelectedLine],
    additions: usize,
    deletions: usize,
    initially_truncated: bool,
    options: PreviewOptions,
) -> Result<BoundedDiffProjection, BoundedPreviewError> {
    if selected.len() > options.max_lines {
        return Err(BoundedPreviewError::PreviewBudgetExceeded);
    }

    let mut lines: Vec<ProjectedLine> = Vec::with_capacity(selected.len());
    let mut encoded_bytes: usize = 0;
    let mut truncated = initially_truncated;
    let mut retained_additions: usize = 0;
    let mut retained_deletions: usize = 0;
    for source in selected {
        if source.op == PreviewOp::Addition {
            retained_additions += 1;
        }
        if source.op == PreviewOp::Deletion {
            retained_deletions += 1;
        }
    }
    let omitted_additions = additions - retained_additions;
    let omitted_deletions = deletions - retained_deletions;
    for source in selected {
        let remaining = options.max_encoded_preview_bytes - encoded_bytes;
        let row_limit = options.max_encoded_line_bytes.min(remaining);
        let elision_text;
        let source_text: &[u8] = if source.op == PreviewOp::Elision {
            elision_text =
                format_elision_text(omitted_additions, omitted_deletions);
            elision_text.as_bytes()
        } else {
            &source.text
        };
        let encoded = text_utils::encode_terminal_safe(source_text, row_limit);
        lines.push(ProjectedLine {
            op: source.op,
            old_line: source.old_line,
            new_line: source.new_line,
            text: encoded.bytes,
        });
        encoded_bytes += lines.last().unwrap().text.len();
        truncated = truncated || encoded.truncated;
    }

    Ok(BoundedDiffProjection {
        lines,
        additions,
        deletions,
        truncated,
    })
}

fn selection_plan(total_changed: usize, max_lines: usize) -> SelectionPlan {
    if total_changed == 0 {
        return SelectionPlan {
            output_count: 1,
            head_count: 0,
            tail_count: 0,
            truncated: false,
        };
    }
    if total_changed <= max_lines {
        return SelectionPlan {
            output_count: total_changed,
            head_count: total_changed,
            tail_count: 0,
            truncated: false,
        };
    }

    let retained_lines = max_lines - 1;
    let head_count = (retained_lines + 1) / 2;
    SelectionPlan {
        output_count: max_lines,
        head_count,
        tail_count: retained_lines - head_count,
        truncated: true,
    }
}

fn initialize_selection(
    selected: &mut [SelectedLine; MAX_PREVIEW_LINES],
    plan: SelectionPlan,
) {
    if plan.output_count == 1 && plan.head_count == 0 && !plan.truncated {
        selected[0] = SelectedLine {
            op: PreviewOp::Notice,
            old_line: None,
            new_line: None,
            text: b"No changes".to_vec(),
        };
    } else if plan.truncated {
        selected[plan.head_count] = SelectedLine {
            op: PreviewOp::Elision,
            old_line: None,
            new_line: None,
            text: Vec::new(),
        };
    }
}

fn format_elision_text(
    omitted_additions: usize,
    omitted_deletions: usize,
) -> String {
    debug_assert!(omitted_additions > 0 || omitted_deletions > 0);
    if omitted_additions > 0 && omitted_deletions > 0 {
        return format!(
            "⋯ +{omitted_additions} -{omitted_deletions} omitted"
        );
    }
    let (prefix, omitted) = if omitted_additions > 0 {
        ("⋯ +", omitted_additions)
    } else {
        ("⋯ -", omitted_deletions)
    };
    format!("{prefix}{omitted} omitted")
}

fn selection_slot(
    ordinal: usize,
    total_changed: usize,
    plan: SelectionPlan,
) -> Option<usize> {
    if !plan.truncated {
        return Some(ordinal);
    }
    if ordinal < plan.head_count {
        return Some(ordinal);
    }

    let tail_start = total_changed - plan.tail_count;
    if ordinal >= tail_start {
        return Some(plan.head_count + 1 + ordinal - tail_start);
    }
    None
}

fn count_text_lines(text: &[u8]) -> usize {
    if text.is_empty() {
        return 0;
    }
    let mut count: usize = 0;
    for &byte in text {
        if byte == b'\n' {
            count += 1;
        }
    }
    if text[text.len() - 1] != b'\n' {
        count += 1;
    }
    count
}

fn index_line_starts(text: &[u8]) -> Vec<usize> {
    let mut starts = vec![0usize; count_text_lines(text)];
    let mut cursor: usize = 0;
    for start in &mut starts {
        *start = cursor;
        cursor = match text[cursor..].iter().position(|&b| b == b'\n') {
            Some(newline) => cursor + newline + 1,
            None => text.len(),
        };
    }
    starts
}

fn indexed_text_line<'a>(
    text: &'a [u8],
    starts: &[usize],
    index: usize,
) -> TextLine<'a> {
    let start = starts[index];
    let end = if index + 1 < starts.len() {
        starts[index + 1] - 1
    } else if !text.is_empty() && text[text.len() - 1] == b'\n' {
        text.len() - 1
    } else {
        text.len()
    };
    TextLine {
        number: index + 1,
        text: &text[start..end],
    }
}

fn trailing_newline_marker(
    old_text: &[u8],
    new_text: &[u8],
    old_side: bool,
) -> Option<&'static [u8]> {
    if old_text.is_empty() || new_text.is_empty() {
        return None;
    }
    let old_has_trailing_newline = old_text.ends_with(b"\n");
    let new_has_trailing_newline = new_text.ends_with(b"\n");
    if old_has_trailing_newline == new_has_trailing_newline {
        return None;
    }
    if old_side && old_has_trailing_newline {
        return Some(TRAILING_NEWLINE_REMOVED_MARKER);
    }
    if !old_side && new_has_trailing_newline {
        return Some(TRAILING_NEWLINE_ADDED_MARKER);
    }
    None
}

fn checked_add(a: usize, b: usize) -> Result<usize, BoundedPreviewError> {
    a.checked_add(b).ok_or(BoundedPreviewError::ArithmeticOverflow)
}

fn checked_mul(a: usize, b: usize) -> Result<usize, BoundedPreviewError> {
    a.checked_mul(b).ok_or(BoundedPreviewError::ArithmeticOverflow)
}

fn projected_line_number(number: usize) -> Option<u32> {
    u32::try_from(number).ok()
}

fn append_lines(text: &[u8]) -> Vec<&[u8]> {
    let mut lines = Vec::new();
    let mut start: usize = 0;
    for (index, &byte) in text.iter().enumerate() {
        if byte == b'\n' {
            lines.push(&text[start..index]);
            start = index + 1;
        }
    }
    if start < text.len() {
        lines.push(&text[start..]);
    }
    lines
}

fn append_trailing_newline_marker(
    old_lines: &mut Vec<&[u8]>,
    new_lines: &mut Vec<&[u8]>,
    old_text: &[u8],
    new_text: &[u8],
) {
    if old_text.is_empty() || new_text.is_empty() {
        return;
    }
    let old_has_trailing_newline = old_text.ends_with(b"\n");
    let new_has_trailing_newline = new_text.ends_with(b"\n");
    if old_has_trailing_newline == new_has_trailing_newline {
        return;
    }

    if old_has_trailing_newline {
        old_lines.push(TRAILING_NEWLINE_REMOVED_MARKER);
    } else {
        new_lines.push(TRAILING_NEWLINE_ADDED_MARKER);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[u8]) -> String {
        String::from_utf8(v.to_vec()).unwrap()
    }

    #[test]
    fn compute_identical_inputs_produce_only_equal_ops() {
        let diff = compute(b"a\nb\nc\n", b"a\nb\nc\n");
        assert_eq!(diff.len(), 3);
        for line in &diff {
            assert_eq!(line.op, LineOp::Equal);
        }
        assert_eq!(count_stats(&diff), Stats::default());
    }

    #[test]
    fn compute_pure_insert_in_the_middle() {
        let diff = compute(b"a\nb\n", b"a\nX\nb\n");
        assert_eq!(diff.len(), 3);
        assert_eq!(diff[0].op, LineOp::Equal);
        assert_eq!(diff[1].op, LineOp::Add);
        assert_eq!(diff[1].text, b"X");
        assert_eq!(diff[1].old_num, None);
        assert_eq!(diff[1].new_num, Some(2));
        assert_eq!(diff[2].op, LineOp::Equal);
        assert_eq!(
            count_stats(&diff),
            Stats {
                additions: 1,
                ..Default::default()
            }
        );
    }

    #[test]
    fn compute_pure_delete() {
        let diff = compute(b"a\nb\nc\n", b"a\nc\n");
        assert_eq!(diff.len(), 3);
        assert_eq!(diff[1].op, LineOp::Remove);
        assert_eq!(diff[1].text, b"b");
        assert_eq!(diff[1].old_num, Some(2));
        assert_eq!(diff[1].new_num, None);
        assert_eq!(
            count_stats(&diff),
            Stats {
                deletions: 1,
                ..Default::default()
            }
        );
    }

    #[test]
    fn compute_replace_single_line() {
        let diff = compute(b"a\nold\nc\n", b"a\nnew\nc\n");
        assert_eq!(diff.len(), 4);
        assert_eq!(diff[1].op, LineOp::Remove);
        assert_eq!(diff[1].text, b"old");
        assert_eq!(diff[2].op, LineOp::Add);
        assert_eq!(diff[2].text, b"new");
        assert_eq!(
            count_stats(&diff),
            Stats {
                additions: 1,
                deletions: 1
            }
        );
    }

    #[test]
    fn compute_trailing_newline_only_changes_are_visible() {
        let removed = compute(b"a\n", b"a");
        assert_eq!(
            count_stats(&removed),
            Stats {
                deletions: 1,
                ..Default::default()
            }
        );
        assert_eq!(removed[1].op, LineOp::Remove);
        assert_eq!(removed[1].text, TRAILING_NEWLINE_REMOVED_MARKER);

        let added = compute(b"a", b"a\n");
        assert_eq!(
            count_stats(&added),
            Stats {
                additions: 1,
                ..Default::default()
            }
        );
        assert_eq!(added[1].op, LineOp::Add);
        assert_eq!(added[1].text, TRAILING_NEWLINE_ADDED_MARKER);
    }

    #[test]
    fn compute_empty_old_new_file_creation() {
        let diff = compute(b"", b"first\nsecond\n");
        assert_eq!(diff.len(), 2);
        assert_eq!(diff[0].op, LineOp::Add);
        assert_eq!(diff[0].new_num, Some(1));
        assert_eq!(diff[1].op, LineOp::Add);
        assert_eq!(diff[1].new_num, Some(2));
        assert_eq!(
            count_stats(&diff),
            Stats {
                additions: 2,
                ..Default::default()
            }
        );
    }

    #[test]
    fn compute_line_numbers_increment_monotonically() {
        let diff = compute(b"a\nb\nc\n", b"a\nX\nb\nc\n");
        assert_eq!(diff.len(), 4);
        assert_eq!(diff[0].old_num, Some(1));
        assert_eq!(diff[0].new_num, Some(1));
        assert_eq!(diff[1].old_num, None);
        assert_eq!(diff[1].new_num, Some(2));
        assert_eq!(diff[2].old_num, Some(2));
        assert_eq!(diff[2].new_num, Some(3));
        assert_eq!(diff[3].old_num, Some(3));
        assert_eq!(diff[3].new_num, Some(4));
    }

    #[test]
    fn format_unified_rows_contain_signs_for_changed_lines() {
        let diff = compute(b"a\nold\nc\n", b"a\nnew\nc\n");
        let out = format_unified(&diff, FormatStyles::default(), None, EditKind::Edited);
        assert!(s(&out).contains("+ new"));
        assert!(s(&out).contains("- old"));
    }

    #[test]
    fn format_unified_marker_accent_colors_only_the_number_and_sign() {
        let diff = compute(b"a\nold\nc\n", b"a\nnew\nc\n");
        let styles = FormatStyles {
            added_marker_fg: b"[G]",
            removed_marker_fg: b"[R]",
            ..FormatStyles::default()
        };
        let out = format_unified(&diff, styles, None, EditKind::Edited);
        let text = s(&out);
        assert!(text.contains("[G]2 +"));
        assert!(text.contains("[R]2 -"));
        assert!(text.contains("+\x1b[38;5;252m new"));
        assert!(text.contains("-\x1b[38;5;252m old"));
        assert!(!text.contains("[G]2 + new"));
    }

    #[test]
    fn format_unified_trailing_newline_only_edits_render_markers() {
        let diff = compute(b"a\n", b"a");
        let out = format_unified(
            &diff,
            FormatStyles::default(),
            Some(b"file.txt"),
            EditKind::Edited,
        );
        assert!(s(&out).contains("- (trailing newline removed)"));
    }

    #[test]
    fn format_unified_gutter_width_fits_largest_line_number() {
        let mut old_buf = Vec::new();
        let mut new_buf = Vec::new();
        for index in 1..=12u32 {
            old_buf.extend_from_slice(format!("line{index}\n").as_bytes());
            new_buf.extend_from_slice(format!("line{index}\n").as_bytes());
        }
        new_buf.extend_from_slice(b"extra\n");

        let diff = compute(&old_buf, &new_buf);
        let out = format_unified(&diff, FormatStyles::default(), None, EditKind::Edited);
        assert!(s(&out).contains("13 + extra"));
    }

    #[test]
    fn format_unified_only_emits_changed_hunks_with_limited_context() {
        let mut old_buf = Vec::new();
        let mut new_buf = Vec::new();
        for index in 1..=20u32 {
            let old_line = if index == 10 { "old-line-10\n" } else { "same\n" };
            let new_line = if index == 10 { "new-line-10\n" } else { "same\n" };
            old_buf.extend_from_slice(old_line.as_bytes());
            new_buf.extend_from_slice(new_line.as_bytes());
        }

        let diff = compute(&old_buf, &new_buf);
        let out = format_unified(&diff, FormatStyles::default(), None, EditKind::Edited);
        let text = s(&out);
        assert!(text.contains("+ new-line-10"));
        assert!(text.contains("- old-line-10"));
        assert!(!text.contains(" 1   same"));
        assert!(!text.contains("20   same"));
    }

    #[test]
    fn format_unified_single_line_number_column() {
        let diff = compute(b"a\nold\nc\n", b"a\nnew\nc\n");
        let out = format_unified(&diff, FormatStyles::default(), None, EditKind::Edited);
        assert!(!s(&out).contains("1 1 "));
    }

    #[test]
    fn free_diff_entry_payload_releases_all_owned_buffers() {
        let payload = DiffEntryPayload {
            preview: b"preview".to_vec(),
            additions: 0,
            deletions: 0,
            full: Some(FullDiff {
                content: b"full".to_vec(),
                lifecycle_id: types::ToolLifecycleId {
                    turn_id: 1,
                    call_id: b"call".to_vec(),
                },
            }),
        };
        free_diff_entry_payload(payload);
    }

    #[test]
    fn format_unified_emits_diff_lines_without_header() {
        let diff = compute(b"a\nold\nc\n", b"a\nnew\nc\n");
        let out = format_unified(
            &diff,
            FormatStyles::default(),
            Some(b"src/app/page.tsx"),
            EditKind::Added,
        );
        assert!(s(&out).contains("+ new"));
        assert!(s(&out).contains("- old"));
    }

    #[test]
    fn format_unified_separated_hunks_emit_vertical_elision_row() {
        let mut old_buf = Vec::new();
        let mut new_buf = Vec::new();
        for index in 1..=16u32 {
            let old_line = if index == 3 {
                "old-line-3\n"
            } else if index == 14 {
                "old-line-14\n"
            } else {
                "same\n"
            };
            let new_line = if index == 3 {
                "new-line-3\n"
            } else if index == 14 {
                "new-line-14\n"
            } else {
                "same\n"
            };
            old_buf.extend_from_slice(old_line.as_bytes());
            new_buf.extend_from_slice(new_line.as_bytes());
        }

        let diff = compute(&old_buf, &new_buf);
        let out = format_unified(&diff, FormatStyles::default(), None, EditKind::Edited);
        let text = s(&out);
        assert!(text.contains('⋯'));
        assert!(text.contains("+ new-line-3"));
    }

    #[test]
    fn persisted_edit_emits_one_canonical_preview_without_a_full_snapshot() {
        let presentation = types::CommittedFilePresentation {
            path: b"existing.md".to_vec(),
            kind: types::CommittedFilePresentationKind::Edited,
            lines: vec![types::CommittedFilePresentationLine {
                kind: types::CommittedFilePresentationLineKind::Addition,
                old_line: None,
                new_line: Some(1),
                text: b"changed".to_vec(),
            }],
            additions: 1,
            deletions: 0,
            truncated: false,
            previous_content: None,
            after_content: None,
            lifecycle_id: None,
        };
        let payload =
            format_persisted_file_change_payload(&presentation, FormatStyles::default());

        assert!(s(&payload.preview).contains("changed"));
        assert_eq!(payload.additions, 1);
        assert_eq!(payload.deletions, 0);
        assert!(payload.full.is_none());
    }

    #[test]
    fn wrap_with_markers_surrounds_content_with_start_end_osc_markers() {
        let out = wrap_with_markers(7, b"hello\n");
        assert_eq!(out, b"\x1b]9050;7\x07hello\n\x1b]9051;7\x07");
    }

    #[test]
    fn diff_entry_deinit_releases_owned_full_detail() {
        let mut entry = DiffEntry {
            id: 1,
            full: Some(FullDiff {
                content: b"full".to_vec(),
                lifecycle_id: types::ToolLifecycleId {
                    turn_id: 1,
                    call_id: b"call".to_vec(),
                },
            }),
        };
        entry.deinit();
        assert!(entry.full.is_none());
    }

    #[test]
    fn build_bounded_preview_preserves_admitted_compute_ordering_and_line_numbers() {
        let cases = [
            (b"a\nb\na\n".as_slice(), b"a\na\nb\n".as_slice()),
            (b"line\n".as_slice(), b"line".as_slice()),
        ];

        for (old_text, new_text) in cases {
            let full_diff = compute(old_text, new_text);
            let mut preview =
                build_bounded_preview(old_text, new_text, PreviewOptions::default())
                    .unwrap();

            let stats = count_stats(&full_diff);
            assert_eq!(preview.additions, stats.additions as usize);
            assert_eq!(preview.deletions, stats.deletions as usize);

            let mut projected_index: usize = 0;
            for line in &full_diff {
                if line.op == LineOp::Equal {
                    continue;
                }
                while projected_index < preview.lines.len()
                    && preview.lines[projected_index].op == PreviewOp::Context
                {
                    projected_index += 1;
                }
                let projected = &preview.lines[projected_index];
                assert_eq!(
                    projected.op,
                    match line.op {
                        LineOp::Equal => unreachable!(),
                        LineOp::Add => PreviewOp::Addition,
                        LineOp::Remove => PreviewOp::Deletion,
                    }
                );
                assert_eq!(projected.old_line, line.old_num);
                assert_eq!(projected.new_line, line.new_num);
                assert_eq!(projected.text, line.text);
                projected_index += 1;
            }
            while projected_index < preview.lines.len() {
                assert_eq!(
                    preview.lines[projected_index].op,
                    PreviewOp::Context
                );
                projected_index += 1;
            }

            preview.deinit();
        }
    }

    #[test]
    fn build_bounded_preview_includes_symmetric_context_for_one_exact_hunk() {
        let mut preview = build_bounded_preview(
            b"alpha\nbefore\nomega\n",
            b"alpha\nafter\nomega\n",
            PreviewOptions::default(),
        )
        .unwrap();

        assert_eq!(preview.lines.len(), 4);
        assert_eq!(preview.lines[0].op, PreviewOp::Context);
        assert_eq!(preview.lines[0].old_line, Some(1));
        assert_eq!(preview.lines[0].new_line, Some(1));
        assert_eq!(preview.lines[0].text, b"alpha");
        assert_eq!(preview.lines[1].op, PreviewOp::Deletion);
        assert_eq!(preview.lines[1].text, b"before");
        assert_eq!(preview.lines[2].op, PreviewOp::Addition);
        assert_eq!(preview.lines[2].text, b"after");
        assert_eq!(preview.lines[3].op, PreviewOp::Context);
        assert_eq!(preview.lines[3].old_line, Some(3));
        assert_eq!(preview.lines[3].new_line, Some(3));
        assert_eq!(preview.lines[3].text, b"omega");
        assert!(!preview.truncated);

        preview.deinit();
    }

    #[test]
    fn build_bounded_preview_includes_one_exact_context_only_at_a_file_boundary() {
        let cases = [
            (
                b"before\nomega\n".as_slice(),
                b"after\nomega\n".as_slice(),
                2usize,
                b"omega".as_slice(),
            ),
            (
                b"alpha\nbefore\n".as_slice(),
                b"alpha\nafter\n".as_slice(),
                0usize,
                b"alpha".as_slice(),
            ),
        ];
        for (old_text, new_text, context_index, context_text) in cases {
            let mut preview = build_bounded_preview(
                old_text,
                new_text,
                PreviewOptions::default(),
            )
            .unwrap();
            assert_eq!(preview.lines.len(), 3);
            assert_eq!(preview.lines[context_index].op, PreviewOp::Context);
            assert_eq!(preview.lines[context_index].text, context_text);
            preview.deinit();
        }
    }

    #[test]
    fn build_bounded_preview_leaves_a_lone_interior_context_slot_unused() {
        let mut preview = build_bounded_preview(
            b"alpha\nbefore\nomega\n",
            b"alpha\nafter\nomega\n",
            PreviewOptions {
                max_lines: 3,
                ..PreviewOptions::default()
            },
        )
        .unwrap();

        assert_eq!(preview.lines.len(), 2);
        for line in &preview.lines {
            assert_ne!(line.op, PreviewOp::Context);
        }
        preview.deinit();
    }

    #[test]
    fn build_bounded_preview_does_not_synthesize_context_for_multiple_hunks() {
        let mut preview = build_bounded_preview(
            b"alpha\nbefore\nmiddle\nold\nomega\n",
            b"alpha\nafter\nmiddle\nnew\nomega\n",
            PreviewOptions::default(),
        )
        .unwrap();

        assert_eq!(preview.lines.len(), 4);
        for line in &preview.lines {
            assert_ne!(line.op, PreviewOp::Context);
        }
        preview.deinit();
    }

    #[test]
    fn build_bounded_preview_bounds_selected_rows_with_explicit_elision() {
        let mut preview = build_bounded_preview(
            b"a\nb\nc\nd\ne\n",
            b"v\nw\nx\ny\nz\n",
            PreviewOptions::default(),
        )
        .unwrap();

        assert_eq!(preview.additions, 5);
        assert_eq!(preview.deletions, 5);
        assert_eq!(preview.lines.len(), 6);
        assert!(preview.truncated);

        let elisions = preview
            .lines
            .iter()
            .filter(|line| line.op == PreviewOp::Elision)
            .count();
        assert_eq!(elisions, 1);

        for line in &preview.lines {
            if line.op != PreviewOp::Elision {
                continue;
            }
            let mut retained_additions = 0usize;
            let mut retained_deletions = 0usize;
            for retained in &preview.lines {
                if retained.op == PreviewOp::Addition {
                    retained_additions += 1;
                }
                if retained.op == PreviewOp::Deletion {
                    retained_deletions += 1;
                }
            }
            let expected = format!(
                "⋯ +{} -{} omitted",
                preview.additions - retained_additions,
                preview.deletions - retained_deletions
            );
            assert_eq!(line.text, expected.as_bytes());
        }
        preview.deinit();
    }

    #[test]
    fn file_review_exposes_every_changed_line_beyond_the_bounded_preview_cap() {
        let mut after = Vec::new();
        for line_number in 1..31u32 {
            after.extend_from_slice(format!("line {line_number}\n").as_bytes());
        }

        let mut review = FileReview::init(b"", &after).unwrap();
        let mut viewport = review.viewport(24, 6);

        assert_eq!(review.row_count(), 30);
        assert_eq!(viewport.lines.len(), 6);
        assert_eq!(viewport.lines[0].new_line, Some(25));
        assert_eq!(viewport.lines[0].text, b"line 25");
        assert_eq!(viewport.lines[5].new_line, Some(30));
        assert_eq!(viewport.lines[5].text, b"line 30");

        viewport.deinit();
        review.deinit();
    }

    #[test]
    fn fallback_file_review_seeks_to_replacement_boundaries() {
        let mut before = Vec::new();
        let mut after = Vec::new();
        for line_number in 1..2049u32 {
            before.extend_from_slice(
                format!("old-{line_number:04}\n").as_bytes(),
            );
            after.extend_from_slice(
                format!("new-{line_number:04}\n").as_bytes(),
            );
        }

        let mut review = FileReview::init(&before, &after).unwrap();
        assert_eq!(review.row_count(), 4096);

        let mut middle = review.viewport(2047, 2);
        assert_eq!(middle.lines[0].op, PreviewOp::Deletion);
        assert_eq!(middle.lines[0].text, b"old-2048");
        assert_eq!(middle.lines[1].op, PreviewOp::Addition);
        assert_eq!(middle.lines[1].text, b"new-0001");

        let tail_start = review.row_count() - 2;
        let mut tail = review.viewport(tail_start, 2);
        assert_eq!(tail.lines[0].text, b"new-2047");
        assert_eq!(tail.lines[1].text, b"new-2048");

        middle.deinit();
        tail.deinit();
        review.deinit();
    }

    #[test]
    fn file_review_projects_five_context_lines_and_exact_computed_elisions() {
        let before = concat!(
            "lead-01\nlead-02\nlead-03\nlead-04\nlead-05\nlead-06\nlead-07\nlead-08\n",
            "old-one\n",
            "middle-01\nmiddle-02\nmiddle-03\nmiddle-04\nmiddle-05\nmiddle-06\n",
            "middle-07\nmiddle-08\nmiddle-09\nmiddle-10\nmiddle-11\nmiddle-12\n",
            "old-two\n",
            "tail-01\ntail-02\ntail-03\ntail-04\ntail-05\ntail-06\ntail-07\ntail-08\n",
        );
        let after = concat!(
            "lead-01\nlead-02\nlead-03\nlead-04\nlead-05\nlead-06\nlead-07\nlead-08\n",
            "new-one\n",
            "middle-01\nmiddle-02\nmiddle-03\nmiddle-04\nmiddle-05\nmiddle-06\n",
            "middle-07\nmiddle-08\nmiddle-09\nmiddle-10\nmiddle-11\nmiddle-12\n",
            "new-two\n",
            "tail-01\ntail-02\ntail-03\ntail-04\ntail-05\ntail-06\ntail-07\ntail-08\n",
        );

        let mut review = FileReview::init(before.as_bytes(), after.as_bytes()).unwrap();
        assert_eq!(review.row_count(), 27);

        let mut rows = review.viewport(0, review.row_count());
        assert_eq!(rows.lines[0].op, PreviewOp::Elision);
        assert_eq!(rows.lines[0].elision_count, 3);
        assert_eq!(rows.lines[0].text, b"");

        for index in 0..5 {
            let line = &rows.lines[index + 1];
            assert_eq!(line.op, PreviewOp::Context);
            assert_eq!(line.old_line, Some((index + 4) as u32));
            assert_eq!(line.new_line, Some((index + 4) as u32));
        }
        assert_eq!(rows.lines[1].text, b"lead-04");
        assert_eq!(rows.lines[5].text, b"lead-08");
        assert_eq!(rows.lines[6].op, PreviewOp::Deletion);
        assert_eq!(rows.lines[6].text, b"old-one");
        assert_eq!(rows.lines[7].op, PreviewOp::Addition);
        assert_eq!(rows.lines[7].text, b"new-one");

        assert_eq!(rows.lines[8].text, b"middle-01");
        assert_eq!(rows.lines[12].text, b"middle-05");
        assert_eq!(rows.lines[13].op, PreviewOp::Elision);
        assert_eq!(rows.lines[13].elision_count, 2);
        assert_eq!(rows.lines[14].text, b"middle-08");
        assert_eq!(rows.lines[18].text, b"middle-12");
        assert_eq!(rows.lines[21].text, b"tail-01");
        assert_eq!(rows.lines[25].text, b"tail-05");
        assert_eq!(rows.lines[26].op, PreviewOp::Elision);
        assert_eq!(rows.lines[26].elision_count, 3);

        rows.deinit();
        review.deinit();
    }

    #[test]
    fn file_review_projects_only_proven_fallback_context_with_exact_elisions() {
        let mut before = Vec::new();
        let mut after = Vec::new();
        for line_number in 1..1001u32 {
            let line = format!("line-{line_number:04}\n");
            before.extend_from_slice(line.as_bytes());
            after.extend_from_slice(line.as_bytes());
            if line_number == 500 {
                after.extend_from_slice(b"inserted-context-a\ninserted-context-b\n");
            }
        }

        let mut review = FileReview::init(&before, &after).unwrap();
        assert_eq!(review.row_count(), 14);

        let mut rows = review.viewport(0, review.row_count());
        assert_eq!(rows.lines[0].op, PreviewOp::Elision);
        assert_eq!(rows.lines[0].elision_count, 495);
        assert_eq!(rows.lines[1].text, b"line-0496");
        assert_eq!(rows.lines[5].text, b"line-0500");
        assert_eq!(rows.lines[6].op, PreviewOp::Addition);
        assert_eq!(rows.lines[6].new_line, Some(501));
        assert_eq!(rows.lines[6].text, b"inserted-context-a");
        assert_eq!(rows.lines[7].op, PreviewOp::Addition);
        assert_eq!(rows.lines[7].new_line, Some(502));
        assert_eq!(rows.lines[7].text, b"inserted-context-b");
        assert_eq!(rows.lines[8].text, b"line-0501");
        assert_eq!(rows.lines[8].old_line, Some(501));
        assert_eq!(rows.lines[8].new_line, Some(503));
        assert_eq!(rows.lines[12].text, b"line-0505");
        assert_eq!(rows.lines[13].op, PreviewOp::Elision);
        assert_eq!(rows.lines[13].elision_count, 495);

        rows.deinit();
        review.deinit();
    }

    #[test]
    fn file_review_preserves_replacements_and_trailing_newline_markers() {
        let mut replacement = FileReview::init(b"old\n", b"new\n").unwrap();
        let mut replacement_rows =
            replacement.viewport(0, replacement.row_count());
        assert_eq!(replacement_rows.lines.len(), 2);
        assert_eq!(replacement_rows.lines[0].op, PreviewOp::Deletion);
        assert_eq!(replacement_rows.lines[0].text, b"old");
        assert_eq!(replacement_rows.lines[1].op, PreviewOp::Addition);
        assert_eq!(replacement_rows.lines[1].text, b"new");

        let mut newline_change = FileReview::init(b"a\n", b"a").unwrap();
        let mut newline_rows =
            newline_change.viewport(0, newline_change.row_count());
        assert_eq!(newline_rows.lines.len(), 2);
        assert_eq!(newline_rows.lines[0].op, PreviewOp::Context);
        assert_eq!(newline_rows.lines[0].text, b"a");
        assert_eq!(newline_rows.lines[1].op, PreviewOp::Deletion);
        assert_eq!(
            newline_rows.lines[1].text,
            TRAILING_NEWLINE_REMOVED_MARKER
        );

        replacement_rows.deinit();
        newline_rows.deinit();
        replacement.deinit();
        newline_change.deinit();
    }

    #[test]
    fn build_bounded_preview_elision_reports_one_sided_omitted_polarity() {
        let cases = [
            (
                b"".as_slice(),
                b"1\n2\n3\n4\n5\n6\n7\n".as_slice(),
                "⋯ +2 omitted",
            ),
            (
                b"1\n2\n3\n4\n5\n6\n7\n".as_slice(),
                b"".as_slice(),
                "⋯ -2 omitted",
            ),
        ];
        for (old_text, new_text, expected) in cases {
            let mut preview = build_bounded_preview(
                old_text,
                new_text,
                PreviewOptions::default(),
            )
            .unwrap();
            let mut found = false;
            for line in &preview.lines {
                if line.op != PreviewOp::Elision {
                    continue;
                }
                found = true;
                assert_eq!(line.text, expected.as_bytes());
            }
            assert!(found);
            preview.deinit();
        }
    }

    #[test]
    fn build_bounded_preview_rejects_lcs_matrix_amplification_from_one_byte_lines() {
        let line_count = 1001usize;
        let mut old_text = Vec::new();
        let mut new_text = Vec::new();
        for _ in 0..line_count {
            old_text.extend_from_slice(b"a\n");
            new_text.extend_from_slice(b"b\n");
        }

        let mut preview = build_bounded_preview(
            &old_text,
            &new_text,
            PreviewOptions::default(),
        )
        .unwrap();
        assert_eq!(preview.additions, line_count);
        assert_eq!(preview.deletions, line_count);
        preview.deinit();
    }

    #[test]
    fn build_bounded_preview_streams_fallback_without_materializing_changed_middle() {
        let line_count = 20_000usize;
        let mut old_text = Vec::new();
        let mut new_text = Vec::new();
        for _ in 0..line_count {
            old_text.extend_from_slice(b"a\n");
            new_text.extend_from_slice(b"b\n");
        }

        let mut preview = build_bounded_preview(
            &old_text,
            &new_text,
            PreviewOptions::default(),
        )
        .unwrap();
        assert_eq!(preview.additions, line_count);
        assert_eq!(preview.deletions, line_count);
        assert!(preview.lines.len() <= 6);
        assert!(preview.truncated);
        preview.deinit();
    }

    #[test]
    fn build_bounded_preview_fallback_retains_exact_common_prefix_and_suffix_counts() {
        let mut preview = build_bounded_preview(
            b"same\nold-a\nold-b\nlast\n",
            b"same\nnew-a\nlast\n",
            PreviewOptions {
                max_matrix_cells: 1,
                ..PreviewOptions::default()
            },
        )
        .unwrap();

        assert_eq!(preview.additions, 1);
        assert_eq!(preview.deletions, 2);
        assert_eq!(preview.lines.len(), 3);
        assert_eq!(preview.lines[0].text, b"old-a");
        assert_eq!(preview.lines[1].text, b"old-b");
        assert_eq!(preview.lines[2].text, b"new-a");
        for line in &preview.lines {
            assert_ne!(line.op, PreviewOp::Context);
        }
        preview.deinit();
    }

    #[test]
    fn build_bounded_preview_fallback_keeps_informative_elision_without_context() {
        let mut preview = build_bounded_preview(
            b"same\n1\n2\n3\n4\n5\n6\nlast\n",
            b"same\na\nb\nc\nd\ne\nf\nlast\n",
            PreviewOptions {
                max_matrix_cells: 1,
                ..PreviewOptions::default()
            },
        )
        .unwrap();

        let mut elision_count = 0usize;
        for line in &preview.lines {
            assert_ne!(line.op, PreviewOp::Context);
            if line.op == PreviewOp::Elision {
                elision_count += 1;
                assert!(s(&line.text).contains("omitted"));
                assert!(s(&line.text).contains('+'));
                assert!(s(&line.text).contains('-'));
            }
        }
        assert_eq!(elision_count, 1);
        preview.deinit();
    }

    #[test]
    fn build_bounded_preview_fallback_never_treats_an_aligned_middle_as_context() {
        let mut preview = build_bounded_preview(
            b"start\nold-a\nmiddle\nold-b\nend\n",
            b"start\nnew-a\nmiddle\nnew-b\nend\n",
            PreviewOptions {
                max_matrix_cells: 1,
                ..PreviewOptions::default()
            },
        )
        .unwrap();

        for line in &preview.lines {
            assert_ne!(line.op, PreviewOp::Context);
        }
        assert_eq!(preview.deletions, 3);
        assert_eq!(preview.additions, 3);
        preview.deinit();
    }

    #[test]
    fn build_bounded_preview_preserves_trailing_newline_markers_with_boundary_context() {
        let mut preview = build_bounded_preview(
            b"alpha\nbeta\n",
            b"alpha\nbeta",
            PreviewOptions::default(),
        )
        .unwrap();

        assert_eq!(preview.lines.len(), 2);
        assert_eq!(preview.lines[0].op, PreviewOp::Context);
        assert_eq!(preview.lines[0].text, b"beta");
        assert_eq!(preview.lines[1].op, PreviewOp::Deletion);
        assert_eq!(
            preview.lines[1].text,
            TRAILING_NEWLINE_REMOVED_MARKER
        );
        preview.deinit();
    }

    #[test]
    fn build_bounded_preview_bounds_rows_and_escapes_terminal_controls() {
        let mut preview = build_bounded_preview(
            b"",
            b"\x1b[31mred\nsecond\nthird\nfourth\nfifth\nsixth\nseventh\n",
            PreviewOptions::default(),
        )
        .unwrap();

        assert!(preview.truncated);
        let mut encoded_bytes = 0usize;
        for line in &preview.lines {
            assert!(line.text.len() <= 4096);
            assert!(!line.text.contains(&0x1b));
            assert!(!line.text.contains(&b'\n'));
            encoded_bytes += line.text.len();
        }
        assert!(encoded_bytes <= MAX_ENCODED_PREVIEW_BYTES);
        assert!(s(&preview.lines[0].text).contains("\\x1b"));
        preview.deinit();
    }

    #[test]
    fn build_bounded_preview_enforces_per_line_and_aggregate_encoded_budgets() {
        let mut preview = build_bounded_preview(
            b"",
            b"\x1b\x1b\x1b\x1b\n\x1b\x1b\x1b\x1b\n",
            PreviewOptions {
                max_encoded_line_bytes: 8,
                max_encoded_preview_bytes: 12,
                ..PreviewOptions::default()
            },
        )
        .unwrap();

        assert!(preview.truncated);
        let mut encoded_bytes = 0usize;
        for line in &preview.lines {
            assert!(line.text.len() <= 8);
            encoded_bytes += line.text.len();
        }
        assert!(encoded_bytes <= 12);
        preview.deinit();
    }

    #[test]
    fn build_bounded_preview_encoding_truncation_does_not_invent_data_elision() {
        let mut preview = build_bounded_preview(
            b"",
            b"abcdefghijklmnopqrstuvwxyz\n",
            PreviewOptions {
                max_encoded_line_bytes: 8,
                max_encoded_preview_bytes: 8,
                ..PreviewOptions::default()
            },
        )
        .unwrap();

        assert!(preview.truncated);
        assert_eq!(preview.lines.len(), 1);
        assert_eq!(preview.lines[0].op, PreviewOp::Addition);
        preview.deinit();
    }

    #[test]
    fn file_change_preview_validates_bounds_and_line_number_semantics() {
        let valid = FileChangePreview {
            path: b"note.txt".to_vec(),
            path_basename_start: 0,
            path_source_truncated: false,
            lines: vec![
                PreviewLine {
                    op: PreviewOp::Context,
                    old_line: Some(1),
                    new_line: Some(1),
                    text: b"alpha".to_vec(),
                },
                PreviewLine {
                    op: PreviewOp::Deletion,
                    old_line: Some(2),
                    new_line: None,
                    text: b"beta".to_vec(),
                },
                PreviewLine {
                    op: PreviewOp::Addition,
                    old_line: None,
                    new_line: Some(2),
                    text: b"BETA".to_vec(),
                },
            ],
            additions: 1,
            deletions: 1,
            truncated: false,
        };
        valid.validate().unwrap();

        let invalid_lines = [
            PreviewLine {
                op: PreviewOp::Context,
                old_line: Some(1),
                new_line: None,
                text: b"missing new".to_vec(),
            },
            PreviewLine {
                op: PreviewOp::Addition,
                old_line: Some(1),
                new_line: Some(1),
                text: b"old forbidden".to_vec(),
            },
            PreviewLine {
                op: PreviewOp::Deletion,
                old_line: Some(1),
                new_line: Some(1),
                text: b"new forbidden".to_vec(),
            },
            PreviewLine {
                op: PreviewOp::Elision,
                old_line: Some(1),
                new_line: None,
                text: b"...".to_vec(),
            },
            PreviewLine {
                op: PreviewOp::Notice,
                old_line: None,
                new_line: Some(1),
                text: b"notice".to_vec(),
            },
            PreviewLine {
                op: PreviewOp::Addition,
                old_line: None,
                new_line: Some(0),
                text: b"zero".to_vec(),
            },
        ];
        for line in invalid_lines {
            let mut invalid = valid.clone();
            invalid.lines = vec![line];
            assert_eq!(
                invalid.validate(),
                Err(PreviewValidationError::InvalidLineNumbers)
            );
        }

        let too_many_rows = vec![
            PreviewLine {
                op: PreviewOp::Addition,
                old_line: None,
                new_line: Some(1),
                text: b"x".to_vec(),
            };
            MAX_PREVIEW_LINES + 1
        ];
        let mut invalid = valid.clone();
        invalid.lines = too_many_rows;
        assert_eq!(
            invalid.validate(),
            Err(PreviewValidationError::TooManyLines)
        );

        let mut invalid = valid.clone();
        invalid.path = vec![0u8; MAX_ENCODED_PATH_BYTES + 1];
        assert_eq!(
            invalid.validate(),
            Err(PreviewValidationError::PathTooLong)
        );

        let mut invalid = valid.clone();
        invalid.lines = vec![PreviewLine {
            op: PreviewOp::Addition,
            old_line: None,
            new_line: Some(1),
            text: vec![0u8; MAX_ENCODED_LINE_BYTES + 1],
        }];
        assert_eq!(
            invalid.validate(),
            Err(PreviewValidationError::LineTooLong)
        );

        let aggregate_lines = vec![
            PreviewLine {
                op: PreviewOp::Addition,
                old_line: None,
                new_line: Some(1),
                text: vec![0u8; MAX_ENCODED_LINE_BYTES],
            };
            MAX_PREVIEW_LINES
        ];
        let mut invalid = valid;
        invalid.lines = aggregate_lines;
        invalid.additions = MAX_PREVIEW_LINES;
        invalid.deletions = 0;
        invalid.validate().unwrap();
    }
}
