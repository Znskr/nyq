//! Worker status projection (ported 1:1 from
//! zig-reference/src/core/output/worker_status.zig).

use crate::core::output::activity_runtime::{self, Tone};
use crate::core::shared::types;

const RECOVERED_STATUS_VISIBLE_MS: i64 = 1_500;

struct RouteRecoveryStatus {
    status: types::RouteRecoveryStatus,
    label: Vec<u8>,
    expires_at_ms: i64,
}

impl RouteRecoveryStatus {
    fn init(status: types::RouteRecoveryStatus, now_ms: i64) -> Self {
        let label = format_status_label(&status).into_bytes();
        Self {
            status,
            label,
            expires_at_ms: if status.is_recovered() {
                now_ms + RECOVERED_STATUS_VISIBLE_MS
            } else {
                0
            },
        }
    }

    fn refresh_retry_label(&mut self, now: i128) -> bool {
        if self.status.kind != types::RouteRecoveryKind::AutoRetry
            || self.status.retry_deadline.is_none()
        {
            return false;
        }

        let next_label = format_retry_label(&self.status, now).into_bytes();
        if self.label == next_label {
            return false;
        }
        self.label = next_label;
        true
    }

    fn projection(&self) -> activity_runtime::ActivityProjection {
        let tone = match self.status.tone() {
            types::RouteRecoveryStatusTone::Warning => Tone::Warning,
            types::RouteRecoveryStatusTone::Success => Tone::Success,
            types::RouteRecoveryStatusTone::Danger => Tone::Danger,
        };
        activity_runtime::ActivityProjection::TurnThinking(
            activity_runtime::TurnThinking {
                label: self.label.clone(),
                tone,
            },
        )
    }

    fn is_expired(&self, now_ms: i64) -> bool {
        self.expires_at_ms != 0 && now_ms >= self.expires_at_ms
    }
}

struct ApiStatus {
    label: Vec<u8>,
    tone: Tone,
}

impl ApiStatus {
    fn init(label: &[u8], tone: Tone) -> Self {
        Self {
            label: label.to_vec(),
            tone,
        }
    }

    fn projection(&self) -> activity_runtime::ActivityProjection {
        activity_runtime::ActivityProjection::TurnThinking(
            activity_runtime::TurnThinking {
                label: self.label.clone(),
                tone: self.tone,
            },
        )
    }
}

#[allow(private_interfaces)]
pub enum State {
    None,
    RouteRecovery(RouteRecoveryStatus),
    Api(ApiStatus),
}

impl State {
    pub fn projection(&self) -> Option<activity_runtime::ActivityProjection> {
        match self {
            State::None => None,
            State::RouteRecovery(status) => Some(status.projection()),
            State::Api(status) => Some(status.projection()),
        }
    }

    pub fn set_route_recovery(
        &mut self,
        status: types::RouteRecoveryStatus,
        now_ms: i64,
    ) {
        *self = State::RouteRecovery(RouteRecoveryStatus::init(status, now_ms));
    }

    pub fn set_api(&mut self, label: &[u8], tone: Tone) {
        *self = State::Api(ApiStatus::init(label, tone));
    }

    pub fn clear_route_recovery(&mut self) -> bool {
        match self {
            State::RouteRecovery(_) => {
                *self = State::None;
                true
            }
            State::None | State::Api(_) => false,
        }
    }

    pub fn clear_recovered_route(&mut self) -> bool {
        match self {
            State::RouteRecovery(status) if status.status.is_recovered() => {
                *self = State::None;
                true
            }
            State::RouteRecovery(_) | State::None | State::Api(_) => false,
        }
    }

    pub fn clear(&mut self) -> bool {
        match self {
            State::None => false,
            State::RouteRecovery(_) | State::Api(_) => {
                *self = State::None;
                true
            }
        }
    }

    pub fn reset(&mut self) {
        *self = State::None;
    }

    pub fn expire_transient(&mut self, now_ms: i64) -> bool {
        match self {
            State::RouteRecovery(status) if status.is_expired(now_ms) => {
                *self = State::None;
                true
            }
            State::RouteRecovery(_) | State::None | State::Api(_) => false,
        }
    }

    pub fn refresh_route_recovery(&mut self, now: i128) -> bool {
        match self {
            State::RouteRecovery(status) => status.refresh_retry_label(now),
            State::None | State::Api(_) => false,
        }
    }
}

fn format_status_label(status: &types::RouteRecoveryStatus) -> String {
    if status.is_recovered() {
        return format_recovered_label(status);
    }
    let base = route_recovery_base_label(status);
    if status.action == Some(types::ModelRecoveryAction::WaitingForConnectivity) {
        return format!("{base} · Esc to try later");
    }
    if status.action == Some(types::ModelRecoveryAction::Paused) {
        return match status.required_action {
            types::ModelRecoveryRequiredAction::ContinueLater => {
                format!("{base} · /continue to resume")
            }
            types::ModelRecoveryRequiredAction::InspectUncertainTool => {
                format!("{base} · inspect tool state before /continue")
            }
            types::ModelRecoveryRequiredAction::ChangeRequest => {
                format!("{base} · change the request to continue")
            }
            types::ModelRecoveryRequiredAction::None => base,
        };
    }
    base
}

fn format_retry_label(
    status: &types::RouteRecoveryStatus,
    now: i128,
) -> String {
    let mut projected = *status;
    let Some(deadline) = status.retry_deadline else {
        return format_status_label(status);
    };
    if now < deadline {
        let remaining_ns = deadline - now;
        let remaining_seconds = (remaining_ns - 1) / 1_000_000_000 + 1;
        projected.delay_seconds =
            u64::try_from(remaining_seconds).unwrap_or(u64::MAX);
    } else {
        projected.delay_seconds = 0;
    }
    format_status_label(&projected)
}

fn format_recovered_label(status: &types::RouteRecoveryStatus) -> String {
    match status.kind {
        types::RouteRecoveryKind::AutoRecovered => {
            if status.succeeded_attempt > 0 && status.attempt_limit > 0 {
                format!(
                    "✓ recovered · attempt {}/{}",
                    status.succeeded_attempt, status.attempt_limit
                )
            } else {
                "✓ recovered".to_string()
            }
        }
        types::RouteRecoveryKind::ManualRecoveredWithoutFast => {
            "✓ recovered · Fast disabled".to_string()
        }
        _ => route_recovery_base_label(status),
    }
}

fn route_recovery_base_label(status: &types::RouteRecoveryStatus) -> String {
    match status.kind {
        types::RouteRecoveryKind::AutoRetry => recovery_label(status),
        types::RouteRecoveryKind::AutoRecovered => format!(
            "✓ recovered · succeeded on attempt {}/{}",
            status.succeeded_attempt, status.attempt_limit
        ),
        types::RouteRecoveryKind::ManualRetryWithoutFast => {
            manual_retry_label(status)
        }
        types::RouteRecoveryKind::ManualRecoveredWithoutFast => {
            "✓ recovered · Fast disabled".to_string()
        }
        types::RouteRecoveryKind::TerminalProviderError => paused_label(status),
        types::RouteRecoveryKind::UnsafeAssistantOutput => fixed_failure_label(
            status,
            "Response interrupted",
            "partial output preserved",
        ),
        types::RouteRecoveryKind::UnsafeToolStart => {
            fixed_failure_label(status, "Tool state needs review", "context preserved")
        }
        types::RouteRecoveryKind::ContentFilter => {
            fixed_failure_label(status, "blocked", "content filter")
        }
    }
}

fn diagnostic_text(status: &types::RouteRecoveryStatus) -> Option<String> {
    status
        .diagnostic
        .map(|d| String::from_utf8_lossy(d.view()).into_owned())
}

fn manual_retry_label(status: &types::RouteRecoveryStatus) -> String {
    match diagnostic_text(status) {
        Some(d) => {
            format!("⚠ Provider route unavailable · {d} · continuing without Fast")
        }
        None => "⚠ Provider route unavailable · continuing without Fast".to_string(),
    }
}

fn fixed_failure_label(
    status: &types::RouteRecoveryStatus,
    cause: &str,
    action: &str,
) -> String {
    match diagnostic_text(status) {
        Some(d) => format!("⚠ {cause} · {d} · {action}"),
        None => format!("⚠ {cause} · {action}"),
    }
}

fn recovery_label(status: &types::RouteRecoveryStatus) -> String {
    let cause = match status
        .cause
        .unwrap_or(types::ModelRecoveryCause::ProviderUnavailable)
    {
        types::ModelRecoveryCause::NetworkInterrupted => "Network interrupted",
        types::ModelRecoveryCause::ResponseInterrupted => "Response ended early",
        types::ModelRecoveryCause::ProviderUnavailable => "Provider unavailable",
        types::ModelRecoveryCause::RateLimited => "Rate limited",
        types::ModelRecoveryCause::SystemResumed => "Mac woke from sleep",
        types::ModelRecoveryCause::Authentication => "Authentication refreshed",
        types::ModelRecoveryCause::RequestLimitReached => {
            "Provider request limit reached"
        }
    };
    let action = match status
        .action
        .unwrap_or(types::ModelRecoveryAction::RetryingRequest)
    {
        types::ModelRecoveryAction::RetryingRequest => "retrying request",
        types::ModelRecoveryAction::ContinuingResponse => "continuing response",
        types::ModelRecoveryAction::RegeneratingTool => {
            "regenerating unstarted tool"
        }
        types::ModelRecoveryAction::ContinuingAfterTool => {
            "continuing after confirmed tool"
        }
        types::ModelRecoveryAction::ReconcilingTool => {
            "checking uncertain tool state"
        }
        types::ModelRecoveryAction::WaitingForConnectivity => {
            "waiting for connection"
        }
        types::ModelRecoveryAction::Paused => "recovery paused",
    };
    let diag = diagnostic_text(status);
    let (failed, limit) = (status.failed_attempt, status.attempt_limit);
    if status.delay_seconds > 0 {
        return match diag {
            Some(d) => format!(
                "⚠ {cause} · {d} · {action} in {}s · attempt {failed}/{limit}",
                status.delay_seconds
            ),
            None => format!(
                "⚠ {cause} · {action} in {}s · attempt {failed}/{limit}",
                status.delay_seconds
            ),
        };
    }
    match diag {
        Some(d) => {
            format!("⚠ {cause} · {d} · {action} · attempt {failed}/{limit}")
        }
        None => format!("⚠ {cause} · {action} · attempt {failed}/{limit}"),
    }
}

fn paused_label(status: &types::RouteRecoveryStatus) -> String {
    let Some(cause) = status.cause else {
        return paused_cause_label(status, "Provider unavailable");
    };
    if cause == types::ModelRecoveryCause::RateLimited
        && status.failed_attempt < status.attempt_limit
    {
        let (failed, limit) = (status.failed_attempt, status.attempt_limit);
        return match diagnostic_text(status) {
            Some(d) => format!(
                "⚠ Rate limited · {d} · server requested a longer wait · recovery paused · attempt {failed}/{limit}"
            ),
            None => format!(
                "⚠ Rate limited · server requested a longer wait · recovery paused · attempt {failed}/{limit}"
            ),
        };
    }
    if cause == types::ModelRecoveryCause::SystemResumed
        && status.failed_attempt < status.attempt_limit
    {
        let (failed, limit) = (status.failed_attempt, status.attempt_limit);
        return match diagnostic_text(status) {
            Some(d) => format!(
                "⚠ Mac woke from sleep · {d} · connection still unavailable · recovery paused · attempt {failed}/{limit}"
            ),
            None => format!(
                "⚠ Mac woke from sleep · connection still unavailable · recovery paused · attempt {failed}/{limit}"
            ),
        };
    }
    if cause == types::ModelRecoveryCause::RequestLimitReached {
        let (failed, limit) = (status.failed_attempt, status.attempt_limit);
        return match diagnostic_text(status) {
            Some(d) => format!(
                "⚠ Response paused · {d} · {failed}/{limit} provider-request safety limit reached"
            ),
            None => format!(
                "⚠ Response paused · {failed}/{limit} provider-request safety limit reached"
            ),
        };
    }
    let name = match cause {
        types::ModelRecoveryCause::NetworkInterrupted => "Network interrupted",
        types::ModelRecoveryCause::ResponseInterrupted => "Response ended early",
        types::ModelRecoveryCause::ProviderUnavailable => "Provider unavailable",
        types::ModelRecoveryCause::RateLimited => "Rate limited",
        types::ModelRecoveryCause::SystemResumed => "Mac woke from sleep",
        types::ModelRecoveryCause::Authentication => "Authentication expired",
        types::ModelRecoveryCause::RequestLimitReached => unreachable!(),
    };
    paused_cause_label(status, name)
}

fn paused_cause_label(status: &types::RouteRecoveryStatus, name: &str) -> String {
    let (failed, limit) = (status.failed_attempt, status.attempt_limit);
    match diagnostic_text(status) {
        Some(d) => {
            format!("⚠ {name} · {d} · recovery paused after {failed}/{limit} attempts")
        }
        None => format!("⚠ {name} · recovery paused after {failed}/{limit} attempts"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn awake_timestamp(milliseconds: i64) -> i128 {
        (milliseconds as i128) * 1_000_000
    }

    fn route_status(
        kind: types::RouteRecoveryKind,
        failed_attempt: usize,
        attempt_limit: usize,
    ) -> types::RouteRecoveryStatus {
        types::RouteRecoveryStatus {
            kind,
            failed_attempt,
            succeeded_attempt: 0,
            attempt_limit,
            cause: None,
            action: None,
            required_action: types::ModelRecoveryRequiredAction::None,
            delay_seconds: 0,
            retry_deadline: None,
            diagnostic: None,
        }
    }

    fn turn_thinking(
        projection: activity_runtime::ActivityProjection,
    ) -> activity_runtime::TurnThinking {
        match projection {
            activity_runtime::ActivityProjection::TurnThinking(p) => p,
            _ => panic!("expected turn thinking projection"),
        }
    }

    #[test]
    fn worker_status_projects_route_recovery_and_expires_recovered_state() {
        let mut state = State::None;
        state.set_route_recovery(
            route_status(types::RouteRecoveryKind::AutoRetry, 1, 3),
            1_000,
        );

        let projection = turn_thinking(state.projection().unwrap());
        assert_eq!(projection.tone, Tone::Warning);
        assert_eq!(
            projection.label,
            "⚠ Provider unavailable · retrying request · attempt 1/3".as_bytes()
        );

        state.set_route_recovery(
            types::RouteRecoveryStatus {
                kind: types::RouteRecoveryKind::AutoRecovered,
                succeeded_attempt: 3,
                attempt_limit: 3,
                ..route_status(types::RouteRecoveryKind::AutoRecovered, 0, 3)
            },
            1_000,
        );
        assert!(!state.expire_transient(2_499));
        assert!(state.expire_transient(2_500));
        assert!(state.projection().is_none());
    }

    #[test]
    fn worker_status_refreshes_retry_countdown_from_awake_deadline() {
        let mut state = State::None;
        state.set_route_recovery(
            types::RouteRecoveryStatus {
                kind: types::RouteRecoveryKind::AutoRetry,
                failed_attempt: 1,
                attempt_limit: 3,
                delay_seconds: 4,
                retry_deadline: Some(awake_timestamp(4_000)),
                ..route_status(types::RouteRecoveryKind::AutoRetry, 1, 3)
            },
            0,
        );

        assert!(!state.refresh_route_recovery(awake_timestamp(0)));
        assert!(state.refresh_route_recovery(awake_timestamp(1_000)));
        assert_eq!(
            turn_thinking(state.projection().unwrap()).label,
            "⚠ Provider unavailable · retrying request in 3s · attempt 1/3"
                .as_bytes()
        );

        assert!(state.refresh_route_recovery(awake_timestamp(2_001)));
        assert_eq!(
            turn_thinking(state.projection().unwrap()).label,
            "⚠ Provider unavailable · retrying request in 2s · attempt 1/3"
                .as_bytes()
        );

        assert!(state.refresh_route_recovery(awake_timestamp(3_750)));
        assert_eq!(
            turn_thinking(state.projection().unwrap()).label,
            "⚠ Provider unavailable · retrying request in 1s · attempt 1/3"
                .as_bytes()
        );

        assert!(state.refresh_route_recovery(awake_timestamp(4_000)));
        assert_eq!(
            turn_thinking(state.projection().unwrap()).label,
            "⚠ Provider unavailable · retrying request · attempt 1/3".as_bytes()
        );
        assert!(!state.refresh_route_recovery(awake_timestamp(4_250)));

        state.set_route_recovery(
            route_status(types::RouteRecoveryKind::AutoRetry, 2, 3),
            0,
        );
        assert!(!state.refresh_route_recovery(awake_timestamp(5_000)));
    }

    #[test]
    fn worker_status_api_state_is_sticky_until_explicitly_cleared() {
        let mut state = State::None;
        state.set_api(
            "⚠ API access denied · HTTP 403 · Provider: wafer".as_bytes(),
            Tone::Danger,
        );

        assert!(!state.expire_transient(i64::MAX));
        let projection = turn_thinking(state.projection().unwrap());
        assert_eq!(projection.tone, Tone::Danger);
        assert_eq!(
            projection.label,
            "⚠ API access denied · HTTP 403 · Provider: wafer".as_bytes()
        );
        assert!(state.clear());
        assert!(state.projection().is_none());
    }

    #[test]
    fn worker_status_route_recovery_labels_expose_required_controls() {
        let mut state = State::None;
        state.set_route_recovery(
            types::RouteRecoveryStatus {
                kind: types::RouteRecoveryKind::AutoRetry,
                failed_attempt: 2,
                attempt_limit: 10,
                cause: Some(types::ModelRecoveryCause::SystemResumed),
                action: Some(types::ModelRecoveryAction::WaitingForConnectivity),
                ..route_status(types::RouteRecoveryKind::AutoRetry, 2, 10)
            },
            0,
        );
        assert_eq!(
            turn_thinking(state.projection().unwrap()).label,
            "⚠ Mac woke from sleep · waiting for connection · attempt 2/10 · Esc to try later"
                .as_bytes()
        );

        state.set_route_recovery(
            types::RouteRecoveryStatus {
                kind: types::RouteRecoveryKind::TerminalProviderError,
                failed_attempt: 2,
                attempt_limit: 10,
                cause: Some(types::ModelRecoveryCause::SystemResumed),
                action: Some(types::ModelRecoveryAction::Paused),
                required_action: types::ModelRecoveryRequiredAction::ContinueLater,
                ..route_status(
                    types::RouteRecoveryKind::TerminalProviderError,
                    2,
                    10,
                )
            },
            0,
        );
        assert_eq!(
            turn_thinking(state.projection().unwrap()).label,
            "⚠ Mac woke from sleep · connection still unavailable · recovery paused · attempt 2/10 · /continue to resume"
                .as_bytes()
        );

        state.set_route_recovery(
            types::RouteRecoveryStatus {
                kind: types::RouteRecoveryKind::TerminalProviderError,
                failed_attempt: 10,
                attempt_limit: 10,
                cause: Some(types::ModelRecoveryCause::ResponseInterrupted),
                action: Some(types::ModelRecoveryAction::Paused),
                required_action: types::ModelRecoveryRequiredAction::InspectUncertainTool,
                ..route_status(
                    types::RouteRecoveryKind::TerminalProviderError,
                    10,
                    10,
                )
            },
            0,
        );
        assert_eq!(
            turn_thinking(state.projection().unwrap()).label,
            "⚠ Response ended early · recovery paused after 10/10 attempts · inspect tool state before /continue"
                .as_bytes()
        );

        state.set_route_recovery(
            types::RouteRecoveryStatus {
                kind: types::RouteRecoveryKind::ManualRecoveredWithoutFast,
                ..route_status(
                    types::RouteRecoveryKind::ManualRecoveredWithoutFast,
                    0,
                    0,
                )
            },
            0,
        );
        assert_eq!(
            turn_thinking(state.projection().unwrap()).label,
            "✓ recovered · Fast disabled".as_bytes()
        );
    }
}
