//! The system prompt, plus any project instructions found in the workspace.

use std::path::Path;

const BASE: &str = "\
You are nyq, a coding agent running in the user's terminal. You help with \
software engineering tasks in the current workspace using the provided tools.

- Be concise. Lead with the answer or the change you made; skip preamble.
- Investigate before editing: read the relevant files, search the codebase.
- Make focused changes that match the surrounding code style. Prefer edit_file \
over rewriting whole files.
- Verify your work when practical (build, run tests) and report results honestly.
- Shell commands run without a sandbox; avoid destructive commands unless asked.
- If a tool call is denied, do not retry it; ask the user how to proceed.
- Format answers as GitHub-flavored markdown. Refer to code as path:line.";

/// Files whose contents are appended as project instructions, in order.
const INSTRUCTION_FILES: &[&str] = &["AGENTS.md", "CLAUDE.md", ".nyq/instructions.md"];

pub fn system(cwd: &Path) -> String {
    let mut s = String::from(BASE);
    s.push_str(&format!(
        "\n\nEnvironment:\n- Working directory: {}\n- Platform: {}\n- Date: {}",
        cwd.display(),
        std::env::consts::OS,
        chrono::Local::now().format("%Y-%m-%d"),
    ));
    if cwd.join(".git").exists() {
        s.push_str("\n- The workspace is a git repository.");
    }
    for name in INSTRUCTION_FILES {
        if let Ok(text) = std::fs::read_to_string(cwd.join(name)) {
            let text = text.trim();
            if !text.is_empty() {
                s.push_str(&format!("\n\n# Project instructions ({name})\n\n{text}"));
            }
        }
    }
    s
}
