//! Context limit names, sources, values, and overrides.
//!
//! Mirrors `zig-reference/src/core/config/context_limits.zig`.

pub const EMERGENCY_CEILING_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Name {
    SkillDescriptionBytes,
    SkillCatalogBytes,
    SkillChunkBytes,
    SkillFileBytes,
    McpDescriptionBytes,
    McpSearchResultBytes,
    McpServerInstructionsBytes,
    McpSelectedSchemaBytes,
    ProjectInstructionFileBytes,
    ProjectInstructionsTotalBytes,
    ImageAdapterOutputBytes,
}

impl Name {
    pub const ALL: [Name; 11] = [
        Name::SkillDescriptionBytes,
        Name::SkillCatalogBytes,
        Name::SkillChunkBytes,
        Name::SkillFileBytes,
        Name::McpDescriptionBytes,
        Name::McpSearchResultBytes,
        Name::McpServerInstructionsBytes,
        Name::McpSelectedSchemaBytes,
        Name::ProjectInstructionFileBytes,
        Name::ProjectInstructionsTotalBytes,
        Name::ImageAdapterOutputBytes,
    ];

    pub fn default_bytes(self) -> usize {
        match self {
            Name::SkillDescriptionBytes => 1024,
            Name::SkillCatalogBytes => 16 * 1024,
            Name::SkillChunkBytes => 20 * 1024,
            Name::SkillFileBytes => 1024 * 1024,
            Name::McpDescriptionBytes => 1024,
            Name::McpSearchResultBytes => 16 * 1024,
            Name::McpServerInstructionsBytes => 2 * 1024,
            Name::McpSelectedSchemaBytes => 64 * 1024,
            Name::ProjectInstructionFileBytes => 64 * 1024,
            Name::ProjectInstructionsTotalBytes => 128 * 1024,
            Name::ImageAdapterOutputBytes => 20 * 1024,
        }
    }

    pub fn parse(raw: &str) -> Option<Name> {
        match raw {
            "skill_description_bytes" => Some(Name::SkillDescriptionBytes),
            "skill_catalog_bytes" => Some(Name::SkillCatalogBytes),
            "skill_chunk_bytes" => Some(Name::SkillChunkBytes),
            "skill_file_bytes" => Some(Name::SkillFileBytes),
            "mcp_description_bytes" => Some(Name::McpDescriptionBytes),
            "mcp_search_result_bytes" => Some(Name::McpSearchResultBytes),
            "mcp_server_instructions_bytes" => Some(Name::McpServerInstructionsBytes),
            "mcp_selected_schema_bytes" => Some(Name::McpSelectedSchemaBytes),
            "project_instruction_file_bytes" => Some(Name::ProjectInstructionFileBytes),
            "project_instructions_total_bytes" => Some(Name::ProjectInstructionsTotalBytes),
            "image_adapter_output_bytes" => Some(Name::ImageAdapterOutputBytes),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    CompiledDefault,
    UserGlobal,
    UserWorkspace,
    CommandLine,
}

impl Source {
    pub fn label(self) -> &'static str {
        match self {
            Source::CompiledDefault => "compiled default",
            Source::UserGlobal => "global settings",
            Source::UserWorkspace => "workspace settings",
            Source::CommandLine => "command line",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Value {
    Bytes(usize),
    Off,
}

impl Value {
    pub fn normal_bytes(self) -> Option<usize> {
        match self {
            Value::Bytes(bytes) => Some(bytes),
            Value::Off => None,
        }
    }

    pub fn effective_bytes(self) -> usize {
        std::cmp::min(
            self.normal_bytes().unwrap_or(EMERGENCY_CEILING_BYTES),
            EMERGENCY_CEILING_BYTES,
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Resolved {
    pub value: Value,
    pub source: Source,
}

impl Resolved {
    pub fn effective_bytes(self) -> usize {
        self.value.effective_bytes()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Override {
    pub name: Name,
    pub value: Value,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Values {
    pub skill_description_bytes: Resolved,
    pub skill_catalog_bytes: Resolved,
    pub skill_chunk_bytes: Resolved,
    pub skill_file_bytes: Resolved,
    pub mcp_description_bytes: Resolved,
    pub mcp_search_result_bytes: Resolved,
    pub mcp_server_instructions_bytes: Resolved,
    pub mcp_selected_schema_bytes: Resolved,
    pub project_instruction_file_bytes: Resolved,
    pub project_instructions_total_bytes: Resolved,
    pub image_adapter_output_bytes: Resolved,
}

impl Default for Values {
    fn default() -> Self {
        Values {
            skill_description_bytes: default_resolved(Name::SkillDescriptionBytes),
            skill_catalog_bytes: default_resolved(Name::SkillCatalogBytes),
            skill_chunk_bytes: default_resolved(Name::SkillChunkBytes),
            skill_file_bytes: default_resolved(Name::SkillFileBytes),
            mcp_description_bytes: default_resolved(Name::McpDescriptionBytes),
            mcp_search_result_bytes: default_resolved(Name::McpSearchResultBytes),
            mcp_server_instructions_bytes: default_resolved(Name::McpServerInstructionsBytes),
            mcp_selected_schema_bytes: default_resolved(Name::McpSelectedSchemaBytes),
            project_instruction_file_bytes: default_resolved(Name::ProjectInstructionFileBytes),
            project_instructions_total_bytes: default_resolved(Name::ProjectInstructionsTotalBytes),
            image_adapter_output_bytes: default_resolved(Name::ImageAdapterOutputBytes),
        }
    }
}

impl Values {
    pub fn get(&self, name: Name) -> Resolved {
        match name {
            Name::SkillDescriptionBytes => self.skill_description_bytes,
            Name::SkillCatalogBytes => self.skill_catalog_bytes,
            Name::SkillChunkBytes => self.skill_chunk_bytes,
            Name::SkillFileBytes => self.skill_file_bytes,
            Name::McpDescriptionBytes => self.mcp_description_bytes,
            Name::McpSearchResultBytes => self.mcp_search_result_bytes,
            Name::McpServerInstructionsBytes => self.mcp_server_instructions_bytes,
            Name::McpSelectedSchemaBytes => self.mcp_selected_schema_bytes,
            Name::ProjectInstructionFileBytes => self.project_instruction_file_bytes,
            Name::ProjectInstructionsTotalBytes => self.project_instructions_total_bytes,
            Name::ImageAdapterOutputBytes => self.image_adapter_output_bytes,
        }
    }

    pub fn apply(&mut self, overrides: &Overrides) {
        for name in Name::ALL {
            if let Some(value) = overrides.get(name) {
                self.set(name, value);
            }
        }
    }

    pub fn apply_command_line(&mut self, overrides: &[Override]) {
        for item in overrides {
            self.set(
                item.name,
                Resolved {
                    value: item.value,
                    source: Source::CommandLine,
                },
            );
        }
    }

    pub fn set(&mut self, name: Name, value: Resolved) {
        match name {
            Name::SkillDescriptionBytes => self.skill_description_bytes = value,
            Name::SkillCatalogBytes => self.skill_catalog_bytes = value,
            Name::SkillChunkBytes => self.skill_chunk_bytes = value,
            Name::SkillFileBytes => self.skill_file_bytes = value,
            Name::McpDescriptionBytes => self.mcp_description_bytes = value,
            Name::McpSearchResultBytes => self.mcp_search_result_bytes = value,
            Name::McpServerInstructionsBytes => self.mcp_server_instructions_bytes = value,
            Name::McpSelectedSchemaBytes => self.mcp_selected_schema_bytes = value,
            Name::ProjectInstructionFileBytes => self.project_instruction_file_bytes = value,
            Name::ProjectInstructionsTotalBytes => self.project_instructions_total_bytes = value,
            Name::ImageAdapterOutputBytes => self.image_adapter_output_bytes = value,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Overrides {
    pub skill_description_bytes: Option<Resolved>,
    pub skill_catalog_bytes: Option<Resolved>,
    pub skill_chunk_bytes: Option<Resolved>,
    pub skill_file_bytes: Option<Resolved>,
    pub mcp_description_bytes: Option<Resolved>,
    pub mcp_search_result_bytes: Option<Resolved>,
    pub mcp_server_instructions_bytes: Option<Resolved>,
    pub mcp_selected_schema_bytes: Option<Resolved>,
    pub project_instruction_file_bytes: Option<Resolved>,
    pub project_instructions_total_bytes: Option<Resolved>,
    pub image_adapter_output_bytes: Option<Resolved>,
}

impl Overrides {
    pub fn get(&self, name: Name) -> Option<Resolved> {
        match name {
            Name::SkillDescriptionBytes => self.skill_description_bytes,
            Name::SkillCatalogBytes => self.skill_catalog_bytes,
            Name::SkillChunkBytes => self.skill_chunk_bytes,
            Name::SkillFileBytes => self.skill_file_bytes,
            Name::McpDescriptionBytes => self.mcp_description_bytes,
            Name::McpSearchResultBytes => self.mcp_search_result_bytes,
            Name::McpServerInstructionsBytes => self.mcp_server_instructions_bytes,
            Name::McpSelectedSchemaBytes => self.mcp_selected_schema_bytes,
            Name::ProjectInstructionFileBytes => self.project_instruction_file_bytes,
            Name::ProjectInstructionsTotalBytes => self.project_instructions_total_bytes,
            Name::ImageAdapterOutputBytes => self.image_adapter_output_bytes,
        }
    }

    fn get_mut(&mut self, name: Name) -> Option<&mut Resolved> {
        match name {
            Name::SkillDescriptionBytes => self.skill_description_bytes.as_mut(),
            Name::SkillCatalogBytes => self.skill_catalog_bytes.as_mut(),
            Name::SkillChunkBytes => self.skill_chunk_bytes.as_mut(),
            Name::SkillFileBytes => self.skill_file_bytes.as_mut(),
            Name::McpDescriptionBytes => self.mcp_description_bytes.as_mut(),
            Name::McpSearchResultBytes => self.mcp_search_result_bytes.as_mut(),
            Name::McpServerInstructionsBytes => self.mcp_server_instructions_bytes.as_mut(),
            Name::McpSelectedSchemaBytes => self.mcp_selected_schema_bytes.as_mut(),
            Name::ProjectInstructionFileBytes => self.project_instruction_file_bytes.as_mut(),
            Name::ProjectInstructionsTotalBytes => self.project_instructions_total_bytes.as_mut(),
            Name::ImageAdapterOutputBytes => self.image_adapter_output_bytes.as_mut(),
        }
    }

    pub fn set(&mut self, name: Name, value: Resolved) {
        match name {
            Name::SkillDescriptionBytes => self.skill_description_bytes = Some(value),
            Name::SkillCatalogBytes => self.skill_catalog_bytes = Some(value),
            Name::SkillChunkBytes => self.skill_chunk_bytes = Some(value),
            Name::SkillFileBytes => self.skill_file_bytes = Some(value),
            Name::McpDescriptionBytes => self.mcp_description_bytes = Some(value),
            Name::McpSearchResultBytes => self.mcp_search_result_bytes = Some(value),
            Name::McpServerInstructionsBytes => self.mcp_server_instructions_bytes = Some(value),
            Name::McpSelectedSchemaBytes => self.mcp_selected_schema_bytes = Some(value),
            Name::ProjectInstructionFileBytes => self.project_instruction_file_bytes = Some(value),
            Name::ProjectInstructionsTotalBytes => self.project_instructions_total_bytes = Some(value),
            Name::ImageAdapterOutputBytes => self.image_adapter_output_bytes = Some(value),
        }
    }

    pub fn retag(&mut self, source: Source) {
        for name in Name::ALL {
            if let Some(value) = self.get_mut(name) {
                value.source = source;
            }
        }
    }

    pub fn merge(&mut self, incoming: Overrides) {
        for name in Name::ALL {
            if let Some(value) = incoming.get(name) {
                self.set(name, value);
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContextLimitError {
    InvalidContextLimitOverride,
    UnknownContextLimit,
    InvalidContextLimitsType,
    InvalidContextLimitValue,
}

/// Minimal JSON value model so `parse_json_object` can accept parsed JSON
/// without pulling in an external crate. Only the variants the config code
/// inspects are semantically meaningful here; the rest exist to represent the
/// full value space so the `else` branch in the Zig source is preserved.
#[derive(Clone, Debug, PartialEq)]
pub enum JsonValue {
    Null,
    Bool(bool),
    Integer(i64),
    Number(f64),
    String(String),
    Array(Vec<JsonValue>),
    Object(Vec<(String, JsonValue)>),
}

pub fn parse_override(raw: &str) -> Result<Override, ContextLimitError> {
    let separator = raw
        .find('=')
        .ok_or(ContextLimitError::InvalidContextLimitOverride)?;
    let raw_name = raw[..separator].trim_matches(|c: char| c == ' ' || c == '\t' || c == '\r' || c == '\n');
    let raw_value =
        raw[separator + 1..].trim_matches(|c: char| c == ' ' || c == '\t' || c == '\r' || c == '\n');
    if raw_name.is_empty() || raw_value.is_empty() {
        return Err(ContextLimitError::InvalidContextLimitOverride);
    }
    let name = Name::parse(raw_name).ok_or(ContextLimitError::UnknownContextLimit)?;
    Ok(Override {
        name,
        value: parse_value_string(raw_value)?,
    })
}

pub fn parse_json_object(value: JsonValue) -> Result<Overrides, ContextLimitError> {
    let entries = match value {
        JsonValue::Object(entries) => entries,
        _ => return Err(ContextLimitError::InvalidContextLimitsType),
    };

    let mut result = Overrides::default();
    for (key, entry_value) in entries {
        let name = Name::parse(&key).ok_or(ContextLimitError::UnknownContextLimit)?;
        result.set(
            name,
            Resolved {
                value: parse_json_value(entry_value)?,
                source: Source::CompiledDefault,
            },
        );
    }
    Ok(result)
}

fn parse_json_value(value: JsonValue) -> Result<Value, ContextLimitError> {
    match value {
        JsonValue::Integer(integer) => {
            let bytes =
                usize::try_from(integer).map_err(|_| ContextLimitError::InvalidContextLimitValue)?;
            Ok(Value::Bytes(bytes))
        }
        JsonValue::String(string) => parse_value_string(&string),
        _ => Err(ContextLimitError::InvalidContextLimitValue),
    }
}

fn parse_value_string(raw: &str) -> Result<Value, ContextLimitError> {
    if raw.eq_ignore_ascii_case("off") {
        return Ok(Value::Off);
    }
    let bytes = raw
        .parse::<usize>()
        .map_err(|_| ContextLimitError::InvalidContextLimitValue)?;
    Ok(Value::Bytes(bytes))
}

fn default_resolved(name: Name) -> Resolved {
    Resolved {
        value: Value::Bytes(name.default_bytes()),
        source: Source::CompiledDefault,
    }
}

pub fn utf8_prefix_length(bytes: &[u8], max_bytes: usize) -> usize {
    let mut end = std::cmp::min(max_bytes, bytes.len());
    while end > 0 && std::str::from_utf8(&bytes[..end]).is_err() {
        end -= 1;
    }
    end
}

pub fn line_safe_prefix_length(bytes: &[u8], max_bytes: usize) -> usize {
    let utf8_end = utf8_prefix_length(bytes, max_bytes);
    if utf8_end == bytes.len() {
        return utf8_end;
    }
    if let Some(newline) = bytes[..utf8_end].iter().rposition(|&byte| byte == b'\n') {
        return newline + 1;
    }
    utf8_end
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_public_context_limit_contract() {
        let values = Values::default();
        for name in Name::ALL {
            assert_eq!(name.default_bytes(), values.get(name).effective_bytes());
            assert_eq!(Source::CompiledDefault, values.get(name).source);
        }
    }

    #[test]
    fn context_limit_overrides_accept_bytes_and_off() {
        let bytes = parse_override("skill_chunk_bytes=4096").unwrap();
        assert_eq!(Name::SkillChunkBytes, bytes.name);
        assert_eq!(
            4096,
            match bytes.value {
                Value::Bytes(value) => value,
                Value::Off => panic!("expected byte value"),
            }
        );

        let off = parse_override("mcp_description_bytes=off").unwrap();
        assert_eq!(Name::McpDescriptionBytes, off.name);
        assert_eq!(EMERGENCY_CEILING_BYTES, off.value.effective_bytes());
    }

    #[test]
    fn context_limit_overrides_reject_unknown_names_and_malformed_values() {
        assert_eq!(
            Err(ContextLimitError::UnknownContextLimit),
            parse_override("wat=1")
        );
        assert_eq!(
            Err(ContextLimitError::InvalidContextLimitValue),
            parse_override("skill_chunk_bytes=-1")
        );
        assert_eq!(
            Err(ContextLimitError::InvalidContextLimitOverride),
            parse_override("skill_chunk_bytes")
        );
    }

    #[test]
    fn line_safe_prefix_preserves_utf8_and_complete_lines_when_possible() {
        assert_eq!(4, line_safe_prefix_length(b"one\ntwo\n", 7));
        assert_eq!(0, line_safe_prefix_length("éclair".as_bytes(), 1));
        assert_eq!(2, line_safe_prefix_length("éclair".as_bytes(), 2));
        assert_eq!(3, line_safe_prefix_length(&[b'a', b'b', b'c', 0xe4], 4));
    }
}
