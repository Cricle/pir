//! `pi.inbox` and submission admission/terminal transitions (spec §6).
//!
//! These exercise the pure halves of `harness/inbox.ts` and
//! `harness/submissions.ts`: boundary selection and positional removal,
//! staleness, withdrawal, admission, `requestId` dedupe, and run settlement
//! under every `afterFailedRun` policy. Nothing here touches storage.

use pir::durable::harness::inbox::{
    boundary, check_input_entry, has_queued_reset, is_display_only, is_stale, is_stale_entry,
    remove_selected, withdraw_inputs, Boundary, InboxItem, InboxState, InputEntry, InputMode,
    Selected, SelectedKind, INBOX_DEFINITION, INBOX_KIND, INBOX_VERSION, REASON_ABORTED,
    REASON_STALE,
};
use pir::durable::harness::settings::{
    resolve_settings, AfterFailedRun, HarnessSettings, QueueMode, Settings,
};
use pir::durable::harness::submissions::{
    admission_status, admit, admit_in_inbox, find_request_submission, queued_item,
    resolve_request_id, settle_run_inputs, Admission, AdmissionStatus, RequestIdConflict,
    RequestIdResolution, RunOutcome, SubmissionDraft, WhenBusy, REASON_FAULTED, REASON_RUN_FAILED,
};
use pir::durable::session::SubmissionChange;
use pir::durable::{
    ConversationId, DocumentFork, DocumentHistory, DocumentSemantics, EntryId, SubmissionId,
    SubmissionRecord, SubmissionStatus, SubmissionType,
};
use serde_json::{json, Map, Value};

// ── helpers ────────────────────────────────────────────────────────────────

fn submission_id(raw: i64) -> SubmissionId {
    SubmissionId::from_raw(raw)
}

fn entry_id(raw: i64) -> EntryId {
    EntryId::from_raw(raw)
}

fn steer(id: i64, content: &str) -> InboxItem {
    InboxItem::Input {
        id: submission_id(id),
        mode: InputMode::Steer,
        content: json!(content),
        entry: None,
    }
}

fn follow_up(id: i64, content: &str) -> InboxItem {
    InboxItem::Input {
        id: submission_id(id),
        mode: InputMode::FollowUp,
        content: json!(content),
        entry: None,
    }
}

/// A write draft with only `kind` and `data`: display-only.
fn display_write(id: i64, kind: &str) -> InboxItem {
    let mut entry = Map::new();
    entry.insert("kind".to_owned(), json!(kind));
    entry.insert("data".to_owned(), json!({"text": format!("write-{id}")}));
    InboxItem::Write {
        id: submission_id(id),
        entry,
    }
}

/// A write draft with a `model`: selected at a boundary, not display-only.
fn model_write(id: i64, head: Value) -> InboxItem {
    let mut entry = Map::new();
    entry.insert("kind".to_owned(), json!("app.note"));
    entry.insert(
        "model".to_owned(),
        json!([{"role": "user", "content": "x"}]),
    );
    entry.insert("head".to_owned(), head);
    InboxItem::Write {
        id: submission_id(id),
        entry,
    }
}

fn inbox(items: Vec<InboxItem>) -> InboxState {
    InboxState { items }
}

fn modes(steering: QueueMode, follow_up: QueueMode) -> Settings {
    resolve_settings(&HarnessSettings {
        steering_mode: Some(steering),
        follow_up_mode: Some(follow_up),
        ..HarnessSettings::default()
    })
}

fn one_at_a_time() -> Settings {
    modes(QueueMode::OneAtATime, QueueMode::OneAtATime)
}

fn all_items() -> Settings {
    modes(QueueMode::All, QueueMode::All)
}

fn selected_ids(selected: &[Selected]) -> Vec<i64> {
    selected.iter().map(|item| item.id.as_i64()).collect()
}

fn selected_kinds(selected: &[Selected]) -> Vec<SelectedKind> {
    selected.iter().map(|item| item.kind).collect()
}

fn ids(items: &[InboxItem]) -> Vec<i64> {
    items.iter().map(|item| item.id().as_i64()).collect()
}

fn display_draft(kind: &str) -> SubmissionDraft {
    let mut entry = Map::new();
    entry.insert("kind".to_owned(), json!(kind));
    entry.insert("data".to_owned(), json!({"text": "shown"}));
    SubmissionDraft::Write {
        entry,
        request_id: None,
    }
}

fn model_draft() -> SubmissionDraft {
    let mut entry = Map::new();
    entry.insert("kind".to_owned(), json!("app.note"));
    entry.insert(
        "model".to_owned(),
        json!([{"role": "assistant", "content": "x"}]),
    );
    SubmissionDraft::Write {
        entry,
        request_id: None,
    }
}

fn input_draft(content: &str, when_busy: WhenBusy) -> SubmissionDraft {
    SubmissionDraft::Input {
        content: json!(content),
        when_busy,
        entry: None,
        request_id: None,
    }
}

fn conversation() -> ConversationId {
    ConversationId::from_raw(7)
}

fn submission_record(
    id: i64,
    submission_type: SubmissionType,
    status: SubmissionStatus,
    request_id: Option<&str>,
) -> SubmissionRecord {
    SubmissionRecord {
        trace: None,
        id: submission_id(id),
        conversation_id: conversation(),
        request_id: request_id.map(str::to_owned),
        submission_type,
        status,
        entry: None,
        answer: None,
        reason: None,
        detail: None,
    }
}

fn unanswered(changes: &[(SubmissionId, SubmissionChange)]) -> Vec<(i64, String)> {
    changes
        .iter()
        .filter_map(|(id, change)| match change {
            SubmissionChange::Unanswered { reason, .. } => Some((id.as_i64(), reason.clone())),
            _ => None,
        })
        .collect()
}

// ── the document definition ────────────────────────────────────────────────

#[test]
fn inbox_definition_matches_the_spec_table() {
    let definition = &*INBOX_DEFINITION;
    assert_eq!(definition.common.kind, INBOX_KIND);
    assert_eq!(definition.common.kind, "pi.inbox");
    assert_eq!(definition.common.version, INBOX_VERSION);
    assert_eq!(definition.common.version, 1);
    assert_eq!(
        definition.common.semantics,
        DocumentSemantics::ConversationLatest {
            fork: DocumentFork::Initial,
        }
    );
    assert_eq!(
        definition.common.semantics.history(),
        Some(DocumentHistory::Latest)
    );
    assert_eq!(
        definition.common.semantics.fork(),
        Some(DocumentFork::Initial)
    );
    assert_eq!(
        Value::Object(definition.initial_value()),
        json!({"items": []})
    );
    assert!(!definition.has_migration());
}

#[test]
fn inbox_checkpoint_predicate_is_true_only_when_empty() {
    let definition = &*INBOX_DEFINITION;
    assert!(definition.forces_checkpoint(&json!({"items": []}).as_object().unwrap().clone()));
    // The initial value reads as empty too.
    assert!(definition.forces_checkpoint(&definition.initial_value()));
    // Missing `items` is the initial shape.
    assert!(definition.forces_checkpoint(&Map::new()));
    // Anything queued is a delta, not a base.
    let queued = inbox(vec![steer(1, "hold")]);
    let value = serde_json::to_value(&queued).expect("serialize");
    assert!(!definition.forces_checkpoint(value.as_object().unwrap()));
    // The same holds with a write only, and with many items.
    let writes = inbox(vec![display_write(1, "app.note"), follow_up(2, "later")]);
    let value = serde_json::to_value(&writes).expect("serialize");
    assert!(!definition.forces_checkpoint(value.as_object().unwrap()));
}

#[test]
fn inbox_wire_shape_matches_upstream() {
    let state = inbox(vec![
        InboxItem::Input {
            id: submission_id(3),
            mode: InputMode::FollowUp,
            content: json!([{"type": "text", "text": "hi"}]),
            entry: Some(InputEntry {
                kind: "app.author".to_owned(),
                data: Some(json!({"name": "Ada"})),
            }),
        },
        steer(4, "stop"),
        display_write(5, "app.note"),
    ]);
    let value = serde_json::to_value(&state).expect("serialize");
    assert_eq!(
        value,
        json!({
            "items": [
                {
                    "id": 3,
                    "mode": "followUp",
                    "content": [{"type": "text", "text": "hi"}],
                    "entry": {"kind": "app.author", "data": {"name": "Ada"}},
                },
                {"id": 4, "mode": "steer", "content": "stop"},
                {"id": 5, "mode": "write", "entry": {"kind": "app.note", "data": {"text": "write-5"}}},
            ]
        })
    );
    let decoded: InboxState = serde_json::from_value(value).expect("decode");
    assert_eq!(decoded, state);

    // A malformed write item is rejected rather than silently dropped.
    assert!(serde_json::from_value::<InboxState>(json!({
        "items": [{"id": 1, "mode": "write", "entry": "not an object"}]
    }))
    .is_err());
    assert!(serde_json::from_value::<InboxState>(json!({
        "items": [{"id": 1, "mode": "bogus"}]
    }))
    .is_err());
}

// ── boundary table (spec §6) ────────────────────────────────────────────────

/// Items deliberately interleaved so positional selection must be explicit:
/// 1 steer, 2 write, 3 follow-up, 4 steer, 5 write, 6 follow-up.
fn mixed_inbox() -> InboxState {
    inbox(vec![
        steer(1, "one"),
        model_write(2, json!(null)),
        follow_up(3, "three"),
        steer(4, "four"),
        model_write(5, json!(null)),
        follow_up(6, "six"),
    ])
}

#[test]
fn post_tools_one_at_a_time_takes_all_writes_and_the_first_steer() {
    let state = mixed_inbox();
    let selected = boundary(&state, Boundary::PostTools, &one_at_a_time());

    // Writes first in ID order, then the first steer. No follow-up.
    assert_eq!(selected_ids(&selected), vec![2, 5, 1]);
    assert_eq!(
        selected_kinds(&selected),
        vec![SelectedKind::Write, SelectedKind::Write, SelectedKind::User]
    );
    // Positions are the ones in the state that was read.
    assert_eq!(
        selected.iter().map(|item| item.index).collect::<Vec<_>>(),
        vec![1, 4, 0]
    );
}

#[test]
fn post_tools_all_takes_all_writes_and_every_steer() {
    let state = mixed_inbox();
    let selected = boundary(&state, Boundary::PostTools, &all_items());
    assert_eq!(selected_ids(&selected), vec![2, 5, 1, 4]);
}

#[test]
fn final_one_at_a_time_takes_all_writes_first_steer_and_first_follow_up() {
    let state = mixed_inbox();
    let selected = boundary(&state, Boundary::Final, &one_at_a_time());
    assert_eq!(selected_ids(&selected), vec![2, 5, 1, 3]);
}

#[test]
fn final_all_takes_all_writes_steers_and_follow_ups() {
    let state = mixed_inbox();
    let selected = boundary(&state, Boundary::Final, &all_items());
    // Writes in ID order, then every steer in ID order (1, 4), then every
    // follow-up in ID order (3, 6) — upstream `take`.
    assert_eq!(selected_ids(&selected), vec![2, 5, 1, 4, 3, 6]);
}

#[test]
fn queue_modes_are_independent_per_mode() {
    let state = mixed_inbox();
    let steering_all = modes(QueueMode::All, QueueMode::OneAtATime);
    let selected = boundary(&state, Boundary::Final, &steering_all);
    assert_eq!(selected_ids(&selected), vec![2, 5, 1, 4, 3]);

    let follow_up_all = modes(QueueMode::OneAtATime, QueueMode::All);
    let selected = boundary(&state, Boundary::Final, &follow_up_all);
    assert_eq!(selected_ids(&selected), vec![2, 5, 1, 3, 6]);
}

#[test]
fn post_tools_with_a_queued_reset_selects_follow_ups_too() {
    // The reset (id 2) is a write, so every write is selected either way; the
    // reset makes the boundary `final`, so the follow-up (id 1) is selected.
    let state = inbox(vec![follow_up(1, "after"), model_write(2, json!("self"))]);
    assert!(has_queued_reset(&state));
    let selected = boundary(&state, Boundary::PostTools, &one_at_a_time());
    assert_eq!(selected_ids(&selected), vec![2, 1]);
}

#[test]
fn a_write_with_a_numeric_head_is_not_a_reset() {
    let state = inbox(vec![model_write(1, json!(40)), follow_up(2, "later")]);
    assert!(!has_queued_reset(&state));
    let selected = boundary(&state, Boundary::PostTools, &one_at_a_time());
    assert_eq!(
        selected_ids(&selected),
        vec![1],
        "no follow-up at postTools"
    );
}

#[test]
fn empty_inbox_selects_nothing() {
    let state = InboxState::new();
    for at in [Boundary::PostTools, Boundary::Final] {
        assert!(boundary(&state, at, &all_items()).is_empty());
    }
}

#[test]
fn selection_is_deterministic_by_item_id_position() {
    // Selecting twice from the same state yields the same result, and the
    // boundary function itself never mutates the inbox.
    let state = mixed_inbox();
    let first = boundary(&state, Boundary::Final, &one_at_a_time());
    let second = boundary(&state, Boundary::Final, &one_at_a_time());
    assert_eq!(first, second);
    assert_eq!(state.items.len(), 6);
}

// ── positional removal ─────────────────────────────────────────────────────

#[test]
fn remove_selected_preserves_retained_order() {
    let mut state = mixed_inbox();
    let selected = boundary(&state, Boundary::Final, &one_at_a_time());
    // Selected: 2 (idx 1), 5 (idx 4), 1 (idx 0), 3 (idx 2).
    remove_selected(&mut state, &selected);
    assert_eq!(ids(&state.items), vec![4, 6]);
}

#[test]
fn remove_selected_at_post_tools_keeps_follow_ups() {
    let mut state = mixed_inbox();
    let selected = boundary(&state, Boundary::PostTools, &one_at_a_time());
    remove_selected(&mut state, &selected);
    assert_eq!(ids(&state.items), vec![3, 4, 6]);
}

#[test]
fn remove_selected_supports_scattered_positions_and_all() {
    let mut state = mixed_inbox();
    let selected = boundary(&state, Boundary::Final, &all_items());
    remove_selected(&mut state, &selected);
    assert!(state.is_empty());

    // Non-adjacent positions in arbitrary order are removed correctly and an
    // out-of-range position is ignored (the caller may have already trimmed).
    let mut state = mixed_inbox();
    remove_selected(
        &mut state,
        &[
            Selected {
                index: 4,
                id: submission_id(5),
                kind: SelectedKind::Write,
            },
            Selected {
                index: 0,
                id: submission_id(1),
                kind: SelectedKind::User,
            },
            Selected {
                index: 99,
                id: submission_id(99),
                kind: SelectedKind::Write,
            },
        ],
    );
    assert_eq!(ids(&state.items), vec![2, 3, 4, 6]);
}

// ── stale heads ────────────────────────────────────────────────────────────

#[test]
fn a_head_before_the_active_range_start_is_stale() {
    let entry = model_write(1, json!(40)).write_entry().unwrap().clone();
    assert!(is_stale_entry(&entry, entry_id(50)));
    // Equal to the range start is not older: it is inside the range.
    assert!(!is_stale_entry(&entry, entry_id(40)));
    // Newer than the start is fine.
    assert!(!is_stale_entry(&entry, entry_id(30)));
}

#[test]
fn self_head_is_never_stale_and_no_head_is_never_stale() {
    let reset = model_write(1, json!("self")).write_entry().unwrap().clone();
    assert!(!is_stale_entry(&reset, entry_id(1_000)));
    let plain = model_write(1, json!(null)).write_entry().unwrap().clone();
    assert!(!is_stale_entry(&plain, entry_id(1_000)));
    let mut no_head = Map::new();
    no_head.insert("kind".to_owned(), json!("app.note"));
    assert!(!is_stale_entry(&no_head, entry_id(1_000)));
}

#[test]
fn inputs_are_never_stale() {
    assert!(!is_stale(&steer(1, "x"), entry_id(1_000)));
    assert!(!is_stale(&follow_up(2, "y"), entry_id(1_000)));
    assert!(is_stale(&model_write(3, json!(1)), entry_id(1_000)));
}

#[test]
fn a_queued_reset_makes_an_older_summary_stale() {
    // A queued reset (id 1) and a summary cutting at entry 70 (id 2), with the
    // active range starting at 10 when the boundary is prepared.
    let state = inbox(vec![
        model_write(1, json!("self")),
        model_write(2, json!(70)),
    ]);
    let selected = boundary(&state, Boundary::Final, &one_at_a_time());
    assert_eq!(selected_ids(&selected), vec![1, 2]);

    // Both are selected; the caller places the reset first and advances the
    // active range start to its new entry (100). The summary then targets 70,
    // which is older than 100.
    let mut active_range_start = entry_id(10);
    let reset = state.items[selected[0].index].write_entry().unwrap();
    assert!(!is_stale_entry(reset, active_range_start));
    active_range_start = entry_id(100);
    let summary = state.items[selected[1].index].write_entry().unwrap();
    assert!(
        is_stale_entry(summary, active_range_start),
        "a summary cutting before the reset's range is stale"
    );

    // The same summary is not stale against the pre-boundary range (70 >= 10).
    assert!(!is_stale_entry(summary, entry_id(10)));
}

#[test]
fn a_newer_summary_after_a_reset_is_placed() {
    let state = inbox(vec![
        model_write(1, json!("self")),
        model_write(2, json!(150)),
    ]);
    let selected = boundary(&state, Boundary::Final, &one_at_a_time());
    let active_range_start = entry_id(100);
    let summary = state.items[selected[1].index].write_entry().unwrap();
    assert!(!is_stale_entry(summary, active_range_start));
}

// ── admission table (spec §6) ──────────────────────────────────────────────

#[test]
fn idle_input_with_empty_inbox_is_placed() {
    let draft = input_draft("hello", WhenBusy::FollowUp);
    assert_eq!(
        admit(conversation(), false, false, &draft, false),
        Admission::Placed
    );
    assert_eq!(Admission::Placed.status(), Some(AdmissionStatus::Placed));
}

#[test]
fn idle_write_with_empty_inbox_is_done() {
    let draft = model_draft();
    assert_eq!(
        admit(conversation(), false, false, &draft, false),
        Admission::Done
    );
    assert_eq!(Admission::Done.status(), Some(AdmissionStatus::Done));
}

#[test]
fn display_only_write_is_placed_at_once_busy_or_idle() {
    let draft = display_draft("app.note");
    assert!(is_display_only(match &draft {
        SubmissionDraft::Write { entry, .. } => entry,
        _ => unreachable!(),
    }));
    for busy in [false, true] {
        for non_empty in [false, true] {
            assert_eq!(
                admit(conversation(), busy, non_empty, &draft, false),
                Admission::DisplayOnlyPlaced,
                "busy={busy} non_empty={non_empty}"
            );
        }
    }
    assert_eq!(
        Admission::DisplayOnlyPlaced.status(),
        Some(AdmissionStatus::Done)
    );
}

#[test]
fn display_only_write_queues_behind_a_queued_reset() {
    let draft = display_draft("app.note");
    // Even idle with an otherwise empty inbox, a queued reset holds it back.
    assert_eq!(
        admit(conversation(), false, true, &draft, true),
        Admission::Queued
    );
    // Busy too: the reset wins over the display-only shortcut.
    assert_eq!(
        admit(conversation(), true, true, &draft, true),
        Admission::Queued
    );
}

#[test]
fn every_write_that_is_not_display_only_is_busy_sensitive() {
    // `model` present counts: not display-only.
    let draft = model_draft();
    assert_eq!(
        admit(conversation(), true, false, &draft, false),
        Admission::Queued
    );
    assert_eq!(
        admit(conversation(), false, true, &draft, false),
        Admission::Queued
    );

    // `head` or `edits` present counts too, even when empty.
    for field in ["head", "edits"] {
        let mut entry = Map::new();
        entry.insert("kind".to_owned(), json!("app.note"));
        entry.insert(field.to_owned(), json!([]));
        let draft = SubmissionDraft::Write {
            entry,
            request_id: None,
        };
        assert!(!is_display_only(match &draft {
            SubmissionDraft::Write { entry, .. } => entry,
            _ => unreachable!(),
        }));
        assert_eq!(
            admit(conversation(), true, false, &draft, false),
            Admission::Queued
        );
    }
}

#[test]
fn busy_input_queues_or_rejects_by_when_busy() {
    let steer_draft = input_draft("steer me", WhenBusy::Steer);
    assert_eq!(
        admit(conversation(), true, false, &steer_draft, false),
        Admission::Queued
    );
    let follow_draft = input_draft("later", WhenBusy::FollowUp);
    assert_eq!(
        admit(conversation(), true, false, &follow_draft, false),
        Admission::Queued
    );

    let reject_draft = input_draft("no", WhenBusy::Reject);
    let admission = admit(conversation(), true, false, &reject_draft, false);
    assert!(admission.is_rejected());
    assert_eq!(
        admission,
        Admission::Rejected(pir::durable::errors::ConversationBusy {
            conversation_id: conversation()
        })
    );
    assert_eq!(admission.status(), None);
}

#[test]
fn reject_when_busy_only_matters_while_busy() {
    // An idle conversation places a `reject` input: the mode decides only what
    // a busy conversation does.
    let draft = input_draft("no", WhenBusy::Reject);
    assert_eq!(
        admit(conversation(), false, false, &draft, false),
        Admission::Placed
    );
    assert_eq!(
        admit(conversation(), false, true, &draft, false),
        Admission::Queued
    );
}

#[test]
fn non_empty_inbox_queues_new_items_behind_it() {
    // Idle with queued items: a new input queues behind them.
    let draft = input_draft("new", WhenBusy::FollowUp);
    assert_eq!(
        admit(conversation(), false, true, &draft, false),
        Admission::Queued
    );
    // A non-display write does too.
    assert_eq!(
        admit(conversation(), false, true, &model_draft(), false),
        Admission::Queued
    );
}

#[test]
fn queued_mode_follows_when_busy() {
    assert_eq!(WhenBusy::Steer.mode(), InputMode::Steer);
    assert_eq!(WhenBusy::FollowUp.mode(), InputMode::FollowUp);
    assert_eq!(WhenBusy::Reject.mode(), InputMode::FollowUp);
    assert_eq!(WhenBusy::default(), WhenBusy::FollowUp);
}

#[test]
fn queued_item_carries_the_draft() {
    let id = submission_id(11);
    let steer_draft = SubmissionDraft::Input {
        content: json!([{"type": "text", "text": "hi"}]),
        when_busy: WhenBusy::Steer,
        entry: Some(InputEntry {
            kind: "app.author".to_owned(),
            data: Some(json!({"name": "Ada"})),
        }),
        request_id: None,
    };
    assert_eq!(
        queued_item(id, &steer_draft),
        InboxItem::Input {
            id,
            mode: InputMode::Steer,
            content: json!([{"type": "text", "text": "hi"}]),
            entry: Some(InputEntry {
                kind: "app.author".to_owned(),
                data: Some(json!({"name": "Ada"})),
            }),
        }
    );
    let write_draft = display_draft("app.note");
    let SubmissionDraft::Write { entry, .. } = write_draft.clone() else {
        unreachable!()
    };
    assert_eq!(
        queued_item(id, &write_draft),
        InboxItem::Write { id, entry }
    );

    // Admission against the inbox's own flags agrees with `admit`.
    let state = inbox(vec![steer(1, "a")]);
    assert_eq!(
        admit_in_inbox(
            conversation(),
            false,
            &state,
            &input_draft("b", WhenBusy::FollowUp)
        ),
        Admission::Queued
    );
}

#[test]
fn input_entry_kind_must_be_application_owned() {
    let ok = InputEntry {
        kind: "app.author".to_owned(),
        data: None,
    };
    assert!(check_input_entry(&ok).is_ok());
    for kind in ["", "pi.user", "pi.anything"] {
        let entry = InputEntry {
            kind: kind.to_owned(),
            data: None,
        };
        assert!(check_input_entry(&entry).is_err(), "kind {kind:?}");
    }
}

// ── requestId dedupe ───────────────────────────────────────────────────────

#[test]
fn request_id_without_a_key_or_record_is_fresh() {
    assert_eq!(
        resolve_request_id(SubmissionType::Input, None, None).unwrap(),
        RequestIdResolution::Fresh
    );
    assert_eq!(
        resolve_request_id(SubmissionType::Input, Some("r1"), None).unwrap(),
        RequestIdResolution::Fresh
    );
}

#[test]
fn request_id_reuses_the_existing_submission_of_the_same_type() {
    let cases = [
        (SubmissionStatus::Queued, AdmissionStatus::Queued),
        (SubmissionStatus::Placed, AdmissionStatus::Placed),
        (SubmissionStatus::Done, AdmissionStatus::Done),
        (SubmissionStatus::Unanswered, AdmissionStatus::Done),
    ];
    for (status, expected) in cases {
        for submission_type in [SubmissionType::Input, SubmissionType::Write] {
            let record = submission_record(9, submission_type, status, Some("r1"));
            assert_eq!(
                resolve_request_id(submission_type, Some("r1"), Some(&record)).unwrap(),
                RequestIdResolution::Existing {
                    id: submission_id(9),
                    status: expected,
                },
                "status {status:?}"
            );
        }
    }
}

#[test]
fn request_id_reused_across_types_rejects() {
    let write_record =
        submission_record(9, SubmissionType::Write, SubmissionStatus::Done, Some("r1"));
    let error =
        resolve_request_id(SubmissionType::Input, Some("r1"), Some(&write_record)).unwrap_err();
    assert_eq!(error, RequestIdConflict::new("r1", SubmissionType::Write));
    assert_eq!(
        error.to_string(),
        "Request r1 already identifies a submission of type write"
    );

    let input_record = submission_record(
        9,
        SubmissionType::Input,
        SubmissionStatus::Placed,
        Some("r1"),
    );
    assert!(resolve_request_id(SubmissionType::Write, Some("r1"), Some(&input_record)).is_err());
}

#[test]
fn request_id_is_scoped_to_one_conversation() {
    let record = submission_record(
        9,
        SubmissionType::Input,
        SubmissionStatus::Queued,
        Some("r1"),
    );
    let records = [record];
    assert_eq!(
        find_request_submission(records.iter(), conversation(), "r1")
            .map(|record| record.id.as_i64()),
        Some(9)
    );
    assert!(find_request_submission(records.iter(), ConversationId::from_raw(99), "r1").is_none());
    assert!(find_request_submission(records.iter(), conversation(), "r2").is_none());
}

#[test]
fn admission_status_maps_terminal_unanswered_to_done() {
    assert_eq!(
        admission_status(SubmissionStatus::Queued),
        AdmissionStatus::Queued
    );
    assert_eq!(
        admission_status(SubmissionStatus::Placed),
        AdmissionStatus::Placed
    );
    assert_eq!(
        admission_status(SubmissionStatus::Done),
        AdmissionStatus::Done
    );
    assert_eq!(
        admission_status(SubmissionStatus::Unanswered),
        AdmissionStatus::Done
    );
}

// ── run settlement: five outcomes × three policies ─────────────────────────

fn settled_ids(changes: &[(SubmissionId, SubmissionChange)]) -> Vec<i64> {
    changes.iter().map(|(id, _)| id.as_i64()).collect()
}

/// The inputs a run placed.
fn run_inputs() -> Vec<SubmissionId> {
    vec![submission_id(1), submission_id(2)]
}

/// Items queued behind the run: a steer, a follow-up and a write.
fn queued_behind() -> InboxState {
    inbox(vec![
        steer(10, "steer"),
        display_write(11, "app.note"),
        follow_up(12, "later"),
    ])
}

#[test]
fn answered_run_settles_inputs_done_under_every_policy() {
    for policy in [
        AfterFailedRun::Keep,
        AfterFailedRun::Continue,
        AfterFailedRun::Withdraw,
    ] {
        let changes = settle_run_inputs(
            &run_inputs(),
            &queued_behind(),
            RunOutcome::Answered(entry_id(50)),
            policy,
        );
        assert_eq!(settled_ids(&changes), vec![1, 2]);
        for (_, change) in &changes {
            assert_eq!(
                change,
                &SubmissionChange::Done {
                    answer: entry_id(50)
                }
            );
        }
    }
}

#[test]
fn failed_run_settles_inputs_unanswered_and_applies_the_policy() {
    let outcome = || RunOutcome::Failed("model_error".to_owned(), Some(json!("boom")));

    // keep: inputs only; queued items stay.
    let keep = settle_run_inputs(
        &run_inputs(),
        &queued_behind(),
        outcome(),
        AfterFailedRun::Keep,
    );
    assert_eq!(settled_ids(&keep), vec![1, 2]);
    assert_eq!(
        unanswered(&keep),
        vec![(1, "model_error".to_owned()), (2, "model_error".to_owned())]
    );
    assert!(keep.iter().all(|(_, change)| matches!(
        change,
        SubmissionChange::Unanswered { detail: Some(detail), .. } if detail == &json!("boom")
    )));

    // continue: the boundary places them in the same commit; settlement adds
    // nothing here.
    let continue_policy = settle_run_inputs(
        &run_inputs(),
        &queued_behind(),
        outcome(),
        AfterFailedRun::Continue,
    );
    assert_eq!(settled_ids(&continue_policy), vec![1, 2]);

    // withdraw: queued steer/follow-ups settle `run_failed`; the write stays.
    let withdraw = settle_run_inputs(
        &run_inputs(),
        &queued_behind(),
        outcome(),
        AfterFailedRun::Withdraw,
    );
    assert_eq!(settled_ids(&withdraw), vec![1, 2, 10, 12]);
    assert_eq!(
        unanswered(&withdraw),
        vec![
            (1, "model_error".to_owned()),
            (2, "model_error".to_owned()),
            (10, REASON_RUN_FAILED.to_owned()),
            (12, REASON_RUN_FAILED.to_owned()),
        ]
    );
    assert!(
        !settled_ids(&withdraw).contains(&11),
        "queued writes stay for later placement"
    );
}

#[test]
fn aborted_run_settles_inputs_aborted_and_keeps_the_inbox_under_every_policy() {
    for policy in [
        AfterFailedRun::Keep,
        AfterFailedRun::Continue,
        AfterFailedRun::Withdraw,
    ] {
        let changes =
            settle_run_inputs(&run_inputs(), &queued_behind(), RunOutcome::Aborted, policy);
        assert_eq!(settled_ids(&changes), vec![1, 2]);
        assert_eq!(
            unanswered(&changes),
            vec![
                (1, REASON_ABORTED.to_owned()),
                (2, REASON_ABORTED.to_owned())
            ]
        );
    }
}

#[test]
fn faulted_run_settles_inputs_with_the_error_detail_under_every_policy() {
    for policy in [
        AfterFailedRun::Keep,
        AfterFailedRun::Continue,
        AfterFailedRun::Withdraw,
    ] {
        let changes = settle_run_inputs(
            &run_inputs(),
            &queued_behind(),
            RunOutcome::Faulted(json!("phase threw")),
            policy,
        );
        assert_eq!(settled_ids(&changes), vec![1, 2]);
        assert_eq!(
            unanswered(&changes),
            vec![
                (1, REASON_FAULTED.to_owned()),
                (2, REASON_FAULTED.to_owned())
            ]
        );
        assert!(changes.iter().all(|(_, change)| matches!(
            change,
            SubmissionChange::Unanswered { detail: Some(detail), .. } if detail == &json!("phase threw")
        )));
    }
}

#[test]
fn orphaned_run_settles_inputs_with_the_orphan_reason_under_every_policy() {
    for policy in [
        AfterFailedRun::Keep,
        AfterFailedRun::Continue,
        AfterFailedRun::Withdraw,
    ] {
        let changes = settle_run_inputs(
            &run_inputs(),
            &queued_behind(),
            RunOutcome::Orphaned("missing_task".to_owned()),
            policy,
        );
        assert_eq!(settled_ids(&changes), vec![1, 2]);
        assert_eq!(
            unanswered(&changes),
            vec![
                (1, "missing_task".to_owned()),
                (2, "missing_task".to_owned())
            ]
        );
    }
}

#[test]
fn only_failures_are_governed_by_after_failed_run() {
    assert!(RunOutcome::Failed("model_error".to_owned(), None).is_failure());
    assert!(RunOutcome::Failed("no_model".to_owned(), None).is_failure());
    assert!(!RunOutcome::Answered(entry_id(1)).is_failure());
    assert!(!RunOutcome::Aborted.is_failure());
    assert!(!RunOutcome::Faulted(json!("x")).is_failure());
    assert!(!RunOutcome::Orphaned("y".to_owned()).is_failure());

    // An empty inbox means withdrawal has nothing extra to settle.
    let changes = settle_run_inputs(
        &run_inputs(),
        &InboxState::new(),
        RunOutcome::Failed("model_error".to_owned(), None),
        AfterFailedRun::Withdraw,
    );
    assert_eq!(settled_ids(&changes), vec![1, 2]);
}

#[test]
fn withdraw_run_failure_ignores_already_placed_inputs_list() {
    // `run.inputs` lists only what the run placed; queued items are settled by
    // the policy, never by the input list.
    let changes = settle_run_inputs(
        &[],
        &queued_behind(),
        RunOutcome::Failed("no_model".to_owned(), None),
        AfterFailedRun::Withdraw,
    );
    assert_eq!(settled_ids(&changes), vec![10, 12]);
}

// ── withdrawal ─────────────────────────────────────────────────────────────

#[test]
fn withdraw_inputs_keeps_writes_and_preserves_their_order() {
    let mut state = inbox(vec![
        steer(1, "a"),
        display_write(2, "app.one"),
        follow_up(3, "b"),
        model_write(4, json!(null)),
        steer(5, "c"),
    ]);
    let withdrawn = withdraw_inputs(&mut state);
    assert_eq!(
        withdrawn.iter().map(|id| id.as_i64()).collect::<Vec<_>>(),
        vec![1, 3, 5]
    );
    assert_eq!(ids(&state.items), vec![2, 4]);
    assert!(state.items.iter().all(InboxItem::is_write));
}

#[test]
fn withdraw_inputs_on_a_writes_only_inbox_is_a_no_op() {
    let mut state = inbox(vec![display_write(1, "app.a"), display_write(2, "app.b")]);
    assert!(withdraw_inputs(&mut state).is_empty());
    assert_eq!(ids(&state.items), vec![1, 2]);
}

#[test]
fn settlement_reasons_match_the_spec() {
    assert_eq!(REASON_ABORTED, "aborted");
    assert_eq!(REASON_STALE, "stale");
    assert_eq!(REASON_RUN_FAILED, "run_failed");
    assert_eq!(REASON_FAULTED, "faulted");
}
