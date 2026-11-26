//! Pure phone relay envelope + resync shaping.
//!
//! These helpers build the JSON lines the agent sends to the relay. They are
//! host-neutral: the WebSocket transport, session bridge and catalog
//! subscription stay in the host.

use serde_json::{json, Value};

/// The bridge's default session id (the CLI parent session).
pub const PARENT_SESSION_ID: &str = "parent-session";

/// Capabilities advertised in the agent `hello` frame.
pub const AGENT_CAPABILITIES: [&str; 5] = [
    "sessions",
    "tools",
    "extension-ui.v2",
    "session-pool",
    "custom-events.v1",
];

/// Build the agent `hello` frame.
#[allow(clippy::too_many_arguments)]
pub fn hello_frame(
    device_id: &str,
    device_name: &str,
    platform: &str,
    cwd: Option<&str>,
    has_pending_ui_request: bool,
    last_activity_at: i64,
) -> Value {
    json!({
        "type": "hello",
        "version": 1,
        "deviceId": device_id,
        "deviceName": device_name,
        "platform": platform,
        "capabilities": AGENT_CAPABILITIES,
        "customEvents": ["phone.custom.*"],
        "sessions": [{
            "id": PARENT_SESSION_ID,
            "label": device_name,
            "kind": "parent",
            "cwd": cwd,
            "hasPendingUiRequest": has_pending_ui_request,
            "lastActivityAt": last_activity_at,
        }],
    })
}

/// One pool-session `session-upsert` frame built from a catalog row.
pub fn session_upsert_frame(row: &Value) -> Option<Value> {
    let id = row.get("id").and_then(Value::as_str)?;
    if id == PARENT_SESSION_ID {
        return None;
    }
    Some(json!({
        "type": "session-upsert",
        "session": {
            "id": id,
            "sessionId": row.get("sessionId").cloned().unwrap_or(Value::Null),
            "label": row.get("label").cloned().unwrap_or(Value::Null),
            "kind": row.get("kind").cloned().unwrap_or_else(|| json!("parallel")),
            "cwd": row.get("cwd").cloned().unwrap_or(Value::Null),
            "model": row.get("model").cloned().unwrap_or(Value::Null),
            "thinkingLevel": row.get("thinkingLevel").cloned().unwrap_or(Value::Null),
            "isStreaming": row.get("isStreaming").cloned().unwrap_or_else(|| json!(false)),
            "hasPendingUiRequest": row.get("hasPendingUiRequest").cloned().unwrap_or_else(|| json!(false)),
            "lastActivityAt": row.get("lastActivityAt").cloned().unwrap_or(Value::Null),
            "messageCount": row.get("messageCount").cloned().unwrap_or_else(|| json!(0)),
        }
    }))
}

/// A `session-remove` frame for one stale pool session.
pub fn session_remove_frame(session_id: &str) -> Value {
    json!({ "type": "session-remove", "sessionId": session_id })
}

/// Wrap a host event payload in the relay `frame` envelope.
pub fn frame_for(session_id: Option<&str>, payload: Value) -> Value {
    json!({
        "type": "frame",
        "sessionId": session_id.unwrap_or(PARENT_SESSION_ID),
        "payload": payload,
    })
}

/// Build the bridge client lines for a relay `resync` message.
///
/// The relay sends `resync` for the parent session whenever a phone joins.
/// That session is the bridge's default; synthesizing
/// `session-select parent-session` would route it through the parallel-session
/// pool and be rejected as an unknown parallel session (a spurious
/// `server/client-error` on the phone). Only pool children need an explicit
/// select. Every resync still asks the host for a fresh
/// status/catalog/snapshot.
pub fn resync_client_lines(session_id: Option<&str>) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(session_id) = session_id {
        if session_id != PARENT_SESSION_ID {
            lines.push(json!({ "kind": "session-select", "sessionId": session_id }).to_string());
        }
    }
    lines.push(r#"{"kind":"refresh"}"#.to_owned());
    lines
}

/// The remote connection states, as the wire discriminates them.
///
/// The host keeps the authoritative state cell; these are the values it stores
/// and the values [`remote_status_text`] renders. One definition so a host
/// cannot add a state the renderer treats as unknown.
pub const STATE_OFF: u8 = 0;
pub const STATE_CONNECTING: u8 = 1;
pub const STATE_CONNECTED: u8 = 2;
pub const STATE_RECONNECTING: u8 = 3;

/// The AGENT's status text for the current connection state.
pub fn remote_status_text(
    state: u8,
    server_url: &str,
    device_name: &str,
    device_id: &str,
    connected_secs: Option<u64>,
    last_error: &str,
) -> String {
    match state {
        STATE_CONNECTED => format!(
            "Pi Phone connected to {} as {} ({}) for {}s",
            server_url,
            device_name,
            device_id,
            connected_secs.unwrap_or(0)
        ),
        STATE_CONNECTING => format!("Pi Phone connecting to {server_url}…"),
        STATE_RECONNECTING => format!(
            "Pi Phone reconnecting to {}{}",
            server_url,
            if last_error.is_empty() {
                String::new()
            } else {
                format!(": {last_error}")
            }
        ),
        STATE_OFF => "Pi Phone disconnected".to_owned(),
        _ => "Pi Phone disconnected".to_owned(),
    }
}
