// Integration tests (kept out of src/).

use pir_builtin_todo::*;
use serde_json::json;

fn params(value: serde_json::Value) -> TodoParams {
    TodoParams::parse(&value).expect("valid params")
}

fn state_with(subjects: &[&str]) -> TodoState {
    let mut state = TodoState::empty();
    for subject in subjects {
        let op = apply_mutation(
            &mut state,
            TaskAction::Create,
            &params(json!({ "subject": subject })),
        );
        assert!(matches!(op, TodoOp::Create { .. }), "{op:?}");
    }
    state
}

#[test]
fn create_update_and_delete_content_strings() {
    let mut state = TodoState::empty();
    let op = apply_mutation(
        &mut state,
        TaskAction::Create,
        &params(json!({ "subject": "Write the parser" })),
    );
    assert_eq!(
        format_content(&op, &state),
        "Created #1: Write the parser (pending)"
    );
    let op = apply_mutation(
        &mut state,
        TaskAction::Update,
        &params(json!({ "id": 1, "status": "in_progress" })),
    );
    assert_eq!(
        format_content(&op, &state),
        "Updated #1 (pending → in_progress)"
    );
    let op = apply_mutation(
        &mut state,
        TaskAction::Update,
        &params(json!({ "id": 1, "status": "in_progress" })),
    );
    assert_eq!(
        format_content(&op, &state),
        "No change: #1 already matches the requested values (status: in_progress)"
    );
    let op = apply_mutation(
        &mut state,
        TaskAction::Update,
        &params(json!({ "id": 1, "subject": "renamed" })),
    );
    assert_eq!(format_content(&op, &state), "Updated #1");
    let op = apply_mutation(&mut state, TaskAction::Delete, &params(json!({ "id": 1 })));
    assert_eq!(format_content(&op, &state), "Deleted #1: renamed");
    assert_eq!(
        format_content(
            &TodoOp::Error {
                message: "boom".to_owned()
            },
            &state
        ),
        "Error: boom"
    );
}

#[test]
fn list_filters_and_renders_rows() {
    let mut state = state_with(&["a", "b", "c"]);
    apply_mutation(
        &mut state,
        TaskAction::Update,
        &params(json!({ "id": 2, "status": "in_progress", "activeForm": "writing tests" })),
    );
    apply_mutation(&mut state, TaskAction::Delete, &params(json!({ "id": 3 })));

    let op = apply_mutation(&mut state, TaskAction::List, &params(json!({})));
    assert_eq!(
        format_content(&op, &state),
        "[pending] #1 a\n[in_progress] #2 b (writing tests)"
    );
    let op = apply_mutation(
        &mut state,
        TaskAction::List,
        &params(json!({ "includeDeleted": true })),
    );
    assert_eq!(
        format_content(&op, &state),
        "[pending] #1 a\n[in_progress] #2 b (writing tests)\n[deleted] #3 c"
    );
    let op = apply_mutation(
        &mut state,
        TaskAction::List,
        &params(json!({ "status": "completed" })),
    );
    assert_eq!(format_content(&op, &state), "No tasks");
    let mut empty = TodoState::empty();
    let op = apply_mutation(&mut empty, TaskAction::List, &params(json!({})));
    assert_eq!(format_content(&op, &empty), "No tasks");
}

#[test]
fn list_rows_carry_the_dependency_chain() {
    let mut state = state_with(&["a", "b"]);
    apply_mutation(
        &mut state,
        TaskAction::Update,
        &params(json!({ "id": 2, "addBlockedBy": [1] })),
    );
    let op = apply_mutation(&mut state, TaskAction::List, &params(json!({})));
    assert_eq!(
        format_content(&op, &state),
        "[pending] #1 a\n[pending] #2 b ⛓ #1"
    );
}

#[test]
fn get_renders_every_field_in_order() {
    let mut state = state_with(&["a", "b"]);
    apply_mutation(
        &mut state,
        TaskAction::Update,
        &params(json!({
            "id": 1,
            "description": "long form",
            "activeForm": "working",
            "status": "in_progress",
            "owner": "agent-1",
            "addBlockedBy": [2]
        })),
    );
    let op = apply_mutation(&mut state, TaskAction::Get, &params(json!({ "id": 1 })));
    assert_eq!(
        format_content(&op, &state),
        "#1 [in_progress] a\n  description: long form\n  activeForm: working\n  blockedBy: #2\n  owner: agent-1"
    );
}

#[test]
fn get_reports_reverse_blocks_edges() {
    let mut state = state_with(&["a", "b"]);
    apply_mutation(
        &mut state,
        TaskAction::Update,
        &params(json!({ "id": 2, "addBlockedBy": [1] })),
    );
    assert_eq!(
        format_content(&TodoOp::Get { id: 1 }, &state),
        "#1 [pending] a\n  blocks: #2"
    );
}

#[test]
fn batch_content_is_numbered() {
    let mut state = TodoState::empty();
    let first = apply_mutation(
        &mut state,
        TaskAction::Create,
        &params(json!({ "subject": "a" })),
    );
    let second = apply_mutation(
        &mut state,
        TaskAction::Create,
        &params(json!({ "subject": "b" })),
    );
    let op = TodoOp::Batch {
        ops: vec![first, second],
    };
    assert_eq!(
        format_content(&op, &state),
        "1. Created #1: a (pending)\n2. Created #2: b (pending)"
    );
}

#[test]
fn todos_command_groups_by_status_and_hides_tombstones() {
    let mut state = TodoState::empty();
    for subject in ["one", "two", "three", "four", "five"] {
        apply_mutation(
            &mut state,
            TaskAction::Create,
            &params(json!({ "subject": subject })),
        );
    }
    apply_mutation(
        &mut state,
        TaskAction::Update,
        &params(json!({ "id": 2, "status": "in_progress", "activeForm": "working" })),
    );
    apply_mutation(
        &mut state,
        TaskAction::Update,
        &params(json!({ "id": 3, "status": "completed" })),
    );
    apply_mutation(&mut state, TaskAction::Delete, &params(json!({ "id": 4 })));
    apply_mutation(
        &mut state,
        TaskAction::Update,
        &params(json!({ "id": 5, "addBlockedBy": [1] })),
    );
    let text = format_todos_command(&state).expect("non-empty list");
    let expected = [
        "1/4 completed · 1 in progress · 2 pending",
        "── Pending ──",
        "  ○ #1 one",
        "  ○ #5 five    ⛓ #1",
        "── In Progress ──",
        "  ◐ #2 two (working)",
        "── Completed ──",
        "  ✓ #3 three",
    ]
    .join("\n");
    assert_eq!(text, expected);
    assert!(!text.contains("#4"), "tombstones are never listed: {text}");

    assert_eq!(format_todos_command(&TodoState::empty()), None);
    let mut only_deleted = state_with(&["x"]);
    apply_mutation(
        &mut only_deleted,
        TaskAction::Delete,
        &params(json!({ "id": 1 })),
    );
    assert_eq!(format_todos_command(&only_deleted), None);
}

#[test]
fn model_text_is_sanitized_in_rows() {
    let mut state = state_with(&["a\u{1b}[31mred"]);
    apply_mutation(
        &mut state,
        TaskAction::Update,
        &params(json!({ "id": 1, "status": "in_progress", "activeForm": "x\u{1b}[0my" })),
    );
    let op = apply_mutation(&mut state, TaskAction::List, &params(json!({})));
    assert_eq!(format_content(&op, &state), "[in_progress] #1 ared (xy)");
}
