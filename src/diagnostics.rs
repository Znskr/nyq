//! `/feedback` and `/trace`: the issue page, and a redacted diagnostic
//! bundle (version, platform, settings, recent session events) to attach to
//! a report.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::{json, Value};

use crate::types::Item;

pub const FEEDBACK_URL: &str = "https://github.com/sharqlabs/nyq/issues/new";
/// Session items included in a trace.
const RECENT_ITEMS: usize = 50;
/// Characters of text kept per item.
const EXCERPT: usize = 240;

/// Replace things that look like credentials with `[redacted]`: API keys,
/// bearer tokens, JWTs, `key=value` secrets and long opaque strings.
pub fn redact(text: &str) -> String {
    use std::sync::OnceLock;
    static PATTERNS: OnceLock<Vec<regex::Regex>> = OnceLock::new();
    let patterns = PATTERNS.get_or_init(|| {
        [
            r"(?i)\b(bearer|basic)\s+[A-Za-z0-9._~+/=-]{8,}",
            r"\beyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]*",
            r"\b(sk|pk|rk)-[A-Za-z0-9_-]{12,}",
            r"\b(ghp|gho|ghu|ghs|ghr|github_pat|glpat|xox[abprs])[-_][A-Za-z0-9_-]{10,}",
            r"\bAKIA[0-9A-Z]{16}\b",
            r#"(?i)\b([a-z0-9_]*(api[_-]?key|token|secret|password|passwd|auth)[a-z0-9_]*)\s*[:=]\s*["']?[^\s"',;]{4,}"#,
            r"\b[A-Za-z0-9+/_-]{40,}={0,2}",
        ]
        .iter()
        .map(|p| regex::Regex::new(p).expect("valid redaction pattern"))
        .collect()
    });
    let mut out = text.to_string();
    for (i, re) in patterns.iter().enumerate() {
        out = if i == 5 {
            re.replace_all(&out, "$1=[redacted]").into_owned()
        } else {
            re.replace_all(&out, "[redacted]").into_owned()
        };
    }
    out
}

fn excerpt(text: &str) -> Value {
    let short: String = text.chars().take(EXCERPT).collect();
    let more = text.chars().count().saturating_sub(EXCERPT);
    json!({ "chars": text.chars().count(), "excerpt": redact(&short), "truncated": more > 0 })
}

/// One session item, reduced to what helps debugging.
fn item_event(item: &Item) -> Value {
    match item {
        Item::User { text, images } => json!({ "type": "user", "text": excerpt(text), "images": images.len() }),
        Item::Assistant { text } => json!({ "type": "assistant", "text": excerpt(text) }),
        Item::Reasoning { summary, encrypted, .. } => json!({ "type": "reasoning", "summary_chars": summary.chars().count(), "encrypted": encrypted.is_some() }),
        Item::ToolCall { name, arguments, .. } => json!({ "type": "tool_call", "name": name, "arguments": excerpt(arguments) }),
        Item::ToolResult { output, is_error, .. } => json!({ "type": "tool_result", "is_error": is_error, "output": excerpt(output) }),
        Item::Compaction { summary } => json!({ "type": "compaction", "summary_chars": summary.chars().count() }),
    }
}

/// Settings with secret-looking values removed (e.g. an `api_key` someone
/// put in settings.json).
fn scrub(v: &Value) -> Value {
    match v {
        Value::Object(m) => Value::Object(
            m.iter()
                .map(|(k, v)| {
                    let secret = ["key", "token", "secret", "password", "auth"].iter().any(|s| k.to_ascii_lowercase().contains(s));
                    (k.clone(), if secret { json!("[redacted]") } else { scrub(v) })
                })
                .collect(),
        ),
        Value::Array(a) => Value::Array(a.iter().map(scrub).collect()),
        Value::String(s) => Value::String(redact(s)),
        other => other.clone(),
    }
}

/// What the trace describes besides the session items.
pub struct TraceInfo<'a> {
    pub provider: &'a str,
    pub model: &'a str,
    pub effort: Option<&'a str>,
    pub fast: bool,
    pub sandbox: &'a str,
    pub yolo: bool,
    pub session_id: Option<&'a str>,
    pub context_tokens: u64,
    pub context_window: u64,
    pub usage: crate::types::Usage,
}

pub fn trace(info: &TraceInfo, items: &[Item]) -> Value {
    let settings = serde_json::to_value(crate::settings::current()).unwrap_or(Value::Null);
    let skip = items.len().saturating_sub(RECENT_ITEMS);
    json!({
        "nyq": env!("CARGO_PKG_VERSION"),
        "created": chrono::Utc::now().to_rfc3339(),
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "terminal": { "TERM": std::env::var("TERM").ok(), "TERM_PROGRAM": std::env::var("TERM_PROGRAM").ok(), "tmux": std::env::var_os("TMUX").is_some() },
        "provider": info.provider,
        "model": info.model,
        "effort": info.effort,
        "fast": info.fast,
        "sandbox": info.sandbox,
        "yolo": info.yolo,
        "session": info.session_id,
        "context": { "tokens": info.context_tokens, "window": info.context_window },
        "usage": info.usage,
        "settings": scrub(&settings),
        "items_total": items.len(),
        "recent_events": items[skip..].iter().map(item_event).collect::<Vec<_>>(),
    })
}

/// Write a trace to `<home>/traces/trace-<time>.json` (0600) and return its path.
pub fn write_trace(home: &Path, trace: &Value) -> Result<PathBuf> {
    let name = format!("trace-{}.json", chrono::Local::now().format("%Y%m%d-%H%M%S"));
    let path = home.join("traces").join(name);
    crate::session::write_private(&path, &serde_json::to_vec_pretty(trace)?).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_secrets() {
        let text = "export OPENAI_API_KEY=sk-proj-abcdefghijklmnop1234 and Authorization: Bearer abc.def.ghijklmnop \
                    token: hunter2xyz ghp_abcdefghijklmnopqrstuvwx eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.sig AKIAABCDEFGHIJKLMNOP";
        let out = redact(text);
        for secret in ["sk-proj-abcdefghijklmnop1234", "abc.def.ghijklmnop", "hunter2xyz", "ghp_abcdefghijklmnopqrstuvwx", "eyJhbGciOiJIUzI1NiJ9", "AKIAABCDEFGHIJKLMNOP"] {
            assert!(!out.contains(secret), "{secret} leaked: {out}");
        }
        assert!(out.contains("OPENAI_API_KEY=[redacted]"), "{out}");
        assert_eq!(redact("cargo test --release in src/main.rs"), "cargo test --release in src/main.rs");
    }

    #[test]
    fn trace_bundle_is_redacted_and_private() {
        let items = vec![
            Item::user("my key is sk-abcdefghijklmnopqrstuv"),
            Item::ToolCall { id: "1".into(), name: "bash".into(), arguments: r#"{"command":"curl -H 'Authorization: Bearer abcdefghijklmnop' x"}"#.into() },
            Item::ToolResult { id: "1".into(), output: "x".repeat(1000), is_error: false },
        ];
        let info = TraceInfo {
            provider: "codex",
            model: "m",
            effort: Some("high"),
            fast: false,
            sandbox: "sandboxed",
            yolo: false,
            session_id: Some("abc"),
            context_tokens: 1,
            context_window: 2,
            usage: Default::default(),
        };
        let t = trace(&info, &items);
        let text = t.to_string();
        assert!(!text.contains("sk-abcdefghijklmnopqrstuv") && !text.contains("abcdefghijklmnop'"), "{text}");
        assert_eq!(t["recent_events"].as_array().unwrap().len(), 3);
        assert_eq!(t["recent_events"][2]["output"]["truncated"], true);
        assert_eq!(scrub(&json!({"api_key": "x", "nested": {"Token": 1}, "ok": "fine"})), json!({"api_key": "[redacted]", "nested": {"Token": "[redacted]"}, "ok": "fine"}));
        let dir = tempfile::tempdir().unwrap();
        let path = write_trace(dir.path(), &t).unwrap();
        assert!(path.starts_with(dir.path().join("traces")));
        let back: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(back["model"], "m");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
    }
}
