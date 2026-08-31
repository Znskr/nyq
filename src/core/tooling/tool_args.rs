//! Tool-argument JSON readers (ported 1:1 from
//! zig-reference/src/core/tooling/tool_args.zig).

use crate::core::mcp::json_value::{self, JsonMap, Value};
use crate::core::shared::lexical_relevance::{self, PreparedQuery};

#[derive(Debug)]
pub struct OwnedSearchQueryInput {
    pub query: Vec<u8>,
}

impl OwnedSearchQueryInput {
    pub fn prepared(&self) -> PreparedQuery<'_> {
        lexical_relevance::prepare(&self.query).expect("query already validated")
    }

    pub fn deinit(&mut self) {
        self.query.clear();
    }
}

#[derive(Debug)]
pub enum OwnedSearchQueryDecode {
    Failure(Vec<u8>),
    Input(Box<OwnedSearchQueryInput>),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    OutOfMemory,
    InvalidToolArguments,
}

pub fn decode_owned_search_query(
    args_json: &str,
    tool_name: &[u8],
) -> Result<OwnedSearchQueryDecode, Error> {
    let parsed = match json_value::parse_json(args_json) {
        Ok(value) => value,
        Err(_) => {
            return Ok(OwnedSearchQueryDecode::Failure(prefixed_failure(
                tool_name,
                b" arguments must be valid JSON",
            )?));
        }
    };

    let object = match parsed {
        Value::Object(object) => object,
        _ => {
            return Ok(OwnedSearchQueryDecode::Failure(prefixed_failure(
                tool_name,
                b" arguments must be an object",
            )?));
        }
    };
    let query_value = match object.get("query") {
        Some(value) => value,
        None => {
            return Ok(OwnedSearchQueryDecode::Failure(prefixed_failure(
                tool_name,
                b" field \"query\" is required",
            )?));
        }
    };
    let query_string = match query_value {
        Value::String(value) => value,
        _ => {
            return Ok(OwnedSearchQueryDecode::Failure(prefixed_failure(
                tool_name,
                b" field \"query\" must be a string",
            )?));
        }
    };
    if query_string.len() > lexical_relevance::max_query_bytes {
        return Ok(OwnedSearchQueryDecode::Failure(prefixed_failure(
            tool_name,
            b" query must not exceed 4096 bytes",
        )?));
    }

    let query = query_string.as_bytes().to_vec();
    match lexical_relevance::prepare(&query) {
        Ok(_) => {}
        Err(lexical_relevance::PrepareError::QueryTooLong) => unreachable!(),
        Err(lexical_relevance::PrepareError::TooManyTokens) => {
            let body = prefixed_failure(tool_name, b" query must not exceed 64 tokens")?;
            return Ok(OwnedSearchQueryDecode::Failure(body));
        }
    }
    Ok(OwnedSearchQueryDecode::Input(Box::new(OwnedSearchQueryInput {
        query,
    })))
}

pub fn destroy_owned_search_query_input(input: Box<OwnedSearchQueryInput>) {
    drop(input);
}

fn prefixed_failure(prefix: &[u8], suffix: &[u8]) -> Result<Vec<u8>, Error> {
    let len = prefix
        .len()
        .checked_add(suffix.len())
        .ok_or(Error::OutOfMemory)?;
    let mut message = Vec::with_capacity(len);
    message.extend_from_slice(prefix);
    message.extend_from_slice(suffix);
    Ok(message)
}

pub fn parse_tool_args_object(args_json: &str) -> Result<JsonMap, Error> {
    let parsed = json_value::parse_json(args_json).map_err(|_| Error::InvalidToolArguments)?;
    match parsed {
        Value::Object(object) => Ok(object),
        _ => Err(Error::InvalidToolArguments),
    }
}

pub fn required_string_arg<'a>(args: &'a JsonMap, key: &str) -> Result<&'a str, Error> {
    let value = args.get(key).ok_or(Error::InvalidToolArguments)?;
    match value {
        Value::String(text) => Ok(text),
        _ => Err(Error::InvalidToolArguments),
    }
}

pub fn optional_string_arg<'a>(args: &'a JsonMap, key: &str) -> Option<&'a str> {
    match args.get(key) {
        Some(Value::String(text)) => Some(text),
        _ => None,
    }
}

/// Tool schemas that require every field tell the model to send null for the
/// fields its selected action does not use. Models routinely serialize that null
/// as the literal text "null", so readers treat it as the absence it expresses.
pub fn is_null_placeholder_text(text: &str) -> bool {
    text.trim_matches(|c: char| c.is_ascii_whitespace())
        .eq_ignore_ascii_case("null")
}

/// Reads an optional string argument from a schema whose unused fields arrive as
/// nulls, so textual null placeholders read as absent instead of as a value.
pub fn nullable_placeholder_string_arg<'a>(args: &'a JsonMap, key: &str) -> Option<&'a str> {
    let text = optional_string_arg(args, key)?;
    if is_null_placeholder_text(text) {
        None
    } else {
        Some(text)
    }
}

pub fn optional_bool_arg(args: &JsonMap, key: &str) -> Option<bool> {
    match args.get(key) {
        Some(Value::Bool(value)) => Some(*value),
        _ => None,
    }
}

pub fn optional_int_arg(args: &JsonMap, key: &str) -> Option<i64> {
    match args.get(key) {
        Some(Value::Integer(value)) => Some(*value),
        Some(Value::NumberString(lexeme)) => parse_json_integer(lexeme),
        _ => None,
    }
}

fn parse_json_integer(lexeme: &str) -> Option<i64> {
    if lexeme.contains('.') || lexeme.contains('e') || lexeme.contains('E') {
        return None;
    }
    lexeme.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_tool_args_object_parses_object_roots() {
        let args = parse_tool_args_object("{\"path\":\"src/main.zig\"}").unwrap();
        assert_eq!(args.get("path").unwrap().as_str().unwrap(), "src/main.zig");
    }

    #[test]
    fn parse_tool_args_object_rejects_non_object_roots() {
        assert_eq!(
            parse_tool_args_object("[]").unwrap_err(),
            Error::InvalidToolArguments
        );
    }

    #[test]
    fn required_string_arg_rejects_missing_and_wrong_type_values() {
        let args = parse_tool_args_object("{\"path\":\"src/main.zig\",\"count\":3}").unwrap();
        assert_eq!(required_string_arg(&args, "path").unwrap(), "src/main.zig");
        assert_eq!(
            required_string_arg(&args, "missing").unwrap_err(),
            Error::InvalidToolArguments
        );
        assert_eq!(
            required_string_arg(&args, "count").unwrap_err(),
            Error::InvalidToolArguments
        );
    }

    #[test]
    fn optional_typed_args_return_payloads_only_for_matching_tags() {
        let args =
            parse_tool_args_object("{\"name\":\"fx\",\"enabled\":true,\"count\":3,\"other\":1.25}")
                .unwrap();

        assert_eq!(optional_string_arg(&args, "name").unwrap(), "fx");
        assert!(optional_string_arg(&args, "missing").is_none());
        assert!(optional_string_arg(&args, "enabled").is_none());

        assert_eq!(optional_bool_arg(&args, "enabled").unwrap(), true);
        assert!(optional_bool_arg(&args, "missing").is_none());
        assert!(optional_bool_arg(&args, "name").is_none());

        assert_eq!(optional_int_arg(&args, "count").unwrap(), 3);
        assert!(optional_int_arg(&args, "missing").is_none());
        assert!(optional_int_arg(&args, "other").is_none());
    }

    #[test]
    fn null_placeholder_reads_treat_textual_nulls_as_absent() {
        let args = parse_tool_args_object(
            "{\"cwd\":\"null\",\"profile\":\" NULL \",\"shell\":null,\"command\":\"echo null\",\"dir\":\"nullify\"}",
        )
        .unwrap();

        assert!(nullable_placeholder_string_arg(&args, "cwd").is_none());
        assert!(nullable_placeholder_string_arg(&args, "profile").is_none());
        assert!(nullable_placeholder_string_arg(&args, "shell").is_none());
        assert!(nullable_placeholder_string_arg(&args, "missing").is_none());
        assert_eq!(
            nullable_placeholder_string_arg(&args, "command").unwrap(),
            "echo null"
        );
        assert_eq!(
            nullable_placeholder_string_arg(&args, "dir").unwrap(),
            "nullify"
        );
        assert_eq!(optional_string_arg(&args, "cwd").unwrap(), "null");
    }
}
