//! Facade conformance: the host-facing `Session` surface.
//!
//! These tests exercise the kernel through [`pir::durable::session::Session`]
//! only: one commit per typed convenience, reads on the line, close ordering
//! and committed publication. They are written against `MemoryStorage`, whose
//! commit is immediately visible to every reader of the same instance.
//!
//! Registered in `tests/g_durable.rs` as `mod durable_facade;`.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use pir::durable::harness::telemetry::TraceCarrier;
use pir::durable::session::line::JobValue;
use pir::durable::session::transaction::EntryDraft;
use pir::durable::session::{
    CommitOptions, CommitPublication, ConversationOwnership, DeferDeadline, HeadTarget, Session,
    SessionEnd, SessionOptions, StagedDocument, SubmissionCreate, TaskOptions, TransactionScope,
};
use pir::durable::types::StorageFuture;
use pir::durable::{
    address_id, CommonDocDefinition, Context, ConversationId, ConversationQuery,
    ConversationRecord, Cursor, DocOwner, DocumentAddress, DocumentContent, DocumentCreate,
    DocumentFork, DocumentId, DocumentPoint, DocumentQuery, DocumentRecord, DocumentScope,
    DocumentSemantics, EntryId, EntryQuery, EntryRecord, EntryWithSeq, JsonObject, MemoryStorage,
    Page, ResolvedAddress, Seq, Storage, StorageError, StorageWrite, StoredDocument, SubmissionId,
    SubmissionQuery, SubmissionRecord, SubmissionStatus, SubmissionType, TaskDefinition, TaskId,
    TaskQuery, TaskRecord, TaskState,
};

/// Open a facade over one shared storage instance.
fn open(storage: Arc<MemoryStorage>) -> Session {
    let storage: Arc<dyn Storage> = storage;
    Session::open(storage, SessionOptions::default())
}

/// A plain user entry draft.
fn entry_draft(text: &str) -> EntryDraft {
    EntryDraft {
        kind: "user".to_owned(),
        model: Some(vec![json!({ "role": "user", "content": text })]),
        data: Some(json!({ "text": text })),
        head: None,
        edits: None,
    }
}

#[tokio::test]
async fn entries_written_through_one_session_are_visible_to_a_second() {
    let storage = Arc::new(MemoryStorage::new());
    let first = open(storage.clone());
    let ctx = Context::background();

    let conversation = first
        .create_conversation(ConversationOwnership::Ownerless, None, ctx.clone())
        .await
        .expect("create conversation");
    assert!(conversation.parent.is_none());
    assert!(conversation.owner.is_none());

    let mut draft = entry_draft("hello");
    draft.head = Some(HeadTarget::SelfEntry);
    let entry = first
        .append_entry(conversation.id, draft, ctx.clone())
        .await
        .expect("append entry");
    assert_eq!(entry.conversation_id, conversation.id);
    assert_eq!(entry.head, Some(entry.id));
    // The default commit scope stamps no task.
    assert!(entry.by_task_id.is_none());

    // A second Session over the same storage observes the committed records.
    let second = open(storage.clone());
    let reread = second
        .conversation(conversation.id, &ctx)
        .await
        .expect("read conversation")
        .expect("conversation is committed");
    assert_eq!(reread, conversation);
    let reread_entry = second
        .entry(entry.id, &ctx)
        .await
        .expect("read entry")
        .expect("entry is committed");
    assert_eq!(reread_entry, entry);
    let page = second
        .scan_entries(
            EntryQuery {
                conversation_id: conversation.id,
                min_entry_id: None,
                max_entry_id: None,
                order: None,
            },
            10,
            None,
            &ctx,
        )
        .await
        .expect("scan entries");
    assert_eq!(page.items, vec![entry]);
    assert!(page.next.is_none());

    first.close().await.expect("close first");
    second.close().await.expect("close second");
}

#[tokio::test]
async fn a_commit_callback_reads_its_own_staged_writes() {
    let storage = Arc::new(MemoryStorage::new());
    let session = open(storage);
    let ctx = Context::background();
    let conversation = session
        .create_conversation(ConversationOwnership::Ownerless, None, ctx.clone())
        .await
        .expect("conversation");

    let scope = TransactionScope {
        conversation_id: Some(conversation.id),
        task_id: Some(TaskId::from_raw(777)),
    };
    let seen: (
        TransactionScope,
        Option<TaskRecord>,
        Option<JsonObject>,
        Option<JsonObject>,
    ) = session
        .commit_scoped(scope, ctx, move |tx, _state| {
            Box::pin(async move {
                let task = TaskRecord {
                    trace: None,
                    links: Vec::new(),
                    id: TaskId::from_raw(9001),
                    conversation_id: conversation.id,
                    kind: "demo".to_owned(),
                    version: 1,
                    input: json!({ "n": 1 }),
                    owner: None,
                    background: false,
                    abort_requested: false,
                    abort_reason: None,
                    abandon_on_restart: None,
                    started_at: None,
                    ended_at: None,
                    restarts: None,
                    state: TaskState::Pending {
                        checkpoint: json!({ "phase": "run" }),
                    },
                    memos: None,
                };
                tx.stage_task(task.clone(), true);
                let staged_task = tx.staged_task(task.id).cloned();

                let address = DocumentAddress {
                    kind: "demo".to_owned(),
                    scope: DocumentScope::Session,
                    key: None,
                };
                let address_id = address_id(&address);
                tx.stage_document(StagedDocument {
                    address,
                    address_id: address_id.clone(),
                    definition: None,
                    family: None,
                    value: JsonObject::new(),
                    created: true,
                    loaded: None,
                    retire: false,
                });
                let before = tx
                    .latest_document(&address_id)
                    .map(|document| document.value.clone());
                tx.latest_document_mut(&address_id)
                    .expect("staged document")
                    .value
                    .insert("count".to_owned(), json!(1));
                let after = tx
                    .latest_document(&address_id)
                    .map(|document| document.value.clone());

                let answer = (tx.scope(), staged_task, before, after);
                Ok(Box::new(answer) as JobValue)
            })
        })
        .await
        .expect("commit");

    let (observed_scope, staged_task, before, after) = seen;
    assert_eq!(observed_scope, scope);
    assert_eq!(
        staged_task.as_ref().map(|task| task.id),
        Some(TaskId::from_raw(9001))
    );
    assert!(before.expect("staged document").is_empty());
    assert_eq!(
        after.expect("edited document").get("count"),
        Some(&json!(1))
    );
    session.close().await.expect("close");
}

#[tokio::test]
async fn a_table_read_after_a_table_write_is_rejected() {
    let storage = Arc::new(MemoryStorage::new());
    let session = open(storage);
    let result: Result<(), StorageError> = session
        .commit::<(), _>(Context::background(), |tx, _state| {
            Box::pin(async move {
                tx.stage_conversation(
                    ConversationRecord {
                        id: ConversationId::from_raw(2),
                        parent: None,
                        owner: None,
                    },
                    None,
                );
                // `expect_err` proves the transaction rejects the read; the
                // callback returns the error, so nothing is committed.
                Err::<JobValue, _>(
                    tx.check_table_read("entry")
                        .expect_err("a table read after a table write"),
                )
            })
        })
        .await;
    let error = result.expect_err("the commit fails");
    assert!(
        matches!(error, StorageError::Invalid(_)),
        "unexpected error: {error}"
    );
    assert!(
        error
            .to_string()
            .contains("cannot read tables after the first table write"),
        "{error}"
    );
    session.close().await.expect("close");
}

#[tokio::test]
async fn a_commit_without_writes_returns_its_value_and_publishes_nothing() {
    let storage = Arc::new(MemoryStorage::new());
    let session = open(storage);
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    session
        .subscribe_commits(Arc::new(
            move |publication: &CommitPublication, _context: &Context| {
                sink.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(publication.seq);
            },
        ))
        .expect("subscribe");

    let value: u32 = session
        .commit(Context::background(), |_tx, _state| {
            Box::pin(async move { Ok(Box::new(7_u32) as JobValue) })
        })
        .await
        .expect("commit without writes");
    assert_eq!(value, 7);
    // No write means no commit sequence and no publication.
    assert!(seen
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .is_empty());
    session.close().await.expect("close");
}

#[tokio::test]
async fn a_closed_session_rejects_commits() {
    let storage = Arc::new(MemoryStorage::new());
    let session = open(storage);
    session.close().await.expect("close");

    let result: Result<u32, StorageError> = session
        .commit(Context::background(), |_tx, _state| {
            Box::pin(async move { Ok(Box::new(1_u32) as JobValue) })
        })
        .await;
    assert!(
        matches!(result, Err(StorageError::Closed(_))),
        "a closed session rejects the commit"
    );
}

#[tokio::test]
async fn subscribe_commits_sees_one_publication_per_commit_and_unsubscribe_stops_it() {
    let storage = Arc::new(MemoryStorage::new());
    let session = open(storage);
    let ctx = Context::background();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    let id = session
        .subscribe_commits(Arc::new(
            move |publication: &CommitPublication, _context: &Context| {
                sink.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(publication.seq);
            },
        ))
        .expect("subscribe");

    let conversation = session
        .create_conversation(ConversationOwnership::Ownerless, None, ctx.clone())
        .await
        .expect("conversation");
    session
        .append_entry(conversation.id, entry_draft("first"), ctx.clone())
        .await
        .expect("entry");

    let published: Vec<_> = seen
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    assert_eq!(published.len(), 2, "one publication per commit");
    assert!(published[0] < published[1], "sequences increase");

    session.unsubscribe_commits(id);
    session
        .append_entry(conversation.id, entry_draft("second"), ctx.clone())
        .await
        .expect("entry after unsubscribe");
    assert_eq!(
        seen.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len(),
        2,
        "the removed listener receives nothing"
    );
    session.close().await.expect("close");
}

fn initial_counter() -> JsonObject {
    let mut value = JsonObject::new();
    value.insert("count".to_owned(), json!(0));
    value
}

#[tokio::test]
async fn doc_loads_the_committed_value_and_snapshot_returns_it() {
    let storage = Arc::new(MemoryStorage::new());
    // Seed one committed incarnation directly so the read path starts from a
    // value the facade did not itself write.
    let document_id = DocumentId::from_raw(storage.mint_id().await.expect("mint document id"));
    let mut value = JsonObject::new();
    value.insert("count".to_owned(), json!(5));
    storage
        .commit(vec![StorageWrite::DocumentCreate {
            record: DocumentCreate {
                id: document_id,
                kind: "counter".to_owned(),
                key: None,
                scope: DocumentScope::Session,
                history: None,
                fork: None,
            },
            content: DocumentContent::Base { version: 1, value },
        }])
        .await
        .expect("seed document");

    let session = open(storage);
    let ctx = Context::background();
    let resolved = ResolvedAddress::new("counter", DocumentSemantics::Session, None, None)
        .expect("resolved address");
    let snapshot = session
        .snapshot(&resolved, &ctx)
        .await
        .expect("snapshot")
        .expect("committed value");
    assert_eq!(snapshot.get("count"), Some(&json!(5)));

    let definition = CommonDocDefinition::new("counter", 1, DocumentSemantics::Session)
        .expect("definition")
        .behaviour(initial_counter, None, None);
    let edited = session
        .doc(resolved.clone(), definition, ctx.clone(), |draft| {
            let count = draft.get("count").and_then(Value::as_i64).unwrap_or(0);
            draft.insert("count".to_owned(), json!(count + 1));
            draft.get("count").cloned()
        })
        .await
        .expect("edit document");
    // The edit ran on the loaded draft (5 -> 6), not on the initial value.
    assert_eq!(edited, Some(json!(6)));

    // Assembly committed the change; a fresh snapshot reloads it.
    let after = session
        .snapshot(&resolved, &ctx)
        .await
        .expect("snapshot after edit")
        .expect("still live");
    assert_eq!(after.get("count"), Some(&json!(6)));

    session
        .retire_doc(resolved.clone(), ctx.clone())
        .await
        .expect("retire document");
    assert!(
        session
            .snapshot(&resolved, &ctx)
            .await
            .expect("snapshot after retire")
            .is_none(),
        "a retired document has no current value"
    );
    session.close().await.expect("close");
}

#[tokio::test]
async fn task_and_submission_conveniences_create_identities() {
    let storage = Arc::new(MemoryStorage::new());
    let session = open(storage);
    let ctx = Context::background();
    let conversation = session
        .create_conversation(ConversationOwnership::Ownerless, None, ctx.clone())
        .await
        .expect("conversation");

    let definition =
        TaskDefinition::define("demo", 1, vec!["run".to_owned()]).expect("task definition");
    let task_id = session
        .create_task(
            &definition,
            json!({ "n": 1 }),
            TaskOptions {
                conversation_id: Some(conversation.id),
                ..TaskOptions::default()
            },
            ctx.clone(),
        )
        .await
        .expect("create task");
    assert!(task_id.as_i64() > 0);

    let submission = session
        .create_submission(
            SubmissionCreate::new(conversation.id, SubmissionType::Input),
            ctx.clone(),
        )
        .await
        .expect("create submission");
    assert!(submission.id.as_i64() > 0);
    assert_eq!(submission.conversation_id, conversation.id);
    assert_eq!(submission.status, SubmissionStatus::Queued);

    // Assembly wrote both records in their own commits.
    let page = session
        .scan_tasks(
            TaskQuery {
                conversation_id: Some(conversation.id),
                ..TaskQuery::default()
            },
            10,
            None,
            &ctx,
        )
        .await
        .expect("scan tasks");
    assert_eq!(
        page.items.iter().map(|task| task.id).collect::<Vec<_>>(),
        vec![task_id]
    );
    let stored_task = session
        .task(task_id, &ctx)
        .await
        .expect("read task")
        .expect("task is committed");
    assert_eq!(stored_task.conversation_id, conversation.id);
    assert_eq!(stored_task.kind, "demo");
    assert_eq!(stored_task.status(), pir::durable::TaskStatus::Pending);

    let stored_submission = session
        .submission(submission.id, &ctx)
        .await
        .expect("read submission")
        .expect("submission is committed");
    assert_eq!(stored_submission, submission);

    session.close().await.expect("close");
}

#[tokio::test]
async fn task_creation_inherits_the_first_submission_trace_and_links_the_rest() {
    let storage = Arc::new(MemoryStorage::new());
    let session = open(storage);
    let ctx = Context::background();
    let conversation = session
        .create_conversation(ConversationOwnership::Ownerless, None, ctx.clone())
        .await
        .expect("conversation");

    let definition =
        TaskDefinition::define("demo.links", 1, vec!["run".to_owned()]).expect("task definition");
    let first_trace = TraceCarrier::from_pairs([("trace", "first")]);
    let second_trace = TraceCarrier::from_pairs([("trace", "second")]);

    let mut first = SubmissionCreate::new(conversation.id, SubmissionType::Write);
    first.trace = Some(first_trace.clone());
    let first = session
        .create_submission(first, ctx.clone())
        .await
        .expect("first submission");
    let mut second = SubmissionCreate::new(conversation.id, SubmissionType::Write);
    second.trace = Some(second_trace.clone());
    let second = session
        .create_submission(second, ctx.clone())
        .await
        .expect("second submission");

    let task_id = session
        .create_task(
            &definition,
            json!({}),
            TaskOptions {
                conversation_id: Some(conversation.id),
                submission_ids: vec![first.id, second.id],
                ..TaskOptions::default()
            },
            ctx.clone(),
        )
        .await
        .expect("create task");
    let stored = session
        .task(task_id, &ctx)
        .await
        .expect("read task")
        .expect("task is committed");

    assert_eq!(
        stored.trace,
        Some(first_trace),
        "the first submission is the task's parent carrier"
    );
    assert_eq!(
        stored.links,
        vec![second_trace],
        "later submissions are immutable links, not parents"
    );

    session.close().await.expect("close");
}

// ─── CommitPersistence (deferred commits) ──────────────────────────────────

/// Initial value of the deferred-progress probe: `{ "count": 0 }`.
fn probe_initial() -> JsonObject {
    let mut value = JsonObject::new();
    value.insert("count".to_owned(), json!(0));
    value
}

/// A `history: latest`, `fork: initial` conversation document: the only shape
/// a deferred commit may hold.
fn probe_address(conversation: ConversationId) -> ResolvedAddress {
    ResolvedAddress::new(
        "probe",
        DocumentSemantics::ConversationLatest {
            fork: DocumentFork::Initial,
        },
        Some(DocOwner::Conversation(conversation)),
        None,
    )
    .expect("resolved address")
}

fn probe_definition() -> pir::durable::AnyDocDefinition {
    CommonDocDefinition::new(
        "probe",
        1,
        DocumentSemantics::ConversationLatest {
            fork: DocumentFork::Initial,
        },
    )
    .expect("definition")
    .behaviour(probe_initial, None, None)
}

/// A rewindable conversation document: never holdable, so a deferred edit
/// stores immediately.
fn rewindable_address(conversation: ConversationId) -> ResolvedAddress {
    ResolvedAddress::new(
        "rewindable-probe",
        DocumentSemantics::ConversationRewindable {
            fork: DocumentFork::AsOf,
        },
        Some(DocOwner::Conversation(conversation)),
        None,
    )
    .expect("resolved address")
}

fn rewindable_definition() -> pir::durable::AnyDocDefinition {
    CommonDocDefinition::new(
        "rewindable-probe",
        1,
        DocumentSemantics::ConversationRewindable {
            fork: DocumentFork::AsOf,
        },
    )
    .expect("definition")
    .behaviour(probe_initial, None, None)
}

/// Create a conversation and commit its probe document at `count: 0`.
async fn seeded_probe(
    storage: &Arc<MemoryStorage>,
    session: &Session,
    ctx: &Context,
) -> (ResolvedAddress, DocumentId) {
    let conversation = session
        .create_conversation(ConversationOwnership::Ownerless, None, ctx.clone())
        .await
        .expect("conversation");
    let address = probe_address(conversation.id);
    session
        .doc(address.clone(), probe_definition(), ctx.clone(), |draft| {
            draft.insert("count".to_owned(), json!(0));
        })
        .await
        .expect("seed probe");
    let record = storage
        .find_document(address.address.clone(), DocumentPoint::Current)
        .await
        .expect("find document")
        .expect("document is committed");
    (address, record.id)
}

async fn stored_count(storage: &Arc<MemoryStorage>, id: DocumentId) -> i64 {
    storage
        .document(id, DocumentPoint::Current)
        .await
        .expect("read document")
        .expect("document is live")
        .value
        .get("count")
        .and_then(Value::as_i64)
        .expect("count")
}

/// A [`MemoryStorage`] that tolerates `close()` and counts commits.
///
/// `MemoryStorage::close` makes every later read fail, so a test cannot inspect
/// what a session's close flushed. This wrapper delegates every operation to the
/// same instance and only makes `close` a no-op, which lets the test read the
/// flushed value (and open a second session over it) after the first closed.
struct KeepOpenMemory {
    inner: Arc<MemoryStorage>,
    commits: AtomicUsize,
    fail_next: std::sync::atomic::AtomicBool,
}

impl KeepOpenMemory {
    fn new(inner: Arc<MemoryStorage>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            commits: AtomicUsize::new(0),
            fail_next: std::sync::atomic::AtomicBool::new(false),
        })
    }

    fn commits(&self) -> usize {
        self.commits.load(Ordering::Acquire)
    }

    /// Make the next `commit` fail with an IO error, failing the session.
    fn fail_next_commit(&self) {
        self.fail_next.store(true, Ordering::Release);
    }
}

impl Storage for KeepOpenMemory {
    fn commit(&self, writes: Vec<StorageWrite>) -> StorageFuture<'_, Result<Seq, StorageError>> {
        if self.fail_next.swap(false, Ordering::AcqRel) {
            return Box::pin(async { Err(StorageError::Io("injected commit failure".to_owned())) });
        }
        Box::pin(async move {
            self.commits.fetch_add(1, Ordering::AcqRel);
            self.inner.commit(writes).await
        })
    }

    fn mint_id(&self) -> StorageFuture<'_, Result<i64, StorageError>> {
        self.inner.mint_id()
    }

    fn conversation(
        &self,
        id: ConversationId,
    ) -> StorageFuture<'_, Result<Option<ConversationRecord>, StorageError>> {
        self.inner.conversation(id)
    }

    fn scan_conversations(
        &self,
        query: ConversationQuery,
        limit: usize,
        cursor: Option<Cursor>,
    ) -> StorageFuture<'_, Result<Page<ConversationRecord>, StorageError>> {
        self.inner.scan_conversations(query, limit, cursor)
    }

    fn entry(&self, id: EntryId) -> StorageFuture<'_, Result<Option<EntryWithSeq>, StorageError>> {
        self.inner.entry(id)
    }

    fn entry_in(
        &self,
        conversation_id: ConversationId,
        id: EntryId,
    ) -> StorageFuture<'_, Result<Option<EntryWithSeq>, StorageError>> {
        self.inner.entry_in(conversation_id, id)
    }

    fn find_latest_head_marker(
        &self,
        conversation_id: ConversationId,
        at_or_before_entry_id: Option<EntryId>,
    ) -> StorageFuture<'_, Result<Option<EntryRecord>, StorageError>> {
        self.inner
            .find_latest_head_marker(conversation_id, at_or_before_entry_id)
    }

    fn scan_entries(
        &self,
        query: EntryQuery,
        limit: usize,
        cursor: Option<Cursor>,
    ) -> StorageFuture<'_, Result<Page<EntryRecord>, StorageError>> {
        self.inner.scan_entries(query, limit, cursor)
    }

    fn task(&self, id: TaskId) -> StorageFuture<'_, Result<Option<TaskRecord>, StorageError>> {
        self.inner.task(id)
    }

    fn scan_tasks(
        &self,
        query: TaskQuery,
        limit: usize,
        cursor: Option<Cursor>,
    ) -> StorageFuture<'_, Result<Page<TaskRecord>, StorageError>> {
        self.inner.scan_tasks(query, limit, cursor)
    }

    fn submission(
        &self,
        id: SubmissionId,
    ) -> StorageFuture<'_, Result<Option<SubmissionRecord>, StorageError>> {
        self.inner.submission(id)
    }

    fn scan_submissions(
        &self,
        query: SubmissionQuery,
        limit: usize,
        cursor: Option<Cursor>,
    ) -> StorageFuture<'_, Result<Page<SubmissionRecord>, StorageError>> {
        self.inner.scan_submissions(query, limit, cursor)
    }

    fn submission_by_request(
        &self,
        conversation_id: ConversationId,
        request_id: &str,
    ) -> StorageFuture<'_, Result<Option<SubmissionRecord>, StorageError>> {
        self.inner
            .submission_by_request(conversation_id, request_id)
    }

    fn find_document(
        &self,
        address: DocumentAddress,
        at: DocumentPoint,
    ) -> StorageFuture<'_, Result<Option<DocumentRecord>, StorageError>> {
        self.inner.find_document(address, at)
    }

    fn document(
        &self,
        id: DocumentId,
        at: DocumentPoint,
    ) -> StorageFuture<'_, Result<Option<StoredDocument>, StorageError>> {
        self.inner.document(id, at)
    }

    fn scan_documents(
        &self,
        query: DocumentQuery,
        limit: usize,
        cursor: Option<Cursor>,
    ) -> StorageFuture<'_, Result<Page<DocumentRecord>, StorageError>> {
        self.inner.scan_documents(query, limit, cursor)
    }

    fn close(&self) -> StorageFuture<'_, Result<(), StorageError>> {
        Box::pin(async { Ok(()) })
    }
}

#[tokio::test]
async fn a_deferred_commit_is_published_in_memory_and_stored_by_a_read() {
    let storage = Arc::new(MemoryStorage::new());
    let session = open(storage.clone());
    let ctx = Context::background();
    let (address, record_id) = seeded_probe(&storage, &session, &ctx).await;

    session
        .doc_with_options(
            address.clone(),
            probe_definition(),
            ctx.clone(),
            CommitOptions::deferred_within(DeferDeadline::Never),
            |draft| {
                draft.insert("count".to_owned(), json!(1));
            },
        )
        .await
        .expect("deferred edit");

    assert!(
        session.handle().has_deferred(),
        "the deferred commit is held in memory"
    );
    assert_eq!(
        stored_count(&storage, record_id).await,
        0,
        "storage still has the committed value"
    );

    let snapshot = session
        .snapshot(&address, &ctx)
        .await
        .expect("snapshot")
        .expect("still live");
    assert_eq!(
        snapshot.get("count").and_then(Value::as_i64),
        Some(1),
        "the read observes the held value"
    );
    assert!(!session.handle().has_deferred(), "the read stored the hold");
    assert_eq!(
        stored_count(&storage, record_id).await,
        1,
        "storage has the flushed value"
    );

    session.close().await.expect("close");
}

#[tokio::test]
async fn consecutive_deferred_commits_merge_into_one_delta() {
    let storage = Arc::new(MemoryStorage::new());
    let session = open(storage.clone());
    let ctx = Context::background();
    let (address, record_id) = seeded_probe(&storage, &session, &ctx).await;

    for count in 1..=2 {
        session
            .doc_with_options(
                address.clone(),
                probe_definition(),
                ctx.clone(),
                CommitOptions::deferred_within(DeferDeadline::Never),
                move |draft| {
                    draft.insert("count".to_owned(), json!(count));
                },
            )
            .await
            .expect("deferred edit");
    }
    assert_eq!(
        stored_count(&storage, record_id).await,
        0,
        "neither deferred commit reached storage"
    );

    session.flush_deferred(&ctx).await.expect("flush");
    assert_eq!(stored_count(&storage, record_id).await, 2);
    let stored = storage
        .document(record_id, DocumentPoint::Current)
        .await
        .expect("read document")
        .expect("document is live");
    assert_eq!(
        stored.deltas_since_base, 1,
        "both held commits were stored as one delta"
    );

    session.close().await.expect("close");
}

#[tokio::test]
async fn a_deferred_deadline_stores_the_hold() {
    let storage = Arc::new(MemoryStorage::new());
    let storage_dyn: Arc<dyn Storage> = storage.clone();
    let session = Session::open(
        storage_dyn,
        SessionOptions {
            persist_within_ms: Some(10),
            ..SessionOptions::default()
        },
    );
    let ctx = Context::background();
    let (address, record_id) = seeded_probe(&storage, &session, &ctx).await;

    session
        .doc_with_options(
            address,
            probe_definition(),
            ctx.clone(),
            CommitOptions::deferred(),
            |draft| {
                draft.insert("count".to_owned(), json!(9));
            },
        )
        .await
        .expect("deferred edit");
    assert_eq!(stored_count(&storage, record_id).await, 0);

    // The native deadline timer fires after ~10 ms; wait well past it.
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    assert_eq!(
        stored_count(&storage, record_id).await,
        9,
        "the deadline stored the held commit"
    );
    assert!(!session.handle().has_deferred());

    session.close().await.expect("close");
}

#[tokio::test]
async fn an_immediate_commit_stores_earlier_holds_first() {
    let storage = Arc::new(MemoryStorage::new());
    let session = open(storage.clone());
    let ctx = Context::background();
    let (address, record_id) = seeded_probe(&storage, &session, &ctx).await;

    session
        .doc_with_options(
            address.clone(),
            probe_definition(),
            ctx.clone(),
            CommitOptions::deferred_within(DeferDeadline::Never),
            |draft| {
                draft.insert("count".to_owned(), json!(1));
            },
        )
        .await
        .expect("deferred edit");
    session
        .doc(address, probe_definition(), ctx.clone(), |draft| {
            draft.insert("count".to_owned(), json!(2));
        })
        .await
        .expect("immediate edit");

    assert!(!session.handle().has_deferred());
    assert_eq!(stored_count(&storage, record_id).await, 2);
    let stored = storage
        .document(record_id, DocumentPoint::Current)
        .await
        .expect("read document")
        .expect("document is live");
    assert_eq!(
        stored.deltas_since_base, 2,
        "the held delta was stored before the immediate one"
    );

    session.close().await.expect("close");
}

#[tokio::test]
async fn close_stores_held_changes() {
    let memory = Arc::new(MemoryStorage::new());
    let storage = KeepOpenMemory::new(memory);
    let storage_dyn: Arc<dyn Storage> = storage.clone();
    let session = Session::open(storage_dyn, SessionOptions::default());
    let ctx = Context::background();

    let conversation = session
        .create_conversation(ConversationOwnership::Ownerless, None, ctx.clone())
        .await
        .expect("conversation");
    let address = probe_address(conversation.id);
    session
        .doc(address.clone(), probe_definition(), ctx.clone(), |draft| {
            draft.insert("count".to_owned(), json!(0));
        })
        .await
        .expect("seed probe");
    let record_id = storage
        .find_document(address.address.clone(), DocumentPoint::Current)
        .await
        .expect("find document")
        .expect("document is committed")
        .id;
    let before = storage.commits();

    session
        .doc_with_options(
            address.clone(),
            probe_definition(),
            ctx.clone(),
            CommitOptions::deferred_within(DeferDeadline::Never),
            |draft| {
                draft.insert("count".to_owned(), json!(5));
            },
        )
        .await
        .expect("deferred edit");
    assert_eq!(
        storage.commits(),
        before,
        "a deferred commit performs no storage commit"
    );

    session.close().await.expect("close");
    assert_eq!(
        storage.commits(),
        before + 1,
        "close stored the held change in one batch"
    );
    let stored = storage
        .document(record_id, DocumentPoint::Current)
        .await
        .expect("read document")
        .expect("document is live");
    assert_eq!(stored.value.get("count"), Some(&json!(5)));

    // A fresh session over the same storage reads the flushed value.
    let storage_dyn: Arc<dyn Storage> = storage.clone();
    let reopened = Session::open(storage_dyn, SessionOptions::default());
    let snapshot = reopened
        .snapshot(&address, &ctx)
        .await
        .expect("snapshot")
        .expect("still live");
    assert_eq!(snapshot.get("count").and_then(Value::as_i64), Some(5));
    reopened.close().await.expect("close reopened");
}

#[tokio::test]
async fn an_unstored_deferred_commit_is_lost_on_a_crash() {
    let storage = Arc::new(MemoryStorage::new());
    let session = open(storage.clone());
    let ctx = Context::background();
    let (address, record_id) = seeded_probe(&storage, &session, &ctx).await;

    session
        .doc_with_options(
            address,
            probe_definition(),
            ctx.clone(),
            CommitOptions::deferred_within(DeferDeadline::Never),
            |draft| {
                draft.insert("count".to_owned(), json!(7));
            },
        )
        .await
        .expect("deferred edit");
    // Dropping the session without closing it models a crash: the hold never
    // reaches storage.
    drop(session);
    assert_eq!(
        stored_count(&storage, record_id).await,
        0,
        "a crash loses the held change, as upstream documents"
    );
}

#[tokio::test]
async fn a_deferred_commit_of_a_rewindable_document_stores_immediately() {
    let storage = Arc::new(MemoryStorage::new());
    let session = open(storage.clone());
    let ctx = Context::background();
    let conversation = session
        .create_conversation(ConversationOwnership::Ownerless, None, ctx.clone())
        .await
        .expect("conversation");
    let address = rewindable_address(conversation.id);
    session
        .doc(
            address.clone(),
            rewindable_definition(),
            ctx.clone(),
            |draft| {
                draft.insert("count".to_owned(), json!(0));
            },
        )
        .await
        .expect("seed");
    let record_id = storage
        .find_document(address.address.clone(), DocumentPoint::Current)
        .await
        .expect("find")
        .expect("committed")
        .id;

    session
        .doc_with_options(
            address,
            rewindable_definition(),
            ctx.clone(),
            CommitOptions::deferred_within(DeferDeadline::Never),
            |draft| {
                draft.insert("count".to_owned(), json!(3));
            },
        )
        .await
        .expect("deferred edit");

    assert!(
        !session.handle().has_deferred(),
        "a rewindable document is never held"
    );
    assert_eq!(stored_count(&storage, record_id).await, 3);

    session.close().await.expect("close");
}

#[tokio::test]
async fn a_held_commit_is_announced_once_and_the_store_announces_nothing() {
    let storage = Arc::new(MemoryStorage::new());
    let session = open(storage.clone());
    let ctx = Context::background();
    let (address, _record_id) = seeded_probe(&storage, &session, &ctx).await;

    let seen = Arc::new(Mutex::new(Vec::<JsonObject>::new()));
    let sink = seen.clone();
    session
        .subscribe_commits(Arc::new(
            move |publication: &CommitPublication, _context| {
                let mut values = sink.lock().expect("lock");
                for change in &publication.changes {
                    if let pir::durable::session::publication::CommitChange::Document(
                        pir::durable::DocumentCommitChange::Document {
                            value: Some(value), ..
                        },
                    ) = change
                    {
                        values.push(value.clone());
                    }
                }
            },
        ))
        .expect("subscribe");

    session
        .doc_with_options(
            address,
            probe_definition(),
            ctx.clone(),
            CommitOptions::deferred_within(DeferDeadline::Never),
            |draft| {
                draft.insert("count".to_owned(), json!(4));
            },
        )
        .await
        .expect("deferred edit");
    assert_eq!(
        seen.lock().expect("lock").len(),
        1,
        "the held commit is published when memory adopts it"
    );

    session.flush_deferred(&ctx).await.expect("flush");
    assert_eq!(
        seen.lock().expect("lock").len(),
        1,
        "storing a hold publishes nothing, so no frame is applied twice"
    );

    session.close().await.expect("close");
}

// ─── Session end observation ───────────────────────────────────────────────

#[tokio::test]
async fn closed_settles_and_close_listeners_run() {
    let storage = Arc::new(MemoryStorage::new());
    let session = open(storage);
    let order = Arc::new(Mutex::new(Vec::<&'static str>::new()));

    let first = order.clone();
    session
        .subscribe_close(Arc::new(move || first.lock().expect("lock").push("first")))
        .expect("subscribe first");
    let removed = order.clone();
    let removed_id = session
        .subscribe_close(Arc::new(move || {
            removed.lock().expect("lock").push("removed")
        }))
        .expect("subscribe removed");
    session.unsubscribe_close(removed_id);
    let second = order.clone();
    session
        .subscribe_close(Arc::new(move || {
            second.lock().expect("lock").push("second")
        }))
        .expect("subscribe second");

    let closed = session.closed();
    session.close().await.expect("close");

    assert_eq!(
        order.lock().expect("lock").as_slice(),
        ["first", "second"],
        "close listeners run in registration order; an unsubscribed one never runs"
    );
    assert!(
        matches!(closed.await, SessionEnd::Closed),
        "the closed future reports a normal close"
    );
    assert!(
        session.subscribe_close(Arc::new(|| {})).is_err(),
        "a closed session refuses new close subscriptions"
    );
}

#[tokio::test]
async fn closed_reports_a_storage_failure_after_its_close_listeners_ran() {
    let memory = Arc::new(MemoryStorage::new());
    let storage = KeepOpenMemory::new(memory);
    let storage_dyn: Arc<dyn Storage> = storage.clone();
    let session = Session::open(storage_dyn, SessionOptions::default());
    let ctx = Context::background();
    let closings = Arc::new(AtomicUsize::new(0));
    let seen = closings.clone();
    session
        .subscribe_close(Arc::new(move || {
            seen.fetch_add(1, Ordering::AcqRel);
        }))
        .expect("subscribe close");

    let closed = session.closed();
    storage.fail_next_commit();
    let error = session
        .create_conversation(ConversationOwnership::Ownerless, None, ctx.clone())
        .await
        .expect_err("the injected storage failure fails the commit");
    assert!(
        matches!(error, StorageError::Io(_)),
        "the commit reports the injected failure: {error:?}"
    );

    // The end signal is published after the close listeners ran, and the
    // failure is the session's end reason.
    match closed.await {
        SessionEnd::Failed(cause) => {
            assert!(matches!(cause, StorageError::Io(_)));
        }
        SessionEnd::Closed => panic!("a failed session ends as failed"),
    }
    assert_eq!(
        closings.load(Ordering::Acquire),
        1,
        "the failure ran the close listener exactly once"
    );
    assert!(session.is_failed());
    assert!(session
        .create_conversation(ConversationOwnership::Ownerless, None, ctx)
        .await
        .is_err());
}

// ─── snapshotAsOf ──────────────────────────────────────────────────────────

#[tokio::test]
async fn snapshot_as_of_reads_the_document_history_at_an_entry() {
    let storage = Arc::new(MemoryStorage::new());
    let session = open(storage);
    let ctx = Context::background();
    let conversation = session
        .create_conversation(ConversationOwnership::Ownerless, None, ctx.clone())
        .await
        .expect("conversation");
    let address = rewindable_address(conversation.id);
    let definition = rewindable_definition();

    // Create the incarnation, then mark its initial value with an entry.
    session
        .doc(address.clone(), definition.clone(), ctx.clone(), |draft| {
            draft.insert("count".to_owned(), json!(0));
        })
        .await
        .expect("create document");
    let before = session
        .append_entry(conversation.id, entry_draft("before"), ctx.clone())
        .await
        .expect("entry before");

    session
        .doc(address.clone(), definition, ctx.clone(), |draft| {
            draft.insert("count".to_owned(), json!(1));
        })
        .await
        .expect("edit document");
    let after = session
        .append_entry(conversation.id, entry_draft("after"), ctx.clone())
        .await
        .expect("entry after");

    let as_of_before = session
        .snapshot_as_of(&address, before.id, &ctx)
        .await
        .expect("as-of before")
        .expect("document existed before");
    assert_eq!(
        as_of_before.get("count").and_then(Value::as_i64),
        Some(0),
        "the document is read at the entry's commit"
    );

    let as_of_after = session
        .snapshot_as_of(&address, after.id, &ctx)
        .await
        .expect("as-of after")
        .expect("document exists after");
    assert_eq!(as_of_after.get("count").and_then(Value::as_i64), Some(1));

    // A session-scoped address is refused, and the refusal does not fail the
    // session.
    let session_scoped =
        ResolvedAddress::new("probe", DocumentSemantics::Session, None, None).expect("session");
    let error = session
        .snapshot_as_of(&session_scoped, after.id, &ctx)
        .await
        .expect_err("a session document has no history");
    assert!(matches!(error, StorageError::Invalid(_)));
    assert!(!session.is_failed());
    assert!(session
        .snapshot(&address, &ctx)
        .await
        .expect("session still reads")
        .is_some());

    session.close().await.expect("close");
}
