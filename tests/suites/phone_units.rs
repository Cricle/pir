//! Phone builtin unit tests moved out of `src/plugins/builtin/phone/**`
//! inline `#[cfg(test)]` modules:
//!
//! - `src/plugins/builtin/phone/session.rs` → `pending_ui_tests`: the
//!   bridge's pending-dialog accounting across parent and pool-child dialogs
//!   (tokio test). Everything it touches (`SessionBridge::start`,
//!   `BridgeConfig`, `FnHostCommands`, `subscribe`, `pending_ui_request`,
//!   `handle_client_line`) is public.
//! - `src/plugins/builtin/phone/remote.rs` `tests` → the relay `resync` line
//!   synthesis (`resync_client_lines`, a `#[doc(hidden)] pub` seam now; the
//!   only production caller is the WS client task loop).
//! - `src/plugins/builtin/phone/mod.rs` `tree_bridge_tests` → the child
//!   `get_tree` → phone rows bridge (`child_tree_to_phone_tree`) and the
//!   `PI_PHONE_CUSTOM_EVENTS` parse (`parse_custom_events`), both
//!   `#[doc(hidden)] pub` seams.
#![cfg(feature = "phone")]

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};

use pir::plugins::builtin::phone::session::{
    BridgeConfig, FnHostCommands, ForwardedCommand, HostCommands, HostReply, SessionBridge,
};

// ---------------------------------------------------------------------------
// src/plugins/builtin/phone/session.rs — pending_ui_tests
// ---------------------------------------------------------------------------

fn test_bridge() -> (SessionBridge, tokio::sync::mpsc::Sender<Value>) {
    let host: Arc<dyn HostCommands> =
        Arc::new(FnHostCommands(|_command: ForwardedCommand| async {
            Ok(HostReply::default())
        }));
    SessionBridge::start(BridgeConfig::default(), host)
}

async fn wait_for<P: Fn() -> bool>(predicate: P, label: &str) {
    for _ in 0..200 {
        if predicate() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    panic!("timed out waiting for {label}");
}

#[tokio::test]
async fn pending_ui_request_tracks_dialog_events_and_responses() {
    let (bridge, host_tx) = test_bridge();
    let subscriber = bridge.subscribe().expect("subscribe");
    assert!(!bridge.pending_ui_request("parent-session"));

    host_tx
        .send(json!({
            "type": "extension_ui_request",
            "id": "ui-1",
            "method": "confirm",
            "title": "Continue?",
        }))
        .await
        .expect("send parent dialog");
    wait_for(
        || bridge.pending_ui_request("parent-session"),
        "parent dialog",
    )
    .await;
    assert!(!bridge.pending_ui_request("active-session-1"));

    // Notification-only methods do not block the agent.
    host_tx
        .send(json!({
            "type": "extension_ui_request",
            "id": "ui-2",
            "method": "notify",
            "message": "hi",
        }))
        .await
        .expect("send notify");
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(bridge.pending_ui_request("parent-session"));

    // Pool child dialogs arrive wrapped with their worker id.
    host_tx
        .send(json!({
            "channel": "rpc",
            "sessionWorkerId": "active-session-1",
            "payload": {
                "type": "extension_ui_request",
                "id": "ui-3",
                "method": "select",
            },
        }))
        .await
        .expect("send child dialog");
    wait_for(
        || bridge.pending_ui_request("active-session-1"),
        "child dialog",
    )
    .await;

    let handled = bridge
        .handle_client_line(
            subscriber.id(),
            "{\"kind\":\"rpc\",\"command\":{\"type\":\"extension_ui_response\",\"id\":\"ui-1\",\"confirmed\":false}}",
        )
        .await;
    assert!(handled);
    assert!(!bridge.pending_ui_request("parent-session"));
    assert!(bridge.pending_ui_request("active-session-1"));

    // Answering a child dialog targets the client's selected worker.
    let handled = bridge
        .handle_client_line(
            subscriber.id(),
            "{\"kind\":\"session-select\",\"sessionId\":\"active-session-1\"}",
        )
        .await;
    assert!(handled);
    let handled = bridge
        .handle_client_line(
            subscriber.id(),
            "{\"kind\":\"rpc\",\"command\":{\"type\":\"extension_ui_response\",\"id\":\"ui-3\",\"value\":\"yes\"}}",
        )
        .await;
    assert!(handled);
    assert!(!bridge.pending_ui_request("active-session-1"));
}

// ---------------------------------------------------------------------------
// src/plugins/builtin/phone/remote.rs — tests
// ---------------------------------------------------------------------------

fn parse_client_line(line: &str) -> Value {
    serde_json::from_str(line).expect("client line is JSON")
}

#[test]
fn resync_parent_session_only_refreshes() {
    use pir::plugins::builtin::phone::remote::resync_client_lines;

    let lines = resync_client_lines(Some("parent-session"));
    assert_eq!(lines.len(), 1, "parent resync must not synthesize a select");
    assert_eq!(parse_client_line(&lines[0])["kind"], "refresh");
}

#[test]
fn resync_pool_session_selects_then_refreshes() {
    use pir::plugins::builtin::phone::remote::resync_client_lines;

    let lines = resync_client_lines(Some("active-session-3"));
    assert_eq!(lines.len(), 2);
    let select = parse_client_line(&lines[0]);
    assert_eq!(select["kind"], "session-select");
    assert_eq!(select["sessionId"], "active-session-3");
    assert_eq!(parse_client_line(&lines[1])["kind"], "refresh");
}

#[test]
fn resync_pool_lookalike_id_still_selects() {
    use pir::plugins::builtin::phone::remote::resync_client_lines;

    let lines = resync_client_lines(Some("parent-session-2"));
    assert_eq!(lines.len(), 2);
    assert_eq!(
        parse_client_line(&lines[0])["sessionId"],
        "parent-session-2"
    );
}

#[test]
fn resync_without_session_id_only_refreshes() {
    use pir::plugins::builtin::phone::remote::resync_client_lines;

    let lines = resync_client_lines(None);
    assert_eq!(lines, vec!["{\"kind\":\"refresh\"}".to_owned()]);
}

// ---------------------------------------------------------------------------
// src/plugins/builtin/phone/mod.rs — tree_bridge_tests
// ---------------------------------------------------------------------------

/// The child RPC answers a nested forest (`get_tree`); the phone client
/// consumes flat rows. Cover ids, parents, depths, kinds, previews and the
/// leaf path in one tree.
#[test]
fn child_tree_is_flattened_into_phone_rows() {
    use pir::plugins::builtin::phone::child_tree_to_phone_tree;

    let data = json!({
        "tree": [{
            "id": "u1",
            "entry": {
                "type": "message",
                "id": "u1",
                "timestamp": "2026-11-09T19:00:00.000Z",
                "message": {
                    "role": "user",
                    "content": [{ "type": "text", "text": "hello\nworld" }]
                }
            },
            "label": null,
            "children": [{
                "id": "a1",
                "entry": {
                    "type": "message",
                    "id": "a1",
                    "parentId": "u1",
                    "timestamp": "2026-11-09T19:00:01.000Z",
                    "message": {
                        "role": "assistant",
                        "content": [{ "type": "text", "text": "hi\nthere" }]
                    }
                },
                "label": "reply",
                "children": []
            }]
        }],
        "leafId": "a1"
    });
    let tree = child_tree_to_phone_tree(&data, Some("s.jsonl".to_owned()));
    assert_eq!(tree["sessionFile"], "s.jsonl");
    assert_eq!(tree["currentLeafId"], "a1");
    assert_eq!(tree["currentPathIds"], json!(["u1", "a1"]));
    let nodes = tree["nodes"].as_array().expect("nodes");
    assert_eq!(nodes.len(), 2);
    assert_eq!(nodes[0]["id"], "u1");
    assert_eq!(nodes[0]["type"], "user_message");
    assert_eq!(nodes[0]["parentId"], Value::Null);
    assert_eq!(nodes[0]["summary"]["preview"], "hello world");
    assert_eq!(nodes[0]["summary"]["role"], "user");
    assert!(nodes[0]["timestamp"].as_u64().unwrap() > 0);
    assert_eq!(nodes[1]["id"], "a1");
    assert_eq!(nodes[1]["parentId"], "u1");
    assert_eq!(nodes[1]["depth"], 1);
    assert_eq!(nodes[1]["type"], "assistant_message");
    assert_eq!(nodes[1]["label"], "reply");
    assert_eq!(nodes[1]["summary"]["preview"], "hi there");
    assert_eq!(nodes[1]["summary"]["role"], "assistant");
}

#[test]
fn empty_child_tree_keeps_the_phone_envelope() {
    use pir::plugins::builtin::phone::child_tree_to_phone_tree;

    let tree = child_tree_to_phone_tree(&json!({ "tree": [], "leafId": null }), None);
    assert_eq!(tree["sessionFile"], Value::Null);
    assert_eq!(tree["currentLeafId"], Value::Null);
    assert_eq!(tree["currentPathIds"], json!([]));
    assert_eq!(tree["nodes"], json!([]));
}

#[test]
fn custom_events_are_filtered_deduped_and_order_stable() {
    use pir::plugins::builtin::phone::parse_custom_events;

    assert_eq!(parse_custom_events(""), Vec::<String>::new());
    assert_eq!(
        parse_custom_events(
            "phone.custom.b, phone.custom.a,phone.custom.b,not-custom,phone.custom.a"
        ),
        vec!["phone.custom.b".to_owned(), "phone.custom.a".to_owned()],
        "unsupported names are dropped, duplicates keep their first position"
    );
}
