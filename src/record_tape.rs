//! Asciicast v2 terminal output capture and timed playback.
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{ensure, Context, Result};
use serde_json::json;

static ACTIVE: Mutex<Option<Tape>> = Mutex::new(None);

struct Tape {
    file: File,
    start: Instant,
    size: (u16, u16),
    pending: Vec<u8>,
    failure: Option<String>,
    /// Also record keystrokes (`NYQ_RECORD_INPUT`).
    input: bool,
}
impl Tape {
    fn event(&mut self, time: f64, kind: &str, data: &str) -> io::Result<()> {
        serde_json::to_writer(&mut self.file, &json!([time, kind, data]))?;
        self.file.write_all(b"\n")?;
        self.file.flush()
    }
    fn output(&mut self, bytes: &[u8]) -> io::Result<()> {
        let time = self.start.elapsed().as_secs_f64();
        let size = crossterm::terminal::size().unwrap_or(self.size);
        if size != self.size {
            self.event(time, "r", &format!("{}x{}", size.0, size.1))?;
            self.size = size;
        }
        self.pending.extend_from_slice(bytes);
        // A Write call may end in the middle of a UTF-8 character.
        let len = match std::str::from_utf8(&self.pending) {
            Ok(_) => self.pending.len(),
            Err(e) if e.error_len().is_none() => e.valid_up_to(),
            Err(_) => self.pending.len(),
        };
        if len > 0 {
            let text = String::from_utf8_lossy(&self.pending[..len]).into_owned();
            self.event(time, "o", &text)?;
            self.pending.drain(..len);
        }
        Ok(())
    }
}

/// `NYQ_RECORD`: record the interactive session to this tape file.
pub fn env_path() -> Option<std::path::PathBuf> {
    std::env::var_os("NYQ_RECORD")
        .filter(|v| !v.is_empty())
        .map(std::path::PathBuf::from)
}

/// `NYQ_RECORD_INPUT=1`: also record keystrokes as input (`i`) events.
fn records_input() -> bool {
    std::env::var_os("NYQ_RECORD_INPUT").is_some_and(|v| !v.is_empty() && v != "0")
}

/// Record typed input (when `NYQ_RECORD_INPUT` is set and a tape is open).
pub fn input(text: &str) {
    let mut active = ACTIVE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(tape) = active.as_mut().filter(|t| t.input && t.failure.is_none()) {
        let time = tape.start.elapsed().as_secs_f64();
        if let Err(e) = tape.event(time, "i", text) {
            tape.failure = Some(e.to_string());
        }
    }
}

/// The bytes a terminal would send for a key, for input events.
pub fn key_text(key: &crossterm::event::KeyEvent) -> Option<String> {
    use crossterm::event::{KeyCode, KeyModifiers};
    let text = match key.code {
        KeyCode::Char(c) if key.modifiers.contains(KeyModifiers::CONTROL) && c.is_ascii_alphabetic() => {
            ((c.to_ascii_lowercase() as u8 - b'a' + 1) as char).to_string()
        }
        KeyCode::Char(c) if key.modifiers.contains(KeyModifiers::ALT) => format!("\x1b{c}"),
        KeyCode::Char(c) => c.to_string(),
        KeyCode::Enter => "\r".into(),
        KeyCode::Tab => "\t".into(),
        KeyCode::BackTab => "\x1b[Z".into(),
        KeyCode::Backspace => "\x7f".into(),
        KeyCode::Esc => "\x1b".into(),
        KeyCode::Up => "\x1b[A".into(),
        KeyCode::Down => "\x1b[B".into(),
        KeyCode::Right => "\x1b[C".into(),
        KeyCode::Left => "\x1b[D".into(),
        KeyCode::Home => "\x1b[H".into(),
        KeyCode::End => "\x1b[F".into(),
        KeyCode::Delete => "\x1b[3~".into(),
        _ => return None,
    };
    Some(text)
}

pub struct Capture;
impl Capture {
    /// Start recording to `path`, or `<home>/tapes/<session>.cast`.
    pub fn start(home: &Path, session: &str, path: Option<std::path::PathBuf>) -> Result<Self> {
        let path = path.unwrap_or_else(|| home.join("tapes").join(format!("{session}.cast")));
        ensure!(!path.exists(), "tape already exists: {}", path.display());
        let size = crossterm::terminal::size()
            .ok()
            .filter(|(w, h)| *w > 0 && *h > 0)
            .unwrap_or((80, 24));
        let header = json!({"version":2,"width":size.0,"height":size.1,
            "timestamp":chrono::Utc::now().timestamp(),"title":format!("nyq session {session}")});
        crate::session::write_private(&path, format!("{header}\n").as_bytes())?;
        let file = OpenOptions::new().append(true).open(&path)?;
        *ACTIVE.lock().unwrap_or_else(|e| e.into_inner()) = Some(Tape {
            file,
            start: Instant::now(),
            size,
            pending: Vec::new(),
            failure: None,
            input: records_input(),
        });
        Ok(Self)
    }
}
impl Drop for Capture {
    fn drop(&mut self) {
        if let Some(mut tape) = ACTIVE.lock().unwrap_or_else(|e| e.into_inner()).take() {
            if let Some(error) = &tape.failure {
                eprintln!("nyq: terminal tape recording failed: {error}");
            }
            let result = (|| -> io::Result<()> {
                if !tape.pending.is_empty() {
                    let text = String::from_utf8_lossy(&tape.pending).into_owned();
                    tape.event(tape.start.elapsed().as_secs_f64(), "o", &text)?;
                }
                tape.file.sync_all()
            })();
            if let Err(e) = result {
                eprintln!("nyq: saving terminal tape: {e}");
            }
        }
    }
}

pub struct Output;
pub fn stdout() -> Output {
    Output
}
impl Write for Output {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let mut active = ACTIVE.lock().unwrap_or_else(|e| e.into_inner());
        let n = io::stdout().write(bytes)?;
        if let Some(tape) = active.as_mut() {
            if tape.failure.is_none() {
                if let Err(e) = tape.output(&bytes[..n]) {
                    tape.failure = Some(e.to_string());
                }
            }
        }
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        io::stdout().flush()
    }
}

#[derive(Debug)]
struct Frame {
    at: f64,
    kind: String,
    data: String,
}
fn read(path: &Path) -> Result<Vec<Frame>> {
    let mut lines =
        BufReader::new(File::open(path).with_context(|| format!("opening {}", path.display()))?)
            .lines();
    let header: serde_json::Value = serde_json::from_str(&lines.next().context("empty tape")??)?;
    ensure!(
        header["version"] == 2
            && header["width"].as_u64().is_some_and(|n| n > 0)
            && header["height"].as_u64().is_some_and(|n| n > 0),
        "invalid asciicast v2 header"
    );
    let mut frames = Vec::new();
    let mut previous = 0.0;
    for (i, line) in lines.enumerate() {
        let (at, kind, data): (f64, String, String) =
            serde_json::from_str(&line?).with_context(|| format!("tape line {}", i + 2))?;
        ensure!(
            at.is_finite() && at >= previous,
            "invalid tape timestamp at line {}",
            i + 2
        );
        previous = at;
        frames.push(Frame { at, kind, data });
    }
    Ok(frames)
}
fn delay(at: f64, speed: f64) -> Result<Duration> {
    ensure!(
        speed.is_finite() && speed > 0.0,
        "speed must be a finite positive number"
    );
    Duration::try_from_secs_f64(at / speed).context("replay delay is too large")
}
pub async fn replay(path: &Path, speed: f64) -> Result<()> {
    delay(0.0, speed)?;
    let frames = read(path)?;
    // Validate before emitting any output.
    for frame in &frames {
        delay(frame.at, speed)?;
    }
    let start = tokio::time::Instant::now();
    let mut out = io::stdout();
    for frame in frames {
        tokio::time::sleep_until(start + delay(frame.at, speed)?).await;
        if frame.kind == "o" {
            out.write_all(frame.data.as_bytes())?;
            out.flush()?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    #[test]
    fn capture_creates_private_asciicast() {
        let tmp = tempfile::tempdir().unwrap();
        let capture = Capture::start(tmp.path(), "test-session", None).unwrap();
        ACTIVE
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .output(b"hello\r\n")
            .unwrap();
        drop(capture);
        let path = tmp.path().join("tapes/test-session.cast");
        let frames = read(&path).unwrap();
        assert!(frames.iter().any(|f| f.data == "hello\r\n"));
        assert!(frames.last().unwrap().at >= 0.0);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }
    #[test]
    fn keys_become_terminal_input() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let key = |code, mods| key_text(&KeyEvent::new(code, mods));
        assert_eq!(key(KeyCode::Char('a'), KeyModifiers::NONE).as_deref(), Some("a"));
        assert_eq!(key(KeyCode::Char('c'), KeyModifiers::CONTROL).as_deref(), Some("\x03"));
        assert_eq!(key(KeyCode::Enter, KeyModifiers::NONE).as_deref(), Some("\r"));
        assert_eq!(key(KeyCode::Up, KeyModifiers::NONE).as_deref(), Some("\x1b[A"));
        assert_eq!(key(KeyCode::F(5), KeyModifiers::NONE), None);
    }
    #[test]
    fn tape_roundtrip_and_timing() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("test.cast");
        fs::write(&path, "{\"version\":2,\"width\":80,\"height\":24}\n").unwrap();
        let mut tape = Tape {
            file: OpenOptions::new().append(true).open(&path).unwrap(),
            start: Instant::now(),
            size: (80, 24),
            pending: Vec::new(),
            failure: None,
            input: false,
        };
        tape.event(0.25, "o", "hello\r\n\u{1b}[31m").unwrap();
        tape.event(1.0, "r", "100x30").unwrap();
        let frames = read(&path).unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].at, 0.25);
        assert_eq!(frames[0].data, "hello\r\n\u{1b}[31m");
        assert_eq!(frames[1].at, 1.0);
        assert_eq!(frames[1].kind, "r");
        assert_eq!(frames[1].data, "100x30");
        // Use a fresh tape for live timestamps and split UTF-8 output.
        // (Reopen rather than truncate: Windows refuses set_len on an append handle.)
        fs::write(&path, "{\"version\":2,\"width\":80,\"height\":24}\n").unwrap();
        tape.file = OpenOptions::new().append(true).open(&path).unwrap();
        tape.output(&[0xc3]).unwrap();
        tape.output(&[0xa9]).unwrap();
        let frames = read(&path).unwrap();
        assert_eq!(frames.last().unwrap().data, "é");
        assert_eq!(delay(1.0, 2.0).unwrap(), Duration::from_millis(500));
        for speed in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert!(delay(1.0, speed).is_err());
        }
        fs::write(
            &path,
            "{\"version\":2,\"width\":80,\"height\":24}\n[1,\"o\",\"a\"]\n[0.5,\"o\",\"b\"]\n",
        )
        .unwrap();
        assert!(read(&path).is_err());
    }
}
