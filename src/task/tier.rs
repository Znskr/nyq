//! Model tiers (design §10): `tier.low`, `tier.medium` and `tier.high` in
//! settings map to models. Low runs the classifier and the progress check;
//! High runs the judge and the diagnosis, and is what the ladder's tier bump
//! switches to. An unset tier falls back to what nyq used before (the
//! classifier model, the provider's reviewer model, the session model).

use super::engine::Tier;
use super::options::{ModelRef, TaskOptions};
use crate::provider::Provider;

fn configured(opts: &TaskOptions, tier: Tier) -> Option<&ModelRef> {
    match tier {
        Tier::Low => opts.tier_low.as_ref(),
        Tier::Medium => opts.tier_medium.as_ref(),
        Tier::High => opts.tier_high.as_ref(),
    }
}

/// A provider for `model`: the session provider forked onto another model,
/// or a different provider built from the login on disk.
fn build(base: &dyn Provider, model: &ModelRef) -> anyhow::Result<Box<dyn Provider>> {
    match model.provider.as_deref() {
        None => {}
        Some(p) if p == base.name() => {}
        Some(p) => {
            return crate::models::build(
                &crate::config::home(),
                &crate::models::ModelChoice {
                    provider: p.to_string(),
                    model: model.model.clone(),
                    ..Default::default()
                },
                None,
            )
        }
    }
    let mut provider = base.fork();
    provider.set_model(model.model.clone());
    Ok(provider)
}

/// The provider for a helper call on `tier`. Errors building a configured
/// provider fall back to the session model, with the reason.
pub fn provider_for(
    base: &dyn Provider,
    tier: Tier,
    opts: &TaskOptions,
) -> (Box<dyn Provider>, Option<String>) {
    let (mut provider, warning) = match configured(opts, tier) {
        Some(m) => match build(base, m) {
            Ok(p) => (p, None),
            Err(e) => (
                base.fork(),
                Some(format!(
                    "tier.{}: could not use {}: {e:#}",
                    tier_key(tier),
                    m.model
                )),
            ),
        },
        None => {
            let mut p = base.fork();
            if tier == Tier::Low {
                if let Some(m) =
                    crate::settings::classifier_model().or_else(|| base.reviewer_model())
                {
                    p.set_model(m);
                }
            }
            (p, None)
        }
    };
    if tier == Tier::Low {
        // Cheap and quick: no extra reasoning, no priority tier.
        provider.set_effort(None);
        provider.set_fast(false);
    }
    (provider, warning)
}

fn tier_key(t: Tier) -> &'static str {
    match t {
        Tier::Low => "low",
        Tier::Medium => "medium",
        Tier::High => "high",
    }
}

/// The auto-mode classifier's model: `classifier_model`, else `tier.low` when
/// it is on the session's provider, else None (the provider's reviewer model).
pub fn classifier_model(base: &dyn Provider, opts: &TaskOptions) -> Option<String> {
    crate::settings::classifier_model().or_else(|| {
        opts.tier_low
            .as_ref()
            .filter(|m| m.provider.as_deref().is_none_or(|p| p == base.name()))
            .map(|m| m.model.clone())
    })
}

/// Whether the ladder's tier bump has somewhere to go: `tier.high` is set and
/// is not the model already in use.
pub fn bump_available(base: &dyn Provider, opts: &TaskOptions) -> bool {
    opts.tier_high
        .as_ref()
        .is_some_and(|m| !m.is(base.name(), base.model()))
}

/// The provider to switch the task to on a tier bump.
pub fn bumped(base: &dyn Provider, opts: &TaskOptions) -> anyhow::Result<Box<dyn Provider>> {
    let m = opts
        .tier_high
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("tier.high is not set"))?;
    build(base, m)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::test_support::ScriptedTurns;

    struct Named(String);
    #[async_trait::async_trait]
    impl Provider for Named {
        fn name(&self) -> &str {
            "fake"
        }
        fn model(&self) -> &str {
            &self.0
        }
        fn set_model(&mut self, m: String) {
            self.0 = m;
        }
        fn fork(&self) -> Box<dyn Provider> {
            Box::new(Named(self.0.clone()))
        }
        fn reviewer_model(&self) -> Option<String> {
            Some("mini".into())
        }
        async fn stream(
            &self,
            _: crate::types::Request<'_>,
            _: crate::provider::EventSink<'_>,
        ) -> anyhow::Result<crate::types::Response> {
            anyhow::bail!("unused")
        }
    }

    #[test]
    fn tiers_pick_models() {
        let base = Named("main".into());
        let none = TaskOptions::default();
        assert_eq!(provider_for(&base, Tier::Low, &none).0.model(), "mini");
        assert_eq!(provider_for(&base, Tier::High, &none).0.model(), "main");
        assert!(!bump_available(&base, &none));
        assert_eq!(classifier_model(&base, &none), None);

        let opts = TaskOptions {
            tier_low: ModelRef::parse("fake/cheap"),
            tier_high: ModelRef::parse("big"),
            ..TaskOptions::default()
        };
        assert_eq!(provider_for(&base, Tier::Low, &opts).0.model(), "cheap");
        assert_eq!(provider_for(&base, Tier::High, &opts).0.model(), "big");
        assert!(bump_available(&base, &opts));
        assert_eq!(bumped(&base, &opts).unwrap().model(), "big");
        assert_eq!(classifier_model(&base, &opts).as_deref(), Some("cheap"));
        assert!(
            !bump_available(&Named("big".into()), &opts),
            "already on high"
        );

        // Another provider's low model is not the classifier's.
        let other = TaskOptions {
            tier_low: ModelRef::parse("elsewhere/cheap"),
            ..TaskOptions::default()
        };
        assert_eq!(classifier_model(&base, &other), None);
        let scripted = ScriptedTurns {
            left: Default::default(),
        };
        assert_eq!(
            provider_for(&scripted, Tier::Medium, &none).0.name(),
            "fake"
        );
    }
}
