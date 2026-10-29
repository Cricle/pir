//! Conformance tests for the durable entries + storage core.
//!
//! The same assertions run against both backends (`MemoryStorage` and
//! `JsonlStorage`) through [`run_conformance`], so a behavior that only one
//! backend has fails the suite. JSONL-specific tests cover reopen, torn-line
//! recovery, corruption detection and the `fsync` path.

use pir::durable::{
    apply_immutable_batches, delay_steps, format_size, id_from_number, rank_documents, scan_start,
    seq_from_number, tokenize, truncate_head, truncate_head_of, AssistantEntry, CompactionEntry,
    ConversationId, ConversationOwner, ConversationParent, ConversationQuery, ConversationRecord,
    Cursor, DocDefinition, DocumentAddress, DocumentContent, DocumentCreate, DocumentFork,
    DocumentHistory, DocumentPoint, DocumentPointQuery, DocumentQuery, DocumentScope,
    DocumentSemantics, EntryId, EntryQuery, EntryRecord, JsonlStorage, JsonlStorageOptions,
    MemoryStorage, Op, Page, ResetEntry, ScanOrder, SearchDocument, Seg, Seq, Storage,
    StorageError, StorageWrite, SubmissionId, SubmissionRecord, SubmissionStatus, SubmissionType,
    SystemEntry, TaskId, TaskQuery, TaskRecord, TaskState, TaskStatus, ToolResultEntry, Totals,
    TruncatedBy, TruncationOptions, TypedEntry, UserEntry, MAX_TIMER_DELAY_MS,
};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

// ── helpers ────────────────────────────────────────────────────────────────

fn temp_dir(tag: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let unique = COUNTER.fetch_add(1, Ordering::SeqCst);
    // PIDs can be recycled between nextest's process-per-test launches; a
    // nanosecond stamp keeps two runs of the same tag from sharing a dir even
    // when the PID and per-process counter happen to repeat.
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!(
        "pir-durable-{tag}-{}-{unique}-{stamp}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

async fn remove_dir(path: &Path) {
    // Windows refuses to delete an open handle; retry briefly after the
    // storage has been dropped/closed.
    for _ in 0..20 {
        match tokio::fs::remove_dir_all(path).await {
            Ok(()) => return,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
            Err(_) => tokio::time::sleep(std::time::Duration::from_millis(25)).await,
        }
    }
    let _ = tokio::fs::remove_dir_all(path).await;
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
        model: Some(vec![json!({"role": "user", "content": "hi"})]),
        data: None,
        head: head.map(EntryId::from_raw),
        edits: None,
        by_task_id: None,
    }
}

fn task(id: i64, conversation_id: i64, status: TaskStatus) -> TaskRecord {
    TaskRecord {
        trace: None,
        links: Vec::new(),
        id: TaskId::from_raw(id),
        conversation_id: ConversationId::from_raw(conversation_id),
        kind: "demo".to_owned(),
        version: 1,
        input: json!({"n": 1}),
        owner: None,
        background: false,
        abort_requested: false,
        abort_reason: None,
        abandon_on_restart: None,
        started_at: None,
        ended_at: None,
        restarts: None,
        state: match status {
            TaskStatus::Pending => TaskState::Pending {
                checkpoint: json!({"phase": "start"}),
            },
            TaskStatus::Running => TaskState::Running {
                checkpoint: json!({"phase": "start"}),
            },
            TaskStatus::Terminal => TaskState::Terminal {
                outcome: pir::durable::TaskOutcome::Completed {
                    result: json!({"ok": true}),
                },
            },
            // The conformance helper only asks for the statuses above; any
            // other status falls back to a pending state.
            _ => TaskState::Pending {
                checkpoint: json!({"phase": "start"}),
            },
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

fn document_create(
    id: i64,
    kind: &str,
    history: DocumentHistory,
    fork: DocumentFork,
) -> DocumentCreate {
    DocumentCreate {
        id: pir::durable::DocumentId::from_raw(id),
        kind: kind.to_owned(),
        key: None,
        scope: DocumentScope::Conversation {
            conversation_id: ConversationId::from_raw(1),
        },
        history: Some(history),
        fork: Some(fork),
    }
}

fn document_create_in(
    conversation: i64,
    id: i64,
    kind: &str,
    history: DocumentHistory,
    fork: DocumentFork,
) -> DocumentCreate {
    DocumentCreate {
        id: pir::durable::DocumentId::from_raw(id),
        kind: kind.to_owned(),
        key: None,
        scope: DocumentScope::Conversation {
            conversation_id: ConversationId::from_raw(conversation),
        },
        history: Some(history),
        fork: Some(fork),
    }
}

fn address(kind: &str) -> DocumentAddress {
    DocumentAddress {
        kind: kind.to_owned(),
        scope: DocumentScope::Conversation {
            conversation_id: ConversationId::from_raw(1),
        },
        key: None,
    }
}

fn document_query(kind: &str) -> DocumentQuery {
    DocumentQuery {
        scope: DocumentScope::Conversation {
            conversation_id: ConversationId::from_raw(1),
        },
        at: DocumentPointQuery::from(DocumentPoint::Current),
        kind: Some(kind.to_owned()),
    }
}

// ── shared conformance ─────────────────────────────────────────────────────

async fn run_conformance<S: Storage + ?Sized>(storage: &S) {
    // One batch: a conversation, three entries (the middle one a head), a task
    // and a submission.
    let seq = storage
        .commit(vec![
            StorageWrite::Conversation {
                value: conversation(1),
            },
            StorageWrite::Entry {
                value: entry(10, 1, UserEntry::KIND.kind, None),
            },
            StorageWrite::Entry {
                value: entry(11, 1, AssistantEntry::KIND.kind, Some(10)),
            },
            StorageWrite::Entry {
                value: entry(12, 1, SystemEntry::KIND.kind, None),
            },
            StorageWrite::Task {
                value: task(20, 1, TaskStatus::Pending),
            },
            StorageWrite::Submission {
                value: submission(30, 1, Some("req-1"), SubmissionStatus::Queued),
            },
        ])
        .await
        .expect("commit");
    assert_eq!(seq, Seq::from_raw(1), "first commit carries sequence 1");

    // Read back by ID.
    let found = storage
        .entry(EntryId::from_raw(11))
        .await
        .expect("entry")
        .expect("present");
    assert_eq!(found.entry.kind, AssistantEntry::KIND.kind);
    assert_eq!(found.commit_seq, seq);
    assert!(AssistantEntry::is(Some(&found.entry)));
    assert!(!UserEntry::is(Some(&found.entry)));

    // Scan by conversation, default order descending.
    let page = storage
        .scan_entries(
            EntryQuery {
                conversation_id: ConversationId::from_raw(1),
                min_entry_id: None,
                max_entry_id: None,
                order: None,
            },
            10,
            None,
        )
        .await
        .expect("scan entries");
    let ids: Vec<i64> = page.items.iter().map(|entry| entry.id.as_i64()).collect();
    assert_eq!(ids, vec![12, 11, 10], "descending by id");

    // Pagination: one at a time, cursor continues the descending order.
    let mut cursor: Option<Cursor> = None;
    let mut paged = Vec::new();
    loop {
        let page = storage
            .scan_entries(
                EntryQuery {
                    conversation_id: ConversationId::from_raw(1),
                    min_entry_id: None,
                    max_entry_id: None,
                    order: None,
                },
                1,
                cursor.clone(),
            )
            .await
            .expect("paged scan");
        paged.extend(page.items.iter().map(|entry| entry.id.as_i64()));
        match page.next {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    assert_eq!(
        paged,
        vec![12, 11, 10],
        "cursor pagination covers every entry once"
    );

    // A cursor is bound to its order; asking for another order is a request error.
    let mismatch = storage
        .scan_entries(
            EntryQuery {
                conversation_id: ConversationId::from_raw(1),
                min_entry_id: None,
                max_entry_id: None,
                order: Some(ScanOrder::Ascending),
            },
            1,
            cursor.clone(),
        )
        .await;
    assert!(matches!(mismatch, Err(StorageError::Request(_))));

    // Ascending scan.
    let ascending = storage
        .scan_entries(
            EntryQuery {
                conversation_id: ConversationId::from_raw(1),
                min_entry_id: None,
                max_entry_id: None,
                order: Some(ScanOrder::Ascending),
            },
            10,
            None,
        )
        .await
        .expect("ascending scan");
    let ascending_ids: Vec<i64> = ascending
        .items
        .iter()
        .map(|entry| entry.id.as_i64())
        .collect();
    assert_eq!(ascending_ids, vec![10, 11, 12]);

    // Unknown conversation is an invalid request, not a fatal error.
    let unknown = storage
        .scan_entries(
            EntryQuery {
                conversation_id: ConversationId::from_raw(999),
                min_entry_id: None,
                max_entry_id: None,
                order: None,
            },
            10,
            None,
        )
        .await;
    assert!(matches!(unknown, Err(StorageError::Request(_))));

    // Latest head marker: entry 11 carries head 10.
    let marker = storage
        .find_latest_head_marker(ConversationId::from_raw(1), None)
        .await
        .expect("head marker")
        .expect("present");
    assert_eq!(marker.id, EntryId::from_raw(11));
    assert_eq!(marker.head, Some(EntryId::from_raw(10)));

    // Task + submission reads.
    let stored_task = storage
        .task(TaskId::from_raw(20))
        .await
        .expect("task")
        .expect("present");
    assert_eq!(stored_task.status(), TaskStatus::Pending);
    assert_eq!(stored_task.input, json!({"n": 1}));

    let stored_submission = storage
        .submission(SubmissionId::from_raw(30))
        .await
        .expect("submission")
        .expect("present");
    assert_eq!(stored_submission.status, SubmissionStatus::Queued);
    let by_request = storage
        .submission_by_request(ConversationId::from_raw(1), "req-1")
        .await
        .expect("submission by request")
        .expect("present");
    assert_eq!(by_request.id, SubmissionId::from_raw(30));

    // Task status transition moves the record between status indexes.
    let mut running = stored_task.clone();
    running.state = TaskState::Running {
        checkpoint: json!({"phase": "work"}),
    };
    storage
        .commit(vec![StorageWrite::Task { value: running }])
        .await
        .expect("task transition");
    let pending_page = storage
        .scan_tasks(
            TaskQuery {
                status: Some(TaskStatus::Pending),
                ..TaskQuery::default()
            },
            10,
            None,
        )
        .await
        .expect("pending scan");
    assert!(pending_page.items.is_empty(), "the task left pending");
    let running_page = storage
        .scan_tasks(
            TaskQuery {
                status: Some(TaskStatus::Running),
                ..TaskQuery::default()
            },
            10,
            None,
        )
        .await
        .expect("running scan");
    assert_eq!(running_page.items.len(), 1);

    // Conversation scan with owner filter.
    let mut child = conversation(2);
    child.owner = Some(ConversationOwner {
        conversation_id: ConversationId::from_raw(1),
        task_id: TaskId::from_raw(20),
    });
    storage
        .commit(vec![StorageWrite::Conversation { value: child }])
        .await
        .expect("child conversation");
    let owned = storage
        .scan_conversations(
            ConversationQuery {
                owner_conversation_id: Some(ConversationId::from_raw(1)),
                owner_task_id: None,
                order: None,
            },
            10,
            None,
        )
        .await
        .expect("owned conversations");
    assert_eq!(owned.items.len(), 1);
    assert_eq!(owned.items[0].id.as_i64(), 2);

    // ── documents ─────────────────────────────────────────────────────────
    let document_id = pir::durable::DocumentId::from_raw(40);
    storage
        .commit(vec![StorageWrite::DocumentCreate {
            record: document_create(
                40,
                "demo.doc",
                DocumentHistory::Rewindable,
                DocumentFork::Current,
            ),
            content: DocumentContent::Base {
                version: 1,
                value: serde_json::Map::from_iter([("count".to_owned(), json!(1))]),
            },
        }])
        .await
        .expect("document create");

    let stored = storage
        .document(document_id, DocumentPoint::Current)
        .await
        .expect("document")
        .expect("present");
    assert_eq!(stored.version, 1);
    assert_eq!(stored.value.get("count"), Some(&json!(1)));
    assert_eq!(stored.deltas_since_base, 0);

    let by_address = storage
        .find_document(address("demo.doc"), DocumentPoint::Current)
        .await
        .expect("find document")
        .expect("present");
    assert_eq!(by_address.id, document_id);

    // A delta bumps the count and keeps the base.
    storage
        .commit(vec![StorageWrite::DocumentChange {
            id: document_id,
            content: DocumentContent::Delta {
                version: 1,
                ops: vec![Op::Set(vec![Seg::Key("count".to_owned())], json!(2))],
            },
        }])
        .await
        .expect("document change");
    let changed = storage
        .document(document_id, DocumentPoint::Current)
        .await
        .expect("document")
        .expect("present");
    assert_eq!(changed.value.get("count"), Some(&json!(2)));
    assert_eq!(changed.deltas_since_base, 1);

    // Scan documents in the scope.
    let documents = storage
        .scan_documents(document_query("demo.doc"), 10, None)
        .await
        .expect("scan documents");
    assert_eq!(documents.items.len(), 1);
    assert_eq!(documents.items[0].id, document_id);

    // Retire it: current lookups go empty, historical ones still resolve.
    let retire_seq = storage
        .commit(vec![StorageWrite::DocumentRetire { id: document_id }])
        .await
        .expect("document retire");
    assert!(storage
        .document(document_id, DocumentPoint::Current)
        .await
        .expect("document")
        .is_none());
    let historical = storage
        .document(document_id, DocumentPoint::Seq(retire_seq))
        .await
        .expect("historical document");
    assert!(
        historical.is_none(),
        "a retired incarnation is not alive at the retiring sequence"
    );
    let before_retire = storage
        .document(
            document_id,
            DocumentPoint::Seq(Seq::from_raw(retire_seq.as_i64() - 1)),
        )
        .await
        .expect("historical document")
        .expect("alive before retirement");
    assert_eq!(before_retire.value.get("count"), Some(&json!(2)));
    assert!(storage
        .find_document(address("demo.doc"), DocumentPoint::Current)
        .await
        .expect("find document")
        .is_none());

    // A current-only document rejects historical reads.
    storage
        .commit(vec![StorageWrite::DocumentCreate {
            record: document_create(
                41,
                "demo.latest",
                DocumentHistory::Latest,
                DocumentFork::Current,
            ),
            content: DocumentContent::Base {
                version: 1,
                value: serde_json::Map::from_iter([("n".to_owned(), json!(0))]),
            },
        }])
        .await
        .expect("latest document");
    let historical_latest = storage
        .document(
            pir::durable::DocumentId::from_raw(41),
            DocumentPoint::Seq(Seq::from_raw(1)),
        )
        .await;
    assert!(matches!(historical_latest, Err(StorageError::Request(_))));

    // A definition-free copy materializes the source's current value.
    storage
        .commit(vec![StorageWrite::DocumentCreate {
            record: document_create(
                42,
                "demo.copy",
                DocumentHistory::Rewindable,
                DocumentFork::Current,
            ),
            content: DocumentContent::Base {
                version: 3,
                value: serde_json::Map::from_iter([("v".to_owned(), json!(7))]),
            },
        }])
        .await
        .expect("copy source");
    storage
        .commit(vec![StorageWrite::DocumentCopy {
            record: document_create_in(
                2,
                43,
                "demo.copy",
                DocumentHistory::Rewindable,
                DocumentFork::Current,
            ),
            source: pir::durable::DocumentCopySource {
                id: pir::durable::DocumentId::from_raw(42),
                at: DocumentPoint::Current,
            },
        }])
        .await
        .expect("document copy");
    let copied = storage
        .document(
            pir::durable::DocumentId::from_raw(43),
            DocumentPoint::Current,
        )
        .await
        .expect("copied document")
        .expect("present");
    assert_eq!(
        copied.version, 3,
        "the copy keeps the source's definition version"
    );
    assert_eq!(copied.value.get("v"), Some(&json!(7)));

    // ── IDs ───────────────────────────────────────────────────────────────
    // `mint_id` returns the raw number; brand it here.
    let first = id_from_number::<pir::durable::EntryIdKind>(storage.mint_id().await.expect("mint"));
    let second =
        id_from_number::<pir::durable::EntryIdKind>(storage.mint_id().await.expect("mint"));
    assert!(first.as_i64() > 41, "minted IDs clear the stored watermark");
    assert!(second > first, "minting is monotonic");
}

/// Compile-time proof that a backend's operation futures are `Send`, so callers
/// can `tokio::spawn` them.
fn assert_send<T: Send>(_: T) {}

// ── memory backend ─────────────────────────────────────────────────────────

#[tokio::test]
async fn memory_conformance() {
    let storage = MemoryStorage::new();
    assert_send(storage.conversation(ConversationId::from_raw(1)));
    assert_send(storage.commit(Vec::new()));
    assert_send(storage.mint_id());
    run_conformance(&storage).await;
    storage.close().await.expect("close");
    assert!(matches!(
        storage.conversation(ConversationId::from_raw(1)).await,
        Err(StorageError::Closed(_))
    ));
}

#[tokio::test]
async fn memory_rejects_duplicate_ids_and_bad_batches() {
    let storage = MemoryStorage::new();
    storage
        .commit(vec![
            StorageWrite::Conversation {
                value: conversation(1),
            },
            StorageWrite::Entry {
                value: entry(10, 1, UserEntry::KIND.kind, None),
            },
        ])
        .await
        .expect("seed");

    let duplicate = storage
        .commit(vec![StorageWrite::Entry {
            value: entry(10, 1, SystemEntry::KIND.kind, None),
        }])
        .await;
    assert!(matches!(duplicate, Err(StorageError::Invalid(_))));

    let unknown_document = storage
        .commit(vec![StorageWrite::DocumentChange {
            id: pir::durable::DocumentId::from_raw(77),
            content: DocumentContent::Base {
                version: 1,
                value: serde_json::Map::new(),
            },
        }])
        .await;
    assert!(matches!(unknown_document, Err(StorageError::Invalid(_))));

    // A rejected batch leaves no trace.
    assert!(storage
        .entry(EntryId::from_raw(10))
        .await
        .expect("entry")
        .is_some());
}

// ── object safety ──────────────────────────────────────────────────────────

/// One commit and a few reads through `&dyn Storage`, the shape the harness
/// holds (`Arc<dyn Storage>`).
async fn exercise_through_trait_object(storage: &dyn Storage) {
    let seq = storage
        .commit(vec![
            StorageWrite::Conversation {
                value: conversation(1),
            },
            StorageWrite::Entry {
                value: entry(10, 1, UserEntry::KIND.kind, None),
            },
        ])
        .await
        .expect("commit through the trait object");
    assert!(seq.as_i64() > 0);

    let found = storage
        .conversation(ConversationId::from_raw(1))
        .await
        .expect("conversation through the trait object")
        .expect("present");
    assert_eq!(found.id, ConversationId::from_raw(1));

    let found = storage
        .entry(EntryId::from_raw(10))
        .await
        .expect("entry through the trait object")
        .expect("present");
    assert_eq!(found.entry.id, EntryId::from_raw(10));

    let minted = storage
        .mint_id()
        .await
        .expect("mint through the trait object");
    assert!(minted > 10, "the mint watermark covers stored IDs");
}

#[tokio::test]
async fn backends_are_object_safe() {
    let memory: std::sync::Arc<dyn Storage> = std::sync::Arc::new(MemoryStorage::new());
    exercise_through_trait_object(memory.as_ref()).await;
    memory.close().await.expect("close");
    assert!(matches!(
        memory.conversation(ConversationId::from_raw(1)).await,
        Err(StorageError::Closed(_))
    ));

    let dir = temp_dir("object-safe");
    let jsonl: std::sync::Arc<dyn Storage> =
        std::sync::Arc::new(JsonlStorage::open(&dir).await.expect("open"));
    exercise_through_trait_object(jsonl.as_ref()).await;
    jsonl.close().await.expect("close");
    drop(jsonl);
    remove_dir(&dir).await;
}

// ── JSONL backend ──────────────────────────────────────────────────────────

#[tokio::test]
async fn jsonl_conformance() {
    let dir = temp_dir("conformance");
    let storage = JsonlStorage::open(&dir).await.expect("open");
    assert_send(storage.scan_entries(
        EntryQuery {
            conversation_id: ConversationId::from_raw(1),
            min_entry_id: None,
            max_entry_id: None,
            order: None,
        },
        1,
        None,
    ));
    run_conformance(&storage).await;
    storage.close().await.expect("close");
    remove_dir(&dir).await;
}

#[tokio::test]
async fn jsonl_survives_reopen_with_ids_and_documents() {
    let dir = temp_dir("reopen");
    let highest_id;
    {
        let storage = JsonlStorage::open(&dir).await.expect("open");
        storage
            .commit(vec![
                StorageWrite::Conversation {
                    value: conversation(1),
                },
                StorageWrite::Entry {
                    value: entry(10, 1, UserEntry::KIND.kind, None),
                },
                StorageWrite::DocumentCreate {
                    record: document_create(
                        50,
                        "demo.doc",
                        DocumentHistory::Rewindable,
                        DocumentFork::Current,
                    ),
                    content: DocumentContent::Base {
                        version: 1,
                        value: serde_json::Map::from_iter([("value".to_owned(), json!("base"))]),
                    },
                },
            ])
            .await
            .expect("seed");
        storage
            .commit(vec![StorageWrite::DocumentChange {
                id: pir::durable::DocumentId::from_raw(50),
                content: DocumentContent::Delta {
                    version: 1,
                    ops: vec![Op::Set(vec![Seg::Key("value".to_owned())], json!("delta"))],
                },
            }])
            .await
            .expect("change");
        let minted: pir::durable::Id<pir::durable::EntryIdKind> =
            id_from_number(storage.mint_id().await.expect("mint"));
        highest_id = minted.as_i64();
        drop(storage);
    }

    let reopened = JsonlStorage::open(&dir).await.expect("reopen");
    let entries = reopened
        .scan_entries(
            EntryQuery {
                conversation_id: ConversationId::from_raw(1),
                min_entry_id: None,
                max_entry_id: None,
                order: None,
            },
            10,
            None,
        )
        .await
        .expect("scan");
    assert_eq!(entries.items.len(), 1);
    assert_eq!(entries.items[0].id.as_i64(), 10);

    let document = reopened
        .document(
            pir::durable::DocumentId::from_raw(50),
            DocumentPoint::Current,
        )
        .await
        .expect("document")
        .expect("present");
    assert_eq!(document.value.get("value"), Some(&json!("delta")));
    assert_eq!(document.deltas_since_base, 1);

    let minted: pir::durable::Id<pir::durable::EntryIdKind> =
        id_from_number(reopened.mint_id().await.expect("mint"));
    assert!(
        minted.as_i64() > highest_id,
        "ids stay monotonic across reopen: {} > {highest_id}",
        minted.as_i64()
    );
    reopened.close().await.expect("close");
    remove_dir(&dir).await;
}

#[tokio::test]
async fn jsonl_recovers_from_a_torn_trailing_line() {
    let dir = temp_dir("torn");
    {
        let storage = JsonlStorage::open(&dir).await.expect("open");
        storage
            .commit(vec![
                StorageWrite::Conversation {
                    value: conversation(1),
                },
                StorageWrite::Entry {
                    value: entry(10, 1, UserEntry::KIND.kind, None),
                },
            ])
            .await
            .expect("seed");
        storage
            .close()
            .await
            .expect("close before manual file edit");
    }

    // Append a half-written marker, as a crash mid-append would leave.
    let main = dir.join("main.jsonl");
    let seed_raw = std::fs::read_to_string(&main).expect("read main");
    assert!(
        seed_raw.contains("\"conversation\""),
        "seed commit wrote no conversation marker: {seed_raw:?}"
    );
    let mut text = seed_raw;
    text.push_str(
        "{\"format\":1,\"type\":\"commit\",\"seq\":2,\"writes\":[{\"type\":\"entry\",\"val",
    );
    std::fs::write(&main, &text).expect("append torn line");

    let reopened = JsonlStorage::open(&dir)
        .await
        .expect("reopen after torn line");
    let entries = reopened
        .scan_entries(
            EntryQuery {
                conversation_id: ConversationId::from_raw(1),
                min_entry_id: None,
                max_entry_id: None,
                order: None,
            },
            10,
            None,
        )
        .await
        .expect("scan");
    assert_eq!(entries.items.len(), 1, "the committed line survives");

    // The torn tail is gone, so a new commit starts from the recovered state.
    let seq = reopened
        .commit(vec![StorageWrite::Entry {
            value: entry(11, 1, AssistantEntry::KIND.kind, None),
        }])
        .await
        .expect("commit after recovery");
    assert_eq!(seq, Seq::from_raw(2));
    let raw = std::fs::read_to_string(&main).expect("read main");
    assert_eq!(raw.lines().count(), 2);
    assert!(raw.ends_with('\n'));
    reopened.close().await.expect("close");
    remove_dir(&dir).await;
}

#[tokio::test]
async fn jsonl_reports_corruption_for_a_malformed_complete_line() {
    let dir = temp_dir("corrupt");
    {
        let storage = JsonlStorage::open(&dir).await.expect("open");
        storage
            .commit(vec![StorageWrite::Conversation {
                value: conversation(1),
            }])
            .await
            .expect("seed");
        storage
            .close()
            .await
            .expect("close before manual file edit");
    }

    // A complete (newline-terminated) but unparseable line is corruption.
    let main = dir.join("main.jsonl");
    let mut text = std::fs::read_to_string(&main).expect("read main");
    text.push_str("this is not json\n");
    std::fs::write(&main, text).expect("write corruption");

    match JsonlStorage::open(&dir).await {
        Err(StorageError::Corruption(error)) => {
            assert!(
                error.message.contains("Malformed complete"),
                "unexpected message: {}",
                error.message
            );
        }
        other => {
            assert!(
                matches!(other, Err(StorageError::Corruption(_))),
                "expected corruption, got {other:?}"
            );
        }
    }
    remove_dir(&dir).await;
}

#[tokio::test]
async fn jsonl_rejects_out_of_order_sequences() {
    let dir = temp_dir("out-of-order");
    {
        let storage = JsonlStorage::open(&dir).await.expect("open");
        storage
            .commit(vec![StorageWrite::Conversation {
                value: conversation(1),
            }])
            .await
            .expect("first");
        storage
            .commit(vec![StorageWrite::Conversation {
                value: conversation(2),
            }])
            .await
            .expect("second");
        storage
            .close()
            .await
            .expect("close before manual file edit");
    }
    let main = dir.join("main.jsonl");
    let raw = std::fs::read_to_string(&main).expect("read main");
    let mut lines: Vec<String> = raw.lines().map(str::to_owned).collect();
    assert!(
        lines.len() >= 2,
        "two commits must leave two markers in {main:?}; got {raw:?}"
    );
    lines.swap(0, 1);
    std::fs::write(&main, format!("{}\n", lines.join("\n"))).expect("rewrite");

    match JsonlStorage::open(&dir).await {
        Err(StorageError::Corruption(error)) => {
            assert!(
                error.message.contains("does not strictly increase"),
                "unexpected message: {}",
                error.message
            );
        }
        other => {
            assert!(
                matches!(other, Err(StorageError::Corruption(_))),
                "expected corruption, got {other:?}"
            );
        }
    }
    remove_dir(&dir).await;
}

#[tokio::test]
async fn jsonl_fsync_path_commits_and_reopens() {
    let dir = temp_dir("fsync");
    {
        let storage = JsonlStorage::open_with_options(&dir, JsonlStorageOptions { fsync: true })
            .await
            .expect("open");
        storage
            .commit(vec![
                StorageWrite::Conversation {
                    value: conversation(1),
                },
                StorageWrite::Entry {
                    value: entry(10, 1, UserEntry::KIND.kind, None),
                },
                StorageWrite::DocumentCreate {
                    record: document_create(
                        60,
                        "fsync.doc",
                        DocumentHistory::Latest,
                        DocumentFork::Current,
                    ),
                    content: DocumentContent::Base {
                        version: 1,
                        value: serde_json::Map::from_iter([("n".to_owned(), json!(1))]),
                    },
                },
            ])
            .await
            .expect("commit");
        drop(storage);
    }
    let reopened = JsonlStorage::open_with_options(&dir, JsonlStorageOptions { fsync: true })
        .await
        .expect("reopen");
    assert!(reopened
        .entry(EntryId::from_raw(10))
        .await
        .expect("entry")
        .is_some());
    assert!(reopened
        .document(
            pir::durable::DocumentId::from_raw(60),
            DocumentPoint::Current
        )
        .await
        .expect("document")
        .is_some());
    reopened.close().await.expect("close");
    remove_dir(&dir).await;
}

#[tokio::test]
async fn jsonl_live_task_sidecar_survives_reopen_and_is_reclaimed_when_terminal() {
    let dir = temp_dir("task-sidecar");
    {
        let storage = JsonlStorage::open(&dir).await.expect("open");
        storage
            .commit(vec![
                StorageWrite::Conversation {
                    value: conversation(1),
                },
                StorageWrite::Task {
                    value: task(20, 1, TaskStatus::Running),
                },
            ])
            .await
            .expect("live task");
        drop(storage);
    }
    let sidecars = pir::durable::storage::jsonl::sidecar_files(&dir)
        .await
        .expect("list sidecars");
    assert_eq!(sidecars, vec!["task-20.jsonl".to_owned()]);

    let storage = JsonlStorage::open(&dir).await.expect("reopen");
    let live = storage
        .task(TaskId::from_raw(20))
        .await
        .expect("task")
        .expect("present");
    assert_eq!(live.status(), TaskStatus::Running);

    storage
        .commit(vec![StorageWrite::Task {
            value: task(20, 1, TaskStatus::Terminal),
        }])
        .await
        .expect("terminal task");
    let sidecars = pir::durable::storage::jsonl::sidecar_files(&dir)
        .await
        .expect("list sidecars");
    assert!(
        sidecars.is_empty(),
        "a terminal task's sidecar is reclaimed"
    );

    drop(storage);
    let reopened = JsonlStorage::open(&dir).await.expect("reopen terminal");
    let terminal = reopened
        .task(TaskId::from_raw(20))
        .await
        .expect("task")
        .expect("present");
    assert_eq!(terminal.status(), TaskStatus::Terminal);
    reopened.close().await.expect("close");
    remove_dir(&dir).await;
}

// ── pure functions ─────────────────────────────────────────────────────────

#[test]
fn truncate_head_keeps_whole_lines() {
    let result = truncate_head(
        "one\ntwo\nthree\nfour",
        TruncationOptions {
            max_lines: Some(2),
            max_bytes: None,
        },
    );
    assert!(result.truncated);
    assert_eq!(result.truncated_by, Some(TruncatedBy::Lines));
    assert_eq!(result.content, "one\ntwo");
    assert_eq!(result.output_lines, 2);
    assert_eq!(result.total_lines, 4);
    assert_eq!(result.output_bytes, 7);
    assert_eq!(result.total_bytes, 18);
    assert!(!result.first_line_exceeds_limit);
}

#[test]
fn truncate_head_byte_limit_and_first_line() {
    let result = truncate_head(
        "abcdef\nghijkl",
        TruncationOptions {
            max_lines: None,
            max_bytes: Some(10),
        },
    );
    assert_eq!(result.truncated_by, Some(TruncatedBy::Bytes));
    assert_eq!(result.content, "abcdef");

    let first_line = truncate_head(
        "0123456789abcdef",
        TruncationOptions {
            max_lines: None,
            max_bytes: Some(4),
        },
    );
    assert!(first_line.truncated);
    assert!(first_line.first_line_exceeds_limit);
    assert_eq!(first_line.content, "");
    assert_eq!(first_line.output_lines, 0);
}

#[test]
fn truncate_head_no_truncation_and_trailing_newline() {
    let result = truncate_head("a\nb\n", TruncationOptions::default());
    assert!(!result.truncated);
    assert_eq!(
        result.total_lines, 2,
        "a trailing newline adds no empty line"
    );
    assert_eq!(result.content, "a\nb\n");

    let empty = truncate_head("", TruncationOptions::default());
    assert!(!empty.truncated);
    assert_eq!(empty.total_lines, 0);
    assert_eq!(empty.content, "");
}

#[test]
fn truncate_head_of_matches_truncate_head() {
    let text = "alpha\nbeta\ngamma\ndelta\n";
    let totals = Totals {
        lines: 4,
        bytes: text.len(),
    };
    let options = TruncationOptions {
        max_lines: Some(2),
        max_bytes: None,
    };
    assert_eq!(
        truncate_head_of(text, totals, options),
        truncate_head(text, options)
    );
}

#[test]
fn format_size_boundaries() {
    assert_eq!(format_size(0), "0B");
    assert_eq!(format_size(1023), "1023B");
    assert_eq!(format_size(1024), "1.0KB");
    assert_eq!(format_size(1536), "1.5KB");
    assert_eq!(format_size(1024 * 1024), "1.0MB");
    assert_eq!(format_size(3 * 1024 * 1024), "3.0MB");
}

#[test]
fn timer_delay_steps_clamp() {
    assert_eq!(delay_steps(0).collect::<Vec<_>>(), vec![0]);
    assert_eq!(delay_steps(-5).collect::<Vec<_>>(), vec![0]);
    assert_eq!(delay_steps(1000).collect::<Vec<_>>(), vec![1000]);
    assert_eq!(
        delay_steps(MAX_TIMER_DELAY_MS).collect::<Vec<_>>(),
        vec![MAX_TIMER_DELAY_MS]
    );
    assert_eq!(
        delay_steps(MAX_TIMER_DELAY_MS + 1).collect::<Vec<_>>(),
        vec![MAX_TIMER_DELAY_MS, 1]
    );
    let steps: Vec<i64> = delay_steps(2 * MAX_TIMER_DELAY_MS + 7).collect();
    assert_eq!(steps, vec![MAX_TIMER_DELAY_MS, MAX_TIMER_DELAY_MS, 7]);
    assert_eq!(steps.iter().sum::<i64>(), 2 * MAX_TIMER_DELAY_MS + 7);
}

#[test]
fn scan_start_rejects_invalid_cursors() {
    let default_order = ScanOrder::Ascending;
    let start = scan_start(None, None, default_order).expect("no cursor");
    assert_eq!(start.order, ScanOrder::Ascending);
    assert_eq!(start.after, None);

    let legacy = Cursor {
        after: 7,
        order: None,
    };
    let start = scan_start(None, Some(&legacy), ScanOrder::Descending).expect("legacy cursor");
    assert_eq!(start.order, ScanOrder::Descending);
    assert_eq!(start.after, Some(7));

    let bound = Cursor {
        after: 7,
        order: Some(ScanOrder::Descending),
    };
    assert!(scan_start(Some(ScanOrder::Descending), Some(&bound), default_order).is_ok());
    assert!(matches!(
        scan_start(Some(ScanOrder::Ascending), Some(&bound), default_order),
        Err(StorageError::Request(_))
    ));
    let negative = Cursor {
        after: -1,
        order: None,
    };
    assert!(matches!(
        scan_start(None, Some(&negative), default_order),
        Err(StorageError::Request(_))
    ));
}

#[test]
fn delta_operations_round_trip() {
    let base = json!({"count": 1, "items": ["a", "b"], "text": "hi"});
    let ops = vec![
        Op::Set(vec![Seg::Key("count".to_owned())], json!(2)),
        Op::Append(vec![Seg::Key("text".to_owned())], "!".to_owned()),
        Op::Splice {
            path: vec![Seg::Key("items".to_owned())],
            index: 1,
            remove: 1,
            items: vec![json!("c")],
        },
    ];
    let applied = apply_immutable_batches(&base, vec![ops.clone()]).expect("apply");
    assert_eq!(
        applied,
        json!({"count": 2, "items": ["a", "c"], "text": "hi!"})
    );
    assert_eq!(
        base,
        json!({"count": 1, "items": ["a", "b"], "text": "hi"}),
        "the base is untouched"
    );

    // Wire round trip.
    let encoded = serde_json::to_value(&ops).expect("encode");
    let decoded: Vec<Op> = serde_json::from_value(encoded.clone()).expect("decode");
    assert_eq!(decoded, ops);
    assert_eq!(encoded[0], json!(["s", ["count"], 2]));
}

#[test]
fn delta_rejects_reserved_paths_and_bad_shapes() {
    let reserved: Result<Op, _> = serde_json::from_value(json!(["s", ["__proto__"], 1]));
    assert!(reserved.is_err());
    let unknown: Result<Op, _> = serde_json::from_value(json!(["z", []]));
    assert!(unknown.is_err());
    let bad_permutation: Result<Op, _> = serde_json::from_value(json!(["m", ["a"], [0, 0]]));
    assert!(bad_permutation.is_err());
}

#[test]
fn tokenize_splits_camel_case_and_stems() {
    assert_eq!(
        tokenize("readFile and write_file issues"),
        vec!["read", "file", "write", "file", "issue"]
    );
    assert_eq!(tokenize("HTTPRequest"), vec!["http", "request"]);
    assert!(tokenize("the of and").is_empty());
}

#[test]
fn rank_documents_prefers_matching_names() {
    let documents = [
        SearchDocument {
            name: "read",
            text: "read a file from disk",
        },
        SearchDocument {
            name: "write",
            text: "write content to a file",
        },
        SearchDocument {
            name: "grep",
            text: "search text with a pattern",
        },
    ];
    let ranked = rank_documents("file read", &documents, 2);
    assert_eq!(ranked.first().map(String::as_str), Some("read"));
    assert!(rank_documents("", &documents, 2).is_empty());
    assert!(rank_documents("file", &documents, 0).is_empty());
}

#[test]
fn entry_kind_guards() {
    let record = entry(1, 1, UserEntry::KIND.kind, None);
    assert!(UserEntry::is(Some(&record)));
    assert!(!ResetEntry::is(Some(&record)));
    assert!(!ToolResultEntry::is(None));
    assert!(AssistantEntry::is(Some(&entry(
        2,
        1,
        AssistantEntry::KIND.kind,
        None
    ))));
    assert!(CompactionEntry::is(Some(&entry(
        3,
        1,
        CompactionEntry::KIND.kind,
        None
    ))));
    assert!(SystemEntry::is(Some(&entry(
        4,
        1,
        SystemEntry::KIND.kind,
        None
    ))));
}

#[test]
fn typed_entry_decodes_data() {
    let mut record = entry(1, 1, CompactionEntry::KIND.kind, None);
    record.data = Some(json!({"reason": "threshold"}));
    let typed: TypedEntry<pir::durable::CompactionData> = record.typed().expect("typed data");
    assert_eq!(typed.data.reason, "threshold");
    assert_eq!(typed.record.id.as_i64(), 1);
    assert!(
        record.clone().typed::<u32>().is_none(),
        "mismatched payload"
    );
}

#[test]
fn document_definitions_validate_versions() {
    let definition = DocDefinition::session("demo", 2).expect("valid");
    assert_eq!(definition.version, 2);
    assert!(matches!(
        DocDefinition::session("demo", 0),
        Err(pir::durable::StorageRequestError { .. })
    ));
    let semantics = DocumentSemantics::ConversationLatest {
        fork: DocumentFork::Current,
    };
    assert_eq!(semantics.scope_kind(), "conversation");
    assert_eq!(semantics.history(), Some(DocumentHistory::Latest));
}

#[test]
fn ids_and_sequences_serialize_as_numbers() {
    let entry_id: EntryId = id_from_number(42);
    assert_eq!(serde_json::to_string(&entry_id).expect("id"), "42");
    let seq: Seq = seq_from_number(9);
    assert_eq!(serde_json::to_string(&seq).expect("seq"), "9");
    assert_eq!(Seq::from_raw(1).next(), Seq::from_raw(2));
    assert_eq!(entry_id.next().as_i64(), 43);
    let decoded: EntryId = serde_json::from_str("7").expect("decode");
    assert_eq!(decoded.as_i64(), 7);
}

#[test]
fn record_shapes_use_camel_case_on_the_wire() {
    let mut record = conversation(3);
    record.parent = Some(ConversationParent {
        conversation_id: ConversationId::from_raw(1),
        at: EntryId::from_raw(10),
    });
    let value = serde_json::to_value(&record).expect("serialize");
    assert_eq!(
        value,
        json!({"id": 3, "parent": {"conversationId": 1, "at": 10}})
    );

    // Reads are lenient: an unknown field from a newer producer is ignored.
    let decoded: ConversationRecord =
        serde_json::from_value(json!({"id": 4, "unknownFutureField": true})).expect("lenient read");
    assert_eq!(decoded.id.as_i64(), 4);
    assert_eq!(decoded.parent, None);
}

#[test]
fn page_serializes_its_continuation() {
    let page: Page<i64> = Page {
        items: vec![1, 2],
        next: Some(Cursor {
            after: 2,
            order: Some(ScanOrder::Descending),
        }),
    };
    let value = serde_json::to_value(&page).expect("serialize");
    assert_eq!(
        value,
        json!({"items": [1, 2], "next": {"after": 2, "order": "descending"}})
    );
    let exhausted: Page<i64> = Page {
        items: vec![],
        next: None,
    };
    assert_eq!(
        serde_json::to_value(&exhausted).expect("serialize"),
        json!({"items": []})
    );
}

#[test]
fn document_content_serializes_tagged() {
    let base = DocumentContent::Base {
        version: 1,
        value: serde_json::Map::from_iter([("a".to_owned(), Value::from(1))]),
    };
    assert_eq!(
        serde_json::to_value(&base).expect("serialize"),
        json!({"kind": "base", "version": 1, "value": {"a": 1}})
    );
    let decoded: DocumentContent =
        serde_json::from_value(json!({"kind": "delta", "version": 1, "ops": [["r", {"b": 2}]]}))
            .expect("decode");
    assert_eq!(
        decoded,
        DocumentContent::Delta {
            version: 1,
            ops: vec![Op::Replace(json!({"b": 2}))],
        }
    );
}

#[test]
fn document_commit_change_serializes() {
    let record = pir::durable::DocumentRecord {
        id: pir::durable::DocumentId::from_raw(5),
        kind: "demo".to_owned(),
        key: None,
        created_at: Seq::from_raw(3),
        retired_at: None,
        scope: DocumentScope::Conversation {
            conversation_id: ConversationId::from_raw(1),
        },
        history: Some(DocumentHistory::Rewindable),
        fork: Some(DocumentFork::Current),
    };
    let change = pir::durable::DocumentCommitChange::Document {
        record: record.clone(),
        conversation_id: Some(ConversationId::from_raw(1)),
        version: Some(2),
        value: Some(serde_json::Map::from_iter([("a".to_owned(), json!(1))])),
        ops: vec![Op::Set(vec![Seg::Key("a".to_owned())], json!(1))],
    };
    assert_eq!(
        serde_json::to_value(&change).expect("serialize"),
        json!({
            "type": "document",
            "record": {
                "id": 5,
                "kind": "demo",
                "createdAt": 3,
                "scope": {"kind": "conversation", "conversationId": 1},
                "history": "rewindable",
                "fork": "current"
            },
            "conversationId": 1,
            "version": 2,
            "value": {"a": 1},
            "ops": [["s", ["a"], 1]]
        })
    );

    // A retirement carries `value: null` and no version.
    let retired = pir::durable::DocumentCommitChange::Document {
        record: pir::durable::DocumentRecord {
            retired_at: Some(Seq::from_raw(4)),
            ..record.clone()
        },
        conversation_id: Some(ConversationId::from_raw(1)),
        version: None,
        value: None,
        ops: Vec::new(),
    };
    let value = serde_json::to_value(&retired).expect("serialize");
    assert_eq!(value.get("value"), Some(&Value::Null));
    assert!(value.get("version").is_none());

    let copy = pir::durable::DocumentCommitChange::Copy {
        record: pir::durable::DocumentRecord {
            id: pir::durable::DocumentId::from_raw(6),
            ..serde_json::from_value::<pir::durable::DocumentRecord>(
                serde_json::to_value(&record).expect("round trip"),
            )
            .expect("decode")
        },
        conversation_id: ConversationId::from_raw(2),
        source: pir::durable::DocumentCopySource {
            id: pir::durable::DocumentId::from_raw(5),
            at: DocumentPoint::Current,
        },
    };
    assert_eq!(
        serde_json::to_value(&copy).expect("serialize"),
        json!({
            "type": "document.copy",
            "record": {
                "id": 6,
                "kind": "demo",
                "createdAt": 3,
                "scope": {"kind": "conversation", "conversationId": 1},
                "history": "rewindable",
                "fork": "current"
            },
            "conversationId": 2,
            "source": {"id": 5, "at": "current"}
        })
    );
}

#[test]
fn submission_and_task_states_serialize_tagged() {
    let record = submission(5, 1, None, SubmissionStatus::Placed);
    let value = serde_json::to_value(&record).expect("serialize");
    assert_eq!(
        value,
        json!({
            "id": 5,
            "conversationId": 1,
            "type": "input",
            "status": "placed"
        })
    );
    let state = TaskState::Waiting {
        checkpoint: json!({"phase": "wait"}),
        on: vec![TaskId::from_raw(1)],
        policy: pir::durable::JoinPolicy::FailFast,
    };
    assert_eq!(state.status(), TaskStatus::Waiting);
    assert_eq!(
        serde_json::to_value(&state).expect("serialize"),
        json!({"status": "waiting", "checkpoint": {"phase": "wait"}, "on": [1], "policy": "failFast"})
    );
}
