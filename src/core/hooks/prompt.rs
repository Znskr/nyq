//! Stop-hook prompt helpers (ported 1:1 from zig-reference/src/core/hooks/prompt.zig).

pub use super::definitions::{StopAction, StopHandler, StopInput, StopOutcome};

const CONTINUATION_PREFIX: &str = "Continue the turn. fx hook context:\n";

pub fn build_continuation_message(context: &str) -> String {
    let mut out = String::with_capacity(CONTINUATION_PREFIX.len() + context.len());
    out.push_str(CONTINUATION_PREFIX);
    out.push_str(context);
    out
}

pub fn join_visible_segments(first: Option<&str>, second: Option<&str>) -> String {
    let earlier = first.unwrap_or("");
    let later = second.unwrap_or("");
    if earlier.is_empty() {
        return later.to_string();
    }
    if later.is_empty() {
        return earlier.to_string();
    }
    let mut out = String::with_capacity(earlier.len() + 1 + later.len());
    out.push_str(earlier);
    out.push('\n');
    out.push_str(later);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stop_continuation_message_uses_the_shared_hook_prefix() {
        let message = build_continuation_message("verify the answer");
        assert_eq!(
            message,
            "Continue the turn. fx hook context:\nverify the answer"
        );
    }

    #[test]
    fn visible_stop_segments_join_with_one_newline() {
        let joined = join_visible_segments(Some("first"), Some("second"));
        assert_eq!(joined, "first\nsecond");
    }
}
