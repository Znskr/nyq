//! Notification provider contract
//! (ported 1:1 from zig-reference/src/core/notifications/notification_contract.zig).

use std::ffi::c_void;

use crate::core::hooks::definitions::{AttentionKind, RegistrationError};

/// Errors surfaced by `Provider::configure`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigureError {
    OutOfMemory,
    Registration(RegistrationError),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Preferences {
    pub turn_end: bool,
    pub attention_required: bool,
    // Max retains the ordinary sound gates and enables additional direct cues.
    pub max: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cue {
    Success,
    Error,
    Bloom,
    Press,
    Click,
    Release,
    Toggle,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    TurnEnd,
    AttentionRequired,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Notification {
    pub kind: Kind,
    pub cue: Cue,
}

pub struct Provider {
    pub configure: fn(*mut c_void) -> Result<(), ConfigureError>,
    pub set_preferences: fn(*mut c_void, Preferences),
    pub preferences: fn(*const c_void) -> Preferences,
    pub max_enabled: fn(*const c_void) -> bool,
    pub queue: fn(*mut c_void, Notification),
    pub presentation_finished: fn(*mut c_void),
    pub flush: fn(*mut c_void),
    pub play_cue: fn(*mut c_void, Cue),
    pub play_max_cue: fn(*mut c_void, Cue),
    pub dispatch_attention_required: fn(*mut c_void, u64, AttentionKind),
}
