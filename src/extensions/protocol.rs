//! The extension manifest (`extension.json`) and the shapes exchanged over
//! JSON-RPC: what `initialize` registers, event names and tool results.
//! The wire format is documented in `docs/extensions.md`.

use std::collections::BTreeMap;
use std::time::Duration;

use serde_json::{Map, Value};

use crate::tools::ToolOutput;

/// The protocol version nyq speaks; sent in `initialize`.
pub const PROTOCOL: u64 = 1;
const DEFAULT_EVENT_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_EVENT_TIMEOUT_MS: u64 = 120_000;
const MAX_NAME: usize = 64;

/// A parsed `extension.json`.
#[derive(Clone, Debug, PartialEq)]
pub struct Manifest {
    pub name: String,
    pub version: String,
    pub description: String,
    /// Program and arguments, run in the extension's directory.
    pub command: Vec<String>,
    pub env: BTreeMap<String, String>,
    /// Run once after `nyq extensions install`, after confirmation.
    pub install: Option<Vec<String>>,
    /// How long an event may take before nyq goes on without it.
    pub event_timeout: Duration,
}

/// A name usable as a directory, a tool-name part and a slash command:
/// ASCII letters, digits, `-` and `_`, starting with a letter or digit.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME
        && name.starts_with(|c: char| c.is_ascii_alphanumeric())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        && !name.contains("__")
}

/// A command given as a list of words or one string split on whitespace.
fn words(v: &Value, key: &str) -> Result<Option<Vec<String>>, String> {
    let list = match v {
        Value::Null => return Ok(None),
        Value::String(s) => s.split_whitespace().map(String::from).collect(),
        Value::Array(a) => a
            .iter()
            .map(|w| {
                w.as_str()
                    .map(String::from)
                    .ok_or_else(|| format!("\"{key}\" must hold strings, got {w}"))
            })
            .collect::<Result<Vec<_>, _>>()?,
        other => return Err(format!("\"{key}\" must be a list of strings, got {other}")),
    };
    if list.is_empty() || list[0].trim().is_empty() {
        return Err(format!("\"{key}\" is empty"));
    }
    Ok(Some(list))
}

impl Manifest {
    pub fn parse(text: &str) -> Result<Self, String> {
        let v: Value = serde_json::from_str(text).map_err(|e| format!("not valid JSON: {e}"))?;
        let Value::Object(map) = v else {
            return Err("expected a JSON object".into());
        };
        let text_of = |key: &str| {
            map.get(key)
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string()
        };
        let name = text_of("name");
        if !valid_name(&name) {
            return Err(format!(
                "\"name\" must be letters, digits, - or _ (got {})",
                map.get("name").unwrap_or(&Value::Null)
            ));
        }
        let command = words(map.get("command").unwrap_or(&Value::Null), "command")?
            .ok_or("\"command\" is required, e.g. [\"python3\", \"ext.py\"]")?;
        let install = words(map.get("install").unwrap_or(&Value::Null), "install")?;
        let env = match map.get("env") {
            None | Some(Value::Null) => BTreeMap::new(),
            Some(Value::Object(m)) => m
                .iter()
                .map(|(k, v)| match v {
                    Value::String(s) => Ok((k.clone(), s.clone())),
                    other => Err(format!("\"env.{k}\" must be a string, got {other}")),
                })
                .collect::<Result<_, _>>()?,
            Some(other) => return Err(format!("\"env\" must be an object, got {other}")),
        };
        let event_timeout = match map.get("event_timeout_ms") {
            None | Some(Value::Null) => DEFAULT_EVENT_TIMEOUT,
            Some(v) => match v.as_u64().filter(|n| *n > 0) {
                Some(ms) => Duration::from_millis(ms.min(MAX_EVENT_TIMEOUT_MS)),
                None => {
                    return Err(format!(
                        "\"event_timeout_ms\" must be a positive integer, got {v}"
                    ))
                }
            },
        };
        Ok(Manifest {
            name,
            version: text_of("version"),
            description: text_of("description"),
            command,
            env,
            install,
            event_timeout,
        })
    }
}

/// Conversation events an extension may subscribe to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Event {
    SessionStart,
    UserPrompt,
    ToolCall,
    ToolResult,
    TurnEnd,
    BeforeCompact,
    SessionEnd,
}

impl Event {
    pub const ALL: [Event; 7] = [
        Event::SessionStart,
        Event::UserPrompt,
        Event::ToolCall,
        Event::ToolResult,
        Event::TurnEnd,
        Event::BeforeCompact,
        Event::SessionEnd,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Event::SessionStart => "session_start",
            Event::UserPrompt => "user_prompt",
            Event::ToolCall => "tool_call",
            Event::ToolResult => "tool_result",
            Event::TurnEnd => "turn_end",
            Event::BeforeCompact => "before_compact",
            Event::SessionEnd => "session_end",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|e| e.as_str() == s)
    }

    /// The JSON-RPC method nyq calls for this event.
    pub fn method(self) -> String {
        format!("event/{}", self.as_str())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub read_only: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CommandDef {
    pub name: String,
    pub description: String,
    pub args_hint: String,
}

/// What an extension registered in its `initialize` result.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Registration {
    pub tools: Vec<ToolDef>,
    pub commands: Vec<CommandDef>,
    pub events: Vec<Event>,
    /// The initial status-line text.
    pub status: Option<String>,
}

/// Parse an `initialize` result. Bad entries are skipped with a warning.
pub fn parse_registration(v: &Value) -> (Registration, Vec<String>) {
    let mut reg = Registration::default();
    let mut warnings = Vec::new();
    let list = |key: &str, warnings: &mut Vec<String>| -> Vec<Value> {
        match v.get(key) {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(a)) => a.clone(),
            Some(other) => {
                warnings.push(format!("\"{key}\" must be a list, got {other}"));
                Vec::new()
            }
        }
    };
    let str_of = |o: &Map<String, Value>, k: &str| {
        o.get(k)
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string()
    };
    for (i, t) in list("tools", &mut warnings).iter().enumerate() {
        let Some(o) = t.as_object() else {
            warnings.push(format!("tools[{i}]: expected an object"));
            continue;
        };
        let name = str_of(o, "name");
        if !valid_name(&name) {
            warnings.push(format!("tools[{i}]: invalid name `{name}`"));
            continue;
        }
        if reg.tools.iter().any(|d| d.name == name) {
            warnings.push(format!("tools[{i}]: duplicate tool `{name}`"));
            continue;
        }
        let input_schema = match o.get("input_schema") {
            None | Some(Value::Null) => serde_json::json!({"type": "object", "properties": {}}),
            Some(s @ Value::Object(_)) => s.clone(),
            Some(other) => {
                warnings.push(format!(
                    "tools[{i}]: \"input_schema\" must be an object, got {other}"
                ));
                continue;
            }
        };
        reg.tools.push(ToolDef {
            name,
            description: str_of(o, "description"),
            input_schema,
            read_only: o.get("read_only").and_then(Value::as_bool).unwrap_or(false),
        });
    }
    for (i, c) in list("commands", &mut warnings).iter().enumerate() {
        let Some(o) = c.as_object() else {
            warnings.push(format!("commands[{i}]: expected an object"));
            continue;
        };
        let name = str_of(o, "name");
        let name = name.trim_start_matches('/').to_string();
        if !valid_name(&name) {
            warnings.push(format!("commands[{i}]: invalid name `{name}`"));
            continue;
        }
        if reg.commands.iter().any(|d| d.name == name) {
            warnings.push(format!("commands[{i}]: duplicate command `/{name}`"));
            continue;
        }
        reg.commands.push(CommandDef {
            name,
            description: str_of(o, "description"),
            args_hint: str_of(o, "args_hint"),
        });
    }
    for e in list("events", &mut warnings) {
        match e.as_str().and_then(Event::parse) {
            Some(ev) if !reg.events.contains(&ev) => reg.events.push(ev),
            Some(_) => {}
            None => warnings.push(format!("unknown event {e}")),
        }
    }
    reg.status = v
        .get("status")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from);
    (reg, warnings)
}

/// A `tool/call` result: `{content: [{type: "text", text} | {type: "image",
/// data, mime_type}], is_error}`. A bare string is text.
pub fn tool_output(v: &Value) -> ToolOutput {
    let mut out = ToolOutput::default();
    let mut texts = Vec::new();
    match v.get("content").unwrap_or(v) {
        Value::String(s) => texts.push(s.clone()),
        Value::Array(blocks) => {
            for b in blocks {
                match b.get("type").and_then(Value::as_str) {
                    Some("image") => {
                        let data = b.get("data").and_then(Value::as_str).unwrap_or("");
                        let media = b
                            .get("mime_type")
                            .or_else(|| b.get("mimeType"))
                            .and_then(Value::as_str)
                            .unwrap_or("image/png");
                        if !data.is_empty() {
                            out.images.push(crate::types::Image {
                                media_type: media.to_string(),
                                data: data.to_string(),
                            });
                        }
                    }
                    _ => texts.extend(b.get("text").and_then(Value::as_str).map(String::from)),
                }
            }
        }
        Value::Null => {}
        other => texts.push(other.to_string()),
    }
    out.content = texts.join("\n");
    if out.content.is_empty() && !out.images.is_empty() {
        out.content = format!("[{} image(s)]", out.images.len());
    }
    out.is_error = v.get("is_error").and_then(Value::as_bool).unwrap_or(false);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn manifests_parse_and_validate() {
        let m = Manifest::parse(
            r#"{"name":"words","version":"1.0","description":"count","command":["python3","ext.py"],
                "env":{"A":"1"},"install":"npm install","event_timeout_ms":250}"#,
        )
        .unwrap();
        assert_eq!(m.command, ["python3", "ext.py"]);
        assert_eq!(
            m.install.as_deref(),
            Some(&["npm".to_string(), "install".into()][..])
        );
        assert_eq!(m.env["A"], "1");
        assert_eq!(m.event_timeout, Duration::from_millis(250));
        assert_eq!(
            Manifest::parse(r#"{"name":"x","command":"./bin"}"#)
                .unwrap()
                .event_timeout,
            DEFAULT_EVENT_TIMEOUT
        );
        for bad in [
            r#"{"name":"../x","command":["a"]}"#,
            r#"{"name":"a__b","command":["a"]}"#,
            r#"{"name":"x"}"#,
            r#"{"name":"x","command":[]}"#,
            r#"{"name":"x","command":["a"],"env":{"A":1}}"#,
            r#"{"name":"x","command":["a"],"event_timeout_ms":0}"#,
            "[]",
        ] {
            assert!(Manifest::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn registrations_skip_bad_entries() {
        let (reg, warnings) = parse_registration(&json!({
            "tools": [
                {"name": "word_count", "description": "Count", "input_schema": {"type": "object"}, "read_only": true},
                {"name": "bad name"},
                {"name": "word_count"},
                {"name": "plain"}
            ],
            "commands": [{"name": "/hello", "description": "Greet", "args_hint": "[name]"}, 3],
            "events": ["tool_call", "nope", "tool_call", "user_prompt"],
            "status": " ready "
        }));
        assert_eq!(reg.tools.len(), 2);
        assert!(reg.tools[0].read_only && !reg.tools[1].read_only);
        assert_eq!(reg.tools[1].input_schema["type"], "object");
        assert_eq!(reg.commands[0].name, "hello");
        assert_eq!(reg.events, [Event::ToolCall, Event::UserPrompt]);
        assert_eq!(reg.status.as_deref(), Some("ready"));
        assert_eq!(warnings.len(), 4, "{warnings:?}");
        assert_eq!(parse_registration(&json!({})).0, Registration::default());
    }

    #[test]
    fn tool_results_become_outputs() {
        let out = tool_output(
            &json!({"content": [{"type": "text", "text": "a"}, {"type": "text", "text": "b"},
            {"type": "image", "data": "AAAA", "mime_type": "image/jpeg"}], "is_error": true}),
        );
        assert_eq!(out.content, "a\nb");
        assert!(out.is_error);
        assert_eq!(out.images[0].media_type, "image/jpeg");
        assert_eq!(tool_output(&json!({"content": "plain"})).content, "plain");
        assert_eq!(tool_output(&json!("bare")).content, "bare");
    }
}
