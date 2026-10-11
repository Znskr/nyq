//! Inline terminal UI. The transcript is printed into the normal
//! scrollback; below it a pane stays pinned to the bottom with the streaming
//! line, the running tool group, a status line, queued messages, the input
//! (always accepting text, even while the agent works), menus, and a footer.

use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
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
use super::render::{
    diff, is_table_sep, looks_like_diff, one_line, sanitize_text, table, table_cells, truncate,
    visible_width, Markdown,
};
use super::statusline::{tilde, tokens};
use crate::agent::{Approval, ApprovalRequest, Approver, Ui};
use crate::models::ModelChoice;
use crate::session::Meta;
use crate::tools::ToolOutput;
use crate::types::{Item, StreamEvent, Usage};

const BRIGHT: Color = Color::Rgb(238, 238, 238);
const MID: Color = Color::Rgb(208, 208, 208);
const GRAY: Color = Color::Rgb(138, 138, 138);
const FAINT: Color = Color::Rgb(88, 88, 88);
const RED: Color = Color::Rgb(215, 95, 95);
const YELLOW: Color = Color::Rgb(215, 175, 95);
const MAX_PREVIEW: usize = 14;
const MENU_ROWS: usize = 8;

/// What the app loop receives from the input task.
pub enum Command {
    Submit(String),
    /// A model picked in the switcher.
    SetModel(ModelChoice),
    /// A session picked in the resume picker (its id).
    Resume(String),
    /// Shift+Tab: next reasoning effort.
    CycleEffort,
    /// A session setting changed in the settings menu (effort, fast, permission).
    Setting(crate::settings::Id, String),
    /// A mode picked in the `/permissions` menu (`ask`, `auto`, or `yolo`).
    SetPermissionMode(String),
    /// Ctrl+G: relaunch the installed upgrade on this conversation.
    ApplyUpgrade,
    Exit,
}

/// An image waiting to be sent with the next message.
pub struct Attachment {
    pub label: String,
    pub image: crate::types::Image,
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
    pub effort: Option<String>,
    pub sandbox: String,
    /// `ask`, `auto`, or `yolo`. Empty hides the segment.
    pub permission_mode: String,
    pub usage: Usage,
    pub context_tokens: u64,
    pub context_window: u64,
    /// Codex fast mode is on.
    pub fast: bool,
    pub branch: Option<String>,
    pub segments: crate::settings::StatusLine,
    /// Efforts the current model supports, for the settings menu.
    pub efforts: Vec<String>,
    pub fast_supported: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Block {
    None,
    Text,
    Reasoning,
    Tools,
    User,
    Info,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Status {
    Running,
    Ok,
    Failed,
}

struct ToolRow {
    tool: String,
    verb: String,
    target: String,
    note: String,
    detail: Vec<String>,
    status: Status,
}

struct PendingApproval {
    title: String,
    /// Sanitized preview. Wrapped to the terminal width unless `prewrapped`.
    lines: Vec<String>,
    /// Diff rows are already display width; plain text is wrapped while painting.
    prewrapped: bool,
    scroll: usize,
    /// What "always" grants; None when the call can't be scoped.
    rule: Option<String>,
    /// The raw rule "always, and save" writes to permissions.json; None hides `s`.
    save_rule: Option<String>,
    /// Who asks when it isn't the main agent, e.g. "subagent · Count files".
    origin: Option<String>,
    reply: oneshot::Sender<Approval>,
}

struct PendingQuestion {
    question: String,
    options: Vec<String>,
    selected: usize,
    /// A menu (`/setup`, `/login`): shown under this title, no typed answers.
    menu: Option<String>,
    reply: oneshot::Sender<Option<String>>,
}

/// A masked one-line prompt (API keys): never echoed, never in history.
struct PendingSecret {
    prompt: String,
    text: String,
    reply: oneshot::Sender<Option<String>>,
}

struct MenuItem {
    label: String,
    detail: String,
    tag: String,
}

/// The model switcher.
struct ModelMenu {
    choices: Vec<ModelChoice>,
    tabs: Vec<String>,
    tab: usize,
    filter: String,
    selected: usize,
    loading: bool,
    warnings: Vec<String>,
}

impl ModelMenu {
    fn visible(&self) -> Vec<&ModelChoice> {
        let tab = &self.tabs[self.tab];
        let f = self.filter.to_lowercase();
        self.choices
            .iter()
            .filter(|c| tab == "All" || &c.provider == tab)
            .filter(|c| {
                f.is_empty() || c.model.to_lowercase().contains(&f) || c.provider.contains(&f)
            })
            .collect()
    }
}

/// The session picker (`/resume`, `nyq -r`).
struct SessionMenu {
    /// Most recently active first, without the current session.
    sessions: Vec<Meta>,
    /// Ids of the sessions started in this workspace.
    local: std::collections::HashSet<String>,
    /// Show every workspace's sessions (Tab toggles).
    all: bool,
    filter: String,
    selected: usize,
}

impl SessionMenu {
    fn new(mut sessions: Vec<Meta>, cwd: &std::path::Path, current: Option<&str>) -> Self {
        sessions.retain(|m| Some(m.id.as_str()) != current);
        sessions.sort_by_key(|m| std::cmp::Reverse(m.updated));
        let local: std::collections::HashSet<String> = sessions
            .iter()
            .filter(|m| crate::session::same_cwd(&m.cwd, cwd))
            .map(|m| m.id.clone())
            .collect();
        Self {
            all: local.is_empty(),
            sessions,
            local,
            filter: String::new(),
            selected: 0,
        }
    }

    fn visible(&self) -> Vec<&Meta> {
        let f = self.filter.to_lowercase();
        self.sessions
            .iter()
            .filter(|m| self.all || self.local.contains(&m.id))
            .filter(|m| {
                f.is_empty()
                    || m.title.to_lowercase().contains(&f)
                    || m.preview.to_lowercase().contains(&f)
                    || m.cwd.to_string_lossy().to_lowercase().contains(&f)
                    || m.id.starts_with(&f)
            })
            .collect()
    }
}

/// "5m ago"-style age of `then` at `now`.
fn ago(then: chrono::DateTime<chrono::Utc>, now: chrono::DateTime<chrono::Utc>) -> String {
    let secs = (now - then).num_seconds().max(0);
    match secs {
        0..=59 => "just now".into(),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=86_399 => format!("{}h ago", secs / 3600),
        86_400..=604_799 => format!("{}d ago", secs / 86_400),
        604_800..=2_591_999 => format!("{}w ago", secs / 604_800),
        _ => then
            .with_timezone(&chrono::Local)
            .format("%Y-%m-%d")
            .to_string(),
    }
}

pub struct Screen {
    out: crate::record_tape::Output,
    width: usize,
    height: usize,
    /// Rows of the pane currently on screen and the cursor's row within it.
    pane_rows: usize,
    cursor_row: usize,
    live: String,
    live_block: Block,
    last_block: Block,
    md: Markdown,
    fence: Option<String>,
    /// Markdown table lines held back until the table ends (column widths
    /// depend on every row); a lone line with `|` is a candidate header.
    table: Vec<String>,
    busy: Option<(String, Instant)>,
    thinking_since: Option<Instant>,
    frame: usize,
    pub editor: Editor,
    pub queue: Vec<(Queued, String)>,
    approval: Option<PendingApproval>,
    question: Option<PendingQuestion>,
    secret: Option<PendingSecret>,
    tools: Vec<ToolRow>,
    menu: Option<ModelMenu>,
    sessions: Option<SessionMenu>,
    settings: Option<super::settings::SettingsMenu>,
    modes: Option<super::mode_menu::ModeMenu>,
    /// Everything shown so far, for the transcript viewer.
    pub log: super::transcript::Log,
    viewer: Option<super::transcript::Viewer>,
    /// Scrollback rows held while the viewer covers the screen.
    deferred: Vec<String>,
    /// Images for the next message.
    pub attachments: Vec<Attachment>,
    /// Ctrl+V asked for a clipboard image (read by the input task).
    clipboard_request: bool,
    slash_selected: usize,
    pub footer: Footer,
    cancel: Option<CancellationToken>,
    /// The last prompt saved to the history file (repeats are not saved again).
    last_prompt: Option<String>,
    /// A one-off hint shown in the status area until the next key.
    hint: Option<String>,
    home: PathBuf,
    /// The background auto-upgrade, when it runs in this session.
    pub upgrader: Option<Arc<crate::auto_upgrade::Upgrader>>,
}

static SCREEN: Mutex<Option<Arc<Mutex<Screen>>>> = Mutex::new(None);

/// The active screen, if the TUI is running.
pub fn active() -> Option<Arc<Mutex<Screen>>> {
    SCREEN.lock().ok()?.clone()
}

fn lock(s: &Arc<Mutex<Screen>>) -> MutexGuard<'_, Screen> {
    s.lock().unwrap_or_else(|e| e.into_inner())
}

fn fg(c: Color) -> Style {
    Style::new().fg(c)
}

fn paint(style: Style, s: &str) -> String {
    if s.is_empty() {
        String::new()
    } else {
        style.paint(s).to_string()
    }
}

/// Soft-wrap plain text into rows of at most `width` columns.
/// Cut a possibly-styled row to `width` columns, keeping escape sequences.
fn clip_width(s: &str, width: usize) -> String {
    if width == 0 || visible_width(s) <= width {
        return if width == 0 {
            String::new()
        } else {
            s.to_string()
        };
    }
    let limit = width - 1;
    let mut out = String::new();
    let mut w = 0;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            out.push(c);
            for c in chars.by_ref() {
                out.push(c);
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        let cw = c.width().unwrap_or(0);
        if w + cw > limit {
            break;
        }
        w += cw;
        out.push(c);
    }
    out.push('…');
    // A cut can land inside a styled span and drop its reset, so later rows
    // keep the last color. Put the reset back when the source used escapes.
    if s.contains('\x1b') {
        out.push_str("\x1b[0m");
    }
    out
}

/// One listing row, with terminal controls removed before it is painted.
fn list_row(line: &str, width: usize) -> String {
    let line = sanitize_text(line);
    paint(fg(MID), &truncate(&format!("  {line}"), width))
}

/// Header plus a filter that cannot grow past the terminal width.
fn fit_header(prefix: String, filter: &str, placeholder: bool, width: usize) -> String {
    let room = width.saturating_sub(visible_width(&prefix));
    let style = if placeholder { fg(FAINT) } else { fg(MID) };
    format!("{prefix}{}", paint(style, &truncate(filter, room)))
}

/// Steering texts, leaving slash commands in `queue` for the app loop.
fn is_slash_command(text: &str) -> bool {
    text.trim_start().starts_with('/')
}

fn take_steer_texts(queue: &mut Vec<(Queued, String)>) -> Vec<String> {
    let end = queue
        .iter()
        .position(|(_, t)| is_slash_command(t))
        .unwrap_or(queue.len());
    let (steer, rest): (Vec<_>, Vec<_>) =
        queue.drain(..end).partition(|(k, _)| *k == Queued::Steer);
    queue.splice(..0, rest);
    steer.into_iter().map(|(_, t)| t).collect()
}

fn wrap(s: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut rows = vec![String::new()];
    let mut w = 0;
    for c in s.chars() {
        if c == '\n' {
            rows.push(String::new());
            w = 0;
            continue;
        }
        let cw = c.width().unwrap_or(0);
        if w + cw > width && w > 0 {
            rows.push(String::new());
            w = 0;
        }
        rows.last_mut().unwrap().push(c);
        w += cw;
    }
    rows
}

fn approval_rows(a: &PendingApproval, width: usize) -> Vec<String> {
    if a.prewrapped {
        return a.lines.clone();
    }
    let width = width.saturating_sub(2).max(1);
    a.lines
        .iter()
        .flat_map(|line| wrap(line, width))
        .map(|line| paint(fg(GRAY), &format!("  {line}")))
        .collect()
}

fn rule(width: usize) -> String {
    paint(fg(FAINT), &"─".repeat(width))
}

/// `left` and `right` on one row, right-aligned when they fit.
fn spread(left: &str, right: &str, width: usize) -> String {
    let gap = width.saturating_sub(visible_width(left) + visible_width(right));
    if right.is_empty() || gap < 2 {
        left.to_string()
    } else {
        format!("{left}{}{right}", " ".repeat(gap))
    }
}

/// Key hints: bright key, gray action.
fn hints(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, a)| format!("{} {}", paint(fg(BRIGHT), k), paint(fg(GRAY), a)))
        .collect::<Vec<_>>()
        .join("     ")
}

/// A label with a moving highlight (a "shimmer").
fn shimmer(label: &str, frame: usize) -> String {
    let n = label.chars().count();
    if n == 0 {
        return String::new();
    }
    let pos = frame % (n + 6);
    label
        .chars()
        .enumerate()
        .map(|(i, c)| {
            let d = (i as isize - pos as isize).unsigned_abs();
            let color = match d {
                0 => BRIGHT,
                1 => MID,
                _ => GRAY,
            };
            paint(fg(color), &c.to_string())
        })
        .collect()
}

/// How a tool call reads in the transcript: a verb and its target.
fn describe_tool(tool: &str, summary: &str) -> (String, String) {
    let verb = match tool {
        "read_file" => "Read",
        "write_file" => "Wrote",
        "edit_file" => "Edited",
        "bash" => "Ran",
        "grep" => "Searched",
        "glob" => "Found",
        "list_dir" => "Listed",
        "view_image" => "Viewed",
        _ => "",
    };
    if verb.is_empty() {
        let name = match tool.strip_prefix("mcp__").and_then(|r| r.split_once("__")) {
            Some((server, t)) => format!("{server}.{t}"),
            None => tool.to_string(),
        };
        return (
            "Called".into(),
            format!("{name} {}", one_line(summary, 200)),
        );
    }
    (verb.into(), one_line(summary, 400))
}

fn tool_kind(tool: &str) -> &'static str {
    match tool {
        "read_file" | "grep" | "glob" | "list_dir" | "view_image" => "read",
        "write_file" | "edit_file" => "edit",
        "bash" => "command",
        _ => "tool",
    }
}

/// "+3 / -1" from a unified diff.
fn diff_stat(d: &str) -> String {
    let add = d
        .lines()
        .filter(|l| l.starts_with('+') && !l.starts_with("+++"))
        .count();
    let del = d
        .lines()
        .filter(|l| l.starts_with('-') && !l.starts_with("---"))
        .count();
    format!("+{add} / -{del}")
}

/// One collapsed line for a reasoning block.
fn thinking_summary(text: &str, secs: u64, width: usize) -> String {
    let first = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .trim_matches('*');
    let head = if secs > 0 {
        format!("• Thought for {secs}s")
    } else {
        "• Thought".to_string()
    };
    let rest = width.saturating_sub(head.chars().count() + 3);
    if first.is_empty() || rest < 8 {
        head
    } else {
        format!("{head} · {}", truncate(first, rest))
    }
}

impl Screen {
    fn new(footer: Footer, editor: Editor, home: PathBuf) -> Self {
        let (w, h) = terminal::size().unwrap_or((80, 24));
        Screen {
            out: crate::record_tape::stdout(),
            width: (w as usize).max(1),
            height: (h as usize).max(1),
            pane_rows: 0,
            cursor_row: 0,
            live: String::new(),
            live_block: Block::None,
            last_block: Block::None,
            md: Markdown::default(),
            fence: None,
            table: Vec::new(),
            busy: None,
            thinking_since: None,
            frame: 0,
            editor,
            queue: Vec::new(),
            approval: None,
            question: None,
            secret: None,
            tools: Vec::new(),
            menu: None,
            sessions: None,
            settings: None,
            modes: None,
            log: Default::default(),
            viewer: None,
            deferred: Vec::new(),
            attachments: Vec::new(),
            clipboard_request: false,
            slash_selected: 0,
            footer,
            cancel: None,
            last_prompt: None,
            hint: None,
            home,
            upgrader: None,
        }
    }

    // ------------------------------------------------------------ drawing

    fn clear_pane(&mut self) {
        if self.cursor_row > 0 {
            let _ = queue!(self.out, cursor::MoveUp(self.cursor_row as u16));
        }
        let _ = queue!(
            self.out,
            cursor::MoveToColumn(0),
            terminal::Clear(terminal::ClearType::FromCursorDown)
        );
        self.pane_rows = 0;
        self.cursor_row = 0;
    }

    fn tool_rows(&self, rows: &mut Vec<String>, live: bool) {
        let w = self.width;
        let n = self.tools.len();
        if n == 0 {
            return;
        }
        let row_text = |t: &ToolRow, lead: &str| {
            let verb_style = match t.status {
                Status::Failed => fg(RED),
                Status::Running if live => fg(MID),
                _ => fg(GRAY),
            };
            let note = if t.note.is_empty() {
                String::new()
            } else {
                format!(" {}", t.note)
            };
            let used = visible_width(lead) + t.verb.chars().count() + note.chars().count() + 1;
            let target = truncate(&t.target, w.saturating_sub(used));
            let verb = if t.status == Status::Running && live {
                shimmer(&t.verb, self.frame)
            } else {
                paint(verb_style, &t.verb)
            };
            format!(
                "{lead}{verb} {}{}",
                paint(fg(GRAY), &target),
                paint(fg(GRAY), &note)
            )
        };
        let detail = |t: &ToolRow, rows: &mut Vec<String>, bar: &str| {
            let style = if t.status == Status::Failed {
                fg(RED)
            } else {
                fg(FAINT)
            };
            for d in &t.detail {
                rows.push(format!(
                    "{}{}",
                    paint(fg(FAINT), bar),
                    paint(style, &truncate(d, w.saturating_sub(4)))
                ));
            }
        };
        if n == 1 {
            let t = &self.tools[0];
            rows.push(
                row_text(t, &format!("{} ", paint(fg(BRIGHT), "●"))).replacen(
                    &paint(fg(GRAY), &t.verb),
                    &paint(fg(BRIGHT), &t.verb),
                    1,
                ),
            );
            detail(t, rows, "  │ ");
            return;
        }
        let mut counts: Vec<(&str, usize)> = Vec::new();
        for t in &self.tools {
            let k = tool_kind(&t.tool);
            match counts.iter_mut().find(|(c, _)| *c == k) {
                Some((_, n)) => *n += 1,
                None => counts.push((k, 1)),
            }
        }
        let parts: Vec<String> = counts
            .iter()
            .map(|(k, c)| match (*k, *c) {
                ("edit", 1) => "1 edit".into(),
                ("edit", c) => format!("{c} edits"),
                ("command", 1) => "1 command".into(),
                ("command", c) => format!("{c} commands"),
                ("tool", 1) => "1 tool".into(),
                ("tool", c) => format!("{c} tools"),
                (k, c) => format!("{c} {k}"),
            })
            .collect();
        rows.push(format!(
            "{} {}",
            paint(fg(BRIGHT), "●"),
            paint(fg(GRAY), &format!("{n} tool calls · {}", parts.join(" · ")))
        ));
        for (i, t) in self.tools.iter().enumerate() {
            let last = i + 1 == n;
            rows.push(row_text(
                t,
                &paint(fg(GRAY), if last { "└ " } else { "├ " }),
            ));
            detail(t, rows, if last { "  │ " } else { "│ │ " });
        }
    }

    fn pane(&self) -> (Vec<String>, usize, usize) {
        let w = self.width;
        let mut rows = Vec::new();
        match self.live_block {
            Block::Reasoning => {}
            _ if !self.live.is_empty() || !self.table.is_empty() => {
                // A table still streaming shows laid out at the current width.
                match self.table.len() {
                    0 => {}
                    1 => rows.extend(
                        wrap(&self.table[0], w.saturating_sub(2))
                            .into_iter()
                            .map(|r| format!("  {r}")),
                    ),
                    _ => rows.extend(self.table_rows(&self.table)),
                }
                for r in wrap(&self.live, w.saturating_sub(2))
                    .into_iter()
                    .filter(|_| !self.live.is_empty())
                {
                    rows.push(format!("  {r}"));
                }
            }
            _ => {}
        }
        self.tool_rows(&mut rows, true);
        // One compact line per running subagent.
        if self.busy.is_some() {
            for line in crate::subagents::live_lines() {
                let text = truncate(&sanitize_text(&line), w.saturating_sub(5).max(1));
                rows.push(format!("  {} {}", paint(fg(FAINT), "↳"), paint(fg(GRAY), &text)));
            }
        }

        // Status: what the agent is doing.
        if let Some((label, start)) = &self.busy {
            let secs = start.elapsed().as_secs();
            let suffix = format!(" ({secs}s · esc to interrupt)");
            let label = truncate(label, w.saturating_sub(suffix.chars().count() + 4).max(1));
            let detail = if self.live_block == Block::Reasoning {
                let last = self
                    .live
                    .lines()
                    .rev()
                    .find(|l| !l.trim().is_empty())
                    .unwrap_or("")
                    .trim();
                truncate(
                    last,
                    w.saturating_sub(label.chars().count() + suffix.chars().count() + 4),
                )
            } else {
                String::new()
            };
            let mut line = format!("{} {}", paint(fg(BRIGHT), "•"), shimmer(&label, self.frame));
            line.push_str(&paint(fg(GRAY), &suffix));
            if !detail.is_empty() {
                line.push_str(&paint(fg(FAINT).italic(), &format!(" {detail}")));
            }
            if !rows.is_empty() {
                rows.push(String::new());
            }
            rows.push(line);
        }
        if let Some(h) = &self.hint {
            rows.push(paint(fg(YELLOW), h));
        }
        for (kind, text) in &self.queue {
            let tag = match kind {
                Queued::Steer => " (steer)",
                Queued::FollowUp => " (follow-up)",
            };
            let text = truncate(&one_line(text, w), w.saturating_sub(tag.len() + 3));
            rows.push(format!(
                "{} {}{}",
                paint(fg(FAINT), "┃"),
                paint(fg(GRAY), &text),
                paint(fg(FAINT), tag)
            ));
        }
        if !rows.is_empty() {
            rows.push(String::new());
        }

        if !self.attachments.is_empty() && self.approval.is_none() {
            let names: Vec<String> = self
                .attachments
                .iter()
                .enumerate()
                .map(|(i, a)| format!("[Image #{}] {}", i + 1, a.label))
                .collect();
            let text = truncate(&sanitize_text(&names.join(" · ")), w.saturating_sub(4));
            rows.push(format!(
                "{} {}",
                paint(fg(GRAY), "▣"),
                paint(fg(MID), &text)
            ));
        }

        // Input, or the approval prompt in its place.
        let (cursor_row, cursor_col);
        if let Some(a) = &self.approval {
            if let Some(origin) = &a.origin {
                rows.push(format!(
                    "{} {}",
                    paint(fg(YELLOW), "┃"),
                    paint(fg(GRAY), &truncate(origin, w.saturating_sub(3)))
                ));
            }
            rows.push(format!("{} {}", paint(fg(YELLOW), "┃"), a.title));
            cursor_row = rows.len() - 1;
            cursor_col = 2;
            rows.push(rule(w));
            rows.push(paint(fg(BRIGHT).bold(), "Approve"));
            rows.push(String::new());
            let shown = approval_rows(a, w);
            let max_scroll = shown.len().saturating_sub(MAX_PREVIEW);
            let scroll = a.scroll.min(max_scroll);
            let more = shown.len().saturating_sub(scroll + MAX_PREVIEW);
            rows.extend(shown.into_iter().skip(scroll).take(MAX_PREVIEW));
            if scroll > 0 || more > 0 {
                rows.push(paint(
                    fg(GRAY),
                    &format!("  … {scroll} above, {more} below"),
                ));
            }
            if !a.lines.is_empty() {
                rows.push(String::new());
            }
            let always = match &a.rule {
                Some(r) => format!("Always allow {r}"),
                None => "Always: can't be scoped to a rule here, approves once".into(),
            };
            let save = a
                .save_rule
                .as_ref()
                .map(|r| format!("Always, and save `{r}` to permissions.json"));
            let mut choices = vec![("y", "Yes, once"), ("a", always.as_str())];
            choices.extend(save.as_deref().map(|t| ("s", t)));
            choices.push(("n", "No, tell nyq what to do instead"));
            for (key, text) in choices {
                rows.push(format!(
                    "{}  {}",
                    paint(fg(BRIGHT), key),
                    paint(fg(MID), &truncate(text, w.saturating_sub(3)))
                ));
            }
            rows.push(rule(w));
            let mut keys = vec![("y", "Once"), ("a", "Always")];
            if save.is_some() {
                keys.push(("s", "Save"));
            }
            keys.extend([("n", "Deny"), ("↑↓", "Scroll"), ("Esc", "Interrupt")]);
            rows.push(hints(&keys));
        } else if let Some(sec) = &self.secret {
            for l in wrap(&sanitize_text(&sec.prompt), w.saturating_sub(2).max(1)) {
                rows.push(paint(fg(BRIGHT).bold(), &l));
            }
            let dots = "•".repeat(sec.text.chars().count().min(w.saturating_sub(3)));
            cursor_row = rows.len();
            cursor_col = 2 + dots.chars().count();
            rows.push(format!("{} {dots}", paint(fg(BRIGHT), "┃")));
            rows.push(rule(w));
            rows.push(hints(&[("Enter", "Save"), ("Esc", "Cancel")]));
        } else {
            let view = self.editor.render(w.saturating_sub(2).max(1));
            let top = rows.len();
            for l in &view.lines {
                rows.push(format!("{} {l}", paint(fg(BRIGHT), "┃")));
            }
            cursor_row = top + view.cursor_row;
            cursor_col = 2 + view.cursor_col;
            if let Some(q) = &self.question {
                rows.push(rule(w));
                rows.push(paint(fg(BRIGHT).bold(), q.menu.as_deref().unwrap_or("Question")));
                let question = sanitize_text(&q.question);
                for l in wrap(&question, w.saturating_sub(2).max(1)) {
                    rows.push(paint(fg(MID), &l));
                }
                rows.push(String::new());
                for (i, o) in q.options.iter().enumerate() {
                    let sel = i == q.selected;
                    let lead = if sel {
                        paint(fg(BRIGHT), "› ")
                    } else {
                        "  ".into()
                    };
                    let text = one_line(&format!("{}. {o}", i + 1), w.saturating_sub(2).max(1));
                    rows.push(format!(
                        "{lead}{}",
                        if sel {
                            paint(fg(BRIGHT).bold(), &text)
                        } else {
                            paint(fg(MID), &text)
                        }
                    ));
                }
                rows.push(rule(w));
                rows.push(if q.menu.is_some() {
                    hints(&[("↑↓", "Navigate"), ("Enter", "Choose"), ("Esc", "Close")])
                } else {
                    hints(&[("↑↓", "Navigate"), ("Enter", "Choose"), ("type", "Own answer"), ("Esc", "Skip")])
                });
            } else if !self.menu_rows(&mut rows) {
                rows.push(String::new());
                rows.push(self.footer_line());
            }
        }

        // Keep the pane on screen: drop rows above the cursor first.
        let max = self.height.max(1);
        let excess = rows.len().saturating_sub(max);
        let dropped = excess.min(cursor_row);
        rows.drain(..dropped);
        rows.truncate(max);
        for row in &mut rows {
            if visible_width(row) > w {
                *row = clip_width(row, w);
            }
        }
        (rows, cursor_row - dropped, cursor_col)
    }

    fn footer_line(&self) -> String {
        let f = &self.footer;
        let left = match super::statusline::left(f) {
            Some(text) => {
                let (effort, rest) = text.split_once(" · ").unwrap_or((&text, ""));
                format!(
                    "{} {} {}",
                    paint(fg(MID), effort),
                    paint(fg(GRAY), "·"),
                    paint(fg(GRAY), &sanitize_text(rest))
                )
            }
            None => String::new(),
        };
        if visible_width(&left) > self.width {
            return clip_width(&left, self.width);
        }
        let right = super::statusline::right(f, self.width).join(" · ");
        let mut room = self.width.saturating_sub(visible_width(&left) + 4);
        let upgrade = self.upgrade_label();
        let upgrade = if upgrade.is_empty() {
            String::new()
        } else {
            let label = truncate(&upgrade, room);
            room = room.saturating_sub(label.chars().count() + 3);
            let color = if label.starts_with("upgrading") { GRAY } else { YELLOW };
            let sep = if right.is_empty() || room == 0 { "" } else { " · " };
            format!("{}{}", paint(fg(color), &label), paint(fg(FAINT), sep))
        };
        let right = format!("{upgrade}{}", paint(fg(FAINT), &truncate(&sanitize_text(&right), room)));
        if left.is_empty() {
            return right;
        }
        spread(&left, &right, self.width)
    }

    /// The auto-upgrade status for the status line (empty: nothing to show).
    fn upgrade_label(&self) -> String {
        match &self.upgrader {
            Some(u) if crate::auto_upgrade::enabled(crate::settings::current().auto_upgrade) => u.status_label(),
            _ => String::new(),
        }
    }

    /// What Ctrl+G must know about the screen to decide whether to relaunch.
    pub fn upgrade_facts(&self) -> crate::auto_upgrade::ApplyFacts {
        let enabled = self.upgrader.is_some() && crate::auto_upgrade::enabled(crate::settings::current().auto_upgrade);
        crate::auto_upgrade::ApplyFacts {
            busy: self.is_busy(),
            queued: !self.queue.is_empty(),
            question: self.question.is_some(),
            approval: self.approval.is_some(),
            secret: self.secret.is_some(),
            session_picker: self.sessions.is_some(),
            menu: self.menu.is_some() || self.settings.is_some() || self.modes.is_some(),
            transcript: self.viewer.is_some(),
            draft: !self.editor.is_empty(),
            attachments: !self.attachments.is_empty(),
            enabled,
            state: self.upgrader.as_ref().map(|u| u.state()),
        }
    }

    /// Replace the one-off hint in the status area.
    pub fn set_hint(&mut self, hint: Option<String>) {
        self.hint = hint;
        self.redraw();
    }

    /// Ctrl+G: ask the app loop to relaunch, or say why it can't now.
    fn request_upgrade(&mut self, commands: &mpsc::UnboundedSender<Command>) {
        match crate::auto_upgrade::apply_refusal(&self.upgrade_facts()) {
            Some(notice) => self.hint = Some(notice.to_string()),
            None => {
                self.hint = Some("Reloading nyq…".into());
                let _ = commands.send(Command::ApplyUpgrade);
            }
        }
        self.redraw();
    }

    /// The slash-command or model menu under the input. Returns false if none.
    fn menu_rows(&self, rows: &mut Vec<String>) -> bool {
        let w = self.width;
        let (header, items, selected, keys): (String, Vec<MenuItem>, usize, Vec<(&str, &str)>) =
            if let Some(m) = &self.menu {
                let visible = m.visible();
                let tabs: Vec<String> = m
                    .tabs
                    .iter()
                    .enumerate()
                    .map(|(i, t)| {
                        if i == m.tab {
                            paint(fg(BRIGHT), &format!("[{t}]"))
                        } else {
                            paint(fg(GRAY), t)
                        }
                    })
                    .collect();
                let prefix = format!(
                    "{} {}  {}  ",
                    paint(fg(BRIGHT).bold(), "Models"),
                    paint(fg(GRAY), &visible.len().to_string()),
                    tabs.join(" ")
                );
                let filter_text = if m.filter.is_empty() {
                    "Type to filter"
                } else {
                    m.filter.as_str()
                };
                let header = fit_header(prefix, filter_text, m.filter.is_empty(), w);
                let current = (self.footer.provider.as_str(), self.footer.model.as_str());
                let items = visible
                    .iter()
                    .map(|c| MenuItem {
                        label: sanitize_text(&c.model),
                        detail: [
                            if (c.provider.as_str(), c.model.as_str()) == current {
                                "current".to_string()
                            } else {
                                String::new()
                            },
                            c.context_window
                                .map(|n| format!("{} ctx", tokens(n)))
                                .unwrap_or_default(),
                            if c.efforts.is_empty() {
                                String::new()
                            } else {
                                format!("effort {}", c.efforts.join("/"))
                            },
                            c.price.clone().unwrap_or_default(),
                        ]
                        .into_iter()
                        .filter(|s| !s.is_empty())
                        .collect::<Vec<_>>()
                        .join(" · "),
                        tag: c.provider.clone(),
                    })
                    .collect();
                (
                    header,
                    items,
                    m.selected,
                    vec![
                        ("↑↓", "Navigate"),
                        ("Tab", "Provider"),
                        ("Enter", "Use"),
                        ("Esc", "Close"),
                    ],
                )
            } else if let Some(m) = &self.settings {
                let s = crate::settings::current();
                let live = self.live();
                let items = m.items(&s, &live);
                let tabs: Vec<String> = crate::settings::Category::ALL
                    .iter()
                    .map(|c| {
                        if *c == m.category {
                            paint(fg(BRIGHT), &format!("[{}]", c.label()))
                        } else {
                            paint(fg(GRAY), c.label())
                        }
                    })
                    .collect();
                let prefix = format!(
                    "{} {}  {}  ",
                    paint(fg(BRIGHT).bold(), "Settings"),
                    paint(fg(GRAY), &items.len().to_string()),
                    tabs.join(" ")
                );
                let filter_text = if m.filter.is_empty() {
                    "Type to filter"
                } else {
                    m.filter.as_str()
                };
                let header = fit_header(prefix, filter_text, m.filter.is_empty(), w);
                let items = items
                    .iter()
                    .map(|spec| MenuItem {
                        label: spec.label.to_string(),
                        detail: sanitize_text(&crate::settings::value(spec.id, &s, &live)),
                        tag: spec.description.to_string(),
                    })
                    .collect();
                (
                    header,
                    items,
                    m.selected,
                    vec![
                        ("↑↓", "Navigate"),
                        ("Tab", "Category"),
                        ("Enter", "Change"),
                        ("Esc", "Close"),
                    ],
                )
            } else if let Some(m) = &self.modes {
                let visible = m.rows();
                let prefix = format!(
                    "{} {}  ",
                    paint(fg(BRIGHT).bold(), "Permissions"),
                    paint(fg(GRAY), &visible.len().to_string())
                );
                let filter_text = if m.filter.is_empty() {
                    "Type to filter"
                } else {
                    m.filter.as_str()
                };
                let header = fit_header(prefix, filter_text, m.filter.is_empty(), w);
                let current = self.footer.permission_mode.as_str();
                let items = visible
                    .iter()
                    .map(|row| MenuItem {
                        label: row.label.to_string(),
                        detail: row.detail.to_string(),
                        tag: if row.id == current {
                            "current".into()
                        } else {
                            String::new()
                        },
                    })
                    .collect();
                (
                    header,
                    items,
                    m.selected,
                    vec![("↑↓", "Navigate"), ("Enter", "Use"), ("Esc", "Close")],
                )
            } else if let Some(m) = &self.sessions {
                let visible = m.visible();
                let scope = |label: &str, on: bool| {
                    if on {
                        paint(fg(BRIGHT), &format!("[{label}]"))
                    } else {
                        paint(fg(GRAY), label)
                    }
                };
                let prefix = format!(
                    "{} {}  {} {}  ",
                    paint(fg(BRIGHT).bold(), "Sessions"),
                    paint(fg(GRAY), &visible.len().to_string()),
                    scope("This workspace", !m.all),
                    scope("All workspaces", m.all)
                );
                let filter_text = if m.filter.is_empty() {
                    "Type to filter"
                } else {
                    m.filter.as_str()
                };
                let header = fit_header(prefix, filter_text, m.filter.is_empty(), w);
                let now = chrono::Utc::now();
                let items = visible
                    .iter()
                    .map(|m| MenuItem {
                        label: one_line(&m.title, 200),
                        detail: one_line(&tilde(&m.cwd.display().to_string()), 200),
                        tag: format!(
                            "{} turn{} · {}",
                            m.turns,
                            if m.turns == 1 { "" } else { "s" },
                            ago(m.updated, now)
                        ),
                    })
                    .collect();
                (
                    header,
                    items,
                    m.selected,
                    vec![
                        ("↑↓", "Navigate"),
                        ("Tab", "Scope"),
                        ("Enter", "Resume"),
                        ("Esc", "Close"),
                    ],
                )
            } else {
                let s = self.editor.suggestions();
                if s.is_empty() || self.approval.is_some() {
                    return false;
                }
                let header = format!(
                    "{} {}",
                    paint(fg(BRIGHT).bold(), "Commands"),
                    paint(fg(GRAY), &s.len().to_string())
                );
                let items = s
                    .into_iter()
                    .map(|(c, d)| MenuItem {
                        label: c,
                        detail: d,
                        tag: String::new(),
                    })
                    .collect();
                (
                    header,
                    items,
                    self.slash_selected,
                    vec![
                        ("↑↓", "Navigate"),
                        ("Enter", "Use"),
                        ("Tab", "Complete"),
                        ("Esc", "Close"),
                    ],
                )
            };
        rows.push(rule(w));
        rows.push(header);
        rows.push(String::new());
        if let Some(m) = &self.menu {
            if m.loading {
                rows.push(paint(fg(GRAY), "  Loading models…"));
            } else if items.is_empty() {
                rows.push(paint(fg(GRAY), "  No models found."));
            }
            for warn in &m.warnings {
                rows.push(paint(
                    fg(YELLOW),
                    &format!("  {}", one_line(warn, w.saturating_sub(2).max(1))),
                ));
            }
        }
        if self.settings.is_some() && items.is_empty() {
            rows.push(paint(fg(GRAY), "  No matching settings."));
        }
        if self.modes.is_some() && items.is_empty() {
            rows.push(paint(fg(GRAY), "  No matching modes."));
        }
        if self.sessions.is_some() && items.is_empty() {
            rows.push(paint(fg(GRAY), "  No saved sessions found."));
        }
        let selected = selected.min(items.len().saturating_sub(1));
        let start = selected.saturating_sub(MENU_ROWS - 1);
        let label_w = items
            .iter()
            .map(|i| i.label.chars().count())
            .max()
            .unwrap_or(0)
            .min(32)
            + 3;
        for (i, item) in items.iter().enumerate().skip(start).take(MENU_ROWS) {
            let sel = i == selected;
            let lead = if sel {
                paint(fg(BRIGHT), "› ")
            } else {
                "  ".into()
            };
            // Rows without a detail column give the label all the room left of the tag.
            let tag_w = visible_width(&item.tag);
            let label_w = if item.detail.is_empty() {
                w.saturating_sub(tag_w + 6).max(1)
            } else {
                label_w.max(1)
            };
            let label = truncate(&item.label, label_w - 1);
            let label = if item.detail.is_empty() {
                label
            } else {
                format!("{label:<label_w$}")
            };
            let label = if sel {
                paint(fg(BRIGHT).bold(), &label)
            } else {
                paint(fg(MID), &label)
            };
            let room = w.saturating_sub(label_w + 4 + tag_w);
            let left = format!(
                "{lead}{label}{}",
                paint(fg(GRAY), &truncate(&item.detail, room))
            );
            rows.push(spread(&left, &paint(fg(GRAY), &item.tag), w));
        }
        rows.push(rule(w));
        let range = if items.is_empty() {
            String::new()
        } else {
            format!("{}–{}", start + 1, (start + MENU_ROWS).min(items.len()))
        };
        rows.push(spread(&hints(&keys), &paint(fg(GRAY), &range), w));
        true
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
        let _ = queue!(
            self.out,
            cursor::MoveToColumn(col as u16),
            terminal::EndSynchronizedUpdate
        );
        let _ = self.out.flush();
        self.pane_rows = rows.len();
        self.cursor_row = row;
    }

    pub fn redraw(&mut self) {
        if self.viewer.is_some() {
            // The viewer covers the screen; it follows new output.
            return self.draw_viewer(false);
        }
        self.clear_pane();
        self.draw_pane();
    }

    /// Print finished rows into the scrollback above the pane.
    fn emit(&mut self, rows: &[String]) {
        if self.viewer.is_some() {
            self.deferred.extend_from_slice(rows);
            return self.draw_viewer(false);
        }
        self.clear_pane();
        for r in rows {
            let _ = self.out.write_all(r.as_bytes());
            let _ = self.out.write_all(b"\r\n");
        }
        self.draw_pane();
    }

    /// Start a new block, separated from the previous one by a blank line.
    fn begin(&mut self, block: Block) -> Vec<String> {
        let gap =
            self.last_block != Block::None && (self.last_block != block || block == Block::User);
        self.last_block = block;
        if gap {
            vec![String::new()]
        } else {
            Vec::new()
        }
    }

    // ------------------------------------------------------------ content

    pub fn print_info(&mut self, text: &str, style: Style) {
        let text = sanitize_text(text);
        self.log.info(&super::render::strip_ansi(&text));
        self.print_styled_info(&text, style);
    }

    pub(super) fn print_styled_info(&mut self, text: &str, style: Style) {
        self.flush_all();
        let mut rows = self.begin(Block::Info);
        rows.extend(text.lines().map(|l| paint(style, l)));
        self.emit(&rows);
    }

    /// Rows that should each stay on one screen line (listings).
    pub fn print_list(&mut self, lines: &[String]) {
        self.log.info(&sanitize_text(&lines.join("\n")));
        let w = self.width;
        let rows: Vec<String> = lines.iter().map(|l| list_row(l, w)).collect();
        self.flush_all();
        let mut out = self.begin(Block::Info);
        out.extend(rows);
        self.emit(&out);
    }

    pub fn print_error(&mut self, text: &str) {
        let text = sanitize_text(text);
        self.log.info(&format!("error: {text}"));
        self.flush_all();
        let mut rows = self.begin(Block::Info);
        for (i, l) in text.lines().enumerate() {
            let lead = if i == 0 {
                paint(fg(RED), "● ")
            } else {
                "  ".into()
            };
            rows.push(format!("{lead}{}", paint(fg(MID), l)));
        }
        self.emit(&rows);
    }

    pub fn print_user(&mut self, text: &str) {
        let text = sanitize_text(text);
        self.log.user(&text);
        self.flush_all();
        let w = self.width;
        let mut rows = self.begin(Block::User);
        for line in text.trim().lines() {
            for r in wrap(line, w.saturating_sub(2)) {
                rows.push(format!(
                    "{} {}",
                    paint(fg(BRIGHT), "┃"),
                    paint(Style::new().bold(), &r)
                ));
            }
        }
        self.emit(&rows);
    }

    pub fn print_markdown(&mut self, text: &str) {
        let text = sanitize_text(text);
        self.log.text(&text, false);
        self.log.end_message();
        self.flush_all();
        let mut rows = self.begin(Block::Text);
        self.md = Markdown::default();
        self.fence = None;
        self.table.clear();
        for l in text.trim().lines() {
            self.md_line(l, &mut rows);
        }
        self.flush_table(&mut rows);
        self.emit(&rows);
    }

    /// A finished tool call (resumed transcripts and `!` commands).
    pub fn print_tool(&mut self, tool: &str, summary: &str, output: Option<&ToolOutput>) {
        self.flush_all();
        self.tool_start(tool, summary);
        match output {
            Some(o) => self.tool_end(o),
            None => {
                if let Some(t) = self.tools.last_mut() {
                    t.status = Status::Ok;
                }
            }
        }
    }

    /// Feed one finished markdown line into `out`; table lines are held back
    /// until the table ends.
    fn md_line(&mut self, line: &str, out: &mut Vec<String>) {
        if self.fence.is_none() {
            match self.table.len() {
                1 if is_table_sep(line)
                    && table_cells(line).len() == table_cells(&self.table[0]).len() =>
                {
                    return self.table.push(line.into())
                }
                n if n >= 2 && line.contains('|') => return self.table.push(line.into()),
                _ => self.flush_table(out),
            }
            let t = line.trim_start();
            if line.contains('|') && !t.starts_with("```") && !t.starts_with("~~~") {
                return self.table.push(line.into());
            }
        }
        out.push(self.render_line(line));
    }

    /// Emit held-back table lines: laid out if they form a table, else as text.
    fn flush_table(&mut self, out: &mut Vec<String>) {
        let lines = std::mem::take(&mut self.table);
        if lines.len() >= 2 {
            out.extend(self.table_rows(&lines));
        } else {
            for l in lines {
                out.push(self.render_line(&l));
            }
        }
    }

    fn table_rows(&self, lines: &[String]) -> Vec<String> {
        table(lines, self.width.saturating_sub(2), true)
            .into_iter()
            .map(|r| format!("  {r}"))
            .collect()
    }

    /// Markdown line, indented, with framed code blocks.
    fn render_line(&mut self, line: &str) -> String {
        let t = line.trim_start();
        if let Some(lang) = t.strip_prefix("```").or_else(|| t.strip_prefix("~~~")) {
            return if self.fence.take().is_some() {
                format!("  {}", paint(fg(FAINT), &"─".repeat(34)))
            } else {
                let lang = if lang.trim().is_empty() {
                    "code"
                } else {
                    lang.trim()
                };
                self.fence = Some(lang.to_string());
                format!(
                    "  {} {} {}",
                    paint(fg(FAINT), "─"),
                    paint(fg(GRAY), lang),
                    paint(fg(FAINT), &"─".repeat(32usize.saturating_sub(lang.len())))
                )
            };
        }
        if self.fence.is_some() {
            return format!("  {}", paint(fg(MID), line));
        }
        format!("  {}", self.md.line(line, true))
    }

    fn stream(&mut self, block: Block, text: &str) {
        let text = sanitize_text(text);
        self.log.text(&text, block == Block::Reasoning);
        if self.live_block != block {
            self.flush_live();
            self.flush_tools();
            self.live_block = block;
            if block == Block::Reasoning {
                self.thinking_since = Some(Instant::now());
                self.set_label("Thinking");
            } else {
                self.set_label("Working");
            }
        }
        self.live.push_str(&text);
        if block == Block::Reasoning {
            self.redraw();
            return;
        }
        if let Some(i) = self.live.rfind('\n') {
            let done: String = self.live.drain(..=i).collect();
            let mut rows = self.begin(block);
            for line in done.trim_end_matches('\n').split('\n') {
                self.md_line(line, &mut rows);
            }
            self.emit(&rows);
        } else {
            self.redraw();
        }
    }

    fn flush_live(&mut self) {
        let block = self.live_block;
        self.live_block = Block::None;
        if self.live.is_empty() && self.table.is_empty() {
            return;
        }
        let text = std::mem::take(&mut self.live);
        let mut rows = self.begin(block);
        if block == Block::Reasoning {
            let secs = self
                .thinking_since
                .take()
                .map_or(0, |t| t.elapsed().as_secs());
            rows.push(paint(
                fg(GRAY).italic(),
                &thinking_summary(&text, secs, self.width),
            ));
        } else {
            if !text.is_empty() {
                self.md_line(&text, &mut rows);
            }
            self.flush_table(&mut rows);
        }
        self.emit(&rows);
    }

    /// Commit the running tool group to the scrollback.
    fn flush_tools(&mut self) {
        if self.tools.is_empty() {
            return;
        }
        let mut rows = self.begin(Block::Tools);
        self.tool_rows(&mut rows, false);
        self.tools.clear();
        self.emit(&rows);
    }

    fn flush_all(&mut self) {
        self.flush_live();
        self.flush_tools();
    }

    fn tool_start(&mut self, tool: &str, summary: &str) {
        self.flush_live();
        self.log.tool_start(tool, summary);
        let (verb, target) = describe_tool(tool, summary);
        self.tools.push(ToolRow {
            tool: tool.into(),
            verb,
            target,
            note: String::new(),
            detail: Vec::new(),
            status: Status::Running,
        });
        self.set_label(&format!("Running {}", tool.replace('_', " ")));
        self.redraw();
    }

    fn tool_end(&mut self, output: &ToolOutput) {
        self.log
            .tool_end(&output.content, output.display.as_deref(), output.is_error);
        let Some(t) = self
            .tools
            .iter_mut()
            .rev()
            .find(|t| t.status == Status::Running)
        else {
            return;
        };
        t.status = if output.is_error {
            Status::Failed
        } else {
            Status::Ok
        };
        if let Some(d) = output.display.as_deref().filter(|d| looks_like_diff(d)) {
            t.note = diff_stat(d);
        }
        let lines: Vec<&str> = output
            .content
            .trim_end()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .collect();
        if output.is_error {
            t.detail = lines.iter().take(3).map(|l| sanitize_text(l)).collect();
        } else if t.tool == "bash" && !lines.is_empty() {
            let skip = lines.len().saturating_sub(3);
            if skip > 0 {
                t.detail.push(format!("… {skip} lines more"));
            }
            t.detail
                .extend(lines[skip..].iter().map(|l| sanitize_text(l)));
        }
        self.set_label("Working");
        self.redraw();
    }

    fn set_label(&mut self, label: &str) {
        if let Some((l, _)) = &mut self.busy {
            *l = one_line(label, 120);
        }
    }

    // ------------------------------------------------------------ state

    pub fn set_busy(&mut self, label: Option<&str>, cancel: Option<CancellationToken>) {
        if label.is_none() {
            self.flush_all();
            self.approval.take();
            self.question.take();
        }
        self.busy = label.map(|l| (l.to_string(), Instant::now()));
        self.cancel = cancel;
        self.redraw();
    }

    /// End a non-agent busy period (`!cmd`, `/compact`, `/mcp login`): text
    /// typed meanwhile goes back to the input instead of waiting in a queue
    /// that only agent turns drain.
    pub fn idle(&mut self) {
        self.set_busy(None, None);
        self.dequeue();
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

    /// Copy text to the clipboard: a native tool when one is available, and
    /// always the OSC 52 terminal escape (works over SSH in most terminals).
    pub fn copy_osc52(&mut self, text: &str) {
        use base64::Engine;
        let osc = format!(
            "\x1b]52;c;{}\x07",
            base64::engine::general_purpose::STANDARD.encode(text)
        );
        let _ = self.out.write_all(osc.as_bytes());
        let _ = self.out.flush();
    }

    /// Open the model switcher; `models` arrive later via `set_models`.
    pub fn open_models(&mut self) {
        self.sessions = None;
        self.settings = None;
        self.modes = None;
        self.menu = Some(ModelMenu {
            choices: Vec::new(),
            tabs: vec!["All".into()],
            tab: 0,
            filter: String::new(),
            selected: 0,
            loading: true,
            warnings: Vec::new(),
        });
        self.redraw();
    }

    pub fn set_models(&mut self, choices: Vec<ModelChoice>, warnings: Vec<String>) {
        let Some(m) = &mut self.menu else { return };
        let mut tabs = vec!["All".to_string()];
        for c in &choices {
            if !tabs.contains(&c.provider) {
                tabs.push(c.provider.clone());
            }
        }
        m.selected = choices
            .iter()
            .position(|c| c.provider == self.footer.provider && c.model == self.footer.model)
            .unwrap_or(0);
        m.choices = choices;
        m.tabs = tabs;
        m.warnings = warnings;
        m.loading = false;
        self.redraw();
    }

    /// Open the session picker over `all` (as from `session::list`).
    pub fn open_sessions(&mut self, all: Vec<Meta>, cwd: &std::path::Path, current: Option<&str>) {
        self.menu = None;
        self.settings = None;
        self.modes = None;
        self.sessions = Some(SessionMenu::new(all, cwd, current));
        self.redraw();
    }

    fn on_sessions_key(&mut self, key: KeyEvent, commands: &mpsc::UnboundedSender<Command>) {
        let Some(m) = &mut self.sessions else { return };
        let count = m.visible().len();
        match key.code {
            KeyCode::Esc => self.sessions = None,
            KeyCode::Char('c')
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                self.sessions = None
            }
            KeyCode::Up => {
                m.selected = m.selected.checked_sub(1).unwrap_or(count.saturating_sub(1))
            }
            KeyCode::Down => {
                m.selected = if count == 0 {
                    0
                } else {
                    (m.selected + 1) % count
                }
            }
            KeyCode::Tab | KeyCode::BackTab => {
                m.all = !m.all;
                m.selected = 0;
            }
            KeyCode::Backspace => {
                m.filter.pop();
                m.selected = 0;
            }
            KeyCode::Enter => {
                if let Some(id) = m.visible().get(m.selected).map(|m| m.id.clone()) {
                    let _ = commands.send(Command::Resume(id));
                    self.sessions = None;
                }
            }
            KeyCode::Char(ch) if text_key(key, ch) => {
                m.filter.push(ch);
                m.selected = 0;
            }
            _ => {}
        }
        self.redraw();
    }

    /// Session state for the settings menu.
    fn live(&self) -> crate::settings::Live {
        let f = &self.footer;
        crate::settings::Live {
            model: if f.provider.is_empty() {
                f.model.clone()
            } else {
                format!("{} · {}", f.provider, f.model)
            },
            effort: f.effort.clone(),
            efforts: f.efforts.clone(),
            fast: f.fast,
            fast_supported: f.fast_supported,
            permission_mode: if f.permission_mode.is_empty() {
                "ask".into()
            } else {
                f.permission_mode.clone()
            },
        }
    }

    /// Open the settings menu on `category`.
    pub fn open_settings(&mut self, category: crate::settings::Category) {
        self.menu = None;
        self.sessions = None;
        self.modes = None;
        self.settings = Some(super::settings::SettingsMenu::new(category));
        self.redraw();
    }

    /// Open the permission-mode switcher.
    pub fn open_permission_modes(&mut self) {
        self.menu = None;
        self.sessions = None;
        self.settings = None;
        self.modes = Some(super::mode_menu::ModeMenu::new());
        self.redraw();
    }

    fn on_modes_key(&mut self, key: KeyEvent, commands: &mpsc::UnboundedSender<Command>) {
        use super::mode_menu::Action;
        let Some(m) = &mut self.modes else { return };
        match m.on_key(key) {
            Action::None => {}
            Action::Close => self.modes = None,
            Action::Select(id) => {
                self.modes = None;
                if id == "rules" {
                    let _ = commands.send(Command::Submit("/permissions rules".into()));
                } else {
                    if self.is_busy() {
                        self.hint = Some("Applies when the current task finishes.".into());
                    }
                    self.footer.permission_mode = id.to_string();
                    let _ = commands.send(Command::SetPermissionMode(id.to_string()));
                }
            }
        }
        self.redraw();
    }

    fn on_settings_key(&mut self, key: KeyEvent, commands: &mpsc::UnboundedSender<Command>) {
        use super::settings::MenuAction;
        use crate::settings::Id;
        let current = crate::settings::current();
        let live = self.live();
        let Some(m) = &mut self.settings else { return };
        match m.on_key(key, &current, &live) {
            MenuAction::None => {}
            MenuAction::Close => self.settings = None,
            MenuAction::OpenModels => {
                self.settings = None;
                let _ = commands.send(Command::Submit("/model".into()));
            }
            MenuAction::Change(id, value) => {
                let mut next = current.clone();
                if crate::settings::apply(id, &value, &mut next) {
                    match crate::settings::update(&self.home, |s| *s = next.clone()) {
                        Ok(saved) => {
                            self.footer.segments = saved.statusline;
                            crate::agent::set_auto_compact_percent(saved.auto_compact);
                        }
                        Err(e) => self.hint = Some(format!("Could not save settings: {e:#}")),
                    }
                    if id == Id::Sandbox {
                        self.hint = Some("The default sandbox applies from the next start.".into());
                    }
                    if id == Id::AutoUpgrade {
                        if value == "off" {
                            if let Some(u) = &self.upgrader {
                                u.stop();
                            }
                        } else {
                            self.hint = Some("Auto-upgrade starts with the next session.".into());
                        }
                    }
                } else {
                    // Session settings belong to the app loop; show them at once.
                    match id {
                        Id::Effort => {
                            self.footer.effort = Some(value.clone()).filter(|v| v != "auto")
                        }
                        Id::Fast => self.footer.fast = value == "on",
                        Id::Permission => self.footer.permission_mode = value.clone(),
                        _ => {}
                    }
                    if self.is_busy() {
                        self.hint = Some("Applies when the current task finishes.".into());
                    }
                    let _ = commands.send(Command::Setting(id, value));
                }
            }
        }
        self.redraw();
    }

    /// Add an image to the next message.
    pub fn attach(&mut self, image: crate::types::Image, label: &str) {
        let label = sanitize_text(label);
        self.attachments.push(Attachment { label, image });
        self.hint = Some(format!(
            "Attached [Image #{}] · /images lists or removes attachments",
            self.attachments.len()
        ));
        self.redraw();
    }

    // ------------------------------------------------------- transcript viewer

    pub fn open_transcript(&mut self) {
        if self.viewer.is_some() {
            return;
        }
        // Commit what is on screen so the viewer and the scrollback agree.
        self.clear_pane();
        let _ = self.out.flush();
        // Set before entering so a panic in the viewer still leaves the alt screen.
        TRANSCRIPT_ALT_SCREEN.store(true, Ordering::SeqCst);
        let _ = execute!(self.out, terminal::EnterAlternateScreen, cursor::Hide);
        self.viewer = Some(super::transcript::Viewer::new(
            &self.log,
            self.width,
            self.height,
        ));
        self.draw_viewer(true);
    }

    /// Leave the viewer and restore the inline UI with anything printed meanwhile.
    pub fn close_transcript(&mut self) {
        if self.viewer.take().is_none() {
            return;
        }
        if TRANSCRIPT_ALT_SCREEN.swap(false, Ordering::SeqCst) {
            let _ = execute!(self.out, terminal::LeaveAlternateScreen, cursor::Show);
        } else {
            let _ = execute!(self.out, cursor::Show);
        }
        self.pane_rows = 0;
        self.cursor_row = 0;
        let rows = std::mem::take(&mut self.deferred);
        if rows.is_empty() {
            self.draw_pane();
        } else {
            self.emit(&rows);
        }
    }

    fn draw_viewer(&mut self, force: bool) {
        let Some(v) = &mut self.viewer else { return };
        if !v.refresh(&self.log, self.width, self.height) && !force {
            return;
        }
        let frame = v.frame();
        let _ = queue!(
            self.out,
            terminal::BeginSynchronizedUpdate,
            cursor::MoveTo(0, 0)
        );
        for (i, row) in frame.iter().enumerate() {
            let _ = queue!(
                self.out,
                cursor::MoveTo(0, i as u16),
                terminal::Clear(terminal::ClearType::CurrentLine)
            );
            let _ = self.out.write_all(row.as_bytes());
        }
        let _ = queue!(self.out, terminal::EndSynchronizedUpdate);
        let _ = self.out.flush();
    }

    fn on_viewer_key(&mut self, key: KeyEvent) {
        let Some(v) = &mut self.viewer else { return };
        match v.on_key(key) {
            super::transcript::Action::Close => self.close_transcript(),
            super::transcript::Action::Stay => self.draw_viewer(true),
        }
    }

    fn remember(&mut self, text: &str) {
        self.editor.push_history(text);
        let saving = crate::settings::current().prompt_history && !self.home.as_os_str().is_empty();
        if saving && self.last_prompt.as_deref() != Some(text) {
            if let Err(e) = super::history::append(&self.home.join("history"), text, &self.footer.cwd) {
                self.hint = Some(format!("could not save prompt history: {e}"));
            }
        }
        self.last_prompt = Some(text.to_string());
    }

    fn on_secret_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL) && !key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Enter => {
                if let Some(s) = self.secret.take() {
                    let _ = s.reply.send(Some(s.text));
                }
            }
            KeyCode::Esc | KeyCode::Char('c' | 'd') if key.code == KeyCode::Esc || ctrl => {
                if let Some(s) = self.secret.take() {
                    let _ = s.reply.send(None);
                }
            }
            KeyCode::Char('u') if ctrl => self.secret.iter_mut().for_each(|s| s.text.clear()),
            KeyCode::Backspace => {
                self.secret.iter_mut().for_each(|s| {
                    s.text.pop();
                });
            }
            KeyCode::Char(ch) if text_key(key, ch) => self.secret.iter_mut().for_each(|s| s.text.push(ch)),
            _ => {}
        }
        self.redraw();
    }

    fn on_menu_key(&mut self, key: KeyEvent, commands: &mpsc::UnboundedSender<Command>) {
        let Some(m) = &mut self.menu else { return };
        let count = m.visible().len();
        match key.code {
            KeyCode::Esc => self.menu = None,
            KeyCode::Up => {
                m.selected = m.selected.checked_sub(1).unwrap_or(count.saturating_sub(1))
            }
            KeyCode::Down => {
                m.selected = if count == 0 {
                    0
                } else {
                    (m.selected + 1) % count
                }
            }
            KeyCode::Tab => {
                m.tab = (m.tab + 1) % m.tabs.len();
                m.selected = 0;
            }
            KeyCode::BackTab => {
                m.tab = (m.tab + m.tabs.len() - 1) % m.tabs.len();
                m.selected = 0;
            }
            KeyCode::Backspace => {
                m.filter.pop();
                m.selected = 0;
            }
            KeyCode::Enter => {
                if let Some(c) = m.visible().get(m.selected).map(|c| (*c).clone()) {
                    let _ = commands.send(Command::SetModel(c));
                    self.menu = None;
                }
            }
            KeyCode::Char('c')
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                self.menu = None
            }
            KeyCode::Char(ch) if text_key(key, ch) => {
                m.filter.push(ch);
                m.selected = 0;
            }
            _ => {}
        }
        self.redraw();
    }

    fn on_key(&mut self, key: KeyEvent, commands: &mpsc::UnboundedSender<Command>) {
        if key.code == KeyCode::Char('g')
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && !key.modifiers.contains(KeyModifiers::ALT)
        {
            return self.request_upgrade(commands);
        }
        if let Some(mut a) = self.approval.take() {
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL)
                && !key.modifiers.contains(KeyModifiers::ALT);
            if key.code == KeyCode::Esc
                || (ctrl && matches!(key.code, KeyCode::Char('d') | KeyCode::Char('c')))
            {
                let _ = a.reply.send(Approval::No);
                if let Some(c) = &self.cancel {
                    c.cancel();
                }
                if ctrl && key.code == KeyCode::Char('d') {
                    let _ = commands.send(Command::Exit);
                }
                self.redraw();
                return;
            }
            if matches!(key.code, KeyCode::Up | KeyCode::Down) {
                if key.code == KeyCode::Up {
                    a.scroll = a.scroll.saturating_sub(1);
                } else {
                    a.scroll = a.scroll.saturating_add(1);
                }
                self.approval = Some(a);
                self.redraw();
                return;
            }
            let answer = match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => Some(Approval::Yes),
                KeyCode::Char('a') | KeyCode::Char('A') => Some(Approval::Always),
                KeyCode::Char('s') | KeyCode::Char('S') if a.save_rule.is_some() => {
                    Some(Approval::AlwaysSave)
                }
                KeyCode::Char('n') | KeyCode::Char('N') => Some(Approval::No),
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
        if self.secret.is_some() {
            return self.on_secret_key(key);
        }
        if self.viewer.is_some() {
            return self.on_viewer_key(key);
        }
        if self.menu.is_some() {
            return self.on_menu_key(key, commands);
        }
        if self.settings.is_some() {
            return self.on_settings_key(key, commands);
        }
        if self.modes.is_some() {
            return self.on_modes_key(key, commands);
        }
        if self.sessions.is_some() {
            return self.on_sessions_key(key, commands);
        }
        if self.question.is_some() {
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL)
                && !key.modifiers.contains(KeyModifiers::ALT);
            let interrupt = key.code == KeyCode::Esc
                || (ctrl && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('d')));
            if interrupt {
                if let Some(q) = self.question.take() {
                    let _ = q.reply.send(None);
                }
                if let Some(c) = &self.cancel {
                    c.cancel();
                }
                if ctrl && key.code == KeyCode::Char('d') {
                    let _ = commands.send(Command::Exit);
                }
                return self.redraw();
            }
        }
        if let Some(q) = &mut self.question {
            let n = q.options.len().max(1);
            let arrows = self.editor.is_empty() || q.menu.is_some();
            match key.code {
                KeyCode::Up if arrows => q.selected = q.selected.checked_sub(1).unwrap_or(n - 1),
                KeyCode::Down if arrows => q.selected = (q.selected + 1) % n,
                KeyCode::Char(d @ '1'..='9') if arrows && (d as usize - '1' as usize) < q.options.len() => {
                    q.selected = d as usize - '1' as usize;
                }
                KeyCode::Enter if key.modifiers.is_empty() => {
                    let q = self.question.take().unwrap();
                    let typed = if q.menu.is_some() { String::new() } else { self.editor.text() };
                    let answer = if typed.trim().is_empty() { q.options.get(q.selected).cloned() } else { Some(typed) };
                    if q.menu.is_none() {
                        self.editor.set_text("");
                    }
                    let _ = q.reply.send(answer);
                }
                _ if q.menu.is_some() => {}
                _ => {
                    self.editor.handle_key(key, true);
                }
            }
            return self.redraw();
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL)
            && !key.modifiers.contains(KeyModifiers::ALT);
        if ctrl && key.code == KeyCode::Char('o') {
            return self.open_transcript();
        }
        if ctrl && key.code == KeyCode::Char('v') {
            self.clipboard_request = true;
            self.hint = Some("Reading the clipboard…".into());
            return self.redraw();
        }
        if ctrl && key.code == KeyCode::Char('l') {
            let _ = commands.send(Command::Submit("/model".into()));
            return;
        }
        if key.code == KeyCode::BackTab {
            let _ = commands.send(Command::CycleEffort);
            return;
        }
        // The slash menu takes arrows and Enter while it is showing.
        let suggestions = self.editor.suggestions();
        if !suggestions.is_empty() {
            match key.code {
                KeyCode::Up => {
                    self.slash_selected = self
                        .slash_selected
                        .checked_sub(1)
                        .unwrap_or(suggestions.len() - 1);
                    return self.redraw();
                }
                KeyCode::Down => {
                    self.slash_selected = (self.slash_selected + 1) % suggestions.len();
                    return self.redraw();
                }
                KeyCode::Enter if key.modifiers.is_empty() => {
                    let cmd = suggestions[self.slash_selected.min(suggestions.len() - 1)]
                        .0
                        .clone();
                    self.editor.set_text("");
                    self.slash_selected = 0;
                    self.remember(&cmd);
                    if self.is_busy() {
                        self.hint = Some(format!("{cmd} runs when the current task finishes"));
                        self.queue.push((Queued::FollowUp, cmd));
                    } else {
                        let _ = commands.send(Command::Submit(cmd));
                    }
                    return self.redraw();
                }
                KeyCode::Tab => {
                    let cmd = suggestions[self.slash_selected.min(suggestions.len() - 1)]
                        .0
                        .clone();
                    self.editor.set_text(&format!("{cmd} "));
                    self.slash_selected = 0;
                    return self.redraw();
                }
                KeyCode::Esc => {
                    self.editor.set_text("");
                    self.slash_selected = 0;
                    return self.redraw();
                }
                _ => {}
            }
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
                    self.hint = Some("Press ctrl+c again to exit".into());
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
        if let Some(status) = self.editor.search_status() {
            self.hint = Some(status);
        }
        self.slash_selected = self
            .slash_selected
            .min(self.editor.suggestions().len().saturating_sub(1));
        self.redraw();
    }
}

fn text_key(key: KeyEvent, ch: char) -> bool {
    let mods = key.modifiers & (KeyModifiers::CONTROL | KeyModifiers::ALT);
    !ch.is_control() && (mods.is_empty() || mods == (KeyModifiers::CONTROL | KeyModifiers::ALT))
}

fn clipboard_bytes(text: &str, windows_clip: bool) -> Vec<u8> {
    if windows_clip {
        std::iter::once(0xfeffu16)
            .chain(text.encode_utf16())
            .flat_map(u16::to_le_bytes)
            .collect()
    } else {
        text.as_bytes().to_vec()
    }
}

/// Copy with a native clipboard tool, if one is installed. Blocking; call it
/// without holding the screen lock. Returns the tool used.
pub fn copy_native(text: &str) -> Option<&'static str> {
    let tools: &[(&'static str, &[&str])] = if cfg!(target_os = "macos") {
        &[("pbcopy", &[])]
    } else if cfg!(windows) {
        &[("clip.exe", &[])]
    } else if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        &[("wl-copy", &[]), ("xclip", &["-selection", "clipboard"])]
    } else {
        &[
            ("xclip", &["-selection", "clipboard"]),
            ("xsel", &["--clipboard", "--input"]),
        ]
    };
    for (cmd, args) in tools {
        let Ok(mut child) = std::process::Command::new(cmd)
            .args(*args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        else {
            continue;
        };
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(&clipboard_bytes(text, *cmd == "clip.exe"));
        }
        if child.wait().is_ok_and(|s| s.success()) {
            return Some(cmd);
        }
    }
    None
}

#[cfg(any(windows, test))]
/// Windows has no bracketed paste: a paste arrives as a burst of key events.
/// Enter or Tab inside a burst (several keys under 10ms apart) is text, not
/// a command. Requiring a streak keeps a real Enter after a brief UI stall.
#[cfg_attr(not(windows), allow(dead_code))]
fn paste_key(
    key: KeyEvent,
    previous: Option<Instant>,
    now: Instant,
    streak: usize,
) -> Option<&'static str> {
    let fast = previous.is_some_and(|t| now.duration_since(t) < Duration::from_millis(10));
    if !fast || streak < 3 || !(key.modifiers - KeyModifiers::CONTROL).is_empty() {
        return None;
    }
    match key.code {
        // A pasted bare LF may arrive as Ctrl+Enter.
        KeyCode::Enter => Some("\n"),
        KeyCode::Tab if key.modifiers.is_empty() => Some("\t"),
        _ => None,
    }
}

// ---------------------------------------------------------------- lifecycle

/// Owns the terminal mode for the interactive session; restores it on drop.
pub struct Tui {
    pub screen: Arc<Mutex<Screen>>,
    pub commands: mpsc::UnboundedReceiver<Command>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    enhanced: bool,
}

impl Tui {
    pub fn start(
        footer: Footer,
        home: PathBuf,
        commands: Vec<(String, String)>,
    ) -> io::Result<Self> {
        let history = super::history::load(&home.join("history"), &footer.cwd);
        let screen = Arc::new(Mutex::new(Screen::new(
            footer,
            Editor::new(history, commands),
            home,
        )));
        terminal::enable_raw_mode()?;
        let mut setup = TerminalSetup::arm();
        let enhanced = terminal::supports_keyboard_enhancement().unwrap_or(false);
        let mut out = crate::record_tape::stdout();
        execute!(out, crossterm::event::EnableBracketedPaste)?;
        setup.bracketed = true;
        if enhanced {
            execute!(
                out,
                PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
            )?;
            setup.enhanced = true;
            KEYBOARD_ENHANCED.store(true, Ordering::SeqCst);
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
                #[cfg(windows)]
                let (mut previous, mut streak) = (None::<Instant>, 0usize);
                while let Some(Ok(ev)) = events.next().await {
                    let mut s = lock(&screen);
                    match ev {
                        Event::Key(k) if k.kind != KeyEventKind::Release => {
                            #[cfg(windows)]
                            {
                                let now = Instant::now();
                                let pasted = paste_key(k, previous, now, streak);
                                streak = if previous.is_some_and(|t| now.duration_since(t) < Duration::from_millis(10)) { streak + 1 } else { 0 };
                                if let Some(text) = pasted.filter(|_| s.approval.is_none() && s.menu.is_none() && s.modes.is_none() && s.sessions.is_none() && s.secret.is_none()) {
                                    s.editor.paste(text);
                                    s.redraw();
                                    previous = Some(Instant::now());
                                    continue;
                                }
                            }
                            s.on_key(k, &tx);
                            if std::mem::take(&mut s.clipboard_request) {
                                let screen = screen.clone();
                                tokio::spawn(async move {
                                    let result =
                                        tokio::task::spawn_blocking(super::clipboard::read_image)
                                            .await
                                            .unwrap_or_else(|e| Err(e.to_string()));
                                    let mut s = lock(&screen);
                                    s.hint = None;
                                    match result {
                                        Ok(image) => s.attach(image, "clipboard"),
                                        Err(e) => s.hint = Some(e),
                                    }
                                    s.redraw();
                                });
                            }
                            // Exclude rendering time when measuring the pause between keys.
                            #[cfg(windows)]
                            {
                                previous = matches!(
                                    k.code,
                                    KeyCode::Char(_) | KeyCode::Enter | KeyCode::Tab
                                )
                                .then_some(Instant::now());
                            }
                        }
                        Event::Paste(text) => {
                            if let Some(sec) = &mut s.secret {
                                sec.text.push_str(text.trim());
                            } else if s.approval.is_none() && s.menu.is_none() && s.sessions.is_none() {
                                s.editor.paste(&text);
                            }
                            s.redraw();
                        }
                        Event::Resize(w, h) => {
                            s.width = (w as usize).max(1);
                            s.height = (h as usize).max(1);
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
                let mut tick = tokio::time::interval(Duration::from_millis(80));
                loop {
                    tick.tick().await;
                    let mut s = lock(&screen);
                    let upgrade_changed = s.upgrader.as_ref().is_some_and(|u| u.take_render_dirty());
                    if s.busy.is_some() && s.approval.is_none() {
                        s.frame += 1;
                        s.redraw();
                    } else if upgrade_changed {
                        s.redraw();
                    }
                }
            })
        };
        setup.disarm();
        Ok(Tui {
            screen,
            commands: rx,
            tasks: vec![input, ticker],
            enhanced,
        })
    }

    pub fn screen(&self) -> MutexGuard<'_, Screen> {
        lock(&self.screen)
    }

    /// Open the model switcher and fill it in the background.
    pub fn open_models(&self) {
        let home = {
            let mut s = self.screen();
            s.open_models();
            s.home.clone()
        };
        let screen = self.screen.clone();
        tokio::spawn(async move {
            let (choices, warnings) = crate::models::list(&home).await;
            lock(&screen).set_models(choices, warnings);
        });
    }

    /// Show a menu titled `title` in the input area; the chosen option's
    /// index, or None when it is closed with Esc.
    pub async fn pick(&self, title: &str, prompt: &str, options: &[String]) -> Option<usize> {
        let shown: Vec<String> = options.iter().map(|o| one_line(o, 200)).collect();
        let (tx, rx) = oneshot::channel();
        {
            let mut s = self.screen();
            s.flush_all();
            s.question = Some(PendingQuestion {
                question: sanitize_text(prompt),
                options: shown.clone(),
                selected: 0,
                menu: Some(title.to_string()),
                reply: tx,
            });
            s.redraw();
        }
        let guard = ClearQuestion(Arc::clone(&self.screen));
        let answer = rx.await.ok().flatten();
        drop(guard);
        answer.and_then(|a| shown.iter().position(|o| *o == a))
    }

    /// Ask for a secret (an API key) without echoing it; None if cancelled
    /// or empty. The text never reaches the editor, history or transcript.
    pub async fn secret(&self, prompt: &str) -> Option<String> {
        let (tx, rx) = oneshot::channel();
        {
            let mut s = self.screen();
            s.flush_all();
            s.secret = Some(PendingSecret { prompt: prompt.to_string(), text: String::new(), reply: tx });
            s.redraw();
        }
        let answer = rx.await.ok().flatten();
        let mut s = self.screen();
        if s.secret.take().is_some() || answer.is_some() {
            s.redraw();
        }
        answer.map(|k| k.trim().to_string()).filter(|k| !k.is_empty())
    }

    /// The `agent::Ui` for one run.
    pub fn ui(&self, cwd: PathBuf) -> TuiUi {
        TuiUi {
            screen: self.screen.clone(),
            cwd,
        }
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
        {
            let mut s = lock(&self.screen);
            s.close_transcript();
            s.flush_all();
            s.clear_pane();
            let _ = s.out.flush();
        }
        *SCREEN.lock().unwrap() = None;
        if self.enhanced {
            pop_keyboard_enhancement();
        }
        restore_terminal();
    }
}

/// Restores the terminal if `start` fails after raw mode is on. Disarmed once
/// `Tui` exists, so `Drop` is the only restorer for a live session.
struct TerminalSetup {
    armed: bool,
    bracketed: bool,
    enhanced: bool,
}

impl TerminalSetup {
    fn arm() -> Self {
        Self {
            armed: true,
            bracketed: false,
            enhanced: false,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for TerminalSetup {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if self.enhanced {
            pop_keyboard_enhancement();
        }
        if self.bracketed {
            let _ = execute!(crate::record_tape::stdout(), crossterm::event::DisableBracketedPaste);
        }
        let _ = execute!(crate::record_tape::stdout(), cursor::Show);
        let _ = terminal::disable_raw_mode();
    }
}

/// The viewer is on the alternate screen. The panic hook and `Drop` both restore.
static TRANSCRIPT_ALT_SCREEN: AtomicBool = AtomicBool::new(false);
/// Keyboard enhancement flags were pushed. Pop them once.
static KEYBOARD_ENHANCED: AtomicBool = AtomicBool::new(false);

fn pop_keyboard_enhancement() {
    if KEYBOARD_ENHANCED.swap(false, Ordering::SeqCst) {
        let _ = execute!(crate::record_tape::stdout(), PopKeyboardEnhancementFlags);
    }
}

/// Flags the panic hook should undo, taken so a later `Drop` does not repeat them.
fn take_terminal_fixes() -> (bool, bool) {
    (
        TRANSCRIPT_ALT_SCREEN.swap(false, Ordering::SeqCst),
        KEYBOARD_ENHANCED.swap(false, Ordering::SeqCst),
    )
}

fn restore_terminal() {
    let (alt, enhanced) = take_terminal_fixes();
    let mut out = crate::record_tape::stdout();
    if alt {
        let _ = execute!(out, terminal::LeaveAlternateScreen);
    }
    if enhanced {
        let _ = execute!(out, PopKeyboardEnhancementFlags);
    }
    let _ = execute!(out, crossterm::event::DisableBracketedPaste, cursor::Show);
    let _ = terminal::disable_raw_mode();
}

// ---------------------------------------------------------------- agent::Ui

/// Clears a pending question if `ask` is cancelled before the user answers.
struct ClearQuestion(Arc<Mutex<Screen>>);

impl Drop for ClearQuestion {
    fn drop(&mut self) {
        let mut s = lock(&self.0);
        if s.question.take().is_some() {
            s.redraw();
        }
    }
}

/// Show the approval pane on `screen` and wait for the answer. `origin`
/// names who asks when it isn't the main agent (shown above the title).
async fn approve_on(
    screen: &Arc<Mutex<Screen>>,
    origin: Option<&str>,
    req: &ApprovalRequest<'_>,
) -> Approval {
    let (tool, summary, preview) = (req.tool, req.summary, req.preview);
    let (tx, rx) = oneshot::channel();
    {
        let mut s = lock(screen);
        s.flush_live();
        let w = s.width;
        let (verb, target) = describe_tool(tool, summary);
        let title = format!(
            "{} {}",
            paint(fg(BRIGHT), &verb),
            paint(
                fg(GRAY),
                &one_line(&target, w.saturating_sub(verb.len() + 3).max(1))
            )
        );
        let mut lines = Vec::new();
        let mut prewrapped = false;
        if let Some(p) = preview {
            let mut p = sanitize_text(p);
            const MAX_PREVIEW_CHARS: usize = 200_000;
            if p.chars().count() > MAX_PREVIEW_CHARS {
                let cut = p
                    .char_indices()
                    .nth(MAX_PREVIEW_CHARS)
                    .map(|(i, _)| i)
                    .unwrap_or(p.len());
                p.truncate(cut);
                p.push_str("\n… [preview truncated; the rest is omitted from the display]");
            }
            if looks_like_diff(&p) {
                lines = diff(&p, true, usize::MAX, w, "  ")
                    .lines()
                    .map(str::to_string)
                    .collect();
                prewrapped = true;
            } else {
                lines = p.lines().map(str::to_string).collect();
            }
        } else if tool == "bash" || tool == "web_fetch" {
            lines = sanitize_text(summary).lines().map(str::to_string).collect();
        }
        if lines.len() > 500 {
            lines.truncate(500);
            lines.push("… [preview truncated]".into());
        }
        let origin = origin.map(|o| {
            let o = match o.split_once(": ") {
                Some((who, what)) => format!("{who} · {what}"),
                None => o.to_string(),
            };
            one_line(&o, 200)
        });
        crate::notify::notify(&crate::notify::Event::Attention(&format!(
            "Approve: {verb} {}",
            one_line(&target, 80)
        )));
        s.approval = Some(PendingApproval {
            title,
            lines,
            prewrapped,
            scroll: 0,
            rule: req.rule.as_deref().map(|r| one_line(r, 300)),
            save_rule: req.save_rule.as_deref().map(|r| one_line(r, 300)),
            origin,
            reply: tx,
        });
        s.redraw();
    }
    let guard = ClearApproval(Arc::clone(screen));
    let answer = rx.await.unwrap_or(Approval::No);
    drop(guard);
    answer
}

/// Asks on the screen directly, so it works while the main turn is blocked
/// inside a tool (a subagent) and holds the `&mut Ui`.
struct TuiApprover {
    screen: Arc<Mutex<Screen>>,
}

#[async_trait::async_trait]
impl Approver for TuiApprover {
    async fn approve(&self, origin: &str, req: &ApprovalRequest<'_>) -> Approval {
        approve_on(&self.screen, Some(origin), req).await
    }
}

/// Clears a pending approval if the prompt is dropped before the user answers.
struct ClearApproval(Arc<Mutex<Screen>>);

impl Drop for ClearApproval {
    fn drop(&mut self) {
        let mut s = lock(&self.0);
        if s.approval.take().is_some() {
            s.redraw();
        }
    }
}

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
                s.flush_live();
                s.set_label(&format!(
                    "Preparing {}",
                    one_line(&name.replace('_', " "), 80)
                ));
                s.redraw();
            }
        }
    }

    fn end_message(&mut self) {
        let mut s = self.s();
        s.log.end_message();
        s.flush_live();
        s.set_label("Working");
        s.redraw();
    }

    fn tool_start(&mut self, tool: &str, summary: &str) {
        self.s().tool_start(tool, summary);
    }

    fn tool_end(&mut self, tool: &str, output: &ToolOutput) {
        let mut s = self.s();
        // A denied call never started; show it as a failed row.
        if !s.tools.iter().any(|t| t.status == Status::Running) {
            s.tool_start(tool, "");
        }
        s.tool_end(output);
    }

    async fn approve(&mut self, req: &ApprovalRequest<'_>) -> Approval {
        let answer = approve_on(&self.screen, None, req).await;
        if answer == Approval::No {
            // The loop reports the denial through tool_end.
            self.s().tool_start(req.tool, req.summary);
        }
        answer
    }

    fn approver(&self) -> Option<Arc<dyn Approver>> {
        Some(Arc::new(TuiApprover {
            screen: Arc::clone(&self.screen),
        }))
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
        self.s().print_info(&format!("  {msg}"), fg(GRAY));
    }

    async fn ask(&mut self, question: &str, options: &[String]) -> Option<String> {
        let (tx, rx) = oneshot::channel();
        {
            let mut s = self.s();
            s.flush_all();
            crate::notify::notify(&crate::notify::Event::Attention(&format!(
                "Question: {}",
                one_line(question, 120)
            )));
            s.question = Some(PendingQuestion {
                question: sanitize_text(question),
                options: options.iter().map(|o| one_line(o, 200)).collect(),
                selected: 0,
                menu: None,
                reply: tx,
            });
            s.redraw();
        }
        // Dropped on cancel: clear the pane so the next key isn't answered into a closed channel.
        let guard = ClearQuestion(Arc::clone(&self.screen));
        let answer = rx.await.ok().flatten();
        drop(guard);
        let mut s = self.s();
        let shown = answer.clone().unwrap_or_else(|| "(skipped)".into());
        s.print_info(
            &format!(
                "{} {}\n  {}",
                paint(fg(BRIGHT), "?"),
                paint(fg(MID), question),
                paint(fg(GRAY), &shown)
            ),
            Style::new(),
        );
        answer
    }

    fn take_steering(&mut self) -> Vec<Item> {
        let steer = {
            let mut s = self.s();
            take_steer_texts(&mut s.queue)
        };
        steer.into_iter().map(|t| self.deliver(t)).collect()
    }

    fn take_follow_up(&mut self) -> Option<Item> {
        let text = {
            let mut s = self.s();
            take_follow_up_text(&mut s.queue)?
        };
        Some(self.deliver(text))
    }
}

/// A slash command is a barrier: a later follow-up does not run before it.
fn take_follow_up_text(queue: &mut Vec<(Queued, String)>) -> Option<String> {
    let (kind, text) = queue.first()?;
    if *kind != Queued::FollowUp || is_slash_command(text) {
        return None;
    }
    Some(queue.remove(0).1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;

    #[test]
    fn panic_restore_takes_alt_screen_and_keyboard_flags_once() {
        TRANSCRIPT_ALT_SCREEN.store(true, Ordering::SeqCst);
        KEYBOARD_ENHANCED.store(true, Ordering::SeqCst);
        assert_eq!(take_terminal_fixes(), (true, true));
        assert_eq!(take_terminal_fixes(), (false, false));
    }

    #[test]
    fn windows_input_and_clipboard() {
        let now = Instant::now();
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        let tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(
            paste_key(enter, Some(now), now + Duration::from_millis(1), 5),
            Some("\n")
        );
        assert_eq!(
            paste_key(tab, Some(now), now + Duration::from_millis(1), 5),
            Some("\t")
        );
        assert_eq!(
            paste_key(enter, Some(now), now + Duration::from_millis(50), 5),
            None
        );
        assert_eq!(paste_key(enter, None, now, 5), None);
        // A couple of keys released together after a stall are not a paste.
        assert_eq!(
            paste_key(enter, Some(now), now + Duration::from_millis(1), 1),
            None
        );
        let mut editor = Editor::default();
        for (i, c) in "a\\\nb\n".chars().enumerate() {
            let key = KeyEvent::new(
                if c == '\n' {
                    KeyCode::Enter
                } else {
                    KeyCode::Char(c)
                },
                KeyModifiers::NONE,
            );
            let time = now + Duration::from_millis(i as u64);
            if let Some(text) = paste_key(key, Some(now), time, 3) {
                editor.paste(text);
            } else {
                assert_eq!(editor.handle_key(key, true), Action::None);
            }
        }
        assert_eq!(editor.text(), "a\\\nb\n");
        assert_eq!(
            editor.handle_key(enter, true),
            Action::Submit("a\\\nb\n".into())
        );
        assert!(text_key(
            KeyEvent::new(
                KeyCode::Char('@'),
                KeyModifiers::CONTROL | KeyModifiers::ALT
            ),
            '@'
        ));
        let bytes = clipboard_bytes("€🙂", true);
        let units: Vec<u16> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| u16::from_le_bytes(*b))
            .collect();
        assert_eq!(units[0], 0xfeff);
        assert_eq!(String::from_utf16(&units[1..]).unwrap(), "€🙂");
    }

    #[test]
    fn widths_and_wrapping() {
        assert_eq!(visible_width("\x1b[1mab\x1b[0m界"), 4);
        assert_eq!(truncate("ab界c", 3), "ab…");
        assert_eq!(truncate("abc", 3), "abc");
        assert_eq!(wrap("abcdef", 4), vec!["abcd", "ef"]);
        assert_eq!(wrap("", 4), vec![""]);
        assert_eq!(wrap("a\nb", 10), vec!["a".to_string(), "b".into()]);
        assert_eq!(wrap("a\nb", 1).len(), 2);
        assert_eq!(tokens(950), "950");
        assert_eq!(tokens(14_000), "14k");
        assert_eq!(tokens(1_400), "1.4k");
    }

    #[test]
    fn thinking_collapses_to_one_line() {
        assert_eq!(
            thinking_summary("**Planning**\n\nstep one", 3, 80),
            "• Thought for 3s · Planning"
        );
        assert_eq!(thinking_summary("", 0, 80), "• Thought");
        assert!(visible_width(&thinking_summary(&"x".repeat(200), 1, 40)) <= 40);
    }

    #[test]
    fn tools_read_like_fx() {
        assert_eq!(
            describe_tool("read_file", "src/main.rs"),
            ("Read".into(), "src/main.rs".into())
        );
        assert_eq!(
            describe_tool("bash", "cargo test"),
            ("Ran".into(), "cargo test".into())
        );
        assert_eq!(describe_tool("mcp__wiki__ask", "{}").0, "Called");
        assert!(describe_tool("mcp__wiki__ask", "{}")
            .1
            .starts_with("wiki.ask"));
        assert_eq!(
            diff_stat("--- a/x\n+++ b/x\n@@\n-old\n+new\n+more\n"),
            "+2 / -1"
        );
        assert_eq!(tool_kind("grep"), "read");
    }

    #[test]
    fn spread_aligns_right() {
        assert_eq!(spread("ab", "cd", 8), "ab    cd");
        assert_eq!(spread("abcdef", "cd", 8), "abcdef");
    }

    fn screen() -> Screen {
        let mut s = Screen::new(
            Footer::default(),
            Editor::new(Vec::new(), Vec::new()),
            PathBuf::new(),
        );
        s.width = 40;
        s
    }

    fn feed(s: &mut Screen, text: &str) -> Vec<String> {
        let mut out = Vec::new();
        for l in text.lines() {
            s.md_line(l, &mut out);
        }
        s.flush_table(&mut out);
        out.iter()
            .map(|r| super::super::render::strip_ansi(r))
            .collect()
    }

    #[test]
    fn tables_are_buffered_and_laid_out() {
        let mut s = screen();
        let out = feed(&mut s, "Intro\n| a | bb |\n|---|---|\n| 1 | 2 |\n\nafter");
        assert_eq!(
            out,
            vec![
                "  Intro",
                "  a │ bb",
                "  ──┼───",
                "  1 │ 2",
                "  ",
                "  after"
            ]
        );
        // While streaming, rows wait until the table ends.
        let mut out = Vec::new();
        for l in ["| x | y |", "|---|---|", "| 1 | 2 |"] {
            s.md_line(l, &mut out);
        }
        assert!(out.is_empty() && s.table.len() == 3);
        s.md_line("done", &mut out);
        assert_eq!(out.len(), 4);
        assert!(s.table.is_empty());
        // A line with a pipe that isn't a table header comes out unchanged.
        assert_eq!(feed(&mut s, "a | b\nplain"), vec!["  a | b", "  plain"]);
        assert_eq!(feed(&mut s, "x | y"), vec!["  x | y"]);
        // Pipes inside code blocks are code.
        assert_eq!(feed(&mut s, "```\n| a |\n|---|\n```").len(), 4);
        assert!(s.table.is_empty());
    }

    fn meta(id: &str, cwd: &str, mins: i64) -> Meta {
        let when = chrono::Utc::now() - chrono::Duration::minutes(mins);
        Meta {
            id: id.into(),
            // Created long ago: ordering follows the last activity.
            created: when - chrono::Duration::days(mins),
            updated: when,
            cwd: cwd.into(),
            model: String::new(),
            title: format!("title {id}"),
            preview: format!("first prompt of {id}"),
            turns: 1,
            prefs: None,
            path: PathBuf::new(),
        }
    }

    #[test]
    fn session_picker_scopes_orders_and_filters() {
        let all = vec![
            meta("a", "/other", 1),
            meta("b", "/here", 5),
            meta("cur", "/here", 0),
            meta("c", "/here", 2),
        ];
        let mut m = SessionMenu::new(all.clone(), std::path::Path::new("/here"), Some("cur"));
        let ids = |m: &SessionMenu| m.visible().iter().map(|m| m.id.clone()).collect::<Vec<_>>();
        assert!(!m.all);
        assert_eq!(ids(&m), ["c", "b"]);
        let key = |m: &mut Screen, code| {
            let (tx, _rx) = mpsc::unbounded_channel();
            m.on_sessions_key(KeyEvent::new(code, KeyModifiers::NONE), &tx);
        };
        let mut s = Screen::new(Footer::default(), Editor::default(), PathBuf::new());
        s.sessions = Some(SessionMenu::new(all.clone(), std::path::Path::new("/here"), None));
        key(&mut s, KeyCode::Tab);
        let menu = s.sessions.as_ref().unwrap();
        assert!(menu.all);
        assert_eq!(ids(menu), ["cur", "a", "c", "b"]);
        m.all = true;
        m.filter = "other".into();
        assert_eq!(ids(&m), ["a"]);
        m.filter = "TITLE B".into();
        assert_eq!(ids(&m), ["b"]);
        m.filter = "prompt of c".into();
        assert_eq!(ids(&m), ["c"]);
        // Nothing here: start on every workspace.
        assert!(SessionMenu::new(all, std::path::Path::new("/nowhere"), None).all);
    }

    #[test]
    fn relative_times() {
        let now = chrono::Utc::now();
        let ago_s = |s: i64| ago(now - chrono::Duration::seconds(s), now);
        assert_eq!(ago_s(5), "just now");
        assert_eq!(ago_s(-5), "just now");
        assert_eq!(ago_s(120), "2m ago");
        assert_eq!(ago_s(3 * 3600), "3h ago");
        assert_eq!(ago_s(2 * 86_400), "2d ago");
        assert_eq!(ago_s(15 * 86_400), "2w ago");
        assert_eq!(ago_s(400 * 86_400).len(), 10);
    }

    #[test]
    fn steering_keeps_slash_commands_queued() {
        let mut queue = vec![
            (Queued::Steer, "look at main".into()),
            (Queued::Steer, "/compact now".into()),
            (Queued::Steer, "after slash".into()),
            (Queued::FollowUp, "later".into()),
            (Queued::Steer, "/model gpt".into()),
            (Queued::Steer, "  /model x".into()),
        ];
        assert_eq!(
            take_steer_texts(&mut queue),
            vec!["look at main".to_string()]
        );
        assert_eq!(
            queue,
            vec![
                (Queued::Steer, "/compact now".into()),
                (Queued::Steer, "after slash".into()),
                (Queued::FollowUp, "later".into()),
                (Queued::Steer, "/model gpt".into()),
                (Queued::Steer, "  /model x".into()),
            ]
        );
    }

    #[test]
    fn follow_up_stops_at_a_slash_command() {
        let mut queue = vec![
            (Queued::FollowUp, "/new".into()),
            (Queued::FollowUp, "next prompt".into()),
        ];
        assert!(take_follow_up_text(&mut queue).is_none());
        assert_eq!(queue.len(), 2);
        queue.remove(0);
        assert_eq!(
            take_follow_up_text(&mut queue).as_deref(),
            Some("next prompt")
        );
        let mut joined = vec![
            (Queued::FollowUp, "one".into()),
            (Queued::FollowUp, "two".into()),
        ];
        assert_eq!(take_follow_up_text(&mut joined).as_deref(), Some("one"));
        assert_eq!(joined.len(), 1);
    }

    #[test]
    fn question_newlines_are_rows_and_controls_are_stripped() {
        let mut s = screen();
        s.width = 20;
        s.height = 24;
        let (tx, _rx) = oneshot::channel();
        s.question = Some(PendingQuestion {
            question: "line\n\x1b]52;c;eA==\x07next".into(),
            options: vec!["ok\nmore".into()],
            selected: 0,
            menu: None,
            reply: tx,
        });
        let (rows, _, _) = s.pane();
        let text = rows.join("\n");
        assert!(text.contains("line"), "{text}");
        assert!(text.contains("next"), "{text}");
        assert!(!text.contains("\x1b]52"), "{text}");
        assert!(rows.iter().all(|r| visible_width(r) <= 20), "{rows:?}");
    }

    #[test]
    fn pane_rows_stay_within_width() {
        let mut s = screen();
        s.width = 30;
        s.height = 40;
        s.busy = Some((
            "Running an extremely long tool name that must not wrap".into(),
            Instant::now(),
        ));
        let (tx, _rx) = oneshot::channel();
        s.approval = Some(PendingApproval {
            title: "Approve".into(),
            lines: vec!["x".repeat(400)],
            prewrapped: false,
            scroll: 0,
            rule: None,
            save_rule: None,
            origin: None,
            reply: tx,
        });
        let (rows, _, _) = s.pane();
        assert!(rows.iter().all(|r| visible_width(r) <= 30), "{rows:?}");
        s.approval = None;
        s.menu = Some(ModelMenu {
            choices: Vec::new(),
            tabs: vec!["All".into()],
            tab: 0,
            filter: "y".repeat(200),
            selected: 0,
            loading: false,
            warnings: Vec::new(),
        });
        let (rows, _, _) = s.pane();
        assert!(rows.iter().all(|r| visible_width(r) <= 30), "{rows:?}");
    }

    #[test]
    fn tool_rows_keep_the_path_visible() {
        let mut s = screen();
        s.width = 24;
        s.height = 40;
        let row = |target: &str, note: &str| ToolRow {
            tool: "read_file".into(),
            verb: "Read".into(),
            target: target.into(),
            note: note.into(),
            detail: Vec::new(),
            status: Status::Ok,
        };
        s.tools = vec![row("a.rs", ""), row("src/lib.rs", "+1")];
        let (rows, _, _) = s.pane();
        assert!(
            rows.iter()
                .any(|r| super::super::render::strip_ansi(r).contains("src/lib.rs")),
            "{rows:?}"
        );
        assert!(rows.iter().all(|r| visible_width(r) <= 24), "{rows:?}");
    }

    #[test]
    fn approval_esc_cancels_and_ctrl_d_exits() {
        let mut s = screen();
        let cancel = CancellationToken::new();
        s.cancel = Some(cancel.clone());
        let (tx, mut rx) = oneshot::channel();
        s.approval = Some(PendingApproval {
            title: "t".into(),
            lines: Vec::new(),
            prewrapped: false,
            scroll: 0,
            rule: None,
            save_rule: None,
            origin: None,
            reply: tx,
        });
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        s.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &cmd_tx);
        assert_eq!(rx.try_recv().unwrap(), Approval::No);
        assert!(cancel.is_cancelled());
        assert!(s.approval.is_none());

        let (tx, mut rx) = oneshot::channel();
        s.approval = Some(PendingApproval {
            title: "t".into(),
            lines: Vec::new(),
            prewrapped: false,
            scroll: 0,
            rule: None,
            save_rule: None,
            origin: None,
            reply: tx,
        });
        s.on_key(
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL),
            &cmd_tx,
        );
        assert_eq!(rx.try_recv().unwrap(), Approval::No);
        assert!(matches!(cmd_rx.try_recv().unwrap(), Command::Exit));
    }

    #[test]
    fn save_key_only_when_offered() {
        let mut s = screen();
        s.width = 80;
        s.height = 40;
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();
        let pending = |save: Option<&str>, tx| PendingApproval {
            title: "Called wiki.ask".into(),
            lines: Vec::new(),
            prewrapped: false,
            scroll: 0,
            rule: Some("`mcp__wiki__ask`".into()),
            save_rule: save.map(str::to_string),
            origin: Some("subagent · Count files".into()),
            reply: tx,
        };
        let (tx, mut rx) = oneshot::channel();
        s.approval = Some(pending(None, tx));
        let text = super::super::render::strip_ansi(&s.pane().0.join("\n"));
        assert!(
            text.contains("subagent · Count files") && !text.contains("save"),
            "{text}"
        );
        s.on_key(
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE),
            &cmd_tx,
        );
        assert!(rx.try_recv().is_err() && s.approval.is_some());
        s.approval = None;

        let (tx, mut rx) = oneshot::channel();
        s.approval = Some(pending(Some("mcp__wiki__ask"), tx));
        let text = super::super::render::strip_ansi(&s.pane().0.join("\n"));
        assert!(
            text.contains("Always, and save `mcp__wiki__ask` to permissions.json"),
            "{text}"
        );
        s.on_key(
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE),
            &cmd_tx,
        );
        assert_eq!(rx.try_recv().unwrap(), Approval::AlwaysSave);
        assert!(s.approval.is_none());
    }

    #[test]
    fn dropping_ask_clears_the_question() {
        let screen = Arc::new(Mutex::new(screen()));
        let mut ui = TuiUi {
            screen: Arc::clone(&screen),
            cwd: PathBuf::new(),
        };
        let options = vec!["a".to_string()];
        let mut fut = Box::pin(Ui::ask(&mut ui, "pick", &options));
        let waker = std::task::Waker::noop();
        let mut cx = std::task::Context::from_waker(waker);
        assert!(fut.as_mut().poll(&mut cx).is_pending());
        assert!(lock(&screen).question.is_some());
        drop(fut);
        assert!(lock(&screen).question.is_none());
    }

    #[test]
    fn question_interrupt_cancels_and_ctrl_d_exits() {
        let mut s = screen();
        let cancel = CancellationToken::new();
        s.cancel = Some(cancel.clone());
        let (tx, mut rx) = oneshot::channel();
        s.question = Some(PendingQuestion {
            question: "q".into(),
            options: vec!["a".into()],
            selected: 0,
            menu: None,
            reply: tx,
        });
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        s.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &cmd_tx);
        assert!(rx.try_recv().unwrap().is_none());
        assert!(cancel.is_cancelled());
        assert!(s.question.is_none());
        assert!(cmd_rx.try_recv().is_err());

        let cancel = CancellationToken::new();
        s.cancel = Some(cancel.clone());
        let (tx, mut rx) = oneshot::channel();
        s.question = Some(PendingQuestion {
            question: "q".into(),
            options: vec!["a".into()],
            selected: 0,
            menu: None,
            reply: tx,
        });
        s.on_key(
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            &cmd_tx,
        );
        assert!(rx.try_recv().unwrap().is_none());
        assert!(cancel.is_cancelled());
        assert!(cmd_rx.try_recv().is_err());

        let (tx, _rx) = oneshot::channel();
        s.question = Some(PendingQuestion {
            question: "q".into(),
            options: vec!["a".into()],
            selected: 0,
            menu: None,
            reply: tx,
        });
        s.on_key(
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL),
            &cmd_tx,
        );
        assert!(matches!(cmd_rx.try_recv().unwrap(), Command::Exit));
    }

    #[test]
    fn dropping_approval_clears_the_pane() {
        let screen = Arc::new(Mutex::new(screen()));
        {
            let mut s = lock(&screen);
            let (tx, _rx) = oneshot::channel();
            s.approval = Some(PendingApproval {
                title: "t".into(),
                lines: Vec::new(),
                prewrapped: false,
                scroll: 0,
                rule: None,
                save_rule: None,
                origin: None,
                reply: tx,
            });
        }
        drop(ClearApproval(Arc::clone(&screen)));
        assert!(lock(&screen).approval.is_none());
    }

    #[test]
    fn clip_width_restores_sgr_reset() {
        let painted = format!("\x1b[38;2;138;138;138m{}\x1b[0m", "m".repeat(40));
        let clipped = clip_width(&painted, 12);
        assert!(clipped.contains("\x1b[0m"), "{clipped:?}");
        assert!(visible_width(&clipped) <= 12, "{}", visible_width(&clipped));
        assert_eq!(clip_width("plain", 10), "plain");
    }

    #[test]
    fn list_rows_drop_terminal_controls() {
        let row = list_row("helper\u{1b}]52;c;eA==\u{7}ok", 80);
        assert!(!row.contains("52;c"), "{row}");
        let plain = super::super::render::strip_ansi(&row);
        assert!(plain.contains("helper") && plain.contains("ok"), "{plain}");
    }

    #[test]
    fn narrow_footer_stays_in_width_and_resets() {
        let mut s = screen();
        s.width = 20;
        s.footer.model = "gpt-5.6-sol-high\u{1b}[31m".into();
        s.footer.effort = Some("medium".into());
        s.footer.provider = "codex".into();
        s.footer.sandbox = "workspace-write".into();
        s.footer.cwd = "/home/someone/very/long".into();
        let line = s.footer_line();
        assert!(
            visible_width(&line) <= 20,
            "{} {line:?}",
            visible_width(&line)
        );
        assert!(line.contains("\x1b[0m"), "{line:?}");
        assert!(!line.contains("[31m"), "{line:?}");
    }

    #[test]
    fn model_menu_labels_are_sanitized() {
        let mut s = screen();
        s.width = 60;
        s.height = 40;
        s.menu = Some(ModelMenu {
            choices: vec![ModelChoice {
                provider: "openai".into(),
                model: "evil\u{1b}]52;c;eA==\u{7}model".into(),
                ..Default::default()
            }],
            tabs: vec!["All".into()],
            tab: 0,
            filter: String::new(),
            selected: 0,
            loading: false,
            warnings: Vec::new(),
        });
        let (rows, _, _) = s.pane();
        let joined = rows.join("\n");
        assert!(!joined.contains("]52"), "{joined}");
        assert!(
            rows.iter()
                .any(|r| super::super::render::strip_ansi(r).contains("evilmodel")),
            "{rows:?}"
        );
    }
}
