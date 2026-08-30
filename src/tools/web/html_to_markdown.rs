//! Bounded HTML-to-Markdown conversion for the `web_fetch` tool.
//!
//! Conservative port of `tools/web/html_to_markdown.zig`. The converter works
//! on raw bytes and never allocates beyond the configured output budget.

/// Conversion options.
#[derive(Debug, Clone, Copy)]
pub struct Options {
    pub max_output_bytes: usize,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            max_output_bytes: 10 * 1024 * 1024,
        }
    }
}

/// Convert `html` into bounded markdown.
pub fn convert(html: &[u8], options: Options) -> Vec<u8> {
    let mut parser = Parser::new(options.max_output_bytes);
    parser.convert(html);
    parser.out
}

struct Parser {
    out: Vec<u8>,
    title: Vec<u8>,
    link_text: Vec<u8>,
    max_output_bytes: usize,
    suppress_depth: usize,
    title_active: bool,
    pre_depth: usize,
    active_link_href: Option<Vec<u8>>,
    table_cell_open: bool,
    last_was_space: bool,
}

impl Parser {
    fn new(max_output_bytes: usize) -> Parser {
        Parser {
            out: Vec::new(),
            title: Vec::new(),
            link_text: Vec::new(),
            max_output_bytes,
            suppress_depth: 0,
            title_active: false,
            pre_depth: 0,
            active_link_href: None,
            table_cell_open: false,
            last_was_space: true,
        }
    }

    fn convert(&mut self, html: &[u8]) {
        let mut index: usize = 0;
        while index < html.len() && !self.full() {
            if html[index] != b'<' {
                let next = find_byte(&html[index..], b'<').unwrap_or(html.len() - index);
                self.append_text(&html[index..index + next]);
                index += next;
                continue;
            }

            if html[index..].starts_with(b"<!--") {
                match find_subslice(&html[index + 4..], b"-->") {
                    Some(end) => {
                        index += 4 + end + 3;
                    }
                    None => {
                        index = html.len();
                    }
                }
                continue;
            }

            match find_byte(&html[index..], b'>') {
                Some(tag_end) => {
                    self.handle_tag(&html[index + 1..index + tag_end]);
                    index += tag_end + 1;
                }
                None => {
                    self.append_text(&html[index..]);
                    break;
                }
            }
        }
        self.trim_trailing_whitespace();
        self.prepend_title();
        self.trim_trailing_whitespace();
        if !self.out.is_empty() {
            self.append_byte(b'\n');
        }
    }

    fn handle_tag(&mut self, raw_tag: &[u8]) {
        let mut tag = trim_ws(raw_tag);
        if tag.is_empty() {
            return;
        }
        if tag[0] == b'!' || tag[0] == b'?' {
            return;
        }

        let closing = tag[0] == b'/';
        if closing {
            tag = trim_ws(&tag[1..]);
        }
        let self_closing = !tag.is_empty() && tag[tag.len() - 1] == b'/';
        if self_closing {
            tag = trim_end_ws(&tag[..tag.len() - 1]);
        }

        let name_end = tag_name_end(tag);
        if name_end == 0 {
            return;
        }
        let raw_name = &tag[..name_end];
        let attrs = &tag[name_end..];
        let mut name_buf = [0u8; 32];
        let name = lower_tag_name(&mut name_buf, raw_name);

        if name == b"title" {
            if closing {
                self.title_active = false;
            } else if !self_closing && self.title.is_empty() {
                self.title_active = true;
            }
            return;
        }

        if suppressed_tag(name) {
            if closing {
                if self.suppress_depth > 0 {
                    self.suppress_depth -= 1;
                }
            } else if !self_closing {
                self.suppress_depth += 1;
            }
            return;
        }
        if self.suppress_depth > 0 {
            return;
        }

        if let Some(level) = heading_level(name) {
            if !closing {
                self.block_break();
                for _ in 0..level {
                    self.append_byte(b'#');
                }
                self.append_byte(b' ');
            } else {
                self.block_break();
            }
            return;
        }

        if name == b"p"
            || name == b"div"
            || name == b"section"
            || name == b"article"
            || name == b"main"
            || name == b"header"
            || name == b"footer"
            || name == b"blockquote"
        {
            self.block_break();
            return;
        }
        if name == b"br" {
            self.soft_break();
            return;
        }
        if name == b"ul" || name == b"ol" {
            self.block_break();
            return;
        }
        if name == b"li" {
            if !closing {
                self.block_break();
                self.append_slice(b"- ");
            } else {
                self.block_break();
            }
            return;
        }
        if name == b"a" {
            if !closing {
                if self.active_link_href.is_none() {
                    self.active_link_href = attribute_value(attrs, b"href");
                    self.link_text.clear();
                }
            } else {
                self.flush_link();
            }
            return;
        }
        if name == b"code" {
            if self.pre_depth == 0 {
                self.append_byte(b'`');
            }
            return;
        }
        if name == b"pre" {
            if !closing {
                self.block_break();
                self.append_slice(b"```\n");
                self.pre_depth += 1;
            } else {
                if self.pre_depth > 0 {
                    self.pre_depth -= 1;
                }
                self.trim_trailing_whitespace();
                self.append_slice(b"\n```");
                self.block_break();
            }
            return;
        }
        if name == b"tr" {
            if !closing {
                self.block_break();
            } else {
                if self.table_cell_open {
                    self.append_byte(b' ');
                    self.table_cell_open = false;
                }
                self.append_byte(b'|');
                self.block_break();
            }
            return;
        }
        if name == b"th" || name == b"td" {
            if !closing {
                self.append_slice(b"| ");
                self.table_cell_open = true;
            } else if self.table_cell_open {
                self.append_byte(b' ');
                self.table_cell_open = false;
            }
            return;
        }
        if name == b"img" && !closing {
            if let Some(alt) = attribute_value(attrs, b"alt") {
                self.append_text(&alt);
            }
        }
    }

    fn append_text(&mut self, raw: &[u8]) {
        if self.title_active {
            return self.append_title_text(raw);
        }
        if self.suppress_depth > 0 {
            return;
        }

        let mut index: usize = 0;
        while index < raw.len() && !self.full() {
            if raw[index] == b'&' {
                if self.append_entity(raw, &mut index) {
                    continue;
                }
            }
            let byte = raw[index];
            index += 1;
            if self.pre_depth > 0 {
                self.append_byte(byte);
            } else if is_whitespace(byte) {
                self.append_space();
            } else {
                self.append_byte(byte);
            }
        }
    }

    fn append_title_text(&mut self, raw: &[u8]) {
        let mut index: usize = 0;
        while index < raw.len() && self.title.len() < self.max_output_bytes {
            let start = index;
            index += 1;
            let mut entity_buf = [0u8; 4];
            let bytes: &[u8] = if raw[start] == b'&' {
                let limit = raw.len().min(start + 32);
                match find_byte(&raw[start..limit], b';') {
                    Some(semi_rel) => {
                        let end = start + semi_rel;
                        let entity = &raw[start + 1..end];
                        index = end + 1;
                        if let Some(codepoint) = decode_numeric_entity(entity) {
                            let len = encode_utf8(codepoint, &mut entity_buf);
                            &entity_buf[..len]
                        } else if let Some(value) = named_entity(entity) {
                            value
                        } else {
                            &raw[start..end + 1]
                        }
                    }
                    None => &raw[start..index],
                }
            } else {
                &raw[start..index]
            };
            for &byte in bytes {
                self.append_title_byte(byte);
            }
        }
    }

    fn append_title_byte(&mut self, byte: u8) {
        if self.title.len() >= self.max_output_bytes {
            return;
        }
        if is_whitespace(byte) {
            if !self.title.is_empty() && self.title.last() != Some(&b' ') {
                self.title.push(b' ');
            }
        } else {
            self.title.push(byte);
        }
    }

    fn append_entity(&mut self, raw: &[u8], index: &mut usize) -> bool {
        let start = *index;
        let limit = raw.len().min(start + 32);
        let semi_rel = match find_byte(&raw[start..limit], b';') {
            Some(offset) => offset,
            None => return false,
        };
        let end = start + semi_rel;
        let entity = &raw[start + 1..end];
        *index = end + 1;

        if let Some(codepoint) = decode_numeric_entity(entity) {
            let mut buf = [0u8; 4];
            let len = encode_utf8(codepoint, &mut buf);
            self.append_slice(&buf[..len]);
            return true;
        }
        if let Some(value) = named_entity(entity) {
            self.append_slice(value);
            return true;
        }
        self.append_slice(&raw[start..end + 1]);
        true
    }

    fn append_space(&mut self) {
        if self.last_was_space {
            return;
        }
        self.append_byte(b' ');
    }

    fn block_break(&mut self) {
        self.trim_trailing_inline_whitespace();
        if self.out.is_empty() {
            return;
        }
        if self.out.ends_with(b"\n\n") {
            return;
        }
        if self.out.ends_with(b"\n") {
            self.append_byte(b'\n');
        } else {
            self.append_slice(b"\n\n");
        }
    }

    fn soft_break(&mut self) {
        self.trim_trailing_inline_whitespace();
        if self.out.is_empty() || self.out.ends_with(b"\n") {
            return;
        }
        self.append_byte(b'\n');
    }

    fn flush_link(&mut self) {
        let href = match self.active_link_href.take() {
            Some(href) => href,
            None => return,
        };
        let mut link_text = std::mem::take(&mut self.link_text);
        trim_ws_vec(&mut link_text);
        if link_text.is_empty() {
            return;
        }
        self.append_byte(b'[');
        self.append_slice(&link_text);
        self.append_slice(b"](");
        self.append_slice(&href);
        self.append_byte(b')');
    }

    fn append_byte(&mut self, byte: u8) {
        self.append_slice(&[byte]);
    }

    fn append_slice(&mut self, bytes: &[u8]) {
        if bytes.is_empty() || self.full() {
            return;
        }
        let remaining = self.max_output_bytes - self.out.len();
        let take = bytes.len().min(remaining);
        if self.active_link_href.is_some() && self.pre_depth == 0 {
            self.link_text.extend_from_slice(&bytes[..take]);
        } else {
            self.out.extend_from_slice(&bytes[..take]);
        }
        if take > 0 {
            self.last_was_space = is_whitespace(bytes[take - 1]);
        }
    }

    fn full(&self) -> bool {
        self.out.len() >= self.max_output_bytes
    }

    fn trim_trailing_whitespace(&mut self) {
        while self.out.last().map_or(false, |&b| is_whitespace(b)) {
            self.out.pop();
        }
        self.last_was_space = true;
    }

    fn trim_trailing_inline_whitespace(&mut self) {
        while let Some(&byte) = self.out.last() {
            if byte != b' ' && byte != b'\t' {
                break;
            }
            self.out.pop();
        }
        self.last_was_space = self.out.is_empty() || self.out.last().map_or(false, |&b| is_whitespace(b));
    }

    fn trim_title(&mut self) {
        trim_ws_vec(&mut self.title);
    }

    fn prepend_title(&mut self) {
        self.trim_title();
        if self.title.is_empty() || self.max_output_bytes == 0 {
            return;
        }

        let out = std::mem::take(&mut self.out);
        let mut merged: Vec<u8> = Vec::new();
        append_within_output_limit(self.max_output_bytes, &mut merged, b"# ");
        append_within_output_limit(self.max_output_bytes, &mut merged, &self.title);
        if !out.is_empty() {
            append_within_output_limit(self.max_output_bytes, &mut merged, b"\n\n");
            append_within_output_limit(self.max_output_bytes, &mut merged, &out);
        }
        self.out = merged;
    }
}

fn append_within_output_limit(max_output_bytes: usize, output: &mut Vec<u8>, bytes: &[u8]) {
    let remaining = max_output_bytes - output.len();
    output.extend_from_slice(&bytes[..bytes.len().min(remaining)]);
}

fn trim_ws_vec(value: &mut Vec<u8>) {
    while value.last().map_or(false, |&b| is_whitespace(b)) {
        value.pop();
    }
    let mut start: usize = 0;
    while start < value.len() && is_whitespace(value[start]) {
        start += 1;
    }
    if start > 0 {
        value.drain(..start);
    }
}

fn tag_name_end(tag: &[u8]) -> usize {
    let mut index: usize = 0;
    while index < tag.len() {
        if !tag[index].is_ascii_alphanumeric() {
            break;
        }
        index += 1;
    }
    index
}

fn lower_tag_name<'a>(buf: &'a mut [u8; 32], raw: &[u8]) -> &'a [u8] {
    let len = buf.len().min(raw.len());
    for (index, &byte) in raw[..len].iter().enumerate() {
        buf[index] = byte.to_ascii_lowercase();
    }
    &buf[..len]
}

fn suppressed_tag(name: &[u8]) -> bool {
    name == b"script"
        || name == b"style"
        || name == b"head"
        || name == b"noscript"
        || name == b"template"
        || name == b"svg"
        || name == b"canvas"
}

fn heading_level(name: &[u8]) -> Option<u8> {
    if name.len() != 2 || name[0] != b'h' {
        return None;
    }
    match name[1] {
        b'1'..=b'6' => Some(name[1] - b'0'),
        _ => None,
    }
}

fn attribute_value(attrs: &[u8], wanted_name: &[u8]) -> Option<Vec<u8>> {
    let mut index: usize = 0;
    while index < attrs.len() {
        while index < attrs.len() && is_whitespace(attrs[index]) {
            index += 1;
        }
        let name_start = index;
        while index < attrs.len()
            && (attrs[index].is_ascii_alphanumeric()
                || attrs[index] == b'-'
                || attrs[index] == b'_')
        {
            index += 1;
        }
        if index == name_start {
            index += 1;
            continue;
        }
        let name = &attrs[name_start..index];
        while index < attrs.len() && is_whitespace(attrs[index]) {
            index += 1;
        }
        if index >= attrs.len() || attrs[index] != b'=' {
            continue;
        }
        index += 1;
        while index < attrs.len() && is_whitespace(attrs[index]) {
            index += 1;
        }
        if index >= attrs.len() {
            break;
        }

        let value_start = if attrs[index] == b'"' || attrs[index] == b'\'' {
            let quote = attrs[index];
            index += 1;
            let start = index;
            while index < attrs.len() && attrs[index] != quote {
                index += 1;
            }
            start
        } else {
            let start = index;
            while index < attrs.len() && !is_whitespace(attrs[index]) {
                index += 1;
            }
            start
        };
        let value_end = index;
        if index < attrs.len() && (attrs[index] == b'"' || attrs[index] == b'\'') {
            index += 1;
        }

        if !eql_ignore_case(name, wanted_name) {
            continue;
        }
        let mut out = Parser::new(value_end - value_start + 16);
        out.append_text(&attrs[value_start..value_end]);
        return Some(out.out);
    }
    None
}

fn decode_numeric_entity(entity: &[u8]) -> Option<u32> {
    if entity.len() < 2 || entity[0] != b'#' {
        return None;
    }
    let (radix, digits) = if entity.len() > 2 && (entity[1] == b'x' || entity[1] == b'X') {
        (16, &entity[2..])
    } else {
        (10, &entity[1..])
    };
    let digits = std::str::from_utf8(digits).ok()?;
    let value = u32::from_str_radix(digits, radix).ok()?;
    if value == 0 || value > 0x10ffff {
        return None;
    }
    if (0xd800..=0xdfff).contains(&value) {
        return None;
    }
    Some(value)
}

fn named_entity(entity: &[u8]) -> Option<&'static [u8]> {
    const ENTRIES: &[(&[u8], &[u8])] = &[
        ("amp".as_bytes(), "&".as_bytes()),
        ("lt".as_bytes(), "<".as_bytes()),
        ("gt".as_bytes(), ">".as_bytes()),
        ("quot".as_bytes(), "\"".as_bytes()),
        ("apos".as_bytes(), "'".as_bytes()),
        ("nbsp".as_bytes(), " ".as_bytes()),
        ("copy".as_bytes(), "©".as_bytes()),
        ("reg".as_bytes(), "®".as_bytes()),
        ("trade".as_bytes(), "™".as_bytes()),
        ("mdash".as_bytes(), "—".as_bytes()),
        ("ndash".as_bytes(), "–".as_bytes()),
        ("hellip".as_bytes(), "…".as_bytes()),
        ("lsquo".as_bytes(), "‘".as_bytes()),
        ("rsquo".as_bytes(), "’".as_bytes()),
        ("ldquo".as_bytes(), "“".as_bytes()),
        ("rdquo".as_bytes(), "”".as_bytes()),
        ("laquo".as_bytes(), "«".as_bytes()),
        ("raquo".as_bytes(), "»".as_bytes()),
        ("times".as_bytes(), "×".as_bytes()),
        ("divide".as_bytes(), "÷".as_bytes()),
        ("bull".as_bytes(), "•".as_bytes()),
        ("middot".as_bytes(), "·".as_bytes()),
        ("sect".as_bytes(), "§".as_bytes()),
        ("para".as_bytes(), "¶".as_bytes()),
        ("deg".as_bytes(), "°".as_bytes()),
        ("plusmn".as_bytes(), "±".as_bytes()),
        ("euro".as_bytes(), "€".as_bytes()),
        ("pound".as_bytes(), "£".as_bytes()),
        ("yen".as_bytes(), "¥".as_bytes()),
        ("cent".as_bytes(), "¢".as_bytes()),
    ];
    ENTRIES
        .iter()
        .find(|(name, _)| *name == entity)
        .map(|(_, value)| *value)
}

fn encode_utf8(codepoint: u32, buf: &mut [u8; 4]) -> usize {
    let c = char::from_u32(codepoint).expect("valid codepoint");
    c.encode_utf8(buf).len()
}

fn is_whitespace(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

fn trim_ws(mut s: &[u8]) -> &[u8] {
    while let Some((&first, rest)) = s.split_first() {
        if matches!(first, b' ' | b'\t' | b'\r' | b'\n') {
            s = rest;
        } else {
            break;
        }
    }
    while let Some((&last, rest)) = s.split_last() {
        if matches!(last, b' ' | b'\t' | b'\r' | b'\n') {
            s = rest;
        } else {
            break;
        }
    }
    s
}

fn trim_end_ws(mut s: &[u8]) -> &[u8] {
    while let Some((&last, rest)) = s.split_last() {
        if matches!(last, b' ' | b'\t' | b'\r' | b'\n') {
            s = rest;
        } else {
            break;
        }
    }
    s
}

fn eql_ignore_case(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b.iter())
            .all(|(x, y)| x.to_ascii_lowercase() == y.to_ascii_lowercase())
}

fn find_byte(haystack: &[u8], needle: u8) -> Option<usize> {
    haystack.iter().position(|&b| b == needle)
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    haystack.windows(needle.len()).position(|window| window == needle)
}

#[cfg(test)]
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    find_subslice(haystack, needle).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn web_fetch_converts_representative_html_to_bounded_markdown() {
        let html = br#"<!doctype html>
<html>
<head><title>Ignored</title><style>.x{}</style><script>alert(1)</script></head>
<body>
<h1>Release &amp; Notes</h1>
<p>Hello <a href="/docs">docs</a> and <code>fx ask</code>.</p>
<ul><li>One</li><li>Two&nbsp;items</li></ul>
<pre><code>const x = 1 &lt; 2;</code></pre>
<table><tr><th>Name</th><th>Value</th></tr><tr><td>A</td><td>42</td></tr></table>
<img src="x.png" alt="diagram">
<p>&#169; &#x00AE; &unknown;</p>
</body></html>"#;

        let markdown = convert(html, Options { max_output_bytes: 4096 });

        assert!(contains(&markdown, b"# Release & Notes"));
        assert!(contains(&markdown, b"# Ignored"));
        assert!(contains(&markdown, b"[docs](/docs)"));
        assert!(contains(&markdown, b"`fx ask`"));
        assert!(contains(&markdown, b"- One"));
        assert!(contains(&markdown, b"Two items"));
        assert!(contains(&markdown, b"const x = 1 < 2;"));
        assert!(contains(&markdown, b"| Name | Value |"));
        assert!(contains(&markdown, b"diagram"));
        assert!(contains(&markdown, "© ® &unknown;".as_bytes()));
        assert!(!contains(&markdown, b"alert(1)"));
        assert!(!contains(&markdown, b".x{}"));
    }

    #[test]
    fn web_fetch_decodes_entities_in_html_titles() {
        let markdown = convert(
            b"<html><head><title>News &amp; Updates &mdash; Site</title></head><body><p>Body</p></body></html>",
            Options { max_output_bytes: 4096 },
        );

        assert_eq!(markdown, "# News & Updates — Site\n\nBody\n".as_bytes());
    }

    #[test]
    fn web_fetch_malformed_html_conversion_remains_bounded() {
        let mut html: Vec<u8> = Vec::new();
        for _ in 0..2048 {
            html.extend_from_slice(b"<div><span>unclosed &amp; text ");
        }

        let markdown = convert(&html, Options { max_output_bytes: 1024 });

        assert!(markdown.len() <= 1024);
        assert!(contains(&markdown, b"unclosed & text"));
    }
}
