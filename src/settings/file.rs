//! The settings files on disk: `<home>/settings.toml` and a project's
//! `.nyq/settings.toml`. Every reader and writer goes through here and sees
//! a file as a JSON object, so the rest of nyq works with `serde_json` values.
//!
//! Saving edits the parsed document in place: only the changed keys are
//! touched, so the user's comments, ordering and other keys survive. TOML
//! has no null; a removed value deletes its key.
//!
//! Earlier versions used `settings.json`. The user file is converted once
//! ([`migrate`]) and the old file left for older nyq versions; a project's
//! `.nyq/settings.json` is still read (never rewritten) when it has no TOML.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde_json::{Map, Number, Value};
use toml_edit::{DocumentMut, InlineTable, Item, Table, TableLike};

pub const FILE: &str = "settings.toml";
const LEGACY: &str = "settings.json";

/// One changed setting: its key path and new value (`None`: removed).
pub type Change = (Vec<String>, Option<Value>);

pub fn user_path(home: &Path) -> PathBuf {
    home.join(FILE)
}

fn project_dir(cwd: &Path) -> PathBuf {
    cwd.join(".nyq")
}

/// The project file in effect: `.nyq/settings.toml`, else a legacy
/// `.nyq/settings.json` when only that exists.
pub fn project_path(cwd: &Path) -> PathBuf {
    let toml = project_dir(cwd).join(FILE);
    let legacy = project_dir(cwd).join(LEGACY);
    if !toml.exists() && legacy.exists() {
        legacy
    } else {
        toml
    }
}

/// Whether `path` is an old JSON settings file.
pub fn is_legacy(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "json")
}

/// Read a settings file (TOML, or JSON for a legacy `.json` path) as an
/// object. A missing file is empty.
pub fn read(path: &Path) -> Result<Map<String, Value>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Map::new()),
        Err(e) => return Err(format!("reading {}: {e}", path.display())),
    };
    if is_legacy(path) {
        return match serde_json::from_str::<Value>(&text) {
            Ok(Value::Object(m)) => Ok(m),
            Ok(_) => Err(format!("{}: expected a JSON object", path.display())),
            Err(e) => Err(format!("{}: {e}", path.display())),
        };
    }
    let doc = text
        .parse::<DocumentMut>()
        .map_err(|e| format!("{}: {}", path.display(), e.to_string().trim_end()))?;
    Ok(table_to_json(doc.as_table()))
}

/// The user's settings. Until [`migrate`] has run, a legacy `settings.json`
/// is read instead.
pub fn read_user(home: &Path) -> Result<Map<String, Value>, String> {
    let path = user_path(home);
    let legacy = home.join(LEGACY);
    if !path.exists() && legacy.exists() {
        return read(&legacy);
    }
    read(&path)
}

/// Convert the user's `settings.json` to `settings.toml` once, when only the
/// JSON file exists. The JSON file stays for older nyq versions. Returns a
/// notice when it converted. JSON that doesn't parse is left alone: nyq keeps
/// reading it and warns as before, and converts it once it is fixed.
pub fn migrate(home: &Path) -> Result<Option<String>, String> {
    let path = user_path(home);
    let legacy = home.join(LEGACY);
    if path.exists() || !legacy.exists() {
        return Ok(None);
    }
    let Ok(map) = read(&legacy) else { return Ok(None) };
    let text = format!(
        "# nyq settings. Migrated from settings.json, which nyq no longer reads\n\
         # (it is kept for older nyq versions).\n\n{}",
        json_to_document(&map)
    );
    crate::session::write_private(&path, text.as_bytes())
        .map_err(|e| format!("writing {}: {e}", path.display()))?;
    Ok(Some(format!(
        "Settings moved to {}; {} is no longer read.",
        path.display(),
        legacy.display()
    )))
}

/// Apply `changes` to the user file, keeping everything else in it
/// (comments and formatting included). Written atomically, mode 0600.
pub fn write_changes(home: &Path, changes: &[Change]) -> Result<()> {
    if changes.is_empty() {
        return Ok(());
    }
    // Don't drop the keys of a settings.json that wasn't converted yet.
    let _ = migrate(home);
    let path = user_path(home);
    let mut doc = match std::fs::read_to_string(&path) {
        Ok(t) => match t.parse::<DocumentMut>() {
            Ok(d) => d,
            // Rewriting a broken file would lose what the user wrote.
            Err(e) => bail!("{} does not parse, fix it first: {}", path.display(), e.to_string().trim_end()),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => DocumentMut::new(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    for (p, v) in changes {
        set(doc.as_table_mut(), p, v.as_ref(), false);
    }
    crate::session::write_private(&path, doc.to_string().as_bytes())
        .with_context(|| format!("writing {}", path.display()))
}

/// Notes for `nyq doctor` about old JSON files next to the TOML ones.
pub fn notes(home: &Path, cwd: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let legacy = home.join(LEGACY);
    if user_path(home).exists() && legacy.exists() {
        out.push(format!("{} is ignored ({FILE} is used)", legacy.display()));
    }
    let legacy = project_dir(cwd).join(LEGACY);
    if legacy.exists() {
        if project_dir(cwd).join(FILE).exists() {
            out.push(format!("{} is ignored ({FILE} is used)", legacy.display()));
        } else {
            out.push(format!("{} is the old format; convert it to .nyq/{FILE}", legacy.display()));
        }
    }
    out
}

// ------------------------------------------------------------------ TOML -> JSON

fn table_to_json(t: &dyn TableLike) -> Map<String, Value> {
    t.iter().filter_map(|(k, v)| Some((k.to_string(), item_to_json(v)?))).collect()
}

fn item_to_json(item: &Item) -> Option<Value> {
    match item {
        Item::None => None,
        Item::Value(v) => Some(value_to_json(v)),
        Item::Table(t) => Some(Value::Object(table_to_json(t))),
        Item::ArrayOfTables(a) => Some(Value::Array(a.iter().map(|t| Value::Object(table_to_json(t))).collect())),
    }
}

fn value_to_json(v: &toml_edit::Value) -> Value {
    use toml_edit::Value as T;
    match v {
        T::String(s) => Value::String(s.value().clone()),
        T::Integer(i) => Value::from(*i.value()),
        T::Float(f) => Number::from_f64(*f.value()).map_or(Value::Null, Value::Number),
        T::Boolean(b) => Value::Bool(*b.value()),
        T::Datetime(d) => Value::String(d.value().to_string()),
        T::Array(a) => Value::Array(a.iter().map(value_to_json).collect()),
        T::InlineTable(t) => Value::Object(table_to_json(t)),
    }
}

// ------------------------------------------------------------------ JSON -> TOML

fn json_to_document(map: &Map<String, Value>) -> DocumentMut {
    let mut doc = DocumentMut::new();
    for (k, v) in map {
        set(doc.as_table_mut(), std::slice::from_ref(k), Some(v), false);
    }
    doc
}

/// A JSON value as a TOML item: objects become `[tables]` unless `inline`
/// (inside an inline table or array). `None` for null.
fn json_to_item(v: &Value, inline: bool) -> Option<Item> {
    match v {
        Value::Object(m) if !inline => {
            let mut t = Table::new();
            for (k, v) in m {
                if let Some(item) = json_to_item(v, false) {
                    t.insert(k, item);
                }
            }
            // No bare `[a]` header above `[a.b]` (an empty table keeps its own).
            t.set_implicit(!m.is_empty());
            Some(Item::Table(t))
        }
        _ => json_to_value(v).map(Item::Value),
    }
}

fn json_to_value(v: &Value) -> Option<toml_edit::Value> {
    Some(match v {
        Value::Null => return None,
        Value::Bool(b) => (*b).into(),
        Value::Number(n) => match n.as_i64() {
            Some(i) => i.into(),
            // TOML integers are i64; larger ones become floats.
            None => n.as_f64()?.into(),
        },
        Value::String(s) => s.as_str().into(),
        Value::Array(a) => a.iter().filter_map(json_to_value).collect::<toml_edit::Array>().into(),
        Value::Object(m) => {
            let mut t = InlineTable::new();
            for (k, v) in m {
                if let Some(v) = json_to_value(v) {
                    t.insert(k, v);
                }
            }
            t.into()
        }
    })
}

/// Set (or with `None`, remove) the value at `path`, creating tables on the
/// way. A replaced value keeps its surrounding whitespace and comments.
fn set(table: &mut dyn TableLike, path: &[String], new: Option<&Value>, inline: bool) {
    let Some((key, rest)) = path.split_first() else { return };
    if rest.is_empty() {
        match new.and_then(|v| json_to_item(v, inline)) {
            None => {
                table.remove(key);
            }
            Some(mut item) => match table.get_mut(key) {
                // Replace in place, so the key keeps the comments above it.
                Some(old) => {
                    if let (Item::Value(old), Item::Value(new)) = (&*old, &mut item) {
                        *new.decor_mut() = old.decor().clone();
                    }
                    *old = item;
                }
                None => {
                    table.insert(key, item);
                }
            },
        }
        return;
    }
    if !table.get(key).is_some_and(Item::is_table_like) {
        if new.is_none() {
            return;
        }
        let item = if inline {
            Item::Value(InlineTable::new().into())
        } else {
            // `[a.b]` alone, without an empty `[a]` header.
            let mut t = Table::new();
            t.set_implicit(true);
            Item::Table(t)
        };
        table.insert(key, item);
    }
    let Some(child) = table.get_mut(key) else { return };
    let inline = inline || child.is_inline_table();
    if let Some(child) = child.as_table_like_mut() {
        set(child, rest, new, inline);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn change(path: &[&str], v: Option<Value>) -> Change {
        (path.iter().map(|s| s.to_string()).collect(), v)
    }

    #[test]
    fn save_keeps_comments_and_order() {
        let dir = tempfile::tempdir().unwrap();
        let text = "# my settings\nsound = \"on\" # loud\n\n# models I use\n[models]\nxai = \"grok\"\n\n[statusline]\ntokens = false\n";
        std::fs::write(user_path(dir.path()), text).unwrap();
        write_changes(
            dir.path(),
            &[
                change(&["sound"], Some(json!("max"))),
                change(&["statusline", "branch"], Some(json!(false))),
                change(&["models", "xai"], None),
                change(&["notifications"], Some(json!(true))),
            ],
        )
        .unwrap();
        let out = std::fs::read_to_string(user_path(dir.path())).unwrap();
        assert_eq!(
            out,
            "# my settings\nsound = \"max\" # loud\nnotifications = true\n\n# models I use\n[models]\n\n[statusline]\ntokens = false\nbranch = false\n"
        );
        // Untouched files aren't rewritten into a different shape.
        write_changes(dir.path(), &[]).unwrap();
        assert_eq!(std::fs::read_to_string(user_path(dir.path())).unwrap(), out);
    }

    #[test]
    fn broken_file_is_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(user_path(dir.path()), "sound = ").unwrap();
        assert!(read_user(dir.path()).is_err());
        assert!(write_changes(dir.path(), &[change(&["sound"], Some(json!("on")))]).is_err());
        assert_eq!(std::fs::read_to_string(user_path(dir.path())).unwrap(), "sound = ");
    }

    #[test]
    fn nested_tables_and_arrays_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let value = json!({
            "sound": "on",
            "limit": 12,
            "ratio": 0.5,
            "list": ["a", "b"],
            "mixed": [1, "two", {"three": 3}],
            "objects": [{"name": "x", "on": true}],
            "context_limits": {"read_file": 1000, "shell": "off"},
            "workspaces": {"~/src/app": {"sound": "off", "statusline": {"tokens": false}}},
            "a.b": {"c d": {"e": []}}
        });
        let Value::Object(map) = value.clone() else { unreachable!() };
        let changes: Vec<Change> = map.iter().map(|(k, v)| change(&[k], Some(v.clone()))).collect();
        write_changes(dir.path(), &changes).unwrap();
        assert_eq!(Value::Object(read_user(dir.path()).unwrap()), value);
        let text = std::fs::read_to_string(user_path(dir.path())).unwrap();
        assert!(text.contains("[workspaces.\"~/src/app\"]\nsound = \"off\""), "{text}");
        assert!(!text.contains("[workspaces]"), "{text}");
        // A path key inside an existing inline table stays inline.
        std::fs::write(user_path(dir.path()), "workspaces = { \"~/src/app\" = { sound = \"off\" } }\n").unwrap();
        write_changes(dir.path(), &[change(&["workspaces", "~/src/app", "auto_compact"], Some(json!(50)))]).unwrap();
        let back = read_user(dir.path()).unwrap();
        assert_eq!(back["workspaces"]["~/src/app"], json!({"sound": "off", "auto_compact": 50}));
        // Nulls have no TOML form: they are dropped.
        write_changes(dir.path(), &[change(&["x"], Some(json!({"y": null, "z": [null, 1]})))]).unwrap();
        assert_eq!(read_user(dir.path()).unwrap()["x"], json!({"z": [1]}));
        // TOML-only values read as strings.
        std::fs::write(user_path(dir.path()), "when = 1979-05-27T07:32:00Z\n").unwrap();
        assert_eq!(read_user(dir.path()).unwrap()["when"], "1979-05-27T07:32:00Z");
    }

    #[test]
    fn migrates_the_user_json_once() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let json = r#"{"provider":"codex","sound":"max","effort":null,"models":{"codex":"m"},"workspaces":{"~/src/app":{"sound":"off"}}}"#;
        std::fs::write(home.join(LEGACY), json).unwrap();
        // Before the conversion the JSON file is still read.
        assert_eq!(read_user(home).unwrap()["provider"], "codex");
        let notice = migrate(home).unwrap().unwrap();
        assert!(notice.contains(FILE), "{notice}");
        assert_eq!(std::fs::read_to_string(home.join(LEGACY)).unwrap(), json, "settings.json stays as it was");
        let text = std::fs::read_to_string(user_path(home)).unwrap();
        assert!(text.starts_with("# nyq settings. Migrated from settings.json"), "{text}");
        let map = read_user(home).unwrap();
        assert_eq!(
            Value::Object(map),
            json!({"provider":"codex","sound":"max","models":{"codex":"m"},"workspaces":{"~/src/app":{"sound":"off"}}})
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(user_path(home)).unwrap().permissions().mode() & 0o777, 0o600);
        }
        // Only once: later the JSON file is ignored.
        assert_eq!(migrate(home), Ok(None));
        std::fs::write(home.join(LEGACY), r#"{"sound":"on"}"#).unwrap();
        assert_eq!(read_user(home).unwrap()["sound"], "max");
        assert_eq!(notes(home, home).len(), 1);
        assert!(notes(home, home)[0].contains("ignored"));
    }

    #[test]
    fn bad_json_is_not_migrated() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(LEGACY), "{").unwrap();
        assert_eq!(migrate(dir.path()), Ok(None));
        assert!(!user_path(dir.path()).exists());
        assert!(read_user(dir.path()).is_err());
    }

    #[test]
    fn legacy_project_json_is_read_not_written() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("repo");
        let home = dir.path().join("home");
        std::fs::create_dir_all(cwd.join(".nyq")).unwrap();
        assert_eq!(project_path(&cwd), cwd.join(".nyq").join(FILE));
        std::fs::write(cwd.join(".nyq").join(LEGACY), r#"{"sound":"max"}"#).unwrap();
        assert_eq!(project_path(&cwd), cwd.join(".nyq").join(LEGACY));
        assert_eq!(read(&project_path(&cwd)).unwrap()["sound"], "max");
        assert!(notes(&home, &cwd)[0].contains("convert it"));
        std::fs::write(cwd.join(".nyq").join(FILE), "sound = \"on\"\n").unwrap();
        assert_eq!(project_path(&cwd), cwd.join(".nyq").join(FILE));
        assert!(notes(&home, &cwd)[0].contains("ignored"));
    }
}
