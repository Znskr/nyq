//! The settings files on disk: `<home>/settings.toml` and a project's
//! `.nyq/settings.toml`. Every reader and writer goes through here and sees
//! a file as a JSON object, so the rest of nyq works with `serde_json` values.
//!
//! Saving edits the parsed document in place: only the changed keys are
//! touched, so the user's comments, ordering and other keys survive. TOML
//! has no null; a removed value deletes its key.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde_json::{Map, Number, Value};
use toml_edit::{DocumentMut, InlineTable, Item, Table, TableLike};

pub const FILE: &str = "settings.toml";

/// One changed setting: its key path and new value (`None`: removed).
pub type Change = (Vec<String>, Option<Value>);

pub fn user_path(home: &Path) -> PathBuf {
    home.join(FILE)
}

pub fn project_path(cwd: &Path) -> PathBuf {
    cwd.join(".nyq").join(FILE)
}

/// Read a settings file as an object. A missing file is empty.
pub fn read(path: &Path) -> Result<Map<String, Value>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Map::new()),
        Err(e) => return Err(format!("reading {}: {e}", path.display())),
    };
    let doc = text
        .parse::<DocumentMut>()
        .map_err(|e| format!("{}: {}", path.display(), e.to_string().trim_end()))?;
    Ok(table_to_json(doc.as_table()))
}

/// The user's settings.
pub fn read_user(home: &Path) -> Result<Map<String, Value>, String> {
    read(&user_path(home))
}

/// Apply `changes` to the user file, keeping everything else in it
/// (comments and formatting included). Written atomically, mode 0600.
pub fn write_changes(home: &Path, changes: &[Change]) -> Result<()> {
    if changes.is_empty() {
        return Ok(());
    }
    let path = user_path(home);
    let mut doc = match std::fs::read_to_string(&path) {
        Ok(t) => match t.parse::<DocumentMut>() {
            Ok(d) => d,
            // Rewriting a broken file would lose what the user wrote.
            Err(e) => bail!(
                "{} does not parse, fix it first: {}",
                path.display(),
                e.to_string().trim_end()
            ),
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

// ------------------------------------------------------------------ TOML -> JSON

fn table_to_json(t: &dyn TableLike) -> Map<String, Value> {
    t.iter()
        .filter_map(|(k, v)| Some((k.to_string(), item_to_json(v)?)))
        .collect()
}

fn item_to_json(item: &Item) -> Option<Value> {
    match item {
        Item::None => None,
        Item::Value(v) => Some(value_to_json(v)),
        Item::Table(t) => Some(Value::Object(table_to_json(t))),
        Item::ArrayOfTables(a) => Some(Value::Array(
            a.iter().map(|t| Value::Object(table_to_json(t))).collect(),
        )),
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
        Value::Array(a) => a
            .iter()
            .filter_map(json_to_value)
            .collect::<toml_edit::Array>()
            .into(),
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
    let Some((key, rest)) = path.split_first() else {
        return;
    };
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
    let Some(child) = table.get_mut(key) else {
        return;
    };
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
        assert_eq!(
            std::fs::read_to_string(user_path(dir.path())).unwrap(),
            "sound = "
        );
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
        let Value::Object(map) = value.clone() else {
            unreachable!()
        };
        let changes: Vec<Change> = map
            .iter()
            .map(|(k, v)| change(&[k], Some(v.clone())))
            .collect();
        write_changes(dir.path(), &changes).unwrap();
        assert_eq!(Value::Object(read_user(dir.path()).unwrap()), value);
        let text = std::fs::read_to_string(user_path(dir.path())).unwrap();
        assert!(
            text.contains("[workspaces.\"~/src/app\"]\nsound = \"off\""),
            "{text}"
        );
        assert!(!text.contains("[workspaces]"), "{text}");
        // A path key inside an existing inline table stays inline.
        std::fs::write(
            user_path(dir.path()),
            "workspaces = { \"~/src/app\" = { sound = \"off\" } }\n",
        )
        .unwrap();
        write_changes(
            dir.path(),
            &[change(
                &["workspaces", "~/src/app", "auto_compact"],
                Some(json!(50)),
            )],
        )
        .unwrap();
        let back = read_user(dir.path()).unwrap();
        assert_eq!(
            back["workspaces"]["~/src/app"],
            json!({"sound": "off", "auto_compact": 50})
        );
        // Nulls have no TOML form: they are dropped.
        write_changes(
            dir.path(),
            &[change(&["x"], Some(json!({"y": null, "z": [null, 1]})))],
        )
        .unwrap();
        assert_eq!(read_user(dir.path()).unwrap()["x"], json!({"z": [1]}));
        // TOML-only values read as strings.
        std::fs::write(user_path(dir.path()), "when = 1979-05-27T07:32:00Z\n").unwrap();
        assert_eq!(
            read_user(dir.path()).unwrap()["when"],
            "1979-05-27T07:32:00Z"
        );
    }
}
