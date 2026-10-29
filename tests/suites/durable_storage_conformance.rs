//! Backend-agnostic conformance suite for the durable [`Storage`] contract.
//!
//! Every assertion in [`run_conformance`] runs against both built-in backends
//! (`MemoryStorage` and `JsonlStorage`), so a behavior only one of them has
//! fails here. The two `#[tokio::test]` entry points below are the required
//! pair; the additional JSONL tests cover the parts that only exist for a
//! durable log: reopen recovery, the mint journal and torn/corrupt lines.
//!
//! Spec references: `docs/durable-spec.md` §10 (Storage trait), §10.1
//! (failures) and §11 (backends). Upstream behavioral reference:
//! `pi/packages/durable/src/testing/storage-conformance.ts`.

use pir::durable::{
    ConversationId, ConversationQuery, ConversationRecord, Cursor, DocumentAddress,
    DocumentContent, DocumentCopySource, DocumentCreate, DocumentFork, DocumentHistory, DocumentId,
    DocumentPoint, DocumentPointQuery, DocumentQuery, DocumentScope, EntryId, EntryQuery,
    EntryRecord, JsonObject, JsonlStorage, MemoryStorage, Op, ScanOrder, Seg, Seq, Storage,
    StorageError, StorageWrite, SubmissionId, SubmissionQuery, SubmissionRecord, SubmissionStatus,
    SubmissionType, TaskId, TaskQuery, TaskRecord, TaskState, TaskStatus,
};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

// ─── helpers ───────────────────────────────────────────────────────────────

/// A per-test directory under the OS temp dir, removed on drop.
///
/// Windows refuses to delete a directory while a handle is open, so every test
/// closes/drops its storage before this runs; the retry loop is the extra
/// safety net for handles the OS releases asynchronously.
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::SeqCst);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!(
            "pir-durable-storage-{label}-{}-{nanos}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("create temp directory");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        for _ in 0..20 {
            match std::fs::remove_dir_all(&self.path) {
                Ok(()) => return,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
                Err(_) => std::thread::sleep(Duration::from_millis(10)),
            }
        }
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn conversation(id: i64) -> ConversationRecord {
    ConversationRecord {
        id: ConversationId::from_raw(id),
        parent: None,
        owner: None,
    }
}

fn entry(id: i64, conversation_id: i64, kind: &str, head: Option<i64>) -> EntryRecord {
    EntryRecord {
        id: EntryId::from_raw(id),
        conversation_id: ConversationId::from_raw(conversation_id),
        kind: kind.to_owned(),
        model: Some(vec![json!({
            "role": "assistant",
            "content": [{"type": "text", "text": "hello"}]
        })]),
        data: Some(json!({"tag": kind})),
        head: head.map(EntryId::from_raw),
        edits: None,
        by_task_id: None,
    }
}

fn pending_task(id: i64, conversation_id: i64, phase: &str) -> TaskRecord {
    TaskRecord {
        trace: None,
        links: Vec::new(),
        id: TaskId::from_raw(id),
        conversation_id: ConversationId::from_raw(conversation_id),
        kind: "test.task".to_owned(),
        version: 1,
        input: json!({"task": id}),
        owner: None,
        background: false,
        abort_requested: false,
        abort_reason: None,
        abandon_on_restart: None,
        started_at: None,
        ended_at: None,
        restarts: None,
        state: TaskState::Pending {
            checkpoint: json!({"phase": phase}),
        },
        memos: None,
    }
}

fn submission(
    id: i64,
    conversation_id: i64,
    request_id: Option<&str>,
    status: SubmissionStatus,
) -> SubmissionRecord {
    SubmissionRecord {
        trace: None,
        id: SubmissionId::from_raw(id),
        conversation_id: ConversationId::from_raw(conversation_id),
        request_id: request_id.map(str::to_owned),
        submission_type: SubmissionType::Input,
        status,
        entry: None,
        answer: None,
        reason: None,
        detail: None,
    }
}

fn document_record(
    id: i64,
    kind: &str,
    scope: DocumentScope,
    history: Option<DocumentHistory>,
    fork: Option<DocumentFork>,
) -> DocumentCreate {
    document_record_with_key(id, kind, None, scope, history, fork)
}

fn document_record_with_key(
    id: i64,
    kind: &str,
    key: Option<&str>,
    scope: DocumentScope,
    history: Option<DocumentHistory>,
    fork: Option<DocumentFork>,
) -> DocumentCreate {
    DocumentCreate {
        id: DocumentId::from_raw(id),
        kind: kind.to_owned(),
        key: key.map(str::to_owned),
        scope,
        history,
        fork,
    }
}

fn conversation_scope(conversation_id: i64) -> DocumentScope {
    DocumentScope::Conversation {
        conversation_id: ConversationId::from_raw(conversation_id),
    }
}

fn object(pairs: impl IntoIterator<Item = (&'static str, Value)>) -> JsonObject {
    pairs
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value))
        .collect()
}

fn ids<T>(items: &[T], id: impl Fn(&T) -> i64) -> Vec<i64> {
    items.iter().map(id).collect()
}

fn entry_query(order: Option<ScanOrder>) -> EntryQuery {
    EntryQuery {
        conversation_id: ConversationId::from_raw(1),
        min_entry_id: None,
        max_entry_id: None,
        order,
    }
}

fn conversation_query(order: Option<ScanOrder>) -> ConversationQuery {
    ConversationQuery {
        owner_conversation_id: None,
        owner_task_id: None,
        order,
    }
}

fn task_query(order: Option<ScanOrder>) -> TaskQuery {
    TaskQuery {
        order,
        ..TaskQuery::default()
    }
}

fn submission_query(order: Option<ScanOrder>) -> SubmissionQuery {
    SubmissionQuery {
        order,
        ..SubmissionQuery::default()
    }
}

// ─── the backend-agnostic suite ────────────────────────────────────────────

/// Run every backend-independent conformance case against `storage`.
///
/// The suite leaves a few current documents and tasks alive on purpose; both
/// tests close the storage afterwards, and the JSONL test removes its root.
async fn run_conformance<S: Storage + ?Sized>(storage: &S) {
    // ── 1. one atomic batch across every record table, monotonic sequences ──
    let first_seq = storage
        .commit(vec![StorageWrite::Conversation {
            value: conversation(1),
        }])
        .await
        .expect("seed the root conversation");
    assert_eq!(
        first_seq,
        Seq::from_raw(1),
        "the first commit carries sequence 1"
    );

    let batch_seq = storage
        .commit(vec![
            StorageWrite::Entry {
                value: entry(10, 1, "pi.user", None),
            },
            StorageWrite::Entry {
                value: entry(11, 1, "marker", Some(10)),
            },
            StorageWrite::Entry {
                value: entry(12, 1, "pi.assistant", None),
            },
            StorageWrite::Task {
                value: pending_task(20, 1, "start"),
            },
            StorageWrite::Submission {
                value: submission(30, 1, Some("req-1"), SubmissionStatus::Queued),
            },
        ])
        .await
        .expect("one mixed atomic batch");
    assert!(
        batch_seq > first_seq,
        "commit sequences strictly increase: {batch_seq:?} > {first_seq:?}"
    );

    // Every member of the batch is visible through its own lookup.
    for id in [10, 11, 12] {
        assert!(
            storage
                .entry(EntryId::from_raw(id))
                .await
                .expect("entry lookup")
                .is_some(),
            "entry {id} from the atomic batch is missing"
        );
    }
    let stored_entry = storage
        .entry(EntryId::from_raw(10))
        .await
        .expect("entry lookup")
        .expect("entry 10 is present");
    assert_eq!(
        stored_entry.commit_seq, batch_seq,
        "entry lookup reports the committing sequence"
    );

    let stored_task = storage
        .task(TaskId::from_raw(20))
        .await
        .expect("task lookup")
        .expect("task 20 is present");
    assert_eq!(stored_task.status(), TaskStatus::Pending);
    assert_eq!(stored_task.input, json!({"task": 20}));

    let stored_submission = storage
        .submission(SubmissionId::from_raw(30))
        .await
        .expect("submission lookup")
        .expect("submission 30 is present");
    assert_eq!(stored_submission.status, SubmissionStatus::Queued);
    let by_request = storage
        .submission_by_request(ConversationId::from_raw(1), "req-1")
        .await
        .expect("submission_by_request")
        .expect("the request id is indexed");
    assert_eq!(by_request.id, SubmissionId::from_raw(30));

    let marker = storage
        .find_latest_head_marker(ConversationId::from_raw(1), None)
        .await
        .expect("head marker")
        .expect("entry 11 is a head marker");
    assert_eq!(marker.id, EntryId::from_raw(11));
    assert_eq!(marker.head, Some(EntryId::from_raw(10)));

    // A rejected batch is atomic too: no record appears and the sequence is
    // not consumed by the failure.
    let rejected = storage
        .commit(vec![
            StorageWrite::Task {
                value: pending_task(21, 1, "rejected"),
            },
            // The root conversation is immutable: this duplicate rejects the
            // whole batch *after* the task write was staged.
            StorageWrite::Conversation {
                value: conversation(1),
            },
        ])
        .await;
    assert!(
        matches!(rejected, Err(StorageError::Invalid(_))),
        "a duplicate conversation id must reject the batch, got {rejected:?}"
    );
    assert!(
        storage
            .task(TaskId::from_raw(21))
            .await
            .expect("task lookup")
            .is_none(),
        "a rejected batch must not be visible"
    );
    let after_reject = storage.commit(Vec::new()).await.expect("empty commit");
    assert_eq!(
        after_reject,
        batch_seq.next(),
        "a rejected batch does not consume the next sequence"
    );

    // ── 2. pagination: both orders, limits, cursor continuation, bad cursor ──
    storage
        .commit(vec![
            StorageWrite::Conversation {
                value: conversation(2),
            },
            StorageWrite::Conversation {
                value: conversation(3),
            },
            StorageWrite::Conversation {
                value: conversation(4),
            },
        ])
        .await
        .expect("seed conversations");

    // `entry_in` only sees entries reachable through the conversation chain.
    assert!(storage
        .entry_in(ConversationId::from_raw(1), EntryId::from_raw(10))
        .await
        .expect("entry_in")
        .is_some());
    assert!(
        storage
            .entry_in(ConversationId::from_raw(2), EntryId::from_raw(10))
            .await
            .expect("entry_in")
            .is_none(),
        "entry 10 belongs to conversation 1, not to conversation 2"
    );

    let all_conversations = storage
        .scan_conversations(conversation_query(None), 10, None)
        .await
        .expect("scan conversations");
    assert_eq!(
        ids(&all_conversations.items, |record| record.id.as_i64()),
        vec![1, 2, 3, 4],
        "conversations default to oldest first"
    );
    assert!(all_conversations.next.is_none());

    let first_page = storage
        .scan_conversations(conversation_query(None), 2, None)
        .await
        .expect("first conversation page");
    assert_eq!(
        ids(&first_page.items, |record| record.id.as_i64()),
        vec![1, 2]
    );
    let cursor = first_page.next.expect("a partial page has a cursor");
    assert_eq!(cursor.after, 2);
    assert_eq!(cursor.order, Some(ScanOrder::Ascending));
    // Cursors are opaque JSON; they must survive a round trip.
    let cursor: Cursor =
        serde_json::from_str(&serde_json::to_string(&cursor).expect("serialize the cursor"))
            .expect("round-trip the cursor");
    let second_page = storage
        .scan_conversations(conversation_query(None), 2, Some(cursor.clone()))
        .await
        .expect("second conversation page");
    assert_eq!(
        ids(&second_page.items, |record| record.id.as_i64()),
        vec![3, 4]
    );
    assert!(second_page.next.is_none());

    let wrong_order = storage
        .scan_conversations(
            conversation_query(Some(ScanOrder::Descending)),
            2,
            Some(cursor),
        )
        .await;
    assert!(
        matches!(wrong_order, Err(StorageError::Request(_))),
        "a cursor cannot switch orders, got {wrong_order:?}"
    );

    let descending = storage
        .scan_conversations(conversation_query(Some(ScanOrder::Descending)), 10, None)
        .await
        .expect("descending conversations");
    assert_eq!(
        ids(&descending.items, |record| record.id.as_i64()),
        vec![4, 3, 2, 1]
    );

    // Tasks: default ascending, limit + cursor, cross-order rejection.
    storage
        .commit(vec![
            StorageWrite::Task {
                value: pending_task(21, 1, "one"),
            },
            StorageWrite::Task {
                value: pending_task(22, 1, "two"),
            },
        ])
        .await
        .expect("seed tasks");
    let all_tasks = storage
        .scan_tasks(task_query(None), 10, None)
        .await
        .expect("scan tasks");
    assert_eq!(
        ids(&all_tasks.items, |record| record.id.as_i64()),
        vec![20, 21, 22],
        "tasks default to oldest first"
    );
    let first_task_page = storage
        .scan_tasks(task_query(None), 2, None)
        .await
        .expect("first task page");
    assert_eq!(
        ids(&first_task_page.items, |record| record.id.as_i64()),
        vec![20, 21]
    );
    let task_cursor = first_task_page.next.expect("task continuation");
    let second_task_page = storage
        .scan_tasks(task_query(None), 2, Some(task_cursor.clone()))
        .await
        .expect("second task page");
    assert_eq!(
        ids(&second_task_page.items, |record| record.id.as_i64()),
        vec![22]
    );
    assert!(second_task_page.next.is_none());
    let task_wrong_order = storage
        .scan_tasks(
            task_query(Some(ScanOrder::Descending)),
            2,
            Some(task_cursor),
        )
        .await;
    assert!(
        matches!(task_wrong_order, Err(StorageError::Request(_))),
        "task cursor order is bound to the first page, got {task_wrong_order:?}"
    );
    let descending_tasks = storage
        .scan_tasks(task_query(Some(ScanOrder::Descending)), 10, None)
        .await
        .expect("descending tasks");
    assert_eq!(
        ids(&descending_tasks.items, |record| record.id.as_i64()),
        vec![22, 21, 20]
    );

    // Submissions: default ascending, limit + cursor, cross-order rejection.
    storage
        .commit(vec![
            StorageWrite::Submission {
                value: submission(31, 1, Some("req-2"), SubmissionStatus::Queued),
            },
            StorageWrite::Submission {
                value: submission(32, 1, None, SubmissionStatus::Queued),
            },
        ])
        .await
        .expect("seed submissions");
    let all_submissions = storage
        .scan_submissions(submission_query(None), 10, None)
        .await
        .expect("scan submissions");
    assert_eq!(
        ids(&all_submissions.items, |record| record.id.as_i64()),
        vec![30, 31, 32],
        "submissions default to oldest first"
    );
    let first_submission_page = storage
        .scan_submissions(submission_query(None), 2, None)
        .await
        .expect("first submission page");
    assert_eq!(
        ids(&first_submission_page.items, |record| record.id.as_i64()),
        vec![30, 31]
    );
    let submission_cursor = first_submission_page.next.expect("submission continuation");
    let second_submission_page = storage
        .scan_submissions(submission_query(None), 2, Some(submission_cursor.clone()))
        .await
        .expect("second submission page");
    assert_eq!(
        ids(&second_submission_page.items, |record| record.id.as_i64()),
        vec![32]
    );
    assert!(second_submission_page.next.is_none());
    let submission_wrong_order = storage
        .scan_submissions(
            submission_query(Some(ScanOrder::Descending)),
            2,
            Some(submission_cursor),
        )
        .await;
    assert!(
        matches!(submission_wrong_order, Err(StorageError::Request(_))),
        "submission cursor order is bound to the first page, got {submission_wrong_order:?}"
    );
    let descending_submissions = storage
        .scan_submissions(submission_query(Some(ScanOrder::Descending)), 10, None)
        .await
        .expect("descending submissions");
    assert_eq!(
        ids(&descending_submissions.items, |record| record.id.as_i64()),
        vec![32, 31, 30]
    );

    // Entries: default newest first, and an ascending cursor keeps ascending
    // even when the follow-up query omits `order` (the cursor owns the order).
    let default_entries = storage
        .scan_entries(entry_query(None), 10, None)
        .await
        .expect("default entry scan");
    assert_eq!(
        ids(&default_entries.items, |record| record.id.as_i64()),
        vec![12, 11, 10],
        "entries default to newest first"
    );
    let ascending_entries = storage
        .scan_entries(entry_query(Some(ScanOrder::Ascending)), 10, None)
        .await
        .expect("ascending entry scan");
    assert_eq!(
        ids(&ascending_entries.items, |record| record.id.as_i64()),
        vec![10, 11, 12]
    );

    let ascending_first = storage
        .scan_entries(entry_query(Some(ScanOrder::Ascending)), 1, None)
        .await
        .expect("ascending entry page");
    assert_eq!(
        ids(&ascending_first.items, |record| record.id.as_i64()),
        vec![10]
    );
    let ascending_cursor = ascending_first.next.expect("entry continuation");
    let ascending_second = storage
        .scan_entries(entry_query(None), 1, Some(ascending_cursor.clone()))
        .await
        .expect("cursor continues its own order");
    assert_eq!(
        ids(&ascending_second.items, |record| record.id.as_i64()),
        vec![11],
        "an orderless query continues the cursor's ascending order"
    );
    let entry_wrong_order = storage
        .scan_entries(
            entry_query(Some(ScanOrder::Descending)),
            1,
            Some(ascending_cursor),
        )
        .await;
    assert!(
        matches!(entry_wrong_order, Err(StorageError::Request(_))),
        "entry cursor order is bound to the first page, got {entry_wrong_order:?}"
    );

    let descending_first = storage
        .scan_entries(entry_query(None), 2, None)
        .await
        .expect("descending entry page");
    assert_eq!(
        ids(&descending_first.items, |record| record.id.as_i64()),
        vec![12, 11]
    );
    let descending_cursor = descending_first.next.expect("descending continuation");
    let descending_second = storage
        .scan_entries(entry_query(None), 2, Some(descending_cursor))
        .await
        .expect("descending entry continuation");
    assert_eq!(
        ids(&descending_second.items, |record| record.id.as_i64()),
        vec![10]
    );
    assert!(descending_second.next.is_none());

    // ── 3. documents: lifecycle, history, copy, retirement ─────────────────
    let document_id = DocumentId::from_raw(40);
    let create_seq = storage
        .commit(vec![StorageWrite::DocumentCreate {
            record: document_record(
                40,
                "demo.doc",
                conversation_scope(1),
                Some(DocumentHistory::Rewindable),
                Some(DocumentFork::AsOf),
            ),
            content: DocumentContent::Base {
                version: 1,
                value: object([("count", json!(1)), ("items", json!(["a"]))]),
            },
        }])
        .await
        .expect("create a rewindable document");

    let change_seq = storage
        .commit(vec![StorageWrite::DocumentChange {
            id: document_id,
            content: DocumentContent::Delta {
                version: 1,
                ops: vec![
                    Op::Set(vec![Seg::Key("count".to_owned())], json!(2)),
                    Op::Splice {
                        path: vec![Seg::Key("items".to_owned())],
                        index: 1,
                        remove: 0,
                        items: vec![json!("b")],
                    },
                ],
            },
        }])
        .await
        .expect("apply a delta");

    let current = storage
        .document(document_id, DocumentPoint::Current)
        .await
        .expect("read current document")
        .expect("the document is alive");
    assert_eq!(current.version, 1);
    assert_eq!(current.value.get("count"), Some(&json!(2)));
    assert_eq!(current.value.get("items"), Some(&json!(["a", "b"])));
    assert_eq!(current.deltas_since_base, 1);

    let at_create = storage
        .document(document_id, DocumentPoint::Seq(create_seq))
        .await
        .expect("read the creation point")
        .expect("the document was alive at creation");
    assert_eq!(at_create.version, 1);
    assert_eq!(at_create.value.get("count"), Some(&json!(1)));
    assert_eq!(
        at_create.value.get("items"),
        Some(&json!(["a"])),
        "the delta is not visible before its commit"
    );
    assert_eq!(at_create.deltas_since_base, 0);

    let at_change = storage
        .document(document_id, DocumentPoint::Seq(change_seq))
        .await
        .expect("read the delta point")
        .expect("the document was alive at the change");
    assert_eq!(at_change.value.get("count"), Some(&json!(2)));
    assert_eq!(at_change.deltas_since_base, 1);

    // Returned materializations are detached: mutating one must not change
    // what a later read observes (spec §10 / upstream conformance).
    let mut mutated = current;
    mutated.value.insert("count".to_owned(), json!(999));
    let reread = storage
        .document(document_id, DocumentPoint::Current)
        .await
        .expect("re-read current document")
        .expect("still alive");
    assert_eq!(
        reread.value.get("count"),
        Some(&json!(2)),
        "a read result owns its materialized value"
    );

    let address = DocumentAddress {
        kind: "demo.doc".to_owned(),
        scope: conversation_scope(1),
        key: None,
    };
    assert_eq!(
        storage
            .find_document(address.clone(), DocumentPoint::Current)
            .await
            .expect("find current document")
            .expect("address is occupied")
            .id,
        document_id
    );
    assert_eq!(
        storage
            .find_document(address.clone(), DocumentPoint::Seq(create_seq))
            .await
            .expect("find historical document")
            .expect("address was occupied at creation")
            .id,
        document_id
    );

    // `document.copy` materializes the *stored* source into one independent
    // child base; later source changes must not leak into the copy.
    let copy_id = DocumentId::from_raw(41);
    let copy_seq = storage
        .commit(vec![StorageWrite::DocumentCopy {
            record: document_record(
                41,
                "demo.doc",
                conversation_scope(2),
                Some(DocumentHistory::Rewindable),
                Some(DocumentFork::AsOf),
            ),
            source: DocumentCopySource {
                id: document_id,
                at: DocumentPoint::Current,
            },
        }])
        .await
        .expect("copy the document");
    assert!(copy_seq > change_seq);
    let copied = storage
        .document(copy_id, DocumentPoint::Current)
        .await
        .expect("read the copy")
        .expect("the copy is alive");
    assert_eq!(copied.version, 1);
    assert_eq!(copied.value.get("count"), Some(&json!(2)));
    assert_eq!(copied.deltas_since_base, 0);

    let base_seq = storage
        .commit(vec![StorageWrite::DocumentChange {
            id: document_id,
            content: DocumentContent::Base {
                version: 2,
                value: object([("count", json!(3))]),
            },
        }])
        .await
        .expect("store a new source base");
    assert!(base_seq > copy_seq);
    let source_after = storage
        .document(document_id, DocumentPoint::Current)
        .await
        .expect("read the source")
        .expect("the source is alive");
    assert_eq!(source_after.version, 2);
    assert_eq!(source_after.value.get("count"), Some(&json!(3)));
    assert_eq!(
        source_after.deltas_since_base, 0,
        "a version change stores a base"
    );
    let copy_after = storage
        .document(copy_id, DocumentPoint::Current)
        .await
        .expect("read the copy again")
        .expect("the copy is alive");
    assert_eq!(
        copy_after.value.get("count"),
        Some(&json!(2)),
        "the copy is independent of later source changes"
    );
    let historical_after_base = storage
        .document(document_id, DocumentPoint::Seq(change_seq))
        .await
        .expect("read the historical delta point")
        .expect("still alive");
    assert_eq!(
        historical_after_base.value.get("count"),
        Some(&json!(2)),
        "installing a new base keeps the rewindable revision log"
    );

    // Retirement: current reads go empty; points before `retiredAt` still
    // materialize, and the retiring sequence itself is already outside the
    // half-open lifetime.
    let retire_seq = storage
        .commit(vec![StorageWrite::DocumentRetire { id: document_id }])
        .await
        .expect("retire the document");
    assert!(retire_seq > base_seq);
    assert!(
        storage
            .document(document_id, DocumentPoint::Current)
            .await
            .expect("read retired current")
            .is_none(),
        "a retired incarnation has no current state"
    );
    assert!(
        storage
            .document(document_id, DocumentPoint::Seq(retire_seq))
            .await
            .expect("read the retiring point")
            .is_none(),
        "the retiring commit is outside the incarnation's lifetime"
    );
    let before_retire = storage
        .document(document_id, DocumentPoint::Seq(base_seq))
        .await
        .expect("read before retirement")
        .expect("alive before retirement");
    assert_eq!(before_retire.value.get("count"), Some(&json!(3)));
    assert!(
        storage
            .find_document(address, DocumentPoint::Current)
            .await
            .expect("find after retirement")
            .is_none(),
        "the address is free again"
    );

    // A current-only document rejects historical materialization with a
    // `Request` error (spec §10.1: only invalid requests fail non-fatally).
    let latest_create_seq = storage
        .commit(vec![StorageWrite::DocumentCreate {
            record: document_record(42, "demo.latest", DocumentScope::Session, None, None),
            content: DocumentContent::Base {
                version: 1,
                value: object([("n", json!(1))]),
            },
        }])
        .await
        .expect("create a current-only document");
    assert!(
        matches!(
            storage
                .document(
                    DocumentId::from_raw(42),
                    DocumentPoint::Seq(latest_create_seq)
                )
                .await,
            Err(StorageError::Request(_))
        ),
        "current-only documents do not retain historical content"
    );
    storage
        .commit(vec![StorageWrite::DocumentRetire {
            id: DocumentId::from_raw(42),
        }])
        .await
        .expect("retire the current-only document");
    assert!(storage
        .document(DocumentId::from_raw(42), DocumentPoint::Current)
        .await
        .expect("read retired current-only document")
        .is_none());

    // `scan_documents` pages alive incarnations of one scope in ascending ID
    // order. The two documents form a keyed family so both may be current.
    storage
        .commit(vec![
            StorageWrite::DocumentCreate {
                record: document_record_with_key(
                    43,
                    "demo.scan",
                    Some("a"),
                    DocumentScope::Session,
                    None,
                    None,
                ),
                content: DocumentContent::Base {
                    version: 1,
                    value: object([("n", json!(43))]),
                },
            },
            StorageWrite::DocumentCreate {
                record: document_record_with_key(
                    44,
                    "demo.scan",
                    Some("b"),
                    DocumentScope::Session,
                    None,
                    None,
                ),
                content: DocumentContent::Base {
                    version: 1,
                    value: object([("n", json!(44))]),
                },
            },
        ])
        .await
        .expect("seed a document family");
    let document_query = DocumentQuery {
        scope: DocumentScope::Session,
        at: DocumentPointQuery::from(DocumentPoint::Current),
        kind: Some("demo.scan".to_owned()),
    };
    let first_document_page = storage
        .scan_documents(document_query.clone(), 1, None)
        .await
        .expect("scan documents");
    assert_eq!(
        ids(&first_document_page.items, |record| record.id.as_i64()),
        vec![43]
    );
    let document_cursor = first_document_page.next.expect("document continuation");
    let second_document_page = storage
        .scan_documents(document_query, 1, Some(document_cursor))
        .await
        .expect("continue the document scan");
    assert_eq!(
        ids(&second_document_page.items, |record| record.id.as_i64()),
        vec![44]
    );
    assert!(second_document_page.next.is_none());

    // ── 4. mint_id: strictly increasing and above every stored ID ─────────
    let first_mint = storage.mint_id().await.expect("mint an id");
    let second_mint = storage.mint_id().await.expect("mint an id");
    let third_mint = storage.mint_id().await.expect("mint an id");
    assert!(
        first_mint < second_mint && second_mint < third_mint,
        "minted ids strictly increase: {first_mint}, {second_mint}, {third_mint}"
    );
    assert!(
        first_mint > 44,
        "the mint watermark covers every stored id (got {first_mint})"
    );
}

// ─── backend entry points ──────────────────────────────────────────────────

#[tokio::test]
async fn memory_storage_conformance() {
    let storage = MemoryStorage::new();
    run_conformance(&storage).await;

    // Close semantics: every later operation rejects with `StorageError::Closed`.
    // Memory uses `StorageError::closed("MemoryStorage")`, i.e. the variant
    // carries "MemoryStorage is closed".
    storage.close().await.expect("close memory storage");
    assert!(
        matches!(
            storage.commit(Vec::new()).await,
            Err(StorageError::Closed(_))
        ),
        "commit after close must be `Closed`"
    );
    assert!(
        matches!(
            storage.conversation(ConversationId::from_raw(1)).await,
            Err(StorageError::Closed(_))
        ),
        "reads after close must be `Closed`"
    );
    assert!(
        matches!(storage.mint_id().await, Err(StorageError::Closed(_))),
        "mint after close must be `Closed`"
    );
}

#[tokio::test]
async fn jsonl_storage_conformance() {
    let dir = TempDir::new("conformance");
    let storage = JsonlStorage::open(dir.path())
        .await
        .expect("open the jsonl backend at a fresh directory");
    run_conformance(&storage).await;

    // JSONL uses `StorageError::closed("JsonlStorage")`; the variant is the
    // same `StorageError::Closed(String)`, only the backend name differs.
    storage.close().await.expect("close jsonl storage");
    assert!(matches!(
        storage.commit(Vec::new()).await,
        Err(StorageError::Closed(_))
    ));
    assert!(matches!(
        storage.entry(EntryId::from_raw(10)).await,
        Err(StorageError::Closed(_))
    ));
    assert!(matches!(
        storage.mint_id().await,
        Err(StorageError::Closed(_))
    ));

    // Drop before the TempDir does so Windows can remove every file.
    drop(storage);
}

// ─── SQLite backend ────────────────────────────────────────────────────────

#[cfg(any(feature = "extensions", feature = "context-mode"))]
#[tokio::test]
async fn sqlite_storage_conformance() {
    use pir::durable::storage::sqlite::SqliteStorage;

    let dir = TempDir::new("sqlite-conformance");
    let path = dir.path().join("durable.sqlite");
    let storage = SqliteStorage::open(&path)
        .await
        .expect("open the sqlite backend at a fresh file");
    run_conformance(&storage).await;

    storage.close().await.expect("close sqlite storage");
    assert!(matches!(
        storage.commit(Vec::new()).await,
        Err(StorageError::Closed(_))
    ));
    assert!(matches!(
        storage.entry(EntryId::from_raw(10)).await,
        Err(StorageError::Closed(_))
    ));
    assert!(matches!(
        storage.mint_id().await,
        Err(StorageError::Closed(_))
    ));

    // Drop before the TempDir does so Windows can remove the file.
    drop(storage);
}

#[cfg(any(feature = "extensions", feature = "context-mode"))]
#[tokio::test]
async fn sqlite_reopen_preserves_state_and_the_sequence() {
    use pir::durable::storage::sqlite::SqliteStorage;

    let dir = TempDir::new("sqlite-reopen");
    let path = dir.path().join("durable.sqlite");
    let entry_id = EntryId::from_raw(12);
    let seq = {
        let storage = SqliteStorage::open(&path).await.expect("open");
        let seq = storage
            .commit(vec![
                StorageWrite::Conversation {
                    value: conversation(1),
                },
                StorageWrite::Entry {
                    value: entry(12, 1, "pi.user", None),
                },
            ])
            .await
            .expect("commit");
        storage.close().await.expect("close");
        seq
    };

    let storage = SqliteStorage::open(&path).await.expect("reopen");
    let stored = storage
        .entry(entry_id)
        .await
        .expect("read entry")
        .expect("the entry survived the reopen");
    assert_eq!(stored.commit_seq, seq);
    // The sequence continues from the persisted watermark, not from zero.
    assert_eq!(
        storage.commit(Vec::new()).await.expect("empty commit"),
        Seq::from_raw(seq.as_i64() + 1)
    );
    storage.close().await.expect("close");
}

// ─── JSONL-specific durability ─────────────────────────────────────────────

#[tokio::test]
async fn jsonl_reopen_preserves_data_order_and_watermarks() {
    let dir = TempDir::new("reopen");
    let (seed_seq, delta_seq, base_seq, minted_before) = {
        let storage = JsonlStorage::open(dir.path()).await.expect("open");
        let seed_seq = storage
            .commit(vec![
                StorageWrite::Conversation {
                    value: conversation(1),
                },
                // Committed out of ID order on purpose: the log order is not
                // the scan order.
                StorageWrite::Entry {
                    value: entry(12, 1, "pi.assistant", None),
                },
                StorageWrite::Entry {
                    value: entry(10, 1, "pi.user", None),
                },
                StorageWrite::Entry {
                    value: entry(11, 1, "marker", Some(10)),
                },
                StorageWrite::Task {
                    value: pending_task(20, 1, "running"),
                },
                StorageWrite::Submission {
                    value: submission(30, 1, Some("req-reopen"), SubmissionStatus::Queued),
                },
            ])
            .await
            .expect("seed the log");

        storage
            .commit(vec![StorageWrite::DocumentCreate {
                record: document_record_with_key(
                    40,
                    "demo.reopen",
                    Some("k"),
                    conversation_scope(1),
                    Some(DocumentHistory::Rewindable),
                    Some(DocumentFork::AsOf),
                ),
                content: DocumentContent::Base {
                    version: 1,
                    value: object([("count", json!(1))]),
                },
            }])
            .await
            .expect("create the document");
        let delta_seq = storage
            .commit(vec![StorageWrite::DocumentChange {
                id: DocumentId::from_raw(40),
                content: DocumentContent::Delta {
                    version: 1,
                    ops: vec![Op::Set(vec![Seg::Key("count".to_owned())], json!(2))],
                },
            }])
            .await
            .expect("apply the delta");
        let base_seq = storage
            .commit(vec![StorageWrite::DocumentChange {
                id: DocumentId::from_raw(40),
                content: DocumentContent::Base {
                    version: 2,
                    value: object([("count", json!(3))]),
                },
            }])
            .await
            .expect("apply the new base");

        let minted_before = storage.mint_id().await.expect("mint before reopen");
        storage.close().await.expect("close before reopen");
        assert!(matches!(
            storage.commit(Vec::new()).await,
            Err(StorageError::Closed(_))
        ));
        (seed_seq, delta_seq, base_seq, minted_before)
    };

    let storage = JsonlStorage::open(dir.path()).await.expect("reopen");

    // Data and ID order are unchanged.
    let restored_entries = storage
        .scan_entries(entry_query(None), 10, None)
        .await
        .expect("scan entries after reopen");
    assert_eq!(
        ids(&restored_entries.items, |record| record.id.as_i64()),
        vec![12, 11, 10],
        "entries stay newest-first by ID, not by log order"
    );
    let restored_ascending = storage
        .scan_entries(entry_query(Some(ScanOrder::Ascending)), 10, None)
        .await
        .expect("scan entries ascending after reopen");
    assert_eq!(
        ids(&restored_ascending.items, |record| record.id.as_i64()),
        vec![10, 11, 12]
    );
    assert_eq!(
        storage
            .entry(EntryId::from_raw(10))
            .await
            .expect("entry after reopen")
            .expect("entry 10 survives")
            .commit_seq,
        seed_seq,
        "recovery replays the original commit sequence"
    );
    let restored_marker = storage
        .find_latest_head_marker(ConversationId::from_raw(1), None)
        .await
        .expect("head marker after reopen")
        .expect("entry 11 survives");
    assert_eq!(restored_marker.id, EntryId::from_raw(11));
    assert_eq!(restored_marker.head, Some(EntryId::from_raw(10)));

    let restored_task = storage
        .task(TaskId::from_raw(20))
        .await
        .expect("task after reopen")
        .expect("the live task sidecar survives");
    assert_eq!(restored_task.status(), TaskStatus::Pending);
    let restored_submission = storage
        .submission(SubmissionId::from_raw(30))
        .await
        .expect("submission after reopen")
        .expect("submission survives");
    assert_eq!(restored_submission.status, SubmissionStatus::Queued);
    assert_eq!(
        storage
            .submission_by_request(ConversationId::from_raw(1), "req-reopen")
            .await
            .expect("request index after reopen")
            .expect("request id survives")
            .id,
        SubmissionId::from_raw(30)
    );

    // Document bases and deltas are replayed; historical points still resolve.
    let restored_current = storage
        .document(DocumentId::from_raw(40), DocumentPoint::Current)
        .await
        .expect("document after reopen")
        .expect("document survives");
    assert_eq!(restored_current.version, 2);
    assert_eq!(restored_current.value.get("count"), Some(&json!(3)));
    assert_eq!(restored_current.deltas_since_base, 0);
    let restored_delta = storage
        .document(DocumentId::from_raw(40), DocumentPoint::Seq(delta_seq))
        .await
        .expect("historical document after reopen")
        .expect("the delta point survives");
    assert_eq!(restored_delta.version, 1);
    assert_eq!(restored_delta.value.get("count"), Some(&json!(2)));
    assert_eq!(restored_delta.deltas_since_base, 1);

    // The sequence watermark resumes after the highest recovered marker.
    let resumed_seq = storage
        .commit(Vec::new())
        .await
        .expect("commit after reopen");
    assert_eq!(
        resumed_seq,
        base_seq.next(),
        "the next commit continues the recovered sequence"
    );

    // The mint journal restores IDs handed out before the reopen; the new
    // value is also above every stored ID (max stored is 40).
    let minted_after = storage.mint_id().await.expect("mint after reopen");
    assert!(
        minted_after > minted_before,
        "minted ids never repeat across reopen: {minted_after} > {minted_before}"
    );
    assert!(minted_after > 40);

    storage.close().await.expect("close");
    drop(storage);
}

#[tokio::test]
async fn jsonl_reopen_truncates_a_torn_trailing_line() {
    let dir = TempDir::new("torn");
    {
        let storage = JsonlStorage::open(dir.path()).await.expect("open");
        storage
            .commit(vec![
                StorageWrite::Conversation {
                    value: conversation(1),
                },
                StorageWrite::Entry {
                    value: entry(10, 1, "pi.user", None),
                },
            ])
            .await
            .expect("seed");
        storage.close().await.expect("close");
    }

    // A crash mid-append leaves a newline-terminated prefix plus a partial
    // marker line. Recovery must drop the partial line, not fail.
    let main = dir.path().join("main.jsonl");
    let mut text = std::fs::read_to_string(&main).expect("read main log");
    text.push_str(
        "{\"format\":1,\"type\":\"commit\",\"seq\":2,\"writes\":[{\"type\":\"entry\",\"val",
    );
    std::fs::write(&main, &text).expect("append a torn marker");

    let storage = JsonlStorage::open(dir.path())
        .await
        .expect("reopen after a torn trailing line");
    let entries = storage
        .scan_entries(entry_query(None), 10, None)
        .await
        .expect("scan after torn recovery");
    assert_eq!(entries.items.len(), 1, "the committed marker survives");

    let recovered = std::fs::read_to_string(&main).expect("read recovered log");
    assert_eq!(
        recovered.lines().count(),
        1,
        "the torn tail is truncated on open"
    );
    assert!(recovered.ends_with('\n'), "the log ends on a line boundary");

    // The dropped torn marker did not advance the sequence.
    let next_seq = storage
        .commit(vec![StorageWrite::Entry {
            value: entry(11, 1, "pi.assistant", None),
        }])
        .await
        .expect("commit after recovery");
    assert_eq!(next_seq, Seq::from_raw(2));

    storage.close().await.expect("close");
    drop(storage);
}

#[tokio::test]
async fn jsonl_reopen_reports_a_complete_corrupt_line() {
    let dir = TempDir::new("corrupt");
    {
        let storage = JsonlStorage::open(dir.path()).await.expect("open");
        storage
            .commit(vec![StorageWrite::Conversation {
                value: conversation(1),
            }])
            .await
            .expect("seed");
        storage.close().await.expect("close");
    }

    // A complete (newline-terminated) but unparseable line is corruption; only
    // an unterminated final line is treated as a torn append.
    let main = dir.path().join("main.jsonl");
    let mut text = std::fs::read_to_string(&main).expect("read main log");
    text.push_str("this is a complete but malformed line\n");
    std::fs::write(&main, text).expect("append a corrupt line");

    match JsonlStorage::open(dir.path()).await {
        Err(StorageError::Corruption(error)) => {
            assert!(
                error.message.contains("Malformed complete"),
                "unexpected corruption message: {}",
                error.message
            );
        }
        other => panic!("expected StorageError::Corruption, got {other:?}"),
    }
}

#[tokio::test]
async fn jsonl_commit_completes_after_the_caller_future_is_dropped() {
    let dir = TempDir::new("commit-cancel");
    let storage = JsonlStorage::open(dir.path()).await.expect("open");

    // Poll once so `commit_inner` hands the work to its spawned task, then
    // drop the caller's future. The spawned task must still finish the marker,
    // apply the batch, and advance the sequence.
    let mut future = storage.commit(vec![StorageWrite::Conversation {
        value: conversation(1),
    }]);
    let mut cx = Context::from_waker(Waker::noop());
    assert!(
        matches!(future.as_mut().poll(&mut cx), Poll::Pending),
        "commit should yield after spawning its non-cancellable task"
    );
    drop(future);

    let mut committed = false;
    for _ in 0..200 {
        tokio::task::yield_now().await;
        if storage
            .conversation(ConversationId::from_raw(1))
            .await
            .expect("read after dropped commit")
            .is_some()
        {
            committed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    assert!(
        committed,
        "the spawned commit must complete after the caller future is dropped"
    );

    // The detached commit must have advanced the sequence, not left it free
    // for the next commit to reuse.
    let next_seq = storage.commit(Vec::new()).await.expect("next commit");
    assert_eq!(next_seq, Seq::from_raw(2));

    storage.close().await.expect("close");
    drop(storage);
}

#[tokio::test]
async fn jsonl_recovery_maps_invalid_committed_state_to_corruption() {
    let dir = TempDir::new("recover-invalid-state");
    {
        let storage = JsonlStorage::open(dir.path()).await.expect("open");
        storage
            .commit(vec![StorageWrite::Conversation {
                value: conversation(1),
            }])
            .await
            .expect("seed");
        storage.close().await.expect("close");
    }

    // This marker is syntactically valid and passes the wire-shape checks, but
    // reuses conversation ID 1. `prepare_commit_at` rejects it, and recovery
    // must classify the committed-state failure as corruption, not as an
    // invalid caller request.
    let main = dir.path().join("main.jsonl");
    let mut text = std::fs::read_to_string(&main).expect("read main log");
    text.push_str(
        "{\"format\":1,\"type\":\"commit\",\"seq\":2,\"writes\":[{\"type\":\"conversation\",\"value\":{\"id\":1}}]}\n",
    );
    std::fs::write(&main, text).expect("append duplicate conversation marker");

    match JsonlStorage::open(dir.path()).await {
        Err(StorageError::Corruption(error)) => {
            assert!(
                error
                    .message
                    .contains("Invalid committed state at sequence 2"),
                "unexpected corruption message: {}",
                error.message
            );
        }
        other => panic!("expected StorageError::Corruption, got {other:?}"),
    }
}

#[tokio::test]
async fn jsonl_recovery_rejects_an_exhausted_sequence_as_corruption() {
    let dir = TempDir::new("recover-max-seq");
    let main = dir.path().join("main.jsonl");
    std::fs::write(
        &main,
        "{\"format\":1,\"type\":\"commit\",\"seq\":9223372036854775807,\"writes\":[]}\n",
    )
    .expect("write max-seq marker");

    match JsonlStorage::open(dir.path()).await {
        Err(StorageError::Corruption(error)) => {
            assert!(
                error
                    .message
                    .contains("Invalid committed state at sequence 9223372036854775807"),
                "unexpected corruption message: {}",
                error.message
            );
        }
        other => panic!("expected StorageError::Corruption, got {other:?}"),
    }
}

#[tokio::test]
async fn memory_rejects_exhausted_sequence_and_id_space_without_overflow() {
    let sequence_storage = MemoryStorage::new();
    let prepared = sequence_storage
        .prepare_commit_at(Vec::new(), Seq::from_raw(i64::MAX))
        .await
        .expect("prepare the maximum sequence");
    let sequence_error = sequence_storage
        .apply_prepared(prepared)
        .await
        .expect_err("the maximum sequence must be exhausted");
    assert!(
        matches!(sequence_error, StorageError::Io(_)),
        "expected an Io sequence-space error, got {sequence_error:?}"
    );

    let id_storage = MemoryStorage::new();
    id_storage
        .commit(vec![StorageWrite::Task {
            value: pending_task(i64::MAX, 1, "max-id"),
        }])
        .await
        .expect("commit the maximum record ID");
    let id_error = id_storage
        .mint_id()
        .await
        .expect_err("the maximum record ID must exhaust the mint watermark");
    assert!(
        matches!(id_error, StorageError::Io(_)),
        "expected an Io ID-space error, got {id_error:?}"
    );
}
