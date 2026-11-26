// Integration tests (kept out of src/).

use pir_builtin_todo::*;
use serde_json::{json, Value as JsonValue};

#[test]
fn parses_every_supported_field() {
    let params = TodoParams::parse(&json!({
        "action": "update",
        "subject": "s",
        "description": "d",
        "activeForm": "a",
        "status": "in_progress",
        "blockedBy": [1, 2],
        "addBlockedBy": [3],
        "removeBlockedBy": [4],
        "owner": "me",
        "metadata": { "k": 1 },
        "id": 7,
        "includeDeleted": true
    }))
    .expect("valid params");
    assert_eq!(params.subject.as_deref(), Some("s"));
    assert_eq!(params.status, Some(TaskStatus::InProgress));
    assert_eq!(params.blocked_by, Some(vec![1, 2]));
    assert_eq!(params.add_blocked_by, Some(vec![3]));
    assert_eq!(params.remove_blocked_by, Some(vec![4]));
    assert_eq!(params.id, Some(7));
    assert_eq!(params.include_deleted, Some(true));
}

#[test]
fn rejects_wrong_types_with_documented_messages() {
    assert_eq!(
        TodoParams::parse(&json!({ "subject": 5 })).expect_err("type error"),
        "invalid params: subject must be a string"
    );
    assert_eq!(
        TodoParams::parse(&json!({ "id": -1 })).expect_err("negative id"),
        "invalid params: id must be a non-negative integer"
    );
    assert_eq!(
        TodoParams::parse(&json!({ "status": "bogus" })).expect_err("bad status"),
        "invalid params: status must be one of pending, in_progress, completed, deleted"
    );
    assert_eq!(
        TodoParams::parse(&json!({ "blockedBy": ["x"] })).expect_err("bad ids"),
        "invalid params: blockedBy must be an array of task ids"
    );
    assert_eq!(
        TodoParams::parse(&json!({ "metadata": 4 })).expect_err("bad metadata"),
        "invalid params: metadata must be an object"
    );
    assert_eq!(
        TodoParams::parse(&JsonValue::Null).expect_err("non-object"),
        "invalid params: expected an object"
    );
    // Unknown keys are ignored (the open params bag).
    assert!(TodoParams::parse(&json!({ "action": "list", "extra": true })).is_ok());
    // Null means absent.
    assert_eq!(TodoParams::parse(&json!({ "id": null })).unwrap().id, None);
}

#[test]
fn batch_parses_mutating_items_only() {
    let items = parse_batch_items(&json!({
        "action": "batch",
        "updates": [
            { "action": "create", "subject": "a" },
            { "action": "update", "id": 1, "status": "completed" },
            { "action": "delete", "id": 2 },
            { "action": "clear" }
        ]
    }))
    .expect("valid batch");
    assert_eq!(items.len(), 4);
    assert_eq!(items[0].action, TaskAction::Create);
    assert_eq!(items[1].action, TaskAction::Update);
    assert_eq!(items[2].action, TaskAction::Delete);
    assert_eq!(items[3].action, TaskAction::Clear);
}

#[test]
fn batch_rejects_empty_read_only_nested_and_oversized() {
    assert_eq!(
        parse_batch_items(&json!({ "action": "batch", "updates": [] })).expect_err("empty batch"),
        "batch requires a non-empty updates array"
    );
    assert_eq!(
        parse_batch_items(&json!({ "action": "batch" })).expect_err("missing updates"),
        "batch requires a non-empty updates array"
    );
    assert_eq!(
        parse_batch_items(&json!({
            "action": "batch",
            "updates": [{ "action": "list" }]
        }))
        .expect_err("read-only item"),
        "batch item 1: list is read-only; call it directly"
    );
    assert_eq!(
        parse_batch_items(&json!({
            "action": "batch",
            "updates": [{ "action": "batch", "updates": [] }]
        }))
        .expect_err("nested batch"),
        "batch item 1: nested batch is not allowed"
    );
    assert_eq!(
        parse_batch_items(&json!({
            "action": "batch",
            "updates": [{ "subject": "missing action" }]
        }))
        .expect_err("missing action"),
        "batch item 1: unknown action \"\""
    );
    let too_many: Vec<JsonValue> = (0..=MAX_BATCH_ITEMS)
        .map(|_| json!({ "action": "clear" }))
        .collect();
    assert_eq!(
        parse_batch_items(&json!({ "action": "batch", "updates": too_many }))
            .expect_err("oversized batch"),
        format!(
            "batch supports at most {MAX_BATCH_ITEMS} updates (got {})",
            MAX_BATCH_ITEMS + 1
        )
    );
}
