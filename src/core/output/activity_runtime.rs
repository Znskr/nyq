//! Tool activity lifecycle runtime (ported 1:1 from
//! zig-reference/src/core/output/activity_runtime.zig).

use crate::core::shared::types;
use std::{collections::HashMap, ffi::c_void};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolLifecyclePhase {
    Provisional,
    Authoritative,
    Terminal,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolPresentationRecord {
    pub id: types::ToolLifecycleId,
    pub entry_id: u32,
    pub tool_name: Option<Vec<u8>>,
    pub activity_kind: types::ToolActivityKind,
    pub captured_command: bool,
    pub phase: ToolLifecyclePhase,
    pub focus_seq: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Tone {
    Thinking,
    Neutral,
    Warning,
    Success,
    Danger,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TurnThinking {
    pub label: Vec<u8>,
    pub tone: Tone,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolSlot {
    pub entry_id: u32,
    pub fallback_label: Vec<u8>,
    pub active: bool,
    pub kind: types::ToolActivityKind,
    pub thinking_label: Option<Vec<u8>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ActivityProjection {
    None,
    TurnThinking(TurnThinking),
    ToolSlot(ToolSlot),
}

/// Immutable view of lifecycle state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LifecycleSnapshot {
    pub finalized_turn_watermark: u64,
    pub active_tool_count: usize,
    pub focused_entry_id: Option<u32>,
    pub focused_activity_kind: Option<types::ToolActivityKind>,
    pub activity: ActivityProjection,
}

/// Result of one lifecycle mutation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LifecycleTransition {
    pub previous_focused_entry_id: Option<u32>,
    pub snapshot: LifecycleSnapshot,
    pub applied_activity_kind: Option<types::ToolActivityKind>,
    pub terminal_record: Option<ToolPresentationRecord>,
}

impl LifecycleTransition {
    pub fn focus_changed(&self) -> bool {
        self.previous_focused_entry_id != self.snapshot.focused_entry_id
    }
}

pub type LifecycleError = Box<dyn std::error::Error>;

#[derive(Clone, Copy)]
pub struct LifecyclePresenter {
    pub ctx: *mut c_void,
    pub snapshot_fn: fn(*mut c_void) -> LifecycleSnapshot,
    pub record_fn: fn(
        *mut c_void,
        types::ToolLifecycleId,
    ) -> Option<ToolPresentationRecord>,
    pub apply_fn: fn(
        *mut c_void,
        types::ToolLifecycleEvent,
    ) -> Result<LifecycleTransition, LifecycleError>,
    pub finish_batch_fn: fn(*mut c_void) -> Result<(), LifecycleError>,
}

impl LifecyclePresenter {
    pub fn snapshot(self) -> LifecycleSnapshot {
        (self.snapshot_fn)(self.ctx)
    }

    pub fn record(
        self,
        id: types::ToolLifecycleId,
    ) -> Option<ToolPresentationRecord> {
        (self.record_fn)(self.ctx, id)
    }

    pub fn apply(
        self,
        event: types::ToolLifecycleEvent,
    ) -> Result<LifecycleTransition, LifecycleError> {
        (self.apply_fn)(self.ctx, event)
    }

    pub fn finish_batch(self) -> Result<(), LifecycleError> {
        (self.finish_batch_fn)(self.ctx)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FallbackRecord {
    pub record_id: types::ToolLifecycleId,
    pub was_provisional: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StartAdmission {
    Accepted,
    InvalidIdentity,
    Finalized(u64),
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
struct RecordKey {
    turn_id: u64,
    call_id: Vec<u8>,
}

impl RecordKey {
    fn from_id(id: &types::ToolLifecycleId) -> Self {
        Self {
            turn_id: id.turn_id,
            call_id: id.call_id.clone(),
        }
    }
}

#[derive(Default)]
pub struct ToolActivityState {
    records: HashMap<RecordKey, ToolPresentationRecord>,
    batch_finalized_turn_watermark: Option<u64>,
    finalized_turn_watermark: u64,
    next_focus_seq: u64,
}

impl ToolActivityState {
    pub fn deinit(&mut self) {
        self.records.clear();
        self.batch_finalized_turn_watermark = None;
        self.finalized_turn_watermark = 0;
        self.next_focus_seq = 0;
    }

    pub fn ensure_record_capacity(&mut self) {
        self.records.reserve(1);
    }

    pub fn record(
        &self,
        id: &types::ToolLifecycleId,
    ) -> Option<ToolPresentationRecord> {
        self.records.get(&RecordKey::from_id(id)).cloned()
    }

    pub fn record_ptr(
        &mut self,
        id: &types::ToolLifecycleId,
    ) -> Option<&mut ToolPresentationRecord> {
        self.records.get_mut(&RecordKey::from_id(id))
    }

    pub fn record_count(&self) -> usize {
        self.records.len()
    }

    pub fn active_count(&self) -> usize {
        self.records
            .values()
            .filter(|record| record.phase != ToolLifecyclePhase::Terminal)
            .count()
    }

    pub fn focused_record(&self) -> Option<&ToolPresentationRecord> {
        let mut focused: Option<&ToolPresentationRecord> = None;
        for record in self.records.values() {
            if record.phase == ToolLifecyclePhase::Terminal {
                continue;
            }
            if focused.is_none() || record.focus_seq > focused.unwrap().focus_seq {
                focused = Some(record);
            }
        }
        focused
    }

    pub fn admit_start(&self, id: &types::ToolLifecycleId) -> StartAdmission {
        if id.turn_id == 0 || id.call_id.is_empty() {
            return StartAdmission::InvalidIdentity;
        }
        let fence = self
            .finalized_turn_watermark
            .max(self.batch_finalized_turn_watermark.unwrap_or(0));
        if id.turn_id <= fence {
            return StartAdmission::Finalized(fence);
        }
        StartAdmission::Accepted
    }

    pub fn is_fenced(&self, turn_id: u64) -> bool {
        turn_id
            <= self
                .finalized_turn_watermark
                .max(self.batch_finalized_turn_watermark.unwrap_or(0))
    }

    pub fn put_owned_record(
        &mut self,
        id: types::ToolLifecycleId,
        tool_name: Option<Vec<u8>>,
        activity_kind: types::ToolActivityKind,
        entry_id: u32,
        phase: ToolLifecyclePhase,
    ) {
        let focus_seq = self.claim_focus();
        let key = RecordKey::from_id(&id);
        self.records.insert(
            key,
            ToolPresentationRecord {
                id,
                entry_id,
                tool_name,
                activity_kind,
                captured_command: false,
                phase,
                focus_seq,
            },
        );
    }

    pub fn promote_owned(
        &mut self,
        id: &types::ToolLifecycleId,
        tool_name: Vec<u8>,
        activity_kind: types::ToolActivityKind,
    ) -> bool {
        let key = RecordKey::from_id(id);
        let first_authoritative = self
            .records
            .get(&key)
            .expect("promote_owned requires an existing record")
            .phase
            != ToolLifecyclePhase::Authoritative;
        let focus_seq = first_authoritative.then(|| self.claim_focus());
        let record = self
            .records
            .get_mut(&key)
            .expect("promote_owned requires an existing record");
        record.tool_name = Some(tool_name);
        record.activity_kind = activity_kind;
        record.phase = ToolLifecyclePhase::Authoritative;
        if let Some(seq) = focus_seq {
            record.focus_seq = seq;
        }
        first_authoritative
    }

    pub fn rekey_owned(
        &mut self,
        old_id: &types::ToolLifecycleId,
        new_id: types::ToolLifecycleId,
        tool_name: Vec<u8>,
        activity_kind: types::ToolActivityKind,
    ) {
        let mut record = self
            .records
            .remove(&RecordKey::from_id(old_id))
            .expect("rekey_owned requires an existing record");
        record.id = new_id;
        record.tool_name = Some(tool_name);
        record.activity_kind = activity_kind;
        record.phase = ToolLifecyclePhase::Authoritative;
        record.focus_seq = self.claim_focus();
        let key = RecordKey::from_id(&record.id);
        self.records.insert(key, record);
    }

    pub fn should_apply_turn_finished(&self, turn_id: u64) -> bool {
        if turn_id == 0 {
            return false;
        }
        if turn_id <= self.finalized_turn_watermark {
            return false;
        }
        if let Some(watermark) = self.batch_finalized_turn_watermark {
            if turn_id <= watermark {
                return false;
            }
        }
        true
    }

    pub fn collect_fallback_records(&self, turn_id: u64) -> Vec<FallbackRecord> {
        self.records
            .values()
            .filter(|record| {
                record.id.turn_id == turn_id
                    && record.phase != ToolLifecyclePhase::Terminal
            })
            .map(|record| FallbackRecord {
                record_id: record.id.clone(),
                was_provisional: record.phase == ToolLifecyclePhase::Provisional,
            })
            .collect()
    }

    pub fn commit_turn_finished(
        &mut self,
        turn_id: u64,
        fallbacks: &[FallbackRecord],
    ) {
        for fallback in fallbacks {
            if let Some(record) = self
                .records
                .get_mut(&RecordKey::from_id(&fallback.record_id))
            {
                record.phase = ToolLifecyclePhase::Terminal;
            }
        }
        self.batch_finalized_turn_watermark = Some(
            self.batch_finalized_turn_watermark
                .unwrap_or(0)
                .max(turn_id),
        );
    }

    pub fn cleanup_entry_ids(&self, watermark: u64) -> Vec<u32> {
        self.records
            .values()
            .filter(|record| record.id.turn_id <= watermark)
            .map(|record| record.entry_id)
            .collect()
    }

    pub fn paused_turn_entry_ids(&self, turn_id: u64) -> Vec<u32> {
        self.records
            .values()
            .filter(|record| record.id.turn_id == turn_id)
            .map(|record| record.entry_id)
            .collect()
    }

    pub fn release_paused_turn(&mut self, turn_id: u64) {
        let keys: Vec<RecordKey> = self
            .records
            .iter()
            .filter(|(_, record)| record.id.turn_id == turn_id)
            .map(|(key, _)| key.clone())
            .collect();
        for key in keys {
            self.records.remove(&key);
        }
    }

    pub fn commit_cleanup(&mut self, watermark: u64) {
        let keys: Vec<RecordKey> = self
            .records
            .iter()
            .filter(|(_, record)| record.id.turn_id <= watermark)
            .map(|(key, _)| key.clone())
            .collect();
        for key in keys {
            self.records.remove(&key);
        }
        self.finalized_turn_watermark = self.finalized_turn_watermark.max(watermark);
        self.batch_finalized_turn_watermark = None;
    }

    fn claim_focus(&mut self) -> u64 {
        self.next_focus_seq = self.next_focus_seq.wrapping_add(1);
        if self.next_focus_seq == 0 {
            self.next_focus_seq = 1;
        }
        self.next_focus_seq
    }
}

pub fn replace_tool_name(
    record: &mut ToolPresentationRecord,
    owned_tool_name: Vec<u8>,
) {
    record.tool_name = Some(owned_tool_name);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(turn_id: u64, call_id: &[u8]) -> types::ToolLifecycleId {
        types::ToolLifecycleId {
            turn_id,
            call_id: call_id.to_vec(),
        }
    }

    #[test]
    fn tool_activity_start_admission_describes_invalid_and_finalized_identities() {
        let mut state = ToolActivityState::default();

        assert_eq!(
            state.admit_start(&id(0, b"call")),
            StartAdmission::InvalidIdentity
        );
        assert_eq!(
            state.admit_start(&id(1, b"")),
            StartAdmission::InvalidIdentity
        );
        assert_eq!(
            state.admit_start(&id(1, b"call")),
            StartAdmission::Accepted
        );

        state.commit_cleanup(1);
        assert_eq!(
            state.admit_start(&id(1, b"call")),
            StartAdmission::Finalized(1)
        );
    }

    #[test]
    fn lifecycle_transition_reports_focus_changes_from_immutable_snapshots() {
        let unchanged = LifecycleTransition {
            previous_focused_entry_id: Some(4),
            snapshot: LifecycleSnapshot {
                finalized_turn_watermark: 1,
                active_tool_count: 1,
                focused_entry_id: Some(4),
                focused_activity_kind: Some(types::ToolActivityKind::Read),
                activity: ActivityProjection::None,
            },
            applied_activity_kind: Some(types::ToolActivityKind::Read),
            terminal_record: None,
        };
        assert!(!unchanged.focus_changed());

        let mut changed = unchanged;
        changed.snapshot.focused_entry_id = Some(7);
        assert!(changed.focus_changed());
    }
}
