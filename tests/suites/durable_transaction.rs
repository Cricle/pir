//! `session::transaction` — the staging surface a commit callback mutates.
//!
//! Port of upstream `session/transaction.ts`'s `Tx`. The transaction is the
//! only thing a callback may touch: it stages writes, tasks, submissions and
//! document drafts, and the line seals it before assembly. These tests pin the
//! rules that make that safe rather than the assembly that consumes it (see
//! `durable_assemble.rs` for those).
//!
//! The two rules that matter most:
//!
//! * **Table reads before the first table write.** A callback that writes and
//!   then reads would see a state the commit does not have, so the read is
//!   refused with [`ReadAfterWrite`](pir::durable::ReadAfterWrite).
//! * **Sealing is final.** After `seal`, nothing new can be staged, which is
//!   what lets the line treat the batch as fixed.

use serde_json::json;

use pir::durable::session::transaction::EntryDraft;
use pir::durable::session::transaction::{
    HeadTarget, StagedDocument, TaskOptions, TaskOwnership, Transaction, TransactionScope,
};
use pir::durable::{
    address_id, decode_id, Context, ConversationRecord, DocumentAddress, DocumentFork,
    DocumentScope, StorageWrite, SubmissionRecord, SubmissionStatus, SubmissionType, TaskRecord,
    TaskState,
};

fn tx() -> Transaction {
    Transaction::new(Context::background(), TransactionScope::default())
}

fn conversation(id: i64) -> ConversationRecord {
    ConversationRecord {
        id: decode_id(id),
        parent: None,
        owner: None,
    }
}

fn address(kind: &str, conversation: i64) -> DocumentAddress {
    DocumentAddress {
        kind: kind.to_owned(),
        scope: DocumentScope::Conversation {
            conversation_id: decode_id(conversation),
        },
        key: None,
    }
}

fn staged(address: DocumentAddress) -> StagedDocument {
    let address_id = address_id(&address);
    StagedDocument {
        address,
        address_id,
        definition: None,
        family: None,
        value: serde_json::Map::new(),
        created: true,
        loaded: None,
        retire: false,
    }
}

// ── the read-before-write rule ──────────────────────────────────────────────

#[test]
fn a_table_read_before_any_table_write_is_allowed() {
    let tx = tx();
    assert!(!tx.has_table_write());
    assert!(
        tx.check_table_read("conversation").is_ok(),
        "a read before any write is the normal case"
    );
}

#[test]
fn a_table_read_after_a_table_write_is_refused() {
    let mut tx = tx();
    tx.stage_conversation(conversation(1), None);
    assert!(tx.has_table_write());

    let error = tx
        .check_table_read("conversation")
        .expect_err("a read after a write cannot be answered consistently");
    assert!(
        error.to_string().contains("ReadAfterWrite")
            || error.to_string().to_lowercase().contains("after"),
        "the error names the rule: {error}"
    );
}

#[test]
fn a_document_stage_does_not_count_as_a_table_write() {
    // Documents are drafts, not table rows: reading a table after staging one
    // is still consistent.
    let mut tx = tx();
    tx.stage_document(staged(address("counter", 1)));
    assert!(
        !tx.has_table_write(),
        "a document draft is not a table write"
    );
    assert!(tx.check_table_read("task").is_ok());
}

// ── sealing ────────────────────────────────────────────────────────────────

#[test]
fn sealing_is_recorded_and_final() {
    let mut tx = tx();
    assert!(!tx.is_sealed());
    tx.seal();
    assert!(tx.is_sealed());
}

// ── writes ─────────────────────────────────────────────────────────────────

#[test]
fn take_writes_drains_the_batch_exactly_once() {
    let mut tx = tx();
    tx.stage_conversation(conversation(1), None);
    tx.stage_conversation(conversation(2), None);
    assert_eq!(tx.writes().len(), 2);

    let drained = tx.take_writes();
    assert_eq!(drained.len(), 2, "both writes come out");
    assert!(
        tx.writes().is_empty(),
        "the batch is empty after draining; a second take yields nothing"
    );
    assert!(tx.take_writes().is_empty());
}

#[test]
fn a_created_conversation_is_registered_for_later_checks() {
    let mut tx = tx();
    tx.stage_conversation(conversation(7), None);
    assert!(tx.creates_conversation(decode_id(7)));
    assert!(!tx.creates_conversation(decode_id(8)));
    assert_eq!(tx.created_conversations(), &[decode_id(7)]);
}

#[test]
fn a_fork_parent_is_registered_as_a_source() {
    let mut tx = tx();
    tx.stage_conversation(conversation(9), Some(decode_id(4)));
    assert_eq!(tx.fork_source_conversations(), &[decode_id(4)]);
    assert!(
        tx.created_conversations().contains(&decode_id(9)),
        "the child is still a creation"
    );
}

#[test]
fn an_empty_transaction_reports_empty() {
    assert!(tx().is_empty());
    let mut tx = tx();
    tx.stage_document(staged(address("counter", 1)));
    assert!(
        !tx.is_empty(),
        "a staged document makes the transaction non-empty"
    );
}

// ── documents ──────────────────────────────────────────────────────────────

#[test]
fn latest_document_is_read_your_writes() {
    let mut tx = tx();
    let address = address("counter", 1);
    let id = address_id(&address);

    let first = staged(address.clone());
    tx.stage_document(first);
    assert!(tx.latest_document(&id).is_some());

    // A second stage at the same address becomes the latest, in staging order.
    let mut second = staged(address.clone());
    second.value.insert("count".to_owned(), json!(2));
    tx.stage_document(second);
    let latest = tx.latest_document(&id).expect("the latest draft");
    assert_eq!(latest.value.get("count"), Some(&json!(2)));
    assert_eq!(tx.documents().len(), 2, "both drafts are kept in order");
}

#[test]
fn retiring_a_draft_marks_the_latest_one() {
    let mut tx = tx();
    let address = address("counter", 1);
    let id = address_id(&address);
    tx.stage_document(staged(address));
    tx.retire_document(&id);
    assert!(
        tx.latest_document(&id)
            .is_some_and(|document| document.retire),
        "the draft is marked for retirement"
    );
}

#[test]
fn stage_retirement_records_a_retire_only_plan() {
    // A retirement discovered without a draft carries only its committed
    // record; assembly plans it so adoption can publish the terminal frame.
    let mut tx = tx();
    let address = address("counter", 1);
    let record = pir::durable::DocumentRecord {
        id: decode_id::<pir::durable::DocumentIdKind>(11),
        kind: address.kind.clone(),
        key: None,
        created_at: pir::durable::Seq::from_raw(1),
        retired_at: None,
        scope: address.scope,
        history: None,
        fork: Some(DocumentFork::Initial),
    };
    assert!(!tx.retires_record(record.id));

    tx.stage_retirement(record.clone());
    assert_eq!(tx.staged_retirements().len(), 1);
    assert!(
        tx.retires_record(record.id),
        "a staged retire-only record is a retirement of that incarnation"
    );
    assert!(
        tx.documents().is_empty(),
        "a retire-only plan stages no draft"
    );
}

#[test]
fn retires_record_sees_a_draft_retirement_too() {
    let mut tx = tx();
    let address = address("counter", 1);
    let id = address_id(&address);
    let document_id = decode_id::<pir::durable::DocumentIdKind>(12);

    let mut document = staged(address);
    document.created = false;
    document.loaded = Some(pir::durable::session::documents::LoadedDocument {
        address_id: id.clone(),
        record: pir::durable::DocumentRecord {
            id: document_id,
            kind: "counter".to_owned(),
            key: None,
            created_at: pir::durable::Seq::from_raw(1),
            retired_at: None,
            scope: DocumentScope::Conversation {
                conversation_id: decode_id(1),
            },
            history: None,
            fork: Some(DocumentFork::Initial),
        },
        stored_version: 1,
        value_version: 1,
        deltas_since_base: 0,
        value: serde_json::Map::new(),
    });
    tx.stage_document(document);
    tx.retire_document(&id);
    assert!(
        tx.retires_record(document_id),
        "the loaded incarnation is the one retired"
    );
}

// ── tasks and submissions ──────────────────────────────────────────────────

fn task(id: i64, conversation: i64, state: TaskState) -> TaskRecord {
    TaskRecord {
        trace: None,
        links: Vec::new(),
        id: decode_id(id),
        conversation_id: decode_id(conversation),
        kind: "pi.test".into(),
        version: 1,
        input: serde_json::Value::Null,
        owner: None,
        background: false,
        abort_requested: false,
        abort_reason: None,
        abandon_on_restart: None,
        started_at: None,
        ended_at: None,
        restarts: None,
        state,
        memos: None,
    }
}

#[test]
fn a_staged_task_keeps_its_creation_flag() {
    let mut tx = tx();
    tx.stage_task(
        task(
            3,
            1,
            TaskState::Pending {
                checkpoint: json!({ "phase": "start" }),
            },
        ),
        true,
    );
    assert!(tx.task_is_created(decode_id(3)));
    assert!(
        tx.staged_task(decode_id(3)).is_some(),
        "the staged record is readable"
    );
    assert_eq!(tx.staged_tasks().len(), 1);
}

#[test]
fn a_read_task_is_no_longer_marked_as_created() {
    let mut tx = tx();
    let record = task(
        4,
        1,
        TaskState::Pending {
            checkpoint: json!({ "phase": "start" }),
        },
    );
    tx.stage_task(record.clone(), false);
    tx.cache_task_read(record);
    assert!(
        !tx.task_is_created(decode_id(4)),
        "a task staged as a replacement is not a creation"
    );
}

#[test]
fn staged_submissions_are_kept_and_changes_are_ordered() {
    let mut tx = tx();
    let record = SubmissionRecord {
        trace: None,
        id: decode_id(5),
        conversation_id: decode_id(1),
        submission_type: SubmissionType::Input,
        request_id: Some("r1".to_owned()),
        status: SubmissionStatus::Queued,
        entry: None,
        answer: None,
        reason: None,
        detail: None,
    };
    tx.stage_submission(record);
    tx.stage_submission_change(
        decode_id(5),
        pir::durable::session::transaction::SubmissionChange::Done {
            answer: decode_id(6),
        },
    );
    assert_eq!(tx.staged_submissions().count(), 1);
    assert_eq!(tx.submission_changes().len(), 1);
    assert_eq!(tx.submission_changes()[0].0, decode_id(5));
}

// ── scope defaults and the draft shape ────────────────────────────────────

#[test]
fn the_scope_is_available_to_the_callback() {
    let scope = TransactionScope {
        conversation_id: Some(decode_id(2)),
        task_id: Some(decode_id(3)),
    };
    let tx = Transaction::new(Context::background(), scope);
    assert_eq!(tx.scope().conversation_id, Some(decode_id(2)));
    assert_eq!(tx.scope().task_id, Some(decode_id(3)));
}

#[test]
fn the_context_is_available_to_the_callback() {
    let context = Context::background().with_name("commit-1");
    let tx = Transaction::new(context, TransactionScope::default());
    assert_eq!(
        tx.context().span(),
        None,
        "the transaction keeps the caller's context (which carries no span here)"
    );
}

#[test]
fn a_default_entry_draft_has_no_model_or_head() {
    let draft = EntryDraft::default();
    assert!(draft.model.is_none());
    assert!(draft.head.is_none());
    assert!(draft.edits.is_none());
    assert!(draft.kind.is_empty());
}

#[test]
fn a_head_target_is_copy_and_comparable() {
    let target = HeadTarget::SelfEntry;
    let copy = target;
    assert_eq!(target, copy);
    assert_ne!(HeadTarget::SelfEntry, HeadTarget::Entry(decode_id(1)));
}

#[test]
fn task_options_default_to_no_conversation_and_conversation_ownership() {
    let options = TaskOptions::default();
    assert!(options.conversation_id.is_none());
    assert!(matches!(options.ownership, TaskOwnership::Conversation));
}

#[test]
fn the_document_scope_of_a_resolved_address_is_its_address_scope() {
    let resolved = pir::durable::ResolvedAddress::new(
        "counter",
        pir::durable::DocumentSemantics::ConversationLatest {
            fork: DocumentFork::Initial,
        },
        Some(pir::durable::DocOwner::Conversation(decode_id(1))),
        None,
    )
    .expect("resolved address");
    assert_eq!(
        Transaction::address_scope(&resolved),
        DocumentScope::Conversation {
            conversation_id: decode_id(1)
        }
    );
}

#[test]
fn a_conversation_write_is_the_batch_entry_it_stages() {
    let mut tx = tx();
    tx.stage_conversation(conversation(1), None);
    let writes = tx.take_writes();
    assert_eq!(writes.len(), 1);
    match &writes[0] {
        StorageWrite::Conversation { value } => {
            assert_eq!(value.id, decode_id::<pir::durable::ConversationIdKind>(1));
        }
        other => panic!("expected a conversation write, got {other:?}"),
    }
}
