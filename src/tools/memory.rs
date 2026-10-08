//! `memory`: durable facts the user asked nyq to remember, stored as a JSON
//! array of strings in `<home>/memory.json` and listed in the system prompt.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use super::{Registry, Tool, ToolCtx, ToolOutput};
use crate::types::ToolSpec;

/// The store is refused (and left untouched) beyond this size.
const MAX_STORE_BYTES: u64 = 1024 * 1024;
/// A single fact longer than this is refused.
const MAX_FACT_CHARS: usize = 1_000;
/// The system prompt lists at most this many facts and characters.
const PROMPT_MAX_FACTS: usize = 50;
const PROMPT_MAX_CHARS: usize = 4_000;
/// Start of the prompt section; [`refresh_section`] replaces from here.
const HEADER: &str = "\n\n# Memory\n\nFacts the user asked you to remember in earlier sessions:\n";

pub fn path(home: &Path) -> PathBuf {
    home.join("memory.json")
}

/// The saved facts, oldest first. A missing file is an empty list.
pub fn load(home: &Path) -> Result<Vec<String>, String> {
    let path = path(home);
    let meta = match std::fs::metadata(&path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("memory store could not be read: {e}")),
    };
    if !meta.is_file() {
        return Err(format!("memory store {} is not a regular file", path.display()));
    }
    if meta.len() > MAX_STORE_BYTES {
        return Err(format!("memory store {} exceeds the 1 MiB limit; it was not modified", path.display()));
    }
    let text = std::fs::read_to_string(&path).map_err(|e| format!("memory store could not be read: {e}"))?;
    serde_json::from_str::<Vec<String>>(&text)
        .map_err(|_| format!("memory store {} is malformed (expected a JSON array of strings); it was not modified", path.display()))
}

fn save(home: &Path, facts: &[String]) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(facts).map_err(|e| e.to_string())?;
    crate::session::write_private(&path(home), &bytes).map_err(|e| format!("could not save memory: {e}"))
}

/// Add `fact` unless it is already saved. Returns whether it was new.
pub fn add(home: &Path, fact: &str) -> Result<bool, String> {
    let fact = fact.trim();
    if fact.is_empty() {
        return Err("no fact provided".into());
    }
    if fact.chars().count() > MAX_FACT_CHARS {
        return Err(format!("a fact may be at most {MAX_FACT_CHARS} characters"));
    }
    let mut facts = load(home)?;
    if facts.iter().any(|f| f == fact) {
        return Ok(false);
    }
    facts.push(fact.to_string());
    save(home, &facts)?;
    Ok(true)
}

/// Remove the fact numbered `which` (1-based, as listed) or equal to it.
pub fn remove(home: &Path, which: &str) -> Result<String, String> {
    let mut facts = load(home)?;
    let which = which.trim();
    let index = match which.trim_start_matches('#').parse::<usize>() {
        Ok(n) if (1..=facts.len()).contains(&n) => n - 1,
        Ok(n) => return Err(format!("no memory #{n} (there are {})", facts.len())),
        Err(_) => facts.iter().position(|f| f == which).ok_or_else(|| format!("no saved memory matches \"{which}\""))?,
    };
    let removed = facts.remove(index);
    save(home, &facts)?;
    Ok(removed)
}

/// Numbered lines for `/memory` and the tool's `list`.
pub fn list_lines(facts: &[String]) -> Vec<String> {
    if facts.is_empty() {
        return vec!["No saved memories.".into()];
    }
    facts.iter().enumerate().map(|(i, f)| format!("{:>3}. {}", i + 1, one_line(f))).collect()
}

fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The system-prompt section for `facts` (empty when there are none).
pub fn prompt_section(facts: &[String]) -> String {
    if facts.is_empty() {
        return String::new();
    }
    let mut out = String::from(HEADER);
    let mut shown = 0;
    for fact in facts.iter().take(PROMPT_MAX_FACTS) {
        let line = format!("- {}\n", one_line(fact));
        if out.len() + line.len() > PROMPT_MAX_CHARS {
            break;
        }
        out.push_str(&line);
        shown += 1;
    }
    if shown < facts.len() {
        out.push_str(&format!("- ({} more; use the memory tool's list action to see them)\n", facts.len() - shown));
    }
    out.truncate(out.trim_end().len());
    out
}

/// Replace (or add, or drop) the memory section in an existing system prompt.
pub fn refresh_section(system: &mut String, home: &Path) {
    if let Some(start) = system.find(HEADER) {
        let body = start + HEADER.len();
        let end = system[body..].find("\n\n").map_or(system.len(), |i| body + i);
        system.replace_range(start..end, "");
    }
    let facts = load(home).unwrap_or_default();
    system.push_str(&prompt_section(&facts));
}

/// `/memory` and `/memory remove N`.
pub fn slash(home: &Path, args: &[&str], system: &mut String) {
    match args {
        [] | ["list"] => match load(home) {
            Ok(facts) => crate::ui::list(&list_lines(&facts)),
            Err(e) => crate::ui::error(&e),
        },
        ["remove" | "rm" | "forget", rest @ ..] if !rest.is_empty() => match remove(home, &rest.join(" ")) {
            Ok(fact) => {
                refresh_section(system, home);
                crate::ui::info(&format!("  Forgot: {}", one_line(&fact)));
            }
            Err(e) => crate::ui::error(&e),
        },
        _ => crate::ui::error("usage: /memory [list] | /memory remove N"),
    }
}

struct Memory;

#[async_trait::async_trait]
impl Tool for Memory {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "memory".into(),
            description: "Add, list, or remove durable facts and preferences that persist across nyq sessions \
                (saved facts are shown in the system prompt). Use it only when the user explicitly asks you to \
                remember or forget something. Never store secrets, temporary task notes, or things the user did not ask to keep."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["add", "list", "remove"], "description": "add a fact, list saved facts, or remove one."},
                    "fact": {"type": "string", "description": "For add: the fact, one short sentence. For remove: the fact's number from list, or its exact text."}
                },
                "required": ["action"]
            }),
        }
    }

    fn needs_approval(&self, args: &Value) -> bool {
        // Listing is read-only. Adding or removing a fact changes every later session.
        !matches!(args.get("action").and_then(Value::as_str), Some("list"))
    }

    fn summary(&self, args: &Value) -> String {
        let action = args.get("action").and_then(Value::as_str).unwrap_or("");
        match args.get("fact").and_then(Value::as_str) {
            Some(f) if action != "list" => format!("{action}: {}", one_line(f).chars().take(80).collect::<String>()),
            _ => action.to_string(),
        }
    }

    async fn run(&self, args: Value, _ctx: &ToolCtx) -> ToolOutput {
        let home = crate::config::home();
        let fact = args.get("fact").and_then(Value::as_str).unwrap_or("");
        let result = match args.get("action").and_then(Value::as_str).unwrap_or("") {
            "add" | "save" => add(&home, fact).map(|new| if new { "Remembered." } else { "Already remembered." }.to_string()),
            "list" => load(&home).map(|facts| list_lines(&facts).join("\n")),
            "remove" | "forget" if fact.trim().is_empty() => Err("remove needs `fact`: a number from list or the exact text".into()),
            "remove" | "forget" => remove(&home, fact).map(|f| format!("Forgot: {}", one_line(&f))),
            other => Err(format!("memory: unknown action \"{other}\" (use add, list or remove)")),
        };
        match result {
            Ok(s) => ToolOutput::ok(s),
            Err(e) => ToolOutput::err(e),
        }
    }
}

pub fn register(r: &mut Registry) {
    r.add(Box::new(Memory));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn facts_persist_and_are_removed() {
        let home = tempfile::tempdir().unwrap();
        let h = home.path();
        assert!(load(h).unwrap().is_empty());
        assert!(add(h, "  prefers tabs  ").unwrap());
        assert!(!add(h, "prefers tabs").unwrap(), "duplicates are not stored twice");
        assert!(add(h, "name is Ada").unwrap());
        assert!(add(h, "").is_err());
        assert_eq!(load(h).unwrap(), ["prefers tabs", "name is Ada"]);
        let raw: Value = serde_json::from_slice(&std::fs::read(path(h)).unwrap()).unwrap();
        assert_eq!(raw, json!(["prefers tabs", "name is Ada"]));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(path(h)).unwrap().permissions().mode() & 0o777, 0o600);
        }
        assert_eq!(list_lines(&load(h).unwrap()), ["  1. prefers tabs", "  2. name is Ada"]);

        assert_eq!(remove(h, "1").unwrap(), "prefers tabs");
        assert!(remove(h, "5").is_err());
        assert_eq!(remove(h, "name is Ada").unwrap(), "name is Ada");
        assert!(load(h).unwrap().is_empty());

        std::fs::write(path(h), "{not json").unwrap();
        assert!(add(h, "x").unwrap_err().contains("malformed"));
        assert_eq!(std::fs::read_to_string(path(h)).unwrap(), "{not json", "a malformed store is left alone");
    }

    #[test]
    fn prompt_section_is_capped_and_refreshable() {
        assert_eq!(prompt_section(&[]), "");
        let facts: Vec<String> = (0..100).map(|i| format!("fact {i}\nwith a newline")).collect();
        let section = prompt_section(&facts);
        assert!(section.starts_with("\n\n# Memory"));
        assert!(section.contains("- fact 0 with a newline"));
        assert!(section.len() <= PROMPT_MAX_CHARS + 100);
        assert!(section.contains("more; use the memory tool"));

        let home = tempfile::tempdir().unwrap();
        let mut system = "base prompt".to_string();
        refresh_section(&mut system, home.path());
        assert_eq!(system, "base prompt");
        add(home.path(), "likes short answers").unwrap();
        refresh_section(&mut system, home.path());
        assert!(system.ends_with("- likes short answers"), "{system}");
        system.push_str("\n\n# Skills\n\nlater section");
        add(home.path(), "uses fish").unwrap();
        refresh_section(&mut system, home.path());
        assert_eq!(system.matches("# Memory").count(), 1);
        assert!(system.contains("# Skills\n\nlater section"));
        assert!(system.ends_with("- likes short answers\n- uses fish"), "{system}");
        remove(home.path(), "1").unwrap();
        remove(home.path(), "1").unwrap();
        refresh_section(&mut system, home.path());
        assert!(!system.contains("# Memory"), "{system}");
        assert!(system.starts_with("base prompt\n\n# Skills"));
    }

    #[test]
    fn mutations_need_approval_and_listing_does_not() {
        let tool = Memory;
        assert!(!tool.needs_approval(&json!({"action": "list"})));
        for action in ["add", "save", "remove", "forget", "nope", ""] {
            assert!(tool.needs_approval(&json!({"action": action})), "{action}");
        }
        assert!(tool.needs_approval(&json!({})));
    }
}
