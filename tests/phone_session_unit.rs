//! Unit-level phone session helpers: bounded line reading, client command
//! parsing, and host-event windowing/truncation.
//!
//! Moved out of `src/plugins/builtin/phone/session.rs` (all tests live under
//! `tests/`). The helpers are exposed as `#[doc(hidden)]` seams.

use serde_json::json;
use tokio::io::BufReader;

use pir::plugins::builtin::phone::session::{
    map_host_event, parse_client_message, read_bounded_line, BridgeConfig, MappedEvent,
    PhoneCommand,
};

#[tokio::test(flavor = "current_thread")]
async fn bounded_line_caps_without_newline() {
    let mut reader = BufReader::new(&b"1234567890\nabc"[..]);
    let mut buffer = Vec::new();
    let first = read_bounded_line(&mut reader, &mut buffer, 4)
        .await
        .expect("read");
    assert_eq!(first, Some(true));
    assert_eq!(buffer, b"1234");
    let second = read_bounded_line(&mut reader, &mut buffer, 4)
        .await
        .expect("read");
    assert_eq!(second, Some(false));
    assert_eq!(buffer, b"abc");
}

#[test]
fn parses_scope_commands() {
    let prompt = parse_client_message(&json!({
        "kind": "rpc",
        "command": {
            "type": "prompt",
            "message": "hello",
            "images": [],
            "streamingBehavior": "steer",
        },
    }))
    .expect("prompt");
    assert!(matches!(
        prompt,
        PhoneCommand::Prompt {
            ref message,
            streaming_behavior: Some(ref behavior),
            ..
        } if message == "hello" && behavior == "steer"
    ));

    let reload = parse_client_message(&json!({
        "kind": "local-command",
        "command": "reload",
    }))
    .expect("reload");
    assert_eq!(reload, PhoneCommand::Reload);

    let tree = parse_client_message(&json!({
        "kind": "rpc",
        "command": { "type": "phone_get_tree" },
    }))
    .expect("tree");
    assert_eq!(tree, PhoneCommand::PhoneGetTree);
}

#[test]
fn snapshot_window_and_tool_cap() {
    let config = BridgeConfig::default();
    let messages: Vec<serde_json::Value> = (0..65)
        .map(|index| json!({ "role": "user", "content": index.to_string() }))
        .collect();
    let mapped = map_host_event(
        &json!({ "type": "snapshot", "messages": messages, "commands": [] }),
        &config,
    )
    .expect("snapshot");
    let MappedEvent::Snapshot { envelope, .. } = mapped else {
        panic!("expected snapshot");
    };
    assert_eq!(envelope["messages"].as_array().unwrap().len(), 60);
    assert_eq!(envelope["historyTruncated"], true);
    assert_eq!(envelope["totalMessageCount"], 65);

    let big = "x".repeat(40_000);
    let padded = map_host_event(
        &json!({
            "type": "tool_execution_end",
            "toolCallId": "tool-1",
            "result": { "content": [{ "type": "text", "text": big }] },
        }),
        &config,
    )
    .expect("tool");
    let MappedEvent::Line(envelope) = padded else {
        panic!("expected line");
    };
    let text = envelope["payload"]["result"]["content"][0]["text"]
        .as_str()
        .unwrap();
    assert!(text.contains("truncated"));
    assert!(text.chars().count() <= config.tool_text_cap_chars + 80);
}
