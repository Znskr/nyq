//! Syntax highlighting for fenced code blocks, one line at a time, with the
//! syntaxes bundled in syntect and colors from the theme palette.

use std::sync::OnceLock;

use nu_ansi_term::Style;
use syntect::parsing::{ParseState, Scope, ScopeStack, SyntaxSet};

use super::theme::{fg, BLUE, GRAY, GREEN, MAGENTA, MID, ORANGE};

fn syntaxes() -> &'static SyntaxSet {
    static SET: OnceLock<SyntaxSet> = OnceLock::new();
    SET.get_or_init(SyntaxSet::load_defaults_newlines)
}

#[derive(Clone, Copy)]
enum Role {
    Comment,
    Str,
    Number,
    Keyword,
    Function,
    Type,
    Plain,
}

/// Scope prefixes and their role, most specific first.
fn roles() -> &'static [(Scope, Role)] {
    static ROLES: OnceLock<Vec<(Scope, Role)>> = OnceLock::new();
    ROLES.get_or_init(|| {
        [
            ("comment", Role::Comment),
            ("string", Role::Str),
            ("constant.numeric", Role::Number),
            ("constant.language", Role::Number),
            ("constant.character", Role::Str),
            ("keyword.operator", Role::Plain),
            ("keyword", Role::Keyword),
            ("storage", Role::Keyword),
            ("entity.name.function", Role::Function),
            ("support.function", Role::Function),
            ("entity.name", Role::Type),
            ("support.type", Role::Type),
            ("support.class", Role::Type),
            ("entity.other.attribute-name", Role::Type),
        ]
        .iter()
        .filter_map(|(s, r)| Some((Scope::new(s).ok()?, *r)))
        .collect()
    })
}

fn style(stack: &ScopeStack) -> Style {
    let role = stack
        .as_slice()
        .iter()
        .rev()
        .find_map(|scope| roles().iter().find(|(p, _)| p.is_prefix_of(*scope)).map(|(_, r)| *r))
        .unwrap_or(Role::Plain);
    match role {
        Role::Comment => fg(GRAY).italic(),
        Role::Str => fg(GREEN),
        Role::Number => fg(ORANGE),
        Role::Keyword => fg(MAGENTA),
        Role::Function => fg(BLUE),
        Role::Type => fg(BLUE).italic(),
        Role::Plain => fg(MID),
    }
}

/// Fence info strings that name a bundled syntax differently.
fn token(lang: &str) -> &str {
    match lang {
        "ts" | "typescript" | "tsx" | "jsx" | "javascript" | "mjs" | "cjs" | "node" => "js",
        "shell" | "bash" | "zsh" | "console" | "shellscript" | "sh-session" => "sh",
        "python" | "python3" | "py3" => "py",
        "rust" => "rs",
        "golang" => "go",
        "c++" | "cxx" | "hpp" => "cpp",
        "c#" | "csharp" => "cs",
        "yml" => "yaml",
        "ruby" => "rb",
        "markdown" => "md",
        "objc" | "objective-c" => "m",
        other => other,
    }
}

/// The highlighting state of one code block.
pub struct Code {
    state: ParseState,
    stack: ScopeStack,
}

impl Code {
    /// None when no bundled syntax matches `lang` (the fence's info string).
    pub fn new(lang: &str) -> Option<Code> {
        let lang = lang.split_whitespace().next()?.trim_start_matches('.').to_ascii_lowercase();
        let syntax = syntaxes().find_syntax_by_token(token(&lang))?;
        Some(Code { state: ParseState::new(syntax), stack: ScopeStack::new() })
    }

    /// One line of the block, colored.
    pub fn line(&mut self, line: &str) -> String {
        let text = format!("{line}\n");
        let Ok(ops) = self.state.parse_line(&text, syntaxes()) else {
            return fg(MID).paint(line).to_string();
        };
        let mut out = String::new();
        let mut at = 0;
        let put = |out: &mut String, s: &str, stack: &ScopeStack| {
            if !s.is_empty() {
                out.push_str(&style(stack).paint(s).to_string());
            }
        };
        for (pos, op) in ops {
            let pos = pos.min(line.len());
            if pos > at {
                put(&mut out, &line[at..pos], &self.stack);
                at = pos;
            }
            let _ = self.stack.apply(&op);
        }
        put(&mut out, &line[at..], &self.stack);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::render::strip_ansi;

    #[test]
    fn highlights_known_languages() {
        let mut rust = Code::new("rust").unwrap();
        let out = rust.line("fn main() { let x = \"hi\"; // note");
        assert_eq!(strip_ansi(&out), "fn main() { let x = \"hi\"; // note");
        let keyword = fg(MAGENTA).paint("fn").to_string();
        assert!(out.starts_with(&keyword), "{out:?}");
        assert!(out.contains(&fg(GREEN).paint("hi").to_string()), "{out:?}");
        assert!(out.contains(&fg(GRAY).italic().paint(" note").to_string()), "{out:?}");
        // Block comments carry over to the next line.
        let mut c = Code::new("c").unwrap();
        c.line("/* start");
        assert!(c.line("still comment").contains(&fg(GRAY).italic().paint("still comment").to_string()));
        for lang in ["ts", "bash", "python", "go", "json", "yaml", "sql", "java", "ruby", "c++", "html", "css"] {
            assert!(Code::new(lang).is_some(), "{lang}");
        }
        assert!(Code::new("no-such-language").is_none());
        assert!(Code::new("").is_none());
    }
}
