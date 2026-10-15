//! What a shell command does, judged from a small lexer instead of a shell.
//!
//! [`read_only`] admits pipelines (up to 8 stages) of commands that only read
//! the workspace and print: `ls`, `cat`, `head`, `tail`, `wc`, `grep`, `rg`,
//! `pwd`, `echo`, `printf`, and read-only `git` (status, log, diff, show,
//! branch listing, rev-parse, remote -v, worktree list, ls-files). Such calls
//! run without approval in every mode; the command that runs is rebuilt from
//! the parsed words (re-quoted, with `git` hardened against repository config
//! that would run programs), so the shell sees exactly what was checked.
//!
//! [`known_reversible`] admits `&&` chains of ordinary development commands
//! (`git status`/`fetch`, package installs and tests, `cargo build`…) that
//! auto mode runs without asking the classifier. An install or `run` that
//! passes a global flag, a config file, `--config.*`, or a directory
//! (`--prefix`, `--dir`, `-C`, `--modules-dir`) is not included: those can
//! write outside the project.
//!
//! Anything else — redirections, substitutions, variables, globs, `~`,
//! backslashes, assignments, unknown programs or options, and paths that leave
//! the workspace — is not admitted and goes through the normal approval path.

use std::path::{Path, PathBuf};

/// Longest command considered.
const MAX_COMMAND: usize = 8 * 1024;
/// Most pipeline stages a read-only command may have.
pub const MAX_STAGES: usize = 8;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Token {
    Word(Word),
    /// An operator: `|`, `&&`, `;`, `>`, `2>&1`'s `>&`, `(`…
    Op(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Word {
    /// The word after quote removal.
    pub text: String,
    /// Expands at run time: unquoted `$`, backtick, glob or brace characters,
    /// a leading `~`, or `$`/backtick inside double quotes.
    pub dynamic: bool,
}

/// Split `cmd` into words and operators. None when the command uses syntax
/// this lexer does not model (backslashes outside single quotes, newlines,
/// comments, unterminated quotes, NUL).
pub(crate) fn lex(cmd: &str) -> Option<Vec<Token>> {
    if cmd.len() > MAX_COMMAND || cmd.contains(['\n', '\r', '\0']) {
        return None;
    }
    let mut out = Vec::new();
    let mut chars = cmd.chars().peekable();
    let mut word: Option<Word> = None;
    while let Some(c) = chars.next() {
        match c {
            ' ' | '\t' => out.extend(word.take().map(Token::Word)),
            '\'' => {
                let w = word.get_or_insert_with(empty);
                loop {
                    match chars.next()? {
                        '\'' => break,
                        ch => w.text.push(ch),
                    }
                }
            }
            '"' => {
                let w = word.get_or_insert_with(empty);
                loop {
                    match chars.next()? {
                        '"' => break,
                        '\\' => return None,
                        ch @ ('$' | '`') => {
                            w.dynamic = true;
                            w.text.push(ch);
                        }
                        ch => w.text.push(ch),
                    }
                }
            }
            '|' | '&' | ';' | '<' | '>' | '(' | ')' => {
                out.extend(word.take().map(Token::Word));
                let mut op = c.to_string();
                if let Some(&next) = chars.peek() {
                    let pair = matches!(
                        (c, next),
                        ('&', '&') | ('|', '|') | ('|', '&') | (';', ';') | ('>', '>' | '&' | '|') | ('<', '<' | '&' | '>') | ('&', '>')
                    );
                    if pair {
                        op.push(next);
                        chars.next();
                    }
                }
                out.push(Token::Op(op));
            }
            '\\' => return None,
            '#' if word.is_none() => return None,
            _ => {
                let starts = word.is_none();
                let w = word.get_or_insert_with(empty);
                if matches!(c, '$' | '`' | '*' | '?' | '[' | ']' | '{' | '}') || (starts && c == '~') {
                    w.dynamic = true;
                }
                w.text.push(c);
            }
        }
    }
    out.extend(word.take().map(Token::Word));
    Some(out)
}

fn empty() -> Word {
    Word {
        text: String::new(),
        dynamic: false,
    }
}

/// Words of `cmd` split into stages on `sep`; None when any other operator
/// (or an empty stage) appears. `2>&1` at the end of a stage is dropped when
/// `allow_merge` is set.
fn stages(cmd: &str, sep: &str, allow_merge: bool) -> Option<Vec<Vec<Word>>> {
    let tokens = lex(cmd.trim())?;
    let mut out: Vec<Vec<Word>> = vec![Vec::new()];
    let mut i = 0;
    while i < tokens.len() {
        match &tokens[i] {
            Token::Op(op) if op == sep => out.push(Vec::new()),
            Token::Op(op) if allow_merge && op == ">&" => {
                // `… 2>&1`: the `2` was lexed as the stage's last word.
                let stage = out.last_mut()?;
                let fd = stage.pop()?;
                let one = matches!(tokens.get(i + 1), Some(Token::Word(w)) if w.text == "1" && !w.dynamic);
                let end = matches!(tokens.get(i + 2), None | Some(Token::Op(_)));
                if fd.text != "2" || fd.dynamic || !one || !end {
                    return None;
                }
                i += 1;
            }
            Token::Op(_) => return None,
            Token::Word(w) => out.last_mut()?.push(w.clone()),
        }
        i += 1;
    }
    if out.iter().any(|s| s.is_empty()) {
        return None;
    }
    Some(out)
}

/// The command to run in place of `cmd` when it only reads the workspace (see
/// the module docs); None when it needs the normal approval path.
pub fn read_only(cmd: &str, cwd: &Path, extra: &[PathBuf]) -> Option<String> {
    let stages = stages(cmd, "|", false)?;
    if stages.len() > MAX_STAGES {
        return None;
    }
    let secrets = crate::tools::SecretRoots::load();
    let ok_path = |p: &str| {
        crate::agent::inside_any(cwd, extra, p) && !secrets.blocks_path(&cwd.join(p))
    };
    let mut out = Vec::new();
    for words in &stages {
        if words.iter().any(|w| w.dynamic) {
            return None;
        }
        let words: Vec<&str> = words.iter().map(|w| w.text.as_str()).collect();
        let argv = read_only_stage(&words, &ok_path)?;
        out.push(argv.iter().map(|a| quote(a)).collect::<Vec<_>>().join(" "));
    }
    Some(out.join(" | "))
}

/// Shell-quote one word.
fn quote(w: &str) -> String {
    let plain = !w.is_empty()
        && w.chars().all(|c| c.is_ascii_alphanumeric() || "-_./=:@,+%".contains(c));
    if plain {
        w.to_string()
    } else {
        format!("'{}'", w.replace('\'', "'\\''"))
    }
}

fn is_flag(w: &str) -> bool {
    w.starts_with('-') && w.len() > 1
}

fn digits(w: &str) -> bool {
    !w.is_empty() && w.len() <= 9 && w.bytes().all(|b| b.is_ascii_digit())
}

/// One stage of a read-only pipeline: the argv to run, or None.
fn read_only_stage(w: &[&str], ok_path: &dyn Fn(&str) -> bool) -> Option<Vec<String>> {
    let (&program, args) = w.split_first()?;
    let owned = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    // Operands after the options must all be paths inside the workspace.
    let paths = |ops: &[&str]| ops.iter().all(|p| ok_path(p));
    match program {
        "pwd" => args.iter().all(|a| matches!(*a, "-L" | "-P")).then(|| owned(w)),
        "echo" => Some(owned(w)),
        "printf" => (!args.first().is_some_and(|a| a.starts_with('-'))).then(|| owned(w)),
        "ls" | "cat" => {
            let (_, ops) = split_options(args, |_| BOOL)?;
            paths(&ops).then(|| owned(w))
        }
        "wc" => {
            let (_, ops) = split_options(args, |f| {
                let short = !f.starts_with("--") && f[1..].chars().all(|c| "clmwL".contains(c));
                let long = matches!(f, "--bytes" | "--chars" | "--lines" | "--words" | "--max-line-length");
                if short || long {
                    BOOL
                } else {
                    None
                }
            })?;
            paths(&ops).then(|| owned(w))
        }
        "head" | "tail" => {
            let (_, ops) = split_options(args, |f| match f {
                "-n" | "-c" | "--lines" | "--bytes" => VALUE,
                "-q" | "-v" | "-z" | "--quiet" | "--silent" | "--verbose" => BOOL,
                _ => {
                    let value = f
                        .strip_prefix("--lines=")
                        .or_else(|| f.strip_prefix("--bytes="))
                        .or_else(|| f.strip_prefix("-n"))
                        .or_else(|| f.strip_prefix("-c"))
                        .or_else(|| f.strip_prefix('-'))?;
                    if digits(value.trim_start_matches(['+', '-'])) {
                        BOOL
                    } else {
                        None
                    }
                }
            })?;
            paths(&ops).then(|| owned(w))
        }
        "grep" | "rg" => {
            let rg = program == "rg";
            let (patterns, ops) = split_options(args, |f| search_flag(rg, f))?;
            let pattern_given = patterns || (rg && args.iter().any(|a| matches!(*a, "--files" | "--type-list")));
            let files = if pattern_given { &ops[..] } else { ops.get(1..).unwrap_or(&[]) };
            let ok = (pattern_given || !ops.is_empty()) && paths(files);
            ok.then(|| owned(w))
        }
        "git" => git_stage(args, &paths),
        _ => None,
    }
}

/// Option kinds for [`split_options`].
const BOOL: Option<(bool, bool)> = Some((false, false));
const VALUE: Option<(bool, bool)> = Some((true, false));

/// Split `args` into options and operands. `flag` says whether an option is
/// allowed: `Some((takes_next_word, supplies_pattern))`. Returns whether an
/// option supplied a pattern (`-e`, `--regexp`), and the operands. `--` ends
/// the options.
fn split_options<'a>(args: &[&'a str], flag: impl Fn(&str) -> Option<(bool, bool)>) -> Option<(bool, Vec<&'a str>)> {
    let mut ops = Vec::new();
    let mut pattern = false;
    let mut i = 0;
    while i < args.len() {
        let a = args[i];
        if a == "--" {
            ops.extend(&args[i + 1..]);
            break;
        }
        if is_flag(a) {
            let (takes, supplies) = flag(a)?;
            pattern |= supplies;
            if takes {
                args.get(i + 1)?;
                i += 1;
            }
        } else {
            ops.push(a);
        }
        i += 1;
    }
    Some((pattern, ops))
}

/// Options `grep` / `rg` may use: none that run programs, read other files or
/// follow symlinks out of the tree. See [`split_options`] for the result.
fn search_flag(rg: bool, f: &str) -> Option<(bool, bool)> {
    if let Some(long) = f.strip_prefix("--") {
        let (name, value) = match long.split_once('=') {
            Some((n, _)) => (n, true),
            None => (long, false),
        };
        let valued: &[&str] = if rg {
            &[
                "glob", "iglob", "type", "type-not", "max-count", "after-context", "before-context", "context",
                "max-depth", "replace", "regexp", "max-columns", "sort", "sortr", "color", "colors", "threads",
                "encoding", "max-filesize",
            ]
        } else {
            &[
                "regexp", "max-count", "after-context", "before-context", "context", "include", "exclude",
                "exclude-dir", "label", "binary-files", "group-separator",
            ]
        };
        let boolean: &[&str] = if rg {
            &[
                "hidden", "no-ignore", "no-ignore-vcs", "ignore-case", "smart-case", "case-sensitive",
                "line-number", "no-line-number", "files", "files-with-matches", "files-without-match", "count",
                "count-matches", "word-regexp", "line-regexp", "invert-match", "fixed-strings",
                "with-filename", "no-filename", "only-matching", "multiline", "pcre2", "json", "vimgrep",
                "no-heading", "heading", "trim", "stats", "type-list", "text", "null", "column", "no-messages",
                "unrestricted", "quiet", "byte-offset", "no-config",
            ]
        } else {
            &[
                "extended-regexp", "fixed-strings", "basic-regexp", "perl-regexp", "ignore-case", "no-ignore-case",
                "invert-match", "word-regexp", "line-regexp", "count", "files-with-matches",
                "files-without-match", "line-number", "with-filename", "no-filename", "no-messages", "quiet",
                "silent", "only-matching", "byte-offset", "recursive", "text", "null",
                "null-data", "initial-tab", "color", "colour",
            ]
        };
        return if valued.contains(&name) {
            Some((!value, name == "regexp"))
        } else if boolean.contains(&name) || (value && matches!(name, "color" | "colour")) {
            BOOL
        } else {
            None
        };
    }
    // Short options: a cluster of boolean letters, optionally ending in a
    // valued one (`-iA3`, `-e PAT`).
    let (booleans, valued) = if rg {
        ("iIsSnNlcwvxFHhqoUuPa.", "egtTmABCMjr")
    } else {
        ("EFGPiyvwxclLnhHsqobarZzUTI", "emABC")
    };
    for (i, c) in f[1..].char_indices() {
        if valued.contains(c) {
            let rest = &f[1 + i + c.len_utf8()..];
            return Some((rest.is_empty(), c == 'e'));
        }
        if !booleans.contains(c) {
            return None;
        }
    }
    BOOL
}

/// Read-only git: the argv to run (with config that could start programs
/// switched off), or None.
fn git_stage(args: &[&str], paths: &dyn Fn(&[&str]) -> bool) -> Option<Vec<String>> {
    let args = match args.first() {
        Some(&"--no-pager") => &args[1..],
        _ => args,
    };
    let (&sub, rest) = args.split_first()?;
    let (_, ops) = split_options(rest, |_| BOOL)?;
    let has = |names: &[&str]| rest.iter().any(|a| names.iter().any(|n| a == n || a.strip_prefix(n).is_some_and(|r| r.starts_with('='))));
    let mut extra: &[&str] = &[];
    let ok = match sub {
        "status" | "ls-files" | "rev-parse" => paths(&ops),
        "diff" | "log" | "show" => {
            extra = &["--no-ext-diff", "--no-textconv"];
            !has(&["--output", "--no-index", "--ext-diff", "--textconv", "--show-signature"])
                && !rest.iter().any(|a| a.starts_with("-O"))
                && paths(&ops)
        }
        "branch" => {
            let flags_ok = rest.iter().filter(|a| is_flag(a)).all(|a| {
                let name = a.split('=').next().unwrap_or(a);
                matches!(
                    name,
                    "-a" | "-r" | "-v" | "-vv" | "-l" | "-i" | "--list" | "--all" | "--remotes" | "--verbose"
                        | "--show-current" | "--merged" | "--no-merged" | "--contains" | "--no-contains"
                        | "--points-at" | "--sort" | "--format" | "--color" | "--no-color" | "--column"
                        | "--no-column" | "--ignore-case" | "--abbrev" | "--no-abbrev"
                )
            });
            // Names are patterns only in list mode (or a filter's commit).
            let listing = has(&["-l", "--list", "--merged", "--no-merged", "--contains", "--no-contains", "--points-at"]);
            flags_ok && (ops.is_empty() || listing)
        }
        "remote" => matches!(rest, [] | ["-v"] | ["--verbose"]) || matches!(rest, ["get-url", name] if !is_flag(name)),
        "worktree" => {
            matches!(rest.first(), Some(&"list")) && rest[1..].iter().all(|a| matches!(*a, "--porcelain" | "-v" | "--verbose" | "-z"))
        }
        _ => false,
    };
    if !ok {
        return None;
    }
    let argv = [
        "git",
        "--no-pager",
        "-c",
        "core.fsmonitor=false",
        "-c",
        "core.hooksPath=/dev/null",
        "-c",
        "log.showSignature=false",
        sub,
    ]
    .iter()
    .chain(extra)
    .chain(rest)
    .map(|s| s.to_string())
    .collect();
    Some(argv)
}

/// Whether auto mode may run `cmd` without the classifier: a `&&` chain of
/// read-only commands and known, reversible development commands.
pub fn known_reversible(cmd: &str, cwd: &Path, extra: &[PathBuf]) -> bool {
    let Some(stages) = stages(cmd, "&&", true) else {
        return false;
    };
    stages.iter().all(|words| {
        if words.iter().any(|w| w.dynamic) {
            return false;
        }
        let text: Vec<&str> = words.iter().map(|w| w.text.as_str()).collect();
        let line = text.iter().map(|w| quote(w)).collect::<Vec<_>>().join(" ");
        reversible_stage(&text) || read_only(&line, cwd, extra).is_some()
    })
}

const VERSIONED: &[&str] = &[
    "node", "npm", "pnpm", "bun", "yarn", "deno", "cargo", "rustc", "rustup", "git", "python", "python3", "go",
    "java", "javac", "make", "cmake", "gcc", "clang", "tsc",
];

fn reversible_stage(w: &[&str]) -> bool {
    let Some((&program, args)) = w.split_first() else {
        return false;
    };
    if matches!(args, ["--version"]) || (matches!(args, ["-v"]) && matches!(program, "node" | "npm" | "pnpm" | "bun")) {
        return VERSIONED.contains(&program);
    }
    match (program, args) {
        ("git", ["status", rest @ ..]) => rest.iter().all(|a| is_flag(a)),
        ("git", ["remote", "-v"]) => true,
        ("git", ["worktree", "list", rest @ ..]) => rest.iter().all(|a| matches!(*a, "--porcelain" | "-v" | "--verbose")),
        ("git", ["fetch", rest @ ..]) => rest.iter().all(|a| {
            matches!(*a, "--all" | "--tags" | "--no-tags" | "-q" | "--quiet" | "-v" | "--verbose" | "--dry-run")
                || a.strip_prefix("--depth=").is_some_and(digits)
                || (!is_flag(a) && !a.contains("::"))
        }),
        ("npm", [sub, rest @ ..]) if matches!(*sub, "install" | "i" | "ci" | "test" | "t") => !install_escapes(rest),
        ("pnpm" | "bun", [sub, rest @ ..]) if matches!(*sub, "install" | "i" | "ci" | "test") => !install_escapes(rest),
        ("npm" | "pnpm" | "bun", ["run", script, rest @ ..]) => !is_flag(script) && !install_escapes(rest),
        ("cargo", [sub, rest @ ..]) => {
            let outside = rest.iter().any(|a| {
                ["--config", "-Z", "--manifest-path", "--target-dir", "--out-dir", "--artifact-dir"]
                    .iter()
                    .any(|f| a == f || a.strip_prefix(f).is_some_and(|r| r.starts_with('=')))
                    || a.starts_with("-Z")
            });
            !outside
                && (matches!(*sub, "build" | "b" | "test" | "t" | "check" | "c" | "clippy")
                    || (*sub == "fmt" && rest.contains(&"--check")))
        }
        _ => false,
    }
}

/// Whether an `npm`/`pnpm`/`bun` install or `run` argument can leave the
/// project. `--global=true` is a global install; a config file or
/// `--config.*` can set `global`, `prefix`, or a script shell; `--dir`,
/// `-C` and `--modules-dir` write another directory.
fn install_escapes(args: &[&str]) -> bool {
    args.iter().any(|a| {
        matches!(
            *a,
            "-g" | "--global"
                | "--location"
                | "--prefix"
                | "--userconfig"
                | "--globalconfig"
                | "--config"
                | "-C"
                | "--dir"
                | "--modules-dir"
        ) || [
            "--location=",
            "--prefix=",
            "--global=",
            "--userconfig=",
            "--globalconfig=",
            "--config.",
            "--config=",
            "--dir=",
            "--modules-dir=",
        ]
        .iter()
        .any(|p| a.starts_with(p))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ro(cmd: &str) -> Option<String> {
        let dir = std::env::temp_dir();
        read_only(cmd, &dir, &[])
    }

    #[test]
    fn dynamic_extra_arguments_are_not_read_only() {
        for arg in ["$'--short'", "$'\\x2d\\x2dshort'", "$\"--short\"", "$", "$FLAGS", "${FLAGS}", "`echo --short`", "$(echo --short)"] {
            let cmd = format!("git status {arg}");
            assert!(ro(&cmd).is_none(), "{cmd}");
            assert!(!known_reversible(&cmd, &std::env::temp_dir(), &[]), "{cmd}");
        }
        assert!(ro("echo '$HOME'").is_some());
    }

    #[test]
    fn lexing() {
        let words = |cmd: &str| -> Vec<String> {
            lex(cmd)
                .unwrap()
                .into_iter()
                .map(|t| match t {
                    Token::Word(w) => format!("{}{}", w.text, if w.dynamic { "!" } else { "" }),
                    Token::Op(o) => format!("<{o}>"),
                })
                .collect()
        };
        assert_eq!(words("grep -n 'a b' \"c d\"|wc -l"), ["grep", "-n", "a b", "c d", "<|>", "wc", "-l"]);
        assert_eq!(words("a&&b 2>&1;c"), ["a", "<&&>", "b", "2", "<>&>", "1", "<;>", "c"]);
        assert_eq!(words("echo $HOME \"$x\" '$y' *.rs ~/a a~b"), ["echo", "$HOME!", "$x!", "$y", "*.rs!", "~/a!", "a~b"]);
        assert_eq!(words("echo it''s"), ["echo", "its"]);
        assert_eq!(words("printf '%s\\n' a"), ["printf", "%s\\n", "a"]);
        for bad in ["echo 'open", "echo \"open", "a\\ b", "echo \"a\\\"\"", "ls\nrm x", "ls # c"] {
            assert!(lex(bad).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn read_only_commands_are_admitted() {
        for cmd in [
            "ls",
            "ls -la src",
            "cat Cargo.toml",
            "head -n 20 README.md",
            "tail -5 a.log",
            "wc -l src/main.rs",
            "grep -rn 'fn main' src",
            "grep -e foo -e bar a.txt",
            "rg -n --hidden -g '*.rs' parse src",
            "rg --files",
            "pwd",
            "echo hello world",
            "printf '%s\\n' a",
            "git status --short",
            "git log --oneline -n 5",
            "git diff HEAD~1 -- src",
            "git show HEAD:README.md",
            "git branch -a",
            "git branch --list 'feat*'",
            "git rev-parse --abbrev-ref HEAD",
            "git remote -v",
            "git worktree list --porcelain",
            "git --no-pager log",
            "git status --short | wc -l",
        ] {
            assert!(ro(cmd).is_some(), "{cmd}");
        }
    }

    #[test]
    fn everything_else_still_asks() {
        for cmd in [
            "",
            "rm -rf x",
            "ls > out.txt",
            "cat < in",
            "cat $(echo a)",
            "cat `x`",
            "echo $HOME",
            "cat *.rs",
            "cat ~/.ssh/id_rsa",
            "cat /etc/passwd",
            "cat ../outside",
            "FOO=1 ls",
            "ls; rm x",
            "ls && rm x",
            "ls || true",
            "ls &",
            "(ls)",
            "sort a",
            "tail -f a.log",
            "wc --files0-from=list",
            "grep -f patterns.txt a",
            "grep -ie x /etc/passwd",
            "grep -R x src",
            "rg -L x",
            "git log -O/etc/order",
            "grep pattern /etc/hosts",
            "rg --pre cat x",
            "rg --pre=cat x",
            "git push",
            "git -C /tmp status",
            "git -c core.pager=x log",
            "git diff --output=x",
            "git diff --no-index a /etc/passwd",
            "git branch new-branch",
            "git branch -D old",
            "git remote add x y",
            "git worktree add ../x",
            "git log --show-signature",
            "printf -v x a",
            "a | b | c | d | e | f | g | h | i",
            "echo 1 | echo 2 | echo 3 | echo 4 | echo 5 | echo 6 | echo 7 | echo 8 | echo 9",
        ] {
            assert!(ro(cmd).is_none(), "{cmd:?}");
        }
        assert!(ro("echo 1 | echo 2 | echo 3 | echo 4 | echo 5 | echo 6 | echo 7 | echo 8").is_some());
    }

    #[test]
    fn the_command_that_runs_is_rebuilt_and_hardened() {
        assert_eq!(ro("grep -n \"it's here\" a.txt").unwrap(), "grep -n 'it'\\''s here' a.txt");
        assert_eq!(
            ro("git diff --stat").unwrap(),
            "git --no-pager -c core.fsmonitor=false -c core.hooksPath=/dev/null -c log.showSignature=false diff --no-ext-diff --no-textconv --stat"
        );
        assert!(ro("git status | head -n 3").unwrap().ends_with("status | head -n 3"));
    }

    #[test]
    fn reversible_chains_for_auto_mode() {
        let dir = std::env::temp_dir();
        let ok = |cmd: &str| known_reversible(cmd, &dir, &[]);
        for cmd in [
            "git status && git fetch origin main",
            "git remote -v",
            "git worktree list",
            "npm install 2>&1",
            "npm ci && npm test",
            "pnpm install && pnpm run build",
            "bun test",
            "cargo build --release && cargo test -p nyq",
            "cargo clippy --all-targets -- -D warnings",
            "cargo fmt --check",
            "node --version && npm -v",
            "cargo check && git diff --stat",
        ] {
            assert!(ok(cmd), "{cmd}");
        }
        for cmd in [
            "git fetch --prune origin",
            "git fetch --upload-pack=evil origin",
            "git push origin HEAD",
            "npm install -g x",
            "npm install --prefix=/tmp/g",
            "npm install --global=true left-pad",
            "npm install --userconfig=.npmrc left-pad",
            "npm install --globalconfig=/tmp/npmrc left-pad",
            "pnpm install --global=true left-pad",
            "pnpm install --config.global=true left-pad",
            "pnpm install --dir /tmp/other left-pad",
            "pnpm install -C /tmp/other left-pad",
            "pnpm install --modules-dir=/tmp/mods left-pad",
            "pnpm run build --config.script-shell=/tmp/evil",
            "npm exec -- rm -rf .",
            "npm run",
            "cargo fmt",
            "cargo build --target-dir /tmp/x",
            "cargo build; rm x",
            "cargo build || true",
            "cargo build | tee log",
            "cargo build > log",
            "sh -c 'npm install'",
            "rm -rf .",
            "evil --version",
            "npm install $PKG",
            "cargo test 2>&1 | tail",
        ] {
            assert!(!ok(cmd), "{cmd}");
        }
    }
}
