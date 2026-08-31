//! Feedback entry point (ported 1:1 from zig-reference/src/core/feedback/runtime.zig).

/// Feedback URL surfaced to the CLI. It never changes per-profile, so it is a
/// plain constant rather than a runtime lookup.
#[allow(non_upper_case_globals)]
pub const url: &str = "https://fx.sh/feedback";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feedback_url_stays_on_the_fx_sh_domain() {
        assert_eq!("https://fx.sh/feedback", url);
    }
}
