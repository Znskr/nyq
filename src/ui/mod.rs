//! Terminal frontends: the interactive pi-style TUI (`tui`) and a plain
//! renderer for `--print` and piped input.

pub mod clipboard;
pub mod editor;
mod files;
pub mod help_menu;
pub mod history;
pub mod mode_menu;
mod highlight;
mod render;
pub mod theme;
pub mod settings;
pub mod statusline;
pub mod transcript;
pub mod tui;

use std::io::{self, IsTerminal, Write};

use nu_ansi_term::{Color, Style};
use serde_json::Value;

use crate::agent::{Approval, ApprovalRequest, Approver, Ui};
use crate::tools::ToolOutput;
use crate::types::{Item, StreamEvent};
use render::{dim, one_line, paint, sanitize_text};

fn color_err() -> bool {
    std::env::var_os("NO_COLOR").is_none() && io::stderr().is_terminal()
}

/// Non-interactive renderer: assistant text to stdout, tool activity to
/// stderr, approvals granted only when `yolo` (or asked on the terminal with
/// `prompt`).
pub struct PlainUi {
    pub yolo: bool,
    pub at_line_start: bool,
    /// Keep assistant text off stdout.
    pub quiet: bool,
    /// Ask y/N on stderr when stdin is a terminal instead of denying.
    pub prompt: bool,
    /// Collects the run for `--json`.
    pub record: Option<RunRecord>,
}

/// What `ask --json` reports about a run.
#[derive(Default, Debug)]
pub struct RunRecord {
    /// All assistant text.
    pub output: String,
    /// Assistant text after the last tool call: the final answer.
    pub final_output: String,
    pub tool_calls: Vec<Value>,
}

impl RunRecord {
    fn tool_start(&mut self, tool: &str, summary: &str) {
        self.final_output.clear();
        self.tool_calls
            .push(serde_json::json!({"name": tool, "summary": summary, "status": "running"}));
    }

    fn tool_end(&mut self, tool: &str, output: &ToolOutput) {
        let status = match (output.is_error, output.content.starts_with("The user denied")) {
            (false, _) => "ok",
            (true, true) => "denied",
            (true, false) => "error",
        };
        let running = self
            .tool_calls
            .iter()
            .rposition(|c| c["name"] == tool && c["status"] == "running");
        let index = running.unwrap_or_else(|| {
            self.tool_calls.push(serde_json::json!({"name": tool}));
            self.tool_calls.len() - 1
        });
        let entry = &mut self.tool_calls[index];
        entry["status"] = status.into();
        entry["output"] = clip(&output.content, 4000).into();
    }
}

/// At most `max` bytes of `s`, cut on a character boundary.
fn clip(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

impl PlainUi {
    pub fn new(yolo: bool) -> Self {
        Self {
            yolo,
            at_line_start: true,
            quiet: false,
            prompt: false,
            record: None,
        }
    }
}

/// Put a permission question to the person at the terminal (stderr/stdin).
async fn ask_terminal(question: String) -> Approval {
    let answer = tokio::task::spawn_blocking(move || {
        eprint!("{question} [y/N/a(lways)] ");
        let _ = io::stderr().flush();
        let mut line = String::new();
        io::stdin().read_line(&mut line).map(|_| line)
    })
    .await;
    match answer.ok().and_then(Result::ok).as_deref().map(str::trim) {
        Some("y" | "Y" | "yes") => Approval::Yes,
        Some("a" | "A" | "always") => Approval::Always,
        _ => Approval::No,
    }
}

fn can_prompt(prompt: bool) -> bool {
    prompt && io::stdin().is_terminal()
}

#[async_trait::async_trait]
impl Ui for PlainUi {
    fn notice(&mut self, msg: &str) {
        eprintln!("{}", sanitize_text(msg));
    }
    fn stream(&mut self, event: &StreamEvent) {
        if let StreamEvent::Retrying(status) = event {
            eprintln!("{}", sanitize_text(status));
        }
        if let StreamEvent::Text(t) = event {
            if let Some(r) = &mut self.record {
                r.output.push_str(t);
                r.final_output.push_str(t);
            }
            let t = sanitize_text(t);
            if !t.is_empty() && !self.quiet {
                let mut out = io::stdout().lock();
                let _ = out.write_all(t.as_bytes());
                let _ = out.flush();
                self.at_line_start = t.ends_with('\n');
            }
        }
    }
    fn end_message(&mut self) {
        if let Some(r) = &mut self.record {
            if !r.output.is_empty() && !r.output.ends_with('\n') {
                r.output.push('\n');
            }
        }
        if !std::mem::replace(&mut self.at_line_start, true) {
            println!();
        }
    }
    fn tool_start(&mut self, tool: &str, summary: &str) {
        if let Some(r) = &mut self.record {
            r.tool_start(tool, summary);
        }
        eprintln!("● {tool}  {}", one_line(summary, 120));
    }
    fn tool_end(&mut self, tool: &str, output: &ToolOutput) {
        if let Some(r) = &mut self.record {
            r.tool_end(tool, output);
        }
        if output.is_error {
            eprintln!("  {tool} failed: {}", one_line(&output.content, 160));
        }
    }
    async fn approve(&mut self, req: &ApprovalRequest<'_>) -> Approval {
        if self.yolo {
            return Approval::Yes;
        }
        if can_prompt(self.prompt) {
            let question = format!("Allow {}  {}?", req.tool, one_line(req.summary, 120));
            return ask_terminal(question).await;
        }
        eprintln!(
            "● {}  {} — denied (use --yolo to allow)",
            req.tool,
            one_line(req.summary, 120)
        );
        Approval::No
    }
    fn approver(&self) -> Option<std::sync::Arc<dyn Approver>> {
        Some(std::sync::Arc::new(PlainApprover {
            yolo: self.yolo,
            prompt: self.prompt,
        }))
    }
}

/// Print mode usually has nobody to ask: allow with --yolo, ask on the
/// terminal with --prompt-permissions, otherwise deny and say so.
struct PlainApprover {
    yolo: bool,
    prompt: bool,
}

#[async_trait::async_trait]
impl Approver for PlainApprover {
    async fn approve(&self, origin: &str, req: &ApprovalRequest<'_>) -> Approval {
        if self.yolo {
            return Approval::Yes;
        }
        if can_prompt(self.prompt) {
            let question = format!(
                "Allow {} · {}  {}?",
                one_line(origin, 60),
                req.tool,
                one_line(req.summary, 120)
            );
            return ask_terminal(question).await;
        }
        eprintln!(
            "● {} · {}  {} — denied (use --yolo to allow)",
            one_line(origin, 60),
            req.tool,
            one_line(req.summary, 120)
        );
        Approval::No
    }
}

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

/// Show a resumed transcript: user messages, answers and tool calls.
pub fn replay(items: &[Item]) {
    let Some(screen) = tui::active() else { return };
    let mut s = screen.lock().unwrap_or_else(|e| e.into_inner());
    // The transcript viewer shows the resumed conversation from here on.
    s.log.clear();
    for item in items {
        match item {
            Item::User { text, .. } => s.print_user(text),
            Item::Assistant { text } => s.print_markdown(text),
            Item::ToolCall {
                name, arguments, ..
            } => s.print_tool(name, &args_digest(arguments), None),
            Item::ToolResult {
                output, is_error, ..
            } => s.log.tool_end(output, None, *is_error),
            Item::Compaction { .. } => s.print_info("  (earlier conversation compacted)", dim()),
            _ => {}
        }
    }
}

fn banner_text(version: &str) -> String {
    format!(
        "{} {}",
        paint(
            true,
            theme::fg(theme::BRIGHT).bold(),
            "𝒏yq"
        ),
        paint(
            true,
            theme::fg(theme::GRAY),
            &format!(
                "v{} · Run /help for commands · ctrl+l models · ctrl+d quit",
                sanitize_text(version)
            )
        )
    )
}

pub fn banner(version: &str) {
    let text = banner_text(version);
    match tui::active() {
        Some(screen) => screen
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .print_styled_info(&text, Style::new()),
        None => eprintln!(
            "{}",
            if color_err() {
                text
            } else {
                sanitize_text(&text)
            }
        ),
    }
}

/// A listing whose rows are cut to the terminal width.
pub fn list(lines: &[String]) {
    match tui::active() {
        Some(screen) => screen
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .print_list(lines),
        None => lines.iter().for_each(|l| eprintln!("{}", sanitize_text(l))),
    }
}

/// The session title shown in the status line and the window title.
pub fn set_session_title(title: &str) {
    if let Some(screen) = tui::active() {
        let mut s = screen.lock().unwrap_or_else(|e| e.into_inner());
        s.footer.title = one_line(title, 200);
        s.redraw();
    }
}

pub fn info(msg: &str) {
    let msg = sanitize_text(msg);
    match tui::active() {
        Some(screen) => screen
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .print_info(&msg, Style::new()),
        None => eprintln!("{msg}"),
    }
}

pub fn error(msg: &str) {
    let msg = sanitize_text(msg);
    let style = Style::new().fg(Color::Red);
    match tui::active() {
        Some(screen) => screen
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .print_error(&msg),
        None => eprintln!("{}", paint(color_err(), style, &format!("error: {msg}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn banner_keeps_its_own_colors() {
        let text = banner_text("0.2\x1b]bad\x07");
        assert!(text.contains("\x1b["));
        assert!(!text.contains("bad"));
        assert!(sanitize_text(&text).starts_with("𝒏yq v0.2 ·"));
    }

    #[test]
    fn run_record_tracks_the_final_answer_and_tool_results() {
        let mut ui = PlainUi::new(false);
        ui.quiet = true;
        ui.record = Some(RunRecord::default());
        ui.stream(&StreamEvent::Text("Looking.".into()));
        ui.end_message();
        ui.tool_start("bash", "ls");
        ui.tool_end("bash", &ToolOutput::ok("a\nb"));
        // A denied call never started.
        ui.tool_end("write_file", &ToolOutput::err("The user denied this tool call."));
        ui.stream(&StreamEvent::Text("Two files.".into()));
        ui.end_message();
        let r = ui.record.unwrap();
        assert_eq!(r.output, "Looking.\nTwo files.\n");
        assert_eq!(r.final_output, "Two files.");
        assert_eq!(r.tool_calls[0]["status"], "ok");
        assert_eq!(r.tool_calls[0]["summary"], "ls");
        assert_eq!(r.tool_calls[1]["status"], "denied");
        assert_eq!(clip("ééé", 3), "é…");
    }

    #[test]
    fn digest_prefers_meaningful_keys() {
        assert_eq!(
            args_digest(r#"{"timeout":5,"command":"cargo test"}"#),
            "cargo test"
        );
        assert_eq!(args_digest(r#"{"x":{"y":1},"name":"hi"}"#), "hi");
        assert_eq!(args_digest("not json"), "not json");
    }
}
