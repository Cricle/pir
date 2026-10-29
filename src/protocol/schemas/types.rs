//! Typed structs for encoding protocol values.

use serde::Serialize;

#[cfg(feature = "server")]
use super::validate::field_opt;
#[cfg(feature = "server")]
use serde_json::Value;

// ---------------------------------------------------------------------------
// Typed structs for encoding
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct ModelRef {
    #[serde(rename = "provider")]
    pub provider: String,
    #[serde(rename = "id")]
    pub id: String,
}

/// Fallback provider/id for incomplete model references; matches the server's
/// default session model.
#[cfg(feature = "server")]
const MODEL_REF_DEFAULT: &str = "default";

impl ModelRef {
    /// Extract a model reference from a JSON value, filling a missing or
    /// non-string `provider`/`id` with the shared default. Returns `None` when
    /// the value is not an object at all.
    #[cfg(feature = "server")]
    pub(crate) fn from_value(value: &Value) -> Option<Self> {
        let obj = value.as_object()?;
        Some(Self {
            provider: field_opt(obj, "provider")
                .and_then(Value::as_str)
                .unwrap_or(MODEL_REF_DEFAULT)
                .to_owned(),
            id: field_opt(obj, "id")
                .and_then(Value::as_str)
                .unwrap_or(MODEL_REF_DEFAULT)
                .to_owned(),
        })
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelCost {
    #[serde(rename = "input")]
    pub input: f64,
    #[serde(rename = "output")]
    pub output: f64,
    #[serde(rename = "cacheRead")]
    pub cache_read: f64,
    #[serde(rename = "cacheWrite")]
    pub cache_write: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelMetadata {
    #[serde(rename = "provider")]
    pub provider: String,
    #[serde(rename = "id")]
    pub id: String,
    #[serde(rename = "name")]
    pub name: String,
    #[serde(rename = "api")]
    pub api: String,
    #[serde(rename = "reasoning")]
    pub reasoning: bool,
    #[serde(rename = "input")]
    pub input: Vec<String>,
    #[serde(rename = "contextWindow")]
    pub context_window: i64,
    #[serde(rename = "maxTokens")]
    pub max_tokens: i64,
    #[serde(rename = "cost")]
    pub cost: ModelCost,
    #[serde(rename = "supportedThinkingLevels")]
    pub supported_thinking_levels: Vec<String>,
    #[serde(rename = "authenticated")]
    pub authenticated: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct TextContent {
    #[serde(rename = "type")]
    pub type_: String,
    #[serde(rename = "text")]
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ThinkingContent {
    #[serde(rename = "type")]
    pub type_: String,
    #[serde(rename = "thinking")]
    pub thinking: String,
    #[serde(rename = "redacted", skip_serializing_if = "Option::is_none")]
    pub redacted: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ImageContent {
    #[serde(rename = "type")]
    pub type_: String,
    #[serde(rename = "data")]
    pub data: String,
    #[serde(rename = "mimeType")]
    pub mime_type: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolCallContent {
    #[serde(rename = "type")]
    pub type_: String,
    #[serde(rename = "toolCallId")]
    pub tool_call_id: String,
    #[serde(rename = "toolName")]
    pub tool_name: String,
    #[serde(rename = "input")]
    pub input: serde_json::Value,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UserContent {
    Text(TextContent),
    Image(ImageContent),
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AssistantContent {
    Text(TextContent),
    Thinking(ThinkingContent),
    ToolCall(ToolCallContent),
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolContent {
    Text(TextContent),
    Image(ImageContent),
}

#[derive(Debug, Clone, Serialize)]
pub struct Usage {
    #[serde(rename = "input")]
    pub input: i64,
    #[serde(rename = "output")]
    pub output: i64,
    #[serde(rename = "cacheRead")]
    pub cache_read: i64,
    #[serde(rename = "cacheWrite")]
    pub cache_write: i64,
    #[serde(rename = "reasoning", skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<i64>,
    #[serde(rename = "totalTokens")]
    pub total_tokens: i64,
    #[serde(rename = "cost")]
    pub cost: Cost,
}

#[derive(Debug, Clone, Serialize)]
pub struct Cost {
    #[serde(rename = "input")]
    pub input: f64,
    #[serde(rename = "output")]
    pub output: f64,
    #[serde(rename = "cacheRead")]
    pub cache_read: f64,
    #[serde(rename = "cacheWrite")]
    pub cache_write: f64,
    #[serde(rename = "total")]
    pub total: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct UserTranscriptItem {
    #[serde(rename = "id")]
    pub id: String,
    #[serde(rename = "role")]
    pub role: String,
    #[serde(rename = "content")]
    pub content: Vec<UserContent>,
    #[serde(rename = "timestamp")]
    pub timestamp: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct AssistantTranscriptItem {
    #[serde(rename = "id")]
    pub id: String,
    #[serde(rename = "role")]
    pub role: String,
    #[serde(rename = "content")]
    pub content: Vec<AssistantContent>,
    #[serde(rename = "model")]
    pub model: ModelRef,
    #[serde(rename = "responseModel", skip_serializing_if = "Option::is_none")]
    pub response_model: Option<String>,
    #[serde(rename = "usage", skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(rename = "status")]
    pub status: String,
    #[serde(rename = "stopReason", skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    #[serde(rename = "errorMessage", skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    #[serde(rename = "timestamp")]
    pub timestamp: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolTranscriptItem {
    #[serde(rename = "id")]
    pub id: String,
    #[serde(rename = "role")]
    pub role: String,
    #[serde(rename = "toolCallId")]
    pub tool_call_id: String,
    #[serde(rename = "toolName")]
    pub tool_name: String,
    #[serde(rename = "input")]
    pub input: serde_json::Value,
    #[serde(rename = "content")]
    pub content: Vec<ToolContent>,
    #[serde(rename = "details", skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
    #[serde(rename = "usage", skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(rename = "status")]
    pub status: String,
    #[serde(rename = "isError")]
    pub is_error: bool,
    #[serde(rename = "timestamp")]
    pub timestamp: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum TranscriptItem {
    User(UserTranscriptItem),
    Assistant(AssistantTranscriptItem),
    Tool(ToolTranscriptItem),
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionMetadata {
    #[serde(rename = "id")]
    pub id: String,
    #[serde(rename = "createdAt")]
    pub created_at: i64,
    #[serde(rename = "updatedAt", skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<i64>,
    #[serde(rename = "parentSessionId", skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    #[serde(rename = "sessionName", skip_serializing_if = "Option::is_none")]
    pub session_name: Option<String>,
    #[serde(rename = "cwd", skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionSnapshot {
    #[serde(rename = "id")]
    pub id: String,
    #[serde(rename = "name", skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(rename = "cwd")]
    pub cwd: String,
    #[serde(rename = "createdAt")]
    pub created_at: i64,
    #[serde(rename = "updatedAt")]
    pub updated_at: i64,
    #[serde(rename = "phase")]
    pub phase: String,
    #[serde(rename = "model")]
    pub model: ModelRef,
    #[serde(rename = "thinkingLevel")]
    pub thinking_level: String,
    #[serde(rename = "attached")]
    pub attached: bool,
    #[serde(rename = "locked")]
    pub locked: bool,
    #[serde(rename = "revision")]
    pub revision: i64,
    #[serde(rename = "transcript")]
    pub transcript: Vec<TranscriptItem>,
    #[serde(rename = "queuedSteer")]
    pub queued_steer: Vec<UserTranscriptItem>,
    #[serde(rename = "queuedSteerCount")]
    pub queued_steer_count: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ServerSnapshot {
    #[serde(rename = "serverId")]
    pub server_id: String,
    #[serde(rename = "protocolVersion")]
    pub protocol_version: i64,
    #[serde(rename = "revision")]
    pub revision: i64,
    #[serde(rename = "sessions")]
    pub sessions: Vec<SessionMetadata>,
    #[serde(rename = "models")]
    pub models: Vec<ModelMetadata>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProtocolError {
    #[serde(rename = "code")]
    pub code: String,
    #[serde(rename = "message")]
    pub message: String,
    /// Never emitted on the wire: upstream's `ProtocolErrorSchema` is a
    /// StrictObject of `code` + `message` only, so `details` is always
    /// skipped regardless of its value.
    #[serde(rename = "details", skip_serializing)]
    pub details: Option<serde_json::Value>,
}
