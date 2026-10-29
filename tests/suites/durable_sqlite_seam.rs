//! Compile-time shape test for the SQLite backend seam.
//!
//! This suite deliberately contains **no SQLite implementation**. It proves
//! the two properties the reserved `Storage` seam must keep for a third
//! backend to be plugged in without touching the selection layer:
//!
//! 1. the trait can be implemented by a foreign type with only the public
//!    types — [`FakeSqliteStorage`] implements every required method;
//! 2. it is dyn-compatible, so the harness's `Arc<dyn Storage>` (and any
//!    `Box<dyn Storage>` backend-selection slot) keeps working.
//!
//! Every fake method returns `StorageError::Closed("... not implemented")`
//! instead of `unimplemented!()`: a compiles-but-runs test must never panic.

use pir::durable::types::{Storage, StorageFuture};
use pir::durable::{
    ConversationId, ConversationQuery, ConversationRecord, Cursor, DocumentAddress, DocumentId,
    DocumentPoint, DocumentQuery, DocumentRecord, EntryId, EntryQuery, EntryRecord, EntryWithSeq,
    Page, Seq, StorageError, StorageWrite, StoredDocument, SubmissionId, SubmissionQuery,
    SubmissionRecord, TaskId, TaskQuery, TaskRecord,
};
use std::sync::Arc;

// ─── the fake third backend ────────────────────────────────────────────────

/// A placeholder backend: its shape is the point, not its behavior.
///
/// The unit struct also proves that implementing `Storage` needs no backend
/// internals beyond `Send + Sync` (the trait's own supertraits) and no
/// private constructor.
struct FakeSqliteStorage;

/// One uniformly typed error body for every method.
///
/// `Box::pin(async { ... })` unsizes to the trait's `StorageFuture` at each
/// return site, exactly as the memory and JSONL backends do.
macro_rules! not_implemented {
    ($operation:literal) => {
        Box::pin(async {
            Err(StorageError::Closed(format!(
                "FakeSqliteStorage::{} is not implemented",
                $operation
            )))
        })
    };
}

impl Storage for FakeSqliteStorage {
    fn commit(&self, _writes: Vec<StorageWrite>) -> StorageFuture<'_, Result<Seq, StorageError>> {
        not_implemented!("commit")
    }

    fn mint_id(&self) -> StorageFuture<'_, Result<i64, StorageError>> {
        not_implemented!("mint_id")
    }

    fn conversation(
        &self,
        _id: ConversationId,
    ) -> StorageFuture<'_, Result<Option<ConversationRecord>, StorageError>> {
        not_implemented!("conversation")
    }

    fn scan_conversations(
        &self,
        _query: ConversationQuery,
        _limit: usize,
        _cursor: Option<Cursor>,
    ) -> StorageFuture<'_, Result<Page<ConversationRecord>, StorageError>> {
        not_implemented!("scan_conversations")
    }

    fn entry(&self, _id: EntryId) -> StorageFuture<'_, Result<Option<EntryWithSeq>, StorageError>> {
        not_implemented!("entry")
    }

    fn entry_in(
        &self,
        _conversation_id: ConversationId,
        _id: EntryId,
    ) -> StorageFuture<'_, Result<Option<EntryWithSeq>, StorageError>> {
        not_implemented!("entry_in")
    }

    fn find_latest_head_marker(
        &self,
        _conversation_id: ConversationId,
        _at_or_before_entry_id: Option<EntryId>,
    ) -> StorageFuture<'_, Result<Option<EntryRecord>, StorageError>> {
        not_implemented!("find_latest_head_marker")
    }

    fn scan_entries(
        &self,
        _query: EntryQuery,
        _limit: usize,
        _cursor: Option<Cursor>,
    ) -> StorageFuture<'_, Result<Page<EntryRecord>, StorageError>> {
        not_implemented!("scan_entries")
    }

    fn task(&self, _id: TaskId) -> StorageFuture<'_, Result<Option<TaskRecord>, StorageError>> {
        not_implemented!("task")
    }

    fn scan_tasks(
        &self,
        _query: TaskQuery,
        _limit: usize,
        _cursor: Option<Cursor>,
    ) -> StorageFuture<'_, Result<Page<TaskRecord>, StorageError>> {
        not_implemented!("scan_tasks")
    }

    fn submission(
        &self,
        _id: SubmissionId,
    ) -> StorageFuture<'_, Result<Option<SubmissionRecord>, StorageError>> {
        not_implemented!("submission")
    }

    fn scan_submissions(
        &self,
        _query: SubmissionQuery,
        _limit: usize,
        _cursor: Option<Cursor>,
    ) -> StorageFuture<'_, Result<Page<SubmissionRecord>, StorageError>> {
        not_implemented!("scan_submissions")
    }

    fn submission_by_request(
        &self,
        _conversation_id: ConversationId,
        _request_id: &str,
    ) -> StorageFuture<'_, Result<Option<SubmissionRecord>, StorageError>> {
        not_implemented!("submission_by_request")
    }

    fn find_document(
        &self,
        _address: DocumentAddress,
        _at: DocumentPoint,
    ) -> StorageFuture<'_, Result<Option<DocumentRecord>, StorageError>> {
        not_implemented!("find_document")
    }

    fn document(
        &self,
        _id: DocumentId,
        _at: DocumentPoint,
    ) -> StorageFuture<'_, Result<Option<StoredDocument>, StorageError>> {
        not_implemented!("document")
    }

    fn scan_documents(
        &self,
        _query: DocumentQuery,
        _limit: usize,
        _cursor: Option<Cursor>,
    ) -> StorageFuture<'_, Result<Page<DocumentRecord>, StorageError>> {
        not_implemented!("scan_documents")
    }

    fn close(&self) -> StorageFuture<'_, Result<(), StorageError>> {
        not_implemented!("close")
    }
}

// ─── compile-time shape proofs ─────────────────────────────────────────────

/// `FakeSqliteStorage` satisfies every supertrait the harness relies on.
///
/// The closure is never called; instantiating it type-checks the bound. If the
/// trait gained another required method, the `impl` above would stop
/// compiling first.
const _: fn() = || {
    fn assert_storage<S: Storage + Send + Sync + 'static>() {}
    assert_storage::<FakeSqliteStorage>();
};

/// Naming `dyn Storage` fails to compile if the trait stops being
/// dyn-compatible. This is the exact shape the session holds.
const _: fn(&dyn Storage) = |_| {};

// ─── runtime proof that a third backend is pluggable ──────────────────────

fn assert_closed<T: std::fmt::Debug>(result: Result<T, StorageError>) {
    match result {
        Err(StorageError::Closed(message)) => assert!(
            message.contains("not implemented"),
            "unexpected Closed message: {message}"
        ),
        other => panic!("expected StorageError::Closed, got {other:?}"),
    }
}

#[tokio::test]
async fn storage_can_be_replaced_by_a_third_backend() {
    // `Box<dyn Storage>` is the backend-selection slot: nothing here mentions
    // JSONL, MemoryStorage or their concrete types.
    let boxed: Box<dyn Storage> = Box::new(FakeSqliteStorage);
    assert_closed(boxed.commit(Vec::new()).await);
    assert_closed(boxed.conversation(ConversationId::from_raw(1)).await);
    assert_closed(boxed.mint_id().await);
    assert_closed(boxed.close().await);

    // The harness's `Arc<dyn Storage>` shape, including a `Send` future moved
    // across a task boundary (the trait's futures are explicitly `Send`).
    let shared: Arc<dyn Storage> = Arc::new(FakeSqliteStorage);
    let task_shared = Arc::clone(&shared);
    let joined = tokio::spawn(async move { task_shared.commit(Vec::new()).await })
        .await
        .expect("spawned storage call");
    assert_closed(joined);

    let scan = shared
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
        .await;
    assert_closed(scan);
}
