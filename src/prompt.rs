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
- If a tool call is denied, do not retry it; ask the user how to proceed.
- Format answers as GitHub-flavored markdown. Refer to code as path:line.";

/// Files whose contents are appended as project instructions, in order.
const INSTRUCTION_FILES: &[&str] = &["AGENTS.md", "CLAUDE.md", ".nyq/instructions.md"];

fn sandbox_guidance(policy: &crate::sandbox::Policy) -> String {
    let described = crate::sandbox::describe(policy);
    let net = if policy.network { "" } else { ", and cannot open TCP connections" };
    match policy.mode {
        crate::sandbox::Mode::Off => format!("{described}. Avoid destructive commands unless asked."),
        crate::sandbox::Mode::ReadOnly => format!(
            "{described}. The shell cannot write files{net}. write_file and edit_file are refused."
        ),
        crate::sandbox::Mode::WorkspaceWrite => format!(
            "{described}. The shell can write only inside the workspace and temporary directories{net}. write_file and edit_file can write only inside the workspace."
        ),
    }
}

pub fn system(cwd: &Path, sandbox: &crate::sandbox::Policy) -> String {
    let mut s = String::from(BASE);
    s.push_str(&format!("\n- {}", sandbox_guidance(sandbox)));
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::{Mode, Policy};

    #[test]
    fn system_prompt_follows_the_sandbox() {
        let dir = tempfile::tempdir().unwrap();
        let off = system(dir.path(), &Policy { mode: Mode::Off, ..Policy::default() });
        assert!(off.contains("sandbox: off"));
        assert!(off.contains("Avoid destructive commands"));
        assert!(!off.contains("without a sandbox"));

        let ro = system(dir.path(), &Policy { mode: Mode::ReadOnly, network: false, ..Policy::default() });
        assert!(ro.contains("read-only"), "{ro}");
        assert!(ro.contains("cannot write files") || ro.contains("cannot open TCP"));
        assert!(!ro.contains("without a sandbox"));

        let ww = system(dir.path(), &Policy::default());
        assert!(ww.contains("workspace-write"), "{ww}");
        assert!(ww.contains("only inside the workspace"));
        assert!(!ww.contains("without a sandbox"));
    }
}
