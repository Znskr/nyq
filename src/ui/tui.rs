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
use nu_ansi_term::Style;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use unicode_width::UnicodeWidthChar;

use super::chrome::{self, Mark};
use super::editor::{Action, Editor};
use super::keys::{KeyAction, Keymap};
use super::render::{
    escape_len, is_table_sep, numbered_diff, looks_like_diff, one_line, sanitize_text, table, table_cells,
    truncate, visible_width, wrap_row, file_url, link, Fence, Markdown,
};
use super::statusline::{tilde, tokens};
use super::theme::{fg, BRIGHT, FAINT, GRAY, MID, RED, YELLOW};
use crate::agent::{Approval, ApprovalRequest, Approver, Ui};
use crate::models::ModelChoice;
use crate::session::Meta;
use crate::tools::ToolOutput;
use crate::types::{Item, StreamEvent, Usage};

const MAX_PREVIEW: usize = 14;
const MENU_ROWS: usize = 8;
/// Rows of a running command's output shown live.
const LIVE_OUTPUT_ROWS: usize = 5;
/// Diff rows shown under an edit before the rest is folded.
const INLINE_DIFF_ROWS: usize = 12;
/// Rows kept for repainting after a resize.
const RECENT_ROWS: usize = 1000;

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
    /// The number in its `[Image #N]` token.
    pub id: usize,
    pub label: String,
    pub image: crate::types::Image,
}

impl Attachment {
    pub fn token(&self) -> String {
        format!("[Image #{}]", self.id)
    }
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
    /// Models the current provider listed, for the classifier setting.
    pub models: Vec<String>,
    /// The session title (renamed, or the first prompt); also the window title.
    pub title: String,
    /// Status texts set by extensions (`status/set`).
    pub extensions: Vec<String>,
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
    /// The change an edit or write made (a unified diff), shown inline.
    diff: Option<String>,
    started: Instant,
    /// How long the call took, once it finished.
    took: Option<Duration>,
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
    /// A note for the model, typed after Tab; sent with the answer.
    note: String,
    /// Keys go to `note` (Tab or Enter returns to the choices).
    noting: bool,
    /// The choice picked with the arrows; Enter confirms it. None at first,
    /// so a stray Enter never approves.
    selected: Option<usize>,
    /// The whole preview for the full-screen review, when it doesn't fit.
    review: Option<String>,
    reply: oneshot::Sender<(Approval, Option<String>)>,
}

/// The approval choices: key, answer and label.
fn approval_choices(a: &PendingApproval) -> Vec<(char, Approval, String)> {
    let always = match &a.rule {
        Some(r) => format!("Always allow {r}"),
        None => "Always: can't be scoped to a rule here, approves once".into(),
    };
    let mut choices = vec![('y', Approval::Yes, "Yes, once".to_string()), ('a', Approval::Always, always)];
    if let Some(r) = &a.save_rule {
        choices.push(('s', Approval::AlwaysSave, format!("Always, and save `{r}` to permissions.json")));
        choices.push(('l', Approval::AlwaysSaveLocal, format!("Always, and save `{r}` for this workspace only")));
    }
    choices.push(('n', Approval::No, "No, tell nyq what to do instead".into()));
    choices
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
        crate::models::search(
            self.choices.iter().filter(|c| tab == "All" || &c.provider == tab),
            &self.filter,
        )
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
    fence: Option<Fence>,
    /// The latest rows printed to the scrollback, unwrapped, for repainting
    /// the screen at a new width after a resize.
    recent: std::collections::VecDeque<String>,
    /// The window title last set, to send it only when it changes.
    window_title: String,
    /// Markdown table lines held back until the table ends (column widths
    /// depend on every row); a lone line with `|` is a candidate header.
    table: Vec<String>,
    busy: Option<(String, Instant)>,
    /// Time this turn spent waiting on an approval or question (the clock
    /// stops), and when the current wait began.
    paused: Duration,
    pause_start: Option<Instant>,
    /// This turn's tool calls by kind, and its tokens (in, out).
    tally: Vec<(&'static str, usize)>,
    turn_tokens: (u64, u64),
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
    help: Option<super::help_menu::HelpMenu>,
    /// Key bindings for the global actions (the editor holds its own copy).
    keys: Arc<Keymap>,
    skills: Option<super::skills_menu::SkillsMenu>,
    /// Everything shown so far, for the transcript viewer.
    pub log: super::transcript::Log,
    viewer: Option<super::transcript::Viewer>,
    /// What the viewer shows instead of the log: an approval's full preview.
    review: Option<super::transcript::Log>,
    /// Scrollback rows held while the viewer covers the screen.
    deferred: Vec<String>,
    /// Images for the next message.
    pub attachments: Vec<Attachment>,
    /// Ctrl+V asked for a clipboard image (read by the input task).
    clipboard_request: bool,
    /// Workspace files for the `@` picker, indexed on first use.
    files: Option<Vec<String>>,
    file_selected: usize,
    /// Esc closed the `@` picker for the word being typed.
    files_closed: bool,
    slash_selected: usize,
    /// Arrow keys moved the slash selection since the last edit.
    slash_moved: bool,
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

fn paint(style: Style, s: &str) -> String {
    if s.is_empty() {
        String::new()
    } else {
        style.paint(s).to_string()
    }
}

/// Soft-wrap plain text into rows of at most `width` columns.
/// Cut a possibly-styled row to `width` columns, keeping escape sequences.
pub(super) fn clip_width(s: &str, width: usize) -> String {
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
    let mut rest = s;
    while let Some(c) = rest.chars().next() {
        if c == '\x1b' {
            let n = escape_len(rest);
            out.push_str(&rest[..n]);
            rest = &rest[n..];
            continue;
        }
        let cw = c.width().unwrap_or(0);
        if w + cw > limit {
            break;
        }
        w += cw;
        out.push(c);
        rest = &rest[c.len_utf8()..];
    }
    out.push('…');
    // A cut can land inside a styled span or link and drop its end, so later
    // rows keep the last color. Put the resets back when the source used escapes.
    if s.contains("\x1b]8;") {
        out.push_str("\x1b]8;;\x1b\\");
    }
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

/// "3 read · 1 edited · 2 commands": a turn's tool calls by kind.
fn tally_text(tally: &[(&str, usize)]) -> String {
    tally
        .iter()
        .map(|(kind, n)| match (*kind, *n) {
            ("read", n) => format!("{n} read"),
            ("edit", n) => format!("{n} edited"),
            ("command", 1) => "1 command".into(),
            ("command", n) => format!("{n} commands"),
            (_, 1) => "1 tool".into(),
            (_, n) => format!("{n} tools"),
        })
        .collect::<Vec<_>>()
        .join(", ")
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
            recent: Default::default(),
            window_title: String::new(),
            table: Vec::new(),
            busy: None,
            paused: Duration::ZERO,
            pause_start: None,
            tally: Vec::new(),
            turn_tokens: (0, 0),
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
            help: None,
            keys: super::keys::current(),
            skills: None,
            log: Default::default(),
            viewer: None,
            review: None,
            deferred: Vec::new(),
            attachments: Vec::new(),
            clipboard_request: false,
            files: None,
            file_selected: 0,
            files_closed: false,
            slash_selected: 0,
            slash_moved: false,
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
        let mark = |status: Status| {
            let m = match status {
                Status::Running => Mark::Running,
                Status::Ok => Mark::Ok,
                Status::Failed => Mark::Failed,
            };
            chrome::mark(m, self.frame)
        };
        // Durations under a tenth of a second are noise.
        let took = |d: Option<Duration>| {
            d.filter(|d| *d >= Duration::from_millis(100))
                .map(|d| paint(fg(FAINT), &format!("  {}", chrome::elapsed(d))))
                .unwrap_or_default()
        };
        let row_text = |t: &ToolRow, lead: &str| {
            let verb = match t.status {
                Status::Running if live => shimmer(&t.verb, self.frame),
                Status::Failed => paint(fg(RED), &t.verb),
                _ => paint(fg(BRIGHT), &t.verb),
            };
            let note = if t.note.is_empty() {
                String::new()
            } else {
                format!(" {}", chrome::note(&t.note))
            };
            let used = visible_width(lead) + 3 + t.verb.chars().count() + visible_width(&note);
            // The time goes before the target has to be cut.
            let took = Some(took(t.took))
                .filter(|took| used + visible_width(took) + visible_width(&t.target) <= w)
                .unwrap_or_default();
            let used = used + visible_width(&took);
            let target = truncate(&t.target, w.saturating_sub(used));
            let mut target = paint(fg(GRAY), &target);
            if matches!(t.tool.as_str(), "read_file" | "write_file" | "edit_file") {
                if let Some(url) = file_url(&self.footer.cwd, &t.target) {
                    target = link(&url, &target);
                }
            }
            format!("{lead}{} {verb} {target}{note}{took}", mark(t.status))
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
            if let Some(d) = &t.diff {
                let bar = paint(fg(FAINT), bar);
                let mut shown = numbered_diff(d, true, w, &bar);
                if shown.len() > INLINE_DIFF_ROWS {
                    let more = shown.len() - INLINE_DIFF_ROWS;
                    shown.truncate(INLINE_DIFF_ROWS);
                    let view = self.keys.hint(KeyAction::ExpandTools).unwrap_or_else(|| "/transcript".into());
                    shown.push(format!("{bar}{}", paint(fg(FAINT), &format!("… {more} more lines ({view} to view)"))));
                }
                rows.extend(shown);
            }
        };
        if n == 1 {
            let t = &self.tools[0];
            rows.push(row_text(t, ""));
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
        // The group's mark: running while any call runs, else failed if any failed.
        let status = [Status::Running, Status::Failed]
            .into_iter()
            .find(|s| self.tools.iter().any(|t| t.status == *s))
            .unwrap_or(Status::Ok);
        let total = self.tools.iter().filter_map(|t| t.took).sum::<Duration>();
        rows.push(format!(
            "{} {}{}{}",
            mark(status),
            paint(fg(BRIGHT), &format!("{n} tool calls")),
            paint(fg(GRAY), &format!(" · {}", parts.join(" · "))),
            took((status != Status::Running).then_some(total))
        ));
        if !live && crate::settings::current().collapse_tool_calls {
            return;
        }
        for (i, t) in self.tools.iter().enumerate() {
            let last = i + 1 == n;
            rows.push(row_text(
                t,
                &paint(fg(FAINT), if last { "└ " } else { "├ " }),
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

        // The tail of a running command's output.
        let shell = self.busy.as_ref().is_some_and(|(l, _)| l == "Running");
        if shell || self.tools.iter().any(|t| t.status == Status::Running && t.tool == "bash") {
            for line in crate::tools::live_tail(LIVE_OUTPUT_ROWS) {
                let text = truncate(&sanitize_text(&line), w.saturating_sub(4).max(1));
                rows.push(format!("{}{}", paint(fg(FAINT), "  │ "), paint(fg(FAINT), &text)));
            }
        }

        // Status: what the agent is doing.
        if let Some((label, start)) = &self.busy {
            let mut facts = vec![chrome::clock(self.working_time(*start).as_secs())];
            if self.turn_tokens != (0, 0) {
                facts.push(format!("↑{} ↓{}", tokens(self.turn_tokens.0), tokens(self.turn_tokens.1)));
            }
            if !self.tally.is_empty() {
                facts.push(tally_text(&self.tally));
            }
            let detail = if self.live_block == Block::Reasoning {
                self.live.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("").trim()
            } else {
                ""
            };
            let label = shimmer(&truncate(label, w.saturating_sub(24).max(8)), self.frame);
            let interrupt = self.keys.hint(KeyAction::Interrupt);
            if !rows.is_empty() {
                rows.push(String::new());
            }
            rows.push(chrome::working_line(&label, &facts, detail, interrupt.as_deref(), self.frame, w));
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
                paint(fg(FAINT), "›"),
                paint(fg(GRAY), &text),
                paint(fg(FAINT), tag)
            ));
        }
        // A blank row always separates the input from what is above it.
        rows.push(String::new());

        // Input, or the approval prompt in its place.
        let (mut cursor_row, mut cursor_col);
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
            for (i, (key, _, text)) in approval_choices(a).iter().enumerate() {
                let sel = Some(i) == a.selected;
                let label = truncate(&format!("{}. {text}", i + 1), w.saturating_sub(6));
                rows.push(format!(
                    "{}{}  {}",
                    if sel { paint(fg(BRIGHT), "› ") } else { "  ".into() },
                    paint(fg(if sel { BRIGHT } else { GRAY }), &key.to_string()),
                    if sel { paint(fg(BRIGHT).bold(), &label) } else { paint(fg(MID), &label) }
                ));
            }
            if a.noting || !a.note.is_empty() {
                // Show the end of a long note, where the cursor is.
                let room = w.saturating_sub(8).max(1);
                let shown: String = {
                    let chars: Vec<char> = a.note.chars().collect();
                    chars[chars.len().saturating_sub(room)..].iter().collect()
                };
                if a.noting {
                    cursor_row = rows.len();
                    cursor_col = 6 + visible_width(&shown);
                }
                rows.push(format!("{} {}", paint(fg(YELLOW), "note"), paint(fg(MID), &shown)));
            }
            rows.push(rule(w));
            let keys = if a.noting {
                vec![("Enter", "Done"), ("Esc", "Clear note")]
            } else {
                let mut keys = vec![("↑↓", "Select"), ("Enter", "Confirm"), ("Tab", "Note")];
                if scroll > 0 || more > 0 {
                    keys.push(("PgUp/PgDn", "Scroll"));
                }
                if a.review.is_some() {
                    keys.push(("v", "Review"));
                }
                keys.push(("Esc", "Interrupt"));
                keys
            };
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
            let view = self.editor.render(chrome::input_width(w));
            let busy = self.busy.is_some();
            let placeholder = self.editor.is_empty().then(|| chrome::placeholder(busy, &self.keys));
            let (framed, text_row, text_col) = chrome::input_box(&view.lines, placeholder.as_deref(), w, busy);
            let top = rows.len();
            rows.extend(framed);
            cursor_row = top + text_row + view.cursor_row;
            cursor_col = text_col + view.cursor_col;
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
            } else {
                let at = rows.len();
                if !self.menu_rows(&mut rows) {
                    rows.push(self.footer_line());
                } else if chrome::boxed(w) && rows.get(at).is_some_and(|r| super::render::strip_ansi(r).chars().all(|c| c == '─')) {
                    // The box already closes the input: no rule above the menu.
                    rows.remove(at);
                }
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
        chrome::status_line(&self.footer, self.width, &self.upgrade_label())
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
            menu: self.menu.is_some() || self.settings.is_some() || self.modes.is_some() || self.skills.is_some(),
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
                        detail: sanitize_text(&match crate::settings::source(spec.id) {
                            Some(layer) => format!("{} · {}", crate::settings::value(spec.id, &s, &live), layer.label()),
                            None => crate::settings::value(spec.id, &s, &live),
                        }),
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
            } else if let Some(m) = &self.help {
                let visible = m.rows();
                let tabs: Vec<String> = std::iter::once(None)
                    .chain(crate::slash::Category::ALL.iter().copied().map(Some))
                    .map(|c| {
                        let label = c.map_or("All", |c| c.label());
                        if c == m.category {
                            paint(fg(BRIGHT), &format!("[{label}]"))
                        } else {
                            paint(fg(GRAY), label)
                        }
                    })
                    .collect();
                let prefix = format!(
                    "{} {}  {}  ",
                    paint(fg(BRIGHT).bold(), "Help"),
                    paint(fg(GRAY), &visible.len().to_string()),
                    tabs.join(" ")
                );
                let filter_text = if m.filter.is_empty() {
                    "Type to filter"
                } else {
                    m.filter.as_str()
                };
                let header = fit_header(prefix, filter_text, m.filter.is_empty(), w);
                let items = visible
                    .iter()
                    .map(|e| MenuItem {
                        label: e.command.clone(),
                        detail: sanitize_text(&e.description),
                        tag: e.category.label().to_string(),
                    })
                    .collect();
                (
                    header,
                    items,
                    m.selected,
                    vec![
                        ("↑↓", "Navigate"),
                        ("Tab", "Category"),
                        ("Enter", "Run"),
                        ("Esc", "Close"),
                    ],
                )
            } else if let Some(m) = &self.skills {
                let visible = m.rows();
                let tabs: Vec<String> = std::iter::once(None)
                    .chain(m.groups.iter().copied().map(Some))
                    .map(|g| {
                        let label = g.unwrap_or("All");
                        if g == m.group {
                            paint(fg(BRIGHT), &format!("[{label}]"))
                        } else {
                            paint(fg(GRAY), label)
                        }
                    })
                    .collect();
                let prefix = format!(
                    "{} {}  {}  ",
                    paint(fg(BRIGHT).bold(), "Skills"),
                    paint(fg(GRAY), &visible.len().to_string()),
                    tabs.join(" ")
                );
                let filter_text = if m.filter.is_empty() {
                    "Type to filter"
                } else {
                    m.filter.as_str()
                };
                let header = fit_header(prefix, filter_text, m.filter.is_empty(), w);
                let items = visible
                    .iter()
                    .map(|e| MenuItem {
                        label: sanitize_text(&e.skill.id),
                        detail: one_line(&e.skill.description, 200),
                        tag: format!("{} · {}", e.group, e.skill.scope),
                    })
                    .collect();
                (
                    header,
                    items,
                    m.selected,
                    // Seven hints do not fit 80 columns; ↑↓ goes without saying.
                    vec![
                        ("Tab", "Source"),
                        ("Enter", "Use"),
                        ("→", "View"),
                        ("Del", "Remove"),
                        ("^N", "Install"),
                        ("Esc", "Close"),
                    ],
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
            } else if let Some(found) = self.file_matches().filter(|_| self.approval.is_none()) {
                let header = format!(
                    "{} {}",
                    paint(fg(BRIGHT).bold(), "Files"),
                    paint(fg(GRAY), &found.len().to_string())
                );
                let items = found
                    .into_iter()
                    .map(|f| MenuItem {
                        label: sanitize_text(&f),
                        detail: String::new(),
                        tag: String::new(),
                    })
                    .collect();
                (
                    header,
                    items,
                    self.file_selected,
                    vec![("↑↓", "Navigate"), ("Tab", "Insert"), ("Esc", "Close")],
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
                let tags = crate::settings::current().slash_menu_categories;
                let items = s
                    .into_iter()
                    .map(|s| MenuItem {
                        label: s.label,
                        detail: s.detail,
                        tag: if tags { s.tag } else { String::new() },
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
        if self.help.is_some() && items.is_empty() {
            rows.push(paint(fg(GRAY), "  No matching commands."));
        }
        if let Some(m) = &self.skills {
            if items.is_empty() {
                rows.push(paint(fg(GRAY), "  No matching skills. Ctrl+N installs one."));
            }
            let note = match (&m.confirm, &m.notice) {
                (Some(id), _) => {
                    let path = m.entry(id).map(|e| crate::tools::slash_path(&e.skill.dir)).unwrap_or_default();
                    Some(format!("Remove skill {id} ({path})? Enter or y removes it, any other key keeps it."))
                }
                (None, notice) => notice.clone(),
            };
            if let Some(note) = note {
                let line = one_line(&sanitize_text(&note), w.saturating_sub(3).max(1));
                rows.push(paint(fg(YELLOW), &format!("  {line}")));
            }
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
            let _ = self.out.write_all(chrome::fold(r).as_bytes());
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

    /// Busy time without the waits for the user.
    fn working_time(&self, start: Instant) -> Duration {
        let waiting = self.pause_start.map_or(Duration::ZERO, |t| t.elapsed());
        start.elapsed().saturating_sub(self.paused + waiting)
    }

    /// Stop the turn clock while an approval or question waits for the user.
    fn sync_clock(&mut self) {
        let waiting = self.busy.is_some() && (self.approval.is_some() || self.question.is_some());
        match (waiting, self.pause_start) {
            (true, None) => self.pause_start = Some(Instant::now()),
            (false, Some(t)) => {
                self.paused += t.elapsed();
                self.pause_start = None;
            }
            _ => {}
        }
    }

    /// "• title · nyq" while working, "title · nyq" otherwise; the title is
    /// the session's, else the workspace folder.
    fn window_title(&self) -> String {
        let folder = std::path::Path::new(&self.footer.cwd).file_name().map(|f| f.to_string_lossy().to_string());
        let base = Some(self.footer.title.clone()).filter(|t| !t.trim().is_empty()).or(folder).unwrap_or_default();
        let base = one_line(&base, 60);
        let state = if self.busy.is_some() { "• " } else { "" };
        if base.is_empty() { format!("{state}nyq") } else { format!("{state}{base} · nyq") }
    }

    fn sync_window_title(&mut self) {
        let title = self.window_title();
        if title != self.window_title {
            let _ = write!(self.out, "\x1b]0;{title}\x07");
            self.window_title = title;
        }
    }

    pub fn redraw(&mut self) {
        self.sync_clock();
        self.sync_window_title();
        if self.viewer.is_some() {
            // The viewer covers the screen; it follows new output.
            return self.draw_viewer(false);
        }
        self.clear_pane();
        self.draw_pane();
    }

    /// Print finished rows into the scrollback above the pane, wrapped to
    /// the width with a hanging indent.
    fn emit(&mut self, rows: &[String]) {
        if self.viewer.is_some() {
            self.deferred.extend_from_slice(rows);
            return self.draw_viewer(false);
        }
        self.clear_pane();
        for r in rows {
            for line in wrap_row(r, self.width) {
                let _ = self.out.write_all(chrome::fold(&line).as_bytes());
                let _ = self.out.write_all(b"\r\n");
            }
        }
        self.recent.extend(rows.iter().cloned());
        let excess = self.recent.len().saturating_sub(RECENT_ROWS);
        self.recent.drain(..excess);
        self.draw_pane();
    }

    /// The screen after a resize: the latest rows wrapped to the new width
    /// and the pane below them, painted from the top. Rows the terminal
    /// reflowed (or tore) on screen are overwritten instead of left behind.
    fn reflow(&mut self) {
        if self.viewer.is_some() {
            return self.draw_viewer(false);
        }
        let room = self.height.saturating_sub(self.pane().0.len());
        let mut rows = std::collections::VecDeque::new();
        for r in self.recent.iter().rev() {
            if rows.len() >= room {
                break;
            }
            for line in wrap_row(r, self.width).into_iter().rev() {
                rows.push_front(line);
            }
        }
        let excess = rows.len().saturating_sub(room);
        rows.drain(..excess);
        let _ = queue!(
            self.out,
            terminal::BeginSynchronizedUpdate,
            cursor::MoveTo(0, 0),
            terminal::Clear(terminal::ClearType::FromCursorDown)
        );
        for r in rows {
            let _ = self.out.write_all(chrome::fold(&r).as_bytes());
            let _ = self.out.write_all(b"\r\n");
        }
        self.pane_rows = 0;
        self.cursor_row = 0;
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

    /// The startup banner, sized to the terminal, with a tip that changes
    /// from launch to launch.
    pub fn print_banner(&mut self, version: &str) {
        let seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as usize);
        let rows = chrome::banner(version, &self.footer, &self.keys, self.width, seed);
        self.print_styled_info(&rows.join("\n"), Style::new());
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
        let mut rows = self.begin(Block::User);
        for (i, line) in text.trim().lines().enumerate() {
            let lead = if i == 0 { paint(fg(chrome::ACCENT).bold(), "›") } else { " ".into() };
            rows.push(format!("{lead} {}", paint(fg(BRIGHT).bold(), line)));
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

    /// Markdown line, indented, with framed and highlighted code blocks.
    fn render_line(&mut self, line: &str) -> String {
        if let Some(f) = &mut self.fence {
            if f.closes(line) {
                self.fence = None;
                return format!("  {}", chrome::fence_close());
            }
            return format!("  {}", f.code_line(line, true));
        }
        if let Some(f) = Fence::open(line) {
            let row = format!("  {}", chrome::fence_open(&sanitize_text(&f.lang)));
            self.fence = Some(f);
            return row;
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
        if self.busy.is_some() {
            let kind = tool_kind(tool);
            match self.tally.iter_mut().find(|(k, _)| *k == kind) {
                Some((_, n)) => *n += 1,
                None => self.tally.push((kind, 1)),
            }
        }
        let (verb, target) = describe_tool(tool, summary);
        self.tools.push(ToolRow {
            tool: tool.into(),
            verb,
            target,
            note: String::new(),
            detail: Vec::new(),
            status: Status::Running,
            diff: None,
            started: Instant::now(),
            took: None,
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
        t.took = Some(t.started.elapsed());
        if let Some(d) = output.display.as_deref().filter(|d| looks_like_diff(d)) {
            t.note = diff_stat(d);
            if !output.is_error && matches!(t.tool.as_str(), "edit_file" | "write_file") {
                t.diff = Some(sanitize_text(d));
            }
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
        self.paused = Duration::ZERO;
        self.pause_start = None;
        self.tally.clear();
        self.turn_tokens = (0, 0);
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

    /// Ctrl+Z: give the terminal back to the shell and stop (job control);
    /// on `fg`, take it again and repaint.
    #[cfg(unix)]
    fn suspend(&mut self) {
        self.clear_pane();
        let _ = self.out.flush();
        let enhanced = KEYBOARD_ENHANCED.load(Ordering::SeqCst);
        restore_terminal();
        // SAFETY: raising a signal in our own process; the default action
        // stops it until SIGCONT.
        unsafe {
            libc::raise(libc::SIGTSTP);
        }
        let _ = terminal::enable_raw_mode();
        let _ = execute!(self.out, crossterm::event::EnableBracketedPaste);
        if enhanced {
            let _ = execute!(
                self.out,
                PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
            );
            KEYBOARD_ENHANCED.store(true, Ordering::SeqCst);
        }
        self.redraw();
    }

    /// Open the model switcher; `models` arrive later via `set_models`.
    pub fn open_models(&mut self) {
        self.sessions = None;
        self.settings = None;
        self.modes = None;
        self.help = None;
        self.skills = None;
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

    /// Open the model switcher on already listed models, filtered by `filter`.
    pub fn show_models(&mut self, choices: Vec<ModelChoice>, warnings: Vec<String>, filter: &str) {
        self.open_models();
        self.set_models(choices, warnings);
        if let Some(m) = &mut self.menu {
            m.filter = filter.to_string();
            m.selected = 0;
        }
        self.redraw();
    }

    /// Open the session picker over `all` (as from `session::list`).
    pub fn open_sessions(&mut self, all: Vec<Meta>, cwd: &std::path::Path, current: Option<&str>) {
        self.menu = None;
        self.settings = None;
        self.modes = None;
        self.help = None;
        self.skills = None;
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
            models: f.models.clone(),
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
        self.help = None;
        self.skills = None;
        self.settings = Some(super::settings::SettingsMenu::new(category));
        self.redraw();
    }

    /// Open the permission-mode switcher.
    pub fn open_permission_modes(&mut self) {
        self.menu = None;
        self.sessions = None;
        self.settings = None;
        self.help = None;
        self.skills = None;
        self.modes = Some(super::mode_menu::ModeMenu::new());
        self.redraw();
    }

    /// Open the `/help` menu over every command the editor knows.
    pub fn open_help(&mut self) {
        self.menu = None;
        self.sessions = None;
        self.settings = None;
        self.modes = None;
        self.skills = None;
        self.help = Some(super::help_menu::HelpMenu::new(self.editor.commands()));
        self.redraw();
    }

    /// Open the `/skills` menu over `entries`.
    pub fn open_skills(&mut self, entries: Vec<super::skills_menu::Entry>) {
        self.menu = None;
        self.sessions = None;
        self.settings = None;
        self.modes = None;
        self.help = None;
        self.skills = Some(super::skills_menu::SkillsMenu::new(entries));
        self.redraw();
    }

    fn on_skills_key(&mut self, key: KeyEvent, commands: &mpsc::UnboundedSender<Command>) {
        use super::skills_menu::Action;
        let Some(m) = &mut self.skills else { return };
        let known: Vec<String> = self.editor.commands().iter().map(|(c, _)| c.clone()).collect();
        match m.on_key(key, |c| known.iter().any(|k| k == c)) {
            Action::None => {}
            Action::Close => self.skills = None,
            Action::Insert(text) => {
                self.skills = None;
                self.editor.set_text(&text);
            }
            Action::View(id) => {
                let Some(e) = m.entry(&id) else { return };
                let body = crate::skills::raw_text(&e.skill).unwrap_or_else(|err| err);
                let text = format!("{}\n\n{}", e.info.join("\n"), body.trim_end());
                // The menu stays open under the viewer.
                return self.open_review(&format!("Skill {id}"), &text);
            }
            Action::Run(cmd) => {
                self.skills = None;
                if self.is_busy() {
                    self.hint = Some(format!("{cmd} runs when the current task finishes"));
                    self.queue.push((Queued::FollowUp, cmd));
                } else {
                    let _ = commands.send(Command::Submit(cmd));
                }
            }
        }
        self.redraw();
    }

    fn on_help_key(&mut self, key: KeyEvent, commands: &mpsc::UnboundedSender<Command>) {
        use super::help_menu::Action;
        let Some(m) = &mut self.help else { return };
        match m.on_key(key) {
            Action::None => {}
            Action::Close => self.help = None,
            Action::Insert(text) => {
                self.help = None;
                self.editor.set_text(&text);
            }
            Action::Run(cmd) => {
                self.help = None;
                self.remember(&cmd);
                if self.is_busy() {
                    self.hint = Some(format!("{cmd} runs when the current task finishes"));
                    self.queue.push((Queued::FollowUp, cmd));
                } else {
                    let _ = commands.send(Command::Submit(cmd));
                }
            }
        }
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
                        Ok((saved, notice)) => {
                            self.footer.segments = saved.statusline;
                            crate::agent::set_auto_compact_percent(saved.auto_compact);
                            super::theme::set_colors(&saved.theme_colors);
                            self.hint = notice;
                        }
                        Err(e) => self.hint = Some(format!("Could not save settings: {e:#}")),
                    }
                    if id == Id::Theme {
                        super::theme::apply_setting(&value);
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
    /// The image shows as an `[Image #N]` token in the input; deleting the
    /// token drops the image.
    pub fn attach(&mut self, image: crate::types::Image, label: &str) {
        let label = sanitize_text(label);
        let id = self.attachments.iter().map(|a| a.id).max().unwrap_or(0) + 1;
        let a = Attachment { id, label, image };
        self.editor.insert_token(&a.token());
        self.hint = Some(format!("Attached {} · /images lists attachments", a.token()));
        self.attachments.push(a);
        self.redraw();
    }

    /// Files matching the `@word` at the cursor while the picker is open.
    fn file_matches(&self) -> Option<Vec<String>> {
        let query = self.editor.at_query().filter(|_| !self.files_closed)?;
        let found = super::files::matches(self.files.as_ref()?, &query, 50);
        (!found.is_empty()).then_some(found)
    }

    /// Drop images whose token is no longer in `text`.
    fn prune_attachments(&mut self, text: &str) {
        self.attachments.retain(|a| text.contains(&a.token()));
    }

    /// Remove an attachment and its token from the input.
    pub fn detach(&mut self, id: usize) -> Option<Attachment> {
        let i = self.attachments.iter().position(|a| a.id == id)?;
        let a = self.attachments.remove(i);
        self.editor.remove_token(&a.token());
        Some(a)
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
        // Mouse reporting only while the viewer is open, for the wheel.
        let _ = execute!(self.out, terminal::EnterAlternateScreen, cursor::Hide, crossterm::event::EnableMouseCapture);
        self.viewer = Some(super::transcript::Viewer::new(
            self.review.as_ref().unwrap_or(&self.log),
            self.width,
            self.height,
        ));
        self.draw_viewer(true);
    }

    /// Show a long approval preview full screen, from its top.
    fn open_review(&mut self, title: &str, text: &str) {
        let mut log = super::transcript::Log::default();
        let (verb, target) = title.split_once(' ').unwrap_or((title, ""));
        log.tool_start(verb, target);
        log.tool_end(text, Some(text), false);
        self.review = Some(log);
        self.open_transcript();
        if let Some(v) = &mut self.viewer {
            v.top = 0;
        }
        self.draw_viewer(true);
    }

    /// Leave the viewer and restore the inline UI with anything printed meanwhile.
    pub fn close_transcript(&mut self) {
        self.review = None;
        if self.viewer.take().is_none() {
            return;
        }
        if TRANSCRIPT_ALT_SCREEN.swap(false, Ordering::SeqCst) {
            let _ = execute!(self.out, crossterm::event::DisableMouseCapture, terminal::LeaveAlternateScreen, cursor::Show);
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
        let log = self.review.as_ref().unwrap_or(&self.log);
        if !v.refresh(log, self.width, self.height) && !force {
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
            let _ = self.out.write_all(chrome::fold(row).as_bytes());
        }
        let _ = queue!(self.out, terminal::EndSynchronizedUpdate);
        let _ = self.out.flush();
    }

    /// Use `keys` for the global and editor actions.
    #[cfg(test)]
    fn set_keys(&mut self, keys: Keymap) {
        let keys = Arc::new(keys);
        self.editor.set_keys(keys.clone());
        self.keys = keys;
    }

    fn on_viewer_key(&mut self, key: KeyEvent) {
        let Some(v) = &mut self.viewer else { return };
        // The key that opened the transcript also closes it.
        if self.keys.matches(KeyAction::ExpandTools, &key) && !v.searching() {
            return self.close_transcript();
        }
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
        // Ctrl+G aborts a Ctrl+R history search (it restores the draft).
        // Otherwise `reload` asks to reload an installed upgrade.
        let ctrl_g = key.code == KeyCode::Char('g') && key.modifiers == KeyModifiers::CONTROL;
        if self.keys.matches(KeyAction::Reload, &key) && !(ctrl_g && self.editor.searching()) {
            return self.request_upgrade(commands);
        }
        if self.approval.is_some() && self.viewer.is_some() {
            // The full-screen review (or transcript) is over the prompt.
            return self.on_viewer_key(key);
        }
        if let Some(mut a) = self.approval.take() {
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL)
                && !key.modifiers.contains(KeyModifiers::ALT);
            if a.noting && !ctrl {
                match key.code {
                    KeyCode::Tab | KeyCode::Enter => a.noting = false,
                    KeyCode::Esc => {
                        a.noting = false;
                        a.note.clear();
                    }
                    KeyCode::Backspace => {
                        a.note.pop();
                    }
                    KeyCode::Char(ch) if text_key(key, ch) => a.note.push(ch),
                    _ => {}
                }
                self.approval = Some(a);
                self.redraw();
                return;
            }
            if key.code == KeyCode::Tab {
                a.noting = true;
                self.approval = Some(a);
                self.redraw();
                return;
            }
            if key.code == KeyCode::Esc
                || (ctrl && matches!(key.code, KeyCode::Char('d') | KeyCode::Char('c')))
            {
                let _ = a.reply.send((Approval::No, None));
                if let Some(c) = &self.cancel {
                    c.cancel();
                }
                if ctrl && key.code == KeyCode::Char('d') {
                    let _ = commands.send(Command::Exit);
                }
                self.redraw();
                return;
            }
            let choices = approval_choices(&a);
            let n = choices.len();
            let page = MAX_PREVIEW - 1;
            let mut moved = true;
            match key.code {
                KeyCode::Up => a.selected = Some(a.selected.map_or(n - 1, |i| (i + n - 1) % n)),
                KeyCode::Down => a.selected = Some(a.selected.map_or(0, |i| (i + 1) % n)),
                KeyCode::PageUp => a.scroll = a.scroll.saturating_sub(page),
                KeyCode::PageDown => a.scroll = a.scroll.saturating_add(page),
                _ => moved = false,
            }
            if moved {
                self.approval = Some(a);
                return self.redraw();
            }
            let plain = key.modifiers.difference(KeyModifiers::SHIFT).is_empty();
            let review = (plain && matches!(key.code, KeyCode::Char('v' | 'V')))
                || (ctrl && key.code == KeyCode::Char('o'))
                || self.keys.matches(KeyAction::ExpandTools, &key);
            if a.review.is_some() && review {
                let (title, text) = (super::render::strip_ansi(&a.title), a.review.clone().unwrap_or_default());
                self.approval = Some(a);
                return self.open_review(&title, &text);
            }
            let answer = match key.code {
                KeyCode::Char(_) if !plain => None,
                KeyCode::Enter => a.selected.and_then(|i| choices.get(i)).map(|c| c.1),
                KeyCode::Char(d @ '1'..='9') => choices.get(d as usize - '1' as usize).map(|c| c.1),
                KeyCode::Char('y') | KeyCode::Char('Y') => Some(Approval::Yes),
                KeyCode::Char('a') | KeyCode::Char('A') => Some(Approval::Always),
                KeyCode::Char('s') | KeyCode::Char('S') if a.save_rule.is_some() => {
                    Some(Approval::AlwaysSave)
                }
                KeyCode::Char('l') | KeyCode::Char('L') if a.save_rule.is_some() => {
                    Some(Approval::AlwaysSaveLocal)
                }
                KeyCode::Char('n') | KeyCode::Char('N') => Some(Approval::No),
                _ => None,
            };
            match answer {
                Some(ans) => {
                    let note = Some(a.note.trim().to_string()).filter(|n| !n.is_empty());
                    let _ = a.reply.send((ans, note));
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
        if self.help.is_some() {
            return self.on_help_key(key, commands);
        }
        if self.skills.is_some() {
            return self.on_skills_key(key, commands);
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
                    let typed = if q.menu.is_some() { String::new() } else { self.editor.submitted_text() };
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
        match self.keys.action(super::keys::Context::Global, &key) {
            Some(KeyAction::ExpandTools) => return self.open_transcript(),
            Some(KeyAction::PasteImage) => {
                self.clipboard_request = true;
                self.hint = Some("Reading the clipboard…".into());
                return self.redraw();
            }
            Some(KeyAction::ModelMenu) => {
                let _ = commands.send(Command::Submit("/model".into()));
                return;
            }
            Some(KeyAction::CycleEffort) => {
                let _ = commands.send(Command::CycleEffort);
                return;
            }
            Some(KeyAction::Suspend) => {
                #[cfg(unix)]
                self.suspend();
                return;
            }
            _ => {}
        }
        // The `@` file picker takes arrows, Tab, Enter and Esc while it shows.
        let searching = self.editor.searching();
        if let Some(found) = self.file_matches().filter(|_| !searching) {
            let n = found.len();
            match key.code {
                KeyCode::Up => {
                    self.file_selected = self.file_selected.checked_sub(1).unwrap_or(n - 1);
                    return self.redraw();
                }
                KeyCode::Down => {
                    self.file_selected = (self.file_selected + 1) % n;
                    return self.redraw();
                }
                KeyCode::Tab | KeyCode::Enter if key.modifiers.is_empty() => {
                    self.editor.complete_at(&found[self.file_selected.min(n - 1)]);
                    self.file_selected = 0;
                    return self.redraw();
                }
                KeyCode::Esc => {
                    self.files_closed = true;
                    return self.redraw();
                }
                _ => {}
            }
        }
        // The slash menu takes arrows and Enter while it is showing.
        let suggestions = if searching { Vec::new() } else { self.editor.suggestions() };
        if !suggestions.is_empty() {
            let pick = &suggestions[self.slash_selected.min(suggestions.len() - 1)];
            match key.code {
                KeyCode::Up => {
                    self.slash_selected = self
                        .slash_selected
                        .checked_sub(1)
                        .unwrap_or(suggestions.len() - 1);
                    self.slash_moved = true;
                    return self.redraw();
                }
                KeyCode::Down => {
                    self.slash_selected = (self.slash_selected + 1) % suggestions.len();
                    self.slash_moved = true;
                    return self.redraw();
                }
                // Right after the space that opened argument completion,
                // Enter still runs what was typed unless an option was picked.
                KeyCode::Enter
                    if key.modifiers.is_empty()
                        && !(pick.arg && self.editor.text().ends_with(' ') && !self.slash_moved) =>
                {
                    let fill = pick.fill.clone();
                    self.slash_selected = 0;
                    self.slash_moved = false;
                    if !pick.run {
                        self.editor.set_text(&format!("{fill} "));
                        return self.redraw();
                    }
                    self.editor.set_text("");
                    self.remember(&fill);
                    if self.is_busy() {
                        self.hint = Some(format!("{fill} runs when the current task finishes"));
                        self.queue.push((Queued::FollowUp, fill));
                    } else {
                        let _ = commands.send(Command::Submit(fill));
                    }
                    return self.redraw();
                }
                KeyCode::Tab => {
                    let fill = pick.fill.clone();
                    self.editor.set_text(&format!("{fill} "));
                    self.slash_selected = 0;
                    self.slash_moved = false;
                    return self.redraw();
                }
                KeyCode::Esc => {
                    self.editor.set_text("");
                    self.slash_selected = 0;
                    self.slash_moved = false;
                    return self.redraw();
                }
                _ => {}
            }
        }
        self.slash_moved = false;
        let busy = self.is_busy();
        let was_empty = self.editor.is_empty();
        self.hint = None;
        let action = self.editor.handle_key(key, busy);
        // An image lives as long as its token: deleting it drops the image.
        // Browsing history swaps the text without editing it.
        let browsing = matches!(key.code, KeyCode::Up | KeyCode::Down)
            || self.keys.matches(KeyAction::HistoryPrev, &key)
            || self.keys.matches(KeyAction::HistoryNext, &key);
        match &action {
            Action::Submit(text) | Action::FollowUp(text) => self.prune_attachments(text),
            _ if !browsing && !self.attachments.is_empty() => self.prune_attachments(&self.editor.text()),
            _ => {}
        }
        match action {
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
                if !busy && !self.editor.is_empty() {
                    if let Some(key) = self.keys.hint(KeyAction::Interrupt) {
                        self.hint = Some(format!("Press {key} again to clear the input"));
                    }
                }
            }
            Action::Copy(text) => {
                self.copy_osc52(&text);
                std::thread::spawn(move || copy_native(&text));
                self.hint = Some("Copied".into());
            }
            Action::CtrlC { exit } => {
                if busy && was_empty {
                    if let Some(c) = &self.cancel {
                        c.cancel();
                    }
                } else if exit && !busy {
                    let _ = commands.send(Command::Exit);
                } else if was_empty && !busy {
                    if let Some(key) = self.keys.hint(KeyAction::Clear) {
                        self.hint = Some(format!("Press {key} again to exit"));
                    }
                }
            }
            Action::Exit => {
                if let Some(c) = &self.cancel {
                    c.cancel();
                }
                let _ = commands.send(Command::Exit);
            }
            Action::Dequeue => {
                // The newest queued message comes back first, ahead of the draft.
                if let Some((_, text)) = self.queue.pop() {
                    let draft = self.editor.text();
                    self.editor.set_text(&if draft.is_empty() { text } else { format!("{text}\n\n{draft}") });
                }
            }
        }
        if let Some(status) = self.editor.search_status() {
            self.hint = Some(status);
        }
        self.slash_selected = self
            .slash_selected
            .min(self.editor.suggestions().len().saturating_sub(1));
        self.file_selected = 0;
        if self.editor.at_query().is_none() {
            self.files_closed = false;
        } else if self.files.is_none() {
            let cwd = std::env::current_dir().unwrap_or_default();
            self.files = Some(super::files::index(&cwd));
        }
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
        // Before anything else reads input: the background query's reply arrives there.
        super::theme::init(&crate::settings::current().theme, true);
        let enhanced = terminal::supports_keyboard_enhancement().unwrap_or(false);
        let mut out = crate::record_tape::stdout();
        execute!(out, crossterm::event::EnableBracketedPaste)?;
        // Save the window title (xterm title stack); Drop restores it.
        let _ = out.write_all(b"\x1b[22;0t");
        if crate::settings::current().startup_scrollback {
            // Scroll the shell output on screen (and the prompt line) into
            // the scrollback, so the session starts at the top.
            if let (Ok((_, row)), Ok((_, h))) = (cursor::position(), terminal::size()) {
                let _ = queue!(out, cursor::MoveTo(0, h.saturating_sub(1)));
                let _ = out.write_all("\n".repeat(row as usize + 1).as_bytes());
                let _ = execute!(out, cursor::MoveTo(0, 0));
            }
        }
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
                                if let Some(text) = pasted.filter(|_| s.approval.is_none() && s.menu.is_none() && s.modes.is_none() && s.skills.is_none() && s.sessions.is_none() && s.secret.is_none()) {
                                    s.editor.paste(text);
                                    s.redraw();
                                    previous = Some(Instant::now());
                                    continue;
                                }
                            }
                            // Typed secrets (API keys) never reach a tape.
                            if s.secret.is_none() {
                                if let Some(text) = crate::record_tape::key_text(&k) {
                                    crate::record_tape::input(&text);
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
                        Event::Mouse(m) => {
                            use crossterm::event::MouseEventKind;
                            let up = match m.kind {
                                MouseEventKind::ScrollUp => true,
                                MouseEventKind::ScrollDown => false,
                                _ => continue,
                            };
                            if let Some(v) = &mut s.viewer {
                                v.wheel(up);
                                s.draw_viewer(true);
                            }
                        }
                        Event::Resize(w, h) => {
                            s.width = (w as usize).max(1);
                            s.height = (h as usize).max(1);
                            s.reflow();
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

    /// Clear a masked prompt left up after sign-in finishes another way.
    pub fn dismiss_secret(&self) {
        let mut s = self.screen();
        if s.secret.take().is_some() {
            s.redraw();
        }
    }

    /// The `agent::Ui` for one run.
    pub fn ui(&self, cwd: PathBuf) -> TuiUi {
        TuiUi {
            screen: self.screen.clone(),
            cwd,
            note: None,
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
        {
            let mut out = crate::record_tape::stdout();
            let _ = out.write_all(b"\x1b]0;\x07\x1b[23;0t");
            let _ = out.flush();
        }
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
        let _ = execute!(out, crossterm::event::DisableMouseCapture, terminal::LeaveAlternateScreen);
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

/// Show the approval pane on `screen` and wait for the answer and any note
/// the user typed. `origin` names who asks when it isn't the main agent
/// (shown above the title).
async fn approve_on(
    screen: &Arc<Mutex<Screen>>,
    origin: Option<&str>,
    req: &ApprovalRequest<'_>,
) -> (Approval, Option<String>) {
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
        let mut review = None;
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
            review = Some(p.clone());
            if looks_like_diff(&p) {
                lines = numbered_diff(&p, true, w, "  ");
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
        let review = review.filter(|_| lines.len() > MAX_PREVIEW);
        s.approval = Some(PendingApproval {
            title,
            lines,
            prewrapped,
            scroll: 0,
            rule: req.rule.as_deref().map(|r| one_line(r, 300)),
            save_rule: req.save_rule.as_deref().map(|r| one_line(r, 300)),
            origin,
            note: String::new(),
            noting: false,
            selected: None,
            review,
            reply: tx,
        });
        s.redraw();
    }
    let guard = ClearApproval(Arc::clone(screen));
    let answer = rx.await.unwrap_or((Approval::No, None));
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
        approve_on(&self.screen, Some(origin), req).await.0
    }
    async fn approve_noted(&self, origin: &str, req: &ApprovalRequest<'_>) -> (Approval, Option<String>) {
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
    /// The note attached to the last approval answer.
    note: Option<String>,
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
            // Shown until output arrives (which starts a new block and label).
            StreamEvent::Retrying(status) => {
                s.flush_live();
                s.set_label(status);
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
        let (answer, note) = approve_on(&self.screen, None, req).await;
        self.note = note;
        if answer == Approval::No {
            // The loop reports the denial through tool_end.
            self.s().tool_start(req.tool, req.summary);
        }
        answer
    }

    fn approval_note(&mut self) -> Option<String> {
        self.note.take()
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
        s.turn_tokens.0 += usage.input_tokens;
        s.turn_tokens.1 += usage.output_tokens;
        s.footer.context_tokens = usage.input_tokens + usage.output_tokens;
        s.redraw();
    }

    fn notice(&mut self, msg: &str) {
        self.s().print_info(&format!("  {msg}"), fg(GRAY));
    }

    async fn ask(&mut self, question: &str, options: &[String]) -> Option<String> {
        ask_on(&self.screen, question, options).await
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

/// Ask on the interactive screen, if there is one: `None` when no terminal
/// is attached (print mode, ACP), else the answer (`None` inside if skipped).
/// Works while the agent loop is blocked inside a tool (MCP elicitation).
pub async fn ask_active(question: &str, options: &[String]) -> Option<Option<String>> {
    let screen = active()?;
    Some(ask_on(&screen, question, options).await)
}

/// Show `text` on the running tool's row and in the busy label (MCP progress).
pub fn tool_progress(text: &str) {
    let Some(screen) = active() else { return };
    let mut s = lock(&screen);
    let text = one_line(&sanitize_text(text), 80);
    let Some(t) = s.tools.iter_mut().rev().find(|t| t.status == Status::Running) else { return };
    t.note = text.clone();
    let label = format!("Running {} · {text}", t.tool.replace('_', " "));
    s.set_label(&label);
    s.redraw();
}

async fn ask_on(screen: &Arc<Mutex<Screen>>, question: &str, options: &[String]) -> Option<String> {
    let (tx, rx) = oneshot::channel();
    {
        let mut s = lock(screen);
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
    let guard = ClearQuestion(Arc::clone(screen));
    let answer = rx.await.ok().flatten();
    drop(guard);
    let mut s = lock(screen);
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
            note: String::new(),
            noting: false,
            selected: None,
            review: None,
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
            diff: None,
            started: Instant::now(),
            took: Some(Duration::from_secs(2)),
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
            note: String::new(),
            noting: false,
            selected: None,
            review: None,
            reply: tx,
        });
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        s.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &cmd_tx);
        assert_eq!(rx.try_recv().unwrap(), (Approval::No, None));
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
            note: String::new(),
            noting: false,
            selected: None,
            review: None,
            reply: tx,
        });
        s.on_key(
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL),
            &cmd_tx,
        );
        assert_eq!(rx.try_recv().unwrap(), (Approval::No, None));
        assert!(matches!(cmd_rx.try_recv().unwrap(), Command::Exit));
    }

    #[test]
    fn approval_shortcuts_require_plain_or_shift_keys() {
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();
        for ch in "yYaAsSlLnNvV1".chars() {
            for modifiers in [
                KeyModifiers::NONE, KeyModifiers::SHIFT, KeyModifiers::CONTROL,
                KeyModifiers::ALT, KeyModifiers::SUPER, KeyModifiers::HYPER,
                KeyModifiers::META, KeyModifiers::CONTROL | KeyModifiers::SHIFT,
                KeyModifiers::ALT | KeyModifiers::SHIFT,
            ] {
                let mut s = screen();
                let (tx, mut rx) = oneshot::channel();
                s.approval = Some(PendingApproval {
                    title: "Approve".into(),
                    lines: Vec::new(),
                    prewrapped: false,
                    scroll: 0,
                    rule: Some("bash:git status".into()),
                    save_rule: Some("bash:git status".into()),
                    origin: None,
                    note: String::new(),
                    noting: false,
                    selected: None,
                    review: Some("preview".into()),
                    reply: tx,
                });
                s.on_key(KeyEvent::new(KeyCode::Char(ch), modifiers), &cmd_tx);
                let plain = modifiers == KeyModifiers::NONE || modifiers == KeyModifiers::SHIFT;
                if plain && matches!(ch, 'v' | 'V') {
                    assert!(s.viewer.is_some());
                    assert!(s.approval.is_some());
                } else if plain {
                    let expected = match ch.to_ascii_lowercase() {
                        'y' | '1' => Approval::Yes,
                        'a' => Approval::Always,
                        's' => Approval::AlwaysSave,
                        'l' => Approval::AlwaysSaveLocal,
                        'n' => Approval::No,
                        _ => unreachable!(),
                    };
                    assert_eq!(rx.try_recv().unwrap(), (expected, None));
                } else {
                    assert!(rx.try_recv().is_err(), "{ch} {modifiers:?}");
                    assert!(s.approval.is_some());
                    assert!(s.viewer.is_none());
                }
            }
        }
    }

    #[test]
    fn rebound_keys_never_answer_an_approval() {
        let (keys, warnings) = super::super::keys::Keymap::parse(
            "reload = \"ctrl+y\"\nexpand_tools = \"alt+y\"\nsubmit = [\"enter\", \"ctrl+a\"]\nclear = \"ctrl+n\"",
        );
        assert!(warnings.is_empty(), "{warnings:?}");
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();
        let pending = |tx| PendingApproval {
            title: "Approve".into(),
            lines: Vec::new(),
            prewrapped: false,
            scroll: 0,
            rule: Some("bash:ls".into()),
            save_rule: Some("bash:ls".into()),
            origin: None,
            note: String::new(),
            noting: false,
            selected: None,
            review: Some("preview".into()),
            reply: tx,
        };
        for (code, modifiers) in [
            (KeyCode::Char('y'), KeyModifiers::CONTROL),
            (KeyCode::Char('y'), KeyModifiers::ALT),
            (KeyCode::Char('a'), KeyModifiers::CONTROL),
            (KeyCode::Char('n'), KeyModifiers::CONTROL),
            (KeyCode::Enter, KeyModifiers::NONE),
        ] {
            let mut s = screen();
            s.set_keys(keys.clone());
            let (tx, mut rx) = oneshot::channel();
            s.approval = Some(pending(tx));
            s.on_key(KeyEvent::new(code, modifiers), &cmd_tx);
            assert!(rx.try_recv().is_err(), "{code:?} {modifiers:?}");
            assert!(s.approval.is_some());
            // The rebound review key opens the review instead.
            assert_eq!(s.viewer.is_some(), modifiers == KeyModifiers::ALT);
        }
    }

    #[test]
    fn rebound_global_keys() {
        let (keys, _) = super::super::keys::Keymap::parse("expand_tools = \"f5\"\nmodel_menu = []");
        let mut s = screen();
        s.set_keys(keys);
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        s.on_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL), &cmd_tx);
        assert!(s.viewer.is_none());
        s.on_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL), &cmd_tx);
        assert!(cmd_rx.try_recv().is_err(), "model_menu is unbound");
        s.on_key(KeyEvent::new(KeyCode::F(5), KeyModifiers::NONE), &cmd_tx);
        assert!(s.viewer.is_some());
        s.on_key(KeyEvent::new(KeyCode::F(5), KeyModifiers::NONE), &cmd_tx);
        assert!(s.viewer.is_none(), "the same key closes it");
        s.on_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT), &cmd_tx);
        assert!(matches!(cmd_rx.try_recv().unwrap(), Command::CycleEffort));
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
            note: String::new(),
            noting: false,
            selected: None,
            review: None,
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
        assert_eq!(rx.try_recv().unwrap(), (Approval::AlwaysSave, None));
        assert!(s.approval.is_none());

        let (tx, mut rx) = oneshot::channel();
        s.approval = Some(pending(Some("mcp__wiki__ask"), tx));
        let text = super::super::render::strip_ansi(&s.pane().0.join("\n"));
        assert!(text.contains("save `mcp__wiki__ask` for this workspace only"), "{text}");
        s.on_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE), &cmd_tx);
        assert_eq!(rx.try_recv().unwrap(), (Approval::AlwaysSaveLocal, None));
    }

    #[test]
    fn help_menu_runs_and_slash_arguments_complete() {
        let mut s = screen();
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        s.editor = Editor::new(
            vec![],
            vec![("/help".into(), "show commands".into()), ("/permissions".into(), "perms".into())],
        );
        s.open_help();
        let text = super::super::render::strip_ansi(&s.pane().0.join("\n"));
        assert!(text.contains("Help 2") && text.contains("[All]") && text.contains("Security"), "{text}");
        for ch in "perm".chars() {
            s.on_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE), &cmd_tx);
        }
        s.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &cmd_tx);
        assert!(matches!(cmd_rx.try_recv(), Ok(Command::Submit(c)) if c == "/permissions"));
        assert!(s.help.is_none());

        // Right after the space, Enter runs what was typed; after picking, the pick.
        s.editor.set_text("/permissions ");
        s.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &cmd_tx);
        assert!(matches!(cmd_rx.try_recv(), Ok(Command::Submit(c)) if c == "/permissions "));
        s.editor.set_text("/permissions ");
        s.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &cmd_tx);
        s.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &cmd_tx);
        assert!(matches!(cmd_rx.try_recv(), Ok(Command::Submit(c)) if c == "/permissions auto"));
        s.editor.set_text("/permissions rem");
        s.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &cmd_tx);
        assert_eq!(s.editor.text(), "/permissions remember ", "options that need more input are inserted");
        assert!(cmd_rx.try_recv().is_err());
    }

    #[test]
    fn skills_menu_inserts_and_removes() {
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join(".claude/skills/pdf");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), "---\nname: pdf\ndescription: Fill PDF forms\n---\nbody\n").unwrap();
        let skills = crate::skills::discover_with(&t.path().join("home"), None, t.path());
        let entry = |managed| super::super::skills_menu::Entry {
            group: crate::skills::source(&skills[0]),
            managed,
            info: Vec::new(),
            skill: skills[0].clone(),
        };
        let mut s = screen();
        s.width = 120;
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        let key = |c: KeyCode| KeyEvent::new(c, KeyModifiers::NONE);
        s.open_skills(vec![entry(false)]);
        let text = super::super::render::strip_ansi(&s.pane().0.join("\n"));
        assert!(text.contains("Skills 1") && text.contains("[All] Claude Code"), "{text}");
        assert!(text.contains("pdf") && text.contains("Fill PDF forms") && text.contains("Claude Code · project"), "{text}");
        s.on_key(key(KeyCode::Delete), &cmd_tx);
        let text = super::super::render::strip_ansi(&s.pane().0.join("\n"));
        assert!(text.contains("only removes skills it installed"), "{text}");
        s.on_key(key(KeyCode::Enter), &cmd_tx);
        assert!(s.skills.is_none());
        assert_eq!(s.editor.text(), "/pdf ");

        s.open_skills(vec![entry(true)]);
        s.on_key(key(KeyCode::Delete), &cmd_tx);
        let text = super::super::render::strip_ansi(&s.pane().0.join("\n"));
        assert!(text.contains("Remove skill pdf"), "{text}");
        s.on_key(key(KeyCode::Char('y')), &cmd_tx);
        assert!(matches!(cmd_rx.try_recv(), Ok(Command::Submit(c)) if c == "/skills remove pdf"));
        assert!(s.skills.is_none());
    }

    #[test]
    fn tab_attaches_a_note_to_the_answer() {
        let mut s = screen();
        s.width = 80;
        s.height = 40;
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();
        let key = |c: KeyCode| KeyEvent::new(c, KeyModifiers::NONE);
        let (tx, mut rx) = oneshot::channel();
        s.approval = Some(PendingApproval {
            title: "Run cargo test".into(),
            lines: Vec::new(),
            prewrapped: false,
            scroll: 0,
            rule: None,
            save_rule: None,
            origin: None,
            note: String::new(),
            noting: false,
            selected: None,
            review: None,
            reply: tx,
        });
        s.on_key(key(KeyCode::Tab), &cmd_tx);
        // While typing a note, y/n are text, not answers.
        for ch in "use nextest".chars() {
            s.on_key(key(KeyCode::Char(ch)), &cmd_tx);
        }
        assert!(rx.try_recv().is_err());
        let text = super::super::render::strip_ansi(&s.pane().0.join("\n"));
        assert!(text.contains("note use nextest"), "{text}");
        s.on_key(key(KeyCode::Enter), &cmd_tx);
        s.on_key(key(KeyCode::Char('n')), &cmd_tx);
        assert_eq!(
            rx.try_recv().unwrap(),
            (Approval::No, Some("use nextest".to_string()))
        );

        // Esc while typing clears the note instead of interrupting.
        let (tx, mut rx) = oneshot::channel();
        s.approval = Some(PendingApproval {
            title: "t".into(),
            lines: Vec::new(),
            prewrapped: false,
            scroll: 0,
            rule: None,
            save_rule: None,
            origin: None,
            note: String::new(),
            noting: false,
            selected: None,
            review: None,
            reply: tx,
        });
        s.on_key(key(KeyCode::Tab), &cmd_tx);
        s.on_key(key(KeyCode::Char('x')), &cmd_tx);
        s.on_key(key(KeyCode::Esc), &cmd_tx);
        s.on_key(key(KeyCode::Char('y')), &cmd_tx);
        assert_eq!(rx.try_recv().unwrap(), (Approval::Yes, None));
    }

    fn pending_approval(tx: oneshot::Sender<(Approval, Option<String>)>, review: Option<String>) -> PendingApproval {
        PendingApproval {
            title: "Edited a.rs".into(),
            lines: (0..30).map(|i| format!("line {i}")).collect(),
            prewrapped: false,
            scroll: 0,
            rule: Some("edits".into()),
            save_rule: None,
            origin: None,
            note: String::new(),
            noting: false,
            selected: None,
            review,
            reply: tx,
        }
    }

    #[test]
    fn approval_choices_take_arrows_numbers_and_review() {
        let mut s = screen();
        s.width = 100;
        s.height = 50;
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();
        let key = |c| KeyEvent::new(c, KeyModifiers::NONE);
        let (tx, mut rx) = oneshot::channel();
        s.approval = Some(pending_approval(tx, Some("@@ -1 +1 @@\n-a\n+b".into())));
        s.on_key(key(KeyCode::Enter), &cmd_tx);
        assert!(rx.try_recv().is_err(), "Enter alone never approves");
        s.on_key(key(KeyCode::Down), &cmd_tx);
        s.on_key(key(KeyCode::Down), &cmd_tx);
        assert_eq!(s.approval.as_ref().unwrap().selected, Some(1));
        let text = super::super::render::strip_ansi(&s.pane().0.join("\n"));
        assert!(text.contains("› a  2. Always allow edits") && text.contains("v Review"), "{text}");
        s.on_key(key(KeyCode::PageDown), &cmd_tx);
        assert_eq!(s.approval.as_ref().unwrap().scroll, MAX_PREVIEW - 1);
        // The review covers the prompt; its keys go to the viewer until it closes.
        s.on_key(key(KeyCode::Char('v')), &cmd_tx);
        assert!(s.viewer.is_some() && s.review.is_some());
        s.on_key(key(KeyCode::Char('y')), &cmd_tx);
        assert!(rx.try_recv().is_err());
        s.on_key(key(KeyCode::Char('q')), &cmd_tx);
        assert!(s.viewer.is_none() && s.review.is_none() && s.approval.is_some());
        s.on_key(key(KeyCode::Enter), &cmd_tx);
        assert_eq!(rx.try_recv().unwrap(), (Approval::Always, None));

        let (tx, mut rx) = oneshot::channel();
        s.approval = Some(pending_approval(tx, None));
        s.on_key(key(KeyCode::Char('v')), &cmd_tx);
        assert!(s.viewer.is_none());
        s.on_key(key(KeyCode::Char('3')), &cmd_tx);
        assert_eq!(rx.try_recv().unwrap(), (Approval::No, None));
    }

    #[test]
    fn ctrl_g_aborts_history_search_instead_of_upgrading() {
        let mut s = screen();
        s.editor = Editor::new(vec!["cargo build".into()], Vec::new());
        s.editor.set_text("draft");
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        let ctrl = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL);
        s.on_key(ctrl('r'), &cmd_tx);
        assert!(s.editor.searching());
        for c in "cargo".chars() {
            s.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE), &cmd_tx);
        }
        assert_eq!(s.editor.text(), "cargo build");
        s.on_key(ctrl('g'), &cmd_tx);
        assert!(!s.editor.searching());
        assert_eq!(s.editor.text(), "draft");
        assert!(cmd_rx.try_recv().is_err());
        assert_ne!(
            s.hint.as_deref(),
            Some("submit or clear the current prompt before upgrading")
        );
    }

    #[test]
    fn edits_show_a_folded_numbered_diff_and_groups_can_collapse() {
        let mut s = screen();
        s.width = 50;
        s.height = 60;
        s.tool_start("edit_file", "a.rs");
        let d: String = "--- a/a.rs\n+++ b/a.rs\n@@ -1,20 +1,20 @@\n".to_string() + &(0..20).map(|i| format!("-old {i}\n+new {i}\n")).collect::<String>();
        s.tool_end(&ToolOutput { content: "Edited a.rs.".into(), display: Some(d), ..ToolOutput::default() });
        let mut rows = Vec::new();
        s.tool_rows(&mut rows, false);
        let plain: Vec<String> = rows.iter().map(|r| super::super::render::strip_ansi(r)).collect();
        assert!(plain[0].starts_with("✓ Edited a.rs +20 -20"), "{plain:?}");
        assert_eq!(plain[1], "  │  1 - old 0");
        assert_eq!(plain.last().unwrap(), "  │ … 28 more lines (ctrl+o to view)");
        assert_eq!(plain.len(), 2 + INLINE_DIFF_ROWS);
        s.tool_start("read_file", "b.rs");
        crate::settings::install(crate::settings::Settings { collapse_tool_calls: true, ..Default::default() });
        let mut rows = Vec::new();
        s.tool_rows(&mut rows, false);
        crate::settings::install(crate::settings::Settings::default());
        assert_eq!(rows.len(), 1, "{rows:?}");
    }

    #[test]
    fn working_line_tallies_and_pauses_for_prompts() {
        let mut s = screen();
        s.width = 120;
        s.height = 40;
        s.set_busy(Some("Working"), None);
        s.tool_start("read_file", "a");
        s.tool_start("grep", "x");
        s.tool_start("bash", "ls");
        s.turn_tokens = (1500, 20);
        let text = super::super::render::strip_ansi(&s.pane().0.join("\n"));
        let line = text.lines().find(|l| l.starts_with("⠋ Running bash")).unwrap_or_default();
        assert!(line.starts_with("⠋ Running bash · 0s · ↑1.5k ↓20 · 2 read, 1 command "), "{text}");
        assert!(line.ends_with(" esc to interrupt") && visible_width(line) == 120, "{text}");
        assert!(text.contains("├ ⠋ Read a") && text.contains("│ › Steer the current task"), "{text}");
        let (tx, _rx) = oneshot::channel();
        s.approval = Some(pending_approval(tx, None));
        s.redraw();
        assert!(s.pause_start.is_some());
        s.busy.as_mut().unwrap().1 -= Duration::from_secs(5);
        s.pause_start = Some(Instant::now() - Duration::from_secs(4));
        assert_eq!(s.working_time(s.busy.as_ref().unwrap().1).as_secs(), 1);
        s.approval = None;
        s.redraw();
        assert!(s.pause_start.is_none() && s.paused >= Duration::from_secs(4));
        assert_eq!(tally_text(&[("edit", 2), ("tool", 1)]), "2 edited, 1 tool");
    }

    #[test]
    fn plain_terminals_get_no_colors_or_unicode() {
        // NO_COLOR, and a locale that isn't UTF-8.
        super::super::theme::test_mode(true, true, true);
        let mut s = screen();
        s.width = 60;
        s.height = 40;
        s.footer.model = "m".into();
        s.footer.permission_mode = "yolo".into();
        s.footer.branch = Some("main".into());
        s.set_busy(Some("Working"), None);
        s.tool_start("edit_file", "a.rs");
        let diff = "@@ -1 +1 @@\n-old\n+new\n";
        s.tool_end(&ToolOutput { content: "Edited a.rs.".into(), display: Some(diff.into()), ..ToolOutput::default() });
        s.tool_start("bash", "ls");
        s.turn_tokens = (10, 2);
        let mut rows = s.pane().0;
        s.tool_rows(&mut rows, false);
        for r in &rows {
            let params = r.split("\x1b[").skip(1).flat_map(|e| e.split('m').next().unwrap_or("").split(';'));
            assert!(params.clone().all(|p| !matches!(p.parse::<u8>(), Ok(30..=49 | 90..=107))), "{r:?}");
            let folded = super::super::render::strip_ansi(&chrome::fold(r));
            assert!(folded.is_ascii(), "{folded:?}");
        }
        let text = super::super::render::strip_ansi(&chrome::fold(&rows.join("\n")));
        assert!(text.contains("| + Edited a.rs +1 -1") && text.contains("| | 1 - old"), "{text}");
        assert!(text.contains("| > Steer the current task") && text.contains("yolo - git:main"), "{text}");
    }

    #[test]
    fn window_title_follows_session_and_work() {
        let mut s = screen();
        s.footer.cwd = "/work/proj".into();
        assert_eq!(s.window_title(), "proj · nyq");
        s.footer.title = "Fix login\x07 bug".into();
        s.busy = Some(("Working".into(), Instant::now()));
        assert_eq!(s.window_title(), "• Fix login bug · nyq");
        s.footer.cwd.clear();
        s.footer.title.clear();
        assert_eq!(s.window_title(), "• nyq");
    }

    #[test]
    fn dropping_ask_clears_the_question() {
        let screen = Arc::new(Mutex::new(screen()));
        let mut ui = TuiUi {
            screen: Arc::clone(&screen),
            cwd: PathBuf::new(),
            note: None,
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
                note: String::new(),
                noting: false,
                selected: None,
                review: None,
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

    #[test]
    fn composer_images_queue_and_file_picker() {
        let mut s = screen();
        let (tx, _rx) = mpsc::unbounded_channel();
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        let img = || crate::types::Image { media_type: "image/png".into(), data: String::new() };
        // Images are tokens in the text; deleting a token drops its image.
        s.attach(img(), "a.png");
        s.attach(img(), "b.png");
        assert_eq!(s.editor.text(), "[Image #1] [Image #2] ");
        s.on_key(key(KeyCode::Backspace), &tx);
        s.on_key(key(KeyCode::Backspace), &tx);
        assert_eq!(s.attachments.iter().map(|a| a.id).collect::<Vec<_>>(), [1]);
        s.attach(img(), "c.png");
        assert_eq!(s.editor.text(), "[Image #1] [Image #2] ");
        assert_eq!(s.detach(1).map(|a| a.label).as_deref(), Some("a.png"));
        assert_eq!(s.editor.text(), "[Image #2]");
        s.on_key(key(KeyCode::Enter), &tx);
        assert_eq!(s.attachments.len(), 1, "sent with the message");

        // Alt+Up takes queued messages back one at a time, newest first.
        s.queue = vec![(Queued::Steer, "one".into()), (Queued::FollowUp, "two".into())];
        let alt_up = KeyEvent::new(KeyCode::Up, KeyModifiers::ALT);
        s.on_key(alt_up, &tx);
        assert_eq!((s.editor.text().as_str(), s.queue.len()), ("two", 1));
        s.on_key(alt_up, &tx);
        assert_eq!(s.editor.text(), "one\n\ntwo");
        assert!(s.queue.is_empty());

        // `@` opens the file picker; Tab inserts the pick, Esc closes it.
        s.files = Some(vec!["README.md".into(), "src/main.rs".into()]);
        s.editor.set_text("open @mai");
        assert_eq!(s.file_matches(), Some(vec!["src/main.rs".to_string()]));
        s.on_key(key(KeyCode::Tab), &tx);
        assert_eq!(s.editor.text(), "open @src/main.rs ");
        s.editor.set_text("@r");
        s.on_key(key(KeyCode::Down), &tx);
        assert_eq!(s.file_selected, 1);
        s.on_key(key(KeyCode::Esc), &tx);
        assert_eq!(s.file_matches(), None);
        s.on_key(key(KeyCode::Char('e')), &tx);
        assert_eq!(s.file_matches(), None, "stays closed for this word");
        s.on_key(key(KeyCode::Char(' ')), &tx);
        s.on_key(key(KeyCode::Char('@')), &tx);
        assert!(s.file_matches().is_some());
        s.on_key(key(KeyCode::Enter), &tx);
        assert_eq!(s.editor.text(), "@re @README.md ");
    }
}
