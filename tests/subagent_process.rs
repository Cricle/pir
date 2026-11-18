//! Process-executor JSON parsing and event-shape tests.

use pir::ai::types::{AssistantMessage, ContentBlock, Message, TextContent};
use pir::plugins::subagent::{dedupe_events_for_test, parse_event_for_test};

#[test]
fn parses_an_assistant_turn_end() {
    let message = Message::Assistant(AssistantMessage {
        content: vec![ContentBlock::Text(TextContent {
            text: "PROCESS_OK".to_owned(),
            text_signature: None,
        })],
        model: "fake-model".to_owned(),
        ..Default::default()
    });
    let line = serde_json::json!({
        "type": "turn_end",
        "turn": 1,
        "message": message,
    })
    .to_string();
    let messages = parse_event_for_test(&line).expect("turn_end parses");
    assert_eq!(messages.len(), 1);
    assert!(matches!(messages[0], Message::Assistant(_)));
}

#[test]
fn parses_a_message_end_without_a_turn() {
    let message = Message::Assistant(AssistantMessage {
        content: vec![ContentBlock::Text(TextContent {
            text: "PARTIAL".to_owned(),
            text_signature: None,
        })],
        model: "fake-model".to_owned(),
        ..Default::default()
    });
    let line = serde_json::json!({
        "type": "message_end",
        "message": message,
    })
    .to_string();
    assert_eq!(parse_event_for_test(&line).expect("message_end").len(), 1);
}

#[test]
fn ignores_non_message_events() {
    assert!(parse_event_for_test(r#"{"type":"tool_execution_start"}"#).is_none());
}

#[test]
fn message_end_and_turn_end_for_the_same_assistant_reply_are_kept_once() {
    let message = Message::Assistant(AssistantMessage {
        content: vec![ContentBlock::Text(TextContent {
            text: "ONE_REPLY".to_owned(),
            text_signature: None,
        })],
        model: "fake-model".to_owned(),
        ..Default::default()
    });
    let message_end = serde_json::json!({
        "type": "message_end",
        "turn": 1,
        "message": message,
    })
    .to_string();
    let turn_end = serde_json::json!({
        "type": "turn_end",
        "turn": 1,
        "message": message,
    })
    .to_string();
    let messages = dedupe_events_for_test(&[&message_end, &turn_end]);
    assert_eq!(messages.len(), 1, "the same reply is not duplicated");

    // A genuinely different assistant reply (a retry) is retained.
    let other = Message::Assistant(AssistantMessage {
        content: vec![ContentBlock::Text(TextContent {
            text: "RETRY_REPLY".to_owned(),
            text_signature: None,
        })],
        model: "fake-model".to_owned(),
        ..Default::default()
    });
    let other_end = serde_json::json!({
        "type": "message_end",
        "turn": 1,
        "message": other,
    })
    .to_string();
    let messages = dedupe_events_for_test(&[&message_end, &turn_end, &other_end]);
    assert_eq!(messages.len(), 2);
}
