//! MCP protocol types (upstream `packages/mcp/src/protocol/types.ts` and
//! `content.ts`), camelCase wire serde.

use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

/// The version this client requests (upstream `LATEST_PROTOCOL_VERSION`).
pub const LATEST_PROTOCOL_VERSION: &str = "2025-11-25";

/// Versions the client accepts from a server (upstream
/// `SUPPORTED_PROTOCOL_VERSIONS`). Servers that do not support the requested
/// version answer with their own latest one, so older versions stay accepted
/// for servers built on older SDKs.
pub const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &[
    LATEST_PROTOCOL_VERSION,
    "2025-06-18",
    "2025-03-26",
    "2024-11-05",
];

/// Client identification sent with `initialize` (upstream `Implementation`).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Implementation {
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub title: Option<String>,
}

/// Server capabilities from `initialize` (upstream `ServerCapabilities`).
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct ServerCapabilities {
    #[serde(default)]
    pub tools: Option<ToolsCapability>,
    #[serde(default)]
    pub resources: Option<ResourcesCapability>,
    #[serde(default)]
    pub prompts: Option<PromptsCapability>,
    #[serde(default)]
    pub logging: Option<JsonValue>,
}

/// The `tools` capability; `listChanged` mirrors the server flag.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct ToolsCapability {
    pub list_changed: Option<bool>,
}

/// The `resources` capability.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct ResourcesCapability {
    pub subscribe: Option<bool>,
    pub list_changed: Option<bool>,
}

/// The `prompts` capability.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct PromptsCapability {
    pub list_changed: Option<bool>,
}

/// Tool annotations (upstream `ToolAnnotations`).
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct ToolAnnotations {
    pub title: Option<String>,
    pub read_only_hint: Option<bool>,
    pub destructive_hint: Option<bool>,
    pub idempotent_hint: Option<bool>,
    pub open_world_hint: Option<bool>,
}

/// One tool from `tools/list` (upstream `Tool`).
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct McpTool {
    pub name: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub input_schema: JsonValue,
    #[serde(default)]
    pub output_schema: Option<JsonValue>,
    #[serde(default)]
    pub annotations: Option<ToolAnnotations>,
}

/// `tools/list` result.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct ListToolsResult {
    #[serde(default)]
    pub tools: Vec<McpTool>,
    #[serde(default)]
    pub next_cursor: Option<String>,
}

/// One content block of a tool/resource/prompt result (upstream
/// `ContentBlock`).
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct ContentBlock {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub data: Option<String>,
    #[serde(default)]
    pub mime_type: Option<String>,
    /// `resource` blocks carry the embedded resource contents.
    #[serde(default)]
    pub resource: Option<ResourceContents>,
    /// `resource_link` blocks name an addressable resource.
    #[serde(default)]
    pub uri: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub size: Option<u64>,
}

/// Embedded resource contents (upstream `TextResourceContents` /
/// `BlobResourceContents`).
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct ResourceContents {
    pub uri: String,
    #[serde(default)]
    pub mime_type: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
    /// Base64-encoded binary contents.
    #[serde(default)]
    pub blob: Option<String>,
}

/// `tools/call` result (upstream `CallToolResult`).
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct CallToolResult {
    #[serde(default)]
    pub content: Vec<ContentBlock>,
    #[serde(default)]
    pub structured_content: Option<JsonValue>,
    #[serde(default)]
    pub is_error: Option<bool>,
    #[serde(default)]
    pub meta: Option<JsonValue>,
}

/// One resource from `resources/list` (upstream `Resource`).
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct McpResource {
    pub uri: String,
    pub name: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub mime_type: Option<String>,
    #[serde(default)]
    pub size: Option<u64>,
}

/// `resources/list` result.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct ListResourcesResult {
    #[serde(default)]
    pub resources: Vec<McpResource>,
    #[serde(default)]
    pub next_cursor: Option<String>,
}

/// One resource template from `resources/templates/list` (upstream
/// `ResourceTemplate`).
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct McpResourceTemplate {
    /// RFC 6570 URI template.
    pub uri_template: String,
    pub name: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub mime_type: Option<String>,
}

/// `resources/templates/list` result.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct ListResourceTemplatesResult {
    #[serde(default)]
    pub resource_templates: Vec<McpResourceTemplate>,
    #[serde(default)]
    pub next_cursor: Option<String>,
}

/// `resources/read` result.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct ReadResourceResult {
    #[serde(default)]
    pub contents: Vec<ResourceContents>,
}

/// One prompt from `prompts/list` (upstream `Prompt`).
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct McpPrompt {
    pub name: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub arguments: Option<Vec<PromptArgument>>,
}

/// One prompt argument.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct PromptArgument {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub required: Option<bool>,
}

/// `prompts/list` result.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct ListPromptsResult {
    #[serde(default)]
    pub prompts: Vec<McpPrompt>,
    #[serde(default)]
    pub next_cursor: Option<String>,
}

/// One message of a `prompts/get` result (upstream `PromptMessage`).
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct PromptMessage {
    pub role: String,
    #[serde(default)]
    pub content: Option<ContentBlock>,
}

/// `prompts/get` result.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct GetPromptResult {
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub messages: Vec<PromptMessage>,
}

/// The `notifications/message` logging payload (upstream
/// `LoggingMessageNotification`), forwarded to `mcp.log` (upstream
/// `packages/coding-agent/src/extensions/mcp/runtime.ts:386`).
#[derive(Debug, Clone, Deserialize, Default, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct LoggingMessage {
    #[serde(default)]
    pub level: Option<String>,
    #[serde(default)]
    pub logger: Option<String>,
    #[serde(default)]
    pub data: Option<JsonValue>,
}
