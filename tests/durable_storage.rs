//! Conformance tests for the durable entries + storage core.
//!
//! The same assertions run against both backends (`MemoryStorage` and
//! `JsonlStorage`) through [`run_conformance`], so a behavior that only one
//! backend has fails the suite. JSONL-specific tests cover reopen, torn-line
//! recovery, corruption detection and the `fsync` path.

use pir::durable::harness::context::derive_context;
use pir::durable::harness::{
    ConversationView, ConversationViews, Harness, HarnessOptions, ViewObserver,
};
use pir::durable::session::transaction::EntryDraft;
use pir::durable::{
    apply_immutable_batches, delay_steps, format_size, id_from_number, rank_documents, scan_start,
    seq_from_number, tokenize, truncate_head, truncate_head_of, AssistantEntry, CompactionEntry,
    Context, ConversationId, ConversationOwner, ConversationParent, ConversationQuery,
    ConversationRecord, Cursor, DocDefinition, DocumentAddress, DocumentContent, DocumentCreate,
    DocumentFork, DocumentHistory, DocumentPoint, DocumentPointQuery, DocumentQuery, DocumentScope,
    DocumentSemantics, EntryId, EntryQuery, EntryRecord, JsonlStorage, JsonlStorageOptions,
    MemoryStorage, Op, Page, ResetEntry, ScanOrder, SearchDocument, Seg, Seq, Storage,
    StorageError, StorageWrite, SubmissionId, SubmissionRecord, SubmissionStatus, SubmissionType,
    SystemEntry, TaskId, TaskQuery, TaskRecord, TaskState, TaskStatus, ToolResultEntry, Totals,
    TruncatedBy, TruncationOptions, TypedEntry, UserEntry, MAX_TIMER_DELAY_MS,
};
use serde_json::{json, Value};
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

// ── test-only allocation counter (R1-L3-008) ────────────────────────────────
//
// The same per-thread counting shape as `tests/backend_copy_audit.rs`:
// allocations are counted only inside a measured section on the measuring
// thread, so the libtest harness and sibling tests on other threads are not
// observed. `#[tokio::test]` uses a current-thread runtime, which keeps the
// recovery future — and therefore its parse/clone work — on this thread.
//
// `LIVE_BYTES`/`PEAK_BYTES` are a heuristic: a value allocated before the
// section (or on a blocking-pool thread, as `tokio::fs::read` does) can be
// freed here without a matching increment, so its `dealloc` subtracts bytes
// this thread never added. `ALLOCATED_BYTES` deliberately never subtracts on
// dealloc, which makes it the authoritative cumulative seam for "did recovery
// read the whole log and copy it around?" assertions: the old whole-file
// reader also allocated one line `String` per line, one parsed marker per line
// and two payload clones on this thread (about 4x the log), while streaming
// recovery parses each marker twice (validation and replay) and copies it into
// `MemoryStorage` (about 3x).
//
// `MAX_ALLOCATION_BYTES` is process-global because the old whole-file `Vec` is
// allocated inside `tokio::fs::read`'s blocking task, where the thread-local
// counters cannot see it. It catches the shape those counters cannot: one
// allocation the size of the entire log.
struct CountingAllocator;

thread_local! {
    static COUNTING: Cell<bool> = const { Cell::new(false) };
    static ALLOCATION_EVENTS: Cell<u64> = const { Cell::new(0) };
    static ALLOCATED_BYTES: Cell<u64> = const { Cell::new(0) };
    /// Bytes currently live from allocations seen since the section opened.
    static LIVE_BYTES: Cell<u64> = const { Cell::new(0) };
    /// High-water mark of [`LIVE_BYTES`], i.e. this thread's allocation peak.
    static PEAK_BYTES: Cell<u64> = const { Cell::new(0) };
}

/// Set while a measured section runs. Unlike [`COUNTING`], this is global so
/// allocations made by blocking-pool threads (where `tokio::fs::read` builds
/// its whole-file `Vec`) are observed too.
static MEASURING: AtomicBool = AtomicBool::new(false);
/// Largest single allocation requested by any thread while [`MEASURING`] was
/// set. The old whole-file reader allocated the entire log as one `Vec` on the
/// blocking pool, which the per-thread live counters cannot see; this seam
/// catches that shape regardless of which thread frees the buffer.
static MAX_ALLOCATION_BYTES: AtomicU64 = AtomicU64::new(0);
/// Serializes measured sections: [`MEASURING`] and [`MAX_ALLOCATION_BYTES`] are
/// process-global, so two allocation tests running in parallel would otherwise
/// attribute each other's fixture allocations to this section. The lock is
/// async so it can be held across the measured future without a blocking
/// mutex guard tripping `await_holding_lock`.
static MEASURE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[expect(
    unsafe_code,
    reason = "test-only GlobalAlloc wrapper: delegates straight to System and \
              adds per-thread counters; the TLS slots are const-initialized so \
              reading them can never allocate (no recursion into the allocator)"
)]
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if MEASURING.load(Ordering::Relaxed) {
            MAX_ALLOCATION_BYTES.fetch_max(layout.size() as u64, Ordering::Relaxed);
        }
        let _ = COUNTING.try_with(|counting| {
            if counting.get() {
                let _ = ALLOCATION_EVENTS.try_with(|events| events.set(events.get() + 1));
                let _ =
                    ALLOCATED_BYTES.try_with(|bytes| bytes.set(bytes.get() + layout.size() as u64));
                let _ = LIVE_BYTES.try_with(|live| {
                    let next = live.get().saturating_add(layout.size() as u64);
                    live.set(next);
                    let _ = PEAK_BYTES.try_with(|peak| peak.set(peak.get().max(next)));
                });
            }
        });
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let _ = COUNTING.try_with(|counting| {
            if counting.get() {
                // A value allocated before the section can be freed inside it;
                // saturating subtraction keeps the live estimate from wrapping.
                let _ = LIVE_BYTES
                    .try_with(|live| live.set(live.get().saturating_sub(layout.size() as u64)));
            }
        });
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AllocStats {
    events: u64,
    /// Cumulative bytes allocated on the measuring thread while the section
    /// ran. Frees never subtract from this total, so a blocking-thread buffer
    /// (e.g. `tokio::fs::read`'s whole-file `Vec`) being dropped back on this
    /// thread cannot cancel the copies made here.
    bytes: u64,
    /// Highest live-byte total observed while the section ran.
    peak_bytes: u64,
    /// Largest single allocation requested by any thread while the section
    /// ran. Unlike the live counters this sees blocking-pool allocations, so
    /// the old whole-file `Vec` shows up here even though its matching
    /// `dealloc` happens on the measuring thread.
    max_allocation_bytes: u64,
}

/// Clears the thread-local and global measurement state even if the measured
/// future panics, so one failed test cannot leave every later allocation in
/// the process attributed to `MAX_ALLOCATION_BYTES`.
struct MeasurementGuard;

impl Drop for MeasurementGuard {
    fn drop(&mut self) {
        let _ = COUNTING.try_with(|counting| counting.set(false));
        MEASURING.store(false, Ordering::SeqCst);
        MAX_ALLOCATION_BYTES.store(0, Ordering::Relaxed);
    }
}

/// Holds [`MEASURE_LOCK`] from before a large fixture is built until its
/// measured section finishes. The lock alone only serializes sections; a
/// fixture built outside the lock could still allocate its multi-megabyte
/// buffers while another test's [`MEASURING`] flag is set, and the global
/// [`MAX_ALLOCATION_BYTES`] seam would attribute that allocation to the wrong
/// test.
struct MeasureSection {
    _section: tokio::sync::MutexGuard<'static, ()>,
}

impl MeasureSection {
    /// Acquires the process-global measurement slot and resets the counters.
    /// Hold the returned value while building a fixture, then pass it to
    /// [`Self::measure`] so the fixture's allocations are never observed by a
    /// concurrent measurement.
    async fn begin() -> Self {
        let section = MEASURE_LOCK.lock().await;
        ALLOCATION_EVENTS.with(|events| events.set(0));
        ALLOCATED_BYTES.with(|bytes| bytes.set(0));
        LIVE_BYTES.with(|live| live.set(0));
        PEAK_BYTES.with(|peak| peak.set(0));
        MAX_ALLOCATION_BYTES.store(0, Ordering::Relaxed);
        Self { _section: section }
    }

    /// Runs one measured section while keeping the fixture slot held.
    async fn measure<T>(self, future: impl Future<Output = T>) -> (T, AllocStats) {
        COUNTING.with(|counting| counting.set(true));
        MEASURING.store(true, Ordering::SeqCst);
        let guard = MeasurementGuard;
        let value = future.await;
        let stats = AllocStats {
            events: ALLOCATION_EVENTS.with(Cell::get),
            bytes: ALLOCATED_BYTES.with(Cell::get),
            peak_bytes: PEAK_BYTES.with(Cell::get),
            max_allocation_bytes: MAX_ALLOCATION_BYTES.load(Ordering::Relaxed),
        };
        drop(guard);
        (value, stats)
    }
}

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

/// Six hundred entries at `limit = 256` must scan in exactly three pages in
/// both directions: each cursor continues where the previous page ended and no
/// entry is returned twice.
async fn assert_three_page_scan(storage: &dyn Storage) {
    let mut writes = vec![StorageWrite::Conversation {
        value: conversation(1),
    }];
    for id in 1..=600 {
        writes.push(StorageWrite::Entry {
            value: entry(1_000 + id, 1, UserEntry::KIND.kind, None),
        });
    }
    storage.commit(writes).await.expect("seed 600 entries");

    let cases: [(ScanOrder, Vec<i64>); 2] = [
        (ScanOrder::Descending, (1_001..=1_600).rev().collect()),
        (ScanOrder::Ascending, (1_001..=1_600).collect()),
    ];
    for (order, expected) in cases {
        let mut cursor: Option<Cursor> = None;
        let mut seen = Vec::new();
        let mut pages = 0;
        loop {
            let page = storage
                .scan_entries(
                    EntryQuery {
                        conversation_id: ConversationId::from_raw(1),
                        min_entry_id: None,
                        max_entry_id: None,
                        order: Some(order),
                    },
                    256,
                    cursor.clone(),
                )
                .await
                .expect("paged scan");
            pages += 1;
            assert!(
                page.items.len() <= 256,
                "a page never exceeds the requested limit"
            );
            seen.extend(page.items.iter().map(|item| item.id.as_i64()));
            match page.next {
                Some(next) => {
                    assert_eq!(next.order, Some(order), "the cursor keeps its order");
                    cursor = Some(next);
                }
                None => break,
            }
            assert!(pages <= 3, "600 entries at limit 256 must fit in 3 pages");
        }
        assert_eq!(pages, 3, "600 entries at limit 256 need exactly 3 pages");
        assert_eq!(seen, expected, "pages continue the scan in order");
        let mut unique = seen.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), 600, "no entry is returned twice");
    }
}

#[tokio::test]
async fn memory_scan_pages_both_orders_across_three_pages() {
    let storage = MemoryStorage::new();
    assert_three_page_scan(&storage).await;
    storage.close().await.expect("close");
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
async fn jsonl_scan_pages_both_orders_across_three_pages() {
    let dir = temp_dir("paging");
    let storage = JsonlStorage::open(&dir).await.expect("open");
    assert_three_page_scan(&storage).await;
    storage.close().await.expect("close");
    remove_dir(&dir).await;
}

#[tokio::test]
async fn jsonl_reclaim_failure_removes_the_temporary_file() {
    let dir = temp_dir("reclaim-failure");
    tokio::fs::create_dir_all(&dir).await.expect("create dir");

    // A directory at the rename target makes `rename(temporary, target)`
    // fail after the temporary `.reclaim` file was written and synced.
    let blocked = dir.join("doc-1.jsonl");
    tokio::fs::create_dir_all(&blocked)
        .await
        .expect("create blocking directory");
    for fsync in [false, true] {
        pir::durable::storage::jsonl::replace_sidecar_file(
            &dir,
            fsync,
            "doc-1.jsonl",
            "{\"format\":1}\n",
        )
        .await;
        assert!(
            !dir.join("doc-1.jsonl.reclaim").exists(),
            "a failed rewrite must not leave its temporary file behind (fsync={fsync})"
        );
    }
    assert!(
        blocked.is_dir(),
        "the failed rename leaves the target alone"
    );

    // The successful path still replaces the file atomically.
    let target = dir.join("doc-2.jsonl");
    std::fs::write(&target, "old\n").expect("seed target");
    pir::durable::storage::jsonl::replace_sidecar_file(&dir, true, "doc-2.jsonl", "new\n").await;
    assert_eq!(
        std::fs::read_to_string(&target).expect("read target"),
        "new\n"
    );
    assert!(!dir.join("doc-2.jsonl.reclaim").exists());

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

// ── R1-L3-003: context derivation copies ───────────────────────────────────

#[test]
fn derive_context_keeps_one_message_copy_per_request() {
    const COUNT: usize = 48;
    const PAYLOAD: usize = 32 * 1024;

    // A large-but-cheap fixture: the old implementation cloned every one of
    // these message strings twice on the way to `ContextView`, even though no
    // compaction path ran.
    let entries: Vec<EntryRecord> = (0..COUNT)
        .map(|index| {
            let mut record = entry(index as i64 + 1, 1, "pi.user", None);
            record.model = Some(vec![json!({
                "role": "user",
                "content": "x".repeat(PAYLOAD),
                "timestamp": index as i64 + 1,
            })]);
            record
        })
        .collect();

    let view = derive_context(&entries, None);

    assert!(
        !view.messages.is_materialized(),
        "an ordinary derive must not parse the ordered message list"
    );
    assert!(
        view.messages.shares_entries_with(&view),
        "the lazy message list must index the view's entries instead of owning a second transcript"
    );
    let clone = view.clone();
    assert!(
        clone.messages.shares_messages_with(&view.messages),
        "cloning a view must share the message list instead of deep-copying every message"
    );
    assert_eq!(view.messages.len(), COUNT);
    assert!(view.messages.is_materialized());
    assert_eq!(
        view.contributions.len(),
        COUNT,
        "the light index still covers every active entry"
    );
    assert!(
        view.contributions.shares_entries_with(&view),
        "contributions must index the view's entries instead of owning a second transcript"
    );
    assert!(
        !view.contributions.is_materialized(),
        "an ordinary derive must not build the per-entry message copy"
    );

    // Reading the contributions still yields the parsed messages; this access
    // is the lazy materialization the compaction path pays for.
    assert_eq!(view.contributions[0].len(), 1);
    assert!(view.contributions.is_materialized());
}

// ── R1-L3-004: mounted-view entry appends ──────────────────────────────────

struct RevisionRecorder {
    initial: Mutex<Option<ConversationView>>,
    advances: Mutex<Vec<ConversationView>>,
}

impl ViewObserver for RevisionRecorder {
    fn advance(&self, value: &ConversationView, _ops: &[Op], _context: &Context) {
        self.advances
            .lock()
            .expect("advances lock")
            .push(value.clone());
    }

    fn close_session(&self, _failure: Option<&StorageError>) {}
}

fn draft(text: &str) -> EntryDraft {
    EntryDraft {
        kind: "app.user".to_owned(),
        model: Some(vec![json!({
            "role": "user",
            "content": text,
            "timestamp": 1,
        })]),
        data: Some(json!({ "text": text })),
        head: None,
        edits: None,
    }
}

#[tokio::test]
async fn view_append_shares_every_historical_entry() {
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
    let harness = Harness::open(storage, HarnessOptions::default(), Context::background())
        .await
        .expect("open harness");
    let conversation = harness.root(Context::background()).await.expect("root");

    for index in 0..8 {
        harness
            .session()
            .append_entry(
                conversation.id(),
                draft(&format!("entry {index}")),
                Context::background(),
            )
            .await
            .expect("append entry");
    }

    let views = ConversationViews::open(harness.session().clone())
        .await
        .expect("open views");
    let attached = views
        .attach::<RevisionRecorder, _>(
            conversation.id(),
            |view| RevisionRecorder {
                initial: Mutex::new(Some(view.clone())),
                advances: Mutex::new(Vec::new()),
            },
            &Context::background(),
        )
        .await
        .expect("attach recorder");

    harness
        .session()
        .append_entry(conversation.id(), draft("appended"), Context::background())
        .await
        .expect("append next entry");

    let initial = attached
        .observer
        .initial
        .lock()
        .expect("initial lock")
        .clone()
        .expect("factory ran");
    let after = attached
        .observer
        .advances
        .lock()
        .expect("advances lock")
        .first()
        .cloned()
        .expect("one advance");

    assert_eq!(after.entries().len(), initial.entries().len() + 1);
    assert!(
        !Arc::ptr_eq(&initial.entries, &after.entries),
        "the append creates a new entry-list revision"
    );
    for index in 0..initial.entries().len() {
        assert!(
            std::ptr::eq(
                initial.entries().get(index).expect("old entry"),
                after.entries().get(index).expect("shared entry"),
            ),
            "entry {index} was deep-copied by the append"
        );
    }
    assert_eq!(after.entries()[initial.entries().len()].kind, "app.user");

    attached.detach.detach();
}

// ── R1-L3-002: paged scans materialize only the current page ───────────────

/// A two-segment fork chain (root conversation 1 → fork conversation 2) with
/// 400 entries per segment. A full page must materialize exactly `limit + 1`
/// `EntryRecord`s; the old shape deep-copied every remaining visible entry on
/// every page (O(n²) clones), so the first page here would materialize 800.
#[tokio::test]
async fn memory_scan_entries_materializes_only_the_current_page() {
    const SEGMENT: i64 = 400;
    const LIMIT: usize = 256;
    const ROOT_FIRST: i64 = 10;
    const FORK_FIRST: i64 = 1_000;

    let storage = MemoryStorage::new();
    let mut writes = vec![
        StorageWrite::Conversation {
            value: conversation(1),
        },
        StorageWrite::Conversation {
            value: ConversationRecord {
                id: ConversationId::from_raw(2),
                parent: Some(ConversationParent {
                    conversation_id: ConversationId::from_raw(1),
                    at: EntryId::from_raw(ROOT_FIRST + SEGMENT - 1),
                }),
                owner: None,
            },
        },
    ];
    for offset in 0..SEGMENT {
        writes.push(StorageWrite::Entry {
            value: entry(ROOT_FIRST + offset, 1, UserEntry::KIND.kind, None),
        });
    }
    for offset in 0..SEGMENT {
        writes.push(StorageWrite::Entry {
            value: entry(FORK_FIRST + offset, 2, UserEntry::KIND.kind, None),
        });
    }
    storage.commit(writes).await.expect("seed fork chain");

    for order in [ScanOrder::Descending, ScanOrder::Ascending] {
        let mut cursor: Option<Cursor> = None;
        let mut seen = Vec::new();
        let mut pages = 0usize;
        loop {
            storage.reset_entry_scan_materialized();
            let page = storage
                .scan_entries(
                    EntryQuery {
                        conversation_id: ConversationId::from_raw(2),
                        min_entry_id: None,
                        max_entry_id: None,
                        order: Some(order),
                    },
                    LIMIT,
                    cursor.clone(),
                )
                .await
                .expect("paged scan");
            let materialized = storage.entry_scan_materialized();
            // The over-fetch window holds `items + (next ? 1 : 0)` records:
            // a full page is limit + 1, the final page just its items.
            let expected = page.items.len() + usize::from(page.next.is_some());
            assert_eq!(
                materialized, expected,
                "{order:?} page {pages}: materialized {materialized} records, expected {expected}; \
                 the old shape copied the whole remaining chain"
            );
            assert!(
                materialized <= LIMIT + 1,
                "{order:?} page {pages}: {materialized} records materialized, over limit + 1"
            );
            seen.extend(page.items.iter().map(|entry| entry.id.as_i64()));
            pages += 1;
            match page.next {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        assert_eq!(pages, 4, "{order:?}: 800 visible entries span 4 pages");
        assert_eq!(
            seen.len(),
            (2 * SEGMENT) as usize,
            "{order:?}: every visible entry is returned once"
        );
        let mut unique = seen.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), seen.len(), "{order:?}: duplicate ids");
    }
}

// ── R1-L3-007: fsync also syncs the parent directory ──────────────────────

/// `fsync: true` must sync the parent directory after creating/appending a
/// sidecar and after the main marker append. The seam counts the calls, so
/// deleting the `sync_parent_dir` call sites fails this test (on every
/// platform — the directory handle itself is Unix-only).
#[tokio::test]
async fn jsonl_fsync_syncs_the_parent_directory_for_new_files() {
    use pir::durable::storage::jsonl::{parent_dir_sync_calls, reset_parent_dir_sync_calls};

    let dir = temp_dir("fsync-parent");
    let storage = JsonlStorage::open_with_options(&dir, JsonlStorageOptions { fsync: true })
        .await
        .expect("open");

    reset_parent_dir_sync_calls();
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
        .expect("first commit creates main.jsonl");
    assert_eq!(
        parent_dir_sync_calls(),
        1,
        "creating main.jsonl must sync its parent directory exactly once"
    );

    reset_parent_dir_sync_calls();
    storage
        .commit(vec![StorageWrite::DocumentCreate {
            record: document_create(
                70,
                "fsync.parent.doc",
                DocumentHistory::Latest,
                DocumentFork::Current,
            ),
            content: DocumentContent::Base {
                version: 1,
                value: serde_json::Map::from_iter([("n".to_owned(), json!(1))]),
            },
        }])
        .await
        .expect("second commit creates a sidecar");
    assert_eq!(
        parent_dir_sync_calls(),
        2,
        "a new sidecar and the main marker must each sync the parent directory"
    );

    storage.close().await.expect("close");
    remove_dir(&dir).await;
}

// ── R1-L3-008: streaming JSONL recovery keeps one sidecar copy ─────────────

/// The recovery rewrite of a current-only document's sidecar keeps only the
/// newest base. This is the `SidecarReclamation::KeepFrom` path: the stale
/// base line is dropped and the retained suffix is streamed into the rewrite.
#[tokio::test]
async fn jsonl_recovery_reclaims_a_replaced_current_only_base() {
    let dir = temp_dir("recent-base");
    let document_id = pir::durable::DocumentId::from_raw(80);
    {
        let storage = JsonlStorage::open(&dir).await.expect("open");
        storage
            .commit(vec![StorageWrite::DocumentCreate {
                record: document_create(
                    80,
                    "latest.doc",
                    DocumentHistory::Latest,
                    DocumentFork::Current,
                ),
                content: DocumentContent::Base {
                    version: 1,
                    value: serde_json::Map::from_iter([("v".to_owned(), json!(1))]),
                },
            }])
            .await
            .expect("create");
        storage
            .commit(vec![StorageWrite::DocumentChange {
                id: document_id,
                content: DocumentContent::Base {
                    version: 2,
                    value: serde_json::Map::from_iter([("v".to_owned(), json!(2))]),
                },
            }])
            .await
            .expect("replace base");
        storage.close().await.expect("close");
    }

    // Commit-time reclamation already dropped the old base. Put it back as a
    // crash between the marker append and reclamation would leave it, so the
    // reopen has to reclaim the stale line itself.
    let sidecar = dir.join("doc-80.jsonl");
    let latest = std::fs::read_to_string(&sidecar).expect("read sidecar");
    let stale = "{\"format\":1,\"type\":\"record\",\"seq\":1,\"ordinal\":0,\
                 \"payload\":{\"type\":\"document\",\"id\":80,\
                 \"content\":{\"kind\":\"base\",\"version\":1,\"value\":{\"v\":1}}}}\n";
    std::fs::write(&sidecar, format!("{stale}{latest}")).expect("restore stale base");

    let storage = JsonlStorage::open(&dir).await.expect("reopen");
    let document = storage
        .document(document_id, DocumentPoint::Current)
        .await
        .expect("document")
        .expect("present");
    assert_eq!(document.value.get("v"), Some(&json!(2)));
    storage.close().await.expect("close");

    let raw = std::fs::read_to_string(&sidecar).expect("read reclaimed sidecar");
    assert_eq!(raw.lines().count(), 1, "the stale base is reclaimed: {raw}");
    assert!(raw.contains("\"seq\":2"), "the newest base survives: {raw}");
    remove_dir(&dir).await;
}

/// Recovery truncates a complete but unconfirmed sidecar tail at the exact
/// byte offset of its first line; the confirmed prefix still replays.
#[tokio::test]
async fn jsonl_recovery_truncates_an_unconfirmed_sidecar_tail() {
    let dir = temp_dir("unconfirmed-tail");
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
            .expect("seed");
        storage.close().await.expect("close");
    }

    let sidecar = dir.join("task-20.jsonl");
    let mut raw = std::fs::read_to_string(&sidecar).expect("read sidecar");
    raw.push_str(
        "{\"format\":1,\"type\":\"record\",\"seq\":2,\"ordinal\":0,\
         \"payload\":{\"type\":\"document\",\"id\":999,\
         \"content\":{\"kind\":\"base\",\"version\":1,\"value\":{}}}}\n",
    );
    std::fs::write(&sidecar, raw).expect("append unconfirmed line");

    let storage = JsonlStorage::open(&dir).await.expect("reopen");
    let live = storage
        .task(TaskId::from_raw(20))
        .await
        .expect("task")
        .expect("present");
    assert_eq!(live.status(), TaskStatus::Running);
    storage.close().await.expect("close");

    let raw = std::fs::read_to_string(&sidecar).expect("read truncated sidecar");
    assert_eq!(
        raw.lines().count(),
        1,
        "the unconfirmed tail is gone: {raw}"
    );
    remove_dir(&dir).await;
}

/// A torn trailing sidecar line (no newline) is truncated before parsing, just
/// like a torn main marker.
#[tokio::test]
async fn jsonl_recovery_truncates_a_torn_sidecar_tail() {
    let dir = temp_dir("torn-sidecar");
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
            .expect("seed");
        storage.close().await.expect("close");
    }

    let sidecar = dir.join("task-20.jsonl");
    let mut raw = std::fs::read_to_string(&sidecar).expect("read sidecar");
    raw.push_str("{\"format\":1,\"type\":\"record\",\"seq\":2,\"ordinal\":0,\"pay");
    std::fs::write(&sidecar, raw).expect("append torn line");

    let storage = JsonlStorage::open(&dir).await.expect("reopen");
    assert!(storage
        .task(TaskId::from_raw(20))
        .await
        .expect("task")
        .is_some());
    storage.close().await.expect("close");

    let raw = std::fs::read_to_string(&sidecar).expect("read truncated sidecar");
    assert_eq!(raw.lines().count(), 1, "the torn tail is truncated: {raw}");
    assert!(raw.ends_with('\n'));
    remove_dir(&dir).await;
}

/// One `SidecarRecord` copy per line: parsing wraps the record in an `Arc` and
/// replay moves it out. The old implementation deep-cloned every record into
/// `previous`, `record_by_key` and `parsed_files` (plus a clone in
/// `confirm_record`), so this counter would be at least three per record.
#[tokio::test]
async fn jsonl_recovery_keeps_a_single_sidecar_record_copy() {
    use pir::durable::storage::jsonl::{reset_sidecar_record_clones, sidecar_record_clones};

    const RECORDS: usize = 64;
    const PAYLOAD: usize = 4 * 1024;
    let dir = temp_dir("recovery-clones");
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
            .expect("seed");
        for index in 0..RECORDS {
            let mut value = task(20, 1, TaskStatus::Running);
            value.state = TaskState::Running {
                checkpoint: json!({"n": index, "blob": "x".repeat(PAYLOAD)}),
            };
            storage
                .commit(vec![StorageWrite::Task { value }])
                .await
                .expect("task transition");
        }
        storage.close().await.expect("close");
    }

    reset_sidecar_record_clones();
    let storage = JsonlStorage::open(&dir).await.expect("reopen");
    let clones = sidecar_record_clones();
    assert_eq!(
        clones, 0,
        "recovery deep-cloned {clones} sidecar records; each record must be \
         indexed once and moved into the replayed write"
    );
    let live = storage
        .task(TaskId::from_raw(20))
        .await
        .expect("task")
        .expect("present");
    assert_eq!(live.status(), TaskStatus::Running);
    storage.close().await.expect("close");
    remove_dir(&dir).await;
}

/// Write the legacy (pre-R1-L3-009) shape of a live-task sidecar: `records`
/// confirmed full snapshots, one main marker each. The storage-side compaction
/// no longer produces such files, but directories written by an older build,
/// and a crash between a marker append and its compaction rewrite, can contain
/// them; recovery must still replay them and retain only the newest record.
fn write_legacy_live_task_sidecar(dir: &Path, task_id: i64, records: usize, payload: usize) -> u64 {
    std::fs::create_dir_all(dir).expect("create fixture directory");
    let mut sidecar = String::new();
    let mut main = String::new();
    for index in 0..records {
        let seq = index as i64 + 1;
        let mut value = task(task_id, 1, TaskStatus::Running);
        value.state = TaskState::Running {
            checkpoint: json!({"n": index, "blob": "x".repeat(payload)}),
        };
        let record = json!({
            "format": 1,
            "type": "record",
            "seq": seq,
            "ordinal": 0,
            "payload": {"type": "task", "value": value},
        });
        sidecar.push_str(&serde_json::to_string(&record).expect("serialize sidecar record"));
        sidecar.push('\n');
        let marker = json!({
            "format": 1,
            "type": "commit",
            "seq": seq,
            "writes": [{"type": "task.sidecar", "id": task_id, "ordinal": 0}],
        });
        main.push_str(&serde_json::to_string(&marker).expect("serialize marker"));
        main.push('\n');
    }
    std::fs::write(
        dir.join(format!("task-{task_id}.jsonl")),
        sidecar.as_bytes(),
    )
    .expect("write sidecar");
    std::fs::write(dir.join("main.jsonl"), main.as_bytes()).expect("write main");
    std::fs::metadata(dir.join(format!("task-{task_id}.jsonl")))
        .expect("sidecar metadata")
        .len()
}

/// A 16 MiB legacy live-task sidecar recovers correctly through the streaming
/// reader and is compacted to its newest record. Ignored by default so the
/// ordinary suite stays quick.
#[tokio::test]
#[ignore = "16 MiB recovery fixture; run explicitly with --ignored"]
async fn jsonl_recovers_a_large_task_sidecar_session() {
    const RECORDS: usize = 64;
    const PAYLOAD: usize = 256 * 1024;
    // This fixture is also multi-megabyte and this test does not measure its
    // own recovery, so hold the measurement slot to keep both the fixture and
    // the unmeasured recovery from polluting a concurrent measured section.
    let _section = MeasureSection::begin().await;
    let dir = temp_dir("large-recovery");
    let sidecar = dir.join("task-20.jsonl");
    let bytes = write_legacy_live_task_sidecar(&dir, 20, RECORDS, PAYLOAD);
    assert!(
        bytes >= (RECORDS * PAYLOAD) as u64,
        "fixture must be multi-megabyte, got {bytes} bytes"
    );

    let storage = JsonlStorage::open(&dir)
        .await
        .expect("reopen large session");
    let live = storage
        .task(TaskId::from_raw(20))
        .await
        .expect("task")
        .expect("present");
    assert_eq!(live.status(), TaskStatus::Running);
    let TaskState::Running { checkpoint } = &live.state else {
        panic!("unexpected final task state: {:?}", live.state);
    };
    assert_eq!(checkpoint.get("n"), Some(&json!(RECORDS - 1)));
    assert_eq!(
        checkpoint.get("blob").and_then(Value::as_str).map(str::len),
        Some(PAYLOAD)
    );
    storage.close().await.expect("close");

    let raw = std::fs::read_to_string(&sidecar).expect("read reclaimed sidecar");
    assert_eq!(
        raw.lines().count(),
        1,
        "recovery compacts the legacy sidecar to its newest record"
    );
    assert!(raw.contains(&format!("\"n\":{}", RECORDS - 1)));
    remove_dir(&dir).await;
}

/// Allocation limits for reopening an entry-only main log, expressed as
/// percentages of the log's byte size.
///
/// The single-allocation cap is the differential against the old whole-file
/// reader: `tokio::fs::read` materialized the entire log as one `Vec` (100%).
/// The cumulative and peak caps are stability guards for the streaming
/// implementation, calibrated from an actual run rather than estimated: one
/// 4 KiB payload per marker costs about 5.05x cumulatively (two serde parses
/// plus the `MemoryStorage` copy) and peaks at about 1.19x. They leave room
/// for allocator/serde variation while still catching a streaming rewrite
/// that retains a second copy of every line.
#[derive(Debug, Clone, Copy)]
struct EntryLogBudget {
    /// Cap on cumulative bytes allocated while reopening (frees never subtract
    /// from this total). Measured streaming shape: ~5.05x of the log.
    total_percent: u64,
    /// Cap on the largest single allocation. Streaming recovery never buffers
    /// more than one line; the old `tokio::fs::read` allocated the whole log
    /// as one `Vec` on a blocking-pool thread (100%).
    single_percent: u64,
    /// Cap on the live high-water mark. Measured streaming shape: ~1.19x.
    peak_percent: u64,
}

/// The default-gate fixture runs beside the rest of the parallel suite. The
/// global single-allocation seam is widened to tolerate an unrelated sibling
/// test's buffers, while still staying far below the old whole-file `Vec`.
const MODERATE_ENTRY_LOG_BUDGET: EntryLogBudget = EntryLogBudget {
    total_percent: 600,
    single_percent: 50,
    peak_percent: 135,
};

/// The 16 MiB fixture is run explicitly, normally with `--test-threads=1`.
const LARGE_ENTRY_LOG_BUDGET: EntryLogBudget = EntryLogBudget {
    total_percent: 600,
    single_percent: 25,
    peak_percent: 135,
};

/// Checks the allocation budget of reopening an entry-only main log.
///
/// * `stats.max_allocation_bytes` is the direct detector for the old
///   `tokio::fs::read` shape: that reader materialized the whole log in one
///   blocking-pool `Vec`, so its largest single allocation was 100% of
///   `main_bytes`. The streaming reader holds one line (a few KiB).
/// * `stats.bytes` is the cumulative stability guard. It never subtracts on
///   `dealloc`, so it measures every per-marker parse/clone the streaming
///   implementation performs. The old reader is caught by the single-allocation
///   cap above; this cap catches a streaming implementation that stops moving
///   payloads and starts copying them again.
/// * `stats.peak_bytes` guards the streaming live set: it must stay near one
///   copy of the log. Thread-local live accounting under-counts a blocking
///   whole-file read (its `dealloc` subtracts a buffer this thread never
///   added), so the two assertions above are the required signals.
fn assert_entry_log_allocation_budget(stats: &AllocStats, main_bytes: u64, budget: EntryLogBudget) {
    assert!(
        stats.max_allocation_bytes > 0,
        "the global allocation seam observed no allocation; the measurement is broken"
    );
    assert!(
        stats.max_allocation_bytes <= main_bytes * budget.single_percent / 100,
        "recovery's largest single allocation was {} bytes for a {} byte main log \
         ({} allocation events); a streaming reader only buffers one line, while \
         the old whole-file reader allocated the entire log in one Vec",
        stats.max_allocation_bytes,
        main_bytes,
        stats.events
    );
    assert!(
        stats.peak_bytes <= main_bytes * budget.peak_percent / 100,
        "recovery peaked at {} live bytes for a {} byte main log ({} allocation \
         events); streaming replay holds one copy of the state, while the old \
         whole-file read kept the text and every parsed marker alive at once",
        stats.peak_bytes,
        main_bytes,
        stats.events
    );
    assert!(
        stats.bytes <= main_bytes * budget.total_percent / 100,
        "recovery allocated {} bytes in total for a {} byte main log ({} allocation \
         events); streaming replay was measured at about 5x (two validation/replay \
         parses plus the MemoryStorage copy), so this cap only catches a rewrite \
         that reintroduces per-line copies",
        stats.bytes,
        main_bytes,
        stats.events
    );
}

/// Default-gate sized version of the 16 MiB entry-log test: the same
/// streaming allocation budget on a ~4 MiB log, so an ordinary
/// `cargo test --test durable_storage` run exercises the contract. The fixture
/// uses one entry per marker, exactly like the 16 MiB test, so both cases
/// exercise the same per-line reading shape and can share the same budget.
#[tokio::test]
async fn jsonl_recovers_a_moderate_entry_log_without_holding_markers() {
    const ENTRIES: i64 = 1_024;
    const PAYLOAD: usize = 4 * 1024;
    let section = MeasureSection::begin().await;
    let dir = temp_dir("moderate-main-log");
    {
        let storage = JsonlStorage::open(&dir).await.expect("open");
        storage
            .commit(vec![StorageWrite::Conversation {
                value: conversation(1),
            }])
            .await
            .expect("seed conversation");
        for index in 0..ENTRIES {
            let mut value = entry(1_000 + index, 1, UserEntry::KIND.kind, None);
            value.model = Some(vec![json!({
                "role": "user",
                "content": "x".repeat(PAYLOAD),
            })]);
            storage
                .commit(vec![StorageWrite::Entry { value }])
                .await
                .expect("append entry");
        }
        storage.close().await.expect("close");
    }

    let main_bytes = std::fs::metadata(dir.join("main.jsonl"))
        .expect("main log metadata")
        .len();
    assert!(
        main_bytes >= 4 * 1024 * 1024,
        "fixture too small: {main_bytes} bytes"
    );

    let (result, stats) = section.measure(JsonlStorage::open(&dir)).await;
    let storage = result.expect("reopen moderate log");
    assert_entry_log_allocation_budget(&stats, main_bytes, MODERATE_ENTRY_LOG_BUDGET);
    let page = storage
        .scan_entries(
            EntryQuery {
                conversation_id: ConversationId::from_raw(1),
                min_entry_id: None,
                max_entry_id: None,
                order: Some(ScanOrder::Descending),
            },
            8,
            None,
        )
        .await
        .expect("scan recovered entries");
    assert_eq!(page.items.len(), 8);
    assert_eq!(page.items[0].id.as_i64(), 1_000 + ENTRIES - 1);
    assert_eq!(
        page.items[0]
            .model
            .as_ref()
            .and_then(|model| model.first())
            .and_then(|message| message.get("content"))
            .and_then(Value::as_str)
            .map(str::len),
        Some(PAYLOAD)
    );
    storage.close().await.expect("close");
    remove_dir(&dir).await;
}

/// A 16 MiB main log — one marker per entry — replays through the streaming
/// marker pass without retaining the file text or every parsed marker.
/// Ignored by default so the ordinary suite stays quick.
#[tokio::test]
#[ignore = "16 MiB recovery fixture; run explicitly with --ignored"]
async fn jsonl_recovers_a_large_entry_log_without_holding_markers() {
    const ENTRIES: i64 = 4_096;
    const PAYLOAD: usize = 4 * 1024;
    let section = MeasureSection::begin().await;
    let dir = temp_dir("large-main-log");
    {
        let storage = JsonlStorage::open(&dir).await.expect("open");
        storage
            .commit(vec![StorageWrite::Conversation {
                value: conversation(1),
            }])
            .await
            .expect("seed conversation");
        for index in 0..ENTRIES {
            let mut value = entry(1_000 + index, 1, UserEntry::KIND.kind, None);
            value.model = Some(vec![json!({
                "role": "user",
                "content": "x".repeat(PAYLOAD),
            })]);
            storage
                .commit(vec![StorageWrite::Entry { value }])
                .await
                .expect("append entry");
        }
        storage.close().await.expect("close");
    }

    let main_bytes = std::fs::metadata(dir.join("main.jsonl"))
        .expect("main log metadata")
        .len();
    assert!(main_bytes >= 16 * 1024 * 1024, "fixture too small");

    let (result, stats) = section.measure(JsonlStorage::open(&dir)).await;
    let storage = result.expect("reopen large log");
    eprintln!(
        "MEASURE large main_bytes={main_bytes} events={} bytes={} ({:.3}x) peak={} ({:.3}x) max={} ({:.3}%)",
        stats.events,
        stats.bytes,
        stats.bytes as f64 / main_bytes as f64,
        stats.peak_bytes,
        stats.peak_bytes as f64 / main_bytes as f64,
        stats.max_allocation_bytes,
        100.0 * stats.max_allocation_bytes as f64 / main_bytes as f64,
    );
    assert_entry_log_allocation_budget(&stats, main_bytes, LARGE_ENTRY_LOG_BUDGET);
    let page = storage
        .scan_entries(
            EntryQuery {
                conversation_id: ConversationId::from_raw(1),
                min_entry_id: None,
                max_entry_id: None,
                order: Some(ScanOrder::Descending),
            },
            8,
            None,
        )
        .await
        .expect("scan recovered entries");
    assert_eq!(page.items.len(), 8);
    assert_eq!(page.items[0].id.as_i64(), 1_000 + ENTRIES - 1);
    assert_eq!(
        page.items[0]
            .model
            .as_ref()
            .and_then(|model| model.first())
            .and_then(|message| message.get("content"))
            .and_then(Value::as_str)
            .map(str::len),
        Some(PAYLOAD)
    );
    storage.close().await.expect("close");
    remove_dir(&dir).await;
}

/// Allocation-peak harness for R1-L3-008. Recovery holds one parsed copy of
/// the sidecar (`record_by_key`), so the live peak tracks the sidecar size
/// itself. The old `tokio::fs::read` shape peaked at 2x while reading the
/// file, then held the line strings, `record_by_key` and `parsed_files`
/// simultaneously for a 3x+ peak; the replay clones then added more.
///
/// Run alone: `cargo test --test durable_storage -- --ignored --test-threads=1
/// jsonl_recovery_allocation_stays_near_one_copy`.
#[tokio::test]
#[ignore = "allocation harness; run with --test-threads=1 --ignored"]
async fn jsonl_recovery_allocation_stays_near_one_copy() {
    const RECORDS: usize = 512;
    const PAYLOAD: usize = 32 * 1024;
    let section = MeasureSection::begin().await;
    let dir = temp_dir("recovery-allocation");
    let sidecar_bytes = write_legacy_live_task_sidecar(&dir, 20, RECORDS, PAYLOAD);
    assert!(sidecar_bytes >= 16 * 1024 * 1024, "fixture too small");

    let (result, stats) = section.measure(JsonlStorage::open(&dir)).await;
    let storage = result.expect("reopen");
    assert!(
        stats.peak_bytes <= sidecar_bytes + sidecar_bytes / 4,
        "recovery peaked at {} live bytes for a {} byte sidecar ({} allocation events, \
         {} bytes allocated in total); one parsed copy must stay near 1x, while the old \
         read-everything + clone-twice shape peaked above 3x",
        stats.peak_bytes,
        sidecar_bytes,
        stats.events,
        stats.bytes
    );
    assert!(
        stats.bytes <= sidecar_bytes * 3,
        "recovery allocated {} bytes in total for a {} byte sidecar ({} allocation events); \
         the old read-everything + clone-twice shape allocated more than 5x",
        stats.bytes,
        sidecar_bytes,
        stats.events
    );
    assert!(storage
        .task(TaskId::from_raw(20))
        .await
        .expect("task")
        .is_some());
    storage.close().await.expect("close");
    remove_dir(&dir).await;
}

// ── R1-L3-009: live task sidecars stay bounded ─────────────────────────────

/// The old implementation appended one full `TaskRecord` per state change, so
/// a task that changed state N times left N records in `task-<id>.jsonl`, and
/// every reopen parsed and replayed all of them. Compaction now rewrites the
/// sidecar to its newest record at a fixed byte threshold, which bounds both
/// the file size and its line count independently of N.
#[tokio::test]
async fn jsonl_live_task_sidecar_stays_bounded_across_many_transitions() {
    use pir::durable::storage::jsonl::TASK_SIDECAR_COMPACT_BYTES;

    const TRANSITIONS: usize = 600;
    const PAYLOAD: usize = 256;
    let dir = temp_dir("task-bounded");
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
        .expect("seed");

    let mut latest = task(20, 1, TaskStatus::Running);
    for index in 0..TRANSITIONS {
        latest.state = match index % 3 {
            0 => TaskState::Running {
                checkpoint: json!({"n": index, "blob": "x".repeat(PAYLOAD)}),
            },
            1 => TaskState::Waiting {
                checkpoint: json!({"n": index, "blob": "x".repeat(PAYLOAD)}),
                on: vec![TaskId::from_raw(99)],
                policy: pir::durable::JoinPolicy::FailFast,
            },
            _ => TaskState::Sleeping {
                checkpoint: json!({"n": index, "blob": "x".repeat(PAYLOAD)}),
                until: index as i64,
            },
        };
        latest.restarts = Some(index as i64);
        storage
            .commit(vec![StorageWrite::Task {
                value: latest.clone(),
            }])
            .await
            .expect("task transition");
    }

    let sidecar = dir.join("task-20.jsonl");
    let raw = std::fs::read_to_string(&sidecar).expect("read sidecar");
    let lines = raw.lines().count();
    assert!(
        lines <= 256,
        "a {TRANSITIONS}-transition live task kept {lines} sidecar records; the old \
         implementation grew one full record per transition"
    );
    assert!(
        lines * 4 < TRANSITIONS,
        "record count must not track the number of state changes: {lines} records \
         for {TRANSITIONS} transitions"
    );
    assert!(
        (raw.len() as u64) <= TASK_SIDECAR_COMPACT_BYTES + 4096,
        "sidecar grew to {} bytes with a {} byte compaction threshold",
        raw.len(),
        TASK_SIDECAR_COMPACT_BYTES
    );

    storage.close().await.expect("close");
    let reopened = JsonlStorage::open(&dir).await.expect("reopen");
    let recovered = reopened
        .task(TaskId::from_raw(20))
        .await
        .expect("task")
        .expect("present");
    assert_eq!(
        recovered, latest,
        "recovery rebuilds the newest full record even though older records were dropped"
    );
    reopened.close().await.expect("close");
    remove_dir(&dir).await;
}

/// A terminal task still writes its complete record into the main marker (never
/// a sidecar), reclaims the sidecar, and reopens with every field intact even
/// after the live history was compacted.
#[tokio::test]
async fn jsonl_terminal_task_after_compaction_recovers_the_full_record() {
    const TRANSITIONS: usize = 200;
    let dir = temp_dir("task-terminal-compacted");
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
        .expect("seed");
    for index in 0..TRANSITIONS {
        let mut value = task(20, 1, TaskStatus::Running);
        value.state = TaskState::Running {
            checkpoint: json!({"n": index, "blob": "x".repeat(128)}),
        };
        storage
            .commit(vec![StorageWrite::Task { value }])
            .await
            .expect("task transition");
    }

    let mut terminal = task(20, 1, TaskStatus::Terminal);
    terminal.ended_at = Some(4_242);
    terminal.restarts = Some(7);
    terminal.memos = Some(serde_json::Map::from_iter([(
        "note".to_owned(),
        json!("kept"),
    )]));
    storage
        .commit(vec![StorageWrite::Task {
            value: terminal.clone(),
        }])
        .await
        .expect("terminal task");
    assert!(
        !dir.join("task-20.jsonl").exists(),
        "a terminal task's sidecar is reclaimed"
    );
    let main = std::fs::read_to_string(dir.join("main.jsonl")).expect("read main");
    assert!(
        main.contains("\"type\":\"task\""),
        "the terminal record is stored in the main marker: {main}"
    );
    storage.close().await.expect("close");

    let reopened = JsonlStorage::open(&dir).await.expect("reopen");
    let recovered = reopened
        .task(TaskId::from_raw(20))
        .await
        .expect("task")
        .expect("present");
    assert_eq!(recovered, terminal, "the complete terminal record survives");
    reopened.close().await.expect("close");
    remove_dir(&dir).await;
}

/// A crash between the marker append and the compaction rewrite leaves older
/// full snapshots in the sidecar; a crash before the marker leaves an
/// unconfirmed tail record. Recovery must rebuild the newest confirmed state,
/// drop the stale prefix and truncate the unconfirmed tail.
#[tokio::test]
async fn jsonl_recovery_compacts_crashed_task_sidecar_and_drops_unconfirmed_tail() {
    let dir = temp_dir("task-crash-recovery");
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
            .expect("seed");
        let mut second = task(20, 1, TaskStatus::Running);
        second.state = TaskState::Running {
            checkpoint: json!({"n": 2}),
        };
        storage
            .commit(vec![StorageWrite::Task { value: second }])
            .await
            .expect("second transition");
        storage.close().await.expect("close");
    }

    let sidecar = dir.join("task-20.jsonl");
    let mut raw = std::fs::read_to_string(&sidecar).expect("read sidecar");
    assert_eq!(
        raw.lines().count(),
        2,
        "the fixture keeps both pre-compaction records"
    );
    let mut unconfirmed = task(20, 1, TaskStatus::Running);
    unconfirmed.state = TaskState::Running {
        checkpoint: json!({"n": 999}),
    };
    raw.push_str(
        &serde_json::to_string(&json!({
            "format": 1,
            "type": "record",
            "seq": 3,
            "ordinal": 0,
            "payload": {"type": "task", "value": unconfirmed},
        }))
        .expect("serialize unconfirmed record"),
    );
    raw.push('\n');
    std::fs::write(&sidecar, raw).expect("simulate crash shape");

    let storage = JsonlStorage::open(&dir).await.expect("reopen");
    let live = storage
        .task(TaskId::from_raw(20))
        .await
        .expect("task")
        .expect("present");
    let TaskState::Running { checkpoint } = &live.state else {
        panic!("unexpected recovered state: {:?}", live.state);
    };
    assert_eq!(
        checkpoint.get("n"),
        Some(&json!(2)),
        "the unconfirmed transition never landed"
    );
    storage.close().await.expect("close");

    let raw = std::fs::read_to_string(&sidecar).expect("read reclaimed sidecar");
    assert_eq!(
        raw.lines().count(),
        1,
        "recovery keeps only the newest record: {raw}"
    );
    assert!(
        raw.contains("\"n\":2"),
        "the newest confirmed record survives: {raw}"
    );
    remove_dir(&dir).await;
}

#[test]
fn page_of_owned_matches_page_of_for_every_limit() {
    use pir::durable::storage::scan::{page_of, page_of_owned};

    for len in 0..=6usize {
        let values: Vec<i64> = (0..len as i64).collect();
        for limit in 0..=6usize {
            for order in [ScanOrder::Ascending, ScanOrder::Descending] {
                let (expected_items, expected_next) =
                    page_of(&values, limit, order, |value| *value);
                let (items, next) = page_of_owned(values.clone(), limit, order, |value| *value);
                assert_eq!(items, expected_items, "len={len} limit={limit} {order:?}");
                assert_eq!(next, expected_next, "len={len} limit={limit} {order:?}");
            }
        }
    }
}
