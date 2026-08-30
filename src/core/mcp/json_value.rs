//! A minimal JSON value and parser used by the MCP JSON Schema modules.
//!
//! This is the Rust equivalent of the subset of Zig's `std.json.Value` that the
//! translated code relies on. Numbers are always retained as their raw source
//! lexeme (`NumberString`) so downstream code can compare them exactly.

use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_MAP_ID: AtomicU64 = AtomicU64::new(1);

fn next_map_id() -> u64 {
    NEXT_MAP_ID.fetch_add(1, Ordering::Relaxed)
}

#[derive(Clone, Debug)]
pub enum Value {
    Null,
    Bool(bool),
    Integer(i64),
    Float(f64),
    NumberString(String),
    String(String),
    Array(Vec<Value>),
    Object(JsonMap),
}

impl Value {
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    pub fn is_bool(&self) -> bool {
        matches!(self, Value::Bool(_))
    }

    pub fn is_object(&self) -> bool {
        matches!(self, Value::Object(_))
    }

    pub fn is_array(&self) -> bool {
        matches!(self, Value::Array(_))
    }

    pub fn is_string(&self) -> bool {
        matches!(self, Value::String(_))
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(value) => Some(*value),
            _ => None,
        }
    }

    pub fn as_object(&self) -> Option<&JsonMap> {
        match self {
            Value::Object(map) => Some(map),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[Value]> {
        match self {
            Value::Array(items) => Some(items),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(value) => Some(value),
            _ => None,
        }
    }
}

/// Insertion-ordered object map. `id` is a stable identity used in place of
/// Zig's `ObjectMap.keys().ptr` identity checks.
#[derive(Clone, Debug)]
pub struct JsonMap {
    entries: Vec<(String, Value)>,
    id: u64,
}

impl JsonMap {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            id: next_map_id(),
        }
    }

    pub fn from_entries(entries: Vec<(String, Value)>) -> Self {
        Self {
            entries,
            id: next_map_id(),
        }
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.entries
            .iter()
            .find(|(candidate, _)| candidate == key)
            .map(|(_, value)| value)
    }

    pub fn contains(&self, key: &str) -> bool {
        self.entries.iter().any(|(candidate, _)| candidate == key)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &Value)> {
        self.entries
            .iter()
            .map(|(key, value)| (key.as_str(), value))
    }

    pub fn id(&self) -> u64 {
        self.id
    }
}

impl Default for JsonMap {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseError {
    InvalidJson,
}

pub fn parse_json(input: &str) -> Result<Value, ParseError> {
    let mut parser = Parser {
        bytes: input.as_bytes(),
        index: 0,
    };
    let value = parser.parse_value()?;
    parser.skip_ws();
    if parser.index != parser.bytes.len() {
        return Err(ParseError::InvalidJson);
    }
    Ok(value)
}

struct Parser<'a> {
    bytes: &'a [u8],
    index: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.index).copied()
    }

    fn next(&mut self) -> Option<u8> {
        let byte = self.peek()?;
        self.index += 1;
        Some(byte)
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.index += 1;
        }
    }

    fn parse_value(&mut self) -> Result<Value, ParseError> {
        self.skip_ws();
        match self.peek() {
            Some(b'{') => self.parse_object(),
            Some(b'[') => self.parse_array(),
            Some(b'"') => Ok(Value::String(self.parse_string()?)),
            Some(b't') => self.parse_literal(b"true", Value::Bool(true)),
            Some(b'f') => self.parse_literal(b"false", Value::Bool(false)),
            Some(b'n') => self.parse_literal(b"null", Value::Null),
            Some(b'-') | Some(b'0'..=b'9') => {
                let lexeme = self.parse_number()?;
                Ok(Value::NumberString(lexeme))
            }
            _ => Err(ParseError::InvalidJson),
        }
    }

    fn parse_literal(&mut self, literal: &[u8], value: Value) -> Result<Value, ParseError> {
        if self.bytes[self.index..].starts_with(literal) {
            self.index += literal.len();
            Ok(value)
        } else {
            Err(ParseError::InvalidJson)
        }
    }

    fn parse_object(&mut self) -> Result<Value, ParseError> {
        self.index += 1; // consume '{'
        let mut entries = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.index += 1;
            return Ok(Value::Object(JsonMap::from_entries(entries)));
        }
        loop {
            self.skip_ws();
            if self.peek() != Some(b'"') {
                return Err(ParseError::InvalidJson);
            }
            let key = self.parse_string()?;
            self.skip_ws();
            if self.peek() != Some(b':') {
                return Err(ParseError::InvalidJson);
            }
            self.index += 1;
            let value = self.parse_value()?;
            entries.push((key, value));
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.index += 1;
                }
                Some(b'}') => {
                    self.index += 1;
                    break;
                }
                _ => return Err(ParseError::InvalidJson),
            }
        }
        Ok(Value::Object(JsonMap::from_entries(entries)))
    }

    fn parse_array(&mut self) -> Result<Value, ParseError> {
        self.index += 1; // consume '['
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.index += 1;
            return Ok(Value::Array(items));
        }
        loop {
            let value = self.parse_value()?;
            items.push(value);
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.index += 1;
                }
                Some(b']') => {
                    self.index += 1;
                    break;
                }
                _ => return Err(ParseError::InvalidJson),
            }
        }
        Ok(Value::Array(items))
    }

    fn parse_string(&mut self) -> Result<String, ParseError> {
        self.index += 1; // consume '"'
        let mut out = Vec::new();
        loop {
            match self.peek() {
                None => return Err(ParseError::InvalidJson),
                Some(b'"') => {
                    self.index += 1;
                    break;
                }
                Some(b'\\') => {
                    self.index += 1;
                    self.parse_escape(&mut out)?;
                }
                Some(byte) if byte < 0x20 => return Err(ParseError::InvalidJson),
                Some(_) => {
                    out.push(self.bytes[self.index]);
                    self.index += 1;
                }
            }
        }
        String::from_utf8(out).map_err(|_| ParseError::InvalidJson)
    }

    fn parse_escape(&mut self, out: &mut Vec<u8>) -> Result<(), ParseError> {
        let byte = self.next().ok_or(ParseError::InvalidJson)?;
        match byte {
            b'"' => out.push(b'"'),
            b'\\' => out.push(b'\\'),
            b'/' => out.push(b'/'),
            b'b' => out.push(0x08),
            b'f' => out.push(0x0C),
            b'n' => out.push(b'\n'),
            b'r' => out.push(b'\r'),
            b't' => out.push(b'\t'),
            b'u' => {
                let first = self.parse_hex4()?;
                let codepoint = if (0xD800..=0xDBFF).contains(&first) {
                    if self.peek() == Some(b'\\') && self.bytes.get(self.index + 1) == Some(&b'u') {
                        self.index += 2;
                        let second = self.parse_hex4()?;
                        if !(0xDC00..=0xDFFF).contains(&second) {
                            return Err(ParseError::InvalidJson);
                        }
                        0x1_0000 + ((first - 0xD800) << 10) + (second - 0xDC00)
                    } else {
                        return Err(ParseError::InvalidJson);
                    }
                } else if (0xDC00..=0xDFFF).contains(&first) {
                    return Err(ParseError::InvalidJson);
                } else {
                    first
                };
                let ch = char::from_u32(codepoint).ok_or(ParseError::InvalidJson)?;
                let mut buffer = [0u8; 4];
                out.extend_from_slice(ch.encode_utf8(&mut buffer).as_bytes());
            }
            _ => out.push(byte),
        }
        Ok(())
    }

    fn parse_hex4(&mut self) -> Result<u32, ParseError> {
        let mut value = 0u32;
        for _ in 0..4 {
            let byte = self.next().ok_or(ParseError::InvalidJson)?;
            let digit = match byte {
                b'0'..=b'9' => (byte - b'0') as u32,
                b'a'..=b'f' => (byte - b'a' + 10) as u32,
                b'A'..=b'F' => (byte - b'A' + 10) as u32,
                _ => return Err(ParseError::InvalidJson),
            };
            value = (value << 4) | digit;
        }
        Ok(value)
    }

    fn parse_number(&mut self) -> Result<String, ParseError> {
        let start = self.index;
        if self.peek() == Some(b'-') {
            self.index += 1;
        }
        match self.peek() {
            Some(b'0') => {
                self.index += 1;
            }
            Some(b'1'..=b'9') => {
                self.index += 1;
                while matches!(self.peek(), Some(b'0'..=b'9')) {
                    self.index += 1;
                }
            }
            _ => return Err(ParseError::InvalidJson),
        }
        if self.peek() == Some(b'.') {
            self.index += 1;
            let fraction_start = self.index;
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.index += 1;
            }
            if self.index == fraction_start {
                return Err(ParseError::InvalidJson);
            }
        }
        if matches!(self.peek(), Some(b'e') | Some(b'E')) {
            self.index += 1;
            if matches!(self.peek(), Some(b'+') | Some(b'-')) {
                self.index += 1;
            }
            let exponent_start = self.index;
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.index += 1;
            }
            if self.index == exponent_start {
                return Err(ParseError::InvalidJson);
            }
        }
        Ok(String::from_utf8_lossy(&self.bytes[start..self.index]).into_owned())
    }
}
