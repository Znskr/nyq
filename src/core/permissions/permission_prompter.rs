//! Transport-neutral permission prompt capability. Policy evaluation and
//! grant construction stay in tool admission; transports present the request,
//! return the selected response, and may retain grants in session-owned state.
//!
//! Ported from `zig-reference/src/core/permissions/permission_prompter.zig`.

use crate::core::output::diff::FileReview;
use crate::core::shared::types::{PermissionGrant, ToolCall};
use crate::core::permissions::permission_request::{OwnedPermissionResponse, PermissionRequest};

/// Transport-neutral permission prompt capability.
///
/// The Zig source models this as a context pointer plus function pointers;
/// the Rust port uses a boxed closure carrying the same payload.
pub struct Prompter {
    request_fn: Box<dyn FnMut(
        PermissionRequest,
        &ToolCall,
        Option<&FileReview>,
        Option<&[PermissionGrant]>,
    ) -> Result<OwnedPermissionResponse, Box<dyn std::error::Error>>>,
    retain_grant_fn: Option<Box<dyn FnMut(&str, &str) -> Result<(), Box<dyn std::error::Error>>>>,
}

impl Prompter {
    pub fn new(
        request_fn: Box<dyn FnMut(
            PermissionRequest,
            &ToolCall,
            Option<&FileReview>,
            Option<&[PermissionGrant]>,
        ) -> Result<OwnedPermissionResponse, Box<dyn std::error::Error>>>,
    ) -> Self {
        Self {
            request_fn,
            retain_grant_fn: None,
        }
    }

    #[allow(clippy::type_complexity)]
    pub fn with_retain_grant(
        mut self,
        retain_grant_fn: Option<
            Box<dyn FnMut(&str, &str) -> Result<(), Box<dyn std::error::Error>>>,
        >,
    ) -> Self {
        self.retain_grant_fn = retain_grant_fn;
        self
    }

    pub fn request(
        &mut self,
        request_value: PermissionRequest,
        call: &ToolCall,
        review: Option<&FileReview>,
        grant_offer: Option<&[PermissionGrant]>,
    ) -> Result<OwnedPermissionResponse, Box<dyn std::error::Error>> {
        (self.request_fn)(request_value, call, review, grant_offer)
    }

    pub fn retain_grant(&mut self, tool_name: &str, target_path: &str) -> bool {
        let Some(retain) = self.retain_grant_fn.as_mut() else {
            return false;
        };
        retain(tool_name, target_path).is_ok()
    }
}
