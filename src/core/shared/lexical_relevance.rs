use std::cmp::Ordering;

pub const max_query_bytes: usize = 4 * 1024;
pub const max_query_tokens: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrepareError {
    QueryTooLong,
    TooManyTokens,
}

#[derive(Debug, PartialEq, Eq)]
pub struct PreparedQuery<'a> {
    raw: &'a [u8],
    tokens: [&'a [u8]; max_query_tokens],
    token_count: usize,
}

impl<'a> PreparedQuery<'a> {
    fn token_slice(&self) -> &[&'a [u8]] {
        &self.tokens[..self.token_count]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Score {
    pub exact_identity: bool,
    pub strong_hits: usize,
    pub weak_hits: usize,
}

pub fn prepare(query: &[u8]) -> Result<PreparedQuery<'_>, PrepareError> {
    if query.len() > max_query_bytes {
        return Err(PrepareError::QueryTooLong);
    }

    let mut prepared = PreparedQuery {
        raw: query,
        tokens: [b""; max_query_tokens],
        token_count: 0,
    };
    let mut raw_token_count: usize = 0;
    let mut start: Option<usize> = None;
    for (index, &byte) in query.iter().enumerate() {
        if byte.is_ascii_alphanumeric() {
            if start.is_none() {
                start = Some(index);
            }
            continue;
        }
        if let Some(token_start) = start {
            append_token(&mut prepared, &mut raw_token_count, &query[token_start..index])?;
            start = None;
        }
    }
    if let Some(token_start) = start {
        append_token(&mut prepared, &mut raw_token_count, &query[token_start..])?;
    }
    Ok(prepared)
}

pub fn score(
    query: &PreparedQuery<'_>,
    exact_identities: &[&[u8]],
    strong_fields: &[&[u8]],
    weak_fields: &[&[u8]],
) -> Option<Score> {
    let mut result = Score {
        exact_identity: contains_complete_identity(query.raw, exact_identities),
        ..Score::default()
    };

    for token in query.token_slice() {
        if contains_any_complete_token(strong_fields, token) {
            result.strong_hits += 1;
        } else if contains_any_substring(weak_fields, token) {
            result.weak_hits += 1;
        }
    }

    if !result.exact_identity
        && result.strong_hits == 0
        && result.weak_hits == 0
        && !query.raw.is_empty()
    {
        return None;
    }
    Some(result)
}

pub fn order(a: Score, b: Score) -> Ordering {
    let mut result = (a.exact_identity as u8).cmp(&(b.exact_identity as u8));
    if result != Ordering::Equal {
        return result;
    }
    result = a
        .strong_hits
        .saturating_add(a.weak_hits)
        .cmp(&b.strong_hits.saturating_add(b.weak_hits));
    if result != Ordering::Equal {
        return result;
    }
    a.strong_hits.cmp(&b.strong_hits)
}

fn append_token<'a>(
    prepared: &mut PreparedQuery<'a>,
    raw_token_count: &mut usize,
    token: &'a [u8],
) -> Result<(), PrepareError> {
    if *raw_token_count == max_query_tokens {
        return Err(PrepareError::TooManyTokens);
    }
    *raw_token_count += 1;

    for existing in prepared.token_slice() {
        if existing.eq_ignore_ascii_case(token) {
            return Ok(());
        }
    }
    prepared.tokens[prepared.token_count] = token;
    prepared.token_count += 1;
    Ok(())
}

fn contains_any_complete_token(fields: &[&[u8]], token: &[u8]) -> bool {
    fields
        .iter()
        .any(|field| contains_complete_token_ignore_case(field, token))
}

fn contains_any_substring(fields: &[&[u8]], token: &[u8]) -> bool {
    fields
        .iter()
        .any(|field| contains_ignore_case(field, token))
}

fn contains_complete_token_ignore_case(field: &[u8], token: &[u8]) -> bool {
    if token.is_empty() || token.len() > field.len() {
        return false;
    }

    let mut start = 0;
    while start <= field.len() - token.len() {
        let end = start + token.len();
        if field[start..end].eq_ignore_ascii_case(token) {
            if start > 0 && field[start - 1].is_ascii_alphanumeric() {
                start += 1;
                continue;
            }
            if end < field.len() && field[end].is_ascii_alphanumeric() {
                start += 1;
                continue;
            }
            return true;
        }
        start += 1;
    }
    false
}

fn contains_ignore_case(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    if needle.len() > haystack.len() {
        return false;
    }

    let mut start = 0;
    while start <= haystack.len() - needle.len() {
        if haystack[start..start + needle.len()].eq_ignore_ascii_case(needle) {
            return true;
        }
        start += 1;
    }
    false
}

fn contains_complete_identity(raw: &[u8], identities: &[&[u8]]) -> bool {
    for start in 0..raw.len() {
        for &identity in identities {
            if identity.is_empty() || identity.len() > raw.len() - start {
                continue;
            }
            let end = start + identity.len();
            if !raw[start..end].eq_ignore_ascii_case(identity) {
                continue;
            }
            if start > 0 && is_identity_byte(raw[start - 1]) {
                continue;
            }
            if end < raw.len() && is_identity_byte(raw[end]) {
                continue;
            }
            return true;
        }
    }
    false
}

fn is_identity_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-'
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepared_queries_enforce_bounds_and_deduplicate_case_insensitively() {
        let prepared = prepare(b"GitHub github GITHUB issue").unwrap();
        assert_eq!(2usize, prepared.token_count);
        assert_eq!(&b"GitHub"[..], prepared.token_slice()[0]);
        assert_eq!(&b"issue"[..], prepared.token_slice()[1]);

        let _ = prepare(&vec![b'a'; max_query_bytes]).unwrap();
        assert_eq!(
            Err(PrepareError::QueryTooLong),
            prepare(&vec![b'a'; max_query_bytes + 1])
        );

        let mut max_tokens = vec![];
        for _ in 0..(max_query_tokens - 1) {
            max_tokens.extend_from_slice(b"token ");
        }
        max_tokens.extend_from_slice(b"token");
        let _ = prepare(&max_tokens).unwrap();

        let mut too_many = max_tokens.clone();
        too_many.extend_from_slice(b" token");
        assert_eq!(Err(PrepareError::TooManyTokens), prepare(&too_many));
    }

    #[test]
    fn scores_preserve_exact_punctuation_identities_and_fuzzy_partial_matches() {
        let identities: [&[u8]; 1] = [b"read/file"];
        let strong: [&[u8]; 1] = [b"read/file"];
        let weak: [&[u8]; 1] = [b"Read a file from the workspace"];

        let exact_query = prepare(b"please use read/file now").unwrap();
        let exact = score(&exact_query, &identities, &strong, &weak).unwrap();
        assert!(exact.exact_identity);

        let separated_query = prepare(b"please use read file now").unwrap();
        let separated = score(&separated_query, &identities, &strong, &weak).unwrap();
        assert!(!separated.exact_identity);

        let partial_query = prepare(b"please use read/files now").unwrap();
        let partial = score(&partial_query, &identities, &strong, &weak).unwrap();
        assert!(!partial.exact_identity);
        assert!(partial.strong_hits > 0);
    }

    #[test]
    fn scores_count_distinct_strong_and_weak_hits_with_stable_ordering() {
        let no_identities: [&[u8]; 0] = [];
        let strong: [&[u8]; 1] = [b"brave web search"];
        let weak: [&[u8]; 1] = [b"Find current public information"];

        let query = prepare(b"web public public unrelated").unwrap();
        let relevance = score(&query, &no_identities, &strong, &weak).unwrap();
        assert_eq!(1usize, relevance.strong_hits);
        assert_eq!(1usize, relevance.weak_hits);

        let empty_query = prepare(b"").unwrap();
        assert_eq!(
            Score::default(),
            score(&empty_query, &no_identities, &strong, &weak).unwrap()
        );

        let irrelevant_query = prepare(b"calendar").unwrap();
        assert!(score(&irrelevant_query, &no_identities, &strong, &weak).is_none());

        assert_eq!(
            Ordering::Greater,
            order(
                Score { exact_identity: true, ..Score::default() },
                Score { strong_hits: 64, ..Score::default() }
            )
        );
        assert_eq!(
            Ordering::Less,
            order(
                Score { strong_hits: 1, ..Score::default() },
                Score { weak_hits: 64, ..Score::default() }
            )
        );
        assert_eq!(Ordering::Equal, order(relevance, relevance));

        let large = Score { strong_hits: 65_536, ..Score::default() };
        assert_eq!(
            Ordering::Greater,
            order(large, Score { strong_hits: 65_535, ..Score::default() })
        );
    }

    #[test]
    fn strong_fields_require_complete_normalized_query_tokens() {
        let query = prepare(b"send an email").unwrap();
        let no_identities: [&[u8]; 0] = [];
        let analysis_names: [&[u8]; 1] = [b"analysis-tools"];
        let analysis_descriptions: [&[u8]; 1] = [b"Analyze local source code"];
        let mail_names: [&[u8]; 1] = [b"mail-helper"];
        let mail_descriptions: [&[u8]; 1] = [b"Send email messages to recipients"];

        let analysis = score(
            &query,
            &no_identities,
            &analysis_names,
            &analysis_descriptions,
        )
        .unwrap();
        let mail = score(&query, &no_identities, &mail_names, &mail_descriptions).unwrap();

        assert_eq!(0usize, analysis.strong_hits);
        assert_eq!(1usize, analysis.weak_hits);
        assert_eq!(2usize, mail.weak_hits);
        assert_eq!(Ordering::Greater, order(mail, analysis));

        let ui_query = prepare(b"ui").unwrap();
        let ui_names: [&[u8]; 1] = [b"terminal-ui-review"];
        assert_eq!(
            1usize,
            score(&ui_query, &no_identities, &ui_names, &[]).unwrap().strong_hits
        );
    }

    #[test]
    fn matched_query_coverage_precedes_strong_field_tie_breaking() {
        let query = prepare(b"send email").unwrap();
        let no_identities: [&[u8]; 0] = [];
        let one_name_match = score(&query, &no_identities, &[b"send-tool"], &[]).unwrap();
        let two_description_matches = score(
            &query,
            &no_identities,
            &[b"mail-helper"],
            &[b"Send email messages to recipients"],
        )
        .unwrap();

        assert_eq!(1usize, one_name_match.strong_hits);
        assert_eq!(2usize, two_description_matches.weak_hits);
        assert_eq!(Ordering::Greater, order(two_description_matches, one_name_match));

        let equal_coverage_strong = Score { strong_hits: 1, weak_hits: 1, ..Score::default() };
        let equal_coverage_weak = Score { weak_hits: 2, ..Score::default() };
        assert_eq!(Ordering::Greater, order(equal_coverage_strong, equal_coverage_weak));
    }
}
