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
    let net = if policy.network {
        ""
    } else if cfg!(target_os = "linux") {
        ". TCP is blocked; UDP and Unix-domain sockets stay unrestricted"
    } else {
        ", and cannot open network connections"
    };
    match policy.mode {
        crate::sandbox::Mode::Off => format!("{described}. Avoid destructive commands unless asked."),
        crate::sandbox::Mode::ReadOnly => format!(
            "{described}. Workspace writes are blocked{net}, but temporary directories remain writable. write_file and edit_file are refused."
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
    if !sandbox.writable.is_empty() {
        s.push_str(
            "\n\nAdditional workspace directories (writable, and inside the workspace for file tools). \
Relative paths still resolve from the working directory. Project instructions are read only from the working directory.\n",
        );
        for dir in &sandbox.writable {
            s.push_str(&format!("- {}\n", dir.display()));
        }
    }
    for name in INSTRUCTION_FILES {
        let path = cwd.join(name);
        if let Ok(text) = crate::tools::read_rooted_text(cwd, &path) {
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
        let off = system(
            dir.path(),
            &Policy {
                mode: Mode::Off,
                ..Policy::default()
            },
        );
        assert!(off.contains("sandbox: off"));
        assert!(off.contains("Avoid destructive commands"));
        assert!(!off.contains("without a sandbox"));

        let ro = system(
            dir.path(),
            &Policy {
                mode: Mode::ReadOnly,
                network: false,
                ..Policy::default()
            },
        );
        assert!(ro.contains("read-only"), "{ro}");
        assert!(ro.contains("temporary directories remain writable"), "{ro}");
        assert!(ro.contains("write_file and edit_file are refused"), "{ro}");
        assert!(!ro.contains("without a sandbox"));

        let ww = system(dir.path(), &Policy::default());
        assert!(ww.contains("workspace-write"), "{ww}");
        assert!(ww.contains("only inside the workspace"));
        assert!(!ww.contains("without a sandbox"));

        let extra = tempfile::tempdir().unwrap();
        let listed = system(
            dir.path(),
            &Policy {
                writable: vec![extra.path().to_path_buf()],
                ..Policy::default()
            },
        );
        assert!(listed.contains(extra.path().to_str().unwrap()), "{listed}");
        assert!(
            listed.contains("Additional workspace directories"),
            "{listed}"
        );
        assert!(listed.contains("Relative paths still resolve"), "{listed}");
    }

    #[test]
    fn project_instructions_are_regular_files_inside_the_workspace() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "Use tabs.\n").unwrap();
        let text = system(dir.path(), &Policy::default());
        assert!(text.contains("Use tabs."), "{text}");

        #[cfg(unix)]
        {
            let outside = tempfile::tempdir().unwrap();
            std::fs::write(outside.path().join("secret"), "SECRET_TOKEN_9f3a").unwrap();
            let linked = tempfile::tempdir().unwrap();
            std::os::unix::fs::symlink(
                outside.path().join("secret"),
                linked.path().join("AGENTS.md"),
            )
            .unwrap();
            let text = system(linked.path(), &Policy::default());
            assert!(!text.contains("SECRET_TOKEN_9f3a"), "{text}");
            if std::path::Path::new("/dev/zero").exists() {
                let zero = tempfile::tempdir().unwrap();
                std::os::unix::fs::symlink("/dev/zero", zero.path().join("CLAUDE.md")).unwrap();
                let text = system(zero.path(), &Policy::default());
                assert!(!text.contains("Project instructions (CLAUDE.md)"), "{text}");
            }
        }
    }
}
