#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    pub raw_start: usize,
    pub raw_end: usize,
}

impl Span {
    pub fn is_valid(self, input_len: usize) -> bool {
        self.raw_start < self.raw_end && self.raw_end <= input_len
    }

    pub fn contains(self, raw_offset: usize) -> bool {
        self.raw_start < raw_offset && raw_offset < self.raw_end
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImageTokenSpan {
    pub span: Span,
    pub id: usize,
}

/// Projects a registered span through an insertion. Insertion at the start
/// moves the entity; insertion at the end leaves it in place. Null means the
/// edit entered the entity or arithmetic overflowed.
pub fn after_insert(span: Span, raw_offset: usize, byte_len: usize) -> Option<Span> {
    if byte_len == 0 || raw_offset >= span.raw_end {
        return Some(span);
    }
    if raw_offset > span.raw_start {
        return None;
    }
    Some(Span {
        raw_start: span.raw_start.checked_add(byte_len)?,
        raw_end: span.raw_end.checked_add(byte_len)?,
    })
}

/// Projects a registered span through a deletion. Null means the deleted range
/// overlapped the entity.
pub fn after_delete(span: Span, delete_start: usize, delete_end: usize) -> Option<Span> {
    if delete_start >= delete_end || delete_start >= span.raw_end {
        return Some(span);
    }
    if delete_end <= span.raw_start {
        let removed = delete_end - delete_start;
        return Some(Span {
            raw_start: span.raw_start - removed,
            raw_end: span.raw_end - removed,
        });
    }
    None
}

pub fn overlaps(a: Span, b: Span) -> bool {
    a.raw_start < b.raw_end && b.raw_start < a.raw_end
}

pub fn contains_span(outer: Span, inner: Span) -> bool {
    outer.raw_start <= inner.raw_start && inner.raw_end <= outer.raw_end
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entity_spans_shift_only_for_edits_outside_their_contents() {
        let span = Span { raw_start: 4, raw_end: 10 };

        assert_eq!(Span { raw_start: 6, raw_end: 12 }, after_insert(span, 4, 2).unwrap());
        assert_eq!(span, after_insert(span, 10, 2).unwrap());
        assert!(after_insert(span, 7, 2).is_none());

        assert_eq!(Span { raw_start: 2, raw_end: 8 }, after_delete(span, 1, 3).unwrap());
        assert_eq!(span, after_delete(span, 10, 12).unwrap());
        assert!(after_delete(span, 3, 5).is_none());
        assert!(after_delete(span, 5, 7).is_none());
    }

    #[test]
    fn entity_span_relationship_helpers_use_half_open_ranges() {
        let entity = Span { raw_start: 4, raw_end: 10 };
        assert!(entity.is_valid(10));
        assert!(!entity.is_valid(9));
        assert!(!entity.contains(4));
        assert!(entity.contains(5));
        assert!(!entity.contains(10));
        assert!(overlaps(entity, Span { raw_start: 9, raw_end: 12 }));
        assert!(!overlaps(entity, Span { raw_start: 10, raw_end: 12 }));
        assert!(contains_span(entity, Span { raw_start: 5, raw_end: 9 }));
    }
}
