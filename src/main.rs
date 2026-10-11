mod account;
mod acp;
mod agent;
mod auto_upgrade;
mod catalog;
mod classifier;
mod command_effect;
mod commands;
mod config;
mod diagnostics;
mod doctor;
mod github;
mod grants;
mod hooks;
mod limits;
mod login;
mod mcp;
mod mcp_cli;
mod mcp_features;
mod mcp_slash;
mod models;
mod notify;
mod permissions;
mod pricing;
mod prompt;
mod provider;
mod sandbox;
mod session;
mod record_tape;
mod settings;
mod skill_install;
mod skill_invoke;
mod skills;
mod slash;
mod subagents;
mod templates;
mod trace;
mod tools;
mod types;
mod ui;
mod undo;
mod upgrade;
mod usage;
mod workspace;

use std::collections::HashSet;
use std::path::Path;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use tokio_util::sync::CancellationToken;

use agent::{Agent, TurnEnd};
use mcp::{McpManager, ServerConfig};
use session::Session;
use tools::Registry;
use types::Usage;
use ui::tui::{Command as UiCommand, Footer, Tui};

#[derive(Parser)]
#[command(
    name = "nyq",
    version,
    disable_version_flag = true,
    about = "A small, friendly coding agent for your terminal"
)]
struct Cli {
    /// Start the conversation with this message.
    prompt: Vec<String>,
    /// Print version.
    #[arg(short = 'v', short_alias = 'V', long, action = clap::ArgAction::Version)]
    version: Option<bool>,
    /// Answer the prompt non-interactively and exit (reads stdin if no prompt).
    #[arg(short, long)]
    print: bool,
    #[command(flatten)]
    ask: AskOpts,
    /// Record interactive terminal output under <home>/tapes/<session>.cast.
    #[arg(long)]
    record: bool,
    /// Model id (overrides config).
    #[arg(short, long, global = true)]
    model: Option<String>,
    /// Provider: codex, openai, ollama, opencode-go, opencode, xai or vercel.
    #[arg(long, global = true)]
    provider: Option<String>,
    /// Run tools without asking for approval.
    #[arg(long, global = true)]
    yolo: bool,
    /// Permission mode: ask (default), auto, or yolo. Overrides --yolo and settings.
    #[arg(long, global = true, value_name = "MODE")]
    permission_mode: Option<String>,
    /// Confine shell commands the model runs [default: workspace-write where supported].
    #[arg(long, value_enum, global = true)]
    sandbox: Option<sandbox::Mode>,
    /// Allow network access from sandboxed shell commands.
    #[arg(long, global = true)]
    allow_network: bool,
    /// Add a workspace directory for this run. Repeatable.
    #[arg(long = "add-dir", value_name = "PATH", global = true)]
    add_dir: Vec<std::path::PathBuf>,
    /// Ignore additional directories saved for this workspace.
    #[arg(long = "no-additional-dirs", global = true)]
    no_additional_dirs: bool,
    /// Continue the most recent conversation in this directory.
    #[arg(short = 'c', long = "continue", global = true)]
    continue_last: bool,
    /// Resume a saved conversation by id (or id prefix); without an id, pick one.
    #[arg(short, long, value_name = "ID", num_args = 0..=1, default_missing_value = "", global = true)]
    resume: Option<String>,
    /// Override a prompt budget for this run: NAME=BYTES or NAME=off. Repeatable.
    #[arg(long = "context-limit", value_name = "NAME=BYTES", global = true)]
    context_limit: Vec<String>,
    #[command(subcommand)]
    command: Option<Command>,
}

/// Options for one non-interactive answer (`nyq ask`, `nyq -p`).
#[derive(clap::Args, Clone, Default)]
struct AskOpts {
    /// Print one JSON object with the answer, tool calls and exit code (implies -p).
    #[arg(long)]
    json: bool,
    /// Don't print the assistant's text; with --json only the JSON is printed (implies -p).
    #[arg(long)]
    quiet: bool,
    /// Replace the built-in base system prompt for this run.
    #[arg(long, value_name = "TEXT")]
    system: Option<String>,
    /// Don't create or save a session (implies -p; not with --resume or --continue).
    #[arg(long)]
    no_save: bool,
    /// Ask y/N on the terminal for permission requests instead of denying them.
    #[arg(long)]
    prompt_permissions: bool,
    /// Attach an image to the prompt. Repeatable.
    #[arg(long, value_name = "PATH")]
    image: Vec<std::path::PathBuf>,
    /// Continue the interrupted or cut-off response of the resumed session (with -r ID or -c).
    #[arg(long)]
    continue_recovery: bool,
}

impl AskOpts {
    /// Options that only make sense without the interactive terminal.
    fn print_only(&self) -> bool {
        self.json || self.quiet || self.no_save || self.continue_recovery
    }
}

#[derive(Subcommand)]
enum Command {
    /// Play an asciicast v2 terminal tape.
    Replay { tape: std::path::PathBuf, #[arg(long, default_value_t = 1.0)] speed: f64 },
    /// List saved conversations from this directory (newest first).
    Sessions {
        /// Include every directory.
        #[arg(long)]
        all: bool,
        /// Show at most this many (1-100).
        #[arg(long, value_parser = clap::value_parser!(u16).range(1..=100))]
        limit: Option<u16>,
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// Resume a saved conversation (same as -r): an id, a unique prefix, or `last`.
    Resume { which: Option<String> },
    /// Answer one prompt and exit (same as -p). Reads stdin when no prompt is given.
    Ask {
        /// The request. When omitted, nyq reads stdin.
        prompt: Vec<String>,
        #[command(flatten)]
        opts: AskOpts,
    },
    /// Inspect, resume, or delete a saved conversation (`nyq session ID` shows one).
    Session {
        #[command(subcommand)]
        action: Option<SessionCommand>,
    },
    /// List, add, or remove additional workspace directories.
    Workspace {
        #[command(subcommand)]
        action: Option<WorkspaceCommand>,
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// Summarize recorded usage (default: 30 days).
    Usage {
        #[arg(long, value_enum, default_value = "30d")]
        period: usage::Period,
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// Show configuration and runtime status without contacting providers.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Print the permission mode and persistent rules.
    Permissions {
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// Check authentication, endpoint, sandbox and local environment health.
    Doctor {
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// List the same provider model catalog as the interactive picker.
    Models {
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// Sign in: ChatGPT subscription (default, Codex models), or store an API
    /// key for opencode-go, opencode, xai or vercel.
    Login {
        /// codex (default), opencode-go, opencode, xai or vercel.
        provider: Option<String>,
    },
    /// Remove nyq's stored login for a provider, or every stored login.
    Logout {
        /// codex, opencode-go, opencode, xai or vercel [default: all of them].
        provider: Option<String>,
    },
    /// Run as an Agent Client Protocol (ACP) agent over stdio, for editors like Zed.
    Acp {
        /// Append request and error logs to this file (stdout carries the protocol).
        #[arg(long, value_name = "PATH")]
        log_file: Option<std::path::PathBuf>,
    },
    /// Draft a pull request for the current branch with the model; publish it with gh.
    Pr {
        /// Publish without asking (the branch is still pushed only after confirmation).
        #[arg(long, alias = "create")]
        publish: bool,
        /// Open the pull request as a draft.
        #[arg(long)]
        draft: bool,
        /// Base branch [default: the repository's default branch].
        #[arg(long, value_name = "BRANCH")]
        base: Option<String>,
        /// Extra context for the draft.
        context: Vec<String>,
    },
    /// Draft a GitHub issue from the context and repository state; publish it with gh.
    Issue {
        /// Publish without asking.
        #[arg(long, alias = "create")]
        publish: bool,
        /// What the issue is about.
        context: Vec<String>,
    },
    /// Upgrade nyq to the newest release (verifies SHA256SUMS).
    Upgrade {
        /// Release channel; remembered for later upgrades [default: stable].
        #[arg(long, value_enum)]
        channel: Option<upgrade::Channel>,
        /// Only report whether an update is available.
        #[arg(long)]
        check: bool,
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// Choose the default provider (codex, opencode-go, opencode, xai, vercel, openai, ollama).
    Provider { name: Option<String> },
    /// Show sign-in status per provider and the default; on a terminal, set them up.
    Setup,
    /// Choose the Vercel team used by AI Gateway (not available with API keys).
    Teams,
    /// Show the AI Gateway credit balance.
    #[command(alias = "balance")]
    Credits { #[arg(long)] json: bool },
    /// Manage MCP servers and project trust without opening the interactive shell.
    Mcp {
        #[command(subcommand)]
        action: mcp_cli::McpCommand,
    },
}

#[derive(Subcommand)]
enum SessionCommand {
    /// Copy readable items to a new session, skipping broken lines.
    Recover {
        which: String,
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// Add the current format version to an older session.
    Migrate {
        which: String,
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// Print a transcript summary.
    Show {
        /// Session id, a unique prefix, or `last`.
        #[arg(default_value = "last")]
        which: String,
        /// Print the metadata and stored items as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Resume a saved conversation (same as -r).
    Resume {
        /// Session id, a unique prefix, or `last`. Omit to pick one.
        which: Option<String>,
    },
    /// Delete a saved conversation.
    Delete {
        /// Session id, a unique prefix, or `last`.
        which: String,
    },
    /// `nyq session ID`: show that conversation.
    #[command(external_subcommand)]
    Other(Vec<String>),
}

#[derive(Subcommand)]
enum WorkspaceCommand {
    /// List the primary directory and additional directories.
    List,
    /// Persist an existing directory for this workspace.
    Add { path: std::path::PathBuf },
    /// Remove a saved or launch-only additional directory.
    Remove { path: std::path::PathBuf },
    /// Remove every saved additional directory for this workspace.
    Clear,
}

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
        "switch model or provider (ctrl+l); /model ID|QUERY [effort] [fast|normal]",
    ),
    ("/compact", "summarize the conversation to free up context"),
    ("/copy", "copy the last answer to the clipboard"),
    ("/undo", "revert the last file change nyq made"),
    ("/tools", "list available tools"),
    ("/skills", "skills: /skills [list|install SOURCE [--project]|remove NAME|info NAME|create NAME|path]"),
    ("/agents", "named subagents: /agents [list|show NAME|stop NAME|delete NAME]"),
    ("/login", "sign in: ChatGPT in the browser, or an API key (hidden input)"),
    ("/logout", "remove nyq's stored sign-in: /logout [provider]"),
    ("/setup", "connections, default provider, credential source, AI Gateway"),
    ("/credits", "AI Gateway credit balance (also /balance)"),
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

fn main() {
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
            println!("{}", ask_json(None, 1, None, None, 0, Some(format!("{e:#}"))));
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
    let cli = Cli::parse();
    let ask = match &cli.command {
        Some(Command::Ask { opts, .. }) => opts.clone(),
        _ => cli.ask.clone(),
    };
    JSON_ERRORS.store(ask.json, std::sync::atomic::Ordering::Relaxed);
    trace::init_from_env();
    let home = config::home();
    let sessions_dir = home.join("sessions");
    let cwd = std::env::current_dir().context("reading current directory")?;
    let (user_settings, settings_warnings) = settings::load_layered(&home, &cwd);
    settings::install(user_settings);
    for w in settings_warnings {
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

    match &cli.command {
        Some(Command::Replay { tape, speed }) => return record_tape::replay(tape, *speed).await,
        Some(Command::Session { action: Some(SessionCommand::Recover { which, json }) }) => {
            let (id, kept, skipped) = session::recover(&sessions_dir, which, &cwd)?;
            if *json {
                println!("{}", serde_json::json!({"id": id, "kept": kept, "skipped": skipped}));
            } else {
                println!("Recovered {id}: {kept} items, {skipped} broken lines skipped");
            }
            return Ok(());
        }
        Some(Command::Session { action: Some(SessionCommand::Migrate { which, json }) }) => {
            let changed = session::migrate(&sessions_dir, which, &cwd)?;
            if *json {
                println!("{}", serde_json::json!({"migrated": changed}));
            } else {
                println!("{}", if changed { "Session migrated" } else { "Session already current" });
            }
            return Ok(());
        }
        Some(Command::Usage { period, json }) => return usage::show(&home, *period, *json),
        Some(Command::Models { json }) => return doctor::models(&home, *json).await,
        Some(Command::Status { json }) => {
            let provider =
                config::provider(&home, cli.provider.as_deref(), cli.model.as_deref()).ok();
            let policy = sandbox::Policy {
                mode: cli.sandbox.unwrap_or_default(),
                network: cli.allow_network,
                writable: extra_dirs(&home, &cwd, &cli),
            };
            return doctor::cli_status(&home, &cwd, provider.as_deref(), &policy, *json);
        }
        Some(Command::Doctor { json }) => {
            let provider =
                config::provider(&home, cli.provider.as_deref(), cli.model.as_deref()).ok();
            let policy = sandbox::Policy {
                mode: cli.sandbox.unwrap_or_default(),
                network: cli.allow_network,
                writable: extra_dirs(&home, &cwd, &cli),
            };
            let facts = doctor::Facts { permission_mode, sessions_dir: &sessions_dir };
            return doctor::doctor(&home, &cwd, provider.as_deref(), &policy, &facts, *json).await;
        }
        Some(Command::Session { action: None }) => {
            return session::print_summary(&sessions_dir, "last", &cwd, false);
        }
        Some(Command::Session { action: Some(SessionCommand::Show { which, json }) }) => {
            return session::print_summary(&sessions_dir, which, &cwd, *json);
        }
        Some(Command::Session { action: Some(SessionCommand::Other(args)) }) => {
            let (which, json) = match args.as_slice() {
                [which] => (which, false),
                [which, flag] if flag == "--json" => (which, true),
                _ => bail!("usage: nyq session <ID|last> [--json] (see nyq session --help)"),
            };
            return session::print_summary(&sessions_dir, which, &cwd, json);
        }
        Some(Command::Session { action: Some(SessionCommand::Delete { which }) }) => {
            let meta = session::delete(&sessions_dir, which, &cwd)?;
            println!("Deleted {} ({})", meta.id, meta.title);
            return Ok(());
        }
        Some(Command::Workspace { action, json }) => {
            let request = match action {
                None | Some(WorkspaceCommand::List) => workspace::CliRequest::List,
                Some(WorkspaceCommand::Add { path }) => workspace::CliRequest::Add(path),
                Some(WorkspaceCommand::Remove { path }) => workspace::CliRequest::Remove(path),
                Some(WorkspaceCommand::Clear) => workspace::CliRequest::Clear,
            };
            return workspace::run_cli(&home, &cwd, request, *json);
        }
        Some(Command::Provider { name }) => return account::cli_provider(&home, name.as_deref()),
        Some(Command::Setup) => return account::cli_setup(&home).await,
        Some(Command::Teams) => return account::cli_teams(&home),
        Some(Command::Credits { json }) => return account::cli_credits(&home, *json).await,
        Some(Command::Permissions { json }) => {
            return account::cli_permissions(&home, &cwd, permission_mode, *json)
        }
        Some(Command::Sessions { all, limit, json }) => {
            return list_sessions(&sessions_dir, (!*all).then_some(cwd.as_path()), *limit, *json)
        }
        _ => {}
    }
    match cli.command {
        Some(Command::Login { provider }) => {
            let msg = match provider.as_deref().filter(|p| *p != "codex") {
                Some(name) => {
                    let service = provider::hosted::Service::parse(name).with_context(|| {
                        format!("unknown provider `{name}` (expected codex, opencode-go, opencode, xai or vercel)")
                    })?;
                    login::login_key(&home, service).await?
                }
                None => {
                    login::login(&home, |url| {
                        eprintln!("{}", login::sign_in_prompt(url));
                        login::open_browser(url);
                    })
                    .await?
                }
            };
            println!("{msg}");
            return Ok(());
        }
        Some(Command::Logout { provider }) => {
            let msg = match provider {
                Some(p) => login::logout(&home, &p)?,
                None => login::logout_all(&home)?,
            };
            println!("{msg}");
            return Ok(());
        }
        Some(Command::Mcp { action }) => return mcp_cli::run(&home, &cwd, action).await,
        Some(Command::Acp { log_file }) => {
            let opts = acp::Options {
                yolo: permission_mode.is_yolo(),
                permission_mode,
                sandbox: cli.sandbox,
                allow_network: cli.allow_network,
                model: cli.model,
                provider: cli.provider,
                add_dirs: cli.add_dir,
                no_additional_dirs: cli.no_additional_dirs,
                log_file,
            };
            let result = acp::run(opts).await;
            tools::background::kill_all();
            return result;
        }
        Some(Command::Upgrade { channel, check, json }) => {
            return upgrade::run(&home, channel, check, json).await
        }
        Some(Command::Pr { publish, draft, base, context }) => {
            let provider = config::provider(&home, cli.provider.as_deref(), cli.model.as_deref())?;
            let opts = github::Options { context: context.join(" "), publish, draft, base };
            return github::run(&cwd, &home, provider.as_ref(), github::Workflow::PullRequest, opts).await;
        }
        Some(Command::Issue { publish, context }) => {
            let provider = config::provider(&home, cli.provider.as_deref(), cli.model.as_deref())?;
            let opts = github::Options { context: context.join(" "), publish, ..Default::default() };
            return github::run(&cwd, &home, provider.as_ref(), github::Workflow::Issue, opts).await;
        }
        _ => {}
    }

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
    agent.context_window = config::context_window_for(agent.provider.name(), agent.provider.context_window());
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

    let mut text = prompt.join(" ");
    let interactive = !print
        && std::io::IsTerminal::is_terminal(&std::io::stdin())
        && std::io::IsTerminal::is_terminal(&std::io::stdout());
    // A yolo saved from the settings menu never carries over to print mode or
    // piped input: scripts get mutations only when they ask on the command line.
    let explicit_mode = cli.permission_mode.is_some() || cli.yolo || env_mode.is_some();
    if !interactive && agent.permission_mode.is_yolo() && !explicit_mode {
        agent.permission_mode = permissions::Mode::Ask;
        eprintln!("note: the saved yolo permission mode does not apply here; pass --yolo to allow changes");
    }
    anyhow::ensure!(!cli.record || interactive, "--record requires an interactive terminal");
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
    if let Some(meta) = resumed.as_ref().filter(|_| cli.model.is_none() && cli.provider.is_none()) {
        commands::restore_prefs(&home, &mut agent, meta.prefs.as_ref());
    }
    if !interactive {
        anyhow::ensure!(
            !pick,
            "-r without an id needs a terminal to pick a session; pass -r ID or -c"
        );
        return print_mode(&mut agent, &mut mcp, &servers, &ask, &mut text, resumed.as_ref()).await;
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
        .chain(["quit", "q", "clear", "img"])
        .collect();
    let templates = templates::discover(&home, &cwd, &reserved);
    let commands: Vec<(String, String)> = COMMANDS
        .iter()
        .chain(commands::COMMANDS)
        .map(|(c, d)| (c.to_string(), d.to_string()))
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
    let mut tui = Tui::start(footer, home.clone(), commands)?;
    tui.screen().editor.set_skills(skill_invoke::entries(&skills));
    sync_footer(&tui, &agent, None);
    ui::banner(env!("CARGO_PKG_VERSION"));
    ui::info(&sandbox::describe(&agent.sandbox));
    let exe = upgrade::current_exe().ok();
    let upgrader = auto_upgrade::should_start(settings::current().auto_upgrade, exe.as_deref())
        .then(|| auto_upgrade::Upgrader::start(&home));
    tui.screen().upgrader = upgrader.clone();
    if let Some(meta) = &resumed {
        ui::replay(&agent.items);
        ui::info(&format!("Resumed {} · {}", meta.id, meta.title));
    }
    mcp_slash::connect_profile_servers(&mut mcp, &mut agent, &servers, false).await;
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
                Some(UiCommand::ApplyUpgrade) => match prepare_relaunch(&tui, &mut agent, &raw_args, &cwd) {
                    Ok(r) => {
                        relaunch = Some(r);
                        break;
                    }
                    Err(notice) => {
                        tui.screen().set_hint(None);
                        ui::error(&notice);
                        continue;
                    }
                },
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
        // A prompt template (`/name args`) sends its expanded text.
        let line = match templates::invoke(&templates, &line) {
            Some(text) if !text.trim().is_empty() => text,
            _ => line,
        };
        // `/path/to/file what is this?` and `/skill-name …` are prompts.
        let is_command = line.starts_with('/')
            && skill_invoke::leading(&skills, &line, |c| known.contains(c)).is_none()
            && !slash::is_prompt(&line, |c| known.contains(c));
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
                tui.screen().editor.set_skills(skill_invoke::entries(&skills));
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
            let names: Vec<&str> = chosen.iter().map(|s| s.name.as_str()).collect();
            ui::info(&format!("Using skill {}", names.join(", ")));
            turn_text = text;
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
    if let Some(u) = &upgrader {
        u.stop();
    }
    tools::background::kill_all();
    mcp.shutdown().await;
    if let Some(r) = relaunch {
        // Close the session (and its lock) and the tape before handing over.
        let resume = agent.session.take().filter(|s| s.is_saved()).map(|s| s.id.clone());
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

/// Additional workspace directories for `nyq status` / `nyq doctor`, which
/// report the configuration without failing on a bad directory.
fn extra_dirs(home: &Path, cwd: &Path, cli: &Cli) -> Vec<std::path::PathBuf> {
    workspace::Runtime::prepare(home, cwd, &cli.add_dir, cli.no_additional_dirs)
        .map(|p| p.active)
        .unwrap_or_default()
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
    tui.screen().set_busy(Some("Listing models"), Some(cancel.clone()));
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
            ui::info(&format!("  Model set to {} (not in the model list).", choice.model));
            apply_model_options(home, agent, None, &wanted);
            return Ok(());
        }
        [] => anyhow::bail!("no model matches `{}`", wanted.query),
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

fn apply_model_options(home: &std::path::Path, agent: &mut Agent, current: Option<&models::ModelChoice>, wanted: &models::ModelArgs) {
    if let Some(effort) = &wanted.effort {
        commands::apply_setting(home, agent, current, settings::Id::Effort, effort);
        ui::info(&format!("  Reasoning effort: {effort}."));
    }
    if let Some(fast) = wanted.fast {
        commands::apply_setting(home, agent, current, settings::Id::Fast, if fast { "on" } else { "off" });
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
async fn print_mode(
    agent: &mut Agent,
    mcp: &mut McpManager,
    servers: &[ServerConfig],
    ask: &AskOpts,
    text: &mut String,
    resumed: Option<&session::Meta>,
) -> Result<()> {
    let (cancel, signal) = cancel_on_signal();
    if text.trim().is_empty() && !ask.continue_recovery {
        use std::io::Read;
        std::io::stdin().read_to_string(text)?;
    }
    let mut images = Vec::new();
    for path in &ask.image {
        images.push(tools::image::load(&agent.cwd.join(path))?);
    }
    if let Some(meta) = resumed {
        ui::info(&format!("Resumed {} · {}", meta.id, meta.title));
    }
    mcp_slash::connect_profile_servers(mcp, agent, servers, true).await;
    mcp_features::sync(agent, mcp);
    hooks::session_start(agent, if resumed.is_some() { "resume" } else { "startup" }).await;
    let mut plain = ui::PlainUi::new(agent.permission_mode.is_yolo());
    plain.quiet = ask.quiet || ask.json;
    plain.prompt = ask.prompt_permissions;
    plain.record = ask.json.then(ui::RunRecord::default);
    let requests_before = agent.stats.requests;
    let result = if ask.continue_recovery {
        agent.continue_turn(&mut plain, cancel.clone()).await
    } else if let Some(msg) = hooks::prompt_submit(agent, text, &cancel).await {
        Err(anyhow::anyhow!("{msg}"))
    } else {
        let (mut item, _, warning) = tools::image::user_turn(text, &agent.cwd);
        if let Some(warning) = warning {
            ui::error(&warning);
        }
        if let types::Item::User { images: attached, .. } = &mut item {
            attached.extend(images);
        }
        agent.turn(item, &mut plain, cancel.clone()).await
    };
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
        return if code == 0 { Ok(()) } else { Err(Exit(code).into()) };
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

/// A token that fires on Ctrl-C or SIGTERM, and the exit status the signal
/// calls for (130 or 143; 0 until one arrives). Cancel the token yourself to
/// stop listening.
fn cancel_on_signal() -> (CancellationToken, std::sync::Arc<std::sync::atomic::AtomicI32>) {
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

fn format_sessions(all: &[session::Meta]) -> Vec<String> {
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
    hooks::session_start(agent, "resume").await;
    Ok(())
}

/// Open the session picker (this folder's sessions first).
fn open_sessions(tui: &Tui, sessions_dir: &std::path::Path, agent: &Agent) -> Result<()> {
    let all = session::list(sessions_dir)?;
    let current = agent.session.as_ref().map(|s| s.id.as_str());
    tui.screen().open_sessions(all, &agent.cwd, current);
    Ok(())
}

/// `nyq sessions`: this directory's conversations (or every one when `cwd`
/// is None), newest first.
fn list_sessions(dir: &Path, cwd: Option<&Path>, limit: Option<u16>, json: bool) -> Result<()> {
    let (mut all, skipped) = session::list_report(dir)?;
    if let Some(warning) = session::skipped_warning(skipped.len()) {
        eprintln!("{warning}");
    }
    if let Some(cwd) = cwd {
        all.retain(|m| session::same_cwd(&m.cwd, cwd));
    }
    if let Some(n) = limit {
        all.truncate(n.into());
    }
    if json {
        let list: Vec<_> = all.iter().map(session::Meta::to_json).collect();
        println!("{}", serde_json::to_string_pretty(&list)?);
    } else if all.is_empty() && cwd.is_some() {
        println!("No saved conversations in this directory (nyq sessions --all lists every one).");
    } else {
        for l in format_sessions(&all) {
            println!("{l}");
        }
    }
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
            tui.screen().open_help();
            ui::info("  !CMD run a shell command yourself (not sandboxed) · $skill or /skill sends a skill with your message\n  Enter while working steers · Alt+Enter queues a follow-up · Alt+Up edits the queue · Esc interrupts\n  Ctrl+O transcript · Ctrl+V paste an image · Ctrl+G reload into a ready upgrade · prompt templates: see /alias");
        }
        "/new" | "/clear" => {
            commands::fresh(agent, sessions_dir)?;
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
        "/agents" => ui::list(&subagents::command(agent.session.as_ref().map(|s| s.path.as_path()), &arg)?),
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
        "/tools" => {
            let lines: Vec<String> = agent
                .tools
                .specs()
                .iter()
                .map(|t| {
                    format!(
                        "{:<24} {}",
                        t.name,
                        t.description.lines().next().unwrap_or("")
                    )
                })
                .collect();
            ui::list(&lines);
        }
        "/mcp" => mcp_slash::command(agent, mcp, servers, tui, &arg).await?,
        "/permissions" => commands::permissions(agent, tui, &config::home(), &arg).await,
        "/allowlist" => commands::allowlist(agent, &arg),
        "/hooks" => hooks::command(agent, arg.first().copied()),
        "/login" | "/logout" | "/setup" | "/credits" | "/balance" => account::slash(cmd, &arg, agent, tui).await?,
        "/yolo" => commands::toggle_yolo(&config::home(), agent),
        "/usage" | "/cost" => usage::slash(&config::home(), &arg, agent.session.as_ref().map(|s| s.id.as_str()))?,
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
            "nyq", "ask", "-r", "abc", "--json", "--quiet", "--no-save", "--prompt-permissions",
            "--system", "be brief", "--image", "a.png", "--image=b.png", "--continue-recovery", "go",
        ]) {
            Cli { resume, command: Some(Command::Ask { prompt, opts }), .. } => {
                assert_eq!(resume.as_deref(), Some("abc"));
                assert_eq!(prompt, ["go"]);
                assert!(opts.json && opts.quiet && opts.no_save && opts.prompt_permissions && opts.continue_recovery);
                assert_eq!(opts.system.as_deref(), Some("be brief"));
                assert_eq!(opts.image.len(), 2);
            }
            _ => panic!("ask options did not parse"),
        }
        assert!(parse(&["nyq", "ask", "-c", "hi"]).continue_last);
        let print = parse(&["nyq", "-p", "--json", "hi"]);
        assert!(print.print && print.ask.json && print.ask.print_only());
        assert_eq!(
            parse(&["nyq", "--context-limit", "skill_catalog_bytes=off", "ask", "x"]).context_limit,
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
            Some(Command::Sessions { all: true, limit: Some(5), json: true })
        ));
        assert!(Cli::try_parse_from(["nyq", "sessions", "--limit", "0"]).is_err());
        assert!(matches!(parse(&["nyq", "logout"]).command, Some(Command::Logout { provider: None })));
        assert!(matches!(
            parse(&["nyq", "acp", "--log-file", "/tmp/x.log"]).command,
            Some(Command::Acp { log_file: Some(_) })
        ));
        assert!(matches!(
            parse(&["nyq", "workspace"]).command,
            Some(Command::Workspace { action: None, json: false })
        ));
        assert!(matches!(
            parse(&["nyq", "workspace", "--json"]).command,
            Some(Command::Workspace { action: None, json: true })
        ));
        assert!(matches!(
            parse(&["nyq", "workspace", "add", "lib"]).command,
            Some(Command::Workspace {
                action: Some(WorkspaceCommand::Add { .. }),
                ..
            })
        ));
        for args in [["nyq", "doctor", "--json"], ["nyq", "models", "--json"], ["nyq", "usage", "--json"]] {
            assert!(Cli::try_parse_from(args).is_ok(), "{args:?}");
        }
        assert!(matches!(
            parse(&["nyq", "mcp", "list", "--connect", "--json"]).command,
            Some(Command::Mcp { action: mcp_cli::McpCommand::List { connect: true, json: true } })
        ));
        match parse(&["nyq", "mcp", "add", "--transport", "http", "web", "https://x.test/mcp"]).command {
            Some(Command::Mcp { action: mcp_cli::McpCommand::Add { transport, command, .. } }) => {
                assert_eq!((transport.as_deref(), command), (Some("http"), vec!["https://x.test/mcp".to_string()]));
            }
            _ => panic!("mcp add --transport did not parse"),
        }
        assert!(matches!(
            parse(&["nyq", "mcp", "auth", "web"]).command,
            Some(Command::Mcp { action: mcp_cli::McpCommand::Login { .. } })
        ));
        assert!(Cli::try_parse_from(["nyq", "workspace", "add"]).is_err());
        assert!(matches!(parse(&["nyq", "balance", "--json"]).command, Some(Command::Credits { json: true })));
        assert!(matches!(parse(&["nyq", "provider", "xai"]).command, Some(Command::Provider { name: Some(n) }) if n == "xai"));
        match parse(&["nyq", "mcp", "add", "fs", "--env", "A=1", "--", "npx", "-y", "srv"]).command {
            Some(Command::Mcp { action: mcp_cli::McpCommand::Add { name, env, command, .. } }) => {
                assert_eq!((name.as_str(), env, command), ("fs", vec!["A=1".to_string()], vec!["npx".to_string(), "-y".into(), "srv".into()]));
            }
            _ => panic!("mcp add did not parse"),
        }
        assert!(matches!(
            parse(&["nyq", "mcp", "trust", "approve", "x"]).command,
            Some(Command::Mcp { action: mcp_cli::McpCommand::Trust { action: Some(mcp_cli::TrustAction::Approve { .. }) } })
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
            assert_eq!(auto_upgrade::VALUE_LONG.contains(&long.as_str()), takes, "{long}");
            if let Some(short) = arg.get_short() {
                assert_eq!(auto_upgrade::VALUE_SHORT.contains(&short), takes, "-{short}");
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
