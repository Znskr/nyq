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
use crate::provider::custom::{self, Definition, Protocol};
use crate::provider::hosted::{self, Service};
use crate::ui::{self, tui::Tui};

/// Built-in providers, in menu order; saved compatible providers follow these.
pub const PROVIDERS: [&str; 8] = [
    "codex",
    "grok",
    "anthropic",
    "opencode-go",
    "opencode",
    "xai",
    "openai",
    "ollama",
];
/// Providers `/login` can sign in to (the rest are configured by endpoint).
const LOGIN: [&str; 6] = [
    "codex",
    "grok",
    "anthropic",
    "opencode-go",
    "opencode",
    "xai",
];

const OPENAI_HELP: &str = "Add a named OpenAI- or Anthropic-compatible endpoint with `nyq login custom` or /setup → Connections → Add compatible provider. Existing NYQ_BASE_URL, NYQ_MODEL and NYQ_API_KEY settings still work.";
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
    anyhow!(
        "unknown provider `{name}` (expected {} or a saved custom id; add one with `nyq login custom`)",
        PROVIDERS.join(", ")
    )
}

pub fn title(id: &str) -> &str {
    match id {
        "codex" => "ChatGPT (Codex)",
        "grok" => "Grok subscription",
        "openai" => "OpenAI-compatible API",
        "ollama" => "Ollama (local)",
        other => match Service::parse(other) {
            Some(service) => service.title(),
            None => other,
        },
    }
}

/// Built-in aliases remain separate from user-defined provider ids.
fn resolve(home: &Path, name: &str) -> Result<String> {
    if let Some(id) = canonical(name) {
        return Ok(id.into());
    }
    let id = custom::normalize_id(name)?;
    custom::get(home, &id)?
        .map(|d| d.id)
        .ok_or_else(|| unknown(name))
}

fn provider_ids(home: &Path) -> Result<Vec<String>> {
    let mut ids: Vec<String> = PROVIDERS.iter().map(|id| (*id).into()).collect();
    ids.extend(custom::load(home)?.into_iter().map(|d| d.id));
    Ok(ids)
}

fn labels(home: &Path, ids: &[String]) -> Vec<String> {
    ids.iter()
        .map(|id| format!("{:<24} {}", title(id), connection(home, id).status))
        .collect()
}

/// Sign-in state of one provider. Never contains credentials.
pub struct Connection {
    pub status: String,
    pub connected: bool,
}

pub fn connection(home: &Path, id: &str) -> Connection {
    let Some(id) = canonical(id) else {
        return match custom::get(home, id) {
            Ok(Some(d)) => Connection {
                status: if d.api_key.is_some() {
                    "endpoint configured · API key (providers.json)".into()
                } else {
                    "endpoint configured · no API key".into()
                },
                connected: true,
            },
            Ok(None) => Connection {
                status: "unknown provider".into(),
                connected: false,
            },
            Err(_) => Connection {
                status: "unknown provider or invalid providers.json".into(),
                connected: false,
            },
        };
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
        "ollama" => (
            format!("local, no sign-in ({})", config::ollama_url()),
            false,
        ),
        other => match Service::parse(other).and_then(|s| hosted::key_source(home, s)) {
            Some(source) => (format!("API key ({source})"), true),
            None => ("not connected".into(), false),
        },
    };
    Connection { status, connected }
}

/// Whether nyq itself stores a sign-in for `id` (what `logout` removes).
fn stored_by_nyq(home: &Path, id: &str) -> bool {
    if canonical(id).is_none() {
        return custom::get(home, id)
            .ok()
            .flatten()
            .is_some_and(|d| d.api_key.is_some());
    }
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
    if let Some(m) = read_settings(home)["models"][id]
        .as_str()
        .filter(|m| !m.trim().is_empty())
    {
        return Some(m.to_string());
    }
    match id {
        "codex" => Some(config::DEFAULT_CODEX_MODEL.into()),
        "grok" => Some(crate::provider::grok::DEFAULT_MODEL.into()),
        "openai" => config::openai_config(home).ok().and_then(|c| c.2),
        "ollama" => None,
        other => Service::parse(other)
            .map(|s| s.default_model())
            .or_else(|| custom::get(home, other).ok().flatten().map(|d| d.model)),
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
            let choice = ModelChoice {
                provider: id.into(),
                model: m.clone(),
                ..Default::default()
            };
            crate::models::save_last(home, &choice, None)?;
        }
        None => match std::fs::remove_file(home.join("last-model.json")) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                return Err(e).context("removing last-model.json")
            }
            _ => {}
        },
    }
    Ok(model)
}

/// `nyq provider NAME`, `/setup` → Model provider. Returns a message and the
/// model to switch to now (when known).
pub fn set_default_provider(home: &Path, name: &str) -> Result<(String, Option<ModelChoice>)> {
    let id = resolve(home, name)?;
    let id = id.as_str();
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
        "ollama" if model.is_none() => {
            msg.push_str("\nSet a model with --model or NYQ_MODEL, or pick one with /model.")
        }
        "openai" | "ollama" => {}
        _ if !connection(home, id).connected => {
            msg.push_str(&format!("\nNot signed in yet: run `nyq login {id}`."))
        }
        _ => {}
    }
    for var in ["NYQ_PROVIDER", "NYQ_BASE_URL"] {
        if std::env::var(var).is_ok_and(|v| !v.trim().is_empty()) {
            msg.push_str(&format!("\nNote: {var} is set and takes precedence."));
        }
    }
    let choice = model.map(|model| ModelChoice {
        provider: id.into(),
        model,
        ..Default::default()
    });
    Ok((msg, choice))
}

/// What a plain `nyq` would start with.
fn default_summary(home: &Path) -> String {
    match config::provider(home, None, None) {
        Ok(p) => format!("{} · {}", p.name(), p.model()),
        Err(e) => format!(
            "not ready: {}",
            format!("{e:#}").lines().next().unwrap_or_default()
        ),
    }
}

fn connection_lines(home: &Path) -> Vec<String> {
    match provider_ids(home) {
        Ok(ids) => labels(home, &ids)
            .into_iter()
            .map(|line| format!("  {line}"))
            .collect(),
        Err(e) => {
            let mut lines = labels(home, &PROVIDERS.map(String::from));
            lines.push(format!("Compatible providers: {e:#}"));
            lines
        }
    }
}

/// Which credential each provider uses, and the order nyq looks for them.
fn source_lines(home: &Path) -> Vec<String> {
    let mut lines = vec!["Credential sources".to_string()];
    lines.extend(connection_lines(home));
    lines.push("API keys: the provider's environment variable, then the OpenCode CLI's login (OpenCode only), then nyq's auth.json.".into());
    lines.push(
        "ChatGPT: NYQ_CODEX_AUTH, then a valid codex CLI login, then nyq's own sign-in.".into(),
    );
    lines.push("Grok subscription: nyq's sign-in in grok-auth.json.".into());
    lines.push("Compatible providers: each named endpoint's own key in providers.json (or explicitly no key); built-in environment keys are not inherited.".into());
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
            println!("Providers    {}", provider_ids(home)?.join(", "));
            println!("Set one with `nyq provider NAME`.");
        }
    }
    Ok(())
}

/// `nyq permissions [--json]`: the permission mode (as resolved from
/// `--permission-mode`, `--yolo` and settings) and the rules.
pub fn cli_permissions(
    home: &Path,
    cwd: &Path,
    mode: crate::permissions::Mode,
    as_json: bool,
) -> Result<()> {
    let (perms, warnings) = crate::permissions::Permissions::load(home, cwd);
    println!(
        "{}",
        crate::permissions::format_report(&perms, mode, &warnings, as_json)
    );
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
    Ok(read_line("> ")?
        .and_then(|a| a.parse::<usize>().ok())
        .filter(|n| (1..=options.len()).contains(n))
        .map(|n| n - 1))
}

/// Shared guided prompts: keys always use the dedicated hidden reader, never input.
struct ConnectionPrompt<'a>(Option<&'a Tui>);

impl ConnectionPrompt<'_> {
    async fn choose(&self, question: &str, options: &[String]) -> Result<Option<usize>> {
        match self.0 {
            Some(tui) => Ok(tui.pick("Compatible provider", question, options).await),
            None => choose(question, options),
        }
    }

    async fn input(&self, question: &str) -> Result<Option<String>> {
        match self.0 {
            Some(tui) => Ok(tui.input(question).await),
            None => Ok(read_line(&format!("{question}: "))?.filter(|s| !s.is_empty())),
        }
    }

    async fn key(&self) -> Result<Option<Option<String>>> {
        match self
            .choose(
                "Authentication",
                &[
                    "API key (hidden input)".into(),
                    "No key (unauthenticated endpoint)".into(),
                ],
            )
            .await?
        {
            Some(0) => {
                let key = match self.0 {
                    Some(tui) => tui.secret("API key (hidden; blank cancels)").await,
                    None => {
                        eprintln!("API key (hidden; blank cancels):");
                        Some(crate::login::read_secret()?)
                    }
                };
                Ok(key.filter(|k| !k.trim().is_empty()).map(Some))
            }
            Some(1) => Ok(Some(None)),
            _ => Ok(None),
        }
    }

    async fn discover(&self, definition: &Definition) -> Result<Option<Vec<ModelChoice>>> {
        if let Some(tui) = self.0 {
            tui.screen()
                .set_busy(Some("Discovering models (up to 6 seconds)"), None);
        }
        let result = custom::discover(definition).await;
        if let Some(tui) = self.0 {
            tui.screen().idle();
        }
        discovery_result(result)
    }

    fn info(&self, message: &str) {
        if self.0.is_some() {
            ui::info(message);
        } else {
            println!("{message}");
        }
    }
}

/// Offline/unsupported discovery may fall back; explicit credential rejection may not.
fn discovery_result(result: Result<Vec<ModelChoice>>) -> Result<Option<Vec<ModelChoice>>> {
    match result {
        Ok(models) if !models.is_empty() => Ok(Some(models)),
        Err(e) if custom::is_auth_error(&e) => Err(e)
            .context("Access rejected; nothing saved (any existing provider/key is unchanged)"),
        _ => Ok(None),
    }
}

fn custom_saved(id: &str) -> String {
    format!("Saved compatible provider `{id}`. Its models are available in /model. Set it as the default with `nyq provider {id}` or /setup → Model provider.")
}

async fn add_custom(home: &Path, prompt: &ConnectionPrompt<'_>) -> Result<String> {
    let cancelled = || "No provider saved.".to_string();
    let protocol = match prompt
        .choose(
            "API protocol (choose explicitly)",
            &[
                "OpenAI-compatible (Chat Completions)".into(),
                "Anthropic-compatible (Messages)".into(),
            ],
        )
        .await?
    {
        Some(0) => Protocol::Openai,
        Some(1) => Protocol::Anthropic,
        _ => return Ok(cancelled()),
    };
    let Some(id) = prompt
        .input("Unique provider id (for example, work-gateway)")
        .await?
    else {
        return Ok(cancelled());
    };
    let id = custom::normalize_id(&id)?;
    if custom::get(home, &id)?.is_some()
        && prompt
            .choose(
                "This id already exists. Replace its entire configuration?",
                &[
                    "Cancel (keep existing provider)".into(),
                    "Replace provider".into(),
                ],
            )
            .await?
            != Some(1)
    {
        return Ok(cancelled());
    }
    let Some(base_url) = prompt
        .input("Base URL (include the API prefix, e.g. https://host/v1; no credentials)")
        .await?
    else {
        return Ok(cancelled());
    };
    let Some(api_key) = prompt.key().await? else {
        return Ok(cancelled());
    };
    let mut definition = Definition {
        id,
        protocol,
        base_url,
        api_key,
        model: String::new(),
    };
    let models = prompt.discover(&definition).await?;
    if let Some(models) = models {
        let mut options: Vec<String> = models.iter().map(|m| m.model.clone()).collect();
        options.push("Enter a model id manually".into());
        match prompt
            .choose("Initial/default model for this provider", &options)
            .await?
        {
            Some(i) if i < models.len() => definition.model = models[i].model.clone(),
            Some(_) => {}
            None => return Ok(cancelled()),
        }
    } else {
        prompt.info("Model discovery is unavailable (offline or unsupported). Enter a model id manually; the endpoint/key could not be verified.");
    }
    if definition.model.is_empty() {
        let Some(model) = prompt.input("Initial/default model id").await? else {
            return Ok(cancelled());
        };
        definition.model = model;
    }
    custom::save(home, &definition)?;
    Ok(custom_saved(&definition.id))
}

fn remove_custom(home: &Path, id: &str) -> Result<String> {
    custom::remove(home, id)?;
    if read_settings(home)["provider"].as_str() == Some(id) {
        crate::settings::file::write_changes(home, &[(vec!["provider".into()], None)])?;
        let mut current = crate::settings::current();
        if current.extra.get("provider").and_then(Value::as_str) == Some(id) {
            current.extra.remove("provider");
            crate::settings::install(current);
        }
    }
    let last = home.join("last-model.json");
    if std::fs::read(&last)
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .is_some_and(|v| v["provider"].as_str() == Some(id))
    {
        std::fs::remove_file(last).context("removing the removed provider's last model")?;
    }
    Ok(format!("Removed compatible provider `{id}` and its key. If it is active in this session, switch with /model."))
}

async fn replace_custom_key(
    home: &Path,
    prompt: &ConnectionPrompt<'_>,
    definition: Definition,
) -> Result<String> {
    let Some(key) = prompt.key().await? else {
        return Ok("No key saved.".into());
    };
    store_custom_key(home, prompt, definition, key).await
}

async fn store_custom_key(
    home: &Path,
    prompt: &ConnectionPrompt<'_>,
    mut definition: Definition,
    key: Option<String>,
) -> Result<String> {
    definition.api_key = key;
    if prompt.discover(&definition).await?.is_none() {
        prompt.info("Could not verify the endpoint/key (offline or discovery unsupported); keeping its configured model.");
    }
    custom::save(home, &definition)?;
    Ok(custom_saved(&definition.id))
}

/// Sign in to `id` from the command line (browser or hidden key prompt).
pub async fn cli_login(home: &Path, id: &str) -> Result<String> {
    if id.eq_ignore_ascii_case("custom") {
        return add_custom(home, &ConnectionPrompt(None)).await;
    }
    let id = resolve(home, id)?;
    if canonical(&id).is_none() {
        return replace_custom_key(
            home,
            &ConnectionPrompt(None),
            custom::get(home, &id)?.ok_or_else(|| unknown(&id))?,
        )
        .await;
    }
    match id.as_str() {
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
        other => {
            crate::login::login_key(home, Service::parse(other).ok_or_else(|| unknown(other))?)
                .await
        }
    }
}

/// `nyq setup`: a summary; on a terminal, prompts to sign in and pick the default.
pub async fn cli_setup(home: &Path) -> Result<()> {
    use std::io::IsTerminal;
    summary_lines(home).iter().for_each(|l| println!("{l}"));
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        println!("\nSign in with `nyq login NAME`, add a compatible endpoint with `nyq login custom`, choose the default with `nyq provider NAME`, or run /setup inside nyq.");
        return Ok(());
    }
    loop {
        let ids = provider_ids(home)?;
        let actions = [
            "Connections: sign in, replace a key, or add compatible provider",
            "Sign out (remove stored credentials only)",
            "Choose the default provider",
            "Remove a compatible provider",
            "Done",
        ]
        .map(String::from);
        match choose("What would you like to do?", &actions)? {
            Some(0) => {
                let mut options = labels(home, &ids);
                options.push("Add compatible provider".into());
                if let Some(i) = choose("Connections", &options)? {
                    let id = ids.get(i).map_or("custom", String::as_str);
                    match cli_login(home, id).await {
                        Ok(msg) => println!("{msg}"),
                        Err(e) => ui::error(&format!("{e:#}")),
                    }
                }
            }
            Some(1) => {
                let stored: Vec<String> = ids
                    .into_iter()
                    .filter(|id| stored_by_nyq(home, id))
                    .collect();
                if stored.is_empty() {
                    println!("No sign-ins stored by nyq.");
                } else if let Some(i) = choose("Sign out of", &labels(home, &stored))? {
                    println!("{}", crate::login::logout(home, &stored[i]).await?);
                }
            }
            Some(2) => {
                if let Some(i) = choose("Default provider", &labels(home, &ids))? {
                    println!("{}", set_default_provider(home, &ids[i])?.0);
                }
            }
            Some(3) => {
                let custom: Vec<String> = custom::load(home)?.into_iter().map(|d| d.id).collect();
                if custom.is_empty() {
                    println!("No compatible providers configured.");
                } else if let Some(i) = choose("Remove compatible provider", &custom)? {
                    if choose(
                        &format!("Remove `{}` and its saved key?", custom[i]),
                        &["Cancel".into(), "Remove provider".into()],
                    )? == Some(1)
                    {
                        println!("{}", remove_custom(home, &custom[i])?);
                    }
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
        Some(n) if n.eq_ignore_ascii_case("custom") => "custom".into(),
        Some(n) => resolve(home, n)?,
        None => {
            let mut ids: Vec<String> = LOGIN.iter().map(|id| (*id).into()).collect();
            ids.extend(custom::load(home)?.into_iter().map(|d| d.id));
            let mut options = labels(home, &ids);
            options.push("Add compatible provider".into());
            match tui.pick("Sign in", "Choose a provider", &options).await {
                Some(i) => ids.get(i).cloned().unwrap_or_else(|| "custom".into()),
                None => return Ok(()),
            }
        }
    };
    if id == "custom" {
        ui::info(&add_custom(home, &ConnectionPrompt(Some(tui))).await?);
        return Ok(());
    }
    if canonical(&id).is_none() {
        let definition = custom::get(home, &id)?.ok_or_else(|| unknown(&id))?;
        ui::info(&replace_custom_key(home, &ConnectionPrompt(Some(tui)), definition).await?);
        return Ok(());
    }
    match id.as_str() {
        "openai" => ui::info(OPENAI_HELP),
        "ollama" => ui::info(OLLAMA_HELP),
        "codex" => {
            let cancel = CancellationToken::new();
            tui.screen()
                .set_busy(Some("Signing in to ChatGPT"), Some(cancel.clone()));
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
            tui.screen()
                .set_busy(Some("Signing in to Grok"), Some(cancel.clone()));
            let _hide = HideSecret(tui);
            let prompt =
                "Paste the Grok authorization code if the browser does not return (hidden)";
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
        Some(n) => resolve(home, n)?,
        None => {
            let stored: Vec<String> = provider_ids(home)?
                .into_iter()
                .filter(|id| stored_by_nyq(home, id))
                .collect();
            if stored.is_empty() {
                ui::info("No sign-ins stored by nyq (keys from environment variables or other tools stay where they are).");
                return Ok(());
            }
            let options: Vec<String> = stored.iter().map(|id| title(id).to_string()).collect();
            match tui
                .pick("Sign out", "Forget nyq's stored sign-in for", &options)
                .await
            {
                Some(i) => stored[i].clone(),
                None => return Ok(()),
            }
        }
    };
    anyhow::ensure!(
        LOGIN.contains(&id.as_str()) || canonical(&id).is_none(),
        "{} has no sign-in to remove",
        title(&id)
    );
    ui::info(&crate::login::logout(home, &id).await?);
    Ok(())
}

async fn tui_setup(home: &Path, agent: &mut Agent, tui: &Tui) -> Result<()> {
    loop {
        let connected = provider_ids(home)?
            .iter()
            .filter(|id| connection(home, id).connected)
            .count();
        let current = agent.provider.name().to_string();
        let options = vec![
            format!("{:<20} {connected} connected", "Connections"),
            format!("{:<20} {current}", "Model provider"),
            format!("{:<20} {}", "Model", agent.provider.model()),
            format!(
                "{:<20} {}",
                "Credential source",
                connection(home, &current).status
            ),
        ];
        let Some(choice) = tui
            .pick("Setup", "Accounts and model provider", &options)
            .await
        else {
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
    let ids = provider_ids(home)?;
    let mut options = labels(home, &ids);
    options.push("Add compatible provider".into());
    let Some(i) = tui
        .pick(
            "Connections",
            "Sign in, replace a key, or sign out",
            &options,
        )
        .await
    else {
        return Ok(());
    };
    let Some(id) = ids.get(i).map(String::as_str) else {
        return tui_login(home, tui, Some("custom")).await;
    };
    if canonical(id).is_none() {
        let options = [
            "Replace the API key / choose no key".into(),
            "Sign out (remove key only)".into(),
            "Remove provider configuration".into(),
        ];
        return match tui
            .pick(title(id), "Manage compatible provider", &options)
            .await
        {
            Some(0) => tui_login(home, tui, Some(id)).await,
            Some(1) => tui_logout(home, tui, Some(id)).await,
            Some(2) => {
                if tui
                    .pick(
                        "Remove provider",
                        &format!("Remove `{id}` and its saved key?"),
                        &["Cancel".into(), "Remove provider".into()],
                    )
                    .await
                    == Some(1)
                {
                    ui::info(&remove_custom(home, id)?);
                }
                Ok(())
            }
            _ => Ok(()),
        };
    }
    if !stored_by_nyq(home, id) {
        return tui_login(home, tui, Some(id)).await;
    }
    let again = if id == "codex" || id == "grok" {
        "Sign in again"
    } else {
        "Replace the API key"
    };
    match tui
        .pick(
            title(id),
            "Stored by nyq",
            &[again.to_string(), "Sign out".to_string()],
        )
        .await
    {
        Some(0) => tui_login(home, tui, Some(id)).await,
        Some(_) => tui_logout(home, tui, Some(id)).await,
        None => Ok(()),
    }
}

async fn tui_provider(home: &Path, agent: &mut Agent, tui: &Tui) -> Result<()> {
    let ids = provider_ids(home)?;
    let options: Vec<String> = ids
        .iter()
        .map(|id| {
            let state = if id == agent.provider.name() {
                "current"
            } else if connection(home, id).connected {
                "connected"
            } else {
                "not connected"
            };
            format!("{:<24} {state}", title(id))
        })
        .collect();
    let Some(i) = tui
        .pick(
            "Model provider",
            "Choose the default provider (remembered for new sessions)",
            &options,
        )
        .await
    else {
        return Ok(());
    };
    let (msg, choice) = set_default_provider(home, &ids[i])?;
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
        assert!(PROVIDERS
            .iter()
            .all(|p| canonical(p) == Some(*p) && title(p) != "unknown"));
    }

    fn compatible(id: &str, key: Option<&str>) -> Definition {
        Definition {
            id: id.into(),
            protocol: Protocol::Openai,
            base_url: "http://127.0.0.1:1/v1".into(),
            api_key: key.map(String::from),
            model: "gateway-model".into(),
        }
    }

    #[test]
    fn compatible_connection_and_credential_states() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        custom::save(home, &compatible("work-gateway", Some("private-test-key"))).unwrap();
        custom::save(home, &compatible("local-gateway", None)).unwrap();
        assert_eq!(canonical("work-gateway"), None);
        assert_eq!(resolve(home, " Work-Gateway ").unwrap(), "work-gateway");
        assert_eq!(resolve(home, "Claude").unwrap(), "anthropic");
        assert!(resolve(home, "missing").is_err());
        assert_eq!(title(&String::from("work-gateway")), "work-gateway");
        let ids = provider_ids(home).unwrap();
        assert!(ids.contains(&"work-gateway".into()));
        assert!(ids.contains(&"local-gateway".into()));
        assert!(connection(home, "work-gateway").connected);
        assert!(connection(home, "work-gateway")
            .status
            .contains("API key (providers.json)"));
        assert!(connection(home, "local-gateway").connected);
        assert!(connection(home, "local-gateway")
            .status
            .contains("no API key"));
        assert!(stored_by_nyq(home, "work-gateway"));
        assert!(!stored_by_nyq(home, "local-gateway"));
        let summary = summary_lines(home).join("\n");
        let sources = source_lines(home).join("\n");
        assert!(summary.contains("work-gateway"));
        assert!(sources.contains("local-gateway"));
        assert!(!summary.contains("private-test-key"));
        assert!(!sources.contains("private-test-key"));
    }

    #[test]
    fn compatible_default_model_and_removal() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        custom::save(home, &compatible("work-gateway", None)).unwrap();
        assert_eq!(
            persist_default(home, "work-gateway").unwrap().as_deref(),
            Some("gateway-model")
        );
        assert_eq!(read_settings(home)["provider"], "work-gateway");
        let last: Value =
            serde_json::from_slice(&std::fs::read(home.join("last-model.json")).unwrap()).unwrap();
        assert_eq!(last["provider"], "work-gateway");
        assert_eq!(last["model"], "gateway-model");
        crate::settings::file::write_changes(
            home,
            &[(
                vec!["models".into(), "work-gateway".into()],
                Some(json!("preferred-model")),
            )],
        )
        .unwrap();
        assert_eq!(
            default_model(home, "work-gateway").as_deref(),
            Some("preferred-model")
        );
        remove_custom(home, "work-gateway").unwrap();
        assert!(custom::get(home, "work-gateway").unwrap().is_none());
        assert!(read_settings(home)["provider"].is_null());
        assert!(!home.join("last-model.json").exists());
        assert!(resolve(home, "work-gateway").is_err());
    }

    #[tokio::test]
    async fn compatible_key_replacement_rejects_auth_without_saving() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        for status in [401, 403] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let task = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = vec![0; 4096];
                let _ = socket.read(&mut request).await.unwrap();
                socket.write_all(format!("HTTP/1.1 {status} Denied\r\nContent-Length: 16\r\nConnection: close\r\n\r\nprivate-test-key").as_bytes()).await.unwrap();
            });
            let dir = tempfile::tempdir().unwrap();
            let home = dir.path();
            let mut definition = compatible("work-gateway", Some("old-key"));
            definition.base_url = format!("http://{addr}/v1");
            custom::save(home, &definition).unwrap();
            let before = std::fs::read(home.join("providers.json")).unwrap();
            let error = store_custom_key(
                home,
                &ConnectionPrompt(None),
                definition,
                Some("private-test-key".into()),
            )
            .await
            .unwrap_err();
            assert!(custom::is_auth_error(&error));
            assert!(!format!("{error:#}").contains("private-test-key"));
            assert_eq!(std::fs::read(home.join("providers.json")).unwrap(), before);
            task.await.unwrap();
        }
    }

    #[tokio::test]
    async fn compatible_key_replacement_offline_keeps_endpoint_and_model() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let definition = compatible("work-gateway", Some("old-key"));
        custom::save(home, &definition).unwrap();
        // A malformed discovery response is unsupported, not credential rejection.
        assert!(discovery_result(Ok(Vec::new())).unwrap().is_none());
        assert!(discovery_result(Err(anyhow!("offline"))).unwrap().is_none());
        store_custom_key(
            home,
            &ConnectionPrompt(None),
            definition.clone(),
            Some("new-key".into()),
        )
        .await
        .unwrap();
        let saved = custom::get(home, "work-gateway").unwrap().unwrap();
        assert_eq!(saved.api_key.as_deref(), Some("new-key"));
        assert_eq!(saved.base_url, definition.base_url);
        assert_eq!(saved.model, definition.model);
        assert!(cli_login(home, "missing").await.is_err());
        assert!(cli_login(home, "OPENAI")
            .await
            .unwrap()
            .contains("nyq login custom"));
        assert!(cli_login(home, "Ollama")
            .await
            .unwrap()
            .contains("no sign-in"));
    }

    #[test]
    fn default_provider_persists() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        std::fs::write(
            home.join("settings.toml"),
            "# keep me\nsound = \"max\"\nother = 1\n\n[models]\nxai = \"grok-custom\"\n",
        )
        .unwrap();
        std::fs::write(
            home.join("last-model.json"),
            r#"{"provider":"codex","model":"old","effort":"high"}"#,
        )
        .unwrap();
        assert_eq!(
            persist_default(home, "xai").unwrap().as_deref(),
            Some("grok-custom")
        );
        let settings = read_settings(home);
        assert_eq!(settings["provider"], "xai");
        assert!(std::fs::read_to_string(home.join("settings.toml"))
            .unwrap()
            .starts_with("# keep me\n"));
        assert_eq!(
            (settings["sound"].as_str(), settings["other"].as_i64()),
            (Some("max"), Some(1))
        );
        let last: Value =
            serde_json::from_slice(&std::fs::read(home.join("last-model.json")).unwrap()).unwrap();
        assert_eq!(
            (last["provider"].as_str(), last["model"].as_str()),
            (Some("xai"), Some("grok-custom"))
        );
        assert!(last["effort"].is_null());
        // Built-in default model, then a provider without one drops last-model.json.
        assert_eq!(
            persist_default(home, "opencode").unwrap().as_deref(),
            Some(Service::Opencode.default_model().as_str())
        );
        assert_eq!(persist_default(home, "ollama").unwrap(), None);
        assert!(!home.join("last-model.json").exists());
        assert_eq!(read_settings(home)["provider"], "ollama");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(home.join("settings.toml"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        assert!(set_default_provider(home, "bogus").is_err());
    }
}
