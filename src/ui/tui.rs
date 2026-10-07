//! Pi-style inline terminal UI. The transcript is printed into the normal
//! scrollback; below it a pane stays pinned to the bottom with the streaming
//! line, a status rule, queued messages, the editor (always accepting input,
//! even while the agent works) and a footer.

use std::io::{self, Stdout, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crossterm::event::{
    Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::{cursor, execute, queue, terminal};
use futures::StreamExt;
use nu_ansi_term::{Color, Style};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use unicode_width::UnicodeWidthChar;

use super::editor::{Action, Editor};
use super::render::{diff, looks_like_diff, one_line, Markdown};
use crate::agent::{Approval, ApprovalRequest, Ui};
use crate::tools::ToolOutput;
use crate::types::{Item, StreamEvent, Usage};

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const USER_BG: Color = Color::Rgb(52, 53, 65);
const TOOL_BG: Color = Color::Rgb(40, 50, 40);
const ERROR_BG: Color = Color::Rgb(62, 36, 36);
const ACCENT: Color = Color::Rgb(138, 173, 244);
const MAX_PREVIEW: usize = 14;
const TOOL_TAIL: usize = 6;

/// What the app loop receives from the input task.
pub enum Command {
    Submit(String),
    Exit,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Queued {
    Steer,
    FollowUp,
}

#[derive(Clone, Default)]
pub struct Footer {
    pub cwd: String,
    pub provider: String,
    pub model: String,
    pub sandbox: String,
    pub yolo: bool,
    pub usage: Usage,
    pub context_tokens: u64,
    pub context_window: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Block {
    None,
    Text,
    Reasoning,
    Tool,
    User,
    Info,
}

struct PendingApproval {
    lines: Vec<String>,
    reply: oneshot::Sender<Approval>,
}

pub struct Screen {
    out: Stdout,
    width: usize,
    height: usize,
    /// Rows of the pane currently on screen and the cursor's row within it.
    pane_rows: usize,
    cursor_row: usize,
    live: String,
    live_block: Block,
    last_block: Block,
    md: Markdown,
    busy: Option<(String, Instant)>,
    frame: usize,
    pub editor: Editor,
    pub queue: Vec<(Queued, String)>,
    approval: Option<PendingApproval>,
    pub footer: Footer,
    cancel: Option<CancellationToken>,
    tool_started: Option<Instant>,
    history: Vec<String>,
    /// A one-off hint shown in the status rule until the next key.
    hint: Option<String>,
}

static SCREEN: Mutex<Option<Arc<Mutex<Screen>>>> = Mutex::new(None);

/// The active screen, if the TUI is running.
pub fn active() -> Option<Arc<Mutex<Screen>>> {
    SCREEN.lock().ok()?.clone()
}

fn lock(s: &Arc<Mutex<Screen>>) -> MutexGuard<'_, Screen> {
    s.lock().unwrap_or_else(|e| e.into_inner())
}

fn paint(style: Style, s: &str) -> String {
    style.paint(s).to_string()
}

fn dim() -> Style {
    Style::new().dimmed()
}

/// Display width of text that may contain ANSI escapes.
fn visible_width(s: &str) -> usize {
    let mut w = 0;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            w += c.width().unwrap_or(0);
        }
    }
    w
}

/// Cut plain text to at most `width` columns.
fn truncate(s: &str, width: usize) -> String {
    let mut out = String::new();
    let mut w = 0;
    for c in s.chars() {
        let cw = c.width().unwrap_or(0);
        if w + cw > width {
            break;
        }
        w += cw;
        out.push(c);
    }
    out
}

/// Soft-wrap plain text into rows of at most `width` columns.
fn wrap(s: &str, width: usize) -> Vec<String> {
    let mut rows = vec![String::new()];
    let mut w = 0;
    for c in s.chars() {
        let cw = c.width().unwrap_or(0);
        if w + cw > width.max(1) {
            rows.push(String::new());
            w = 0;
        }
        rows.last_mut().unwrap().push(c);
        w += cw;
    }
    rows
}

/// A full-width row with a background color (pi's message blocks).
fn block_row(text: &str, bg: Color, width: usize) -> String {
    let pad = width.saturating_sub(visible_width(text) + 1);
    // Re-apply the background after any reset inside `text`.
    let bg_code = Style::new().on(bg).prefix().to_string();
    let body = text.replace("\x1b[0m", &format!("\x1b[0m{bg_code}"));
    format!("{bg_code} {body}{}\x1b[0m", " ".repeat(pad))
}

fn rule(width: usize, label: &str) -> String {
    if label.is_empty() {
        return paint(dim(), &"─".repeat(width));
    }
    let rest = width.saturating_sub(visible_width(label) + 4);
    format!("{} {label} {}", paint(dim(), "──"), paint(dim(), &"─".repeat(rest)))
}

fn tilde(path: &str) -> String {
    match dirs::home_dir().map(|h| h.display().to_string()) {
        Some(h) if !h.is_empty() && (path == h || path.starts_with(&format!("{h}/"))) => format!("~{}", &path[h.len()..]),
        _ => path.to_string(),
    }
}

fn tokens(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1000..=999_999 => format!("{:.1}k", n as f64 / 1000.0).replace(".0k", "k"),
        _ => format!("{:.1}M", n as f64 / 1_000_000.0),
    }
}

/// One collapsed line for a reasoning block: its first line (summaries from
/// Codex start with a bold title) plus how much was hidden.
fn thinking_summary(text: &str, width: usize) -> String {
    let lines: Vec<&str> = text.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    let first = lines.first().map_or("", |l| l.trim_matches('*'));
    let more = if lines.len() > 1 { format!(" (+{} lines of thinking)", lines.len() - 1) } else { String::new() };
    let first = truncate(first, width.saturating_sub(more.chars().count() + 1));
    format!("{first}{more}")
}

/// How a tool call is titled in its block.
fn tool_title(tool: &str, summary: &str, width: usize) -> String {
    let summary = truncate(&one_line(summary, 400), width.saturating_sub(tool.len() + 1));
    match tool {
        "bash" => format!("{} {}", paint(Style::new().bold(), "$"), paint(Style::new().bold(), &summary)),
        _ => format!("{} {}", paint(Style::new().bold(), tool), paint(Style::new().fg(ACCENT), &summary)),
    }
}

impl Screen {
    fn new(footer: Footer, editor: Editor, history: Vec<String>) -> Self {
        let (w, h) = terminal::size().unwrap_or((80, 24));
        Screen {
            out: io::stdout(),
            width: w.max(20) as usize,
            height: h.max(8) as usize,
            pane_rows: 0,
            cursor_row: 0,
            live: String::new(),
            live_block: Block::None,
            last_block: Block::None,
            md: Markdown::default(),
            busy: None,
            frame: 0,
            editor,
            queue: Vec::new(),
            approval: None,
            footer,
            cancel: None,
            tool_started: None,
            history,
            hint: None,
        }
    }

    // ------------------------------------------------------------ drawing

    fn clear_pane(&mut self) {
        if self.cursor_row > 0 {
            let _ = queue!(self.out, cursor::MoveUp(self.cursor_row as u16));
        }
        let _ = queue!(self.out, cursor::MoveToColumn(0), terminal::Clear(terminal::ClearType::FromCursorDown));
        self.pane_rows = 0;
        self.cursor_row = 0;
    }

    fn pane(&self) -> (Vec<String>, usize, usize) {
        let w = self.width;
        let mut rows = Vec::new();
        if self.live_block == Block::Reasoning {
            // Thinking stays collapsed: just its latest line, live.
            if let Some(last) = self.live.lines().rev().find(|l| !l.trim().is_empty()) {
                rows.push(paint(dim().italic(), &truncate(last.trim(), w)));
            }
        } else if !self.live.is_empty() {
            rows.extend(wrap(&self.live, w));
        }
        let status = match (&self.busy, &self.approval) {
            (_, Some(_)) => paint(Style::new().fg(Color::Yellow), "Approval needed"),
            (Some((label, start)), None) => {
                let secs = start.elapsed().as_secs();
                let time = if secs >= 2 { format!(" {secs}s") } else { String::new() };
                format!(
                    "{} {}{}",
                    paint(Style::new().fg(ACCENT), SPINNER[self.frame % SPINNER.len()]),
                    paint(Style::new().fg(ACCENT), &format!("{label}…")),
                    paint(dim(), &format!("{time} · esc to interrupt"))
                )
            }
            (None, None) => match &self.hint {
                Some(h) => paint(Style::new().fg(Color::Yellow), h),
                None => paint(dim(), "ctrl+d or /exit to quit"),
            },
        };
        rows.push(rule(w, &status));
        for (kind, text) in &self.queue {
            let tag = match kind {
                Queued::Steer => "steer",
                Queued::FollowUp => "follow-up",
            };
            let text = truncate(&one_line(text, w), w.saturating_sub(tag.len() + 5));
            rows.push(format!("  {} {}", paint(Style::new().fg(Color::Yellow), &format!("{tag} ›")), paint(dim(), &text)));
        }
        if !self.queue.is_empty() {
            rows.push(paint(dim(), "  alt+↑ to edit queued messages"));
        }
        let (cursor_row, cursor_col);
        if let Some(a) = &self.approval {
            rows.extend(a.lines.iter().cloned());
            rows.push(format!(
                "{}  {} once   {} always   {} deny",
                paint(Style::new().fg(Color::Yellow).bold(), "Allow?"),
                paint(Style::new().bold(), "y"),
                paint(Style::new().bold(), "a"),
                paint(Style::new().bold(), "n")
            ));
            cursor_row = rows.len() - 1;
            cursor_col = 6;
        } else {
            let view = self.editor.render(w.saturating_sub(2).max(1));
            let top = rows.len();
            for (i, l) in view.lines.iter().enumerate() {
                let lead = if i == 0 { paint(Style::new().fg(ACCENT).bold(), "› ") } else { "  ".into() };
                rows.push(format!("{lead}{l}"));
            }
            cursor_row = top + view.cursor_row;
            cursor_col = 2 + view.cursor_col;
            for (cmd, desc) in self.editor.suggestions() {
                rows.push(format!("  {}  {}", paint(Style::new().fg(ACCENT), &format!("{cmd:<10}")), paint(dim(), &truncate(&desc, w.saturating_sub(15)))));
            }
        }
        rows.push(rule(w, ""));
        let f = &self.footer;
        let mut left = tilde(&f.cwd);
        if !f.sandbox.is_empty() {
            left.push_str(&format!(" · {}", f.sandbox));
        }
        if f.yolo {
            left.push_str(" · yolo");
        }
        rows.push(paint(dim(), &truncate(&left, w)));
        let mut stats = format!("↑{} ↓{}", tokens(f.usage.input_tokens), tokens(f.usage.output_tokens));
        if f.usage.cached_tokens > 0 {
            stats.push_str(&format!(" R{}", tokens(f.usage.cached_tokens)));
        }
        if f.context_window > 0 {
            let pct = f.context_tokens as f64 * 100.0 / f.context_window as f64;
            stats.push_str(&format!(" {pct:.1}%/{}", tokens(f.context_window)));
        }
        let model = format!("({}) {}", f.provider, f.model);
        let gap = w.saturating_sub(visible_width(&stats) + visible_width(&model));
        let line = if gap >= 2 { format!("{stats}{}{model}", " ".repeat(gap)) } else { truncate(&stats, w) };
        rows.push(paint(dim(), &line));

        // Keep the pane on screen: drop streaming rows first.
        let max = self.height.saturating_sub(1).max(4);
        let excess = rows.len().saturating_sub(max);
        let dropped = excess.min(cursor_row);
        rows.drain(..dropped);
        (rows, cursor_row - dropped, cursor_col)
    }

    fn draw_pane(&mut self) {
        let (rows, row, col) = self.pane();
        let _ = queue!(self.out, terminal::BeginSynchronizedUpdate);
        for (i, r) in rows.iter().enumerate() {
            if i > 0 {
                let _ = self.out.write_all(b"\r\n");
            }
            let _ = self.out.write_all(r.as_bytes());
        }
        let up = rows.len().saturating_sub(1).saturating_sub(row);
        if up > 0 {
            let _ = queue!(self.out, cursor::MoveUp(up as u16));
        }
        let _ = queue!(self.out, cursor::MoveToColumn(col as u16), terminal::EndSynchronizedUpdate);
        let _ = self.out.flush();
        self.pane_rows = rows.len();
        self.cursor_row = row;
    }

    pub fn redraw(&mut self) {
        self.clear_pane();
        self.draw_pane();
    }

    /// Print finished rows into the scrollback above the pane.
    fn emit(&mut self, rows: &[String]) {
        self.clear_pane();
        for r in rows {
            let _ = self.out.write_all(r.as_bytes());
            let _ = self.out.write_all(b"\r\n");
        }
        self.draw_pane();
    }

    /// Start a new block, separated from the previous one by a blank line.
    fn begin(&mut self, block: Block) -> Vec<String> {
        let gap = self.last_block != Block::None && (self.last_block != block || matches!(block, Block::Tool | Block::User));
        self.last_block = block;
        if gap {
            vec![String::new()]
        } else {
            Vec::new()
        }
    }

    // ------------------------------------------------------------ content

    pub fn print_info(&mut self, text: &str, style: Style) {
        self.flush_live();
        let mut rows = self.begin(Block::Info);
        rows.extend(text.lines().map(|l| paint(style, l)));
        self.emit(&rows);
    }

    pub fn print_user(&mut self, text: &str) {
        self.flush_live();
        let w = self.width;
        let mut rows = self.begin(Block::User);
        rows.push(block_row("", USER_BG, w));
        for line in text.trim().lines() {
            for r in wrap(line, w.saturating_sub(2)) {
                rows.push(block_row(&r, USER_BG, w));
            }
        }
        rows.push(block_row("", USER_BG, w));
        self.emit(&rows);
    }

    pub fn print_markdown(&mut self, text: &str) {
        self.flush_live();
        let mut rows = self.begin(Block::Text);
        let mut md = Markdown::default();
        rows.extend(text.trim().lines().map(|l| md.line(l, true)));
        self.emit(&rows);
    }

    pub fn print_tool_call(&mut self, tool: &str, summary: &str) {
        let w = self.width;
        let mut rows = self.begin(Block::Tool);
        rows.push(block_row("", TOOL_BG, w));
        rows.push(block_row(&tool_title(tool, summary, w.saturating_sub(2)), TOOL_BG, w));
        rows.push(block_row("", TOOL_BG, w));
        self.emit(&rows);
    }

    fn stream(&mut self, block: Block, text: &str) {
        if self.live_block != block {
            self.flush_live();
            self.live_block = block;
            if block == Block::Text {
                self.md = Markdown::default();
            }
        }
        self.live.push_str(text);
        if block == Block::Reasoning {
            self.redraw();
            return;
        }
        if let Some(i) = self.live.rfind('\n') {
            let done: String = self.live.drain(..=i).collect();
            let mut rows = self.begin(block);
            for line in done.trim_end_matches('\n').split('\n') {
                rows.push(self.render_line(block, line));
            }
            self.emit(&rows);
        } else {
            self.redraw();
        }
    }

    fn render_line(&mut self, _block: Block, line: &str) -> String {
        self.md.line(line, true)
    }

    fn flush_live(&mut self) {
        if self.live.is_empty() {
            return;
        }
        let text = std::mem::take(&mut self.live);
        let block = self.live_block;
        let mut rows = self.begin(block);
        if block == Block::Reasoning {
            rows.push(paint(dim().italic(), &thinking_summary(&text, self.width)));
        } else {
            rows.push(self.render_line(block, &text));
        }
        self.emit(&rows);
    }

    /// Show the result of a command the user ran with `!`.
    pub fn print_tool_output(&mut self, output: &ToolOutput, started: Instant) {
        self.tool_started = Some(started);
        self.tool_result(output);
    }

    fn tool_result(&mut self, output: &ToolOutput) {
        let w = self.width;
        let inner = w.saturating_sub(2);
        let bg = if output.is_error { ERROR_BG } else { TOOL_BG };
        let mut rows = Vec::new();
        let body: Vec<String> = match &output.display {
            Some(d) if looks_like_diff(d) => diff(d, true, 40, "").lines().map(str::to_string).collect(),
            _ => {
                let lines: Vec<&str> = output.content.trim_end().lines().collect();
                let skip = lines.len().saturating_sub(TOOL_TAIL);
                let mut v = Vec::new();
                if skip > 0 {
                    v.push(paint(dim(), &format!("… ({skip} earlier lines)")));
                }
                let style = if output.is_error { Style::new().fg(Color::Rgb(240, 160, 160)) } else { dim() };
                v.extend(lines[skip..].iter().map(|l| paint(style, &truncate(l, inner))));
                v
            }
        };
        for r in body {
            rows.push(block_row(&r, bg, w));
        }
        if let Some(start) = self.tool_started.take() {
            rows.push(block_row("", bg, w));
            rows.push(block_row(&paint(dim(), &format!("Took {:.1}s", start.elapsed().as_secs_f64())), bg, w));
        }
        rows.push(block_row("", bg, w));
        self.last_block = Block::Tool;
        self.emit(&rows);
    }

    // ------------------------------------------------------------ state

    pub fn set_busy(&mut self, label: Option<&str>, cancel: Option<CancellationToken>) {
        self.busy = label.map(|l| (l.to_string(), Instant::now()));
        self.cancel = cancel;
        self.redraw();
    }

    pub fn is_busy(&self) -> bool {
        self.busy.is_some()
    }

    /// Move every queued message back into the editor (after an interrupt or
    /// on alt+up).
    pub fn dequeue(&mut self) {
        if self.queue.is_empty() {
            return;
        }
        let mut parts: Vec<String> = self.queue.drain(..).map(|(_, t)| t).collect();
        if !self.editor.is_empty() {
            parts.push(self.editor.text());
        }
        self.editor.set_text(&parts.join("\n\n"));
    }

    fn remember(&mut self, text: &str) {
        self.editor.push_history(text);
        if self.history.last().map(String::as_str) != Some(text) {
            self.history.push(text.to_string());
        }
    }

    fn on_key(&mut self, key: KeyEvent, commands: &mpsc::UnboundedSender<Command>) {
        if let Some(a) = self.approval.take() {
            let answer = match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => Some(Approval::Yes),
                KeyCode::Char('a') | KeyCode::Char('A') => Some(Approval::Always),
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => Some(Approval::No),
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => Some(Approval::No),
                _ => None,
            };
            match answer {
                Some(ans) => {
                    let _ = a.reply.send(ans);
                }
                None => self.approval = Some(a),
            }
            self.redraw();
            return;
        }
        let busy = self.is_busy();
        let was_empty = self.editor.is_empty();
        self.hint = None;
        match self.editor.handle_key(key, busy) {
            Action::None => {}
            Action::Submit(text) => {
                self.remember(&text);
                if busy {
                    self.queue.push((Queued::Steer, text));
                } else {
                    let _ = commands.send(Command::Submit(text));
                }
            }
            Action::FollowUp(text) => {
                self.remember(&text);
                self.queue.push((Queued::FollowUp, text));
            }
            Action::Interrupt => {
                if let Some(c) = &self.cancel {
                    c.cancel();
                }
            }
            Action::CtrlC { exit } => {
                if busy && was_empty {
                    if let Some(c) = &self.cancel {
                        c.cancel();
                    }
                } else if exit && !busy {
                    let _ = commands.send(Command::Exit);
                } else if was_empty && !busy {
                    self.hint = Some("press ctrl+c again to exit".into());
                }
            }
            Action::Exit => {
                if let Some(c) = &self.cancel {
                    c.cancel();
                }
                let _ = commands.send(Command::Exit);
            }
            Action::Dequeue => self.dequeue(),
        }
        self.redraw();
    }
}

// ---------------------------------------------------------------- lifecycle

/// Owns the terminal mode for the interactive session; restores it on drop.
pub struct Tui {
    pub screen: Arc<Mutex<Screen>>,
    pub commands: mpsc::UnboundedReceiver<Command>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    enhanced: bool,
    history_path: PathBuf,
}

impl Tui {
    pub fn start(footer: Footer, history_path: PathBuf, commands: Vec<(String, String)>) -> io::Result<Self> {
        // One JSON string per line (multi-line prompts); plain lines are accepted too.
        let history: Vec<String> = std::fs::read_to_string(&history_path)
            .map(|s| s.lines().filter(|l| !l.is_empty()).map(|l| serde_json::from_str(l).unwrap_or_else(|_| l.to_string())).collect())
            .unwrap_or_default();
        let screen = Arc::new(Mutex::new(Screen::new(footer, Editor::new(history.clone(), commands), history)));
        terminal::enable_raw_mode()?;
        let enhanced = terminal::supports_keyboard_enhancement().unwrap_or(false);
        let mut out = io::stdout();
        execute!(out, crossterm::event::EnableBracketedPaste)?;
        if enhanced {
            execute!(out, PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES))?;
        }
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore_terminal();
            prev(info);
        }));
        *SCREEN.lock().unwrap() = Some(screen.clone());
        lock(&screen).redraw();

        let (tx, rx) = mpsc::unbounded_channel();
        let input = {
            let screen = screen.clone();
            tokio::spawn(async move {
                let mut events = EventStream::new();
                while let Some(Ok(ev)) = events.next().await {
                    let mut s = lock(&screen);
                    match ev {
                        Event::Key(k) if k.kind != KeyEventKind::Release => s.on_key(k, &tx),
                        Event::Paste(text) => {
                            if s.approval.is_none() {
                                s.editor.paste(&text);
                            }
                            s.redraw();
                        }
                        Event::Resize(w, h) => {
                            s.width = (w as usize).max(20);
                            s.height = (h as usize).max(8);
                            s.redraw();
                        }
                        _ => {}
                    }
                }
                let _ = tx.send(Command::Exit);
            })
        };
        let ticker = {
            let screen = screen.clone();
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(Duration::from_millis(90));
                loop {
                    tick.tick().await;
                    let mut s = lock(&screen);
                    if s.busy.is_some() && s.approval.is_none() {
                        s.frame += 1;
                        s.redraw();
                    }
                }
            })
        };
        Ok(Tui { screen, commands: rx, tasks: vec![input, ticker], enhanced, history_path })
    }

    pub fn screen(&self) -> MutexGuard<'_, Screen> {
        lock(&self.screen)
    }

    /// The `agent::Ui` for one run.
    pub fn ui(&self, cwd: PathBuf) -> TuiUi {
        TuiUi { screen: self.screen.clone(), cwd }
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
        {
            let mut s = lock(&self.screen);
            s.clear_pane();
            let _ = s.out.flush();
            let skip = s.history.len().saturating_sub(1000);
            let history: Vec<String> = s.history[skip..].iter().map(|h| serde_json::to_string(h).unwrap_or_default()).collect();
            if let Some(dir) = self.history_path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(&self.history_path, history.join("\n") + "\n");
        }
        *SCREEN.lock().unwrap() = None;
        if self.enhanced {
            let _ = execute!(io::stdout(), PopKeyboardEnhancementFlags);
        }
        restore_terminal();
    }
}

fn restore_terminal() {
    let _ = execute!(io::stdout(), crossterm::event::DisableBracketedPaste, cursor::Show);
    let _ = terminal::disable_raw_mode();
}

// ---------------------------------------------------------------- agent::Ui

pub struct TuiUi {
    screen: Arc<Mutex<Screen>>,
    cwd: PathBuf,
}

impl TuiUi {
    fn s(&self) -> MutexGuard<'_, Screen> {
        lock(&self.screen)
    }

    fn deliver(&self, text: String) -> Item {
        self.s().print_user(&text);
        match crate::tools::image::user_message(&text, &self.cwd) {
            Ok((item, _)) => item,
            Err(_) => Item::user(text),
        }
    }
}

#[async_trait::async_trait]
impl Ui for TuiUi {
    fn stream(&mut self, event: &StreamEvent) {
        let mut s = self.s();
        match event {
            StreamEvent::Text(t) => s.stream(Block::Text, t),
            StreamEvent::Reasoning(t) => s.stream(Block::Reasoning, t),
            StreamEvent::ToolCallStart { name, .. } => {
                if let Some((label, _)) = &mut s.busy {
                    *label = format!("Calling {name}");
                }
                s.redraw();
            }
        }
    }

    fn end_message(&mut self) {
        let mut s = self.s();
        s.flush_live();
        if let Some((label, _)) = &mut s.busy {
            *label = "Working".into();
        }
        s.redraw();
    }

    fn tool_start(&mut self, tool: &str, summary: &str) {
        let mut s = self.s();
        s.flush_live();
        s.print_tool_call(tool, summary);
        s.tool_started = Some(Instant::now());
        if let Some((label, _)) = &mut s.busy {
            *label = format!("Running {tool}");
        }
        s.redraw();
    }

    fn tool_end(&mut self, _tool: &str, output: &ToolOutput) {
        let mut s = self.s();
        s.tool_result(output);
        if let Some((label, _)) = &mut s.busy {
            *label = "Working".into();
        }
        s.redraw();
    }

    async fn approve(&mut self, req: &ApprovalRequest<'_>) -> Approval {
        let (tx, rx) = oneshot::channel();
        {
            let mut s = self.s();
            s.flush_live();
            let w = s.width;
            let mut lines = vec![tool_title(req.tool, req.summary, w)];
            if let Some(p) = req.preview {
                let body: Vec<String> = if looks_like_diff(p) {
                    diff(p, true, usize::MAX, "  ").lines().map(str::to_string).collect()
                } else {
                    p.lines().map(|l| paint(dim(), &format!("  {}", truncate(l, w.saturating_sub(2))))).collect()
                };
                let more = body.len().saturating_sub(MAX_PREVIEW);
                lines.extend(body.into_iter().take(MAX_PREVIEW));
                if more > 0 {
                    lines.push(paint(dim(), &format!("  … {more} more lines")));
                }
            }
            s.approval = Some(PendingApproval { lines, reply: tx });
            s.redraw();
        }
        let answer = rx.await.unwrap_or(Approval::No);
        if answer == Approval::No {
            // The loop reports the denial through tool_end; show the call first.
            self.s().print_tool_call(req.tool, req.summary);
        }
        answer
    }

    fn usage(&mut self, usage: &Usage) {
        let mut s = self.s();
        s.footer.usage.input_tokens += usage.input_tokens;
        s.footer.usage.output_tokens += usage.output_tokens;
        s.footer.usage.cached_tokens += usage.cached_tokens;
        s.footer.context_tokens = usage.input_tokens + usage.output_tokens;
        s.redraw();
    }

    fn notice(&mut self, msg: &str) {
        self.s().print_info(msg, dim());
    }

    fn take_steering(&mut self) -> Vec<Item> {
        let steer: Vec<String> = {
            let mut s = self.s();
            let (steer, rest): (Vec<_>, Vec<_>) = s.queue.drain(..).partition(|(k, _)| *k == Queued::Steer);
            s.queue = rest;
            steer.into_iter().map(|(_, t)| t).collect()
        };
        steer.into_iter().map(|t| self.deliver(t)).collect()
    }

    fn take_follow_up(&mut self) -> Option<Item> {
        let text = {
            let mut s = self.s();
            let i = s.queue.iter().position(|(k, _)| *k == Queued::FollowUp)?;
            s.queue.remove(i).1
        };
        Some(self.deliver(text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn widths_and_wrapping() {
        assert_eq!(visible_width("\x1b[1mab\x1b[0m界"), 4);
        assert_eq!(truncate("ab界c", 3), "ab");
        assert_eq!(wrap("abcdef", 4), vec!["abcd", "ef"]);
        assert_eq!(wrap("", 4), vec![""]);
        assert_eq!(tokens(950), "950");
        assert_eq!(tokens(14_000), "14k");
        assert_eq!(tokens(1_400), "1.4k");
    }

    #[test]
    fn thinking_collapses_to_one_line() {
        assert_eq!(thinking_summary("**Planning**\n\nstep one\nstep two", 80), "Planning (+2 lines of thinking)");
        assert_eq!(thinking_summary("just this", 80), "just this");
        assert!(visible_width(&thinking_summary(&"x".repeat(200), 40)) <= 40);
    }

    #[test]
    fn block_rows_fill_the_width() {
        let row = block_row("hi", TOOL_BG, 10);
        assert_eq!(visible_width(&row), 10);
        assert!(row.ends_with("\x1b[0m"));
    }
}
