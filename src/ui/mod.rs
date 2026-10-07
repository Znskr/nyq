//! Terminal frontend: line editor, streaming renderer, approvals.

mod render;

use std::borrow::Cow;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use nu_ansi_term::{Color, Style};
use reedline::{
    default_emacs_keybindings, ColumnarMenu, Completer, CompletionResult, EditCommand, Emacs, FileBackedHistory, KeyCode,
    KeyModifiers, MenuBuilder, PromptEditMode, PromptHistorySearch, Reedline, ReedlineEvent, ReedlineMenu, Signal, Span,
    Suggestion, ValidationResult, Validator,
};
use serde_json::Value;

use crate::agent::{Approval, ApprovalRequest, Ui};
use crate::tools::ToolOutput;
use crate::types::{Item, StreamEvent};
use render::{capped, diff, dim, looks_like_diff, one_line, paint, Markdown};

/// Lines of diff/preview shown before truncating.
const MAX_PREVIEW: usize = 60;

/// Something was printed since the last prompt (print a gap before the next).
static DIRTY: AtomicBool = AtomicBool::new(false);
/// The prompt just handed a message to the agent; the next block starts a turn.
static NEW_TURN: AtomicBool = AtomicBool::new(false);
/// PlainUi: whether stdout currently ends with a newline.
static PLAIN_AT_BOL: AtomicBool = AtomicBool::new(true);

fn colors(tty: bool) -> bool {
    tty && std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty())
}
fn color_out() -> bool {
    colors(io::stdout().is_terminal())
}
fn color_err() -> bool {
    colors(io::stderr().is_terminal())
}

// ---------------------------------------------------------------- spinner

struct Spinner {
    stop: Arc<AtomicBool>,
    handle: JoinHandle<()>,
}

static SPINNER: Mutex<Option<Spinner>> = Mutex::new(None);

/// Show an animated `label` on stderr until `spinner_stop`.
fn spinner_start(label: &str) {
    spinner_stop();
    if !io::stderr().is_terminal() {
        return;
    }
    let (stop, color) = (Arc::new(AtomicBool::new(false)), color_err());
    let label = one_line(label, 40);
    let flag = stop.clone();
    let handle = std::thread::spawn(move || {
        const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        let start = Instant::now();
        // Short grace period so instant responses don't flash a spinner.
        std::thread::park_timeout(Duration::from_millis(120));
        let mut i = 0;
        let mut drawn = false;
        while !flag.load(Ordering::Acquire) {
            let secs = start.elapsed().as_secs();
            let time = if secs >= 2 { format!(" {secs}s") } else { String::new() };
            let text = format!("{} {label}…{time}", FRAMES[i % FRAMES.len()]);
            let _ = write!(io::stderr(), "\r\x1b[2K{}", paint(color, dim(), &text));
            let _ = io::stderr().flush();
            drawn = true;
            i += 1;
            std::thread::park_timeout(Duration::from_millis(80));
        }
        if drawn {
            let _ = write!(io::stderr(), "\r\x1b[2K");
            let _ = io::stderr().flush();
        }
    });
    *SPINNER.lock().unwrap_or_else(|e| e.into_inner()) = Some(Spinner { stop, handle });
}

/// Stop and erase the spinner (blocks until its line is cleared).
fn spinner_stop() {
    let spinner = SPINNER.lock().unwrap_or_else(|e| e.into_inner()).take();
    if let Some(s) = spinner {
        s.stop.store(true, Ordering::Release);
        s.handle.thread().unpark();
        let _ = s.handle.join();
    }
}

// ---------------------------------------------------------------- prompt

pub enum Input {
    Line(String),
    /// Ctrl-C on an empty prompt.
    Interrupt,
    /// Ctrl-D.
    Eof,
}

struct PromptStyle {
    status: String,
}

impl reedline::Prompt for PromptStyle {
    fn render_prompt_left(&self) -> Cow<'_, str> {
        Cow::Borrowed("")
    }
    fn render_prompt_right(&self) -> Cow<'_, str> {
        Cow::Borrowed(&self.status)
    }
    fn render_prompt_indicator(&self, _mode: PromptEditMode) -> Cow<'_, str> {
        Cow::Borrowed("› ")
    }
    fn render_prompt_multiline_indicator(&self) -> Cow<'_, str> {
        Cow::Borrowed("  ")
    }
    fn render_prompt_history_search_indicator(&self, search: PromptHistorySearch) -> Cow<'_, str> {
        Cow::Owned(format!("(search: {}) ", search.term))
    }
    fn get_indicator_color(&self) -> Color {
        Color::Cyan
    }
    fn get_prompt_right_color(&self) -> Color {
        Color::DarkGray
    }
}

struct SlashCompleter(Vec<String>);

impl Completer for SlashCompleter {
    fn complete(&mut self, line: &str, pos: usize) -> CompletionResult {
        let prefix = &line[..pos.min(line.len())];
        if !prefix.starts_with('/') || prefix.contains(char::is_whitespace) {
            return CompletionResult::fresh(Vec::new());
        }
        let suggestions: Vec<Suggestion> = self
            .0
            .iter()
            .filter(|c| c.starts_with(prefix))
            .map(|c| Suggestion { value: c.clone(), span: Span::new(0, pos), append_whitespace: true, ..Default::default() })
            .collect();
        CompletionResult::fresh(suggestions)
    }
}

/// A trailing backslash continues the input on the next line.
struct Continuation;

impl Validator for Continuation {
    fn validate(&self, line: &str) -> ValidationResult {
        if line.ends_with('\\') {
            ValidationResult::Incomplete
        } else {
            ValidationResult::Complete
        }
    }
}

/// Reedline-based prompt with history and slash-command completion.
pub struct Prompt {
    editor: Reedline,
    style: PromptStyle,
}

impl Prompt {
    pub fn new(history: PathBuf, commands: Vec<String>) -> Result<Self> {
        if let Some(dir) = history.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let history = FileBackedHistory::with_file(1000, history).map_err(|e| anyhow!("history: {e}"))?;
        let mut keys = default_emacs_keybindings();
        keys.add_binding(
            KeyModifiers::NONE,
            KeyCode::Tab,
            ReedlineEvent::UntilFound(vec![ReedlineEvent::Menu("completion_menu".into()), ReedlineEvent::MenuNext]),
        );
        for m in [KeyModifiers::ALT, KeyModifiers::SHIFT] {
            keys.add_binding(m, KeyCode::Enter, ReedlineEvent::Edit(vec![EditCommand::InsertNewline]));
        }
        let menu = ColumnarMenu::default().with_name("completion_menu");
        let editor = Reedline::create()
            .with_history(Box::new(history))
            .with_completer(Box::new(SlashCompleter(commands)))
            .with_menu(ReedlineMenu::EngineCompleter(Box::new(menu)))
            .with_edit_mode(Box::new(Emacs::new(keys)))
            .with_validator(Box::new(Continuation))
            .with_ansi_colors(color_out())
            .use_kitty_keyboard_enhancement(true);
        Ok(Self { editor, style: PromptStyle { status: String::new() } })
    }

    /// `status` is a short right-aligned hint such as the model name.
    pub fn read(&mut self, status: &str) -> Result<Input> {
        spinner_stop();
        if DIRTY.swap(false, Ordering::Relaxed) {
            println!();
        }
        self.style.status = status.to_string();
        Ok(match self.editor.read_line(&self.style).map_err(|e| anyhow!("{e}"))? {
            Signal::Success(line) => {
                let line = line.replace("\\\n", "\n");
                let t = line.trim();
                if !t.is_empty() && !t.starts_with('/') && !t.starts_with('!') {
                    println!();
                    NEW_TURN.store(true, Ordering::Relaxed);
                    spinner_start("thinking");
                }
                Input::Line(line)
            }
            Signal::CtrlD => Input::Eof,
            _ => Input::Interrupt,
        })
    }
}

// ---------------------------------------------------------------- TermUi

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    None,
    User,
    Reasoning,
    Text,
    Tool,
}

/// Interactive renderer. Streams assistant text with light markdown styling,
/// shows tool calls/results compactly, and asks y/a/n approvals.
pub struct TermUi {
    color: bool,
    md: Markdown,
    /// Partial line of the stream currently being received.
    buf: String,
    /// Kind of content in `buf`.
    cur: Kind,
    /// Whether the current stream has printed anything yet.
    started: bool,
    /// Blank lines held back until more content follows.
    blanks: usize,
    /// Kind of the last block printed (for spacing between blocks).
    printed: Kind,
    /// Tool whose header the approval prompt already printed.
    approved: Option<String>,
    denied: bool,
}

impl Default for TermUi {
    fn default() -> Self {
        Self::new()
    }
}

impl TermUi {
    pub fn new() -> Self {
        Self {
            color: color_out(),
            md: Markdown::default(),
            buf: String::new(),
            cur: Kind::None,
            started: false,
            blanks: 0,
            printed: Kind::None,
            approved: None,
            denied: false,
        }
    }

    /// Start a block of `kind`, separating it from a different previous one.
    fn begin(&mut self, kind: Kind) {
        if NEW_TURN.swap(false, Ordering::Relaxed) {
            self.printed = Kind::None;
        }
        if self.printed != Kind::None && self.printed != kind {
            println!();
        }
        self.printed = kind;
        DIRTY.store(true, Ordering::Relaxed);
    }

    /// Switch the stream to `kind`, flushing any partial line of the old one.
    fn switch(&mut self, kind: Kind) {
        if self.cur != kind {
            self.flush_partial();
            self.cur = kind;
            self.started = false;
            self.blanks = 0;
        }
    }

    fn flush_partial(&mut self) {
        if !self.buf.is_empty() {
            let line = std::mem::take(&mut self.buf);
            self.emit(&line);
        }
        let _ = io::stdout().flush();
    }

    fn emit(&mut self, line: &str) {
        let line = line.trim_end_matches('\r');
        if line.trim().is_empty() {
            if self.started {
                self.blanks += 1;
            }
            return;
        }
        let first = !self.started;
        if first {
            self.begin(self.cur);
            self.started = true;
        }
        let mut out = io::stdout().lock();
        for _ in 0..std::mem::take(&mut self.blanks) {
            let _ = writeln!(out);
        }
        let rendered = match self.cur {
            Kind::Reasoning => {
                let text = line.replace("**", "");
                let text = if first { format!("thinking: {text}") } else { text };
                paint(self.color, dim().italic(), &text)
            }
            _ => self.md.line(line, self.color),
        };
        let _ = writeln!(out, "{rendered}");
    }

    fn push(&mut self, kind: Kind, delta: &str) {
        spinner_stop();
        self.switch(kind);
        self.buf.push_str(delta);
        while let Some(i) = self.buf.find('\n') {
            let line: String = self.buf.drain(..=i).collect();
            self.emit(&line[..i]);
        }
        let _ = io::stdout().flush();
    }

    fn header(&mut self, bullet: Color, tool: &str, summary: &str) {
        self.switch(Kind::None);
        self.begin(Kind::Tool);
        let c = self.color;
        println!(
            "{} {}  {}",
            paint(c, Style::new().fg(bullet), "●"),
            paint(c, Style::new().bold(), tool),
            paint(c, dim(), &one_line(summary, 100))
        );
    }
}

impl Ui for TermUi {
    fn notice(&mut self, msg: &str) {
        info(msg);
    }
    fn stream(&mut self, event: &StreamEvent) {
        match event {
            StreamEvent::Text(t) => self.push(Kind::Text, t),
            StreamEvent::Reasoning(t) => self.push(Kind::Reasoning, t),
            StreamEvent::ToolCallStart { name, .. } => {
                spinner_stop();
                self.switch(Kind::None);
                spinner_start(name);
            }
        }
    }

    fn end_message(&mut self) {
        spinner_stop();
        self.switch(Kind::None);
        self.md = Markdown::default();
    }

    fn tool_start(&mut self, tool: &str, summary: &str) {
        spinner_stop();
        if self.approved.take().as_deref() != Some(tool) {
            self.header(Color::Green, tool, summary);
        }
        spinner_start(tool);
    }

    fn tool_end(&mut self, _tool: &str, output: &ToolOutput) {
        spinner_stop();
        self.switch(Kind::None);
        self.begin(Kind::Tool);
        let c = self.color;
        let mut out = String::new();
        if std::mem::take(&mut self.denied) {
            out = format!("  ⎿ {}\n", paint(c, Style::new().fg(Color::Red), "denied"));
        } else if output.is_error {
            let red = Style::new().fg(Color::Red);
            let text = output.content.trim();
            let text = if text.is_empty() { "failed" } else { text };
            for (i, l) in text.lines().take(4).enumerate() {
                let lead = if i == 0 { "  ⎿ " } else { "    " };
                out.push_str(&format!("{lead}{}\n", paint(c, red, &one_line(l, 160))));
            }
            if text.lines().count() > 4 {
                out.push_str(&format!("    {}\n", paint(c, dim(), "…")));
            }
        } else if let Some(display) = output.display.as_deref().filter(|d| !d.trim().is_empty()) {
            out = diff(display, c, MAX_PREVIEW, "    ");
        } else {
            let text = output.content.trim();
            let n = text.lines().count();
            let digest = match n {
                0 => "done".to_string(),
                1 if text.chars().count() <= 80 => text.to_string(),
                _ => format!("{n} lines"),
            };
            out = format!("  ⎿ {}\n", paint(c, dim(), &digest));
        }
        print!("{out}");
        let _ = io::stdout().flush();
        spinner_start("thinking");
    }

    fn approve(&mut self, req: &ApprovalRequest<'_>) -> Approval {
        spinner_stop();
        self.header(Color::Yellow, req.tool, req.summary);
        let c = self.color;
        if let Some(p) = req.preview.filter(|p| !p.trim().is_empty()) {
            let body = if looks_like_diff(p) {
                diff(p, c, MAX_PREVIEW, "    ")
            } else {
                capped(p.lines(), MAX_PREVIEW, "    ", c, |l| paint(c, dim(), l))
            };
            print!("{body}");
        }
        let key = |k: &str| paint(c, Style::new().bold(), k);
        print!("  Allow? [{}]es / [{}]lways / [{}]o ", key("y"), key("a"), key("n"));
        let _ = io::stdout().flush();
        let answer = read_approval();
        let (word, style) = match answer {
            Approval::Yes => ("yes", Style::new().fg(Color::Green)),
            Approval::Always => ("always", Style::new().fg(Color::Green)),
            Approval::No => ("no", Style::new().fg(Color::Red)),
        };
        println!("{}", paint(c, style, word));
        match answer {
            Approval::No => self.denied = true,
            _ => self.approved = Some(req.tool.to_string()),
        }
        answer
    }
}

struct RawGuard;

impl Drop for RawGuard {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

/// Read y / a / n (or Esc, Ctrl-C) as a single key; Enter does nothing.
fn read_approval() -> Approval {
    use crossterm::event::{read, Event, KeyCode, KeyEventKind, KeyModifiers};
    if !io::stdin().is_terminal() || crossterm::terminal::enable_raw_mode().is_err() {
        let mut line = String::new();
        let _ = io::stdin().lock().read_line(&mut line);
        return match line.trim().to_lowercase().as_str() {
            "y" | "yes" => Approval::Yes,
            "a" | "always" => Approval::Always,
            _ => Approval::No,
        };
    }
    let _guard = RawGuard;
    loop {
        let Ok(ev) = read() else { return Approval::No };
        let Event::Key(k) = ev else { continue };
        if k.kind != KeyEventKind::Press {
            continue;
        }
        match k.code {
            KeyCode::Char('c' | 'd') if k.modifiers.contains(KeyModifiers::CONTROL) => return Approval::No,
            KeyCode::Char('y' | 'Y') => return Approval::Yes,
            KeyCode::Char('a' | 'A') => return Approval::Always,
            KeyCode::Char('n' | 'N') | KeyCode::Esc => return Approval::No,
            _ => {}
        }
    }
}

// ---------------------------------------------------------------- PlainUi

/// Non-interactive renderer for `nyq -p`: assistant text to stdout, tool
/// activity to stderr, approvals granted only when `yolo`.
pub struct PlainUi {
    pub yolo: bool,
}

impl Ui for PlainUi {
    fn notice(&mut self, msg: &str) {
        eprintln!("{msg}");
    }
    fn stream(&mut self, event: &StreamEvent) {
        if let StreamEvent::Text(t) = event {
            if !t.is_empty() {
                let mut out = io::stdout().lock();
                let _ = out.write_all(t.as_bytes());
                let _ = out.flush();
                PLAIN_AT_BOL.store(t.ends_with('\n'), Ordering::Relaxed);
            }
        }
    }
    fn end_message(&mut self) {
        if !PLAIN_AT_BOL.swap(true, Ordering::Relaxed) {
            println!();
        }
    }
    fn tool_start(&mut self, tool: &str, summary: &str) {
        eprintln!("● {tool}  {}", one_line(summary, 120));
    }
    fn tool_end(&mut self, tool: &str, output: &ToolOutput) {
        if output.is_error {
            eprintln!("  {tool} failed: {}", one_line(&output.content, 160));
        }
    }
    fn approve(&mut self, req: &ApprovalRequest<'_>) -> Approval {
        if self.yolo {
            return Approval::Yes;
        }
        eprintln!("● {}  {} — denied (use --yolo to allow)", req.tool, one_line(req.summary, 120));
        Approval::No
    }
}

// ---------------------------------------------------------------- misc

/// A short human digest of raw JSON tool arguments.
fn args_digest(args: &str) -> String {
    let scalar = |v: &Value| match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    };
    let text = match serde_json::from_str::<Value>(args) {
        Ok(Value::Object(m)) => ["command", "path", "file_path", "pattern", "url", "query"]
            .iter()
            .find_map(|k| m.get(*k).and_then(scalar))
            .or_else(|| m.values().find_map(scalar))
            .unwrap_or_default(),
        _ => args.to_string(),
    };
    one_line(&text, 80)
}

/// Print a compact view of a resumed transcript.
pub fn replay(items: &[Item]) {
    let c = color_out();
    let mut out = io::stdout().lock();
    let mut last = Kind::None;
    for item in items {
        let kind = match item {
            Item::User { .. } => Kind::User,
            Item::Assistant { .. } => Kind::Text,
            Item::ToolCall { .. } => Kind::Tool,
            _ => continue,
        };
        if last != Kind::None && (last != kind || kind != Kind::Tool) {
            let _ = writeln!(out);
        }
        last = kind;
        match item {
            Item::User { text, .. } => {
                for (i, l) in text.trim().lines().enumerate() {
                    let lead = if i == 0 { "› " } else { "  " };
                    let _ = writeln!(out, "{}{}", paint(c, Style::new().fg(Color::Cyan), lead), paint(c, Style::new().bold(), l));
                }
            }
            Item::Assistant { text } => {
                let mut md = Markdown::default();
                for l in text.trim().lines() {
                    let _ = writeln!(out, "{}", md.line(l, c));
                }
            }
            Item::ToolCall { name, arguments, .. } => {
                let _ = writeln!(
                    out,
                    "{} {}  {}",
                    paint(c, Style::new().fg(Color::Green), "●"),
                    paint(c, Style::new().bold(), name),
                    paint(c, dim(), &args_digest(arguments))
                );
            }
            _ => {}
        }
    }
    if last != Kind::None {
        DIRTY.store(true, Ordering::Relaxed);
    }
}

pub fn banner(provider: &str, model: &str, cwd: &str) {
    let c = color_err();
    let home = dirs::home_dir().map(|h| h.display().to_string()).filter(|h| !h.is_empty());
    let cwd = match home.as_deref().and_then(|h| cwd.strip_prefix(h)) {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => format!("~{rest}"),
        _ => cwd.to_string(),
    };
    eprintln!(
        "{} {}",
        paint(c, Style::new().bold().fg(Color::Cyan), "nyq"),
        paint(c, dim(), &format!("· {provider}/{model} · {cwd}"))
    );
    eprintln!("{}\n", paint(c, dim(), "/help for commands · Alt-Enter for a newline · Ctrl-D to quit"));
}

pub fn info(msg: &str) {
    spinner_stop();
    eprintln!("{msg}");
}

pub fn error(msg: &str) {
    spinner_stop();
    eprintln!("{}", paint(color_err(), Style::new().fg(Color::Red), &format!("error: {msg}")));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_prefers_meaningful_keys() {
        assert_eq!(args_digest(r#"{"timeout":5,"command":"cargo test"}"#), "cargo test");
        assert_eq!(args_digest(r#"{"x":{"y":1},"name":"hi"}"#), "hi");
        assert_eq!(args_digest("not json"), "not json");
    }

    #[test]
    fn slash_completion() {
        let mut c = SlashCompleter(vec!["/help".into(), "/history".into(), "/new".into()]);
        let r = c.complete("/h", 2);
        let v: Vec<_> = r.suggestions().iter().map(|s| s.value.as_str()).collect();
        assert_eq!(v, ["/help", "/history"]);
        assert!(c.complete("hello", 5).suggestions().is_empty());
        assert!(c.complete("/new x", 6).suggestions().is_empty());
    }
}
