//! Accounts and management: sign-in status per provider, the default
//! provider and the permission summary. Backs `/login`, `/logout`, `/setup`
//! and `nyq provider|setup|permissions`.

use std::path::Path;

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::agent::Agent;
use crate::config;
use crate::models::ModelChoice;
use crate::provider::hosted::{self, Service};
use crate::ui::{self, tui::Tui};

/// Every provider nyq can use, in menu order.
pub const PROVIDERS: [&str; 8] = ["codex", "grok", "anthropic", "opencode-go", "opencode", "xai", "openai", "ollama"];
/// Providers `/login` can sign in to (the rest are configured by endpoint).
const LOGIN: [&str; 6] = ["codex", "grok", "anthropic", "opencode-go", "opencode", "xai"];

const OPENAI_HELP: &str = "OpenAI-compatible APIs need no sign-in: set NYQ_BASE_URL, NYQ_MODEL and NYQ_API_KEY, or base_url/model/api_key in ~/.nyq/model.json.";
const OLLAMA_HELP: &str = "Ollama runs locally and needs no sign-in: start it (OLLAMA_HOST picks the address) and choose a model with /model.";

/// The provider id for a name or alias (`chatgpt` → `codex`, `claude` →
/// `anthropic`, …).
pub fn canonical(name: &str) -> Option<&'static str> {
    match name.trim().to_ascii_lowercase().as_str() {
        "codex" | "chatgpt" => Some("codex"),
        "grok" => Some("grok"),
        "openai" => Some("openai"),
        "ollama" => Some("ollama"),
        other => Service::parse(other).map(Service::id),
    }
}

fn unknown(name: &str) -> anyhow::Error {
    anyhow!("unknown provider `{name}` (expected {})", PROVIDERS.join(", "))
}

pub fn title(id: &str) -> &'static str {
    match id {
        "codex" => "ChatGPT (Codex)",
        "grok" => "Grok subscription",
        "openai" => "OpenAI-compatible API",
        "ollama" => "Ollama (local)",
        other => Service::parse(other).map_or("unknown", Service::title),
    }
}

/// Sign-in state of one provider. Never contains credentials.
pub struct Connection {
    pub status: String,
    pub connected: bool,
}

pub fn connection(home: &Path, id: &str) -> Connection {
    let Some(id) = canonical(id) else {
        return Connection { status: "unknown provider".into(), connected: false };
    };
    let (status, connected) = match id {
        "codex" => {
            let path = config::codex_auth_path(home);
            if !path.exists() {
                ("not signed in".into(), false)
            } else if std::env::var_os("NYQ_CODEX_AUTH").is_some() {
                ("signed in (NYQ_CODEX_AUTH)".into(), true)
            } else if path == home.join("chatgpt-auth.json") {
                ("signed in (nyq)".into(), true)
            } else {
                ("signed in (codex CLI login)".into(), true)
            }
        }
        "grok" => match crate::provider::grok::sign_in(home) {
            crate::provider::grok::SignIn::Ready => ("signed in (nyq)".into(), true),
            crate::provider::grok::SignIn::Unusable => ("unusable private file".into(), false),
            crate::provider::grok::SignIn::Missing => ("not signed in".into(), false),
        },
        "openai" => match config::openai_config(home).ok().and_then(|c| c.0) {
            Some(url) => (format!("endpoint {url}"), true),
            None => ("not configured".into(), false),
        },
        "ollama" => (format!("local, no sign-in ({})", config::ollama_url()), false),
        other => match Service::parse(other).and_then(|s| hosted::key_source(home, s)) {
            Some(source) => (format!("API key ({source})"), true),
            None => ("not connected".into(), false),
        },
    };
    Connection { status, connected }
}

/// Whether nyq itself stores a sign-in for `id` (what `logout` removes).
fn stored_by_nyq(home: &Path, id: &str) -> bool {
    if id == "codex" {
        return home.join("chatgpt-auth.json").exists();
    }
    if id == "grok" {
        return crate::provider::grok::auth_path(home).exists();
    }
    std::fs::read(home.join("auth.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .is_some_and(|v| v[id]["key"].as_str().is_some_and(|k| !k.trim().is_empty()))
}

fn read_settings(home: &Path) -> Value {
    crate::settings::file::read_user(home).map_or(Value::Null, Value::Object)
}

/// The model a provider starts with: `settings.toml` `models.<id>`, else its default.
fn default_model(home: &Path, id: &str) -> Option<String> {
    if let Some(m) = read_settings(home)["models"][id].as_str().filter(|m| !m.trim().is_empty()) {
        return Some(m.to_string());
    }
    match id {
        "codex" => Some(config::DEFAULT_CODEX_MODEL.into()),
        "grok" => Some(crate::provider::grok::DEFAULT_MODEL.into()),
        "openai" => config::openai_config(home).ok().and_then(|c| c.2),
        "ollama" => None,
        other => Service::parse(other).map(|s| s.default_model()),
    }
}

/// Persist `id` as the default provider: `provider` in settings.toml (other
/// keys kept) and `last-model.json` pointing at its model, which startup
/// prefers over settings. Returns the model, when one is known.
fn persist_default(home: &Path, id: &str) -> Result<Option<String>> {
    crate::settings::file::write_changes(home, &[(vec!["provider".into()], Some(json!(id)))])?;
    let model = default_model(home, id);
    match &model {
        Some(m) => {
            let choice = ModelChoice { provider: id.into(), model: m.clone(), ..Default::default() };
            crate::models::save_last(home, &choice, None)?;
        }
        None => match std::fs::remove_file(home.join("last-model.json")) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e).context("removing last-model.json"),
            _ => {}
        },
    }
    Ok(model)
}

/// `nyq provider NAME`, `/setup` → Model provider. Returns a message and the
/// model to switch to now (when known).
pub fn set_default_provider(home: &Path, name: &str) -> Result<(String, Option<ModelChoice>)> {
    let id = canonical(name).ok_or_else(|| unknown(name))?;
    let model = persist_default(home, id)?;
    // Keep the settings in effect in step, so a later settings save keeps it.
    let mut current = crate::settings::current();
    current.extra.insert("provider".into(), json!(id));
    crate::settings::install(current);
    let mut msg = format!("Default provider: {} ({id})", title(id));
    if let Some(m) = &model {
        msg.push_str(&format!(" · {m}"));
    }
    msg.push('.');
    match id {
        "openai" if !connection(home, id).connected => msg.push_str(&format!("\n{OPENAI_HELP}")),
        "ollama" if model.is_none() => msg.push_str("\nSet a model with --model or NYQ_MODEL, or pick one with /model."),
        "openai" | "ollama" => {}
        _ if !connection(home, id).connected => msg.push_str(&format!("\nNot signed in yet: run `nyq login {id}`.")),
        _ => {}
    }
    for var in ["NYQ_PROVIDER", "NYQ_BASE_URL"] {
        if std::env::var(var).is_ok_and(|v| !v.trim().is_empty()) {
            msg.push_str(&format!("\nNote: {var} is set and takes precedence."));
        }
    }
    let choice = model.map(|model| ModelChoice { provider: id.into(), model, ..Default::default() });
    Ok((msg, choice))
}

/// What a plain `nyq` would start with.
fn default_summary(home: &Path) -> String {
    match config::provider(home, None, None) {
        Ok(p) => format!("{} · {}", p.name(), p.model()),
        Err(e) => format!("not ready: {}", format!("{e:#}").lines().next().unwrap_or_default()),
    }
}

fn connection_lines(home: &Path) -> Vec<String> {
    PROVIDERS.iter().map(|id| format!("  {:<24} {}", title(id), connection(home, id).status)).collect()
}

/// Which credential each provider uses, and the order nyq looks for them.
fn source_lines(home: &Path) -> Vec<String> {
    let mut lines = vec!["Credential sources".to_string()];
    lines.extend(connection_lines(home));
    lines.push("API keys: the provider's environment variable, then the OpenCode CLI's login (OpenCode only), then nyq's auth.json.".into());
    lines.push("ChatGPT: NYQ_CODEX_AUTH, then a valid codex CLI login, then nyq's own sign-in.".into());
    lines.push("Grok subscription: nyq's sign-in in grok-auth.json.".into());
    lines.push(format!(
        "Anthropic: ANTHROPIC_API_KEY, then ANTHROPIC_AUTH_TOKEN (sent as a bearer token), then nyq's auth.json · endpoint {} (ANTHROPIC_BASE_URL)",
        hosted::Service::Anthropic.base_url()
    ));
    lines
}

fn summary_lines(home: &Path) -> Vec<String> {
    let mut lines = vec!["Connections".to_string()];
    lines.extend(connection_lines(home));
    lines.push(format!("Default      {}", default_summary(home)));
    lines
}

// ---------------------------------------------------------------------- CLI

/// `nyq provider [NAME]`.
pub fn cli_provider(home: &Path, name: Option<&str>) -> Result<()> {
    match name {
        Some(name) => println!("{}", set_default_provider(home, name)?.0),
        None => {
            println!("Default      {}", default_summary(home));
            println!("Providers    {}", PROVIDERS.join(", "));
            println!("Set one with `nyq provider NAME`.");
        }
    }
    Ok(())
}

/// `nyq permissions [--json]`: the permission mode (as resolved from
/// `--permission-mode`, `--yolo` and settings) and the rules.
pub fn cli_permissions(home: &Path, cwd: &Path, mode: crate::permissions::Mode, as_json: bool) -> Result<()> {
    let (perms, warnings) = crate::permissions::Permissions::load(home, cwd);
    println!("{}", crate::permissions::format_report(&perms, mode, &warnings, as_json));
    if !as_json {
        for w in warnings {
            ui::error(&w);
        }
    }
    Ok(())
}

fn read_line(prompt: &str) -> Result<Option<String>> {
    use std::io::Write;
    print!("{prompt}");
    std::io::stdout().flush()?;
    let mut line = String::new();
    Ok((std::io::stdin().read_line(&mut line)? > 0).then(|| line.trim().to_string()))
}

/// A numbered choice on the terminal; None on EOF or an empty answer.
fn choose(question: &str, options: &[String]) -> Result<Option<usize>> {
    println!("\n{question}");
    for (i, o) in options.iter().enumerate() {
        println!("  {}) {o}", i + 1);
    }
    Ok(read_line("> ")?.and_then(|a| a.parse::<usize>().ok()).filter(|n| (1..=options.len()).contains(n)).map(|n| n - 1))
}

/// Sign in to `id` from the command line (browser or hidden key prompt).
pub async fn cli_login(home: &Path, id: &str) -> Result<String> {
    match id {
        "codex" => {
            crate::login::login(home, |url| {
                eprintln!("{}", crate::login::sign_in_prompt(url));
                crate::login::open_browser(url);
            })
            .await
        }
        "grok" => crate::login::login_grok(home, crate::provider::grok::stdin_code()).await,
        "openai" => Ok(OPENAI_HELP.into()),
        "ollama" => Ok(OLLAMA_HELP.into()),
        other => crate::login::login_key(home, Service::parse(other).ok_or_else(|| unknown(other))?).await,
    }
}

/// `nyq setup`: a summary; on a terminal, prompts to sign in and pick the default.
pub async fn cli_setup(home: &Path) -> Result<()> {
    use std::io::IsTerminal;
    summary_lines(home).iter().for_each(|l| println!("{l}"));
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        println!("\nSign in with `nyq login [codex|grok|anthropic|opencode-go|opencode|xai]`, choose the default with `nyq provider NAME`, or run /setup inside nyq.");
        return Ok(());
    }
    let labels = |home: &Path| -> Vec<String> { PROVIDERS.iter().map(|id| format!("{:<24} {}", title(id), connection(home, id).status)).collect() };
    loop {
        let actions = ["Sign in or add an API key", "Sign out", "Choose the default provider", "Done"].map(String::from);
        match choose("What would you like to do?", &actions)? {
            Some(0) => {
                if let Some(i) = choose("Sign in to", &labels(home))? {
                    match cli_login(home, PROVIDERS[i]).await {
                        Ok(msg) => println!("{msg}"),
                        Err(e) => ui::error(&format!("{e:#}")),
                    }
                }
            }
            Some(1) => {
                let stored: Vec<&str> = LOGIN.into_iter().filter(|id| stored_by_nyq(home, id)).collect();
                if stored.is_empty() {
                    println!("No sign-ins stored by nyq.");
                } else if let Some(i) = choose("Sign out of", &stored.iter().map(|id| title(id).to_string()).collect::<Vec<_>>())? {
                    println!("{}", crate::login::logout(home, stored[i]).await?);
                }
            }
            Some(2) => {
                if let Some(i) = choose("Default provider", &labels(home))? {
                    println!("{}", set_default_provider(home, PROVIDERS[i])?.0);
                }
            }
            _ => break,
        }
    }
    println!("\nDefault      {}", default_summary(home));
    Ok(())
}

// --------------------------------------------------------------------- TUI

/// `/login`, `/logout`, `/setup`.
pub async fn slash(cmd: &str, args: &[&str], agent: &mut Agent, tui: &Tui) -> Result<()> {
    let home = config::home();
    match cmd {
        "/login" => tui_login(&home, tui, args.first().copied()).await,
        "/logout" => tui_logout(&home, tui, args.first().copied()).await,
        _ => tui_setup(&home, agent, tui).await,
    }
}

/// Clears a pasted-code prompt when browser sign-in finishes first.
struct HideSecret<'a>(&'a Tui);
impl Drop for HideSecret<'_> {
    fn drop(&mut self) {
        self.0.dismiss_secret();
    }
}

async fn tui_login(home: &Path, tui: &Tui, name: Option<&str>) -> Result<()> {
    let id = match name {
        Some(n) => canonical(n).ok_or_else(|| unknown(n))?,
        None => {
            let options: Vec<String> = LOGIN.iter().map(|id| format!("{:<24} {}", title(id), connection(home, id).status)).collect();
            match tui.pick("Sign in", "Choose a provider", &options).await {
                Some(i) => LOGIN[i],
                None => return Ok(()),
            }
        }
    };
    match id {
        "openai" => ui::info(OPENAI_HELP),
        "ollama" => ui::info(OLLAMA_HELP),
        "codex" => {
            let cancel = CancellationToken::new();
            tui.screen().set_busy(Some("Signing in to ChatGPT"), Some(cancel.clone()));
            let result = tokio::select! {
                r = crate::login::login(home, |url| {
                    ui::info(&format!("Sign in to ChatGPT in your browser (waiting up to 5 minutes):\n  {url}"));
                    crate::login::open_browser(url);
                }) => Some(r),
                _ = cancel.cancelled() => None,
            };
            tui.screen().idle();
            match result {
                Some(r) => ui::info(&format!("{} Pick a ChatGPT model with /model.", r?)),
                None => ui::info("Sign-in cancelled."),
            }
        }
        "grok" => {
            let cancel = CancellationToken::new();
            tui.screen().set_busy(Some("Signing in to Grok"), Some(cancel.clone()));
            let _hide = HideSecret(tui);
            let prompt = "Paste the Grok authorization code if the browser does not return (hidden)";
            let result = tokio::select! {
                r = crate::login::login_grok(home, async { Ok(tui.secret(prompt).await) }) => Some(r),
                _ = cancel.cancelled() => None,
            };
            drop(_hide);
            tui.screen().idle();
            match result {
                Some(r) => ui::info(&format!("{} Pick a Grok model with /model.", r?)),
                None => ui::info("Sign-in cancelled."),
            }
        }
        other => {
            let service = Service::parse(other).ok_or_else(|| unknown(other))?;
            let prompt = format!("{} (hidden)", crate::login::key_prompt(service));
            match tui.secret(&prompt).await {
                Some(key) => {
                    tui.screen().set_busy(Some("Checking the key"), None);
                    let saved = crate::login::store_key(home, service, &key).await;
                    tui.screen().idle();
                    ui::info(&format!("{} Pick a model with /model.", saved?));
                }
                None => ui::info("No key saved."),
            }
        }
    }
    Ok(())
}

async fn tui_logout(home: &Path, tui: &Tui, name: Option<&str>) -> Result<()> {
    let id = match name {
        Some(n) => canonical(n).ok_or_else(|| unknown(n))?,
        None => {
            let stored: Vec<&str> = LOGIN.into_iter().filter(|id| stored_by_nyq(home, id)).collect();
            if stored.is_empty() {
                ui::info("No sign-ins stored by nyq (keys from environment variables or other tools stay where they are).");
                return Ok(());
            }
            let options: Vec<String> = stored.iter().map(|id| title(id).to_string()).collect();
            match tui.pick("Sign out", "Forget nyq's stored sign-in for", &options).await {
                Some(i) => stored[i],
                None => return Ok(()),
            }
        }
    };
    anyhow::ensure!(LOGIN.contains(&id), "{} has no sign-in to remove", title(id));
    ui::info(&crate::login::logout(home, id).await?);
    Ok(())
}

async fn tui_setup(home: &Path, agent: &mut Agent, tui: &Tui) -> Result<()> {
    loop {
        let connected = PROVIDERS.iter().filter(|id| connection(home, id).connected).count();
        let current = agent.provider.name().to_string();
        let options = vec![
            format!("{:<20} {connected} connected", "Connections"),
            format!("{:<20} {current}", "Model provider"),
            format!("{:<20} {}", "Model", agent.provider.model()),
            format!("{:<20} {}", "Credential source", connection(home, &current).status),
        ];
        let Some(choice) = tui.pick("Setup", "Accounts and model provider", &options).await else {
            return Ok(());
        };
        let result = match choice {
            0 => tui_connections(home, tui).await,
            1 => tui_provider(home, agent, tui).await,
            2 => {
                tui.open_models();
                return Ok(());
            }
            _ => {
                ui::list(&source_lines(home));
                Ok(())
            }
        };
        if let Err(e) = result {
            ui::error(&format!("{e:#}"));
        }
    }
}

async fn tui_connections(home: &Path, tui: &Tui) -> Result<()> {
    let options: Vec<String> = PROVIDERS.iter().map(|id| format!("{:<24} {}", title(id), connection(home, id).status)).collect();
    let Some(i) = tui.pick("Connections", "Sign in, replace a key, or sign out", &options).await else {
        return Ok(());
    };
    let id = PROVIDERS[i];
    if !stored_by_nyq(home, id) {
        return tui_login(home, tui, Some(id)).await;
    }
    let again = if id == "codex" || id == "grok" { "Sign in again" } else { "Replace the API key" };
    match tui.pick(title(id), "Stored by nyq", &[again.to_string(), "Sign out".to_string()]).await {
        Some(0) => tui_login(home, tui, Some(id)).await,
        Some(_) => tui_logout(home, tui, Some(id)).await,
        None => Ok(()),
    }
}

async fn tui_provider(home: &Path, agent: &mut Agent, tui: &Tui) -> Result<()> {
    let options: Vec<String> = PROVIDERS
        .iter()
        .map(|id| {
            let state = if *id == agent.provider.name() {
                "current"
            } else if connection(home, id).connected {
                "connected"
            } else {
                "not connected"
            };
            format!("{:<24} {state}", title(id))
        })
        .collect();
    let Some(i) = tui.pick("Model provider", "Choose the default provider (remembered for new sessions)", &options).await else {
        return Ok(());
    };
    let (msg, choice) = set_default_provider(home, PROVIDERS[i])?;
    ui::info(&msg);
    if let Some(choice) = choice {
        crate::switch_model(home, agent, &choice);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_names() {
        assert_eq!(canonical("ChatGPT"), Some("codex"));
        assert_eq!(canonical("vercel"), None);
        assert_eq!(canonical("gateway"), None);
        assert_eq!(canonical("grok"), Some("grok"));
        assert_eq!(canonical("Claude"), Some("anthropic"));
        assert_eq!(title("anthropic"), "Anthropic API");
        assert_eq!(title("grok"), "Grok subscription");
        assert_eq!(canonical("ollama"), Some("ollama"));
        assert_eq!(canonical("nope"), None);
        assert!(PROVIDERS.iter().all(|p| canonical(p) == Some(*p) && title(p) != "unknown"));
    }

    #[test]
    fn default_provider_persists() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        std::fs::write(home.join("settings.toml"), "# keep me\nsound = \"max\"\nother = 1\n\n[models]\nxai = \"grok-custom\"\n").unwrap();
        std::fs::write(home.join("last-model.json"), r#"{"provider":"codex","model":"old","effort":"high"}"#).unwrap();
        assert_eq!(persist_default(home, "xai").unwrap().as_deref(), Some("grok-custom"));
        let settings = read_settings(home);
        assert_eq!(settings["provider"], "xai");
        assert!(std::fs::read_to_string(home.join("settings.toml")).unwrap().starts_with("# keep me\n"));
        assert_eq!((settings["sound"].as_str(), settings["other"].as_i64()), (Some("max"), Some(1)));
        let last: Value = serde_json::from_slice(&std::fs::read(home.join("last-model.json")).unwrap()).unwrap();
        assert_eq!((last["provider"].as_str(), last["model"].as_str()), (Some("xai"), Some("grok-custom")));
        assert!(last["effort"].is_null());
        // Built-in default model, then a provider without one drops last-model.json.
        assert_eq!(persist_default(home, "opencode").unwrap().as_deref(), Some(Service::Opencode.default_model().as_str()));
        assert_eq!(persist_default(home, "ollama").unwrap(), None);
        assert!(!home.join("last-model.json").exists());
        assert_eq!(read_settings(home)["provider"], "ollama");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(home.join("settings.toml")).unwrap().permissions().mode() & 0o777, 0o600);
        }
        assert!(set_default_provider(home, "bogus").is_err());
    }
}
