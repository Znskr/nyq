//! Active approval prompt state: request ownership, review binding, and the
//! immutable projection transports render from.
//!
//! Ported from `zig-reference/src/core/permissions/approval_prompt.zig`.
//! The Zig source keeps allocator-owned clones plus borrowed views; the Rust
//! port stores the owned request directly and projects borrows from it.

use crate::core::output::diff::FileChangePreview;
use crate::core::permissions::approval_decision::{Action, Event, State as DecisionState};
use crate::core::permissions::permission_request::{
    FileApprovalRequest, OwnedPermissionResponse, PermissionRequest, RequestError,
};
use crate::core::permissions::session_permission_state::{ConfirmedRuleEvent, OwnedConfirmedRuleEvent};
use crate::core::shared::types::ToolPermissionDecision;

/// Borrowed immutable view of the active prompt. Every slice remains valid
/// only until the next prompt mutation (enforced by the borrow checker).
pub struct Projection<'a> {
    pub request: &'a PermissionRequest,
    pub choice_index: u8,
    pub amendment_choice: Option<u8>,
    pub allow_draft: &'a [u8],
    pub allow_draft_cursor: usize,
    pub deny_draft: &'a [u8],
    pub deny_draft_cursor: usize,
    pub review: Option<ReviewId>,
}

impl<'a> Projection<'a> {
    pub fn can_amend_selected_choice(&self) -> bool {
        self.request.amendment_allowed && (self.choice_index == 0 || self.choice_index == 2)
    }

    pub fn amendment_choice(&self) -> Option<u8> {
        self.amendment_choice
    }

    pub fn draft_for_choice(&self, choice: u8) -> &'a [u8] {
        match choice {
            0 => self.allow_draft,
            2 => self.deny_draft,
            _ => b"",
        }
    }

    pub fn draft_cursor_for_choice(&self, choice: u8) -> usize {
        match choice {
            0 => self.allow_draft_cursor,
            2 => self.deny_draft_cursor,
            _ => 0,
        }
    }

    pub fn current_review(&self) -> Option<ReviewId> {
        self.review
    }
}

/// Reviews live in the caller's store; the prompt keeps a stable handle
/// instead of a pointer so review lifetimes stay caller-owned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReviewId(pub usize);

#[derive(Default)]
pub struct ApprovalPrompt {
    request: Option<PermissionRequest>,
    pub decision: DecisionState,
    review: Option<ReviewId>,
    review_request_id: u64,
    rule_management: Option<OwnedConfirmedRuleEvent>,
}

impl ApprovalPrompt {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_active(&self) -> bool {
        self.request.is_some()
    }

    /// Returns a borrowed immutable view of the active prompt.
    pub fn projection(&self) -> Option<Projection<'_>> {
        let request = self.request.as_ref()?;
        Some(Projection {
            request,
            choice_index: self.decision.choice_index,
            amendment_choice: self.decision.amendment_choice(),
            allow_draft: self.decision.draft_for_choice(0),
            allow_draft_cursor: self.decision.draft_cursor_for_choice(0),
            deny_draft: self.decision.draft_for_choice(2),
            deny_draft_cursor: self.decision.draft_cursor_for_choice(2),
            review: self.current_review_id(),
        })
    }

    pub fn clear(&mut self) {
        self.clear_with_reason("cleared");
    }

    pub fn clear_with_reason(&mut self, reason: &str) {
        self.rule_management = None;
        self.request = None;
        self.decision.reset(reason);
        self.clear_review();
    }

    pub fn sync_request(
        &mut self,
        request: Option<PermissionRequest>,
    ) -> Result<bool, RequestError> {
        if let Some(value) = request {
            if let Some(current) = &self.request {
                if current.eql(&value) {
                    return Ok(false);
                }
            }
            crate::core::permissions::permission_request::preflight_request(&value)?;
            let reason = if self.request.is_some() {
                "request_replaced"
            } else {
                "request_started"
            };
            self.clear_with_reason(reason);
            self.decision.set_confirmation_only(value.confirmation_only);
            self.request = Some(value);
            return Ok(true);
        }

        if self.request.is_none() {
            return Ok(false);
        }
        self.clear_with_reason("request_cleared");
        Ok(true)
    }

    pub fn sync_rule_management(
        &mut self,
        request: &PermissionRequest,
        event: &ConfirmedRuleEvent,
    ) -> Result<(), RequestError> {
        if !request.confirmation_only || request.file.is_some() {
            return Err(RequestError::RequestProjectionTooLarge);
        }
        crate::core::permissions::permission_request::preflight_request(request)?;
        let owned_event = OwnedConfirmedRuleEvent::dupe(event);
        self.clear_with_reason("rule_management_started");
        self.request = Some(request.clone());
        self.rule_management = Some(owned_event);
        self.decision.set_confirmation_only(true);
        Ok(())
    }

    pub fn rule_management(&self) -> Option<&OwnedConfirmedRuleEvent> {
        self.rule_management.as_ref()
    }

    pub fn sync_review(&mut self, review: Option<ReviewId>) -> bool {
        let Some(request) = &self.request else {
            let changed = self.review.is_some() || self.review_request_id != 0;
            self.clear_review();
            return changed;
        };
        if request.file.is_none() {
            let changed = self.review.is_some() || self.review_request_id != 0;
            self.clear_review();
            return changed;
        }

        if self.review == review && self.review_request_id == request.id {
            return false;
        }
        self.review = review;
        self.review_request_id = if review.is_none() { 0 } else { request.id };
        true
    }

    pub fn current_review_id(&self) -> Option<ReviewId> {
        let request = self.request.as_ref()?;
        if request.file.is_none() || self.review_request_id != request.id {
            return None;
        }
        self.review
    }

    pub fn is_amending(&self) -> bool {
        self.decision.is_amending()
    }

    pub fn can_amend_selected_choice(&self) -> bool {
        let Some(request) = &self.request else {
            return false;
        };
        self.decision.can_amend(request.amendment_allowed)
    }

    pub fn amendment_choice(&self) -> Option<u8> {
        self.decision.amendment_choice()
    }

    pub fn draft_for_choice(&self, choice: u8) -> &[u8] {
        self.decision.draft_for_choice(choice)
    }

    pub fn draft_cursor_for_choice(&self, choice: u8) -> usize {
        self.decision.draft_cursor_for_choice(choice)
    }

    pub fn apply(
        &mut self,
        action: Action,
        amendment_allowed: Option<bool>,
        max_len: Option<usize>,
    ) -> Event {
        self.decision.apply(action, amendment_allowed, max_len)
    }

    pub fn materialize_response(
        &self,
        decision: ToolPermissionDecision,
    ) -> OwnedPermissionResponse {
        self.decision.materialize_response(decision)
    }

    pub fn clear_after_accepted_submission(&mut self) {
        self.rule_management = None;
        self.request = None;
        self.decision.reset_after_submission();
        self.clear_review();
    }

    fn clear_review(&mut self) {
        self.review = None;
        self.review_request_id = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::output::diff::{PreviewLine, PreviewOp};

    fn approval_preview() -> FileChangePreview {
        FileChangePreview {
            path: b"note.txt".to_vec(),
            path_basename_start: 5,
            path_source_truncated: false,
            lines: vec![
                PreviewLine {
                    op: PreviewOp::Deletion,
                    old_line: Some(2),
                    new_line: None,
                    text: b"before".to_vec(),
                },
                PreviewLine {
                    op: PreviewOp::Addition,
                    old_line: None,
                    new_line: Some(2),
                    text: b"after".to_vec(),
                },
            ],
            additions: 1,
            deletions: 1,
            truncated: false,
        }
    }

    fn approval_file_request(preview: FileChangePreview) -> FileApprovalRequest {
        FileApprovalRequest {
            kind: crate::core::tooling::file_mutation_contract::Kind::Edit,
            intent: crate::core::tooling::file_mutation_contract::FileApprovalIntent::Mutation,
            preview,
            scope: crate::core::tooling::file_mutation_contract::FileApprovalScope::WorkspaceFiles,
        }
    }

    #[test]
    fn owns_replaces_and_clears_structured_requests() {
        let request = PermissionRequest {
            label: b"edit_file note.txt".to_vec(),
            file: Some(approval_file_request(approval_preview())),
            ..PermissionRequest::default()
        };

        let mut prompt = ApprovalPrompt::new();
        assert!(prompt.sync_request(Some(request.clone())).unwrap());
        let stored = prompt.request.as_ref().unwrap();
        assert_eq!(stored.label, request.label);
        assert!(stored
            .file
            .as_ref()
            .unwrap()
            .preview
            .eql(&request.file.as_ref().unwrap().preview));

        prompt.decision.choice_index = 2;
        assert!(!prompt.sync_request(Some(request.clone())).unwrap());
        assert_eq!(prompt.decision.choice_index, 2);

        assert!(prompt
            .sync_request(Some(PermissionRequest {
                label: b"terminal.exec npm test".to_vec(),
                ..PermissionRequest::default()
            }))
            .unwrap());
        assert_eq!(
            prompt.request.as_ref().unwrap().label,
            b"terminal.exec npm test"
        );
        assert!(prompt.request.as_ref().unwrap().file.is_none());
        assert_eq!(prompt.decision.choice_index, 0);

        prompt.decision.choice_index = 1;
        assert!(prompt.sync_request(None).unwrap());
        assert!(!prompt.is_active());
        assert_eq!(prompt.decision.choice_index, 0);
        assert!(!prompt.sync_request(None).unwrap());
    }

    #[test]
    fn treats_byte_identical_replacement_ids_as_new_requests() {
        let mut prompt = ApprovalPrompt::new();

        let request_a = PermissionRequest {
            id: 10,
            label: b"file_mutation".to_vec(),
            file: Some(approval_file_request(approval_preview())),
            ..PermissionRequest::default()
        };
        assert!(prompt.sync_request(Some(request_a.clone())).unwrap());
        prompt.decision.choice_index = 2;

        let mut request_b = request_a.clone();
        request_b.id = 11;
        assert!(prompt.sync_request(Some(request_b)).unwrap());
        assert_eq!(prompt.request.as_ref().unwrap().id, 11);
        assert_eq!(prompt.decision.choice_index, 0);
    }

    #[test]
    fn keeps_full_review_request_bound() {
        let mut prompt = ApprovalPrompt::new();

        let request_a = PermissionRequest {
            id: 10,
            label: b"file_mutation".to_vec(),
            file: Some(approval_file_request(approval_preview())),
            ..PermissionRequest::default()
        };
        assert!(prompt.sync_request(Some(request_a.clone())).unwrap());
        assert!(prompt.sync_review(Some(ReviewId(3))));
        assert_eq!(prompt.current_review_id(), Some(ReviewId(3)));

        let mut request_b = request_a;
        request_b.id = 11;
        assert!(prompt.sync_request(Some(request_b)).unwrap());
        assert_eq!(prompt.current_review_id(), None);
    }

    #[test]
    fn projection_borrows_immutable_active_state() {
        let mut prompt = ApprovalPrompt::new();
        assert!(prompt.projection().is_none());
        assert!(prompt
            .sync_request(Some(PermissionRequest {
                id: 41,
                label: b"edit_file note.txt".to_vec(),
                file: Some(approval_file_request(approval_preview())),
                ..PermissionRequest::default()
            }))
            .unwrap());
        assert!(prompt.sync_review(Some(ReviewId(5))));
        prompt.apply(Action::Tab, Some(true), None);
        prompt.apply(Action::InsertAscii(b'y'), Some(true), None);
        prompt.apply(Action::MoveChoice(crate::core::permissions::approval_decision::Move::Previous), Some(true), None);
        prompt.apply(Action::Tab, Some(true), None);
        prompt.apply(Action::InsertAscii(b'n'), Some(true), None);

        let projection = prompt.projection().unwrap();
        assert_eq!(projection.request.id, 41);
        assert_eq!(projection.request.label, b"edit_file note.txt");
        assert_eq!(projection.choice_index, 2);
        assert_eq!(projection.amendment_choice(), Some(2));
        assert!(projection.can_amend_selected_choice());
        assert_eq!(projection.draft_for_choice(0), b"y");
        assert_eq!(projection.draft_for_choice(2), b"n");
        assert_eq!(projection.draft_cursor_for_choice(0), 1);
        assert_eq!(projection.draft_cursor_for_choice(2), 1);
        assert_eq!(projection.current_review(), Some(ReviewId(5)));
        assert_eq!(prompt.current_review_id(), Some(ReviewId(5)));
    }

    #[test]
    fn command_approval_ignores_absent_file_review() {
        let mut prompt = ApprovalPrompt::new();
        assert!(prompt
            .sync_request(Some(PermissionRequest {
                id: 21,
                label: b"terminal.exec printf '%s' command-review".to_vec(),
                ..PermissionRequest::default()
            }))
            .unwrap());

        assert!(!prompt.sync_review(None));
        assert_eq!(prompt.current_review_id(), None);
        assert_eq!(prompt.review_request_id, 0);
    }

    #[test]
    fn rule_management_requires_confirmation_only_without_file() {
        let mut prompt = ApprovalPrompt::new();
        let file_request = PermissionRequest {
            label: b"edit_file note.txt".to_vec(),
            file: Some(approval_file_request(approval_preview())),
            confirmation_only: true,
            ..PermissionRequest::default()
        };
        let event = ConfirmedRuleEvent::Set(crate::core::permissions::session_permission_state::SetEvent {
            key: crate::core::permissions::session_permission_state::RuleKey {
                kind: crate::core::permissions::session_permission_state::RuleKeyKind::FileMutation,
                digest: [0u8; 32],
                canonical: b"edit".to_vec(),
            },
            decision: crate::core::permissions::session_permission_state::Decision::Allow,
            display_identity: b"edit".to_vec(),
            expected_generation: None,
        });

        assert!(prompt.sync_rule_management(&file_request, &event).is_err());

        let plain_request = PermissionRequest {
            label: b"terminal.exec npm test".to_vec(),
            ..PermissionRequest::default()
        };
        assert!(prompt.sync_rule_management(&plain_request, &event).is_err());

        let confirmation_request = PermissionRequest {
            label: b"terminal.exec npm test".to_vec(),
            confirmation_only: true,
            ..PermissionRequest::default()
        };
        assert!(prompt.sync_rule_management(&confirmation_request, &event).is_ok());
        assert!(prompt.rule_management().is_some());
        assert!(prompt.decision.confirmation_only);
    }

    #[test]
    fn clear_after_accepted_submission_keeps_selected_draft_semantics() {
        let mut prompt = ApprovalPrompt::new();
        prompt
            .sync_request(Some(PermissionRequest {
                label: b"terminal.exec npm test".to_vec(),
                ..PermissionRequest::default()
            }))
            .unwrap();
        prompt.apply(Action::Tab, Some(true), None);
        prompt.apply(Action::InsertAscii(b'y'), Some(true), None);

        prompt.clear_after_accepted_submission();
        assert!(!prompt.is_active());
        assert!(!prompt.is_amending());
        assert_eq!(prompt.draft_for_choice(0), b"");
    }
}
