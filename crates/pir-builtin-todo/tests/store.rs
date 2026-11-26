// Integration tests (kept out of src/).

use pir_builtin_todo::*;
use serde_json::{json, Value as JsonValue};

fn store_with_session() -> TodoStore {
    let store = TodoStore::new();
    store.set_active_session("s1");
    store
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
    json!({ "role": "user", "content": [{ "type": "text", "text": text }] })
}

#[test]
fn dispatch_runs_create_update_list_and_clear() {
    let store = store_with_session();
    let created = store.dispatch(&json!({ "action": "create", "subject": "a" }));
    assert_eq!(created.content, "Created #1: a (pending)");
    assert_eq!(created.details["action"], json!("create"));

    let updated = store.dispatch(&json!({ "action": "update", "id": 1, "status": "in_progress" }));
    assert_eq!(updated.content, "Updated #1 (pending → in_progress)");

    let listed = store.dispatch(&json!({ "action": "list" }));
    assert_eq!(listed.content, "[in_progress] #1 a");

    let cleared = store.dispatch(&json!({ "action": "clear" }));
    assert_eq!(cleared.content, "Cleared 1 tasks");
    assert_eq!(store.state("s1"), TodoState::empty());
    let stats = store.stats().snapshot();
    assert_eq!(stats.tool_calls, 4);
}

#[test]
fn dispatch_reports_unknown_actions_and_bad_params_in_band() {
    let store = store_with_session();
    let response = store.dispatch(&json!({ "action": "explode" }));
    assert_eq!(
        response.content,
        "Error: invalid params: unknown action \"explode\""
    );
    assert_eq!(response.details["action"], json!("explode"));
    assert_eq!(
        response.error_message(),
        Some("invalid params: unknown action \"explode\"")
    );

    let response = store.dispatch(&json!({ "action": "create", "subject": 5 }));
    assert_eq!(
        response.content,
        "Error: invalid params: subject must be a string"
    );
    assert!(!response.is_error);
    assert_eq!(store.state("s1"), TodoState::empty());
}

#[test]
fn batch_applies_multiple_changes_atomically() {
    let store = store_with_session();
    store.dispatch(&json!({ "action": "create", "subject": "a" }));
    store.dispatch(&json!({ "action": "create", "subject": "b" }));
    let response = store.dispatch(&json!({
        "action": "batch",
        "updates": [
            { "action": "update", "id": 1, "status": "completed" },
            { "action": "update", "id": 2, "status": "in_progress", "activeForm": "working" },
            { "action": "create", "subject": "c", "status": "completed" }
        ]
    }));
    assert!(!response.is_error);
    assert!(
        response
            .content
            .contains("1. Updated #1 (pending → completed)"),
        "{}",
        response.content
    );
    assert!(
        response
            .content
            .contains("2. Updated #2 (pending → in_progress)"),
        "{}",
        response.content
    );
    assert!(
        response.content.contains("3. Created #3: c (completed)"),
        "{}",
        response.content
    );
    assert_eq!(response.details["action"], json!("batch"));
    assert_eq!(response.details["nextId"], json!(4));
    assert_eq!(response.details["tasks"][0]["status"], json!("completed"));
    assert_eq!(response.details["tasks"][1]["status"], json!("in_progress"));
    assert_eq!(response.details["tasks"][2]["status"], json!("completed"));
    assert_eq!(store.state("s1").tasks.len(), 3);
}

#[test]
fn batch_failure_leaves_the_previous_state_unchanged() {
    let store = store_with_session();
    store.dispatch(&json!({ "action": "create", "subject": "a" }));
    store.dispatch(&json!({ "action": "create", "subject": "b" }));
    let response = store.dispatch(&json!({
        "action": "batch",
        "updates": [
            { "action": "update", "id": 1, "status": "completed" },
            { "action": "update", "id": 99, "status": "completed" }
        ]
    }));
    assert!(
        response
            .content
            .contains("batch item 2 (update): #99 not found"),
        "{}",
        response.content
    );
    let state = store.state("s1");
    assert_eq!(state.tasks.len(), 2, "no partial batch commit");
    assert_eq!(state.tasks[0].status, TaskStatus::Pending);
    assert_eq!(state.tasks[1].status, TaskStatus::Pending);
    // The failure envelope embeds the unchanged snapshot.
    assert_eq!(response.details["tasks"].as_array().map(Vec::len), Some(2));
}

#[test]
fn replay_is_idempotent_across_repeated_pushes() {
    let store = store_with_session();
    let response = store.dispatch(&json!({ "action": "create", "subject": "a" }));
    let state = store.state("s1");
    let entries = vec![user_entry("go"), entry_from(&response, "call-1", 42)];
    assert!(store.replay("s1", &entries));
    assert_eq!(store.state("s1"), state);
    // A second identical push touches nothing.
    assert!(!store.replay("s1", &entries));
    assert_eq!(store.state("s1"), state);
    let stats = store.stats().snapshot();
    assert_eq!(stats.scans, 2);
    assert_eq!(stats.replay_skips, 1);
    // One touch from the live create and one from the first replay; the
    // identical second push touches nothing.
    assert_eq!(stats.state_touches, 2);
    assert_eq!(stats.deserializations, 1);
}

#[test]
fn replay_handles_compaction_truncation_and_reset() {
    let store = store_with_session();
    let first = store.dispatch(&json!({ "action": "create", "subject": "a" }));
    let first_state = store.state("s1");
    let second = store.dispatch(&json!({ "action": "create", "subject": "b" }));
    let full = store.state("s1");
    let entries = vec![
        user_entry("go"),
        entry_from(&first, "call-1", 1),
        entry_from(&second, "call-2", 2),
    ];
    assert!(store.replay("s1", &entries));
    assert_eq!(store.state("s1"), full);

    // Compaction truncates the newest snapshot: the surviving (older)
    // snapshot wins, exactly like a last-write-wins walk.
    let truncated = vec![user_entry("go"), entry_from(&first, "call-1", 1)];
    assert!(store.replay("s1", &truncated));
    assert_eq!(store.state("s1"), first_state);

    // Truncating everything (all todo results dropped) resets to empty.
    assert!(store.replay("s1", &[user_entry("go")]));
    assert_eq!(store.state("s1"), TodoState::empty());
    assert_eq!(store.state("s1").next_id, 1, "ids restart with the reset");
}

#[test]
fn replay_after_a_live_commit_keeps_the_newer_state() {
    let store = store_with_session();
    let first = store.dispatch(&json!({ "action": "create", "subject": "a" }));
    let entries = vec![entry_from(&first, "call-1", 1)];
    store.replay("s1", &entries);
    assert_eq!(store.state("s1").tasks.len(), 1);

    // A live commit lands as a whole-state replace; stale entries are
    // skipped because the replay fingerprint did not move.
    let second = store.dispatch(&json!({ "action": "create", "subject": "b" }));
    let live = store.state("s1");
    assert_eq!(live.tasks.len(), 2);
    assert!(!store.replay("s1", &entries), "stale entries are skipped");
    assert_eq!(store.state("s1"), live);

    // Once the transcript catches up, replay replaces with the same state.
    assert!(store.replay(
        "s1",
        &[entries[0].clone(), entry_from(&second, "call-2", 2)]
    ));
    assert_eq!(store.state("s1"), live);
}

#[test]
fn replay_deserializes_only_the_last_snapshot() {
    let store = store_with_session();
    let mut entries = Vec::new();
    for index in 0..100 {
        entries.push(entry_from(
            &store.dispatch(&json!({ "action": "create", "subject": format!("t{index}") })),
            &format!("call-{index}"),
            index,
        ));
    }
    assert!(store.replay("s1", &entries));
    assert_eq!(store.stats().snapshot().deserializations, 1);
    assert_eq!(store.state("s1").tasks.len(), 100);
    assert!(!store.replay("s1", &entries));
    assert_eq!(store.stats().snapshot().deserializations, 1);
}

#[test]
fn invalidate_replay_cache_makes_the_next_push_authoritative() {
    let store = store_with_session();
    let response = store.dispatch(&json!({ "action": "create", "subject": "a" }));
    let entries = vec![entry_from(&response, "call-1", 1)];
    assert!(store.replay("s1", &entries));
    store.replace_state("s1", TodoState::empty());
    store.invalidate_replay_cache();
    assert!(store.replay("s1", &entries), "invalidated cache replays");
    assert_eq!(store.state("s1").tasks.len(), 1);

    store.invalidate_replay_cache();
    assert!(store.replay("s1", &[user_entry("gone")]));
    assert_eq!(store.state("s1"), TodoState::empty());
}

#[test]
fn session_slots_are_bounded_by_lru() {
    let store = TodoStore::new();
    for index in 0..(MAX_SESSION_SLOTS + 3) {
        let session = format!("s{index}");
        store.set_active_session(&session);
        store.replace_state(&session, TodoState::empty());
    }
    let slots = store.session_slot_count();
    assert!(slots <= MAX_SESSION_SLOTS, "{slots} slots");
}

#[test]
fn existing_session_writes_do_not_evict_other_slots() {
    let store = TodoStore::new();
    for index in 0..MAX_SESSION_SLOTS {
        let session = format!("s{index}");
        store.set_active_session(&session);
        store.replace_state(
            &session,
            TodoState {
                tasks: vec![Task {
                    id: 1,
                    subject: session.clone(),
                    status: TaskStatus::Pending,
                    ..Task::default()
                }],
                next_id: 2,
            },
        );
    }
    assert_eq!(store.session_slot_count(), MAX_SESSION_SLOTS);

    store.replace_state("s0", store.state("s0"));
    assert_eq!(store.session_slot_count(), MAX_SESSION_SLOTS);
    for index in 0..MAX_SESSION_SLOTS {
        let session = format!("s{index}");
        assert!(
            !store.state(&session).tasks.is_empty(),
            "write to s0 evicted {session}"
        );
    }

    // The replay path shares the eviction helper.
    store.set_active_session("s0");
    store.replay("s0", &[]);
    for index in 1..MAX_SESSION_SLOTS {
        let session = format!("s{index}");
        assert!(
            !store.state(&session).tasks.is_empty(),
            "replay into s0 evicted {session}"
        );
    }
}

#[test]
fn clear_drops_slots_and_resets_the_active_session() {
    let store = store_with_session();
    store.dispatch(&json!({ "action": "create", "subject": "a" }));
    assert_eq!(store.session_slot_count(), 1);
    store.clear();
    assert_eq!(store.session_slot_count(), 0);
    assert_eq!(store.active_session(), "");
    assert_eq!(store.state("s1"), TodoState::empty());
}
