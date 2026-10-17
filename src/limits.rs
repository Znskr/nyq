//! Prompt-budget limits: how much skill, MCP and project text goes to the
//! model. Each has a default; `context_limits` in settings.json and
//! `--context-limit NAME=BYTES|off` (highest) override it for a run.

use std::sync::Mutex;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

/// (name, default, what it caps); None means unlimited.
pub const LIMITS: &[(&str, Option<usize>, &str)] = &[
    ("skill_description_bytes", Some(120), "each skill description in the system prompt"),
    ("skill_catalog_bytes", None, "the whole skill list in the system prompt"),
    ("mcp_description_bytes", None, "each MCP tool description"),
    ("mcp_content_block_bytes", Some(8 * 1024), "each text block of an MCP prompt or resource"),
    ("mcp_content_total_bytes", Some(32 * 1024), "a whole MCP prompt or resource"),
    ("project_instruction_file_bytes", None, "each project instruction or prompt file (AGENTS.md, system.md, ...)"),
    ("project_instructions_total_bytes", None, "all project instructions together"),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Default,
    Settings,
    Flag,
}

impl Source {
    fn as_str(self) -> &'static str {
        match self {
            Source::Default => "default",
            Source::Settings => "settings",
            Source::Flag => "--context-limit",
        }
    }
}

type Overrides = Vec<(&'static str, Option<usize>, Source)>;

static OVERRIDES: Mutex<Overrides> = Mutex::new(Vec::new());

fn known(name: &str) -> Result<&'static str> {
    LIMITS
        .iter()
        .map(|(n, ..)| *n)
        .find(|n| *n == name)
        .with_context(|| {
            let names: Vec<_> = LIMITS.iter().map(|(n, ..)| *n).collect();
            format!("unknown context limit `{name}` (expected one of: {})", names.join(", "))
        })
}

fn parse_value(name: &str, raw: &str) -> Result<Option<usize>> {
    match raw.trim() {
        "off" | "none" => Ok(None),
        n => n
            .parse::<usize>()
            .ok()
            .filter(|n| *n > 0)
            .map(Some)
            .with_context(|| format!("context limit {name}: expected a positive byte count or `off`, got `{raw}`")),
    }
}

/// Parse one `NAME=BYTES|off` flag value.
pub fn parse(spec: &str) -> Result<(&'static str, Option<usize>)> {
    let Some((name, value)) = spec.split_once('=') else {
        bail!("--context-limit expects NAME=BYTES or NAME=off, got `{spec}`");
    };
    let name = known(name.trim())?;
    Ok((name, parse_value(name, value)?))
}

/// The settings' `context_limits` object, then the flags over it.
fn build(settings: &crate::settings::Settings, flags: &[String]) -> Result<Overrides> {
    let mut out = Vec::new();
    if let Some(map) = settings.extra.get("context_limits").and_then(Value::as_object) {
        for (name, value) in map {
            let name = known(name).context("settings.json context_limits")?;
            let limit = match value {
                Value::Number(n) => parse_value(name, &n.to_string())?,
                Value::String(s) => parse_value(name, s)?,
                Value::Null => None,
                other => bail!("settings.json context_limits.{name}: unexpected value {other}"),
            };
            out.push((name, limit, Source::Settings));
        }
    }
    for spec in flags {
        let (name, limit) = parse(spec)?;
        out.push((name, limit, Source::Flag));
    }
    Ok(out)
}

/// Install the limits for this run.
pub fn configure(settings: &crate::settings::Settings, flags: &[String]) -> Result<()> {
    let out = build(settings, flags)?;
    *OVERRIDES.lock().unwrap_or_else(|e| e.into_inner()) = out;
    Ok(())
}

fn resolve_in(overrides: &Overrides, name: &str) -> (Option<usize>, Source) {
    if let Some((_, limit, source)) = overrides.iter().rev().find(|(n, ..)| *n == name) {
        return (*limit, *source);
    }
    let default = LIMITS.iter().find(|(n, ..)| *n == name).and_then(|(_, d, _)| *d);
    (default, Source::Default)
}

fn resolve(name: &str) -> (Option<usize>, Source) {
    resolve_in(&OVERRIDES.lock().unwrap_or_else(|e| e.into_inner()), name)
}

/// The limit in effect for `name` (one of [`LIMITS`]); None is unlimited.
pub fn get(name: &str) -> Option<usize> {
    debug_assert!(LIMITS.iter().any(|(n, ..)| *n == name), "unknown limit {name}");
    resolve(name).0
}

/// `text` cut to the `name` limit (see [`clip_to`]).
pub fn clip(name: &str, text: &str) -> String {
    clip_to(get(name), text)
}

/// `text` cut to `max` bytes on a character boundary, with a marker.
pub fn clip_to(max: Option<usize>, text: &str) -> String {
    match max {
        Some(max) if text.len() > max => {
            let marker = "\n[truncated]";
            let mut end = max.saturating_sub(marker.len());
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            format!("{}{marker}", &text[..end])
        }
        _ => text.to_string(),
    }
}

/// Every limit with its value and where it came from, for status and doctor.
pub fn report() -> Value {
    Value::Array(
        LIMITS
            .iter()
            .map(|(name, ..)| {
                let (limit, source) = resolve(name);
                json!({"name": name, "bytes": limit, "source": source.as_str()})
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_override_settings_and_defaults() {
        assert!(parse("nope=1").is_err());
        assert!(parse("skill_catalog_bytes").is_err());
        assert!(parse("skill_catalog_bytes=0").is_err());
        assert_eq!(parse("skill_catalog_bytes=off").unwrap(), ("skill_catalog_bytes", None));
        let mut s = crate::settings::Settings::default();
        s.extra.insert(
            "context_limits".into(),
            json!({"skill_catalog_bytes": 500, "project_instructions_total_bytes": "40"}),
        );
        let o = build(&s, &["project_instructions_total_bytes=64".into()]).unwrap();
        assert_eq!(resolve_in(&o, "skill_catalog_bytes"), (Some(500), Source::Settings));
        assert_eq!(resolve_in(&o, "project_instructions_total_bytes"), (Some(64), Source::Flag));
        assert_eq!(resolve_in(&o, "mcp_content_total_bytes"), (Some(32 * 1024), Source::Default));
        s.extra.insert("context_limits".into(), json!({"bogus": 1}));
        assert!(build(&s, &[]).is_err());
        let clipped = clip_to(Some(64), &"é".repeat(100));
        assert!(clipped.len() <= 64 && clipped.ends_with("[truncated]"), "{clipped}");
        assert_eq!(clip_to(None, "short"), "short");
        assert_eq!(report().as_array().unwrap().len(), LIMITS.len());
    }
}
