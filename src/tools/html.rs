//! A small HTML → Markdown converter for `web_fetch`.
//!
//! Not an HTML5 parser: a forgiving tokenizer feeds a shallow tree builder
//! (implied end tags for p/li/dt/dd/tr/td/option, stray end tags ignored,
//! misnested ones pop to the nearest match, nesting capped), and a renderer
//! walks the tree. The aim is readable Markdown for a model, in linear time.

use std::borrow::Cow;
use std::collections::HashMap;

/// Elements whose content is dropped.
const SKIP: &[&str] = &[
    "head", "script", "style", "noscript", "template", "nav", "footer", "iframe", "svg", "canvas", "form", "button", "title",
];
const VOID: &[&str] =
    &["area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source", "track", "wbr"];
/// Elements whose content is text up to the matching end tag.
const RAW: &[&str] = &["script", "style", "textarea", "title", "xmp", "iframe", "noembed", "noframes", "noscript"];
/// Start tags that close an open `<p>`.
const CLOSES_P: &[&str] = &[
    "address", "article", "aside", "blockquote", "center", "details", "dialog", "dir", "div", "dl", "dd", "dt", "fieldset",
    "figcaption", "figure", "footer", "form", "h1", "h2", "h3", "h4", "h5", "h6", "header", "hgroup", "hr", "li", "main",
    "menu", "nav", "ol", "p", "pre", "section", "summary", "table", "ul",
];
/// Rendered as paragraphs (blank line before and after).
const BLOCK: &[&str] = &[
    "p", "div", "section", "article", "main", "header", "aside", "address", "center", "details", "summary", "dialog",
    "fieldset", "figure", "figcaption", "hgroup", "legend", "search", "body", "html",
];
const HEAD_OK: &[&str] = &["meta", "link", "title", "style", "script", "base", "noscript", "template"];
const KEEP_ATTRS: &[&str] = &["href", "src", "alt", "class", "start", "colspan", "rowspan", "type", "checked", "hidden"];
const TABLE_PARTS: &[&str] = &["table", "thead", "tbody", "tfoot", "tr", "td", "th", "caption"];
const MAX_DEPTH: usize = 160;

/// Convert an HTML document to Markdown.
pub(crate) fn to_markdown(html: &str) -> String {
    let html = if html.contains('\r') { Cow::Owned(html.replace("\r\n", "\n").replace('\r', "\n")) } else { Cow::Borrowed(html) };
    let nodes = parse(&html);
    let mut w = W::new(&nodes, false);
    w.kids(0);
    w.out.trim().to_string()
}

// ---------------------------------------------------------------- entities

const NAMED: &[(&str, char)] = &[
    ("amp", '&'), ("lt", '<'), ("gt", '>'), ("quot", '"'), ("apos", '\''), ("nbsp", ' '), ("copy", '©'),
    ("reg", '®'), ("trade", '™'), ("hellip", '…'), ("mdash", '—'), ("ndash", '–'), ("lsquo", '‘'), ("rsquo", '’'),
    ("ldquo", '“'), ("rdquo", '”'), ("sbquo", '‚'), ("bdquo", '„'), ("bull", '•'), ("middot", '·'), ("times", '×'),
    ("divide", '÷'), ("deg", '°'), ("plusmn", '±'), ("euro", '€'), ("pound", '£'), ("yen", '¥'), ("cent", '¢'),
    ("sect", '§'), ("para", '¶'), ("laquo", '«'), ("raquo", '»'), ("lsaquo", '‹'), ("rsaquo", '›'), ("larr", '←'),
    ("rarr", '→'), ("uarr", '↑'), ("darr", '↓'), ("harr", '↔'), ("rArr", '⇒'), ("lArr", '⇐'), ("hArr", '⇔'),
    ("check", '✓'), ("cross", '✗'), ("le", '≤'), ("ge", '≥'), ("ne", '≠'), ("minus", '−'), ("infin", '∞'),
    ("prime", '′'), ("dagger", '†'), ("micro", 'µ'), ("frac12", '½'), ("frac14", '¼'), ("frac34", '¾'),
    ("sup2", '²'), ("ensp", ' '), ("emsp", ' '), ("thinsp", ' '), ("shy", '\u{ad}'), ("zwj", '\u{200d}'),
    ("zwnj", '\u{200c}'), ("ZeroWidthSpace", '\u{200b}'), ("iexcl", '¡'), ("iquest", '¿'),
];

/// Decode numeric character references and common named entities; unknown
/// names pass through unchanged.
pub(crate) fn decode(s: &str) -> Cow<'_, str> {
    if !s.contains('&') {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let semi = rest.bytes().take(34).position(|b| b == b';');
        match semi.and_then(|e| entity(&rest[1..e]).map(|c| (c, e))) {
            Some((c, e)) => {
                out.push(c);
                rest = &rest[e + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    Cow::Owned(out)
}

fn entity(name: &str) -> Option<char> {
    let num = |digits: &str, radix| u32::from_str_radix(digits, radix).ok().map(|n| char::from_u32(n).filter(|&c| c != '\0').unwrap_or('\u{fffd}'));
    match name.strip_prefix('#') {
        Some(n) if n.starts_with(['x', 'X']) && n.len() > 1 => num(&n[1..], 16),
        Some(n) if !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) => num(n, 10),
        Some(_) => None,
        None => NAMED.iter().find(|(k, _)| *k == name).map(|&(_, c)| c),
    }
}

// ---------------------------------------------------------------- parsing

enum Node {
    Text(String),
    Elem { tag: String, attrs: Vec<(String, String)>, kids: Vec<usize> },
}

struct Tree {
    nodes: Vec<Node>,
    stack: Vec<usize>,
    /// How many elements of each tag are on the stack, so stray end tags
    /// are dismissed without scanning it.
    open: HashMap<String, usize>,
}

impl Tree {
    fn tag(&self, id: usize) -> &str {
        match &self.nodes[id] {
            Node::Elem { tag, .. } => tag,
            Node::Text(_) => "",
        }
    }

    fn top(&self) -> &str {
        self.tag(*self.stack.last().unwrap())
    }

    fn add(&mut self, node: Node) -> usize {
        let id = self.nodes.len();
        self.nodes.push(node);
        let parent = *self.stack.last().unwrap();
        if let Node::Elem { kids, .. } = &mut self.nodes[parent] {
            kids.push(id);
        }
        id
    }

    fn text(&mut self, s: &str) {
        if s.is_empty() {
            return;
        }
        if self.top() == "head" && !s.trim_ascii().is_empty() {
            self.pop_to(self.stack.len() - 1);
        }
        let parent = *self.stack.last().unwrap();
        if let Node::Elem { kids, .. } = &self.nodes[parent] {
            if let Some(&last) = kids.last() {
                if let Node::Text(t) = &mut self.nodes[last] {
                    t.push_str(s);
                    return;
                }
            }
        }
        self.add(Node::Text(s.to_string()));
    }

    fn pop_to(&mut self, len: usize) {
        while self.stack.len() > len {
            let id = self.stack.pop().unwrap();
            if let Node::Elem { tag, .. } = &self.nodes[id] {
                if let Some(n) = self.open.get_mut(tag.as_str()) {
                    *n -= 1;
                }
            }
        }
    }

    /// Pop up to and including the nearest open element named in `names`,
    /// unless an element in `stop` comes first.
    fn close(&mut self, names: &[&str], stop: &[&str]) -> bool {
        if names.iter().all(|n| self.open.get(*n).is_none_or(|&c| c == 0)) {
            return false;
        }
        for i in (1..self.stack.len()).rev() {
            let tag = self.tag(self.stack[i]);
            if names.contains(&tag) {
                self.pop_to(i);
                return true;
            }
            if stop.contains(&tag) {
                return false;
            }
        }
        false
    }

    fn start(&mut self, tag: String, attrs: Vec<(String, String)>, raw: Option<&str>) {
        match tag.as_str() {
            "html" => return,
            "body" => {
                self.close(&["head"], &[]);
                return;
            }
            _ => {}
        }
        if self.top() == "head" && !HEAD_OK.contains(&tag.as_str()) {
            self.pop_to(self.stack.len() - 1);
        }
        let t = tag.as_str();
        if CLOSES_P.contains(&t) {
            self.close(&["p"], &["table", "td", "th", "caption", "button"]);
        }
        match t {
            "li" => _ = self.close(&["li"], &["ul", "ol", "menu", "table", "td", "th"]),
            "dt" | "dd" => _ = self.close(&["dt", "dd"], &["dl", "table", "td", "th"]),
            "tr" => _ = self.close(&["tr"], &["table"]),
            "td" | "th" => _ = self.close(&["td", "th"], &["tr", "table"]),
            "thead" | "tbody" | "tfoot" => _ = self.close(&["thead", "tbody", "tfoot"], &["table"]),
            "option" => _ = self.close(&["option"], &["select"]),
            "a" => _ = self.close(&["a"], &["td", "th", "table"]),
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" if heading_level(self.top()).is_some() => self.pop_to(self.stack.len() - 1),
            _ => {}
        }
        let push = !VOID.contains(&t) && raw.is_none() && self.stack.len() < MAX_DEPTH;
        if push {
            match self.open.get_mut(t) {
                Some(n) => *n += 1,
                None => _ = self.open.insert(tag.clone(), 1),
            }
        }
        let id = self.add(Node::Elem { tag, attrs, kids: Vec::new() });
        if let Some(text) = raw {
            self.stack.push(id);
            self.text(text);
            self.stack.pop();
        } else if push {
            self.stack.push(id);
        }
    }

    fn end(&mut self, tag: &str) {
        match tag {
            "html" | "body" => {}
            "br" => self.start("br".into(), Vec::new(), None),
            "p" => _ = self.close(&["p"], &["table", "td", "th", "caption", "button"]),
            "li" => _ = self.close(&["li"], &["ul", "ol", "menu", "table"]),
            "table" => _ = self.close(&["table"], &[]),
            _ if TABLE_PARTS.contains(&tag) => _ = self.close(&[tag], &["table"]),
            _ => _ = self.close(&[tag], &["td", "th", "table", "caption"]),
        }
    }
}

fn find_from(hay: &str, from: usize, needle: &str) -> Option<usize> {
    hay.get(from..)?.find(needle).map(|i| i + from)
}

fn parse(html: &str) -> Vec<Node> {
    let mut tree = Tree { nodes: vec![Node::Elem { tag: String::new(), attrs: Vec::new(), kids: Vec::new() }], stack: vec![0], open: HashMap::new() };
    let lower = html.to_ascii_lowercase();
    let b = html.as_bytes();
    let n = b.len();
    let (mut i, mut text_start) = (0, 0);
    // Index just past the next `>` at or after `from`.
    let past_gt = |from: usize| find_from(html, from, ">").map_or(n, |e| e + 1);
    while i < n {
        if b[i] != b'<' {
            i = find_from(html, i, "<").unwrap_or(n);
            continue;
        }
        let next = b.get(i + 1).copied().unwrap_or(0);
        if !(next.is_ascii_alphabetic() || matches!(next, b'!' | b'?' | b'/')) {
            i += 1; // a literal `<`
            continue;
        }
        tree.text(&decode(&html[text_start..i]));
        if lower[i..].starts_with("<!--") {
            i = find_from(html, i + 2, "-->").map_or(n, |e| e + 3);
        } else if lower[i..].starts_with("<![cdata[") {
            let end = find_from(html, i + 9, "]]>").unwrap_or(n);
            tree.text(&html[i + 9..end]);
            i = (end + 3).min(n);
        } else if next == b'!' || next == b'?' {
            i = past_gt(i);
        } else if next == b'/' {
            let start = i + 2;
            let len = b[start..].iter().position(|c| !c.is_ascii_alphanumeric()).unwrap_or(n - start);
            if len > 0 {
                tree.end(&lower[start..start + len]);
            }
            i = past_gt(start);
        } else {
            i = start_tag(html, &lower, i, &mut tree);
        }
        text_start = i;
    }
    tree.text(&decode(&html[text_start.min(n)..]));
    tree.nodes
}

/// Parse the start tag at `i` (and the content of a raw-text element);
/// returns the index after it.
fn start_tag(html: &str, lower: &str, i: usize, tree: &mut Tree) -> usize {
    let b = html.as_bytes();
    let n = b.len();
    let is_delim = |c: u8| c.is_ascii_whitespace() || c == b'>' || c == b'/';
    let mut j = i + 1;
    while j < n && !is_delim(b[j]) {
        j += 1;
    }
    let tag = lower[i + 1..j].to_string();
    let mut attrs = Vec::new();
    loop {
        while j < n && (b[j].is_ascii_whitespace() || b[j] == b'/') {
            j += 1;
        }
        if j >= n {
            return n; // unterminated tag: dropped
        }
        if b[j] == b'>' {
            j += 1;
            break;
        }
        let name_start = j;
        j += 1;
        while j < n && !is_delim(b[j]) && b[j] != b'=' {
            j += 1;
        }
        let name = &lower[name_start..j];
        while j < n && b[j].is_ascii_whitespace() {
            j += 1;
        }
        let mut value = "";
        if j < n && b[j] == b'=' {
            j += 1;
            while j < n && b[j].is_ascii_whitespace() {
                j += 1;
            }
            if j < n && (b[j] == b'"' || b[j] == b'\'') {
                let q = if b[j] == b'"' { "\"" } else { "'" };
                let end = find_from(html, j + 1, q).unwrap_or(n);
                value = &html[j + 1..end];
                j = (end + 1).min(n);
            } else {
                let start = j;
                while j < n && !b[j].is_ascii_whitespace() && b[j] != b'>' {
                    j += 1;
                }
                value = &html[start..j];
            }
        }
        if KEEP_ATTRS.contains(&name) {
            attrs.push((name.to_string(), decode(value).into_owned()));
        }
    }
    if RAW.contains(&tag.as_str()) {
        let close = format!("</{tag}");
        let end = find_from(lower, j, &close).unwrap_or(n);
        let body = &html[j..end];
        let body = if matches!(tag.as_str(), "textarea" | "title") { decode(body) } else { Cow::Borrowed(body) };
        tree.start(tag, attrs, Some(&body));
        return find_from(html, end, ">").map_or(n, |e| e + 1);
    }
    tree.start(tag, attrs, None);
    j
}

// ---------------------------------------------------------------- rendering

fn heading_level(tag: &str) -> Option<usize> {
    match tag.as_bytes() {
        [b'h', d @ b'1'..=b'6'] => Some((d - b'0') as usize),
        _ => None,
    }
}

fn attr<'a>(attrs: &'a [(String, String)], name: &str) -> Option<&'a str> {
    attrs.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
}

/// A code language named by a class like `language-rust` or `lang-rust`.
fn class_lang(attrs: &[(String, String)]) -> Option<String> {
    let class = attr(attrs, "class")?;
    class.split_ascii_whitespace().find_map(|c| c.strip_prefix("language-").or_else(|| c.strip_prefix("lang-"))).map(str::to_string)
}

/// A language hinted by a wrapper: GitHub's `highlight-source-rust`, Sphinx's `highlight-python3`.
fn hint_lang(attrs: &[(String, String)]) -> Option<String> {
    attr(attrs, "class")?.split_ascii_whitespace().find_map(|c| {
        let lang = c.strip_prefix("highlight-source-").or_else(|| c.strip_prefix("highlight-"))?;
        (!lang.is_empty() && lang != "default" && !lang.contains('-')).then(|| lang.to_string())
    })
}

/// One level of line prefix: a list item's indent, a blockquote's `> `.
/// `first` (a list marker) replaces `rest` on the next line written.
struct Level {
    rest: &'static str,
    first: Option<String>,
}

struct W<'a> {
    t: &'a [Node],
    out: String,
    levels: Vec<Level>,
    /// Newlines owed before the next content (2 = blank line).
    nl: u8,
    /// Prefix depth to use for an owed blank line.
    blank_depth: usize,
    space: bool,
    /// Inside a container that has not written anything yet.
    fresh: bool,
    /// Separator to write before the next content on the same line.
    sep: Option<&'static str>,
    /// Single-line mode (link text, table cells, headings).
    inline: bool,
    lists: usize,
    lang: Option<String>,
}

const INDENT: &[&str] = &["", " ", "  ", "   ", "    ", "     ", "      "];

impl<'a> W<'a> {
    fn new(t: &'a [Node], inline: bool) -> Self {
        W {
            t,
            out: String::new(),
            levels: Vec::new(),
            nl: 0,
            blank_depth: usize::MAX,
            space: false,
            fresh: false,
            sep: None,
            inline,
            lists: 0,
            lang: None,
        }
    }

    fn kids(&mut self, id: usize) {
        if let Node::Elem { kids, .. } = &self.t[id] {
            for &k in kids {
                self.node(k);
            }
        }
    }

    fn prefix(&mut self, trim: bool, depth: usize) {
        let start = self.out.len();
        for l in &mut self.levels[..depth] {
            match l.first.take() {
                Some(m) if !trim => self.out.push_str(&m),
                _ => self.out.push_str(l.rest),
            }
        }
        if trim {
            let keep = self.out[start..].trim_end().len();
            self.out.truncate(start + keep);
        }
    }

    /// Settle owed newlines and spaces before writing content.
    fn flush(&mut self) {
        if self.inline {
            if self.space {
                self.out.push(' ');
            }
        } else if self.out.is_empty() {
            self.prefix(false, self.levels.len());
        } else if self.nl > 0 {
            if self.nl > 1 {
                self.out.push('\n');
                self.prefix(true, self.blank_depth.min(self.levels.len()));
            }
            self.out.push('\n');
            self.prefix(false, self.levels.len());
        } else if let Some(sep) = self.sep {
            self.out.push_str(sep);
        } else if self.space {
            self.out.push(' ');
        }
        self.nl = 0;
        self.blank_depth = usize::MAX;
        self.space = false;
        self.sep = None;
        self.fresh = false;
    }

    fn raw(&mut self, s: &str) {
        self.flush();
        self.out.push_str(s);
    }

    fn text(&mut self, s: &str) {
        if s.starts_with(|c: char| c.is_ascii_whitespace()) {
            self.space = true;
        }
        for (k, word) in s.split_ascii_whitespace().enumerate() {
            if k > 0 {
                self.space = true;
            }
            self.raw(word);
        }
        if s.ends_with(|c: char| c.is_ascii_whitespace()) {
            self.space = true;
        }
    }

    fn block(&mut self, n: u8) {
        if self.inline {
            self.space = true;
            return;
        }
        if self.fresh || self.out.is_empty() {
            return;
        }
        self.nl = self.nl.max(n);
        self.blank_depth = self.blank_depth.min(self.levels.len());
        self.space = false;
        self.sep = None;
    }

    fn push_level(&mut self, rest: &'static str, first: Option<String>) {
        self.levels.push(Level { rest, first });
        self.fresh = true;
    }

    fn pop_level(&mut self) {
        self.levels.pop();
        self.fresh = false;
    }

    /// Render an element's content on one line.
    fn inline_of(&self, id: usize) -> String {
        let mut w = W::new(self.t, true);
        w.kids(id);
        if w.space {
            w.out.push(' ');
        }
        w.out
    }

    /// Write `inner` (from [`Self::inline_of`]) between `open` and `close`,
    /// keeping its edge spaces outside.
    fn wrap(&mut self, inner: &str, open: &str, close: &str) {
        let core = inner.trim_matches(' ');
        if inner.starts_with(' ') {
            self.space = true;
        }
        if !core.is_empty() {
            let wrapped = open == close && core.len() > 2 * open.len() && core.starts_with(open) && core.ends_with(open);
            if wrapped || open.is_empty() {
                self.raw(core);
            } else {
                self.flush();
                for s in [open, core, close] {
                    self.out.push_str(s);
                }
            }
            if inner.ends_with(' ') {
                self.space = true;
            }
        }
    }

    fn node(&mut self, id: usize) {
        let t = self.t;
        let (tag, attrs) = match &t[id] {
            Node::Text(s) => return self.text(s),
            Node::Elem { tag, attrs, .. } => (tag.as_str(), attrs.as_slice()),
        };
        if SKIP.contains(&tag) || attr(attrs, "hidden").is_some() {
            return;
        }
        match tag {
            "br" if self.inline => self.space = true,
            "br" if !self.fresh && !self.out.is_empty() => self.nl = if self.nl > 0 { 2 } else { 1 },
            "hr" if !self.inline => {
                self.block(2);
                self.raw("---");
                self.block(2);
            }
            "pre" => self.pre(id, attrs),
            "code" | "kbd" | "samp" | "tt" => {
                let mut s = String::new();
                self.text_of(id, &mut s);
                self.code(&s);
            }
            "b" | "strong" => self.wrap(&self.inline_of(id), "**", "**"),
            "i" | "em" => self.wrap(&self.inline_of(id), "*", "*"),
            "del" | "s" | "strike" => self.wrap(&self.inline_of(id), "~~", "~~"),
            "a" => self.link(id, attrs),
            "img" => self.img(attrs),
            "ul" | "ol" | "menu" => self.list(id, tag == "ol", attrs),
            "li" => self.item(id, "- ".into()),
            "table" => self.table(id),
            "blockquote" if !self.inline => {
                self.block(2);
                self.push_level("> ", None);
                self.kids(id);
                self.pop_level();
                self.block(2);
            }
            "dd" if !self.inline => {
                self.block(1);
                self.push_level("  ", Some(": ".into()));
                self.kids(id);
                self.pop_level();
                self.block(1);
            }
            "dt" | "tr" | "dl" => {
                self.block(1);
                self.kids(id);
                self.block(1);
            }
            "td" | "th" => {
                self.kids(id);
                self.space = true;
            }
            "input" if attr(attrs, "type").is_some_and(|t| t.eq_ignore_ascii_case("checkbox")) => {
                self.raw(if attr(attrs, "checked").is_some() { "[x]" } else { "[ ]" });
                self.space = true;
            }
            _ if heading_level(tag).is_some() => {
                let inner = self.inline_of(id);
                self.block(2);
                if self.inline {
                    self.wrap(&inner, "", "");
                } else if !inner.trim().is_empty() {
                    let hashes = &"######"[..heading_level(tag).unwrap()];
                    self.raw(hashes);
                    self.out.push(' ');
                    self.out.push_str(inner.trim());
                }
                self.block(2);
            }
            _ if BLOCK.contains(&tag) => {
                self.block(2);
                let saved = match hint_lang(attrs) {
                    Some(lang) => self.lang.replace(lang),
                    None => self.lang.clone(),
                };
                self.kids(id);
                self.lang = saved;
                self.block(2);
            }
            _ => self.kids(id),
        }
    }

    /// The text under `id`, as written (for code); `<br>` becomes a newline.
    fn text_of(&self, id: usize, out: &mut String) {
        match &self.t[id] {
            Node::Text(s) => out.push_str(s),
            Node::Elem { tag, kids, .. } => {
                if tag == "br" {
                    out.push('\n');
                } else if !SKIP.contains(&tag.as_str()) {
                    for &k in kids {
                        self.text_of(k, out);
                    }
                }
            }
        }
    }

    fn code(&mut self, s: &str) {
        let body = s.split_ascii_whitespace().collect::<Vec<_>>().join(" ");
        if s.starts_with(|c: char| c.is_ascii_whitespace()) {
            self.space = true;
        }
        if body.is_empty() {
            return;
        }
        let fence = "`".repeat(longest_run(&body, '`') + 1);
        let pad = if body.starts_with('`') || body.ends_with('`') { " " } else { "" };
        self.flush();
        for part in [&fence, pad, &body, pad, &fence] {
            self.out.push_str(part);
        }
        if s.ends_with(|c: char| c.is_ascii_whitespace()) {
            self.space = true;
        }
    }

    fn pre(&mut self, id: usize, attrs: &[(String, String)]) {
        let mut s = String::new();
        self.text_of(id, &mut s);
        if self.inline {
            return self.code(&s);
        }
        let body = s.strip_prefix('\n').unwrap_or(&s).trim_end();
        if body.trim_start().is_empty() {
            return;
        }
        let code_kid = match &self.t[id] {
            Node::Elem { kids, .. } => kids.iter().find_map(|&k| match &self.t[k] {
                Node::Elem { tag, attrs, .. } if tag == "code" => class_lang(attrs),
                _ => None,
            }),
            Node::Text(_) => None,
        };
        let lang = class_lang(attrs).or(code_kid).or_else(|| self.lang.clone()).unwrap_or_default();
        let fence = "`".repeat(longest_run(body, '`').max(2) + 1);
        self.block(2);
        self.raw(&fence);
        self.out.push_str(&lang);
        let depth = self.levels.len();
        for line in body.split('\n') {
            self.out.push('\n');
            self.prefix(line.is_empty(), depth);
            self.out.push_str(line);
        }
        self.out.push('\n');
        self.prefix(false, depth);
        self.out.push_str(&fence);
        self.block(2);
    }

    fn link(&mut self, id: usize, attrs: &[(String, String)]) {
        let inner = self.inline_of(id);
        let href = attr(attrs, "href").unwrap_or("").trim();
        if href.is_empty() || href.get(..11).is_some_and(|s| s.eq_ignore_ascii_case("javascript:")) {
            return self.wrap(&inner, "", "");
        }
        let close = format!("]({})", href.replace(' ', "%20"));
        self.wrap(&inner, "[", &close);
    }

    fn img(&mut self, attrs: &[(String, String)]) {
        let alt = attr(attrs, "alt").unwrap_or("").split_ascii_whitespace().collect::<Vec<_>>().join(" ");
        let src = attr(attrs, "src").unwrap_or("").trim();
        if src.is_empty() || src.starts_with("data:") {
            return self.text(&alt);
        }
        self.raw(&format!("![{alt}]({})", src.replace(' ', "%20")));
    }

    fn list(&mut self, id: usize, ordered: bool, attrs: &[(String, String)]) {
        if self.inline {
            return self.kids(id);
        }
        let gap = if self.lists > 0 { 1 } else { 2 };
        self.block(gap);
        self.lists += 1;
        let first: i64 = attr(attrs, "start").and_then(|s| s.trim().parse().ok()).unwrap_or(1);
        let mut n = first;
        if let Node::Elem { kids, .. } = &self.t[id] {
            for &k in kids {
                match &self.t[k] {
                    Node::Elem { tag, .. } if tag == "li" => {
                        if n != first {
                            self.nl = self.nl.min(1); // keep items tight
                        }
                        let marker = if ordered { format!("{n}. ") } else { "- ".into() };
                        n = n.saturating_add(1);
                        self.item(k, marker);
                    }
                    _ => self.node(k),
                }
            }
        }
        self.lists -= 1;
        self.block(gap);
    }

    fn item(&mut self, id: usize, marker: String) {
        if self.inline {
            self.space = true;
            return self.kids(id);
        }
        self.block(1);
        let indent = INDENT[marker.len().min(INDENT.len() - 1)];
        self.push_level(indent, Some(marker));
        self.kids(id);
        self.pop_level();
        self.block(1);
    }

    fn table(&mut self, id: usize) {
        let t = self.t;
        let kids = |id: usize| match &t[id] {
            Node::Elem { kids, .. } => kids.as_slice(),
            Node::Text(_) => &[],
        };
        let tag = |id: usize| match &t[id] {
            Node::Elem { tag, .. } => tag.as_str(),
            Node::Text(_) => "",
        };
        let (mut rows, mut caption, mut other) = (Vec::new(), None, Vec::new());
        for &k in kids(id) {
            match tag(k) {
                "tr" => rows.push(k),
                "thead" | "tbody" | "tfoot" => {
                    rows.extend(kids(k).iter().filter(|&&r| tag(r) == "tr"));
                }
                "caption" => caption = Some(k),
                "" | "colgroup" | "col" => {}
                _ => other.push(k),
            }
        }
        for k in other {
            self.node(k); // misplaced content: html5ever would hoist it before the table
        }
        let cells = |r: usize| kids(r).iter().copied().filter(|&c| matches!(tag(c), "td" | "th"));
        if self.inline {
            for c in rows.iter().flat_map(|&r| cells(r)) {
                self.node(c);
            }
            return;
        }
        let ncols = rows.first().map_or(0, |&r| cells(r).count());
        let spans = |c: usize| match &t[c] {
            Node::Elem { attrs, .. } => ["colspan", "rowspan"]
                .iter()
                .any(|a| attr(attrs, a).and_then(|v| v.trim().parse::<u32>().ok()).is_some_and(|v| v > 1)),
            Node::Text(_) => false,
        };
        let simple = ncols >= 2
            && rows.len() >= 2
            && rows.iter().all(|&r| cells(r).count() == ncols && !cells(r).any(spans))
            && !kids(id).iter().any(|&k| self.contains(k, "table"));
        self.block(2);
        if let Some(c) = caption {
            let s = self.inline_of(c);
            if !s.trim().is_empty() {
                self.raw(s.trim());
                self.block(1);
            }
        }
        for (i, &r) in rows.iter().enumerate() {
            self.block(1);
            if simple {
                let mut line = String::from("|");
                for c in cells(r) {
                    line.push(' ');
                    line.push_str(&self.inline_of(c).trim().replace('|', "\\|"));
                    line.push_str(" |");
                }
                self.raw(&line);
                if i == 0 {
                    self.block(1);
                    self.raw(&"| --- ".repeat(ncols));
                    self.out.push('|');
                }
            } else {
                for c in cells(r) {
                    let before = self.out.len();
                    self.kids(c);
                    if self.out.len() != before && self.nl == 0 {
                        self.sep = Some(" | ");
                    }
                }
            }
        }
        self.block(2);
    }

    fn contains(&self, id: usize, name: &str) -> bool {
        match &self.t[id] {
            Node::Elem { tag, kids, .. } => tag == name || kids.iter().any(|&k| self.contains(k, name)),
            Node::Text(_) => false,
        }
    }
}

fn longest_run(s: &str, c: char) -> usize {
    let (mut best, mut cur) = (0, 0);
    for ch in s.chars() {
        cur = if ch == c { cur + 1 } else { 0 };
        best = best.max(cur);
    }
    best
}

#[cfg(test)]
mod tests {
    use super::{decode, to_markdown as md};
    use std::time::{Duration, Instant};

    #[test]
    fn blocks_and_inline() {
        assert_eq!(md("<h1>Title</h1><p>One <b>two</b> <em>three</em></p><p>a<br>b</p><hr><h3> x </h3>"), "# Title\n\nOne **two** *three*\n\na\nb\n\n---\n\n### x");
        assert_eq!(md("<p>  lots \n\t of   space </p>"), "lots of space");
        assert_eq!(md("<p>x<b> y </b>z <s>old</s></p>"), "x **y** z ~~old~~");
        assert_eq!(md("<p>a<br><br>b</p>"), "a\n\nb");
        assert_eq!(md("<div>one</div><div>two</div>"), "one\n\ntwo");
        assert_eq!(md("<p>Use <code>a  `b`</code> or <kbd>Ctrl</kbd></p>"), "Use `` a `b` `` or `Ctrl`");
        assert_eq!(md("<b><b>x</b></b> <b></b>y"), "**x** y");
    }

    #[test]
    fn links_and_images() {
        assert_eq!(md("<a href=\"https://x.org/a b\">the <i>link</i></a>"), "[the *link*](https://x.org/a%20b)");
        assert_eq!(md("<a href=\"javascript:void(0)\">click</a> <a href=\"#top\"></a><a>bare</a>"), "click bare");
        assert_eq!(md("<a href=/x>see <img src=i.png alt=\"An image\"></a>"), "[see ![An image](i.png)](/x)");
        assert_eq!(md("<img src=\"data:image/png;base64,AAAA\" alt=logo> <img alt=none>"), "logo none");
        assert_eq!(md("<a href=\"?a=1&amp;b=2\">q</a>"), "[q](?a=1&b=2)");
    }

    #[test]
    fn code_blocks() {
        let html = "<pre><code class=\"language-rust\">\nfn main() {\n    let x = 1 &lt; 2;\n\n    println!(\"hi\");\n}\n</code></pre>";
        assert_eq!(md(html), "```rust\nfn main() {\n    let x = 1 < 2;\n\n    println!(\"hi\");\n}\n```");
        assert_eq!(md("<pre>a <b>b</b><br>  c</pre>"), "```\na b\n  c\n```");
        assert_eq!(md("<pre>```\nx\n```</pre>"), "````\n```\nx\n```\n````");
        assert_eq!(md("<div class=\"highlight highlight-source-shell\"><pre>ls</pre></div>"), "```shell\nls\n```");
        assert_eq!(md("<ul><li>Run:<pre>make\n  all</pre></li></ul>"), "- Run:\n\n  ```\n  make\n    all\n  ```");
        assert_eq!(md("<p>a</p><pre>  \n </pre><p>b</p>"), "a\n\nb");
    }

    #[test]
    fn lists() {
        let html = "<ul><li>one</li><li>two<ul><li>a</li><li>b<ol start=3><li>x</li><li>y</li></ol></li></ul></li><li>three</li></ul>";
        assert_eq!(md(html), "- one\n- two\n  - a\n  - b\n    3. x\n    4. y\n- three");
        // Unclosed <li> and <p> are closed implicitly.
        assert_eq!(md("<p>Intro<ol><li>first<li>second</ol><p>After"), "Intro\n\n1. first\n2. second\n\nAfter");
        assert_eq!(md("<ul><li><p>para one</p><p>para two</p></li><li>next</li></ul>"), "- para one\n\n  para two\n- next");
        assert_eq!(md("<ul><li><ul><li>deep</li></ul></li></ul>"), "- - deep");
        assert_eq!(md("<ul><li><input type=checkbox checked> done</li><li><input type=checkbox> todo</li></ul>"), "- [x] done\n- [ ] todo");
    }

    #[test]
    fn quotes_and_definitions() {
        assert_eq!(md("<p>x</p><blockquote><p>one</p><p>two</p><blockquote>deep</blockquote></blockquote><p>y</p>"), "x\n\n> one\n>\n> two\n>\n> > deep\n\ny");
        assert_eq!(md("<dl><dt>Term</dt><dd>Meaning</dd><dt>Other</dt><dd><p>a</p><p>b</p></dd></dl>"), "Term\n: Meaning\nOther\n: a\n\n  b");
    }

    #[test]
    fn tables() {
        let html = "<table><caption>Data</caption><thead><tr><th>Name</th><th>Value</th></tr></thead>\
                    <tbody><tr><td>a|b</td><td><b>1</b></td></tr><tr><td>c</td><td></td></tr></tbody></table>";
        assert_eq!(md(html), "Data\n| Name | Value |\n| --- | --- |\n| a\\|b | **1** |\n| c |  |");
        // Spans, ragged rows, single rows and nested tables fall back to rows of text.
        let complex = "<table><tr><th colspan=2>Head</th></tr><tr><td>a</td><td>b</td></tr><tr><td>c</td></tr></table>";
        assert_eq!(md(complex), "Head\na | b\nc");
        assert_eq!(md("<table><tr><td>x</td><td><table><tr><td>in</td></tr></table></td></tr></table>"), "x\n\nin");
        assert_eq!(md("<table><tr><td>left</td><td><p>p1</p><p>p2</p></td></tr></table>"), "left\n\np1\n\np2");
        assert_eq!(md("<div><a href=/t><table><tr><td>1</td><td>2</td></tr></table></a></div>"), "[1 2](/t)");
    }

    #[test]
    fn entities() {
        assert_eq!(decode("a &amp; b &lt;c&gt; &quot;q&quot; &#39;s&#x27; &#X41;"), "a & b <c> \"q\" 's' A");
        assert_eq!(decode("&copy;&nbsp;&mdash;&hellip;&rarr;&euro;&check;"), "© —…→€✓");
        assert_eq!(decode("&bogus; &amp &#; &#xZZ; &#0; AT&T &;"), "&bogus; &amp &#; &#xZZ; \u{fffd} AT&T &;");
        assert_eq!(decode("&#99999999999;"), "&#99999999999;");
        assert_eq!(md("<p title=\"&lt;\">&lt;b&gt; is not a tag &amp;&amp; 1 &lt; 2</p>"), "<b> is not a tag && 1 < 2");
    }

    #[test]
    fn skipped_and_hidden() {
        let html = "<html><head><title>T</title><style>p{}</style><script>var a = '</div>';</script></head><body>\
                    <nav>menu</nav><p>keep</p><div hidden>secret</div><form><input>f</form><button>b</button>\
                    <svg><text>s</text></svg><noscript><p>ns</p></noscript><template>t</template><footer>foot</footer></body></html>";
        assert_eq!(md(html), "keep");
        // A head without </head> ends at the first body content.
        assert_eq!(md("<html><head><meta charset=utf-8><title>T</title><p>body text"), "body text");
    }

    #[test]
    fn malformed() {
        assert_eq!(md("<b><i>x</b>y</i>"), "***x***y");
        assert_eq!(md("</p></div>text</span><p>more"), "text\n\nmore");
        assert_eq!(md("a < b and 1<2 <3 <"), "a < b and 1<2 <3 <");
        assert_eq!(md("<p>before<!-- hidden <p>comment --> after <!-- unterminated"), "before after");
        assert_eq!(md("<!DOCTYPE html><?xml version='1.0'?><p>x<![CDATA[ a<b ]]>y</p>"), "x a<b y");
        assert_eq!(md("<p>unterminated <a href=\"x"), "unterminated");
        assert_eq!(md("<div class='a' data-x=1 id=b disabled><P CLASS=x>Upper</P></DIV>"), "Upper");
        assert_eq!(md("<p>x<script>if (a<b) {}"), "x");
        assert_eq!(md("<textarea>&lt;raw <b>text</b></textarea>"), "<raw <b>text</b>");
        assert_eq!(md("<h2>one<h3>two</h3>"), "## one\n\n### two");
        assert_eq!(md("<a href=/1>one<a href=/2>two</a>"), "[one](/1)[two](/2)");
        assert_eq!(md("<p>line\r\nbreak</p><pre>a\r\nb</pre>"), "line break\n\n```\na\nb\n```");
        assert_eq!(md(""), "");
        assert_eq!(md("<"), "<");
    }

    /// Run `f` and fail if it takes far longer than linear work should.
    fn quick(f: impl FnOnce() -> String) -> String {
        let start = Instant::now();
        let out = f();
        assert!(start.elapsed() < Duration::from_secs(5), "took {:?}", start.elapsed());
        out
    }

    #[test]
    fn large_inputs_are_linear() {
        let deep = "<div><b><ul><li><a href=x>".repeat(20_000) + "core" + &"</a></li></ul></b></div>".repeat(20_000);
        assert!(quick(|| md(&deep)).contains("core"));
        let unclosed = "<span><b><i>".repeat(100_000);
        quick(|| md(&unclosed));
        let stray = "<div>".repeat(150) + &"</p></x></li>".repeat(100_000);
        quick(|| md(&stray));
        let tables = "<table><tr><td>".repeat(5_000) + "cell";
        assert!(quick(|| md(&tables)).contains("cell"));
        let paras = "<p>Some <b>bold</b> and <a href=\"/l\">a link</a> &amp; text.</p>\n".repeat(50_000);
        let out = quick(|| md(&paras));
        assert_eq!(out.matches("[a link](/l)").count(), 50_000);
        let items = "<ul>".to_string() + &"<li>item".repeat(100_000);
        assert_eq!(quick(|| md(&items)).lines().count(), 100_000);
        quick(|| md(&"&amp;&#x41;&bogus;<".repeat(200_000)));
        quick(|| md(&format!("<pre>{}</pre>", "x\n".repeat(200_000))));
    }

    fn fixture(name: &str) -> String {
        let path = format!("{}/tests/fixtures/html/{name}", env!("CARGO_MANIFEST_DIR"));
        md(&std::fs::read_to_string(path).unwrap())
    }

    #[test]
    fn github_readme() {
        let out = fixture("github_readme.html");
        assert!(out.starts_with("## ripgrep (rg)\n\nripgrep is a line-oriented search tool"), "{out}");
        assert!(out.contains("\n### Documentation quick links\n\n- [Installation](#installation)\n- [User Guide](/BurntSushi/ripgrep/blob/master/GUIDE.md)\n"));
        assert!(out.contains("| Tool | Command | Line count | Time |\n| --- | --- | --- | --- |\n| ripgrep (Unicode) | `rg -n -w '[A-Z]+_SUSPEND'` | 536 | **0.082s** (1.00x) |"));
        assert!(out.contains("```\n$ brew install ripgrep\n```"), "{out}");
        assert!(!out.contains("[](#"), "empty heading anchors are dropped");
    }

    #[test]
    fn python_docs() {
        let out = fixture("python_docs.html");
        assert!(out.starts_with("# `json` — JSON encoder and decoder[¶](#module-json)"), "{out}");
        assert!(out.contains("```python3\n>>> import json\n>>> json.dumps(['foo', {'bar': ('baz', None, 1.0, 2)}])\n"), "{out}");
        assert!(out.contains("\n: Serialize *obj* as a JSON formatted stream"), "{out}");
        assert!(out.contains("\n  : - **obj** ([*object*](../builtins/functions.html#object)) – The Python object to be serialized.\n    - **fp**"));
    }

    #[test]
    fn rustdoc_page() {
        let out = fixture("rustdoc_vec.html");
        assert!(out.contains("# Struct Vec"), "{out}");
        assert!(out.contains("let mut vec = Vec::new();\nvec.push(1);\nvec.push(2);\n"), "{out}");
        assert!(out.contains("// The following is equivalent, but potentially slower:\nlet mut vec = Vec::with_capacity(5);"));
        assert!(!out.contains("play.rust-lang.org"), "icon-only links are dropped");
    }

    #[test]
    fn blog_post() {
        let out = fixture("rust_blog.html");
        assert!(out.starts_with("## Announcing Rust 1.83.0\n\nNov. 28, 2024 · The Rust Release Team\n\n"), "{out}");
        assert!(out.contains("\n### New const capabilities\n"));
        assert!(out.contains("```\nstatic S: i32 = 25;\nconst C: &i32 = &S;\n```"));
        assert!(out.contains("you can [get `rustup`](https://www.rust-lang.org/install.html) from"));
    }

    #[test]
    fn wikipedia_article() {
        let out = fixture("wikipedia.html");
        assert!(out.contains("[Filename extensions](https://en.wikipedia.org/wiki/Filename_extension) | `.md`, `.markdown`"), "{out}");
        assert!(out.contains("\n## History\n"));
        assert!(out.contains("[conventions](https://en.wikipedia.org/wiki/Convention_(norm)) for marking up"));
    }
}
