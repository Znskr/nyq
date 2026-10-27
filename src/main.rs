mod account;
mod acp;
mod agent;
mod auto_upgrade;
mod catalog;
mod classifier;
mod cli;
mod command_effect;
mod commands;
mod config;
mod diagnostics;
mod doctor;
mod extensions;
mod github;
mod grants;
mod hooks;
mod http;
mod limits;
mod login;
mod mcp;
mod mcp_cli;
mod mcp_client;
mod mcp_features;
mod mcp_schema;
mod mcp_slash;
mod mcp_sse;
mod models;
mod notify;
mod permissions;
mod pricing;
mod prompt;
mod provider;
mod record_tape;
mod sandbox;
mod session;
mod settings;
mod skill_install;
mod skill_invoke;
mod skills;
mod slash;
mod subagents;
mod templates;
mod tool_list;
mod tools;
mod trace;
mod types;
mod ui;
mod undo;
mod upgrade;
mod usage;
mod workspace;

use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result};
use clap::Parser;
use tokio_util::sync::CancellationToken;

use agent::{Agent, TurnEnd};
use cli::{AskOpts, Cli, Command, Ctx, SessionCommand};
use mcp::{McpManager, ServerConfig};
use session::Session;
use tools::Registry;
use types::Usage;
use ui::tui::{Command as UiCommand, Footer, Tui};

const COMMANDS: &[(&str, &str)] = &[
    ("/help", "show commands"),
    ("/new", "start a fresh conversation"),
    (
        "/clear",
        "start a fresh session (background processes are kept)",
    ),
    ("/reset", "clear this conversation and stop background processes, keeping the session"),
    ("/continue", "continue an interrupted or cut-off response"),
    (
        "/rename",
        "set the session title; /rename alone asks the model",
    ),
    (
        "/resume",
        "resume a saved conversation: pick one, or /resume ID|last",
    ),
    ("/sessions", "list saved conversations"),
    (
        "/workspace",
        "additional directories: /workspace [list|add|remove PATH|clear]",
    ),
    (
        "/model",
        "switch model or provider ({key}); /model ID|QUERY [effort] [fast|normal]",
    ),
    ("/compact", "summarize the conversation to free up context"),
    ("/copy", "copy the last answer to the clipboard"),
    ("/undo", "revert the last file change nyq made"),
    ("/tools", "every tool, whether it is on and how it is approved (with --debug): /tools [list|info|disable|enable|trust|untrust NAME]"),
    ("/skills", "skills: browse, use and manage skills; /skills [list|install SOURCE [--project]|remove NAME|info NAME|create NAME|path]"),
    ("/agents", "named subagents: /agents [list|show NAME|stop NAME|delete NAME]"),
    ("/login", "sign in: ChatGPT or Grok in the browser, or an API key (hidden input)"),
    ("/logout", "remove nyq's stored sign-in: /logout [provider]"),
    ("/setup", "connections, default provider, credential source"),
    ("/mcp", "MCP servers and tools: /mcp [list|add|remove|reload|path|connect|disconnect|login|logout|trust|resource|prompt]"),
    ("/memory", "saved memories; /memory remove N forgets one"),
    (
        "/background",
        "background processes; /background logs|open|kill ID|last",
    ),
    (
        "/permissions",
        "permission mode and rules: /permissions [ask|auto|yolo|rules|reload|reset|remember|revoke]",
    ),
    (
        "/allowlist",
        "trusted commands, tools and URLs: /allowlist [view|add|remove|reset], scoped with local|user",
    ),
    (
        "/hooks",
        "list hooks and trust state; /hooks trust trusts the project's hooks",
    ),
    (
        "/extensions",
        "extensions and their state: /extensions [list|reload|trust]",
    ),
    (
        "/yolo",
        "toggle yolo mode (never ask; deny rules still block)",
    ),
    ("/usage", "usage totals (also /cost): /usage [session|24h|7d|30d]"),
    (
        "/stats",
        "session turns, requests, tokens, tools and duration",
    ),
    ("/status", "configuration and runtime status"),
    ("/version", "show nyq version"),
    ("/exit", "quit (also Ctrl-D)"),
];

/// Commands listed only with `--debug` (see [`config::debug`]).
const DEBUG_COMMANDS: &[&str] = &["/tools"];

fn main() {
    http::init();
    // Re-exec entry point used to confine a command before it starts. Runs
    // before the async runtime so no other threads exist yet.
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some(sandbox::HELPER_ARG) {
        sandbox::helper_main(args[2..].to_vec());
    }
    let runtime = tokio::runtime::Runtime::new().expect("starting async runtime");
    if let Err(e) = runtime.block_on(run()) {
        if let Some(Exit(code)) = e.downcast_ref::<Exit>() {
            std::process::exit(*code);
        }
        if JSON_ERRORS.load(std::sync::atomic::Ordering::Relaxed) {
            // `ask --json` failed before it ran: still one JSON object.
            println!(
                "{}",
                ask_json(None, 1, None, None, 0, Some(format!("{e:#}")))
            );
        } else {
            ui::error(&format!("{e:#}"));
        }
        std::process::exit(1);
    }
}

/// Ends the process with this status; whatever needed saying was printed.
#[derive(Debug)]
struct Exit(i32);

impl std::fmt::Display for Exit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "exit status {}", self.0)
    }
}

impl std::error::Error for Exit {}

/// Set for `ask --json`, so an early failure is reported as JSON too.
static JSON_ERRORS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The single object `ask --json` prints.
fn ask_json(
    record: Option<&ui::RunRecord>,
    exit_code: i32,
    model: Option<&str>,
    session_id: Option<&str>,
    steps: u64,
    error: Option<String>,
) -> serde_json::Value {
    let mut v = serde_json::json!({
        "output": record.map_or("", |r| r.output.as_str()),
        "final_output": record.map_or("", |r| r.final_output.trim()),
        "exit_code": exit_code,
        "model": model,
        "session_id": session_id,
        "steps": steps,
        "tool_calls": record.map_or(&[][..], |r| r.tool_calls.as_slice()),
        "notices": record.map_or(&[][..], |r| r.notices.as_slice()),
    });
    if let Some(e) = error {
        v["error"] = e.into();
    }
    v
}

/// `NYQ_PERMISSION_MODE`, when set: the default below the command-line flags.
fn env_permission_mode() -> Option<String> {
    std::env::var("NYQ_PERMISSION_MODE")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

async fn run() -> Result<()> {
    // Kept for relaunching after an auto-upgrade (Ctrl+G).
    let raw_args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    let mut cli = Cli::parse();
    let ask = match &cli.command {
        Some(Command::Ask { opts, .. }) => opts.clone(),
        _ => cli.ask.clone(),
    };
    JSON_ERRORS.store(ask.json, std::sync::atomic::Ordering::Relaxed);
    config::set_debug(cli.debug || config::debug_from_env());
    trace::init_from_env();
    let home = config::home();
    let sessions_dir = home.join("sessions");
    let cwd = std::env::current_dir().context("reading current directory")?;
    let (user_settings, settings_warnings) = settings::load_layered(&home, &cwd);
    let color_warnings = ui::theme::set_colors(&user_settings.theme_colors);
    settings::install(user_settings);
    for w in settings_warnings.into_iter().chain(color_warnings) {
        ui::error(&w);
    }
    limits::configure(&settings::current(), &cli.context_limit)?;
    let env_mode = env_permission_mode();
    let (permission_mode, mode_warning) = permissions::resolve_mode(
        cli.permission_mode
            .as_deref()
            .or(env_mode.as_deref().filter(|_| !cli.yolo)),
        cli.yolo,
        &settings::current().permission_mode,
    )?;
    if let Some(w) = &mode_warning {
        ui::error(w);
    }

    let explicit_mode = cli.permission_mode.is_some() || cli.yolo || env_mode.is_some();
    let print = cli.print || matches!(cli.command, Some(Command::Ask { .. })) || ask.print_only();
    let prompt = match &cli.command {
        Some(Command::Ask { prompt, .. }) => prompt.clone(),
        _ => cli.prompt.clone(),
    };
    let resume = match &cli.command {
        Some(Command::Session {
            action: Some(SessionCommand::Resume { which }),
        })
        | Some(Command::Resume { which }) => Some(which.clone().unwrap_or_default()),
        _ => cli
            .resume
            .clone()
            .or_else(|| cli.continue_last.then(|| "last".to_string())),
    };
    anyhow::ensure!(
        !(ask.no_save && resume.is_some()),
        "--no-save cannot be combined with --resume or --continue"
    );
    anyhow::ensure!(
        !ask.continue_recovery || resume.as_deref().is_some_and(|r| !r.is_empty()),
        "--continue-recovery needs a session: pass -r ID or -c"
    );
    if let Some(system) = &ask.system {
        prompt::set_base(system.clone());
    }

    // Subcommands that run and exit are handled in `cli`; the rest continue
    // into the interactive or print path below.
    let command = cli.command.take();
    let ctx = Ctx {
        cli: &cli,
        home: &home,
        sessions_dir: &sessions_dir,
        cwd: &cwd,
        permission_mode,
        explicit_mode,
    };
    if ctx.dispatch(command).await? {
        return Ok(());
    }

    let provider = config::provider(&home, cli.provider.as_deref(), cli.model.as_deref())?;
    let prepared = workspace::Runtime::prepare(&home, &cwd, &cli.add_dir, cli.no_additional_dirs)?;
    for warning in &prepared.warnings {
        ui::error(warning);
    }
    let mut policy = sandbox_policy(&cli)?;
    policy.writable = prepared.active;
    let sandbox = std::sync::Arc::new(policy);
    let mut ws = prepared.runtime;
    let (hooks, hook_warnings) = hooks::Hooks::load(&home, &cwd);
    for w in &hook_warnings {
        ui::error(w);
    }
    let terminal = !print
        && std::io::IsTerminal::is_terminal(&std::io::stdin())
        && std::io::IsTerminal::is_terminal(&std::io::stdout());
    let ext_mode = if terminal {
        extensions::Mode::Tui
    } else {
        extensions::Mode::Print
    };
    if terminal {
        let (keys, key_warnings) = ui::keys::load(&home);
        ui::keys::install(keys);
        for w in &key_warnings {
            ui::error(w);
        }
    }
    let (ext, ext_warnings) = extensions::Extensions::load(&home, &cwd, ext_mode);
    for w in &ext_warnings {
        ui::error(w);
    }
    let hooks = hooks.with_extensions(ext.clone());
    let session = if ask.no_save && print {
        None
    } else {
        Some(Session::create(&sessions_dir, &cwd, provider.model())?)
    };
    let mut agent = Agent {
        session,
        usage_session_id: None,
        provider,
        tools: Registry::builtin(),
        system: String::new(),
        items: Vec::new(),
        cwd: cwd.clone(),
        permission_mode,
        review_cache: Default::default(),
        allowed: HashSet::new(),
        permissions: std::sync::Arc::new(load_permissions(&home, &cwd)),
        usage: Usage::default(),
        stats: Default::default(),
        context_window: 0,
        context_tokens: 0,
        context_estimated: false,
        suppress_auto_compact: false,
        submitted: 0,
        hooks: std::sync::Arc::new(hooks),
        sandbox,
        undo: Default::default(),
    };
    agent.context_window =
        config::context_window_for(agent.provider.name(), agent.provider.context_window());
    let mut skills = skills::discover(&home, &cwd);
    commands::sync_prompt(&mut agent, &skills);
    skills::register(&mut agent.tools, skills.clone(), &home, &cwd);

    // `-r` without an id opens the session picker once the TUI is up.
    let pick = resume.as_deref() == Some("");
    let mut resumed = None;
    if let Some(which) = resume.as_ref().filter(|_| !pick) {
        let (s, meta, items) = session::load(&sessions_dir, which, &cwd)?;
        agent.session = Some(s);
        agent.items = items;
        agent.adopt_transcript();
        grants::restore(&mut agent);
        resumed = Some(meta);
    }

    let mut mcp = McpManager::new(home.clone());
    let (mut servers, warnings) = mcp::load_configs(&home, &cwd);
    for w in &warnings {
        ui::error(w);
    }

    ext.attach(&mut agent.tools);
    if let Some(s) = &agent.session {
        ext.set_session(&s.id);
    }
    let mut text = prompt.join(" ");
    let interactive = terminal;
    // A yolo saved from the settings menu never carries over to print mode or
    // piped input: scripts get mutations only when they ask on the command line.
    if !interactive {
        agent.permission_mode = cli::noninteractive_mode(agent.permission_mode, explicit_mode);
    }
    anyhow::ensure!(
        !cli.record || interactive,
        "--record requires an interactive terminal"
    );
    // NYQ_RECORD names the tape file, with or without --record.
    let tape = record_tape::env_path();
    let _capture = if interactive && (cli.record || tape.is_some()) {
        let id = &agent.session.as_ref().expect("session").id;
        Some(record_tape::Capture::start(&home, id, tape)?)
    } else {
        None
    };
    commands::apply_startup(&home, &mut agent);
    // A resumed session keeps its model unless one is given on the command line.
    if let Some(meta) = resumed
        .as_ref()
        .filter(|_| cli.model.is_none() && cli.provider.is_none())
    {
        commands::restore_prefs(&home, &mut agent, meta.prefs.as_ref());
    }
    if !interactive {
        anyhow::ensure!(
            !pick,
            "-r without an id needs a terminal to pick a session; pass -r ID or -c"
        );
        return print_mode(
            &mut agent,
            &mut mcp,
            &servers,
            &ext,
            &ask,
            &mut text,
            resumed.as_ref(),
        )
        .await;
    }
    let footer = Footer {
        cwd: cwd.display().to_string(),
        sandbox: sandbox_label(&agent.sandbox),
        ..Footer::default()
    };
    let reserved: Vec<&str> = COMMANDS
        .iter()
        .chain(commands::COMMANDS)
        .map(|(c, _)| &c[1..])
        .chain(["quit", "q", "clear", "img", "transcript", "paste"])
        .collect();
    ext.reserve(reserved.iter().map(|c| c.to_string()));
    let templates = templates::discover(&home, &cwd, &reserved);
    let commands: Vec<(String, String)> = COMMANDS
        .iter()
        .chain(commands::COMMANDS)
        .map(|(c, d)| (c.to_string(), commands::describe(c, d)))
        .chain(templates.iter().map(|t| {
            (
                format!("/{}", t.name),
                format!("{} ({})", t.description, t.scope),
            )
        }))
        .collect();
    // Words that are commands; any other `/word …` line is a prompt.
    let known: HashSet<String> = commands
        .iter()
        .map(|(c, _)| c.clone())
        .chain(slash::ALIASES.iter().map(|(a, _)| a.to_string()))
        .collect();
    // Developer-only commands still run (and say how to get them), but only
    // `--debug` lists them in the `/` menu, `/help` and completion.
    let commands: Vec<(String, String)> = commands
        .into_iter()
        .filter(|(c, _)| config::debug() || !DEBUG_COMMANDS.contains(&c.as_str()))
        .collect();
    let mut tui = Tui::start(footer, home.clone(), commands)?;
    tui.screen()
        .editor
        .set_skills(skill_invoke::entries(&skills));
    sync_footer(&tui, &agent, None);
    ui::banner(env!("CARGO_PKG_VERSION"));
    let exe = upgrade::current_exe().ok();
    let upgrader = auto_upgrade::should_start(settings::current().auto_upgrade, exe.as_deref())
        .then(|| auto_upgrade::Upgrader::start(&home));
    tui.screen().upgrader = upgrader.clone();
    if let Some(meta) = &resumed {
        ui::replay(&agent.items);
        ui::info(&format!("Resumed {} · {}", meta.id, meta.title));
        ui::set_session_title(&meta.title);
    }
    // Prompt commands follow servers that connect or change later.
    let screen = tui.screen.clone();
    mcp.on_change(move |mcp| {
        let commands = mcp_features::prompt_commands(mcp);
        screen
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .editor
            .set_mcp_commands(commands);
    });
    // Extensions start in the background; their commands and status
    // segments appear as they come up.
    let screen = tui.screen.clone();
    let watched = ext.clone();
    ext.on_change(move || {
        let mut s = screen.lock().unwrap_or_else(|e| e.into_inner());
        s.editor.set_extension_commands(watched.commands());
        s.footer.extensions = watched.statuses();
        s.redraw();
    });
    ext.start();
    if let Err(e) = mcp_slash::connect_profile_servers(&mut mcp, &mut agent, &servers, false).await
    {
        drop(tui);
        mcp.shutdown().await;
        ext.shutdown().await;
        return Err(e);
    }
    mcp_features::sync(&mut agent, &mcp);
    tui.screen()
        .editor
        .set_mcp_commands(mcp_features::prompt_commands(&mcp));
    hooks::session_start(
        &agent,
        if resumed.is_some() {
            "resume"
        } else {
            "startup"
        },
    )
    .await;
    agent.tools.add(Box::new(agent::AskUserTool));
    if pick {
        // A prompt given with `-r` waits in the editor until a session is picked.
        tui.screen().editor.set_text(text.trim());
        text.clear();
        open_sessions(&tui, &sessions_dir, &agent)?;
    }
    let mut pending: std::collections::VecDeque<String> = (!text.trim().is_empty())
        .then_some(text)
        .into_iter()
        .collect();
    // The model's catalog entry, once picked in the switcher (for its efforts).
    let mut current: Option<models::ModelChoice> = None;
    // Set by Ctrl+G once an installed upgrade can take over this conversation.
    let mut relaunch = None;

    loop {
        grants::persist(&mut agent);
        agent.sync_prefs();
        sync_footer(&tui, &agent, current.as_ref());
        let next = match pending.pop_front() {
            Some(t) => Some(UiCommand::Submit(t)),
            None => tokio::select! {
                c = tui.commands.recv() => c,
                // An extension sent a message while nyq was idle.
                _ = ext.queued() => {
                    let queued = ext.take_all_queued();
                    if queued.is_empty() {
                        continue;
                    }
                    Some(UiCommand::Submit(queued.join("\n\n")))
                }
            },
        };
        let line = match next {
            Some(UiCommand::Submit(t)) => t,
            Some(UiCommand::SetModel(choice)) => {
                switch_model(&home, &mut agent, &choice);
                current = Some(choice);
                continue;
            }
            Some(UiCommand::Resume(id)) => {
                if let Err(e) = resume_session(&mut agent, &sessions_dir, &id).await {
                    ui::error(&format!("{e:#}"));
                }
                continue;
            }
            Some(UiCommand::CycleEffort) => {
                cycle_effort(&home, &mut agent, current.as_ref());
                continue;
            }
            Some(UiCommand::Setting(id, value)) => {
                commands::apply_setting(&home, &mut agent, current.as_ref(), id, &value);
                continue;
            }
            Some(UiCommand::SetPermissionMode(mode)) => {
                commands::set_permission_mode(&home, &mut agent, &mode);
                continue;
            }
            Some(UiCommand::ApplyUpgrade) => {
                match prepare_relaunch(&tui, &mut agent, &raw_args, &cwd) {
                    Ok(r) => {
                        relaunch = Some(r);
                        break;
                    }
                    Err(notice) => {
                        tui.screen().set_hint(None);
                        ui::error(&notice);
                        continue;
                    }
                }
            }
            Some(UiCommand::Exit) | None => break,
        };
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }
        if let Some(cmd) = line.strip_prefix('!') {
            shell(cmd, &cwd, &tui).await;
            continue;
        }
        // A prompt template (`/name args`) sends its expanded text.
        let line = match templates::invoke(&templates, &line) {
            Some(text) if !text.trim().is_empty() => text,
            _ => line,
        };
        // `/path/to/file what is this?` and `/skill-name …` are prompts.
        let is_known = |c: &str| known.contains(c) || ext.has_command(c);
        let is_command = line.starts_with('/')
            && skill_invoke::leading(&skills, &line, is_known).is_none()
            && !slash::is_prompt(&line, is_known);
        let first = line.split_whitespace().next().unwrap_or("");
        // An MCP prompt's messages become the turn's text.
        let mut turn_text = line.clone();
        if is_command && !known.contains(first) && ext.has_command(first) {
            // An extension command prints a message and/or sends a prompt.
            let cancel = CancellationToken::new();
            tui.screen().set_busy(Some("Running"), Some(cancel.clone()));
            let args = line[first.len()..].trim().to_string();
            let result = ext.run_command(first, &args, &cancel).await;
            tui.screen().idle();
            match result {
                Ok(r) => {
                    if let Some(m) = &r.message {
                        ui::info(m);
                    }
                    match r.prompt {
                        Some(p) => turn_text = p,
                        None => continue,
                    }
                }
                Err(e) => {
                    ui::error(&format!("{e:#}"));
                    continue;
                }
            }
        } else if line.starts_with("/mcp:") {
            match mcp_features::prompt_turn(&mcp, &line).await {
                Ok(t) => turn_text = t,
                Err(e) => {
                    ui::error(&format!("{e:#}"));
                    continue;
                }
            }
        } else if is_command {
            if line.trim() == "/model" {
                tui.open_models();
                continue;
            }
            if let Some(rest) = line.strip_prefix("/model ") {
                let args: Vec<&str> = rest.split_whitespace().collect();
                if let Err(e) = model_command(&home, &mut agent, &tui, &mut current, &args).await {
                    ui::error(&format!("{e:#}"));
                }
                continue;
            }
            let cmd = line.split_whitespace().next().unwrap_or("");
            if commands::handles(cmd) {
                let args: Vec<&str> = line.split_whitespace().skip(1).collect();
                let ctx = commands::Ctx {
                    agent: &mut agent,
                    tui: &tui,
                    home: &home,
                    templates: &templates,
                };
                if let Err(e) = commands::run(cmd, &args, ctx).await {
                    ui::error(&format!("{e:#}"));
                }
                continue;
            }
            let result = slash(
                &line,
                &mut agent,
                &mut mcp,
                &mut servers,
                &sessions_dir,
                &tui,
                &mut skills,
                &mut ws,
            )
            .await;
            if line.starts_with("/mcp") {
                // Servers came or went: refresh resource tools and prompt commands.
                mcp_features::sync(&mut agent, &mcp);
                tui.screen()
                    .editor
                    .set_mcp_commands(mcp_features::prompt_commands(&mcp));
            }
            if line.starts_with("/skills") {
                tui.screen()
                    .editor
                    .set_skills(skill_invoke::entries(&skills));
            }
            match result {
                Ok(true) => continue,
                Ok(false) => break,
                Err(e) => {
                    ui::error(&format!("{e:#}"));
                    continue;
                }
            }
        }
        tui.screen().print_user(&line);
        // Skills named in the prompt send their instructions with it.
        let chosen = skill_invoke::named(&skills, &line);
        if !chosen.is_empty() {
            let (text, errors) = skill_invoke::turn_text(&chosen, &turn_text);
            errors.iter().for_each(|e| ui::error(e));
            let names: Vec<&str> = chosen.iter().map(|s| s.id.as_str()).collect();
            ui::info(&format!("Using skill {}", names.join(", ")));
            turn_text = text;
        }
        if tui.screen().footer.title.is_empty() {
            ui::set_session_title(&session::title_of(&line));
        }
        // UserPromptSubmit sees the text the model gets (templates and MCP
        // prompts expanded); a blocked prompt keeps its pending attachments.
        if let Some(msg) =
            hooks::prompt_submit(&mut agent, &turn_text, &CancellationToken::new()).await
        {
            ui::error(&msg);
            continue;
        }
        let (mut item, mut attached, warning) = tools::image::user_turn(&turn_text, &cwd);
        // Images added with /image, /paste or ctrl+v go with this message.
        let extra = std::mem::take(&mut tui.screen().attachments);
        if let types::Item::User { images, .. } = &mut item {
            for a in extra {
                images.push(a.image);
                attached.push(a.label);
            }
        }
        if let Some(warning) = warning {
            ui::error(&warning);
        } else if !attached.is_empty() {
            ui::info(&format!("Attached {}", attached.join(", ")));
        }
        let cancel = CancellationToken::new();
        tui.screen().set_busy(Some("Working"), Some(cancel.clone()));
        let started = std::time::Instant::now();
        let result = agent
            .turn(item, &mut tui.ui(cwd.clone()), cancel.clone())
            .await;
        if !matches!(result, Ok(TurnEnd::Cancelled)) {
            notify::notify(&notify::Event::TurnEnd {
                secs: started.elapsed().as_secs(),
                failed: result.is_err(),
            });
        }
        let leftover = {
            let mut s = tui.screen();
            s.set_busy(None, None);
            if !matches!(result, Ok(TurnEnd::Done)) {
                // Interrupted or failed: give queued messages back for editing.
                s.dequeue();
                s.redraw();
                Vec::new()
            } else {
                s.queue.drain(..).map(|(_, t)| t).collect::<Vec<_>>()
            }
        };
        match result {
            Ok(TurnEnd::Done) => {}
            Ok(TurnEnd::Cancelled) => ui::info("Interrupted."),
            Err(e) => ui::error(&format!("{e:#}")),
        }
        // Keep queue order: a slash command is a barrier, so a later message
        // does not run before it, and an earlier message is not held behind it.
        pending.extend(queue_leftovers(leftover));
    }
    drop(tui);
    if relaunch.is_none() {
        if let Some(id) = agent
            .session
            .as_ref()
            .filter(|s| s.is_saved())
            .map(|s| s.id.clone())
        {
            eprintln!("Resume with: nyq -r {id}");
        }
    }
    if let Some(u) = &upgrader {
        u.stop();
    }
    tools::background::kill_all();
    mcp.shutdown().await;
    ext.shutdown().await;
    if let Some(r) = relaunch {
        // Close the session (and its lock) and the tape before handing over.
        let resume = agent
            .session
            .take()
            .filter(|s| s.is_saved())
            .map(|s| s.id.clone());
        drop(_capture);
        let e = auto_upgrade::relaunch(&r);
        ui::error(&format!(
            "upgrade installed, but relaunch failed: {e}; {}",
            auto_upgrade::manual_hint(resume.as_deref())
        ));
    }
    Ok(())
}

/// Ctrl+G: check that the conversation can be handed to the upgraded binary
/// and build its command line. Errors are notices; the session keeps running.
fn prepare_relaunch(
    tui: &Tui,
    agent: &mut Agent,
    args: &[std::ffi::OsString],
    cwd: &Path,
) -> std::result::Result<auto_upgrade::Relaunch, String> {
    if let Some(notice) = auto_upgrade::apply_refusal(&tui.screen().upgrade_facts()) {
        return Err(notice.to_string());
    }
    let resume = match agent.session.as_mut() {
        Some(s) => {
            s.flush().map_err(|e| {
                format!("upgrade paused because this conversation is not safely resumable: {e:#}")
            })?;
            s.is_saved().then(|| s.id.clone())
        }
        None => None,
    };
    let manual = auto_upgrade::manual_hint(resume.as_deref());
    let exe = upgrade::current_exe().map_err(|e| {
        format!("upgrade installed, but the executable path could not be resolved: {e:#}; {manual}")
    })?;
    if !exe.is_file() {
        return Err(format!(
            "upgrade installed, but relaunch could not be prepared: {} is missing; {manual}",
            exe.display()
        ));
    }
    Ok(auto_upgrade::Relaunch {
        exe,
        args: auto_upgrade::relaunch_args(args, resume.as_deref()),
        cwd: cwd.to_path_buf(),
    })
}

/// Read permissions.json (and the project's), reporting problems once.
fn load_permissions(home: &Path, cwd: &Path) -> permissions::Permissions {
    let (perms, warnings) = permissions::Permissions::load(home, cwd);
    for w in &warnings {
        ui::error(w);
    }
    perms
}

fn sandbox_label(policy: &sandbox::Policy) -> String {
    match policy.mode {
        sandbox::Mode::Off => "no sandbox".into(),
        sandbox::Mode::ReadOnly => {
            format!("read-only{}", if policy.network { " + net" } else { "" })
        }
        sandbox::Mode::WorkspaceWrite => {
            format!("sandboxed{}", if policy.network { " + net" } else { "" })
        }
    }
}

/// `/model ID|QUERY [effort] [fast|normal]`: switch to the model `ID`, or to
/// the only model matching `QUERY` (several open the switcher filtered by
/// it). An id nobody lists is set on the current provider.
async fn model_command(
    home: &std::path::Path,
    agent: &mut Agent,
    tui: &Tui,
    current: &mut Option<models::ModelChoice>,
    args: &[&str],
) -> Result<()> {
    let wanted = models::parse_args(args);
    let cancel = CancellationToken::new();
    tui.screen()
        .set_busy(Some("Listing models"), Some(cancel.clone()));
    let listed = tokio::select! {
        r = models::list(home) => Some(r),
        _ = cancel.cancelled() => None,
    };
    tui.screen().idle();
    let Some((choices, warnings)) = listed else {
        return Ok(());
    };
    let found = models::find(&choices, &wanted.query, agent.provider.name());
    let choice = match found.as_slice() {
        [one] => (*one).clone(),
        [] if !wanted.query.contains(char::is_whitespace) => {
            // Not listed: keep the provider and take the id as given.
            let choice = models::ModelChoice {
                provider: agent.provider.name().into(),
                model: wanted.query.clone(),
                efforts: agent.provider.efforts(),
                ..Default::default()
            };
            // Fast mode is checked against the provider when it is applied.
            models::check_options(&choice, wanted.effort.as_deref(), None)?;
            agent.provider.set_model(choice.model.clone());
            agent.suppress_auto_compact = false;
            ui::info(&format!(
                "  Model set to {} (not in the model list).",
                choice.model
            ));
            apply_model_options(home, agent, None, &wanted);
            return Ok(());
        }
        [] => anyhow::bail!("no model matches `{}`", wanted.query),
        _ if !ui::menus_available() => {
            let names: Vec<String> = choices
                .iter()
                .map(|c| format!("  {} · {}", c.provider, c.model))
                .collect();
            ui::list(&names);
            anyhow::bail!(
                "`{}` matches {} models; name one exactly",
                wanted.query,
                choices.len()
            )
        }
        _ => {
            tui.screen().show_models(choices, warnings, &wanted.query);
            return Ok(());
        }
    };
    models::check_options(&choice, wanted.effort.as_deref(), wanted.fast)?;
    let same = choice.provider == agent.provider.name() && choice.model == agent.provider.model();
    if !same && !switch_model(home, agent, &choice) {
        return Ok(());
    }
    *current = Some(choice);
    apply_model_options(home, agent, current.as_ref(), &wanted);
    Ok(())
}

fn apply_model_options(
    home: &std::path::Path,
    agent: &mut Agent,
    current: Option<&models::ModelChoice>,
    wanted: &models::ModelArgs,
) {
    if let Some(effort) = &wanted.effort {
        commands::apply_setting(home, agent, current, settings::Id::Effort, effort);
        ui::info(&format!("  Reasoning effort: {effort}."));
    }
    if let Some(fast) = wanted.fast {
        commands::apply_setting(
            home,
            agent,
            current,
            settings::Id::Fast,
            if fast { "on" } else { "off" },
        );
    }
}

/// Switch provider and model, keeping the conversation. False on failure
/// (already reported).
fn switch_model(home: &std::path::Path, agent: &mut Agent, choice: &models::ModelChoice) -> bool {
    let effort = agent
        .provider
        .effort()
        .map(str::to_string)
        .filter(|e| choice.efforts.contains(e));
    match models::build(home, choice, effort.clone()) {
        Ok(provider) => {
            agent.provider = provider;
            agent.provider.set_fast(settings::current().fast_mode);
            agent.set_context_window(config::context_window_for(
                &choice.provider,
                choice.context_window,
            ));
            if let Err(e) = models::save_last(home, choice, effort.as_deref()) {
                ui::error(&format!("could not remember the model: {e:#}"));
            }
            ui::info(&format!(
                "  Switched to {} · {}",
                choice.provider, choice.model
            ));
            true
        }
        Err(e) => {
            ui::error(&format!("{e:#}"));
            false
        }
    }
}

/// Shift+Tab: auto → each supported effort → auto.
fn cycle_effort(home: &std::path::Path, agent: &mut Agent, current: Option<&models::ModelChoice>) {
    let efforts: Vec<String> = match current {
        Some(c) if c.provider == agent.provider.name() && c.model == agent.provider.model() => {
            c.efforts.clone()
        }
        _ => agent.provider.efforts(),
    };
    if efforts.is_empty() {
        ui::info("  This model has no adjustable reasoning effort.");
        return;
    }
    let next = match agent.provider.effort() {
        None => Some(efforts[0].clone()),
        Some(e) => efforts
            .iter()
            .position(|x| x == e)
            .and_then(|i| efforts.get(i + 1))
            .cloned(),
    };
    agent.provider.set_effort(next.clone());
    if let Some(c) = current {
        let _ = models::save_last(home, c, next.as_deref());
    }
}

fn sync_footer(tui: &Tui, agent: &Agent, current: Option<&models::ModelChoice>) {
    let mut s = tui.screen();
    commands::sync_footer(&mut s.footer, agent, current);
    s.footer.provider = agent.provider.name().to_string();
    s.footer.model = agent.provider.model().to_string();
    s.footer.effort = agent.provider.effort().map(str::to_string);
    s.footer.permission_mode = agent.permission_mode.as_str().to_string();
    s.footer.usage = agent.usage;
    s.footer.context_tokens = agent.context_tokens;
    s.footer.context_window = agent.context_window;
    s.redraw();
}

/// The requested sandbox, or by default workspace-write when this system
/// supports it (falling back to off with a warning when it doesn't).
fn sandbox_policy(cli: &Cli) -> Result<sandbox::Policy> {
    let mut policy = sandbox::Policy {
        mode: cli
            .sandbox
            .unwrap_or_else(|| settings::sandbox_mode(&settings::current())),
        network: cli.allow_network,
        writable: Vec::new(),
    };
    if let Err(e) = sandbox::check(&policy) {
        if cli.sandbox.is_some() {
            anyhow::bail!(
                "sandbox unavailable: {e} (use --sandbox off to run commands unconfined)"
            );
        }
        ui::error(&format!(
            "sandbox unavailable ({e}); shell commands will run unconfined"
        ));
        policy.mode = sandbox::Mode::Off;
    }
    Ok(policy)
}

/// Answer one prompt without the TUI (`nyq -p`, `nyq ask`). Exits 130 after
/// Ctrl-C and 143 after SIGTERM, cleaning up either way.
#[allow(clippy::too_many_arguments)]
async fn print_mode(
    agent: &mut Agent,
    mcp: &mut McpManager,
    servers: &[ServerConfig],
    ext: &extensions::Extensions,
    ask: &AskOpts,
    text: &mut String,
    resumed: Option<&session::Meta>,
) -> Result<()> {
    let (cancel, signal) = cancel_on_signal();
    let mut plain = ui::PlainUi::new(agent.permission_mode.is_yolo());
    plain.quiet = ask.quiet || ask.json;
    plain.prompt = ask.prompt_permissions;
    plain.record = ask.json.then(ui::RunRecord::default);
    let requests_before = agent.stats.requests;
    let result = async {
        if text.trim().is_empty() && !ask.continue_recovery {
            match read_print_input(std::io::stdin(), &cancel).await? {
                Some(input) => text.push_str(&input),
                None => return Ok(TurnEnd::Cancelled),
            }
        }
        let mut images = Vec::new();
        for path in &ask.image {
            images.push(tools::image::load(&agent.cwd.join(path))?);
        }
        if let Some(meta) = resumed {
            ui::info(&format!("Resumed {} · {}", meta.id, meta.title));
            ui::set_session_title(&meta.title);
        }
        ext.start();
        mcp_slash::connect_profile_servers(mcp, agent, servers, true).await?;
        mcp_features::sync(agent, mcp);
        // A single answer: wait for the extensions' tools and handlers first.
        ext.wait_ready(extensions::STARTUP_TIMEOUT).await;
        hooks::session_start(
            agent,
            if resumed.is_some() {
                "resume"
            } else {
                "startup"
            },
        )
        .await;
        if ask.continue_recovery {
            agent.continue_turn(&mut plain, cancel.clone()).await
        } else if let Some(msg) = hooks::prompt_submit(agent, text, &cancel).await {
            Err(anyhow::anyhow!("{msg}"))
        } else {
            let (mut item, _, warning) = tools::image::user_turn(text, &agent.cwd);
            if let Some(warning) = warning {
                ui::error(&warning);
            }
            if let types::Item::User {
                images: attached, ..
            } = &mut item
            {
                attached.extend(images);
            }
            agent.turn(item, &mut plain, cancel.clone()).await
        }
    }
    .await;
    let code = match &result {
        Ok(TurnEnd::Done) => 0,
        Ok(TurnEnd::Cancelled) => match signal.load(std::sync::atomic::Ordering::SeqCst) {
            0 => 130,
            n => n,
        },
        Err(_) => 1,
    };
    cancel.cancel();
    tools::background::kill_all();
    mcp.shutdown().await;
    ext.shutdown().await;
    if let Some(s) = agent.session.as_mut() {
        let _ = s.flush();
    }
    if ask.json {
        let value = ask_json(
            plain.record.as_ref(),
            code,
            Some(agent.provider.model()),
            agent.session.as_ref().map(|s| s.id.as_str()),
            agent.stats.requests - requests_before,
            result.as_ref().err().map(|e| format!("{e:#}")),
        );
        println!("{value}");
        return if code == 0 {
            Ok(())
        } else {
            Err(Exit(code).into())
        };
    }
    match result {
        Ok(TurnEnd::Done) => Ok(()),
        Ok(TurnEnd::Cancelled) => {
            ui::info("Interrupted.");
            Err(Exit(code).into())
        }
        Err(e) => Err(e),
    }
}

/// Stdin can wait indefinitely for EOF; keep it off the async workers.
async fn read_print_input(
    mut input: impl std::io::Read + Send + 'static,
    cancel: &CancellationToken,
) -> Result<Option<String>> {
    let read = tokio::task::spawn_blocking(move || {
        let mut text = String::new();
        input.read_to_string(&mut text).map(|_| text)
    });
    tokio::select! {
        result = read => Ok(Some(result??)),
        _ = cancel.cancelled() => Ok(None),
    }
}

/// A token that fires on Ctrl-C or SIGTERM, and the exit status the signal
/// calls for (130 or 143; 0 until one arrives). Cancel the token yourself to
/// stop listening.
fn cancel_on_signal() -> (
    CancellationToken,
    std::sync::Arc<std::sync::atomic::AtomicI32>,
) {
    use std::sync::atomic::{AtomicI32, Ordering};
    let token = CancellationToken::new();
    let code = std::sync::Arc::new(AtomicI32::new(0));
    let (t, c) = (token.clone(), code.clone());
    tokio::spawn(async move {
        #[cfg(unix)]
        let term = async {
            use tokio::signal::unix::{signal, SignalKind};
            match signal(SignalKind::terminate()) {
                Ok(mut s) => {
                    s.recv().await;
                }
                Err(_) => std::future::pending().await,
            }
        };
        #[cfg(not(unix))]
        let term = std::future::pending::<()>();
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                c.store(130, Ordering::SeqCst);
                t.cancel();
            }
            _ = term => {
                c.store(143, Ordering::SeqCst);
                t.cancel();
            }
            _ = t.cancelled() => {}
        }
    });
    (token, code)
}

/// `!cmd`: run a command yourself (unsandboxed) and show its output.
async fn shell(cmd: &str, cwd: &Path, tui: &Tui) {
    let cancel = CancellationToken::new();
    tui.screen().set_busy(Some("Running"), Some(cancel.clone()));
    let policy = sandbox::Policy {
        mode: sandbox::Mode::Off,
        ..Default::default()
    };
    let output = tools::run_shell(cmd, cwd, &policy, &cancel, None).await;
    let mut screen = tui.screen();
    screen.idle();
    screen.print_tool("bash", cmd, Some(&output));
}

fn session_lines(dir: &std::path::Path) -> Result<Vec<String>> {
    let (all, skipped) = session::list_report(dir)?;
    let mut lines = format_sessions(&all);
    lines.extend(session::skipped_warning(skipped.len()));
    Ok(lines)
}

pub(crate) fn format_sessions(all: &[session::Meta]) -> Vec<String> {
    if all.is_empty() {
        return vec!["No saved conversations.".into()];
    }
    all.iter()
        .map(|m| {
            format!(
                "{}  {}  {}  {}",
                session_label(&m.id),
                m.updated
                    .with_timezone(&chrono::Local)
                    .format("%Y-%m-%d %H:%M"),
                m.cwd.display(),
                m.title
            )
        })
        .collect()
}

/// Switch to a saved session (`id`, id prefix or `last`), replaying it.
async fn resume_session(
    agent: &mut Agent,
    sessions_dir: &std::path::Path,
    which: &str,
) -> Result<()> {
    // The open session holds the file lock. Drop it before reopening, including
    // when the target is this same session, and put it back if that fails.
    let previous = agent.session.take();
    let restore_id = previous.as_ref().map(|s| s.id.clone());
    drop(previous);
    let (s, meta, items) = match session::load(sessions_dir, which, &agent.cwd) {
        Ok(loaded) => loaded,
        Err(e) => {
            if let Some(id) = restore_id {
                if let Ok((s, _, _)) = session::load(sessions_dir, &id, &agent.cwd) {
                    agent.session = Some(s);
                }
            }
            return Err(e);
        }
    };
    agent.reset(Some(s));
    commands::restore_prefs(&config::home(), agent, meta.prefs.as_ref());
    ui::replay(&items);
    agent.items = items;
    agent.adopt_transcript();
    grants::restore(agent);
    ui::info(&format!("Resumed {} · {}", meta.id, meta.title));
    ui::set_session_title(&meta.title);
    hooks::session_start(agent, "resume").await;
    Ok(())
}

/// Open the session picker (this folder's sessions first); without a
/// terminal, list them.
fn open_sessions(tui: &Tui, sessions_dir: &std::path::Path, agent: &Agent) -> Result<()> {
    if !ui::menus_available() {
        ui::list(&session_lines(sessions_dir)?);
        ui::info("  Resume one with /resume ID or /resume last.");
        return Ok(());
    }
    let all = session::list(sessions_dir)?;
    let current = agent.session.as_ref().map(|s| s.id.as_str());
    tui.screen().open_sessions(all, &agent.cwd, current);
    Ok(())
}

/// Handle a slash command. Returns Ok(false) to quit.
#[allow(clippy::too_many_arguments)]
async fn slash(
    line: &str,
    agent: &mut Agent,
    mcp: &mut McpManager,
    servers: &mut Vec<ServerConfig>,
    sessions_dir: &std::path::Path,
    tui: &Tui,
    skills: &mut Vec<skills::Skill>,
    ws: &mut workspace::Runtime,
) -> Result<bool> {
    let mut parts = line.split_whitespace();
    let cmd = parts.next().unwrap_or("");
    let arg: Vec<&str> = parts.collect();
    match cmd {
        "/exit" | "/quit" | "/q" => return Ok(false),
        "/help" | "/?" => {
            if ui::menus_available() {
                tui.screen().open_help();
            } else {
                let lines: Vec<String> = tui
                    .screen()
                    .editor
                    .commands()
                    .iter()
                    .map(|(c, d)| format!("{c:<14} {d}"))
                    .collect();
                ui::list(&lines);
            }
            let k = ui::keys::current();
            use ui::keys::KeyAction as K;
            ui::info(&format!(
                "  !CMD run a shell command yourself (not sandboxed) · $skill or /skill sends a skill with your message\n  {} while working steers · {} queues a follow-up · {} edits the queue · {} interrupts\n  {} transcript · {} paste an image · {} reload into a ready upgrade · /keys key bindings · prompt templates: see /alias",
                k.title(K::Submit),
                k.title(K::FollowUp),
                k.title(K::Dequeue),
                k.title(K::Interrupt),
                k.title(K::ExpandTools),
                k.title(K::PasteImage),
                k.title(K::Reload),
            ));
        }
        "/new" | "/clear" => {
            commands::fresh(agent, sessions_dir)?;
            ui::set_session_title("");
            hooks::session_start(agent, "new").await;
        }
        "/reset" => commands::reset(agent)?,
        "/continue" => commands::continue_response(agent, tui).await?,
        "/rename" => {
            let explicit = if arg.is_empty() {
                None
            } else {
                Some(arg.join(" "))
            };
            commands::rename(agent, tui, explicit.as_deref()).await?;
        }
        "/workspace" => commands::workspace_cmd(agent, skills, ws, &arg.join(" "))?,
        "/sessions" => ui::list(&session_lines(sessions_dir)?),
        "/skills" => skill_install::command(agent, tui, skills, &arg).await?,
        "/agents" => ui::list(&subagents::command(
            agent.session.as_ref().map(|s| s.path.as_path()),
            &arg,
        )?),
        "/memory" => tools::memory::slash(&config::home(), &arg, &mut agent.system),
        "/background" => tools::background::slash(&arg).await,
        "/undo" if agent.undo.is_empty() => ui::info("  Nothing to undo."),
        "/undo" => match agent.undo.undo() {
            Ok(msg) => {
                ui::info(&format!("  {msg}"));
                // Tell the model, so it doesn't assume its edit is still there.
                agent.push(types::Item::user(format!(
                    "[The user undid your last file change: {msg}]"
                )));
            }
            Err(e) => ui::info(&format!("  {e}")),
        },
        "/copy" => {
            let last = agent.items.iter().rev().find_map(|i| match i {
                types::Item::Assistant { text } if !text.trim().is_empty() => Some(text.clone()),
                _ => None,
            });
            match last {
                Some(text) => {
                    tui.screen().copy_osc52(&text);
                    let native = {
                        let text = text.clone();
                        tokio::time::timeout(
                            std::time::Duration::from_secs(3),
                            tokio::task::spawn_blocking(move || ui::tui::copy_native(&text)),
                        )
                        .await
                    };
                    let via = match native {
                        Ok(Ok(Some(tool))) => tool,
                        _ => "terminal (OSC 52)",
                    };
                    ui::info(&format!(
                        "  Copied the last answer ({} chars) via {via}.",
                        text.chars().count()
                    ));
                }
                None => ui::info("  Nothing to copy yet."),
            }
        }
        "/compact" => {
            let cancel = CancellationToken::new();
            tui.screen()
                .set_busy(Some("Compacting"), Some(cancel.clone()));
            let result = agent.compact(&mut tui.ui(agent.cwd.clone()), &cancel).await;
            tui.screen().idle();
            if !result? {
                ui::info("Interrupted.");
            }
        }
        "/resume" => match arg.first() {
            Some(which) => resume_session(agent, sessions_dir, which).await?,
            None => open_sessions(tui, sessions_dir, agent)?,
        },
        "/tools" => tool_list::command(agent, tui, &arg),
        "/mcp" => mcp_slash::command(agent, mcp, servers, tui, &arg).await?,
        "/permissions" => commands::permissions(agent, tui, &config::home(), &arg).await,
        "/allowlist" => commands::allowlist(agent, &arg),
        "/hooks" => hooks::command(agent, arg.first().copied()),
        "/extensions" => {
            if let Some(ext) = agent.hooks.extensions().cloned() {
                extensions::slash(&ext, &arg).await;
            }
        }
        "/login" | "/logout" | "/setup" => account::slash(cmd, &arg, agent, tui).await?,
        "/yolo" => commands::toggle_yolo(&config::home(), agent),
        "/usage" | "/cost" => usage::slash(
            &config::home(),
            &arg,
            agent.session.as_ref().map(|s| s.id.as_str()),
        )?,
        "/stats" => agent.stats.show(agent.usage),
        "/status" => doctor::show_agent_status(agent, mcp, servers, skills.len())?,
        "/version" => doctor::version(),
        other => ui::error(&format!("unknown command {other} (try /help)")),
    }
    Ok(true)
}

fn session_label(id: &str) -> String {
    id.chars().take(8).collect()
}

fn is_slash_command(text: &str) -> bool {
    text.trim_start().starts_with('/')
}

/// Preserve FIFO order. Consecutive chat messages still join into one turn;
/// a slash command is not pulled ahead of or skipped past a message.
fn queue_leftovers(leftover: Vec<String>) -> Vec<String> {
    let mut pending = Vec::new();
    let mut batch = String::new();
    for text in leftover {
        if is_slash_command(&text) {
            if !batch.is_empty() {
                pending.push(std::mem::take(&mut batch));
            }
            pending.push(text);
        } else if batch.is_empty() {
            batch = text;
        } else {
            batch.push_str("\n\n");
            batch.push_str(&text);
        }
    }
    if !batch.is_empty() {
        pending.push(batch);
    }
    pending
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn piped_input_keeps_the_async_worker_responsive() {
        struct WaitingInput {
            started: Option<tokio::sync::oneshot::Sender<()>>,
            release: std::sync::mpsc::Receiver<()>,
        }
        impl std::io::Read for WaitingInput {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                if let Some(started) = self.started.take() {
                    let _ = started.send(());
                }
                let _ = self.release.recv_timeout(std::time::Duration::from_secs(2));
                Ok(0)
            }
        }
        let cancel = CancellationToken::new();
        let (started, ready) = tokio::sync::oneshot::channel();
        let (release, wait) = std::sync::mpsc::channel();
        let token = cancel.clone();
        tokio::spawn(async move {
            ready.await.unwrap();
            token.cancel();
        });
        let result = read_print_input(
            WaitingInput {
                started: Some(started),
                release: wait,
            },
            &cancel,
        )
        .await;
        release.send(()).unwrap();
        assert!(result.unwrap().is_none());
        assert_eq!(
            read_print_input(
                std::io::Cursor::new("hello\nworld"),
                &CancellationToken::new()
            )
            .await
            .unwrap()
            .as_deref(),
            Some("hello\nworld")
        );
    }

    #[test]
    fn resume_flag_value_is_optional() {
        let parse = |args: &[&str]| Cli::try_parse_from(args).unwrap();
        assert_eq!(parse(&["nyq", "-r"]).resume.as_deref(), Some(""));
        assert_eq!(parse(&["nyq", "--resume"]).resume.as_deref(), Some(""));
        assert_eq!(parse(&["nyq", "-r", "abc"]).resume.as_deref(), Some("abc"));
        assert_eq!(parse(&["nyq", "--resume=abc", "hello"]).prompt, ["hello"]);
        assert_eq!(parse(&["nyq", "hi"]).resume, None);
        assert!(parse(&["nyq", "-c"]).continue_last);
        assert_eq!(
            parse(&["nyq", "--permission-mode", "auto"])
                .permission_mode
                .as_deref(),
            Some("auto")
        );
        assert!(matches!(
            parse(&["nyq", "permissions", "--json"]).command,
            Some(Command::Permissions { json: true })
        ));
        let ask = parse(&["nyq", "ask", "hello", "world"]);
        match ask.command {
            Some(Command::Ask { prompt, .. }) => assert_eq!(prompt, ["hello", "world"]),
            _ => panic!("ask did not parse as a subcommand"),
        }
        assert!(
            matches!(parse(&["nyq", "ask"]).command, Some(Command::Ask { prompt, .. }) if prompt.is_empty())
        );
        // Ask options and -r/-c work after the subcommand, and on -p.
        match parse(&[
            "nyq",
            "ask",
            "-r",
            "abc",
            "--json",
            "--quiet",
            "--no-save",
            "--prompt-permissions",
            "--system",
            "be brief",
            "--image",
            "a.png",
            "--image=b.png",
            "--continue-recovery",
            "go",
        ]) {
            Cli {
                resume,
                command: Some(Command::Ask { prompt, opts }),
                ..
            } => {
                assert_eq!(resume.as_deref(), Some("abc"));
                assert_eq!(prompt, ["go"]);
                assert!(
                    opts.json
                        && opts.quiet
                        && opts.no_save
                        && opts.prompt_permissions
                        && opts.continue_recovery
                );
                assert_eq!(opts.system.as_deref(), Some("be brief"));
                assert_eq!(opts.image.len(), 2);
            }
            _ => panic!("ask options did not parse"),
        }
        assert!(parse(&["nyq", "ask", "-c", "hi"]).continue_last);
        let print = parse(&["nyq", "-p", "--json", "hi"]);
        assert!(print.print && print.ask.json && print.ask.print_only());
        assert_eq!(
            parse(&[
                "nyq",
                "--context-limit",
                "skill_catalog_bytes=off",
                "ask",
                "x"
            ])
            .context_limit,
            ["skill_catalog_bytes=off"]
        );
        let dirs = parse(&[
            "nyq",
            "--add-dir",
            "/a",
            "--add-dir=/b",
            "--no-additional-dirs",
            "ask",
            "hi",
        ]);
        assert_eq!(
            dirs.add_dir,
            [
                std::path::PathBuf::from("/a"),
                std::path::PathBuf::from("/b")
            ]
        );
        assert!(dirs.no_additional_dirs);
        assert!(matches!(dirs.command, Some(Command::Ask { .. })));
        assert!(matches!(
            parse(&["nyq", "session", "show"]).command,
            Some(Command::Session { action: Some(SessionCommand::Show { which, json: false }) }) if which == "last"
        ));
        assert!(matches!(
            parse(&["nyq", "session"]).command,
            Some(Command::Session { action: None })
        ));
        assert!(matches!(
            parse(&["nyq", "session", "abc123"]).command,
            Some(Command::Session { action: Some(SessionCommand::Other(args)) }) if args == ["abc123"]
        ));
        assert!(matches!(
            parse(&["nyq", "session", "resume"]).command,
            Some(Command::Session {
                action: Some(SessionCommand::Resume { which: None })
            })
        ));
        assert!(matches!(
            parse(&["nyq", "resume", "last"]).command,
            Some(Command::Resume { which: Some(w) }) if w == "last"
        ));
        assert!(matches!(
            parse(&["nyq", "session", "delete", "abc"]).command,
            Some(Command::Session { action: Some(SessionCommand::Delete { which }) }) if which == "abc"
        ));
        assert!(matches!(
            parse(&["nyq", "sessions", "--all", "--limit", "5", "--json"]).command,
            Some(Command::Sessions {
                all: true,
                limit: Some(5),
                json: true
            })
        ));
        assert!(Cli::try_parse_from(["nyq", "sessions", "--limit", "0"]).is_err());
        assert!(matches!(
            parse(&["nyq", "logout"]).command,
            Some(Command::Logout { provider: None })
        ));
        assert!(matches!(
            parse(&["nyq", "acp", "--log-file", "/tmp/x.log"]).command,
            Some(Command::Acp { log_file: Some(_) })
        ));
        assert!(matches!(
            parse(&["nyq", "workspace"]).command,
            Some(Command::Workspace {
                action: None,
                json: false
            })
        ));
        assert!(matches!(
            parse(&["nyq", "workspace", "--json"]).command,
            Some(Command::Workspace {
                action: None,
                json: true
            })
        ));
        assert!(matches!(
            parse(&["nyq", "workspace", "add", "lib"]).command,
            Some(Command::Workspace {
                action: Some(crate::cli::WorkspaceCommand::Add { .. }),
                ..
            })
        ));
        for args in [
            ["nyq", "doctor", "--json"],
            ["nyq", "models", "--json"],
            ["nyq", "usage", "--json"],
        ] {
            assert!(Cli::try_parse_from(args).is_ok(), "{args:?}");
        }
        assert!(matches!(
            parse(&["nyq", "mcp", "list", "--connect", "--json"]).command,
            Some(Command::Mcp {
                action: mcp_cli::McpCommand::List {
                    connect: true,
                    json: true
                }
            })
        ));
        match parse(&[
            "nyq",
            "mcp",
            "add",
            "--transport",
            "http",
            "web",
            "https://x.test/mcp",
        ])
        .command
        {
            Some(Command::Mcp {
                action:
                    mcp_cli::McpCommand::Add {
                        transport, command, ..
                    },
            }) => {
                assert_eq!(
                    (transport.as_deref(), command),
                    (Some("http"), vec!["https://x.test/mcp".to_string()])
                );
            }
            _ => panic!("mcp add --transport did not parse"),
        }
        assert!(matches!(
            parse(&["nyq", "mcp", "auth", "web"]).command,
            Some(Command::Mcp {
                action: mcp_cli::McpCommand::Login { .. }
            })
        ));
        assert!(Cli::try_parse_from(["nyq", "workspace", "add"]).is_err());
        assert!(
            matches!(parse(&["nyq", "provider", "xai"]).command, Some(Command::Provider { name: Some(n) }) if n == "xai")
        );
        match parse(&[
            "nyq", "mcp", "add", "fs", "--env", "A=1", "--", "npx", "-y", "srv",
        ])
        .command
        {
            Some(Command::Mcp {
                action:
                    mcp_cli::McpCommand::Add {
                        name, env, command, ..
                    },
            }) => {
                assert_eq!(
                    (name.as_str(), env, command),
                    (
                        "fs",
                        vec!["A=1".to_string()],
                        vec!["npx".to_string(), "-y".into(), "srv".into()]
                    )
                );
            }
            _ => panic!("mcp add did not parse"),
        }
        assert!(matches!(
            parse(&["nyq", "mcp", "trust", "approve", "x"]).command,
            Some(Command::Mcp {
                action: mcp_cli::McpCommand::Trust {
                    action: Some(mcp_cli::TrustAction::Approve { .. })
                }
            })
        ));
        assert!(Cli::try_parse_from(["nyq", "session", "delete"]).is_err());
    }

    #[test]
    fn relaunch_knows_every_option_that_takes_a_value() {
        use clap::CommandFactory;
        let cmd = Cli::command();
        for arg in cmd.get_arguments().filter(|a| !a.is_positional()) {
            let long = format!("--{}", arg.get_long().unwrap_or_default());
            let takes = arg.get_action().takes_values();
            if long == "--resume" {
                continue;
            }
            assert_eq!(
                auto_upgrade::VALUE_LONG.contains(&long.as_str()),
                takes,
                "{long}"
            );
            if let Some(short) = arg.get_short() {
                assert_eq!(
                    auto_upgrade::VALUE_SHORT.contains(&short),
                    takes,
                    "-{short}"
                );
            }
        }
    }

    #[test]
    fn session_id_prefix_is_char_aligned() {
        assert_eq!(session_label("ééééé"), "ééééé");
        assert_eq!(session_label("ééééééééEXTRA").chars().count(), 8);
        assert!(is_slash_command("  /model x"));
        assert!(!is_slash_command("  model"));
    }

    #[test]
    fn queued_slash_commands_stay_in_order() {
        assert_eq!(
            queue_leftovers(vec!["/new".into(), "next prompt".into(), "/compact".into()]),
            vec!["/new".to_string(), "next prompt".into(), "/compact".into()]
        );
        assert_eq!(
            queue_leftovers(vec!["one".into(), "two".into(), "/new".into()]),
            vec!["one\n\ntwo".to_string(), "/new".into()]
        );
    }
}
