//! Prompt templates (aliases): markdown files in `<home>/prompts/*.md` and
//! `<cwd>/.nyq/prompts/*.md` become slash commands. `/name args` sends the
//! file's text with `$ARGUMENTS` (all arguments) and `$1`…`$9` filled in.

use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq)]
pub struct Template {
    /// Command name without the slash.
    pub name: String,
    pub description: String,
    pub body: String,
    pub path: PathBuf,
    /// "project" or "profile".
    pub scope: &'static str,
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

/// Split optional `---` front matter from the body; returns (description, body).
fn parse(text: &str) -> (Option<String>, String) {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut description = None;
    let mut body = text;
    if let Some(rest) = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))
    {
        if let Some(end) = rest.find("\n---") {
            for line in rest[..end].lines() {
                if let Some((k, v)) = line.split_once(':') {
                    if k.trim() == "description" {
                        description =
                            Some(v.trim().trim_matches(|c| c == '"' || c == '\'').to_string());
                    }
                }
            }
            body = rest[end + 4..]
                .trim_start_matches(['-'])
                .trim_start_matches(['\r', '\n']);
        }
    }
    (
        description.filter(|d| !d.is_empty()),
        body.trim_end().to_string(),
    )
}

fn read_dir(dir: &Path, scope: &'static str, out: &mut Vec<Template>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "md"))
        .collect();
    paths.sort();
    for path in paths {
        let Some(name) = path
            .file_stem()
            .and_then(|s| s.to_str())
            .map(str::to_string)
            .filter(|n| valid_name(n))
        else {
            continue;
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let (description, body) = parse(&text);
        if body.trim().is_empty() {
            continue;
        }
        let description = description.unwrap_or_else(|| {
            let first = body
                .lines()
                .map(str::trim)
                .find(|l| !l.is_empty())
                .unwrap_or("")
                .trim_start_matches('#')
                .trim();
            first.chars().take(60).collect()
        });
        out.retain(|t: &Template| t.name != name);
        out.push(Template {
            name,
            description,
            body,
            path,
            scope,
        });
    }
}

/// Templates from the profile, then the project (a project file wins over a
/// profile file with the same name). Names in `reserved` (built-in commands,
/// without the slash) are skipped.
pub fn discover(home: &Path, cwd: &Path, reserved: &[&str]) -> Vec<Template> {
    let mut out = Vec::new();
    read_dir(&home.join("prompts"), "profile", &mut out);
    if !crate::config::workspace_is_profile(cwd) {
        read_dir(&cwd.join(".nyq").join("prompts"), "project", &mut out);
    }
    out.retain(|t| !reserved.contains(&t.name.as_str()));
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Split arguments like a shell would for `$1`…`$9`: whitespace separates,
/// single or double quotes group.
pub fn split_args(args: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote = None;
    let mut started = false;
    for c in args.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => cur.push(c),
            (None, '"' | '\'') => {
                quote = Some(c);
                started = true;
            }
            (None, c) if c.is_whitespace() => {
                if started || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
                started = false;
            }
            (None, c) => cur.push(c),
        }
    }
    if started || !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Fill in `$ARGUMENTS` and `$1`…`$9` (missing ones become empty; `$$` is a
/// literal `$`). A template
/// without placeholders gets the arguments appended as a new paragraph.
pub fn expand(body: &str, args: &str) -> String {
    let args = args.trim();
    let positional = split_args(args);
    let mut out = String::with_capacity(body.len() + args.len());
    let mut used = false;
    let mut rest = body;
    while let Some(i) = rest.find('$') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        if let Some(tail) = after.strip_prefix('$') {
            // `$$` is a literal dollar sign.
            out.push('$');
            rest = tail;
        } else if let Some(tail) = after.strip_prefix("ARGUMENTS") {
            out.push_str(args);
            used = true;
            rest = tail;
        } else if let Some(d) = after.chars().next().filter(|c| ('1'..='9').contains(c)) {
            let n = d as usize - '1' as usize;
            out.push_str(positional.get(n).map(String::as_str).unwrap_or(""));
            used = true;
            rest = &after[1..];
        } else {
            out.push('$');
            rest = after;
        }
    }
    out.push_str(rest);
    if !used && !args.is_empty() {
        out.push_str("\n\n");
        out.push_str(args);
    }
    out
}

/// If `line` invokes a template (`/name args`), the message to send.
pub fn invoke(templates: &[Template], line: &str) -> Option<String> {
    let line = line.trim_start().strip_prefix('/')?;
    let (name, args) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
    templates
        .iter()
        .find(|t| t.name == name)
        .map(|t| expand(&t.body, args))
}

/// `/alias` listing rows.
pub fn list_lines(templates: &[Template]) -> Vec<String> {
    if templates.is_empty() {
        return vec![
            "No prompt templates. Add markdown files to ~/.nyq/prompts/ or .nyq/prompts/;".into(),
            "each NAME.md becomes /NAME ($ARGUMENTS and $1…$9 are replaced by what you type after it).".into(),
        ];
    }
    templates
        .iter()
        .map(|t| {
            format!(
                "/{:<16} {}  ({} · {})",
                t.name,
                t.description,
                t.scope,
                t.path.display()
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substitution() {
        assert_eq!(
            expand("Review $1 for $2.", "src/main.rs \"memory leaks\""),
            "Review src/main.rs for memory leaks."
        );
        assert_eq!(expand("Do: $ARGUMENTS!", "  a b  "), "Do: a b!");
        assert_eq!(
            expand("Missing [$3] costs $$5 or $x", "a"),
            "Missing [] costs $5 or $x"
        );
        assert_eq!(
            expand("Explain the code.", "in detail"),
            "Explain the code.\n\nin detail"
        );
        assert_eq!(expand("Explain the code.", ""), "Explain the code.");
        assert_eq!(split_args("a 'b c' \"\" d"), vec!["a", "b c", "", "d"]);
        assert_eq!(expand("é$1é", "ü"), "éüé");
    }

    #[test]
    fn discovery_and_invocation() {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let profile = home.path().join("prompts");
        let project = cwd.path().join(".nyq").join("prompts");
        std::fs::create_dir_all(&profile).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(
            profile.join("review.md"),
            "---\ndescription: \"Review a file\"\n---\nReview $1 carefully.\n",
        )
        .unwrap();
        std::fs::write(profile.join("fix.md"), "# Fix the bug\nFix: $ARGUMENTS").unwrap();
        std::fs::write(project.join("fix.md"), "Project fix: $ARGUMENTS").unwrap();
        std::fs::write(profile.join("help.md"), "shadowed by a built-in").unwrap();
        std::fs::write(profile.join("bad name.md"), "ignored").unwrap();
        std::fs::write(profile.join("notes.txt"), "ignored").unwrap();
        let t = discover(home.path(), cwd.path(), &["help"]);
        let names: Vec<&str> = t.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["fix", "review"]);
        assert_eq!(t[0].scope, "project");
        assert_eq!(t[1].description, "Review a file");
        assert_eq!(t[1].body, "Review $1 carefully.");
        assert_eq!(
            invoke(&t, "/review src/a.rs").as_deref(),
            Some("Review src/a.rs carefully.")
        );
        assert_eq!(
            invoke(&t, "/fix the  tests").as_deref(),
            Some("Project fix: the  tests")
        );
        assert_eq!(invoke(&t, "/reviewer x"), None);
        assert_eq!(invoke(&t, "review x"), None);
        assert!(list_lines(&t)[0].starts_with("/fix"));
        let only_profile = discover(home.path(), &home.path().join("none"), &[]);
        assert_eq!(
            only_profile
                .iter()
                .find(|t| t.name == "fix")
                .unwrap()
                .description,
            "Fix the bug"
        );
    }
}
