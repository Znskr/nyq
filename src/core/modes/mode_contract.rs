//! Mode contract (ported 1:1 from zig-reference/src/core/modes/mode_contract.zig).

use crate::core::shared::types::PermissionMode;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolPolicy {
    Full,
    ReadOnly,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModeSpec {
    pub id: String,
    pub name: String,
    pub description: String,
    pub permission_mode: PermissionMode,
    pub tool_policy: ToolPolicy,
    pub tool_policy_denial_message: Option<String>,
}

impl Default for ModeSpec {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            description: String::new(),
            permission_mode: PermissionMode::Ask,
            tool_policy: ToolPolicy::Full,
            tool_policy_denial_message: None,
        }
    }
}
