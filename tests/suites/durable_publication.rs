//! Commit publication of documents: the `adopt` step of the line.
//!
//! Upstream's `#runCommit` plans document writes during assembly, commits the
//! batch, and then *adopts* the plans into publication changes
//! (`session.ts:716-768`, `transaction.ts` `#adopt`). An earlier layout ran
//! assembly inside the facade callback, which meant the line never saw plans
//! and document changes were written but never published — observers stayed
//! blind to every document edit.
//!
//! These tests pin the corrected split: a document commit publishes its exact
//! operations, a version transition publishes a base with no operations, and a
//! retirement publishes the terminal frame. The `pi.live`-style streaming case
//! (a growing string arriving as one append) is the reason the leaf-by-leaf
//! assignment exists, so it is checked here too.

use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use pir::durable::session::{CommitChange, CommitPublication, Session, SessionOptions};
use pir::durable::{
    AnyDocDefinition, CommonDocDefinition, Context, DocumentCommitChange, DocumentSemantics,
    JsonObject, MemoryStorage, ResolvedAddress, Storage,
};

fn open(storage: Arc<MemoryStorage>) -> Session {
    let storage: Arc<dyn Storage> = storage;
    Session::open(storage, SessionOptions::default())
}

/// Collect every publication the session emits.
fn watch(session: &Session) -> Arc<Mutex<Vec<CommitPublication>>> {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    session
        .subscribe_commits(Arc::new(
            move |publication: &CommitPublication, _context: &Context| {
                sink.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(publication.clone());
            },
        ))
        .expect("subscribe");
    seen
}

fn counter_definition() -> AnyDocDefinition {
    CommonDocDefinition::new("counter", 1, DocumentSemantics::Session)
        .expect("definition")
        .behaviour(initial_counter, None, None)
}

fn initial_empty() -> JsonObject {
    JsonObject::new()
}

fn initial_counter() -> JsonObject {
    let mut value = JsonObject::new();
    value.insert("count".to_owned(), json!(0));
    value
}

/// Migration from version 1 keeps the value unchanged.
fn migrate_identity(_from: i64, value: Value) -> Result<Value, pir::durable::DocumentError> {
    Ok(value)
}

fn counter_address() -> ResolvedAddress {
    ResolvedAddress::new("counter", DocumentSemantics::Session, None, None)
        .expect("resolved address")
}

fn counter_value(count: i64) -> JsonObject {
    let mut value = JsonObject::new();
    value.insert("count".to_owned(), json!(count));
    value
}

/// One published document change, flattened for assertions.
#[derive(Clone)]
struct PublishedDocument {
    record: pir::durable::DocumentRecord,
    version: Option<i64>,
    value: Option<JsonObject>,
    ops: Vec<pir::durable::delta::Op>,
}

/// The document changes of one publication, in order.
fn document_changes(publication: &CommitPublication) -> Vec<PublishedDocument> {
    publication
        .changes
        .iter()
        .filter_map(|change| match change {
            CommitChange::Document(DocumentCommitChange::Document {
                record,
                version,
                value,
                ops,
                ..
            }) => Some(PublishedDocument {
                record: record.clone(),
                version: *version,
                value: value.clone(),
                ops: ops.clone(),
            }),
            CommitChange::Document(DocumentCommitChange::Copy { .. }) => None,
            CommitChange::Table(_) => None,
        })
        .collect()
}

#[tokio::test]
async fn a_document_creation_publishes_a_base_with_no_operations() {
    let storage = Arc::new(MemoryStorage::new());
    let session = open(storage);
    let ctx = Context::background();
    let seen = watch(&session);

    session
        .doc(
            counter_address(),
            counter_definition(),
            ctx.clone(),
            |draft| draft.insert("count".to_owned(), json!(1)),
        )
        .await
        .expect("create document");

    let publications = seen
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    assert_eq!(publications.len(), 1, "one commit, one publication");
    let documents = document_changes(&publications[0]);
    assert_eq!(documents.len(), 1, "the creation is published once");
    let created = &documents[0];
    assert!(created.ops.is_empty(), "a creation carries no operations");
    assert_eq!(created.version, Some(1), "the creation is at version 1");
    assert_eq!(
        created.value.as_ref().and_then(|value| value.get("count")),
        Some(&json!(1)),
        "the published value is the committed value"
    );
    assert!(created.record.retired_at.is_none());

    session.close().await.expect("close");
}

#[tokio::test]
async fn a_document_edit_publishes_the_exact_operations() {
    let storage = Arc::new(MemoryStorage::new());
    let session = open(storage);
    let ctx = Context::background();
    let seen = watch(&session);

    // Creation, then one edit, as two commits.
    session
        .doc(
            counter_address(),
            counter_definition(),
            ctx.clone(),
            |draft| draft.insert("count".to_owned(), json!(1)),
        )
        .await
        .expect("create document");
    session
        .doc(
            counter_address(),
            counter_definition(),
            ctx.clone(),
            |draft| {
                draft.insert("count".to_owned(), json!(2));
            },
        )
        .await
        .expect("edit document");

    let publications = seen
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    assert_eq!(publications.len(), 2, "two commits, two publications");
    let edit = document_changes(&publications[1]);
    assert_eq!(edit.len(), 1, "the edit is published once");
    let changed = &edit[0];
    assert_eq!(
        changed.ops.len(),
        1,
        "one leaf changed, so one operation: {:?}",
        changed.ops
    );
    // Replaying the published operations on the previous value reproduces the
    // published new value. That is the contract a watcher applies.
    let mut replayed = Value::Object(counter_value(1));
    pir::durable::apply(&mut replayed, &changed.ops).expect("ops apply");
    assert_eq!(replayed.get("count"), Some(&json!(2)));
    assert_eq!(
        changed.value.as_ref().and_then(|value| value.get("count")),
        Some(&json!(2))
    );

    session.close().await.expect("close");
}

#[tokio::test]
async fn a_growing_string_is_published_as_one_append() {
    // `pi.live` retains a streaming answer's text and relies on the differ
    // emitting an append for the retained head, not a full set of the message.
    let storage = Arc::new(MemoryStorage::new());
    let session = open(storage);
    let ctx = Context::background();
    let seen = watch(&session);

    let address = || {
        ResolvedAddress::new("assistant", DocumentSemantics::Session, None, None)
            .expect("resolved address")
    };
    let definition = || {
        CommonDocDefinition::new("assistant", 1, DocumentSemantics::Session)
            .expect("definition")
            .behaviour(initial_empty, None, None)
    };

    session
        .doc(address(), definition(), ctx.clone(), |draft| {
            draft.insert("text".to_owned(), json!("Hel"));
        })
        .await
        .expect("create");
    session
        .doc(address(), definition(), ctx.clone(), |draft| {
            draft.insert("text".to_owned(), json!("Hello, world"));
        })
        .await
        .expect("grow");

    let publications = seen
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let grow = document_changes(&publications[1]);
    assert_eq!(grow.len(), 1);
    assert_eq!(
        grow[0].ops,
        vec![pir::durable::delta::Op::Append(
            vec![pir::durable::delta::Seg::Key("text".to_owned())],
            "lo, world".to_owned()
        )],
        "a longer string on the same leaf is one append"
    );

    session.close().await.expect("close");
}

#[tokio::test]
async fn a_version_transition_publishes_a_base_with_no_operations() {
    let storage = Arc::new(MemoryStorage::new());
    let session = open(storage);
    let ctx = Context::background();

    session
        .doc(
            counter_address(),
            counter_definition(),
            ctx.clone(),
            |draft| draft.insert("count".to_owned(), json!(1)),
        )
        .await
        .expect("create at version 1");

    // Re-access the same incarnation as version 2 with an unchanged value.
    let seen = watch(&session);
    let v2 = CommonDocDefinition::new("counter", 2, DocumentSemantics::Session)
        .expect("v2 definition")
        .behaviour(initial_counter, Some(migrate_identity), None);
    session
        .doc(counter_address(), v2, ctx.clone(), |_draft| {})
        .await
        .expect("access as version 2");

    let publications = seen
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let transition: Vec<_> = publications.iter().flat_map(document_changes).collect();
    assert_eq!(transition.len(), 1, "the transition is published once");
    assert!(
        transition[0].ops.is_empty(),
        "a version transition stores a base, so it publishes no operations"
    );
    assert_eq!(transition[0].version, Some(2));

    session.close().await.expect("close");
}

#[tokio::test]
async fn a_retirement_publishes_the_terminal_frame() {
    let storage = Arc::new(MemoryStorage::new());
    let session = open(storage);
    let ctx = Context::background();

    session
        .doc(
            counter_address(),
            counter_definition(),
            ctx.clone(),
            |draft| draft.insert("count".to_owned(), json!(1)),
        )
        .await
        .expect("create");

    let seen = watch(&session);
    session
        .retire_doc(counter_address(), ctx.clone())
        .await
        .expect("retire");

    let publications = seen
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    assert_eq!(publications.len(), 1);
    let documents = document_changes(&publications[0]);
    assert_eq!(documents.len(), 1);
    let retired = &documents[0];
    assert!(
        retired.value.is_none(),
        "retirement publishes null, which a watcher reads as `[[\"r\", null]]`"
    );
    assert!(retired.version.is_none());
    assert!(retired.ops.is_empty(), "retirement carries no operations");
    assert_eq!(
        retired.record.retired_at,
        Some(publications[0].seq),
        "retirement is stamped with the commit sequence"
    );

    session.close().await.expect("close");
}
