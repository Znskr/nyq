//! Shared command runner configuration.
//!
//! Ported from `zig-reference/src/core/execution/command_runner.zig`
//! (Config slice; the runner itself is a later wave).

use crate::core::shared::types::ToolLifecycleId;
use crate::core::tooling::command_output_content::{Callback, Stream};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum CallbackProjection {
    #[default]
    ModelSafe,
    Raw,
}

/// Boxable callback shape matching `command_output_content.Callback`.
pub type OutputChunkCallback = Box<
    dyn FnMut(Option<&ToolLifecycleId>, Stream, &[u8]) -> Result<(), std::io::Error>,
>;

/// Subset of the runner config consumed by direct execution. Artifact and
/// background-provider fields arrive with the full runner port.
pub struct Config {
    pub max_command_output_bytes: usize,
    pub cancel_flag: Option<std::sync::atomic::AtomicBool>,
    pub output_chunk_lifecycle_id: Option<ToolLifecycleId>,
    pub on_output_chunk: Option<OutputChunkCallback>,
    pub on_accepted_output_chunk: Option<OutputChunkCallback>,
    pub callback_projection: CallbackProjection,
    pub timeout_ms: Option<usize>,
    pub timeout_started_ms: Option<i64>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_command_output_bytes: 0,
            cancel_flag: None,
            output_chunk_lifecycle_id: None,
            on_output_chunk: None,
            on_accepted_output_chunk: None,
            callback_projection: CallbackProjection::ModelSafe,
            timeout_ms: None,
            timeout_started_ms: None,
        }
    }
}
