use super::io;
use std::{
    fmt,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex, OnceLock,
    },
};

const DEFAULT_LOG_MAX_BYTES: u64 = 2 * 1024 * 1024;
const MAX_TRACE_LINE_BYTES: usize = 64 * 1024;
#[derive(Clone, Debug, Default)]
struct State {
    configured: bool,
    enabled: bool,
    stderr_enabled: bool,
    scope_filter: Option<String>,
    file_path: Option<PathBuf>,
}
fn state() -> &'static Mutex<State> {
    static STATE: OnceLock<Mutex<State>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(State::default()))
}
static NEXT_TURN_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_STEP_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_SUBAGENT_ID: AtomicU64 = AtomicU64::new(1);
#[derive(Clone, Debug, Default)]
pub struct Options {
    pub file_path: Option<PathBuf>,
    pub stderr_enabled: bool,
    pub scope_filter: Option<String>,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TraceContext {
    pub turn_id: u64,
    pub step_id: u64,
    pub subagent_id: u64,
}
pub fn configure_from_env(workspace_root: impl AsRef<Path>) {
    let log = io::getenv(b"FX_TRACE_LOG");
    let enabled = truthy(io::getenv(b"FX_TRACE").as_deref());
    let stderr = truthy(io::getenv(b"FX_TRACE_STDERR").as_deref());
    let scopes = io::getenv(b"FX_TRACE_SCOPES").map(|v| String::from_utf8_lossy(&v).into_owned());
    let path = log
        .and_then(|v| {
            let s = String::from_utf8_lossy(&v);
            resolve_log_path(workspace_root.as_ref(), &s).ok()
        })
        .or_else(|| enabled.then(default_log_path));
    if path.is_some() || stderr {
        let _ = configure(Options {
            file_path: path,
            stderr_enabled: stderr,
            scope_filter: scopes,
        });
    }
}
pub fn shutdown() {
    if let Ok(mut s) = state().lock() {
        *s = State::default()
    }
}
pub fn active_log_path() -> Option<PathBuf> {
    state().lock().ok()?.file_path.clone()
}
pub fn next_turn_id() -> u64 {
    NEXT_TURN_ID.fetch_add(1, Ordering::SeqCst)
}
pub fn next_step_id() -> u64 {
    NEXT_STEP_ID.fetch_add(1, Ordering::SeqCst)
}
pub fn next_subagent_id() -> u64 {
    NEXT_SUBAGENT_ID.fetch_add(1, Ordering::SeqCst)
}
pub fn logf(scope: &str, args: fmt::Arguments<'_>) {
    if !is_scope_enabled(scope) {
        return;
    }
    write_line(&format!("{} [{scope}] {args}", io::milli_timestamp()))
}
pub fn eventf(scope: &str, event: &str, ctx: TraceContext, args: fmt::Arguments<'_>) {
    if !is_scope_enabled(scope) {
        return;
    }
    let mut head = format!("{} [{scope}] event={event}", io::milli_timestamp());
    if ctx.turn_id != 0 {
        head += &format!(" turn_id={}", ctx.turn_id)
    }
    if ctx.step_id != 0 {
        head += &format!(" step_id={}", ctx.step_id)
    }
    if ctx.subagent_id != 0 {
        head += &format!(" subagent_id={}", ctx.subagent_id)
    }
    if !args.as_str().is_some_and(str::is_empty) {
        head.push(' ');
        head += &args.to_string()
    }
    write_line(&head)
}
fn safe_trace(raw: &[u8]) -> Vec<u8> {
    let mut o = Vec::new();
    for &b in raw {
        let n = if (0x20..=0x7e).contains(&b) { 1 } else { 4 };
        if o.len() + n + 3 > MAX_TRACE_LINE_BYTES {
            o.extend_from_slice(b"...");
            break;
        }
        if n == 1 {
            o.push(b)
        } else {
            o.extend_from_slice(format!("\\x{b:02x}").as_bytes())
        }
    }
    o.push(b'\n');
    o
}
fn write_line(line: &str) {
    let data = safe_trace(line.as_bytes());
    let Ok(s) = state().lock() else { return };
    if !s.enabled {
        return;
    }
    if s.stderr_enabled {
        let _ = std::io::stderr().write_all(&data);
    }
    if let Some(path) = &s.file_path {
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
            let _ = f.write_all(&data);
        }
    }
}
pub fn preview(text: &[u8], max: usize) -> &[u8] {
    let t = trim(text);
    let end = t.iter().position(|&b| b == b'\n').unwrap_or(t.len());
    let mut line = &t[..end];
    if line.last() == Some(&b'\r') {
        line = &line[..line.len() - 1]
    }
    &line[..line.len().min(max)]
}
pub fn terminal_preview<'a>(buf: &'a mut [u8], text: &[u8]) -> &'a [u8] {
    let line = preview(text, 512);
    let (mut i, mut o) = (0, 0);
    while i < line.len() && o < buf.len() {
        if line[i] == 0x1b {
            i = skip_escape(line, i);
            continue;
        }
        let b = line[i];
        buf[o] = if b == b'\r' || b == b'\n' || b == b'\t' {
            b' '
        } else if b < 0x20 || b == 0x7f {
            b'?'
        } else {
            b
        };
        o += 1;
        i += 1
    }
    &buf[..o]
}
fn skip_escape(t: &[u8], s: usize) -> usize {
    if s + 1 >= t.len() {
        return s + 1;
    }
    match t[s + 1] {
        b'[' => {
            for i in s + 2..t.len() {
                if (0x40..=0x7e).contains(&t[i]) {
                    return i + 1;
                }
            }
            t.len()
        }
        b']' => {
            let mut i = s + 2;
            while i < t.len() {
                if t[i] == 7 {
                    return i + 1;
                }
                if t[i] == 0x1b && t.get(i + 1) == Some(&b'\\') {
                    return i + 2;
                }
                i += 1
            }
            t.len()
        }
        _ => s + 2,
    }
}
pub fn redacted_json_preview(text: &[u8]) -> Vec<u8> {
    json_preview(text, true)
}
pub fn keyless_json_preview(text: &[u8]) -> Vec<u8> {
    json_preview(text, false)
}
fn json_preview(text: &[u8], keys: bool) -> Vec<u8> {
    if text.is_empty() {
        return b"<empty>".to_vec();
    }
    if super::types::ToolArgumentIntegrity::classify_serialized(text)
        == super::types::ToolArgumentIntegrity::MalformedJson
    {
        return format!("<invalid-json bytes={}>", text.len()).into_bytes();
    }
    let mut out = Vec::new();
    if text.first() == Some(&b'{') {
        let mut fields = Vec::new();
        let (mut i, mut depth) = (1, 0);
        while i < text.len() - 1 && fields.len() < 6 {
            while i < text.len() && (text[i].is_ascii_whitespace() || text[i] == b',') {
                i += 1
            }
            if text.get(i) != Some(&b'"') {
                break;
            }
            i += 1;
            let start = i;
            while i < text.len() && text[i] != b'"' {
                i += 1
            }
            let key = &text[start..i];
            i += 1;
            while text
                .get(i)
                .is_some_and(|b| b.is_ascii_whitespace() || *b == b':')
            {
                i += 1
            }
            let shape = match text.get(i) {
                Some(b'"') => {
                    i += 1;
                    let s = i;
                    while i < text.len() && text[i] != b'"' {
                        i += 1
                    }
                    let n = i - s;
                    i += 1;
                    format!("<string_bytes={n}>")
                }
                Some(b'{') => {
                    depth += 1;
                    while i < text.len() && depth > 0 {
                        if text[i] == b'{' {
                            depth += 1
                        }
                        if text[i] == b'}' {
                            depth -= 1
                        }
                        i += 1
                    }
                    "<object_fields=1>".into()
                }
                Some(b'[') => {
                    let s = i;
                    i += 1;
                    while i < text.len() && text[i] != b']' {
                        i += 1
                    }
                    let n = if i == s + 1 {
                        0
                    } else {
                        text[s + 1..i].iter().filter(|&&b| b == b',').count() + 1
                    };
                    i += 1;
                    format!("<array_len={n}>")
                }
                Some(b't' | b'f') => {
                    while i < text.len() && text[i].is_ascii_alphabetic() {
                        i += 1
                    }
                    "<bool>".into()
                }
                Some(b'n') => {
                    i += 4;
                    "<null>".into()
                }
                _ => {
                    while i < text.len() && !matches!(text[i], b',' | b'}') {
                        i += 1
                    }
                    "<number>".into()
                }
            };
            fields.push((key.to_vec(), shape));
        }
        if keys {
            out.push(b'{');
            for (n, (k, v)) in fields.iter().enumerate() {
                if n > 0 {
                    out.push(b',')
                }
                out.push(b'"');
                out.extend(k);
                out.extend_from_slice(b"\":");
                out.extend(v.as_bytes())
            }
            out.push(b'}')
        } else {
            out.extend_from_slice(format!("<object_fields={} values=[", fields.len()).as_bytes());
            for (n, (_, v)) in fields.iter().enumerate() {
                if n > 0 {
                    out.push(b',')
                }
                out.extend(v.as_bytes())
            }
            out.extend_from_slice(b"]>")
        }
    } else if text.first() == Some(&b'[') {
        out.extend_from_slice(b"<array_len=1>")
    } else {
        out.extend_from_slice(b"<number>")
    }
    out
}
pub fn reset_for_test() {
    shutdown();
    NEXT_TURN_ID.store(1, Ordering::SeqCst);
    NEXT_STEP_ID.store(1, Ordering::SeqCst);
    NEXT_SUBAGENT_ID.store(1, Ordering::SeqCst)
}
pub fn configure_for_test(path: impl Into<PathBuf>) -> std::io::Result<()> {
    configure(Options {
        file_path: Some(path.into()),
        ..Options::default()
    })
}
pub fn configure_for_test_with_scopes(
    path: impl Into<PathBuf>,
    filter: &str,
) -> std::io::Result<()> {
    configure(Options {
        file_path: Some(path.into()),
        scope_filter: Some(filter.into()),
        ..Options::default()
    })
}
pub fn configure(options: Options) -> std::io::Result<()> {
    let mut s = state().lock().expect("trace state lock poisoned");
    if s.configured {
        return Ok(());
    }
    s.configured = true;
    s.stderr_enabled = options.stderr_enabled;
    if let Some(path) = options.file_path {
        if let Some(p) = path.parent() {
            fs::create_dir_all(p)?
        }
        if fs::metadata(&path).is_ok_and(|m| m.len() > DEFAULT_LOG_MAX_BYTES) {
            fs::write(&path, [])?
        }
        OpenOptions::new().create(true).append(true).open(&path)?;
        s.file_path = Some(path)
    }
    s.scope_filter = options
        .scope_filter
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    s.enabled = s.stderr_enabled || s.file_path.is_some();
    Ok(())
}
pub fn is_enabled() -> bool {
    state().lock().is_ok_and(|s| s.enabled)
}
pub fn is_scope_enabled(scope: &str) -> bool {
    state().lock().is_ok_and(|s| {
        s.enabled
            && s.scope_filter
                .as_ref()
                .is_none_or(|f| f.split(',').any(|x| x.trim() == scope))
    })
}
fn truthy(v: Option<&[u8]>) -> bool {
    v.is_some_and(|x| {
        [b"1".as_slice(), b"true", b"yes", b"on"]
            .iter()
            .any(|y| trim(x).eq_ignore_ascii_case(y))
    })
}
fn trim(mut s: &[u8]) -> &[u8] {
    while s.first().is_some_and(u8::is_ascii_whitespace) {
        s = &s[1..]
    }
    while s.last().is_some_and(u8::is_ascii_whitespace) {
        s = &s[..s.len() - 1]
    }
    s
}
fn resolve_log_path(root: &Path, raw: &str) -> std::io::Result<PathBuf> {
    let p = raw.trim();
    if p.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "empty trace path",
        ));
    }
    let p = Path::new(p);
    Ok(if p.is_absolute() {
        p.to_owned()
    } else {
        root.join(p)
    })
}
fn default_log_path() -> PathBuf {
    io::getenv(b"HOME").filter(|v| !v.is_empty()).map_or_else(
        || PathBuf::from(format!("/tmp/fx-trace-{}.log", io::milli_timestamp())),
        |h| PathBuf::from(String::from_utf8_lossy(&h).into_owned()).join(".fx/logs/trace.log"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn previews() {
        assert_eq!(preview(b" \nfirst\r\nsecond", 128), b"first");
        let mut b = [0; 64];
        assert_eq!(
            terminal_preview(&mut b, b"hi \x1b[31mred\x1b[0m"),
            b"hi red"
        )
    }
    #[test]
    fn ids() {
        reset_for_test();
        assert_eq!(next_turn_id(), 1);
        assert_eq!(next_turn_id(), 2);
        reset_for_test();
        assert_eq!(next_turn_id(), 1)
    }
    #[test]
    fn logger() {
        let p = std::env::temp_dir().join(format!("trace-{}.log", io::nano_timestamp()));
        reset_for_test();
        configure_for_test(&p).unwrap();
        logf("test", format_args!("hello {}", 42));
        assert!(fs::read(&p)
            .unwrap()
            .windows(15)
            .any(|w| w == b"[test] hello 42"));
        shutdown();
        fs::remove_file(p).unwrap()
    }
}
