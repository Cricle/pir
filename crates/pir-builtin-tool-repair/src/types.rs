//! Neutral message DTOs for the pure tool-repair pipeline.
//!
//! The upstream repair core operated on `pir-core`'s `ai::types` message
//! model. This crate must not depend on the engine, so the handful of shapes
//! the pure passes actually read or rewrite are mirrored here, with the same
//! wire representation:
//!
//! - [`AssistantMessage`] keeps `role` / `content` / `stopReason` /
//!   `errorMessage` typed and preserves every other key in
//!   [`AssistantMessage::extra`], so a host adapter can round-trip a core
//!   message through [`AssistantMessage::from_json_value`] /
//!   [`AssistantMessage::to_json_value`] without losing provider fields
//!   (`api`, `provider`, `model`, `usage`, `diagnostics`, …).
//! - [`ContentBlock`] and [`ToolCall`] reuse the core serializers' exact shape
//!   (externally tagged `type` field, camelCase option names, omitted options).
//!
//! The repair passes only need `content`, `stop_reason` and `error_message`;
//! the rest exists for lossless conversion at the host boundary.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

// ---------------------------------------------------------------------------
// Content blocks
// ---------------------------------------------------------------------------

/// A block of text content.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TextContent {
    pub text: String,
    /// Optional text signature (e.g. for OpenAI responses).
    #[serde(
        rename = "textSignature",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub text_signature: Option<String>,
}

impl TextContent {
    /// Plain text with no signature.
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            text_signature: None,
        }
    }
}

/// A block of thinking/reasoning content.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThinkingContent {
    pub thinking: String,
    /// Optional thinking signature (e.g. for OpenAI responses).
    #[serde(
        rename = "thinkingSignature",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub thinking_signature: Option<String>,
    /// When true, the thinking content was redacted by safety filters.
    #[serde(default, skip_serializing_if = "is_false")]
    pub redacted: bool,
}

impl ThinkingContent {
    /// Plain thinking text with no signature and no redaction flag.
    pub fn new(thinking: impl Into<String>) -> Self {
        Self {
            thinking: thinking.into(),
            thinking_signature: None,
            redacted: false,
        }
    }
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// A block of image content (base64).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImageContent {
    pub data: String,
    #[serde(rename = "mimeType")]
    pub mime_type: String,
}

/// A tool call produced by the model.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: BTreeMap<String, JsonValue>,
    /// Google-specific: opaque signature for reusing thought context.
    #[serde(rename = "thoughtSignature", alias = "thought_signature", default)]
    pub thought_signature: Option<String>,
    /// OpenAI Responses namespace for dynamically loaded or namespaced tools.
    #[serde(default)]
    pub namespace: Option<String>,
    /// Set when the model's complete arguments text was not a valid JSON
    /// object (upstream `ToolCall.argumentsError`).
    #[serde(default, rename = "argumentsError", alias = "arguments_error")]
    pub arguments_error: Option<String>,
}

impl ToolCall {
    /// A tool call with no provider-specific metadata.
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        arguments: BTreeMap<String, JsonValue>,
    ) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            arguments,
            thought_signature: None,
            namespace: None,
            arguments_error: None,
        }
    }
}

impl Serialize for ToolCall {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;

        let mut fields = 4; // type, id, name, arguments
        if self.thought_signature.is_some() {
            fields += 1;
        }
        if self.namespace.is_some() {
            fields += 1;
        }
        if self.arguments_error.is_some() {
            fields += 1;
        }

        let mut state = serializer.serialize_struct("ToolCall", fields)?;
        state.serialize_field("type", "toolCall")?;
        state.serialize_field("id", &self.id)?;
        state.serialize_field("name", &self.name)?;
        state.serialize_field("arguments", &self.arguments)?;
        if let Some(signature) = &self.thought_signature {
            state.serialize_field("thoughtSignature", signature)?;
        }
        if let Some(namespace) = &self.namespace {
            state.serialize_field("namespace", namespace)?;
        }
        if let Some(error) = &self.arguments_error {
            state.serialize_field("argumentsError", error)?;
        }
        state.end()
    }
}

/// A content block within a message.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "type")]
pub enum ContentBlock {
    #[serde(rename = "text")]
    Text(TextContent),
    #[serde(rename = "thinking")]
    Thinking(ThinkingContent),
    #[serde(rename = "image")]
    Image(ImageContent),
    #[serde(rename = "toolCall")]
    ToolCall(ToolCall),
}

impl ContentBlock {
    /// A plain text block.
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text(TextContent::new(text))
    }

    /// A plain thinking block.
    pub fn thinking(thinking: impl Into<String>) -> Self {
        Self::Thinking(ThinkingContent::new(thinking))
    }

    /// The text this block carries, when it is a text block.
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text(content) => Some(content.text.as_str()),
            _ => None,
        }
    }

    /// The thinking text this block carries, when it is a thinking block.
    pub fn as_thinking(&self) -> Option<&str> {
        match self {
            Self::Thinking(content) => Some(content.thinking.as_str()),
            _ => None,
        }
    }

    /// The tool call this block carries, when it is a tool-call block.
    pub fn as_tool_call(&self) -> Option<&ToolCall> {
        match self {
            Self::ToolCall(call) => Some(call),
            _ => None,
        }
    }

    /// The tool call this block carries, when it is a tool-call block.
    pub fn as_tool_call_mut(&mut self) -> Option<&mut ToolCall> {
        match self {
            Self::ToolCall(call) => Some(call),
            _ => None,
        }
    }
}

impl Serialize for ContentBlock {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;

        match self {
            ContentBlock::Text(content) => {
                let mut fields = 2; // type, text
                if content.text_signature.is_some() {
                    fields += 1;
                }
                let mut state = serializer.serialize_struct("TextContent", fields)?;
                state.serialize_field("type", "text")?;
                state.serialize_field("text", &content.text)?;
                if let Some(signature) = &content.text_signature {
                    state.serialize_field("textSignature", signature)?;
                }
                state.end()
            }
            ContentBlock::Thinking(content) => {
                let mut fields = 2; // type, thinking
                if content.thinking_signature.is_some() {
                    fields += 1;
                }
                if content.redacted {
                    fields += 1;
                }
                let mut state = serializer.serialize_struct("ThinkingContent", fields)?;
                state.serialize_field("type", "thinking")?;
                state.serialize_field("thinking", &content.thinking)?;
                if let Some(signature) = &content.thinking_signature {
                    state.serialize_field("thinkingSignature", signature)?;
                }
                if content.redacted {
                    state.serialize_field("redacted", &true)?;
                }
                state.end()
            }
            ContentBlock::Image(content) => {
                let mut state = serializer.serialize_struct("ImageContent", 3)?;
                state.serialize_field("type", "image")?;
                state.serialize_field("data", &content.data)?;
                state.serialize_field("mimeType", &content.mime_type)?;
                state.end()
            }
            // ToolCall's own serializer emits `type: "toolCall"`, so the
            // ContentBlock tag must not be written a second time.
            ContentBlock::ToolCall(content) => serde::Serialize::serialize(content, serializer),
        }
    }
}

// ---------------------------------------------------------------------------
// Stop reason
// ---------------------------------------------------------------------------

/// Why the model stopped generating.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum StopReason {
    /// Still generating (streaming partial).
    #[serde(rename = "pending")]
    #[default]
    Pending,
    /// Model stopped naturally.
    #[serde(rename = "stop")]
    Stop,
    /// Max tokens reached.
    #[serde(rename = "length")]
    Length,
    /// Model requested tool use.
    #[serde(rename = "toolUse")]
    ToolUse,
    /// An error occurred.
    #[serde(rename = "error")]
    Error,
    /// Request was aborted.
    #[serde(rename = "aborted")]
    Aborted,
    /// Response was deferred (async).
    #[serde(rename = "deferred")]
    Deferred,
}

// ---------------------------------------------------------------------------
// Message
// ---------------------------------------------------------------------------

/// The assistant role. A single variant, so a host message that lacks `role`
/// still deserializes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum AssistantRole {
    #[default]
    #[serde(rename = "assistant")]
    Assistant,
}

/// A neutral assistant message: the repair pipelines' input.
///
/// Only `content`, `stopReason` and `errorMessage` are interpreted. Every other
/// key (the core `AssistantMessage`'s `api`, `provider`, `model`, `usage`,
/// `diagnostics`, `deferred`, timestamps, …) is preserved verbatim in
/// [`Self::extra`] and re-emitted on serialization by the host adapter.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssistantMessage {
    #[serde(default)]
    pub role: AssistantRole,
    #[serde(default)]
    pub content: Vec<ContentBlock>,
    #[serde(rename = "stopReason", default)]
    pub stop_reason: StopReason,
    #[serde(
        rename = "errorMessage",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub error_message: Option<String>,
    /// Uninterpreted message fields, preserved for lossless round-tripping.
    #[serde(flatten)]
    pub extra: BTreeMap<String, JsonValue>,
}

impl Default for AssistantMessage {
    fn default() -> Self {
        Self {
            role: AssistantRole::Assistant,
            content: Vec::new(),
            stop_reason: StopReason::Pending,
            error_message: None,
            extra: BTreeMap::new(),
        }
    }
}

impl AssistantMessage {
    /// An assistant message carrying `content` (stop reason `pending`).
    pub fn new(content: Vec<ContentBlock>) -> Self {
        Self {
            content,
            ..Self::default()
        }
    }

    /// Deserialize from the core message's JSON representation.
    pub fn from_json_value(value: JsonValue) -> Result<Self, serde_json::Error> {
        serde_json::from_value(value)
    }

    /// Serialize to the core message's JSON representation.
    pub fn to_json_value(&self) -> JsonValue {
        serde_json::to_value(self).expect("AssistantMessage DTO serializes")
    }

    /// Whether the message carries at least one tool-call block.
    pub fn has_tool_calls(&self) -> bool {
        self.content
            .iter()
            .any(|block| matches!(block, ContentBlock::ToolCall(_)))
    }

    /// Iterate the message's tool-call blocks in content order.
    pub fn tool_calls(&self) -> impl Iterator<Item = &ToolCall> {
        self.content.iter().filter_map(ContentBlock::as_tool_call)
    }
}
