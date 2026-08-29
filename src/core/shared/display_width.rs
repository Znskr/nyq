use std::sync::OnceLock;

use super::unicode_display_data::{self, Range};

/// A packed trie node. `edge_start` indexes into `rgi_trie_edges`; the node's
/// children occupy `edge_len` consecutive, sorted edge slots there. Because the
/// edges are sorted by codepoint, lookups use binary search.
#[derive(Clone, Copy)]
#[repr(C)]
struct RgiNode {
    edge_start: u16,
    edge_len: u8,
    terminal: bool,
}

/// Runtime lookup data decoded from the immutable tables in
/// `unicode_display_data`. This mirrors the Zig `RgiData` layout and size.
#[repr(C)]
struct RgiData {
    nodes: [RgiNode; unicode_display_data::rgi_trie_nodes.len()],
    edge_codepoint_ids: [u8; unicode_display_data::rgi_trie_edges.len()],
    codepoints: [u32; RGI_CODEPOINT_COUNT],
}

const fn unique_codepoint_count() -> usize {
    let edges = &unicode_display_data::rgi_trie_edges;
    let mut unique = [0u32; unicode_display_data::rgi_trie_edges.len()];
    let mut count = 0usize;
    let mut i = 0usize;
    while i < edges.len() {
        let cp = edges[i].codepoint;
        let mut seen = false;
        let mut j = 0usize;
        while j < count {
            if unique[j] == cp {
                seen = true;
                break;
            }
            j += 1;
        }
        if !seen {
            unique[count] = cp;
            count += 1;
        }
        i += 1;
    }
    count
}

const RGI_CODEPOINT_COUNT: usize = unique_codepoint_count();

static RGI_DATA: OnceLock<RgiData> = OnceLock::new();

fn rgi_data() -> &'static RgiData {
    RGI_DATA.get_or_init(build_rgi_data)
}

fn build_rgi_data() -> RgiData {
    let mut data = RgiData {
        nodes: [RgiNode {
            edge_start: 0,
            edge_len: 0,
            terminal: false,
        }; unicode_display_data::rgi_trie_nodes.len()],
        edge_codepoint_ids: [0u8; unicode_display_data::rgi_trie_edges.len()],
        codepoints: [0u32; RGI_CODEPOINT_COUNT],
    };

    let mut codepoint_count = 0usize;
    for (edge_index, edge) in unicode_display_data::rgi_trie_edges.iter().enumerate() {
        assert_eq!(
            edge.child,
            (edge_index + 1) as u32,
            "RGI trie child numbering is no longer implicit"
        );

        let mut codepoint_id: Option<u8> = None;
        for (index, &codepoint) in data.codepoints[..codepoint_count].iter().enumerate() {
            if codepoint == edge.codepoint {
                codepoint_id = Some(index as u8);
                break;
            }
        }

        let id = match codepoint_id {
            Some(id) => id,
            None => {
                let id = codepoint_count as u8;
                data.codepoints[codepoint_count] = edge.codepoint;
                codepoint_count += 1;
                id
            }
        };
        data.edge_codepoint_ids[edge_index] = id;
    }

    assert_eq!(
        codepoint_count,
        RGI_CODEPOINT_COUNT,
        "RGI trie edge alphabet count mismatch"
    );

    for (index, node) in unicode_display_data::rgi_trie_nodes.iter().enumerate() {
        data.nodes[index] = RgiNode {
            edge_start: node.edge_start as u16,
            edge_len: node.edge_len as u8,
            terminal: node.terminal,
        };
    }

    data
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecodedRune {
    pub len: usize,
    pub codepoint: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisplayUnit {
    pub byte_len: usize,
    pub cell_width: usize,
}

#[inline(never)]
pub fn visible_width(text: &[u8]) -> usize {
    let mut width: usize = 0;
    let mut i: usize = 0;
    while i < text.len() {
        let unit = display_unit_at(text, i);
        width = width.saturating_add(unit.cell_width);
        i += unit.byte_len;
    }
    width
}

/// Same as `visible_width`, but skips over ANSI escape sequences so callers can
/// measure the rendered cell width of text that already contains styling.
#[inline(never)]
pub fn visible_width_ignoring_ansi(text: &[u8]) -> usize {
    let mut width: usize = 0;
    let mut i: usize = 0;
    while i < text.len() {
        if text[i] == 0x1b {
            i = ansi_sequence_end(text, i);
            continue;
        }
        let unit = display_unit_at(text, i);
        width = width.saturating_add(unit.cell_width);
        i += unit.byte_len;
    }
    width
}

/// Returns the first variant whose visible width fits `max_width`, falling back
/// to the last (shortest) variant so callers always get something to clip.
#[inline(never)]
pub fn widest_fitting<'a>(variants: &[&'a [u8]], max_width: usize) -> &'a [u8] {
    assert!(!variants.is_empty());
    for variant in variants {
        if visible_width(variant) <= max_width {
            return variant;
        }
    }
    variants[variants.len() - 1]
}

#[inline(never)]
pub fn prefix_by_width<'a>(text: &'a [u8], max_width: usize) -> &'a [u8] {
    if max_width == 0 {
        return &[];
    }

    let mut width: usize = 0;
    let mut i: usize = 0;
    while i < text.len() {
        let unit = display_unit_at(text, i);
        if unit.cell_width > max_width - width {
            break;
        }
        width += unit.cell_width;
        i += unit.byte_len;
    }
    &text[..i]
}

/// Same as `prefix_by_width`, but skips ANSI escape sequences so callers can
/// clip text that already carries styling bytes without the escape sequences
/// eating into the width budget.
#[inline(never)]
pub fn prefix_by_width_ignoring_ansi<'a>(text: &'a [u8], max_width: usize) -> &'a [u8] {
    if max_width == 0 {
        return &[];
    }

    let mut width: usize = 0;
    let mut i: usize = 0;
    while i < text.len() {
        if text[i] == 0x1b {
            i = ansi_sequence_end(text, i);
            continue;
        }
        let unit = display_unit_at(text, i);
        if unit.cell_width > max_width - width {
            break;
        }
        width += unit.cell_width;
        i += unit.byte_len;
    }
    &text[..i]
}

#[inline(never)]
pub fn suffix_by_width<'a>(text: &'a [u8], max_width: usize) -> &'a [u8] {
    if max_width == 0 {
        return &text[text.len()..];
    }

    let mut remaining = visible_width(text);
    if remaining <= max_width {
        return text;
    }

    let mut i: usize = 0;
    while i < text.len() && remaining > max_width {
        let unit = display_unit_at(text, i);
        remaining -= unit.cell_width;
        i += unit.byte_len;
    }
    &text[i..]
}

/// Same as `suffix_by_width`, but ANSI-aware. Walks forward so escape sequences
/// stay attached to the visible rune that follows them; splitting an OSC-8
/// open/close pair would leak a dangling hyperlink into the terminal.
#[inline(never)]
pub fn suffix_by_width_ignoring_ansi<'a>(text: &'a [u8], max_width: usize) -> &'a [u8] {
    if max_width == 0 {
        return &text[text.len()..];
    }

    let total = visible_width_ignoring_ansi(text);
    if total <= max_width {
        return text;
    }

    let mut remaining = total;
    let mut i: usize = 0;
    while i < text.len() && remaining > max_width {
        if text[i] == 0x1b {
            i = ansi_sequence_end(text, i);
            continue;
        }
        let unit = display_unit_at(text, i);
        remaining -= unit.cell_width;
        i += unit.byte_len;
    }
    &text[i..]
}

#[inline(never)]
pub fn display_unit_at(text: &[u8], index: usize) -> DisplayUnit {
    if index >= text.len() {
        return DisplayUnit {
            byte_len: 0,
            cell_width: 0,
        };
    }

    let first_byte = text[index];
    let may_start_keycap = (first_byte == b'#'
        || first_byte == b'*'
        || (first_byte >= b'0' && first_byte <= b'9'))
        && has_rgi_keycap_suffix(text, index + 1);
    if first_byte < 0x80 && !may_start_keycap {
        let cell_width = if first_byte == 0 || first_byte < 32 || first_byte == 0x7f {
            0
        } else {
            1
        };
        return DisplayUnit {
            byte_len: 1,
            cell_width,
        };
    }

    let rgi_len = match_rgi_sequence(text, index);
    if rgi_len != 0 {
        return DisplayUnit {
            byte_len: rgi_len,
            cell_width: 2,
        };
    }

    let first = decode_next_rune(text, index);
    let next_index = index + first.len;
    if next_index < text.len() && is_in_ranges(first.codepoint, &unicode_display_data::variation_bases)
    {
        let selector = decode_next_rune(text, next_index);
        if selector.codepoint == 0xfe0e {
            let cell_width =
                if is_in_ranges(first.codepoint, &unicode_display_data::wide_ranges) {
                    2
                } else {
                    1
                };
            return DisplayUnit {
                byte_len: first.len + selector.len,
                cell_width,
            };
        }
        if selector.codepoint == 0xfe0f {
            return DisplayUnit {
                byte_len: first.len + selector.len,
                cell_width: 2,
            };
        }
    }

    DisplayUnit {
        byte_len: first.len,
        cell_width: rune_width(first.codepoint),
    }
}

fn has_rgi_keycap_suffix(text: &[u8], suffix_start: usize) -> bool {
    let variation_selector_16: &[u8] = b"\xef\xb8\x8f";
    let combining_keycap: &[u8] = b"\xe2\x83\xa3";
    let mut index = suffix_start;
    if starts_with(&text[index..], variation_selector_16) {
        index += variation_selector_16.len();
    }
    starts_with(&text[index..], combining_keycap)
}

fn starts_with(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.len() >= needle.len() && &haystack[..needle.len()] == needle
}

#[inline(never)]
fn rune_width(codepoint: u32) -> usize {
    if codepoint == 0 {
        return 0;
    }
    if codepoint < 32 || (codepoint >= 0x7f && codepoint < 0xa0) {
        return 0;
    }
    if is_zero_width_continuation(codepoint) {
        return 0;
    }
    if is_in_ranges(codepoint, &unicode_display_data::wide_ranges)
        || is_in_ranges(codepoint, &unicode_display_data::emoji_presentation_ranges)
    {
        return 2;
    }
    1
}

/// Returns whether width `w` at one-based `col` exceeds `cols` under DECAWM-OFF.
/// Requires non-zero inputs and widens arithmetic to avoid `u16` overflow.
pub fn should_wrap_at(col: u16, w: u16, cols: u16) -> bool {
    (col as u32) + (w as u32) - 1 > cols as u32
}

/// Returns the next fixed eight-column tab stop, clamped to the right margin.
/// Columns are one-based. Callers must provide non-zero `col` and `cols`.
pub fn next_tab_stop_column(col: u16, cols: u16) -> u16 {
    assert!(col > 0);
    assert!(cols > 0);
    if col >= cols {
        return cols;
    }

    let zero_based: u32 = (col as u32) - 1;
    let next_stop = ((zero_based / 8) + 1) * 8 + 1;
    next_stop.min(cols as u32) as u16
}

#[inline(never)]
pub fn decode_next_rune(text: &[u8], index: usize) -> DecodedRune {
    if index >= text.len() {
        return DecodedRune {
            len: 0,
            codepoint: 0,
        };
    }
    let first = text[index];
    if first < 0x80 {
        return DecodedRune {
            len: 1,
            codepoint: first as u32,
        };
    }

    let len = match utf8_byte_sequence_length(first) {
        Ok(len) => len,
        Err(()) => {
            return DecodedRune {
                len: 1,
                codepoint: 0xfffd,
            }
        }
    };
    if index + len > text.len() {
        return DecodedRune {
            len: 1,
            codepoint: 0xfffd,
        };
    }

    match utf8_decode(&text[index..index + len]) {
        Ok(codepoint) => DecodedRune { len, codepoint },
        Err(()) => DecodedRune {
            len: 1,
            codepoint: 0xfffd,
        },
    }
}

fn utf8_byte_sequence_length(first: u8) -> Result<usize, ()> {
    match first {
        0x00..=0x7f => Ok(1),
        0xc0..=0xdf => Ok(2),
        0xe0..=0xef => Ok(3),
        0xf0..=0xf7 => Ok(4),
        _ => Err(()),
    }
}

fn utf8_decode(bytes: &[u8]) -> Result<u32, ()> {
    let first = bytes[0];
    let n = utf8_byte_sequence_length(first)?;
    if bytes.len() < n {
        return Err(());
    }
    for &b in &bytes[1..n] {
        if b & 0xc0 != 0x80 {
            return Err(());
        }
    }
    let codepoint = match n {
        2 => ((first as u32 & 0x1f) << 6) | (bytes[1] as u32 & 0x3f),
        3 => {
            ((first as u32 & 0x0f) << 12)
                | ((bytes[1] as u32 & 0x3f) << 6)
                | (bytes[2] as u32 & 0x3f)
        }
        4 => {
            ((first as u32 & 0x07) << 18)
                | ((bytes[1] as u32 & 0x3f) << 12)
                | ((bytes[2] as u32 & 0x3f) << 6)
                | (bytes[3] as u32 & 0x3f)
        }
        _ => return Err(()),
    };
    if n == 2 && codepoint < 0x80 {
        return Err(());
    }
    if n == 3 && codepoint < 0x800 {
        return Err(());
    }
    if n == 4 && codepoint < 0x10000 {
        return Err(());
    }
    if (0xd800..=0xdfff).contains(&codepoint) {
        return Err(());
    }
    if codepoint > 0x10ffff {
        return Err(());
    }
    Ok(codepoint)
}

#[inline(never)]
pub fn ansi_sequence_end(text: &[u8], index: usize) -> usize {
    if index >= text.len() || text[index] != 0x1b {
        return index;
    }
    if index + 1 >= text.len() {
        return text.len();
    }

    let kind = text[index + 1];
    if kind == b'[' {
        let mut i = index + 2;
        while i < text.len() {
            let b = text[i];
            if (b'@'..=b'~').contains(&b) {
                return i + 1;
            }
            i += 1;
        }
        return text.len();
    }

    if kind == b']' {
        let mut i = index + 2;
        while i < text.len() {
            if text[i] == 0x07 {
                return i + 1;
            }
            if text[i] == 0x1b && i + 1 < text.len() && text[i + 1] == b'\\' {
                return i + 2;
            }
            i += 1;
        }
        return text.len();
    }

    (index + 2).min(text.len())
}

#[inline(never)]
pub fn previous_rune_start(text: &[u8], end: usize) -> usize {
    if end == 0 {
        return 0;
    }
    let mut i = end - 1;
    while i > 0 && (text[i] & 0b1100_0000) == 0b1000_0000 {
        i -= 1;
    }
    i
}

#[inline(never)]
pub fn status_prefix_end(label: &[u8]) -> usize {
    if label.is_empty() {
        return 0;
    }
    let first = display_unit_at(label, 0);
    if first.byte_len == 0 || first.byte_len >= label.len() {
        return 0;
    }
    if label[first.byte_len] == b' ' {
        first.byte_len + 1
    } else {
        0
    }
}

#[inline(never)]
pub fn wrap_cut_ignoring_ansi<'a>(text: &'a [u8], max_width: usize) -> &'a [u8] {
    let prefix = prefix_by_width_ignoring_ansi(text, max_width);
    if prefix.len() == text.len() {
        return prefix;
    }

    let mut last_space: Option<usize> = None;
    let mut index: usize = 0;
    while index < prefix.len() {
        let unit = display_unit_at(prefix, index);
        if prefix[index] == b' ' || prefix[index] == b'\t' {
            last_space = Some(index);
        }
        index += unit.byte_len;
    }
    if let Some(space_index) = last_space {
        if space_index > 0 {
            return &text[..space_index];
        }
    }
    prefix
}

#[inline(never)]
pub fn trim_break_whitespace(text: &[u8]) -> &[u8] {
    let mut index: usize = 0;
    while index < text.len() && (text[index] == b' ' || text[index] == b'\t') {
        index += 1;
    }
    &text[index..]
}

fn is_combining(codepoint: u32) -> bool {
    (codepoint >= 0x0300 && codepoint <= 0x036f)
        || (codepoint >= 0x1ab0 && codepoint <= 0x1aff)
        || (codepoint >= 0x1dc0 && codepoint <= 0x1dff)
        || (codepoint >= 0x20d0 && codepoint <= 0x20ff)
        || (codepoint >= 0xfe20 && codepoint <= 0xfe2f)
        || (codepoint >= 0xfe00 && codepoint <= 0xfe0f)
}

fn is_zero_width_continuation(codepoint: u32) -> bool {
    is_combining(codepoint)
        || is_in_ranges(codepoint, &unicode_display_data::emoji_modifier_ranges)
        || codepoint == 0x200c
        || codepoint == 0x200d
        || (codepoint >= 0x200b && codepoint <= 0x200f)
        || (codepoint >= 0x202a && codepoint <= 0x202e)
        || (codepoint >= 0x2060 && codepoint <= 0x206f)
        || codepoint == 0xfeff
        || (codepoint >= 0xe0001 && codepoint <= 0xe007f)
        || (codepoint >= 0xe0100 && codepoint <= 0xe01ef)
}

fn is_in_ranges(codepoint: u32, ranges: &[Range]) -> bool {
    let mut low: usize = 0;
    let mut high = ranges.len();
    while low < high {
        let middle = low + (high - low) / 2;
        let range = &ranges[middle];
        if codepoint < range.first {
            high = middle;
        } else if codepoint > range.last {
            low = middle + 1;
        } else {
            return true;
        }
    }
    false
}

fn match_rgi_sequence(text: &[u8], index: usize) -> usize {
    let mut node_index: u32 = 0;
    let mut cursor = index;
    let mut longest: usize = 0;
    let mut depth: usize = 0;

    while cursor < text.len() && depth < unicode_display_data::max_rgi_sequence_codepoints {
        let rune = decode_next_rune(text, cursor);
        node_index = match find_trie_child(node_index, rune.codepoint) {
            Some(child) => child,
            None => break,
        };
        cursor += rune.len;
        let node = &rgi_data().nodes[node_index as usize];
        if node.terminal {
            longest = cursor - index;
        }
        depth += 1;
    }

    longest
}

fn find_trie_child(node_index: u32, codepoint: u32) -> Option<u32> {
    let data = rgi_data();
    let node = &data.nodes[node_index as usize];
    let edge_start: usize = node.edge_start as usize;
    let mut low = edge_start;
    let mut high = edge_start + node.edge_len as usize;
    while low < high {
        let middle = low + (high - low) / 2;
        let edge_codepoint = data.codepoints[data.edge_codepoint_ids[middle] as usize];
        if codepoint < edge_codepoint {
            high = middle;
        } else if codepoint > edge_codepoint {
            low = middle + 1;
        } else {
            return Some((middle + 1) as u32);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgi_runtime_lookup_data_stays_within_the_measured_size_budget() {
        assert!(std::mem::size_of::<RgiData>() <= 22_272);
    }

    #[test]
    fn prefix_by_width_avoids_cutting_emoji_bytes() {
        let text = "a\u{1F600}b".as_bytes();
        assert_eq!(prefix_by_width(text, 2), "a".as_bytes());
        assert_eq!(prefix_by_width(text, 3), "a\u{1F600}".as_bytes());
    }

    #[test]
    fn suffix_by_width_keeps_whole_runes() {
        assert_eq!(
            suffix_by_width("ab\u{1F600}cd".as_bytes(), 4),
            "\u{1F600}cd".as_bytes()
        );
    }

    #[test]
    fn visible_width_counts_emoji_as_two_cells() {
        assert_eq!(visible_width("a\u{1F600}b".as_bytes()), 4);
    }

    #[test]
    fn visible_width_ignoring_ansi_skips_csi_sequences() {
        assert_eq!(
            visible_width_ignoring_ansi("\x1b[1mhello\x1b[22m".as_bytes()),
            5
        );
        assert_eq!(
            visible_width_ignoring_ansi("\x1b[48;5;236mfoo\x1b[49m".as_bytes()),
            3
        );
        assert_eq!(
            visible_width_ignoring_ansi("\x1b[1mEnfoque\x1b[22m".as_bytes()),
            7
        );
    }

    #[test]
    fn should_wrap_at_is_strictly_overflow_based() {
        assert!(!should_wrap_at(10, 1, 10));
        assert!(should_wrap_at(10, 2, 10));
        assert!(!should_wrap_at(1, 1, 1));

        // Start at c = 2 so c - 1 stays >= 1; cols = 0 is excluded by the
        // helper precondition and guarded by current callers.
        let mut c: u16 = 2;
        while c <= 256 {
            assert!(!should_wrap_at(c, 1, c));
            assert!(should_wrap_at(c, 1, c - 1));
            c += 1;
        }

        assert!(should_wrap_at(65535, 2, 65535));
    }

    #[test]
    fn next_tab_stop_column_uses_absolute_stops_and_clamps_safely() {
        assert_eq!(next_tab_stop_column(1, 80), 9u16);
        assert_eq!(next_tab_stop_column(8, 80), 9u16);
        assert_eq!(next_tab_stop_column(9, 80), 17u16);
        assert_eq!(next_tab_stop_column(1, 8), 8u16);
        assert_eq!(next_tab_stop_column(10, 10), 10u16);
        assert_eq!(next_tab_stop_column(11, 10), 10u16);
        assert_eq!(next_tab_stop_column(65535, 65535), 65535u16);
    }

    #[test]
    fn prefix_by_width_ignoring_ansi_keeps_opening_osc8_attached_to_visible_bytes() {
        // OSC-8 open does not consume any visible cells; the first two bytes of
        // the hyperlink body do. Budget of 2 keeps the open + 2 cells.
        let text = "\x1b]8;;file:///x\x1b\\ab\x1b]8;;\x1b\\".as_bytes();
        let out = prefix_by_width_ignoring_ansi(text, 2);
        assert_eq!(visible_width_ignoring_ansi(out), 2);
        assert!(find_bytes(out, "\x1b]8;;file:///x\x1b\\".as_bytes()).is_some());
    }

    #[test]
    fn prefix_by_width_ignoring_ansi_returns_empty_on_zero_budget() {
        assert_eq!(
            prefix_by_width_ignoring_ansi("\x1b[31mhello\x1b[0m".as_bytes(), 0),
            "".as_bytes()
        );
    }

    #[test]
    fn suffix_by_width_ignoring_ansi_tolerates_ansi_around_multibyte_runes() {
        let text = "a\x1b]8;;file:///\u{00A0}.png\x1b\\[Image 1]\x1b]8;;\x1b\\".as_bytes();
        let out = suffix_by_width_ignoring_ansi(text, 9);
        assert_eq!(visible_width_ignoring_ansi(out), 9);
    }

    #[test]
    fn suffix_by_width_ignoring_ansi_returns_full_text_when_it_fits() {
        let text = "\x1b[31mhi\x1b[0m".as_bytes();
        assert_eq!(suffix_by_width_ignoring_ansi(text, 5), text);
    }

    #[test]
    fn decode_next_rune_advances_through_invalid_utf8() {
        let text = b"\xf0\x28\x8c\x28".as_slice();
        let first = decode_next_rune(text, 0);
        assert_eq!(first.len, 1);
        assert_eq!(first.codepoint, 0xfffd);

        let second = decode_next_rune(text, first.len);
        assert_eq!(second.len, 1);
        assert_eq!(second.codepoint, b'(' as u32);
    }

    #[test]
    fn ansi_sequence_end_handles_osc_terminated_by_esc_backslash() {
        let text = "\x1b]8;;file:///x\x1b\\body".as_bytes();
        assert_eq!(ansi_sequence_end(text, 0), 16);
        assert_eq!(visible_width_ignoring_ansi(text), 4);
    }

    #[test]
    fn rune_width_preserves_current_width_table_semantics() {
        assert_eq!(rune_width(0), 0);
        assert_eq!(rune_width(0x07), 0);
        assert_eq!(rune_width(0x1b), 0);
        assert_eq!(rune_width(0x80), 0);
        assert_eq!(rune_width(0x9f), 0);
        assert_eq!(rune_width(0x0301), 0);
        assert_eq!(rune_width(0x1ab0), 0);
        assert_eq!(rune_width(0x4e00), 2);
        assert_eq!(rune_width(0x1f600), 2);
        assert_eq!(rune_width(0xff66), 1);
        assert_eq!(rune_width(b'a' as u32), 1);
        assert_eq!(rune_width(b' ' as u32), 1);
        assert_eq!(rune_width(0x09), 0);
    }

    #[test]
    fn decode_next_rune_preserves_boundary_and_invalid_byte_behavior() {
        assert_eq!(
            decode_next_rune("".as_bytes(), 0),
            DecodedRune {
                len: 0,
                codepoint: 0
            }
        );
        assert_eq!(
            decode_next_rune("a".as_bytes(), 1),
            DecodedRune {
                len: 0,
                codepoint: 0
            }
        );
        assert_eq!(
            decode_next_rune(b"\xff", 0),
            DecodedRune {
                len: 1,
                codepoint: 0xfffd
            }
        );
        assert_eq!(
            decode_next_rune(b"\xf0", 0),
            DecodedRune {
                len: 1,
                codepoint: 0xfffd
            }
        );
        assert_eq!(
            decode_next_rune(b"\xc0\x80", 0),
            DecodedRune {
                len: 1,
                codepoint: 0xfffd
            }
        );
    }

    #[test]
    fn ansi_sequence_end_preserves_current_csi_osc_and_esc_boundaries() {
        assert_eq!(ansi_sequence_end("\x1b[m".as_bytes(), 0), 3);
        assert_eq!(ansi_sequence_end("\x1b[1;31m".as_bytes(), 0), 7);
        assert_eq!(ansi_sequence_end("\x1b[K".as_bytes(), 0), 3);

        let unterminated_csi = "\x1b[".as_bytes();
        assert_eq!(ansi_sequence_end(unterminated_csi, 0), unterminated_csi.len());

        assert_eq!(ansi_sequence_end("\x1b]8;;url\x07".as_bytes(), 0), 9);
        assert_eq!(ansi_sequence_end("\x1b]8;;url\x1b\\".as_bytes(), 0), 10);

        let unterminated_osc = "\x1b]8;;url".as_bytes();
        assert_eq!(ansi_sequence_end(unterminated_osc, 0), unterminated_osc.len());

        assert_eq!(ansi_sequence_end("\x1bA".as_bytes(), 0), 2);

        let bare_escape = "\x1b".as_bytes();
        assert_eq!(ansi_sequence_end(bare_escape, 0), bare_escape.len());
    }

    #[test]
    fn previous_rune_start_preserves_byte_boundary_behavior() {
        assert_eq!(previous_rune_start("hello".as_bytes(), 5), 4);
        assert_eq!(previous_rune_start("a\u{1F600}b".as_bytes(), 5), 1);
        assert_eq!(previous_rune_start("".as_bytes(), 0), 0);
        assert_eq!(previous_rune_start("a".as_bytes(), 1), 0);
    }

    #[test]
    fn prefix_by_width_and_suffix_by_width_preserve_boundary_behavior() {
        assert_eq!(prefix_by_width("ab".as_bytes(), 0), "".as_bytes());
        assert_eq!(prefix_by_width("ab".as_bytes(), 1), "a".as_bytes());
        assert_eq!(prefix_by_width("ab".as_bytes(), 2), "ab".as_bytes());
        assert_eq!(prefix_by_width("ab".as_bytes(), 100), "ab".as_bytes());

        let suffix_zero = suffix_by_width("ab".as_bytes(), 0);
        assert_eq!(suffix_zero.len(), 0);
        assert_eq!(suffix_by_width("ab".as_bytes(), 1), "b".as_bytes());
        assert_eq!(
            suffix_by_width("a\u{1F600}b".as_bytes(), 3),
            "\u{1F600}b".as_bytes()
        );
    }

    #[test]
    fn should_wrap_at_preserves_documented_edge_behavior() {
        assert!(should_wrap_at(1, 2, 1));
        assert!(!should_wrap_at(1, 1, 80));
        assert!(!should_wrap_at(80, 1, 80));
        assert!(should_wrap_at(80, 2, 80));
    }

    #[test]
    fn status_prefix_end_detects_a_leading_status_rune_and_space() {
        assert_eq!(status_prefix_end("▲ error".as_bytes()), 4);
        assert_eq!(status_prefix_end("x error".as_bytes()), 2);
        assert_eq!(status_prefix_end("error text".as_bytes()), 0);
        assert_eq!(status_prefix_end("▲".as_bytes()), 0);
        assert_eq!(status_prefix_end("".as_bytes()), 0);
    }

    #[test]
    fn wrap_cut_ignoring_ansi_prefers_the_last_space_inside_the_budget() {
        assert_eq!(
            wrap_cut_ignoring_ansi("aaaa bbbb cccc".as_bytes(), 10),
            "aaaa bbbb".as_bytes()
        );
        assert_eq!(
            wrap_cut_ignoring_ansi("aaaaabbbbbcc".as_bytes(), 5),
            "aaaaa".as_bytes()
        );
        assert_eq!(wrap_cut_ignoring_ansi(" abcd".as_bytes(), 2), " a".as_bytes());
        assert_eq!(
            wrap_cut_ignoring_ansi("aaaa bbbb cccc".as_bytes(), 20),
            "aaaa bbbb cccc".as_bytes()
        );
        assert_eq!(
            wrap_cut_ignoring_ansi("\x1b[31maaaa bbbb\x1b[0m".as_bytes(), 6),
            "\x1b[31maaaa".as_bytes()
        );
    }

    #[test]
    fn trim_break_whitespace_strips_leading_spaces_and_tabs() {
        assert_eq!(trim_break_whitespace(" \t abc".as_bytes()), "abc".as_bytes());
        assert_eq!(trim_break_whitespace("abc ".as_bytes()), "abc ".as_bytes());
        assert_eq!(trim_break_whitespace("  ".as_bytes()), "".as_bytes());
    }

    #[test]
    fn widest_fitting_picks_the_first_variant_that_fits_and_falls_back_to_the_last() {
        let variants: &[&[u8]] = &[
            "long variant here".as_bytes(),
            "medium one".as_bytes(),
            "short".as_bytes(),
        ];
        assert_eq!(widest_fitting(variants, 40), "long variant here".as_bytes());
        assert_eq!(widest_fitting(variants, 17), "long variant here".as_bytes());
        assert_eq!(widest_fitting(variants, 16), "medium one".as_bytes());
        assert_eq!(widest_fitting(variants, 9), "short".as_bytes());
        assert_eq!(widest_fitting(variants, 2), "short".as_bytes());

        let wide: &[&[u8]] = &["漢漢漢".as_bytes(), "漢".as_bytes()];
        assert_eq!(widest_fitting(wide, 5), "漢".as_bytes());
    }

    #[test]
    fn display_unit_widths_cover_presentation_and_rgi_sequences() {
        let cases: &[(&[u8], usize)] = &[
            ("A".as_bytes(), 1),
            ("\u{754C}".as_bytes(), 2),
            ("a\u{0301}".as_bytes(), 1),
            (&b"\xff"[..], 1),
            ("\u{2705}".as_bytes(), 2),
            ("\u{274C}".as_bytes(), 2),
            ("\u{2600}\u{FE0E}".as_bytes(), 1),
            ("\u{231A}\u{FE0E}".as_bytes(), 2),
            ("\u{26A1}\u{FE0E}".as_bytes(), 2),
            ("\u{2600}\u{FE0F}".as_bytes(), 2),
            ("\u{1F44D}\u{1F3FD}".as_bytes(), 2),
            ("\u{1F1FA}\u{1F1F8}".as_bytes(), 2),
            ("#\u{FE0F}\u{20E3}".as_bytes(), 2),
            (
                "\u{1F3F4}\u{E0067}\u{E0062}\u{E0065}\u{E006E}\u{E0067}\u{E007F}".as_bytes(),
                2,
            ),
            ("\u{1F469}\u{200D}\u{1F4BB}".as_bytes(), 2),
        ];

        for &(text, width) in cases {
            assert_eq!(visible_width(text), width);
        }
    }

    #[test]
    fn display_unit_scanner_returns_exact_sequence_byte_spans() {
        let cases: &[(&[u8], usize)] = &[
            ("A".as_bytes(), 1),
            ("\u{754C}".as_bytes(), 2),
            (&b"\xff"[..], 1),
            ("\u{2705}".as_bytes(), 2),
            ("\u{2600}\u{FE0E}".as_bytes(), 1),
            ("\u{231A}\u{FE0E}".as_bytes(), 2),
            ("\u{26A1}\u{FE0E}".as_bytes(), 2),
            ("\u{2600}\u{FE0F}".as_bytes(), 2),
            ("\u{1F44D}\u{1F3FD}".as_bytes(), 2),
            ("\u{1F1FA}\u{1F1F8}".as_bytes(), 2),
            ("#\u{FE0F}\u{20E3}".as_bytes(), 2),
            (
                "\u{1F3F4}\u{E0067}\u{E0062}\u{E0065}\u{E006E}\u{E0067}\u{E007F}".as_bytes(),
                2,
            ),
            ("\u{1F469}\u{200D}\u{1F4BB}".as_bytes(), 2),
        ];

        for &(text, cell_width) in cases {
            assert_eq!(
                display_unit_at(text, 0),
                DisplayUnit {
                    byte_len: text.len(),
                    cell_width,
                }
            );
        }

        assert_eq!(
            display_unit_at("".as_bytes(), 0),
            DisplayUnit {
                byte_len: 0,
                cell_width: 0
            }
        );
    }

    #[test]
    fn display_unit_scanner_rejects_sequence_lookalikes() {
        let stray_selector = "A\u{FE0F}".as_bytes();
        assert_eq!(
            display_unit_at(stray_selector, 0),
            DisplayUnit {
                byte_len: 1,
                cell_width: 1
            }
        );
        assert_eq!(visible_width(stray_selector), 1);

        let invalid_flag = "\u{1F1E6}\u{1F1E6}".as_bytes();
        assert_eq!(visible_width(invalid_flag), 4);
        assert_eq!(display_unit_at(invalid_flag, 0).byte_len, 4);

        let incomplete_keycap = "#\u{20E3}".as_bytes();
        assert_eq!(display_unit_at(incomplete_keycap, 0).byte_len, 1);
        assert_eq!(visible_width(incomplete_keycap), 1);

        let incomplete_tag =
            "\u{1F3F4}\u{E0067}\u{E0062}\u{E0065}\u{E006E}\u{E0067}".as_bytes();
        assert_eq!(display_unit_at(incomplete_tag, 0).byte_len, 4);

        let invalid_zwj = "\u{1F469}\u{200D}A".as_bytes();
        assert_eq!(display_unit_at(invalid_zwj, 0).byte_len, 4);
        assert_eq!(visible_width(invalid_zwj), 3);
    }

    #[test]
    fn ordinary_ascii_keycap_candidates_stay_on_the_single_byte_path() {
        let text = "0123456789#*".as_bytes();
        for index in 0..text.len() {
            assert_eq!(
                display_unit_at(text, index),
                DisplayUnit {
                    byte_len: 1,
                    cell_width: 1
                }
            );
        }
    }

    #[test]
    fn display_unit_clipping_preserves_sequence_boundaries() {
        let sequence = "\u{1F44D}\u{1F3FD}";
        let text = format!("a{}b", sequence);
        let text = text.as_bytes();
        assert_eq!(prefix_by_width(text, 2), "a".as_bytes());
        assert_eq!(prefix_by_width(text, 3), format!("a{}", sequence).as_bytes());
        assert_eq!(suffix_by_width(text, 1), "b".as_bytes());
        assert_eq!(suffix_by_width(text, 3), format!("{}b", sequence).as_bytes());

        let styled = format!("\x1b[31m{}\x1b[0m", sequence);
        let styled = styled.as_bytes();
        assert_eq!(prefix_by_width_ignoring_ansi(styled, 2), styled);
        assert_eq!(suffix_by_width_ignoring_ansi(styled, 2), styled);
        assert_eq!(
            status_prefix_end(format!("{} active", sequence).as_bytes()),
            sequence.len() + 1
        );
    }

    #[test]
    fn fuzz_display_unit_boundaries() {
        let corpus: &[&[u8]] = &[
            b"",
            b"plain text",
            b"\xff\xf0\x28\x8c\x28",
            "\u{2600}\u{FE0E}\u{2600}\u{FE0F}".as_bytes(),
            "\u{1F44D}\u{1F3FD}\u{1F1FA}\u{1F1F8}".as_bytes(),
            "#\u{FE0F}\u{20E3}\u{1F469}\u{200D}\u{1F4BB}".as_bytes(),
        ];

        let mut inputs: Vec<Vec<u8>> = corpus.iter().map(|s| s.to_vec()).collect();

        // Add deterministic pseudo-random byte strings to exercise the same
        // invariants Zig's fuzzer would.
        let mut state: u64 = 0x9e3779b97f4a7c15;
        for _ in 0..256 {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let len = (state >> 33) as usize % 256;
            let mut buf = vec![0u8; len];
            for b in buf.iter_mut() {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                *b = (state >> 33) as u8;
            }
            inputs.push(buf);
        }

        for input in &inputs {
            check_display_unit_boundaries(input);
        }
    }

    fn check_display_unit_boundaries(text: &[u8]) {
        let mut expected_width: usize = 0;
        let mut index: usize = 0;
        while index < text.len() {
            let unit = display_unit_at(text, index);
            assert!(unit.byte_len > 0);
            assert!(unit.byte_len <= text.len() - index);
            assert!(unit.cell_width <= 2);
            assert_eq!(unit, display_unit_at(text, index));
            expected_width = expected_width.saturating_add(unit.cell_width);
            index += unit.byte_len;
        }
        assert_eq!(text.len(), index);
        assert_eq!(expected_width, visible_width(text));

        let budgets = [
            0usize,
            (expected_width / 2).min(16),
            expected_width.saturating_add(2).min(16),
        ];
        for budget in budgets {
            let prefix = prefix_by_width(text, budget);
            let suffix = suffix_by_width(text, budget);
            assert!(is_display_unit_boundary(text, prefix.len()));
            assert!(is_display_unit_boundary(text, text.len() - suffix.len()));
            assert!(visible_width(prefix) <= budget);
            assert!(visible_width(suffix) <= budget);
        }
    }

    fn is_display_unit_boundary(text: &[u8], target: usize) -> bool {
        if target > text.len() {
            return false;
        }
        let mut index: usize = 0;
        while index < target {
            let unit = display_unit_at(text, index);
            if unit.byte_len == 0 || unit.byte_len > target - index {
                return false;
            }
            index += unit.byte_len;
        }
        index == target
    }

    fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        if needle.is_empty() {
            return Some(0);
        }
        haystack.windows(needle.len()).position(|window| window == needle)
    }
}
