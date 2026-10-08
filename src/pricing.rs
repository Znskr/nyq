//! Estimated token prices, in USD per million tokens. Local catalogs override defaults.
use crate::types::Usage;
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
/// A model's price from the models.dev catalog (see `crate::catalog`). A
/// `provider/model` id (as on aggregators) is looked up under that provider.
fn from_catalog(home: &Path, provider: &str, model: &str) -> Option<Price> {
    let lookup = |p: &str, m: &str| {
        let meta = crate::catalog::load(home, p).remove(m)?;
        let (input, output) = meta.cost?;
        let cached = meta.cached_cost.unwrap_or(input);
        [input, output, cached].iter().all(|n| n.is_finite() && *n >= 0.0).then_some(Price { input, output, cached })
    };
    model.split_once('/').and_then(|(p, m)| lookup(p, m)).or_else(|| lookup(provider, model))
}

pub fn lookup(home: &Path, provider: &str, model: &str) -> Option<Price> {
    if provider == "codex" {
        return None;
    }
    if let Some(price) = from_catalog(home, provider, model) {
        return Some(price);
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
        let home = tempfile::tempdir().unwrap();
        let doc = serde_json::json!({"opencode":{"models":{"free":{"cost":{"input":0,"output":0}},"paid":{"cost":{"input":2,"output":3,"cache_read":1}}}}});
        std::fs::write(home.path().join("models.json"), doc.to_string()).unwrap();
        assert_eq!(from_catalog(home.path(), "openai", "opencode/paid").unwrap().cached, 1.0);
        assert_eq!(from_catalog(home.path(), "opencode", "free").unwrap().input, 0.0);
        assert!(from_catalog(home.path(), "openai", "unknown").is_none());
    }
}
