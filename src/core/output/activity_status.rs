//! Activity status labels (ported 1:1 from
//! zig-reference/src/core/output/activity_status.zig).

use crate::core::shared::types;
use std::io::Write;

pub use types::StreamState;

#[derive(Clone, Copy)]
struct ActivityPart {
    count: usize,
    singular: &'static [u8],
    plural: &'static [u8],
    verb: &'static [u8],
}

fn stream_status_verb(stream: &StreamState) -> &'static [u8] {
    match stream.last_activity_kind {
        None => b"thinking",
        Some(types::ToolActivityKind::List) => b"listing",
        Some(types::ToolActivityKind::Read) => b"reading",
        Some(types::ToolActivityKind::Write) => b"writing",
        Some(types::ToolActivityKind::Edit) => b"editing",
        Some(types::ToolActivityKind::Command) => b"running",
        Some(types::ToolActivityKind::Subagent) => b"delegating",
        Some(types::ToolActivityKind::Open) => b"opening",
        Some(types::ToolActivityKind::Ask) => b"asking",
    }
}

fn marked_turn_phase_label(phase: types::TurnPhase) -> &'static [u8] {
    match phase {
        types::TurnPhase::Thinking => "• Thinking".as_bytes(),
        types::TurnPhase::Generating => "• Generating".as_bytes(),
        types::TurnPhase::Running => "• Running".as_bytes(),
    }
}

pub fn build_turn_label(
    stream: &StreamState,
    now_ms: i64,
) -> Option<Vec<u8>> {
    if stream.last_activity_kind.is_some() {
        return None;
    }
    let mut out = Vec::new();
    out.extend_from_slice(marked_turn_phase_label(stream.phase));
    append_turn_elapsed_suffix(&mut out, stream, now_ms);
    append_turn_token_suffix(&mut out, stream);
    Some(out)
}

pub const ACTIVITY_BLINK_HALF_PERIOD_MS: i64 = 500;

fn turn_clock_now(stream: &StreamState, now_ms: i64) -> i64 {
    if stream.waiting_since_ms > 0 && stream.waiting_since_ms < now_ms {
        return stream.waiting_since_ms;
    }
    now_ms
}

pub fn activity_blink_visible(
    stream: &StreamState,
    now_ms: i64,
) -> Option<bool> {
    if stream.turn_started_ms <= 0 || now_ms < stream.turn_started_ms {
        return None;
    }
    if stream.waiting_since_ms > 0 {
        return Some(true);
    }
    let half_periods =
        (now_ms - stream.turn_started_ms) / ACTIVITY_BLINK_HALF_PERIOD_MS;
    Some(half_periods % 2 == 0)
}

fn append_turn_elapsed_suffix(
    out: &mut Vec<u8>,
    stream: &StreamState,
    now_ms: i64,
) {
    if stream.turn_started_ms <= 0 || now_ms < stream.turn_started_ms {
        return;
    }
    let ms_per_second = 2 * ACTIVITY_BLINK_HALF_PERIOD_MS;
    let seconds =
        (turn_clock_now(stream, now_ms) - stream.turn_started_ms) / ms_per_second;
    out.extend_from_slice(b" (");
    write_elapsed(out, seconds);
    out.push(b')');
}

fn write_elapsed(out: &mut Vec<u8>, seconds: i64) {
    let secs = seconds % 60;
    let total_minutes = seconds / 60;
    let mins = total_minutes % 60;
    let hours = total_minutes / 60;
    if hours > 0 {
        let _ = write!(out, "{}h{}m{}s", hours, mins, secs);
    } else if mins > 0 {
        let _ = write!(out, "{}m{}s", mins, secs);
    } else {
        let _ = write!(out, "{}s", secs);
    }
}

pub fn sync_waiting_clock(stream: &mut StreamState, waiting: bool, now_ms: i64) {
    if waiting {
        if stream.active && stream.waiting_since_ms == 0 {
            stream.waiting_since_ms = now_ms;
        }
        return;
    }
    if stream.waiting_since_ms == 0 {
        return;
    }
    let waited = now_ms - stream.waiting_since_ms;
    if waited > 0 && stream.turn_started_ms > 0 {
        stream.turn_started_ms += waited;
    }
    stream.waiting_since_ms = 0;
}

const MARKER_INDENT: &[u8] = b"  ";

pub fn build_completed_turn_label(stream: &StreamState) -> Vec<u8> {
    let progress = stream.token_progress;
    if progress.input_tokens == 0 && progress.output_tokens == 0 {
        return MARKER_INDENT.to_vec();
    }
    let mut out = Vec::new();
    out.extend_from_slice(MARKER_INDENT);
    write_token_progress(&mut out, progress);
    out
}

pub fn build_progress_label(stream: &StreamState) -> Option<Vec<u8>> {
    if !stream.active {
        return None;
    }
    Some(build_thinking_label_full(stream))
}

fn build_thinking_label_full(stream: &StreamState) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(stream_status_verb(stream));

    let parts = activity_parts(stream);
    if !has_activity_parts(&parts) {
        append_turn_token_suffix(&mut out, stream);
        return out;
    }

    out.extend_from_slice(b" | ");

    let mut first = true;
    for part in &parts {
        if part.count == 0 {
            continue;
        }
        write_activity_part(&mut out, &mut first, *part);
    }

    append_turn_token_suffix(&mut out, stream);
    out
}

fn activity_parts(stream: &StreamState) -> [ActivityPart; 7] {
    [
        ActivityPart {
            count: stream.read_count,
            singular: b"file",
            plural: b"files",
            verb: b"read",
        },
        ActivityPart {
            count: stream.list_count,
            singular: b"directory",
            plural: b"directories",
            verb: b"listed",
        },
        ActivityPart {
            count: stream.write_count,
            singular: b"file",
            plural: b"files",
            verb: b"wrote",
        },
        ActivityPart {
            count: stream.edit_count,
            singular: b"file",
            plural: b"files",
            verb: b"edited",
        },
        ActivityPart {
            count: stream.command_count,
            singular: b"command",
            plural: b"commands",
            verb: b"started",
        },
        ActivityPart {
            count: stream.subagent_count,
            singular: b"subagent",
            plural: b"subagents",
            verb: b"created",
        },
        ActivityPart {
            count: stream.open_count,
            singular: b"file",
            plural: b"files",
            verb: b"opened",
        },
    ]
}

fn has_activity_parts(parts: &[ActivityPart]) -> bool {
    parts.iter().any(|part| part.count > 0)
}

fn write_activity_part(
    out: &mut Vec<u8>,
    first: &mut bool,
    part: ActivityPart,
) {
    if !*first {
        out.extend_from_slice(b", ");
    }
    *first = false;
    let noun = if part.count == 1 {
        part.singular
    } else {
        part.plural
    };
    out.extend_from_slice(
        format!(
            "{} {} {}",
            part.count,
            String::from_utf8_lossy(noun),
            String::from_utf8_lossy(part.verb)
        )
        .as_bytes(),
    );
}

pub fn format_token_count_compact(tokens: u64) -> Vec<u8> {
    if tokens < 1000 {
        return format!("{tokens}").into_bytes();
    }

    let whole = tokens / 1000;
    let tenths = (tokens % 1000) / 100;
    if whole < 10 && tenths > 0 {
        return format!("{whole}.{tenths}k").into_bytes();
    }
    format!("{whole}k").into_bytes()
}

fn write_token_progress(
    out: &mut Vec<u8>,
    progress: types::TurnTokenProgress,
) {
    let input = format_token_count_compact(progress.input_tokens);
    let output = format_token_count_compact(progress.output_tokens);
    out.extend_from_slice(
        format!(
            "(↑{} ↓{})",
            String::from_utf8_lossy(&input),
            String::from_utf8_lossy(&output)
        )
        .as_bytes(),
    );
}

pub fn append_token_progress_suffix(
    out: &mut Vec<u8>,
    progress: types::TurnTokenProgress,
) {
    if progress.input_tokens == 0 && progress.output_tokens == 0 {
        return;
    }
    out.push(b' ');
    write_token_progress(out, progress);
}

pub fn append_turn_token_suffix(out: &mut Vec<u8>, stream: &StreamState) {
    append_token_progress_suffix(out, stream.token_progress);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream(active: bool) -> StreamState {
        StreamState {
            active,
            ..StreamState::default()
        }
    }

    fn s(v: Vec<u8>) -> String {
        String::from_utf8(v).unwrap()
    }

    fn opt_s(v: Option<Vec<u8>>) -> String {
        s(v.unwrap())
    }

    #[test]
    fn build_turn_label_returns_thinking_when_no_tool_activity() {
        assert_eq!(
            build_turn_label(&stream(true), 0).as_deref(),
            Some("• Thinking".as_bytes())
        );
    }

    #[test]
    fn build_turn_label_renders_turn_token_suffix_with_input_only() {
        let state = StreamState {
            active: true,
            token_progress: types::TurnTokenProgress {
                input_tokens: 10,
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(
            build_turn_label(&state, 0).as_deref(),
            Some("• Thinking (↑10 ↓0)".as_bytes())
        );
    }

    #[test]
    fn build_turn_label_renders_turn_token_suffix_with_input_and_output() {
        let state = StreamState {
            active: true,
            token_progress: types::TurnTokenProgress {
                input_tokens: 10,
                output_tokens: 20,
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(
            build_turn_label(&state, 0).as_deref(),
            Some("• Thinking (↑10 ↓20)".as_bytes())
        );
    }

    #[test]
    fn build_turn_label_renders_elapsed_seconds_for_the_running_turn() {
        let state = StreamState {
            active: true,
            turn_started_ms: 1_000,
            ..Default::default()
        };
        assert_eq!(
            build_turn_label(&state, 6_500).as_deref(),
            Some("• Thinking (5s)".as_bytes())
        );
    }

    #[test]
    fn build_turn_label_renders_elapsed_minutes_and_seconds_for_long_turns() {
        let state = StreamState {
            active: true,
            turn_started_ms: 1_000,
            ..Default::default()
        };
        assert_eq!(
            build_turn_label(&state, 1_000 + 1_080 * 1_000).as_deref(),
            Some("• Thinking (18m0s)".as_bytes())
        );
    }

    #[test]
    fn build_turn_label_renders_elapsed_hours_for_very_long_turns() {
        let state = StreamState {
            active: true,
            turn_started_ms: 1_000,
            ..Default::default()
        };
        assert_eq!(
            build_turn_label(&state, 1_000 + 3_663 * 1_000).as_deref(),
            Some("• Thinking (1h1m3s)".as_bytes())
        );
    }

    #[test]
    fn build_turn_label_renders_elapsed_seconds_before_the_token_suffix() {
        let state = StreamState {
            active: true,
            turn_started_ms: 1_000,
            token_progress: types::TurnTokenProgress {
                input_tokens: 10,
                output_tokens: 20,
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(
            build_turn_label(&state, 13_000).as_deref(),
            Some("• Thinking (12s) (↑10 ↓20)".as_bytes())
        );
    }

    #[test]
    fn activity_blink_relights_exactly_when_the_seconds_digit_advances() {
        let state = StreamState {
            active: true,
            turn_started_ms: 1_000,
            ..Default::default()
        };
        assert_eq!(activity_blink_visible(&state, 1_000), Some(true));
        assert_eq!(activity_blink_visible(&state, 1_499), Some(true));
        assert_eq!(activity_blink_visible(&state, 1_500), Some(false));
        assert_eq!(activity_blink_visible(&state, 1_999), Some(false));
        assert_eq!(activity_blink_visible(&state, 2_000), Some(true));

        let at_relight = build_turn_label(&state, 2_000).unwrap();
        assert_eq!(s(at_relight), "• Thinking (1s)");
    }

    #[test]
    fn activity_blink_has_no_clock_without_a_running_turn() {
        assert_eq!(activity_blink_visible(&stream(true), 5_000), None);
        let state = StreamState {
            active: true,
            turn_started_ms: 6_000,
            ..Default::default()
        };
        assert_eq!(activity_blink_visible(&state, 5_000), None);
    }

    #[test]
    fn turn_clock_freezes_while_waiting_on_user_input_and_excludes_the_wait() {
        let mut state = StreamState {
            active: true,
            turn_started_ms: 1_000,
            ..Default::default()
        };

        sync_waiting_clock(&mut state, true, 5_000);
        assert_eq!(state.waiting_since_ms, 5_000);

        assert_eq!(
            build_turn_label(&state, 900_000).as_deref(),
            Some("• Thinking (4s)".as_bytes())
        );
        assert_eq!(activity_blink_visible(&state, 900_000), Some(true));

        sync_waiting_clock(&mut state, false, 900_000);
        assert_eq!(state.waiting_since_ms, 0);
        assert_eq!(
            build_turn_label(&state, 900_000).as_deref(),
            Some("• Thinking (4s)".as_bytes())
        );
        assert_eq!(
            build_turn_label(&state, 903_000).as_deref(),
            Some("• Thinking (7s)".as_bytes())
        );
    }

    #[test]
    fn waiting_clock_accumulates_across_repeated_prompts_in_one_turn() {
        let mut state = StreamState {
            active: true,
            turn_started_ms: 0,
            ..Default::default()
        };
        sync_waiting_clock(&mut state, true, 1_000);
        sync_waiting_clock(&mut state, false, 9_000);
        assert_eq!(state.turn_started_ms, 0);
        assert_eq!(state.waiting_since_ms, 0);

        state = StreamState {
            active: true,
            turn_started_ms: 1_000,
            ..Default::default()
        };
        sync_waiting_clock(&mut state, true, 2_000);
        sync_waiting_clock(&mut state, false, 10_000);
        sync_waiting_clock(&mut state, true, 11_000);
        sync_waiting_clock(&mut state, false, 20_000);
        assert_eq!(
            build_turn_label(&state, 20_000).as_deref(),
            Some("• Thinking (2s)".as_bytes())
        );
    }

    #[test]
    fn waiting_clock_does_not_start_without_an_active_stream() {
        let mut state = stream(false);
        sync_waiting_clock(&mut state, true, 5_000);
        assert_eq!(state.waiting_since_ms, 0);
    }

    #[test]
    fn build_turn_label_hides_elapsed_seconds_when_the_clock_runs_behind_the_mark() {
        let state = StreamState {
            active: true,
            turn_started_ms: 2_000,
            ..Default::default()
        };
        assert_eq!(
            build_turn_label(&state, 1_000).as_deref(),
            Some("• Thinking".as_bytes())
        );
    }

    #[test]
    fn build_completed_turn_label_renders_token_progress_without_approximation_markers() {
        let state = StreamState {
            active: true,
            token_progress: types::TurnTokenProgress {
                input_tokens: 1_500,
                output_tokens: 20,
                input_exact: false,
                output_exact: false,
            },
            ..Default::default()
        };
        assert_eq!(
            build_completed_turn_label(&state),
            "  (↑1.5k ↓20)".as_bytes()
        );
    }

    #[test]
    fn build_completed_turn_label_holds_the_marker_column_before_the_first_usage_report() {
        assert_eq!(build_completed_turn_label(&stream(true)), b"  ");
    }

    #[test]
    fn build_turn_label_returns_null_when_tool_activity_is_set() {
        let state = StreamState {
            active: true,
            last_activity_kind: Some(types::ToolActivityKind::Write),
            ..Default::default()
        };
        assert!(build_turn_label(&state, 0).is_none());
    }

    #[test]
    fn build_thinking_label_full_renders_ordered_activity_counts() {
        let state = StreamState {
            active: true,
            last_activity_kind: Some(types::ToolActivityKind::List),
            read_count: 2,
            list_count: 3,
            write_count: 4,
            edit_count: 5,
            command_count: 6,
            subagent_count: 7,
            open_count: 8,
            ..Default::default()
        };
        assert_eq!(
            build_thinking_label_full(&state),
            "listing | 2 files read, 3 directories listed, 4 files wrote, 5 files edited, 6 commands started, 7 subagents created, 8 files opened".as_bytes()
        );
    }

    #[test]
    fn build_progress_label_surfaces_active_phase_and_command_count() {
        let state = StreamState {
            active: true,
            last_activity_kind: Some(types::ToolActivityKind::Command),
            command_count: 1,
            ..Default::default()
        };
        assert_eq!(
            opt_s(build_progress_label(&state)),
            "running | 1 command started"
        );
    }

    #[test]
    fn build_progress_label_keeps_ask_status_semantic() {
        let state = StreamState {
            active: true,
            last_activity_kind: Some(types::ToolActivityKind::Ask),
            ..Default::default()
        };
        assert_eq!(opt_s(build_progress_label(&state)), "asking");
    }

    #[test]
    fn build_progress_label_appends_turn_token_suffix_after_activity_list() {
        let state = StreamState {
            active: true,
            last_activity_kind: Some(types::ToolActivityKind::List),
            read_count: 2,
            list_count: 3,
            token_progress: types::TurnTokenProgress {
                input_tokens: 10,
                output_tokens: 20,
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(
            opt_s(build_progress_label(&state)),
            "listing | 2 files read, 3 directories listed (↑10 ↓20)"
        );
    }

    #[test]
    fn build_progress_label_keeps_worst_case_activity_and_token_suffix_within_buffer() {
        let state = StreamState {
            active: true,
            last_activity_kind: Some(types::ToolActivityKind::Command),
            read_count: 9,
            list_count: 9,
            write_count: 9,
            edit_count: 9,
            command_count: 9,
            subagent_count: 9,
            open_count: 9,
            token_progress: types::TurnTokenProgress {
                input_tokens: 999_999,
                output_tokens: 999_999,
                ..Default::default()
            },
            ..Default::default()
        };
        let label = s(build_progress_label(&state).unwrap());
        assert!(label.contains("(↑999k ↓999k)"));
        assert_ne!(label, "running");
    }

    #[test]
    fn token_progress_suffix_abbreviates_values_without_exactness_markers() {
        let mut out = Vec::new();
        append_token_progress_suffix(
            &mut out,
            types::TurnTokenProgress {
                input_tokens: 50_000,
                output_tokens: 1_250,
                input_exact: true,
                output_exact: false,
            },
        );
        assert_eq!(s(out), " (↑50k ↓1.2k)");
    }
}
