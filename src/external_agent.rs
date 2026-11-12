//! External agents over ACP (design §8.1): `subagent` with
//! `mode: "external:<name>"` launches `agents.external.<name>` from the
//! user's settings.toml as an Agent Client Protocol subprocess, sends one
//! prompt, collects the final message and routes the agent's permission
//! requests to the parent's approver.
//!
//! STUB: interface agreed for W11; implementation lands separately.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::agent::Approver;

/// `[agents.external.<name>]` in the user's settings.toml.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Config {
    pub command: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
}

/// The named external agent from the user's own settings (never a project
/// layer: a repository must not be able to choose a command to launch).
pub fn config(name: &str) -> anyhow::Result<Config> {
    anyhow::bail!("external agent `{name}`: not implemented")
}

/// One prompt to an external agent.
pub struct Run<'a> {
    pub task: &'a str,
    /// The session cwd sent in `session/new` and the process's cwd.
    pub cwd: &'a Path,
    /// Who answers the agent's `session/request_permission`; None denies.
    pub approver: Option<Arc<dyn Approver>>,
    /// Allow every permission request without asking (parent in yolo mode).
    pub auto_approve: bool,
    /// Shown with approval prompts, e.g. "external claude: fix the parser".
    pub origin: String,
    pub cancel: CancellationToken,
    pub timeout: Duration,
    /// Called for each tool call the agent reports (title, kind/status).
    pub on_tool: Option<Box<dyn Fn(&str, &str) + Send + Sync>>,
}

/// What came back from one prompt.
#[derive(Clone, Debug, Default)]
pub struct Outcome {
    /// The agent's message text (chunks concatenated).
    pub text: String,
    /// The `PromptResponse` stop reason, e.g. `end_turn`.
    pub stop_reason: String,
    /// (title, status) of each tool call the agent reported.
    pub tool_calls: Vec<(String, String)>,
}

/// Launch `cfg` and run one prompt.
pub async fn run(cfg: &Config, run: Run<'_>) -> anyhow::Result<Outcome> {
    let _ = (cfg, run);
    anyhow::bail!("external agents: not implemented")
}
