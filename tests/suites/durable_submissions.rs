//! `harness/submissions.rs`: admission decisions, `requestId` reuse and run
//! settlement.
//!
//! `durable_inbox.rs` owns the inbox half of spec §6 (the document definition,
//! boundary selection, positional removal, staleness and withdrawal). This
//! suite stays on the submission state machine and on the seam the facade uses:
//!
//! * [`admit_in_inbox`] driven by real `InboxState`s, including the branch
//!   order (display-only before reject/queue) and a genuinely pending reset;
//! * [`resolve_request_id`] over records located through
//!   [`find_request_submission`], plus the full four-status × two-type reuse
//!   matrix and both conflict directions;
//! * the five [`RunOutcome`]s × three [`AfterFailedRun`] policies with richer
//!   inputs and a mixed queue, plus purity and ordering properties;
//! * `requestId` scoping through `MemoryStorage` and the `Session` facade, so
//!   the pure predicate and the storage seam are checked against each other.
//!
//! Registered in `tests/g_durable.rs` as `mod durable_submissions;`.

use std::sync::Arc;

use serde_json::{json, Value};

use pir::durable::errors::ConversationBusy;
use pir::durable::harness::inbox::{
    has_queued_reset, InboxItem, InboxState, InputMode, REASON_ABORTED,
};
use pir::durable::harness::settings::AfterFailedRun;
use pir::durable::harness::submissions::{
    admission_status, admit_in_inbox, existing_type_name, find_request_submission,
    resolve_request_id, settle_run_inputs, Admission, AdmissionStatus, RequestIdConflict,
    RequestIdResolution, RunOutcome, SubmissionDraft, WhenBusy, REASON_FAULTED, REASON_RUN_FAILED,
};
use pir::durable::session::{
    ConversationOwnership, Session, SessionOptions, SubmissionChange, SubmissionCreate,
};
use pir::durable::{
    Context, ConversationId, EntryId, JsonObject, MemoryStorage, Storage, SubmissionId,
    SubmissionQuery, SubmissionRecord, SubmissionStatus, SubmissionType,
};

// ── helpers ────────────────────────────────────────────────────────────────

fn submission_id(raw: i64) -> SubmissionId {
    SubmissionId::from_raw(raw)
}

fn entry_id(raw: i64) -> EntryId {
    EntryId::from_raw(raw)
}

fn conversation(raw: i64) -> ConversationId {
    ConversationId::from_raw(raw)
}

/// Conversation A: the subject of most pure tests.
fn conversation_a() -> ConversationId {
    conversation(41)
}

/// Conversation B: the same-conversation-isolation twin.
fn conversation_b() -> ConversationId {
    conversation(42)
}

fn entry(fields: &[(&str, Value)]) -> JsonObject {
    let mut entry = JsonObject::new();
    for (key, value) in fields {
        entry.insert((*key).to_owned(), value.clone());
    }
    entry
}

fn input_draft(content: &str, when_busy: WhenBusy) -> SubmissionDraft {
    SubmissionDraft::Input {
        content: json!(content),
        when_busy,
        entry: None,
        request_id: None,
    }
}

/// Only `kind` and `data`: the display-only shape.
fn display_only_write() -> SubmissionDraft {
    SubmissionDraft::Write {
        entry: entry(&[
            ("kind", json!("app.note")),
            ("data", json!({"text": "shown"})),
        ]),
        request_id: None,
    }
}

/// A `model` key is present, so this write needs a boundary.
fn model_write_draft() -> SubmissionDraft {
    SubmissionDraft::Write {
        entry: entry(&[
            ("kind", json!("app.note")),
            ("model", json!([{"role": "assistant", "content": "x"}])),
        ]),
        request_id: None,
    }
}

fn inbox(items: Vec<InboxItem>) -> InboxState {
    InboxState { items }
}

fn queued_input(id: i64, mode: InputMode, content: &str) -> InboxItem {
    InboxItem::Input {
        id: submission_id(id),
        mode,
        content: json!(content),
        entry: None,
    }
}

fn queued_write(id: i64, entry: JsonObject) -> InboxItem {
    InboxItem::Write {
        id: submission_id(id),
        entry,
    }
}

/// A queued reset (`head: "self"`), which is never display-only.
fn reset_write(id: i64) -> InboxItem {
    queued_write(
        id,
        entry(&[("kind", json!("app.note")), ("head", json!("self"))]),
    )
}

fn record(
    id: i64,
    conversation_id: ConversationId,
    submission_type: SubmissionType,
    status: SubmissionStatus,
    request_id: Option<&str>,
) -> SubmissionRecord {
    SubmissionRecord {
        trace: None,
        id: submission_id(id),
        conversation_id,
        request_id: request_id.map(str::to_owned),
        submission_type,
        status,
        entry: None,
        answer: None,
        reason: None,
        detail: None,
    }
}

fn policies() -> [AfterFailedRun; 3] {
    [
        AfterFailedRun::Keep,
        AfterFailedRun::Continue,
        AfterFailedRun::Withdraw,
    ]
}

fn ids(changes: &[(SubmissionId, SubmissionChange)]) -> Vec<i64> {
    changes.iter().map(|(id, _)| id.as_i64()).collect()
}

/// Every change as `(id, reason, detail)`; panics on a non-`Unanswered` one.
fn unanswered(changes: &[(SubmissionId, SubmissionChange)]) -> Vec<(i64, String, Option<Value>)> {
    changes
        .iter()
        .map(|(id, change)| match change {
            SubmissionChange::Unanswered { reason, detail } => {
                (id.as_i64(), reason.clone(), detail.clone())
            }
            other => panic!("expected an unanswered change, got {other:?}"),
        })
        .collect()
}

fn unanswered_for(
    ids: &[i64],
    reason: &str,
    detail: Option<Value>,
) -> Vec<(i64, String, Option<Value>)> {
    ids.iter()
        .map(|id| (*id, reason.to_owned(), detail.clone()))
        .collect()
}

// ── draft and admission reporting ──────────────────────────────────────────

#[test]
fn draft_accessors_report_the_wire_type_and_queue_mode() {
    for (when_busy, expected_mode) in [
        (WhenBusy::Steer, InputMode::Steer),
        (WhenBusy::FollowUp, InputMode::FollowUp),
        // `reject` only decides behaviour while busy; it queues as a follow-up.
        (WhenBusy::Reject, InputMode::FollowUp),
    ] {
        let draft = input_draft("hello", when_busy);
        assert_eq!(draft.submission_type(), SubmissionType::Input);
        assert_eq!(draft.when_busy(), when_busy);
        assert_eq!(when_busy.mode(), expected_mode);
    }

    // A write has no busy policy of its own and reports the default.
    let draft = display_only_write();
    assert_eq!(draft.submission_type(), SubmissionType::Write);
    assert_eq!(draft.when_busy(), WhenBusy::FollowUp);
    let draft = model_write_draft();
    assert_eq!(draft.submission_type(), SubmissionType::Write);
    assert_eq!(draft.when_busy(), WhenBusy::FollowUp);
}

#[test]
fn admission_status_reports_each_branch() {
    assert_eq!(Admission::Placed.status(), Some(AdmissionStatus::Placed));
    assert_eq!(
        Admission::DisplayOnlyPlaced.status(),
        Some(AdmissionStatus::Done)
    );
    assert_eq!(Admission::Done.status(), Some(AdmissionStatus::Done));
    assert_eq!(Admission::Queued.status(), Some(AdmissionStatus::Queued));

    let rejected = Admission::Rejected(ConversationBusy {
        conversation_id: conversation_a(),
    });
    assert_eq!(rejected.status(), None);
    assert!(rejected.is_rejected());

    for admission in [
        Admission::Placed,
        Admission::DisplayOnlyPlaced,
        Admission::Done,
        Admission::Queued,
    ] {
        assert!(!admission.is_rejected(), "{admission:?} writes something");
    }
}

// ── admit(): the five branches through admit_in_inbox ──────────────────────

#[test]
fn idle_empty_inbox_places_inputs_and_settles_writes() {
    let empty = InboxState::new();

    // An idle input places and starts a run. `reject` is irrelevant here.
    for when_busy in [WhenBusy::Steer, WhenBusy::FollowUp, WhenBusy::Reject] {
        let admission = admit_in_inbox(
            conversation_a(),
            false,
            &empty,
            &input_draft("hello", when_busy),
        );
        assert_eq!(admission, Admission::Placed);
        assert_eq!(admission.status(), Some(AdmissionStatus::Placed));
    }

    // An idle write needs no boundary and is done at once.
    let admission = admit_in_inbox(conversation_a(), false, &empty, &model_write_draft());
    assert_eq!(admission, Admission::Done);
    assert_eq!(admission.status(), Some(AdmissionStatus::Done));
}

#[test]
fn busy_or_non_empty_inbox_queues_inputs_and_writes() {
    let empty = InboxState::new();
    let held = inbox(vec![queued_input(1, InputMode::FollowUp, "held")]);
    let states: [(bool, &InboxState); 3] = [(true, &empty), (false, &held), (true, &held)];

    let input = input_draft("next", WhenBusy::FollowUp);
    for (busy, state) in states {
        let admission = admit_in_inbox(conversation_a(), busy, state, &input);
        assert_eq!(admission, Admission::Queued, "busy={busy}");
        assert_eq!(admission.status(), Some(AdmissionStatus::Queued));
        assert!(!admission.is_rejected());
    }

    // A non-display write queues under exactly the same conditions.
    for (busy, state) in states {
        assert_eq!(
            admit_in_inbox(conversation_a(), busy, state, &model_write_draft()),
            Admission::Queued,
            "busy={busy}"
        );
    }

    // An idle `reject` input queues behind existing items rather than
    // rejecting: the policy only applies while busy.
    assert_eq!(
        admit_in_inbox(
            conversation_a(),
            false,
            &held,
            &input_draft("later", WhenBusy::Reject)
        ),
        Admission::Queued
    );
}

#[test]
fn busy_input_with_when_busy_reject_is_rejected_once() {
    let empty = InboxState::new();
    let held = inbox(vec![queued_input(1, InputMode::Steer, "held")]);

    for state in [&empty, &held] {
        let admission = admit_in_inbox(
            conversation_a(),
            true,
            state,
            &input_draft("no", WhenBusy::Reject),
        );
        assert_eq!(
            admission,
            Admission::Rejected(ConversationBusy {
                conversation_id: conversation_a()
            })
        );
        assert_eq!(admission.status(), None);
        assert!(admission.is_rejected());
    }

    // The other two policies queue instead of rejecting while busy.
    for when_busy in [WhenBusy::Steer, WhenBusy::FollowUp] {
        assert_eq!(
            admit_in_inbox(
                conversation_a(),
                true,
                &empty,
                &input_draft("wait", when_busy)
            ),
            Admission::Queued
        );
    }
}

#[test]
fn display_only_write_bypasses_busy_and_the_queue() {
    let empty = InboxState::new();
    let held = inbox(vec![
        queued_input(1, InputMode::Steer, "held"),
        resetless_write(2),
    ]);
    let states: [(bool, &InboxState); 4] = [
        (false, &empty),
        (true, &empty),
        (false, &held),
        (true, &held),
    ];

    for (busy, state) in states {
        let admission = admit_in_inbox(conversation_a(), busy, state, &display_only_write());
        assert_eq!(
            admission,
            Admission::DisplayOnlyPlaced,
            "busy={busy}: a display-only write is appended and done without a boundary"
        );
        assert_eq!(admission.status(), Some(AdmissionStatus::Done));
    }

    // A write that is not display-only takes the queueing path while busy.
    assert_eq!(
        admit_in_inbox(conversation_a(), true, &empty, &model_write_draft()),
        Admission::Queued
    );
}

/// A write with only `kind` and `data`, used to make the inbox non-empty
/// without introducing a reset.
fn resetless_write(id: i64) -> InboxItem {
    queued_write(
        id,
        entry(&[("kind", json!("app.note")), ("data", json!({"n": id}))]),
    )
}

#[test]
fn a_pending_reset_disables_the_display_only_shortcut() {
    let draft = display_only_write();

    // With an actual reset queued, the display-only write must wait behind it:
    // appended before the reset it would fall outside the context the reset
    // starts.
    let with_reset = inbox(vec![reset_write(2)]);
    assert!(has_queued_reset(&with_reset));
    for busy in [false, true] {
        assert_eq!(
            admit_in_inbox(conversation_a(), busy, &with_reset, &draft),
            Admission::Queued,
            "busy={busy}: the reset wins over the display-only shortcut"
        );
    }

    // The reset also wins with unrelated items around it.
    let reset_between = inbox(vec![
        queued_input(1, InputMode::Steer, "before"),
        reset_write(2),
        queued_input(3, InputMode::FollowUp, "after"),
    ]);
    assert!(has_queued_reset(&reset_between));
    assert_eq!(
        admit_in_inbox(conversation_a(), false, &reset_between, &draft),
        Admission::Queued
    );

    // Without a reset the same non-empty inbox still takes the shortcut.
    let without_reset = inbox(vec![
        queued_input(1, InputMode::Steer, "before"),
        resetless_write(3),
    ]);
    assert!(!has_queued_reset(&without_reset));
    assert_eq!(
        admit_in_inbox(conversation_a(), false, &without_reset, &draft),
        Admission::DisplayOnlyPlaced
    );
}

// ── requestId resolution ───────────────────────────────────────────────────

#[test]
fn request_id_is_fresh_without_a_key_or_a_record() {
    assert_eq!(
        resolve_request_id(SubmissionType::Input, None, None).expect("no key"),
        RequestIdResolution::Fresh
    );

    // No key ignores an existing record: deduplication is keyed, not implicit.
    let existing = record(
        1,
        conversation_a(),
        SubmissionType::Input,
        SubmissionStatus::Queued,
        None,
    );
    assert_eq!(
        resolve_request_id(SubmissionType::Input, None, Some(&existing)).expect("no key"),
        RequestIdResolution::Fresh
    );

    // A key the conversation never used is fresh.
    assert_eq!(
        resolve_request_id(SubmissionType::Write, Some("unused"), None).expect("no record"),
        RequestIdResolution::Fresh
    );
}

#[test]
fn existing_request_id_matrix_covers_four_statuses_and_both_types() {
    let statuses = [
        (SubmissionStatus::Queued, AdmissionStatus::Queued),
        (SubmissionStatus::Placed, AdmissionStatus::Placed),
        (SubmissionStatus::Done, AdmissionStatus::Done),
        // A terminal unanswered submission is reported as done: the host asked
        // about something that no longer needs an answer.
        (SubmissionStatus::Unanswered, AdmissionStatus::Done),
    ];

    for (status, expected) in statuses {
        assert_eq!(admission_status(status), expected, "status {status:?}");
        for submission_type in [SubmissionType::Input, SubmissionType::Write] {
            let target = record(
                90,
                conversation_a(),
                submission_type,
                status,
                Some("reused"),
            );
            // Same key in another conversation: the lookup must select the
            // target even when the decoy is stored first.
            let decoy = record(
                91,
                conversation_b(),
                submission_type,
                SubmissionStatus::Done,
                Some("reused"),
            );
            let records = [decoy, target.clone()];

            let found = find_request_submission(records.iter(), conversation_a(), "reused")
                .expect("the key exists in conversation A");
            assert_eq!(found, &target);

            let resolution =
                resolve_request_id(submission_type, Some("reused"), Some(found)).expect("reuse");
            assert_eq!(
                resolution,
                RequestIdResolution::Existing {
                    id: target.id,
                    status: expected,
                },
                "type {submission_type:?} status {status:?}"
            );
        }
    }
}

#[test]
fn request_id_reused_across_types_rejects_in_both_directions() {
    let input = record(
        1,
        conversation_a(),
        SubmissionType::Input,
        SubmissionStatus::Placed,
        Some("key"),
    );
    let error = resolve_request_id(SubmissionType::Write, Some("key"), Some(&input))
        .expect_err("write draft cannot reuse an input key");
    assert_eq!(error, RequestIdConflict::new("key", SubmissionType::Input));
    assert_eq!(
        error.to_string(),
        "Request key already identifies a submission of type input"
    );

    let write = record(
        2,
        conversation_a(),
        SubmissionType::Write,
        SubmissionStatus::Done,
        Some("key"),
    );
    let error = resolve_request_id(SubmissionType::Input, Some("key"), Some(&write))
        .expect_err("input draft cannot reuse a write key");
    assert_eq!(error, RequestIdConflict::new("key", SubmissionType::Write));
    assert_eq!(
        error.to_string(),
        "Request key already identifies a submission of type write"
    );

    // The wire names come from serde, not from a second hand-written table.
    assert_eq!(existing_type_name(SubmissionType::Input), "input");
    assert_eq!(existing_type_name(SubmissionType::Write), "write");

    // The conflict is a standard error (hosts can box it).
    fn assert_std_error<E: std::error::Error>() {}
    assert_std_error::<RequestIdConflict>();
}

#[test]
fn find_request_submission_keeps_the_same_key_in_different_conversations_apart() {
    let a = record(
        10,
        conversation_a(),
        SubmissionType::Input,
        SubmissionStatus::Queued,
        Some("shared"),
    );
    let b = record(
        20,
        conversation_b(),
        SubmissionType::Write,
        SubmissionStatus::Done,
        Some("shared"),
    );
    let no_key = record(
        30,
        conversation_a(),
        SubmissionType::Input,
        SubmissionStatus::Unanswered,
        None,
    );
    let records = [a.clone(), b.clone(), no_key];

    assert_eq!(
        find_request_submission(records.iter(), conversation_a(), "shared").map(|r| r.id),
        Some(a.id)
    );
    assert_eq!(
        find_request_submission(records.iter(), conversation_b(), "shared").map(|r| r.id),
        Some(b.id)
    );

    // Other keys and other conversations never match.
    assert!(find_request_submission(records.iter(), conversation_a(), "other").is_none());
    assert!(find_request_submission(records.iter(), conversation(99), "shared").is_none());

    // A record without a requestId is not a wildcard: even the empty key misses.
    let only_no_key = [record(
        31,
        conversation_a(),
        SubmissionType::Input,
        SubmissionStatus::Done,
        None,
    )];
    assert!(find_request_submission(only_no_key.iter(), conversation_a(), "").is_none());
}

// ── run settlement: five outcomes × three policies ─────────────────────────

/// The inputs a run placed, deliberately not in numeric order so ordering
/// cannot come from sorting.
fn run_inputs() -> Vec<SubmissionId> {
    vec![submission_id(23), submission_id(21), submission_id(22)]
}

/// Items queued behind the run, interleaved so write skipping and user order
/// are both observable.
fn settlement_inbox() -> InboxState {
    inbox(vec![
        queued_input(31, InputMode::Steer, "steer"),
        queued_write(
            32,
            entry(&[("kind", json!("app.note")), ("data", json!({"n": 1}))]),
        ),
        queued_input(33, InputMode::FollowUp, "follow"),
        queued_write(
            34,
            entry(&[("kind", json!("app.note")), ("model", json!([]))]),
        ),
        queued_input(35, InputMode::Steer, "steer again"),
    ])
}

#[test]
fn answered_run_settles_placed_inputs_done_under_every_policy() {
    let inputs = run_inputs();
    let state = settlement_inbox();

    for policy in policies() {
        let changes =
            settle_run_inputs(&inputs, &state, RunOutcome::Answered(entry_id(900)), policy);
        assert_eq!(ids(&changes), vec![23, 21, 22], "input order is preserved");
        assert!(changes.iter().all(|(_, change)| {
            *change
                == SubmissionChange::Done {
                    answer: entry_id(900),
                }
        }));
    }
}

#[test]
fn failed_run_settles_inputs_and_applies_each_after_failed_run_policy() {
    let inputs = run_inputs();
    let state = settlement_inbox();

    for (reason, detail) in [
        ("model_error", Some(json!({"message": "boom"}))),
        ("no_model", None),
    ] {
        // keep: only the placed inputs settle; the queue stays untouched.
        let keep = settle_run_inputs(
            &inputs,
            &state,
            RunOutcome::Failed(reason.to_owned(), detail.clone()),
            AfterFailedRun::Keep,
        );
        assert_eq!(
            unanswered(&keep),
            unanswered_for(&[23, 21, 22], reason, detail.clone())
        );

        // continue: the caller's final boundary places the queue in the same
        // commit, so settlement itself adds nothing.
        let continue_policy = settle_run_inputs(
            &inputs,
            &state,
            RunOutcome::Failed(reason.to_owned(), detail.clone()),
            AfterFailedRun::Continue,
        );
        assert_eq!(continue_policy, keep);

        // withdraw: queued steer/follow-ups settle `run_failed`; writes stay
        // for a later boundary.
        let withdraw = settle_run_inputs(
            &inputs,
            &state,
            RunOutcome::Failed(reason.to_owned(), detail.clone()),
            AfterFailedRun::Withdraw,
        );
        assert_eq!(ids(&withdraw), vec![23, 21, 22, 31, 33, 35]);
        let mut expected = unanswered_for(&[23, 21, 22], reason, detail.clone());
        expected.extend(unanswered_for(&[31, 33, 35], REASON_RUN_FAILED, None));
        assert_eq!(unanswered(&withdraw), expected);
    }
}

#[test]
fn aborted_faulted_and_orphaned_runs_ignore_after_failed_run() {
    let inputs = run_inputs();
    let state = settlement_inbox();

    for (outcome, reason, detail) in [
        (RunOutcome::Aborted, REASON_ABORTED, None),
        (
            RunOutcome::Faulted(json!({"phase": "run"})),
            REASON_FAULTED,
            Some(json!({"phase": "run"})),
        ),
        // A null detail is still a detail on a fault.
        (
            RunOutcome::Faulted(Value::Null),
            REASON_FAULTED,
            Some(Value::Null),
        ),
        (
            RunOutcome::Orphaned("scheduler_restart".to_owned()),
            "scheduler_restart",
            None,
        ),
    ] {
        assert!(!outcome.is_failure(), "only Failed is a model failure");
        for policy in policies() {
            let changes = settle_run_inputs(&inputs, &state, outcome.clone(), policy);
            assert_eq!(
                unanswered(&changes),
                unanswered_for(&[23, 21, 22], reason, detail.clone()),
                "{outcome:?} under {policy:?} leaves the queue alone"
            );
        }
    }
}

#[test]
fn settlement_without_placed_inputs_only_withdraws_queued_users_on_failure() {
    let writes_only = inbox(vec![
        resetless_write(50),
        queued_write(
            51,
            entry(&[("kind", json!("app.note")), ("head", json!(3))]),
        ),
    ]);

    // Nothing placed and no withdrawable item: every outcome is a no-op.
    for policy in policies() {
        for outcome in [
            RunOutcome::Answered(entry_id(1)),
            RunOutcome::Failed("model_error".to_owned(), None),
            RunOutcome::Aborted,
            RunOutcome::Faulted(json!("x")),
            RunOutcome::Orphaned("gone".to_owned()),
        ] {
            assert!(
                settle_run_inputs(&[], &writes_only, outcome.clone(), policy).is_empty(),
                "{outcome:?} under {policy:?} has nothing to settle"
            );
        }
    }

    // With queued users, keep/continue still settle nothing; withdraw settles
    // exactly the users.
    let mixed = settlement_inbox();
    for policy in [AfterFailedRun::Keep, AfterFailedRun::Continue] {
        assert!(settle_run_inputs(
            &[],
            &mixed,
            RunOutcome::Failed("no_model".to_owned(), None),
            policy
        )
        .is_empty());
    }
    let withdraw = settle_run_inputs(
        &[],
        &mixed,
        RunOutcome::Failed("no_model".to_owned(), None),
        AfterFailedRun::Withdraw,
    );
    assert_eq!(ids(&withdraw), vec![31, 33, 35]);
    assert_eq!(
        unanswered(&withdraw),
        unanswered_for(&[31, 33, 35], REASON_RUN_FAILED, None)
    );
}

#[test]
fn settlement_is_pure_and_preserves_queue_order() {
    let inputs = run_inputs();
    let state = settlement_inbox();
    let before = state.clone();
    let outcome = RunOutcome::Failed("model_error".to_owned(), Some(json!("x")));

    let first = settle_run_inputs(&inputs, &state, outcome.clone(), AfterFailedRun::Withdraw);
    let second = settle_run_inputs(&inputs, &state, outcome, AfterFailedRun::Withdraw);
    assert_eq!(first, second, "settlement is deterministic");
    assert_eq!(state, before, "settlement only reads the inbox");
    assert_eq!(
        ids(&first),
        vec![23, 21, 22, 31, 33, 35],
        "placed inputs keep their order; queue users follow in inbox order"
    );
}

// ── requestId scope through storage and the facade ─────────────────────────

#[tokio::test(flavor = "current_thread")]
async fn request_id_scope_is_enforced_by_storage_and_the_session() {
    let memory = Arc::new(MemoryStorage::new());
    let session = Session::open(memory.clone(), SessionOptions::default());
    let ctx = Context::background();

    let a = session
        .create_conversation(ConversationOwnership::Ownerless, None, ctx.clone())
        .await
        .expect("conversation A");
    let b = session
        .create_conversation(ConversationOwnership::Ownerless, None, ctx.clone())
        .await
        .expect("conversation B");

    let mut create = SubmissionCreate::new(a.id, SubmissionType::Input);
    create.request_id = Some("shared".to_owned());
    let a_shared = session
        .create_submission(create, ctx.clone())
        .await
        .expect("A's shared input");

    let mut create = SubmissionCreate::new(b.id, SubmissionType::Input);
    create.request_id = Some("shared".to_owned());
    create.status = SubmissionStatus::Placed;
    let b_shared = session
        .create_submission(create, ctx.clone())
        .await
        .expect("B's shared input");

    let mut create = SubmissionCreate::new(a.id, SubmissionType::Write);
    create.request_id = Some("only-a".to_owned());
    create.status = SubmissionStatus::Done;
    let a_write = session
        .create_submission(create, ctx.clone())
        .await
        .expect("A's write");

    // The storage seam resolves the key inside its own conversation only.
    let a_lookup = memory
        .submission_by_request(a.id, "shared")
        .await
        .expect("read A's key")
        .expect("A has the key");
    assert_eq!(a_lookup.id, a_shared.id);
    let b_lookup = memory
        .submission_by_request(b.id, "shared")
        .await
        .expect("read B's key")
        .expect("B has the key");
    assert_eq!(b_lookup.id, b_shared.id);

    assert!(memory
        .submission_by_request(a.id, "only-b")
        .await
        .expect("read an unused key")
        .is_none());
    assert!(memory
        .submission_by_request(conversation(999), "shared")
        .await
        .expect("read an unused conversation")
        .is_none());

    // The facade's scan feeds the pure lookup, which applies the same scope.
    let a_page = session
        .scan_submissions(
            SubmissionQuery {
                conversation_id: Some(a.id),
                status: None,
                order: None,
            },
            10,
            None,
            &ctx,
        )
        .await
        .expect("scan A");
    assert_eq!(a_page.items.len(), 2);
    assert_eq!(
        find_request_submission(a_page.items.iter(), a.id, "shared").map(|record| record.id),
        Some(a_shared.id)
    );
    assert!(
        find_request_submission(a_page.items.iter(), b.id, "shared").is_none(),
        "B's key must not be visible among A's records"
    );

    let b_page = session
        .scan_submissions(
            SubmissionQuery {
                conversation_id: Some(b.id),
                status: None,
                order: None,
            },
            10,
            None,
            &ctx,
        )
        .await
        .expect("scan B");
    assert_eq!(
        find_request_submission(b_page.items.iter(), b.id, "shared").map(|record| record.id),
        Some(b_shared.id)
    );

    // Resolution on the records the storage returned: reuse, conflict, fresh.
    assert_eq!(
        resolve_request_id(SubmissionType::Input, Some("shared"), Some(&a_lookup))
            .expect("same type reuses"),
        RequestIdResolution::Existing {
            id: a_shared.id,
            status: AdmissionStatus::Queued,
        }
    );
    assert_eq!(
        resolve_request_id(SubmissionType::Input, Some("shared"), Some(&b_lookup))
            .expect("same type reuses"),
        RequestIdResolution::Existing {
            id: b_shared.id,
            status: AdmissionStatus::Placed,
        }
    );
    let conflict = resolve_request_id(SubmissionType::Input, Some("only-a"), Some(&a_write))
        .expect_err("A's write key cannot be reused for an input");
    assert_eq!(conflict.existing_type, SubmissionType::Write);

    let missing = memory
        .submission_by_request(a.id, "never-used")
        .await
        .expect("read a missing key");
    assert!(missing.is_none());
    assert_eq!(
        resolve_request_id(SubmissionType::Input, Some("never-used"), missing.as_ref())
            .expect("a missing key is fresh"),
        RequestIdResolution::Fresh
    );

    session.close().await.expect("close session");
}
