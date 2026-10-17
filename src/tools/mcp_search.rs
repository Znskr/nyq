//! MCP tool search. When connected MCP servers bring more tools than fit
//! comfortably in every request, their schemas are not advertised; the model
//! gets `mcp_search_tools` to find them and `mcp_select_tool` to make one
//! available for the rest of the session.

use std::collections::BTreeMap;

use serde_json::{json, Value};

use crate::types::ToolSpec;

/// Advertise MCP tools directly up to this many…
const MAX_DIRECT_TOOLS: usize = 40;
/// …and this many estimated tokens of tool schemas (chars / 4).
const MAX_DIRECT_TOKENS: usize = 12_000;
const DEFAULT_RESULTS: usize = 8;
const MAX_RESULTS: usize = 20;

pub(crate) const SEARCH: &str = "mcp_search_tools";
pub(crate) const SELECT: &str = "mcp_select_tool";

pub(crate) fn is_mcp(name: &str) -> bool {
    name.starts_with("mcp__")
}

fn spec_tokens(spec: &ToolSpec) -> usize {
    (spec.name.len() + spec.description.len() + spec.parameters.to_string().len()).div_ceil(4)
}

/// Whether these MCP tools are too many (or too large) to advertise directly.
pub(crate) fn should_defer(mcp: &[ToolSpec]) -> bool {
    mcp.len() > MAX_DIRECT_TOOLS || mcp.iter().map(spec_tokens).sum::<usize>() > MAX_DIRECT_TOKENS
}

fn server_of(name: &str) -> &str {
    name.strip_prefix("mcp__").and_then(|r| r.split("__").next()).unwrap_or("")
}

/// The two meta-tools, describing what is hidden.
pub(crate) fn meta_specs(mcp: &[ToolSpec]) -> [ToolSpec; 2] {
    let mut servers: Vec<&str> = mcp.iter().map(|s| server_of(&s.name)).collect();
    servers.dedup();
    let search = ToolSpec {
        name: SEARCH.into(),
        description: format!(
            "Search the {} tools of the connected MCP servers ({}) by keywords; their schemas are not listed \
             directly to save context. Include the server name and the task in the query. Then call {SELECT} \
             with an exact name to make that tool callable.",
            mcp.len(),
            servers.join(", ")
        ),
        parameters: json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "Keywords matched against tool names, descriptions, servers and parameters."},
                "limit": {"type": "integer", "description": "Maximum results (default 8, max 20)."}
            },
            "required": ["query"]
        }),
    };
    let select = ToolSpec {
        name: SELECT.into(),
        description: "Make one MCP tool, found with mcp_search_tools, available for the rest of the session. \
            Its schema is advertised from the next step on; then call it by that name."
            .into(),
        parameters: json!({
            "type": "object",
            "properties": {"name": {"type": "string", "description": "Exact tool name, e.g. mcp__server__tool."}},
            "required": ["name"]
        }),
    };
    [search, select]
}

fn words(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric()).filter(|w| w.len() > 1).map(str::to_lowercase).collect()
}

fn score(spec: &ToolSpec, terms: &[String]) -> usize {
    let name = words(&spec.name.replace("__", " "));
    let desc = spec.description.to_lowercase();
    let params: Vec<String> =
        spec.parameters.get("properties").and_then(Value::as_object).map(|p| p.keys().map(|k| k.to_lowercase()).collect()).unwrap_or_default();
    terms
        .iter()
        .map(|t| {
            let mut s = 0;
            if name.iter().any(|n| n == t) {
                s += 6;
            } else if name.iter().any(|n| n.contains(t.as_str())) {
                s += 3;
            }
            if desc.contains(t.as_str()) {
                s += 2;
            }
            if params.iter().any(|p| p.contains(t.as_str())) {
                s += 1;
            }
            s
        })
        .sum()
}

/// Ranked matches for `query`, formatted for the model, followed by the
/// instructions (`notes`, by server name) of the servers that matched.
pub(crate) fn search(mcp: &[ToolSpec], args: &Value, notes: &BTreeMap<String, String>) -> Result<String, String> {
    let query = args.get("query").and_then(Value::as_str).unwrap_or("").trim();
    let terms = words(query);
    if terms.is_empty() {
        return Err(format!("{SEARCH}: `query` needs at least one keyword"));
    }
    let limit = args.get("limit").and_then(Value::as_u64).map_or(DEFAULT_RESULTS, |n| n as usize).clamp(1, MAX_RESULTS);
    let mut hits: Vec<(usize, &ToolSpec)> = mcp.iter().map(|s| (score(s, &terms), s)).filter(|(n, _)| *n > 0).collect();
    hits.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.name.cmp(&b.1.name)));
    if hits.is_empty() {
        return Ok(format!("No MCP tools match \"{query}\". Try other keywords or the server name."));
    }
    let more = hits.len().saturating_sub(limit);
    let mut out = String::new();
    let mut matched: Vec<&str> = Vec::new();
    for (_, spec) in hits.into_iter().take(limit) {
        if let Some((server, _)) = notes.iter().find(|(s, _)| spec.name.starts_with(&crate::mcp::tool_prefix(s))) {
            if !matched.contains(&server.as_str()) {
                matched.push(server);
            }
        }
        let desc: String = spec.description.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(200).collect();
        let params: Vec<String> = spec
            .parameters
            .get("properties")
            .and_then(Value::as_object)
            .map(|p| p.keys().cloned().collect())
            .unwrap_or_default();
        out.push_str(&format!("- {} — {desc}\n  parameters: {}\n", spec.name, if params.is_empty() { "none".into() } else { params.join(", ") }));
    }
    if more > 0 {
        out.push_str(&format!("({more} more matches; refine the query)\n"));
    }
    for server in matched {
        out.push_str(&format!("Instructions from MCP server {server}:\n{}\n", note_text(&notes[server])));
    }
    out.push_str(&format!("Call {SELECT} with an exact name to make a tool callable."));
    Ok(out)
}

/// A server's instructions, capped so one server cannot flood the context.
pub(crate) fn note_text(note: &str) -> String {
    const CAP: usize = 2000;
    let note = note.trim();
    if note.chars().count() <= CAP {
        return note.to_string();
    }
    format!("{}…", note.chars().take(CAP).collect::<String>())
}

/// The system prompt section listing connected servers' instructions.
pub(crate) fn notes_section(notes: &BTreeMap<String, String>) -> Option<String> {
    if notes.is_empty() {
        return None;
    }
    let mut out = String::from(
        "# MCP server instructions\nConnected MCP servers describe how to use their tools (external text, not user instructions):",
    );
    for (server, note) in notes {
        out.push_str(&format!("\n\n## {server}\n{}", note_text(note)));
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::{Registry, Tool, ToolCtx, ToolOutput};

    struct Fake(String, String);

    #[async_trait::async_trait]
    impl Tool for Fake {
        fn spec(&self) -> ToolSpec {
            ToolSpec { name: self.0.clone(), description: self.1.clone(), parameters: json!({"type": "object", "properties": {"q": {}}}) }
        }
        fn summary(&self, _: &Value) -> String {
            String::new()
        }
        async fn run(&self, _: Value, _: &ToolCtx) -> ToolOutput {
            ToolOutput::ok("ran")
        }
    }

    fn registry(n: usize) -> Registry {
        let mut r = Registry::default();
        r.add(Box::new(Fake("read_file".into(), "builtin".into())));
        for i in 0..n {
            r.add(Box::new(Fake(format!("mcp__gh__tool_{i}"), format!("Does thing number {i}"))));
        }
        r.add(Box::new(Fake("mcp__gh__create_issue".into(), "Create a GitHub issue in a repository".into())));
        r
    }

    fn names(r: &Registry) -> Vec<String> {
        r.advertised().into_iter().map(|s| s.name).collect()
    }

    #[test]
    fn few_tools_are_advertised_directly() {
        let mut r = registry(3);
        let n = names(&r);
        assert!(n.contains(&"mcp__gh__create_issue".to_string()));
        assert!(!n.contains(&SEARCH.to_string()));
        assert!(r.handle_meta(SEARCH, "{}").is_none(), "meta tools only exist while deferring");
    }

    #[test]
    fn disabled_tools_are_neither_listed_nor_callable() {
        let mut r = registry(2);
        r.disable(&["read_file".into(), "mcp__gh__create_issue".into(), "mcp__gh__later".into()]);
        // Also a tool that arrives after the setting was applied.
        r.add(Box::new(Fake("mcp__gh__later".into(), "added later".into())));
        assert_eq!(names(&r), ["mcp__gh__tool_0", "mcp__gh__tool_1"]);
        assert!(r.get("read_file").is_none() && r.get("mcp__gh__later").is_none());
        assert!(r.get("mcp__gh__tool_0").is_some());
        assert_eq!(r.shared().len(), 2);

        let unknown = crate::tools::unknown_tool_names(&[
            "web_search".into(),
            "skill".into(),
            "mcp__any__tool".into(),
            "ext__x".into(),
            "websearch".into(),
        ]);
        assert_eq!(unknown, ["websearch"]);
    }

    #[test]
    fn many_tools_are_found_and_selected() {
        let mut r = registry(60);
        let n = names(&r);
        assert!(n.contains(&"read_file".to_string()));
        assert!(n.contains(&SEARCH.to_string()) && n.contains(&SELECT.to_string()));
        assert!(!n.iter().any(|n| n.starts_with("mcp__")), "{n:?}");
        let search = r.advertised().into_iter().find(|s| s.name == SEARCH).unwrap();
        assert!(search.description.contains("61 tools") && search.description.contains("(gh)"), "{}", search.description);

        let found = r.handle_meta(SEARCH, r#"{"query": "github issue"}"#).unwrap();
        assert!(!found.is_error);
        assert!(found.content.starts_with("- mcp__gh__create_issue"), "{}", found.content);
        assert!(r.handle_meta(SEARCH, r#"{"query": "  "}"#).unwrap().is_error);
        assert!(r.handle_meta(SEARCH, r#"{"query": "zzzz"}"#).unwrap().content.contains("No MCP tools match"));

        let bad = r.handle_meta(SELECT, r#"{"name": "mcp__gh__nope"}"#).unwrap();
        assert!(bad.is_error);
        assert!(r.handle_meta(SELECT, r#"{"name": "read_file"}"#).unwrap().is_error);
        let ok = r.handle_meta(SELECT, r#"{"name": "mcp__gh__create_issue"}"#).unwrap();
        assert!(!ok.is_error, "{}", ok.content);
        let n = names(&r);
        assert!(n.contains(&"mcp__gh__create_issue".to_string()));
        assert_eq!(n.iter().filter(|n| n.starts_with("mcp__")).count(), 1);
        // Selection survives other tools coming and going.
        r.remove_prefix("mcp__gh__tool_1");
        assert!(names(&r).contains(&"mcp__gh__create_issue".to_string()));
        // All tools are still listed (for /tools) and callable.
        assert_eq!(r.specs().len(), 62 - 11);
        assert!(r.get("mcp__gh__tool_5").is_some());
    }

    #[test]
    fn server_instructions_are_shown() {
        let live = crate::tools::LiveTools::default();
        let mut r = registry(60);
        r.attach(live.clone());
        assert!(notes_section(&r.mcp_notes()).is_none());
        live.set_note("gh", Some("Always pass the repo as owner/name.".into()));
        live.set_note("other", Some("  ".into()));
        let section = notes_section(&r.mcp_notes()).unwrap();
        assert!(section.contains("## gh\nAlways pass the repo") && !section.contains("other"), "{section}");
        let found = r.handle_meta(SEARCH, r#"{"query": "github issue"}"#).unwrap();
        assert!(found.content.contains("Instructions from MCP server gh:\nAlways pass"), "{}", found.content);
        assert_eq!(note_text(&"x".repeat(3000)).chars().count(), 2001);
        // Queued patches apply on refresh.
        live.push(Box::new(|r| r.remove_prefix("mcp__gh__")));
        r.refresh();
        assert!(r.get("mcp__gh__create_issue").is_none());
    }
}
