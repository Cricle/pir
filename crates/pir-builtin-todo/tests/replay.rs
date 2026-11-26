// Integration tests (kept out of src/).

use pir_builtin_todo::*;
use serde_json::{json, Value as JsonValue};

fn call(state: &mut TodoState, args: JsonValue) -> TodoResponse {
    let action = TaskAction::parse(args["action"].as_str().unwrap_or("")).expect("action");
    let params = TodoParams::parse(&args).expect("params");
    let op = apply_mutation(state, action, &params);
    build_response(action.as_str(), &args, state, &op)
}

fn entry_from(response: &TodoResponse, call_id: &str, timestamp: u64) -> JsonValue {
    json!({
        "role": "toolResult",
        "toolName": TOOL_NAME,
        "toolCallId": call_id,
        "timestamp": timestamp,
        "details": response.details
    })
}

fn user_entry(text: &str) -> JsonValue {
    json!({
        "role": "user",
        "content": [{ "type": "text", "text": text }]
    })
}

#[test]
fn takes_the_last_snapshot_and_ignores_unrelated_entries() {
    let mut state = TodoState::empty();
    let first = call(&mut state, json!({ "action": "create", "subject": "a" }));
    let second = call(
        &mut state,
        json!({ "action": "update", "id": 1, "status": "completed" }),
    );
    let entries = vec![
        user_entry("plan this"),
        entry_from(&first, "call-1", 100),
        entry_from(&second, "call-2", 200),
    ];
    let scan = replay_from_entries(&entries);
    assert_eq!(scan.matches, 2);
    assert_eq!(scan.entries_visited, 3);
    assert_eq!(
        scan.deserializations, 1,
        "a one-shot scan deserializes only the last matching snapshot"
    );
    assert_eq!(scan.state.as_ref(), Some(&state));

    // Non-todo tool results and assistant/user messages are inert.
    let mut with_noise = entries.clone();
    with_noise.push(json!({
        "role": "toolResult",
        "toolName": "bash",
        "details": { "tasks": [], "nextId": 1 }
    }));
    with_noise.push(json!({ "role": "assistant", "content": [] }));
    assert_eq!(
        replay_from_entries(&with_noise).state.as_ref(),
        Some(&state)
    );
}

#[test]
fn invalid_snapshot_shapes_are_skipped() {
    let entries = vec![
        json!({ "role": "toolResult", "toolName": TOOL_NAME, "details": { "nextId": 1 } }),
        json!({ "role": "toolResult", "toolName": TOOL_NAME, "details": { "tasks": "nope", "nextId": 1 } }),
        json!({ "role": "toolResult", "toolName": TOOL_NAME, "details": { "tasks": [{ "id": "x" }], "nextId": 1 } }),
    ];
    let scan = replay_from_entries(&entries);
    assert_eq!(scan.state, None);
    assert_eq!(scan.matches, 1, "only the shape-matching entry counts");
    assert_eq!(scan.deserializations, 1);
    assert_eq!(
        snapshot_from_details(&json!({ "tasks": [], "nextId": 1 })),
        Some(TodoState::empty())
    );
}

#[test]
fn malformed_last_snapshot_falls_back_to_the_previous_one() {
    let mut state = TodoState::empty();
    let good = call(&mut state, json!({ "action": "create", "subject": "a" }));
    let entries = vec![
        entry_from(&good, "call-1", 1),
        // Shape-valid but the task body is malformed.
        json!({
            "role": "toolResult",
            "toolName": TOOL_NAME,
            "toolCallId": "call-2",
            "timestamp": 2,
            "details": { "tasks": [{ "id": "x" }], "nextId": 2 }
        }),
    ];
    let scan = replay_from_entries(&entries);
    assert_eq!(scan.state.as_ref(), Some(&state));
    assert_eq!(scan.deserializations, 2);
}

#[test]
fn fingerprint_moves_with_the_last_snapshot_only() {
    let mut state = TodoState::empty();
    let first = call(&mut state, json!({ "action": "create", "subject": "a" }));
    let entry = entry_from(&first, "call-1", 1);
    let base = entries_fingerprint(std::slice::from_ref(&entry));
    let noise = vec![user_entry("hello")];
    assert_ne!(entries_fingerprint(&noise), base);
    // Earlier non-matching entries do not change the fingerprint when the
    // same last snapshot stays last (only the entry count is folded in,
    // so compare two equal-length transcripts).
    let earlier = user_entry("earlier");
    let other_noise = user_entry("different");
    assert_eq!(
        entries_fingerprint(&[earlier, entry.clone()]),
        entries_fingerprint(&[other_noise, entry.clone()])
    );
    // A changed details snapshot moves it.
    let mut changed = entry.clone();
    changed["details"]["nextId"] = json!(99);
    assert_ne!(entries_fingerprint(&[changed]), base);
    // Appending transcript noise moves it too (the count is part of the
    // change detector, so a resumption cannot keep a stale list).
    assert_ne!(entries_fingerprint(&[user_entry("hello"), entry]), base);
}

#[test]
fn empty_transcript_yields_no_state() {
    let scan = replay_from_entries(&[]);
    assert_eq!(scan.state, None);
    assert_eq!(scan.matches, 0);
    assert_eq!(scan.deserializations, 0);
    let op = TodoOp::Clear { count: 0 };
    let response = build_response("clear", &json!({}), &TodoState::empty(), &op);
    assert_eq!(response.content, "Cleared 0 tasks");
}
