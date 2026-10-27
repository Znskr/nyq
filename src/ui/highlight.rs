//! Syntax highlighting for fenced code blocks, one line at a time: a small
//! table-driven lexer (comments, strings, numbers, keywords, types, calls)
//! with colors from the theme palette. Approximate by design.

use nu_ansi_term::Style;

use super::theme::{fg, BLUE, GRAY, GREEN, MAGENTA, MID, ORANGE, RED};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    Comment,
    Str,
    Number,
    Keyword,
    Function,
    Type,
    Plain,
    Removed,
}

fn style(role: Role) -> Style {
    match role {
        Role::Comment => fg(GRAY).italic(),
        Role::Str => fg(GREEN),
        Role::Number => fg(ORANGE),
        Role::Keyword => fg(MAGENTA),
        Role::Function => fg(BLUE),
        Role::Type => fg(BLUE).italic(),
        Role::Plain => fg(MID),
        Role::Removed => fg(RED),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Code,
    Diff,
    Markup,
    Json,
    Yaml,
    Toml,
    Make,
    Docker,
    Css,
}

/// String delimiter flags.
const ML: u8 = 1; // may span lines
const ESC: u8 = 2; // backslash escapes
const CHR: u8 = 4; // Rust-style char literal (else a lifetime / label)

/// Language flags.
const CAPS: u16 = 1; // Capitalized words are types, ALL_CAPS are constants
const NOCASE: u16 = 2; // keywords are case-insensitive
const VARS: u16 = 4; // `$name`, `${..}`, `$(..)` are variables
const DOLLAR: u16 = 8; // `$` is an identifier character
const PREFIX: u16 = 16; // short string prefixes (`r"`, `b'`, `f"`)
const RUST: u16 = 32; // raw strings `r#".."#`, macros `name!(`
const PREPROC: u16 = 64; // `#include` and friends are keywords
const DASH: u16 = 128; // `-` is an identifier character

struct Lang {
    line: &'static [&'static str],
    block: &'static [(&'static str, &'static str)],
    strs: &'static [(&'static str, &'static str, u8)],
    /// Space-separated word lists.
    kw: &'static str,
    types: &'static str,
    consts: &'static str,
    flags: u16,
    mode: Mode,
}

const SLASH: &[&str] = &["//"];
const HASH: &[&str] = &["#"];
const C_BLOCK: &[(&str, &str)] = &[("/*", "*/")];
const QUOTES: &[(&str, &str, u8)] = &[("\"", "\"", ESC), ("'", "'", ESC)];

const fn code(
    line: &'static [&'static str],
    block: &'static [(&'static str, &'static str)],
    strs: &'static [(&'static str, &'static str, u8)],
    kw: &'static str,
    types: &'static str,
    consts: &'static str,
    flags: u16,
) -> Lang {
    Lang {
        line,
        block,
        strs,
        kw,
        types,
        consts,
        flags,
        mode: Mode::Code,
    }
}

const fn special(
    mode: Mode,
    line: &'static [&'static str],
    strs: &'static [(&'static str, &'static str, u8)],
    consts: &'static str,
) -> Lang {
    Lang {
        line,
        block: &[],
        strs,
        kw: "",
        types: "",
        consts,
        flags: 0,
        mode,
    }
}

const C_TYPES: &str = "char double float int long short signed unsigned void bool size_t ssize_t int8_t int16_t int32_t int64_t uint8_t uint16_t uint32_t uint64_t";

static RUST_L: Lang = code(
    SLASH,
    C_BLOCK,
    &[("\"", "\"", ML | ESC), ("'", "'", ESC | CHR)],
    "as async await break const continue crate dyn else enum extern fn for if impl in let loop match mod move mut pub ref return self Self static struct super trait type unsafe use where while yield",
    "bool char str u8 u16 u32 u64 u128 usize i8 i16 i32 i64 i128 isize f32 f64",
    "true false",
    CAPS | PREFIX | RUST,
);
static PY: Lang = code(
    HASH,
    &[],
    &[("\"\"\"", "\"\"\"", ML | ESC), ("'''", "'''", ML | ESC), ("\"", "\"", ESC), ("'", "'", ESC)],
    "and as assert async await break case class continue def del elif else except finally for from global if import in is lambda match nonlocal not or pass raise return self try while with yield",
    "int float str bool list dict set tuple bytes object type",
    "True False None",
    CAPS | PREFIX,
);
static JS: Lang = code(
    SLASH,
    C_BLOCK,
    &[("\"", "\"", ESC), ("'", "'", ESC), ("`", "`", ML | ESC)],
    "abstract as async await break case catch class const continue debugger declare default delete do else enum export extends finally for from function if implements import in instanceof interface keyof let namespace new of private protected public readonly return static super switch this throw try type typeof var void while with yield",
    "string number boolean any unknown never object symbol bigint",
    "true false null undefined NaN Infinity",
    CAPS | DOLLAR,
);
static GO: Lang = code(
    SLASH,
    C_BLOCK,
    &[("\"", "\"", ESC), ("'", "'", ESC), ("`", "`", ML)],
    "break case chan const continue default defer else fallthrough for func go goto if import interface map package range return select struct switch type var",
    "bool byte complex64 complex128 error float32 float64 int int8 int16 int32 int64 rune string uint uint8 uint16 uint32 uint64 uintptr any",
    "true false nil iota",
    CAPS,
);
static C: Lang = code(
    SLASH,
    C_BLOCK,
    QUOTES,
    "auto break case const continue default do else enum extern for goto if inline register restrict return sizeof static struct switch typedef union volatile while",
    C_TYPES,
    "true false NULL",
    CAPS | PREPROC,
);
static CPP: Lang = code(
    SLASH,
    C_BLOCK,
    QUOTES,
    "alignas alignof auto break case catch class concept const consteval constexpr const_cast continue co_await co_return co_yield decltype default delete do dynamic_cast else enum explicit export extern final for friend goto if inline mutable namespace new noexcept operator override private protected public register reinterpret_cast requires return sizeof static static_assert static_cast struct switch template this throw try typedef typename union using virtual volatile while",
    "char double float int long short signed unsigned void bool size_t wchar_t char8_t char16_t char32_t int8_t int16_t int32_t int64_t uint8_t uint16_t uint32_t uint64_t std string vector",
    "true false nullptr NULL",
    CAPS | PREPROC,
);
static CS: Lang = code(
    SLASH,
    C_BLOCK,
    QUOTES,
    "abstract as async await base break case catch checked class const continue default delegate do else enum event explicit extern finally fixed for foreach get goto if implicit in init interface internal is lock namespace new operator out override params private protected public readonly record ref return sealed set sizeof stackalloc static struct switch this throw try typeof unchecked unsafe using var virtual volatile when where while yield",
    "bool byte char decimal double dynamic float int long object sbyte short string uint ulong ushort void",
    "true false null",
    CAPS | PREPROC,
);
static JAVA: Lang = code(
    SLASH,
    C_BLOCK,
    &[("\"\"\"", "\"\"\"", ML | ESC), ("\"", "\"", ESC), ("'", "'", ESC)],
    "abstract assert break case catch class continue default do else enum extends final finally for if implements import instanceof interface native new package permits private protected public record return sealed static super switch synchronized this throw throws transient try var volatile while yield",
    "boolean byte char double float int long short void",
    "true false null",
    CAPS,
);
static KOTLIN: Lang = code(
    SLASH,
    C_BLOCK,
    &[("\"\"\"", "\"\"\"", ML), ("\"", "\"", ESC), ("'", "'", ESC)],
    "abstract as break by catch class companion constructor continue data do else enum finally for fun get if import in init inner interface internal is lateinit object open override package private protected public return sealed set super suspend this throw try typealias val var when while",
    "",
    "true false null",
    CAPS,
);
static SWIFT: Lang = code(
    SLASH,
    C_BLOCK,
    &[("\"\"\"", "\"\"\"", ML | ESC), ("\"", "\"", ESC)],
    "any as associatedtype async await break case catch class continue default defer deinit do else enum extension fallthrough fileprivate final for func guard if import in init inout internal is lazy let mutating open operator override private protocol public repeat rethrows return self Self some static struct subscript switch throw throws try typealias var weak where while",
    "",
    "true false nil",
    CAPS,
);
static RUBY: Lang = code(
    HASH,
    &[("=begin", "=end")],
    &[("\"", "\"", ESC), ("'", "'", ESC), ("`", "`", ESC)],
    "BEGIN END alias and attr_accessor attr_reader attr_writer begin break case class def defined? do else elsif end ensure for if in module next not or private puts redo require require_relative rescue retry return self super then undef unless until when while yield",
    "",
    "true false nil",
    CAPS,
);
static PHP: Lang = code(
    &["//", "#"],
    C_BLOCK,
    QUOTES,
    "abstract and array as break callable case catch class clone const continue declare default do echo else elseif empty enddeclare endfor endforeach endif endswitch endwhile extends final finally fn for foreach function global goto if implements include include_once instanceof insteadof interface isset list match namespace new or print private protected public readonly require require_once return static switch throw trait try unset use var while yield",
    "int float string bool array object mixed void",
    "true false null TRUE FALSE NULL",
    CAPS | DOLLAR,
);
const SH_KW: &str = "alias break case continue declare do done elif else esac eval exec exit export fi for function if in local readonly return select set shift source then trap unset until while";
static SH: Lang = code(
    HASH,
    &[],
    &[("\"", "\"", ML | ESC), ("'", "'", ML)],
    SH_KW,
    "",
    "true false",
    VARS,
);
static SQL: Lang = code(
    &["--"],
    C_BLOCK,
    &[("'", "'", 0), ("\"", "\"", 0)],
    "add all alter and as asc begin between by case check column commit constraint create cross default delete desc distinct drop else end exists foreign from full group having if in index inner insert into is join key left like limit not offset on or order outer primary references returning right rollback select set table then transaction union unique update values view when where with",
    "bigint blob bool boolean char date decimal double float int integer json jsonb numeric real serial smallint text timestamp uuid varchar",
    "true false null",
    NOCASE,
);
static LUA: Lang = code(
    &["--"],
    &[("--[[", "]]")],
    &[("\"", "\"", ESC), ("'", "'", ESC), ("[[", "]]", ML)],
    "and break do else elseif end for function goto if in local not or repeat return then until while",
    "",
    "true false nil",
    0,
);
static CSS: Lang = Lang {
    line: &[],
    block: C_BLOCK,
    strs: QUOTES,
    kw: "",
    types: "",
    consts: "",
    flags: DASH,
    mode: Mode::Css,
};
static DOCKER: Lang = Lang {
    line: HASH,
    block: &[],
    strs: QUOTES,
    kw: SH_KW,
    types: "",
    consts: "",
    flags: VARS,
    mode: Mode::Docker,
};
static MAKE: Lang = Lang {
    line: HASH,
    block: &[],
    strs: QUOTES,
    kw: "define else endef endif export ifdef ifeq ifndef ifneq include override",
    types: "",
    consts: "",
    flags: VARS,
    mode: Mode::Make,
};
static JSON: Lang = special(Mode::Json, SLASH, &[("\"", "\"", ESC)], "true false null");
static YAML: Lang = special(Mode::Yaml, HASH, QUOTES, "true false null yes no on off ~");
static TOML: Lang = special(
    Mode::Toml,
    HASH,
    &[
        ("\"\"\"", "\"\"\"", ML | ESC),
        ("'''", "'''", ML),
        ("\"", "\"", ESC),
        ("'", "'", 0),
    ],
    "true false",
);
static MARKUP: Lang = special(Mode::Markup, &[], &[], "");
static DIFF: Lang = special(Mode::Diff, &[], &[], "");

/// Instructions that open a Dockerfile line.
const DOCKER_LEAD: &str = "from run cmd label maintainer expose env add copy entrypoint volume user workdir arg onbuild stopsignal healthcheck shell";

/// Fence info strings that name a language differently.
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

fn lang(token: &str) -> Option<&'static Lang> {
    Some(match token {
        "rs" => &RUST_L,
        "py" | "pyi" | "gyp" => &PY,
        "js" | "json5" => &JS,
        "go" => &GO,
        "c" | "h" | "m" => &C,
        "cpp" | "cc" | "hh" | "hxx" | "cuda" | "cu" => &CPP,
        "cs" => &CS,
        "java" | "groovy" | "gradle" | "scala" | "dart" => &JAVA,
        "kt" | "kts" | "kotlin" => &KOTLIN,
        "swift" => &SWIFT,
        "rb" | "rake" | "gemspec" => &RUBY,
        "php" => &PHP,
        "sh" | "ksh" | "fish" | "zshrc" | "bashrc" | "env" | "dotenv" | "ps1" | "powershell" => &SH,
        "sql" | "psql" | "mysql" | "sqlite" | "postgres" | "postgresql" => &SQL,
        "lua" => &LUA,
        "css" | "scss" | "less" => &CSS,
        "dockerfile" | "docker" | "containerfile" => &DOCKER,
        "makefile" | "make" | "mk" | "mak" => &MAKE,
        "json" | "jsonc" | "jsonl" | "ndjson" | "geojson" => &JSON,
        "yaml" => &YAML,
        "toml" | "ini" | "cfg" | "conf" => &TOML,
        "html" | "htm" | "xml" | "xhtml" | "svg" | "vue" | "svelte" | "plist" | "xsl" => &MARKUP,
        "diff" | "patch" | "udiff" => &DIFF,
        _ => return None,
    })
}

fn has(list: &str, word: &str, nocase: bool) -> bool {
    list.split(' ').any(|w| {
        if nocase {
            w.eq_ignore_ascii_case(word)
        } else {
            w == word
        }
    })
}

fn ident_byte(b: u8, flags: u16) -> bool {
    b.is_ascii_alphanumeric()
        || b == b'_'
        || b >= 0x80
        || (b == b'$' && flags & DOLLAR != 0)
        || (b == b'-' && flags & DASH != 0)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Normal,
    Block(&'static str),
    Str(&'static str, u8),
    Tag,
}

/// Raw string closers by `#` count.
const RAW: [&str; 4] = ["\"", "\"#", "\"##", "\"###"];

/// The highlighting state of one code block.
pub struct Code {
    lang: &'static Lang,
    state: State,
    /// Brace depth (CSS: inside a rule block).
    depth: u32,
}

/// Spans of one line, adjacent same-role spans merged.
struct Out {
    spans: Vec<(usize, usize, Role)>,
}

impl Out {
    fn push(&mut self, a: usize, b: usize, role: Role) {
        if a >= b {
            return;
        }
        match self.spans.last_mut() {
            Some(last) if last.2 == role && last.1 == a => last.1 = b,
            _ => self.spans.push((a, b, role)),
        }
    }
}

impl Code {
    /// None when `lang` (the fence's info string) names no known language.
    pub fn new(lang: &str) -> Option<Code> {
        let name = lang
            .split_whitespace()
            .next()?
            .trim_start_matches('.')
            .to_ascii_lowercase();
        Some(Code {
            lang: self::lang(token(&name))?,
            state: State::Normal,
            depth: 0,
        })
    }

    /// One line of the block, colored.
    pub fn line(&mut self, line: &str) -> String {
        let mut out = Out { spans: Vec::new() };
        match self.lang.mode {
            Mode::Diff => out.push(0, line.len(), diff_role(line)),
            Mode::Markup => self.markup(line, &mut out),
            _ => {
                let start = if self.state == State::Normal {
                    self.lead(line, &mut out)
                } else {
                    0
                };
                self.code(line, start, &mut out);
            }
        }
        out.spans
            .iter()
            .map(|&(a, b, role)| style(role).paint(&line[a..b]).to_string())
            .collect()
    }

    /// Line-leading constructs (keys, targets, instructions); returns where
    /// the regular lexer takes over.
    fn lead(&self, s: &str, out: &mut Out) -> usize {
        let b = s.as_bytes();
        let indent = b.iter().take_while(|c| c.is_ascii_whitespace()).count();
        let rest = &s[indent..];
        out.push(0, indent, Role::Plain);
        match self.lang.mode {
            Mode::Yaml => {
                let mut at = indent;
                while s[at..].starts_with("- ") {
                    out.push(at, at + 2, Role::Plain);
                    at += 2;
                }
                let key = s[at..]
                    .find(": ")
                    .or_else(|| s[at..].strip_suffix(':').map(|k| k.len()));
                match key {
                    Some(k)
                        if !matches!(b.get(at), Some(b'#' | b'"' | b'\'' | b'{' | b'['))
                            && !s[at..at + k].contains(" #") =>
                    {
                        out.push(at, at + k, Role::Type);
                        at + k
                    }
                    _ => at,
                }
            }
            Mode::Toml if rest.starts_with('[') => {
                let end = rest.find(']').map_or(s.len(), |e| indent + e + 1);
                out.push(indent, end, Role::Keyword);
                end
            }
            Mode::Toml => match rest.find('=') {
                Some(k) if !rest[..k].contains(['"', '\'', '#']) => {
                    let k = rest[..k].trim_end().len();
                    out.push(indent, indent + k, Role::Type);
                    indent + k
                }
                _ => indent,
            },
            Mode::Make if indent == 0 => match s.find(':') {
                Some(k)
                    if k > 0
                        && b.get(k + 1) != Some(&b'=')
                        && !s[..k].contains(['=', '$', '#']) =>
                {
                    out.push(0, k, Role::Function);
                    k
                }
                _ => 0,
            },
            Mode::Docker => {
                let n = rest.bytes().take_while(u8::is_ascii_alphabetic).count();
                if n > 0 && has(DOCKER_LEAD, &rest[..n], true) {
                    out.push(indent, indent + n, Role::Keyword);
                    indent + n
                } else {
                    indent
                }
            }
            _ => indent,
        }
    }

    fn code(&mut self, s: &str, mut i: usize, out: &mut Out) {
        let l = self.lang;
        let b = s.as_bytes();
        let n = b.len();
        let nocase = l.flags & NOCASE != 0;
        // Where the current string started (its prefix or opening quote).
        let mut start = i;
        // A string prefix (`f`, `rb`) waiting for its quote.
        let mut pre: Option<usize> = None;
        // Role forced on the next identifier (`fn name`, `class Name`).
        let mut next: Option<Role> = None;
        'scan: while i < n {
            match self.state {
                State::Block(end) => {
                    let j = match s[i..].find(end) {
                        Some(j) => {
                            self.state = State::Normal;
                            i + j + end.len()
                        }
                        None => n,
                    };
                    out.push(i, j, Role::Comment);
                    i = j;
                    continue;
                }
                State::Str(close, f) => {
                    let mut k = i;
                    let mut done = false;
                    while k < n {
                        if f & ESC != 0 && b[k] == b'\\' {
                            k += 2;
                        } else if b[k..].starts_with(close.as_bytes()) {
                            k += close.len();
                            done = true;
                            break;
                        } else {
                            k += 1;
                        }
                    }
                    let k = k.min(n);
                    let key = done && l.mode == Mode::Json && s[k..].trim_start().starts_with(':');
                    out.push(start, k, if key { Role::Type } else { Role::Str });
                    if done {
                        self.state = State::Normal;
                    }
                    i = k;
                    continue;
                }
                State::Normal | State::Tag => {}
            }
            let c = b[i];
            let prev = if i > 0 { b[i - 1] } else { b' ' };
            let rest = &s[i..];
            if c.is_ascii_whitespace() {
                out.push(i, i + 1, Role::Plain);
                i += 1;
                continue;
            }
            for &(open, close) in l.block {
                if rest.starts_with(open) {
                    out.push(i, i + open.len(), Role::Comment);
                    i += open.len();
                    self.state = State::Block(close);
                    continue 'scan;
                }
            }
            for m in l.line {
                if rest.starts_with(m) && !(*m == "#" && (ident_byte(prev, 0) || prev == b'$')) {
                    out.push(i, n, Role::Comment);
                    break 'scan;
                }
            }
            for &(open, close, f) in l.strs {
                if !rest.starts_with(open) {
                    continue;
                }
                if open == "'" && ident_byte(prev, 0) && pre.is_none() {
                    continue; // an apostrophe inside a word
                }
                if f & CHR != 0 {
                    let w = b.get(i + 1).map_or(1, |&c| char_len(c));
                    if b.get(i + 1) != Some(&b'\\') && b.get(i + 1 + w) != Some(&b'\'') {
                        // A lifetime or label.
                        let e =
                            i + 1 + b[i + 1..].iter().take_while(|&&c| ident_byte(c, 0)).count();
                        out.push(i, e, Role::Type);
                        i = e;
                        continue 'scan;
                    }
                }
                start = pre.take().unwrap_or(i);
                i += open.len();
                self.state = State::Str(close, f);
                continue 'scan;
            }
            if c.is_ascii_digit()
                || (c == b'.'
                    && b.get(i + 1).is_some_and(u8::is_ascii_digit)
                    && !ident_byte(prev, 0))
            {
                let mut k = i + 1;
                while k < n {
                    let d = b[k];
                    let exp = matches!(d, b'+' | b'-')
                        && matches!(b[k - 1], b'e' | b'E')
                        && !s[i..].starts_with("0x");
                    let dot = d == b'.' && b.get(k + 1).is_some_and(u8::is_ascii_digit);
                    if !(d.is_ascii_alphanumeric() || d == b'_' || dot || exp) {
                        break;
                    }
                    k += 1;
                }
                out.push(i, k, Role::Number);
                i = k;
                continue;
            }
            if ident_byte(c, l.flags) && c != b'-' {
                let e = i + b[i..]
                    .iter()
                    .take_while(|&&c| ident_byte(c, l.flags))
                    .count();
                let word = &s[i..e];
                // String prefixes: r"..", b'..', f"..", r#".."#.
                if l.flags & RUST != 0 && (word == "r" || word == "br") {
                    let h = b[e..].iter().take_while(|&&c| c == b'#').count();
                    if h < RAW.len() && b.get(e + h) == Some(&b'"') {
                        start = i;
                        i = e + h + 1;
                        self.state = State::Str(RAW[h], ML);
                        continue;
                    }
                }
                if l.flags & PREFIX != 0
                    && word.len() <= 2
                    && word.bytes().all(|c| b"rbfuRBFU".contains(&c))
                    && matches!(b.get(e), Some(b'"' | b'\''))
                {
                    pre = Some(i);
                    i = e;
                    continue;
                }
                let after = s[e..].trim_start();
                let mut end = e;
                let role = if l.mode == Mode::Css {
                    if self.depth > 0 && after.starts_with(':') {
                        Role::Type
                    } else if after.starts_with('(') {
                        Role::Function
                    } else if self.depth == 0 {
                        Role::Keyword
                    } else {
                        Role::Plain
                    }
                } else if has(l.kw, word, nocase) {
                    next = if has("fn def func function fun sub", word, false) {
                        Some(Role::Function)
                    } else if has("class struct enum trait interface type union record protocol extension module", word, false) {
                        Some(Role::Type)
                    } else {
                        None
                    };
                    i = e;
                    out.push(i - word.len(), e, Role::Keyword);
                    continue;
                } else if has(l.consts, word, nocase) {
                    Role::Number
                } else if let Some(r) = next.take() {
                    r
                } else if after.starts_with('(') {
                    Role::Function
                } else if l.flags & RUST != 0
                    && s[e..].starts_with('!')
                    && matches!(b.get(e + 1), Some(b'(' | b'[' | b'{'))
                {
                    end = e + 1;
                    Role::Function
                } else if has(l.types, word, nocase) {
                    Role::Type
                } else if l.flags & CAPS != 0 && c.is_ascii_uppercase() {
                    if word.len() > 1 && !word.bytes().any(|c| c.is_ascii_lowercase()) {
                        Role::Number
                    } else {
                        Role::Type
                    }
                } else {
                    Role::Plain
                };
                next = None;
                out.push(i, end, role);
                i = end;
                continue;
            }
            // `@decorator`, `#include`, `$VAR`, CSS `#fff` / `.class` / `@media`.
            let word = |from: usize| {
                from + b[from..]
                    .iter()
                    .take_while(|&&c| ident_byte(c, l.flags))
                    .count()
            };
            let (end, role) = match c {
                b'@' if word(i + 1) > i + 1 => (
                    word(i + 1),
                    if l.mode == Mode::Css {
                        Role::Keyword
                    } else {
                        Role::Type
                    },
                ),
                b'#' if l.flags & PREPROC != 0 && word(i + 1) > i + 1 => {
                    (word(i + 1), Role::Keyword)
                }
                b'#' if l.mode == Mode::Css && word(i + 1) > i + 1 => {
                    let e = word(i + 1);
                    let hex = self.depth > 0 && b[i + 1..e].iter().all(u8::is_ascii_hexdigit);
                    (e, if hex { Role::Number } else { Role::Function })
                }
                b'.' | b':' if l.mode == Mode::Css && self.depth == 0 && word(i + 1) > i + 1 => (
                    word(i + 1),
                    if c == b'.' {
                        Role::Function
                    } else {
                        Role::Type
                    },
                ),
                b'!' if l.mode == Mode::Css && word(i + 1) > i + 1 => (word(i + 1), Role::Keyword),
                b'$' if l.flags & VARS != 0 => {
                    let e = match b.get(i + 1) {
                        Some(b'{') => s[i..].find('}').map_or(n, |k| i + k + 1),
                        Some(b'(') => s[i..].find(')').map_or(n, |k| i + k + 1),
                        Some(c) if ident_byte(*c, 0) => word(i + 1),
                        Some(b'@' | b'?' | b'#' | b'*' | b'!' | b'$' | b'<' | b'^') => i + 2,
                        _ => i + 1,
                    };
                    (e, Role::Type)
                }
                _ => {
                    if l.mode == Mode::Css {
                        match c {
                            b'{' => self.depth += 1,
                            b'}' => self.depth = self.depth.saturating_sub(1),
                            _ => {}
                        }
                    }
                    (i + char_len(c).min(n - i), Role::Plain)
                }
            };
            out.push(i, end, role);
            i = end;
        }
        if let State::Str(_, f) = self.state {
            if f & ML == 0 {
                self.state = State::Normal; // unterminated single-line string
            }
        }
    }

    /// HTML / XML: tags, attributes, strings, comments and entities.
    fn markup(&mut self, s: &str, out: &mut Out) {
        let b = s.as_bytes();
        let n = b.len();
        let name = |from: usize| {
            from + b[from..]
                .iter()
                .take_while(|&&c| c.is_ascii_alphanumeric() || b"-_:.".contains(&c) || c >= 0x80)
                .count()
        };
        let mut i = 0;
        while i < n {
            let rest = &s[i..];
            let (end, role) = match self.state {
                State::Block(close) | State::Str(close, _) => {
                    let role = if matches!(self.state, State::Block(_)) {
                        Role::Comment
                    } else {
                        Role::Str
                    };
                    match rest.find(close) {
                        Some(k) => {
                            self.state = if role == Role::Str {
                                State::Tag
                            } else {
                                State::Normal
                            };
                            (i + k + close.len(), role)
                        }
                        None => (n, role),
                    }
                }
                State::Tag => match b[i] {
                    b'>' => {
                        self.state = State::Normal;
                        (i + 1, Role::Plain)
                    }
                    q @ (b'"' | b'\'') => {
                        self.state = State::Str(if q == b'"' { "\"" } else { "'" }, 0);
                        (i + 1, Role::Str)
                    }
                    c if c.is_ascii_alphabetic() || c >= 0x80 => (name(i), Role::Type),
                    c => (i + char_len(c).min(n - i), Role::Plain),
                },
                State::Normal => {
                    if rest.starts_with("<!--") {
                        self.state = State::Block("-->");
                        (i + 4, Role::Comment)
                    } else if b[i] == b'<'
                        && b.get(i + 1)
                            .is_some_and(|c| c.is_ascii_alphabetic() || b"/!?".contains(c))
                    {
                        let tag = i + 1 + usize::from(b"/!?".contains(&b[i + 1]));
                        out.push(i, tag, Role::Plain);
                        i = tag;
                        self.state = State::Tag;
                        (name(tag), Role::Keyword)
                    } else if b[i] == b'&'
                        && rest[1..]
                            .find(';')
                            .is_some_and(|k| k < 10 && !rest[1..k + 1].contains(' '))
                    {
                        (i + rest.find(';').unwrap_or(0) + 1, Role::Number)
                    } else {
                        let w = char_len(b[i]).min(rest.len());
                        let k = rest[w..].find(['<', '&']).map_or(n, |k| i + w + k);
                        (k, Role::Plain)
                    }
                }
            };
            out.push(i, end, role);
            i = end;
        }
    }
}

fn diff_role(line: &str) -> Role {
    if ["+++", "---", "diff ", "index "]
        .iter()
        .any(|p| line.starts_with(p))
    {
        Role::Keyword
    } else if line.starts_with('+') {
        Role::Str
    } else if line.starts_with('-') {
        Role::Removed
    } else if line.starts_with("@@") {
        Role::Function
    } else {
        Role::Plain
    }
}

/// UTF-8 sequence length from the leading byte.
fn char_len(b: u8) -> usize {
    match b {
        0xf0.. => 4,
        0xe0.. => 3,
        0xc0.. => 2,
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::render::strip_ansi;

    fn paint(role: Role, s: &str) -> String {
        style(role).paint(s).to_string()
    }

    /// Highlights `lines` in `lang`, checking the text survives unchanged.
    fn run(lang: &str, lines: &[&str]) -> Vec<String> {
        let mut code = Code::new(lang).unwrap_or_else(|| panic!("{lang}"));
        lines
            .iter()
            .map(|l| {
                let out = code.line(l);
                assert_eq!(strip_ansi(&out), *l, "{lang}: {out:?}");
                out
            })
            .collect()
    }

    fn has_span(out: &str, role: Role, text: &str) {
        assert!(
            out.contains(&paint(role, text)),
            "missing {text:?} in {out:?}"
        );
    }

    #[test]
    fn highlights_known_languages() {
        let mut rust = Code::new("rust").unwrap();
        let out = rust.line("fn main() { let x = \"hi\"; // note");
        assert_eq!(strip_ansi(&out), "fn main() { let x = \"hi\"; // note");
        let keyword = fg(MAGENTA).paint("fn").to_string();
        assert!(out.starts_with(&keyword), "{out:?}");
        assert!(
            out.contains(&fg(GREEN).paint("\"hi\"").to_string()),
            "{out:?}"
        );
        assert!(
            out.contains(&fg(GRAY).italic().paint("// note").to_string()),
            "{out:?}"
        );
        // Block comments carry over to the next line.
        let mut c = Code::new("c").unwrap();
        c.line("/* start");
        assert!(c
            .line("still comment")
            .contains(&fg(GRAY).italic().paint("still comment").to_string()));
        for lang in [
            "ts",
            "tsx",
            "jsx",
            "bash",
            "zsh",
            "sh",
            "python",
            "go",
            "json",
            "yaml",
            "toml",
            "sql",
            "java",
            "kotlin",
            "swift",
            "ruby",
            "php",
            "c++",
            "c#",
            "html",
            "xml",
            "css",
            "lua",
            "diff",
            "dockerfile",
            "makefile",
            "rust",
            ".rs",
            "python title=x.py",
        ] {
            assert!(Code::new(lang).is_some(), "{lang}");
        }
        assert!(Code::new("no-such-language").is_none());
        assert!(Code::new("").is_none());
    }

    #[test]
    fn rust_details() {
        let out = run(
            "rs",
            &[
                "impl<'a> Foo<'a> { fn new(x: u32) -> Self { println!(\"{x}\\n\"); let c = '\\'' ; let r = r#\"a \"q\" b\"#; MAX } }",
                "let s = \"multi",
                "line\"; 0x1F 1_000 2.5e-3 'λ' /* c */",
            ],
        );
        has_span(&out[0], Role::Keyword, "impl");
        has_span(&out[0], Role::Type, "'a");
        has_span(&out[0], Role::Type, "Foo");
        has_span(&out[0], Role::Function, "new");
        has_span(&out[0], Role::Type, "u32");
        has_span(&out[0], Role::Function, "println!");
        has_span(&out[0], Role::Str, "\"{x}\\n\"");
        has_span(&out[0], Role::Str, "'\\''");
        has_span(&out[0], Role::Str, "r#\"a \"q\" b\"#");
        has_span(&out[0], Role::Number, "MAX");
        has_span(&out[2], Role::Str, "line\"");
        has_span(&out[2], Role::Number, "0x1F");
        has_span(&out[2], Role::Number, "1_000");
        has_span(&out[2], Role::Number, "2.5e-3");
        has_span(&out[2], Role::Str, "'λ'");
        has_span(&out[2], Role::Comment, "/* c */");
    }

    #[test]
    fn python_and_js_strings() {
        let out = run(
            "python",
            &[
                "def f(a):",
                "    s = f\"x{a}\" + '''doc",
                "still doc''' # done",
                "    return None",
            ],
        );
        has_span(&out[0], Role::Function, "f");
        has_span(&out[1], Role::Str, "f\"x{a}\"");
        has_span(&out[2], Role::Str, "still doc'''");
        has_span(&out[2], Role::Comment, "# done");
        has_span(&out[3], Role::Number, "None");
        let out = run(
            "ts",
            &[
                "const t = `a ${b}",
                "c`; let n: number = 1; // x",
                "class A extends B {}",
            ],
        );
        has_span(&out[0], Role::Keyword, "const");
        has_span(&out[1], Role::Str, "c`");
        has_span(&out[1], Role::Type, "number");
        has_span(&out[2], Role::Type, "A");
    }

    #[test]
    fn unterminated_and_escapes() {
        // Unterminated single-line strings end at the line end.
        let out = run("js", &["let s = \"open \\\" quote", "let t = 1"]);
        has_span(&out[0], Role::Str, "\"open \\\" quote");
        has_span(&out[1], Role::Keyword, "let");
        // A trailing backslash does not run past the end.
        run("c", &["char *s = \"x\\", "int y;"]);
        // Unterminated block comment stays a comment.
        let out = run("go", &["x := 1 /* never", "closed"]);
        has_span(&out[1], Role::Comment, "closed");
        // Apostrophes inside words are not strings.
        let out = run("yaml", &["name: don't stop # c"]);
        has_span(&out[0], Role::Comment, "# c");
    }

    #[test]
    fn unicode_and_long_lines() {
        run(
            "rs",
            &[
                "let ä = \"日本\"; // ünïcode → ok",
                "日本語 'é' “quoted” 🎉",
            ],
        );
        run("html", &["<p title=\"é\">日本 &amp; 🎉</p>"]);
        let long = "let x = \"a\"; ".repeat(20_000);
        let start = std::time::Instant::now();
        run("rust", &[&long]);
        assert!(start.elapsed().as_secs() < 5);
        run("python", &[&"x".repeat(100_000)]);
    }

    #[test]
    fn data_languages() {
        let out = run("json", &["{\"key\": \"value\", \"n\": 1.5, \"ok\": true}"]);
        has_span(&out[0], Role::Type, "\"key\"");
        has_span(&out[0], Role::Str, "\"value\"");
        has_span(&out[0], Role::Number, "1.5");
        has_span(&out[0], Role::Number, "true");
        let out = run(
            "yaml",
            &["- name: build", "  run: echo \"hi\" # c", "  on: true"],
        );
        has_span(&out[0], Role::Type, "name");
        has_span(&out[1], Role::Type, "run");
        has_span(&out[1], Role::Str, "\"hi\"");
        has_span(&out[2], Role::Number, "true");
        let out = run("toml", &["[package]", "name = \"nyq\" # c", "lto = true"]);
        has_span(&out[0], Role::Keyword, "[package]");
        has_span(&out[1], Role::Type, "name");
        has_span(&out[1], Role::Str, "\"nyq\"");
        has_span(&out[2], Role::Number, "true");
    }

    #[test]
    fn special_modes() {
        let out = run(
            "diff",
            &["--- a/x", "+added", "-removed", "@@ -1 +1 @@", " same"],
        );
        has_span(&out[1], Role::Str, "+added");
        has_span(&out[2], Role::Removed, "-removed");
        has_span(&out[3], Role::Function, "@@ -1 +1 @@");
        let out = run(
            "html",
            &[
                "<div class=\"a\" id='b'>text &amp; <!-- c",
                "c --></div>",
                "<a",
                "  href=\"x\">",
            ],
        );
        has_span(&out[0], Role::Keyword, "div");
        has_span(&out[0], Role::Type, "class");
        has_span(&out[0], Role::Str, "\"a\"");
        has_span(&out[0], Role::Str, "'b'");
        has_span(&out[0], Role::Number, "&amp;");
        has_span(&out[1], Role::Comment, "c -->");
        has_span(&out[3], Role::Type, "href");
        let out = run(
            "css",
            &[
                "@media screen {",
                "  .a:hover { color: #fff; margin: 0 4px !important; }",
                "}",
            ],
        );
        has_span(&out[0], Role::Keyword, "@media");
        has_span(&out[1], Role::Type, "color");
        has_span(&out[1], Role::Number, "#fff");
        has_span(&out[1], Role::Number, "4px");
        let out = run("sh", &["if [ -n \"$X\" ]; then echo ${HOME} $1 # c; fi"]);
        has_span(&out[0], Role::Keyword, "if");
        has_span(&out[0], Role::Type, "${HOME}");
        has_span(&out[0], Role::Comment, "# c; fi");
        let out = run(
            "dockerfile",
            &["FROM rust:1 AS build", "RUN cargo build # x"],
        );
        has_span(&out[0], Role::Keyword, "FROM");
        has_span(&out[1], Role::Keyword, "RUN");
        let out = run(
            "makefile",
            &["build: src", "\tcargo build $(FLAGS)", "X := 1"],
        );
        has_span(&out[0], Role::Function, "build");
        has_span(&out[1], Role::Type, "$(FLAGS)");
        let out = run("sql", &["SELECT id FROM t WHERE name = 'x' -- c"]);
        has_span(&out[0], Role::Keyword, "SELECT");
        has_span(&out[0], Role::Str, "'x'");
        let out = run("lua", &["--[[ a", "b ]] local x = nil"]);
        has_span(&out[1], Role::Comment, "b ]]");
        has_span(&out[1], Role::Keyword, "local");
        let out = run(
            "c",
            &["#include <stdio.h>", "int main(void) { return NULL; }"],
        );
        has_span(&out[0], Role::Keyword, "#include");
        has_span(&out[1], Role::Type, "int");
        has_span(&out[1], Role::Function, "main");
    }

    /// `cargo test highlight::tests::show -- --ignored --nocapture` to eyeball.
    #[test]
    #[ignore]
    fn show() {
        let samples: &[(&str, &str)] = &[
            ("rust", "/// Doc\n#[derive(Debug)]\npub struct Foo<'a> { x: &'a str }\nfn main() {\n    let v: Vec<u32> = vec![1, 2];\n    println!(\"{} {}\", v.len(), r#\"raw\"#); // done\n}"),
            ("python", "@dataclass\nclass A(Base):\n    def go(self, n=3):\n        \"\"\"Doc\n        string\"\"\"\n        return f\"{n}\" if n else None  # c"),
            ("ts", "import { x } from './y';\nexport async function f(a: string): Promise<void> {\n  const t = `hi ${a}`;\n  return null;\n}"),
            ("bash", "#!/bin/sh\nfor f in *.rs; do\n  echo \"$f\" ${HOME} # loop\ndone"),
            ("json", "{\n  \"name\": \"nyq\",\n  \"n\": [1, 2.5, true, null]\n}"),
            ("yaml", "on:\n  push:\n    branches: [main]\njobs:\n  - name: \"x\" # c"),
            ("html", "<!DOCTYPE html>\n<div class=\"a\">Hi &amp; bye<!-- c --></div>"),
            ("css", ".a > b:hover {\n  color: #ff0000;\n  margin: 0 auto !important;\n}"),
            ("diff", "--- a/x\n+++ b/x\n@@ -1,2 +1,2 @@\n-old\n+new\n same"),
            ("sql", "SELECT id, count(*) FROM users WHERE name = 'bob' -- c"),
        ];
        for (lang, text) in samples {
            println!("--- {lang}");
            let mut code = Code::new(lang).unwrap();
            for line in text.lines() {
                println!("{}", code.line(line));
            }
        }
    }
}
