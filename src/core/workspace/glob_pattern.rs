//! Borrowed, pre-split glob patterns for matching workspace paths.

/// Maximum accepted glob pattern length in bytes.
#[allow(non_upper_case_globals)]
pub const max_pattern_bytes: usize = 4096;

const MAX_PATTERN_SEGMENTS: usize = max_pattern_bytes + 1;

/// Error returned when compiling a borrowed glob pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompileError {
    OutOfMemory,
    PatternTooLong,
}

/// Borrowed, pre-split glob pattern for matching workspace paths.
pub struct Pattern<'a> {
    raw: &'a [u8],
    segments: Vec<&'a [u8]>,
    has_path_separator: bool,
}

impl<'a> Pattern<'a> {
    /// Compiles a borrowed pattern. The returned pattern borrows `raw`; call
    /// [`Pattern::deinit`] to release the owned segment storage.
    pub fn compile(raw: &'a [u8]) -> Result<Pattern<'a>, CompileError> {
        if raw.len() > max_pattern_bytes {
            return Err(CompileError::PatternTooLong);
        }

        let has_path_separator = raw.contains(&b'/');
        if !has_path_separator {
            return Ok(Pattern {
                raw,
                segments: Vec::new(),
                has_path_separator: false,
            });
        }

        let mut segments: Vec<&'a [u8]> = Vec::new();
        append_segments(raw, &mut segments);
        Ok(Pattern {
            raw,
            segments,
            has_path_separator: true,
        })
    }

    /// Releases the owned segment storage. The raw pattern bytes are borrowed.
    pub fn deinit(&mut self) {
        self.segments = Vec::new();
        self.raw = &[];
        self.has_path_separator = false;
    }

    /// Returns true when the pattern matches a path. Patterns without a slash
    /// match the candidate's basename.
    pub fn matches_path(&self, candidate_path: &[u8]) -> bool {
        if !self.has_path_separator {
            return match_segment(self.raw, basename(candidate_path));
        }
        match_segmented(&self.segments, candidate_path)
    }

    /// Returns true when a slash-free pattern matches a single basename.
    pub fn matches_basename(&self, basename: &[u8]) -> bool {
        if self.has_path_separator {
            return false;
        }
        match_segment(self.raw, basename)
    }
}

fn append_segments<'a>(path: &'a [u8], segments: &mut Vec<&'a [u8]>) {
    let mut rest = path;
    while !rest.is_empty() {
        match rest.iter().position(|&b| b == b'/') {
            None => {
                segments.push(rest);
                return;
            }
            Some(slash) => {
                segments.push(&rest[..slash]);
                rest = &rest[slash + 1..];
            }
        }
    }
}

fn match_segmented(pattern_segments: &[&[u8]], candidate_path: &[u8]) -> bool {
    let segment_count = pattern_segments.len();
    debug_assert!(segment_count <= MAX_PATTERN_SEGMENTS);

    let mut previous = [false; MAX_PATTERN_SEGMENTS + 1];
    let mut current = [false; MAX_PATTERN_SEGMENTS + 1];

    previous[0] = true;
    for (index, segment) in pattern_segments.iter().enumerate() {
        previous[index + 1] = previous[index] && is_double_star(segment);
    }

    let mut candidate_it = SegmentIterator::init(candidate_path);
    while let Some(candidate_segment) = candidate_it.next() {
        current[..segment_count + 1].fill(false);
        for (index, pattern_segment) in pattern_segments.iter().enumerate() {
            current[index + 1] = if is_double_star(pattern_segment) {
                current[index] || previous[index + 1]
            } else {
                previous[index] && match_segment(pattern_segment, candidate_segment)
            };
        }
        previous[..segment_count + 1].copy_from_slice(&current[..segment_count + 1]);
    }

    previous[segment_count]
}

struct SegmentIterator<'a> {
    rest: &'a [u8],
}

impl<'a> SegmentIterator<'a> {
    fn init(path: &'a [u8]) -> SegmentIterator<'a> {
        SegmentIterator { rest: path }
    }

    fn next(&mut self) -> Option<&'a [u8]> {
        if self.rest.is_empty() {
            return None;
        }
        match self.rest.iter().position(|&b| b == b'/') {
            None => {
                let segment = self.rest;
                self.rest = &[];
                Some(segment)
            }
            Some(slash) => {
                let segment = &self.rest[..slash];
                self.rest = &self.rest[slash + 1..];
                Some(segment)
            }
        }
    }
}

fn is_double_star(segment: &[u8]) -> bool {
    segment == b"**"
}

fn match_segment(pattern: &[u8], candidate: &[u8]) -> bool {
    let mut pattern_index: usize = 0;
    let mut candidate_index: usize = 0;
    let mut star_pattern_index: Option<usize> = None;
    let mut star_candidate_index: usize = 0;

    while candidate_index < candidate.len() {
        if pattern_index < pattern.len() {
            let token = pattern[pattern_index];
            if token == b'*' {
                while pattern_index < pattern.len() && pattern[pattern_index] == b'*' {
                    pattern_index += 1;
                }
                if pattern_index == pattern.len() {
                    return true;
                }
                star_pattern_index = Some(pattern_index);
                star_candidate_index = candidate_index;
                continue;
            }
            if token == b'?' || token == candidate[candidate_index] {
                if candidate[candidate_index] == b'/' {
                    return false;
                }
                pattern_index += 1;
                candidate_index += 1;
                continue;
            }
        }

        let retry_pattern_index = match star_pattern_index {
            Some(value) => value,
            None => return false,
        };
        star_candidate_index += 1;
        candidate_index = star_candidate_index;
        pattern_index = retry_pattern_index;
    }

    while pattern_index < pattern.len() && pattern[pattern_index] == b'*' {
        pattern_index += 1;
    }
    pattern_index == pattern.len()
}

fn basename(path: &[u8]) -> &[u8] {
    let mut end_index = path.len();
    while end_index != 0 && path[end_index - 1] == b'/' {
        end_index -= 1;
    }
    if end_index == 0 {
        return &path[..0];
    }
    let mut start_index = end_index - 1;
    while start_index != 0 && path[start_index - 1] != b'/' {
        start_index -= 1;
    }
    &path[start_index..end_index]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expect_compiled_match(pattern: &[u8], candidate_path: &[u8], expected: bool) {
        let mut compiled = Pattern::compile(pattern).unwrap();
        assert_eq!(expected, compiled.matches_path(candidate_path));
        compiled.deinit();
    }

    #[test]
    fn glob_pattern_matches_recursive_and_segment_wildcards() {
        expect_compiled_match(b"**/*.zig", b"main.zig", true);
        expect_compiled_match(b"**/*.zig", b"src/core/main.zig", true);
        expect_compiled_match(b"*.zig", b"src/main.zig", true);
        expect_compiled_match(b"file?.zig", b"dir/file1.zig", true);
        expect_compiled_match(b"file?.zig", b"dir/file12.zig", false);
        expect_compiled_match(b"foo\\bar.txt", b"src/foo\\bar.txt", true);
        expect_compiled_match(b"src/*.zig", b"src/main.zig", true);
        expect_compiled_match(b"src/*.zig", b"src/core/main.zig", false);
        expect_compiled_match(b"src/*.zig", b"lib/src/main.zig", false);
    }

    #[test]
    fn glob_pattern_preserves_globstar_zero_or_more_segment_semantics() {
        expect_compiled_match(b"**", b"", true);
        expect_compiled_match(b"src/", b"src", true);
        expect_compiled_match(b"src/**", b"src", true);
        expect_compiled_match(b"src/**", b"src/core/main.zig", true);
        expect_compiled_match(b"src/**/main.zig", b"src/main.zig", true);
        expect_compiled_match(b"src/**/main.zig", b"src/core/nested/main.zig", true);
        expect_compiled_match(b"src/**/main.zig", b"lib/src/core/main.zig", false);
    }

    #[test]
    fn glob_pattern_treats_backslashes_literally() {
        expect_compiled_match(b"dir\\*.zig", b"dir\\main.zig", true);
        expect_compiled_match(b"dir\\*.zig", b"dir/main.zig", false);
    }

    #[test]
    fn glob_pattern_handles_adversarial_repeated_star_nonmatches_without_recursion() {
        let star_count = 1024;

        let mut consecutive = vec![b'*'; star_count + 1];
        consecutive[star_count] = b'z';

        let candidate = vec![b'a'; star_count];

        let mut compiled_consecutive = Pattern::compile(&consecutive).unwrap();
        assert!(!compiled_consecutive.matches_path(&candidate));
        compiled_consecutive.deinit();

        let mut separated: Vec<u8> = Vec::new();
        for _ in 0..512 {
            separated.extend_from_slice(b"*a");
        }
        separated.push(b'z');
        let mut compiled_separated = Pattern::compile(&separated).unwrap();
        assert!(!compiled_separated.matches_path(&candidate));
        compiled_separated.deinit();
    }

    #[test]
    fn glob_pattern_rejects_patterns_beyond_the_maximum_accepted_length() {
        let accepted = vec![b'a'; max_pattern_bytes];
        let mut accepted_pattern = Pattern::compile(&accepted).unwrap();
        assert!(accepted_pattern.matches_path(&accepted));
        accepted_pattern.deinit();

        let rejected = vec![b'a'; max_pattern_bytes + 1];
        assert!(matches!(
            Pattern::compile(&rejected),
            Err(CompileError::PatternTooLong)
        ));
    }
}
