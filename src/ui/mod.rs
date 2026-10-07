//! Terminal frontends: the interactive pi-style TUI (`tui`) and a plain
//! renderer for `--print` and piped input.

pub mod editor;
mod render;
pub mod tui;

use std::io::{self, IsTerminal, Write};

use nu_ansi_term::{Color, Style};
use serde_json::Value;

use crate::agent::{Approval, ApprovalRequest, Ui};
use crate::tools::ToolOutput;
use crate::types::{Item, StreamEvent};
use render::{dim, one_line, paint};

fn color_err() -> bool {
    std::env::var_os("NO_COLOR").is_none() && io::stderr().is_terminal()
}

/// Non-interactive renderer: assistant text to stdout, tool activity to
/// stderr, approvals granted only when `yolo`.
pub struct PlainUi {
    pub yolo: bool,
    pub at_line_start: bool,
}

impl PlainUi {
    pub fn new(yolo: bool) -> Self {
        Self { yolo, at_line_start: true }
    }
}

#[async_trait::async_trait]
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
                self.at_line_start = t.ends_with('\n');
            }
        }
    }
    fn end_message(&mut self) {
        if !std::mem::replace(&mut self.at_line_start, true) {
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
    async fn approve(&mut self, req: &ApprovalRequest<'_>) -> Approval {
        if self.yolo {
            return Approval::Yes;
        }
        eprintln!("● {}  {} — denied (use --yolo to allow)", req.tool, one_line(req.summary, 120));
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
    for item in items {
        match item {
            Item::User { text, .. } => s.print_user(text),
            Item::Assistant { text } => s.print_markdown(text),
            Item::ToolCall { name, arguments, .. } => s.print_tool(name, &args_digest(arguments), None),
            Item::Compaction { .. } => s.print_info("  (earlier conversation compacted)", dim()),
            _ => {}
        }
    }
}

pub fn banner(version: &str) {
    let text = format!(
        "{} {}",
        paint(true, Style::new().bold().fg(Color::Rgb(238, 238, 238)), "𝒏yq"),
        paint(true, Style::new().fg(Color::Rgb(138, 138, 138)), &format!("v{version} · Run /help for commands · ctrl+l models · ctrl+d quit"))
    );
    info(&text);
}

/// A listing whose rows are cut to the terminal width.
pub fn list(lines: &[String]) {
    match tui::active() {
        Some(screen) => screen.lock().unwrap_or_else(|e| e.into_inner()).print_list(lines),
        None => lines.iter().for_each(|l| eprintln!("{l}")),
    }
}

pub fn info(msg: &str) {
    match tui::active() {
        Some(screen) => screen.lock().unwrap_or_else(|e| e.into_inner()).print_info(msg, Style::new()),
        None => eprintln!("{msg}"),
    }
}

pub fn error(msg: &str) {
    let style = Style::new().fg(Color::Red);
    match tui::active() {
        Some(screen) => screen.lock().unwrap_or_else(|e| e.into_inner()).print_error(msg),
        None => eprintln!("{}", paint(color_err(), style, &format!("error: {msg}"))),
    }
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
}
