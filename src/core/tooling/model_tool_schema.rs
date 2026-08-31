//! Model-facing tool JSON Schema (ported 1:1 from
//! zig-reference/src/core/tooling/model_tool_schema.zig).

use std::io::{self, Write};

pub const description_max_bytes: usize = 1024;
pub const truncation_marker: &[u8] = b"... [truncated]";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JsonType {
    String,
    Integer,
    Boolean,
    Object,
    Array,
}

impl JsonType {
    fn tag_name(self) -> &'static [u8] {
        match self {
            JsonType::String => b"string",
            JsonType::Integer => b"integer",
            JsonType::Boolean => b"boolean",
            JsonType::Object => b"object",
            JsonType::Array => b"array",
        }
    }
}

const no_u32_bound: u32 = u32::MAX;
const no_u64_bound: u64 = u64::MAX;

#[derive(Clone, Copy, Debug)]
pub enum PropertyShape<'a> {
    EnumValues(&'a [&'a [u8]]),
    Object(&'a ObjectSchema<'a>),
    ArrayValues {
        json_type: JsonType,
        enum_values: &'a [&'a [u8]],
    },
    ArrayObjects(&'a ObjectSchema<'a>),
}

#[derive(Clone, Copy, Debug)]
pub struct PropertyBounds {
    pub min_length: u32,
    pub max_length: u32,
    pub minimum: u64,
    pub maximum: u64,
    pub min_items: u32,
    pub max_items: u32,
}

impl Default for PropertyBounds {
    fn default() -> Self {
        Self {
            min_length: no_u32_bound,
            max_length: no_u32_bound,
            minimum: no_u64_bound,
            maximum: no_u64_bound,
            min_items: no_u32_bound,
            max_items: no_u32_bound,
        }
    }
}

const no_property_bounds: PropertyBounds = PropertyBounds {
    min_length: no_u32_bound,
    max_length: no_u32_bound,
    minimum: no_u64_bound,
    maximum: no_u64_bound,
    min_items: no_u32_bound,
    max_items: no_u32_bound,
};

#[derive(Clone, Copy, Debug)]
pub struct NullableMetadata<'a> {
    pub description: &'a [u8],
}

impl Default for NullableMetadata<'static> {
    fn default() -> Self {
        Self { description: b"" }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Property<'a> {
    pub name: &'a [u8],
    pub description: &'a [u8],
    pub shape: Option<&'a PropertyShape<'a>>,
    pub bounds: Option<&'a PropertyBounds>,
    pub nullable: Option<&'a NullableMetadata<'a>>,
    pub json_type: JsonType,
}

impl<'a> Property<'a> {
    pub fn new(name: &'a [u8], json_type: JsonType) -> Self {
        Self {
            name,
            description: b"",
            shape: None,
            bounds: None,
            nullable: None,
            json_type,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct ObjectSchema<'a> {
    pub properties: &'a [Property<'a>],
    pub required: &'a [&'a [u8]],
    pub additional_properties: Option<bool>,
    pub min_properties: u32,
    pub max_properties: u32,
    pub one_of: &'a [ObjectSchema<'a>],
}

impl Default for ObjectSchema<'static> {
    fn default() -> Self {
        Self {
            properties: &[],
            required: &[],
            additional_properties: None,
            min_properties: no_u32_bound,
            max_properties: no_u32_bound,
            one_of: &[],
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct FunctionSchema<'a> {
    pub name: &'a [u8],
    pub description: &'a [u8],
    pub input_schema: ObjectSchema<'a>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    InvalidObjectSchema,
}

pub fn is_single_required_object_union_field(schema: ObjectSchema<'_>, field_name: &[u8]) -> bool {
    if schema.properties.len() != 1
        || schema.required.len() != 1
        || schema.additional_properties != Some(false)
        || schema.properties[0].name != field_name
        || schema.required[0] != field_name
    {
        return false;
    }
    let Some(shape) = schema.properties[0].shape else {
        return false;
    };
    match shape {
        PropertyShape::Object(object) => !object.one_of.is_empty(),
        _ => false,
    }
}

fn capped_description_alloc(text: &[u8]) -> Vec<u8> {
    if text.len() <= description_max_bytes {
        return text.to_vec();
    }
    let prefix_len = description_max_bytes - truncation_marker.len();
    let mut out = Vec::with_capacity(description_max_bytes);
    out.extend_from_slice(&text[..prefix_len]);
    out.extend_from_slice(truncation_marker);
    out
}

pub fn write_capped_description_json_string<W: Write>(
    writer: &mut W,
    text: &[u8],
) -> io::Result<()> {
    let capped_description = capped_description_alloc(text);
    write_json_string(writer, &capped_description)
}

fn write_function_schema_open<W: Write>(
    writer: &mut W,
    name: &[u8],
    description: &[u8],
) -> io::Result<()> {
    writer.write_all(br#"{"type":"function","name":"#)?;
    write_json_string(writer, name)?;
    writer.write_all(br#","description":"#)?;
    write_json_string(writer, description)?;
    writer.write_all(br#","inputSchema":"#)?;
    Ok(())
}

pub fn write_builtin_function_schema<W: Write>(
    writer: &mut W,
    schema: FunctionSchema<'_>,
) -> Result<(), SchemaWriteError> {
    let description = capped_description_alloc(schema.description);
    write_function_schema_open(writer, schema.name, &description)?;
    write_object_schema(writer, schema.input_schema)?;
    writer.write_all(b"}")?;
    Ok(())
}

pub fn builtin_function_schema_json_alloc(
    schema: FunctionSchema<'_>,
) -> Result<Vec<u8>, SchemaWriteError> {
    let mut out = Vec::new();
    write_builtin_function_schema(&mut out, schema)?;
    Ok(out)
}

/// Envelope for a dynamic (MCP) tool whose input schema is already rendered
/// JSON. Caller owns the returned slice.
pub fn dynamic_function_schema_json_alloc(
    name: &[u8],
    description: &[u8],
    input_schema_json: &[u8],
) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    write_function_schema_open(&mut out, name, description)?;
    out.extend_from_slice(input_schema_json);
    out.push(b'}');
    Ok(out)
}

#[derive(Debug)]
pub enum SchemaWriteError {
    InvalidObjectSchema,
    Io(io::Error),
}

impl From<io::Error> for SchemaWriteError {
    fn from(error: io::Error) -> Self {
        SchemaWriteError::Io(error)
    }
}

impl PartialEq for SchemaWriteError {
    fn eq(&self, other: &Self) -> bool {
        matches!(
            (self, other),
            (
                SchemaWriteError::InvalidObjectSchema,
                SchemaWriteError::InvalidObjectSchema
            )
        )
    }
}

pub fn write_object_schema<W: Write>(
    writer: &mut W,
    schema: ObjectSchema<'_>,
) -> Result<(), SchemaWriteError> {
    if !schema.one_of.is_empty() {
        if !schema.properties.is_empty()
            || !schema.required.is_empty()
            || schema.additional_properties.is_some()
            || schema.min_properties != no_u32_bound
            || schema.max_properties != no_u32_bound
        {
            return Err(SchemaWriteError::InvalidObjectSchema);
        }
        writer.write_all(br#"{"oneOf":["#)?;
        for (index, alternative) in schema.one_of.iter().enumerate() {
            if index > 0 {
                writer.write_all(b",")?;
            }
            write_object_schema(writer, *alternative)?;
        }
        writer.write_all(b"]}")?;
        return Ok(());
    }

    writer.write_all(br#"{"type":"object","properties":{"#)?;
    for (index, property) in schema.properties.iter().enumerate() {
        if index > 0 {
            writer.write_all(b",")?;
        }
        write_json_string(writer, property.name)?;
        writer.write_all(b":")?;
        write_property_schema(writer, *property)?;
    }
    writer.write_all(b"}")?;
    if let Some(value) = schema.additional_properties {
        writer.write_all(br#","additionalProperties":"#)?;
        writer.write_all(if value { b"true" } else { b"false" })?;
    }
    if schema.min_properties != no_u32_bound {
        write!(writer, ",\"minProperties\":{}", schema.min_properties)?;
    }
    if schema.max_properties != no_u32_bound {
        write!(writer, ",\"maxProperties\":{}", schema.max_properties)?;
    }
    if !schema.required.is_empty() {
        writer.write_all(br#","required":["#)?;
        for (index, name) in schema.required.iter().enumerate() {
            if index > 0 {
                writer.write_all(b",")?;
            }
            write_json_string(writer, name)?;
        }
        writer.write_all(b"]")?;
    }
    writer.write_all(b"}")?;
    Ok(())
}

fn write_property_schema<W: Write>(
    writer: &mut W,
    property: Property<'_>,
) -> Result<(), SchemaWriteError> {
    if let Some(nullable) = property.nullable {
        let mut concrete = property;
        concrete.nullable = None;
        writer.write_all(br#"{"anyOf":["#)?;
        write_property_schema(writer, concrete)?;
        writer.write_all(br#",{"type":"null"}]"#)?;
        if !nullable.description.is_empty() {
            writer.write_all(br#","description":"#)?;
            write_capped_description_json_string(writer, nullable.description)?;
        }
        writer.write_all(b"}")?;
        return Ok(());
    }
    if let Some(shape) = property.shape {
        if let PropertyShape::Object(object_schema) = shape {
            write_object_schema(writer, **object_schema)?;
            return Ok(());
        }
    }
    writer.write_all(br#"{"type":"#)?;
    write_json_string(writer, property.json_type.tag_name())?;
    if let Some(shape) = property.shape {
        if let PropertyShape::EnumValues(values) = shape {
            writer.write_all(br#","enum":["#)?;
            for (index, value) in values.iter().enumerate() {
                if index > 0 {
                    writer.write_all(b",")?;
                }
                write_json_string(writer, value)?;
            }
            writer.write_all(b"]")?;
        }
    }
    let bounds = property.bounds.copied().unwrap_or(no_property_bounds);
    if bounds.min_length != no_u32_bound {
        write!(writer, ",\"minLength\":{}", bounds.min_length)?;
    }
    if bounds.max_length != no_u32_bound {
        write!(writer, ",\"maxLength\":{}", bounds.max_length)?;
    }
    if bounds.minimum != no_u64_bound {
        write!(writer, ",\"minimum\":{}", bounds.minimum)?;
    }
    if bounds.maximum != no_u64_bound {
        write!(writer, ",\"maximum\":{}", bounds.maximum)?;
    }
    if !property.description.is_empty() {
        writer.write_all(br#","description":"#)?;
        write_capped_description_json_string(writer, property.description)?;
    }
    if bounds.min_items != no_u32_bound {
        write!(writer, ",\"minItems\":{}", bounds.min_items)?;
    }
    if bounds.max_items != no_u32_bound {
        write!(writer, ",\"maxItems\":{}", bounds.max_items)?;
    }
    if let Some(shape) = property.shape {
        match shape {
            PropertyShape::ArrayValues {
                json_type,
                enum_values,
            } => {
                writer.write_all(br#","items":{"type":"#)?;
                write_json_string(writer, json_type.tag_name())?;
                if !enum_values.is_empty() {
                    writer.write_all(br#","enum":["#)?;
                    for (index, value) in enum_values.iter().enumerate() {
                        if index > 0 {
                            writer.write_all(b",")?;
                        }
                        write_json_string(writer, value)?;
                    }
                    writer.write_all(b"]")?;
                }
                writer.write_all(b"}")?;
            }
            PropertyShape::ArrayObjects(items) => {
                writer.write_all(br#","items":"#)?;
                write_object_schema(writer, **items)?;
            }
            _ => {}
        }
    }
    writer.write_all(b"}")?;
    Ok(())
}

fn write_json_string<W: Write>(writer: &mut W, bytes: &[u8]) -> io::Result<()> {
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

fn json_integer(value: &crate::core::mcp::json_value::Value) -> Option<i64> {
    match value {
        crate::core::mcp::json_value::Value::Integer(n) => Some(*n),
        crate::core::mcp::json_value::Value::NumberString(lexeme) => {
            if lexeme.contains('.') || lexeme.contains('e') || lexeme.contains('E') {
                None
            } else {
                lexeme.parse().ok()
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::mcp::json_value::{parse_json, Value};

    #[test]
    fn static_property_representation_stays_within_the_measured_size_budget() {
        assert!(std::mem::size_of::<Property<'static>>() <= 64);
    }

    #[test]
    fn nullable_properties_preserve_concrete_constraints_and_add_one_null_branch() {
        let object_value_schema = ObjectSchema {
            properties: &[Property::new(b"kind", JsonType::String)],
            required: &[b"kind"],
            additional_properties: Some(false),
            min_properties: no_u32_bound,
            max_properties: no_u32_bound,
            one_of: &[],
        };
        let choice_nullable = NullableMetadata {
            description: b"Concrete choice. Set null when unused.",
        };
        let choice_shape = PropertyShape::EnumValues(&[b"one", b"two"]);
        let config_nullable = NullableMetadata {
            description: b"Set null when unused.",
        };
        let config_shape = PropertyShape::Object(&object_value_schema);
        let mut choice = Property::new(b"choice", JsonType::String);
        choice.description = b"Concrete choice.";
        choice.nullable = Some(&choice_nullable);
        choice.shape = Some(&choice_shape);
        let mut config = Property::new(b"config", JsonType::Object);
        config.nullable = Some(&config_nullable);
        config.shape = Some(&config_shape);
        let schema = FunctionSchema {
            name: b"nullable",
            description: b"nullable",
            input_schema: ObjectSchema {
                properties: &[choice, config],
                required: &[b"choice", b"config"],
                additional_properties: None,
                min_properties: no_u32_bound,
                max_properties: no_u32_bound,
                one_of: &[],
            },
        };

        let json = builtin_function_schema_json_alloc(schema).unwrap();
        let parsed = parse_json(std::str::from_utf8(&json).unwrap()).unwrap();
        let properties = parsed
            .as_object()
            .unwrap()
            .get("inputSchema")
            .unwrap()
            .as_object()
            .unwrap()
            .get("properties")
            .unwrap()
            .as_object()
            .unwrap();
        let choice = properties.get("choice").unwrap().as_object().unwrap();
        assert_eq!(
            choice.get("description").unwrap().as_str().unwrap(),
            "Concrete choice. Set null when unused."
        );
        let choice_alternatives = choice.get("anyOf").unwrap().as_array().unwrap();
        assert_eq!(choice_alternatives.len(), 2);
        assert_eq!(
            choice_alternatives[0]
                .as_object()
                .unwrap()
                .get("type")
                .unwrap()
                .as_str()
                .unwrap(),
            "string"
        );
        assert_eq!(
            choice_alternatives[0]
                .as_object()
                .unwrap()
                .get("enum")
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            choice_alternatives[1]
                .as_object()
                .unwrap()
                .get("type")
                .unwrap()
                .as_str()
                .unwrap(),
            "null"
        );

        let config_alternatives = properties
            .get("config")
            .unwrap()
            .as_object()
            .unwrap()
            .get("anyOf")
            .unwrap()
            .as_array()
            .unwrap();
        assert_eq!(
            config_alternatives[0]
                .as_object()
                .unwrap()
                .get("type")
                .unwrap()
                .as_str()
                .unwrap(),
            "object"
        );
        assert_eq!(
            config_alternatives[0]
                .as_object()
                .unwrap()
                .get("additionalProperties")
                .unwrap()
                .as_bool()
                .unwrap(),
            false
        );
        assert_eq!(
            config_alternatives[1]
                .as_object()
                .unwrap()
                .get("type")
                .unwrap()
                .as_str()
                .unwrap(),
            "null"
        );
    }

    #[test]
    fn nested_object_schema_serializes_exact_property_bounds() {
        let create_schema = ObjectSchema {
            properties: &[Property::new(b"name", JsonType::String)],
            required: &[b"name"],
            additional_properties: Some(false),
            min_properties: no_u32_bound,
            max_properties: no_u32_bound,
            one_of: &[],
        };
        let create_shape = PropertyShape::Object(&create_schema);
        let mut create = Property::new(b"create", JsonType::Object);
        create.shape = Some(&create_shape);
        let nested = ObjectSchema {
            properties: &[create],
            required: &[],
            additional_properties: Some(false),
            min_properties: 1,
            max_properties: 1,
            one_of: &[],
        };
        let schema = FunctionSchema {
            name: b"nested",
            description: b"nested",
            input_schema: nested,
        };

        let json = builtin_function_schema_json_alloc(schema).unwrap();
        let json_text = std::str::from_utf8(&json).unwrap();
        assert!(json_text.contains("\"minProperties\":1"));
        assert!(json_text.contains("\"maxProperties\":1"));
        assert!(json_text.contains("\"create\":{\"type\":\"object\",\"properties\":{"));
        assert!(json_text.contains("\"additionalProperties\":false"));
    }

    #[test]
    fn object_alternatives_serialize_as_exclusive_object_branches() {
        let read_action_shape = PropertyShape::EnumValues(&[b"read"]);
        let mut read_action = Property::new(b"action", JsonType::String);
        read_action.shape = Some(&read_action_shape);
        let list_action_shape = PropertyShape::EnumValues(&[b"list"]);
        let mut list_action = Property::new(b"action", JsonType::String);
        list_action.shape = Some(&list_action_shape);
        let alternatives = [
            ObjectSchema {
                properties: &[read_action, Property::new(b"path", JsonType::String)],
                required: &[b"action", b"path"],
                additional_properties: Some(false),
                min_properties: no_u32_bound,
                max_properties: no_u32_bound,
                one_of: &[],
            },
            ObjectSchema {
                properties: &[list_action],
                required: &[b"action"],
                additional_properties: Some(false),
                min_properties: no_u32_bound,
                max_properties: no_u32_bound,
                one_of: &[],
            },
        ];
        let schema = FunctionSchema {
            name: b"alternative",
            description: b"alternative",
            input_schema: ObjectSchema {
                properties: &[],
                required: &[],
                additional_properties: None,
                min_properties: no_u32_bound,
                max_properties: no_u32_bound,
                one_of: &alternatives,
            },
        };

        let json = builtin_function_schema_json_alloc(schema).unwrap();
        let parsed = parse_json(std::str::from_utf8(&json).unwrap()).unwrap();
        let input_schema = parsed
            .as_object()
            .unwrap()
            .get("inputSchema")
            .unwrap()
            .as_object()
            .unwrap();
        assert!(input_schema.get("type").is_none());
        assert!(input_schema.get("properties").is_none());
        let one_of = input_schema.get("oneOf").unwrap().as_array().unwrap();
        assert_eq!(one_of.len(), 2);
        assert_eq!(
            one_of[0]
                .as_object()
                .unwrap()
                .get("properties")
                .unwrap()
                .as_object()
                .unwrap()
                .get("action")
                .unwrap()
                .as_object()
                .unwrap()
                .get("enum")
                .unwrap()
                .as_array()
                .unwrap()[0]
                .as_str()
                .unwrap(),
            "read"
        );
        assert_eq!(
            one_of[1]
                .as_object()
                .unwrap()
                .get("properties")
                .unwrap()
                .as_object()
                .unwrap()
                .get("action")
                .unwrap()
                .as_object()
                .unwrap()
                .get("enum")
                .unwrap()
                .as_array()
                .unwrap()[0]
                .as_str()
                .unwrap(),
            "list"
        );
        assert_eq!(
            one_of[0]
                .as_object()
                .unwrap()
                .get("additionalProperties")
                .unwrap()
                .as_bool()
                .unwrap(),
            false
        );
        assert_eq!(
            one_of[1]
                .as_object()
                .unwrap()
                .get("additionalProperties")
                .unwrap()
                .as_bool()
                .unwrap(),
            false
        );
    }

    #[test]
    fn object_alternatives_reject_contradictory_ordinary_object_metadata() {
        let schema = FunctionSchema {
            name: b"invalid_alternative",
            description: b"invalid alternative",
            input_schema: ObjectSchema {
                properties: &[Property::new(b"value", JsonType::String)],
                required: &[],
                additional_properties: None,
                min_properties: no_u32_bound,
                max_properties: no_u32_bound,
                one_of: &[ObjectSchema {
                    properties: &[Property::new(b"action", JsonType::String)],
                    required: &[],
                    additional_properties: None,
                    min_properties: no_u32_bound,
                    max_properties: no_u32_bound,
                    one_of: &[],
                }],
            },
        };

        assert_eq!(
            builtin_function_schema_json_alloc(schema).unwrap_err(),
            SchemaWriteError::InvalidObjectSchema
        );
    }

    #[test]
    fn capped_description_alloc_appends_explicit_truncation_marker() {
        let oversized = vec![b'x'; description_max_bytes + 20];
        let capped = capped_description_alloc(&oversized);
        assert_eq!(capped.len(), description_max_bytes);
        assert!(capped.ends_with(truncation_marker));
    }

    #[test]
    fn builtin_function_schema_json_alloc_serializes_strings_and_object_schema() {
        let mut path = Property::new(b"path", JsonType::String);
        path.description = br#"File path "with quotes"."#;
        let schema = FunctionSchema {
            name: b"read_file",
            description: br#"Read "quoted" paths. When to use: test escaping. When NOT to use: unescaped JSON."#,
            input_schema: ObjectSchema {
                properties: &[path],
                required: &[b"path"],
                additional_properties: None,
                min_properties: no_u32_bound,
                max_properties: no_u32_bound,
                one_of: &[],
            },
        };

        let json = builtin_function_schema_json_alloc(schema).unwrap();
        let parsed = parse_json(std::str::from_utf8(&json).unwrap()).unwrap();
        assert_eq!(
            parsed
                .as_object()
                .unwrap()
                .get("name")
                .unwrap()
                .as_str()
                .unwrap(),
            "read_file"
        );
        let json_text = std::str::from_utf8(&json).unwrap();
        assert!(json_text.contains(r#"\"quoted\""#));
        assert!(json_text.contains(r#"\"with quotes\""#));
    }

    #[test]
    fn builtin_function_schema_json_alloc_serializes_every_supported_property_shape() {
        let object_value_schema = ObjectSchema {
            properties: &[Property::new(b"name", JsonType::String)],
            required: &[b"name"],
            additional_properties: Some(false),
            min_properties: no_u32_bound,
            max_properties: no_u32_bound,
            one_of: &[],
        };
        let array_item_schema = ObjectSchema {
            properties: &[Property::new(b"id", JsonType::Integer)],
            required: &[b"id"],
            additional_properties: Some(false),
            min_properties: no_u32_bound,
            max_properties: no_u32_bound,
            one_of: &[],
        };
        let text_shape = PropertyShape::EnumValues(&[b"alpha", b"beta"]);
        let text_bounds = PropertyBounds {
            min_length: 1,
            max_length: 8,
            ..PropertyBounds::default()
        };
        let count_bounds = PropertyBounds {
            minimum: 2,
            maximum: 9,
            ..PropertyBounds::default()
        };
        let config_shape = PropertyShape::Object(&object_value_schema);
        let tags_bounds = PropertyBounds {
            min_items: 1,
            max_items: 3,
            ..PropertyBounds::default()
        };
        let tags_shape = PropertyShape::ArrayValues {
            json_type: JsonType::String,
            enum_values: &[b"red", b"blue"],
        };
        let records_shape = PropertyShape::ArrayObjects(&array_item_schema);

        let mut text = Property::new(b"text", JsonType::String);
        text.description = b"bounded choice";
        text.shape = Some(&text_shape);
        text.bounds = Some(&text_bounds);
        let mut count = Property::new(b"count", JsonType::Integer);
        count.bounds = Some(&count_bounds);
        let enabled = Property::new(b"enabled", JsonType::Boolean);
        let mut config = Property::new(b"config", JsonType::Object);
        config.shape = Some(&config_shape);
        let mut tags = Property::new(b"tags", JsonType::Array);
        tags.bounds = Some(&tags_bounds);
        tags.shape = Some(&tags_shape);
        let mut records = Property::new(b"records", JsonType::Array);
        records.shape = Some(&records_shape);

        let schema = FunctionSchema {
            name: b"schema_matrix",
            description: b"schema matrix",
            input_schema: ObjectSchema {
                properties: &[text, count, enabled, config, tags, records],
                required: &[
                    b"text",
                    b"count",
                    b"enabled",
                    b"config",
                    b"tags",
                    b"records",
                ],
                additional_properties: Some(false),
                min_properties: 6,
                max_properties: 6,
                one_of: &[],
            },
        };

        let json = builtin_function_schema_json_alloc(schema).unwrap();
        let parsed = parse_json(std::str::from_utf8(&json).unwrap()).unwrap();
        let input_schema = parsed
            .as_object()
            .unwrap()
            .get("inputSchema")
            .unwrap()
            .as_object()
            .unwrap();
        assert_eq!(
            input_schema.get("type").unwrap().as_str().unwrap(),
            "object"
        );
        assert_eq!(
            input_schema
                .get("additionalProperties")
                .unwrap()
                .as_bool()
                .unwrap(),
            false
        );
        assert_eq!(json_integer(input_schema.get("minProperties").unwrap()), Some(6));
        assert_eq!(json_integer(input_schema.get("maxProperties").unwrap()), Some(6));
        assert_eq!(
            input_schema
                .get("required")
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            6
        );

        let properties = input_schema.get("properties").unwrap().as_object().unwrap();
        let text_property = properties.get("text").unwrap().as_object().unwrap();
        assert_eq!(
            text_property.get("type").unwrap().as_str().unwrap(),
            "string"
        );
        assert_eq!(
            text_property.get("description").unwrap().as_str().unwrap(),
            "bounded choice"
        );
        assert_eq!(
            text_property
                .get("enum")
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(json_integer(text_property.get("minLength").unwrap()), Some(1));
        assert_eq!(json_integer(text_property.get("maxLength").unwrap()), Some(8));

        let count_property = properties.get("count").unwrap().as_object().unwrap();
        assert_eq!(
            count_property.get("type").unwrap().as_str().unwrap(),
            "integer"
        );
        assert_eq!(json_integer(count_property.get("minimum").unwrap()), Some(2));
        assert_eq!(json_integer(count_property.get("maximum").unwrap()), Some(9));
        assert_eq!(
            properties
                .get("enabled")
                .unwrap()
                .as_object()
                .unwrap()
                .get("type")
                .unwrap()
                .as_str()
                .unwrap(),
            "boolean"
        );

        let config_property = properties.get("config").unwrap().as_object().unwrap();
        assert_eq!(
            config_property.get("type").unwrap().as_str().unwrap(),
            "object"
        );
        assert_eq!(
            config_property
                .get("additionalProperties")
                .unwrap()
                .as_bool()
                .unwrap(),
            false
        );
        assert_eq!(
            config_property
                .get("properties")
                .unwrap()
                .as_object()
                .unwrap()
                .get("name")
                .unwrap()
                .as_object()
                .unwrap()
                .get("type")
                .unwrap()
                .as_str()
                .unwrap(),
            "string"
        );

        let tags_property = properties.get("tags").unwrap().as_object().unwrap();
        assert_eq!(
            tags_property.get("type").unwrap().as_str().unwrap(),
            "array"
        );
        assert_eq!(json_integer(tags_property.get("minItems").unwrap()), Some(1));
        assert_eq!(json_integer(tags_property.get("maxItems").unwrap()), Some(3));
        assert_eq!(
            tags_property
                .get("items")
                .unwrap()
                .as_object()
                .unwrap()
                .get("type")
                .unwrap()
                .as_str()
                .unwrap(),
            "string"
        );
        assert_eq!(
            tags_property
                .get("items")
                .unwrap()
                .as_object()
                .unwrap()
                .get("enum")
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            2
        );

        let record_items = properties
            .get("records")
            .unwrap()
            .as_object()
            .unwrap()
            .get("items")
            .unwrap()
            .as_object()
            .unwrap();
        assert_eq!(record_items.get("type").unwrap().as_str().unwrap(), "object");
        assert_eq!(
            record_items
                .get("properties")
                .unwrap()
                .as_object()
                .unwrap()
                .get("id")
                .unwrap()
                .as_object()
                .unwrap()
                .get("type")
                .unwrap()
                .as_str()
                .unwrap(),
            "integer"
        );
        let _ = Value::Null;
    }

    #[test]
    fn dynamic_function_schema_json_alloc_wraps_rendered_input_schema_in_the_flattened_envelope() {
        let mut description = vec![b'd'; description_max_bytes + 1];
        description.extend_from_slice(b"tail");
        let json = dynamic_function_schema_json_alloc(
            b"mcp_fs_read",
            &description,
            br#"{"type":"object","properties":{}}"#,
        )
        .unwrap();

        let parsed = parse_json(std::str::from_utf8(&json).unwrap()).unwrap();
        let object = parsed.as_object().unwrap();
        assert_eq!(object.get("type").unwrap().as_str().unwrap(), "function");
        assert_eq!(object.get("name").unwrap().as_str().unwrap(), "mcp_fs_read");
        assert_eq!(
            object.get("description").unwrap().as_str().unwrap().as_bytes(),
            description.as_slice()
        );
        assert!(object
            .get("inputSchema")
            .unwrap()
            .as_object()
            .unwrap()
            .get("properties")
            .is_some());
        assert!(object.get("function").is_none());
    }
}
