//! Task-engine settings: `[tier]`, `[verify]` and `[intents]` in
//! `settings.toml`.
//!
//! They are read from the settings' pass-through map (`Settings::extra`), so
//! the settings struct and its menu stay as they are. Every key is optional;
//! a missing or malformed key means its default.
//!
//! ```toml
//! [tier]
//! low = "openai/gpt-5-mini"      # provider/model, or a bare model id
//! medium = "openai/gpt-5"
//! high = "anthropic/claude-opus-5-5"
//!
//! [verify]
//! auto = true          # background build after writes (default: only for
//!                      # tasks with a build, tests or lint validator)
//! tests = false        # also run the tests in the background
//! required = false     # what the `ci` profile sets: always verify
//! debounce_ms = 1500
//! wait_ms = 2000       # how long a request waits for a running verify
//! progress_every = 3   # consecutive red builds before the ladder climbs
//!
//! [intents]
//! enabled = true       # offer the `intent` field on mutating tools
//! required = false     # a mutating call without an intent is rejected
//! ```

use std::time::Duration;

use serde_json::{Map, Value};

/// A model named in settings: `provider/model`, or a bare model id for the
/// session's provider. The model part may itself contain slashes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelRef {
    pub provider: Option<String>,
    pub model: String,
}

impl ModelRef {
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        if s.is_empty() {
            return None;
        }
        Some(match s.split_once('/') {
            Some((p, m)) if !p.is_empty() && !m.trim().is_empty() => Self {
                provider: Some(p.to_string()),
                model: m.trim().to_string(),
            },
            _ => Self {
                provider: None,
                model: s.to_string(),
            },
        })
    }

    fn from_value(v: &Value) -> Option<Self> {
        match v {
            Value::String(s) => Self::parse(s),
            Value::Object(m) => {
                let model = m.get("model")?.as_str()?.trim();
                (!model.is_empty()).then(|| Self {
                    provider: m
                        .get("provider")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|p| !p.is_empty())
                        .map(String::from),
                    model: model.to_string(),
                })
            }
            _ => None,
        }
    }

    /// Whether this names `model` on `provider` (a bare id matches any
    /// provider).
    pub fn is(&self, provider: &str, model: &str) -> bool {
        self.model == model && self.provider.as_deref().is_none_or(|p| p == provider)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct TaskOptions {
    pub tier_low: Option<ModelRef>,
    pub tier_medium: Option<ModelRef>,
    pub tier_high: Option<ModelRef>,
    /// `verify.auto`: None means "decide per task".
    pub verify_auto: Option<bool>,
    pub verify_tests: bool,
    /// `verify.required` (the `ci` profile): verify whenever a build system
    /// is detected.
    pub verify_required: bool,
    pub verify_debounce: Duration,
    pub verify_wait: Duration,
    pub verify_progress_every: u32,
    /// `intents.enabled`: None means "on for explicit tasks only".
    pub intents_enabled: Option<bool>,
    pub intents_required: bool,
}

impl Default for TaskOptions {
    fn default() -> Self {
        Self {
            tier_low: None,
            tier_medium: None,
            tier_high: None,
            verify_auto: None,
            verify_tests: false,
            verify_required: false,
            verify_debounce: Duration::from_millis(1500),
            verify_wait: Duration::from_secs(2),
            verify_progress_every: 3,
            intents_enabled: None,
            intents_required: false,
        }
    }
}

impl TaskOptions {
    /// The options in effect. `[intents]` is a typed settings table (it
    /// also holds the note's `show`), so it is read from there; a profile's
    /// `verify.required` (the `ci` profile) turns verification on.
    pub fn current() -> Self {
        let mut o = Self::from_settings(&crate::settings::current());
        o.verify_required |= crate::profiles::verify_required();
        o
    }

    /// The options a resolved settings tree sets.
    pub fn from_settings(settings: &crate::settings::Settings) -> Self {
        let mut o = Self::from_extra(&settings.extra);
        o.intents_enabled = settings.intents.enabled.or(o.intents_enabled);
        o.intents_required |= settings.intents.required;
        o
    }

    pub fn from_extra(extra: &Map<String, Value>) -> Self {
        let mut o = Self::default();
        let table = |k: &str| extra.get(k).and_then(Value::as_object);
        if let Some(t) = table("tier") {
            o.tier_low = t.get("low").and_then(ModelRef::from_value);
            o.tier_medium = t.get("medium").and_then(ModelRef::from_value);
            o.tier_high = t.get("high").and_then(ModelRef::from_value);
        }
        if let Some(v) = table("verify") {
            o.verify_auto = v.get("auto").and_then(Value::as_bool);
            o.verify_tests = v.get("tests").and_then(Value::as_bool).unwrap_or(false);
            o.verify_required = v.get("required").and_then(Value::as_bool).unwrap_or(false);
            let ms = |k: &str| v.get(k).and_then(Value::as_u64).map(Duration::from_millis);
            if let Some(d) = ms("debounce_ms") {
                o.verify_debounce = d;
            }
            if let Some(d) = ms("wait_ms") {
                o.verify_wait = d;
            }
            if let Some(n) = v.get("progress_every").and_then(Value::as_u64) {
                o.verify_progress_every = n.min(u64::from(u32::MAX)) as u32;
            }
        }
        if let Some(i) = table("intents") {
            o.intents_enabled = i.get("enabled").and_then(Value::as_bool);
            o.intents_required = i.get("required").and_then(Value::as_bool).unwrap_or(false);
        }
        o
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn model_refs() {
        assert_eq!(
            ModelRef::parse("openrouter/anthropic/claude"),
            Some(ModelRef {
                provider: Some("openrouter".into()),
                model: "anthropic/claude".into()
            })
        );
        assert_eq!(
            ModelRef::parse(" gpt-5-mini "),
            Some(ModelRef {
                provider: None,
                model: "gpt-5-mini".into()
            })
        );
        assert_eq!(ModelRef::parse("  "), None);
        assert!(ModelRef::parse("m").unwrap().is("any", "m"));
        assert!(!ModelRef::parse("p/m").unwrap().is("q", "m"));
    }

    #[test]
    fn the_ci_profile_requires_verification_and_intents_keys_are_kept() {
        let ci = crate::profiles::resolve(std::path::Path::new("/nonexistent"), Some("ci"))
            .unwrap()
            .unwrap();
        let settings: crate::settings::Settings =
            serde_json::from_value(Value::Object(ci.settings)).unwrap();
        let o = TaskOptions::from_settings(&settings);
        assert!(o.verify_required, "the ci profile turns the verifier on");
        // `[intents]` is a typed table: its task-engine keys survive parsing
        // (and saving) next to the note's `show`.
        let text = "[intents]\nshow = 3\nenabled = true\nrequired = true\n";
        let map = crate::settings::file::parse_str(text).unwrap();
        let settings: crate::settings::Settings =
            serde_json::from_value(Value::Object(map)).unwrap();
        assert_eq!(settings.intents.show, 3);
        let o = TaskOptions::from_settings(&settings);
        assert_eq!(o.intents_enabled, Some(true));
        assert!(o.intents_required);
        let saved = serde_json::to_value(&settings).unwrap();
        assert_eq!(saved["intents"]["required"], true);
        let plain = serde_json::to_value(crate::settings::Settings::default()).unwrap();
        assert_eq!(plain["intents"], json!({"show": 8}));
        assert_eq!(
            TaskOptions::from_settings(&crate::settings::Settings::default()),
            TaskOptions::default()
        );
    }

    #[test]
    fn options_from_settings() {
        let extra = json!({
            "tier": {"low": "openai/mini", "high": {"provider": "anthropic", "model": "opus"}},
            "verify": {"auto": true, "tests": true, "debounce_ms": 10, "progress_every": 2},
            "intents": {"required": true}
        });
        let o = TaskOptions::from_extra(extra.as_object().unwrap());
        assert_eq!(o.tier_low.unwrap().model, "mini");
        assert_eq!(o.tier_high.unwrap().provider.as_deref(), Some("anthropic"));
        assert_eq!(o.tier_medium, None);
        assert_eq!(o.verify_auto, Some(true));
        assert!(o.verify_tests && !o.verify_required);
        assert_eq!(o.verify_debounce, Duration::from_millis(10));
        assert_eq!(o.verify_progress_every, 2);
        assert!(o.intents_required);
        assert_eq!(TaskOptions::from_extra(&Map::new()), TaskOptions::default());
        // Garbage is ignored, not fatal.
        let bad = json!({"tier": "x", "verify": {"auto": "yes"}});
        assert_eq!(
            TaskOptions::from_extra(bad.as_object().unwrap()),
            TaskOptions::default()
        );
    }
}
