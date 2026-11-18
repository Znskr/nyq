//! Deferred built-in tools. Rarely needed tools are not sent with every
//! request: their names and a few words each are listed in `load_tools`,
//! which adds their full schemas from the next step on. A direct call to one
//! also works (and loads it). `NYQ_DEFER_TOOLS=0` lists everything directly.

use serde_json::{json, Value};

use crate::types::ToolSpec;

pub(crate) const LOAD: &str = "load_tools";

/// The deferred tools, with the words `load_tools` shows for each.
const SUMMARIES: &[(&str, &str)] = &[
    ("edit_range", "regex-anchored line-range edit"),
    ("process_write", "stdin to a background process"),
    ("process_wait", "await a background process"),
    ("session_search", "search past conversation"),
    ("memory", "saved memories"),
    ("skill", "load a skill"),
    ("install_skill", "install a skill"),
    ("workflow", "scripted workflow"),
    ("run_code", "script calling tools"),
    ("send_message", "message a subagent"),
    ("wait_agent", "await subagents"),
    ("team_task_create", "team tasks"),
    ("team_task_update", "team tasks"),
    ("team_task_claim", "team tasks"),
    ("team_task_list", "team tasks"),
    ("open", "working set"),
    ("close", "working set"),
    ("focus", "working set"),
    ("unfocus", "working set"),
    ("workset", "working set"),
    ("note_intent", "record an intent"),
];

pub(crate) const DEFERRED: &[&str] = &[
    "edit_range",
    "process_write",
    "process_wait",
    "session_search",
    "memory",
    "skill",
    "install_skill",
    "workflow",
    "run_code",
    "send_message",
    "wait_agent",
    "team_task_create",
    "team_task_update",
    "team_task_claim",
    "team_task_list",
    "open",
    "close",
    "focus",
    "unfocus",
    "workset",
    "note_intent",
];

/// `NYQ_DEFER_TOOLS=0` (or `false`/`off`): send every tool's schema.
pub(crate) fn disabled_by_env() -> bool {
    std::env::var("NYQ_DEFER_TOOLS")
        .map(|v| matches!(v.trim(), "0" | "false" | "off" | "no"))
        .unwrap_or(false)
}

fn summary(name: &str) -> &'static str {
    SUMMARIES
        .iter()
        .find(|(n, _)| *n == name)
        .map_or("", |(_, s)| s)
}

/// The `load_tools` spec listing the held-back tools.
pub(crate) fn load_spec(held: &[ToolSpec]) -> ToolSpec {
    let list: Vec<String> = held
        .iter()
        .map(|s| match summary(&s.name) {
            "" => s.name.clone(),
            w => format!("{} ({w})", s.name),
        })
        .collect();
    ToolSpec {
        name: LOAD.into(),
        description: format!(
            "Load tools by name; their schemas arrive from the next step. Available: {}.",
            list.join("; ")
        ),
        parameters: json!({
            "type": "object",
            "properties": {"names": {"type": "array", "items": {"type": "string"}}},
            "required": ["names"]
        }),
    }
}

/// The names to load, checked against the available deferred tools.
pub(crate) fn load(args: &Value, available: &[String]) -> Result<Vec<String>, String> {
    let names: Vec<String> = match args.get("names") {
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        Some(Value::String(s)) => s
            .split([',', ' '])
            .filter(|n| !n.is_empty())
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    };
    if names.is_empty() {
        return Err("Pass `names`: the tools to load.".into());
    }
    let unknown: Vec<&String> = names.iter().filter(|n| !available.contains(n)).collect();
    if !unknown.is_empty() {
        return Err(format!(
            "Not a loadable tool: {}. Loadable: {}.",
            unknown
                .iter()
                .map(|n| n.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            available.join(", ")
        ));
    }
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_deferred_tool_has_a_summary_and_the_list_parses() {
        for name in DEFERRED {
            assert!(!summary(name).is_empty(), "{name}");
        }
        let avail = vec!["memory".to_string(), "edit_range".to_string()];
        assert_eq!(
            load(&json!({"names": ["memory"]}), &avail).unwrap(),
            vec!["memory"]
        );
        assert_eq!(
            load(&json!({"names": "memory, edit_range"}), &avail)
                .unwrap()
                .len(),
            2
        );
        assert!(load(&json!({"names": ["bash"]}), &avail)
            .unwrap_err()
            .contains("Not a loadable tool"));
        assert!(load(&json!({}), &avail).is_err());
    }
}
