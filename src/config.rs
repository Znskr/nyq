//! Locating the profile directory and choosing a model provider.
//!
//! Profile directory: `$NYQ_HOME`, else `~/.fx` (shared with the original app).
//!
//! Provider resolution, first match wins:
//! 1. `--provider` flag or `NYQ_PROVIDER` (`codex` | `openai`)
//! 2. `NYQ_BASE_URL` set → `openai`
//! 3. `settings.json` `"provider"`
//! 4. a ChatGPT login present (codex CLI or `chatgpt-auth.json`) → `codex`
//! 5. `model.json` present → `openai`

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use crate::provider::{codex::CodexProvider, openai::OpenAiProvider, Provider};

pub const DEFAULT_CODEX_MODEL: &str = "gpt-5.6-terra";

pub fn home() -> PathBuf {
    if let Some(h) = std::env::var_os("NYQ_HOME") {
        return PathBuf::from(h);
    }
    dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")).join(".fx")
}

#[derive(Default, Deserialize)]
struct Settings {
    provider: Option<String>,
    #[serde(default)]
    models: std::collections::BTreeMap<String, String>,
    effort: Option<String>,
}

#[derive(Default, Deserialize)]
struct ModelFile {
    base_url: Option<String>,
    model: Option<String>,
    api_key: Option<String>,
}

fn read_json<T: for<'de> Deserialize<'de> + Default>(path: &Path) -> Result<T> {
    match std::fs::read_to_string(path) {
        Ok(s) => serde_json::from_str(&s).with_context(|| format!("parsing {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

/// The ChatGPT login to use: `$NYQ_CODEX_AUTH`, else the codex CLI's login
/// while its access token is still valid (it keeps that file fresh), else
/// `<home>/chatgpt-auth.json`, which nyq refreshes itself.
fn codex_auth_path(home: &Path) -> PathBuf {
    if let Some(p) = env("NYQ_CODEX_AUTH") {
        return PathBuf::from(p);
    }
    let cli = dirs::home_dir().unwrap_or_default().join(".codex").join("auth.json");
    let cli_valid = std::fs::read_to_string(&cli)
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v["tokens"]["access_token"].as_str().and_then(crate::provider::codex::jwt_exp_ms))
        .is_some_and(|exp| exp - 60_000 > chrono::Utc::now().timestamp_millis());
    let own = home.join("chatgpt-auth.json");
    if cli_valid && (std::env::var_os("NYQ_HOME").is_none() || !own.exists()) {
        cli
    } else {
        own
    }
}

pub fn provider(home: &Path, provider_flag: Option<&str>, model_flag: Option<&str>) -> Result<Box<dyn Provider>> {
    let settings: Settings = read_json(&home.join("settings.json"))?;
    let model_file: ModelFile = read_json(&home.join("model.json"))?;
    let auth_path = codex_auth_path(home);

    let kind = provider_flag
        .map(str::to_string)
        .or_else(|| env("NYQ_PROVIDER"))
        .or_else(|| env("NYQ_BASE_URL").map(|_| "openai".into()))
        .or_else(|| settings.provider.clone())
        .or_else(|| auth_path.exists().then(|| "codex".into()))
        .or_else(|| model_file.base_url.as_ref().map(|_| "openai".into()));

    match kind.as_deref() {
        Some("codex") => {
            if !auth_path.exists() {
                bail!("codex provider needs a ChatGPT login: run `codex login`, or set NYQ_CODEX_AUTH");
            }
            let model = model_flag
                .map(str::to_string)
                .or_else(|| env("NYQ_MODEL"))
                .or_else(|| settings.models.get("codex").cloned())
                .unwrap_or_else(|| DEFAULT_CODEX_MODEL.into());
            let effort = settings.effort.filter(|e| e != "auto");
            Ok(Box::new(CodexProvider::new(auth_path, model, effort)))
        }
        Some("openai") => {
            let base_url = env("NYQ_BASE_URL")
                .or(model_file.base_url)
                .context("openai provider needs NYQ_BASE_URL or base_url in model.json")?;
            let model = model_flag
                .map(str::to_string)
                .or_else(|| env("NYQ_MODEL"))
                .or(model_file.model)
                .or_else(|| settings.models.get("openai").cloned())
                .context("openai provider needs a model: --model, NYQ_MODEL, or model in model.json")?;
            let api_key = env("NYQ_API_KEY").or(model_file.api_key);
            Ok(Box::new(OpenAiProvider::new(base_url, api_key, model)))
        }
        Some(other) => bail!("unknown provider `{other}` (expected codex or openai)"),
        None => bail!(
            "no model configured.\n  • ChatGPT subscription: put chatgpt-auth.json in {}\n  • Any OpenAI-compatible API: export NYQ_BASE_URL=… NYQ_MODEL=… [NYQ_API_KEY=…]",
            home.display()
        ),
    }
}

/// Context window in tokens, for automatic compaction: `NYQ_CONTEXT_WINDOW`,
/// else a conservative default per provider. 0 disables auto-compaction.
pub fn context_window(provider: &str) -> u64 {
    if let Some(n) = env("NYQ_CONTEXT_WINDOW").and_then(|v| v.parse().ok()) {
        return n;
    }
    match provider {
        "codex" => 272_000,
        _ => 128_000,
    }
}
