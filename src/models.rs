//! Model catalog across providers, for the `/model` switcher.

use std::path::Path;

use anyhow::Result;

use crate::provider::Provider;

/// One selectable model.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelChoice {
    /// Provider group shown as a tab: "codex", "ollama", or "openai".
    pub provider: String,
    pub model: String,
    /// Tokens, when the provider reports it.
    pub context_window: Option<u64>,
    /// Supported reasoning efforts, lowest first (empty if not adjustable).
    pub efforts: Vec<String>,
}

/// All models from every reachable provider (Codex login, local Ollama, the
/// configured OpenAI-compatible endpoint). Unreachable providers are skipped
/// and reported as warnings. Never takes longer than a few seconds.
pub async fn list(home: &Path) -> (Vec<ModelChoice>, Vec<String>) {
    use crate::{
        config,
        provider::{codex::CodexProvider, openai::OpenAiProvider},
    };
    let auth = config::codex_auth_path(home);
    let ollama_url = config::ollama_url();
    let codex = async {
        if !auth.exists() {
            return Ok(Vec::new());
        }
        let provider = CodexProvider::new(auth, config::DEFAULT_CODEX_MODEL.into(), None);
        tokio::time::timeout(std::time::Duration::from_secs(6), provider.list_models())
            .await
            .map_err(anyhow::Error::from)?
    };
    let ollama = async {
        let provider =
            OpenAiProvider::with_label("ollama".into(), ollama_url.clone(), None, String::new());
        provider.list_models().await.unwrap_or_default()
    };
    let openai = async {
        let (url, key, model) = config::openai_config(home)?;
        let Some(url) = url.filter(|url| !same_url(url, &ollama_url)) else {
            return Ok((Vec::new(), None));
        };
        let provider = OpenAiProvider::new(url, key, model.clone().unwrap_or_default());
        match provider.list_models().await {
            Ok(models) => Ok((models, None)),
            Err(e) => Ok((model.into_iter().collect(), Some(format!("openai: {e}")))),
        }
    };
    let (codex, ollama, openai): (_, _, Result<_>) = tokio::join!(codex, ollama, openai);
    let mut warnings = Vec::new();
    let mut choices = match codex {
        Ok(models) => models,
        Err(e) => {
            warnings.push(format!("codex: {e}"));
            Vec::new()
        }
    };
    let context = std::env::var("NYQ_CONTEXT_WINDOW")
        .ok()
        .and_then(|v| v.parse().ok());
    choices.extend(ollama.into_iter().map(|model| ModelChoice {
        provider: "ollama".into(),
        model,
        context_window: context,
        efforts: Vec::new(),
    }));
    match openai {
        Ok((models, warning)) => {
            warnings.extend(warning);
            choices.extend(models.into_iter().map(|model| ModelChoice {
                provider: "openai".into(),
                model,
                context_window: None,
                efforts: Vec::new(),
            }));
        }
        Err(e) => warnings.push(format!("openai: {e}")),
    }
    (choices, warnings)
}

#[allow(dead_code)] // Used by the catalog once the UI calls list.
fn same_url(a: &str, b: &str) -> bool {
    match (
        reqwest::Url::parse(a.trim_end_matches('/')),
        reqwest::Url::parse(b.trim_end_matches('/')),
    ) {
        (Ok(a), Ok(b)) => a == b,
        _ => a.trim_end_matches('/') == b.trim_end_matches('/'),
    }
}

/// Build a provider for a choice. `effort` None = provider default ("auto").
pub fn build(
    home: &Path,
    choice: &ModelChoice,
    effort: Option<String>,
) -> Result<Box<dyn Provider>> {
    use crate::{
        config,
        provider::{codex::CodexProvider, openai::OpenAiProvider},
    };
    anyhow::ensure!(!choice.model.trim().is_empty(), "model must not be empty");
    match choice.provider.as_str() {
        "codex" => {
            let auth = config::codex_auth_path(home);
            anyhow::ensure!(auth.exists(), "codex provider needs a ChatGPT login");
            Ok(Box::new(CodexProvider::new(
                auth,
                choice.model.clone(),
                effort.filter(|e| e != "auto"),
            )))
        }
        "ollama" => Ok(Box::new(OpenAiProvider::with_label(
            "ollama".into(),
            config::ollama_url(),
            None,
            choice.model.clone(),
        ))),
        "openai" => {
            let (url, key, _) = config::openai_config(home)?;
            Ok(Box::new(OpenAiProvider::new(
                url.ok_or_else(|| anyhow::anyhow!("openai provider needs a configured base URL"))?,
                key,
                choice.model.clone(),
            )))
        }
        other => anyhow::bail!("unknown provider `{other}`"),
    }
}

/// Remember the last picked model so the next launch starts with it.
pub fn save_last(home: &Path, choice: &ModelChoice, effort: Option<&str>) -> Result<()> {
    let dir = home.to_path_buf();
    std::fs::create_dir_all(&dir)?;
    let last = crate::config::LastModel {
        provider: choice.provider.clone(),
        model: choice.model.clone(),
        effort: effort.filter(|e| *e != "auto").map(str::to_owned),
    };
    let temp = dir.join(format!(".last-model-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> Result<()> {
        std::fs::write(&temp, serde_json::to_vec_pretty(&last)?)?;
        crate::tools::safeio::rename_over(&temp, &dir.join("last-model.json"))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_equivalence() {
        assert!(same_url("http://localhost:80/v1/", "http://localhost/v1"));
        assert!(!same_url("http://localhost/v1", "http://localhost/v2"));
    }

    #[test]
    fn last_model_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let choice = ModelChoice {
            provider: "codex".into(),
            model: "test".into(),
            context_window: Some(42),
            efforts: vec!["low".into()],
        };
        save_last(dir.path(), &choice, Some("low")).unwrap();
        let read = || {
            serde_json::from_slice::<serde_json::Value>(
                &std::fs::read(dir.path().join("last-model.json")).unwrap(),
            )
            .unwrap()
        };
        assert_eq!(
            read(),
            serde_json::json!({"provider":"codex","model":"test","effort":"low"})
        );
        save_last(dir.path(), &choice, None).unwrap();
        assert!(read()["effort"].is_null());
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            1
        );
        let invalid = ModelChoice {
            provider: "unknown".into(),
            ..choice
        };
        assert!(build(dir.path(), &invalid, None).is_err());
    }
}
