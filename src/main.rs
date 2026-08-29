mod cli;
mod record_tape;
mod replay;

use std::env;
use std::io::{self, BufRead, Write};
use std::process::{Command as ProcessCommand, Stdio};

const VERSION: &str = "0.1.0";
const RESET: &str = "\x1b[0m";
const DIM: &str = "\x1b[38;5;245m";
const BOLD: &str = "\x1b[1m";
const BLUE: &str = "\x1b[38;5;75m";

fn main() {
    use cli::Command;
    match cli::parse(env::args().skip(1)) {
        Command::Interactive => interactive(),
        Command::Help => help(),
        Command::Version => println!("nyq {VERSION}"),
        Command::Ask(args) => ask(&args.join(" ")),
        Command::Resume(args) => interactive_with_notice(&format!("resume {}", args.join(" "))),
        Command::Replay(args) => replay(&args),
        Command::Unknown(command) => {
            eprintln!("nyq: unknown command `{command}`\nTry `nyq --help`.");
            std::process::exit(2);
        }
        other => println!(
            "{DIM}nyq: `{}` is recognized; its subsystem is not ported yet.{RESET}",
            command_name(&other)
        ),
    }
}

fn command_name(command: &cli::Command) -> &'static str {
    use cli::Command::*;
    match command {
        Acp(_) => "acp",
        Login(_) => "login",
        Logout(_) => "logout",
        Setup(_) => "setup",
        Status(_) => "status",
        Permissions(_) => "permissions",
        Mcp(_) => "mcp",
        Models(_) => "models",
        Provider(_) => "provider",
        Doctor(_) => "doctor",
        Background(_) => "background",
        Session(_) => "session",
        Sessions(_) => "sessions",
        Credits(_) => "credits",
        Usage(_) => "usage",
        Upgrade(_) => "upgrade",
        Replay(_) => "replay",
        Workspace(_) => "workspace",
        _ => "unknown",
    }
}

fn help() {
    println!("{BOLD}nyq{RESET} — tiny native coding-agent CLI\n\n\
         {BOLD}USAGE{RESET}\n  nyq                 Start an interactive session\n  nyq ask TEXT        Send one prompt\n  nyq resume TARGET   Resume a session\n  nyq COMMAND         Run a top-level command\n\n\
         {BOLD}COMMANDS{RESET}\n  ask, acp, login, logout, setup, status, permissions, mcp\n  models, provider, doctor, background, session, sessions\n  credits, usage, upgrade, replay, workspace\n\n\
         {BOLD}INTERACTIVE COMMANDS{RESET}\n  /help               Show this help\n  /pwd                Print the workspace\n  /run COMMAND        Run a local command\n  /quit               Exit");
}

fn interactive() {
    interactive_with_notice("");
}

fn interactive_with_notice(notice: &str) {
    let stdin = io::stdin();
    let mut input = String::new();
    println!("{BOLD}nyq{RESET} {DIM}v{VERSION}{RESET}");
    if !notice.is_empty() {
        println!("{DIM}{notice}{RESET}");
    }
    println!("{DIM}Dependency-free Rust foundation. Type /help for commands.{RESET}\n");
    loop {
        print!("{BLUE}❯{RESET} ");
        let _ = io::stdout().flush();
        input.clear();
        if stdin.lock().read_line(&mut input).unwrap_or(0) == 0 {
            break;
        }
        let line = input.trim();
        if line.is_empty() {
            continue;
        }
        if line == "/quit" || line == "/exit" {
            break;
        }
        if line == "/help" {
            help();
            continue;
        }
        if line == "/pwd" {
            println!(
                "{}",
                env::current_dir()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default()
            );
            continue;
        }
        if let Some(command) = line.strip_prefix("/run ") {
            run_command(command);
            continue;
        }
        println!("{DIM}prompt received:{RESET} {line}");
        println!("{DIM}model transport is not configured yet; agent runtime is next.{RESET}");
    }
}

fn replay(args: &[String]) {
    match replay::parse_args(args) {
        Ok(options) => {
            let bytes = match std::fs::read(&options.path) {
                Ok(bytes) => bytes,
                Err(error) => {
                    eprintln!("nyq replay: cannot open {}: {error}", options.path);
                    std::process::exit(1);
                }
            };
            match record_tape::Parser::init(&bytes) {
                Ok(mut parser) => {
                    let mut frames = 0;
                    while let Ok(Some(_)) = parser.next() {
                        frames += 1;
                    }
                    println!(
                        "{DIM}replay:{RESET} {}x{} {} frames={} json={} golden={} frames_dir={}",
                        parser.header.cols,
                        parser.header.rows,
                        String::from_utf8_lossy(parser.header.version),
                        frames,
                        options.json,
                        options.golden_path.as_deref().unwrap_or("-"),
                        options.frames_dir.as_deref().unwrap_or("-")
                    );
                }
                Err(error) => {
                    eprintln!("nyq replay: invalid tape: {error:?}");
                    std::process::exit(1);
                }
            }
        }
        Err(error) => {
            eprintln!("nyq replay: {error:?}");
            std::process::exit(2);
        }
    }
}

fn ask(prompt: &str) {
    if prompt.trim().is_empty() {
        eprintln!("nyq ask: missing prompt");
        std::process::exit(2);
    }
    println!("{DIM}prompt received:{RESET} {prompt}");
    println!("{DIM}model transport is not configured yet; agent runtime is next.{RESET}");
}

fn run_command(command: &str) {
    match ProcessCommand::new("sh")
        .arg("-c")
        .arg(command)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
    {
        Ok(status) if !status.success() => {
            println!("{DIM}exit {}{RESET}", status.code().unwrap_or(1))
        }
        Ok(_) => {}
        Err(error) => eprintln!("nyq: {error}"),
    }
}
