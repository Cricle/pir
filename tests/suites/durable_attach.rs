//! The attach layer: one committed incarnation turned into an observer.
//!
//! Port of upstream `session.ts` `#attachDocument` (`:824-863`) and
//! `observedOperations` (`:930-947`). Every case here drives the real line:
//! a [`SessionInner`] is opened with `line::open`, the attachment is made
//! through `SessionHandle::maintain`, and publications are handed to
//! [`deliver`] on that same line.

use std::sync::Arc;

use serde_json::{json, Map, Value};

use pir::durable::delta::{Op, Seg};
use pir::durable::ids::{DocumentId, Seq};
use pir::durable::session::attach::{attach_state, deliver, detach, AttachHost, Attached};
use pir::durable::session::documents::LoadedDocument;
use pir::durable::session::line::{self, SessionHandle, SessionOptions};
use pir::durable::session::observation::{ObservedFrame, WatchEnd};
use pir::durable::session::publication::{CommitChange, CommitPublication};
use pir::durable::types::{DocumentCommitChange, DocumentRecord, DocumentScope};
use pir::durable::{MemoryStorage, StorageError};

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

async fn attach(handle: &SessionHandle, loaded: LoadedDocument) -> Attached {
    let address = address();
    let (tx, rx) = tokio::sync::oneshot::channel();
    handle
        .maintain(move |inner| {
            let _ = tx.send(attach_state(inner, address, &loaded));
        })
        .await
        .expect("maintain");
    rx.await.expect("attachment reply")
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
