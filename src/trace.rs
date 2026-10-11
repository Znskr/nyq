//! Live debug tracing for diagnosing provider, tool, MCP and ACP issues.
//!
//! `NYQ_TRACE=1` (or `all`) traces every scope; `NYQ_TRACE=provider,mcp`
//! only those. Lines go to stderr, or are appended to `NYQ_TRACE_LOG` when
//! it names a file. `nyq acp --log-file PATH` turns tracing on into a file.
//! Credentials never enter the trace: callers log names, sizes and errors.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

struct Tracer {
    /// None traces every scope.
    scopes: Option<Vec<String>>,
    file: Option<(PathBuf, std::fs::File)>,
}

static TRACER: Mutex<Option<Tracer>> = Mutex::new(None);

/// Parse `NYQ_TRACE`: None when off, Some(None) for every scope.
fn parse_scopes(value: &str) -> Option<Option<Vec<String>>> {
    let value = value.trim();
    match value.to_ascii_lowercase().as_str() {
        "" | "0" | "off" | "false" | "no" => None,
        "1" | "on" | "true" | "yes" | "all" | "*" => Some(None),
        list => Some(Some(
            list.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
        )),
    }
}

/// Start tracing from `NYQ_TRACE` / `NYQ_TRACE_LOG`. Problems opening the
/// log file are reported once on stderr.
pub fn init_from_env() {
    let Some(scopes) = std::env::var("NYQ_TRACE").ok().and_then(|v| parse_scopes(&v)) else {
        return;
    };
    let log = std::env::var_os("NYQ_TRACE_LOG").filter(|v| !v.is_empty());
    match log.map(PathBuf::from) {
        Some(path) => {
            if let Err(e) = to_file(&path, scopes) {
                eprintln!("nyq: NYQ_TRACE_LOG {}: {e}", path.display());
            }
        }
        None => install(Tracer { scopes, file: None }),
    }
}

/// Append the trace (for `scopes`, None = all) to `path`.
pub fn to_file(path: &Path, scopes: Option<Vec<String>>) -> std::io::Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let file = opts.open(path)?;
    install(Tracer { scopes, file: Some((path.to_path_buf(), file)) });
    Ok(())
}

fn install(t: Tracer) {
    *TRACER.lock().unwrap_or_else(|e| e.into_inner()) = Some(t);
}

/// Where the trace goes, for `/status` and `nyq status`; None when off.
pub fn describe() -> Option<String> {
    let guard = TRACER.lock().unwrap_or_else(|e| e.into_inner());
    let t = guard.as_ref()?;
    let to = t
        .file
        .as_ref()
        .map_or("stderr".to_string(), |(p, _)| p.display().to_string());
    let scopes = t.scopes.as_ref().map_or("all".to_string(), |s| s.join(","));
    Some(format!("{scopes} → {to}"))
}

/// Write one line for `scope` when that scope is traced. `msg` is only
/// built when it will be written.
pub fn log(scope: &str, msg: impl FnOnce() -> String) {
    let mut guard = TRACER.lock().unwrap_or_else(|e| e.into_inner());
    let Some(t) = guard.as_mut() else { return };
    if t.scopes.as_ref().is_some_and(|s| !s.iter().any(|x| x == scope)) {
        return;
    }
    let line = format!(
        "{} [{scope}] {}\n",
        chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        msg().replace('\n', "\\n")
    );
    let _ = match &mut t.file {
        Some((_, f)) => f.write_all(line.as_bytes()),
        None => std::io::stderr().write_all(line.as_bytes()),
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scopes_parse() {
        assert_eq!(parse_scopes("0"), None);
        assert_eq!(parse_scopes(""), None);
        assert_eq!(parse_scopes("1"), Some(None));
        assert_eq!(parse_scopes("all"), Some(None));
        assert_eq!(
            parse_scopes("provider, mcp"),
            Some(Some(vec!["provider".to_string(), "mcp".into()]))
        );
    }

    #[test]
    fn file_trace_filters_scopes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("logs/trace.log");
        to_file(&path, Some(vec!["mcp".into()])).unwrap();
        log("mcp", || "connected\nok".into());
        log("provider", || "hidden".into());
        assert!(describe().unwrap().starts_with("mcp → "));
        *TRACER.lock().unwrap() = None;
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("[mcp] connected\\nok"), "{text}");
        assert!(!text.contains("hidden"));
    }
}
