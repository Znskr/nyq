#![allow(non_snake_case)]
use super::code_highlight_languages::{BlockComment, KeywordCase, Profile};
const RESET: &[u8] = b"\x1b[39m";
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Theme {
    Dark,
    Light,
}
struct Palette {
    keyword: &'static [u8],
    string: &'static [u8],
    number: &'static [u8],
    comment: &'static [u8],
}
fn palette(t: Theme) -> Palette {
    match t {
        Theme::Dark => Palette {
            keyword: b"\x1b[38;5;252m",
            string: b"\x1b[38;5;250m",
            number: b"\x1b[38;5;250m",
            comment: b"\x1b[38;5;245m",
        },
        Theme::Light => Palette {
            keyword: b"\x1b[38;5;238m",
            string: b"\x1b[38;5;241m",
            number: b"\x1b[38;5;241m",
            comment: b"\x1b[38;5;243m",
        },
    }
}
pub fn highlight(source: &[u8], profile: &Profile, theme: Theme) -> Vec<u8> {
    let p = palette(theme);
    let mut out = Vec::new();
    let mut i = 0;
    while i < source.len() {
        if source[i] == b'\n' {
            out.push(b'\n');
            i += 1;
            continue;
        }
        let found = blockCommentEnd(source, i, profile.block_comment)
            .map(|e| (e, p.comment))
            .or_else(|| lineCommentEnd(source, i, profile.line_comments).map(|e| (e, p.comment)))
            .or_else(|| {
                if profile.quotes.contains(&source[i]) {
                    Some((quotedEnd(source, i), p.string))
                } else {
                    None
                }
            })
            .or_else(|| {
                if isNumberStart(source, i) {
                    Some((numberEnd(source, i), p.number))
                } else {
                    None
                }
            });
        if let Some((end, style)) = found {
            append(&mut out, style, &source[i..end]);
            i = end;
            continue;
        }
        if isIdentifierStart(source[i]) {
            let end = identifierEnd(source, i);
            let token = &source[i..end];
            if inList(token, profile.keywords, profile.keyword_case) {
                append(&mut out, p.keyword, token)
            } else if inList(token, profile.literals, profile.keyword_case) {
                append(&mut out, p.number, token)
            } else {
                out.extend_from_slice(token)
            }
            i = end;
            continue;
        }
        out.push(source[i]);
        i += 1
    }
    out
}
fn append(o: &mut Vec<u8>, s: &[u8], t: &[u8]) {
    o.extend_from_slice(s);
    o.extend_from_slice(t);
    o.extend_from_slice(RESET)
}
fn starts(h: &[u8], n: &[u8]) -> bool {
    h.len() >= n.len() && &h[..n.len()] == n
}
fn blockCommentEnd(s: &[u8], i: usize, c: Option<BlockComment>) -> Option<usize> {
    let c = c?;
    if !starts(&s[i..], c.start) {
        return None;
    }
    let from = i + c.start.len();
    find(&s[from..], c.end)
        .map(|x| from + x + c.end.len())
        .or(Some(s.len()))
}
fn lineCommentEnd(s: &[u8], i: usize, p: &[&[u8]]) -> Option<usize> {
    if p.iter().any(|x| starts(&s[i..], x)) {
        Some(
            s[i..]
                .iter()
                .position(|&b| b == b'\n')
                .map_or(s.len(), |x| i + x),
        )
    } else {
        None
    }
}
fn quotedEnd(s: &[u8], start: usize) -> usize {
    let q = s[start];
    let mut i = start + 1;
    while i < s.len() {
        if s[i] == b'\n' {
            return i;
        }
        if s[i] == b'\\' && i + 1 < s.len() {
            i += 2;
            continue;
        }
        if s[i] == q {
            return i + 1;
        }
        i += 1
    }
    s.len()
}
fn isIdentifierStart(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_' || b == b'$'
}
fn isIdentifierContinue(b: u8) -> bool {
    isIdentifierStart(b) || b.is_ascii_digit()
}
fn isNumberStart(s: &[u8], i: usize) -> bool {
    s[i].is_ascii_digit() && (i == 0 || !isIdentifierContinue(s[i - 1]))
}
fn numberEnd(s: &[u8], mut i: usize) -> usize {
    while i < s.len() && (s[i].is_ascii_alphanumeric() || s[i] == b'.' || s[i] == b'_') {
        i += 1
    }
    i
}
fn identifierEnd(s: &[u8], mut i: usize) -> usize {
    i += 1;
    while i < s.len() && isIdentifierContinue(s[i]) {
        i += 1
    }
    i
}
fn inList(t: &[u8], o: &[&[u8]], c: KeywordCase) -> bool {
    o.iter().any(|x| match c {
        KeywordCase::Sensitive => t == *x,
        KeywordCase::AsciiInsensitive => t.eq_ignore_ascii_case(x),
    })
}
fn find(h: &[u8], n: &[u8]) -> Option<usize> {
    if n.is_empty() {
        return Some(0);
    }
    h.windows(n.len()).position(|w| w == n)
}
#[cfg(test)]
mod tests {
    use super::super::code_highlight_languages as l;
    use super::*;
    fn strip(s: &[u8]) -> Vec<u8> {
        let mut o = vec![];
        let mut i = 0;
        while i < s.len() {
            if i + 1 < s.len() && s[i] == 27 && s[i + 1] == b'[' {
                i += 2;
                while i < s.len() {
                    let b = s[i];
                    i += 1;
                    if (b'@'..=b'~').contains(&b) {
                        break;
                    }
                }
            } else {
                o.push(s[i]);
                i += 1
            }
        }
        o
    }
    fn count(h: &[u8], n: &[u8]) -> usize {
        h.windows(n.len()).filter(|w| *w == n).count()
    }
    #[test]
    fn supported() {
        let src = b"const value = \"const\"; // return\n";
        let s = highlight(src, l::resolve(b"zig").unwrap(), Theme::Dark);
        assert_eq!(strip(&s), src);
        assert_eq!(count(&s, b"\x1b[38;5;"), count(&s, RESET));
        assert_eq!(count(&s, b"\x1b[38;5;252mconst"), 1);
    }
    #[test]
    fn light() {
        let src = b"const value = \"ready\"; // comment\n";
        let s = highlight(src, l::resolve(b"zig").unwrap(), Theme::Light);
        assert_eq!(strip(&s), src);
        assert!(find(&s, b"\x1b[38;5;238mconst\x1b[39m").is_some());
        assert!(find(&s, b"\x1b[38;5;243m// comment\x1b[39m").is_some())
    }
    #[test]
    fn every_profile() {
        let cases: &[(&[u8], &[u8])] = &[
            (b"zig", b"pub fn main() void { return; }"),
            (b"ts", b"const ready = true;"),
            (b"json", b"{\"ready\": true}"),
            (b"sh", b"if true; then echo \"ready\"; fi"),
            (b"python", b"def ready(): return True"),
            (b"yaml", b"ready: true # comment"),
            (b"toml", b"ready = true # comment"),
            (b"sql", b"SELECT id FROM users"),
            (b"dockerfile", b"FROM alpine:3.20"),
            (b"rust", b"fn main() { let ready = true; }"),
            (b"go", b"package main\nfunc main() {}"),
            (b"c", b"int main(void) { return 0; }"),
            (b"cpp", b"class Ready { public: bool value = true; };"),
            (b"csharp", b"public class Ready { }"),
            (b"java", b"public class Ready { }"),
            (b"kotlin", b"fun ready(): Boolean = true"),
            (b"php", b"<?php function ready() { return true; }"),
            (b"ruby", b"def ready\n  true\nend"),
            (b"swift", b"func ready() -> Bool { true }"),
            (b"powershell", b"Function Ready { return $true }"),
            (b"lua", b"local ready = true"),
            (b"html", b"<main class=\"ready\"></main>"),
            (b"xml", b"<?xml version=\"1.0\"?>"),
            (b"css", b".ready { color: red; }"),
            (b"hcl", b"resource \"ready\" \"main\" {}"),
        ];
        for &(label, src) in cases {
            let s = highlight(src, l::resolve(label).unwrap(), Theme::Dark);
            assert_eq!(strip(&s), src);
            assert!(find(&s, b"\x1b[38;5;").is_some())
        }
    }
    #[test]
    fn configured_comments_and_case() {
        let css = highlight(b"/* comment */", l::resolve(b"css").unwrap(), Theme::Dark);
        let sql = highlight(
            b"SELECT id FROM users",
            l::resolve(b"sql").unwrap(),
            Theme::Dark,
        );
        let html = highlight(b"<!-- note -->", l::resolve(b"html").unwrap(), Theme::Dark);
        assert!(find(&css, b"\x1b[38;5;245m/* comment */\x1b[39m").is_some());
        assert!(find(&sql, b"\x1b[38;5;252mSELECT\x1b[39m").is_some());
        assert!(find(&html, b"\x1b[38;5;245m<!-- note -->\x1b[39m").is_some())
    }
}
