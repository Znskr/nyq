//! The intent field (design §4.4): mutating tools take an optional
//! `intent: {did, why, next}`. The field is added to their schemas when the
//! request is built and removed from the arguments before the tool parses
//! them, so no tool has to know about it.
//!
//! [`IntentLog`] is a minimal local store. TODO(W10): replace it with the
//! session's intent store (`record_intent()`), which also renders the last
//! intents in the trailing note.

use std::collections::VecDeque;

use serde_json::{json, Map, Value};

use crate::types::ToolSpec;

pub const FIELD: &str = "intent";

/// Built-in tools that change the workspace. TODO(W7): use `Tool::effects`.
const MUTATING: &[&str] = &[
    "write_file",
    "edit_file",
    "multi_edit",
    "apply_patch",
    "edit_range",
    "bash",
];

pub fn is_mutating(tool: &str) -> bool {
    MUTATING.contains(&tool)
}

/// Tools whose successful call writes the file named by their `path`.
pub fn writes_path(tool: &str) -> bool {
    matches!(
        tool,
        "write_file" | "edit_file" | "multi_edit" | "edit_range"
    )
}

/// File-editing tools: a successful call changed the workspace.
pub fn edits_files(tool: &str) -> bool {
    writes_path(tool) || tool == "apply_patch"
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

/// Add the `intent` property to a mutating tool's schema (`required` makes it
/// mandatory). A tool that already has a property of that name is left alone.
pub fn wrap(spec: &mut ToolSpec, required: bool) -> bool {
    if !is_mutating(&spec.name) {
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
    if props.contains_key(FIELD) {
        return false;
    }
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

/// One stated intent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Intent {
    pub did: String,
    pub why: String,
    pub next: Option<String>,
}

impl Intent {
    fn from_value(v: &Value) -> Option<Self> {
        let text = |k: &str| {
            v.get(k)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
        };
        match v {
            // A model that sends a sentence instead of the object still said
            // what it is doing.
            Value::String(s) if !s.trim().is_empty() => Some(Self {
                did: s.trim().to_string(),
                why: String::new(),
                next: None,
            }),
            Value::Object(_) => {
                let did = text("did")?;
                Some(Self {
                    did,
                    why: text("why").unwrap_or_default(),
                    next: text("next"),
                })
            }
            _ => None,
        }
    }

    /// `did / why / next` on one line, as the cycle detector compares them.
    pub fn line(&self) -> String {
        let mut s = self.did.clone();
        if !self.why.is_empty() {
            s.push_str(" — because ");
            s.push_str(&self.why);
        }
        if let Some(n) = &self.next {
            s.push_str(" — next: ");
            s.push_str(n);
        }
        s
    }
}

/// Remove `intent` from a mutating tool's arguments. `Some(None)`: the field
/// was there but unusable; `None`: it was absent (or the tool is not
/// mutating, in which case nothing is removed).
pub fn take(tool: &str, args: &mut Value) -> Option<Option<Intent>> {
    if !is_mutating(tool) {
        return None;
    }
    let v = args.as_object_mut()?.remove(FIELD)?;
    Some(Intent::from_value(&v))
}

/// What kind of entry an intent is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IntentKind {
    /// Stated by the model on a tool call.
    Action,
    /// A diagnosis the harness injected (ladder rung 3).
    ApproachCorrection,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IntentRecord {
    pub tool: String,
    pub intent: Intent,
    pub kind: IntentKind,
}

/// The last intents of this session, newest last.
#[derive(Clone, Debug, Default)]
pub struct IntentLog {
    entries: VecDeque<IntentRecord>,
    /// Mutating calls made without an intent (accepted but counted).
    pub missing: u32,
}

const KEEP: usize = 32;

impl IntentLog {
    pub fn record(&mut self, tool: &str, intent: Intent, kind: IntentKind) {
        self.entries.push_back(IntentRecord {
            tool: tool.to_string(),
            intent,
            kind,
        });
        while self.entries.len() > KEEP {
            self.entries.pop_front();
        }
    }

    pub fn recent(&self, k: usize) -> impl Iterator<Item = &IntentRecord> {
        self.entries
            .iter()
            .skip(self.entries.len().saturating_sub(k))
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
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
    fn wraps_only_mutating_tools() {
        let mut w = spec("write_file");
        assert!(wrap(&mut w, false));
        assert!(w.parameters["properties"][FIELD].is_object());
        assert_eq!(w.parameters["required"], json!(["path"]));
        assert!(!wrap(&mut w, true), "already wrapped");
        let mut r = spec("read_file");
        assert!(!wrap(&mut r, true));
        assert!(r.parameters["properties"].get(FIELD).is_none());
        let mut b = spec("bash");
        assert!(wrap(&mut b, true));
        assert_eq!(b.parameters["required"], json!(["path", "intent"]));
    }

    #[test]
    fn take_strips_and_parses() {
        let mut args = json!({"path": "a", "intent": {"did": "fix", "why": "bug", "next": "test"}});
        let got = take("edit_file", &mut args).unwrap().unwrap();
        assert_eq!(args, json!({"path": "a"}));
        assert_eq!(got.line(), "fix — because bug — next: test");
        let mut bad = json!({"intent": 3});
        assert_eq!(take("bash", &mut bad), Some(None));
        assert_eq!(bad, json!({}));
        let mut none = json!({"path": "a"});
        assert_eq!(take("bash", &mut none), None);
        let mut read = json!({"intent": {"did": "x"}});
        assert_eq!(take("read_file", &mut read), None);
        assert!(
            read.get(FIELD).is_some(),
            "non-mutating tools keep their args"
        );
        let mut s = json!({"intent": "just a sentence"});
        assert_eq!(
            take("bash", &mut s).unwrap().unwrap().did,
            "just a sentence"
        );
    }

    #[test]
    fn log_keeps_the_newest() {
        let mut log = IntentLog::default();
        for i in 0..40 {
            log.record(
                "bash",
                Intent {
                    did: i.to_string(),
                    why: String::new(),
                    next: None,
                },
                IntentKind::Action,
            );
        }
        assert_eq!(log.len(), KEEP);
        let last: Vec<_> = log.recent(2).map(|r| r.intent.did.clone()).collect();
        assert_eq!(last, ["38", "39"]);
    }
}
