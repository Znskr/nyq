//! Core permission gate for typed tool dispatch.
//!
//! Ported from `zig-reference/src/core/permissions/permission_gate.zig`.

use crate::core::permissions::command_admission::ToolExecutionAuthority;

/// Cloneable view of the authority variant carried by a Decision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuthorityKind {
    Ordinary,
    RunCommand,
    FileMutation,
    VisionPaths,
}
use crate::core::shared::types::{PermissionMode, ToolPermissionDenialReason};

/// Action selected by the core permission gate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Allow,
    Deny,
    Ask,
}

/// Pure permission decision with a human-readable reason.
#[derive(Clone, Debug)]
pub struct Decision {
    pub action: Action,
    pub reason: &'static str,
    pub denial_reason: Option<ToolPermissionDenialReason>,
    pub structured_model_output: bool,
    /// Single-grant behavior is unchanged; suggestions are additive metadata.
    pub grant_suggestions: Vec<crate::core::shared::types::PermissionGrant>,
    /// The authority is kept as an enum tag plus optional payload; cloning
    /// a Decision never clones the mutation payload.
    pub execution_authority: Option<AuthorityKind>,
}

impl Decision {
    fn allow(reason: &'static str) -> Self {
        Self {
            action: Action::Allow,
            reason,
            denial_reason: None,
            structured_model_output: false,
            grant_suggestions: Vec::new(),
            execution_authority: None,
        }
    }

    fn deny(reason: &'static str, denial_reason: ToolPermissionDenialReason) -> Self {
        Self {
            action: Action::Deny,
            reason,
            denial_reason: Some(denial_reason),
            structured_model_output: false,
            grant_suggestions: Vec::new(),
            execution_authority: None,
        }
    }
}

/// Tool descriptor contract required by the gate.
pub trait GateTool<Input> {
    fn reads_only(&self, input: &Input) -> bool;
    fn irreversible(&self, input: &Input) -> bool;
}

/// Context contract required by the gate.
pub struct GateContext<'a, Tool, Input> {
    pub permission_mode: PermissionMode,
    pub permission_decider: Option<
        &'a dyn Fn(&Tool, &Input) -> Result<Decision, PermissionDecisionError>,
    >,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PermissionDecisionError {
    Canceled,
    Unexpected,
    SystemResources,
    ProcessFdQuotaExceeded,
    SystemFdQuotaExceeded,
}

fn yolo_decision() -> Decision {
    Decision::allow("allowed by yolo mode")
}

fn fallback_decision<Tool: GateTool<Input>, Input>(tool: &Tool, input: &Input) -> Decision {
    if !tool.reads_only(input) {
        return Decision::deny(
            "tool invocation is not read-only",
            ToolPermissionDenialReason::PermissionRequired,
        );
    }
    if tool.irreversible(input) {
        return Decision::deny(
            "tool invocation is irreversible",
            ToolPermissionDenialReason::PermissionRequired,
        );
    }
    Decision::allow("read-only invocation allowed")
}

/// Decides an ordinary typed-tool invocation without an operational error edge.
pub fn decide_ordinary<Tool, Input>(
    tool: &Tool,
    input: &Input,
    ctx: &GateContext<'_, Tool, Input>,
) -> Decision
where
    Tool: GateTool<Input>,
{
    if ctx.permission_mode == PermissionMode::Yolo {
        return yolo_decision();
    }
    if let Some(decider) = ctx.permission_decider {
        return decider(tool, input).unwrap_or_else(|_| fallback_decision(tool, input));
    }
    fallback_decision(tool, input)
}

/// Decides a run-dispatch invocation while preserving policy I/O failures.
pub fn decide<Tool, Input>(
    tool: &Tool,
    input: &Input,
    ctx: &GateContext<'_, Tool, Input>,
) -> Result<Decision, PermissionDecisionError>
where
    Tool: GateTool<Input>,
{
    if ctx.permission_mode == PermissionMode::Yolo {
        return Ok(yolo_decision());
    }
    if let Some(decider) = ctx.permission_decider {
        return decider(tool, input);
    }
    Ok(fallback_decision(tool, input))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MockInput {
        read_only: bool,
        destructive: bool,
    }

    impl Default for MockInput {
        fn default() -> Self {
            Self {
                read_only: true,
                destructive: false,
            }
        }
    }

    struct MockTool;

    impl GateTool<MockInput> for MockTool {
        fn reads_only(&self, input: &MockInput) -> bool {
            input.read_only
        }
        fn irreversible(&self, input: &MockInput) -> bool {
            input.destructive
        }
    }

    fn mock_ctx<'a>(
        permission_mode: PermissionMode,
        decider: Option<
            &'a dyn Fn(&MockTool, &MockInput) -> Result<Decision, PermissionDecisionError>,
        >,
    ) -> GateContext<'a, MockTool, MockInput> {
        GateContext {
            permission_mode,
            permission_decider: decider,
        }
    }

    #[test]
    fn decide_allows_read_only_non_destructive_invocation_in_ask_mode() {
        let tool = MockTool;
        let decision = decide_ordinary(&tool, &MockInput::default(), &mock_ctx(PermissionMode::Ask, None));

        assert_eq!(decision.action, Action::Allow);
        assert_eq!(decision.reason, "read-only invocation allowed");
    }

    #[test]
    fn decide_allows_read_only_non_destructive_invocation_in_auto_mode() {
        let tool = MockTool;
        let decision = decide_ordinary(&tool, &MockInput::default(), &mock_ctx(PermissionMode::Auto, None));

        assert_eq!(decision.action, Action::Allow);
    }

    #[test]
    fn decide_denies_non_read_only_invocation() {
        let tool = MockTool;
        let decision = decide_ordinary(
            &tool,
            &MockInput {
                read_only: false,
                destructive: false,
            },
            &mock_ctx(PermissionMode::Ask, None),
        );

        assert_eq!(decision.action, Action::Deny);
        assert_eq!(decision.reason, "tool invocation is not read-only");
    }

    #[test]
    fn decide_denies_irreversible_invocation() {
        let tool = MockTool;
        let decision = decide_ordinary(
            &tool,
            &MockInput {
                read_only: true,
                destructive: true,
            },
            &mock_ctx(PermissionMode::Auto, None),
        );

        assert_eq!(decision.action, Action::Deny);
        assert_eq!(decision.reason, "tool invocation is irreversible");
    }

    #[test]
    fn decide_allows_irreversible_invocation_in_yolo_mode_without_consulting_a_decider() {
        let tool = MockTool;
        fn must_not_be_called(
            _: &MockTool,
            _: &MockInput,
        ) -> Result<Decision, PermissionDecisionError> {
            panic!("decider must not be called in yolo mode");
        }
        let decision = decide_ordinary(
            &tool,
            &MockInput {
                read_only: false,
                destructive: true,
            },
            &mock_ctx(PermissionMode::Yolo, Some(&must_not_be_called)),
        );

        assert_eq!(decision.action, Action::Allow);
        assert_eq!(decision.reason, "allowed by yolo mode");
    }

    #[test]
    fn fallible_permission_gate_preserves_operational_errors() {
        let tool = MockTool;
        fn failing(
            _: &MockTool,
            _: &MockInput,
        ) -> Result<Decision, PermissionDecisionError> {
            Err(PermissionDecisionError::Canceled)
        }
        let errors = [
            PermissionDecisionError::Canceled,
            PermissionDecisionError::Unexpected,
            PermissionDecisionError::SystemResources,
            PermissionDecisionError::ProcessFdQuotaExceeded,
            PermissionDecisionError::SystemFdQuotaExceeded,
        ];
        for err in errors {
            fn make(err: PermissionDecisionError) -> impl Fn(&MockTool, &MockInput) -> Result<Decision, PermissionDecisionError> {
                move |_: &MockTool, _: &MockInput| Err(err)
            }
            let decider = make(err);
            let ctx = GateContext {
                permission_mode: PermissionMode::Ask,
                permission_decider: Some(&decider),
            };
            assert!(matches!(decide(&tool, &MockInput::default(), &ctx), Err(e) if e == err));
        }
        let _ = failing;
    }
}
