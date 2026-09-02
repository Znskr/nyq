#![allow(non_snake_case)]
use crate::core::shared::display_width;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WalkResult {
    pub end_row: u32,
    pub end_col: u16,
}

/// Walks visual rows without capping the result to the viewport. Under
/// DECAWM-OFF, glyphs wrap only when they exceed the right margin.
pub fn walkText(start_row: u32, start_col: u16, bytes: &[u8], cols: u16) -> WalkResult {
    if bytes.is_empty() || cols == 0 {
        return WalkResult {
            end_row: start_row,
            end_col: start_col,
        };
    }

    let mut row: u32 = start_row;
    let mut col: u16 = start_col;
    let mut i: usize = 0;
    while i < bytes.len() {
        let ch = bytes[i];

        if ch == 0x1b {
            i = display_width::ansi_sequence_end(bytes, i);
            continue;
        }

        match ch {
            b'\r' => {
                col = 1;
                i += 1;
                continue;
            }
            b'\n' => {
                col = 1;
                row += 1;
                i += 1;
                continue;
            }
            _ => {}
        }

        if ch == b'\t' {
            col = display_width::next_tab_stop_column(col, cols);
            i += 1;
            continue;
        }

        if ch < 32 {
            i += 1;
            continue;
        }

        let unit = display_width::display_unit_at(bytes, i);
        let w_usize = unit.cell_width;
        if w_usize == 0 {
            i += unit.byte_len;
            continue;
        }

        let w: u16 = w_usize as u16;
        if display_width::should_wrap_at(col, w, cols) {
            col = 1;
            row += 1;
        }
        col += w;
        i += unit.byte_len;
    }
    WalkResult {
        end_row: row,
        end_col: col,
    }
}

// Transcript rows can carry SGR + OSC-8 + other escape sequences (image
// badges, the user-message bar). ANSI-aware walking skips escape bytes so
// terminal position tracking stays correct whether a row is plain text or styled.
pub fn nextCursorColForLine(line: &[u8], cols: u16) -> u16 {
    if cols == 0 {
        return 1;
    }
    cols.min(walkText(1, 1, line, cols).end_col)
}

/// Returns the byte offset of visual row `skip_rows + 1` using the same
/// ANSI-aware wrapping rules as `visualRowsForLine`.
pub fn skipVisualRowsInLine(text: &[u8], cols: u16, skip_rows: u16) -> usize {
    if skip_rows == 0 || cols == 0 || text.is_empty() {
        return 0;
    }

    let mut col: u16 = 1;
    let mut rows_skipped: u16 = 0;
    let mut i: usize = 0;
    while i < text.len() {
        let ch = text[i];
        if ch == 0x1b {
            i = display_width::ansi_sequence_end(text, i);
            continue;
        }
        if ch == b'\r' {
            col = 1;
            i += 1;
            continue;
        }
        if ch == b'\n' {
            col = 1;
            rows_skipped += 1;
            i += 1;
            if rows_skipped >= skip_rows {
                return i;
            }
            continue;
        }
        if ch == b'\t' {
            col = display_width::next_tab_stop_column(col, cols);
            i += 1;
            continue;
        }
        if ch < 32 {
            i += 1;
            continue;
        }

        let unit = display_width::display_unit_at(text, i);
        let w_usize = unit.cell_width;
        if w_usize == 0 {
            i += unit.byte_len;
            continue;
        }
        let w: u16 = w_usize as u16;

        if display_width::should_wrap_at(col, w, cols) {
            col = 1;
            rows_skipped += 1;
            if rows_skipped >= skip_rows {
                return i;
            }
        }
        col += w;
        i += unit.byte_len;
    }
    text.len()
}

pub fn visualRowsForLine(text: &[u8], cols: u16) -> u16 {
    if cols == 0 {
        return 1;
    }
    if text.is_empty() {
        return 1;
    }

    let mut col: u16 = 1;
    let mut rows: u16 = 1;
    let mut rows_with_content: u16 = 0;
    let mut wrote_on_current_row = false;
    let mut i: usize = 0;
    while i < text.len() {
        let ch = text[i];
        if ch == 0x1b {
            i = display_width::ansi_sequence_end(text, i);
            continue;
        }
        match ch {
            b'\r' => {
                col = 1;
                i += 1;
                continue;
            }
            b'\n' => {
                if wrote_on_current_row {
                    rows_with_content = rows;
                }
                col = 1;
                rows += 1;
                wrote_on_current_row = false;
                i += 1;
                continue;
            }
            _ => {}
        }
        if ch == b'\t' {
            col = display_width::next_tab_stop_column(col, cols);
            i += 1;
            continue;
        }
        if ch < 32 {
            i += 1;
            continue;
        }

        let unit = display_width::display_unit_at(text, i);
        i += unit.byte_len;
        let w_usize = unit.cell_width;
        if w_usize == 0 {
            continue;
        }
        let w: u16 = w_usize as u16;

        if display_width::should_wrap_at(col, w, cols) {
            col = 1;
            rows += 1;
        }
        col += w;
        wrote_on_current_row = true;
    }

    if wrote_on_current_row {
        rows_with_content = rows;
    }
    if rows_with_content == 0 {
        1
    } else {
        rows_with_content
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::render_engine::assistant_wrap;

    #[test]
    fn skip_visual_rows_skips_hard_newlines() {
        let offset = skipVisualRowsInLine("a\nb\nc\nd".as_bytes(), 80, 2);
        assert_eq!(4, offset);
    }

    #[test]
    fn skip_visual_rows_skips_soft_wrap_boundaries() {
        let offset = skipVisualRowsInLine("abcdef".as_bytes(), 3, 1);
        assert_eq!(3, offset);
    }

    #[test]
    fn skip_visual_rows_with_zero_skip_returns_zero() {
        let offset = skipVisualRowsInLine("anything".as_bytes(), 40, 0);
        assert_eq!(0, offset);
    }

    #[test]
    fn skip_visual_rows_beyond_actual_rows_returns_text_length() {
        let offset = skipVisualRowsInLine("abc".as_bytes(), 10, 5);
        assert_eq!(3, offset);
    }

    #[test]
    fn positional_tab_changes_following_wrap_geometry() {
        let text = "1234567\tXYZ";
        let walked = walkText(1, 1, text.as_bytes(), 10);
        assert_eq!(2, walked.end_row);
        assert_eq!(2, walked.end_col);
        assert_eq!(2, visualRowsForLine(text.as_bytes(), 10));
        assert_eq!(10, skipVisualRowsInLine(text.as_bytes(), 10, 1));
    }

    #[test]
    fn next_cursor_col_follows_the_final_wrapped_row() {
        let record = "a".repeat(249);
        assert_eq!(10, nextCursorColForLine(record.as_bytes(), 120));
        assert_eq!(
            120,
            nextCursorColForLine(record.as_bytes()[..120].as_ref(), 120)
        );
    }

    #[test]
    fn walk_text_wrap_assistant_text_and_visual_rows_agree() {
        let corpus: &[(&[u8], u16)] = &[
            ("".as_bytes(), 1),
            ("a".as_bytes(), 1),
            ("abc".as_bytes(), 1),
            ("abc\ndef".as_bytes(), 1),
            ("漢字".as_bytes(), 2),
            ("a漢".as_bytes(), 2),
            ("a\u{0301}b".as_bytes(), 1),
            ("a\u{2600}\u{FE0E}b".as_bytes(), 1),
            ("a\u{1F44D}\u{1F3FD}b".as_bytes(), 2),
            ("a\u{1F1FA}\u{1F1F8}b".as_bytes(), 2),
            ("a#\u{FE0F}\u{20E3}b".as_bytes(), 2),
            (
                "a\u{1F3F4}\u{E0067}\u{E0062}\u{E0065}\u{E006E}\u{E0067}\u{E007F}b".as_bytes(),
                2,
            ),
            ("a\u{1F469}\u{200D}\u{1F4BB}b".as_bytes(), 2),
            ("\r".as_bytes(), 1),
            ("a\rb".as_bytes(), 1),
            ("\x1b[1;31mabc\x1b[0m".as_bytes(), 1),
            ("\x1b]8;;http://x\x07link\x1b]8;;\x07".as_bytes(), 1),
            ("\t".as_bytes(), 1),
        ];
        let widths = [1u16, 2, 4, 8, 10, 40, 80];
        for &(s, min_cols) in corpus {
            for &c in &widths {
                if c < min_cols {
                    continue;
                }
                let wrapped = assistant_wrap::wrapAssistantText(s, c);
                assert_eq!(
                    walkText(1, 1, s, c).end_row,
                    walkText(1, 1, &wrapped, c).end_row
                );
                if !s.contains(&b'\n') {
                    assert_eq!(visualRowsForLine(s, c) as u32, walkText(1, 1, s, c).end_row);
                }

                let total = visualRowsForLine(s, c);
                assert_eq!(s.len(), skipVisualRowsInLine(s, c, total));

                let mut prev: usize = 0;
                let mut skip: u16 = 1;
                while skip <= total {
                    let offset = skipVisualRowsInLine(s, c, skip);
                    assert!(offset >= prev);
                    prev = offset;
                    skip += 1;
                }
            }
        }
    }

    #[test]
    fn walk_text_cols_zero_is_a_noop_on_row_col() {
        let r = walkText(5, 3, "abc\ndef".as_bytes(), 0);
        assert_eq!(5, r.end_row);
        assert_eq!(3, r.end_col);
    }
}
