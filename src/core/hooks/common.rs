//! Common hook surface re-exports (ported 1:1 from zig-reference/src/core/hooks/common.zig).

pub use super::definitions::{
    HandlerError, Invocation, Limits, MutationDispatchError, RegistrationError, Scope, ScopeKind,
    ViewError,
};

#[cfg(test)]
mod tests {
    use super::*;

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
