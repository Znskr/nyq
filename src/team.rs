//! Team task graph (design §8.2): `TaskNode`s with compare-and-set on `rev`,
//! stored in `team.json` next to `orchestration.jsonl`, guarded by a file lock.
//!
//! STUB: interface agreed for W11; implementation lands separately.
#![allow(dead_code)]

use std::path::PathBuf;

/// Who is calling the team tools: set by the subagent runner around a
/// child's turn. Outside such a scope the caller is the parent (`parent`)
/// and the store directory comes from `ToolCtx::subagent_dir`.
#[derive(Clone, Debug)]
pub struct Member {
    /// The session's subagent store directory (holds `orchestration.jsonl`).
    pub dir: PathBuf,
    /// The child's name.
    pub name: String,
}

tokio::task_local! {
    pub static MEMBER: Member;
}
