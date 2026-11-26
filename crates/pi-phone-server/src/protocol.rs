use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const PROTOCOL_VERSION: u32 = 1;

const MAX_DEVICE_ID_BYTES: usize = 256;
const MAX_DEVICE_NAME_BYTES: usize = 256;
const MAX_PLATFORM_BYTES: usize = 128;
const MAX_SESSION_ID_BYTES: usize = 256;
const MAX_SESSION_NAME_BYTES: usize = 512;
const MAX_SESSION_CWD_BYTES: usize = 4096;
const MAX_SESSION_ERROR_BYTES: usize = 4096;
const MAX_MODEL_FIELD_BYTES: usize = 256;
const MAX_CUSTOM_EVENTS: usize = 128;
const MAX_CUSTOM_EVENT_NAME_BYTES: usize = 128;

/// Validate agent-controlled hello metadata before it is stored or broadcast.
pub fn validate_agent_hello(hello: &AgentHello) -> Result<(), &'static str> {
    if hello.device_id.is_empty() || hello.device_id.len() > MAX_DEVICE_ID_BYTES {
        return Err("deviceId is empty or too long");
    }
    if hello
        .device_name
        .as_deref()
        .is_some_and(|value| value.len() > MAX_DEVICE_NAME_BYTES)
    {
        return Err("deviceName is too long");
    }
    if hello
        .platform
        .as_deref()
        .is_some_and(|value| value.len() > MAX_PLATFORM_BYTES)
    {
        return Err("platform is too long");
    }
    if hello.custom_events.len() > MAX_CUSTOM_EVENTS
        || hello
            .custom_events
            .iter()
            .any(|value| value.len() > MAX_CUSTOM_EVENT_NAME_BYTES)
    {
        return Err("customEvents exceeds the metadata limit");
    }
    for descriptor in &hello.sessions {
        validate_session_descriptor(descriptor)?;
    }
    Ok(())
}

/// Validate agent-controlled session metadata before it is stored or broadcast.
pub fn validate_session_descriptor(descriptor: &SessionDescriptor) -> Result<(), &'static str> {
    if descriptor.id.is_empty() || descriptor.id.len() > MAX_SESSION_ID_BYTES {
        return Err("session id is empty or too long");
    }
    if descriptor
        .session_id
        .as_deref()
        .is_some_and(|value| value.len() > MAX_SESSION_ID_BYTES)
        || descriptor
            .label
            .as_deref()
            .is_some_and(|value| value.len() > MAX_SESSION_NAME_BYTES)
        || descriptor
            .kind
            .as_deref()
            .is_some_and(|value| value.len() > 64)
        || descriptor
            .cwd
            .as_deref()
            .is_some_and(|value| value.len() > MAX_SESSION_CWD_BYTES)
        || descriptor
            .thinking_level
            .as_deref()
            .is_some_and(|value| value.len() > 64)
        || descriptor
            .last_error
            .as_deref()
            .is_some_and(|value| value.len() > MAX_SESSION_ERROR_BYTES)
    {
        return Err("session metadata field exceeds its limit");
    }
    if descriptor.model.as_ref().is_some_and(|model| {
        model.provider.len() > MAX_MODEL_FIELD_BYTES
            || model.id.len() > MAX_MODEL_FIELD_BYTES
            || model
                .name
                .as_deref()
                .is_some_and(|value| value.len() > MAX_MODEL_FIELD_BYTES)
    }) {
        return Err("model metadata field exceeds its limit");
    }
    Ok(())
}

pub const SERVER_CAPABILITIES: &[&str] = &[
    "sessions",
    "tools",
    "extension-ui.v2",
    "custom-events.v1",
    "session-pool",
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelDescriptor {
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionDescriptor {
    pub id: String,
    /// Stable agent-side session id (`id` is the catalog/worker id: for the
    /// CLI parent row they differ, e.g. `parent-session` vs the native id).
    #[serde(default)]
    pub session_id: Option<String>,
    /// True while an agent UI request (select/confirm/input/editor) is waiting
    /// for an answer. Only agents that track the state report it.
    #[serde(default)]
    pub has_pending_ui_request: Option<bool>,
    /// Epoch milliseconds of the last agent activity. Catalog rows fall back
    /// to the relay `updatedAt` when the agent does not report it.
    #[serde(default)]
    pub last_activity_at: Option<i64>,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub model: Option<ModelDescriptor>,
    #[serde(default)]
    pub thinking_level: Option<String>,
    #[serde(default)]
    pub is_streaming: Option<bool>,
    #[serde(default)]
    pub is_compacting: Option<bool>,
    #[serde(default)]
    pub message_count: Option<u64>,
    #[serde(default)]
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentHello {
    pub version: u32,
    pub device_id: String,
    #[serde(default)]
    pub device_name: Option<String>,
    #[serde(default)]
    pub platform: Option<String>,
    #[serde(default)]
    pub sessions: Vec<SessionDescriptor>,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub custom_events: Vec<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HelloOk {
    pub r#type: &'static str,
    pub protocol_version: u32,
    pub device_id: String,
    pub session_ids: Vec<String>,
    pub capabilities: Vec<&'static str>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentEnvelope {
    pub r#type: String,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub session: Option<SessionDescriptor>,
    #[serde(default)]
    pub payload: Option<Value>,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientEnvelope {
    pub kind: String,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub cursor: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub q: Option<String>,
    #[serde(default)]
    pub device_id: Option<String>,
    /// Client correlation id. The PWA sends `requestId` as a number
    /// (`Date.now()` in `app/transport.js`), while the baseline server used a
    /// string; accept either instead of rejecting the whole envelope.
    #[serde(default)]
    pub request_id: Option<Value>,
    #[serde(default)]
    pub command: Option<Value>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateDeviceRequest {
    pub name: String,
    #[serde(default)]
    pub platform: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionRow {
    pub id: String,
    pub user_id: String,
    pub device_id: String,
    pub local_id: String,
    /// `#[serde(default)]` keeps rows persisted before these fields existed
    /// readable after an upgrade.
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub has_pending_ui_request: Option<bool>,
    #[serde(default)]
    pub last_activity_at: Option<i64>,
    pub label: Option<String>,
    pub kind: String,
    pub cwd: Option<String>,
    pub model_provider: Option<String>,
    pub model_id: Option<String>,
    pub model_name: Option<String>,
    pub thinking_level: Option<String>,
    pub is_streaming: bool,
    pub is_compacting: bool,
    pub message_count: u64,
    pub last_error: Option<String>,
    pub online: bool,
    pub created_at: i64,
    pub updated_at: i64,
    pub last_seen_at: Option<i64>,
    pub deleted_at: Option<i64>,
}

/// Serialize a JSON value once into a shared, immutable WebSocket text line.
/// Broadcast paths clone the `Arc` instead of copying the full payload per
/// subscriber.
pub fn json_line(value: Value) -> Arc<str> {
    Arc::from(value.to_string())
}

/// Serialize by reference, avoiding a deep clone of large snapshot trees.
pub fn json_line_ref(value: &Value) -> Arc<str> {
    Arc::from(serde_json::to_string(value).unwrap_or_else(|_| "{}".to_owned()))
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}
