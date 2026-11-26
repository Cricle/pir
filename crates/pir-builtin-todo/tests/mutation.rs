// Integration tests (kept out of src/).

use pir_builtin_todo::*;
use serde_json::{json, Value as JsonValue};

fn params(value: JsonValue) -> TodoParams {
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

fn error_message(op: &TodoOp) -> String {
    match op {
        TodoOp::Error { message } => message.clone(),
        other => panic!("expected an error op, got {other:?}"),
    }
}

#[test]
fn create_assigns_sequential_ids_and_reports_the_task() {
    let mut state = TodoState::empty();
    let op = apply_mutation(
        &mut state,
        TaskAction::Create,
        &params(json!({ "subject": "Write the parser" })),
    );
    assert_eq!(op, TodoOp::Create { task_id: 1 });
    apply_mutation(
        &mut state,
        TaskAction::Create,
        &params(json!({ "subject": "second" })),
    );
    apply_mutation(
        &mut state,
        TaskAction::Create,
        &params(json!({ "subject": "third" })),
    );
    assert_eq!(state.next_id, 4);
    assert_eq!(
        state.tasks.iter().map(|task| task.id).collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
}

#[test]
fn create_requires_a_non_blank_subject_and_rejects_tombstones() {
    let mut state = TodoState::empty();
    for value in [json!({}), json!({ "subject": "   " })] {
        let op = apply_mutation(&mut state, TaskAction::Create, &params(value));
        assert_eq!(error_message(&op), "subject required for create");
    }
    let op = apply_mutation(
        &mut state,
        TaskAction::Create,
        &params(json!({ "subject": "gone", "status": "deleted" })),
    );
    assert_eq!(error_message(&op), "cannot create a deleted task");
    assert!(state.tasks.is_empty());
    assert_eq!(state.next_id, 1);

    let op = apply_mutation(
        &mut state,
        TaskAction::Create,
        &params(json!({
            "subject": "working",
            "status": "in_progress",
            "activeForm": "writing the parser"
        })),
    );
    assert_eq!(op, TodoOp::Create { task_id: 1 });
    assert_eq!(state.tasks[0].status, TaskStatus::InProgress);
    assert_eq!(
        state.tasks[0].active_form.as_deref(),
        Some("writing the parser")
    );
}

#[test]
fn illegal_transition_is_rejected_and_same_status_is_a_no_op() {
    let mut state = state_with(&["a"]);
    let op = apply_mutation(
        &mut state,
        TaskAction::Update,
        &params(json!({ "id": 1, "status": "completed" })),
    );
    assert!(matches!(op, TodoOp::Update { changed: true, .. }));
    let op = apply_mutation(
        &mut state,
        TaskAction::Update,
        &params(json!({ "id": 1, "status": "in_progress" })),
    );
    assert_eq!(
        error_message(&op),
        "illegal transition completed → in_progress"
    );
    assert_eq!(state.tasks[0].status, TaskStatus::Completed);

    let op = apply_mutation(
        &mut state,
        TaskAction::Update,
        &params(json!({ "id": 1, "status": "completed" })),
    );
    match op {
        TodoOp::Update {
            changed, from, to, ..
        } => {
            assert!(!changed);
            assert_eq!((from, to), (TaskStatus::Completed, TaskStatus::Completed));
        }
        other => panic!("expected update, got {other:?}"),
    }
}

#[test]
fn update_requires_a_mutable_field() {
    let mut state = state_with(&["a"]);
    let op = apply_mutation(&mut state, TaskAction::Update, &params(json!({ "id": 1 })));
    assert_eq!(
        error_message(&op),
        "update requires at least one mutable field: subject, description, activeForm, status, owner, metadata, addBlockedBy, or removeBlockedBy"
    );
}

#[test]
fn delete_tombstones_and_clear_resets_the_id_counter() {
    let mut state = state_with(&["a", "b"]);
    let op = apply_mutation(&mut state, TaskAction::Delete, &params(json!({ "id": 1 })));
    assert_eq!(
        op,
        TodoOp::Delete {
            id: 1,
            subject: "a".to_owned()
        }
    );
    let op = apply_mutation(&mut state, TaskAction::Delete, &params(json!({ "id": 1 })));
    assert_eq!(error_message(&op), "#1 is already deleted");
    assert_eq!(state.tasks[0].status, TaskStatus::Deleted);
    // The tombstone still resolves for dependency edges.
    let op = apply_mutation(
        &mut state,
        TaskAction::Create,
        &params(json!({ "subject": "c", "blockedBy": [1] })),
    );
    assert_eq!(error_message(&op), "blockedBy: #1 is deleted");

    let op = apply_mutation(&mut state, TaskAction::Clear, &params(json!({})));
    assert_eq!(op, TodoOp::Clear { count: 2 });
    assert!(state.tasks.is_empty());
    assert_eq!(state.next_id, 1);
    let op = apply_mutation(
        &mut state,
        TaskAction::Create,
        &params(json!({ "subject": "fresh" })),
    );
    assert_eq!(op, TodoOp::Create { task_id: 1 }, "ids restart after clear");
}

#[test]
fn missing_ids_and_required_ids_report_verbatim_errors() {
    let mut state = state_with(&["a"]);
    assert_eq!(
        error_message(&apply_mutation(
            &mut state,
            TaskAction::Get,
            &params(json!({ "id": 9 }))
        )),
        "#9 not found"
    );
    assert_eq!(
        error_message(&apply_mutation(
            &mut state,
            TaskAction::Update,
            &params(json!({ "id": 9, "status": "pending" }))
        )),
        "#9 not found"
    );
    assert_eq!(
        error_message(&apply_mutation(
            &mut state,
            TaskAction::Delete,
            &params(json!({ "id": 9 }))
        )),
        "#9 not found"
    );
    for (action, expected) in [
        (TaskAction::Update, "id required for update"),
        (TaskAction::Get, "id required for get"),
        (TaskAction::Delete, "id required for delete"),
    ] {
        assert_eq!(
            error_message(&apply_mutation(&mut state, action, &params(json!({})))),
            expected
        );
    }
}

#[test]
fn dependencies_are_additive_and_validate_targets() {
    let mut state = state_with(&["a", "b", "c"]);
    let op = apply_mutation(
        &mut state,
        TaskAction::Update,
        &params(json!({ "id": 3, "addBlockedBy": [1, 2] })),
    );
    assert!(matches!(op, TodoOp::Update { changed: true, .. }));
    assert_eq!(state.tasks[2].blocked_by, Some(vec![1, 2]));
    // Re-adding an existing id is a no-op (dedup).
    apply_mutation(
        &mut state,
        TaskAction::Update,
        &params(json!({ "id": 3, "addBlockedBy": [1] })),
    );
    assert_eq!(state.tasks[2].blocked_by, Some(vec![1, 2]));
    apply_mutation(
        &mut state,
        TaskAction::Update,
        &params(json!({ "id": 3, "removeBlockedBy": [1] })),
    );
    assert_eq!(state.tasks[2].blocked_by, Some(vec![2]));
    apply_mutation(
        &mut state,
        TaskAction::Update,
        &params(json!({ "id": 3, "removeBlockedBy": [2] })),
    );
    assert_eq!(
        state.tasks[2].blocked_by, None,
        "an emptied set drops the field"
    );

    let op = apply_mutation(
        &mut state,
        TaskAction::Update,
        &params(json!({ "id": 1, "addBlockedBy": [1] })),
    );
    assert_eq!(error_message(&op), "cannot block #1 on itself");
    let op = apply_mutation(
        &mut state,
        TaskAction::Update,
        &params(json!({ "id": 1, "addBlockedBy": [7] })),
    );
    assert_eq!(error_message(&op), "addBlockedBy: #7 not found");
    apply_mutation(&mut state, TaskAction::Delete, &params(json!({ "id": 2 })));
    let op = apply_mutation(
        &mut state,
        TaskAction::Update,
        &params(json!({ "id": 1, "addBlockedBy": [2] })),
    );
    assert_eq!(error_message(&op), "addBlockedBy: #2 is deleted");
}

#[test]
fn cycle_is_rejected_before_any_state_change() {
    let mut state = state_with(&["a", "b", "c"]);
    apply_mutation(
        &mut state,
        TaskAction::Update,
        &params(json!({ "id": 2, "addBlockedBy": [1] })),
    );
    apply_mutation(
        &mut state,
        TaskAction::Update,
        &params(json!({ "id": 3, "addBlockedBy": [2] })),
    );
    let before = state.clone();
    let op = apply_mutation(
        &mut state,
        TaskAction::Update,
        &params(json!({ "id": 1, "addBlockedBy": [3] })),
    );
    assert_eq!(
        error_message(&op),
        "addBlockedBy would create a cycle in the blockedBy graph"
    );
    assert_eq!(state, before, "a rejected call leaves the list untouched");
}

#[test]
fn metadata_merges_key_by_key_and_null_deletes() {
    let mut state = state_with(&["a"]);
    let op = apply_mutation(
        &mut state,
        TaskAction::Update,
        &params(json!({ "id": 1, "metadata": { "branch": "main", "pr": 42 } })),
    );
    assert!(matches!(op, TodoOp::Update { changed: true, .. }));
    assert_eq!(
        state.tasks[0].metadata,
        Some(
            [
                ("branch".to_owned(), json!("main")),
                ("pr".to_owned(), json!(42))
            ]
            .into_iter()
            .collect()
        )
    );
    // A null value deletes one key and keeps the other.
    apply_mutation(
        &mut state,
        TaskAction::Update,
        &params(json!({ "id": 1, "metadata": { "pr": null } })),
    );
    assert_eq!(
        state.tasks[0].metadata,
        Some([("branch".to_owned(), json!("main"))].into_iter().collect())
    );
    // Emptying the record drops the field entirely.
    apply_mutation(
        &mut state,
        TaskAction::Update,
        &params(json!({ "id": 1, "metadata": { "branch": null } })),
    );
    assert_eq!(state.tasks[0].metadata, None);
    // A metadata-only update of the same values reports "no change".
    apply_mutation(
        &mut state,
        TaskAction::Update,
        &params(json!({ "id": 1, "metadata": { "branch": "main" } })),
    );
    let op = apply_mutation(
        &mut state,
        TaskAction::Update,
        &params(json!({ "id": 1, "metadata": { "branch": "main" } })),
    );
    match op {
        TodoOp::Update { changed, .. } => assert!(!changed),
        other => panic!("expected update, got {other:?}"),
    }
}

#[test]
fn record_and_session_bounds_are_enforced() {
    let mut state = TodoState::empty();
    let op = apply_mutation(
        &mut state,
        TaskAction::Create,
        &params(json!({ "subject": "x".repeat(MAX_SUBJECT_CHARS + 1) })),
    );
    assert_eq!(
        error_message(&op),
        format!("subject too long (limit {MAX_SUBJECT_CHARS} characters)")
    );
    let op = apply_mutation(
        &mut state,
        TaskAction::Create,
        &params(json!({ "subject": "ok", "owner": "y".repeat(MAX_OWNER_CHARS + 1) })),
    );
    assert_eq!(
        error_message(&op),
        format!("owner too long (limit {MAX_OWNER_CHARS} characters)")
    );
    let big = "z".repeat(MAX_METADATA_BYTES + 1);
    let op = apply_mutation(
        &mut state,
        TaskAction::Create,
        &params(json!({ "subject": "ok", "metadata": { "blob": big } })),
    );
    assert_eq!(
        error_message(&op),
        format!("metadata too large (limit {MAX_METADATA_BYTES} bytes per task)")
    );
    assert!(state.tasks.is_empty(), "a rejected create writes nothing");

    // The session cap: fill the list, then one more create is rejected.
    let mut state = TodoState::empty();
    state.tasks = (1..=MAX_TASKS_PER_SESSION as u64)
        .map(|id| Task {
            id,
            subject: "t".to_owned(),
            ..Task::default()
        })
        .collect();
    state.next_id = MAX_TASKS_PER_SESSION as u64 + 1;
    let op = apply_mutation(
        &mut state,
        TaskAction::Create,
        &params(json!({ "subject": "one too many" })),
    );
    assert_eq!(
        error_message(&op),
        format!("task limit reached (limit {MAX_TASKS_PER_SESSION} tasks per session)")
    );
    assert_eq!(state.tasks.len(), MAX_TASKS_PER_SESSION);
}
