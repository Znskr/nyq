//! MCP subsystem (ported 1:1 from zig-reference/src/core/mcp).
//! `json_value` is a shared internal helper for the schema cluster.

pub mod json_number;
pub mod json_schema;
pub mod json_schema_pattern;
pub mod json_schema_resolver;
pub mod json_schema_unicode_letter;
pub mod json_value;
pub mod mcp_json;
pub mod feature_cache;
pub mod health;
pub mod mcp_auth;
pub mod mcp_auth_store;
pub mod mcp_contract;
pub mod protocol_negotiation;
pub mod startup_admission;
pub mod streamable_http;
