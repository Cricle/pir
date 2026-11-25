//! Unit suites for the relay's envelope/catalog helpers, moved out of
//! `src/ws.rs` and `src/protocol.rs`.
//!
//! Source hygiene forbids inline `#[cfg(test)]` modules in `src/`, so these
//! live here and call the same functions the relay calls. The helpers are
//! `pub` rather than `pub(crate)` because integration tests are a separate
//! crate; the relay itself is the only in-tree consumer.

use pir_phone_server::protocol::{ClientEnvelope, SessionDescriptor, SessionRow};
use pir_phone_server::ws::{apply_forward_id, catalog_row, descriptor_from_status};
use serde_json::json;

fn row(session_id: Option<&str>, pending: Option<bool>, last: Option<i64>) -> SessionRow {
    SessionRow {
        id: "device-a:parent-session".to_owned(),
        user_id: "user-a".to_owned(),
        device_id: "device-a".to_owned(),
        local_id: "parent-session".to_owned(),
        session_id: session_id.map(str::to_owned),
        has_pending_ui_request: pending,
        last_activity_at: last,
        label: Some("CLI session".to_owned()),
        kind: "parent".to_owned(),
        cwd: Some("/work".to_owned()),
        model_provider: Some("mock".to_owned()),
        model_id: Some("mock-1".to_owned()),
        model_name: Some("Mock".to_owned()),
        thinking_level: Some("medium".to_owned()),
        is_streaming: false,
        is_compacting: false,
        message_count: 3,
        last_error: None,
        online: true,
        created_at: 1000,
        updated_at: 2000,
        last_seen_at: Some(2000),
        deleted_at: None,
    }
}

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

#[test]
fn catalog_row_carries_pwa_fields_and_falls_back_to_updated_at() {
    let with_agent = catalog_row(&row(Some("native-session"), Some(true), Some(1500)), true);
    assert_eq!(with_agent["sessionId"], "native-session");
    assert_eq!(with_agent["hasPendingUiRequest"], true);
    assert_eq!(with_agent["lastActivityAt"], 1500);

    // Older agent descriptors omit the fields: the row must still be
    // renderable (PWA reads these directly).
    let fallback = catalog_row(&row(None, None, None), false);
    assert!(fallback["sessionId"].is_null());
    assert_eq!(fallback["hasPendingUiRequest"], false);
    assert_eq!(fallback["lastActivityAt"], 2000);
}

#[test]
fn descriptor_from_status_picks_up_pwa_fields() {
    let status = json!({
        "channel": "server",
        "event": "status",
        "data": {
            "sessionId": "native-session",
            "sessionName": "Mock session",
            "hasPendingUiRequest": true,
            "lastActivityAt": 1729000000000i64,
            "isStreaming": false
        }
    });
    let descriptor: SessionDescriptor =
        descriptor_from_status(&status, "parent-session").expect("status");
    assert_eq!(descriptor.session_id.as_deref(), Some("native-session"));
    assert_eq!(descriptor.has_pending_ui_request, Some(true));
    assert_eq!(descriptor.last_activity_at, Some(1729000000000));
}

#[test]
fn extension_ui_response_keeps_dialog_id_and_uses_request_id_for_correlation() {
    let mut dialog = json!({
        "kind": "rpc",
        "command": {"type": "extension_ui_response", "id": "ui-1", "confirmed": true},
    });
    assert_eq!(
        apply_forward_id(&mut dialog, "f-7-1").as_deref(),
        Some("ui-1")
    );
    assert_eq!(dialog["command"]["id"], "ui-1");
    assert_eq!(dialog["requestId"], "f-7-1");

    let mut prompt = json!({
        "kind": "rpc",
        "command": {"type": "prompt", "id": "p-1", "message": "hi"},
    });
    assert_eq!(
        apply_forward_id(&mut prompt, "f-7-2").as_deref(),
        Some("p-1")
    );
    assert_eq!(prompt["command"]["id"], "f-7-2");
    assert!(prompt.get("requestId").is_none());
}
