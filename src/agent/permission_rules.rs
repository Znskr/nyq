//! Approval rules: what "always" grants for a call, and whether a granted
//! rule covers a later one. Path containment helpers live here too.
//!
//! "Always" grants a rule rather than a whole tool: `bash:<prefix>` for shell
//! commands (program plus subcommand, e.g. `bash:cargo test`), the tool name
//! for file writes (only inside the workspace) and for everything else.

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

use serde_json::Value;

/// Programs whose arguments are themselves commands or code: a prefix rule
/// for them would allow anything, so "always" can't be scoped.
const WRAPPERS: &[&str] = &[
    "sh", "bash", "zsh", "fish", "dash", "ksh", "eval", "exec", "source", ".", "sudo", "doas",
    "su", "env", "xargs", "nohup", "time", "timeout", "nice", "watch", "command", "builtin", "ssh",
];
/// Interpreters: scoped only with a script or module word (`python script.py`).
const INTERPRETERS: &[&str] = &[
    "python", "python3", "node", "perl", "ruby", "deno", "php", "lua", "npx", "bunx",
];
/// Package runners whose `run`/`exec` subcommand takes a script name.
const RUNNERS: &[&str] = &[
    "npm", "pnpm", "yarn", "bun", "uv", "poetry", "pipenv", "pdm", "hatch",
];
/// Clients whose later arguments can start another transfer or change the
/// destination. A prefix grant would approve a different operation.
const NETWORK_CLIENTS: &[&str] = &[
    "curl", "wget", "aria2c", "scp", "rsync", "sftp", "nc", "ncat", "socat", "ftp", "rclone",
];

/// Whether a command chains, pipes, substitutes or redirects (or spans lines).
fn has_control(cmd: &str) -> bool {
    cmd.contains('$')
        || cmd
            .chars()
            .any(|c| matches!(c, ';' | '&' | '|' | '`' | '>' | '<' | '\n' | '\r'))
}

/// A plain command word: no flags, quotes, globs, variables or assignments.
fn plain_word(w: &str) -> bool {
    w.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && w.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':' | '/' | '@' | '+')
        })
}

/// A leading flag (`-la`, `--force`), not a lone `-` (often stdin).
fn is_flag(w: &str) -> bool {
    w.starts_with('-') && w.len() > 1
}

/// Last path component, so `./sh` and `bin/bash` match wrappers by name.
pub(crate) fn program_base(program: &str) -> &str {
    program.rsplit(['/', '\\']).next().unwrap_or(program)
}

/// `Some(true)`: the next plain word is an operand and belongs in the grant.
/// `Some(false)`: a boolean flag, or a self-contained `--name=value`.
/// `None`: the option cannot be scoped (the grant would be once-only).
fn flag_operand(program: &str, flag: &str) -> Option<bool> {
    if let Some((name, value)) = flag.split_once('=') {
        if name.starts_with("--") && name.len() > 2 && plain_word(value) {
            return Some(false);
        }
        return None;
    }
    let valued = |flags: &[&str]| flags.contains(&flag).then_some(true);
    let boolean = |flags: &[&str]| flags.contains(&flag).then_some(false);
    match program_base(program) {
        "git" => valued(&[
            "-C",
            "-c",
            "--git-dir",
            "--work-tree",
            "--namespace",
            "--super-prefix",
            "--config-env",
        ])
        .or_else(|| {
            boolean(&[
                "--bare",
                "--no-pager",
                "--paginate",
                "--no-replace-objects",
                "--literal-pathspecs",
                "--glob-pathspecs",
                "--noglob-pathspecs",
                "--icase-pathspecs",
            ])
        }),
        "cargo" => valued(&[
            "--manifest-path",
            "-p",
            "--package",
            "--target",
            "--target-dir",
            "--features",
            "--jobs",
            "-j",
            "--color",
            "--config",
            "--profile",
            "--message-format",
            "--out-dir",
            "--artifact-dir",
            "-Z",
        ])
        .or_else(|| {
            boolean(&[
                "--release",
                "--offline",
                "--locked",
                "--frozen",
                "--all-features",
                "--no-default-features",
                "--workspace",
                "--all",
                "--lib",
                "--bins",
                "--tests",
                "--examples",
                "--benches",
                "--doc",
                "--quiet",
                "-q",
                "--verbose",
                "-v",
            ])
        }),
        "curl" => valued(&[
            "-o",
            "-d",
            "-H",
            "-X",
            "-u",
            "--url",
            "--output",
            "--data",
            "--header",
            "--request",
            "--user",
            "-A",
            "--user-agent",
            "-e",
            "--referer",
            "-b",
            "--cookie",
            "-F",
            "--form",
            "-T",
            "--upload-file",
            "--data-binary",
            "--data-raw",
            "-m",
            "--max-time",
            "--connect-timeout",
            "-w",
            "--write-out",
            "--proxy",
            "-x",
            "--cacert",
            "--cert",
            "--key",
        ])
        .or_else(|| {
            boolean(&[
                "-s",
                "-S",
                "-L",
                "-f",
                "-k",
                "-I",
                "-v",
                "-i",
                "-G",
                "--silent",
                "--show-error",
                "--location",
                "--fail",
                "--insecure",
                "--head",
                "--verbose",
                "--include",
                "--get",
                "--compressed",
            ])
        }),
        _ => None,
    }
}

/// The prefix "always" grants for a shell command: the program, known leading
/// options together with their operands (`git -C foo`, `curl -o file`), and
/// the following subcommand (`git -C foo status`). Unknown options are not
/// scoped. Wrappers and interpreters are matched on the program name with any
/// directory stripped (`./sh`, `bin/bash`, `./python`).
pub fn bash_prefix(cmd: &str) -> Option<String> {
    if has_control(cmd) {
        return None;
    }
    let words: Vec<&str> = cmd.split_whitespace().collect();
    let program = *words.first()?;
    let base = program_base(program);
    if !plain_word(program.strip_prefix("./").unwrap_or(program))
        || WRAPPERS.contains(&base)
        || NETWORK_CLIENTS.contains(&base)
        || program.contains('=')
    {
        return None;
    }
    if INTERPRETERS.contains(&base) && words.get(1).is_some_and(|w| is_flag(w)) {
        return None;
    }
    let mut prefix = vec![program];
    let mut i = 1;
    while words.get(i).is_some_and(|w| is_flag(w)) {
        match flag_operand(program, words[i]) {
            Some(true) => {
                let operand = words.get(i + 1).copied().filter(|w| plain_word(w))?;
                prefix.push(words[i]);
                prefix.push(operand);
                i += 2;
            }
            Some(false) => {
                prefix.push(words[i]);
                i += 1;
            }
            None => return None,
        }
    }
    if INTERPRETERS.contains(&base) {
        prefix.push(words.get(i).filter(|w| plain_word(w))?);
        return Some(prefix.join(" "));
    }
    if let Some(sub) = words.get(i) {
        if plain_word(sub) {
            prefix.push(sub);
            if RUNNERS.contains(&base) && matches!(*sub, "run" | "exec" | "x") {
                prefix.extend(words.get(i + 1).filter(|w| plain_word(w)));
            }
        } else {
            // A quoted, escaped, or otherwise unparsed operand must not fall
            // back to a grant of the bare program (`rm 'file'` is not `rm`).
            return None;
        }
    }
    Some(prefix.join(" "))
}

/// Whether `cmd` is covered by a granted prefix: same leading words and no
/// control operators.
pub fn bash_matches(prefix: &str, cmd: &str) -> bool {
    let want: Vec<&str> = prefix.split_whitespace().collect();
    !want.is_empty()
        && !has_control(cmd)
        && cmd
            .split_whitespace()
            .take(want.len())
            .eq(want.iter().copied())
}

/// A builtin tool by name, for effect queries independent of the session's
/// live registry. Session tools (MCP, extensions) classify as "other",
/// matching the fixed lists this replaces.
pub(crate) fn builtin_tool(name: &str) -> Option<&'static dyn crate::tools::Tool> {
    static R: std::sync::OnceLock<crate::tools::Registry> = std::sync::OnceLock::new();
    R.get_or_init(crate::tools::Registry::all_builtin).get(name)
}

/// Whether `tool` writes exactly the files its arguments name (write_file,
/// edit_file, and apply_patch with the files of its patch), by its static
/// kind ([`crate::tools::ToolKind::FileWrite`]). Path rules (`edit:…`) and
/// grants for such calls scope to those files.
pub(crate) fn is_file_tool(tool: &str) -> bool {
    builtin_tool(tool).is_some_and(|t| t.kind() == crate::tools::ToolKind::FileWrite)
}

/// A subagent call's mode as its grant and permission key reads it:
/// `general`, `external:<name>` for an external ACP agent, or (by default)
/// `explore`. A worktree child without a mode is `general`, as the tool runs
/// it. Each external agent has its own key, so a grant for one never covers
/// another, nor a typed `explore`/`general` child, and an old
/// `subagent:explore` grant never covers an external agent.
pub(crate) fn subagent_mode(args: &Value) -> String {
    let mode = args.get("mode").and_then(Value::as_str).map(str::trim);
    if let Some(name) = mode.and_then(|m| m.strip_prefix("external:")) {
        return format!("external:{}", name.trim());
    }
    let worktree = args.get("isolation").and_then(Value::as_str).map(str::trim) == Some("worktree");
    match mode.filter(|m| !m.is_empty()) {
        Some("general") => "general".into(),
        None if worktree => "general".into(),
        _ => "explore".into(),
    }
}

/// The rule "always" would grant for this call (no workspace context).
#[cfg_attr(not(test), allow(dead_code))]
pub fn grant_rule(tool: &str, args: &Value) -> Option<String> {
    grant_rule_in(
        tool,
        args,
        &std::env::current_dir().unwrap_or_default(),
        &[],
    )
}

/// The rule "always" would grant for this call, as stored in
/// `Agent::allowed`: a bash prefix, the tool, a subagent mode, a site for
/// `web_fetch`, and for paths outside the workspace that directory tree
/// (`read_file:/data/**`, `write_file:/tmp/out/**`).
pub fn grant_rule_in(tool: &str, args: &Value, cwd: &Path, extra: &[PathBuf]) -> Option<String> {
    if tool == "bash" {
        let cmd = args.get("command").and_then(Value::as_str)?;
        return bash_prefix(cmd).map(|p| format!("bash:{p}"));
    }
    if tool == "subagent" {
        return Some(format!("subagent:{}", subagent_mode(args)));
    }
    if tool == "web_fetch" {
        let url = args
            .get("url")
            .and_then(Value::as_str)
            .filter(|u| !u.is_empty())?;
        return Some(match site(url) {
            Some(origin) => format!("web_fetch:{origin}/*"),
            None => format!("web_fetch:{url}"),
        });
    }
    let read = crate::permissions::is_read_tool(tool);
    if tool == "apply_patch" {
        // No single `path`; grantable only when every file it names is a
        // plain in-workspace write.
        let text = args.get("patch").and_then(Value::as_str).unwrap_or("");
        let paths = crate::tools::patch::paths(text);
        return (!paths.is_empty() && paths.iter().all(|p| file_grant_ok_in(p, cwd, extra)))
            .then(|| tool.to_string());
    }
    if read || is_file_tool(tool) {
        let path = args
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or(if read { "." } else { "" });
        if !read && file_grant_ok_in(path, cwd, extra) {
            return Some(tool.to_string());
        }
        if path.is_empty() || (!read && path_under_git(args, cwd)) {
            return None;
        }
        let full = resolved(cwd, path)?;
        // A file grants its directory; a searched or listed directory itself.
        let dir = if full.is_dir() {
            full
        } else {
            full.parent()?.to_path_buf()
        };
        // Never the whole filesystem.
        dir.parent()?;
        return Some(format!(
            "{tool}:{}/**",
            dir.to_string_lossy()
                .replace('\\', "/")
                .trim_end_matches('/')
        ));
    }
    Some(tool.to_string())
}

/// `scheme://host[:port]` of a URL, when it has one.
fn site(url: &str) -> Option<String> {
    let origin = reqwest::Url::parse(url).ok()?.origin();
    origin.is_tuple().then(|| origin.ascii_serialization())
}

/// A URL as parsed (lowercase host, explicit path).
pub(crate) fn normalize_url(url: &str) -> Option<String> {
    reqwest::Url::parse(url).ok().map(|u| u.to_string())
}

/// How a rule reads in the approval menu.
pub fn rule_label(rule: &str) -> String {
    if let Some(p) = rule.strip_prefix("bash:") {
        return format!("`{p} …`");
    }
    if let Some(mode) = rule.strip_prefix("subagent:") {
        if let Some(name) = mode.strip_prefix("external:") {
            return format!("`subagent` external agent `{name}`");
        }
        return format!("`subagent` ({mode})");
    }
    if let Some(url) = rule.strip_prefix("web_fetch:") {
        return match url.strip_suffix("/*") {
            Some(origin) => format!("`web_fetch` on {origin}"),
            None => format!("`web_fetch` {url}"),
        };
    }
    if let Some((name, path)) = rule.split_once(':') {
        if let Some(dir) = path.strip_suffix("/**") {
            let what = if is_file_tool(name) { "edits" } else { "reads" };
            return format!("{what} under {dir} (this session)");
        }
        return format!("`{name}` {path}");
    }
    if is_file_tool(rule) {
        return format!("{} in this workspace", rule.replace('_', " "));
    }
    format!("`{rule}`")
}

/// Whether a file tool's path (with symlinks resolved) is under a `.git`
/// directory.
pub(crate) fn path_under_git(args: &Value, cwd: &Path) -> bool {
    let path = args.get("path").and_then(Value::as_str).unwrap_or("");
    str_under_git(path, cwd)
}

/// `path_under_git` over every path a call writes: a patch is under `.git`
/// if any file it names is.
pub(crate) fn paths_under_git(tool: &str, args: &Value, cwd: &Path) -> bool {
    if tool == "apply_patch" {
        let text = args.get("patch").and_then(Value::as_str).unwrap_or("");
        return crate::tools::patch::paths(text)
            .iter()
            .any(|p| str_under_git(p, cwd));
    }
    path_under_git(args, cwd)
}

fn str_under_git(path: &str, cwd: &Path) -> bool {
    let is_git = |p: &Path| {
        p.components().any(|c| {
            c.as_os_str()
                .to_str()
                .is_some_and(|s| s.trim_end_matches(['.', ' ']).eq_ignore_ascii_case(".git"))
        })
    };
    is_git(Path::new(path)) || resolved(cwd, path).is_none_or(|p| is_git(&p))
}

/// `base.join(path)` normalized, with symlinks along its existing part
/// resolved; None if it escapes via `..`.
pub(crate) fn resolved(base: &Path, path: &str) -> Option<PathBuf> {
    let full = lexical(base, path)?;
    Some(
        match full
            .ancestors()
            .find_map(|a| std::fs::canonicalize(a).ok().map(|real| (a, real)))
        {
            Some((existing, real)) => {
                real.join(full.strip_prefix(existing).unwrap_or(Path::new("")))
            }
            None => full.clone(),
        },
    )
}

/// Lexical normalization of `base.join(path)`, or None if it escapes via `..`.
pub(crate) fn lexical(base: &Path, path: &str) -> Option<PathBuf> {
    let mut full = PathBuf::new();
    for c in base.join(path).components() {
        match c {
            Component::ParentDir => {
                if !full.pop() {
                    return None;
                }
            }
            Component::CurDir => {}
            c => full.push(c),
        }
    }
    Some(full)
}

/// Whether `full` stays inside `root`, including through symlinks on the existing prefix.
fn covers(root: &Path, full: &Path) -> bool {
    let canon = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    // Resolve the existing prefix (symlinks, and spellings such as macOS's
    // /var vs /private/var), keep the not-yet-existing tail, then compare.
    match full
        .ancestors()
        .find_map(|a| std::fs::canonicalize(a).ok().map(|real| (a, real)))
    {
        Some((existing, real)) => real
            .join(full.strip_prefix(existing).unwrap_or(Path::new("")))
            .starts_with(&canon),
        None => full.starts_with(root),
    }
}

/// Whether `path` (relative to `root` or absolute) stays inside `root`, after
/// resolving `..` and any symlinks along the existing part of the path.
pub(crate) fn inside(root: &Path, path: &str) -> bool {
    lexical(root, path).is_some_and(|full| covers(root, &full))
}

/// Whether `path`, resolved against `primary`, stays inside `primary` or any
/// additional directory. Relative paths still start at `primary`.
pub(crate) fn inside_any(primary: &Path, extra: &[PathBuf], path: &str) -> bool {
    let Some(full) = lexical(primary, path) else {
        return false;
    };
    covers(primary, &full) || extra.iter().any(|root| covers(root, &full))
}

/// Whether a grant for a file tool may cover writing `path`: non-empty,
/// inside the workspace, and nowhere under (or linked into) `.git`.
pub(crate) fn file_grant_ok(path: &str, cwd: &Path) -> bool {
    // Hooks and config under .git run later with full permissions: always ask.
    let git_component = |p: &Path| {
        p.components().any(|c| {
            c.as_os_str()
                .to_str()
                .is_some_and(|s| s.trim_end_matches(['.', ' ']).eq_ignore_ascii_case(".git"))
        })
    };
    let resolved = cwd.join(path);
    // Follow links by hand too: canonicalize fails on a dangling link
    // (e.g. `notes.txt -> .git/commondir`), which would otherwise slip by.
    let link_into_git = |mut p: PathBuf| {
        for _ in 0..8 {
            let Ok(target) = std::fs::read_link(&p) else {
                return false;
            };
            if git_component(&target) {
                return true;
            }
            p = p.parent().map_or(target.clone(), |dir| dir.join(&target));
        }
        true // too many hops: treat as suspicious
    };
    let in_git = git_component(Path::new(path))
        || resolved
            .ancestors()
            .take_while(|a| a.starts_with(cwd))
            .any(|a| link_into_git(a.to_path_buf()))
        || resolved
            .ancestors()
            .find_map(|a| std::fs::canonicalize(a).ok())
            .is_some_and(|real| {
                // Only what lies below the workspace counts; a workspace that
                // itself sits under a directory named `.git` is fine.
                let root = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
                git_component(real.strip_prefix(&root).unwrap_or(&real))
            });
    !path.is_empty() && !in_git && inside(cwd, path)
}

/// Like [`file_grant_ok`], also accepting paths inside `extra` directories.
pub(crate) fn file_grant_ok_in(path: &str, cwd: &Path, extra: &[PathBuf]) -> bool {
    if path.is_empty() {
        return false;
    }
    if file_grant_ok(path, cwd) {
        return true;
    }
    let Some(full) = lexical(cwd, path) else {
        return false;
    };
    extra.iter().any(|root| {
        full.strip_prefix(root).is_ok_and(|rel| {
            if rel.as_os_str().is_empty() {
                return false;
            }
            file_grant_ok(rel.to_string_lossy().as_ref(), root)
        })
    })
}

/// Whether a granted rule covers this call.
#[cfg_attr(not(test), allow(dead_code))]
pub fn is_allowed(allowed: &HashSet<String>, tool: &str, args: &Value, cwd: &Path) -> bool {
    is_allowed_in(allowed, tool, args, cwd, &[])
}

/// Like [`is_allowed`], treating `extra` directories as inside the workspace.
pub fn is_allowed_in(
    allowed: &HashSet<String>,
    tool: &str,
    args: &Value,
    cwd: &Path,
    extra: &[PathBuf],
) -> bool {
    if tool == "bash" {
        let cmd = args.get("command").and_then(Value::as_str).unwrap_or("");
        return allowed
            .iter()
            .filter_map(|r| r.strip_prefix("bash:"))
            .any(|p| bash_matches(p, cmd));
    }
    // Directory-tree grants for paths outside the workspace.
    let tree = || {
        allowed
            .iter()
            .any(|r| crate::permissions::path_rule_matches(r, tool, args, cwd, true))
    };
    if is_file_tool(tool) {
        if tool == "apply_patch" {
            let text = args.get("patch").and_then(Value::as_str).unwrap_or("");
            let paths = crate::tools::patch::paths(text);
            return !paths.is_empty()
                && allowed.contains(tool)
                && paths.iter().all(|p| file_grant_ok_in(p, cwd, extra));
        }
        let path = args.get("path").and_then(Value::as_str).unwrap_or("");
        return (allowed.contains(tool) && file_grant_ok_in(path, cwd, extra))
            || (tree() && !path_under_git(args, cwd));
    }
    if tool == "subagent" {
        return allowed.contains(&format!("subagent:{}", subagent_mode(args)));
    }
    if tool == "web_fetch" {
        let url = args.get("url").and_then(Value::as_str).unwrap_or("");
        let norm = normalize_url(url);
        return allowed
            .iter()
            .filter_map(|r| r.strip_prefix("web_fetch:"))
            .any(|p| {
                // A site grant must match the parsed URL, so `https://a.com/*`
                // never covers `https://a.com.evil/` or `https://a.com@evil/`.
                match &norm {
                    Some(n) => crate::permissions::glob(p, n),
                    None => p == url,
                }
            });
    }
    if crate::permissions::is_read_tool(tool) {
        return tree();
    }
    allowed.contains(tool)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bash_prefixes() {
        let p = |c: &str| bash_prefix(c);
        assert_eq!(p("cargo test").as_deref(), Some("cargo test"));
        assert_eq!(
            p("cargo test --release -p nyq").as_deref(),
            Some("cargo test")
        );
        assert_eq!(p("  git   status  ").as_deref(), Some("git status"));
        assert_eq!(p("git -C foo status").as_deref(), Some("git -C foo status"));
        assert_eq!(p("npm run build").as_deref(), Some("npm run build"));
        assert_eq!(
            p("npm run build -- --watch").as_deref(),
            Some("npm run build")
        );
        assert_eq!(p("npm test").as_deref(), Some("npm test"));
        assert_eq!(p("uv run pytest -x").as_deref(), Some("uv run pytest"));
        assert_eq!(p("ls").as_deref(), Some("ls"));
        assert_eq!(p("ls -la src"), None);
        assert_eq!(p("rm -rf target/debug"), None);
        assert_eq!(p("chmod -R 755 ."), None);
        assert_eq!(p("curl -o approved-file https://example.com/a"), None);
        assert_eq!(p("curl https://example.com/health"), None);
        assert_eq!(p("wget https://example.com/a"), None);
        assert_eq!(p("rm 'target/file'"), None);
        assert_eq!(p("rm target/file").as_deref(), Some("rm target/file"));
        assert_eq!(
            p("cargo --manifest-path safe/Cargo.toml test").as_deref(),
            Some("cargo --manifest-path safe/Cargo.toml test")
        );
        assert_eq!(
            p("./python script.py").as_deref(),
            Some("./python script.py")
        );
        assert_eq!(p("python -m pytest"), None);
        assert_eq!(p("./gradlew build").as_deref(), Some("./gradlew build"));
        assert_eq!(p("python script.py").as_deref(), Some("python script.py"));
        assert_eq!(p("make").as_deref(), Some("make"));
        assert_eq!(p("rg 'a b' src"), None);
        // Control operators, wrappers, assignments and odd programs can't be scoped.
        for c in [
            "cargo test; rm -rf /",
            "cargo test && echo",
            "a || b",
            "ls | wc",
            "echo $(id)",
            "echo `id`",
            "ls > out",
            "wc < in",
            "sleep 1 &",
            "ls\nrm x",
            "sh -c 'x'",
            "bash script.sh",
            "sudo ls",
            "env X=1 ls",
            "xargs rm",
            "FOO=1 cargo test",
            "../x",
            "\"git\" status",
            "",
            "   ",
            "node -e 1",
            "eval ls",
            "$HOME/bin/x",
            "./sh -c echo",
            "bin/bash -c echo",
            "./python -c 1",
            "usr/bin/python -m pytest",
        ] {
            assert_eq!(p(c), None, "{c:?}");
        }
    }

    #[test]
    fn bash_grants_reject_dynamic_extra_arguments() {
        let allowed = ["bash:git status".to_string()].into();
        for arg in [
            "$'--short'",
            "$'\\x2d\\x2dshort'",
            "$\"--short\"",
            "$",
            "$FLAGS",
            "${FLAGS}",
            "`echo --short`",
            "$(echo --short)",
        ] {
            let cmd = format!("git status {arg}");
            assert!(!bash_matches("git status", &cmd), "{cmd}");
            assert!(
                !is_allowed(
                    &allowed,
                    "bash",
                    &serde_json::json!({"command": cmd}),
                    Path::new("/tmp")
                ),
                "{cmd}"
            );
            assert!(bash_prefix(&cmd).is_none(), "{cmd}");
        }
        assert!(bash_matches("git status", "git status --short"));
    }

    #[test]
    fn bash_matching() {
        assert!(bash_matches("cargo test", "cargo test"));
        assert!(bash_matches("cargo test", "cargo   test --lib agent"));
        assert!(!bash_matches("cargo test", "cargo testx"));
        assert!(!bash_matches("cargo test", "cargo build"));
        assert!(!bash_matches("cargo test", "cargo"));
        assert!(!bash_matches("cargo test", "cargo test && rm -rf ~"));
        assert!(!bash_matches("cargo test", "cargo test | tee log"));
        assert!(!bash_matches("cargo test", "cargo test > log"));
        assert!(!bash_matches("cargo test", "cargo test $(evil)"));
        assert!(!bash_matches("cargo test", "cargo test\nrm x"));
        assert!(bash_matches("ls", "ls -la"));
        assert!(!bash_matches("ls", "lsof"));
        assert!(bash_matches("rm -rf", "rm -rf dir"));
        assert!(!bash_matches("rm -rf", "rm dir"));
        assert!(bash_matches("ls -la", "ls -la src"));
        assert!(!bash_matches("ls -la", "ls src"));
        assert!(!bash_matches("git -C", "git status"));
        assert!(bash_matches("git -C", "git -C elsewhere reset --hard"));
        assert!(!bash_matches(
            "git -C safe status",
            "git -C elsewhere reset --hard"
        ));
        assert!(!bash_matches("", "anything"));
    }

    #[test]
    fn rules_are_scoped() {
        use serde_json::json;
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        assert_eq!(
            grant_rule("bash", &json!({"command": "git status -s"})).as_deref(),
            Some("bash:git status")
        );
        assert_eq!(grant_rule("bash", &json!({"command": "a; b"})), None);
        assert_eq!(
            grant_rule("write_file", &json!({"path": "x"})).as_deref(),
            Some("write_file")
        );
        let write: HashSet<String> = ["write_file".into()].into();
        assert!(is_allowed(
            &write,
            "write_file",
            &json!({"path": "src/x.rs"}),
            cwd
        ));
        assert!(!is_allowed(
            &write,
            "write_file",
            &json!({"path": ".git/hooks/pre-commit"}),
            cwd
        ));
        assert!(!is_allowed(
            &write,
            "edit_file",
            &json!({"path": "./.git/config"}),
            cwd
        ));
        assert!(!is_allowed(
            &write,
            "write_file",
            &json!({"path": "sub/.git/config"}),
            cwd
        ));
        #[cfg(unix)]
        {
            // A dangling link into .git must not ride on an "always" grant.
            std::os::unix::fs::symlink(".git/commondir", cwd.join("notes.txt")).unwrap();
            assert!(!is_allowed(
                &write,
                "write_file",
                &json!({"path": "notes.txt"}),
                cwd
            ));
            std::os::unix::fs::symlink("src", cwd.join("code")).unwrap();
            assert!(is_allowed(
                &write,
                "write_file",
                &json!({"path": "code/x.rs"}),
                cwd
            ));
        }
        for path in [".GIT/config", ".git. /hooks/new"] {
            assert!(!is_allowed(
                &write,
                "write_file",
                &json!({"path": path}),
                cwd
            ));
        }
        #[cfg(unix)]
        {
            std::fs::create_dir(cwd.join(".git")).unwrap();
            std::os::unix::fs::symlink(".git", cwd.join("docs")).unwrap();
            for path in ["docs/config", "docs/hooks/new"] {
                assert!(!is_allowed(
                    &write,
                    "write_file",
                    &json!({"path": path}),
                    cwd
                ));
            }
        }
        assert_eq!(
            grant_rule("mcp__wiki__ask", &json!({})).as_deref(),
            Some("mcp__wiki__ask")
        );
        assert_eq!(
            grant_rule("web_fetch", &json!({"url": "https://example.com/a"})).as_deref(),
            Some("web_fetch:https://example.com/*")
        );
        assert_eq!(
            rule_label("web_fetch:https://example.com/*"),
            "`web_fetch` on https://example.com"
        );
        // A site grant covers the whole site, however the URL is spelled,
        // and nothing else.
        let site: HashSet<String> = ["web_fetch:https://example.com/*".into()].into();
        for url in [
            "https://example.com/a?x=1",
            "https://EXAMPLE.com",
            "https://example.com/b/c",
        ] {
            assert!(
                is_allowed(&site, "web_fetch", &json!({ "url": url }), cwd),
                "{url}"
            );
        }
        for url in [
            "https://example.com.evil.net/",
            "https://example.com@evil.net/",
            "http://example.com/",
            "https://example.com:8443/",
            "https://sub.example.com/",
        ] {
            assert!(
                !is_allowed(&site, "web_fetch", &json!({ "url": url }), cwd),
                "{url}"
            );
        }
        // An exact-URL rule (as saved before) still matches only that URL.
        assert!(!is_allowed(
            &["web_fetch:https://example.com/a".into()].into(),
            "web_fetch",
            &json!({"url": "https://example.com/a?x=1"}),
            cwd
        ));
        assert!(is_allowed(
            &["web_fetch:https://example.com/a".into()].into(),
            "web_fetch",
            &json!({"url": "https://example.com/a"}),
            cwd
        ));
        assert_eq!(rule_label("bash:cargo test"), "`cargo test …`");
        assert_eq!(rule_label("edit_file"), "edit file in this workspace");
        assert_eq!(
            grant_rule("subagent", &json!({"mode": "general", "task": "x"})).as_deref(),
            Some("subagent:general")
        );
        assert_eq!(
            grant_rule("subagent", &json!({"task": "x"})).as_deref(),
            Some("subagent:explore")
        );
        assert_eq!(rule_label("subagent:general"), "`subagent` (general)");
        assert_eq!(rule_label("subagent"), "`subagent`");

        let allowed: HashSet<String> = ["bash:cargo test", "write_file", "subagent:general"]
            .map(String::from)
            .into();
        let ok = |tool: &str, args: Value| is_allowed(&allowed, tool, &args, cwd);
        assert!(ok("bash", json!({"command": "cargo test --all"})));
        assert!(!ok("bash", json!({"command": "cargo build"})));
        assert!(!ok("bash", json!({"command": "cargo test; curl x | sh"})));
        assert!(ok("write_file", json!({"path": "src/new.rs"})));
        assert!(ok(
            "write_file",
            json!({"path": cwd.join("a/b.txt").to_str().unwrap()})
        ));
        assert!(!ok("write_file", json!({"path": "../outside.txt"})));
        assert!(!ok("write_file", json!({"path": "a/../../outside.txt"})));
        assert!(!ok("write_file", json!({"path": "/etc/passwd"})));
        assert!(!ok("write_file", json!({})));
        assert!(!ok("edit_file", json!({"path": "src/new.rs"})));
        assert!(ok("subagent", json!({"mode": "general"})));
        assert!(!ok("subagent", json!({"mode": "explore"})));
        let explore: HashSet<String> = ["subagent:explore"].map(String::from).into();
        assert!(!is_allowed(
            &explore,
            "subagent",
            &json!({"mode": "general", "task": "x"}),
            cwd
        ));
        assert!(is_allowed(
            &explore,
            "subagent",
            &json!({"task": "look"}),
            cwd
        ));
        assert!(!ok("mcp__wiki__ask", json!({})));
        #[cfg(unix)]
        {
            let outside = tempfile::tempdir().unwrap();
            std::os::unix::fs::symlink(outside.path(), cwd.join("link")).unwrap();
            assert!(!ok("write_file", json!({"path": "link/x.txt"})));
        }
    }

    /// Each external agent has its own grant key: "always" on one covers
    /// only that agent, never another external agent or a typed child, and
    /// neither an old `subagent:explore` grant nor a `subagent:general` one
    /// covers an external agent.
    #[test]
    fn external_subagent_grants_cover_only_that_agent() {
        use serde_json::json;
        let cwd = Path::new("/w");
        let ext = |name: &str| json!({"mode": format!("external:{name}"), "task": "x"});
        assert_eq!(
            grant_rule("subagent", &ext("claude")).as_deref(),
            Some("subagent:external:claude")
        );
        assert_eq!(
            grant_rule(
                "subagent",
                &json!({"mode": " external: codex ", "task": "x"})
            )
            .as_deref(),
            Some("subagent:external:codex")
        );
        // A worktree child without a mode runs as general, and is keyed so.
        assert_eq!(
            grant_rule("subagent", &json!({"isolation": "worktree", "task": "x"})).as_deref(),
            Some("subagent:general")
        );
        assert_eq!(
            grant_rule(
                "subagent",
                &json!({"isolation": "worktree", "mode": "explore", "task": "x"})
            )
            .as_deref(),
            Some("subagent:explore")
        );
        assert_eq!(
            rule_label("subagent:external:claude"),
            "`subagent` external agent `claude`"
        );

        let grants =
            |rules: &[&str]| -> HashSet<String> { rules.iter().map(|r| r.to_string()).collect() };
        let explore = json!({"task": "x"});
        let general = json!({"mode": "general", "task": "x"});

        let claude = grants(&["subagent:external:claude"]);
        assert!(is_allowed(&claude, "subagent", &ext("claude"), cwd));
        assert!(!is_allowed(&claude, "subagent", &ext("codex"), cwd));
        assert!(!is_allowed(&claude, "subagent", &explore, cwd));
        assert!(!is_allowed(&claude, "subagent", &general, cwd));
        // The reverse: typed-child grants (an old saved `subagent:explore`
        // among them) never cover an external agent.
        for old in ["subagent:explore", "subagent:general", "subagent"] {
            let g = grants(&[old]);
            assert!(!is_allowed(&g, "subagent", &ext("claude"), cwd), "{old}");
        }
        assert!(is_allowed(
            &grants(&["subagent:explore"]),
            "subagent",
            &explore,
            cwd
        ));
    }

    #[test]
    fn additional_directories_count_as_inside_for_reads_and_write_grants() {
        use serde_json::json;
        let root = tempfile::tempdir().unwrap();
        let primary = root.path().join("primary");
        let extra = root.path().join("extra");
        std::fs::create_dir(&primary).unwrap();
        std::fs::create_dir(&extra).unwrap();
        std::fs::create_dir(extra.join(".git")).unwrap();
        let note = extra.join("note.txt");
        std::fs::write(&note, "x").unwrap();
        let note = note.to_str().unwrap();
        assert!(inside(&primary, "note.txt"));
        assert!(inside_any(
            &primary,
            std::slice::from_ref(&extra),
            "../extra/note.txt"
        ));
        assert!(!inside(&primary, "../extra/note.txt"));
        assert!(!inside_any(
            &primary,
            std::slice::from_ref(&extra),
            "../missing/note.txt"
        ));
        let mut allowed = HashSet::new();
        allowed.insert("write_file".into());
        assert!(is_allowed_in(
            &allowed,
            "write_file",
            &json!({"path": note}),
            &primary,
            std::slice::from_ref(&extra)
        ));
        assert!(!is_allowed(
            &allowed,
            "write_file",
            &json!({"path": note}),
            &primary
        ));
        let git = extra.join(".git").join("config");
        assert!(!file_grant_ok_in(git.to_str().unwrap(), &primary, &[extra]));
    }

    #[test]
    fn always_outside_the_workspace_grants_the_directory_tree() {
        use serde_json::json;
        let work = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let cwd = work.path();
        let real = std::fs::canonicalize(data.path()).unwrap();
        let root = real.to_str().unwrap().replace('\\', "/");
        std::fs::create_dir_all(real.join("sub")).unwrap();
        let file = format!("{root}/notes.txt");
        let rule = grant_rule_in("read_file", &json!({ "path": file }), cwd, &[]).unwrap();
        assert_eq!(rule, format!("read_file:{root}/**"));
        assert!(rule_label(&rule).starts_with(&format!("reads under {root}")));
        let granted: HashSet<String> = [rule].into();
        let ok = |tool: &str, path: &str| is_allowed(&granted, tool, &json!({ "path": path }), cwd);
        assert!(ok("read_file", &format!("{root}/sub/deep.txt")));
        assert!(ok("list_dir", &root));
        assert!(ok("grep", &format!("{root}/sub")));
        assert!(!ok("read_file", "/etc/hosts"));
        assert!(!ok("write_file", &file), "a read grant never covers edits");

        // Edits outside the workspace: the file's directory tree, not .git.
        let rule = grant_rule_in(
            "write_file",
            &json!({ "path": format!("{root}/sub/a.txt") }),
            cwd,
            &[],
        )
        .unwrap();
        assert_eq!(rule, format!("write_file:{root}/sub/**"));
        let granted: HashSet<String> = [rule].into();
        let ok = |tool: &str, path: &str| is_allowed(&granted, tool, &json!({ "path": path }), cwd);
        assert!(ok("edit_file", &format!("{root}/sub/b.txt")));
        assert!(!ok("edit_file", &format!("{root}/c.txt")));
        assert!(!ok("write_file", &format!("{root}/sub/.git/config")));
        assert_eq!(
            grant_rule_in(
                "write_file",
                &json!({ "path": format!("{root}/.git/x") }),
                cwd,
                &[]
            ),
            None
        );
        // Inside the workspace a write grant stays the plain tool grant; the
        // filesystem root is never granted as a tree.
        assert_eq!(
            grant_rule_in("write_file", &json!({"path": "src/a.rs"}), cwd, &[]).as_deref(),
            Some("write_file")
        );
        #[cfg(unix)]
        assert_eq!(
            grant_rule_in("list_dir", &json!({"path": "/"}), cwd, &[]),
            None
        );
    }
}
