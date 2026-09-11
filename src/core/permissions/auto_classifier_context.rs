//! Bounded trusted root-user context serialization for permission auto
//! classification.
//!
//! Ported from `zig-reference/src/core/permissions/auto_classifier_context.zig`.
//! Compacted summaries are never treated as user-authored evidence.

use crate::core::shared::text_utils::{
    encode_terminal_safe, is_terminal_safe, mask_secrets, write_head_tail_bounded, HeadRounding,
};
use crate::core::shared::types::{ExecutionMemory, HistoryTurn};

pub const MAX_ROOT_USER_BYTES: usize = 1024;
pub const MAX_PERSISTED_PERMISSION_FEEDBACK_BYTES: usize = 320;
pub const MAX_SAME_TURN_PERMISSION_FEEDBACK_BYTES: usize = 384;
pub const CURRENT_LABEL: &[u8] = b"current_request: ";
pub const FIRST_ROOT_USER_LABEL: &[u8] = b"first_root_user_request: ";
pub const RECENT_ROOT_USER_LABEL: &[u8] = b"recent_root_user_request: ";
pub const OMITTED_ROOT_USER_LABEL: &[u8] = b"omitted_proven_root_user_turns: ";
pub const TRUSTED_PERMISSION_FEEDBACK_LABEL: &[u8] = b"trusted_user_permission_feedback: ";
pub const OMITTED_PERMISSION_FEEDBACK_LABEL: &[u8] = b"omitted_trusted_user_permission_feedback: ";
pub const CONTENT_OMISSION_MARKER: &[u8] = b" [... omitted ...] ";
pub const COUNT_MARKER_RESERVE: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContextError {
    RootUserContextOverflow,
    InvalidRootUserContext,
}

pub fn is_canonical_root_user_context(context: &[u8]) -> bool {
    if context.is_empty()
        || context.len() > MAX_ROOT_USER_BYTES
        || !context.ends_with(b"\n")
    {
        return false;
    }

    let mut lines = context[..context.len() - 1].split(|&b| b == b'\n');
    let Some(current) = lines.next() else {
        return false;
    };
    if !canonical_text_line(current, CURRENT_LABEL) {
        return false;
    }

    let mut saw_first = false;
    let mut saw_recent = false;
    let mut saw_omitted_root = false;
    let mut feedback_started = false;
    let mut saw_omitted_feedback = false;
    for line in lines {
        if line.starts_with(FIRST_ROOT_USER_LABEL) {
            if feedback_started || saw_first || saw_recent || saw_omitted_root
                || !canonical_text_line(line, FIRST_ROOT_USER_LABEL)
            {
                return false;
            }
            saw_first = true;
        } else if line.starts_with(RECENT_ROOT_USER_LABEL) {
            if feedback_started || saw_omitted_root
                || !canonical_text_line(line, RECENT_ROOT_USER_LABEL)
            {
                return false;
            }
            saw_recent = true;
        } else if line.starts_with(OMITTED_ROOT_USER_LABEL) {
            if feedback_started || saw_omitted_root
                || !canonical_count_line(line, OMITTED_ROOT_USER_LABEL)
            {
                return false;
            }
            saw_omitted_root = true;
        } else if line.starts_with(TRUSTED_PERMISSION_FEEDBACK_LABEL) {
            if saw_omitted_feedback || !canonical_text_line(line, TRUSTED_PERMISSION_FEEDBACK_LABEL)
            {
                return false;
            }
            feedback_started = true;
        } else if line.starts_with(OMITTED_PERMISSION_FEEDBACK_LABEL) {
            if saw_omitted_feedback
                || !canonical_count_line(line, OMITTED_PERMISSION_FEEDBACK_LABEL)
            {
                return false;
            }
            feedback_started = true;
            saw_omitted_feedback = true;
        } else {
            return false;
        }
    }
    true
}

pub fn current_root_user_request(context: &[u8]) -> Option<&[u8]> {
    if !is_canonical_root_user_context(context) {
        return None;
    }
    line_value(context, CURRENT_LABEL)
}

fn canonical_text_line(line: &[u8], label: &[u8]) -> bool {
    let Some(value) = line.strip_prefix(label) else {
        return false;
    };
    !value.is_empty() && is_terminal_safe(value)
}

fn canonical_count_line(line: &[u8], label: &[u8]) -> bool {
    let Some(value) = line.strip_prefix(label) else {
        return false;
    };
    if value.is_empty() || value[0] == b'0' {
        return false;
    }
    if !value.iter().all(|b| b.is_ascii_digit()) {
        return false;
    }
    std::str::from_utf8(value)
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .is_some_and(|n| n > 0)
}

/// Builds bounded trusted context from canonical session history. Compacted
/// summaries are never treated as user-authored evidence.
pub fn build_canonical_root_user_context(
    current_request: &[u8],
    history: &[HistoryTurn],
) -> Result<Vec<u8>, ContextError> {
    let mut turns: Vec<Vec<u8>> = Vec::new();
    let mut permission_feedback: Vec<Vec<u8>> = Vec::new();

    let mut has_compacted_summary = false;
    let mut compacted_prefix_turns: usize = 0;
    for turn in history {
        match turn {
            HistoryTurn::CompactedSummary(entry) => {
                has_compacted_summary = true;
                compacted_prefix_turns = compacted_prefix_turns.max(entry.removed_turn_count);
                permission_feedback.extend(entry.permission_feedback.iter().cloned());
            }
            HistoryTurn::Assistant(entry) => {
                turns.push(entry.user.text.clone());
                append_execution_permission_feedback(&mut permission_feedback, &entry.execution);
            }
            HistoryTurn::BackgroundCommand(entry) => {
                turns.push(entry.user.text.clone());
                append_execution_permission_feedback(&mut permission_feedback, &entry.execution);
            }
            HistoryTurn::Interrupted(entry) => {
                turns.push(entry.user.text.clone());
                append_execution_permission_feedback(&mut permission_feedback, &entry.execution);
            }
        }
    }
    turns.push(current_request.to_vec());

    Ok(build_root_user_context_with_feedback(
        &turns,
        &permission_feedback,
        if has_compacted_summary {
            compacted_prefix_turns.max(1)
        } else {
            0
        },
        !has_compacted_summary,
    )?)
}

/// Refreshes a queued prompt's bounded trusted context with one exact finished
/// root turn. The existing serialized context preserves a true-first anchor or
/// an explicit unknown prefix.
pub fn refresh_queued_root_user_context(
    current_request: &[u8],
    existing_context: &[u8],
    finished_turn: &HistoryTurn,
) -> Result<Vec<u8>, ContextError> {
    let finished = match finished_turn {
        HistoryTurn::Assistant(entry) => (&entry.user.text, &entry.execution),
        HistoryTurn::BackgroundCommand(entry) => (&entry.user.text, &entry.execution),
        HistoryTurn::Interrupted(entry) => (&entry.user.text, &entry.execution),
        HistoryTurn::CompactedSummary(_) => return Ok(existing_context.to_vec()),
    };
    let finished_user = finished.0.as_slice();

    let mut turns: Vec<Vec<u8>> = Vec::new();
    let prior_omitted_root_turns = prior_omitted_root_turns(existing_context);
    let first = line_value(existing_context, FIRST_ROOT_USER_LABEL);
    let has_unknown_prefix = first.is_none() && prior_omitted_root_turns > 0;
    if let Some(first_request) = first {
        turns.push(first_request.to_vec());
        if let Some(recent) = line_value(existing_context, RECENT_ROOT_USER_LABEL) {
            if recent != first_request && recent != finished_user {
                turns.push(recent.to_vec());
            }
        }
        if first_request != finished_user {
            turns.push(finished_user.to_vec());
        }
    } else {
        if has_unknown_prefix {
            if let Some(recent) = line_value(existing_context, RECENT_ROOT_USER_LABEL) {
                if recent != finished_user {
                    turns.push(recent.to_vec());
                }
            }
        }
        turns.push(finished_user.to_vec());
    }
    turns.push(current_request.to_vec());

    let mut permission_feedback: Vec<Vec<u8>> = Vec::new();
    append_serialized_permission_feedback(&mut permission_feedback, existing_context);
    append_execution_permission_feedback(&mut permission_feedback, finished.1);
    build_root_user_context_with_feedback(
        &turns,
        &permission_feedback,
        prior_omitted_root_turns,
        !has_unknown_prefix,
    )
}

/// Returns a bounded trusted-only projection for a tool execution. Human
/// permission feedback is included, while assistant/tool review evidence is
/// intentionally excluded.
pub fn build_tool_execution_root_user_context(
    root_user_context: &[u8],
    trusted_permission_feedback: &[&[u8]],
) -> Result<Vec<u8>, ContextError> {
    let new_feedback = build_trusted_permission_feedback(
        trusted_permission_feedback,
        MAX_SAME_TURN_PERMISSION_FEEDBACK_BYTES,
    )?;
    if new_feedback.is_empty() {
        return Ok(root_user_context.to_vec());
    }

    let feedback_start = canonical_feedback_start(root_user_context);
    let merged_feedback = merge_permission_feedback(
        &root_user_context[feedback_start..],
        &new_feedback,
    )?;
    let root_requests = &root_user_context[..feedback_start];
    if root_requests.len() + merged_feedback.len() <= MAX_ROOT_USER_BYTES {
        let mut out = Vec::with_capacity(root_requests.len() + merged_feedback.len());
        out.extend_from_slice(root_requests);
        out.extend_from_slice(&merged_feedback);
        return Ok(out);
    }

    let minimum_request_bytes = CURRENT_LABEL.len() + 2;
    if merged_feedback.len() + minimum_request_bytes > MAX_ROOT_USER_BYTES {
        return Err(ContextError::RootUserContextOverflow);
    }
    let requests = rebuild_root_requests_bounded(
        root_requests,
        MAX_ROOT_USER_BYTES - merged_feedback.len(),
    )?;

    let mut out = Vec::with_capacity(requests.len() + merged_feedback.len());
    out.extend_from_slice(&requests);
    out.extend_from_slice(&merged_feedback);
    debug_assert!(out.len() <= MAX_ROOT_USER_BYTES);
    Ok(out)
}

fn canonical_feedback_start(context: &[u8]) -> usize {
    let mut start = context.len();
    let newline_trusted = [b'\n'].iter().chain(TRUSTED_PERMISSION_FEEDBACK_LABEL.iter())
        .copied().collect::<Vec<u8>>();
    let newline_omitted = [b'\n'].iter().chain(OMITTED_PERMISSION_FEEDBACK_LABEL.iter())
        .copied().collect::<Vec<u8>>();
    if let Some(index) = find(context, &newline_trusted) {
        start = start.min(index + 1);
    }
    if let Some(index) = find(context, &newline_omitted) {
        start = start.min(index + 1);
    }
    start
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn merge_permission_feedback(
    existing: &[u8],
    appended: &[u8],
) -> Result<Vec<u8>, ContextError> {
    let mut out: Vec<u8> = Vec::new();
    let mut omitted: usize = 0;
    for block in [existing, appended] {
        for line in block.split(|&b| b == b'\n') {
            if line.is_empty() {
                continue;
            }
            if let Some(value) = line.strip_prefix(TRUSTED_PERMISSION_FEEDBACK_LABEL) {
                out.extend_from_slice(TRUSTED_PERMISSION_FEEDBACK_LABEL);
                out.extend_from_slice(value);
                out.push(b'\n');
                continue;
            }
            if let Some(value) = line.strip_prefix(OMITTED_PERMISSION_FEEDBACK_LABEL) {
                let count = std::str::from_utf8(value)
                    .ok()
                    .and_then(|v| v.parse::<usize>().ok())
                    .ok_or(ContextError::InvalidRootUserContext)?;
                omitted = omitted.saturating_add(count);
                continue;
            }
            return Err(ContextError::InvalidRootUserContext);
        }
    }
    if omitted > 0 {
        out.extend_from_slice(OMITTED_PERMISSION_FEEDBACK_LABEL);
        out.extend_from_slice(omitted.to_string().as_bytes());
        out.push(b'\n');
    }
    Ok(out)
}

fn rebuild_root_requests_bounded(
    context: &[u8],
    max_bytes: usize,
) -> Result<Vec<u8>, ContextError> {
    let current = line_value(context, CURRENT_LABEL)
        .ok_or(ContextError::InvalidRootUserContext)?
        .to_vec();
    let first = line_value(context, FIRST_ROOT_USER_LABEL).map(|v| v.to_vec());

    let mut recent: Vec<Vec<u8>> = Vec::new();
    for line in context.split(|&b| b == b'\n') {
        if let Some(value) = line.strip_prefix(RECENT_ROOT_USER_LABEL) {
            recent.push(value.to_vec());
        }
    }

    let mut turns: Vec<Vec<u8>> = Vec::new();
    if let Some(value) = &first {
        turns.push(value.clone());
    }
    while let Some(item) = recent.pop() {
        turns.push(item);
    }
    turns.push(current);

    let prior_omitted = line_value(context, OMITTED_ROOT_USER_LABEL)
        .and_then(|value| std::str::from_utf8(value).ok())
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    build_root_user_context_bounded(
        &turns,
        max_bytes,
        prior_omitted,
        first.is_some(),
    )
}

fn build_root_user_context_with_feedback(
    turns: &[Vec<u8>],
    permission_feedback: &[Vec<u8>],
    prior_omitted_root_turns: usize,
    first_root_user_is_proven: bool,
) -> Result<Vec<u8>, ContextError> {
    let feedback = build_trusted_permission_feedback(
        permission_feedback,
        MAX_PERSISTED_PERMISSION_FEEDBACK_BYTES,
    )?;
    let requests = build_root_user_context_bounded(
        turns,
        MAX_ROOT_USER_BYTES - feedback.len(),
        prior_omitted_root_turns,
        first_root_user_is_proven,
    )?;

    let mut out = Vec::with_capacity(requests.len() + feedback.len());
    out.extend_from_slice(&requests);
    out.extend_from_slice(&feedback);
    debug_assert!(out.len() <= MAX_ROOT_USER_BYTES);
    Ok(out)
}

fn prior_omitted_root_turns(context: &[u8]) -> usize {
    let mut retained_recent = false;
    let mut omitted: usize = 0;
    for line in context.split(|&b| b == b'\n') {
        if line.starts_with(RECENT_ROOT_USER_LABEL) {
            if retained_recent {
                omitted += 1;
            } else {
                retained_recent = true;
            }
            continue;
        }
        if let Some(value) = line.strip_prefix(OMITTED_ROOT_USER_LABEL) {
            omitted += std::str::from_utf8(value)
                .ok()
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or(0);
        }
    }
    omitted
}

fn line_value<'a>(context: &'a [u8], label: &[u8]) -> Option<&'a [u8]> {
    for line in context.split(|&b| b == b'\n') {
        if let Some(value) = line.strip_prefix(label) {
            return Some(value);
        }
    }
    None
}

fn append_serialized_permission_feedback(feedback: &mut Vec<Vec<u8>>, context: &[u8]) {
    for line in context.split(|&b| b == b'\n') {
        let Some(value) = line.strip_prefix(TRUSTED_PERMISSION_FEEDBACK_LABEL) else {
            continue;
        };
        if !value.is_empty() {
            feedback.push(value.to_vec());
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct RequiredAnchorBudgets {
    current: usize,
    first: usize,
    recent: usize,
}

fn required_anchor_budgets(
    current_len: usize,
    first_len: Option<usize>,
    recent_len: Option<usize>,
    capacity: usize,
) -> RequiredAnchorBudgets {
    let lengths: [Option<usize>; 3] = [Some(current_len), first_len, recent_len];
    let mut budgets: [usize; 3] = [0, 0, 0];
    let present_count = lengths.iter().filter(|l| l.is_some()).count();

    let initial_share = capacity / present_count;
    let mut used: usize = 0;
    for (index, maybe_length) in lengths.iter().enumerate() {
        if let Some(length) = maybe_length {
            budgets[index] = (*length).min(initial_share);
            used += budgets[index];
        }
    }

    let mut remaining = capacity - used;
    while remaining > 0 {
        let unfinished_count = lengths
            .iter()
            .zip(budgets.iter())
            .filter(|(maybe_length, budget)| {
                maybe_length.is_some_and(|length| **budget < length)
            })
            .count();
        if unfinished_count == 0 {
            break;
        }

        let share = 1.max(remaining / unfinished_count);
        for (index, maybe_length) in lengths.iter().enumerate() {
            let Some(length) = maybe_length else { continue };
            let added = (length - budgets[index]).min(share.min(remaining));
            budgets[index] += added;
            remaining -= added;
            if remaining == 0 {
                break;
            }
        }
    }

    RequiredAnchorBudgets {
        current: budgets[0],
        first: budgets[1],
        recent: budgets[2],
    }
}

fn build_root_user_context_bounded(
    turns: &[Vec<u8>],
    max_bytes: usize,
    prior_omitted_root_turns: usize,
    first_root_user_is_proven: bool,
) -> Result<Vec<u8>, ContextError> {
    let latest_raw = turns.last().map(|v| v.as_slice()).unwrap_or(b"");
    let latest = encode_text(latest_raw);

    let first = if first_root_user_is_proven && turns.len() > 1 {
        Some(encode_text(&turns[0]))
    } else {
        None
    };

    let first_turn_count: usize = if first.is_some() { 1 } else { 0 };
    let recent_total = turns.len().saturating_sub(1 + first_turn_count);
    let has_recent = recent_total > 0;
    let newest_recent = if has_recent {
        Some(encode_text(&turns[turns.len() - 2]))
    } else {
        None
    };

    let omitted_after_required =
        recent_total - usize::from(has_recent) + prior_omitted_root_turns;
    let anchor_overhead = CURRENT_LABEL.len()
        + 1
        + if first.is_some() {
            FIRST_ROOT_USER_LABEL.len() + 1
        } else {
            0
        }
        + if newest_recent.is_some() {
            RECENT_ROOT_USER_LABEL.len() + 1
        } else {
            0
        };
    let recent_marker_reserve = if omitted_after_required > 0 {
        COUNT_MARKER_RESERVE
    } else {
        0
    };
    let anchor_content_budget = max_bytes - anchor_overhead - recent_marker_reserve;
    let budgets = required_anchor_budgets(
        latest.len(),
        first.as_ref().map(|t| t.len()),
        newest_recent.as_ref().map(|t| t.len()),
        anchor_content_budget,
    );

    let mut out: Vec<u8> = Vec::new();
    out.extend_from_slice(CURRENT_LABEL);
    write_head_tail(&mut out, &latest, budgets.current).map_err(|_| ContextError::RootUserContextOverflow)?;
    out.push(b'\n');
    if let Some(first_text) = &first {
        out.extend_from_slice(FIRST_ROOT_USER_LABEL);
        write_head_tail(&mut out, first_text, budgets.first)
            .map_err(|_| ContextError::RootUserContextOverflow)?;
        out.push(b'\n');
    }
    if let Some(recent_text) = &newest_recent {
        out.extend_from_slice(RECENT_ROOT_USER_LABEL);
        write_head_tail(&mut out, recent_text, budgets.recent)
            .map_err(|_| ContextError::RootUserContextOverflow)?;
        out.push(b'\n');
    }

    let mut selected_recent = usize::from(newest_recent.is_some());
    let mut history_index = if turns.len() > 1 { turns.len() - 2 } else { 0 };
    while history_index > first_turn_count {
        history_index -= 1;
        let older_turns_remaining = history_index - first_turn_count;
        let omission_reserve = if older_turns_remaining > 0 || prior_omitted_root_turns > 0 {
            COUNT_MARKER_RESERVE
        } else {
            0
        };
        let used = out.len();
        let entry_overhead = RECENT_ROOT_USER_LABEL.len() + 1;
        if used + entry_overhead + CONTENT_OMISSION_MARKER.len() + omission_reserve > max_bytes {
            break;
        }

        let encoded = encode_text(&turns[history_index]);
        let content_budget = max_bytes - used - entry_overhead - omission_reserve;
        out.extend_from_slice(RECENT_ROOT_USER_LABEL);
        write_head_tail(&mut out, &encoded, content_budget)
            .map_err(|_| ContextError::RootUserContextOverflow)?;
        out.push(b'\n');
        selected_recent += 1;
    }

    let omitted_recent = recent_total - selected_recent + prior_omitted_root_turns;
    if omitted_recent > 0 {
        out.extend_from_slice(OMITTED_ROOT_USER_LABEL);
        out.extend_from_slice(omitted_recent.to_string().as_bytes());
        out.push(b'\n');
    }
    debug_assert!(out.len() <= max_bytes);
    Ok(out)
}

fn append_execution_permission_feedback(
    feedback: &mut Vec<Vec<u8>>,
    execution: &ExecutionMemory,
) {
    for step in &execution.tool_steps {
        for result in &step.tool_results {
            for item in &result.permission_feedback {
                if !item.is_empty() {
                    feedback.push(item.clone());
                }
            }
        }
    }
}

fn build_trusted_permission_feedback<I: AsRef<[u8]>>(
    feedback: &[I],
    max_bytes: usize,
) -> Result<Vec<u8>, ContextError> {
    let mut out: Vec<u8> = Vec::new();
    let mut selected: usize = 0;
    let mut index = feedback.len();
    while index > 0 {
        index -= 1;
        let omission_reserve = if index > 0 { COUNT_MARKER_RESERVE } else { 0 };
        let encoded = encode_text(feedback[index].as_ref());
        let used = out.len();
        let overhead = TRUSTED_PERMISSION_FEEDBACK_LABEL.len() + 1;
        if used + overhead + CONTENT_OMISSION_MARKER.len() + omission_reserve > max_bytes {
            break;
        }
        out.extend_from_slice(TRUSTED_PERMISSION_FEEDBACK_LABEL);
        write_head_tail(
            &mut out,
            &encoded,
            max_bytes - used - overhead - omission_reserve,
        )
        .map_err(|_| ContextError::RootUserContextOverflow)?;
        out.push(b'\n');
        selected += 1;
    }
    let omitted = feedback.len() - selected;
    if omitted > 0 {
        out.extend_from_slice(OMITTED_PERMISSION_FEEDBACK_LABEL);
        out.extend_from_slice(omitted.to_string().as_bytes());
        out.push(b'\n');
    }
    debug_assert!(out.len() <= max_bytes);
    Ok(out)
}

fn encode_text(raw: &[u8]) -> Vec<u8> {
    let masked = mask_secrets(raw);
    encode_terminal_safe(&masked, usize::MAX).bytes
}

fn write_head_tail(writer: &mut Vec<u8>, text: &[u8], max_content_bytes: usize) -> std::io::Result<()> {
    write_head_tail_bounded(
        writer,
        text,
        max_content_bytes,
        CONTENT_OMISSION_MARKER,
        HeadRounding::Up,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn big(prefix: &str, ch: char, n: usize) -> String {
        let mut s = String::from(prefix);
        for _ in 0..n {
            s.push(ch);
        }
        s
    }

    #[test]
    fn root_user_context_keeps_first_latest_and_newest_recent_turns_with_visible_omission() {
        let turns: Vec<Vec<u8>> = vec![
            b"first-root-authorization".to_vec(),
            big("oldest-middle ", 'a', 280).into_bytes(),
            big("older-middle ", 'b', 280).into_bytes(),
            big("middle-three ", 'd', 280).into_bytes(),
            big("middle-four ", 'e', 280).into_bytes(),
            big("middle-five ", 'f', 280).into_bytes(),
            big("middle-six ", 'g', 280).into_bytes(),
            big("recent-middle ", 'c', 280).into_bytes(),
            b"newest-recent-root-request".to_vec(),
            b"latest-root-request".to_vec(),
        ];
        let context = build_root_user_context_bounded(&turns, MAX_ROOT_USER_BYTES, 0, true).unwrap();

        assert!(context.len() <= MAX_ROOT_USER_BYTES);
        assert!(find(&context, b"current_request: latest-root-request").is_some());
        assert!(find(&context, b"first_root_user_request: first-root-authorization").is_some());
        assert!(find(&context, b"recent_root_user_request: newest-recent-root-request").is_some());
        assert!(find(&context, b"omitted_proven_root_user_turns:").is_some());
        assert!(find(&context, b"oldest-middle").is_none());
    }

    #[test]
    fn root_user_context_reserves_capacity_for_every_required_oversized_anchor() {
        let turns: Vec<Vec<u8>> = vec![
            big("first-required-marker ", 'a', 4096).into_bytes(),
            big("older-middle-one-marker ", 'b', 4096).into_bytes(),
            big("older-middle-two-marker ", 'c', 4096).into_bytes(),
            big("older-middle-three-marker ", 'd', 4096).into_bytes(),
            big("newest-recent-required-marker ", 'e', 4096).into_bytes(),
            big("current-required-marker ", 'f', 4096).into_bytes(),
        ];
        let context =
            build_root_user_context_bounded(&turns, MAX_ROOT_USER_BYTES, 0, true).unwrap();

        assert!(context.len() <= MAX_ROOT_USER_BYTES);
        assert!(is_canonical_root_user_context(&context));
        assert!(find(&context, b"current-required-marker").is_some());
        assert!(find(&context, b"first-required-marker").is_some());
        assert!(find(&context, b"newest-recent-required-marker").is_some());
        assert!(find(&context, b"older-middle-one-marker").is_none());
        assert!(find(&context, b"older-middle-two-marker").is_none());
        assert!(find(&context, b"older-middle-three-marker").is_none());
        assert_eq!(
            line_value(&context, OMITTED_ROOT_USER_LABEL).unwrap(),
            b"3"
        );
    }

    #[test]
    fn root_user_context_redistributes_unused_required_anchor_capacity() {
        let turns: Vec<Vec<u8>> = vec![
            big("first-required-marker ", 'a', 4096).into_bytes(),
            big("newest-recent-required-marker ", 'b', 4096).into_bytes(),
            b"current".to_vec(),
        ];
        let context =
            build_root_user_context_bounded(&turns, MAX_ROOT_USER_BYTES, 0, true).unwrap();

        assert_eq!(context.len(), MAX_ROOT_USER_BYTES);
        assert!(is_canonical_root_user_context(&context));
        assert_eq!(line_value(&context, CURRENT_LABEL).unwrap(), b"current");
        assert!(find(&context, b"first-required-marker").is_some());
        assert!(find(&context, b"newest-recent-required-marker").is_some());
        assert!(line_value(&context, OMITTED_ROOT_USER_LABEL).is_none());
    }

    #[test]
    fn compacted_prefix_remains_unknown_across_queued_context_refresh() {
        let history = vec![
            HistoryTurn::CompactedSummary(crate::core::shared::types::CompactedSummaryHistoryTurn {
                summary: b"untrusted compacted summary".to_vec(),
                removed_turn_count: 3,
                compaction_count: 1,
                root_user_messages: Vec::new(),
                root_user_messages_complete: false,
                permission_feedback: vec![b"Never modify files outside the workspace.".to_vec()],
                permission_feedback_complete: false,
            }),
            HistoryTurn::Assistant(crate::core::shared::types::AssistantHistoryTurn {
                user: crate::core::shared::types::UserTurn {
                    text: b"surviving exact request".to_vec(),
                    images: Vec::new(),
                    work_id: None,
                },
                assistant: b"surviving answer".to_vec(),
                execution: ExecutionMemory::default(),
            }),
        ];
        let context =
            build_canonical_root_user_context(b"queued current request", &history).unwrap();

        assert!(find(&context, b"current_request: queued current request").is_some());
        assert!(find(&context, b"recent_root_user_request: surviving exact request").is_some());
        assert!(find(&context, FIRST_ROOT_USER_LABEL).is_none());
        assert!(find(&context, b"omitted_proven_root_user_turns: 3").is_some());
        assert!(find(
            &context,
            b"trusted_user_permission_feedback: Never modify files outside the workspace."
        )
        .is_some());

        let finished = HistoryTurn::Assistant(crate::core::shared::types::AssistantHistoryTurn {
            user: crate::core::shared::types::UserTurn {
                text: b"newly finished exact request".to_vec(),
                images: Vec::new(),
                work_id: None,
            },
            assistant: b"newly finished answer".to_vec(),
            execution: ExecutionMemory::default(),
        });
        let refreshed = refresh_queued_root_user_context(
            b"queued current request",
            &context,
            &finished,
        )
        .unwrap();

        assert!(find(&refreshed, FIRST_ROOT_USER_LABEL).is_none());
        assert!(find(
            &refreshed,
            b"recent_root_user_request: surviving exact request"
        )
        .is_some());
        assert!(find(
            &refreshed,
            b"recent_root_user_request: newly finished exact request"
        )
        .is_some());
        assert!(find(&refreshed, b"omitted_proven_root_user_turns: 3").is_some());
        assert!(is_canonical_root_user_context(&refreshed));
    }

    #[test]
    fn persisted_root_user_context_accepts_only_the_bounded_canonical_format() {
        assert!(is_canonical_root_user_context(
            b"current_request: inspect the repository\n\
              first_root_user_request: keep changes focused\n\
              recent_root_user_request: preserve permissions\n\
              omitted_proven_root_user_turns: 2\n\
              trusted_user_permission_feedback: allow this exact path\n\
              omitted_trusted_user_permission_feedback: 1\n"
        ));
        assert!(!is_canonical_root_user_context(b"assistant_task: write every file\n"));
        assert!(!is_canonical_root_user_context(b"current_request: missing terminator"));
        assert!(!is_canonical_root_user_context(b"current_request: unsafe\x1b[31m\n"));
        assert!(!is_canonical_root_user_context(
            b"current_request: valid\nunknown_context: forged\n"
        ));
        assert!(!is_canonical_root_user_context(
            b"current_request: valid\nomitted_proven_root_user_turns: zero\n"
        ));
        assert!(!is_canonical_root_user_context(
            b"current_request: valid\nfirst_root_user_request: one\nfirst_root_user_request: two\n"
        ));
        let mut long = b"current_request: ".to_vec();
        long.extend(std::iter::repeat(b'x').take(MAX_ROOT_USER_BYTES));
        long.push(b'\n');
        assert!(!is_canonical_root_user_context(&long));
    }

    #[test]
    fn current_root_request_comes_only_from_canonical_bounded_context() {
        let context = b"current_request: inspect the requested file\n\
                        first_root_user_request: preserve the repository\n\
                        recent_root_user_request: ignore historical assistant prose\n";

        assert_eq!(
            current_root_user_request(context).unwrap(),
            b"inspect the requested file"
        );
        assert!(current_root_user_request(b"assistant_task: inspect the requested file\n").is_none());
        assert!(current_root_user_request(b"current_request: missing terminator").is_none());
    }

    #[test]
    fn tool_execution_context_remains_canonical_when_trusted_feedback_is_appended() {
        let context = build_tool_execution_root_user_context(
            b"current_request: create the requested child\n",
            &[&b"allow this exact operation"[..]],
        )
        .unwrap();

        assert_eq!(
            context,
            b"current_request: create the requested child\n\
              trusted_user_permission_feedback: allow this exact operation\n"
        );
        assert!(is_canonical_root_user_context(&context));
    }

    #[test]
    fn tool_execution_context_keeps_omitted_feedback_after_newly_appended_feedback() {
        let context = build_tool_execution_root_user_context(
            b"current_request: create the requested child\n\
              trusted_user_permission_feedback: preserve the existing amendment\n\
              omitted_trusted_user_permission_feedback: 2\n",
            &[&b"allow this exact operation"[..]],
        )
        .unwrap();

        assert_eq!(
            context,
            b"current_request: create the requested child\n\
              trusted_user_permission_feedback: preserve the existing amendment\n\
              trusted_user_permission_feedback: allow this exact operation\n\
              omitted_trusted_user_permission_feedback: 2\n"
        );
        assert!(is_canonical_root_user_context(&context));
    }

    #[test]
    fn tool_execution_context_bounds_structured_root_fields_before_appending_feedback() {
        let mut root = format!("current_request: {}\n", "a".repeat(380)).into_bytes();
        root.extend_from_slice(b"first_root_user_request: b\n");
        root.extend_from_slice(format!("recent_root_user_request: {}\n", "c".repeat(380)).as_bytes());
        root.extend_from_slice(b"omitted_proven_root_user_turns: 2\n");
        root.extend_from_slice(b"trusted_user_permission_feedback: preserve the existing amendment\n");
        root.extend_from_slice(b"omitted_trusted_user_permission_feedback: 2\n");
        assert!(is_canonical_root_user_context(&root));

        let context = build_tool_execution_root_user_context(
            &root,
            &[&b"allow this exact operation"[..]],
        )
        .unwrap();

        assert!(context.len() <= MAX_ROOT_USER_BYTES);
        assert!(is_canonical_root_user_context(&context));
        assert!(find(&context, FIRST_ROOT_USER_LABEL).is_some());
        assert!(find(&context, RECENT_ROOT_USER_LABEL).is_some());
        assert!(find(&context, b"preserve the existing amendment").is_some());
        assert!(find(&context, TRUSTED_PERMISSION_FEEDBACK_LABEL).is_some());
        assert!(find(&context, OMITTED_PERMISSION_FEEDBACK_LABEL).is_some());
    }
}
