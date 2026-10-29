//! Message, content-block, tool, and context types.

use std::collections::BTreeMap;

use super::model::ThinkingLevel;
use super::stream::{StopReason, Usage};
use super::{ApiId, JsonValue, ProviderId};

// ---------------------------------------------------------------------------
// Content blocks
// ---------------------------------------------------------------------------

/// A block of text content.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
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

/// A block of thinking/reasoning content.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
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

/// A block of image content (base64).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ImageContent {
    pub data: String,
    #[serde(rename = "mimeType")]
    pub mime_type: String,
}

/// A tool call produced by the model.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: BTreeMap<String, JsonValue>,
    /// Google-specific: opaque signature for reusing thought context.
    ///
    /// Wire name is upstream's camelCase `thoughtSignature`; the snake_case
    /// `thought_signature` is accepted on deserialize so messages persisted
    /// before this rename still round-trip.
    #[serde(rename = "thoughtSignature", alias = "thought_signature", default)]
    pub thought_signature: Option<String>,
    /// OpenAI Responses namespace for dynamically loaded or namespaced tools.
    #[serde(default)]
    pub namespace: Option<String>,
    /// Set when the model's complete arguments text was not a valid JSON
    /// object (upstream `ToolCall.argumentsError`). Optional on the wire:
    /// the manual serializer below omits the key entirely when it is `None`;
    /// the snake_case spelling is accepted on deserialize for backward
    /// compatibility with messages persisted before the camelCase rename.
    #[serde(default, rename = "argumentsError", alias = "arguments_error")]
    pub arguments_error: Option<String>,
}

impl serde::Serialize for ToolCall {
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
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
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

impl serde::Serialize for ContentBlock {
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
// Deferred handle
// ---------------------------------------------------------------------------

/// A handle for deferred (async) responses.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeferredHandle {
    pub provider: ProviderId,
    /// CamelCase on the wire; also accepts the snake_case spelling written by
    /// older builds.
    #[serde(alias = "model_id")]
    pub model_id: String,
    pub api: ApiId,
    /// Provider token, such as a response id or batch id plus row id.
    pub id: String,
    #[serde(default, alias = "expires_at", skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
    #[serde(
        default,
        alias = "poll_after_ms",
        skip_serializing_if = "Option::is_none"
    )]
    pub poll_after_ms: Option<u64>,
    /// Provider conversion data required to reconstruct the final assistant message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<JsonValue>,
}

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

/// A user message.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct UserMessage {
    pub role: UserRole,
    pub content: Vec<ContentBlock>,
    /// Complete definitions of tools that become available at this point
    /// (upstream `SystemMessage.toolsAdded`; only set on system messages).
    #[serde(
        rename = "toolsAdded",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub tools_added: Option<Vec<Tool>>,
    /// Tools that stop being available at this point
    /// (upstream `SystemMessage.toolsRemoved`; only set on system messages).
    #[serde(
        rename = "toolsRemoved",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub tools_removed: Option<Vec<ToolReference>>,
    /// Named prompt sections rendered verbatim after `content` (upstream
    /// `SystemMessage.sections`). The leading system message declares them;
    /// later system messages replace sections by name, and `None` removes one.
    ///
    /// `None` = the field is absent; `Some(map)` = present, where a `None`
    /// value is the upstream `null` (remove the section). A map is used
    /// because the wire shape is a JSON object of `name -> string | null`.
    ///
    /// Ordering is normalized: upstream keeps JavaScript object insertion
    /// order, while `BTreeMap` iterates by name. That is sound here because
    /// replay only depends on per-name set/delete order *across* messages, not
    /// on the order within one message's map, and the durable prompt planner
    /// renders the current sections in its own fixed order anyway.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sections: Option<BTreeMap<String, Option<String>>>,
    /// Unix timestamp in milliseconds.
    pub timestamp: u64,
}

impl<'de> serde::Deserialize<'de> for UserMessage {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        /// Upstream `SystemMessage.content` / `UserMessage.content` accept a
        /// plain string OR an array of content blocks. pir always stores the
        /// array shape (the form every adapter consumes); a bare string
        /// deserializes to one text block.
        struct ContentField(Vec<ContentBlock>);

        impl<'de> serde::Deserialize<'de> for ContentField {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                #[derive(serde::Deserialize)]
                #[serde(untagged)]
                enum Wire {
                    Text(String),
                    Blocks(Vec<ContentBlock>),
                }

                match Wire::deserialize(deserializer)? {
                    Wire::Text(text) => Ok(ContentField(vec![ContentBlock::Text(TextContent {
                        text,
                        text_signature: None,
                    })])),
                    Wire::Blocks(blocks) => Ok(ContentField(blocks)),
                }
            }
        }

        #[derive(serde::Deserialize)]
        struct Wire {
            role: UserRole,
            content: ContentField,
            #[serde(rename = "toolsAdded", default)]
            tools_added: Option<Vec<Tool>>,
            #[serde(rename = "toolsRemoved", default)]
            tools_removed: Option<Vec<ToolReference>>,
            #[serde(default)]
            sections: Option<BTreeMap<String, Option<String>>>,
            timestamp: u64,
        }

        let wire = Wire::deserialize(deserializer)?;
        Ok(Self {
            role: wire.role,
            content: wire.content.0,
            tools_added: wire.tools_added,
            tools_removed: wire.tools_removed,
            sections: wire.sections,
            timestamp: wire.timestamp,
        })
    }
}

/// A tool removed by name (upstream `ToolReference`).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ToolReference {
    pub name: String,
}

/// Role variant for user messages (supports system vs developer).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum UserRole {
    #[serde(rename = "user")]
    User,
    #[serde(rename = "system")]
    System,
    #[serde(rename = "developer")]
    Developer,
}

/// A diagnostic entry for debugging provider/runtime issues.
///
/// Wire shape matches upstream `DiagnosticInfo`:
/// `{type, timestamp, error: {name?, message, stack?, code?}, details?}`.
/// The Rust fields keep the historical flat names (`error_message`,
/// `error_name`, `error_stack`, `error_code`) so existing construction sites
/// stay concise; the serde impls below map them into/out of the nested
/// `error` object. Deserialization also accepts the legacy flat
/// `error_message` / `error_name` fields emitted before this reshape, so
/// persisted diagnostics still load.
#[derive(Debug, Clone, PartialEq)]
pub struct DiagnosticInfo {
    pub diag_type: String,
    pub timestamp: u64,
    pub error_message: Option<String>,
    pub error_name: Option<String>,
    pub error_stack: Option<String>,
    /// Upstream `DiagnosticErrorInfo.code`: `string | number`.
    pub error_code: Option<JsonValue>,
    pub details: Option<BTreeMap<String, JsonValue>>,
}

impl serde::Serialize for DiagnosticInfo {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;

        let has_error = self.error_message.is_some()
            || self.error_name.is_some()
            || self.error_stack.is_some()
            || self.error_code.is_some();
        let mut state = serializer.serialize_struct("DiagnosticInfo", 4)?;
        state.serialize_field("type", &self.diag_type)?;
        state.serialize_field("timestamp", &self.timestamp)?;
        // Upstream nests the error under `error`. Diagnostics with no captured
        // error at all (the internal rewrite note) omit the object so a
        // serialize/deserialize round trip keeps `error_message: None`.
        if has_error {
            state.serialize_field(
                "error",
                &DiagnosticErrorOut {
                    name: self.error_name.as_deref(),
                    message: self.error_message.as_deref().unwrap_or_default(),
                    stack: self.error_stack.as_deref(),
                    code: self.error_code.as_ref(),
                },
            )?;
        }
        if self.details.is_some() {
            state.serialize_field("details", &self.details)?;
        }
        state.end()
    }
}

/// Wire form of upstream `DiagnosticInfo.error` on the serialize side.
#[derive(serde::Serialize)]
struct DiagnosticErrorOut<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<&'a str>,
    message: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    stack: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<&'a JsonValue>,
}

/// Wire form of upstream `DiagnosticInfo` on the deserialize side: nested
/// `error` plus the legacy flat fields as fallbacks.
#[derive(serde::Deserialize)]
struct DiagnosticInfoWire {
    #[serde(rename = "type")]
    diag_type: String,
    timestamp: u64,
    #[serde(default)]
    error: Option<DiagnosticErrorWire>,
    #[serde(default, alias = "errorMessage")]
    error_message: Option<String>,
    #[serde(default, alias = "errorName")]
    error_name: Option<String>,
    #[serde(default, alias = "errorStack")]
    error_stack: Option<String>,
    #[serde(default, alias = "errorCode")]
    error_code: Option<JsonValue>,
    #[serde(default)]
    details: Option<BTreeMap<String, JsonValue>>,
}

/// Wire form of upstream `DiagnosticInfo.error` on the deserialize side.
#[derive(serde::Deserialize)]
struct DiagnosticErrorWire {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    stack: Option<String>,
    #[serde(default)]
    code: Option<JsonValue>,
}

impl<'de> serde::Deserialize<'de> for DiagnosticInfo {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = DiagnosticInfoWire::deserialize(deserializer)?;
        // The serializer writes `message: ""` when the diagnostic only carries
        // a name/stack/code (upstream's `message` is required inside `error`).
        // An empty wire message is therefore treated as the absent Rust value,
        // so stack/code-only diagnostics round-trip exactly.
        let flat_message = non_empty(wire.error_message);
        let (error_name, error_message, error_stack, error_code) = match wire.error {
            Some(error) => (
                error.name.or(wire.error_name),
                non_empty(error.message).or(flat_message),
                error.stack.or(wire.error_stack),
                error.code.or(wire.error_code),
            ),
            None => (
                wire.error_name,
                flat_message,
                wire.error_stack,
                wire.error_code,
            ),
        };
        Ok(Self {
            diag_type: wire.diag_type,
            timestamp: wire.timestamp,
            error_message,
            error_name,
            error_stack,
            error_code,
            details: wire.details,
        })
    }
}

/// An assistant message (model response).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AssistantMessage {
    pub role: AssistantRole,
    pub content: Vec<ContentBlock>,
    pub api: ApiId,
    pub provider: ProviderId,
    pub model: String,
    /// Concrete model id when different from the requested model (e.g. OpenRouter `auto`).
    #[serde(
        rename = "responseModel",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub response_model: Option<String>,
    /// Provider-specific response/message identifier.
    #[serde(
        rename = "responseId",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub response_id: Option<String>,
    /// Redacted provider/runtime diagnostics for failures and recoveries.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<DiagnosticInfo>,
    pub usage: Usage,
    #[serde(rename = "stopReason")]
    pub stop_reason: StopReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deferred: Option<DeferredHandle>,
    #[serde(
        rename = "errorMessage",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub error_message: Option<String>,
    /// Provider raw stop reason for debugging.
    #[serde(
        rename = "rawStopReason",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub raw_stop_reason: Option<String>,
    /// Provider indication of whether the model explicitly ended its turn.
    #[serde(rename = "endTurn", default, skip_serializing_if = "Option::is_none")]
    pub end_turn: Option<bool>,
    /// Exact provider-native effort level used for this response
    /// (upstream `AssistantMessage.providerThinkingLevel`). Absent for legacy
    /// or unmanaged responses.
    #[serde(
        rename = "providerThinkingLevel",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub provider_thinking_level: Option<String>,
    /// Pi thinking level the agent loop requested for this response
    /// (upstream `AssistantMessage.thinkingLevel`). Recorded per turn by the
    /// agent loop; absent for legacy responses.
    #[serde(
        rename = "thinkingLevel",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub thinking_level: Option<ThinkingLevel>,
    /// Milliseconds from `timestamp` until the response ended, measured with a
    /// monotonic clock (upstream `AssistantMessage.durationMs`). Set by the
    /// event stream on the final message of a response it saw start; absent
    /// for legacy messages.
    #[serde(
        rename = "durationMs",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub duration_ms: Option<u64>,
    /// Unix timestamp in milliseconds.
    pub timestamp: u64,
}

impl Default for AssistantMessage {
    fn default() -> Self {
        Self {
            role: AssistantRole::Assistant,
            content: Vec::new(),
            api: String::new(),
            provider: String::new(),
            model: String::new(),
            response_model: None,
            response_id: None,
            diagnostics: Vec::new(),
            usage: Usage::default(),
            stop_reason: StopReason::Pending,
            deferred: None,
            error_message: None,
            raw_stop_reason: None,
            end_turn: None,
            provider_thinking_level: None,
            thinking_level: None,
            duration_ms: None,
            timestamp: 0,
        }
    }
}

/// Role variant for assistant messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum AssistantRole {
    #[serde(rename = "assistant")]
    Assistant,
}

/// A tool result message.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ToolResultMessage {
    pub role: ToolResultRole,
    #[serde(rename = "toolCallId")]
    pub tool_call_id: String,
    #[serde(rename = "toolName")]
    pub tool_name: String,
    pub content: Vec<ContentBlock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<JsonValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    /// Names that became available after this result.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub added_tool_names: Vec<String>,
    /// Machine-readable result matching the tool's output schema, for
    /// programmatic callers (upstream `AgentToolResult.structuredContent`).
    /// Not sent to the model; `content` remains the model-facing result.
    ///
    /// SESSION-WRITE NOTE: upstream's persisted `ToolResultMessage`
    /// (`packages/ai/src/types.ts:625-637`) has no such field —
    /// `createToolResultMessage` (`packages/agent/src/agent-loop.ts:930-946`)
    /// copies only `content`/`details`/`usage`, so `structuredContent` lives
    /// on the execution-time `AgentToolResult` and never in the session
    /// record. pir mirrors that: this field is live-run state, and
    /// [`crate::session_manager::types::StoredMessage::from`] clears it on the
    /// session-write path (see `session_tool_result` there). Consumers that
    /// need it (codemode scripts, `tool_result` handlers) read the live
    /// result, not a replayed message.
    #[serde(
        rename = "structuredContent",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub structured_content: Option<JsonValue>,
    /// Calls this tool made to other tools. Kept for the session record; not
    /// sent to the model (upstream `ToolResultMessage.nestedCalls`).
    #[serde(
        rename = "nestedCalls",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub nested_calls: Option<NestedToolCalls>,
    #[serde(rename = "isError")]
    pub is_error: bool,
    /// Milliseconds the tool's execution took, measured with a monotonic clock
    /// (upstream `ToolResultMessage.durationMs`). Absent for legacy results.
    #[serde(
        rename = "durationMs",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub duration_ms: Option<u64>,
    /// Unix timestamp in milliseconds.
    pub timestamp: u64,
}

/// A tool call that another tool made while it ran, for example from a codemode
/// script (upstream `NestedToolCallRecord`). Results are not recorded.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct NestedToolCallRecord {
    pub id: String,
    pub name: String,
    /// Omitted when over the size limits; `argumentsBytes` then gives their size.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments: Option<BTreeMap<String, JsonValue>>,
    /// UTF-8 size of the arguments as JSON, set when `arguments` is omitted.
    #[serde(
        rename = "argumentsBytes",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub arguments_bytes: Option<u64>,
    pub status: NestedToolCallStatus,
    #[serde(
        rename = "durationMs",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub duration_ms: Option<u64>,
    /// Error text, truncated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Outcome of a nested tool call (upstream `NestedToolCallRecord.status`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum NestedToolCallStatus {
    #[serde(rename = "ok")]
    Ok,
    #[serde(rename = "error")]
    Error,
    /// The call was still running when the calling tool finished.
    #[serde(rename = "unfinished")]
    Unfinished,
}

/// Bounded record of the nested calls a tool made
/// (upstream `NestedToolCalls`).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct NestedToolCalls {
    pub calls: Vec<NestedToolCallRecord>,
    /// False when calls were dropped, arguments omitted, or calls had not
    /// finished.
    pub complete: bool,
}

/// Role variant for tool result messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ToolResultRole {
    #[serde(rename = "toolResult")]
    ToolResult,
}

/// Any message type in a conversation.
///
/// Wire shape: one JSON object per message, `{"role": "...", ...}`, with a
/// single `role` key naming the variant. The derive cannot express this union:
/// every payload struct carries a `role` field of its own, so serde's
/// internally tagged representation (`#[serde(tag = "role")]`) wrote the key
/// twice (serde#2949 — JSON parsers keep the last copy) and, on the way back
/// in, its `TaggedContentVisitor` consumed the tag before the payload was
/// deserialized, leaving the payload's `role` field permanently missing. Every
/// round trip therefore failed — the extension bridge's
/// `JSON.parse`/`JSON.stringify` hop included — so the tag is mapped by hand
/// below instead of by the derive.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    User(UserMessage),
    System(UserMessage),
    Developer(UserMessage),
    Assistant(AssistantMessage),
    ToolResult(ToolResultMessage),
}

/// The wire `role` tags of [`Message`], in variant order (the `unknown
/// variant` list of the deserializer).
const MESSAGE_ROLE_TAGS: &[&str] = &["user", "system", "developer", "assistant", "toolResult"];

impl Message {
    /// This message's wire `role` tag (the JSON `role` key).
    pub fn role(&self) -> &'static str {
        match self {
            Message::User(_) => "user",
            Message::System(_) => "system",
            Message::Developer(_) => "developer",
            Message::Assistant(_) => "assistant",
            Message::ToolResult(_) => "toolResult",
        }
    }
}

impl serde::Serialize for Message {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        // Each payload owns its `role` key, so serializing the payload is the
        // flat `{"role": ..., ...}` shape with exactly one `role` key (writing
        // the variant tag on top of it is what duplicated the key).
        match self {
            Message::User(payload) | Message::System(payload) | Message::Developer(payload) => {
                payload.serialize(serializer)
            }
            Message::Assistant(payload) => payload.serialize(serializer),
            Message::ToolResult(payload) => payload.serialize(serializer),
        }
    }
}

impl<'de> serde::Deserialize<'de> for Message {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error as _;
        use serde_json::from_value;

        // `role` is both the variant tag and a payload field, so the whole
        // object is buffered and handed to the payload (the tag-stripped
        // remainder the derive deserializes no longer has it).
        let mut object = match serde_json::Value::deserialize(deserializer)? {
            serde_json::Value::Object(object) => object,
            _ => return Err(D::Error::custom("message must be a JSON object")),
        };
        let role = match object.get("role") {
            Some(serde_json::Value::String(role)) => role.clone(),
            Some(_) => return Err(D::Error::custom("message `role` must be a string")),
            None => return Err(D::Error::missing_field("role")),
        };
        // The tag is authoritative and is written back into the payload: user,
        // system and developer share one payload struct, so the role the tag
        // names is exactly the role the payload deserializes with.
        object.insert("role".to_owned(), serde_json::Value::String(role.clone()));
        let payload = serde_json::Value::Object(object);
        let parsed = match role.as_str() {
            "user" => from_value::<UserMessage>(payload).map(Message::User),
            "system" => from_value::<UserMessage>(payload).map(Message::System),
            "developer" => from_value::<UserMessage>(payload).map(Message::Developer),
            "assistant" => from_value::<AssistantMessage>(payload).map(Message::Assistant),
            "toolResult" => from_value::<ToolResultMessage>(payload).map(Message::ToolResult),
            other => return Err(D::Error::unknown_variant(other, MESSAGE_ROLE_TAGS)),
        };
        parsed.map_err(D::Error::custom)
    }
}

// ---------------------------------------------------------------------------
// Tool
// ---------------------------------------------------------------------------

/// A tool definition available to the model.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Tool {
    pub name: String,
    pub description: String,
    /// JSON Schema for the tool's parameters.
    pub parameters: JsonValue,
    /// Whether to use constrained sampling for this tool.
    ///
    /// Wire name is upstream's camelCase `constrainedSampling` (read by the
    /// JS shim); the snake_case `constrained_sampling` is accepted on
    /// deserialize so tools persisted before this rename still round-trip.
    #[serde(
        rename = "constrainedSampling",
        alias = "constrained_sampling",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub constrained_sampling: Option<ConstrainedSamplingConfig>,
}

/// Constrained sampling configuration for a tool.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type")]
pub enum ConstrainedSamplingConfig {
    #[serde(rename = "json_schema")]
    JsonSchema { strict: StrictPreference },
    #[serde(rename = "grammar")]
    Grammar { variants: BTreeMap<String, String> },
}

/// Whether strict mode is preferred or required.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum StrictPreference {
    #[serde(rename = "prefer")]
    Prefer,
    #[serde(rename = "require")]
    Require,
}

// ---------------------------------------------------------------------------
// Context
// ---------------------------------------------------------------------------

/// The full context for a model request.
///
/// This is the transcript only — upstream `Context` carries no telemetry
/// handle either. The explicit parent span for the request's
/// `pi.ai.request` telemetry rides
/// [`StreamOptions::telemetry`](crate::ai::types::StreamOptions::telemetry)
/// instead, so this persisted type keeps its exact wire shape and its
/// `PartialEq` semantics.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize, Default)]
pub struct Context {
    /// Wire name is upstream's camelCase `systemPrompt`; the snake_case
    /// spelling is accepted on deserialize so contexts persisted before the
    /// rename still load.
    #[serde(
        rename = "systemPrompt",
        alias = "system_prompt",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub system_prompt: Option<String>,
    #[serde(default)]
    pub messages: Vec<Message>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Tool>>,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn is_false(b: &bool) -> bool {
    !b
}

/// Treat an empty wire string as absent (see `DiagnosticInfo` deserialization).
fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|value| !value.is_empty())
}
