use crate::core::shared::display_width;

pub fn next_character_end(text: &[u8], start: usize) -> usize {
    let mut end = start.min(text.len());
    if end == text.len() {
        return end;
    }

    let first = display_width::display_unit_at(text, end);
    end += first.byte_len.max(1);
    if is_control_boundary(text, start) {
        return end.min(text.len());
    }
    while end < text.len() {
        let continuation = display_width::display_unit_at(text, end);
        if continuation.cell_width != 0 || is_control_boundary(text, end) {
            break;
        }
        end += continuation.byte_len.max(1);
    }
    end.min(text.len())
}

pub fn previous_character_start(text: &[u8], end: usize) -> usize {
    let target = end.min(text.len());
    if target == 0 {
        return 0;
    }

    let mut current: usize = 0;
    let mut previous: usize = 0;
    while current < target {
        previous = current;
        let next = next_character_end(text, current);
        if next >= target {
            return previous;
        }
        current = next;
    }
    previous
}

pub fn previous_word_start(text: &[u8], cursor: usize) -> usize {
    let mut pos = cursor.min(text.len());
    while pos > 0 {
        let previous = previous_character_start(text, pos);
        if is_word_character_at(text, previous) {
            break;
        }
        pos = previous;
    }
    while pos > 0 {
        let previous = previous_character_start(text, pos);
        if !is_word_character_at(text, previous) {
            break;
        }
        pos = previous;
    }
    pos
}

pub fn next_word_end(text: &[u8], cursor: usize) -> usize {
    let mut pos = cursor.min(text.len());
    while pos < text.len() && !is_word_character_at(text, pos) {
        pos = next_character_end(text, pos);
    }
    while pos < text.len() && is_word_character_at(text, pos) {
        pos = next_character_end(text, pos);
    }
    pos
}

pub fn next_word_delete_end(text: &[u8], cursor: usize) -> usize {
    let mut pos = cursor.min(text.len());
    while pos < text.len() && is_word_character_at(text, pos) {
        pos = next_character_end(text, pos);
    }
    while pos < text.len() && !is_word_character_at(text, pos) {
        pos = next_character_end(text, pos);
    }
    pos
}

pub fn is_word_character(codepoint: u32) -> bool {
    if codepoint == b'_' as u32 {
        return true;
    }
    if (b'a' as u32..=b'z' as u32).contains(&codepoint) {
        return true;
    }
    if (b'A' as u32..=b'Z' as u32).contains(&codepoint) {
        return true;
    }
    if (b'0' as u32..=b'9' as u32).contains(&codepoint) {
        return true;
    }
    codepoint >= 0xC0
}

pub fn logical_line_start(text: &[u8], cursor: usize) -> usize {
    let mut start = cursor.min(text.len());
    while start > 0 && text[start - 1] != b'\n' {
        start -= 1;
    }
    start
}

pub fn logical_line_end(text: &[u8], cursor: usize) -> usize {
    let mut end = cursor.min(text.len());
    while end < text.len() && text[end] != b'\n' {
        end += 1;
    }
    end
}

fn line_is_blank(text: &[u8], start: usize, end: usize) -> bool {
    for &byte in &text[start..end] {
        match byte {
            b' ' | b'\t' | b'\r' => {}
            _ => return false,
        }
    }
    true
}

fn previous_line_start(text: &[u8], line_start: usize) -> Option<usize> {
    if line_start == 0 {
        return None;
    }
    Some(logical_line_start(text, line_start - 1))
}

pub fn previous_paragraph_start(text: &[u8], cursor: usize) -> usize {
    let mut line_start = logical_line_start(text, cursor);
    let mut line_end = logical_line_end(text, line_start);

    while line_is_blank(text, line_start, line_end) {
        line_start = match previous_line_start(text, line_start) {
            Some(s) => s,
            None => return 0,
        };
        line_end = logical_line_end(text, line_start);
    }

    let mut paragraph_start = line_start;
    while let Some(candidate) = previous_line_start(text, paragraph_start) {
        if line_is_blank(text, candidate, logical_line_end(text, candidate)) {
            break;
        }
        paragraph_start = candidate;
    }
    if cursor > paragraph_start {
        return paragraph_start;
    }

    let mut previous = match previous_line_start(text, paragraph_start) {
        Some(p) => p,
        None => return 0,
    };
    while line_is_blank(text, previous, logical_line_end(text, previous)) {
        previous = match previous_line_start(text, previous) {
            Some(p) => p,
            None => return 0,
        };
    }
    let mut previous_start = previous;
    while let Some(candidate) = previous_line_start(text, previous_start) {
        if line_is_blank(text, candidate, logical_line_end(text, candidate)) {
            break;
        }
        previous_start = candidate;
    }
    previous_start
}

pub fn next_paragraph_start(text: &[u8], cursor: usize) -> usize {
    let mut line_start = logical_line_start(text, cursor);
    let mut line_end = logical_line_end(text, line_start);

    while !line_is_blank(text, line_start, line_end) {
        if line_end == text.len() {
            return text.len();
        }
        line_start = line_end + 1;
        line_end = logical_line_end(text, line_start);
    }
    while line_is_blank(text, line_start, line_end) {
        if line_end == text.len() {
            return text.len();
        }
        line_start = line_end + 1;
        line_end = logical_line_end(text, line_start);
    }
    line_start
}

pub fn previous_whitespace_delimited_token_start(
    text: &[u8],
    cursor: usize,
    lower_bound: usize,
) -> usize {
    let mut pos = cursor.min(text.len());
    let lower = lower_bound.min(pos);
    while pos > lower {
        let previous = previous_character_start(text, pos);
        if !is_whitespace_character(text, previous) {
            break;
        }
        pos = previous;
    }
    while pos > lower {
        let previous = previous_character_start(text, pos);
        if is_whitespace_character(text, previous) {
            break;
        }
        pos = previous;
    }
    pos
}

fn is_control_boundary(text: &[u8], start: usize) -> bool {
    let codepoint = display_width::decode_next_rune(text, start).codepoint;
    codepoint < 0x20 || codepoint == 0x7F
}

fn is_whitespace_character(text: &[u8], start: usize) -> bool {
    let codepoint = display_width::decode_next_rune(text, start).codepoint;
    codepoint <= 0x7F && ascii_whitespace(codepoint as u8)
}

fn ascii_whitespace(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | 0x0b | b'\r' | 0x0c)
}

fn is_word_character_at(text: &[u8], start: usize) -> bool {
    let codepoint = display_width::decode_next_rune(text, start).codepoint;
    is_word_character(codepoint)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn character_boundaries_group_terminal_character_continuations() {
        let decomposed = "Ae\u{301}B";
        assert_eq!(4, next_character_end(decomposed.as_bytes(), 1));
        assert_eq!(1, previous_character_start(decomposed.as_bytes(), 4));

        let flag = "A🇺🇸B";
        assert_eq!(1 + "🇺🇸".len(), next_character_end(flag.as_bytes(), 1));
        assert_eq!(
            1,
            previous_character_start(flag.as_bytes(), 1 + "🇺🇸".len())
        );

        let modifier = "A👍🏽B";
        assert_eq!(
            1 + "👍🏽".len(),
            next_character_end(modifier.as_bytes(), 1)
        );
        assert_eq!(
            1,
            previous_character_start(modifier.as_bytes(), 1 + "👍🏽".len())
        );

        let family = "A👨‍👩‍👧‍👦B";
        assert_eq!(
            1 + "👨‍👩‍👧‍👦".len(),
            next_character_end(family.as_bytes(), 1)
        );
        assert_eq!(
            1,
            previous_character_start(family.as_bytes(), 1 + "👨‍👩‍👧‍👦".len())
        );
    }

    #[test]
    fn character_boundaries_preserve_ascii_and_precomposed_utf8() {
        let text = "AéB";
        assert_eq!(1, next_character_end(text.as_bytes(), 0));
        assert_eq!(3, next_character_end(text.as_bytes(), 1));
        assert_eq!(1, previous_character_start(text.as_bytes(), 3));
        assert_eq!(3, previous_character_start(text.as_bytes(), text.len()));
        assert_eq!(0, previous_character_start(text.as_bytes(), 0));
        assert_eq!(
            text.len(),
            next_character_end(text.as_bytes(), text.len() + 10)
        );
    }

    #[test]
    fn control_bytes_remain_independent_character_boundaries() {
        let text = "A\n\u{301}\tB";
        assert_eq!(1, next_character_end(text.as_bytes(), 0));
        assert_eq!(2, next_character_end(text.as_bytes(), 1));
        assert_eq!(4, next_character_end(text.as_bytes(), 2));
        assert_eq!(5, next_character_end(text.as_bytes(), 4));
        assert_eq!(4, previous_character_start(text.as_bytes(), 5));
        assert_eq!(2, previous_character_start(text.as_bytes(), 4));
        assert_eq!(1, previous_character_start(text.as_bytes(), 2));
    }

    #[test]
    fn word_boundaries_use_whole_terminal_characters() {
        let text = "alpha beta gamma";
        assert_eq!(11, previous_word_start(text.as_bytes(), text.len()));
        assert_eq!(6, previous_word_start(text.as_bytes(), 11));
        assert_eq!(5, next_word_end(text.as_bytes(), 0));
        assert_eq!(10, next_word_end(text.as_bytes(), 5));
        assert_eq!(text.len(), next_word_end(text.as_bytes(), 11));

        let unicode = "one e\u{301}lan 🇺🇸 flag";
        let flag_start = unicode.find("🇺🇸").unwrap();
        assert_eq!(
            flag_start + "🇺🇸".len(),
            next_word_end(unicode.as_bytes(), "one e\u{301}lan".len())
        );
        assert_eq!(4, previous_word_start(unicode.as_bytes(), flag_start));
    }

    #[test]
    fn word_deletion_boundary_consumes_the_current_word_and_following_separators() {
        let text = "alpha  beta gamma";
        assert_eq!(7, next_word_delete_end(text.as_bytes(), 0));
        assert_eq!(7, next_word_delete_end(text.as_bytes(), 5));
        assert_eq!(12, next_word_delete_end(text.as_bytes(), 7));
        assert_eq!(text.len(), next_word_delete_end(text.as_bytes(), 12));
        assert_eq!(7, next_word_delete_end("alpha, beta".as_bytes(), 0));

        let unicode = "élan 🙂 next";
        assert_eq!("élan ".len(), next_word_delete_end(unicode.as_bytes(), 0));
    }

    #[test]
    fn logical_line_boundaries_clamp_to_the_current_line() {
        let text = "alpha\nbeta\ngamma";
        assert_eq!(0, logical_line_start(text.as_bytes(), 5));
        assert_eq!(6, logical_line_start(text.as_bytes(), 8));
        assert_eq!(10, logical_line_end(text.as_bytes(), 6));
        assert_eq!(
            text.len(),
            logical_line_end(text.as_bytes(), text.len() + 10)
        );
    }

    #[test]
    fn paragraph_boundaries_move_across_blank_line_delimited_blocks() {
        let text = "alpha\nbeta\n \t\ngamma\ndelta\n\nlast";
        let gamma = text.find("gamma").unwrap();
        let last = text.find("last").unwrap();

        assert_eq!(
            0,
            previous_paragraph_start(text.as_bytes(), "alpha\nbeta".len())
        );
        assert_eq!(0, previous_paragraph_start(text.as_bytes(), gamma));
        assert_eq!(gamma, next_paragraph_start(text.as_bytes(), 0));
        assert_eq!(last, next_paragraph_start(text.as_bytes(), gamma));
        assert_eq!(text.len(), next_paragraph_start(text.as_bytes(), last));
    }

    #[test]
    fn whitespace_delimited_token_boundary_stays_within_its_logical_line() {
        let text = "alpha\n  beta gamma";
        let line_start = logical_line_start(text.as_bytes(), text.len());
        assert_eq!(
            13,
            previous_whitespace_delimited_token_start(text.as_bytes(), text.len(), line_start)
        );
        assert_eq!(
            8,
            previous_whitespace_delimited_token_start(text.as_bytes(), 13, line_start)
        );
        assert_eq!(
            line_start,
            previous_whitespace_delimited_token_start(text.as_bytes(), 8, line_start)
        );

        let unicode = "one\te\u{301}lan";
        assert_eq!(
            4,
            previous_whitespace_delimited_token_start(unicode.as_bytes(), unicode.len(), 0)
        );
    }

    #[test]
    fn character_boundary_offsets_stay_ordered_and_in_bounds() {
        let cases: &[&str] = &[
            "",
            "ascii",
            "e\u{301}",
            "🇺🇸",
            "👍🏽",
            "👨‍👩‍👧‍👦",
            "\u{301}\u{302}x",
        ];

        for text in cases {
            let mut cursor: usize = 0;
            while cursor < text.len() {
                let next = next_character_end(text.as_bytes(), cursor);
                assert!(next > cursor);
                assert!(next <= text.len());
                assert_eq!(cursor, previous_character_start(text.as_bytes(), next));
                cursor = next;
            }
            assert_eq!(text.len(), cursor);
        }
    }
}
