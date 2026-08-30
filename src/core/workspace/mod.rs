//! Workspace subsystem (ported 1:1 from zig-reference/src/core/workspace).

pub mod glob_pattern;
pub mod ignored_dirs;
pub mod read_tracker;
pub mod unicode_simple_fold;
pub mod change_tracker;
pub mod current_branch;
pub mod network_metrics;
pub mod statusline_identity;
pub mod workspace_files;
