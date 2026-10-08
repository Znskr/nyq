//! Status line (footer) segments and Git branch detection.

use std::path::Path;

use super::render::one_line;
use super::tui::Footer;
use crate::settings::Segment;

pub fn tokens(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1000..=999_999 => format!("{:.1}k", n as f64 / 1000.0).replace(".0k", "k"),
        _ => format!("{:.1}M", n as f64 / 1_000_000.0),
    }
}

pub fn tilde(path: &str) -> String {
    match dirs::home_dir().map(|h| h.display().to_string()) {
        Some(h) if !h.is_empty() && (path == h || path.starts_with(&format!("{h}/"))) => {
            format!("~{}", &path[h.len()..])
        }
        _ => path.to_string(),
    }
}

/// The left part: `effort · model`, plus `fast` when fast mode is on.
/// None when the segment is hidden.
pub fn left(f: &Footer) -> Option<String> {
    if !f.segments.get(Segment::Model) {
        return None;
    }
    let effort = f.effort.as_deref().unwrap_or("auto");
    let mut s = format!("{effort} · {}", f.model);
    if f.fast {
        s.push_str(" · fast");
    }
    Some(s)
}

/// The right-hand segments, in order, as plain text.
pub fn right(f: &Footer, width: usize) -> Vec<String> {
    let on = |seg| f.segments.get(seg);
    let mut out = Vec::new();
    if on(Segment::Provider) && !f.provider.is_empty() {
        out.push(f.provider.clone());
    }
    if on(Segment::Context) && f.context_window > 0 && f.context_tokens > 0 {
        out.push(format!(
            "{:.0}% of {}",
            f.context_tokens as f64 * 100.0 / f.context_window as f64,
            tokens(f.context_window)
        ));
    }
    if on(Segment::Tokens) && f.usage.input_tokens + f.usage.output_tokens > 0 {
        out.push(format!(
            "↑{} ↓{}",
            tokens(f.usage.input_tokens),
            tokens(f.usage.output_tokens)
        ));
    }
    if on(Segment::Sandbox) {
        if !f.sandbox.is_empty() {
            out.push(f.sandbox.clone());
        }
        if !f.permission_mode.is_empty() {
            out.push(f.permission_mode.clone());
        }
    }
    if on(Segment::Folder) && !f.cwd.is_empty() {
        out.push(one_line(&tilde(&f.cwd), width.max(1)));
    }
    if on(Segment::Branch) {
        if let Some(b) = &f.branch {
            out.push(format!("⎇ {}", one_line(b, 60)));
        }
    }
    out
}

/// The branch named by a `.git/HEAD` file, or a short commit when detached.
pub fn branch_from_head(head: &str) -> Option<String> {
    let head = head.trim();
    if let Some(r) = head.strip_prefix("ref:") {
        let r = r.trim();
        return Some(r.strip_prefix("refs/heads/").unwrap_or(r).to_string());
    }
    (head.len() >= 7 && head.chars().all(|c| c.is_ascii_hexdigit())).then(|| head[..7].to_string())
}

/// The Git branch of the repository containing `cwd` (worktrees included).
pub fn git_branch(cwd: &Path) -> Option<String> {
    for dir in cwd.ancestors() {
        let git = dir.join(".git");
        let head = if git.is_dir() {
            git.join("HEAD")
        } else if git.is_file() {
            // A worktree or submodule: `gitdir: <path>`.
            let text = std::fs::read_to_string(&git).ok()?;
            let gitdir = text.trim().strip_prefix("gitdir:")?.trim();
            dir.join(gitdir).join("HEAD")
        } else {
            continue;
        };
        return branch_from_head(&std::fs::read_to_string(head).ok()?);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::StatusLine;
    use crate::types::Usage;

    fn footer() -> Footer {
        Footer {
            cwd: "/work/nyq".into(),
            provider: "codex".into(),
            model: "gpt-6-sol".into(),
            effort: Some("high".into()),
            sandbox: "sandboxed".into(),
            permission_mode: "yolo".into(),
            usage: Usage {
                input_tokens: 1200,
                output_tokens: 300,
                cached_tokens: 0,
            },
            context_tokens: 27_200,
            context_window: 272_000,
            fast: true,
            branch: Some("main".into()),
            segments: StatusLine::default(),
            ..Footer::default()
        }
    }

    #[test]
    fn segments_render_and_hide() {
        let mut f = footer();
        assert_eq!(left(&f).unwrap(), "high · gpt-6-sol · fast");
        assert_eq!(
            right(&f, 80),
            [
                "codex",
                "10% of 272k",
                "↑1.2k ↓300",
                "sandboxed",
                "yolo",
                "/work/nyq",
                "⎇ main"
            ]
        );
        for seg in [
            Segment::Provider,
            Segment::Tokens,
            Segment::Sandbox,
            Segment::Branch,
        ] {
            f.segments.set(seg, false);
        }
        assert_eq!(right(&f, 80), ["10% of 272k", "/work/nyq"]);
        f.segments.set(Segment::Model, false);
        assert!(left(&f).is_none());
        f.fast = false;
        f.segments.set(Segment::Model, true);
        assert_eq!(left(&f).unwrap(), "high · gpt-6-sol");
    }

    #[test]
    fn git_heads() {
        assert_eq!(
            branch_from_head("ref: refs/heads/feature/x\n").as_deref(),
            Some("feature/x")
        );
        assert_eq!(
            branch_from_head("0123456789abcdef0123456789abcdef01234567").as_deref(),
            Some("0123456")
        );
        assert_eq!(branch_from_head("garbage"), None);
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("repo/.git")).unwrap();
        std::fs::write(dir.path().join("repo/.git/HEAD"), "ref: refs/heads/dev\n").unwrap();
        std::fs::create_dir_all(dir.path().join("repo/src/deep")).unwrap();
        assert_eq!(
            git_branch(&dir.path().join("repo/src/deep")).as_deref(),
            Some("dev")
        );
        std::fs::create_dir_all(dir.path().join("wt")).unwrap();
        std::fs::create_dir_all(dir.path().join("repo/.git/worktrees/wt")).unwrap();
        std::fs::write(
            dir.path().join("repo/.git/worktrees/wt/HEAD"),
            "ref: refs/heads/topic\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("wt/.git"),
            "gitdir: ../repo/.git/worktrees/wt\n",
        )
        .unwrap();
        assert_eq!(git_branch(&dir.path().join("wt")).as_deref(), Some("topic"));
    }
}
