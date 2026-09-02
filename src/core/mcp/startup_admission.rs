//! Pure admission policy for selecting which configured MCP servers connect in
//! each product startup phase.
//!
//! Ported 1:1 from `zig-reference/src/core/mcp/startup_admission.zig`.

use super::mcp_contract::WorkspaceAdmission;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Phase {
    All,
    AskStartup,
    AskDeferred,
    AcpStartup,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Decision {
    Connect,
    Deferred,
    Disabled,
}

pub fn decide(
    enabled: bool,
    required: bool,
    workspace_admission: Option<WorkspaceAdmission>,
    phase: Phase,
) -> Decision {
    if !enabled {
        return Decision::Disabled;
    }
    if let Some(admission) = workspace_admission {
        return match admission {
            WorkspaceAdmission::Rejected | WorkspaceAdmission::Pending => Decision::Disabled,
            WorkspaceAdmission::Approved => match phase {
                Phase::All | Phase::AskStartup | Phase::AcpStartup => Decision::Connect,
                Phase::AskDeferred => Decision::Deferred,
            },
        };
    }
    match phase {
        Phase::All => Decision::Connect,
        Phase::AskStartup => {
            if required {
                Decision::Connect
            } else {
                Decision::Deferred
            }
        }
        Phase::AskDeferred => {
            if required {
                Decision::Deferred
            } else {
                Decision::Connect
            }
        }
        Phase::AcpStartup => Decision::Connect,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_admission_keeps_ask_required_servers_eager_and_optional_servers_deferred() {
        assert_eq!(
            Decision::Connect,
            decide(true, true, None, Phase::All)
        );
        assert_eq!(
            Decision::Connect,
            decide(true, false, None, Phase::All)
        );
        assert_eq!(
            Decision::Connect,
            decide(true, true, None, Phase::AskStartup)
        );
        assert_eq!(
            Decision::Deferred,
            decide(true, false, None, Phase::AskStartup)
        );
        assert_eq!(
            Decision::Deferred,
            decide(true, true, None, Phase::AskDeferred)
        );
        assert_eq!(
            Decision::Connect,
            decide(true, false, None, Phase::AskDeferred)
        );
        assert_eq!(
            Decision::Connect,
            decide(true, true, None, Phase::AcpStartup)
        );
        assert_eq!(
            Decision::Connect,
            decide(true, false, None, Phase::AcpStartup)
        );
    }

    #[test]
    fn disabled_servers_never_enter_a_connection_phase() {
        assert_eq!(
            Decision::Disabled,
            decide(false, true, None, Phase::All)
        );
        assert_eq!(
            Decision::Disabled,
            decide(
                false,
                true,
                Some(WorkspaceAdmission::Pending),
                Phase::AskStartup
            )
        );
        assert_eq!(
            Decision::Disabled,
            decide(
                false,
                false,
                Some(WorkspaceAdmission::Approved),
                Phase::AskDeferred
            )
        );
        assert_eq!(
            Decision::Disabled,
            decide(
                false,
                false,
                Some(WorkspaceAdmission::Rejected),
                Phase::AcpStartup
            )
        );
    }

    #[test]
    fn workspace_admission_is_phase_derived_and_reject_is_absorbing() {
        let cases = [
            (
                WorkspaceAdmission::Approved,
                Decision::Connect,
                Decision::Connect,
                Decision::Deferred,
                Decision::Connect,
            ),
            (
                WorkspaceAdmission::Pending,
                Decision::Disabled,
                Decision::Disabled,
                Decision::Disabled,
                Decision::Disabled,
            ),
            (
                WorkspaceAdmission::Rejected,
                Decision::Disabled,
                Decision::Disabled,
                Decision::Disabled,
                Decision::Disabled,
            ),
        ];
        for (admission, all, ask_startup, ask_deferred, acp_startup) in cases {
            assert_eq!(
                all,
                decide(true, false, Some(admission), Phase::All)
            );
            assert_eq!(
                ask_startup,
                decide(true, false, Some(admission), Phase::AskStartup)
            );
            assert_eq!(
                ask_deferred,
                decide(true, false, Some(admission), Phase::AskDeferred)
            );
            assert_eq!(
                acp_startup,
                decide(true, false, Some(admission), Phase::AcpStartup)
            );
        }
    }
}
