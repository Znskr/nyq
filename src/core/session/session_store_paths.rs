//! Shared paths and validation for the durable session store.
//!
//! Mirrors `zig-reference/src/core/session/session_store_paths.zig`.

use super::session_layout;

/// Cache file holding the per-workspace "latest session" summary list.
pub const SESSION_LIST_JSON_FILE: &str = "list.json";

/// Cache file holding the aggregate session-count / latest-id summary.
pub const SESSION_SUMMARY_FILE: &str = "summary.json";

/// Maximum length accepted for a durable workspace root.
const MAX_PATH_BYTES: usize = 4096;

/// Error returned when a workspace root is empty, too long, or not absolute.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidDurableField;

/// Validates a session id against the shared layout rules.
pub fn validate_session_id(session_id: &[u8]) -> Result<(), session_layout::InvalidSessionId> {
    session_layout::validate_session_id(session_id)
}

/// Builds the absolute directory path for a session.
pub fn session_dir_path(
    sessions_dir: &str,
    session_id: &[u8],
) -> Result<String, session_layout::InvalidSessionId> {
    session_layout::session_dir_path(sessions_dir, session_id)
}

/// Generates a fresh, layout-valid session id.
pub fn generate_session_id() -> String {
    session_layout::generate_session_id()
}

/// Path to the aggregate summary cache under `sessions_dir`.
pub fn summary_path(sessions_dir: &str) -> String {
    format!("{}/{}", sessions_dir, SESSION_SUMMARY_FILE)
}

/// Path to a session's primary `session.json` under `sessions_dir`.
pub fn session_json_path(
    sessions_dir: &str,
    session_id: &[u8],
) -> Result<String, session_layout::InvalidSessionId> {
    let session_dir = session_dir_path(sessions_dir, session_id)?;
    Ok(format!("{}/session.json", session_dir))
}

/// Strips trailing slashes from a workspace root (keeping at least one char),
/// so equivalent roots compare equal.
pub fn normalize_workspace_root(workspace_root: &str) -> &str {
    let mut end = workspace_root.len();
    while end > 1 && workspace_root.as_bytes()[end - 1] == b'/' {
        end -= 1;
    }
    &workspace_root[..end]
}

/// Rejects a workspace root that is empty, too long, or not absolute.
/// (`&str` is always valid UTF-8 in Rust.)
pub fn validate_workspace_root(workspace_root: &str) -> Result<(), InvalidDurableField> {
    if workspace_root.is_empty()
        || workspace_root.len() > MAX_PATH_BYTES
        || !workspace_root.starts_with('/')
    {
        return Err(InvalidDurableField);
    }
    Ok(())
}
