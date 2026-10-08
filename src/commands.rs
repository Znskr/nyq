//! Slash-command actions for the session and the workspace.
//!
//! The match in `main` stays a dispatch table; the work lives here.

use std::path::Path;

use anyhow::{Context, Result};
use tokio_util::sync::CancellationToken;

use crate::agent::{Agent, TurnEnd};
use crate::session::{self, Session};
use crate::skills::Skill;
use crate::ui::tui::Tui;
use crate::{ui, workspace};

/// Rebuild the system prompt from the current sandbox and skills.
pub fn sync_prompt(agent: &mut Agent, skills: &[Skill]) {
    agent.system = crate::prompt::system(&agent.cwd, &agent.sandbox);
    if !skills.is_empty() {
        agent
            .system
            .push_str(&format!("\n\n{}", crate::skills::prompt_section(skills)));
    }
}

pub fn apply_dirs(agent: &mut Agent, skills: &[Skill], dirs: Vec<std::path::PathBuf>) {
    let policy = std::sync::Arc::make_mut(&mut agent.sandbox);
    policy.writable = dirs;
    sync_prompt(agent, skills);
}

/// `/new` and `/clear`: a new session. nyq has no background processes to
/// carry, so `/clear` matches `/new`.
pub fn fresh(agent: &mut Agent, sessions_dir: &Path) -> Result<()> {
    agent.reset(Some(Session::create(
        sessions_dir,
        &agent.cwd,
        agent.provider.model(),
    )?));
    ui::info("Started a new conversation.");
    Ok(())
}

/// `/reset`: drop the transcript and keep this session's settings.
pub fn reset(agent: &mut Agent) -> Result<()> {
    agent.clear_context()?;
    ui::info("Cleared the conversation. Session settings are unchanged.");
    Ok(())
}

/// `/continue`: ask the model to resume a cut-off response, with no new user text.
pub async fn continue_response(agent: &mut Agent, tui: &Tui) -> Result<()> {
    let cancel = CancellationToken::new();
    tui.screen().set_busy(Some("Working"), Some(cancel.clone()));
    let result = agent
        .continue_turn(&mut tui.ui(agent.cwd.clone()), cancel)
        .await;
    tui.screen().idle();
    match result? {
        TurnEnd::Done => {}
        TurnEnd::Cancelled => ui::info("Interrupted."),
    }
    Ok(())
}

/// `/rename [title]`. With no title, ask the model for a short one.
pub async fn rename(agent: &mut Agent, tui: &Tui, explicit: Option<&str>) -> Result<()> {
    let title = match explicit.map(str::trim).filter(|text| !text.is_empty()) {
        Some(raw) => session::validate_title(raw)?,
        None => {
            let cancel = CancellationToken::new();
            tui.screen().set_busy(Some("Naming"), Some(cancel.clone()));
            let result = agent.suggest_title(&cancel).await;
            tui.screen().idle();
            result?
        }
    };
    agent
        .session
        .as_mut()
        .context("no active session to rename")?
        .set_title(&title)?;
    ui::info(&format!("Renamed: {title}"));
    Ok(())
}

/// `/workspace`, `/workspace add <path>`, `/workspace remove <path>`, `/workspace clear`.
pub fn workspace_cmd(
    agent: &mut Agent,
    skills: &[Skill],
    runtime: &mut workspace::Runtime,
    rest: &str,
) -> Result<()> {
    match workspace::parse_action(rest)? {
        workspace::Action::List => list(runtime),
        workspace::Action::Add(path) => {
            let message = match runtime.add(&path)? {
                workspace::Change::Added(path) => format!("Added {}", path.display()),
                workspace::Change::Already(path) => {
                    format!("Already an additional directory: {}", path.display())
                }
            };
            refresh(agent, skills, runtime)?;
            ui::info(&message);
            Ok(())
        }
        workspace::Action::Remove(path) => {
            let removed = runtime.remove(&path)?;
            refresh(agent, skills, runtime)?;
            ui::info(&format!("Removed {}", removed.display()));
            Ok(())
        }
        workspace::Action::Clear => {
            runtime.clear()?;
            refresh(agent, skills, runtime)?;
            ui::info("Cleared saved additional directories.");
            Ok(())
        }
    }
}

fn list(runtime: &workspace::Runtime) -> Result<()> {
    let view = runtime.view()?;
    for warning in view.warnings {
        ui::error(&warning);
    }
    ui::list(&view.lines);
    Ok(())
}

fn refresh(agent: &mut Agent, skills: &[Skill], runtime: &workspace::Runtime) -> Result<()> {
    let view = runtime.view()?;
    apply_dirs(agent, skills, view.active);
    for warning in view.warnings {
        ui::error(&warning);
    }
    ui::list(&view.lines);
    Ok(())
}
