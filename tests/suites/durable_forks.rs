//! Fork document copies (`src/durable/session/forks.rs`).
//!
//! Every test drives the real kernel: `MemoryStorage` behind a
//! [`Session`], a fork created through the facade's `create_conversation`
//! (which runs `prepare_fork_document_copies` and stages the child copies in
//! the same commit), and values read back through the same session. Where the
//! incarnation record itself is the assertion, the shared storage is read
//! directly.
//!
//! Registered in `tests/g_durable.rs` as `mod durable_forks;`.

use std::sync::Arc;

use serde_json::{json, Value};

use pir::durable::session::forks::{copy_write, prepare_fork_document_copies};
use pir::durable::session::line::IntoJobValue;
use pir::durable::session::transaction::EntryDraft;
use pir::durable::session::{
    ConversationOwnership, LoadedDocument, Session, SessionOptions, StagedDocument,
};
use pir::durable::{
    AnyDocDefinition, CommonDocDefinition, Context, ConversationId, ConversationParent,
    ConversationQuery, ConversationRecord, DocDefinition, DocOwner, DocumentAddress, DocumentFork,
    DocumentHistory, DocumentId, DocumentPoint, DocumentScope, DocumentSemantics, EntryId,
    JsonObject, MemoryStorage, ResolvedAddress, Storage, StorageError,
};

// ─── Fixtures ───────────────────────────────────────────────────────────────

const CURRENT_KIND: &str = "demo.fork.current";
const INITIAL_KIND: &str = "demo.fork.initial";
const REWINDABLE_KIND: &str = "demo.fork.rewindable";

/// Latest-only document copied from the parent's current state.
const CURRENT: DocumentSemantics = DocumentSemantics::ConversationLatest {
    fork: DocumentFork::Current,
};
/// Latest-only document the child starts fresh from `initial()`.
const INITIAL: DocumentSemantics = DocumentSemantics::ConversationLatest {
    fork: DocumentFork::Initial,
};
/// Rewindable document copied as of the fork entry's commit sequence.
const AS_OF: DocumentSemantics = DocumentSemantics::ConversationRewindable {
    fork: DocumentFork::AsOf,
};

fn open(storage: Arc<MemoryStorage>) -> Session {
    let storage: Arc<dyn Storage> = storage;
    Session::open(storage, SessionOptions::default())
}

fn object(value: Value) -> JsonObject {
    value.as_object().cloned().expect("object literal")
}

/// `initial()` shared by every test definition.
///
/// A plain function (not a closure) because [`DocInitial`] is a function
/// pointer.
fn initial() -> JsonObject {
    object(json!({ "counter": 0, "origin": "initial" }))
}

fn definition(kind: &str, semantics: DocumentSemantics) -> AnyDocDefinition {
    CommonDocDefinition::new(kind, 1, semantics)
        .expect("valid document definition")
        .behaviour(initial, None, None)
}

fn address(
    kind: &str,
    semantics: DocumentSemantics,
    conversation: ConversationId,
) -> ResolvedAddress {
    ResolvedAddress::new(
        kind,
        semantics,
        Some(DocOwner::Conversation(conversation)),
        None,
    )
    .expect("conversation document address")
}

fn storage_address(resolved: &ResolvedAddress) -> DocumentAddress {
    resolved.address.clone()
}

fn entry_draft(text: &str) -> EntryDraft {
    EntryDraft {
        kind: "user".to_owned(),
        model: Some(vec![json!({ "role": "user", "content": text })]),
        data: Some(json!({ "text": text })),
        head: None,
        edits: None,
    }
}

/// Create or edit the document at `resolved`, setting `counter`.
async fn set_counter(
    session: &Session,
    resolved: &ResolvedAddress,
    semantics: DocumentSemantics,
    counter: i64,
    ctx: &Context,
) {
    let kind = resolved.address.kind.clone();
    session
        .doc(
            resolved.clone(),
            definition(&kind, semantics),
            ctx.clone(),
            move |value| {
                value.insert("counter".to_owned(), json!(counter));
            },
        )
        .await
        .expect("write document");
}

async fn read_document(
    session: &Session,
    resolved: &ResolvedAddress,
    ctx: &Context,
) -> Option<JsonObject> {
    session.snapshot(resolved, ctx).await.expect("snapshot")
}

async fn read_counter(session: &Session, resolved: &ResolvedAddress, ctx: &Context) -> Option<i64> {
    read_document(session, resolved, ctx)
        .await
        .and_then(|value| value.get("counter").and_then(Value::as_i64))
}

/// One ownerless parent conversation.
async fn parent_conversation(session: &Session, ctx: &Context) -> ConversationRecord {
    session
        .create_conversation(ConversationOwnership::Ownerless, None, ctx.clone())
        .await
        .expect("parent conversation")
}

// ─── Fork policies ──────────────────────────────────────────────────────────

#[tokio::test(flavor = "current_thread")]
async fn initial_copies_the_definitions_initial_value_not_the_parents() {
    let storage = Arc::new(MemoryStorage::new());
    let session = open(storage.clone());
    let ctx = Context::background();

    let parent = session
        .create_conversation(ConversationOwnership::Ownerless, None, ctx.clone())
        .await
        .expect("parent conversation");
    let parent_doc = address(INITIAL_KIND, INITIAL, parent.id);
    set_counter(&session, &parent_doc, INITIAL, 5, &ctx).await;
    let fork_entry = session
        .append_entry(parent.id, entry_draft("fork here"), ctx.clone())
        .await
        .expect("fork entry");

    let child = session
        .create_conversation(
            ConversationOwnership::Ownerless,
            Some((parent.id, fork_entry.id)),
            ctx.clone(),
        )
        .await
        .expect("fork conversation");
    let child_doc = address(INITIAL_KIND, INITIAL, child.id);

    // `Initial` forks copy nothing: the child address is empty until the
    // definition first materializes it.
    assert!(
        session
            .snapshot(&child_doc, &ctx)
            .await
            .expect("child snapshot")
            .is_none(),
        "an Initial fork must not copy the parent's document"
    );
    assert!(
        storage
            .find_document(storage_address(&child_doc), DocumentPoint::Current)
            .await
            .expect("find child document")
            .is_none(),
        "no child incarnation exists before the child first reads the document"
    );

    // The first access creates the incarnation from `initial()`, then applies
    // the edit; the parent's current value (5) is never involved.
    let created = session
        .doc(
            child_doc.clone(),
            definition(INITIAL_KIND, INITIAL),
            ctx.clone(),
            |value| {
                value.insert("counter".to_owned(), json!(1));
                value.clone()
            },
        )
        .await
        .expect("materialize child document");
    assert_eq!(
        created.get("origin"),
        Some(&json!("initial")),
        "the child ran the definition's initial()"
    );
    assert_eq!(created.get("counter"), Some(&json!(1)));

    assert_eq!(
        read_counter(&session, &parent_doc, &ctx).await,
        Some(5),
        "the parent keeps its own value"
    );
    assert_eq!(
        read_counter(&session, &child_doc, &ctx).await,
        Some(1),
        "the child has initial+edit, not the parent's current value"
    );

    let child_record = storage
        .find_document(storage_address(&child_doc), DocumentPoint::Current)
        .await
        .expect("find child document")
        .expect("child incarnation");
    assert_eq!(child_record.fork, Some(DocumentFork::Initial));
    assert_eq!(child_record.history, Some(DocumentHistory::Latest));
    assert_eq!(
        child_record.scope,
        DocumentScope::Conversation {
            conversation_id: child.id
        }
    );

    session.close().await.expect("close");
}

#[tokio::test(flavor = "current_thread")]
async fn current_copies_the_parents_committed_value() {
    let storage = Arc::new(MemoryStorage::new());
    let session = open(storage);
    let ctx = Context::background();

    let parent = parent_conversation(&session, &ctx).await;
    let parent_doc = address(CURRENT_KIND, CURRENT, parent.id);
    set_counter(&session, &parent_doc, CURRENT, 5, &ctx).await;
    let fork_entry = session
        .append_entry(parent.id, entry_draft("fork here"), ctx.clone())
        .await
        .expect("fork entry");

    let child = session
        .create_conversation(
            ConversationOwnership::Ownerless,
            Some((parent.id, fork_entry.id)),
            ctx.clone(),
        )
        .await
        .expect("fork conversation");
    let child_doc = address(CURRENT_KIND, CURRENT, child.id);

    assert_eq!(
        read_counter(&session, &child_doc, &ctx).await,
        Some(5),
        "Current copies the parent's committed value"
    );

    // The copy is independent: a later parent edit never reaches the child.
    set_counter(&session, &parent_doc, CURRENT, 7, &ctx).await;
    assert_eq!(read_counter(&session, &parent_doc, &ctx).await, Some(7));
    assert_eq!(
        read_counter(&session, &child_doc, &ctx).await,
        Some(5),
        "the copy froze the value it was created from"
    );

    session.close().await.expect("close");
}

#[tokio::test(flavor = "current_thread")]
async fn as_of_copies_the_stored_state_at_the_fork_entry_not_the_cache() {
    let storage = Arc::new(MemoryStorage::new());
    let session = open(storage.clone());
    let ctx = Context::background();

    let parent = session
        .create_conversation(ConversationOwnership::Ownerless, None, ctx.clone())
        .await
        .expect("parent conversation");
    let parent_doc = address(REWINDABLE_KIND, AS_OF, parent.id);

    // Version 1, then the fork point, then version 2: the two versions are
    // separate storage commits with the fork entry between them.
    set_counter(&session, &parent_doc, AS_OF, 1, &ctx).await;
    let fork_entry = session
        .append_entry(parent.id, entry_draft("fork here"), ctx.clone())
        .await
        .expect("fork entry");
    set_counter(&session, &parent_doc, AS_OF, 2, &ctx).await;
    let _later = session
        .append_entry(parent.id, entry_draft("after the fork"), ctx.clone())
        .await
        .expect("later entry");

    // Load the parent's *current* value into the Session's document cache. An
    // `AsOf` fork that consulted the cache would copy 2; the child must see
    // the stored revision at the entry's commit sequence (1) instead.
    assert_eq!(
        read_counter(&session, &parent_doc, &ctx).await,
        Some(2),
        "the session cache holds the parent's current value"
    );

    let child = session
        .create_conversation(
            ConversationOwnership::Ownerless,
            Some((parent.id, fork_entry.id)),
            ctx.clone(),
        )
        .await
        .expect("fork conversation");
    let child_doc = address(REWINDABLE_KIND, AS_OF, child.id);

    assert_eq!(
        read_counter(&session, &child_doc, &ctx).await,
        Some(1),
        "AsOf reads the stored historical revision, not the live cache"
    );
    assert_eq!(
        read_counter(&session, &parent_doc, &ctx).await,
        Some(2),
        "the parent stays at its current revision"
    );

    // A later parent edit still cannot leak into the copy.
    set_counter(&session, &parent_doc, AS_OF, 3, &ctx).await;
    assert_eq!(
        read_counter(&session, &child_doc, &ctx).await,
        Some(1),
        "the child is pinned to the fork point"
    );

    let child_record = storage
        .find_document(storage_address(&child_doc), DocumentPoint::Current)
        .await
        .expect("find child document")
        .expect("child incarnation");
    assert_eq!(child_record.history, Some(DocumentHistory::Rewindable));
    assert_eq!(child_record.fork, Some(DocumentFork::AsOf));

    session.close().await.expect("close");
}

// ─── Fork-source immutability ───────────────────────────────────────────────

#[tokio::test(flavor = "current_thread")]
async fn a_fork_source_document_cannot_be_written_in_the_fork_transaction() {
    let storage = Arc::new(MemoryStorage::new());
    let session = open(storage.clone());
    let ctx = Context::background();

    let parent = session
        .create_conversation(ConversationOwnership::Ownerless, None, ctx.clone())
        .await
        .expect("parent conversation");
    let parent_doc = address(CURRENT_KIND, CURRENT, parent.id);
    set_counter(&session, &parent_doc, CURRENT, 5, &ctx).await;
    let fork_entry = session
        .append_entry(parent.id, entry_draft("fork here"), ctx.clone())
        .await
        .expect("fork entry");

    let source_address = storage_address(&parent_doc);
    let source_address_id = parent_doc.id.clone();
    let source_id = storage
        .find_document(source_address.clone(), DocumentPoint::Current)
        .await
        .expect("find source document")
        .expect("source exists")
        .id;

    let parent_id = parent.id;
    let entry_id = fork_entry.id;
    let result: Result<ConversationId, StorageError> = session
        .commit(ctx.clone(), move |tx, state| {
            Box::pin(async move {
                let backend = state.storage.storage();

                // Reads first: the fork scan and the source read both run
                // before this transaction stages its first table write.
                let child_id =
                    ConversationId::from_raw(state.storage.write(backend.mint_id()).await?);
                let copies = prepare_fork_document_copies(
                    state.storage,
                    parent_id,
                    entry_id,
                    child_id,
                    tx.context(),
                )
                .await?;
                assert_eq!(copies.len(), 1, "the parent's Current document is copied");
                let stored = state
                    .storage
                    .read(
                        tx.context(),
                        backend.document(source_id, DocumentPoint::Current),
                    )
                    .await?
                    .expect("the fork source is alive");

                // Mutate the fork source in the very transaction that copies
                // it. Assembly must reject the batch.
                let mut draft = stored.value.clone();
                draft.insert("counter".to_owned(), json!(9));
                tx.stage_document(StagedDocument {
                    address: source_address,
                    address_id: source_address_id.clone(),
                    definition: Some(
                        DocDefinition::conversation_latest(CURRENT_KIND, 1, DocumentFork::Current)
                            .expect("identity definition"),
                    ),
                    family: None,
                    value: draft,
                    created: false,
                    loaded: Some(LoadedDocument {
                        address_id: source_address_id.clone(),
                        record: stored.record,
                        stored_version: stored.version,
                        value_version: stored.version,
                        deltas_since_base: stored.deltas_since_base,
                        value: stored.value,
                    }),
                    retire: false,
                });

                // Stage the child conversation and its copy exactly as
                // `Session::create_conversation` does.
                for copy in &copies {
                    tx.stage_fork_source_document(copy.source.id);
                    let raw = state.storage.write(backend.mint_id()).await?;
                    tx.push_write(copy_write(copy, DocumentId::from_raw(raw)));
                }
                tx.stage_conversation(
                    ConversationRecord {
                        id: child_id,
                        parent: Some(ConversationParent {
                            conversation_id: parent_id,
                            at: entry_id,
                        }),
                        owner: None,
                    },
                    Some(parent_id),
                );
                Ok(child_id.into_job_value())
            })
        })
        .await;

    // `forks.rs` registers the source on the transaction and assembly rejects
    // the plan in `reject_fork_source_writes` (`assemble.rs:672`) before
    // `Storage::commit` is reached, so the error is
    // `StorageError::Invalid` with that message.
    let error = result.expect_err("writing a fork source in the fork transaction must be rejected");
    assert_eq!(
        error,
        StorageError::Invalid(format!(
            "Cannot change fork source document {source_id} in the fork transaction"
        ))
    );

    // The rejection is atomic: the conversation and the source edit never
    // reached storage, and the session is still usable.
    assert!(
        !session.is_failed(),
        "an assembly rejection does not fail the session"
    );
    let conversations = session
        .scan_conversations(ConversationQuery::default(), 10, None, &ctx)
        .await
        .expect("scan conversations");
    assert_eq!(
        conversations.items.len(),
        1,
        "the rejected fork stored no conversation"
    );
    assert_eq!(
        read_counter(&session, &parent_doc, &ctx).await,
        Some(5),
        "the rejected batch left the source untouched"
    );

    session.close().await.expect("close");
}

// ─── Copy identity and ownership ────────────────────────────────────────────

#[tokio::test(flavor = "current_thread")]
async fn a_fork_copy_mints_a_new_id_owned_by_the_child_conversation() {
    let storage = Arc::new(MemoryStorage::new());
    let session = open(storage.clone());
    let ctx = Context::background();

    let parent = session
        .create_conversation(ConversationOwnership::Ownerless, None, ctx.clone())
        .await
        .expect("parent conversation");
    let parent_doc = address(CURRENT_KIND, CURRENT, parent.id);
    set_counter(&session, &parent_doc, CURRENT, 5, &ctx).await;
    let fork_entry = session
        .append_entry(parent.id, entry_draft("fork here"), ctx.clone())
        .await
        .expect("fork entry");
    let child = session
        .create_conversation(
            ConversationOwnership::Ownerless,
            Some((parent.id, fork_entry.id)),
            ctx.clone(),
        )
        .await
        .expect("fork conversation");

    let source = storage
        .find_document(storage_address(&parent_doc), DocumentPoint::Current)
        .await
        .expect("find source")
        .expect("source exists");
    let copy_address = DocumentAddress {
        kind: CURRENT_KIND.to_owned(),
        scope: DocumentScope::Conversation {
            conversation_id: child.id,
        },
        key: None,
    };
    let copy = storage
        .find_document(copy_address, DocumentPoint::Current)
        .await
        .expect("find copy")
        .expect("the child inherited the document");

    assert_ne!(
        copy.id, source.id,
        "the copy is a new incarnation, not the parent's record"
    );
    assert_eq!(
        copy.scope,
        DocumentScope::Conversation {
            conversation_id: child.id
        },
        "the copy belongs to the child conversation"
    );
    assert_eq!(copy.kind, source.kind);
    assert_eq!(copy.key, source.key);
    assert_eq!(copy.fork, Some(DocumentFork::Current));
    assert_eq!(copy.history, Some(DocumentHistory::Latest));

    let stored_copy = storage
        .document(copy.id, DocumentPoint::Current)
        .await
        .expect("read copy")
        .expect("copy is alive");
    assert_eq!(stored_copy.value.get("counter"), Some(&json!(5)));

    let child_record = session
        .conversation(child.id, &ctx)
        .await
        .expect("read child conversation")
        .expect("child exists");
    assert_eq!(
        child_record.parent,
        Some(ConversationParent {
            conversation_id: parent.id,
            at: fork_entry.id,
        }),
        "the child records its parent entry"
    );

    session.close().await.expect("close");
}

// ─── Boundaries ─────────────────────────────────────────────────────────────

#[tokio::test(flavor = "current_thread")]
async fn forking_at_a_missing_entry_is_rejected() {
    let storage = Arc::new(MemoryStorage::new());
    let session = open(storage);
    let ctx = Context::background();

    let parent = session
        .create_conversation(ConversationOwnership::Ownerless, None, ctx.clone())
        .await
        .expect("parent conversation");
    let missing = EntryId::from_raw(424_242);

    let error = session
        .create_conversation(
            ConversationOwnership::Ownerless,
            Some((parent.id, missing)),
            ctx.clone(),
        )
        .await
        .expect_err("a fork at a missing entry must be rejected");

    // `prepare_fork_document_copies` (`forks.rs:66`) reports a missing entry
    // as `StorageError::Invalid` with this exact message.
    match &error {
        StorageError::Invalid(message) => assert_eq!(
            message,
            &format!(
                "Fork entry {missing} is not visible from conversation {}",
                parent.id
            )
        ),
        other => panic!("expected StorageError::Invalid, got {other:?}"),
    }
    assert!(
        !session.is_failed(),
        "a missing fork entry is a callback error, not a storage failure"
    );
    let conversations = session
        .scan_conversations(ConversationQuery::default(), 10, None, &ctx)
        .await
        .expect("scan conversations");
    assert_eq!(
        conversations.items.len(),
        1,
        "the rejected fork stored no conversation"
    );

    session.close().await.expect("close");
}

#[tokio::test(flavor = "current_thread")]
async fn forking_before_the_document_existed_copies_nothing() {
    let storage = Arc::new(MemoryStorage::new());
    let session = open(storage.clone());
    let ctx = Context::background();

    let parent = session
        .create_conversation(ConversationOwnership::Ownerless, None, ctx.clone())
        .await
        .expect("parent conversation");
    // The fork point precedes the document's creation commit: there is no
    // stored revision the `AsOf` copy could select.
    let before = session
        .append_entry(parent.id, entry_draft("before the document"), ctx.clone())
        .await
        .expect("early entry");
    let parent_doc = address(REWINDABLE_KIND, AS_OF, parent.id);
    set_counter(&session, &parent_doc, AS_OF, 5, &ctx).await;
    let _after = session
        .append_entry(parent.id, entry_draft("after the document"), ctx.clone())
        .await
        .expect("later entry");

    let child = session
        .create_conversation(
            ConversationOwnership::Ownerless,
            Some((parent.id, before.id)),
            ctx.clone(),
        )
        .await
        .expect("fork conversation");
    let child_doc = address(REWINDABLE_KIND, AS_OF, child.id);

    assert!(
        session
            .snapshot(&child_doc, &ctx)
            .await
            .expect("child snapshot")
            .is_none(),
        "a document created after the fork point is not inherited"
    );
    assert!(storage
        .find_document(storage_address(&child_doc), DocumentPoint::Current)
        .await
        .expect("find child document")
        .is_none());

    session.close().await.expect("close");
}

#[tokio::test(flavor = "current_thread")]
async fn forking_after_the_document_was_retired_copies_nothing() {
    let storage = Arc::new(MemoryStorage::new());
    let session = open(storage.clone());
    let ctx = Context::background();

    let parent = session
        .create_conversation(ConversationOwnership::Ownerless, None, ctx.clone())
        .await
        .expect("parent conversation");
    let parent_doc = address(REWINDABLE_KIND, AS_OF, parent.id);
    set_counter(&session, &parent_doc, AS_OF, 5, &ctx).await;
    session
        .retire_doc(parent_doc.clone(), ctx.clone())
        .await
        .expect("retire parent document");
    let after_retirement = session
        .append_entry(parent.id, entry_draft("after retirement"), ctx.clone())
        .await
        .expect("entry after retirement");

    let child = session
        .create_conversation(
            ConversationOwnership::Ownerless,
            Some((parent.id, after_retirement.id)),
            ctx.clone(),
        )
        .await
        .expect("fork conversation");
    let child_doc = address(REWINDABLE_KIND, AS_OF, child.id);

    assert_eq!(
        read_document(&session, &parent_doc, &ctx).await,
        None,
        "the parent document is retired"
    );
    assert!(
        session
            .snapshot(&child_doc, &ctx)
            .await
            .expect("child snapshot")
            .is_none(),
        "a document retired before the fork point is not inherited"
    );
    assert!(storage
        .find_document(storage_address(&child_doc), DocumentPoint::Current)
        .await
        .expect("find child document")
        .is_none());

    session.close().await.expect("close");
}
