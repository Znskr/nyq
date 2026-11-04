//! Model catalog across providers, for the `/model` switcher.

use std::path::Path;

use anyhow::Result;

use crate::provider::Provider;

/// One selectable model.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModelChoice {
    /// Provider group shown as a tab: "codex", "grok", "ollama", "openai", or a
    /// hosted service ("opencode-go", "opencode", "xai").
    pub provider: String,
    pub model: String,
    /// Tokens, when the provider reports it.
    pub context_window: Option<u64>,
    /// Supported reasoning efforts, lowest first (empty if not adjustable).
    pub efforts: Vec<String>,
    /// Price summary, when known (e.g. `$1.4/$4.4 per 1M`).
    pub price: Option<String>,
}

/// Choices matching a search: those containing every word of `query` in
/// `provider/model`; failing that, those whose model id contains the
/// query's letters and digits in order (`gpt55` finds `gpt-5.5`).
pub fn search<'a>(
    choices: impl IntoIterator<Item = &'a ModelChoice>,
    query: &str,
) -> Vec<&'a ModelChoice> {
    let choices: Vec<&ModelChoice> = choices.into_iter().collect();
    let query = query.to_lowercase();
    let words: Vec<&str> = query.split_whitespace().collect();
    let worded: Vec<_> = choices
        .iter()
        .copied()
        .filter(|c| {
            let hay = format!("{}/{}", c.provider, c.model).to_lowercase();
            words.iter().all(|w| hay.contains(w))
        })
        .collect();
    if !worded.is_empty() || words.is_empty() {
        return worded;
    }
    let wanted: Vec<char> = query.chars().filter(|c| c.is_alphanumeric()).collect();
    choices
        .into_iter()
        .filter(|c| {
            let model = c.model.to_lowercase();
            let mut hay = model.chars();
            wanted.iter().all(|w| hay.any(|h| h == *w))
        })
        .collect()
}

/// `/model` arguments: a model id or search, then an optional reasoning
/// effort and `fast` or `normal`.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ModelArgs {
    pub query: String,
    pub effort: Option<String>,
    pub fast: Option<bool>,
}

const EFFORT_WORDS: [&str; 8] = [
    "auto", "none", "minimal", "low", "medium", "high", "xhigh", "max",
];

pub fn parse_args(args: &[&str]) -> ModelArgs {
    let mut parsed = ModelArgs::default();
    let mut rest = args.to_vec();
    while rest.len() > 1 {
        let last = rest[rest.len() - 1].to_ascii_lowercase();
        match last.as_str() {
            "fast" | "normal" if parsed.fast.is_none() => parsed.fast = Some(last == "fast"),
            e if parsed.effort.is_none() && EFFORT_WORDS.contains(&e) => parsed.effort = Some(last),
            _ => break,
        }
        rest.pop();
    }
    parsed.query = rest.join(" ");
    parsed
}

/// The models `query` picks: an exact id (`model` or `provider/model`,
/// preferring `provider` when several providers list it), else [`search`].
pub fn find<'a>(choices: &'a [ModelChoice], query: &str, provider: &str) -> Vec<&'a ModelChoice> {
    let q = query.trim().to_lowercase();
    let exact: Vec<_> = choices
        .iter()
        .filter(|c| {
            c.model.to_lowercase() == q || format!("{}/{}", c.provider, c.model).to_lowercase() == q
        })
        .collect();
    match exact.iter().find(|c| c.provider == provider) {
        Some(c) => vec![*c],
        None if !exact.is_empty() => exact,
        None => search(choices, &q),
    }
}

/// Check a requested effort and fast mode against what `choice` offers.
pub fn check_options(choice: &ModelChoice, effort: Option<&str>, fast: Option<bool>) -> Result<()> {
    if let Some(e) = effort.filter(|e| *e != "auto") {
        anyhow::ensure!(
            !choice.efforts.is_empty(),
            "{} has no adjustable reasoning effort",
            choice.model
        );
        anyhow::ensure!(
            choice.efforts.iter().any(|x| x == e),
            "{} supports reasoning effort {} (not {e})",
            choice.model,
            choice.efforts.join(", ")
        );
    }
    if fast == Some(true) {
        anyhow::ensure!(
            crate::provider::listed_fast(&choice.provider, &choice.model),
            "{} does not offer fast mode",
            choice.model
        );
    }
    Ok(())
}

/// Models of every hosted service that has an API key. Services without a
/// key are skipped silently; failures become short warnings.
async fn hosted(home: &Path) -> (Vec<ModelChoice>, Vec<String>) {
    use crate::provider::hosted::{api_key, HostedProvider, Service};
    let configured: Vec<_> = Service::ALL
        .into_iter()
        .filter(|s| api_key(home, *s).is_some())
        .collect();
    if configured.is_empty() {
        return (Vec::new(), Vec::new());
    }
    crate::catalog::refresh(home).await;
    let lists = futures::future::join_all(configured.iter().map(|&service| async move {
        let provider = HostedProvider::configured(home, service, service.default_model(), None)?;
        provider.list_models().await
    }))
    .await;
    let (mut choices, mut warnings) = (Vec::new(), Vec::new());
    for (service, list) in configured.into_iter().zip(lists) {
        match list {
            Ok(models) => choices.extend(models),
            Err(e) => warnings.push(format!("{}: {}", service.id(), short(&format!("{e:#}")))),
        }
    }
    (choices, warnings)
}

/// User-named endpoints are independent of hosted-service and environment keys.
async fn custom(home: &Path) -> (Vec<ModelChoice>, Vec<String>) {
    use crate::provider::custom::{self, Protocol};
    let definitions = match custom::load(home) {
        Ok(definitions) => definitions,
        Err(e) => {
            return (
                Vec::new(),
                vec![format!("custom providers: {}", short(&format!("{e:#}")))],
            )
        }
    };
    let lists = futures::future::join_all(definitions.iter().map(custom::discover)).await;
    let (mut choices, mut warnings) = (Vec::new(), Vec::new());
    for (definition, list) in definitions.into_iter().zip(lists) {
        match list {
            Ok(models) => choices.extend(models),
            Err(e) => {
                let rejected = custom::is_auth_error(&e);
                warnings.push(format!(
                    "{}: {}{}",
                    definition.id,
                    short(&format!("{e:#}")),
                    if rejected {
                        ""
                    } else {
                        "; using configured model"
                    }
                ));
                if !rejected {
                    let efforts = if definition.protocol == Protocol::Anthropic {
                        crate::provider::anthropic::default_efforts(&definition.model)
                    } else {
                        Vec::new()
                    };
                    choices.push(ModelChoice {
                        provider: definition.id,
                        model: definition.model,
                        efforts,
                        ..Default::default()
                    });
                }
            }
        }
    }
    (choices, warnings)
}

/// The first line of an error, capped for the switcher.
fn short(error: &str) -> String {
    let line = error.lines().next().unwrap_or_default();
    match line.char_indices().nth(160) {
        Some((i, _)) => format!("{}…", &line[..i]),
        None => line.to_string(),
    }
}

/// All models from every reachable provider (Codex login, Grok subscription,
/// local Ollama, the configured OpenAI-compatible endpoint, hosted services
/// with an API key).
/// Unreachable providers are skipped and reported as warnings. Never takes
/// longer than a few seconds.
pub async fn list(home: &Path) -> (Vec<ModelChoice>, Vec<String>) {
    use crate::{
        config,
        provider::{codex::CodexProvider, grok::GrokProvider, openai::OpenAiProvider},
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
    let grok = async {
        if !crate::provider::grok::has_session(home) {
            return Ok(Vec::new());
        }
        let provider = GrokProvider::new(
            home.to_path_buf(),
            crate::provider::grok::DEFAULT_MODEL.into(),
            None,
        );
        tokio::time::timeout(std::time::Duration::from_secs(6), provider.list_models())
            .await
            .map_err(anyhow::Error::from)?
    };
    let (codex, grok, ollama, openai, hosted, custom): (_, Result<_>, _, Result<_>, _, _) =
        tokio::join!(codex, grok, ollama, openai, hosted(home), custom(home));
    crate::provider::remember_listing("ollama", ollama.clone(), Vec::new());
    if let Ok((models, None)) = &openai {
        crate::provider::remember_listing("openai", models.clone(), Vec::new());
    }
    let mut warnings = Vec::new();
    let mut choices = match codex {
        Ok(models) => models,
        Err(e) => {
            warnings.push(format!("codex: {e}"));
            Vec::new()
        }
    };
    match grok {
        Ok(models) => choices.extend(models),
        Err(e) => warnings.push(format!("grok: {e}")),
    }
    let context = std::env::var("NYQ_CONTEXT_WINDOW")
        .ok()
        .and_then(|v| v.parse().ok());
    choices.extend(ollama.into_iter().map(|model| ModelChoice {
        provider: "ollama".into(),
        model,
        context_window: context,
        ..ModelChoice::default()
    }));
    match openai {
        Ok((models, warning)) => {
            warnings.extend(warning);
            choices.extend(models.into_iter().map(|model| ModelChoice {
                provider: "openai".into(),
                model,
                ..ModelChoice::default()
            }));
        }
        Err(e) => warnings.push(format!("openai: {e}")),
    }
    choices.extend(hosted.0);
    warnings.extend(hosted.1);
    choices.extend(custom.0);
    warnings.extend(custom.1);
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
        provider::{codex::CodexProvider, grok::GrokProvider, openai::OpenAiProvider},
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
        "grok" => {
            anyhow::ensure!(
                crate::provider::grok::has_session(home),
                "grok provider needs a Grok subscription sign-in"
            );
            Ok(Box::new(
                GrokProvider::new(
                    home.to_path_buf(),
                    choice.model.clone(),
                    effort.filter(|e| e != "auto"),
                )
                .with_catalog(choice.efforts.clone(), choice.context_window),
            ))
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
        other => match crate::provider::hosted::Service::parse(other) {
            Some(service) => Ok(Box::new(
                crate::provider::hosted::HostedProvider::configured(
                    home,
                    service,
                    choice.model.clone(),
                    effort,
                )?,
            )),
            None => crate::provider::custom::build(home, other, &choice.model, effort),
        },
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

    #[tokio::test]
    async fn custom_catalog_falls_back_except_for_rejected_credentials() {
        use crate::provider::{
            custom::{self, Definition, Protocol},
            sse::tests::server,
        };
        for status in [401, 403, 404, 500, 200] {
            let body = if status == 200 { r#"{"data":[]}"# } else { "" };
            let (url, _requests, task) = server(vec![(status, body.into())]).await;
            let home = tempfile::tempdir().unwrap();
            custom::save(
                home.path(),
                &Definition {
                    id: "catalog-test".into(),
                    protocol: Protocol::Anthropic,
                    base_url: url,
                    api_key: Some("isolated-key".into()),
                    model: "manual-model".into(),
                },
            )
            .unwrap();
            let (choices, warnings) = super::custom(home.path()).await;
            assert_eq!(warnings.len(), 1);
            assert!(!warnings[0].contains("isolated-key"));
            if matches!(status, 401 | 403) {
                assert!(choices.is_empty());
                assert!(warnings[0].contains("API access denied"));
            } else {
                assert_eq!(choices.len(), 1);
                assert_eq!(choices[0].provider, "catalog-test");
                assert_eq!(choices[0].model, "manual-model");
                assert!(!choices[0].efforts.is_empty());
                assert!(warnings[0].contains("using configured model"));
            }
            task.await.unwrap();
        }
    }

    #[tokio::test]
    async fn custom_catalog_discovers_and_reports_malformed_config() {
        use crate::provider::{
            custom::{self, Definition, Protocol},
            sse::tests::server,
        };
        let (url, _requests, task) = server(vec![(
            200,
            r#"{"data":[{"id":"found","context_window":10000}]}"#.into(),
        )])
        .await;
        let home = tempfile::tempdir().unwrap();
        custom::save(
            home.path(),
            &Definition {
                id: "discovered-test".into(),
                protocol: Protocol::Openai,
                base_url: url,
                api_key: None,
                model: "manual-model".into(),
            },
        )
        .unwrap();
        let (choices, warnings) = super::custom(home.path()).await;
        assert!(warnings.is_empty());
        assert_eq!(choices[0].provider, "discovered-test");
        assert_eq!(choices[0].model, "found");
        assert_eq!(choices[0].context_window, Some(10000));
        task.await.unwrap();
        std::fs::write(home.path().join("providers.json"), "malformed").unwrap();
        let (choices, warnings) = super::custom(home.path()).await;
        assert!(choices.is_empty());
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("providers.json"));
    }

    fn choice(provider: &str, model: &str, efforts: &[&str]) -> ModelChoice {
        ModelChoice {
            provider: provider.into(),
            model: model.into(),
            efforts: efforts.iter().map(|e| e.to_string()).collect(),
            ..ModelChoice::default()
        }
    }

    #[test]
    fn model_arguments() {
        let p = |a: &[&str]| parse_args(a);
        assert_eq!(
            p(&["gpt-5.5"]),
            ModelArgs {
                query: "gpt-5.5".into(),
                ..ModelArgs::default()
            }
        );
        assert_eq!(
            p(&["gpt-5.5", "high", "fast"]),
            ModelArgs {
                query: "gpt-5.5".into(),
                effort: Some("high".into()),
                fast: Some(true)
            }
        );
        assert_eq!(
            p(&["claude", "sonnet", "Normal", "LOW"]),
            ModelArgs {
                query: "claude sonnet".into(),
                effort: Some("low".into()),
                fast: Some(false)
            }
        );
        // A lone word is always the query.
        assert_eq!(p(&["high"]).query, "high");
        assert_eq!(
            p(&["fast", "fast"]),
            ModelArgs {
                query: "fast".into(),
                fast: Some(true),
                ..ModelArgs::default()
            }
        );
    }

    #[test]
    fn finding_models() {
        let all = [
            choice("codex", "gpt-5.5", &["low", "high"]),
            choice("codex", "gpt-5.5-mini", &[]),
            choice("openai", "gpt-5.5", &[]),
            choice("opencode", "claude-sonnet-5", &[]),
            choice("opencode", "claude-opus-5", &[]),
        ];
        let ids = |found: Vec<&ModelChoice>| {
            found
                .iter()
                .map(|c| format!("{}/{}", c.provider, c.model))
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(find(&all, "GPT-5.5", "openai")), ["openai/gpt-5.5"]);
        assert_eq!(
            ids(find(&all, "gpt-5.5", "opencode")),
            ["codex/gpt-5.5", "openai/gpt-5.5"]
        );
        assert_eq!(
            ids(find(&all, "codex/gpt-5.5", "openai")),
            ["codex/gpt-5.5"]
        );
        assert_eq!(
            ids(find(&all, "sonnet", "codex")),
            ["opencode/claude-sonnet-5"]
        );
        assert_eq!(ids(find(&all, "claude 5", "codex")).len(), 2);
        assert_eq!(ids(find(&all, "mini", "codex")), ["codex/gpt-5.5-mini"]);
        // Letters in order when no word matches.
        assert_eq!(ids(find(&all, "clop", "codex")), ["opencode/claude-opus-5"]);
        assert_eq!(
            ids(find(&all, "gpt55mini", "codex")),
            ["codex/gpt-5.5-mini"]
        );
        assert!(find(&all, "llama", "codex").is_empty());
        assert_eq!(search(&all, "").len(), all.len());
    }

    #[test]
    fn checking_options() {
        let gpt = choice("codex", "gpt-options-test", &["low", "high"]);
        assert!(check_options(&gpt, Some("high"), Some(false)).is_ok());
        assert!(check_options(&gpt, Some("auto"), None).is_ok());
        let err = check_options(&gpt, Some("xhigh"), None)
            .unwrap_err()
            .to_string();
        assert_eq!(
            err,
            "gpt-options-test supports reasoning effort low, high (not xhigh)"
        );
        let bare = choice("ollama", "qwen", &[]);
        assert!(check_options(&bare, Some("low"), None)
            .unwrap_err()
            .to_string()
            .contains("no adjustable"));
        assert!(check_options(&gpt, None, Some(true))
            .unwrap_err()
            .to_string()
            .contains("does not offer fast mode"));
        crate::provider::remember_listing(
            "codex",
            vec!["gpt-options-test".into()],
            vec!["gpt-options-test".into()],
        );
        assert!(check_options(&gpt, None, Some(true)).is_ok());
    }

    #[test]
    fn endpoint_equivalence() {
        assert!(same_url("http://localhost:80/v1/", "http://localhost/v1"));
        assert!(!same_url("http://localhost/v1", "http://localhost/v2"));
    }

    #[test]
    fn unknown_saved_provider_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let choice = ModelChoice {
            provider: "vercel".into(),
            model: "m".into(),
            ..Default::default()
        };
        let err = build(dir.path(), &choice, None).err().unwrap().to_string();
        assert!(
            err.starts_with("unknown provider `vercel`") && err.contains("nyq provider NAME"),
            "{err}"
        );
    }

    #[test]
    fn last_model_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let choice = ModelChoice {
            provider: "codex".into(),
            model: "test".into(),
            context_window: Some(42),
            efforts: vec!["low".into()],
            price: None,
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
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
        let invalid = ModelChoice {
            provider: "unknown".into(),
            ..choice
        };
        assert!(build(dir.path(), &invalid, None).is_err());
    }
}
