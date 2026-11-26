// Integration tests (kept out of src/).

use pir_builtin_todo::*;
use serde_json::{json, Value as JsonValue};

fn params(value: JsonValue) -> TodoParams {
    TodoParams::parse(&value).expect("valid params")
}

#[test]
fn envelope_carries_the_full_snapshot_and_the_bare_error() {
    let mut state = TodoState::empty();
    apply_mutation(
        &mut state,
        TaskAction::Create,
        &params(json!({ "subject": "a" })),
    );
    let args = json!({ "action": "create", "subject": "b" });
    let op = apply_mutation(&mut state, TaskAction::Create, &params(args.clone()));
    let result = build_response("create", &args, &state, &op);
    assert!(
        !result.is_error,
        "rejections are in-band, never transport errors"
    );
    assert_eq!(result.content, "Created #2: b (pending)");
    assert_eq!(result.details["action"], json!("create"));
    assert_eq!(result.details["params"], args);
    assert_eq!(result.details["nextId"], json!(3));
    assert_eq!(result.details["tasks"].as_array().map(Vec::len), Some(2));
    assert_eq!(result.details["tasks"][0]["subject"], json!("a"));
    assert!(result.details.get("error").is_none());

    let args = json!({ "action": "update", "id": 1 });
    let op = apply_mutation(&mut state, TaskAction::Update, &params(args.clone()));
    let result = build_response("update", &args, &state, &op);
    assert_eq!(
        result.details["error"],
        json!("update requires at least one mutable field: subject, description, activeForm, status, owner, metadata, addBlockedBy, or removeBlockedBy")
    );
    assert_eq!(
        result.content,
        "Error: update requires at least one mutable field: subject, description, activeForm, status, owner, metadata, addBlockedBy, or removeBlockedBy"
    );
    assert_eq!(
        result.error_message(),
        Some(
            "update requires at least one mutable field: subject, description, activeForm, status, owner, metadata, addBlockedBy, or removeBlockedBy"
        )
    );
    // The rejected call embedded the unchanged (two-task) snapshot.
    assert_eq!(result.details["tasks"].as_array().map(Vec::len), Some(2));
    assert_eq!(result.details["nextId"], json!(3));
}

#[test]
fn task_wire_shape_omits_absent_optionals() {
    let task = Task {
        id: 3,
        subject: "Write the parser".to_owned(),
        description: None,
        active_form: Some("writing the parser".to_owned()),
        status: TaskStatus::InProgress,
        blocked_by: Some(vec![1, 2]),
        owner: None,
        metadata: None,
    };
    let wire = serde_json::to_value(&task).expect("serialize");
    assert_eq!(
        wire,
        json!({
            "id": 3,
            "subject": "Write the parser",
            "activeForm": "writing the parser",
            "status": "in_progress",
            "blockedBy": [1, 2]
        })
    );
    let round_tripped: Task = serde_json::from_value(wire).expect("deserialize");
    assert_eq!(round_tripped, task);
}

#[test]
fn state_wire_shape_is_persisted_shape() {
    let state = TodoState {
        tasks: vec![Task {
            id: 1,
            subject: "a".to_owned(),
            ..Task::default()
        }],
        next_id: 2,
    };
    let wire = serde_json::to_value(&state).expect("serialize");
    assert_eq!(
        wire,
        json!({
            "tasks": [{ "id": 1, "subject": "a", "status": "pending" }],
            "nextId": 2
        })
    );
    let round_tripped: TodoState = serde_json::from_value(wire).expect("deserialize");
    assert_eq!(round_tripped, state);
}

#[test]
fn converts_to_the_plugin_api_result() {
    let state = TodoState::empty();
    let op = TodoOp::Clear { count: 0 };
    let response = build_response("clear", &json!({ "action": "clear" }), &state, &op);
    let plugin: pir_plugin_api::ToolResult = response.into();
    assert_eq!(plugin.content, "Cleared 0 tasks");
    assert_eq!(plugin.details["nextId"], json!(1));
    assert!(!plugin.is_error);
}
