//! `durable::session::assemble` — plan/assemble/adopt conformance.
//!
//! Port of upstream `transaction.ts`'s `#assemble` (`:784-876`), `planDocument`
//! (`:1016-1063`), `publishes`, `#validateOwners` and `applySubmissionChange`.
//! These tests drive the transaction layer directly against `MemoryStorage`, so
//! a rule that regresses fails here rather than only in a harness test.

use std::sync::Arc;

use pir::durable::ids::Seq;
use pir::durable::session::assemble::{adopt, assemble, plan_document};
use pir::durable::session::documents::LoadedDocument;
use pir::durable::session::guard::GuardedStorage;
use pir::durable::session::line::SessionState;
use pir::durable::session::transaction::{StagedDocument, SubmissionChange, Transaction};
use pir::durable::session::FailureLatchCore;
use pir::durable::{
    apply, decode_id, DocumentAddress, DocumentContent, DocumentCreate, DocumentFork,
    DocumentHistory, DocumentRecord, DocumentScope, MemoryStorage, StorageWrite, TaskOutcome,
    TaskRecord, TaskState,
};
use serde_json::{json, Map, Value};

// ── fixtures ────────────────────────────────────────────────────────────────

fn task(
    id: i64,
    conversation: i64,
    state: TaskState,
    owner: Option<i64>,
    abort_requested: bool,
) -> TaskRecord {
    TaskRecord {
        trace: None,
        links: Vec::new(),
        id: decode_id(id),
        conversation_id: decode_id(conversation),
        kind: "pi.test".into(),
        version: 1,
        input: Value::Null,
        owner: owner.map(decode_id),
        background: false,
        abort_requested,
        abort_reason: None,
        abandon_on_restart: None,
        started_at: None,
        ended_at: None,
        restarts: None,
        state,
        memos: None,
    }
}

fn pending() -> TaskState {
    TaskState::Pending {
        checkpoint: json!({ "phase": "start" }),
    }
}

fn terminal() -> TaskState {
    TaskState::Terminal {
        outcome: TaskOutcome::Completed {
            result: Value::Null,
        },
    }
}

fn conversation_address(conversation: i64, kind: &str) -> DocumentAddress {
    DocumentAddress {
        kind: kind.into(),
        scope: DocumentScope::Conversation {
            conversation_id: decode_id(conversation),
        },
        key: None,
    }
}

fn task_address(task: i64, kind: &str) -> DocumentAddress {
    DocumentAddress {
        kind: kind.into(),
        scope: DocumentScope::Task {
            task_id: decode_id(task),
        },
        key: None,
    }
}

fn value_of(pairs: &[(&str, Value)]) -> Map<String, Value> {
    pairs
        .iter()
        .map(|(key, value)| ((*key).to_owned(), value.clone()))
        .collect()
}

/// A staged conversation document that already exists in storage.
fn latest_definition(address: &DocumentAddress) -> pir::durable::DocDefinition {
    pir::durable::DocDefinition::conversation_latest(address.kind.clone(), 1, DocumentFork::Initial)
        .expect("definition")
}

fn staged_loaded(
    address: DocumentAddress,
    loaded_value: Map<String, Value>,
    stored_version: i64,
    draft: Map<String, Value>,
    key: Option<String>,
) -> StagedDocument {
    let address_id = pir::durable::address_id(&address);
    StagedDocument {
        address: address.clone(),
        address_id: address_id.clone(),
        definition: Some(latest_definition(&address)),
        family: None,
        value: draft,
        created: false,
        loaded: Some(LoadedDocument {
            address_id,
            record: DocumentRecord {
                id: decode_id(1_i64),
                kind: address.kind.clone(),
                key: key.clone(),
                created_at: Seq::from_raw(1),
                retired_at: None,
                scope: address.scope,
                history: Some(DocumentHistory::Latest),
                fork: Some(DocumentFork::Initial),
            },
            stored_version,
            value_version: stored_version,
            deltas_since_base: 0,
            value: loaded_value,
        }),
        retire: false,
    }
}

/// A staged document that does not exist yet.
fn staged_created(address: DocumentAddress, draft: Map<String, Value>) -> StagedDocument {
    StagedDocument {
        address_id: pir::durable::address_id(&address),
        definition: Some(latest_definition(&address)),
        address,
        family: None,
        value: draft,
        created: true,
        loaded: None,
        retire: false,
    }
}

fn session_state<'a>(
    storage: &'a GuardedStorage,
    now: &'a (dyn Fn() -> i64 + Send + Sync),
) -> SessionState<'a> {
    SessionState { storage, now }
}

fn guarded() -> (Arc<MemoryStorage>, GuardedStorage) {
    let memory = Arc::new(MemoryStorage::new());
    let guard = GuardedStorage::new(memory.clone(), Arc::new(FailureLatchCore::new()));
    (memory, guard)
}

// ── plan_document ───────────────────────────────────────────────────────────

#[test]
fn a_creation_plans_a_base_and_mints_its_id_during_assembly() {
    let address = conversation_address(7, "pi.live");
    let staged = staged_created(address.clone(), value_of(&[("run", json!(null))]));
    let plan = plan_document(&staged).expect("plan");
    assert!(plan.is_creation(), "a new incarnation plans a create");
    assert!(
        matches!(plan.content, Some(StorageWrite::DocumentCreate { .. })),
        "a creation plans a base"
    );
    assert!(!plan.retire);
    assert_eq!(plan.address_id, pir::durable::address_id(&address));
}

#[test]
fn a_loaded_change_plans_the_operations_between_the_two_revisions() {
    let address = conversation_address(7, "pi.live");
    let before = value_of(&[("output", json!("abc"))]);
    let after = value_of(&[("output", json!("abcdef"))]);
    let staged = staged_loaded(address, before, 1, after, None);
    let plan = plan_document(&staged).expect("plan");
    assert!(!plan.is_creation());
    assert_eq!(plan.ops.len(), 1, "one append: {:?}", plan.ops);
}

#[test]
fn a_version_change_forces_a_base_even_with_no_operations() {
    let address = conversation_address(7, "pi.live");
    let value = value_of(&[("x", json!(1))]);
    // Stored at version 1, accessed as version 2, value unchanged.
    let mut staged = staged_loaded(address.clone(), value.clone(), 1, value, None);
    staged.definition = Some(
        pir::durable::DocDefinition::conversation_latest(
            address.kind.clone(),
            2,
            DocumentFork::Initial,
        )
        .expect("v2 definition"),
    );
    let plan = plan_document(&staged).expect("plan");
    assert_eq!(plan.version, 2, "the plan carries the accessible version");
    assert!(
        matches!(
            plan.content,
            Some(StorageWrite::DocumentChange {
                content: DocumentContent::Base { .. },
                ..
            })
        ),
        "a version transition stores a base even when the value is unchanged"
    );
}

// ── value ownership (one revision per plan) ────────────────────────────────

#[test]
fn an_unchanged_same_version_draft_plans_no_content_and_no_value() {
    let address = conversation_address(7, "pi.live");
    let value = value_of(&[("output", json!("same"))]);
    let staged = staged_loaded(address, value.clone(), 1, value, None);
    let plan = plan_document(&staged).expect("plan");
    assert!(
        plan.content.is_none(),
        "an unchanged same-version draft writes nothing"
    );
    assert!(
        plan.new_value.is_none(),
        "and it keeps no second copy of the value"
    );
    assert!(
        plan.publication_value().is_none(),
        "nor does it publish a revision"
    );
    assert!(!plan.publishes(), "nothing to publish");
    assert!(
        adopt(std::slice::from_ref(&plan), Seq::from_raw(2)).is_empty(),
        "adoption emits no publication"
    );
}

#[test]
fn a_base_keeps_its_revision_only_in_the_content_write() {
    let address = conversation_address(7, "pi.live");
    let staged = staged_loaded(
        address,
        value_of(&[("output", json!("abc"))]),
        1,
        value_of(&[("output", json!("abcdef"))]),
        None,
    );
    // `staged_loaded` writes the draft at version 1; a version-2 definition
    // turns the same draft into a base.
    let mut staged = staged;
    staged.definition = Some(
        pir::durable::DocDefinition::conversation_latest(
            staged.address.kind.clone(),
            2,
            DocumentFork::Initial,
        )
        .expect("v2 definition"),
    );
    let plan = plan_document(&staged).expect("plan");
    assert!(
        matches!(
            plan.content,
            Some(StorageWrite::DocumentChange {
                content: DocumentContent::Base { .. },
                ..
            })
        ),
        "a version change stores a base"
    );
    assert!(
        plan.new_value.is_none(),
        "a base's prepared revision lives in the write, not in a second copy"
    );
    // The publication still carries the exact revision the base writes.
    let changes = adopt(std::slice::from_ref(&plan), Seq::from_raw(3));
    match &changes[0] {
        pir::durable::DocumentCommitChange::Document { value, .. } => {
            assert_eq!(
                value.as_ref().map(|value| value["output"].clone()),
                Some(json!("abcdef")),
                "adoption publishes the base's value"
            );
        }
        other => panic!("expected a document change, got {other:?}"),
    }
}

#[test]
fn a_delta_publishes_the_revision_the_plan_kept() {
    let address = conversation_address(7, "pi.live");
    let staged = staged_loaded(
        address,
        value_of(&[("output", json!("abc"))]),
        1,
        value_of(&[("output", json!("abcdef"))]),
        None,
    );
    let plan = plan_document(&staged).expect("plan");
    assert!(
        plan.new_value.is_some(),
        "a delta keeps its prepared revision on the plan"
    );
    assert_eq!(plan.publication_value(), plan.new_value.as_ref());
    let changes = adopt(std::slice::from_ref(&plan), Seq::from_raw(4));
    match &changes[0] {
        pir::durable::DocumentCommitChange::Document { value, ops, .. } => {
            assert_eq!(
                value.as_ref().map(|value| value["output"].clone()),
                Some(json!("abcdef")),
            );
            assert!(!ops.is_empty(), "the delta's operations are published");
        }
        other => panic!("expected a document change, got {other:?}"),
    }
}

// ── adoption ────────────────────────────────────────────────────────────────

#[test]
fn adopt_stamps_creation_time_and_emits_an_empty_op_document_change() {
    let address = conversation_address(7, "pi.live");
    let staged = staged_created(address.clone(), value_of(&[("v", json!(1))]));
    let mut plan = plan_document(&staged).expect("plan");
    plan.stamp_created_id(decode_id(42));
    let seq = Seq::from_raw(9);
    let changes = adopt(std::slice::from_ref(&plan), seq);
    assert_eq!(changes.len(), 1);
    match &changes[0] {
        pir::durable::DocumentCommitChange::Document {
            record,
            conversation_id,
            version,
            value,
            ops,
        } => {
            assert_eq!(record.created_at, seq);
            assert!(record.retired_at.is_none());
            assert_eq!(*conversation_id, Some(decode_id(7)));
            assert_eq!(*version, Some(1));
            assert!(value.is_some());
            assert!(ops.is_empty(), "a creation publishes no operations");
        }
        other => panic!("expected a document change, got {other:?}"),
    }
}

#[test]
fn adopt_publishes_the_exact_operations_for_a_loaded_change() {
    let address = conversation_address(7, "pi.live");
    let staged = staged_loaded(
        address,
        value_of(&[("output", json!("abc"))]),
        1,
        value_of(&[("output", json!("abcdef"))]),
        None,
    );
    let plan = plan_document(&staged).expect("plan");
    let changes = adopt(std::slice::from_ref(&plan), Seq::from_raw(3));
    match &changes[0] {
        pir::durable::DocumentCommitChange::Document { ops, .. } => {
            assert_eq!(ops.len(), 1);
            // The published value and ops agree: applying them to the old
            // revision reproduces the new one.
            let mut replayed = Value::Object(value_of(&[("output", json!("abc"))]));
            apply(&mut replayed, ops).expect("ops apply");
            assert_eq!(replayed["output"], json!("abcdef"));
        }
        other => panic!("expected a document change, got {other:?}"),
    }
}

#[test]
fn adopt_stamps_retirement_and_publishes_null_with_no_operations() {
    let address = task_address(5, "pi.tool.nested-call");
    let mut staged = staged_loaded(
        address,
        value_of(&[("k", json!(1))]),
        1,
        value_of(&[("k", json!(1))]),
        Some("1".into()),
    );
    staged.retire = true;
    let plan = plan_document(&staged).expect("plan");
    let changes = adopt(std::slice::from_ref(&plan), Seq::from_raw(4));
    match &changes[0] {
        pir::durable::DocumentCommitChange::Document {
            record,
            value,
            version,
            ops,
            ..
        } => {
            assert_eq!(record.retired_at, Some(Seq::from_raw(4)));
            assert!(value.is_none(), "retirement publishes null");
            assert!(version.is_none());
            assert!(ops.is_empty());
        }
        other => panic!("expected a document change, got {other:?}"),
    }
}

// ── assembly validation ─────────────────────────────────────────────────────

#[tokio::test]
async fn a_fork_source_document_cannot_be_changed_in_the_fork_transaction() {
    let (_memory, guard) = guarded();
    let now = || 1_i64;
    let state = session_state(&guard, &now);
    let mut tx = Transaction::new(pir::durable::Context::background(), Default::default());
    let address = conversation_address(9, "pi.live");
    tx.stage_fork_source_document(decode_id(1));
    tx.stage_document(staged_loaded(
        address.clone(),
        value_of(&[("x", json!(1))]),
        1,
        value_of(&[("x", json!(2))]),
        None,
    ));
    // The staged loaded document's record id must equal the registered source.
    let error = assemble(&mut tx, &state, &guard)
        .await
        .expect_err("a fork source change must be rejected");
    let text = error.to_string();
    assert!(
        text.contains("fork source") || text.contains("Fork source"),
        "unexpected error: {text}"
    );
}

#[tokio::test]
async fn a_child_created_under_a_terminal_owner_is_rejected() {
    let (memory, guard) = guarded();
    let now = || 1_i64;
    let state = session_state(&guard, &now);
    seed_conversation(&memory, 1).await;
    seed_task(&memory, task(5, 1, terminal(), None, false)).await;
    let mut tx = Transaction::new(pir::durable::Context::background(), Default::default());
    tx.stage_task(task(6, 1, pending(), Some(5), false), true);
    let error = assemble(&mut tx, &state, &guard).await.unwrap_err();
    assert!(
        error.to_string().contains("terminal") || error.to_string().contains("completing"),
        "unexpected error: {error}"
    );
}

#[tokio::test]
async fn a_child_created_under_an_abort_marked_owner_is_rejected() {
    let (memory, guard) = guarded();
    let now = || 1_i64;
    let state = session_state(&guard, &now);
    seed_conversation(&memory, 1).await;
    seed_task(&memory, task(5, 1, pending(), None, true)).await;
    let mut tx = Transaction::new(pir::durable::Context::background(), Default::default());
    tx.stage_task(task(6, 1, pending(), Some(5), false), true);
    let error = assemble(&mut tx, &state, &guard).await.unwrap_err();
    assert!(
        error.to_string().contains("abort-marked"),
        "unexpected error: {error}"
    );
}

#[tokio::test]
async fn a_terminal_task_retires_every_document_of_its_scope() {
    let (memory, guard) = guarded();
    let now = || 1_i64;
    let state = session_state(&guard, &now);

    // A committed task document that must retire with its task.
    let address = task_address(8, "pi.tool.nested-call");
    let id = decode_id::<pir::durable::DocumentIdKind>(100);
    let create = DocumentCreate {
        id,
        kind: address.kind.clone(),
        key: Some("1".into()),
        scope: address.scope,
        history: None,
        fork: None,
    };
    let _ = <MemoryStorage as pir::durable::Storage>::commit(
        &memory,
        vec![StorageWrite::DocumentCreate {
            record: create,
            content: DocumentContent::Base {
                version: 1,
                value: value_of(&[("k", json!(1))]),
            },
        }],
    )
    .await
    .expect("seed document");

    seed_conversation(&memory, 1).await;
    seed_task(&memory, task(8, 1, pending(), None, false)).await;

    let mut tx = Transaction::new(pir::durable::Context::background(), Default::default());
    tx.stage_task(task(8, 1, terminal(), None, false), false);
    let (writes, plans) = assemble(&mut tx, &state, &guard).await.expect("assemble");
    let retires = writes
        .iter()
        .filter(|write| matches!(write, StorageWrite::DocumentRetire { .. }))
        .count();
    assert_eq!(retires, 1, "the task document retires with its task");
    assert!(plans.iter().any(|plan| plan.retire));
}

#[tokio::test]
async fn submission_settlement_is_resolved_against_the_latest_candidate() {
    let (_memory, guard) = guarded();
    let now = || 1_i64;
    let state = session_state(&guard, &now);
    let mut tx = Transaction::new(pir::durable::Context::background(), Default::default());
    tx.stage_submission_change(
        decode_id(3),
        SubmissionChange::Unanswered {
            reason: "aborted".into(),
            detail: None,
        },
    );
    // A settlement for a submission that does not exist is an error; the
    // fixture proves the resolution path runs during assembly.
    let error = assemble(&mut tx, &state, &guard)
        .await
        .expect_err("an unknown submission cannot settle");
    assert!(error.to_string().contains("does not exist"), "{error}");
}

#[tokio::test]
async fn a_transaction_with_no_documents_assembles_only_its_table_writes() {
    let (_memory, guard) = guarded();
    let now = || 1_i64;
    let state = session_state(&guard, &now);
    let mut tx = Transaction::new(pir::durable::Context::background(), Default::default());
    tx.stage_conversation(
        pir::durable::ConversationRecord {
            id: decode_id(1),
            parent: None,
            owner: None,
        },
        None,
    );
    let (writes, plans) = assemble(&mut tx, &state, &guard).await.expect("assemble");
    assert_eq!(writes.len(), 1);
    assert!(plans.is_empty());
}

// ── seeding ────────────────────────────────────────────────────────────────

async fn seed_conversation(memory: &Arc<MemoryStorage>, id: i64) {
    <MemoryStorage as pir::durable::Storage>::commit(
        &**memory,
        vec![StorageWrite::Conversation {
            value: pir::durable::ConversationRecord {
                id: decode_id(id),
                parent: None,
                owner: None,
            },
        }],
    )
    .await
    .expect("seed conversation");
}

async fn seed_task(memory: &Arc<MemoryStorage>, record: TaskRecord) {
    <MemoryStorage as pir::durable::Storage>::commit(
        &**memory,
        vec![StorageWrite::Task { value: record }],
    )
    .await
    .expect("seed task");
}
