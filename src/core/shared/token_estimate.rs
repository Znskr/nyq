//! Incrementally estimates raw model or request text without making the
//! result depend on transport chunk boundaries.

/// Mirrors Zig's `std.ascii.isWhitespace`, which additionally treats the
/// vertical tab (`0x0b`) as whitespace.
fn is_ascii_whitespace(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

fn estimate_span(span_bytes: u64) -> u64 {
    span_bytes / 4 + u64::from(span_bytes % 4 != 0)
}

/// Incrementally estimates raw model or request text without making the result
/// depend on transport chunk boundaries.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StreamingEstimator {
    pub settled_tokens: u64,
    pub span_bytes: u64,
}

impl StreamingEstimator {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn consume(&mut self, text: &[u8]) {
        for &byte in text {
            if is_ascii_whitespace(byte) {
                self.finish_span();
            } else {
                self.span_bytes = self.span_bytes.wrapping_add(1);
            }
        }
    }

    pub fn estimate(&self) -> u64 {
        self.settled_tokens
            .wrapping_add(estimate_span(self.span_bytes))
    }

    fn finish_span(&mut self) {
        self.settled_tokens = self
            .settled_tokens
            .wrapping_add(estimate_span(self.span_bytes));
        self.span_bytes = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::StreamingEstimator;

    #[test]
    fn streaming_estimator_is_invariant_across_chunk_boundaries() {
        let text = "split 你好 inside words".as_bytes();

        let mut one_shot = StreamingEstimator::new();
        one_shot.consume(text);
        let expected = one_shot.estimate();

        for split in 0..=text.len() {
            let mut fragmented = StreamingEstimator::new();
            fragmented.consume(&text[..split]);
            fragmented.consume(&text[split..]);
            assert_eq!(expected, fragmented.estimate());
        }
    }
}
