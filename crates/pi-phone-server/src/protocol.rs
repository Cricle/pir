use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const PROTOCOL_VERSION: u32 = 1;

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

#[cfg(test)]
mod tests {
    use super::ClientEnvelope;

    #[test]
    fn client_envelope_accepts_numeric_and_string_request_id() {
        // The PWA sends `Date.now()`; older clients/tests send strings.
        let numeric: ClientEnvelope = serde_json::from_str(
            r#"{"kind":"load-earlier-messages","before":0,"count":20,"requestId":1729000000000}"#,
        )
        .expect("numeric requestId must parse");
        assert_eq!(
            numeric.request_id.unwrap(),
            serde_json::json!(1729000000000u64)
        );

        let string: ClientEnvelope =
            serde_json::from_str(r#"{"kind":"load-earlier-messages","requestId":"req-1"}"#)
                .expect("string requestId must parse");
        assert_eq!(string.request_id.unwrap(), serde_json::json!("req-1"));
    }
}
