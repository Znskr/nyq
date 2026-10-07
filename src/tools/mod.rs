//! Tools the model can call. Built-in workspace tools live in submodules; MCP
//! tools are adapted into the same [`Tool`] trait by `crate::mcp`.

mod bash;
mod fs;
pub mod image;
mod search;
pub mod web;
pub mod subagent;

use std::collections::HashSet;
use std::path::PathBuf;

use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::types::ToolSpec;

/// A path as shown to the model and user: always with `/` separators.
pub(crate) fn slash_path(p: &std::path::Path) -> String {
    let s = p.display().to_string();
    if cfg!(windows) { s.replace('\\', "/") } else { s }
}

#[derive(Clone, Default)]
pub struct ToolCtx {
    /// Workspace root; relative paths resolve against it.
    pub cwd: PathBuf,
    /// Fires when the user cancels the turn (Ctrl-C).
    pub cancel: CancellationToken,
    /// How shell commands are confined.
    pub sandbox: std::sync::Arc<crate::sandbox::Policy>,
    /// A copy of the session's model provider, for tools that run their own
    /// model calls (subagents). None in tests.
    pub provider: Option<std::sync::Arc<dyn crate::provider::Provider>>,
    /// The session's `--yolo` flag, inherited by subagents.
    pub yolo: bool,
    /// Rules granted with "always", inherited by subagents.
    pub allowed: HashSet<String>,
}

#[derive(Clone, Debug, Default)]
pub struct ToolOutput {
    /// Text returned to the model.
    pub content: String,
    pub is_error: bool,
    /// Optional richer text for the user (e.g. a unified diff). Not sent to
    /// the model.
    pub display: Option<String>,
    /// Images for the model (sent in a follow-up user message).
    pub images: Vec<crate::types::Image>,
}

impl ToolOutput {
    pub fn ok(content: impl Into<String>) -> Self {
        Self { content: content.into(), ..Self::default() }
    }
    pub fn err(content: impl Into<String>) -> Self {
        Self { content: content.into(), is_error: true, ..Self::default() }
    }
}

#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    /// Whether running this call needs user approval (mutations, shell, MCP).
    fn needs_approval(&self, _args: &Value) -> bool {
        false
    }
    /// One-line human summary of a call, e.g. `src/main.rs` or `cargo test`.
    fn summary(&self, args: &Value) -> String;
    /// Optional preview shown in the approval prompt (e.g. the diff an edit
    /// would make). Must not have side effects.
    fn preview(&self, _args: &Value, _ctx: &ToolCtx) -> Option<String> {
        None
    }
    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolOutput;
}

#[derive(Default)]
pub struct Registry {
    tools: Vec<Box<dyn Tool>>,
}

impl Registry {
    /// The built-in workspace tools: read_file, write_file, edit_file, bash,
    /// glob, grep, list_dir, view_image, web_fetch, web_search, subagent.
    pub fn builtin() -> Self {
        let mut r = Self::default();
        fs::register(&mut r);
        search::register(&mut r);
        bash::register(&mut r);
        image::register(&mut r);
        web::register(&mut r);
        subagent::register(&mut r);
        r
    }

    pub fn add(&mut self, tool: Box<dyn Tool>) {
        self.tools.retain(|t| t.spec().name != tool.spec().name);
        self.tools.push(tool);
    }

    pub fn remove_prefix(&mut self, prefix: &str) {
        self.tools.retain(|t| !t.spec().name.starts_with(prefix));
    }

    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools.iter().find(|t| t.spec().name == name).map(|t| t.as_ref())
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools.iter().map(|t| t.spec()).collect()
    }
}
