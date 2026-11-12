//! Source outlines: signatures, type declarations and doc comments with
//! function bodies elided as `{ … }`. A tree-sitter grammar per language would
//! be more precise, but the grammar crates alone would roughly double the
//! binary, so this is a hand-rolled scanner: a masking pass removes comments
//! and string contents, then a brace- or indentation-aware pass keeps
//! declaration lines. It is a heuristic, not a parser — pathological input may
//! keep or elide a line too much, but it never panics and always keeps line
//! numbers truthful.
//!
//! A future `tree-sitter` cargo feature could swap [`outline`] for grammar-based
//! extraction; the flag exists as a stub (see Cargo.toml) and is documented here.

use std::path::Path;

/// Language families with a dedicated scanner. Anything else gets the
/// generic indentation/brace fallback.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Lang {
    Rust,
    Go,
    Python,
    TsJs,
    JavaKotlin,
    Cpp,
    Ruby,
    Haskell,
    Generic,
}

pub(crate) fn language_for(path: &Path) -> Lang {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    match ext.as_deref() {
        Some("rs") => Lang::Rust,
        Some("go") => Lang::Go,
        Some("py" | "pyi" | "pyw") => Lang::Python,
        Some("ts" | "tsx" | "js" | "jsx" | "mts" | "cts" | "mjs" | "cjs") => Lang::TsJs,
        Some("java" | "kt" | "kts" | "scala") => Lang::JavaKotlin,
        Some("c" | "h" | "cc" | "cpp" | "cxx" | "hh" | "hpp" | "hxx" | "cs") => Lang::Cpp,
        Some("rb" | "rake" | "gemspec") => Lang::Ruby,
        Some("hs" | "lhs") => Lang::Haskell,
        _ => Lang::Generic,
    }
}

/// Whether the extension names a source file (vs a doc/spec). Drives the
/// default working-set mode: source files open as outlines.
pub(crate) fn is_source(path: &Path) -> bool {
    language_for(path) != Lang::Generic
}

/// `(1-based line number, text)` for every kept line. `text` is the original
/// source line, trimmed on the right; elided bodies appear as `{ … }` or `…`.
pub(crate) fn outline(path: &Path, text: &str) -> Vec<(u64, String)> {
    let lang = language_for(path);
    let lines: Vec<Masked> = mask(lang, text);
    match lang {
        Lang::Python => outline_python(&lines),
        Lang::Ruby => outline_ruby(&lines),
        Lang::Haskell => outline_haskell(&lines),
        Lang::Go => outline_brace(&lines, true),
        Lang::Generic if !text.contains('{') => outline_indent(&lines),
        _ => outline_brace(&lines, false),
    }
}

/// The outline in `read_file`'s format: `     N\tline`.
pub(crate) fn render(path: &Path, text: &str) -> String {
    let mut out = String::new();
    for (n, line) in outline(path, text) {
        out.push_str(&format!("{n:>6}\t{line}\n"));
    }
    out
}

// ---------------------------------------------------------------------------
// Masking: replace comment and string contents with spaces (same length), so
// structural scans only see real syntax.

#[derive(Clone)]
struct Masked {
    /// Same byte length as `raw`, with comment/string bytes blanked out, so
    /// byte positions are interchangeable between the two.
    code: String,
    /// The original line (no `\n`).
    raw: String,
    /// The line begins inside a multi-line string literal.
    in_str: bool,
    /// The line ends inside a multi-line string literal.
    str_at_end: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum Quote {
    /// `'…'` is a string (JS/TS, Python, Ruby).
    Str,
    /// `'x'` is a character only; `'a` in Rust is a lifetime (Rust, Go,
    /// Java, Kotlin, C/C++, Haskell).
    Char,
}

struct Lex {
    line_comment: &'static str,
    block_open: &'static str,
    block_close: &'static str,
    /// Haskell `{- … -}` comments nest.
    nested: bool,
    quote: Quote,
    /// Backtick strings (Go raw strings, JS template literals).
    backtick: bool,
    /// Rust `r"…"` / `r#"…"#` raw strings.
    rust_raw: bool,
    /// Python `'''`/`"""` triple strings.
    triple: bool,
    /// Ruby `=begin`/`=end` block comments (column 0 only).
    ruby_blocks: bool,
    /// `--` only starts a comment when not followed by a symbol char.
    sym_comment: bool,
}

const C_LEX: Lex = Lex {
    line_comment: "//",
    block_open: "/*",
    block_close: "*/",
    nested: false,
    quote: Quote::Char,
    backtick: true,
    rust_raw: false,
    triple: false,
    ruby_blocks: false,
    sym_comment: false,
};
const PY_LEX: Lex = Lex {
    line_comment: "#",
    block_open: "",
    block_close: "",
    nested: false,
    quote: Quote::Str,
    backtick: false,
    rust_raw: false,
    triple: true,
    ruby_blocks: false,
    sym_comment: false,
};
const RB_LEX: Lex = Lex {
    line_comment: "#",
    block_open: "",
    block_close: "",
    nested: false,
    quote: Quote::Str,
    backtick: true,
    rust_raw: false,
    triple: false,
    ruby_blocks: true,
    sym_comment: false,
};
const HS_LEX: Lex = Lex {
    line_comment: "--",
    block_open: "{-",
    block_close: "-}",
    nested: true,
    quote: Quote::Char,
    backtick: false,
    rust_raw: false,
    triple: false,
    ruby_blocks: false,
    sym_comment: true,
};

fn lex_for(lang: Lang) -> Lex {
    match lang {
        Lang::Rust => Lex {
            rust_raw: true,
            ..C_LEX
        },
        Lang::Go => C_LEX,
        Lang::TsJs => Lex {
            quote: Quote::Str,
            ..C_LEX
        },
        Lang::JavaKotlin | Lang::Cpp => Lex {
            backtick: false,
            ..C_LEX
        },
        Lang::Python => PY_LEX,
        Lang::Ruby => RB_LEX,
        Lang::Haskell => HS_LEX,
        Lang::Generic => Lex {
            quote: Quote::Str,
            ..C_LEX
        },
    }
}

enum St {
    Normal,
    Block(i32),
    /// `raw` is the `r#"…"#` hash count for Rust raw strings (`None` for
    /// ordinary escape-aware strings). `triple` for Python `'''`/`"""`.
    Str {
        delim: u8,
        raw: Option<usize>,
        triple: bool,
    },
}

/// The `(opener length, delim, is_triple)` for a Python string at `s[0]`,
/// allowing `r`/`b`/`u`/`f` prefixes.
fn py_string_at(s: &[u8]) -> Option<(usize, u8, bool)> {
    let mut i = 0;
    while i < s.len() && matches!(s[i], b'r' | b'R' | b'b' | b'B' | b'u' | b'U' | b'f' | b'F') {
        i += 1;
    }
    if i + 2 < s.len() && (s[i..].starts_with(b"\"\"\"") || s[i..].starts_with(b"'''")) {
        return Some((i + 3, s[i], true));
    }
    if i < s.len() && (s[i] == b'"' || s[i] == b'\'') {
        return Some((i + 1, s[i], false));
    }
    None
}

/// `(opener length, hash count)` for a Rust `r"…"`/`r#"…"#` at `s[0]`.
fn rust_raw_at(s: &[u8]) -> Option<(usize, usize)> {
    if s.first() != Some(&b'r') {
        return None;
    }
    let mut i = 1;
    while i < s.len() && s[i] == b'#' {
        i += 1;
    }
    if i < s.len() && s[i] == b'"' {
        Some((i + 1, i - 1))
    } else {
        None
    }
}

/// Char literal `'x'`, `'\n'`, `'\u{…}'` at `s[0]`; `None` for lifetimes or
/// unterminated quotes.
fn char_lit_len(s: &[u8]) -> Option<usize> {
    if s.first() != Some(&b'\'') {
        return None;
    }
    let mut i = 1;
    if i < s.len() && s[i] == b'\\' {
        i += 1;
        if i < s.len() && (s[i] == b'u' || s[i] == b'x') {
            i += 1;
            if i < s.len() && s[i] == b'{' {
                while i < s.len() && s[i] != b'}' {
                    i += 1;
                }
                i += (i < s.len()) as usize;
            } else {
                i += 2.min(s.len() - i);
            }
        } else {
            i += (i < s.len()) as usize;
        }
    } else if i < s.len() && s[i] != b'\'' {
        i += 1;
    }
    (i < s.len() && s[i] == b'\'').then_some(i + 1)
}

fn mask(lang: Lang, text: &str) -> Vec<Masked> {
    let lex = lex_for(lang);
    let mut st = St::Normal;
    let mut ruby_comment = false;
    let mut out = Vec::new();
    for raw_line in text.lines() {
        let raw = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        let started_in_str = matches!(st, St::Str { .. });
        let b = raw.as_bytes();
        if lex.ruby_blocks {
            let inside = ruby_comment || raw.starts_with("=begin");
            if inside {
                ruby_comment = !raw.trim_start().starts_with("=end");
                out.push(Masked {
                    code: " ".repeat(raw.len()),
                    raw: raw.to_string(),
                    in_str: false,
                    str_at_end: false,
                });
                continue;
            }
        }
        let mut code = String::with_capacity(raw.len());
        let mut i = 0;
        let one = |s: &str| s.chars().next().map_or(0, |c| c.len_utf8());
        while i < b.len() {
            let rest = &b[i..];
            let (adv, keep) = match st {
                St::Normal => {
                    if !lex.line_comment.is_empty()
                        && rest.starts_with(lex.line_comment.as_bytes())
                        && (!lex.sym_comment
                            || rest[lex.line_comment.len()..]
                                .first()
                                .is_none_or(|c| !is_hs_symbol(*c)))
                    {
                        (b.len() - i, false)
                    } else if !lex.block_open.is_empty()
                        && rest.starts_with(lex.block_open.as_bytes())
                    {
                        st = St::Block(1);
                        (lex.block_open.len(), false)
                    } else if lex.rust_raw
                        && rest[0] == b'r'
                        && rust_raw_at(rest).is_some()
                    {
                        let (len, hashes) = rust_raw_at(rest).unwrap();
                        st = St::Str {
                            delim: b'"',
                            raw: Some(hashes),
                            triple: false,
                        };
                        (len, false)
                    } else if lex.triple && py_string_at(rest).is_some() {
                        let (len, d, triple) = py_string_at(rest).unwrap();
                        st = St::Str {
                            delim: d,
                            raw: None,
                            triple,
                        };
                        (len, false)
                    } else if rest[0] == b'"' || (lex.backtick && rest[0] == b'`') {
                        st = St::Str {
                            delim: rest[0],
                            raw: None,
                            triple: false,
                        };
                        (1, false)
                    } else if rest[0] == b'\'' {
                        match lex.quote {
                            Quote::Str => {
                                st = St::Str {
                                    delim: b'\'',
                                    raw: None,
                                    triple: false,
                                };
                                (1, false)
                            }
                            Quote::Char => match char_lit_len(rest) {
                                Some(len) => (len, false),
                                None => (one(raw), true),
                            },
                        }
                    } else {
                        (one(raw), true)
                    }
                }
                St::Block(n) => {
                    if lex.nested
                        && !lex.block_open.is_empty()
                        && rest.starts_with(lex.block_open.as_bytes())
                    {
                        st = St::Block(n + 1);
                        (lex.block_open.len(), false)
                    } else if !lex.block_close.is_empty()
                        && rest.starts_with(lex.block_close.as_bytes())
                    {
                        st = if n <= 1 { St::Normal } else { St::Block(n - 1) };
                        (lex.block_close.len(), false)
                    } else {
                        (one(raw), false)
                    }
                }
                St::Str {
                    delim,
                    raw: raw_hashes,
                    triple,
                } => {
                    if let Some(hashes) = raw_hashes {
                        // Rust raw string: ends at '"' + the same # count.
                        if rest[0] == b'"'
                            && rest[1..]
                                .iter()
                                .take_while(|c| **c == b'#')
                                .count()
                                == hashes
                        {
                            st = St::Normal;
                            (1 + hashes, false)
                        } else {
                            (one(raw), false)
                        }
                    } else if triple {
                        let close = [delim, delim, delim];
                        if rest.starts_with(&close) {
                            st = St::Normal;
                            (3, false)
                        } else if rest[0] == b'\\' {
                            (1 + one(&raw[i + 1..]).min(1), false)
                        } else {
                            (one(raw), false)
                        }
                    } else if rest[0] == b'\\' {
                        // Escape: skip the backslash plus the next char.
                        (1 + one(&raw[i + 1..]), false)
                    } else if rest[0] == delim {
                        st = St::Normal;
                        (1, false)
                    } else {
                        (one(raw), false)
                    }
                }
            };
            if keep {
                code.push_str(&raw[i..i + adv]);
            } else {
                code.push_str(&" ".repeat(adv));
            }
            i += adv;
        }
        out.push(Masked {
            code,
            raw: raw.to_string(),
            in_str: started_in_str,
            str_at_end: matches!(st, St::Str { .. }),
        });
    }
    out
}

/// Haskell `--` starts a comment unless followed by a symbol (so `-->`,
/// `x--y` operators and `a - -1` are not comment starts).
fn is_hs_symbol(c: u8) -> bool {
    matches!(
        c,
        b'!' | b'#'
            | b'$'
            | b'%'
            | b'&'
            | b'*'
            | b'+'
            | b'.'
            | b'/'
            | b'<'
            | b'='
            | b'>'
            | b'?'
            | b'@'
            | b'\\'
            | b'^'
            | b'|'
            | b'-'
            | b'~'
            | b':'
    )
}

// ---------------------------------------------------------------------------
// Brace-family scanner (Rust, Go, TS/JS, Java/Kotlin, C/C++, generic-with-braces)

const FN_WORDS: &[&str] = &[
    "fn",
    "func",
    "function",
    "def",
    "fun",
    "sub",
    "method",
    "operator",
    "constructor",
    "procedure",
];
const CONTAINER_WORDS: &[&str] = &[
    "struct",
    "enum",
    "union",
    "trait",
    "impl",
    "mod",
    "namespace",
    "class",
    "interface",
    "object",
    "record",
    "extern",
    "type",
    "typedef",
    "companion",
    "annotation",
    "contract",
    "library",
    "package",
    "protocol",
    "extension",
    "declare",
    "template",
    "macro_rules",
];

fn word_in(hay: &str, needle: &str) -> bool {
    hay.split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .any(|w| w == needle)
}

fn any_word(hay: &str, words: &[&str]) -> bool {
    words.iter().any(|w| word_in(hay, w))
}

fn brace_delta(code: &str) -> i64 {
    code.bytes()
        .map(|b| match b {
            b'{' => 1,
            b'}' => -1,
            _ => 0,
        })
        .sum()
}

fn emit_all(out: &mut Vec<(u64, String)>, pending: &[(usize, String)]) {
    for (n, line) in pending {
        out.push((*n as u64 + 1, line.trim_end().to_string()));
    }
}

/// Emit `pending` as a function/body: earlier lines verbatim, the last cut at
/// `open` (the `{` byte offset) and closed with `{ … }`.
fn emit_body(out: &mut Vec<(u64, String)>, pending: &[(usize, String)], open: usize) {
    for (n, line) in &pending[..pending.len().saturating_sub(1)] {
        out.push((*n as u64 + 1, line.trim_end().to_string()));
    }
    if let Some((n, last)) = pending.last() {
        let head = last[..open.min(last.len())].trim_end();
        out.push((*n as u64 + 1, format!("{head} {{ … }}")));
    }
}

/// Whether the `{` at `from` is balanced away on the same line (an inline
/// group like `use a::{b}` or `static X = { 1 }`) rather than a block opener.
fn inline_group(code: &str, from: usize) -> bool {
    let bytes = code.as_bytes();
    let mut depth = 1i64;
    for &b in bytes.iter().skip(from + 1) {
        match b {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return true;
                }
            }
            b';' => return false,
            _ => {}
        }
    }
    false
}

fn outline_brace(lines: &[Masked], eol_leaf: bool) -> Vec<(u64, String)> {
    let mut out: Vec<(u64, String)> = Vec::new();
    let mut pending: Vec<(usize, String)> = Vec::new();
    let mut pending_code = String::new();
    let mut depth: i64 = 0;
    let mut skip: Option<i64> = None;
    let (mut paren, mut bracket, mut sbrace) = (0i64, 0i64, 0i64);
    for (i, ml) in lines.iter().enumerate() {
        let code = ml.code.as_str();
        let raw = ml.raw.trim_end();
        if let Some(d) = skip {
            depth += brace_delta(code);
            if depth <= d {
                skip = None;
            }
            continue;
        }
        let tc = code.trim();
        let tr = raw.trim();
        if tc.is_empty() {
            // A blank line, or one that is entirely comment/string text.
            if !tr.is_empty() {
                if pending.is_empty() {
                    // A doc comment or attribute text at declaration level.
                    out.push((i as u64 + 1, raw.to_string()));
                } else {
                    pending.push((i, raw.to_string()));
                }
            }
            continue;
        }
        if tc.starts_with('}') && pending.is_empty() {
            // Closes a container (`}` / `};` / `} // note`).
            out.push((i as u64 + 1, tr.to_string()));
            depth += brace_delta(code);
            continue;
        }
        // Attributes, annotations, decorators, preprocessor lines.
        if pending.is_empty() && (tc.starts_with('#') || tc.starts_with('@')) {
            out.push((i as u64 + 1, raw.to_string()));
            depth += brace_delta(code);
            continue;
        }
        pending.push((i, raw.to_string()));
        pending_code.push_str(tc);
        pending_code.push(' ');
        let bytes = code.as_bytes();
        let mut j = 0;
        let mut opened: Option<usize> = None;
        let mut leaf = false;
        while j < bytes.len() {
            match bytes[j] {
                b'(' => paren += 1,
                b')' => paren = (paren - 1).max(0),
                b'[' => bracket += 1,
                b']' => bracket = (bracket - 1).max(0),
                b'{' if paren == 0 && bracket == 0 && sbrace == 0 => {
                    if inline_group(code, j) {
                        sbrace += 1;
                    } else {
                        opened = Some(j);
                        break;
                    }
                }
                b'{' => sbrace += 1,
                b'}' if sbrace > 0 => sbrace -= 1,
                b'}' => {}
                b';' | b',' if paren == 0 && bracket == 0 && sbrace == 0 => {
                    leaf = true;
                    break;
                }
                _ => {}
            }
            j += 1;
        }
        if let Some(pos) = opened {
            let head = format!("{pending_code}{}", &code[..pos]);
            let container =
                !any_word(&head, FN_WORDS) && any_word(&head, CONTAINER_WORDS);
            if container {
                emit_all(&mut out, &pending);
            } else {
                emit_body(&mut out, &pending, pos);
                skip = Some(depth);
            }
            pending.clear();
            pending_code.clear();
            (paren, bracket, sbrace) = (0, 0, 0);
        } else if leaf
            || (eol_leaf && paren == 0 && bracket == 0 && sbrace == 0 && !tc.ends_with(','))
        {
            emit_all(&mut out, &pending);
            pending.clear();
            pending_code.clear();
            (paren, bracket, sbrace) = (0, 0, 0);
        }
        depth += brace_delta(code);
        if let Some(d) = skip {
            if depth <= d {
                skip = None;
            }
        }
    }
    emit_all(&mut out, &pending);
    out
}

// ---------------------------------------------------------------------------
// Python (indentation)

fn indent_of(raw: &str) -> usize {
    raw.chars()
        .take_while(|c| c.is_whitespace())
        .map(|c| if c == '\t' { 8 } else { 1 })
        .sum()
}

fn leading_ws(raw: &str) -> String {
    raw.chars().take_while(|c| c.is_whitespace()).collect()
}

fn paren_delta(code: &str) -> i64 {
    code.bytes()
        .map(|b| match b {
            b'(' | b'[' => 1,
            b')' | b']' => -1,
            _ => 0,
        })
        .sum()
}

#[derive(Clone, Copy, PartialEq)]
enum PyKind {
    Class,
    Fn,
    /// `if`/`for`/`try`/`with`/`match`/… — a block whose contents are not
    /// declarations.
    Other,
}

fn py_string_line(raw: &str) -> bool {
    py_string_at(raw.trim_start().as_bytes()).is_some()
}

fn outline_python(lines: &[Masked]) -> Vec<(u64, String)> {
    let mut out = Vec::new();
    // (indent, kind, docstring still possible, `…` already emitted)
    let mut frames: Vec<(usize, PyKind, bool, bool)> = Vec::new();
    // A def/class/control head whose `:` arrives on a later line.
    let mut pending_head: Option<(usize, PyKind)> = None;
    let mut paren: i64 = 0;
    // The previous emitted line ended inside a string (docstring or a
    // decl-level literal): keep emitting its continuation.
    let mut keep_str = false;
    for (i, ml) in lines.iter().enumerate() {
        let code = ml.code.as_str();
        let raw = ml.raw.trim_end();
        let tr = raw.trim();
        let tc = code.trim();
        if keep_str && ml.in_str {
            out.push((i as u64 + 1, raw.to_string()));
            keep_str = ml.str_at_end;
            continue;
        }
        keep_str = false;
        if tr.is_empty() {
            continue;
        }
        if ml.in_str {
            // Inside a string that is not a kept docstring.
            continue;
        }
        if paren > 0 {
            // Continuation of a kept statement.
            out.push((i as u64 + 1, raw.to_string()));
            paren += paren_delta(code);
            if paren == 0 {
                if tc.ends_with(':') {
                    if let Some((ind, k)) = pending_head.take() {
                        frames.push((ind, k, k == PyKind::Fn, false));
                    }
                } else {
                    pending_head = None;
                }
            }
            keep_str = ml.str_at_end;
            continue;
        }
        let indent = indent_of(&ml.raw);
        while frames.last().is_some_and(|(ind, _, _, _)| *ind >= indent) {
            frames.pop();
        }
        let ctx = frames.last().map(|f| f.1);
        match ctx {
            Some(PyKind::Fn) => {
                let (_, _, doc, marked) = frames.last_mut().unwrap();
                if *doc && py_string_line(raw) {
                    out.push((i as u64 + 1, raw.to_string()));
                    *doc = false;
                    keep_str = ml.str_at_end;
                } else {
                    *doc = false;
                    if !*marked {
                        out.push((i as u64 + 1, format!("{}…", leading_ws(&ml.raw))));
                        *marked = true;
                    }
                }
                continue;
            }
            Some(PyKind::Other) => {
                let (_, _, _, marked) = frames.last_mut().unwrap();
                if !*marked {
                    out.push((i as u64 + 1, format!("{}…", leading_ws(&ml.raw))));
                    *marked = true;
                }
                continue;
            }
            _ => {}
        }
        // Declaration level (module or class body).
        if tc.is_empty() {
            // A comment or string line — shown verbatim.
            out.push((i as u64 + 1, tr.to_string()));
            keep_str = ml.str_at_end;
            continue;
        }
        let head_kind = if tc.starts_with("class ") || tc == "class" {
            Some(PyKind::Class)
        } else if tc.starts_with("def ") || tc.starts_with("async def ") || tc == "def" {
            Some(PyKind::Fn)
        } else if tc.ends_with(':')
            || matches!(
                tc.split_whitespace().next(),
                Some("if" | "elif" | "else" | "for" | "while" | "try" | "except" | "finally"
                    | "with" | "match" | "case")
            ) && paren_delta(code) > 0
        {
            Some(PyKind::Other)
        } else {
            None
        };
        out.push((i as u64 + 1, raw.to_string()));
        keep_str = ml.str_at_end;
        paren += paren_delta(code);
        match (head_kind, paren == 0) {
            (Some(k), true) => frames.push((indent, k, k == PyKind::Fn, false)),
            (Some(k), false) => pending_head = Some((indent, k)),
            _ => {}
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Ruby (`end`-delimited blocks)

fn outline_ruby(lines: &[Masked]) -> Vec<(u64, String)> {
    let mut out = Vec::new();
    let mut depth: i64 = 0;
    let mut skip: Option<i64> = None;
    for (i, ml) in lines.iter().enumerate() {
        let code = ml.code.as_str();
        let raw = ml.raw.trim_end();
        if let Some(d) = skip {
            depth += ruby_delta(code);
            if depth <= d {
                skip = None;
            }
            continue;
        }
        let tc = code.trim();
        let tr = raw.trim();
        if tc.is_empty() {
            if !tr.is_empty() {
                out.push((i as u64 + 1, tr.to_string()));
            }
            continue;
        }
        let first = tc.split_whitespace().next().unwrap_or("");
        if first == "end" {
            out.push((i as u64 + 1, tr.to_string()));
            depth += ruby_delta(code);
            continue;
        }
        out.push((i as u64 + 1, raw.to_string()));
        // A header that opens a skippable body: def/if/case/begin/loops/do/{.
        if matches!(
            first,
            "def" | "if" | "unless" | "case" | "while" | "until" | "for" | "begin"
        ) || opens_ruby_block(code)
        {
            skip = Some(depth);
        }
        depth += ruby_delta(code);
        if skip == Some(depth) || skip.is_some_and(|d| depth <= d) {
            skip = None;
        }
    }
    out
}

/// `… do |x|`, `… do`, or a `{` opened on this line — a block a `def`-less
/// header still opens.
fn opens_ruby_block(code: &str) -> bool {
    let t = code.trim();
    if t == "do" || t.starts_with("do ") || t.starts_with("do|") || t.ends_with(" do") || t.contains(" do |") || t.contains(" do|") {
        return true;
    }
    brace_delta(code) > 0
}

/// Net block-open change on a masked Ruby line: `def`, `class`, `module`,
/// control heads, `do` blocks and `{` count as opens, `end`/`}` as closes.
fn ruby_delta(code: &str) -> i64 {
    let t = code.trim();
    let first = t.split_whitespace().next().unwrap_or("");
    let mut d = brace_delta(code);
    if matches!(
        first,
        "def" | "class" | "module" | "if" | "unless" | "case" | "while" | "until" | "for"
            | "begin"
    ) {
        d += 1;
    }
    if opens_ruby_block(code) {
        d += 1;
    }
    // `end` counts only at statement position (line start or after `;`),
    // never as `x.end` or inside `sender`.
    let mut rest = t;
    while let Some(pos) = rest.find("end") {
        let before_ok = pos == 0
            || rest[..pos].ends_with(';')
            || rest[..pos].ends_with("; ");
        let after_ok = rest[pos + 3..]
            .chars()
            .next()
            .is_none_or(|c| !(c.is_alphanumeric() || c == '_'));
        if before_ok && after_ok {
            d -= 1;
        }
        rest = &rest[(pos + 3).min(rest.len())..];
    }
    d
}

// ---------------------------------------------------------------------------
// Haskell (top-level declarations; `type`/`data`/`class`/`instance` keep
// their indented members, function bodies are cut at `=`).

const HS_TYPE_WORDS: &[&str] = &[
    "data", "newtype", "type", "family", "class", "instance", "deriving", "module", "import",
    "infix", "infixl", "infixr", "foreign", "pattern", "default",
];

fn hs_sig_line(raw: &str, in_type: bool) -> String {
    let t = raw.trim_end();
    if let Some(eq) = find_body_eq(t, in_type) {
        return format!("{} = …", t[..eq].trim_end());
    }
    t.to_string()
}

/// Byte offset of the `=` in `lhs = rhs` when the line binds a function body
/// (not a type/keyword line, not `==`/`=>`/`<=`/`>=`/`!=`/`|=`).
fn find_body_eq(t: &str, in_type: bool) -> Option<usize> {
    let first = t.split_whitespace().next()?;
    if in_type && matches!(first, "=" | "|" | "deriving" | "where") {
        return None;
    }
    let kw: String = first.chars().filter(|c| c.is_alphanumeric()).collect();
    if HS_TYPE_WORDS.contains(&kw.as_str()) {
        return None;
    }
    let bytes = t.as_bytes();
    for i in 0..bytes.len().saturating_sub(1) {
        if bytes[i] != b'=' {
            continue;
        }
        let before = i.checked_sub(1).map(|j| bytes[j]);
        let after = bytes.get(i + 1).copied();
        if matches!(before, Some(b'<' | b'>' | b'=' | b'!' | b'|' | b'/'))
            || matches!(after, Some(b'=' | b'>'))
        {
            continue;
        }
        return Some(i);
    }
    None
}

fn outline_haskell(lines: &[Masked]) -> Vec<(u64, String)> {
    let mut out = Vec::new();
    let mut in_type = false;
    for (i, ml) in lines.iter().enumerate() {
        let code = ml.code.as_str();
        let raw = ml.raw.trim_end();
        let tr = raw.trim();
        if tr.is_empty() {
            continue;
        }
        let indent = indent_of(&ml.raw);
        if indent == 0 {
            in_type = code
                .split_whitespace()
                .next()
                .is_some_and(|w| HS_TYPE_WORDS.contains(&w));
            if code.trim().is_empty() || tr.starts_with("--") || tr.starts_with("{-") {
                out.push((i as u64 + 1, tr.to_string()));
            } else {
                out.push((i as u64 + 1, hs_sig_line(raw, false)));
            }
        } else if in_type {
            out.push((i as u64 + 1, hs_sig_line(raw, true)));
        }
        // Inside a function body (or other non-type decl): elided.
    }
    out
}

// ---------------------------------------------------------------------------
// Generic fallback: no braces at all → keep column-0 declarations, mark
// indented blocks with `…`.

fn outline_indent(lines: &[Masked]) -> Vec<(u64, String)> {
    let mut out = Vec::new();
    let mut marked = false;
    for (i, ml) in lines.iter().enumerate() {
        let raw = ml.raw.trim_end();
        let tr = raw.trim();
        if tr.is_empty() {
            continue;
        }
        if indent_of(&ml.raw) == 0 {
            out.push((i as u64 + 1, raw.to_string()));
            marked = false;
        } else if !marked {
            out.push((i as u64 + 1, format!("{}…", leading_ws(&ml.raw))));
            marked = true;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ol(lang_file: &str, text: &str) -> String {
        outline(Path::new(lang_file), text)
            .into_iter()
            .map(|(n, l)| format!("{n}:{l}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn rust_signatures_types_and_docs() {
        let src = r#"//! Module docs.
use a::{b, c};

/// Doc comment.
#[derive(Debug)]
pub struct Foo {
    /// Field doc.
    pub x: i32,
    y: String,
}

pub enum E {
    A,
    B(i32),
}

impl Foo {
    /// Method doc.
    pub fn bar(&self, x: i32) -> i32 {
        let y = x + 1;
        y
    }
}

fn helper<T>(v: Vec<T>)
where
    T: Clone,
{
    todo!()
}

pub const N: usize = 3;
"#;
        let joined = ol("f.rs", src);
        assert!(joined.contains("//! Module docs."), "{joined}");
        assert!(joined.contains("use a::{b, c};"), "{joined}");
        assert!(joined.contains("/// Doc comment."), "{joined}");
        assert!(joined.contains("pub struct Foo {"), "{joined}");
        assert!(joined.contains("pub x: i32,"), "{joined}");
        assert!(joined.contains("pub enum E {"), "{joined}");
        assert!(joined.contains("    /// Method doc."), "{joined}");
        assert!(
            joined.contains("    pub fn bar(&self, x: i32) -> i32 { … }"),
            "{joined}"
        );
        assert!(!joined.contains("let y = x + 1"), "{joined}");
        assert!(joined.contains("fn helper<T>(v: Vec<T>)"), "{joined}");
        assert!(joined.contains("{ … }"), "{joined}");
        assert!(joined.contains("pub const N: usize = 3;"), "{joined}");
    }

    #[test]
    fn go_outlines() {
        let src = r#"// Package doc.
package foo

import (
	"fmt"
	"os"
)

type Server struct {
	addr string
	mux  *http.ServeMux
}

type Handler interface {
	Serve(ctx context.Context) error
}

func (s *Server) Start() error {
	fmt.Println(s.addr)
	return nil
}
"#;
        let joined = ol("f.go", src);
        assert!(joined.contains("package foo"), "{joined}");
        assert!(joined.contains("\"fmt\""), "{joined}");
        assert!(joined.contains("type Server struct {"), "{joined}");
        assert!(joined.contains("addr string"), "{joined}");
        assert!(joined.contains("type Handler interface {"), "{joined}");
        assert!(joined.contains("Serve(ctx context.Context) error"), "{joined}");
        assert!(
            joined.contains("func (s *Server) Start() error { … }"),
            "{joined}"
        );
        assert!(!joined.contains("os.Exit"), "{joined}");
    }

    #[test]
    fn python_outlines() {
        let src = "\"\"\"Module docstring.\"\"\"
import os

CONST = 42

class Foo:
    \"\"\"Class docstring.\"\"\"

    def bar(self, x: int) -> int:
        \"\"\"Method docstring.\"\"\"
        y = x + 1
        return y

    @property
    def name(self):
        return \"x\"

def free(a,
         b):
    return a + b

if __name__ == \"__main__\":
    free(1, 2)
";
        let joined = ol("f.py", src);
        assert!(joined.contains("\"\"\"Module docstring.\"\"\""), "{joined}");
        assert!(joined.contains("import os"), "{joined}");
        assert!(joined.contains("CONST = 42"), "{joined}");
        assert!(joined.contains("class Foo:"), "{joined}");
        assert!(joined.contains("\"\"\"Class docstring.\"\"\""), "{joined}");
        assert!(joined.contains("def bar(self, x: int) -> int:"), "{joined}");
        assert!(joined.contains("\"\"\"Method docstring.\"\"\""), "{joined}");
        assert!(!joined.contains("y = x + 1"), "{joined}");
        assert!(joined.contains("@property"), "{joined}");
        assert!(joined.contains("def free(a,"), "{joined}");
        assert!(joined.contains("         b):"), "{joined}");
        assert!(joined.contains("if __name__ == \"__main__\":"), "{joined}");
        assert!(!joined.contains("free(1, 2)"), "{joined}");
    }

    #[test]
    fn ts_and_java_outlines() {
        let ts = r#"// Comment.
export interface User {
  id: number;
  name(): string;
}

export class C implements User {
  id = 1;
  name(): string {
    return "n";
  }
}

export const f = (x: number) => {
  return x * 2;
};

type Alias = { a: number };
"#;
        let joined = ol("f.ts", ts);
        assert!(joined.contains("export interface User {"), "{joined}");
        assert!(joined.contains("name(): string;"), "{joined}");
        assert!(joined.contains("export class C implements User {"), "{joined}");
        assert!(joined.contains("name(): string { … }"), "{joined}");
        assert!(joined.contains("export const f = (x: number) => { … }"), "{joined}");
        assert!(joined.contains("type Alias = { a: number };"), "{joined}");

        let java = r#"package x.y;

/** Doc. */
public class Foo extends Bar {
    private int count;

    @Override
    public String name() {
        return "n";
    }
}
"#;
        let joined = ol("f.java", java);
        assert!(joined.contains("public class Foo extends Bar {"), "{joined}");
        assert!(joined.contains("private int count;"), "{joined}");
        assert!(joined.contains("@Override"), "{joined}");
        assert!(joined.contains("public String name() { … }"), "{joined}");
        assert!(!joined.contains("return \"n\";"), "{joined}");
    }

    #[test]
    fn c_cpp_outlines() {
        let src = r#"#include <stdio.h>
#define MAX 10

struct Point {
    int x;
    int y;
};

int add(int a, int b);

int add(int a, int b) {
    return a + b;
}
"#;
        let joined = ol("f.c", src);
        assert!(joined.contains("#include <stdio.h>"), "{joined}");
        assert!(joined.contains("struct Point {"), "{joined}");
        assert!(joined.contains("int x;"), "{joined}");
        assert!(joined.contains("};"), "{joined}");
        assert!(joined.contains("int add(int a, int b);"), "{joined}");
        assert!(joined.contains("int add(int a, int b) { … }"), "{joined}");
        assert!(!joined.contains("return a + b"), "{joined}");
    }

    #[test]
    fn ruby_outlines() {
        let src = r#"# Comment.
require "json"

class Foo < Bar
  attr_accessor :name
  CONST = 1

  def initialize(name)
    @name = name
  end

  def go
    if @name
      puts 1
    end
  end
end

def free
  1
end
"#;
        let joined = ol("f.rb", src);
        assert!(joined.contains("class Foo < Bar"), "{joined}");
        assert!(joined.contains("attr_accessor :name"), "{joined}");
        assert!(joined.contains("CONST = 1"), "{joined}");
        assert!(joined.contains("def initialize(name)"), "{joined}");
        assert!(joined.contains("def go"), "{joined}");
        assert!(!joined.contains("puts 1"), "{joined}");
        assert!(joined.contains("def free"), "{joined}");
        assert!(!joined.contains("\n  1\n"), "{joined}");
    }

    #[test]
    fn haskell_outlines() {
        let src = r#"-- Comment.
module Foo (bar, Baz(..)) where

import Data.List

data Baz = MkBaz Int | Other
    deriving (Show)

class C a where
    c :: a -> Int
    c x = x + 1

bar :: Int -> Int
bar x = x + 1

main :: IO ()
main = do
    print 1
"#;
        let joined = ol("f.hs", src);
        assert!(joined.contains("module Foo (bar, Baz(..)) where"), "{joined}");
        assert!(joined.contains("data Baz = MkBaz Int | Other"), "{joined}");
        assert!(joined.contains("deriving (Show)"), "{joined}");
        assert!(joined.contains("c :: a -> Int"), "{joined}");
        assert!(joined.contains("c x = …"), "{joined}");
        assert!(joined.contains("bar :: Int -> Int"), "{joined}");
        assert!(joined.contains("bar x = …"), "{joined}");
        assert!(joined.contains("main = …"), "{joined}");
        assert!(!joined.contains("print 1"), "{joined}");
    }

    #[test]
    fn generic_fallback_keeps_top_level() {
        let src = "key = 1\n[section]\n    indented = 2\n    deeper\nnext = 3\n";
        let joined = ol("f.toml", src);
        assert!(joined.contains("key = 1"), "{joined}");
        assert!(joined.contains("[section]"), "{joined}");
        assert!(joined.contains("…"), "{joined}");
        assert!(joined.contains("next = 3"), "{joined}");
        assert!(!joined.contains("indented = 2"), "{joined}");
    }

    #[test]
    fn line_numbers_are_original() {
        let src = "fn a() {\n  1\n}\n\n\nfn b() {\n  2\n}\n";
        let got = outline(Path::new("f.rs"), src);
        let nums: Vec<u64> = got.iter().map(|(n, _)| *n).collect();
        assert!(nums.contains(&1), "{nums:?}");
        assert!(nums.contains(&6), "{nums:?}");
        assert!(!nums.contains(&2), "{nums:?}");
    }

    #[test]
    fn rust_lifetimes_dont_confuse_the_masker() {
        let src = "fn f<'a>(x: &'a str) -> &'a str {\n    x\n}\nlet c = 'x';\n";
        let joined = ol("f.rs", src);
        assert!(
            joined.contains("fn f<'a>(x: &'a str) -> &'a str { … }"),
            "{joined}"
        );
        assert!(joined.contains("let c = 'x';"), "{joined}");
    }

    #[test]
    fn render_uses_numbered_lines() {
        let out = render(Path::new("f.rs"), "fn a() {\n  1\n}\n");
        assert!(out.contains("     1\tfn a() { … }"), "{out}");
    }
}
