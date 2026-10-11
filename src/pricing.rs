//! Estimated token prices, in USD per million tokens. Local catalogs override defaults.
use crate::types::Usage;
use std::path::Path;

#[derive(Clone, Copy, Debug)]
pub struct Price {
    pub input: f64,
    pub output: f64,
    pub cached: f64,
    /// Prompt-cache writes (Anthropic `cache_creation`); the input rate when unlisted.
    pub cache_write: f64,
}
impl Price {
    pub fn cost(self, u: Usage) -> f64 {
        let cached = u.cached_tokens.min(u.input_tokens);
        let written = u.cache_write_tokens.min(u.input_tokens - cached);
        ((u.input_tokens - cached - written) as f64 * self.input
            + cached as f64 * self.cached
            + written as f64 * self.cache_write
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
        let cache_write = meta.cache_write_cost.unwrap_or(input);
        [input, output, cached, cache_write]
            .iter()
            .all(|n| n.is_finite() && *n >= 0.0)
            .then_some(Price { input, output, cached, cache_write })
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
        cache_write: input,
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
            cache_write: 2.0,
        };
        assert_eq!(
            p.cost(Usage {
                input_tokens: 1_000_000,
                cached_tokens: 400_000,
                output_tokens: 100_000,
                ..Default::default()
            }),
            2.2
        );
        assert_eq!(
            p.cost(Usage {
                input_tokens: 10,
                cached_tokens: 20,
                output_tokens: 0,
                ..Default::default()
            }),
            0.000005
        );
    }
    #[test]
    fn cache_writes_use_their_own_rate() {
        let p = Price {
            input: 3.0,
            output: 15.0,
            cached: 0.3,
            cache_write: 3.75,
        };
        let cost = p.cost(Usage {
            input_tokens: 1_000_000,
            cached_tokens: 200_000,
            cache_write_tokens: 400_000,
            ..Default::default()
        });
        assert!((cost - (0.4 * 3.0 + 0.2 * 0.3 + 0.4 * 3.75)).abs() < 1e-9, "{cost}");
    }
    #[test]
    fn catalog_supports_provider_prefix_and_zero_prices() {
        let home = tempfile::tempdir().unwrap();
        let doc = serde_json::json!({"opencode":{"models":{"free":{"cost":{"input":0,"output":0}},"paid":{"cost":{"input":2,"output":3,"cache_read":1,"cache_write":2.5}}}}});
        std::fs::write(home.path().join("models.json"), doc.to_string()).unwrap();
        let paid = from_catalog(home.path(), "openai", "opencode/paid").unwrap();
        assert_eq!((paid.cached, paid.cache_write), (1.0, 2.5));
        let free = from_catalog(home.path(), "opencode", "free").unwrap();
        assert_eq!((free.input, free.cache_write), (0.0, 0.0));
        assert!(from_catalog(home.path(), "openai", "unknown").is_none());
    }
}
