//! Hook contract definitions (ported 1:1 from zig-reference/src/core/hooks/definitions.zig).

use crate::core::shared::types;

/// Handler context token. Zig represents this as `*anyopaque`; the Rust port
/// keeps an opaque address-sized token (`usize`) so callers never need `unsafe`
/// to round-trip it through the runtime. Callers may store an index or a
/// pointer-sized handle and recover it with a safe conversion of their choice.
pub type HandlerContext = usize;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HookKind {
    PreToolUse,
    Stop,
    PostTurnEnd,
    AttentionRequired,
}

impl HookKind {
    pub fn definition(self) -> HookDefinition {
        match self {
            Self::PreToolUse => pre_tool_use,
            Self::Stop => stop,
            Self::PostTurnEnd => post_turn_end,
            Self::AttentionRequired => attention_required,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HookDefinition {
    pub kind: HookKind,
    pub lifecycle_event: &'static str,
    pub agent_loop_point: &'static str,
    pub purpose: &'static str,
}

#[allow(non_upper_case_globals)]
pub const pre_tool_use: HookDefinition = HookDefinition {
    kind: HookKind::PreToolUse,
    lifecycle_event: "PreToolUse",
    agent_loop_point: "before local tool validation, permissions, and execution",
    purpose: "lets handlers keep a call unchanged, rewrite its arguments, or block it",
};

#[allow(non_upper_case_globals)]
pub const stop: HookDefinition = HookDefinition {
    kind: HookKind::Stop,
    lifecycle_event: "Stop",
    agent_loop_point: "after a terminal assistant candidate before turn finalization",
    purpose: "lets handlers allow completion or request one synthetic continuation",
};

#[allow(non_upper_case_globals)]
pub const post_turn_end: HookDefinition = HookDefinition {
    kind: HookKind::PostTurnEnd,
    lifecycle_event: "PostTurnEnd",
    agent_loop_point: "after a terminal turn outcome is accepted",
    purpose: "lets handlers observe terminal turns and run side effects without changing the turn outcome",
};

#[allow(non_upper_case_globals)]
pub const attention_required: HookDefinition = HookDefinition {
    kind: HookKind::AttentionRequired,
    lifecycle_event: "AttentionRequired",
    agent_loop_point: "after a user decision prompt becomes active",
    purpose: "lets handlers observe when a foreground turn is waiting for user attention",
};

#[allow(non_upper_case_globals)]
pub const all_hooks: [HookDefinition; 4] = [
    pre_tool_use,
    stop,
    post_turn_end,
    attention_required,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScopeKind {
    Interactive,
    Ask,
    Acp,
    Subagent,
}

impl ScopeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Interactive => "interactive",
            Self::Ask => "ask",
            Self::Acp => "acp",
            Self::Subagent => "subagent",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Scope {
    pub kind: ScopeKind,
    pub workspace_root: String,
    pub session_id: Option<String>,
    pub subagent_id: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invocation {
    pub scope: Scope,
    pub turn_id: Option<u64>,
}

pub struct Limits;

impl Limits {
    pub const fn handler_name_bytes() -> usize {
        128
    }
    pub const fn reason_bytes() -> usize {
        4 * 1024
    }
    pub const fn context_bytes() -> usize {
        64 * 1024
    }
    pub const fn arguments_json_bytes() -> usize {
        1024 * 1024
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegistrationError {
    EmptyHandlerName,
    HandlerNameTooLong,
    InvalidHandlerName,
    DuplicateHandlerName,
    RuntimeFrozen,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ViewError {
    RuntimeNotFrozen,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HandlerError {
    Failed,
    Cancelled,
}

impl HandlerError {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Failed => "Failed",
            Self::Cancelled => "Cancelled",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MutationDispatchError {
    OutOfMemory,
    HandlerFailed,
    Cancelled,
    InvalidHandlerOutput,
    HandlerOutputTooLarge,
}

impl MutationDispatchError {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OutOfMemory => "OutOfMemory",
            Self::HandlerFailed => "HandlerFailed",
            Self::Cancelled => "Cancelled",
            Self::InvalidHandlerOutput => "InvalidHandlerOutput",
            Self::HandlerOutputTooLarge => "HandlerOutputTooLarge",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreToolUseInput<'a> {
    pub invocation: Invocation,
    pub step_index: usize,
    pub call_id: &'a str,
    pub tool_name: &'a str,
    pub arguments_json: &'a str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreToolUseAction<'a> {
    Continue,
    RewriteArguments(&'a str),
    Block(&'a str),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PreToolUseOutcome {
    Unchanged,
    Rewritten(String),
    Blocked(String),
}

impl PreToolUseOutcome {
    pub fn deinit(&mut self) {
        *self = PreToolUseOutcome::Unchanged;
    }
}

pub type PreToolUseRunFn =
    for<'a> fn(HandlerContext, PreToolUseInput<'a>) -> Result<PreToolUseAction<'a>, HandlerError>;

pub struct PreToolUseHandler<'n> {
    pub name: &'n str,
    pub ctx: HandlerContext,
    pub run: PreToolUseRunFn,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StopInput<'a> {
    pub invocation: Invocation,
    pub step_index: usize,
    pub assistant_text: &'a str,
    pub provider_disposition: types::ProviderCompletionDisposition,
    pub can_continue: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopAction<'a> {
    Allow,
    ContinueOnce(&'a str),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StopOutcome {
    Allow,
    ContinueOnce(String),
}

impl StopOutcome {
    pub fn deinit(&mut self) {
        *self = StopOutcome::Allow;
    }
}

pub type StopRunFn =
    for<'a> fn(HandlerContext, StopInput<'a>) -> Result<StopAction<'a>, HandlerError>;

pub struct StopHandler<'n> {
    pub name: &'n str,
    pub ctx: HandlerContext,
    pub run: StopRunFn,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PostTurnEndInput {
    pub invocation: Invocation,
    pub outcome: types::TurnPresentationOutcome,
    pub provider_disposition: Option<types::ProviderCompletionDisposition>,
}

pub type PostTurnEndRunFn = fn(HandlerContext, PostTurnEndInput) -> Result<(), HandlerError>;

pub struct PostTurnEndHandler<'n> {
    pub name: &'n str,
    pub ctx: HandlerContext,
    pub run: PostTurnEndRunFn,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttentionKind {
    Permission,
    Question,
    RouteRecovery,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttentionRequiredInput {
    pub invocation: Invocation,
    pub kind: AttentionKind,
}

pub type AttentionRequiredRunFn = fn(HandlerContext, AttentionRequiredInput) -> Result<(), HandlerError>;

pub struct AttentionRequiredHandler<'n> {
    pub name: &'n str,
    pub ctx: HandlerContext,
    pub run: AttentionRequiredRunFn,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hook_definitions_enumerate_current_hook_surfaces() {
        assert_eq!(all_hooks.len(), 4);
        assert_eq!(all_hooks[0].kind, HookKind::PreToolUse);
        assert_eq!(all_hooks[1].kind, HookKind::Stop);
        assert_eq!(all_hooks[2].kind, HookKind::PostTurnEnd);
        assert_eq!(all_hooks[3].kind, HookKind::AttentionRequired);
        assert_eq!(pre_tool_use.lifecycle_event, "PreToolUse");
        assert_eq!(stop.lifecycle_event, "Stop");
        assert_eq!(post_turn_end.lifecycle_event, "PostTurnEnd");
        assert_eq!(attention_required.lifecycle_event, "AttentionRequired");
        assert_eq!(HookKind::PreToolUse.definition().kind, HookKind::PreToolUse);
        assert_eq!(HookKind::Stop.definition().kind, HookKind::Stop);
        assert_eq!(HookKind::PostTurnEnd.definition().kind, HookKind::PostTurnEnd);
        assert_eq!(
            HookKind::AttentionRequired.definition().kind,
            HookKind::AttentionRequired
        );
        assert_eq!(
            HookKind::PreToolUse.definition().lifecycle_event,
            "PreToolUse"
        );
        assert_eq!(HookKind::Stop.definition().lifecycle_event, "Stop");
        assert_eq!(
            HookKind::PostTurnEnd.definition().lifecycle_event,
            "PostTurnEnd"
        );
        assert_eq!(
            HookKind::AttentionRequired.definition().lifecycle_event,
            "AttentionRequired"
        );
        assert!(!pre_tool_use.agent_loop_point.is_empty());
        assert!(!pre_tool_use.purpose.is_empty());
        assert!(!stop.agent_loop_point.is_empty());
        assert!(!stop.purpose.is_empty());
        assert!(!post_turn_end.agent_loop_point.is_empty());
        assert!(!post_turn_end.purpose.is_empty());
        assert!(!attention_required.agent_loop_point.is_empty());
        assert!(!attention_required.purpose.is_empty());
    }

    #[test]
    fn hooks_v1_scope_kinds_remain_limited_to_the_current_surfaces() {
        let kinds = [
            ScopeKind::Interactive,
            ScopeKind::Ask,
            ScopeKind::Acp,
            ScopeKind::Subagent,
        ];
        assert_eq!(kinds.len(), 4);
    }
}
