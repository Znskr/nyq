//! Permission grant bookkeeping shared by approval flows.
//!
//! Ported from `zig-reference/src/core/permissions/permissions.zig` (partial,
//! growing as dependent modules are translated).

use crate::core::shared::types::{PermissionGrant, ToolPermissionDecision};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PermissionCallTarget {
    pub role: Vec<u8>,
    pub path: Vec<u8>,
}

pub fn permission_decision_from_index(index: u8) -> ToolPermissionDecision {
    match index {
        0 => ToolPermissionDecision::Once,
        1 => ToolPermissionDecision::Always,
        _ => ToolPermissionDecision::Deny,
    }
}

/// Drops every grant and leaves the list ready for reuse. The Zig source
/// frees each owned pair; the Rust port simply clears the vector.
pub fn clear_allowed_tools(grants: &mut Vec<PermissionGrant>) {
    grants.clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_decision_from_index_maps_approval_choices() {
        assert_eq!(permission_decision_from_index(0), ToolPermissionDecision::Once);
        assert_eq!(permission_decision_from_index(1), ToolPermissionDecision::Always);
        assert_eq!(permission_decision_from_index(2), ToolPermissionDecision::Deny);
        assert_eq!(permission_decision_from_index(3), ToolPermissionDecision::Deny);
    }

    #[test]
    fn clear_allowed_tools_empties_the_grant_list() {
        let mut grants = vec![PermissionGrant {
            tool_name: b"bash".to_vec(),
            target_path: b"npm*".to_vec(),
        }];
        clear_allowed_tools(&mut grants);
        assert!(grants.is_empty());
    }
}
