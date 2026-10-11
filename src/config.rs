//! Locating the profile directory and choosing a model provider.
//!
//! Profile directory: `$NYQ_HOME`, else `~/.nyq`.
//!
//! Provider resolution, first match wins:
//! 1. `--provider` flag or `NYQ_PROVIDER` (`codex` | `openai` | `ollama` |
//!    `opencode-go` | `opencode` | `xai`)
//! 2. `NYQ_BASE_URL` set → `openai`
//! 3. last-model.json (unless explicit model/provider flags or env are set)
//! 4. `settings.json` `"provider"`
//! 5. a ChatGPT login present (codex CLI or `chatgpt-auth.json`) → `codex`
//! 6. `model.json` present → `openai`
//! 7. an API key for a hosted service (OpenCode Go, OpenCode Zen, xAI) →
//!    that service

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::provider::{
    codex::CodexProvider,
    hosted::{HostedProvider, Service},
    openai::OpenAiProvider,
    Provider,
};

pub const DEFAULT_CODEX_MODEL: &str = "gpt-6-sol";

/// Client identification sent to hosted model APIs.
pub const USER_AGENT: &str = concat!("nyq/", env!("CARGO_PKG_VERSION"));

/// Atomically replace `path` with `bytes`, readable only by the user (0600).
pub(crate) fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)?;
    let temp = parent.join(format!(".nyq-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        crate::tools::safeio::rename_over(&temp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result.with_context(|| format!("writing {}", path.display()))
}

pub fn home() -> PathBuf {
    if let Some(h) = std::env::var_os("NYQ_HOME") {
        return PathBuf::from(h);
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".nyq")
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

#[derive(Default, Deserialize, Serialize)]
pub(crate) struct LastModel {
    pub provider: String,
    pub model: String,
    pub effort: Option<String>,
}

pub(crate) fn openai_config(
    home: &Path,
) -> Result<(Option<String>, Option<String>, Option<String>)> {
    let file: ModelFile = read_json(&home.join("model.json"))?;
    let settings: Settings = read_json(&home.join("settings.json"))?;
    Ok((
        env("NYQ_BASE_URL").or(file.base_url),
        env("NYQ_API_KEY").or(file.api_key),
        env("NYQ_MODEL")
            .or(file.model)
            .or_else(|| settings.models.get("openai").cloned()),
    ))
}

pub(crate) fn ollama_url() -> String {
    ollama_url_from(
        env("OLLAMA_HOST")
            .as_deref()
            .unwrap_or("http://127.0.0.1:11434"),
    )
}

fn ollama_url_from(host: &str) -> String {
    let host = host.trim().trim_end_matches('/');
    let url = if host.contains("://") {
        host.to_owned()
    } else {
        format!("http://{host}")
    };
    if url.ends_with("/v1") {
        url
    } else {
        format!("{url}/v1")
    }
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
pub(crate) fn access_token_fresh(exp_ms: i64, now_ms: i64) -> bool {
    exp_ms.saturating_sub(60_000) > now_ms
}

pub(crate) fn codex_auth_path(home: &Path) -> PathBuf {
    if let Some(p) = env("NYQ_CODEX_AUTH") {
        return PathBuf::from(p);
    }
    let cli = dirs::home_dir()
        .unwrap_or_default()
        .join(".codex")
        .join("auth.json");
    let cli_valid = std::fs::read_to_string(&cli)
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| {
            v["tokens"]["access_token"]
                .as_str()
                .and_then(crate::provider::codex::jwt_exp_ms)
        })
        .is_some_and(|exp| access_token_fresh(exp, chrono::Utc::now().timestamp_millis()));
    let own = home.join("chatgpt-auth.json");
    if cli_valid && (std::env::var_os("NYQ_HOME").is_none() || !own.exists()) {
        cli
    } else {
        own
    }
}

/// The error for a provider name nyq does not know, which may come from a
/// flag, the environment, or a saved setting (settings.json, last-model.json).
pub fn unknown_provider(name: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "unknown provider `{name}` (expected codex, ollama, openai, opencode-go, opencode or xai). \
         If it is saved in settings.json or last-model.json, choose another with `nyq provider NAME` or /model."
    )
}

pub fn provider(
    home: &Path,
    provider_flag: Option<&str>,
    model_flag: Option<&str>,
) -> Result<Box<dyn Provider>> {
    let settings: Settings = read_json(&home.join("settings.json"))?;
    let model_file: ModelFile = read_json(&home.join("model.json"))?;
    let auth_path = codex_auth_path(home);
    let explicit = provider_flag.is_some()
        || model_flag.is_some()
        || ["NYQ_PROVIDER", "NYQ_BASE_URL", "NYQ_MODEL"]
            .iter()
            .any(|k| env(k).is_some());
    if !explicit {
        let path = home.join("last-model.json");
        if path.exists() {
            let last: LastModel = read_json(&path)?;
            return crate::models::build(
                home,
                &crate::models::ModelChoice {
                    provider: last.provider,
                    model: last.model,
                    ..Default::default()
                },
                last.effort,
            );
        }
    }

    let kind = provider_flag
        .map(str::to_string)
        .or_else(|| env("NYQ_PROVIDER"))
        .or_else(|| env("NYQ_BASE_URL").map(|_| "openai".into()))
        .or_else(|| settings.provider.clone())
        .or_else(|| auth_path.exists().then(|| "codex".into()))
        .or_else(|| model_file.base_url.as_ref().map(|_| "openai".into()))
        .or_else(|| {
            Service::ALL
                .into_iter()
                .find(|s| crate::provider::hosted::api_key(home, *s).is_some())
                .map(|s| s.id().into())
        });

    match kind.as_deref() {
        Some("codex") => {
            if !auth_path.exists() {
                bail!("codex provider needs a ChatGPT login: run `nyq login` (or `codex login`), or set NYQ_CODEX_AUTH");
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
        Some("ollama") => {
            let model = model_flag.map(str::to_owned).or_else(|| env("NYQ_MODEL"))
                .or_else(|| settings.models.get("ollama").cloned())
                .context("ollama provider needs a model: --model or NYQ_MODEL")?;
            Ok(Box::new(OpenAiProvider::with_label("ollama".into(), ollama_url(), None, model)))
        }
        Some(other) => {
            let service = Service::parse(other).ok_or_else(|| unknown_provider(other))?;
            let model = model_flag
                .map(str::to_string)
                .or_else(|| env("NYQ_MODEL"))
                .or_else(|| settings.models.get(service.id()).cloned())
                .unwrap_or_else(|| service.default_model().into());
            let effort = settings.effort.filter(|e| e != "auto");
            Ok(Box::new(HostedProvider::configured(home, service, model, effort)?))
        }
        None => bail!(
            "no model configured.\n  • ChatGPT subscription: run `nyq login`\n  • OpenCode Go/Zen or xAI: run `nyq login opencode-go|opencode|xai` or set its API key variable\n  • Any OpenAI-compatible API: export NYQ_BASE_URL=… NYQ_MODEL=… [NYQ_API_KEY=…]"
        ),
    }
}

pub fn context_window_for(provider: &str, reported: Option<u64>) -> u64 {
    if let Some(n) = env("NYQ_CONTEXT_WINDOW").and_then(|v| v.parse().ok()) {
        return n;
    }
    if let Some(n) = reported {
        return n;
    }
    match provider {
        "ollama" => 32_768,
        "codex" => 272_000,
        _ => 128_000,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_deadline_does_not_overflow() {
        assert!(!access_token_fresh(i64::MIN, 0));
        assert!(!access_token_fresh(0, 1));
        assert!(!access_token_fresh(60_000, 60_000));
        assert!(access_token_fresh(i64::MAX, 0));
    }

    #[test]
    fn ollama_hosts() {
        for (host, expected) in [
            ("localhost:11434", "http://localhost:11434/v1"),
            ("https://example.test/", "https://example.test/v1"),
            ("http://localhost:11434/v1/", "http://localhost:11434/v1"),
        ] {
            assert_eq!(ollama_url_from(host), expected);
        }
    }

    #[test]
    fn saved_precedence() {
        let _lock = crate::provider::sse::tests::ENV.lock().unwrap_or_else(|e| e.into_inner());
        let keys = [
            "NYQ_HOME",
            "NYQ_PROVIDER",
            "NYQ_BASE_URL",
            "NYQ_MODEL",
            "NYQ_CODEX_AUTH",
            "NYQ_CONTEXT_WINDOW",
        ];
        let old: Vec<_> = keys.iter().map(std::env::var_os).collect();
        struct Restore(Vec<(&'static str, Option<std::ffi::OsString>)>);
        impl Drop for Restore {
            fn drop(&mut self) {
                for (key, value) in &self.0 {
                    if let Some(value) = value {
                        std::env::set_var(key, value);
                    } else {
                        std::env::remove_var(key);
                    }
                }
            }
        }
        let _restore = Restore(keys.into_iter().zip(old).collect());
        for key in keys {
            std::env::remove_var(key);
        }
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("NYQ_HOME", dir.path());
        std::env::set_var("NYQ_CODEX_AUTH", dir.path().join("auth.json"));
        std::fs::write(dir.path().join("auth.json"), "{}").unwrap();
        std::fs::write(dir.path().join("settings.json"), r#"{"provider":"codex"}"#).unwrap();
        std::fs::write(
            dir.path().join("model.json"),
            r#"{"base_url":"http://example.test/v1","model":"configured"}"#,
        )
        .unwrap();
        let choice = crate::models::ModelChoice {
            provider: "ollama".into(),
            model: "local".into(),
            ..Default::default()
        };
        crate::models::save_last(&home(), &choice, Some("high")).unwrap();
        let last: LastModel = read_json(&dir.path().join("last-model.json")).unwrap();
        assert_eq!(
            (
                last.provider.as_str(),
                last.model.as_str(),
                last.effort.as_deref()
            ),
            ("ollama", "local", Some("high"))
        );
        let p = provider(&home(), None, None).unwrap();
        assert_eq!((p.name(), p.model()), ("ollama", "local"));
        assert_eq!(
            provider(&home(), Some("openai"), None).unwrap().model(),
            "configured"
        );
        assert_eq!(
            provider(&home(), None, Some("flag")).unwrap().model(),
            "flag"
        );
        for key in ["NYQ_PROVIDER", "NYQ_BASE_URL", "NYQ_MODEL"] {
            std::env::set_var(
                key,
                match key {
                    "NYQ_PROVIDER" => "openai",
                    "NYQ_BASE_URL" => "http://example.test/v1",
                    _ => "env-model",
                },
            );
            assert_ne!(provider(&home(), None, None).unwrap().name(), "ollama");
            std::env::remove_var(key);
        }
        assert_eq!(context_window_for("ollama", None), 32768);
        assert_eq!(context_window_for("codex", Some(123)), 123);
        std::env::set_var("NYQ_CONTEXT_WINDOW", "0");
        assert_eq!(context_window_for("codex", Some(123)), 0);
    }
}
