//! `durable::harness::live` — the `pi.live` document's pure slot helpers.
//!
//! Ports upstream `packages/durable/src/harness/live.ts`: nested-slot binary
//! search, the tools-before-nested lookup, running/done/removed slot
//! transitions, the bounded-output chunk protocol, and the persisted shape
//! (`nestedTools`, `taskId`, `droppedBytes`, ...).
//!
//! Tests live here as an integration suite rather than inline in the module:
//! the crate forbids inline tests.

use serde_json::{json, Map};

use pir::durable::harness::live::{
    apply_output_chunk, clear_progress, find_nested_slot, find_tool_slot, live_from_object,
    live_to_object, set_slot_done, set_slot_removed, set_slot_running, CompactionFailure,
    CompactionStatus, DeferredStatus, GenerationState, LiveState, NestedToolSlot,
    NestedToolSummary, OutputSkip, RetryStatus, RunState, SlotLocation, SlotProgress, SlotStatus,
    ToolDiagnostic, ToolSlot, LIVE_KIND, LIVE_VERSION,
};
use pir::durable::ids::{EntryId, SubmissionId, TaskId};

// ── helpers ────────────────────────────────────────────────────────────────

fn task(id: i64) -> TaskId {
    TaskId::from_raw(id)
}

fn tool_slot(task_id: i64) -> ToolSlot {
    ToolSlot {
        progress: SlotProgress::new(format!("call-{task_id}"), "bash"),
        task_id: Some(task(task_id)),
        entry: None,
    }
}

fn nested_slot(task_id: i64, parent_task_id: i64) -> NestedToolSlot {
    NestedToolSlot {
        progress: SlotProgress::new(format!("call-{task_id}"), "read"),
        parent_call_id: format!("call-{parent_task_id}"),
        parent_task_id: task(parent_task_id),
        task_id: task(task_id),
        arguments: Map::new(),
        summary: None,
    }
}

fn nested_ids(state: &LiveState) -> Vec<i64> {
    state
        .nested_tools
        .as_ref()
        .expect("nested tools present")
        .iter()
        .map(|slot| slot.task_id.as_i64())
        .collect()
}

// ── slot lookup ────────────────────────────────────────────────────────────

#[test]
fn find_nested_slot_binary_searches_an_ascending_table() {
    let mut state = LiveState::new();

    // No table at all, then an empty one.
    assert_eq!(find_nested_slot(&state, task(10)), None);
    state.nested_tools = Some(Vec::new());
    assert_eq!(find_nested_slot(&state, task(10)), None);

    state.nested_tools = Some(vec![
        nested_slot(10, 1),
        nested_slot(20, 1),
        nested_slot(30, 20),
    ]);

    assert_eq!(find_nested_slot(&state, task(10)), Some(0));
    assert_eq!(find_nested_slot(&state, task(20)), Some(1));
    assert_eq!(find_nested_slot(&state, task(30)), Some(2));

    for missing in [5, 15, 25, 35] {
        assert_eq!(
            find_nested_slot(&state, task(missing)),
            None,
            "task {missing} is not in the table"
        );
    }
}

#[test]
fn find_tool_slot_prefers_tools_then_searches_nested() {
    let mut state = LiveState::new();
    state.tools = Some(vec![tool_slot(5)]);
    state.nested_tools = Some(vec![nested_slot(5, 1), nested_slot(20, 5)]);

    // The same task ID in both lists resolves to the model-issued slot.
    assert_eq!(
        find_tool_slot(&state, task(5)),
        Some(SlotLocation::Tools(0))
    );
    assert_eq!(
        find_tool_slot(&state, task(20)),
        Some(SlotLocation::Nested(1))
    );
    assert_eq!(find_tool_slot(&state, task(99)), None);

    // Without `tools`, the nested table still resolves.
    state.tools = None;
    assert_eq!(
        find_tool_slot(&state, task(5)),
        Some(SlotLocation::Nested(0))
    );
}

// ── running / done / removed ───────────────────────────────────────────────

#[test]
fn set_slot_running_marks_only_the_matching_slot() {
    let mut state = LiveState::new();
    state.tools = Some(vec![tool_slot(5)]);
    state.nested_tools = Some(vec![nested_slot(20, 5)]);

    assert!(set_slot_running(&mut state, task(5)));
    assert_eq!(
        state.tools.as_ref().expect("tools")[0].progress.status,
        SlotStatus::Running
    );
    assert!(state.tools.as_ref().expect("tools")[0]
        .progress
        .status
        .is_running());
    assert_eq!(
        state.nested_tools.as_ref().expect("nested")[0]
            .progress
            .status,
        SlotStatus::Pending
    );

    assert!(set_slot_running(&mut state, task(20)));
    assert_eq!(
        state.nested_tools.as_ref().expect("nested")[0]
            .progress
            .status,
        SlotStatus::Running
    );

    assert!(!set_slot_running(&mut state, task(99)));
}

#[test]
fn set_slot_done_marks_done_and_clears_running_progress() {
    let mut slot = tool_slot(5);
    slot.entry = Some(EntryId::from_raw(9));
    slot.progress.status = SlotStatus::Running;
    slot.progress.output = Some("partial output".to_owned());
    slot.progress.dropped_bytes = Some(11);
    slot.progress.dropped_lines = Some(2);
    slot.progress.details = Some(json!({ "progress": 0.5 }));
    slot.progress.diagnostics = Some(vec![ToolDiagnostic {
        severity: "warn".to_owned(),
        message: "truncated".to_owned(),
        code: Some("truncated".to_owned()),
    }]);

    let mut nested = nested_slot(20, 5);
    nested.progress.status = SlotStatus::Running;
    nested.progress.output = Some("nested output".to_owned());

    let mut state = LiveState::new();
    state.tools = Some(vec![slot]);
    state.nested_tools = Some(vec![nested]);

    assert!(set_slot_done(&mut state, task(5)));
    let slot = &state.tools.as_ref().expect("tools")[0];
    assert_eq!(slot.progress.status, SlotStatus::Done);
    assert_eq!(slot.progress.output, None);
    assert_eq!(slot.progress.dropped_bytes, None);
    assert_eq!(slot.progress.dropped_lines, None);
    assert_eq!(slot.progress.details, None);
    assert_eq!(slot.progress.diagnostics, None);
    // The result entry is the caller's to set and survives.
    assert_eq!(slot.entry, Some(EntryId::from_raw(9)));

    assert!(set_slot_done(&mut state, task(20)));
    assert_eq!(
        state.nested_tools.as_ref().expect("nested")[0]
            .progress
            .output,
        None
    );
    assert!(!set_slot_done(&mut state, task(99)));
}

#[test]
fn clear_progress_clears_every_published_field() {
    let mut progress = SlotProgress::new("call", "bash");
    progress.output = Some("text".to_owned());
    progress.dropped_bytes = Some(1);
    progress.dropped_lines = Some(2);
    progress.details = Some(json!({ "x": 1 }));
    progress.diagnostics = Some(vec![ToolDiagnostic {
        severity: "info".to_owned(),
        message: "note".to_owned(),
        code: None,
    }]);

    clear_progress(&mut progress);

    assert_eq!(progress.output, None);
    assert_eq!(progress.dropped_bytes, None);
    assert_eq!(progress.dropped_lines, None);
    assert_eq!(progress.details, None);
    assert_eq!(progress.diagnostics, None);
    // Identity and lifecycle status are not progress.
    assert_eq!(progress.call_id, "call");
    assert_eq!(progress.name, "bash");
    assert_eq!(progress.status, SlotStatus::Pending);
}

#[test]
fn set_slot_removed_drops_descendants_transitively() {
    let mut state = LiveState::new();
    state.nested_tools = Some(vec![
        nested_slot(2, 1),
        nested_slot(3, 2),
        nested_slot(4, 2),
        nested_slot(5, 4),
        nested_slot(10, 99),
    ]);

    assert!(set_slot_removed(&mut state, task(1)));
    assert_eq!(nested_ids(&state), vec![10]);

    // Nothing below 1 remains, so a second removal is a no-op.
    assert!(!set_slot_removed(&mut state, task(1)));
    assert!(!set_slot_removed(&mut state, task(30)));
    assert_eq!(nested_ids(&state), vec![10]);
}

#[test]
fn set_slot_removed_keeps_the_slot_itself() {
    let mut state = LiveState::new();
    state.nested_tools = Some(vec![
        nested_slot(1, 0),
        nested_slot(2, 1),
        nested_slot(3, 2),
    ]);

    // Only the slots below task 1 leave; slot 1 is settled by its own caller.
    assert!(set_slot_removed(&mut state, task(1)));
    assert_eq!(nested_ids(&state), vec![1]);
    assert_eq!(state.nested_tools.as_ref().expect("nested").len(), 1);
}

#[test]
fn set_slot_removed_turns_an_emptied_table_into_none() {
    let mut state = LiveState::new();
    state.nested_tools = Some(vec![nested_slot(2, 1), nested_slot(3, 1)]);

    assert!(set_slot_removed(&mut state, task(1)));
    assert_eq!(state.nested_tools, None);
}

#[test]
fn set_slot_removed_on_missing_or_empty_tables_reports_false() {
    let mut state = LiveState::new();
    assert!(!set_slot_removed(&mut state, task(1)));
    assert_eq!(state.nested_tools, None);

    // An already-empty list is normalized to `None` while reporting no removal.
    state.nested_tools = Some(Vec::new());
    assert!(!set_slot_removed(&mut state, task(1)));
    assert_eq!(state.nested_tools, None);
}

#[test]
fn set_slot_removed_does_not_remove_a_leaf_by_its_own_id() {
    let mut state = LiveState::new();
    state.nested_tools = Some(vec![nested_slot(5, 1)]);

    // The parameter names the parent whose descendants are removed, not the
    // slot to remove.
    assert!(!set_slot_removed(&mut state, task(5)));
    assert_eq!(nested_ids(&state), vec![5]);
}

// ── output chunks ──────────────────────────────────────────────────────────

#[test]
fn apply_output_chunk_appends_without_a_skip() {
    let mut slot = SlotProgress::new("call", "bash");

    apply_output_chunk(&mut slot, "hello", None);
    assert_eq!(slot.output.as_deref(), Some("hello"));

    apply_output_chunk(&mut slot, " world", None);
    assert_eq!(slot.output.as_deref(), Some("hello world"));

    apply_output_chunk(&mut slot, "", None);
    assert_eq!(slot.output.as_deref(), Some("hello world"));

    assert_eq!(slot.dropped_bytes, None);
    assert_eq!(slot.dropped_lines, None);
}

#[test]
fn apply_output_chunk_with_a_skip_replaces_and_accumulates() {
    let mut slot = SlotProgress::new("call", "bash");
    slot.output = Some("old window".to_owned());

    apply_output_chunk(
        &mut slot,
        "new window",
        Some(OutputSkip {
            bytes: 10,
            newlines: 2,
        }),
    );
    assert_eq!(slot.output.as_deref(), Some("new window"));
    assert_eq!(slot.dropped_bytes, Some(10));
    assert_eq!(slot.dropped_lines, Some(2));

    apply_output_chunk(
        &mut slot,
        "third window",
        Some(OutputSkip {
            bytes: 5,
            newlines: 1,
        }),
    );
    assert_eq!(slot.output.as_deref(), Some("third window"));
    assert_eq!(slot.dropped_bytes, Some(15));
    assert_eq!(slot.dropped_lines, Some(3));

    // A zero skip replaces the window without creating dropped fields.
    apply_output_chunk(
        &mut slot,
        "zero",
        Some(OutputSkip {
            bytes: 0,
            newlines: 0,
        }),
    );
    assert_eq!(slot.output.as_deref(), Some("zero"));
    assert_eq!(slot.dropped_bytes, Some(15));
    assert_eq!(slot.dropped_lines, Some(3));
}

#[test]
fn apply_output_chunk_zero_skip_on_a_fresh_slot_creates_no_dropped_fields() {
    let mut slot = SlotProgress::new("call", "bash");

    apply_output_chunk(&mut slot, "text", Some(OutputSkip::default()));

    assert_eq!(slot.output.as_deref(), Some("text"));
    assert_eq!(slot.dropped_bytes, None);
    assert_eq!(slot.dropped_lines, None);
}

#[test]
fn apply_output_chunk_counts_bytes_and_lines_independently() {
    let mut slot = SlotProgress::new("call", "bash");

    apply_output_chunk(
        &mut slot,
        "only bytes",
        Some(OutputSkip {
            bytes: 3,
            newlines: 0,
        }),
    );
    assert_eq!(slot.dropped_bytes, Some(3));
    assert_eq!(slot.dropped_lines, None);

    apply_output_chunk(
        &mut slot,
        "only lines",
        Some(OutputSkip {
            bytes: 0,
            newlines: 4,
        }),
    );
    assert_eq!(slot.dropped_bytes, Some(3));
    assert_eq!(slot.dropped_lines, Some(4));
}

// ── persisted shape ────────────────────────────────────────────────────────

fn rich_state() -> LiveState {
    let mut tool = tool_slot(5);
    tool.entry = Some(EntryId::from_raw(9));
    tool.progress.status = SlotStatus::Done;
    tool.progress.details = Some(json!({ "lines": 3 }));

    let mut nested = nested_slot(6, 5);
    nested.progress.status = SlotStatus::Done;
    nested.summary = Some(NestedToolSummary {
        is_error: true,
        duration_ms: Some(12),
        usage: None,
        error: Some("bad".to_owned()),
    });

    LiveState {
        run: Some(RunState {
            task_id: task(1),
            inputs: vec![SubmissionId::from_raw(2), SubmissionId::from_raw(3)],
        }),
        generation: Some(GenerationState {
            attempt: 2,
            message: Some(json!({ "role": "assistant", "content": [] })),
            retry: Some(RetryStatus {
                at: 1_000,
                error: "rate limited".to_owned(),
            }),
            deferred: Some(DeferredStatus { poll_at: 2_000 }),
        }),
        tools: Some(vec![tool]),
        nested_tools: Some(vec![nested]),
        compactions: Some(vec![CompactionStatus {
            task_id: task(7),
            reason: "threshold".to_owned(),
            blocking: true,
            attempt: 2,
            retry: None,
        }]),
        compaction_failure: Some(CompactionFailure {
            task_id: task(8),
            reason: "overflow".to_owned(),
            error: "too big".to_owned(),
        }),
    }
}

#[test]
fn live_state_round_trips_through_the_document_object() {
    let state = rich_state();

    let object = live_to_object(&state);

    // camelCase persisted names.
    assert_eq!(object["nestedTools"][0]["parentTaskId"], json!(5));
    assert_eq!(object["nestedTools"][0]["callId"], json!("call-6"));
    assert_eq!(object["generation"]["deferred"]["pollAt"], json!(2_000));
    assert_eq!(object["generation"]["retry"]["at"], json!(1_000));
    assert_eq!(object["compactionFailure"]["taskId"], json!(8));
    assert_eq!(object["tools"][0]["callId"], json!("call-5"));
    // `SlotProgress` is flattened into the slot object.
    assert!(object["tools"][0].get("progress").is_none());
    assert!(object.get("nested_tools").is_none());

    let back = live_from_object(&object);
    assert_eq!(back, state);
}

#[test]
fn live_from_object_tolerates_unknown_fields() {
    let state = rich_state();
    let mut object = live_to_object(&state);

    object.insert("futureField".to_owned(), json!({ "x": 1 }));
    object
        .get_mut("tools")
        .expect("tools")
        .as_array_mut()
        .expect("tools is an array")[0]["futureSlotField"] = json!(true);
    object
        .get_mut("nestedTools")
        .expect("nestedTools")
        .as_array_mut()
        .expect("nestedTools is an array")[0]["futureNestedField"] = json!([1, 2, 3]);

    assert_eq!(live_from_object(&object), state);
}

#[test]
fn live_from_object_falls_back_to_default_on_invalid_values() {
    let mut object = Map::new();
    object.insert("generation".to_owned(), json!("not-an-object"));

    assert_eq!(live_from_object(&object), LiveState::default());

    let mut object = Map::new();
    object.insert("run".to_owned(), json!({ "taskId": 1, "inputs": "nope" }));

    assert_eq!(live_from_object(&object), LiveState::default());
}

#[test]
fn an_empty_live_state_serializes_to_an_empty_object() {
    assert_eq!(live_to_object(&LiveState::new()), Map::new());
    assert_eq!(live_from_object(&Map::new()), LiveState::new());
    assert_eq!(
        serde_json::to_value(LiveState::new()).expect("serialize"),
        json!({})
    );
}

#[test]
fn slot_progress_and_status_use_their_persisted_names() {
    assert_eq!(
        serde_json::to_value(SlotStatus::Pending).expect("serialize"),
        json!("pending")
    );
    assert_eq!(
        serde_json::to_value(SlotStatus::Running).expect("serialize"),
        json!("running")
    );
    assert_eq!(
        serde_json::to_value(SlotStatus::Done).expect("serialize"),
        json!("done")
    );

    assert!(SlotStatus::Running.is_running());
    assert!(!SlotStatus::Pending.is_running());
    assert!(!SlotStatus::Done.is_running());

    let mut progress = SlotProgress::new("call-1", "bash");
    progress.dropped_bytes = Some(3);
    let value = serde_json::to_value(&progress).expect("serialize");

    assert_eq!(value["callId"], json!("call-1"));
    assert_eq!(value["name"], json!("bash"));
    assert_eq!(value["status"], json!("pending"));
    assert_eq!(value["droppedBytes"], json!(3));
    // Absent optional fields are omitted, not null.
    assert!(value.get("droppedLines").is_none());
    assert!(value.get("output").is_none());
    assert!(value.get("details").is_none());
    assert!(value.get("diagnostics").is_none());
}

#[test]
fn kind_and_version_are_the_persisted_pi_live_values() {
    assert_eq!(LIVE_KIND, "pi.live");
    assert_eq!(LIVE_VERSION, 1);
}

#[test]
fn a_value_slot_can_still_be_written_with_clear_progress() {
    // `clear_progress` is public and does not touch `status`; a pending slot
    // stays pending after clearing empty progress.
    let mut slot = tool_slot(5);
    let before = slot.progress.clone();
    clear_progress(&mut slot.progress);
    assert_eq!(slot.progress, before);
    assert_eq!(slot.progress.output, None);
}
