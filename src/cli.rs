//! The command-line surface: clap definitions and a handler for every
//! subcommand that runs without opening a conversation.
//!
//! `main::run` loads configuration and resolves the permission mode, then
//! hands the parsed command here. [`Ctx::dispatch`] reports whether it ran a
//! command, so the caller knows whether to stop or carry on into the
//! interactive or print path.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};

use crate::account;
use crate::acp;
use crate::config;
use crate::doctor;
use crate::extensions;
use crate::github;
use crate::login;
use crate::mcp_cli;
use crate::permissions;
use crate::provider;
use crate::record_tape;
use crate::sandbox;
use crate::session;
use crate::settings;
use crate::tools;
use crate::upgrade;
use crate::usage;
use crate::workspace;

#[derive(Parser)]
#[command(
    name = "nyq",
    version,
    disable_version_flag = true,
    about = "A small, friendly coding agent for your terminal"
)]
pub struct Cli {
    /// Start the conversation with this message.
    pub prompt: Vec<String>,
    /// Print version.
    #[arg(short = 'v', short_alias = 'V', long, action = clap::ArgAction::Version)]
    pub version: Option<bool>,
    /// Answer the prompt non-interactively and exit (reads stdin if no prompt).
    #[arg(short, long)]
    pub print: bool,
    #[command(flatten)]
    pub ask: AskOpts,
    /// Record interactive terminal output under <home>/tapes/<session>.cast.
    #[arg(long)]
    pub record: bool,
    /// Model id (overrides config).
    #[arg(short, long, global = true)]
    pub model: Option<String>,
    /// Provider: codex, grok, anthropic, openai, ollama, opencode-go, opencode or xai.
    #[arg(long, global = true)]
    pub provider: Option<String>,
    /// Run tools without asking for approval.
    #[arg(long, global = true)]
    pub yolo: bool,
    /// Permission mode: ask (default), auto, or yolo. Overrides --yolo and settings.
    #[arg(long, global = true, value_name = "MODE")]
    pub permission_mode: Option<String>,
    /// Confine shell commands the model runs [default: workspace-write where supported].
    #[arg(long, value_enum, global = true)]
    pub sandbox: Option<sandbox::Mode>,
    /// Allow network access from sandboxed shell commands.
    #[arg(long, global = true)]
    pub allow_network: bool,
    /// Add a workspace directory for this run. Repeatable.
    #[arg(long = "add-dir", value_name = "PATH", global = true)]
    pub add_dir: Vec<PathBuf>,
    /// Ignore additional directories saved for this workspace.
    #[arg(long = "no-additional-dirs", global = true)]
    pub no_additional_dirs: bool,
    /// Continue the most recent conversation in this directory.
    #[arg(short = 'c', long = "continue", global = true)]
    pub continue_last: bool,
    /// Resume a saved conversation by id (or id prefix); without an id, pick one.
    #[arg(short, long, value_name = "ID", num_args = 0..=1, default_missing_value = "", global = true)]
    pub resume: Option<String>,
    /// Override a prompt budget for this run: NAME=BYTES or NAME=off. Repeatable.
    #[arg(long = "context-limit", value_name = "NAME=BYTES", global = true)]
    pub context_limit: Vec<String>,
    /// Show developer-only commands (`/tools`) for this run. Also NYQ_DEBUG=1.
    #[arg(long, global = true)]
    pub debug: bool,
    #[command(subcommand)]
    pub command: Option<Command>,
}

/// Options for one non-interactive answer (`nyq ask`, `nyq -p`).
#[derive(clap::Args, Clone, Default)]
pub struct AskOpts {
    /// Print one JSON object with the answer, tool calls and exit code (implies -p).
    #[arg(long)]
    pub json: bool,
    /// Don't print the assistant's text; with --json only the JSON is printed (implies -p).
    #[arg(long)]
    pub quiet: bool,
    /// Replace the built-in base system prompt for this run.
    #[arg(long, value_name = "TEXT")]
    pub system: Option<String>,
    /// Don't create or save a session (implies -p; not with --resume or --continue).
    #[arg(long)]
    pub no_save: bool,
    /// Ask y/N on the terminal for permission requests instead of denying them.
    #[arg(long)]
    pub prompt_permissions: bool,
    /// Attach an image to the prompt. Repeatable.
    #[arg(long, value_name = "PATH")]
    pub image: Vec<PathBuf>,
    /// Continue the interrupted or cut-off response of the resumed session (with -r ID or -c).
    #[arg(long)]
    pub continue_recovery: bool,
}

impl AskOpts {
    /// Options that only make sense without the interactive terminal.
    pub fn print_only(&self) -> bool {
        self.json || self.quiet || self.no_save || self.continue_recovery
    }
}

#[derive(Subcommand)]
pub enum Command {
    /// Play an asciicast v2 terminal tape.
    Replay {
        tape: PathBuf,
        #[arg(long, default_value_t = 1.0)]
        speed: f64,
    },
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
    /// Sign in: ChatGPT subscription (default, Codex models), a Grok
    /// subscription, or store an API key for anthropic, opencode-go, opencode or xai.
    Login {
        /// codex (default), grok, anthropic, opencode-go, opencode or xai.
        provider: Option<String>,
    },
    /// Remove nyq's stored login for a provider, or every stored login.
    Logout {
        /// codex, grok, anthropic, opencode-go, opencode or xai [default: all of them].
        provider: Option<String>,
    },
    /// Run as an Agent Client Protocol (ACP) agent over stdio, for editors like Zed.
    Acp {
        /// Append request and error logs to this file (stdout carries the protocol).
        #[arg(long, value_name = "PATH")]
        log_file: Option<PathBuf>,
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
    /// Choose the default provider (codex, grok, anthropic, opencode-go, opencode, xai, openai, ollama).
    Provider { name: Option<String> },
    /// Show sign-in status per provider and the default; on a terminal, set them up.
    Setup,
    /// Manage MCP servers and project trust without opening the interactive shell.
    Mcp {
        #[command(subcommand)]
        action: mcp_cli::McpCommand,
    },
    /// List, install or remove extensions (alias: ext).
    #[command(alias = "ext")]
    Extensions {
        #[command(subcommand)]
        action: extensions::cli::ExtCommand,
    },
}

#[derive(Subcommand)]
pub enum SessionCommand {
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
pub enum WorkspaceCommand {
    /// List the primary directory and additional directories.
    List,
    /// Persist an existing directory for this workspace.
    Add { path: PathBuf },
    /// Remove a saved or launch-only additional directory.
    Remove { path: PathBuf },
    /// Remove every saved additional directory for this workspace.
    Clear,
}

/// The settings a subcommand handler reads, gathered once by `main::run`.
pub struct Ctx<'a> {
    pub cli: &'a Cli,
    pub home: &'a Path,
    pub sessions_dir: &'a Path,
    pub cwd: &'a Path,
    pub permission_mode: permissions::Mode,
    /// The permission mode was set on the command line (not just saved).
    pub explicit_mode: bool,
}

impl Ctx<'_> {
    /// Run the parsed subcommand, if any: `true` when it ran and the process
    /// is done, `false` when the interactive or print path should carry on.
    pub async fn dispatch(self, command: Option<Command>) -> Result<bool> {
        let Some(command) = command else {
            return Ok(false);
        };
        match command {
            // These three continue into a conversation.
            Command::Ask { .. } | Command::Resume { .. } => return Ok(false),
            Command::Session { action } => return self.session(action).await,

            Command::Replay { tape, speed } => record_tape::replay(&tape, speed).await?,
            Command::Sessions { all, limit, json } => {
                list_sessions(self.sessions_dir, (!all).then_some(self.cwd), limit, json)?
            }
            Command::Workspace { action, json } => {
                let request = match &action {
                    None | Some(WorkspaceCommand::List) => workspace::CliRequest::List,
                    Some(WorkspaceCommand::Add { path }) => workspace::CliRequest::Add(path),
                    Some(WorkspaceCommand::Remove { path }) => workspace::CliRequest::Remove(path),
                    Some(WorkspaceCommand::Clear) => workspace::CliRequest::Clear,
                };
                workspace::run_cli(self.home, self.cwd, request, json)?
            }
            Command::Usage { period, json } => usage::show(self.home, period, json)?,
            Command::Status { json } => {
                let provider = config::provider(
                    self.home,
                    self.cli.provider.as_deref(),
                    self.cli.model.as_deref(),
                )
                .ok();
                doctor::cli_status(
                    self.home,
                    self.cwd,
                    provider.as_deref(),
                    &self.policy(),
                    json,
                )?
            }
            Command::Permissions { json } => {
                account::cli_permissions(self.home, self.cwd, self.permission_mode, json)?
            }
            Command::Doctor { json } => {
                let provider = config::provider(
                    self.home,
                    self.cli.provider.as_deref(),
                    self.cli.model.as_deref(),
                )
                .ok();
                let facts = doctor::Facts {
                    permission_mode: self.permission_mode,
                    sessions_dir: self.sessions_dir,
                };
                doctor::doctor(
                    self.home,
                    self.cwd,
                    provider.as_deref(),
                    &self.policy(),
                    &facts,
                    json,
                )
                .await?
            }
            Command::Models { json } => doctor::models(self.home, json).await?,
            Command::Login { provider } => self.login(provider).await?,
            Command::Logout { provider } => self.logout(provider).await?,
            Command::Acp { log_file } => self.acp(log_file).await?,
            Command::Pr {
                publish,
                draft,
                base,
                context,
            } => {
                self.github(github::Workflow::PullRequest, publish, draft, base, context)
                    .await?
            }
            Command::Issue { publish, context } => {
                self.github(github::Workflow::Issue, publish, false, None, context)
                    .await?
            }
            Command::Upgrade {
                channel,
                check,
                json,
            } => upgrade::run(self.home, channel, check, json).await?,
            Command::Provider { name } => account::cli_provider(self.home, name.as_deref())?,
            Command::Setup => account::cli_setup(self.home).await?,
            Command::Mcp { action } => mcp_cli::run(self.home, self.cwd, action).await?,
            Command::Extensions { action } => {
                extensions::cli::run(self.home, self.cwd, action).await?
            }
        }
        Ok(true)
    }

    /// `nyq session …`: inspect, migrate, recover or delete a saved conversation.
    async fn session(&self, action: Option<SessionCommand>) -> Result<bool> {
        match action {
            // A resume continues into the interactive or print path.
            Some(SessionCommand::Resume { .. }) => return Ok(false),
            None => session::print_summary(self.sessions_dir, "last", self.cwd, false)?,
            Some(SessionCommand::Recover { which, json }) => {
                let (id, kept, skipped) = session::recover(self.sessions_dir, &which, self.cwd)?;
                if json {
                    println!(
                        "{}",
                        serde_json::json!({"id": id, "kept": kept, "skipped": skipped})
                    );
                } else {
                    println!("Recovered {id}: {kept} items, {skipped} broken lines skipped");
                }
            }
            Some(SessionCommand::Migrate { which, json }) => {
                let changed = session::migrate(self.sessions_dir, &which, self.cwd)?;
                if json {
                    println!("{}", serde_json::json!({"migrated": changed}));
                } else {
                    println!(
                        "{}",
                        if changed {
                            "Session migrated"
                        } else {
                            "Session already current"
                        }
                    );
                }
            }
            Some(SessionCommand::Show { which, json }) => {
                session::print_summary(self.sessions_dir, &which, self.cwd, json)?
            }
            Some(SessionCommand::Delete { which }) => {
                let meta = session::delete(self.sessions_dir, &which, self.cwd)?;
                println!("Deleted {} ({})", meta.id, meta.title);
            }
            Some(SessionCommand::Other(args)) => match args.as_slice() {
                [which] => session::print_summary(self.sessions_dir, which, self.cwd, false)?,
                [which, flag] if flag == "--json" => {
                    session::print_summary(self.sessions_dir, which, self.cwd, true)?
                }
                _ => bail!("usage: nyq session <ID|last> [--json] (see nyq session --help)"),
            },
        }
        Ok(true)
    }

    /// `nyq login [provider]`: a browser sign-in, or a stored API key.
    async fn login(&self, provider: Option<String>) -> Result<()> {
        let msg = match provider
            .as_deref()
            .filter(|p| !p.eq_ignore_ascii_case("codex"))
        {
            Some(name) if name.eq_ignore_ascii_case("grok") => {
                login::login_grok(self.home, provider::grok::stdin_code()).await?
            }
            Some(name) => {
                let service = provider::hosted::Service::parse(name).with_context(|| {
                    format!(
                        "unknown provider `{name}` (expected codex, grok, anthropic, opencode-go, opencode or xai)"
                    )
                })?;
                login::login_key(self.home, service).await?
            }
            None => {
                login::login(self.home, |url| {
                    eprintln!("{}", login::sign_in_prompt(url));
                    login::open_browser(url);
                })
                .await?
            }
        };
        println!("{msg}");
        Ok(())
    }

    /// `nyq logout [provider]`: forget one stored login, or all of them.
    async fn logout(&self, provider: Option<String>) -> Result<()> {
        let msg = match provider.as_deref() {
            Some(p) => login::logout(self.home, p).await?,
            None => login::logout_all(self.home).await?,
        };
        println!("{msg}");
        Ok(())
    }

    /// `nyq acp`: serve the Agent Client Protocol until the editor disconnects.
    async fn acp(&self, log_file: Option<PathBuf>) -> Result<()> {
        let permission_mode = noninteractive_mode(self.permission_mode, self.explicit_mode);
        let opts = acp::Options {
            yolo: permission_mode.is_yolo(),
            permission_mode,
            sandbox: self.cli.sandbox,
            allow_network: self.cli.allow_network,
            model: self.cli.model.clone(),
            provider: self.cli.provider.clone(),
            add_dirs: self.cli.add_dir.clone(),
            no_additional_dirs: self.cli.no_additional_dirs,
            log_file,
        };
        let result = acp::run(opts).await;
        tools::background::kill_all();
        result
    }

    /// `nyq pr` and `nyq issue`: draft one with the model, then publish with gh.
    async fn github(
        &self,
        workflow: github::Workflow,
        publish: bool,
        draft: bool,
        base: Option<String>,
        context: Vec<String>,
    ) -> Result<()> {
        let provider = config::provider(
            self.home,
            self.cli.provider.as_deref(),
            self.cli.model.as_deref(),
        )?;
        let opts = github::Options {
            context: context.join(" "),
            publish,
            draft,
            base,
        };
        github::run(self.cwd, self.home, provider.as_ref(), workflow, opts).await
    }

    /// The sandbox `nyq status` and `nyq doctor` report, without failing on an
    /// unusable directory.
    fn policy(&self) -> sandbox::Policy {
        sandbox::Policy {
            mode: settings::sandbox_mode_or(self.cli.sandbox),
            network: self.cli.allow_network,
            writable: extra_dirs(self.home, self.cwd, self.cli),
        }
    }
}

/// Saved yolo is only for interactive terminal sessions.
pub fn noninteractive_mode(mode: permissions::Mode, explicit: bool) -> permissions::Mode {
    if mode.is_yolo() && !explicit {
        eprintln!(
            "note: the saved yolo permission mode does not apply here; pass --yolo to allow changes"
        );
        permissions::Mode::Ask
    } else {
        mode
    }
}

/// Additional workspace directories for `nyq status` / `nyq doctor`, which
/// report the configuration without failing on a bad directory.
fn extra_dirs(home: &Path, cwd: &Path, cli: &Cli) -> Vec<PathBuf> {
    workspace::Runtime::prepare(home, cwd, &cli.add_dir, cli.no_additional_dirs)
        .map(|p| p.active)
        .unwrap_or_default()
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
        for l in crate::format_sessions(&all) {
            println!("{l}");
        }
    }
    Ok(())
}
