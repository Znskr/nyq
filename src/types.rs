//! Provider-neutral conversation model shared by the agent loop, providers,
//! tools, sessions, and UI.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One entry in the conversation transcript. Sessions persist these verbatim
/// (one JSON object per line), so keep the serde shape stable.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Item {
    User {
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<Image>,
    },
    Assistant {
        text: String,
    },
    /// Model reasoning. `encrypted` is an opaque provider blob (Responses API
    /// `encrypted_content`) that must be replayed to keep reasoning continuity.
    Reasoning {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(default)]
        summary: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        encrypted: Option<String>,
    },
    ToolCall {
        id: String,
        name: String,
        /// Raw JSON arguments string exactly as the model produced it.
        arguments: String,
    },
    ToolResult {
        id: String,
        output: String,
        #[serde(default)]
        is_error: bool,
    },
    /// Summary that replaces everything before it (see `Agent::compact`).
    /// Providers send it as a user message; session loading starts here.
    Compaction {
        summary: String,
    },
}

/// How a compaction summary is presented to the model.
pub fn compaction_text(summary: &str) -> String {
    format!("[This conversation was compacted. Summary of everything before this point:]\n\n{summary}")
}

impl Item {
    pub fn user(text: impl Into<String>) -> Self {
        Item::User { text: text.into(), images: Vec::new() }
    }
}

/// An image attached to a user message.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Image {
    /// e.g. `image/png`.
    pub media_type: String,
    /// Base64 (standard alphabet, padded) image bytes.
    pub data: String,
}

impl Image {
    pub fn data_url(&self) -> String {
        format!("data:{};base64,{}", self.media_type, self.data)
    }
}

/// A function tool advertised to the model.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema object for the arguments.
    pub parameters: Value,
}

/// Incremental output while a model response streams.
#[derive(Clone, Debug, PartialEq)]
pub enum StreamEvent {
    Text(String),
    Reasoning(String),
    /// The model started emitting a tool call (arguments still streaming).
    ToolCallStart { id: String, name: String },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default)]
    pub cached_tokens: u64,
}

/// A request for one model response.
pub struct Request<'a> {
    pub system: &'a str,
    pub items: &'a [Item],
    pub tools: &'a [ToolSpec],
}

/// The completed model response: new transcript items in output order
/// (reasoning, assistant text, tool calls) plus usage if reported.
#[derive(Clone, Debug, Default)]
pub struct Response {
    pub items: Vec<Item>,
    pub usage: Option<Usage>,
    /// The model hit its output limit. `items` is the partial reply and is kept.
    pub truncated: bool,
}
