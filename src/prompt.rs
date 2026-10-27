//! The system prompt, plus any project instructions found in the workspace.

use std::path::{Path, PathBuf};

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

/// Replaces the built-in base prompt: `<home>/system.md`, or the project's
/// `.nyq/system.md`, which wins.
const SYSTEM_FILE: &str = "system.md";
/// Added after the base prompt: the user's, then the project's.
const APPEND_FILE: &str = "append-system.md";

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

static BASE_OVERRIDE: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Replace the built-in base prompt for this run (`ask --system`), and the
/// `system.md` / `append-system.md` files with it; tool, skill, project and
/// environment context are still added.
pub fn set_base(text: String) {
    let _ = BASE_OVERRIDE.set(text);
}

/// A prompt file's text when it is a regular file under `root` and not blank.
fn read_prompt_file(root: &Path, path: &Path) -> Option<String> {
    let text = crate::tools::read_rooted_text(root, path).ok()?;
    let text = crate::limits::clip("project_instruction_file_bytes", text.trim());
    (!text.is_empty()).then_some(text)
}

type PromptFile = (PathBuf, String);

/// The active prompt files with their text: the `system.md` that replaces the
/// base prompt (project over user), then the `append-system.md` files (user,
/// then project).
fn prompt_files_in(cwd: &Path, home: &Path) -> (Option<PromptFile>, Vec<PromptFile>) {
    let project = cwd.join(".nyq");
    let read = |root: &Path, path: PathBuf| read_prompt_file(root, &path).map(|text| (path, text));
    // Started in the home directory, the project's files are the user's own.
    let own = same_dir(&project, home);
    let read_project = |name: &str| {
        if own {
            None
        } else {
            read(cwd, project.join(name))
        }
    };
    let base = read_project(SYSTEM_FILE).or_else(|| read(home, home.join(SYSTEM_FILE)));
    let appends = [
        read(home, home.join(APPEND_FILE)),
        read_project(APPEND_FILE),
    ];
    (base, appends.into_iter().flatten().collect())
}

fn same_dir(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// Paths of the active `system.md` / `append-system.md` files, for status.
pub fn prompt_files(cwd: &Path, home: &Path) -> Vec<PathBuf> {
    let (base, appends) = prompt_files_in(cwd, home);
    base.into_iter()
        .chain(appends)
        .map(|(path, _)| path)
        .collect()
}

/// The base prompt (built in, from `system.md`, or `ask --system`), the
/// sandbox guidance, then any `append-system.md` text.
fn base_prompt(cwd: &Path, home: &Path, sandbox: &crate::sandbox::Policy) -> String {
    let (base, appends) = match BASE_OVERRIDE.get() {
        Some(text) => (Some(text.clone()), Vec::new()),
        None => {
            let (base, appends) = prompt_files_in(cwd, home);
            (base.map(|(_, text)| text), appends)
        }
    };
    let mut s = base.unwrap_or_else(|| BASE.to_string());
    s.push_str(&format!("\n- {}", sandbox_guidance(sandbox)));
    for (_, text) in appends {
        s.push_str(&format!("\n\n{text}"));
    }
    s
}

pub fn system(cwd: &Path, sandbox: &crate::sandbox::Policy) -> String {
    let mut s = base_prompt(cwd, &crate::config::home(), sandbox);
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
Relative paths still resolve from the working directory. Project instructions come from the working directory and the parent directories up to the repository root.\n",
        );
        for dir in &sandbox.writable {
            s.push_str(&format!("- {}\n", dir.display()));
        }
    }
    let mut project = instructions_above(cwd, &crate::config::home());
    for name in INSTRUCTION_FILES {
        let path = cwd.join(name);
        if let Ok(text) = crate::tools::read_rooted_text(cwd, &path) {
            let text = crate::limits::clip("project_instruction_file_bytes", text.trim());
            if !text.is_empty() {
                project.push_str(&format!("\n\n# Project instructions ({name})\n\n{text}"));
            }
        }
    }
    s.push_str(&crate::limits::clip(
        "project_instructions_total_bytes",
        &project,
    ));
    let facts = crate::tools::memory::load(&crate::config::home()).unwrap_or_default();
    s.push_str(&crate::tools::memory::prompt_section(&facts));
    s
}

/// Instruction files outside the working directory: the user's global
/// `<home>/AGENTS.md`, then `AGENTS.md`/`CLAUDE.md` in each parent directory
/// from the repository root down (none when the workspace is not inside a
/// repository). Each file is read only from its own directory.
fn instructions_above(cwd: &Path, home: &Path) -> String {
    let mut s = String::new();
    let mut add = |dir: &Path, name: &str, label: String| {
        if let Ok(text) = crate::tools::read_rooted_text(dir, &dir.join(name)) {
            let text = crate::limits::clip("project_instruction_file_bytes", text.trim());
            if !text.is_empty() {
                s.push_str(&format!("\n\n# {label}\n\n{text}"));
            }
        }
    };
    add(home, "AGENTS.md", "User instructions (all projects)".into());
    let root = cwd
        .ancestors()
        .skip(1)
        .find(|dir| dir.join(".git").exists());
    if let Some(root) = root {
        let parents: Vec<&Path> = cwd
            .ancestors()
            .skip(1)
            .take_while(|dir| dir.starts_with(root))
            .collect();
        for (up, dir) in parents.iter().enumerate().rev() {
            for name in ["AGENTS.md", "CLAUDE.md"] {
                add(
                    dir,
                    name,
                    format!("Project instructions ({}{name})", "../".repeat(up + 1)),
                );
            }
        }
    }
    if !s.is_empty() {
        s.push_str(
            "\n\n(When instructions conflict, the user's direct requests come first, then the file closest to \
             the files being changed.)",
        );
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
    fn prompt_files_replace_and_extend_the_base() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let cwd = dir.path().join("work");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(cwd.join(".nyq")).unwrap();
        let policy = Policy::default();
        let plain = base_prompt(&cwd, &home, &policy);
        assert!(plain.starts_with("You are nyq"), "{plain}");
        assert!(prompt_files(&cwd, &home).is_empty());

        // A blank file changes nothing.
        std::fs::write(home.join("system.md"), "  \n").unwrap();
        assert_eq!(base_prompt(&cwd, &home, &policy), plain);

        std::fs::write(home.join("system.md"), "User base.").unwrap();
        std::fs::write(home.join("append-system.md"), "User extra.").unwrap();
        let text = base_prompt(&cwd, &home, &policy);
        assert!(text.starts_with("User base.\n- "), "{text}");
        assert!(text.ends_with("\n\nUser extra."), "{text}");
        assert!(!text.contains("You are nyq"), "{text}");

        std::fs::write(cwd.join(".nyq/system.md"), "Project base.").unwrap();
        std::fs::write(cwd.join(".nyq/append-system.md"), "Project extra.").unwrap();
        let text = base_prompt(&cwd, &home, &policy);
        assert!(text.starts_with("Project base."), "{text}");
        assert!(!text.contains("User base."), "{text}");
        let (user, project) = (
            text.find("User extra.").unwrap(),
            text.find("Project extra.").unwrap(),
        );
        assert!(user < project, "{text}");
        assert_eq!(
            prompt_files(&cwd, &home),
            vec![
                cwd.join(".nyq/system.md"),
                home.join("append-system.md"),
                cwd.join(".nyq/append-system.md"),
            ]
        );
    }

    #[test]
    fn global_and_parent_instructions_are_included() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let repo = dir.path().join("repo");
        let cwd = repo.join("crates/app");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::write(home.join("AGENTS.md"), "Global rule.").unwrap();
        std::fs::write(repo.join("AGENTS.md"), "Repo rule.").unwrap();
        std::fs::write(repo.join("crates/CLAUDE.md"), "Crates rule.").unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "Outside the repo.").unwrap();
        let text = instructions_above(&cwd, &home);
        let global = text.find("Global rule.").unwrap();
        let top = text
            .find("# Project instructions (../../AGENTS.md)\n\nRepo rule.")
            .unwrap();
        let mid = text
            .find("# Project instructions (../CLAUDE.md)\n\nCrates rule.")
            .unwrap();
        assert!(global < top && top < mid, "{text}");
        assert!(!text.contains("Outside the repo."), "{text}");
        // Outside a repository only the global file applies.
        let text = instructions_above(dir.path(), &dir.path().join("none"));
        assert!(text.is_empty(), "{text}");
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
