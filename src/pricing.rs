//! Estimated token prices, in USD per million tokens. Local catalogs override defaults.
use crate::types::Usage;
use serde_json::Value;
use std::path::Path;

#[derive(Clone, Copy, Debug)]
pub struct Price {
    pub input: f64,
    pub output: f64,
    pub cached: f64,
}
impl Price {
    pub fn cost(self, u: Usage) -> f64 {
        (u.input_tokens.saturating_sub(u.cached_tokens) as f64 * self.input
            + u.cached_tokens.min(u.input_tokens) as f64 * self.cached
            + u.output_tokens as f64 * self.output)
            / 1_000_000.0
    }
}
fn catalog(v: &Value, provider: &str, model: &str) -> Option<Price> {
    let (provider, model) = model
        .split_once('/')
        .filter(|(p, _)| v.get(*p).is_some())
        .unwrap_or((provider, model));
    let c = &v[provider]["models"][model]["cost"];
    let input = c["input"].as_f64()?;
    let output = c["output"].as_f64()?;
    let cached = c["cache_read"].as_f64().unwrap_or(input);
    [input, output, cached]
        .iter()
        .all(|n| n.is_finite() && *n >= 0.0)
        .then_some(Price {
            input,
            output,
            cached,
        })
}
pub fn lookup(home: &Path, provider: &str, model: &str) -> Option<Price> {
    if provider == "codex" {
        return None;
    }
    let cache = dirs::home_dir()
        .unwrap_or_default()
        .join(".cache/opencode/models.json");
    for path in [home.join("models.json"), cache] {
        if let Some(price) = std::fs::read(path)
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            .and_then(|v| catalog(&v, provider, model))
        {
            return Some(price);
        }
    }
    if provider != "openai" {
        return None;
    }
    let (input, output, cached) = match model.strip_prefix("openai/").unwrap_or(model) {
        "gpt-4o" => (2.5, 10.0, 1.25),
        "gpt-4o-mini" => (0.15, 0.6, 0.075),
        "gpt-4.1" => (2.0, 8.0, 0.5),
        "gpt-4.1-mini" => (0.4, 1.6, 0.1),
        "gpt-5" => (1.25, 10.0, 0.125),
        "gpt-5-mini" => (0.25, 2.0, 0.025),
        _ => return None,
    };
    Some(Price {
        input,
        output,
        cached,
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cached_input_is_not_billed_twice() {
        let p = Price {
            input: 2.0,
            output: 8.0,
            cached: 0.5,
        };
        assert_eq!(
            p.cost(Usage {
                input_tokens: 1_000_000,
                cached_tokens: 400_000,
                output_tokens: 100_000
            }),
            2.2
        );
        assert_eq!(
            p.cost(Usage {
                input_tokens: 10,
                cached_tokens: 20,
                output_tokens: 0
            }),
            0.000005
        );
    }
    #[test]
    fn catalog_supports_provider_prefix_and_zero_prices() {
        let v = serde_json::json!({"opencode":{"models":{"free":{"cost":{"input":0,"output":0}},"paid":{"cost":{"input":2,"output":3,"cache_read":1}}}}});
        assert_eq!(catalog(&v, "openai", "opencode/paid").unwrap().cached, 1.0);
        assert_eq!(catalog(&v, "opencode", "free").unwrap().input, 0.0);
        assert!(catalog(&v, "openai", "unknown").is_none());
    }
}
