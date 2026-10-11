//! Accounts and management: sign-in status per provider, the default
//! provider, AI Gateway credits and teams, and the permission summary. Backs
//! `/login`, `/logout`, `/setup`, `/credits` and `nyq provider|setup|teams|
//! credits|permissions`.

use std::path::Path;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde::Serialize;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::agent::Agent;
use crate::config;
use crate::models::ModelChoice;
use crate::provider::hosted::{self, Service};
use crate::ui::{self, tui::Tui};

/// Every provider nyq can use, in menu order.
pub const PROVIDERS: [&str; 7] = ["codex", "opencode-go", "opencode", "xai", "vercel", "openai", "ollama"];
/// Providers `/login` can sign in to (the rest are configured by endpoint).
const LOGIN: [&str; 5] = ["codex", "opencode-go", "opencode", "xai", "vercel"];

const OPENAI_HELP: &str = "OpenAI-compatible APIs need no sign-in: set NYQ_BASE_URL, NYQ_MODEL and NYQ_API_KEY, or base_url/model/api_key in ~/.nyq/model.json.";
const OLLAMA_HELP: &str = "Ollama runs locally and needs no sign-in: start it (OLLAMA_HOST picks the address) and choose a model with /model.";
pub const TEAMS_UNAVAILABLE: &str = "Choosing a Vercel team is not available: nyq reaches the AI Gateway with an API key, and each AI Gateway key belongs to exactly one team, so there is no team to switch. To use another team, create a key in that team (vercel.com → AI Gateway → API Keys) and run `nyq login vercel`.";
const NO_GATEWAY_KEY: &str = "AI Gateway credits need an AI Gateway API key: run `nyq login vercel` (or /login vercel), or set AI_GATEWAY_API_KEY. ChatGPT, OpenCode and xAI accounts have no AI Gateway balance.";

/// The provider id for a name or alias (`chatgpt` → `codex`, `gateway` → `vercel`, …).
pub fn canonical(name: &str) -> Option<&'static str> {
    match name.trim().to_ascii_lowercase().as_str() {
        "codex" | "chatgpt" => Some("codex"),
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
    std::fs::read(home.join("auth.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .is_some_and(|v| v[id]["key"].as_str().is_some_and(|k| !k.trim().is_empty()))
}

fn read_settings(home: &Path) -> Value {
    std::fs::read(home.join("settings.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(Value::Null)
}

/// The model a provider starts with: `settings.json` `models.<id>`, else its default.
fn default_model(home: &Path, id: &str) -> Option<String> {
    if let Some(m) = read_settings(home)["models"][id].as_str().filter(|m| !m.trim().is_empty()) {
        return Some(m.to_string());
    }
    match id {
        "codex" => Some(config::DEFAULT_CODEX_MODEL.into()),
        "openai" => config::openai_config(home).ok().and_then(|c| c.2),
        "ollama" => None,
        other => Service::parse(other).map(|s| s.default_model().into()),
    }
}

/// Persist `id` as the default provider: `provider` in settings.json (other
/// keys kept) and `last-model.json` pointing at its model, which startup
/// prefers over settings. Returns the model, when one is known.
fn persist_default(home: &Path, id: &str) -> Result<Option<String>> {
    let path = home.join("settings.json");
    let mut settings = match read_settings(home) {
        Value::Object(m) => m,
        _ => Default::default(),
    };
    settings.insert("provider".into(), json!(id));
    crate::session::write_private(&path, &serde_json::to_vec_pretty(&settings)?).with_context(|| format!("writing {}", path.display()))?;
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
    lines
}

fn summary_lines(home: &Path) -> Vec<String> {
    let mut lines = vec!["Connections".to_string()];
    lines.extend(connection_lines(home));
    lines.push(format!("Default      {}", default_summary(home)));
    lines
}

// ------------------------------------------------------------------ credits

/// The AI Gateway balance (`GET /v1/credits`). Amounts are kept as reported.
#[derive(Debug, Default, PartialEq, Serialize)]
pub struct Credits {
    pub balance: Option<String>,
    pub used: Option<String>,
    pub plan: Option<String>,
}

fn text_field(v: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|k| match v.get(*k) {
        Some(Value::String(s)) if !s.trim().is_empty() => Some(s.trim().to_string()),
        Some(Value::Number(n)) => Some(n.to_string()),
        _ => None,
    })
}

pub fn parse_credits(body: &str) -> Result<Credits> {
    let v: Value = serde_json::from_str(body).context("invalid JSON response from the AI Gateway")?;
    anyhow::ensure!(v.is_object(), "unexpected response format from the AI Gateway");
    Ok(Credits {
        balance: text_field(&v, &["balance"]),
        used: text_field(&v, &["total_used", "used"]),
        plan: text_field(&v, &["plan"]),
    })
}

/// "API access denied · HTTP 403 · credit_card_required: Buy credits…"
fn http_error(status: u16, body: &str) -> String {
    let label = match status {
        401 => "authentication failed (check your AI Gateway API key)",
        403 => "API access denied",
        404 => "credits endpoint not found",
        429 => "rate limited",
        500..=599 => "AI Gateway server error",
        _ => "request failed",
    };
    let detail = serde_json::from_str::<Value>(body).ok().and_then(|v| match &v["error"] {
        Value::String(s) => Some(s.clone()),
        Value::Object(e) => {
            let message = e.get("message").and_then(Value::as_str);
            match (e.get("code").and_then(Value::as_str), message) {
                (Some(c), Some(m)) => Some(format!("{c}: {m}")),
                (c, m) => c.or(m).map(str::to_string),
            }
        }
        _ => v["message"].as_str().map(str::to_string),
    });
    let detail = detail
        .map(|d| format!(" · {}", d.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(200).collect::<String>()))
        .unwrap_or_default();
    format!("{label} · HTTP {status}{detail}")
}

pub async fn fetch_credits(base_url: &str, key: &str) -> Result<Credits> {
    let url = format!("{}/credits", base_url.trim_end_matches('/'));
    let response = reqwest::Client::new()
        .get(&url)
        .bearer_auth(key)
        .header(reqwest::header::USER_AGENT, config::USER_AGENT)
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .context("could not reach the AI Gateway")?;
    let status = response.status().as_u16();
    let body = response.text().await.context("reading the AI Gateway response")?;
    if !(200..300).contains(&status) {
        bail!("{}", http_error(status, &body));
    }
    parse_credits(&body)
}

/// The balance for the configured AI Gateway key.
pub async fn credits(home: &Path) -> Result<Credits> {
    let key = hosted::api_key(home, Service::Vercel).ok_or_else(|| anyhow!(NO_GATEWAY_KEY))?;
    fetch_credits(&Service::Vercel.base_url(), &key).await
}

fn amount(s: &str) -> String {
    match s.parse::<f64>() {
        Ok(n) if n.is_finite() => format!("${n:.2}"),
        _ => s.to_string(),
    }
}

pub fn credits_lines(c: &Credits) -> Vec<String> {
    let mut lines = vec!["AI Gateway credits".to_string()];
    let rows = [("Balance", c.balance.as_deref().map(amount)), ("Used", c.used.as_deref().map(amount)), ("Plan", c.plan.clone())];
    lines.extend(rows.into_iter().filter_map(|(k, v)| v.map(|v| format!("  {k:<9}{v}"))));
    if lines.len() == 1 {
        lines.push("  (no balance reported)".into());
    }
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

/// `nyq credits|balance [--json]`.
pub async fn cli_credits(home: &Path, as_json: bool) -> Result<()> {
    match credits(home).await {
        Ok(c) if as_json => println!("{}", serde_json::to_string_pretty(&c)?),
        Ok(c) => credits_lines(&c).iter().for_each(|l| println!("{l}")),
        Err(e) => {
            if as_json {
                println!("{}", json!({ "error": format!("{e:#}") }));
            }
            return Err(e);
        }
    }
    Ok(())
}

/// `nyq teams`.
pub fn cli_teams(home: &Path) -> Result<()> {
    if hosted::api_key(home, Service::Vercel).is_none() {
        bail!("{TEAMS_UNAVAILABLE}\n(No AI Gateway key is configured yet.)");
    }
    bail!("{TEAMS_UNAVAILABLE}")
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
                eprintln!("Opening your browser to sign in. If it doesn't open, visit:\n  {url}");
                crate::login::open_browser(url);
            })
            .await
        }
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
        println!("\nSign in with `nyq login [codex|opencode-go|opencode|xai|vercel]`, choose the default with `nyq provider NAME`, or run /setup inside nyq.");
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
                    println!("{}", crate::login::logout(home, stored[i])?);
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

/// `/login`, `/logout`, `/setup`, `/credits` (`/balance`).
pub async fn slash(cmd: &str, args: &[&str], agent: &mut Agent, tui: &Tui) -> Result<()> {
    let home = config::home();
    match cmd {
        "/login" => tui_login(&home, tui, args.first().copied()).await,
        "/logout" => tui_logout(&home, tui, args.first().copied()).await,
        "/setup" => tui_setup(&home, agent, tui).await,
        _ => tui_credits(&home, tui).await,
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
        other => {
            let service = Service::parse(other).ok_or_else(|| unknown(other))?;
            let prompt = format!("Paste your {} API key from {} (hidden)", service.title(), crate::login::key_site(service));
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
    ui::info(&crate::login::logout(home, id)?);
    Ok(())
}

async fn tui_credits(home: &Path, tui: &Tui) -> Result<()> {
    let cancel = CancellationToken::new();
    tui.screen().set_busy(Some("Fetching AI Gateway credits"), Some(cancel.clone()));
    let result = tokio::select! {
        r = credits(home) => Some(r),
        _ = cancel.cancelled() => None,
    };
    tui.screen().idle();
    if let Some(r) = result {
        ui::list(&credits_lines(&r?));
    }
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
            format!("{:<20} not available", "Vercel team"),
            format!("{:<20} show balance", "AI Gateway credits"),
        ];
        let Some(choice) = tui.pick("Setup", "Accounts, model provider and AI Gateway access", &options).await else {
            return Ok(());
        };
        let result = match choice {
            0 => tui_connections(home, tui).await,
            1 => tui_provider(home, agent, tui).await,
            2 => {
                tui.open_models();
                return Ok(());
            }
            3 => {
                ui::list(&source_lines(home));
                Ok(())
            }
            4 => {
                ui::info(TEAMS_UNAVAILABLE);
                Ok(())
            }
            _ => tui_credits(home, tui).await,
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
    let again = if id == "codex" { "Sign in again" } else { "Replace the API key" };
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
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn provider_names() {
        assert_eq!(canonical("ChatGPT"), Some("codex"));
        assert_eq!(canonical("gateway"), Some("vercel"));
        assert_eq!(canonical("grok"), Some("xai"));
        assert_eq!(canonical("ollama"), Some("ollama"));
        assert_eq!(canonical("nope"), None);
        assert!(PROVIDERS.iter().all(|p| canonical(p) == Some(*p) && title(p) != "unknown"));
    }

    #[test]
    fn default_provider_persists() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        std::fs::write(home.join("settings.json"), r#"{"models":{"xai":"grok-custom"},"sound":"max","other":1}"#).unwrap();
        std::fs::write(home.join("last-model.json"), r#"{"provider":"codex","model":"old","effort":"high"}"#).unwrap();
        assert_eq!(persist_default(home, "xai").unwrap().as_deref(), Some("grok-custom"));
        let settings: Value = serde_json::from_slice(&std::fs::read(home.join("settings.json")).unwrap()).unwrap();
        assert_eq!(settings["provider"], "xai");
        assert_eq!((settings["sound"].as_str(), settings["other"].as_i64()), (Some("max"), Some(1)));
        let last: Value = serde_json::from_slice(&std::fs::read(home.join("last-model.json")).unwrap()).unwrap();
        assert_eq!((last["provider"].as_str(), last["model"].as_str()), (Some("xai"), Some("grok-custom")));
        assert!(last["effort"].is_null());
        // Built-in default model, then a provider without one drops last-model.json.
        assert_eq!(persist_default(home, "vercel").unwrap().as_deref(), Some(Service::Vercel.default_model()));
        assert_eq!(persist_default(home, "ollama").unwrap(), None);
        assert!(!home.join("last-model.json").exists());
        let settings: Value = serde_json::from_slice(&std::fs::read(home.join("settings.json")).unwrap()).unwrap();
        assert_eq!(settings["provider"], "ollama");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(home.join("settings.json")).unwrap().permissions().mode() & 0o777, 0o600);
        }
        assert!(set_default_provider(home, "bogus").is_err());
    }

    #[test]
    fn credits_parsing() {
        let c = parse_credits(r#"{"balance":"95.5","total_used":"4.50","plan":"pro"}"#).unwrap();
        assert_eq!(c, Credits { balance: Some("95.5".into()), used: Some("4.50".into()), plan: Some("pro".into()) });
        assert_eq!(credits_lines(&c), ["AI Gateway credits", "  Balance  $95.50", "  Used     $4.50", "  Plan     pro"]);
        let n = parse_credits(r#"{"balance":10,"used":false,"plan":null}"#).unwrap();
        assert_eq!((n.balance.as_deref(), n.used, n.plan), (Some("10"), None, None));
        assert_eq!(credits_lines(&Credits::default())[1], "  (no balance reported)");
        assert!(parse_credits("[1]").is_err());
        assert!(parse_credits("not json").is_err());
        assert_eq!(
            http_error(403, r#"{"error":{"code":"credit_card_required","message":"Buy credits to use AI Gateway."}}"#),
            "API access denied · HTTP 403 · credit_card_required: Buy credits to use AI Gateway."
        );
        assert_eq!(http_error(502, "<html>"), "AI Gateway server error · HTTP 502");
    }

    /// Serve one canned HTTP response; returns the base URL and the request.
    async fn serve(status: &'static str, body: &'static str) -> (String, tokio::task::JoinHandle<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            while !bytes.ends_with(b"\r\n\r\n") {
                bytes.push(socket.read_u8().await.unwrap());
            }
            let response = format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            socket.write_all(response.as_bytes()).await.unwrap();
            String::from_utf8(bytes).unwrap()
        });
        (url, task)
    }

    #[tokio::test]
    async fn credits_from_fake_gateway() {
        let (url, request) = serve("200 OK", r#"{"balance":"12.3","total_used":"0.7"}"#).await;
        let c = fetch_credits(&url, "test-key").await.unwrap();
        assert_eq!((c.balance.as_deref(), c.used.as_deref()), (Some("12.3"), Some("0.7")));
        let request = request.await.unwrap().to_ascii_lowercase();
        assert!(request.starts_with("get /v1/credits http/1.1"));
        assert!(request.contains("authorization: bearer test-key"));

        let (url, _) = serve("401 Unauthorized", r#"{"error":"invalid api key"}"#).await;
        let e = fetch_credits(&url, "bad").await.unwrap_err().to_string();
        assert_eq!(e, "authentication failed (check your AI Gateway API key) · HTTP 401 · invalid api key");
    }
}
