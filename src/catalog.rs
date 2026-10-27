//! Model metadata from models.dev (names, context, protocol, efforts, prices).
//!
//! `https://models.dev/api.json` is fetched at most once a day into
//! `<home>/models.json`. Without it nyq reads the OpenCode CLI's copy
//! (`~/.cache/opencode/models.json`), and without either every model uses the
//! defaults (chat completions, no metadata).

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::Duration,
};

use serde::Deserialize;

const URL: &str = "https://models.dev/api.json";
const MAX_AGE: Duration = Duration::from_secs(86_400);

/// Wire protocol a model is served with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protocol {
    Chat,
    Responses,
    Anthropic,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ModelMeta {
    pub name: Option<String>,
    pub context: Option<u64>,
    pub output: Option<u64>,
    /// The AI SDK package that serves the model (`provider.npm`).
    pub npm: Option<String>,
    pub efforts: Vec<String>,
    /// Assistant field that carries interleaved reasoning back.
    pub interleaved: Option<String>,
    /// False for image/video/embedding models.
    pub text_output: bool,
    /// Whether the model reads images; None when the catalog doesn't say.
    pub image_input: Option<bool>,
    /// USD per million tokens: input, output.
    pub cost: Option<(f64, f64)>,
    /// USD per million cached input tokens, when listed.
    pub cached_cost: Option<f64>,
    /// USD per million input tokens written to the prompt cache, when listed.
    pub cache_write_cost: Option<f64>,
}

impl ModelMeta {
    pub fn protocol(&self) -> Protocol {
        match self.npm.as_deref() {
            Some("@ai-sdk/openai") => Protocol::Responses,
            Some("@ai-sdk/anthropic") => Protocol::Anthropic,
            _ => Protocol::Chat,
        }
    }

    /// e.g. `$1.4/$4.4 per 1M`.
    pub fn price(&self) -> Option<String> {
        let (input, output) = self.cost?;
        Some(format!("${input}/${output} per 1M"))
    }
}

/// Models per provider id (`opencode-go`, `xai`, …).
pub type Models = HashMap<String, ModelMeta>;

#[derive(Deserialize)]
struct RawProvider {
    #[serde(default)]
    models: HashMap<String, RawModel>,
}

#[derive(Deserialize)]
struct RawModel {
    name: Option<String>,
    #[serde(default)]
    limit: RawLimit,
    #[serde(default)]
    provider: RawNpm,
    #[serde(default)]
    reasoning_options: Vec<RawReasoning>,
    interleaved: Option<RawInterleaved>,
    modalities: Option<RawModalities>,
    cost: Option<RawCost>,
}

#[derive(Default, Deserialize)]
struct RawLimit {
    context: Option<u64>,
    output: Option<u64>,
}
#[derive(Default, Deserialize)]
struct RawNpm {
    npm: Option<String>,
}
#[derive(Deserialize)]
struct RawReasoning {
    #[serde(rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    values: Vec<String>,
}
#[derive(Deserialize)]
#[serde(untagged)]
enum RawInterleaved {
    Field { field: String },
    Flag(#[allow(dead_code)] bool),
}
#[derive(Deserialize)]
struct RawModalities {
    #[serde(default)]
    input: Vec<String>,
    #[serde(default)]
    output: Vec<String>,
}
#[derive(Deserialize)]
struct RawCost {
    input: Option<f64>,
    output: Option<f64>,
    cache_read: Option<f64>,
    cache_write: Option<f64>,
}

/// The models of `provider` in a models.dev document.
pub fn parse(bytes: &[u8], provider: &str) -> anyhow::Result<Models> {
    let mut all: HashMap<String, serde_json::Value> = serde_json::from_slice(bytes)?;
    let Some(raw) = all.remove(provider) else {
        return Ok(Models::new());
    };
    let raw: RawProvider = serde_json::from_value(raw)?;
    Ok(raw
        .models
        .into_iter()
        .map(|(id, m)| {
            let meta = ModelMeta {
                name: m.name,
                context: m.limit.context,
                output: m.limit.output,
                npm: m.provider.npm,
                efforts: m
                    .reasoning_options
                    .into_iter()
                    .filter(|r| r.kind.as_deref() == Some("effort"))
                    .flat_map(|r| r.values)
                    .collect(),
                interleaved: match m.interleaved {
                    Some(RawInterleaved::Field { field }) => Some(field),
                    _ => None,
                },
                text_output: m
                    .modalities
                    .as_ref()
                    .is_none_or(|x| x.output.iter().any(|o| o == "text")),
                image_input: m
                    .modalities
                    .as_ref()
                    .map(|x| x.input.iter().any(|i| i == "image")),
                cached_cost: m.cost.as_ref().and_then(|c| c.cache_read),
                cache_write_cost: m.cost.as_ref().and_then(|c| c.cache_write),
                cost: m.cost.and_then(|c| Some((c.input?, c.output?))),
            };
            (id, meta)
        })
        .collect())
}

/// A model's metadata; a `provider/model` id (as on aggregators) is looked
/// up under that provider first.
pub fn lookup(home: &Path, provider: &str, model: &str) -> Option<ModelMeta> {
    let find = |p: &str, m: &str| load(home, p).remove(m);
    model
        .split_once('/')
        .and_then(|(p, m)| find(p, m))
        .or_else(|| find(provider, model))
}

/// A model on `provider` that reads images, for describing images to a
/// text-only `current` model: `NYQ_VISION_MODEL` if set, else the catalog
/// model sharing the longest name prefix with `current`.
pub fn vision_model(home: &Path, provider: &str, current: &str) -> Option<String> {
    if let Some(model) = std::env::var("NYQ_VISION_MODEL")
        .ok()
        .filter(|m| !m.trim().is_empty())
    {
        return Some(model.trim().to_string());
    }
    let shared = |id: &str| {
        id.chars()
            .zip(current.chars())
            .take_while(|(a, b)| a == b)
            .count()
    };
    load(home, provider)
        .into_iter()
        .filter(|(_, m)| m.image_input == Some(true) && m.text_output)
        .map(|(id, _)| id)
        .max_by(|a, b| shared(a).cmp(&shared(b)).then_with(|| b.cmp(a)))
}

fn own(home: &Path) -> PathBuf {
    home.join("models.json")
}

fn opencode_cache() -> Option<PathBuf> {
    Some(
        dirs::home_dir()?
            .join(".cache")
            .join("opencode")
            .join("models.json"),
    )
}

/// Model metadata for `provider` from the freshest local copy.
pub fn load(home: &Path, provider: &str) -> Models {
    [Some(own(home)), opencode_cache()]
        .into_iter()
        .flatten()
        .find_map(|path| {
            parse(&std::fs::read(path).ok()?, provider)
                .ok()
                .filter(|m| !m.is_empty())
        })
        .unwrap_or_default()
}

fn fresh(path: &Path) -> bool {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .is_ok_and(|t| t.elapsed().is_ok_and(|age| age < MAX_AGE))
}

/// Fetch models.dev into `<home>/models.json` unless the copy is under a day
/// old. Failures keep the old copy; this never takes more than a few seconds.
pub async fn refresh(home: &Path) {
    let path = own(home);
    if fresh(&path) {
        return;
    }
    let url = std::env::var("NYQ_MODELS_DEV_URL").unwrap_or_else(|_| URL.into());
    let fetched = async {
        let client = crate::http::builder()
            .connect_timeout(Duration::from_secs(3))
            .timeout(Duration::from_secs(8))
            .build()
            .ok()?;
        let response = client
            .get(url)
            .header("User-Agent", crate::config::USER_AGENT)
            .send()
            .await
            .ok()?;
        let bytes = crate::provider::sse::checked(response)
            .await
            .ok()?
            .bytes()
            .await
            .ok()?;
        serde_json::from_slice::<serde_json::Map<String, serde_json::Value>>(&bytes).ok()?;
        Some(bytes)
    }
    .await;
    if let Some(bytes) = fetched {
        let _ = crate::config::write_private(&path, &bytes);
    }
}

#[cfg(test)]
mod vision_tests {
    use super::*;

    #[test]
    fn picks_the_closest_vision_model() {
        let _lock = crate::provider::sse::tests::ENV
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home = tempfile::tempdir().unwrap();
        let doc = serde_json::json!({"p": {"models": {
            "glm-5.3": {"modalities": {"input": ["text"], "output": ["text"]}},
            "glm-5.3v": {"modalities": {"input": ["text", "image"], "output": ["text"]}},
            "qwen-vl": {"modalities": {"input": ["text", "image"], "output": ["text"]}},
            "painter": {"modalities": {"input": ["text", "image"], "output": ["image"]}}
        }}});
        std::fs::write(home.path().join("models.json"), doc.to_string()).unwrap();
        let old = std::env::var_os("NYQ_VISION_MODEL");
        std::env::remove_var("NYQ_VISION_MODEL");
        assert_eq!(
            vision_model(home.path(), "p", "glm-5.3").as_deref(),
            Some("glm-5.3v")
        );
        assert_eq!(
            vision_model(home.path(), "p", "zzz").as_deref(),
            Some("glm-5.3v")
        );
        assert_eq!(vision_model(home.path(), "none", "glm-5.3"), None);
        assert_eq!(
            lookup(home.path(), "x", "p/glm-5.3").and_then(|m| m.image_input),
            Some(false)
        );
        std::env::set_var("NYQ_VISION_MODEL", "picked");
        assert_eq!(
            vision_model(home.path(), "none", "glm").as_deref(),
            Some("picked")
        );
        match old {
            Some(v) => std::env::set_var("NYQ_VISION_MODEL", v),
            None => std::env::remove_var("NYQ_VISION_MODEL"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use crate::provider::sse::tests::server;
    use serde_json::json;

    fn doc() -> serde_json::Value {
        json!({
            "opencode-go": {"id":"opencode-go","models":{
                "glm-5.3": {"name":"GLM-5.3","limit":{"context":1000000,"output":131072},"interleaved":{"field":"reasoning_content"},
                    "reasoning_options":[{"type":"effort","values":["low","high","max"]}],"cost":{"input":1.4,"output":4.4}},
                "grok-4.7": {"provider":{"npm":"@ai-sdk/openai"},"limit":{"context":500000}},
                "claude-haiku-5-5": {"provider":{"npm":"@ai-sdk/anthropic"},"interleaved":true},
                "imagine": {"modalities":{"input":["text"],"output":["image"]}}
            }},
            "other": {"models":{}}
        })
    }

    #[test]
    fn protocol_per_model() {
        let models = parse(doc().to_string().as_bytes(), "opencode-go").unwrap();
        assert_eq!(models["glm-5.3"].protocol(), Protocol::Chat);
        assert_eq!(models["grok-4.7"].protocol(), Protocol::Responses);
        assert_eq!(models["claude-haiku-5-5"].protocol(), Protocol::Anthropic);
        assert_eq!(models["claude-haiku-5-5"].interleaved, None);
        let glm = &models["glm-5.3"];
        assert_eq!(glm.efforts, vec!["low", "high", "max"]);
        assert_eq!(glm.interleaved.as_deref(), Some("reasoning_content"));
        assert_eq!((glm.context, glm.output), (Some(1_000_000), Some(131_072)));
        assert_eq!(glm.price().as_deref(), Some("$1.4/$4.4 per 1M"));
        assert!(glm.text_output && !models["imagine"].text_output);
        assert_eq!(
            (glm.image_input, models["imagine"].image_input),
            (None, Some(false))
        );
        assert!(parse(doc().to_string().as_bytes(), "missing")
            .unwrap()
            .is_empty());
        assert_eq!(ModelMeta::default().protocol(), Protocol::Chat);
    }

    // Points HOME at a temp dir to fake the OpenCode CLI's cache; Windows
    // resolves the home directory without HOME.
    #[cfg(unix)]
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // Serializes env-var mutation.
    async fn cache_freshness_and_fallback() {
        let _lock = crate::provider::sse::tests::ENV
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home = tempfile::tempdir().unwrap();
        let user = tempfile::tempdir().unwrap();
        let old_home = std::env::var_os("HOME");
        std::env::set_var("HOME", user.path());
        // Nothing cached: defaults.
        assert!(load(home.path(), "opencode-go").is_empty());
        // The OpenCode CLI's cache is the fallback.
        let cache = user.path().join(".cache/opencode");
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::write(cache.join("models.json"), doc().to_string()).unwrap();
        assert_eq!(load(home.path(), "opencode-go").len(), 4);

        // A stale copy is refetched once; a fresh one is not.
        let fetched = json!({"opencode-go":{"models":{"new":{"limit":{"context":7}}}}}).to_string();
        let (url, mut requests, task) = server(vec![(200, fetched)]).await;
        std::env::set_var("NYQ_MODELS_DEV_URL", format!("{url}/api.json"));
        refresh(home.path()).await;
        refresh(home.path()).await;
        std::env::remove_var("NYQ_MODELS_DEV_URL");
        match old_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
        task.await.unwrap();
        let request = requests.recv().await.unwrap().to_lowercase();
        assert!(request.starts_with("get /api.json ") && request.contains("user-agent: nyq/"));
        assert!(requests.try_recv().is_err());
        let models = load(home.path(), "opencode-go");
        assert_eq!(models["new"].context, Some(7));
        assert!(fresh(&home.path().join("models.json")));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(home.path().join("models.json"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }
}
