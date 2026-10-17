//! Interactive slash commands for settings, the status line, sounds and
//! notifications, fast mode, image attachments, prompt templates, feedback
//! and traces, plus the session and workspace actions dispatched from `main`.

use std::path::Path;

use anyhow::{Context, Result};
use tokio_util::sync::CancellationToken;

use crate::agent::{Agent, TurnEnd};
use crate::models::ModelChoice;
use crate::session::{self, Session};
use crate::settings::{self, Category, Id, Segment};
use crate::skills::Skill;
use crate::templates::Template;
use crate::ui::{self, tui::Tui};
use crate::workspace;

pub const COMMANDS: &[(&str, &str)] = &[
    ("/settings", "browse and change settings"),
    ("/statusline", "toggle status line segments: /statusline [model|session|provider|context|tokens|sandbox|folder|branch]"),
    ("/sound", "terminal bell for long turns and prompts: /sound [on|off|max]"),
    ("/notifications", "desktop notifications: /notifications [on|off|after SECONDS]"),
    ("/fast", "toggle fast mode (Codex priority processing) when the model supports it"),
    ("/image", "attach an image to the next message: /image PATH"),
    ("/images", "pending image attachments: /images [clear|remove N]"),
    ("/paste", "attach an image from the clipboard (also ctrl+v)"),
    ("/transcript", "full-screen transcript with search (ctrl+o)"),
    ("/keys", "key bindings and their keys (edit ~/.nyq/keybindings.toml)"),
    ("/alias", "list prompt templates from prompts/*.md (each is a /command)"),
    ("/feedback", "open nyq's issue page in your browser"),
    ("/trace", "write a redacted diagnostic trace and copy its path"),
];

/// Whether `cmd` is one of these commands.
pub fn handles(cmd: &str) -> bool {
    cmd == "/img" || COMMANDS.iter().any(|(c, _)| *c == cmd)
}

/// Efforts the current model supports.
pub fn efforts(agent: &Agent, current: Option<&ModelChoice>) -> Vec<String> {
    match current {
        Some(c) if c.provider == agent.provider.name() && c.model == agent.provider.model() => {
            c.efforts.clone()
        }
        _ if agent.provider.name() == "codex" => ["low", "medium", "high", "xhigh"]
            .map(String::from)
            .to_vec(),
        _ => Vec::new(),
    }
}

/// Fill the footer fields this module owns.
pub fn sync_footer(footer: &mut ui::tui::Footer, agent: &Agent, current: Option<&ModelChoice>) {
    footer.fast = agent.provider.fast();
    footer.fast_supported = agent.provider.supports_fast() || fast_unknown(agent);
    footer.efforts = efforts(agent, current);
    footer.models = crate::provider::listing(agent.provider.name()).map(|l| l.models).unwrap_or_default();
    footer.branch = ui::statusline::git_branch(&agent.cwd);
    footer.segments = settings::current().statusline;
}

/// Apply the settings in effect at startup to the agent.
pub fn apply_startup(home: &Path, agent: &mut Agent) {
    let s = settings::current();
    crate::agent::set_auto_compact_percent(s.auto_compact);
    if s.fast_mode {
        agent.provider.set_fast(true);
    }
    // Fast-mode support and the default classifier model come from the listing.
    if s.fast_mode || agent.permission_mode == crate::permissions::Mode::Auto {
        learn_models_in_background(home, agent);
    }
}

/// Fast-mode support for the provider is only known once its models are listed.
fn fast_unknown(agent: &Agent) -> bool {
    let name = agent.provider.name();
    crate::provider::lists_fast(name) && crate::provider::listing(name).is_none()
}

fn learn_models_in_background(home: &Path, agent: &Agent) {
    if fast_unknown(agent) {
        let home = home.to_path_buf();
        tokio::spawn(async move {
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                crate::models::list(&home),
            )
            .await;
        });
    }
}

/// Save settings, saying so when a project or workspace layer overrides the change.
fn persist(home: &Path, f: impl FnOnce(&mut settings::Settings)) -> anyhow::Result<settings::Settings> {
    let (saved, notice) = settings::update(home, f)?;
    if let Some(notice) = notice {
        ui::info(&format!("  {notice}"));
    }
    Ok(saved)
}

fn save(home: &Path, f: impl FnOnce(&mut settings::Settings)) {
    if let Err(e) = persist(home, f) {
        ui::error(&format!("could not save settings: {e:#}"));
    }
}

/// A session setting changed in the settings menu.
pub fn apply_setting(
    home: &Path,
    agent: &mut Agent,
    current: Option<&ModelChoice>,
    id: Id,
    value: &str,
) {
    match id {
        Id::Effort => {
            let effort = Some(value.to_string()).filter(|v| v != "auto");
            agent.provider.set_effort(effort.clone());
            if let Some(c) = current.filter(|c| {
                c.provider == agent.provider.name() && c.model == agent.provider.model()
            }) {
                let _ = crate::models::save_last(home, c, effort.as_deref());
            }
            save(home, |s| s.effort = effort);
        }
        Id::Fast => set_fast(home, agent, value == "on"),
        Id::Permission => set_permission_mode(home, agent, value),
        _ => {}
    }
}

fn apply_mode(home: &Path, agent: &mut Agent, mode: crate::permissions::Mode) {
    agent.permission_mode = mode;
    if mode == crate::permissions::Mode::Auto {
        learn_models_in_background(home, agent);
    }
    save(home, |s| s.permission_mode = mode.as_str().into());
    ui::info(&format!(
        "  Permission mode: {} ({}).",
        mode.as_str(),
        mode.summary()
    ));
}

/// Set the session permission mode and save it.
pub fn set_permission_mode(home: &Path, agent: &mut Agent, raw: &str) {
    match crate::permissions::Mode::parse(raw) {
        Some(mode) => apply_mode(home, agent, mode),
        None => ui::error(&format!(
            "unknown permission mode `{raw}` (ask, auto or yolo)"
        )),
    }
}

/// `/permissions`: open the mode menu, switch mode, list rules, reload,
/// reset, or remember and revoke exact-call rules for this session.
pub async fn permissions(agent: &mut Agent, tui: &Tui, home: &Path, args: &[&str]) {
    match args {
        ["remember", rest @ ..] => {
            if let Err(e) = crate::grants::remember(agent, tui, rest).await {
                ui::error(&format!("{e:#}"));
            }
        }
        ["revoke", id] => {
            if let Err(e) = crate::grants::revoke(agent, id) {
                ui::error(&format!("{e:#}"));
            }
        }
        ["reset"] => {
            agent.allowed.clear();
            std::sync::Arc::make_mut(&mut agent.permissions).session_rules.clear();
            apply_mode(home, agent, crate::permissions::Mode::Ask);
            ui::info("  Forgot this session's grants and rules.");
        }
        [] => tui.screen().open_permission_modes(),
        ["reload"] => {
            let (perms, warnings) = crate::permissions::Permissions::load(home, &agent.cwd);
            for warning in &warnings {
                ui::error(warning);
            }
            agent.permissions = std::sync::Arc::new(perms);
            ui::info("Reloaded permission rules.");
            list_rules(agent);
        }
        ["rules"] => list_rules(agent),
        [name] => match crate::permissions::Mode::parse(name) {
            Some(mode) => apply_mode(home, agent, mode),
            None => ui::error(&format!(
                "unknown /permissions argument `{name}` (ask, auto, yolo, rules, reload, reset, remember or revoke)"
            )),
        },
        _ => ui::error("usage: /permissions [ask|auto|yolo|rules|reload|reset] | remember allow|deny TOOL ARGS-JSON | revoke ID"),
    }
}

fn list_rules(agent: &Agent) {
    let mut lines = vec![format!(
        "mode: {} ({})",
        agent.permission_mode.as_str(),
        agent.permission_mode.summary()
    )];
    lines.extend(agent.permissions.lines(&agent.allowed));
    lines.extend(crate::grants::rule_lines(&agent.permissions));
    ui::list(&lines);
}

/// `/allowlist`: view, add, remove or reset trusted commands, tools and
/// URLs, for every workspace (user) or this one (local).
pub fn allowlist(agent: &mut Agent, args: &[&str]) {
    let perms = std::sync::Arc::make_mut(&mut agent.permissions);
    match crate::grants::allowlist(perms, &mut agent.allowed, args) {
        Ok(lines) if lines.len() == 1 => ui::info(&format!("  {}", lines[0])),
        Ok(lines) => ui::list(&lines),
        Err(e) => ui::error(&e),
    }
}

/// `/yolo`: switch between yolo and ask, and save the choice.
pub fn toggle_yolo(home: &Path, agent: &mut Agent) {
    let next = if agent.permission_mode.is_yolo() {
        crate::permissions::Mode::Ask
    } else {
        crate::permissions::Mode::Yolo
    };
    apply_mode(home, agent, next);
}

fn set_fast(home: &Path, agent: &mut Agent, on: bool) {
    if on && !agent.provider.supports_fast() {
        ui::info("  This model does not come with a fast mode.");
        return;
    }
    agent.provider.set_fast(on);
    save(home, |s| s.fast_mode = on);
    ui::info(match (on, agent.provider.name()) {
        (false, _) => "  Fast mode off.",
        (true, "codex") => "  Fast mode on: priority processing, which uses more of your plan's usage.",
        (true, _) => "  Fast mode on: faster inference, at the model's higher fast-mode price.",
    });
}

/// Context for running a command.
pub struct Ctx<'a> {
    pub agent: &'a mut Agent,
    pub tui: &'a Tui,
    pub home: &'a Path,
    pub templates: &'a [Template],
}

pub async fn run(cmd: &str, args: &[&str], ctx: Ctx<'_>) -> Result<()> {
    let Ctx {
        agent,
        tui,
        home,
        templates,
    } = ctx;
    match cmd {
        "/settings" => tui.screen().open_settings(Category::All),
        "/statusline" if args.is_empty() => tui.screen().open_settings(Category::StatusLine),
        "/statusline" => {
            let mut changes = Vec::new();
            for a in args {
                let Some(seg) = Segment::parse(a) else {
                    anyhow::bail!("unknown status line segment `{a}` (model, provider, context, tokens, sandbox, folder, branch)");
                };
                changes.push(seg);
            }
            let mut s = settings::current();
            for seg in &changes {
                s.statusline.set(*seg, !s.statusline.get(*seg));
            }
            let saved = persist(home, |x| x.statusline = s.statusline)?;
            let shown: Vec<String> = changes
                .iter()
                .map(|seg| {
                    format!(
                        "{} {}",
                        seg.key(),
                        if saved.statusline.get(*seg) {
                            "on"
                        } else {
                            "off"
                        }
                    )
                })
                .collect();
            ui::info(&format!("  Status line: {}", shown.join(", ")));
        }
        "/sound" => {
            let level = match args.first().copied() {
                None => {
                    if settings::current().sound == "off" {
                        "on"
                    } else {
                        "off"
                    }
                }
                Some(l @ ("on" | "off" | "max")) => l,
                Some(_) => anyhow::bail!("Use: /sound [on|off|max]."),
            };
            persist(home, |s| s.sound = level.into())?;
            ui::info(&format!(
                "  Sound {level}{}",
                match level {
                    "on" => " (bell when a long turn ends or nyq waits for you)",
                    "max" => " (bell after every turn and prompt)",
                    _ => "",
                }
            ));
        }
        "/notifications" => {
            let s = settings::current();
            let (on, after) = match args {
                [] => (!s.notifications, s.notify_after_secs),
                ["on"] => (true, s.notify_after_secs),
                ["off"] => (false, s.notify_after_secs),
                ["after", n] | [n] if n.parse::<u64>().is_ok() => (true, n.parse::<u64>()?),
                _ => anyhow::bail!("Use: /notifications [on|off|after SECONDS]."),
            };
            persist(home, |s| {
                s.notifications = on;
                s.notify_after_secs = after;
            })?;
            ui::info(&if on {
                format!("  Desktop notifications on: turns longer than {after}s and prompts that wait for you.")
            } else {
                "  Desktop notifications off.".to_string()
            });
        }
        "/fast" => {
            if agent.provider.fast() {
                set_fast(home, agent, false);
                return Ok(());
            }
            if fast_unknown(agent) {
                let cancel = tokio_util::sync::CancellationToken::new();
                tui.screen()
                    .set_busy(Some("Checking the model catalog"), Some(cancel.clone()));
                tokio::select! {
                    _ = tokio::time::timeout(std::time::Duration::from_secs(8), crate::models::list(home)) => {}
                    _ = cancel.cancelled() => {}
                }
                tui.screen().idle();
            }
            set_fast(home, agent, true);
        }
        "/image" | "/img" => {
            anyhow::ensure!(!args.is_empty(), "usage: /image <path>");
            let raw = args.join(" ");
            let raw = raw.trim_matches(|c| c == '\'' || c == '"');
            let path = match raw.strip_prefix("~/") {
                Some(rest) => dirs::home_dir().unwrap_or_default().join(rest),
                None => agent.cwd.join(raw),
            };
            let image = crate::tools::image::load(&path)?;
            let label = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| raw.to_string());
            ui::info(&format!("  attached image: {label}"));
            tui.screen().attach(image, &label);
        }
        "/images" => {
            let mut s = tui.screen();
            match args {
                [] if s.attachments.is_empty() => {
                    drop(s);
                    ui::info("  no pending images");
                }
                [] => {
                    let lines: Vec<String> = s
                        .attachments
                        .iter()
                        .map(|a| {
                            format!(
                                "{} {}  {} · {} KB",
                                a.token(),
                                a.label,
                                a.image.media_type,
                                a.image.data.len() * 3 / 4 / 1024
                            )
                        })
                        .collect();
                    drop(s);
                    ui::list(&lines);
                    ui::info("  They go with your next message. /images remove N or /images clear drops them.");
                }
                ["clear"] => {
                    let ids: Vec<usize> = s.attachments.iter().map(|a| a.id).collect();
                    for id in ids {
                        s.detach(id);
                    }
                    s.redraw();
                    drop(s);
                    ui::info("  cleared pending images");
                }
                ["remove" | "rm", n] => {
                    let a = n.trim_start_matches('#').parse::<usize>().ok().and_then(|id| s.detach(id));
                    let Some(a) = a else {
                        anyhow::bail!("no pending image #{n}")
                    };
                    s.redraw();
                    drop(s);
                    ui::info(&format!("  removed image: {}", a.label));
                }
                _ => anyhow::bail!("Use: /images [clear|remove N]."),
            }
        }
        "/paste" => {
            let image = tokio::task::spawn_blocking(ui::clipboard::read_image).await?;
            match image {
                Ok(image) => {
                    ui::info("  attached image: clipboard");
                    tui.screen().attach(image, "clipboard");
                }
                Err(e) => ui::info(&format!("  {e}")),
            }
        }
        "/transcript" => tui.screen().open_transcript(),
        "/keys" => {
            let mut lines = ui::keys::current().lines();
            lines.push(format!("Edit {} to change these (see README).", crate::config::home().join("keybindings.toml").display()));
            ui::list(&lines);
        }
        "/alias" => match args.first() {
            Some(name) => {
                let name = name.trim_start_matches('/');
                let t = templates.iter().find(|t| t.name == name).ok_or_else(|| {
                    anyhow::anyhow!("no prompt template named `{name}` (see /alias)")
                })?;
                let mut lines = vec![format!("/{} · {} · {}", t.name, t.scope, t.path.display())];
                lines.extend(t.body.lines().map(str::to_string));
                ui::list(&lines);
            }
            None => ui::list(&crate::templates::list_lines(templates)),
        },
        "/feedback" => {
            crate::login::open_browser(crate::diagnostics::FEEDBACK_URL);
            ui::info(&format!(
                "  Opened {} (if your browser didn't open, visit it yourself).",
                crate::diagnostics::FEEDBACK_URL
            ));
        }
        "/trace" => {
            let sandbox = tui.screen().footer.sandbox.clone();
            let info = crate::diagnostics::TraceInfo {
                provider: agent.provider.name(),
                model: agent.provider.model(),
                effort: agent.provider.effort(),
                fast: agent.provider.fast(),
                sandbox: &sandbox,
                yolo: agent.permission_mode.is_yolo(),
                permission_mode: agent.permission_mode.as_str(),
                session_id: agent.session.as_ref().map(|s| s.id.as_str()),
                context_tokens: agent.context_tokens,
                context_window: agent.context_window,
                usage: agent.usage,
            };
            let trace = crate::diagnostics::trace(&info, &agent.items);
            let path = crate::diagnostics::write_trace(home, &trace)?;
            let text = path.display().to_string();
            tui.screen().copy_osc52(&text);
            let copied = {
                let text = text.clone();
                tokio::time::timeout(
                    std::time::Duration::from_secs(3),
                    tokio::task::spawn_blocking(move || ui::tui::copy_native(&text)),
                )
                .await
            };
            let via = if matches!(copied, Ok(Ok(Some(_)))) {
                "path copied"
            } else {
                "path copied via the terminal"
            };
            ui::info(&format!("  Wrote a diagnostic trace to {text} ({via}). Secrets are redacted; read it before sharing."));
        }
        other => anyhow::bail!("unknown command {other}"),
    }
    Ok(())
}

// --- Session and workspace actions ---

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

/// `/new` and `/clear`: a new session. Background processes are global and
/// keep running either way.
pub fn fresh(agent: &mut Agent, sessions_dir: &Path) -> Result<()> {
    agent.reset(Some(Session::create(
        sessions_dir,
        &agent.cwd,
        agent.provider.model(),
    )?));
    crate::tools::memory::refresh_section(&mut agent.system, &crate::config::home());
    ui::info("Started a new conversation.");
    Ok(())
}

/// Resume with the model, effort and fast mode a session last used. When the
/// model can't be built (signed out, provider gone), the current one stays.
pub fn restore_prefs(home: &Path, agent: &mut Agent, prefs: Option<&session::Prefs>) {
    let Some(p) = prefs.filter(|p| !p.provider.is_empty() && !p.model.is_empty()) else {
        return;
    };
    if p.provider != agent.provider.name() || p.model != agent.provider.model() {
        let choice = ModelChoice {
            provider: p.provider.clone(),
            model: p.model.clone(),
            ..ModelChoice::default()
        };
        match crate::models::build(home, &choice, p.effort.clone()) {
            Ok(provider) => {
                agent.provider = provider;
                agent.set_context_window(crate::config::context_window_for(
                    agent.provider.name(),
                    agent.provider.context_window(),
                ));
            }
            Err(e) => {
                ui::error(&format!(
                    "could not restore {} · {} ({e:#}); using {} · {}",
                    p.provider,
                    p.model,
                    agent.provider.name(),
                    agent.provider.model()
                ));
                return;
            }
        }
    }
    agent.provider.set_effort(p.effort.clone());
    agent.provider.set_fast(p.fast);
}

/// `/reset`: drop the transcript and background processes, keeping this
/// session's settings. (`/new` and `/clear` keep background processes.)
pub fn reset(agent: &mut Agent) -> Result<()> {
    agent.clear_context()?;
    std::sync::Arc::make_mut(&mut agent.permissions).session_rules.clear();
    let stopped = crate::tools::background::forget_all();
    ui::info("Cleared the conversation. Session settings are unchanged.");
    if stopped > 0 {
        ui::info(&format!("Stopped {stopped} background process(es)."));
    }
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
    ui::set_session_title(&title);
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
