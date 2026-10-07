mod agent;
mod config;
mod mcp;
mod prompt;
mod provider;
mod sandbox;
mod session;
mod tools;
mod types;
mod ui;

use std::collections::HashSet;
use std::path::PathBuf;

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
    #[arg(short, long)]
    model: Option<String>,
    /// Provider: codex or openai.
    #[arg(long)]
    provider: Option<String>,
    /// Run tools without asking for approval.
    #[arg(long)]
    yolo: bool,
    /// Confine shell commands the model runs [default: workspace-write where supported].
    #[arg(long, value_enum)]
    sandbox: Option<sandbox::Mode>,
    /// Allow network access from sandboxed shell commands.
    #[arg(long)]
    allow_network: bool,
    /// Continue the most recent conversation in this directory.
    #[arg(short = 'c', long = "continue")]
    continue_last: bool,
    /// Resume a saved conversation by id (or id prefix).
    #[arg(short, long, value_name = "ID")]
    resume: Option<String>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// List saved conversations.
    Sessions,
}

const COMMANDS: &[(&str, &str)] = &[
    ("/help", "show commands"),
    ("/new", "start a fresh conversation"),
    ("/resume", "resume a saved conversation: /resume [id]"),
    ("/sessions", "list saved conversations"),
    ("/model", "show or switch model: /model [name]"),
    ("/compact", "summarize the conversation to free up context"),
    ("/tools", "list available tools"),
    ("/mcp", "MCP servers: /mcp [connect|disconnect|login|logout NAME]"),
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
    let sessions_dir = home.join("nyq").join("sessions");
    let cwd = std::env::current_dir().context("reading current directory")?;

    if let Some(Command::Sessions) = cli.command {
        return list_sessions(&sessions_dir);
    }

    let provider = config::provider(&home, cli.provider.as_deref(), cli.model.as_deref())?;
    let mut agent = Agent {
        session: Some(Session::create(&sessions_dir, &cwd, provider.model())?),
        provider,
        tools: Registry::builtin(),
        system: prompt::system(&cwd),
        items: Vec::new(),
        cwd: cwd.clone(),
        yolo: cli.yolo,
        allowed: HashSet::new(),
        usage: Usage::default(),
        context_window: 0,
        context_tokens: 0,
        sandbox: std::sync::Arc::new(sandbox_policy(&cli)?),
    };
    agent.context_window = config::context_window(agent.provider.name());

    let resume = cli.resume.clone().or_else(|| cli.continue_last.then(|| "last".to_string()));
    let mut resumed = None;
    if let Some(which) = &resume {
        let (s, meta, items) = session::load(&sessions_dir, which, &cwd)?;
        agent.session = Some(s);
        agent.items = items;
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
        if text.trim().is_empty() {
            use std::io::Read;
            std::io::stdin().read_to_string(&mut text)?;
        }
        if let Some(meta) = &resumed {
            ui::info(&format!("Resumed {} · {}", meta.id, meta.title));
        }
        connect_profile_servers(&mut mcp, &mut agent, &servers, true).await;
        let mut plain = ui::PlainUi::new(cli.yolo);
        let cancel = cancel_on_ctrl_c();
        let (item, _) = tools::image::user_message(&text, &cwd)?;
        let r = agent.turn(item, &mut plain, cancel).await;
        mcp.shutdown().await;
        return r.map(|_| ());
    }

    let footer = Footer { cwd: cwd.display().to_string(), sandbox: sandbox_label(&agent.sandbox), ..Footer::default() };
    let commands = COMMANDS.iter().map(|(c, d)| (c.to_string(), d.to_string())).collect();
    let mut tui = Tui::start(footer, home.join("nyq").join("history"), commands)?;
    sync_footer(&tui, &agent);
    ui::banner(env!("CARGO_PKG_VERSION"));
    ui::info(&sandbox::describe(&agent.sandbox));
    if let Some(meta) = &resumed {
        ui::replay(&agent.items);
        ui::info(&format!("Resumed {} · {}", meta.id, meta.title));
    }
    connect_profile_servers(&mut mcp, &mut agent, &servers, false).await;
    let mut pending = (!text.trim().is_empty()).then_some(text);

    loop {
        sync_footer(&tui, &agent);
        let line = match pending.take() {
            Some(t) => t,
            None => match tui.commands.recv().await {
                Some(UiCommand::Submit(t)) => t,
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
        if line.starts_with('/') {
            match slash(&line, &mut agent, &mut mcp, &servers, &sessions_dir, &tui).await {
                Ok(true) => continue,
                Ok(false) => break,
                Err(e) => {
                    ui::error(&format!("{e:#}"));
                    continue;
                }
            }
        }
        let item = match tools::image::user_message(&line, &cwd) {
            Ok((item, attached)) => {
                tui.screen().print_user(&line);
                if !attached.is_empty() {
                    ui::info(&format!("Attached {}", attached.join(", ")));
                }
                item
            }
            Err(e) => {
                ui::error(&format!("{e:#}"));
                continue;
            }
        };
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
        // Messages queued just as the run finished start the next one.
        if !leftover.is_empty() {
            pending = Some(leftover.join("\n\n"));
        }
    }
    drop(tui);
    mcp.shutdown().await;
    Ok(())
}

fn sandbox_label(policy: &sandbox::Policy) -> String {
    match policy.mode {
        sandbox::Mode::Off => "no sandbox".into(),
        sandbox::Mode::ReadOnly => format!("read-only{}", if policy.network { " + net" } else { "" }),
        sandbox::Mode::WorkspaceWrite => format!("sandboxed{}", if policy.network { " + net" } else { "" }),
    }
}

fn sync_footer(tui: &Tui, agent: &Agent) {
    let mut s = tui.screen();
    s.footer.provider = agent.provider.name().to_string();
    s.footer.model = agent.provider.model().to_string();
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

async fn connect(mcp: &mut McpManager, agent: &mut Agent, cfg: &ServerConfig, quiet: bool) {
    match mcp.connect(cfg).await {
        Ok(tools) => {
            if !quiet {
                ui::info(&format!("MCP {}: {} tools", cfg.name, tools.len()));
            }
            for t in tools {
                agent.tools.add(t);
            }
        }
        Err(e) => ui::error(&format!("MCP {}: {e:#}", cfg.name)),
    }
}

/// `!cmd`: run a command yourself (unsandboxed) and show its output.
async fn shell(cmd: &str, cwd: &PathBuf, tui: &Tui) {
    let started = std::time::Instant::now();
    tui.screen().print_tool_call("bash", cmd);
    let out = tokio::process::Command::new("sh").arg("-c").arg(cmd).current_dir(cwd).stdin(std::process::Stdio::null()).output().await;
    let output = match out {
        Ok(o) => {
            let mut text = String::from_utf8_lossy(&o.stdout).into_owned();
            text.push_str(&String::from_utf8_lossy(&o.stderr));
            let ok = o.status.success();
            if !ok {
                text.push_str(&format!("\nexit code {}", o.status.code().unwrap_or(-1)));
            }
            tools::ToolOutput { content: if text.trim().is_empty() { "(no output)".into() } else { text }, is_error: !ok, ..Default::default() }
        }
        Err(e) => tools::ToolOutput::err(e.to_string()),
    };
    tui.screen().print_tool_output(&output, started);
}

fn session_lines(dir: &std::path::Path) -> Result<Vec<String>> {
    let all = session::list(dir)?;
    if all.is_empty() {
        return Ok(vec!["No saved conversations.".into()]);
    }
    Ok(all
        .iter()
        .map(|m| format!("{}  {}  {}  {}", &m.id[..8.min(m.id.len())], m.created.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M"), m.cwd.display(), m.title))
        .collect())
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
            ui::info("  !CMD       run a shell command yourself (not sandboxed)\n  Enter while working steers · Alt+Enter queues a follow-up · Alt+Up edits the queue · Esc interrupts");
        }
        "/new" | "/clear" => {
            agent.reset(Some(Session::create(sessions_dir, &agent.cwd, agent.provider.model())?));
            ui::info("Started a new conversation.");
        }
        "/sessions" => ui::info(&session_lines(sessions_dir)?.join("\n")),
        "/compact" => {
            let cancel = CancellationToken::new();
            tui.screen().set_busy(Some("Compacting"), Some(cancel.clone()));
            let result = agent.compact(&mut tui.ui(agent.cwd.clone()), &cancel).await;
            tui.screen().set_busy(None, None);
            if !result? {
                ui::info("Interrupted.");
            }
        }
        "/resume" => {
            let which = arg.first().copied().unwrap_or("last");
            let (s, meta, items) = session::load(sessions_dir, which, &agent.cwd)?;
            agent.reset(Some(s));
            ui::replay(&items);
            agent.items = items;
            ui::info(&format!("Resumed {} · {}", meta.id, meta.title));
        }
        "/model" => match arg.first() {
            Some(m) => {
                agent.provider.set_model(m.to_string());
                ui::info(&format!("Model set to {m}."));
            }
            None => ui::info(&format!("{} · {}", agent.provider.name(), agent.provider.model())),
        },
        "/tools" => {
            for spec in agent.tools.specs() {
                let first = spec.description.lines().next().unwrap_or("");
                ui::info(&format!("  {:<28} {first}", spec.name));
            }
        }
        "/mcp" => match (arg.first().copied(), arg.get(1).copied()) {
            (Some("connect"), Some(name)) => {
                let cfg = servers.iter().find(|s| s.name == name).with_context(|| format!("no MCP server named `{name}`"))?;
                connect(mcp, agent, cfg, false).await;
            }
            (Some("login"), Some(name)) => {
                let cfg = servers.iter().find(|s| s.name == name).with_context(|| format!("no MCP server named `{name}`"))?;
                let tools = mcp
                    .login(cfg, |url| {
                        ui::info(&format!("Sign in to {name} in your browser (waiting up to 5 minutes):\n  {url}"));
                        mcp::open_browser(url);
                    })
                    .await?;
                agent.tools.remove_prefix(&format!("mcp__{name}__"));
                ui::info(&format!("MCP {name}: signed in · {} tools", tools.len()));
                for t in tools {
                    agent.tools.add(t);
                }
            }
            (Some("logout"), Some(name)) => {
                let existed = mcp.logout(name).await?;
                agent.tools.remove_prefix(&format!("mcp__{name}__"));
                ui::info(&if existed { format!("Signed out of {name}.") } else { format!("No saved sign-in for {name}.") });
            }
            (Some("disconnect"), Some(name)) => {
                mcp.disconnect(name).await?;
                agent.tools.remove_prefix(&format!("mcp__{name}__"));
                ui::info(&format!("Disconnected {name}."));
            }
            _ => {
                let connected = mcp.connected();
                if servers.is_empty() {
                    ui::info("No MCP servers configured (add them to mcp.json or .mcp.json).");
                }
                for s in servers {
                    let state = match connected.iter().find(|(n, _)| n == &s.name) {
                        Some((_, n)) => format!("connected · {n} tools"),
                        None => "not connected".into(),
                    };
                    let scope = if s.scope == Scope::Project { "project" } else { "profile" };
                    ui::info(&format!("  {:<20} {scope:<8} {state}", s.name));
                }
            }
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
