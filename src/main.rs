mod acp;
mod agent;
mod catalog;
mod config;
mod login;
mod mcp;
mod mcp_features;
mod models;
mod permissions;
mod prompt;
mod provider;
mod sandbox;
mod session;
mod skills;
mod tools;
mod types;
mod undo;
mod ui;

use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tokio_util::sync::CancellationToken;

use agent::{Agent, TurnEnd};
use mcp::{McpManager, Scope, ServerConfig};
use session::Session;
use tools::Registry;
use types::Usage;
use ui::tui::{Command as UiCommand, Footer, Tui};

#[derive(Parser)]
#[command(name = "nyq", version, about = "A small, friendly coding agent for your terminal")]
struct Cli {
    /// Start the conversation with this message.
    prompt: Vec<String>,
    /// Answer the prompt non-interactively and exit (reads stdin if no prompt).
    #[arg(short, long)]
    print: bool,
    /// Model id (overrides config).
    #[arg(short, long, global = true)]
    model: Option<String>,
    /// Provider: codex, openai, ollama, opencode-go, opencode, xai or vercel.
    #[arg(long, global = true)]
    provider: Option<String>,
    /// Run tools without asking for approval.
    #[arg(long, global = true)]
    yolo: bool,
    /// Confine shell commands the model runs [default: workspace-write where supported].
    #[arg(long, value_enum, global = true)]
    sandbox: Option<sandbox::Mode>,
    /// Allow network access from sandboxed shell commands.
    #[arg(long, global = true)]
    allow_network: bool,
    /// Continue the most recent conversation in this directory.
    #[arg(short = 'c', long = "continue")]
    continue_last: bool,
    /// Resume a saved conversation by id (or id prefix); without an id, pick one.
    #[arg(short, long, value_name = "ID", num_args = 0..=1, default_missing_value = "")]
    resume: Option<String>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// List saved conversations.
    Sessions,
    /// Sign in: ChatGPT subscription (default, Codex models), or store an API
    /// key for opencode-go, opencode, xai or vercel.
    Login {
        /// codex (default), opencode-go, opencode, xai or vercel.
        provider: Option<String>,
    },
    /// Remove nyq's stored login for a provider.
    Logout {
        /// codex, opencode-go, opencode, xai or vercel.
        provider: String,
    },
    /// Run as an Agent Client Protocol (ACP) agent over stdio, for editors like Zed.
    Acp,
}

const COMMANDS: &[(&str, &str)] = &[
    ("/help", "show commands"),
    ("/new", "start a fresh conversation"),
    ("/resume", "resume a saved conversation: pick one, or /resume ID|last"),
    ("/sessions", "list saved conversations"),
    ("/model", "switch model or provider (ctrl+l); /model NAME sets the model id"),
    ("/compact", "summarize the conversation to free up context"),
    ("/copy", "copy the last answer to the clipboard"),
    ("/undo", "revert the last file change nyq made"),
    ("/tools", "list available tools"),
    ("/skills", "list available skills"),
    ("/mcp", "MCP servers: /mcp [connect|disconnect|login|logout NAME]"),
    ("/memory", "saved memories; /memory remove N forgets one"),
    ("/background", "background processes; /background kill ID|last"),
    ("/permissions", "permission rules and session grants; /permissions reload re-reads them"),
    ("/yolo", "toggle auto-approval of tool calls"),
    ("/usage", "token usage for this conversation"),
    ("/exit", "quit (also Ctrl-D)"),
];

fn main() {
    // Re-exec entry point used to confine a command before it starts. Runs
    // before the async runtime so no other threads exist yet.
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some(sandbox::HELPER_ARG) {
        sandbox::helper_main(args[2..].to_vec());
    }
    let runtime = tokio::runtime::Runtime::new().expect("starting async runtime");
    if let Err(e) = runtime.block_on(run()) {
        ui::error(&format!("{e:#}"));
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let cli = Cli::parse();
    let home = config::home();
    let sessions_dir = home.join("sessions");
    let cwd = std::env::current_dir().context("reading current directory")?;

    match cli.command {
        Some(Command::Sessions) => return list_sessions(&sessions_dir),
        Some(Command::Login { provider }) => {
            let msg = match provider.as_deref().filter(|p| *p != "codex") {
                Some(name) => {
                    let service = provider::hosted::Service::parse(name).with_context(|| {
                        format!("unknown provider `{name}` (expected codex, opencode-go, opencode, xai or vercel)")
                    })?;
                    login::login_key(&home, service)?
                }
                None => {
                    login::login(&home, |url| {
                        eprintln!("Opening your browser to sign in. If it doesn't open, visit:\n  {url}");
                        login::open_browser(url);
                    })
                    .await?
                }
            };
            println!("{msg}");
            return Ok(());
        }
        Some(Command::Logout { provider }) => {
            println!("{}", login::logout(&home, &provider)?);
            return Ok(());
        }
        Some(Command::Acp) => {
            let opts = acp::Options { yolo: cli.yolo, sandbox: cli.sandbox, allow_network: cli.allow_network, model: cli.model, provider: cli.provider };
            let result = acp::run(opts).await;
            tools::background::kill_all();
            return result;
        }
        None => {}
    }

    let provider = config::provider(&home, cli.provider.as_deref(), cli.model.as_deref())?;
    let sandbox = std::sync::Arc::new(sandbox_policy(&cli)?);
    let mut agent = Agent {
        session: Some(Session::create(&sessions_dir, &cwd, provider.model())?),
        provider,
        tools: Registry::builtin(),
        system: prompt::system(&cwd, &sandbox),
        items: Vec::new(),
        cwd: cwd.clone(),
        yolo: cli.yolo,
        allowed: HashSet::new(),
        permissions: std::sync::Arc::new(load_permissions(&home, &cwd)),
        usage: Usage::default(),
        context_window: 0,
        context_tokens: 0,
        context_estimated: false,
        suppress_auto_compact: false,
        submitted: 0,
        sandbox,
        undo: Default::default(),
    };
    agent.context_window = config::context_window_for(agent.provider.name(), agent.provider.context_window());
    let skills = skills::discover(&home, &cwd);
    if !skills.is_empty() {
        agent.system.push_str(&format!("\n\n{}", skills::prompt_section(&skills)));
        agent.tools.add(skills::tool(skills.clone()));
    }

    let resume = cli.resume.clone().or_else(|| cli.continue_last.then(|| "last".to_string()));
    // `-r` without an id opens the session picker once the TUI is up.
    let pick = resume.as_deref() == Some("");
    let mut resumed = None;
    if let Some(which) = resume.as_ref().filter(|_| !pick) {
        let (s, meta, items) = session::load(&sessions_dir, which, &cwd)?;
        agent.session = Some(s);
        agent.items = items;
        agent.adopt_transcript();
        resumed = Some(meta);
    }

    let mut mcp = McpManager::new(home.clone());
    let (servers, warnings) = mcp::load_configs(&home, &cwd);
    for w in &warnings {
        ui::error(w);
    }

    let mut text = cli.prompt.join(" ");
    let interactive = !cli.print && std::io::IsTerminal::is_terminal(&std::io::stdin()) && std::io::IsTerminal::is_terminal(&std::io::stdout());
    if !interactive {
        anyhow::ensure!(!pick, "-r without an id needs a terminal to pick a session; pass -r ID or -c");
        if text.trim().is_empty() {
            use std::io::Read;
            std::io::stdin().read_to_string(&mut text)?;
        }
        if let Some(meta) = &resumed {
            ui::info(&format!("Resumed {} · {}", meta.id, meta.title));
        }
        connect_profile_servers(&mut mcp, &mut agent, &servers, true).await;
        mcp_features::sync(&mut agent, &mcp);
        let mut plain = ui::PlainUi::new(cli.yolo);
        let cancel = cancel_on_ctrl_c();
        let (item, _, warning) = tools::image::user_turn(&text, &cwd);
        if let Some(warning) = warning {
            ui::error(&warning);
        }
        let r = agent.turn(item, &mut plain, cancel).await;
        tools::background::kill_all();
        mcp.shutdown().await;
        return r.map(|_| ());
    }

    let footer = Footer { cwd: cwd.display().to_string(), sandbox: sandbox_label(&agent.sandbox), ..Footer::default() };
    let commands = COMMANDS.iter().map(|(c, d)| (c.to_string(), d.to_string())).collect();
    let mut tui = Tui::start(footer, home.clone(), commands)?;
    sync_footer(&tui, &agent);
    ui::banner(env!("CARGO_PKG_VERSION"));
    ui::info(&sandbox::describe(&agent.sandbox));
    if let Some(meta) = &resumed {
        ui::replay(&agent.items);
        ui::info(&format!("Resumed {} · {}", meta.id, meta.title));
    }
    connect_profile_servers(&mut mcp, &mut agent, &servers, false).await;
    mcp_features::sync(&mut agent, &mcp);
    tui.screen().editor.set_mcp_commands(mcp_features::prompt_commands(&mcp));
    agent.tools.add(Box::new(agent::AskUserTool));
    if pick {
        // A prompt given with `-r` waits in the editor until a session is picked.
        tui.screen().editor.set_text(text.trim());
        text.clear();
        open_sessions(&tui, &sessions_dir, &agent)?;
    }
    let mut pending: std::collections::VecDeque<String> = (!text.trim().is_empty()).then_some(text).into_iter().collect();
    // The model's catalog entry, once picked in the switcher (for its efforts).
    let mut current: Option<models::ModelChoice> = None;

    loop {
        sync_footer(&tui, &agent);
        let line = match pending.pop_front() {
            Some(t) => t,
            None => match tui.commands.recv().await {
                Some(UiCommand::Submit(t)) => t,
                Some(UiCommand::SetModel(choice)) => {
                    switch_model(&home, &mut agent, &choice);
                    current = Some(choice);
                    continue;
                }
                Some(UiCommand::Resume(id)) => {
                    if let Err(e) = resume_session(&mut agent, &sessions_dir, &id) {
                        ui::error(&format!("{e:#}"));
                    }
                    continue;
                }
                Some(UiCommand::CycleEffort) => {
                    cycle_effort(&home, &mut agent, current.as_ref());
                    continue;
                }
                Some(UiCommand::Exit) | None => break,
            },
        };
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }
        if let Some(cmd) = line.strip_prefix('!') {
            shell(cmd, &cwd, &tui).await;
            continue;
        }
        // An MCP prompt's messages become the turn's text.
        let mut turn_text = line.clone();
        if line.starts_with("/mcp:") {
            match mcp_features::prompt_turn(&mcp, &line).await {
                Ok(t) => turn_text = t,
                Err(e) => {
                    ui::error(&format!("{e:#}"));
                    continue;
                }
            }
        } else if line.starts_with('/') {
            if line.trim() == "/model" {
                tui.open_models();
                continue;
            }
            let result = slash(&line, &mut agent, &mut mcp, &servers, &sessions_dir, &tui, &skills).await;
            if line.starts_with("/mcp") {
                // Servers came or went: refresh resource tools and prompt commands.
                mcp_features::sync(&mut agent, &mcp);
                tui.screen().editor.set_mcp_commands(mcp_features::prompt_commands(&mcp));
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
        let (item, attached, warning) = tools::image::user_turn(&turn_text, &cwd);
        tui.screen().print_user(&line);
        if let Some(warning) = warning {
            ui::error(&warning);
        } else if !attached.is_empty() {
            ui::info(&format!("Attached {}", attached.join(", ")));
        }
        let cancel = CancellationToken::new();
        tui.screen().set_busy(Some("Working"), Some(cancel.clone()));
        let result = agent.turn(item, &mut tui.ui(cwd.clone()), cancel.clone()).await;
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
    tools::background::kill_all();
    mcp.shutdown().await;
    Ok(())
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
        sandbox::Mode::ReadOnly => format!("read-only{}", if policy.network { " + net" } else { "" }),
        sandbox::Mode::WorkspaceWrite => format!("sandboxed{}", if policy.network { " + net" } else { "" }),
    }
}

/// Switch provider and model, keeping the conversation.
fn switch_model(home: &std::path::Path, agent: &mut Agent, choice: &models::ModelChoice) {
    let effort = agent.provider.effort().map(str::to_string).filter(|e| choice.efforts.contains(e));
    match models::build(home, choice, effort.clone()) {
        Ok(provider) => {
            agent.provider = provider;
            agent.set_context_window(config::context_window_for(&choice.provider, choice.context_window));
            if let Err(e) = models::save_last(home, choice, effort.as_deref()) {
                ui::error(&format!("could not remember the model: {e:#}"));
            }
            ui::info(&format!("  Switched to {} · {}", choice.provider, choice.model));
        }
        Err(e) => ui::error(&format!("{e:#}")),
    }
}

/// Shift+Tab: auto → each supported effort → auto.
fn cycle_effort(home: &std::path::Path, agent: &mut Agent, current: Option<&models::ModelChoice>) {
    let efforts: Vec<String> = match current {
        Some(c) if c.provider == agent.provider.name() && c.model == agent.provider.model() => c.efforts.clone(),
        _ => agent.provider.efforts(),
    };
    if efforts.is_empty() {
        ui::info("  This model has no adjustable reasoning effort.");
        return;
    }
    let next = match agent.provider.effort() {
        None => Some(efforts[0].clone()),
        Some(e) => efforts.iter().position(|x| x == e).and_then(|i| efforts.get(i + 1)).cloned(),
    };
    agent.provider.set_effort(next.clone());
    if let Some(c) = current {
        let _ = models::save_last(home, c, next.as_deref());
    }
}

fn sync_footer(tui: &Tui, agent: &Agent) {
    let mut s = tui.screen();
    s.footer.provider = agent.provider.name().to_string();
    s.footer.model = agent.provider.model().to_string();
    s.footer.effort = agent.provider.effort().map(str::to_string);
    s.footer.yolo = agent.yolo;
    s.footer.usage = agent.usage;
    s.footer.context_tokens = agent.context_tokens;
    s.footer.context_window = agent.context_window;
    s.redraw();
}

/// The requested sandbox, or by default workspace-write when this system
/// supports it (falling back to off with a warning when it doesn't).
fn sandbox_policy(cli: &Cli) -> Result<sandbox::Policy> {
    let mut policy = sandbox::Policy {
        mode: cli.sandbox.unwrap_or_default(),
        network: cli.allow_network,
        writable: Vec::new(),
    };
    if let Err(e) = sandbox::check(&policy) {
        if cli.sandbox.is_some() {
            anyhow::bail!("sandbox unavailable: {e} (use --sandbox off to run commands unconfined)");
        }
        ui::error(&format!("sandbox unavailable ({e}); shell commands will run unconfined"));
        policy.mode = sandbox::Mode::Off;
    }
    Ok(policy)
}

/// A token that fires on the next Ctrl-C. Cancel it yourself to stop listening.
fn cancel_on_ctrl_c() -> CancellationToken {
    let token = CancellationToken::new();
    let t = token.clone();
    tokio::spawn(async move {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => t.cancel(),
            _ = t.cancelled() => {}
        }
    });
    token
}

async fn connect_profile_servers(mcp: &mut McpManager, agent: &mut Agent, servers: &[ServerConfig], quiet: bool) {
    for cfg in servers.iter().filter(|s| s.scope == Scope::Profile) {
        connect(mcp, agent, cfg, quiet).await;
    }
    if !quiet {
        let project: Vec<_> = servers.iter().filter(|s| s.scope == Scope::Project).map(|s| s.name.as_str()).collect();
        if !project.is_empty() {
            ui::info(&format!(
                "Project MCP servers in .mcp.json: {} (connect with /mcp connect NAME)",
                project.join(", ")
            ));
        }
    }
}

fn drop_server_tools(agent: &mut Agent, mcp: &McpManager, name: &str) {
    let names = mcp.registered_names(name);
    agent.tools.remove_names(&names);
    agent.tools.remove_prefix(&mcp::tool_prefix(name));
}

fn install_mcp_tools(agent: &mut Agent, name: &str, tools: Vec<Box<dyn tools::Tool>>) -> Result<()> {
    let mut added = Vec::new();
    for tool in tools {
        let n = tool.spec().name.clone();
        if let Err(e) = agent.tools.try_insert(tool) {
            agent.tools.remove_names(&added);
            agent.tools.remove_prefix(&mcp::tool_prefix(name));
            anyhow::bail!("MCP {name}: {e}");
        }
        added.push(n);
    }
    Ok(())
}

async fn connect(mcp: &mut McpManager, agent: &mut Agent, cfg: &ServerConfig, quiet: bool) {
    drop_server_tools(agent, mcp, &cfg.name);
    match mcp.connect(cfg).await {
        Ok(tools) => {
            let n = tools.len();
            if let Err(e) = install_mcp_tools(agent, &cfg.name, tools) {
                ui::error(&format!("{e:#}"));
                let _ = mcp.disconnect(&cfg.name).await;
            } else if !quiet {
                ui::info(&format!("MCP {}: {n} tools", cfg.name));
            }
        }
        Err(e) => ui::error(&format!("MCP {}: {e:#}", cfg.name)),
    }
}

/// `!cmd`: run a command yourself (unsandboxed) and show its output.
async fn shell(cmd: &str, cwd: &Path, tui: &Tui) {
    let cancel = CancellationToken::new();
    tui.screen().set_busy(Some("Running"), Some(cancel.clone()));
    let policy = sandbox::Policy { mode: sandbox::Mode::Off, ..Default::default() };
    let output = tools::run_shell(cmd, cwd, &policy, &cancel, None).await;
    let mut screen = tui.screen();
    screen.idle();
    screen.print_tool("bash", cmd, Some(&output));
}

fn session_lines(dir: &std::path::Path) -> Result<Vec<String>> {
    let all = session::list(dir)?;
    if all.is_empty() {
        return Ok(vec!["No saved conversations.".into()]);
    }
    Ok(all
        .iter()
        .map(|m| format!("{}  {}  {}  {}", session_label(&m.id), m.created.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M"), m.cwd.display(), m.title))
        .collect())
}

/// Switch to a saved session (`id`, id prefix or `last`), replaying it.
fn resume_session(agent: &mut Agent, sessions_dir: &std::path::Path, which: &str) -> Result<()> {
    let (s, meta, items) = session::load(sessions_dir, which, &agent.cwd)?;
    agent.reset(Some(s));
    ui::replay(&items);
    agent.items = items;
    agent.adopt_transcript();
    ui::info(&format!("Resumed {} · {}", meta.id, meta.title));
    Ok(())
}

/// Open the session picker (this folder's sessions first).
fn open_sessions(tui: &Tui, sessions_dir: &std::path::Path, agent: &Agent) -> Result<()> {
    let all = session::list(sessions_dir)?;
    let current = agent.session.as_ref().map(|s| s.id.as_str());
    tui.screen().open_sessions(all, &agent.cwd, current);
    Ok(())
}

fn list_sessions(dir: &std::path::Path) -> Result<()> {
    for l in session_lines(dir)? {
        println!("{l}");
    }
    Ok(())
}

/// Handle a slash command. Returns Ok(false) to quit.
async fn slash(
    line: &str,
    agent: &mut Agent,
    mcp: &mut McpManager,
    servers: &[ServerConfig],
    sessions_dir: &std::path::Path,
    tui: &Tui,
    skills: &[skills::Skill],
) -> Result<bool> {
    let mut parts = line.split_whitespace();
    let cmd = parts.next().unwrap_or("");
    let arg: Vec<&str> = parts.collect();
    match cmd {
        "/exit" | "/quit" | "/q" => return Ok(false),
        "/help" | "/?" => {
            for (c, d) in COMMANDS {
                ui::info(&format!("  {c:<10} {d}"));
            }
            for (c, d) in mcp_features::prompt_commands(mcp) {
                ui::info(&format!("  {c} — {d}"));
            }
            ui::info("  !CMD       run a shell command yourself (not sandboxed)\n  Enter while working steers · Alt+Enter queues a follow-up · Alt+Up edits the queue · Esc interrupts");
        }
        "/new" | "/clear" => {
            agent.reset(Some(Session::create(sessions_dir, &agent.cwd, agent.provider.model())?));
            tools::memory::refresh_section(&mut agent.system, &config::home());
            ui::info("Started a new conversation.");
        }
        "/sessions" => ui::list(&session_lines(sessions_dir)?),
        "/skills" => ui::list(&skills::list_lines(skills)),
        "/memory" => tools::memory::slash(&config::home(), &arg, &mut agent.system),
        "/background" => tools::background::slash(&arg).await,
        "/undo" if agent.undo.is_empty() => ui::info("  Nothing to undo."),
        "/undo" => match agent.undo.undo() {
            Ok(msg) => {
                ui::info(&format!("  {msg}"));
                // Tell the model, so it doesn't assume its edit is still there.
                agent.push(types::Item::user(format!("[The user undid your last file change: {msg}]")));
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
                        tokio::time::timeout(std::time::Duration::from_secs(3), tokio::task::spawn_blocking(move || ui::tui::copy_native(&text))).await
                    };
                    let via = match native {
                        Ok(Ok(Some(tool))) => tool,
                        _ => "terminal (OSC 52)",
                    };
                    ui::info(&format!("  Copied the last answer ({} chars) via {via}.", text.chars().count()));
                }
                None => ui::info("  Nothing to copy yet."),
            }
        }
        "/compact" => {
            let cancel = CancellationToken::new();
            tui.screen().set_busy(Some("Compacting"), Some(cancel.clone()));
            let result = agent.compact(&mut tui.ui(agent.cwd.clone()), &cancel).await;
            tui.screen().idle();
            if !result? {
                ui::info("Interrupted.");
            }
        }
        "/resume" => match arg.first() {
            Some(which) => resume_session(agent, sessions_dir, which)?,
            None => open_sessions(tui, sessions_dir, agent)?,
        },
        "/model" => match arg.first() {
            Some(m) => {
                agent.provider.set_model(m.to_string());
                agent.suppress_auto_compact = false;
                ui::info(&format!("Model set to {m}."));
            }
            None => ui::info(&format!("{} · {}", agent.provider.name(), agent.provider.model())),
        },
        "/tools" => {
            let lines: Vec<String> = agent
                .tools
                .specs()
                .iter()
                .map(|t| format!("{:<24} {}", t.name, t.description.lines().next().unwrap_or("")))
                .collect();
            ui::list(&lines);
        }
        "/mcp" => match (arg.first().copied(), arg.get(1).copied()) {
            (Some("connect"), Some(name)) => {
                let cfg = servers.iter().find(|s| s.name == name).with_context(|| format!("no MCP server named `{name}`"))?;
                connect(mcp, agent, cfg, false).await;
            }
            (Some("login"), Some(name)) => {
                let cfg = servers.iter().find(|s| s.name == name).with_context(|| format!("no MCP server named `{name}`"))?;
                drop_server_tools(agent, mcp, name);
                let cancel = CancellationToken::new();
                tui.screen().set_busy(Some("Signing in"), Some(cancel.clone()));
                let result = mcp
                    .login(cfg, Some(&cancel), |url| {
                        ui::info(&format!("Sign in to {name} in your browser (waiting up to 5 minutes):\n  {url}"));
                        mcp::open_browser(url);
                    })
                    .await;
                tui.screen().idle();
                let tools = result?;
                let n = tools.len();
                install_mcp_tools(agent, name, tools)?;
                ui::info(&format!("MCP {name}: signed in · {n} tools"));
            }
            (Some("logout"), Some(name)) => {
                let cfg = servers.iter().find(|s| s.name == name).with_context(|| format!("no MCP server named `{name}`"))?;
                let endpoint = mcp::canonical_endpoint(&cfg.transport)?;
                drop_server_tools(agent, mcp, name);
                let existed = mcp.logout(name, &endpoint).await?;
                ui::info(&if existed { format!("Signed out of {name}.") } else { format!("No saved sign-in for {name}.") });
            }
            (Some("disconnect"), Some(name)) => {
                drop_server_tools(agent, mcp, name);
                mcp.disconnect(name).await?;
                ui::info(&format!("Disconnected {name}."));
            }
            _ => {
                let connected = mcp.connected();
                if servers.is_empty() {
                    ui::info("No MCP servers configured (add them to mcp.json or .mcp.json).");
                }
                for s in servers {
                    let state = match connected.iter().find(|(n, _)| n == &s.name) {
                        Some((_, n)) => {
                            let (action, specific) = agent.permissions.server_default(&mcp::tool_prefix(&s.name));
                            let more = if specific { " + tool rules" } else { "" };
                            format!("connected · {n} tools · {}{more}", action.as_str())
                        }
                        None => "not connected".into(),
                    };
                    let scope = if s.scope == Scope::Project { "project" } else { "profile" };
                    ui::info(&format!("  {:<20} {scope:<8} {state}", s.name));
                }
            }
        },
        "/permissions" => match arg.first().copied() {
            Some("reload") => {
                agent.permissions = std::sync::Arc::new(load_permissions(&config::home(), &agent.cwd));
                ui::info("Reloaded permission rules.");
                ui::list(&agent.permissions.lines(&agent.allowed));
            }
            None => ui::list(&agent.permissions.lines(&agent.allowed)),
            Some(other) => ui::error(&format!("unknown /permissions argument `{other}` (try /permissions or /permissions reload)")),
        },
        "/yolo" => {
            agent.yolo = !agent.yolo;
            ui::info(if agent.yolo { "Auto-approve on: tools run without asking." } else { "Auto-approve off." });
        }
        "/usage" => {
            let u = agent.usage;
            ui::info(&format!("input {} (cached {}) · output {}", u.input_tokens, u.cached_tokens, u.output_tokens));
        }
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

    #[test]
    fn resume_flag_value_is_optional() {
        let parse = |args: &[&str]| Cli::try_parse_from(args).unwrap();
        assert_eq!(parse(&["nyq", "-r"]).resume.as_deref(), Some(""));
        assert_eq!(parse(&["nyq", "--resume"]).resume.as_deref(), Some(""));
        assert_eq!(parse(&["nyq", "-r", "abc"]).resume.as_deref(), Some("abc"));
        assert_eq!(parse(&["nyq", "--resume=abc", "hello"]).prompt, ["hello"]);
        assert_eq!(parse(&["nyq", "hi"]).resume, None);
        assert!(parse(&["nyq", "-c"]).continue_last);
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
