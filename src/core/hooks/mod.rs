//! Hooks subsystem facade (ported 1:1 from zig-reference/src/core/hooks/hooks.zig,
//! which re-exports the package's public API from its sibling modules).

pub mod common;
pub mod definitions;
pub mod prompt;
pub mod runtime;
pub mod tool;

pub use definitions::{
    all_hooks, attention_required, post_turn_end, pre_tool_use, stop, AttentionKind,
    AttentionRequiredHandler, AttentionRequiredInput, HandlerError, HookDefinition, HookKind,
    Invocation, Limits, MutationDispatchError, PostTurnEndHandler, PostTurnEndInput,
    PreToolUseAction, PreToolUseHandler, PreToolUseInput, PreToolUseOutcome,
    RegistrationError, Scope, ScopeKind, StopAction, StopHandler, StopInput, StopOutcome,
    ViewError,
};
pub use runtime::{Runtime, RuntimeView};
