//! Agent step limits and their resolution.
//!
//! Mirrors `zig-reference/src/core/config/agent_steps.zig`.

pub const DEFAULT_MAX_AGENT_STEPS: usize = 0;

pub fn resolve_max_agent_steps(configured: Option<usize>, default_value: usize) -> usize {
    configured.unwrap_or(default_value)
}

pub fn parse_max_agent_steps(raw: Option<&str>) -> Option<usize> {
    let trimmed = raw?.trim_matches(|c: char| c == ' ' || c == '\t' || c == '\r' || c == '\n');
    if trimmed.is_empty() {
        return None;
    }
    trimmed.parse::<usize>().ok()
}

pub fn resolve_max_agent_steps_with_override(
    configured: Option<usize>,
    default_value: usize,
    process_override: Option<&str>,
) -> usize {
    parse_max_agent_steps(process_override)
        .unwrap_or_else(|| resolve_max_agent_steps(configured, default_value))
}

pub fn allows_step(limit: usize, completed_steps: usize) -> bool {
    limit == 0 || completed_steps < limit
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_max_agent_steps_preserves_explicit_unbounded_zero() {
        assert_eq!(25, resolve_max_agent_steps(None, 25));
        assert_eq!(0, resolve_max_agent_steps(Some(0), 25));
        assert_eq!(50, resolve_max_agent_steps(Some(50), 25));
    }

    #[test]
    fn parse_max_agent_steps_accepts_zero_and_rejects_invalid_input() {
        assert_eq!(Some(0), parse_max_agent_steps(Some("0")));
        assert_eq!(Some(24), parse_max_agent_steps(Some("24")));
        assert_eq!(None, parse_max_agent_steps(None));
        assert_eq!(None, parse_max_agent_steps(Some("")));
        assert_eq!(None, parse_max_agent_steps(Some("abc")));
    }

    #[test]
    fn process_overrides_resolve_with_configured_and_compiled_defaults() {
        assert_eq!(
            0,
            resolve_max_agent_steps_with_override(None, DEFAULT_MAX_AGENT_STEPS, None)
        );
        assert_eq!(0, resolve_max_agent_steps_with_override(Some(0), 24, None));
        assert_eq!(24, resolve_max_agent_steps_with_override(None, 0, Some("24")));
        assert_eq!(0, resolve_max_agent_steps_with_override(Some(24), 8, Some("0")));
        assert_eq!(
            24,
            resolve_max_agent_steps_with_override(Some(24), 8, Some("invalid"))
        );
        assert_eq!(
            24,
            resolve_max_agent_steps_with_override(Some(24), 8, Some("  \t\n"))
        );
    }

    #[test]
    fn agent_step_policy_treats_zero_as_unbounded_and_positives_as_exact_caps() {
        assert!(allows_step(0, 0));
        assert!(allows_step(0, 25));
        assert!(allows_step(2, 0));
        assert!(allows_step(2, 1));
        assert!(!allows_step(2, 2));
    }
}
