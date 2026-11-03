//! The attach layer: one committed incarnation turned into an observer.
//!
//! Port of upstream `session.ts` `#attachDocument` (`:824-863`) and
//! `observedOperations` (`:930-947`). Every case here drives the real line:
//! a [`SessionInner`] is opened with `line::open`, the attachment is made
//! through `SessionHandle::maintain`, and publications are handed to
//! [`deliver`] on that same line.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{json, Map, Value};

use pir::durable::delta::{Op, Seg};
use pir::durable::harness::harness::{Harness, HarnessOptions};
use pir::durable::harness::provider::{provider_address, provider_doc};
use pir::durable::harness::registry::create_registry;
use pir::durable::ids::{ConversationId, DocumentId, EntryId, Seq, SubmissionId, TaskId};
use pir::durable::session::assemble::{
    document_publication_clones, reset_document_publication_clones,
};
use pir::durable::session::attach::{attach_state, deliver, detach, AttachHost, Attached};
use pir::durable::session::documents::LoadedDocument;
use pir::durable::session::facade::{ConversationOwnership, Session};
use pir::durable::session::line::{
    self, CommitOptions, DeferDeadline, IntoJobValue, SessionHandle, SessionOptions,
};
use pir::durable::session::observation::{ObservedFrame, WatchEnd};
use pir::durable::session::publication::{CommitChange, CommitPublication};
use pir::durable::session::transaction::{StagedDocument, TransactionScope};
use pir::durable::types::StorageFuture;
use pir::durable::types::{
    ConversationQuery, ConversationRecord, Cursor, DocumentAddress, DocumentCommitChange,
    DocumentPoint, DocumentQuery, DocumentRecord, DocumentScope, EntryQuery, EntryRecord,
    EntryWithSeq, Page, StorageWrite, StoredDocument, SubmissionQuery, SubmissionRecord, TaskQuery,
    TaskRecord,
};
use pir::durable::{
    AnyDocDefinition, CommonDocDefinition, Context, DocCheckpointWhen, DocCheckpointWhenExt,
    DocDefinition, DocOwner, DocumentFork, DocumentSemantics, JsonObject, MemoryStorage,
    ResolvedAddress, Storage, StorageError,
};

// ── helpers ────────────────────────────────────────────────────────────────

fn address() -> String {
    "session/demo".to_owned()
}

fn record(id: DocumentId) -> DocumentRecord {
    DocumentRecord {
        id,
        kind: "demo".to_owned(),
        key: None,
        created_at: Seq::from_raw(1),
        retired_at: None,
        scope: DocumentScope::Session,
        history: None,
        fork: None,
    }
}

fn loaded(id: DocumentId, version: i64, value: Map<String, Value>) -> LoadedDocument {
    LoadedDocument {
        address_id: address(),
        record: record(id),
        stored_version: version,
        value_version: version,
        deltas_since_base: 0,
        value,
    }
}

fn object(value: Value) -> Map<String, Value> {
    value.as_object().cloned().expect("object")
}

fn publication(seq: i64, change: DocumentCommitChange) -> CommitPublication {
    CommitPublication {
        seq: Seq::from_raw(seq),
        changes: vec![CommitChange::Document(change)],
    }
}

fn document_change(
    id: DocumentId,
    version: Option<i64>,
    value: Option<Map<String, Value>>,
    ops: Vec<Op>,
) -> DocumentCommitChange {
    DocumentCommitChange::Document {
        record: record(id),
        conversation_id: None,
        version,
        value,
        ops,
    }
}

async fn open_session() -> (SessionHandle, tokio::task::JoinHandle<()>) {
    line::open(Arc::new(MemoryStorage::new()), SessionOptions::default())
}

/// Open a line session while keeping the backend around for test reads.
async fn open_session_with_storage() -> (
    SessionHandle,
    tokio::task::JoinHandle<()>,
    Arc<MemoryStorage>,
) {
    let storage = Arc::new(MemoryStorage::new());
    let backend: Arc<dyn Storage> = storage.clone();
    let (handle, task) = line::open(backend, SessionOptions::default());
    (handle, task, storage)
}

async fn attach(handle: &SessionHandle, loaded: LoadedDocument) -> Attached {
    attach_at(handle, address(), loaded).await
}

async fn attach_at(handle: &SessionHandle, address_id: String, loaded: LoadedDocument) -> Attached {
    let (tx, rx) = tokio::sync::oneshot::channel();
    handle
        .maintain(move |inner| {
            let _ = tx.send(attach_state(inner, address_id, &loaded));
        })
        .await
        .expect("maintain");
    rx.await.expect("attachment reply")
}

// ── R1-L3-014: publication work only for observers ─────────────────────────

const COUNTER_KIND: &str = "perf.counter";

fn counter_address() -> DocumentAddress {
    DocumentAddress {
        kind: COUNTER_KIND.to_owned(),
        scope: DocumentScope::Session,
        key: None,
    }
}

fn counter_definition() -> DocDefinition {
    DocDefinition::session(COUNTER_KIND, 1).expect("definition")
}

fn counter_value(count: i64) -> Map<String, Value> {
    object(json!({ "count": count }))
}

fn staged_creation(address: &DocumentAddress, value: Map<String, Value>) -> StagedDocument {
    StagedDocument {
        address: address.clone(),
        address_id: pir::durable::address_id(address),
        definition: Some(counter_definition()),
        family: None,
        value,
        created: true,
        loaded: None,
        retire: false,
    }
}

async fn commit_staged(handle: &SessionHandle, staged: StagedDocument) {
    handle
        .commit::<(), _>(
            TransactionScope::default(),
            Context::background(),
            move |tx, _state| {
                Box::pin(async move {
                    tx.stage_document(staged);
                    Ok(().into_job_value())
                })
            },
        )
        .await
        .expect("document commit");
}

async fn stored_document(storage: &MemoryStorage, address: &DocumentAddress) -> LoadedDocument {
    let record = storage
        .find_document(address.clone(), DocumentPoint::Current)
        .await
        .expect("find document")
        .expect("document exists");
    let stored = storage
        .document(record.id, DocumentPoint::Current)
        .await
        .expect("read document")
        .expect("stored value");
    let address_id = pir::durable::address_id(address);
    LoadedDocument {
        address_id,
        record,
        stored_version: stored.version,
        value_version: stored.version,
        deltas_since_base: stored.deltas_since_base,
        value: stored.value,
    }
}

fn staged_edit(loaded: LoadedDocument, value: Map<String, Value>) -> StagedDocument {
    StagedDocument {
        address: DocumentAddress {
            kind: COUNTER_KIND.to_owned(),
            scope: DocumentScope::Session,
            key: None,
        },
        address_id: loaded.address_id.clone(),
        definition: Some(counter_definition()),
        family: None,
        value,
        created: false,
        loaded: Some(loaded),
        retire: false,
    }
}

async fn attach_and_deliver(
    handle: &SessionHandle,
    attached: &Attached,
    publication: CommitPublication,
) {
    let attached = attached.clone();
    handle
        .maintain(move |inner| deliver(inner, &attached, &publication))
        .await
        .expect("maintain");
}

async fn detach_on_line(handle: &SessionHandle, attached: &Attached) {
    let attached = attached.clone();
    handle
        .maintain(move |inner| detach(inner, &attached))
        .await
        .expect("maintain");
}

async fn is_attached(handle: &SessionHandle, id: DocumentId) -> bool {
    let (tx, rx) = tokio::sync::oneshot::channel();
    handle
        .maintain(move |inner| {
            let _ = tx.send(inner.documents.is_attached(id));
        })
        .await
        .expect("maintain");
    rx.await.expect("reply")
}

async fn listener_count(handle: &SessionHandle) -> usize {
    let (tx, rx) = tokio::sync::oneshot::channel();
    handle
        .maintain(move |inner| {
            let _ = tx.send(inner.internal_listeners.len());
        })
        .await
        .expect("maintain");
    rx.await.expect("reply")
}

async fn close_listeners(handle: &SessionHandle) -> usize {
    let (tx, rx) = tokio::sync::oneshot::channel();
    handle
        .maintain(move |inner| {
            // Both registries: a regression that keeps the attach/watch
            // listener in the unremovable `close_listeners` list shows up as
            // growth here, exactly like a leftover keyed subscription.
            let _ = tx.send(inner.close_listeners.len() + inner.close_subscriptions.len());
        })
        .await
        .expect("maintain");
    rx.await.expect("reply")
}

async fn open_harness() -> Harness {
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
    Harness::open(
        storage,
        HarnessOptions {
            registry: Some(Arc::new(create_registry())),
            ..HarnessOptions::default()
        },
        Context::background(),
    )
    .await
    .expect("open harness")
}

/// Drain the observer's pending frames.
fn frames(attached: &Attached) -> Vec<ObservedFrame> {
    let core = attached.source.core();
    let mut core = core
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut drained = Vec::new();
    while let Some(frame) = core.next_frame() {
        drained.push(frame);
    }
    drained
}

fn version(attached: &Attached) -> i64 {
    let core = attached.source.core();
    let core = core
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    core.version()
}

fn ended(attached: &Attached) -> Option<WatchEnd> {
    let core = attached.source.core();
    let core = core
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    core.ended().cloned()
}

fn count(n: i64) -> Map<String, Value> {
    object(json!({ "count": n }))
}

// ── R1-L3-014: publication adoption only for observers ─────────────────────

/// A commit with no listener/attachment/telemetry cannot consume document
/// changes, so it must not clone every publishing plan's complete value and
/// operation batch. The counting seam makes the old `adopt()`-always shape
/// fail here (one value clone for the creation, then value + operations for
/// the edit). Registering an observer must make the very next commit publish
/// the full adopted state again.
#[tokio::test(flavor = "current_thread")]
async fn document_commits_without_observers_build_no_publication_copies() {
    let (handle, task, storage) = open_session_with_storage().await;
    let address = counter_address();

    reset_document_publication_clones();
    commit_staged(&handle, staged_creation(&address, counter_value(1))).await;
    assert_eq!(
        document_publication_clones(),
        0,
        "a creation with no observer must not clone its publication value"
    );

    commit_staged(
        &handle,
        staged_edit(stored_document(&storage, &address).await, counter_value(2)),
    )
    .await;
    assert_eq!(
        document_publication_clones(),
        0,
        "an edit with no observer must not clone its publication value/ops"
    );

    // Register an observer after the no-observer commits. The next commit
    // samples the listener set on the line and publishes the complete adopted
    // revision, exactly as if the observer had always been present.
    let loaded = stored_document(&storage, &address).await;
    let attached = attach_at(&handle, pir::durable::address_id(&address), loaded).await;
    assert_eq!(listener_count(&handle).await, 1);

    reset_document_publication_clones();
    commit_staged(
        &handle,
        staged_edit(stored_document(&storage, &address).await, counter_value(3)),
    )
    .await;
    assert!(
        document_publication_clones() >= 2,
        "an observed edit clones its value and operation batch, got {}",
        document_publication_clones()
    );

    let delivered = frames(&attached);
    assert_eq!(delivered.len(), 1, "the observer sees the next commit");
    assert_eq!(
        delivered[0].value,
        Some(counter_value(3)),
        "the observer receives the full adopted value"
    );
    assert_eq!(
        delivered[0].ops,
        vec![Op::Set(vec![Seg::Key("count".to_owned())], json!(3))],
        "and the exact adopted operations"
    );
    assert_eq!(attached.source.value(), Some(counter_value(3)));

    handle.close().await.expect("close");
    let _ = task.await;
}

// ── P2-ADOPT1-01: the public facade paths must reach the fast path ─────────

const FACADE_COUNTER_KIND: &str = "lane.adopt2.counter";

fn facade_counter_address() -> ResolvedAddress {
    ResolvedAddress::new(FACADE_COUNTER_KIND, DocumentSemantics::Session, None, None)
        .expect("resolved address")
}

fn facade_counter_initial() -> JsonObject {
    let mut value = JsonObject::new();
    value.insert("count".to_owned(), json!(0));
    value
}

fn facade_counter_definition() -> AnyDocDefinition {
    CommonDocDefinition::new(FACADE_COUNTER_KIND, 1, DocumentSemantics::Session)
        .expect("definition")
        .behaviour(facade_counter_initial, None, DocCheckpointWhen::never())
}

async fn facade_doc_set(session: &Session, count: i64) {
    session
        .doc(
            facade_counter_address(),
            facade_counter_definition(),
            Context::background(),
            move |draft| {
                draft.insert("count".to_owned(), json!(count));
            },
        )
        .await
        .expect("facade document commit");
}

/// Drain the single document change a subscriber expects from one publication.
fn sole_document_change(publication: &CommitPublication) -> &DocumentCommitChange {
    let documents: Vec<&DocumentCommitChange> = publication
        .changes
        .iter()
        .filter_map(|change| match change {
            CommitChange::Document(document) => Some(document),
            CommitChange::Table(_) => None,
        })
        .collect();
    assert_eq!(documents.len(), 1, "one document change per edit");
    documents[0]
}

fn assert_published_count(publication: &CommitPublication, expected: i64) {
    match sole_document_change(publication) {
        DocumentCommitChange::Document {
            value: Some(value),
            ops,
            ..
        } => {
            assert_eq!(
                value.get("count").and_then(Value::as_i64),
                Some(expected),
                "the subscriber receives the full adopted value"
            );
            assert_eq!(
                ops,
                &vec![Op::Set(vec![Seg::Key("count".to_owned())], json!(expected))],
                "and the exact adopted operations"
            );
        }
        other => panic!("expected a document change, got {other:?}"),
    }
}

fn watched_publications(session: &Session) -> (Arc<Mutex<Vec<CommitPublication>>>, u64) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    let id = session
        .subscribe_commits(Arc::new(
            move |publication: &CommitPublication, _context: &Context| {
                sink.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(publication.clone());
            },
        ))
        .expect("subscribe commits");
    (seen, id)
}

fn take_publications(seen: &Arc<Mutex<Vec<CommitPublication>>>) -> Vec<CommitPublication> {
    std::mem::take(
        &mut *seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    )
}

/// `Session::doc` is the production entry point. Its permanent fan-out
/// listener must not count as a document observer while the host registry is
/// empty: creation and edit clone no full revision. The first subscription
/// makes the very next commit publish the complete value and operations, and
/// unsubscribing restores the fast path. The pre-fix implementation always
/// installed an unconditional host listener, so the first `== 0` assertion
/// fails there (a creation adopts one value).
#[tokio::test(flavor = "current_thread")]
async fn facade_session_skips_document_publication_until_subscribed() {
    let storage = Arc::new(MemoryStorage::new());
    let session = Session::open(storage.clone(), SessionOptions::default());

    reset_document_publication_clones();
    facade_doc_set(&session, 1).await;
    assert_eq!(
        document_publication_clones(),
        0,
        "a default Session creation must not adopt a publication value"
    );

    facade_doc_set(&session, 2).await;
    assert_eq!(
        document_publication_clones(),
        0,
        "a default Session edit must not clone its publication value/ops"
    );

    let (seen, subscriber_id) = watched_publications(&session);
    reset_document_publication_clones();
    facade_doc_set(&session, 3).await;
    assert!(
        document_publication_clones() >= 2,
        "a subscribed edit clones its value and operation batch, got {}",
        document_publication_clones()
    );
    let publications = take_publications(&seen);
    assert_eq!(publications.len(), 1, "the subscriber sees the next commit");
    assert_published_count(&publications[0], 3);

    // Removing the last subscriber must re-arm the fast path.
    session.unsubscribe_commits(subscriber_id);
    reset_document_publication_clones();
    facade_doc_set(&session, 4).await;
    assert_eq!(
        document_publication_clones(),
        0,
        "unsubscribing restores the no-adoption fast path"
    );

    // An attached document observer is a document consumer too: the next
    // `Session::doc` edit must adopt and deliver the exact revision.
    let address = facade_counter_address();
    let loaded = stored_document(&storage, &address.address).await;
    let attached = attach_at(session.handle(), address.id, loaded).await;
    reset_document_publication_clones();
    facade_doc_set(&session, 5).await;
    assert!(
        document_publication_clones() >= 2,
        "an attached observer clones the adopted value and operation batch"
    );
    let delivered = frames(&attached);
    assert_eq!(delivered.len(), 1, "the attachment sees the next commit");
    assert_eq!(
        delivered[0].value,
        Some(counter_value(5)),
        "the attachment receives the full adopted value"
    );
    assert_eq!(
        delivered[0].ops,
        vec![Op::Set(vec![Seg::Key("count".to_owned())], json!(5))],
        "and the exact adopted operations"
    );

    session.close().await.expect("close");
}

/// The same contract through `Harness::doc`: the scheduler, task-graph,
/// conversation-view and submission-waiter internal listeners must not force
/// document publication while nothing is subscribed or mounted. Before the
/// fix, the permanent submission-waiter listener alone kept `publishing` true.
#[tokio::test(flavor = "current_thread")]
async fn facade_harness_skips_document_publication_until_subscribed() {
    let harness = open_harness().await;

    reset_document_publication_clones();
    harness
        .doc(
            facade_counter_address(),
            facade_counter_definition(),
            Context::background(),
            |draft| {
                draft.insert("count".to_owned(), json!(1));
            },
        )
        .await
        .expect("create document");
    assert_eq!(
        document_publication_clones(),
        0,
        "a default Harness creation must not adopt a publication value"
    );

    harness
        .doc(
            facade_counter_address(),
            facade_counter_definition(),
            Context::background(),
            |draft| {
                draft.insert("count".to_owned(), json!(2));
            },
        )
        .await
        .expect("edit document");
    assert_eq!(
        document_publication_clones(),
        0,
        "a default Harness edit must not clone its publication value/ops"
    );

    let (seen, _subscriber_id) = watched_publications(harness.session());
    reset_document_publication_clones();
    harness
        .doc(
            facade_counter_address(),
            facade_counter_definition(),
            Context::background(),
            |draft| {
                draft.insert("count".to_owned(), json!(3));
            },
        )
        .await
        .expect("edit document");
    assert!(
        document_publication_clones() >= 2,
        "a subscribed harness edit clones its value and operation batch, got {}",
        document_publication_clones()
    );
    let publications = take_publications(&seen);
    assert_eq!(publications.len(), 1, "the subscriber sees the next commit");
    assert_published_count(&publications[0], 3);

    harness.close().await.expect("close");
}

/// The same no-adoption / complete-delivery contract through `Harness::doc`
/// with a document attachment (not only a host commit subscriber): the
/// attachment forces the document half on the next edit and receives the full
/// value and exact operations.
#[tokio::test(flavor = "current_thread")]
async fn facade_harness_attachment_forces_document_publication() {
    let harness = open_harness().await;

    reset_document_publication_clones();
    harness
        .doc(
            facade_counter_address(),
            facade_counter_definition(),
            Context::background(),
            |draft| {
                draft.insert("count".to_owned(), json!(1));
            },
        )
        .await
        .expect("create document");
    assert_eq!(
        document_publication_clones(),
        0,
        "a default Harness creation must not adopt a publication value"
    );

    harness
        .doc(
            facade_counter_address(),
            facade_counter_definition(),
            Context::background(),
            |draft| {
                draft.insert("count".to_owned(), json!(2));
            },
        )
        .await
        .expect("edit document");
    assert_eq!(
        document_publication_clones(),
        0,
        "a default Harness edit must not clone its publication value/ops"
    );

    let address = facade_counter_address();
    let definition = facade_counter_definition();
    let attached = harness
        .document_state(&address, &definition, &Context::background())
        .await
        .expect("attach document")
        .expect("document exists");
    reset_document_publication_clones();
    harness
        .doc(
            facade_counter_address(),
            facade_counter_definition(),
            Context::background(),
            |draft| {
                draft.insert("count".to_owned(), json!(3));
            },
        )
        .await
        .expect("edit document");
    assert!(
        document_publication_clones() >= 2,
        "an attached harness observer clones the adopted value and operation batch"
    );
    let delivered = frames(&attached);
    assert_eq!(delivered.len(), 1, "the attachment sees the next commit");
    assert_eq!(
        delivered[0].value,
        Some(counter_value(3)),
        "the attachment receives the full adopted value"
    );
    assert_eq!(
        delivered[0].ops,
        vec![Op::Set(vec![Seg::Key("count".to_owned())], json!(3))],
        "and the exact adopted operations"
    );

    harness.close().await.expect("close");
}

// ── attach ─────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "current_thread")]
async fn attach_pins_the_incarnation_and_exposes_its_value() {
    let (handle, task) = open_session().await;
    let id = DocumentId::from_raw(7);
    let attached = attach(&handle, loaded(id, 1, count(1))).await;

    assert_eq!(attached.record_id, id);
    assert_eq!(attached.address_id, address());
    assert_eq!(attached.source.value(), Some(count(1)));
    // The hydrated version is the incarnation's value version
    // (`session.ts:847`, `observed = { version: loaded.valueVersion }`).
    assert_eq!(
        attached.host(),
        AttachHost {
            address_id: address(),
            record_id: id,
            version: 1,
        }
    );
    // `session.ts:834` — the incarnation stays cached while an observer is
    // attached.
    assert!(is_attached(&handle, id).await);
    assert_eq!(listener_count(&handle).await, 1);

    handle.close().await.expect("close");
    let _ = task.await;
}

// ── delta delivery ─────────────────────────────────────────────────────────

#[tokio::test(flavor = "current_thread")]
async fn a_delta_publication_delivers_the_exact_ops() {
    let (handle, task) = open_session().await;
    let id = DocumentId::from_raw(7);
    let attached = attach(&handle, loaded(id, 1, count(1))).await;

    let ops = vec![Op::Set(vec![Seg::Key("count".to_owned())], json!(2))];
    attach_and_deliver(
        &handle,
        &attached,
        publication(2, document_change(id, Some(1), Some(count(2)), ops.clone())),
    )
    .await;

    let delivered = frames(&attached);
    assert_eq!(delivered.len(), 1);
    // Same version: the observer receives the exact adopted operations.
    assert_eq!(delivered[0].ops, ops);
    assert_eq!(delivered[0].value, Some(count(2)));
    assert_eq!(version(&attached), 1);
    assert_eq!(attached.source.value(), Some(count(2)));

    handle.close().await.expect("close");
    let _ = task.await;
}

#[tokio::test(flavor = "current_thread")]
async fn another_incarnation_is_never_delivered() {
    let (handle, task) = open_session().await;
    let id = DocumentId::from_raw(7);
    let attached = attach(&handle, loaded(id, 1, count(1))).await;

    // A change of a different record — even at the same address kind — is not
    // this observer's (`session.ts:850`).
    attach_and_deliver(
        &handle,
        &attached,
        publication(
            2,
            document_change(
                DocumentId::from_raw(8),
                Some(1),
                Some(count(9)),
                vec![Op::Set(vec![Seg::Key("count".to_owned())], json!(9))],
            ),
        ),
    )
    .await;

    assert!(frames(&attached).is_empty());
    assert_eq!(attached.source.value(), Some(count(1)));

    handle.close().await.expect("close");
    let _ = task.await;
}

// ── version change ─────────────────────────────────────────────────────────

#[tokio::test(flavor = "current_thread")]
async fn a_version_change_delivers_one_root_replacement_and_advances() {
    let (handle, task) = open_session().await;
    let id = DocumentId::from_raw(7);
    let attached = attach(&handle, loaded(id, 1, count(1))).await;

    // The observer holds a v1 shape: a v2 delta would target the wrong shape,
    // so it receives the whole new value as a root replacement
    // (`session.ts:939-941`).
    attach_and_deliver(
        &handle,
        &attached,
        publication(
            2,
            document_change(
                id,
                Some(2),
                Some(object(json!({ "count": 3, "added": true }))),
                vec![Op::Set(vec![Seg::Key("count".to_owned())], json!(3))],
            ),
        ),
    )
    .await;

    let delivered = frames(&attached);
    assert_eq!(delivered.len(), 1);
    assert_eq!(
        delivered[0].ops,
        vec![Op::Replace(json!({ "count": 3, "added": true }))]
    );
    assert_eq!(version(&attached), 2);
    // The attachment reports the version it is now hydrating at.
    assert_eq!(attached.host().version, 2);

    // A second change at the new version passes through exactly again.
    let ops = vec![Op::Set(vec![Seg::Key("count".to_owned())], json!(4))];
    attach_and_deliver(
        &handle,
        &attached,
        publication(
            3,
            document_change(
                id,
                Some(2),
                Some(object(json!({ "count": 4, "added": true }))),
                ops.clone(),
            ),
        ),
    )
    .await;
    let delivered = frames(&attached);
    assert_eq!(delivered.len(), 1);
    assert_eq!(delivered[0].ops, ops);
    assert_eq!(version(&attached), 2);

    handle.close().await.expect("close");
    let _ = task.await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_migration_only_base_delivers_nothing() {
    let (handle, task) = open_session().await;
    let id = DocumentId::from_raw(7);
    let attached = attach(&handle, loaded(id, 1, count(1))).await;

    // `session.ts:855-856` — the base carries no operations for an observer
    // already hydrated at this version.
    attach_and_deliver(
        &handle,
        &attached,
        publication(2, document_change(id, Some(1), Some(count(1)), Vec::new())),
    )
    .await;

    assert!(frames(&attached).is_empty());
    assert_eq!(attached.source.value(), Some(count(1)));

    handle.close().await.expect("close");
    let _ = task.await;
}

// ── retirement ─────────────────────────────────────────────────────────────

#[tokio::test(flavor = "current_thread")]
async fn retirement_delivers_the_terminal_frame_and_unpins() {
    let (handle, task) = open_session().await;
    let id = DocumentId::from_raw(7);
    let attached = attach(&handle, loaded(id, 1, count(1))).await;
    assert!(is_attached(&handle, id).await);

    attach_and_deliver(
        &handle,
        &attached,
        publication(2, document_change(id, None, None, Vec::new())),
    )
    .await;

    let delivered = frames(&attached);
    assert_eq!(delivered.len(), 1);
    // `observation.ts:19` — the canonical retirement update.
    assert_eq!(delivered[0].ops, vec![Op::Replace(Value::Null)]);
    assert_eq!(delivered[0].value, None);
    // `session.ts:857-859` — a retired incarnation pins nothing, and the
    // observer keeps its last value.
    assert!(!is_attached(&handle, id).await);

    // Nothing follows retirement.
    attach_and_deliver(
        &handle,
        &attached,
        publication(
            3,
            document_change(
                id,
                Some(1),
                Some(count(5)),
                vec![Op::Set(vec![Seg::Key("count".to_owned())], json!(5))],
            ),
        ),
    )
    .await;
    assert!(frames(&attached).is_empty());

    handle.close().await.expect("close");
    let _ = task.await;
}

// ── detach ─────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "current_thread")]
async fn detach_removes_the_listener_and_stops_delivery() {
    let (handle, task) = open_session().await;
    let id = DocumentId::from_raw(7);
    let attached = attach(&handle, loaded(id, 1, count(1))).await;

    detach_on_line(&handle, &attached).await;
    assert_eq!(listener_count(&handle).await, 0);
    assert!(!is_attached(&handle, id).await);

    attach_and_deliver(
        &handle,
        &attached,
        publication(
            2,
            document_change(
                id,
                Some(1),
                Some(count(2)),
                vec![Op::Set(vec![Seg::Key("count".to_owned())], json!(2))],
            ),
        ),
    )
    .await;
    assert!(frames(&attached).is_empty());
    assert_eq!(attached.source.value(), Some(count(1)));

    // Detach is idempotent.
    detach_on_line(&handle, &attached).await;
    assert!(!is_attached(&handle, id).await);

    handle.close().await.expect("close");
    let _ = task.await;
}

#[tokio::test(flavor = "current_thread")]
async fn repeated_attach_detach_releases_every_close_listener() {
    let (handle, task) = open_session().await;
    let id = DocumentId::from_raw(7);

    // R1-L3-005: the close listener captures this attachment's committed
    // document snapshot. Each attach/detach cycle must release it again
    // instead of retaining it until the session closes.
    for round in 0..16 {
        let attached = attach(&handle, loaded(id, 1, count(round))).await;
        assert_eq!(
            close_listeners(&handle).await,
            1,
            "round {round}: a live attachment has one close listener"
        );
        detach_on_line(&handle, &attached).await;
        assert_eq!(
            close_listeners(&handle).await,
            0,
            "round {round}: detach removes the close listener"
        );
        drop(attached);
    }
    assert_eq!(listener_count(&handle).await, 0);

    handle.close().await.expect("close");
    let _ = task.await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_detached_observer_is_not_ended_by_a_later_close() {
    let (handle, task) = open_session().await;
    let id = DocumentId::from_raw(7);
    let attached = attach(&handle, loaded(id, 1, count(1))).await;

    detach_on_line(&handle, &attached).await;
    handle.close().await.expect("close");
    let _ = task.await;

    // The removed close listener must not run at close: a detached observer
    // belongs to the host, not to the session.
    assert_eq!(ended(&attached), None);
}

// ── close ──────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "current_thread")]
async fn close_ends_the_observer_with_session_closed() {
    let (handle, task) = open_session().await;
    let id = DocumentId::from_raw(7);
    let attached = attach(&handle, loaded(id, 1, count(1))).await;
    assert_eq!(ended(&attached), None);

    handle.close().await.expect("close");
    let _ = task.await;

    // `session.ts:861` -> `observation.ts:220-225`.
    assert_eq!(ended(&attached), Some(WatchEnd::SessionClosed));
    // The observer keeps its value; the session no longer delivers.
    assert_eq!(attached.source.value(), Some(count(1)));
    assert!(frames(&attached).is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn a_failed_session_ends_the_observer_with_session_failed() {
    let (handle, task) = open_session().await;
    let id = DocumentId::from_raw(7);
    let attached = attach(&handle, loaded(id, 1, count(1))).await;

    handle
        .maintain(|inner| inner.fail(StorageError::invalid("storage broke")))
        .await
        .expect("maintain");
    handle.close().await.expect("close");
    let _ = task.await;

    assert!(matches!(ended(&attached), Some(WatchEnd::SessionFailed(_))));
}

// ── watch ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn stopping_a_document_watch_removes_its_close_listener() {
    let harness = open_harness().await;
    let ctx = Context::background();
    let conversation = harness
        .create_conversation(ConversationOwnership::Ownerless, ctx.clone())
        .await
        .expect("create conversation")
        .id();
    let resolved = provider_address(conversation);
    let definition = provider_doc();
    let baseline = close_listeners(harness.session().handle()).await;

    // R1-L3-005: each watch registers a close listener that captures the watch
    // state (including its current value). Stop and drop must release it.
    for round in 0..16 {
        let watch = harness
            .watch_doc(&resolved, &definition, &ctx, |_| true)
            .await
            .expect("watch")
            .expect("an incarnation is live");
        assert_eq!(
            close_listeners(harness.session().handle()).await,
            baseline + 1,
            "round {round}: a live watch has one close listener"
        );
        watch.stop();
        assert_eq!(
            close_listeners(harness.session().handle()).await,
            baseline,
            "round {round}: stop removes the close listener"
        );
        drop(watch);
    }

    harness.close().await.expect("close");
}

#[tokio::test]
async fn an_unstopped_watch_is_ended_by_the_session_close() {
    let harness = open_harness().await;
    let ctx = Context::background();
    let conversation = harness
        .create_conversation(ConversationOwnership::Ownerless, ctx.clone())
        .await
        .expect("create conversation")
        .id();
    let watch = harness
        .watch_doc(
            &provider_address(conversation),
            &provider_doc(),
            &ctx,
            |_| true,
        )
        .await
        .expect("watch")
        .expect("an incarnation is live");
    assert!(!watch.is_stopped());

    harness.close().await.expect("close");

    // The close listener still runs for a live watch: removing it on stop must
    // not weaken the close path.
    assert!(watch.is_stopped());
}

// ── P2-ADOPT2-01: a subscription takes effect at commit start ──────────────

/// A [`MemoryStorage`] whose next commit or `find_document` read parks until
/// the test releases it.
///
/// The regression tests use it to keep a commit inside a storage await while
/// they call `Session::subscribe_commits` from the test task. The commit
/// started before that call, so its listener snapshot must not include the new
/// subscriber: the subscriber either gets the complete publication (if the
/// implementation chose to include it) or nothing for that commit — never a
/// frame whose document changes were skipped because the gate was sampled
/// before it arrived.
struct BlockingStorage {
    inner: Arc<MemoryStorage>,
    block_commit: AtomicBool,
    block_find_document: AtomicBool,
    entered: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    release: tokio::sync::Notify,
}

impl BlockingStorage {
    fn new(inner: Arc<MemoryStorage>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            block_commit: AtomicBool::new(false),
            block_find_document: AtomicBool::new(false),
            entered: Mutex::new(None),
            release: tokio::sync::Notify::new(),
        })
    }

    /// Park the next `commit`; the receiver fires once the line is inside it.
    fn block_next_commit(&self) -> tokio::sync::oneshot::Receiver<()> {
        self.block_commit.store(true, Ordering::Release);
        self.arm()
    }

    /// Park the next `find_document` read, the storage await a deferred
    /// document edit performs inside its commit callback.
    fn block_next_find_document(&self) -> tokio::sync::oneshot::Receiver<()> {
        self.block_find_document.store(true, Ordering::Release);
        self.arm()
    }

    fn arm(&self) -> tokio::sync::oneshot::Receiver<()> {
        let (entered, receiver) = tokio::sync::oneshot::channel();
        *self
            .entered
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(entered);
        receiver
    }

    /// Let a parked operation continue. `notify_one` keeps a permit, so this
    /// is safe even if the operation has not polled `notified()` yet.
    fn release(&self) {
        self.release.notify_one();
    }

    async fn park(&self) {
        if let Some(entered) = self
            .entered
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            let _ = entered.send(());
        }
        self.release.notified().await;
    }
}

impl Storage for BlockingStorage {
    fn commit(&self, writes: Vec<StorageWrite>) -> StorageFuture<'_, Result<Seq, StorageError>> {
        Box::pin(async move {
            if self.block_commit.swap(false, Ordering::AcqRel) {
                self.park().await;
            }
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
        Box::pin(async move {
            if self.block_find_document.swap(false, Ordering::AcqRel) {
                self.park().await;
            }
            self.inner.find_document(address, at).await
        })
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
        self.inner.close()
    }
}

const HOLD_COUNTER_KIND: &str = "lane.adopt3.hold-counter";

fn hold_counter_address(conversation: ConversationId) -> ResolvedAddress {
    ResolvedAddress::new(
        HOLD_COUNTER_KIND,
        DocumentSemantics::ConversationLatest {
            fork: DocumentFork::Initial,
        },
        Some(DocOwner::Conversation(conversation)),
        None,
    )
    .expect("resolved address")
}

fn hold_counter_initial() -> JsonObject {
    let mut value = JsonObject::new();
    value.insert("count".to_owned(), json!(0));
    value
}

fn hold_counter_definition() -> AnyDocDefinition {
    CommonDocDefinition::new(
        HOLD_COUNTER_KIND,
        1,
        DocumentSemantics::ConversationLatest {
            fork: DocumentFork::Initial,
        },
    )
    .expect("definition")
    .behaviour(hold_counter_initial, None, DocCheckpointWhen::never())
}

fn hold_counter_set(draft: &mut JsonObject, value: i64) {
    draft.insert("count".to_owned(), json!(value));
}

/// Collect the publications one subscriber receives during a test.
fn subscriber_sink() -> (
    Arc<Mutex<Vec<CommitPublication>>>,
    pir::durable::session::publication::CommitListener,
) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    let listener: pir::durable::session::publication::CommitListener =
        Arc::new(move |publication: &CommitPublication, _context: &Context| {
            sink.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(publication.clone());
        });
    (seen, listener)
}

/// A commit that started before a subscription must never be delivered to that
/// subscriber as a frame without its document change. The gate is sampled when
/// the commit starts; the fan-out dispatches the same captured subscriber set,
/// so the late subscriber misses this commit and receives the next one whole.
#[tokio::test(flavor = "current_thread")]
async fn subscribing_during_run_commit_never_observes_a_document_less_publication() {
    let memory = Arc::new(MemoryStorage::new());
    let storage = BlockingStorage::new(memory.clone());
    let backend: Arc<dyn Storage> = storage.clone();
    let session = Session::open(backend, SessionOptions::default());

    // Seed a document with no subscriber: the commit must run its fast path.
    facade_doc_set(&session, 1).await;

    let (seen, sink) = subscriber_sink();
    let entered = storage.block_next_commit();
    let committing = session.clone();
    let commit = tokio::spawn(async move { facade_doc_set(&committing, 2).await });

    // The commit is now parked inside `Storage::commit`, i.e. after its
    // listener snapshot was taken and before adoption/dispatch.
    tokio::time::timeout(std::time::Duration::from_secs(10), entered)
        .await
        .expect("commit must reach storage")
        .expect("commit entered");

    session
        .subscribe_commits(sink)
        .expect("subscribe during the commit");
    storage.release();
    commit.await.expect("commit task must not panic");

    for publication in take_publications(&seen) {
        assert!(
            publication
                .changes
                .iter()
                .any(|change| matches!(change, CommitChange::Document(_))),
            "a commit that started before the subscription must not deliver a document-less frame: {publication:?}"
        );
        assert_published_count(&publication, 2);
    }

    // The next commit starts with the subscriber in its snapshot and must
    // deliver the complete document change.
    facade_doc_set(&session, 3).await;
    let after = take_publications(&seen);
    assert_eq!(
        after.len(),
        1,
        "the next commit reaches the subscriber exactly once"
    );
    assert_published_count(&after[0], 3);

    session.close().await.expect("close");
}

/// The same contract for `hold_commit`: a deferred document edit is parked in
/// its callback's storage read (after the commit-start listener snapshot and
/// before the hold publishes). A subscriber arriving there must not receive a
/// document-less hold publication; the next deferred commit reaches it whole.
#[tokio::test(flavor = "current_thread")]
async fn subscribing_during_hold_commit_never_observes_a_document_less_publication() {
    let memory = Arc::new(MemoryStorage::new());
    let storage = BlockingStorage::new(memory.clone());
    let backend: Arc<dyn Storage> = storage.clone();
    let session = Session::open(backend, SessionOptions::default());
    let ctx = Context::background();

    let conversation = session
        .create_conversation(ConversationOwnership::Ownerless, None, ctx.clone())
        .await
        .expect("create conversation");
    let address = hold_counter_address(conversation.id);
    let definition = hold_counter_definition();
    session
        .doc(address.clone(), definition.clone(), ctx.clone(), |draft| {
            hold_counter_set(draft, 0)
        })
        .await
        .expect("seed holdable document");

    let (seen, sink) = subscriber_sink();
    let entered = storage.block_next_find_document();
    let committing = session.clone();
    let address_for_commit = address.clone();
    let definition_for_commit = definition.clone();
    let ctx_for_commit = ctx.clone();
    let commit = tokio::spawn(async move {
        committing
            .doc_with_options(
                address_for_commit,
                definition_for_commit,
                ctx_for_commit,
                CommitOptions::deferred_within(DeferDeadline::Never),
                |draft| hold_counter_set(draft, 1),
            )
            .await
    });

    // The deferred commit is parked in its callback's storage read: the
    // commit-start listener snapshot is already taken, the hold has not
    // adopted or published yet.
    tokio::time::timeout(std::time::Duration::from_secs(5), entered)
        .await
        .expect("deferred commit must reach storage")
        .expect("commit entered");

    session
        .subscribe_commits(sink)
        .expect("subscribe during the deferred commit");
    storage.release();
    commit
        .await
        .expect("commit task must not panic")
        .expect("the blocked deferred commit succeeds");
    assert!(
        session.handle().has_deferred(),
        "the edit stayed in memory as a hold"
    );

    for publication in take_publications(&seen) {
        assert!(
            publication
                .changes
                .iter()
                .any(|change| matches!(change, CommitChange::Document(_))),
            "a held commit that started before the subscription must not deliver a document-less frame: {publication:?}"
        );
        assert_published_count(&publication, 1);
    }

    // The next deferred commit starts with the subscriber in its snapshot and
    // publishes the complete held revision.
    session
        .doc_with_options(
            address,
            definition,
            ctx.clone(),
            CommitOptions::deferred_within(DeferDeadline::Never),
            |draft| hold_counter_set(draft, 2),
        )
        .await
        .expect("second deferred edit");
    let after = take_publications(&seen);
    assert_eq!(
        after.len(),
        1,
        "the next held commit reaches the subscriber exactly once"
    );
    assert_published_count(&after[0], 2);

    session.close().await.expect("close");
}
