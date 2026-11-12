//! The intent field (design §4.4): mutating tools take an optional
//! `intent: {did, why, next}`. The field is added to their schemas when the
//! request is built and removed from the arguments before the tool parses
//! them, so no tool has to know about it.
//!
//! Which tools are mutating comes from their declared effects
//! ([`crate::tools::Tool::effects`]), not from a list of names. Stated
//! intents go to the session's one intent store, the working set's
//! (`crate::tools::workset::Workset::record`), which persists them and
//! renders the recent ones in the trailing note.

use serde_json::{json, Map, Value};

use crate::tools::workset::Intent;
use crate::tools::Tool;
use crate::types::ToolSpec;

pub const FIELD: &str = "intent";

/// Whether calls to `tool` change the workspace, by its static kind
/// ([`crate::tools::ToolKind::mutates`]), so the schema the model saw matches
/// what is stripped whatever the call's arguments. Coordination tools
/// (delegation, scripts) change nothing themselves: their children's and
/// sub-calls' own calls carry intents.
pub fn is_mutating(tool: &dyn Tool) -> bool {
    tool.kind().mutates()
}

/// Whether the tool's own schema already has an `intent` property: then it
/// is the tool's argument, never ours to add or strip.
pub fn declares_field(spec: &ToolSpec) -> bool {
    spec.parameters
        .get("properties")
        .and_then(|p| p.get(FIELD))
        .is_some()
}

fn schema() -> Value {
    json!({
        "type": "object",
        "description": "Why you are making this change, in a few words each. It is kept as a log of your recent actions.",
        "properties": {
            "did": {"type": "string", "description": "What this call does."},
            "why": {"type": "string", "description": "Why: the reason or goal it serves."},
            "next": {"type": "string", "description": "What you plan to do after it."}
        },
        "required": ["did", "why"]
    })
}

/// Add the `intent` property to a (mutating) tool's schema; `required` makes
/// it mandatory. A tool that already has a property of that name is left
/// alone.
pub fn wrap(spec: &mut ToolSpec, required: bool) -> bool {
    if declares_field(spec) {
        return false;
    }
    let Some(obj) = spec.parameters.as_object_mut() else {
        return false;
    };
    let props = obj
        .entry("properties")
        .or_insert_with(|| Value::Object(Map::new()));
    let Some(props) = props.as_object_mut() else {
        return false;
    };
    props.insert(FIELD.into(), schema());
    if required {
        let req = obj
            .entry("required")
            .or_insert_with(|| Value::Array(Vec::new()));
        if let Some(list) = req.as_array_mut() {
            list.push(FIELD.into());
        }
    }
    true
}

/// Parse a stated intent. A model that sends a sentence instead of the
/// object still said what it is doing.
fn parse(v: &Value) -> Option<Intent> {
    let text = |k: &str| {
        v.get(k)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("")
    };
    match v {
        Value::String(s) if !s.trim().is_empty() => Some(Intent::new(s, "", "")),
        Value::Object(_) if !text("did").is_empty() => {
            Some(Intent::new(text("did"), text("why"), text("next")))
        }
        _ => None,
    }
}

/// Remove `intent` from a call's arguments. `Some(None)`: the field was
/// there but unusable; `None`: it was absent. The caller decides whether the
/// field is ours to take (a mutating tool that does not declare it).
pub fn take(args: &mut Value) -> Option<Option<Intent>> {
    let v = args.as_object_mut()?.remove(FIELD)?;
    Some(parse(&v))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(name: &str) -> ToolSpec {
        ToolSpec {
            name: name.into(),
            description: String::new(),
            parameters: json!({"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}),
        }
    }

    #[test]
    fn mutating_comes_from_declared_effects() {
        let r = crate::tools::Registry::all_builtin();
        for name in [
            "write_file",
            "edit_file",
            "multi_edit",
            "edit_range",
            "apply_patch",
            "bash",
            "memory",
        ] {
            assert!(is_mutating(r.get(name).unwrap()), "{name}");
        }
        for name in [
            "read_file",
            "list_dir",
            "glob",
            "grep",
            "web_fetch",
            "note_intent",
        ] {
            assert!(!is_mutating(r.get(name).unwrap()), "{name}");
        }
    }

    #[test]
    fn wrap_adds_the_field_once_and_leaves_a_tools_own_field() {
        let mut w = spec("write_file");
        assert!(wrap(&mut w, false));
        assert!(w.parameters["properties"][FIELD].is_object());
        assert_eq!(w.parameters["required"], json!(["path"]));
        assert!(!wrap(&mut w, true), "already wrapped");
        let mut b = spec("bash");
        assert!(wrap(&mut b, true));
        assert_eq!(b.parameters["required"], json!(["path", "intent"]));
        let mut own = spec("mcp__x__plan");
        own.parameters["properties"][FIELD] = json!({"type": "string"});
        assert!(declares_field(&own));
        assert!(!wrap(&mut own, true));
        assert_eq!(
            own.parameters["properties"][FIELD],
            json!({"type": "string"})
        );
    }

    #[test]
    fn take_strips_and_parses() {
        let mut args = json!({"path": "a", "intent": {"did": "fix", "why": "bug", "next": "test"}});
        let got = take(&mut args).unwrap().unwrap();
        assert_eq!(args, json!({"path": "a"}));
        assert_eq!(got.line(), "fix — because bug — next: test");
        let mut bad = json!({"intent": 3});
        assert_eq!(take(&mut bad), Some(None));
        assert_eq!(bad, json!({}));
        let mut no_did = json!({"intent": {"why": "x"}});
        assert_eq!(take(&mut no_did), Some(None));
        let mut none = json!({"path": "a"});
        assert_eq!(take(&mut none), None);
        let mut s = json!({"intent": "just a sentence"});
        assert_eq!(take(&mut s).unwrap().unwrap().did, "just a sentence");
    }
}
