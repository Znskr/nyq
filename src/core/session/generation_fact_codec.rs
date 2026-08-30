//! JSON codec for the shared generation-fact record shape.
//!
//! Mirrors `zig-reference/src/core/session/generation_fact_codec.zig`.

use crate::core::mcp::json_value::Value;
use super::usage_report::{self, GenerationFact, InvalidGenerationFact};

/// Writes a generation fact as a JSON object.
pub fn write<W: std::io::Write>(writer: &mut W, fact: &GenerationFact) -> std::io::Result<()> {
    writer.write_all(b"{\"id\":")?;
    write_json_string(writer, &fact.id)?;
    write!(
        writer,
        ",\"created_at_ms\":{},\"model\":",
        fact.created_at_ms
    )?;
    write_json_string(writer, &fact.model)?;
    write!(
        writer,
        ",\"input_tokens\":{},\"output_tokens\":{},\"cache_read_tokens\":{},\"cache_write_tokens\":{},\"reasoning_tokens\":",
        fact.input_tokens, fact.output_tokens, fact.cache_read_tokens, fact.cache_write_tokens,
    )?;
    match fact.reasoning_tokens {
        Some(reasoning) => write!(writer, "{reasoning}")?,
        None => writer.write_all(b"null")?,
    }
    write!(
        writer,
        ",\"billable_web_search_calls\":{},\"total_cost\":{}}}",
        fact.billable_web_search_calls, fact.total_cost
    )?;
    Ok(())
}

/// Parses a generation fact from an already-decoded JSON value.
pub fn parse(value: &Value) -> Result<GenerationFact, InvalidGenerationFact> {
    let object = value.as_object().ok_or(InvalidGenerationFact)?;
    if object.len() != 9 && object.len() != 10 {
        return Err(InvalidGenerationFact);
    }
    let id_value = object.get("id").ok_or(InvalidGenerationFact)?;
    let model_value = object.get("model").ok_or(InvalidGenerationFact)?;
    let reasoning_value = object.get("reasoning_tokens").ok_or(InvalidGenerationFact)?;
    let id = id_value.as_str().ok_or(InvalidGenerationFact)?;
    let model = model_value.as_str().ok_or(InvalidGenerationFact)?;

    let fact = GenerationFact {
        id: id.as_bytes().to_vec(),
        created_at_ms: parse_i64(object.get("created_at_ms"))?,
        model: model.as_bytes().to_vec(),
        input_tokens: parse_u64(object.get("input_tokens"))?,
        output_tokens: parse_u64(object.get("output_tokens"))?,
        cache_read_tokens: parse_u64(object.get("cache_read_tokens"))?,
        cache_write_tokens: parse_u64(object.get("cache_write_tokens"))?,
        reasoning_tokens: if reasoning_value.is_null() {
            None
        } else {
            Some(parse_u64(Some(reasoning_value))?)
        },
        billable_web_search_calls: match object.get("billable_web_search_calls") {
            Some(field) => parse_u64(Some(field))?,
            None => 0,
        },
        total_cost: parse_cost(object.get("total_cost"))?,
    };
    usage_report::validate_fact(&fact).map_err(|_| InvalidGenerationFact)?;
    Ok(fact)
}

fn parse_u64(value: Option<&Value>) -> Result<u64, InvalidGenerationFact> {
    let actual = value.ok_or(InvalidGenerationFact)?;
    match actual {
        Value::Integer(number) if *number >= 0 => {
            u64::try_from(*number).map_err(|_| InvalidGenerationFact)
        }
        Value::NumberString(text) => text.parse::<u64>().map_err(|_| InvalidGenerationFact),
        _ => Err(InvalidGenerationFact),
    }
}

fn parse_i64(value: Option<&Value>) -> Result<i64, InvalidGenerationFact> {
    let number = parse_u64(value)?;
    i64::try_from(number).map_err(|_| InvalidGenerationFact)
}

fn parse_cost(value: Option<&Value>) -> Result<f64, InvalidGenerationFact> {
    let actual = value.ok_or(InvalidGenerationFact)?;
    let number = match actual {
        Value::Integer(integer) => *integer as f64,
        Value::Float(float) => *float,
        Value::NumberString(text) => text.parse::<f64>().map_err(|_| InvalidGenerationFact)?,
        _ => return Err(InvalidGenerationFact),
    };
    if !number.is_finite() || number < 0.0 {
        return Err(InvalidGenerationFact);
    }
    Ok(number)
}

fn write_json_string<W: std::io::Write>(writer: &mut W, bytes: &[u8]) -> std::io::Result<()> {
    writer.write_all(b"\"")?;
    for &byte in bytes {
        match byte {
            b'"' => writer.write_all(b"\\\"")?,
            b'\\' => writer.write_all(b"\\\\")?,
            b'\n' => writer.write_all(b"\\n")?,
            b'\r' => writer.write_all(b"\\r")?,
            b'\t' => writer.write_all(b"\\t")?,
            0x08 => writer.write_all(b"\\b")?,
            0x0c => writer.write_all(b"\\f")?,
            0x00..=0x1f => write!(writer, "\\u{byte:04x}")?,
            _ => writer.write_all(&[byte])?,
        }
    }
    writer.write_all(b"\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codec_round_trips_the_shared_generation_fact_shape() {
        let expected = GenerationFact {
            id: b"gen_01ARZ3NDEKTSV4RRFFQ69G5FAV".to_vec(),
            created_at_ms: 42,
            model: b"provider/model".to_vec(),
            input_tokens: 8,
            output_tokens: 3,
            cache_read_tokens: 2,
            cache_write_tokens: 1,
            reasoning_tokens: None,
            billable_web_search_calls: 4,
            total_cost: 0.25,
        };

        let mut encoded = Vec::new();
        write(&mut encoded, &expected).unwrap();

        let value = crate::core::mcp::json_value::parse_json(std::str::from_utf8(&encoded).unwrap()).unwrap();
        let decoded = parse(&value).unwrap();

        assert!(expected.eql(&decoded));
        assert!(!encoded.windows(b"request_count".len()).any(|w| w == b"request_count"));
    }
}
