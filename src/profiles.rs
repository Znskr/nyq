//! Named settings profiles: `--profile <name>` or `NYQ_PROFILE` selects an
//! overlay applied between the user's settings and the project's (the
//! project's tighten-only rules still apply after it).
//!
//! Built-ins ship as TOML next to this file and are compiled in;
//! `~/.nyq/profiles/<name>.toml` overrides a built-in of the same name or
//! adds a private profile.

use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use serde_json::{Map, Value};

/// One resolved profile: the overlay merged between user and project layers.
pub struct Profile {
    pub name: String,
    pub settings: Map<String, Value>,
    /// "built-in" or the file it was read from (shown in warnings/dumps).
    pub source: String,
}

/// Profiles compiled into the binary.
const BUILTIN: &[(&str, &str)] = &[
    ("default", include_str!("profiles/default.toml")),
    ("minimal", include_str!("profiles/minimal.toml")),
    ("ci", include_str!("profiles/ci.toml")),
    ("sdk", include_str!("profiles/sdk.toml")),
];

/// The names `--profile` accepts out of the box.
pub fn builtin_names() -> Vec<&'static str> {
    BUILTIN.iter().map(|(name, _)| *name).collect()
}

/// Profile names become file names under `~/.nyq/profiles`, so they are
/// restricted to a safe shape.
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Where a user-defined or overriding profile lives.
fn user_path(home: &Path, name: &str) -> PathBuf {
    home.join("profiles").join(format!("{name}.toml"))
}

/// The profile this run asked for: `--profile`, else `NYQ_PROFILE`.
pub fn requested_name(flag: Option<&str>) -> Result<Option<String>> {
    let name = flag
        .map(str::to_string)
        .or_else(|| std::env::var("NYQ_PROFILE").ok())
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty());
    match name {
        None => Ok(None),
        Some(n) if valid_name(&n) => Ok(Some(n)),
        Some(n) => bail!("invalid profile name `{n}` (letters, digits, `_` and `-` only)"),
    }
}

/// Resolve the requested profile. A file at `~/.nyq/profiles/<name>.toml`
/// beats the built-in of the same name; either may be an empty overlay.
pub fn resolve(home: &Path, flag: Option<&str>) -> Result<Option<Profile>> {
    let Some(name) = requested_name(flag)? else {
        return Ok(None);
    };
    let path = user_path(home, &name);
    if path.is_file() {
        let text = std::fs::read_to_string(&path)
            .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", path.display()))?;
        let settings = crate::settings::file::parse_str(&text)
            .map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
        return Ok(Some(Profile {
            name,
            settings,
            source: path.display().to_string(),
        }));
    }
    for (builtin, toml) in BUILTIN {
        if *builtin == name {
            let settings = crate::settings::file::parse_str(toml)
                .map_err(|e| anyhow::anyhow!("built-in profile `{name}`: {e}"))?;
            return Ok(Some(Profile {
                name: name.to_string(),
                settings,
                source: "built-in".to_string(),
            }));
        }
    }
    bail!(
        "unknown profile `{name}` (built-ins: {}; or add {})",
        builtin_names().join(", "),
        path.display()
    )
}

/// A `[features]` toggle from the resolved settings (`features.<name>`),
/// true unless the layer set it to false. Profiles use these to leave out
/// whole subsystems; projects cannot set them (not in `PROJECT_KEYS`).
pub fn feature(name: &str) -> bool {
    crate::settings::current()
        .extra
        .get("features")
        .and_then(|f| f.get(name))
        .and_then(Value::as_bool)
        .unwrap_or(true)
}

/// `headless = true` in a profile makes the session run in print mode.
pub fn headless() -> bool {
    crate::settings::current()
        .extra
        .get("headless")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// The `verify.required` flag the `ci` profile sets; the task engine and
/// validation hooks read it.
#[allow(dead_code)] // Read by the task engine's verify path when it lands.
pub fn verify_required() -> bool {
    crate::settings::current()
        .extra
        .get("verify")
        .and_then(|v| v.get("required"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// A profile's default command timeout for the `bash` tool, when set.
pub fn command_timeout() -> Option<u64> {
    crate::settings::current().command_timeout_secs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtins_parse() {
        for (name, toml) in BUILTIN {
            crate::settings::file::parse_str(toml).unwrap_or_else(|e| {
                panic!("built-in profile `{name}` does not parse: {e}");
            });
        }
    }

    #[test]
    fn names_are_checked() {
        assert!(valid_name("minimal-ci_v2"));
        assert!(!valid_name("../evil"));
        assert!(!valid_name(""));
        assert!(!valid_name("a/b"));
    }

    #[test]
    fn resolves_builtin_and_user_override() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path();
        let p = resolve(home, Some("minimal")).unwrap().unwrap();
        assert_eq!(p.name, "minimal");
        assert_eq!(p.source, "built-in");
        assert_eq!(
            p.settings.get("command_timeout_secs").and_then(Value::as_u64),
            Some(300)
        );

        std::fs::create_dir_all(home.join("profiles")).unwrap();
        std::fs::write(
            user_path(home, "minimal"),
            "command_timeout_secs = 60\n",
        )
        .unwrap();
        let p = resolve(home, Some("minimal")).unwrap().unwrap();
        assert_eq!(
            p.settings.get("command_timeout_secs").and_then(Value::as_u64),
            Some(60)
        );
        assert!(p.source.ends_with("minimal.toml"));

        std::fs::write(user_path(home, "mine"), "theme = \"dark\"\n").unwrap();
        let p = resolve(home, Some("mine")).unwrap().unwrap();
        assert_eq!(p.settings.get("theme").and_then(Value::as_str), Some("dark"));
    }

    #[test]
    fn unknown_profile_fails() {
        let t = tempfile::tempdir().unwrap();
        assert!(resolve(t.path(), Some("nope")).is_err());
        assert!(resolve(t.path(), Some("../x")).is_err());
        assert!(resolve(t.path(), None).unwrap().is_none());
    }
}
