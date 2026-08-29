#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeywordCase {
    Sensitive,
    AsciiInsensitive,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockComment {
    pub start: &'static [u8],
    pub end: &'static [u8],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Detection {
    None,
    TypescriptAssertion,
    Json,
    ShellShebang,
    PythonHeader,
    SqlSelect,
    DockerfileFrom,
    GoPackage,
    RustFunction,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Profile {
    pub label: &'static [u8],
    pub aliases: &'static [&'static [u8]],
    pub line_comments: &'static [&'static [u8]],
    pub block_comment: Option<BlockComment>,
    pub quotes: &'static [u8],
    pub keywords: &'static [&'static [u8]],
    pub literals: &'static [&'static [u8]],
    pub keyword_case: KeywordCase,
    pub detection: Detection,
}

macro_rules! profile {
    ($label:literal, $aliases:expr $(, $field:ident : $value:expr)*) => {
        Profile {
            label: $label,
            aliases: $aliases,
            $($field: $value,)*
            ..DEFAULT_PROFILE
        }
    };
}

const DEFAULT_PROFILE: Profile = Profile {
    label: b"",
    aliases: &[],
    line_comments: &[],
    block_comment: None,
    quotes: &[],
    keywords: &[],
    literals: &[],
    keyword_case: KeywordCase::Sensitive,
    detection: Detection::None,
};

static PROFILES: &[Profile] = &[
    profile!(b"zig", &[b"zig"], line_comments: &[b"//"], quotes: b"\"", keywords: &[b"const", b"var", b"fn", b"pub", b"return", b"if", b"else", b"while", b"for", b"struct", b"enum", b"union", b"try", b"catch", b"comptime", b"defer", b"errdefer", b"async", b"await", b"anytype", b"void"]),
    profile!(b"ts", &[b"js", b"jsx", b"javascript", b"ts", b"tsx", b"typescript"], line_comments: &[b"//"], block_comment: Some(BlockComment { start: b"/*", end: b"*/" }), quotes: b"\"'`", keywords: &[b"const", b"let", b"var", b"function", b"class", b"interface", b"type", b"export", b"import", b"from", b"return", b"if", b"else", b"for", b"while", b"async", b"await", b"new", b"extends", b"implements", b"public", b"private", b"readonly"], literals: &[b"true", b"false", b"null", b"undefined"], detection: Detection::TypescriptAssertion),
    profile!(b"json", &[b"json"], quotes: b"\"", literals: &[b"true", b"false", b"null"], detection: Detection::Json),
    profile!(b"sh", &[b"sh", b"bash", b"zsh", b"shell"], line_comments: &[b"#"], quotes: b"\"'`", keywords: &[b"if", b"then", b"fi", b"for", b"do", b"done", b"in", b"case", b"esac", b"function", b"local", b"export", b"readonly", b"return"], literals: &[b"true", b"false", b"null"], detection: Detection::ShellShebang),
    profile!(b"python", &[b"python", b"py"], line_comments: &[b"#"], quotes: b"\"'", keywords: &[b"def", b"class", b"return", b"if", b"elif", b"else", b"for", b"while", b"in", b"import", b"from", b"as", b"try", b"except", b"with", b"lambda", b"async", b"await", b"pass", b"raise", b"yield", b"match", b"case"], literals: &[b"True", b"False", b"None"], detection: Detection::PythonHeader),
    profile!(b"yaml", &[b"yaml", b"yml"], line_comments: &[b"#"], quotes: b"\"'", literals: &[b"true", b"false", b"null", b"yes", b"no", b"on", b"off"]),
    profile!(b"toml", &[b"toml"], line_comments: &[b"#"], quotes: b"\"'", literals: &[b"true", b"false"]),
    profile!(b"sql", &[b"sql"], line_comments: &[b"--"], block_comment: Some(BlockComment { start: b"/*", end: b"*/" }), quotes: b"\"'", keywords: &[b"select", b"from", b"where", b"join", b"left", b"right", b"inner", b"outer", b"on", b"insert", b"into", b"values", b"update", b"set", b"delete", b"create", b"alter", b"drop", b"table", b"index", b"group", b"by", b"order", b"having", b"limit", b"as", b"and", b"or", b"not", b"distinct", b"union"], literals: &[b"true", b"false", b"null"], keyword_case: KeywordCase::AsciiInsensitive, detection: Detection::SqlSelect),
    profile!(b"dockerfile", &[b"dockerfile", b"docker"], line_comments: &[b"#"], quotes: b"\"'", keywords: &[b"from", b"run", b"cmd", b"entrypoint", b"copy", b"add", b"workdir", b"env", b"arg", b"expose", b"volume", b"user", b"label", b"onbuild", b"stopsignal", b"healthcheck", b"shell", b"maintainer"], keyword_case: KeywordCase::AsciiInsensitive, detection: Detection::DockerfileFrom),
    profile!(b"rust", &[b"rust", b"rs"], line_comments: &[b"//"], block_comment: Some(BlockComment { start: b"/*", end: b"*/" }), quotes: b"\"'", keywords: &[b"fn", b"let", b"mut", b"pub", b"struct", b"enum", b"impl", b"trait", b"use", b"mod", b"crate", b"return", b"if", b"else", b"match", b"for", b"while", b"loop", b"async", b"await", b"move", b"where", b"self", b"super"], literals: &[b"true", b"false", b"None", b"Some"], detection: Detection::RustFunction),
    profile!(b"go", &[b"go"], line_comments: &[b"//"], block_comment: Some(BlockComment { start: b"/*", end: b"*/" }), quotes: b"\"`", keywords: &[b"package", b"import", b"func", b"var", b"const", b"type", b"struct", b"interface", b"return", b"if", b"else", b"for", b"range", b"switch", b"case", b"go", b"defer", b"select", b"chan", b"map"], literals: &[b"true", b"false", b"nil"], detection: Detection::GoPackage),
    profile!(b"c", &[b"c", b"h"], line_comments: &[b"//"], block_comment: Some(BlockComment { start: b"/*", end: b"*/" }), quotes: b"\"'", keywords: &[b"auto", b"break", b"case", b"char", b"const", b"continue", b"default", b"do", b"double", b"else", b"enum", b"extern", b"float", b"for", b"goto", b"if", b"int", b"long", b"return", b"short", b"signed", b"sizeof", b"static", b"struct", b"switch", b"typedef", b"union", b"unsigned", b"void", b"volatile", b"while"], literals: &[b"true", b"false", b"NULL"]),
    profile!(b"cpp", &[b"cpp", b"c++", b"cc", b"cxx", b"hpp"], line_comments: &[b"//"], block_comment: Some(BlockComment { start: b"/*", end: b"*/" }), quotes: b"\"'", keywords: &[b"auto", b"bool", b"class", b"const", b"constexpr", b"decltype", b"delete", b"enum", b"explicit", b"friend", b"inline", b"namespace", b"new", b"nullptr", b"private", b"protected", b"public", b"template", b"this", b"typename", b"using", b"virtual", b"void"], literals: &[b"true", b"false", b"nullptr", b"NULL"]),
    profile!(b"csharp", &[b"csharp", b"cs"], line_comments: &[b"//"], block_comment: Some(BlockComment { start: b"/*", end: b"*/" }), quotes: b"\"'", keywords: &[b"class", b"namespace", b"using", b"public", b"private", b"protected", b"internal", b"static", b"void", b"string", b"int", b"var", b"new", b"return", b"if", b"else", b"for", b"foreach", b"while", b"async", b"await", b"interface", b"record", b"get", b"set"], literals: &[b"true", b"false", b"null"]),
    profile!(b"java", &[b"java"], line_comments: &[b"//"], block_comment: Some(BlockComment { start: b"/*", end: b"*/" }), quotes: b"\"'", keywords: &[b"class", b"interface", b"package", b"import", b"public", b"private", b"protected", b"static", b"final", b"void", b"new", b"return", b"if", b"else", b"for", b"while", b"try", b"catch", b"throws", b"extends", b"implements", b"record", b"var"], literals: &[b"true", b"false", b"null"]),
    profile!(b"kotlin", &[b"kotlin", b"kt", b"kts"], line_comments: &[b"//"], block_comment: Some(BlockComment { start: b"/*", end: b"*/" }), quotes: b"\"'", keywords: &[b"fun", b"val", b"var", b"class", b"object", b"interface", b"package", b"import", b"public", b"private", b"return", b"if", b"else", b"when", b"for", b"while", b"try", b"catch", b"data", b"sealed", b"suspend"], literals: &[b"true", b"false", b"null"]),
    profile!(b"php", &[b"php"], line_comments: &[b"//", b"#"], block_comment: Some(BlockComment { start: b"/*", end: b"*/" }), quotes: b"\"'", keywords: &[b"function", b"class", b"public", b"private", b"protected", b"namespace", b"use", b"return", b"if", b"else", b"foreach", b"for", b"while", b"try", b"catch", b"new", b"static", b"const", b"echo", b"yield"], literals: &[b"true", b"false", b"null"]),
    profile!(b"ruby", &[b"ruby", b"rb"], line_comments: &[b"#"], quotes: b"\"'", keywords: &[b"def", b"class", b"module", b"end", b"return", b"if", b"elsif", b"else", b"unless", b"case", b"when", b"do", b"while", b"for", b"in", b"begin", b"rescue", b"require", b"attr_reader"], literals: &[b"true", b"false", b"nil"]),
    profile!(b"swift", &[b"swift"], line_comments: &[b"//"], block_comment: Some(BlockComment { start: b"/*", end: b"*/" }), quotes: b"\"'", keywords: &[b"func", b"let", b"var", b"class", b"struct", b"enum", b"protocol", b"extension", b"import", b"public", b"private", b"return", b"if", b"else", b"guard", b"for", b"while", b"switch", b"case", b"async", b"await", b"throws", b"try"], literals: &[b"true", b"false", b"nil"]),
    profile!(b"powershell", &[b"powershell", b"ps1", b"pwsh", b"ps"], line_comments: &[b"#"], block_comment: Some(BlockComment { start: b"<#", end: b"#>" }), quotes: b"\"'", keywords: &[b"function", b"param", b"if", b"else", b"elseif", b"foreach", b"for", b"while", b"switch", b"return", b"throw", b"try", b"catch", b"finally", b"begin", b"process", b"end", b"filter", b"class", b"enum"], literals: &[b"true", b"false", b"null"], keyword_case: KeywordCase::AsciiInsensitive),
    profile!(b"lua", &[b"lua"], line_comments: &[b"--"], block_comment: Some(BlockComment { start: b"--[[", end: b"]]" }), quotes: b"\"'", keywords: &[b"and", b"break", b"do", b"else", b"elseif", b"end", b"false", b"for", b"function", b"goto", b"if", b"in", b"local", b"nil", b"not", b"or", b"repeat", b"return", b"then", b"true", b"until", b"while"], literals: &[b"true", b"false", b"nil"]),
    profile!(b"html", &[b"html", b"htm"], block_comment: Some(BlockComment { start: b"<!--", end: b"-->" }), quotes: b"\"'", keywords: &[b"html", b"head", b"body", b"main", b"header", b"footer", b"section", b"article", b"div", b"span", b"a", b"p", b"script", b"style", b"link", b"meta", b"title", b"button", b"input", b"form", b"img", b"ul", b"li"]),
    profile!(b"xml", &[b"xml"], block_comment: Some(BlockComment { start: b"<!--", end: b"-->" }), quotes: b"\"'", keywords: &[b"xml", b"version", b"encoding", b"DOCTYPE", b"CDATA"]),
    profile!(b"css", &[b"css"], block_comment: Some(BlockComment { start: b"/*", end: b"*/" }), quotes: b"\"'", keywords: &[b"color", b"background", b"display", b"position", b"margin", b"padding", b"border", b"font", b"width", b"height", b"flex", b"grid", b"align", b"justify", b"transition", b"transform", b"animation", b"media"]),
    profile!(b"hcl", &[b"hcl", b"terraform", b"tf"], line_comments: &[b"#", b"//"], block_comment: Some(BlockComment { start: b"/*", end: b"*/" }), quotes: b"\"'", keywords: &[b"resource", b"module", b"variable", b"output", b"provider", b"terraform", b"locals", b"data", b"dynamic", b"for_each", b"count"], literals: &[b"true", b"false", b"null"]),
];

pub fn resolve(label: &[u8]) -> Option<&'static Profile> {
    PROFILES.iter().find(|profile| {
        profile
            .aliases
            .iter()
            .any(|alias| label.eq_ignore_ascii_case(alias))
    })
}

pub fn infer(source: &[u8]) -> Option<&'static Profile> {
    PROFILES
        .iter()
        .find(|profile| matches_detection(profile.detection, source))
}

fn matches_detection(detection: Detection, source: &[u8]) -> bool {
    match detection {
        Detection::None => false,
        Detection::TypescriptAssertion => matches_typescript_assertion(source),
        Detection::Json => is_valid_json(source),
        Detection::ShellShebang => matches_shell_shebang(source),
        Detection::PythonHeader => matches_python_header(source),
        Detection::SqlSelect => matches_sql_select(source),
        Detection::DockerfileFrom => starts_with_ignore_case(first_nonblank_line(source), b"from "),
        Detection::GoPackage => {
            starts_with(first_nonblank_line(source), b"package ")
                && contains_line_start(source, b"func ")
        }
        Detection::RustFunction => matches_rust_function(source),
    }
}

fn matches_typescript_assertion(source: &[u8]) -> bool {
    let needle = b"} as ";
    let mut start = 0;
    while let Some(offset) = find_subslice(&source[start..], needle) {
        let type_start = start + offset + needle.len();
        if source.get(type_start).is_some_and(u8::is_ascii_uppercase) {
            return true;
        }
        start = type_start;
    }
    false
}

fn is_valid_json(source: &[u8]) -> bool {
    let trimmed = trim_bytes(source, b" \t\r\n");
    if !matches!(trimmed.first(), Some(b'{') | Some(b'[')) {
        return false;
    }
    if core::str::from_utf8(trimmed).is_err() {
        return false;
    }
    let mut parser = JsonParser {
        bytes: trimmed,
        index: 0,
    };
    parser.value() && parser.index == trimmed.len()
}

struct JsonParser<'a> {
    bytes: &'a [u8],
    index: usize,
}

impl JsonParser<'_> {
    fn value(&mut self) -> bool {
        self.whitespace();
        match self.bytes.get(self.index) {
            Some(b'{') => self.object(),
            Some(b'[') => self.array(),
            Some(b'\"') => self.string(),
            Some(b't') => self.literal(b"true"),
            Some(b'f') => self.literal(b"false"),
            Some(b'n') => self.literal(b"null"),
            Some(b'-' | b'0'..=b'9') => self.number(),
            _ => false,
        }
    }

    fn object(&mut self) -> bool {
        self.index += 1;
        self.whitespace();
        if self.take(b'}') {
            return true;
        }
        loop {
            if !self.string() {
                return false;
            }
            self.whitespace();
            if !self.take(b':') || !self.value() {
                return false;
            }
            self.whitespace();
            if self.take(b'}') {
                return true;
            }
            if !self.take(b',') {
                return false;
            }
            self.whitespace();
        }
    }

    fn array(&mut self) -> bool {
        self.index += 1;
        self.whitespace();
        if self.take(b']') {
            return true;
        }
        loop {
            if !self.value() {
                return false;
            }
            self.whitespace();
            if self.take(b']') {
                return true;
            }
            if !self.take(b',') {
                return false;
            }
        }
    }

    fn string(&mut self) -> bool {
        if !self.take(b'\"') {
            return false;
        }
        while let Some(&byte) = self.bytes.get(self.index) {
            self.index += 1;
            match byte {
                b'\"' => return true,
                0x00..=0x1f => return false,
                b'\\' => match self.bytes.get(self.index).copied() {
                    Some(b'\"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't') => {
                        self.index += 1
                    }
                    Some(b'u') => {
                        self.index += 1;
                        let Some(code_unit) = self.hex4() else {
                            return false;
                        };
                        if (0xd800..=0xdbff).contains(&code_unit) {
                            if !self.bytes[self.index..].starts_with(b"\\u") {
                                return false;
                            }
                            self.index += 2;
                            let Some(low) = self.hex4() else {
                                return false;
                            };
                            if !(0xdc00..=0xdfff).contains(&low) {
                                return false;
                            }
                        } else if (0xdc00..=0xdfff).contains(&code_unit) {
                            return false;
                        }
                    }
                    _ => return false,
                },
                _ => {}
            }
        }
        false
    }

    fn hex4(&mut self) -> Option<u16> {
        if self.index + 4 > self.bytes.len() {
            return None;
        }
        let mut value = 0u16;
        for &byte in &self.bytes[self.index..self.index + 4] {
            value = value * 16 + u16::from(hex_value(byte)?);
        }
        self.index += 4;
        Some(value)
    }

    fn number(&mut self) -> bool {
        self.take(b'-');
        match self.bytes.get(self.index) {
            Some(b'0') => self.index += 1,
            Some(b'1'..=b'9') => {
                self.index += 1;
                while self.bytes.get(self.index).is_some_and(u8::is_ascii_digit) {
                    self.index += 1;
                }
            }
            _ => return false,
        }
        if self.take(b'.') {
            if !self.bytes.get(self.index).is_some_and(u8::is_ascii_digit) {
                return false;
            }
            while self.bytes.get(self.index).is_some_and(u8::is_ascii_digit) {
                self.index += 1;
            }
        }
        if matches!(self.bytes.get(self.index), Some(b'e' | b'E')) {
            self.index += 1;
            if matches!(self.bytes.get(self.index), Some(b'+' | b'-')) {
                self.index += 1;
            }
            if !self.bytes.get(self.index).is_some_and(u8::is_ascii_digit) {
                return false;
            }
            while self.bytes.get(self.index).is_some_and(u8::is_ascii_digit) {
                self.index += 1;
            }
        }
        true
    }

    fn literal(&mut self, literal: &[u8]) -> bool {
        if self.bytes[self.index..].starts_with(literal) {
            self.index += literal.len();
            true
        } else {
            false
        }
    }

    fn whitespace(&mut self) {
        while matches!(
            self.bytes.get(self.index),
            Some(b' ' | b'\t' | b'\r' | b'\n')
        ) {
            self.index += 1;
        }
    }

    fn take(&mut self, byte: u8) -> bool {
        if self.bytes.get(self.index) == Some(&byte) {
            self.index += 1;
            true
        } else {
            false
        }
    }
}

fn matches_shell_shebang(source: &[u8]) -> bool {
    let line = first_nonblank_line(source);
    line.starts_with(b"#!")
        && (find_subslice(line, b"bash").is_some()
            || find_subslice(line, b"zsh").is_some()
            || find_subslice(line, b"/sh").is_some())
}

fn matches_python_header(source: &[u8]) -> bool {
    let line = first_nonblank_line(source);
    (line.starts_with(b"def ") || line.starts_with(b"class ")) && line.ends_with(b":")
}

fn matches_sql_select(source: &[u8]) -> bool {
    starts_with_ignore_case(first_nonblank_line(source), b"select ")
        && contains_word_ignore_case(source, b"from")
}

fn matches_rust_function(source: &[u8]) -> bool {
    let line = first_nonblank_line(source);
    if !line.starts_with(b"fn ") && !line.starts_with(b"pub fn ") {
        return false;
    }
    find_subslice(source, b"let ").is_some()
        || find_subslice(source, b"println!").is_some()
        || find_subslice(line, b"->").is_some()
}

fn first_nonblank_line(source: &[u8]) -> &[u8] {
    source
        .split(|&byte| byte == b'\n')
        .map(trim_line)
        .find(|line| !line.is_empty())
        .unwrap_or(b"")
}

fn contains_line_start(source: &[u8], prefix: &[u8]) -> bool {
    source.starts_with(prefix)
        || source
            .windows(prefix.len() + 1)
            .any(|window| window[0] == b'\n' && window[1..].starts_with(prefix))
}

fn starts_with(text: &[u8], prefix: &[u8]) -> bool {
    text.starts_with(prefix)
}

fn starts_with_ignore_case(text: &[u8], prefix: &[u8]) -> bool {
    text.get(..prefix.len())
        .is_some_and(|start| start.eq_ignore_ascii_case(prefix))
}

fn contains_word_ignore_case(source: &[u8], word: &[u8]) -> bool {
    source
        .windows(word.len())
        .enumerate()
        .any(|(index, candidate)| {
            let end = index + word.len();
            candidate.eq_ignore_ascii_case(word)
                && (index == 0 || !is_word_byte(source[index - 1]))
                && (end == source.len() || !is_word_byte(source[end]))
        })
}

fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn trim_line(bytes: &[u8]) -> &[u8] {
    trim_bytes(bytes, b" \t\r")
}

fn trim_bytes<'a>(mut bytes: &'a [u8], trim: &[u8]) -> &'a [u8] {
    while bytes.first().is_some_and(|byte| trim.contains(byte)) {
        bytes = &bytes[1..];
    }
    while bytes.last().is_some_and(|byte| trim.contains(byte)) {
        bytes = &bytes[..bytes.len() - 1];
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typescript_assertions_infer_the_canonical_typescript_label() {
        let source = b"const hook = await resumeHook(token, { cleanup: true } as CleanupSignal);";
        assert_eq!(infer(source).unwrap().label, b"ts");
        assert!(infer(b"const value = 1;").is_none());
        assert!(infer(b"const value = {} as cleanupSignal;").is_none());
    }

    #[test]
    fn supported_code_fence_labels_resolve_case_insensitively() {
        let cases: &[(&[u8], &[u8])] = &[
            (b"Zig", b"zig"),
            (b"js", b"ts"),
            (b"JSX", b"ts"),
            (b"javascript", b"ts"),
            (b"TS", b"ts"),
            (b"tsx", b"ts"),
            (b"TypeScript", b"ts"),
            (b"JSON", b"json"),
            (b"sh", b"sh"),
            (b"BASH", b"sh"),
            (b"zsh", b"sh"),
            (b"Shell", b"sh"),
        ];
        for &(label, profile) in cases {
            assert_eq!(resolve(label).unwrap().label, profile);
        }
        assert!(resolve(b"").is_none());
        assert!(resolve(b"text").is_none());
    }

    #[test]
    fn expanded_code_fence_labels_resolve_through_the_language_registry() {
        let cases: &[(&[u8], &[u8])] = &[
            (b"python", b"python"),
            (b"py", b"python"),
            (b"yaml", b"yaml"),
            (b"yml", b"yaml"),
            (b"toml", b"toml"),
            (b"sql", b"sql"),
            (b"dockerfile", b"dockerfile"),
            (b"rust", b"rust"),
            (b"rs", b"rust"),
            (b"go", b"go"),
            (b"c", b"c"),
            (b"cpp", b"cpp"),
            (b"c++", b"cpp"),
            (b"csharp", b"csharp"),
            (b"cs", b"csharp"),
            (b"java", b"java"),
            (b"kotlin", b"kotlin"),
            (b"php", b"php"),
            (b"ruby", b"ruby"),
            (b"swift", b"swift"),
            (b"powershell", b"powershell"),
            (b"ps1", b"powershell"),
            (b"lua", b"lua"),
            (b"html", b"html"),
            (b"xml", b"xml"),
            (b"css", b"css"),
            (b"hcl", b"hcl"),
            (b"terraform", b"hcl"),
            (b"tf", b"hcl"),
        ];
        for &(label, profile) in cases {
            assert_eq!(resolve(label).unwrap().label, profile);
        }
    }

    #[test]
    fn high_confidence_source_shapes_infer_registered_profiles() {
        let cases: &[(&[u8], &[u8])] = &[
            (b"{\"ready\": true}", b"json"),
            (b"#!/usr/bin/env bash\necho ready", b"sh"),
            (b"def render(value):\n    return value", b"python"),
            (b"SELECT id FROM users", b"sql"),
            (b"FROM alpine:3.20\nRUN echo ready", b"dockerfile"),
            (b"package main\nfunc main() {}", b"go"),
            (b"fn main() { println!(\"ready\"); }", b"rust"),
        ];
        for &(source, profile) in cases {
            assert_eq!(infer(source).unwrap().label, profile);
        }
        assert!(infer(b"const value = 1;").is_none());
        assert!(infer(b"title: ready").is_none());
    }

    #[test]
    fn aliases_do_not_collide_across_profiles() {
        for (index, profile) in PROFILES.iter().enumerate() {
            for alias in profile.aliases {
                for other in &PROFILES[index + 1..] {
                    for other_alias in other.aliases {
                        assert!(!alias.eq_ignore_ascii_case(other_alias));
                    }
                }
            }
        }
    }
}
