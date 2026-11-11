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

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use serde_json::{json, Value};

use pir::durable::session::forks::{copy_write, prepare_fork_document_copies};
use pir::durable::session::line::IntoJobValue;
use pir::durable::session::transaction::{EntryDraft, TaskOptions, TaskOwnership};
use pir::durable::session::{
    ConversationOwnership, LoadedDocument, Session, SessionOptions, StagedDocument,
};
use pir::durable::{
    AnyDocDefinition, CommonDocDefinition, Context, ConversationId, ConversationParent,
    ConversationQuery, ConversationRecord, DocCheckpointWhen, DocCheckpointWhenExt, DocDefinition,
    DocFamilyDefinition, DocOwner, DocumentAddress, DocumentContent, DocumentCreate, DocumentError,
    DocumentFork, DocumentHistory, DocumentId, DocumentPoint, DocumentPointQuery, DocumentQuery,
    DocumentScope, DocumentSemantics, EntryId, EntryQuery, EntryRecord, JsonObject, MemoryStorage,
    ResolvedAddress, Storage, StorageError, StorageWrite, TaskDefinition,
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
        .behaviour(initial, None, DocCheckpointWhen::never())
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

#[tokio::test(flavor = "current_thread")]
async fn a_fork_rejects_duplicate_as_of_and_current_sources_for_one_child_address() {
    let storage = Arc::new(MemoryStorage::new());
    let session = open(storage);
    let ctx = Context::background();
    let parent = parent_conversation(&session, &ctx).await;
    let kind = "demo.fork.duplicate-policy";
    let as_of = address(kind, AS_OF, parent.id);
    let current = address(kind, CURRENT, parent.id);

    // The as-of source exists at the fork point and is retired afterwards.
    set_counter(&session, &as_of, AS_OF, 1, &ctx).await;
    let fork_at = session
        .append_entry(parent.id, entry_draft("point"), ctx.clone())
        .await
        .expect("fork point")
        .id;
    session
        .retire_doc(as_of.clone(), ctx.clone())
        .await
        .expect("retire as-of source");

    // The current source is a second incarnation of the same child address.
    set_counter(&session, &current, CURRENT, 2, &ctx).await;

    let error = session
        .create_conversation(
            ConversationOwnership::Ownerless,
            Some((parent.id, fork_at)),
            ctx.clone(),
        )
        .await
        .expect_err("two source policies for one child address must be rejected");
    assert!(
        error
            .to_string()
            .contains("Fork selects multiple source documents"),
        "unexpected error: {error}"
    );
    assert!(!session.is_failed(), "pre-admission rejection is not fatal");

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
        read_counter(&session, &current, &ctx).await,
        Some(2),
        "the rejected fork left the current source untouched"
    );
    let other = session
        .create_conversation(ConversationOwnership::Ownerless, None, ctx.clone())
        .await
        .expect("session remains usable");
    assert_ne!(other.id, parent.id);

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

// ─── Same-commit, paging and pre-admission boundaries ──────────────────────

/// Upstream `session-forks.test.ts`: "uses final document state from the fork
/// entry commit while excluding later same-commit entries".
///
/// One commit appends `included`, writes the document's final state and then
/// appends `excluded`. The fork point is `included`, whose commit sequence is
/// therefore the whole commit: the as-of copy must see the final document
/// state, while the child transcript must stop at `included`.
#[tokio::test(flavor = "current_thread")]
async fn a_fork_copies_the_fork_commit_state_and_stops_at_the_fork_entry() {
    let storage = Arc::new(MemoryStorage::new());
    let session = open(storage);
    let ctx = Context::background();
    let parent = parent_conversation(&session, &ctx).await;
    let kind = "demo.fork.same-commit";
    let parent_doc = address(kind, AS_OF, parent.id);
    let parent_id = parent.id;
    let draft_address = parent_doc.address.clone();
    let draft_address_id = parent_doc.id.clone();

    let (fork_at, excluded): (EntryId, EntryId) = session
        .commit(ctx.clone(), move |tx, state| {
            Box::pin(async move {
                let backend = state.storage.storage();
                let included_id = EntryId::from_raw(state.storage.write(backend.mint_id()).await?);
                tx.push_write(StorageWrite::Entry {
                    value: EntryRecord {
                        id: included_id,
                        conversation_id: parent_id,
                        kind: "included".to_owned(),
                        model: None,
                        data: None,
                        head: None,
                        edits: None,
                        by_task_id: None,
                    },
                });
                tx.stage_document(StagedDocument {
                    address: draft_address,
                    address_id: draft_address_id,
                    definition: Some(
                        DocDefinition::conversation_rewindable(kind, 1, DocumentFork::AsOf)
                            .expect("identity definition"),
                    ),
                    family: None,
                    value: object(json!({ "counter": 42, "origin": "commit" })),
                    created: true,
                    loaded: None,
                    retire: false,
                });
                let excluded_id = EntryId::from_raw(state.storage.write(backend.mint_id()).await?);
                tx.push_write(StorageWrite::Entry {
                    value: EntryRecord {
                        id: excluded_id,
                        conversation_id: parent_id,
                        kind: "excluded".to_owned(),
                        model: None,
                        data: None,
                        head: None,
                        edits: None,
                        by_task_id: None,
                    },
                });
                Ok((included_id, excluded_id).into_job_value())
            })
        })
        .await
        .expect("one commit with two entries and a document write");

    let child = session
        .create_conversation(
            ConversationOwnership::Ownerless,
            Some((parent.id, fork_at)),
            ctx.clone(),
        )
        .await
        .expect("fork conversation");
    let child_doc = address(kind, AS_OF, child.id);
    assert_eq!(
        read_counter(&session, &child_doc, &ctx).await,
        Some(42),
        "the as-of copy reads the final state of the fork entry's commit"
    );
    let visible = session
        .scan_entries(
            EntryQuery {
                conversation_id: child.id,
                min_entry_id: None,
                max_entry_id: None,
                order: None,
            },
            10,
            None,
            &ctx,
        )
        .await
        .expect("child entries");
    let ids: Vec<EntryId> = visible.items.iter().map(|entry| entry.id).collect();
    assert!(
        ids.contains(&fork_at),
        "the fork entry is inherited: {ids:?}"
    );
    assert!(
        !ids.contains(&excluded),
        "a later entry of the same commit is not inherited: {ids:?}"
    );

    session.close().await.expect("close");
}

/// Upstream `session-forks.test.ts`: "copies every family member across
/// storage scan pages".
///
/// The fork scan pages at 256 records; 260 members prove the loop follows the
/// cursor instead of stopping at the first page.
#[tokio::test(flavor = "current_thread")]
async fn a_fork_copies_every_family_member_across_scan_pages() {
    let storage = Arc::new(MemoryStorage::new());
    let session = open(storage);
    let ctx = Context::background();
    let parent = parent_conversation(&session, &ctx).await;
    let kind = "demo.fork.pagination";
    let fork_at = session
        .append_entry(parent.id, entry_draft("point"), ctx.clone())
        .await
        .expect("fork point")
        .id;
    let parent_id = parent.id;
    let family = DocFamilyDefinition::conversation_latest(kind, 1, DocumentFork::Current)
        .expect("family definition");

    let _: () = session
        .commit(ctx.clone(), move |tx, _state| {
            Box::pin(async move {
                for index in 0..260_i64 {
                    let key = format!("member-{index}");
                    let resolved = ResolvedAddress::new(
                        kind,
                        CURRENT,
                        Some(DocOwner::Conversation(parent_id)),
                        Some(key.clone()),
                    )
                    .expect("family member address");
                    tx.stage_document(StagedDocument {
                        address_id: resolved.id,
                        address: resolved.address,
                        definition: None,
                        family: Some(family.clone()),
                        value: object(json!({ "counter": index })),
                        created: true,
                        loaded: None,
                        retire: false,
                    });
                }
                Ok(().into_job_value())
            })
        })
        .await
        .expect("260 family members");

    let child = session
        .create_conversation(
            ConversationOwnership::Ownerless,
            Some((parent.id, fork_at)),
            ctx.clone(),
        )
        .await
        .expect("fork conversation");

    for index in [0_i64, 259] {
        let member = ResolvedAddress::new(
            kind,
            CURRENT,
            Some(DocOwner::Conversation(child.id)),
            Some(format!("member-{index}")),
        )
        .expect("child member address");
        assert_eq!(
            read_counter(&session, &member, &ctx).await,
            Some(index),
            "member-{index} is copied across the page boundary"
        );
    }

    session.close().await.expect("close");
}

/// Upstream `session-forks.test.ts`: a fork rejected before admission leaves
/// no effect.
///
/// A persisted `(latest, asOf)` record is a definition bug, but storage can
/// hold it. Fork planning rejects it before any write; the conversation set and
/// the source document stay untouched, and the Session remains usable.
#[tokio::test(flavor = "current_thread")]
async fn a_pre_admission_fork_failure_leaves_no_effect() {
    let storage = Arc::new(MemoryStorage::new());
    let session = open(storage);
    let ctx = Context::background();
    let parent = parent_conversation(&session, &ctx).await;
    let kind = "demo.fork.invalid-source";
    let parent_doc = address(kind, CURRENT, parent.id);
    let parent_id = parent.id;

    let _: () = session
        .commit(ctx.clone(), move |tx, state| {
            Box::pin(async move {
                let backend = state.storage.storage();
                let id = DocumentId::from_raw(state.storage.write(backend.mint_id()).await?);
                tx.push_write(StorageWrite::DocumentCreate {
                    record: DocumentCreate {
                        id,
                        kind: kind.to_owned(),
                        key: None,
                        scope: DocumentScope::Conversation {
                            conversation_id: parent_id,
                        },
                        history: Some(DocumentHistory::Latest),
                        fork: Some(DocumentFork::AsOf),
                    },
                    content: DocumentContent::Base {
                        version: 1,
                        value: object(json!({ "counter": 7 })),
                    },
                });
                Ok(().into_job_value())
            })
        })
        .await
        .expect("persist a latest-only asOf document");

    let fork_at = session
        .append_entry(parent.id, entry_draft("point"), ctx.clone())
        .await
        .expect("fork point")
        .id;
    let error = session
        .create_conversation(
            ConversationOwnership::Ownerless,
            Some((parent.id, fork_at)),
            ctx.clone(),
        )
        .await
        .expect_err("a latest-only asOf source cannot be copied");
    assert!(
        error
            .to_string()
            .contains("latest-only history and cannot fork asOf"),
        "unexpected error: {error}"
    );
    assert!(!session.is_failed(), "pre-admission rejection is not fatal");

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
        Some(7),
        "the fork source is untouched"
    );

    let other = session
        .create_conversation(ConversationOwnership::Ownerless, None, ctx.clone())
        .await
        .expect("session remains usable");
    assert_ne!(other.id, parent.id);

    session.close().await.expect("close");
}

// ─── Ancestry and stored-version semantics ──────────────────────────────────

/// Upstream "selects the entry-owning ancestor for as-of copies and the
/// immediate parent for current copies".
///
/// A fork at an *inherited* entry must read `asOf` documents from the
/// conversation that owns that entry (the grandparent), while `current`
/// documents always come from the immediate parent's current state.
#[tokio::test(flavor = "current_thread")]
async fn a_fork_reads_as_of_from_the_entry_owner_and_current_from_the_parent() {
    const AS_OF_KIND: &str = "demo.fork.ancestry.as-of";
    const CURRENT_KIND: &str = "demo.fork.ancestry.current";

    let storage = Arc::new(MemoryStorage::new());
    let session = open(storage.clone());
    let ctx = Context::background();

    let root = parent_conversation(&session, &ctx).await;
    let root_as_of = address(AS_OF_KIND, AS_OF, root.id);
    let root_current = address(CURRENT_KIND, CURRENT, root.id);
    set_counter(&session, &root_as_of, AS_OF, 11, &ctx).await;
    set_counter(&session, &root_current, CURRENT, 12, &ctx).await;
    let inherited = session
        .append_entry(root.id, entry_draft("inherited point"), ctx.clone())
        .await
        .expect("inherited entry");

    let parent = session
        .create_conversation(
            ConversationOwnership::Ownerless,
            Some((root.id, inherited.id)),
            ctx.clone(),
        )
        .await
        .expect("parent fork");
    let parent_as_of = address(AS_OF_KIND, AS_OF, parent.id);
    let parent_current = address(CURRENT_KIND, CURRENT, parent.id);
    set_counter(&session, &parent_as_of, AS_OF, 21, &ctx).await;
    set_counter(&session, &parent_current, CURRENT, 22, &ctx).await;
    let own_entry = session
        .append_entry(parent.id, entry_draft("parent point"), ctx.clone())
        .await
        .expect("parent entry");

    // The fork point belongs to the root: `asOf` reads the root at that
    // commit, `current` reads the parent's latest state.
    let inherited_child = session
        .create_conversation(
            ConversationOwnership::Ownerless,
            Some((parent.id, inherited.id)),
            ctx.clone(),
        )
        .await
        .expect("fork at inherited entry");
    assert_eq!(
        read_counter(
            &session,
            &address(AS_OF_KIND, AS_OF, inherited_child.id),
            &ctx
        )
        .await,
        Some(11),
        "asOf copies the entry-owning ancestor (root), not the parent"
    );
    assert_eq!(
        read_counter(
            &session,
            &address(CURRENT_KIND, CURRENT, inherited_child.id),
            &ctx
        )
        .await,
        Some(22),
        "current copies the immediate parent"
    );

    // A fork at an entry the parent owns reads the parent for both policies.
    let own_child = session
        .create_conversation(
            ConversationOwnership::Ownerless,
            Some((parent.id, own_entry.id)),
            ctx.clone(),
        )
        .await
        .expect("fork at parent entry");
    assert_eq!(
        read_counter(&session, &address(AS_OF_KIND, AS_OF, own_child.id), &ctx).await,
        Some(21),
        "asOf copies the parent when it owns the entry"
    );
    assert_eq!(
        read_counter(
            &session,
            &address(CURRENT_KIND, CURRENT, own_child.id),
            &ctx
        )
        .await,
        Some(22),
        "current still copies the parent"
    );

    session.close().await.expect("close");
}

static FORK_MIGRATIONS: AtomicUsize = AtomicUsize::new(0);

fn migrate_fork_v3(from_version: i64, value: Value) -> Result<Value, DocumentError> {
    assert_eq!(from_version, 1, "only version 1 is stored");
    FORK_MIGRATIONS.fetch_add(1, Ordering::SeqCst);
    let mut object = value.as_object().cloned().unwrap_or_default();
    object.insert("migrated".to_owned(), json!(true));
    Ok(Value::Object(object))
}

fn migration_definition(kind: &str) -> AnyDocDefinition {
    CommonDocDefinition::new(kind, 3, AS_OF)
        .expect("valid version-3 definition")
        .behaviour(initial, Some(migrate_fork_v3), DocCheckpointWhen::never())
}

/// Upstream "copies the stored value and version without consulting migration
/// definitions or migrated caches".
///
/// The fork copies the source's stored revision at its stored version: the
/// definition is applied only when the child is later materialized.
#[tokio::test(flavor = "current_thread")]
async fn a_fork_copies_the_stored_version_and_leaves_migration_to_the_definition() {
    const KIND: &str = "demo.fork.stored-version";

    let storage = Arc::new(MemoryStorage::new());
    let session = open(storage.clone());
    let ctx = Context::background();
    let parent = parent_conversation(&session, &ctx).await;
    let parent_doc = address(KIND, AS_OF, parent.id);
    set_counter(&session, &parent_doc, AS_OF, 1, &ctx).await;
    // Warm the session cache at the stored (version 1) value.
    assert_eq!(read_counter(&session, &parent_doc, &ctx).await, Some(1));
    let fork_entry = session
        .append_entry(parent.id, entry_draft("point"), ctx.clone())
        .await
        .expect("fork point");

    FORK_MIGRATIONS.store(0, Ordering::SeqCst);
    let child = session
        .create_conversation(
            ConversationOwnership::Ownerless,
            Some((parent.id, fork_entry.id)),
            ctx.clone(),
        )
        .await
        .expect("fork conversation");
    assert_eq!(
        FORK_MIGRATIONS.load(Ordering::SeqCst),
        0,
        "the fork itself never runs the definition migration"
    );

    let child_doc = address(KIND, AS_OF, child.id);
    let record = storage
        .find_document(storage_address(&child_doc), DocumentPoint::Current)
        .await
        .expect("find child document")
        .expect("child incarnation");
    let stored = storage
        .document(record.id, DocumentPoint::Current)
        .await
        .expect("read child document")
        .expect("stored revision");
    assert_eq!(stored.version, 1, "the copy keeps the source version");
    assert_eq!(stored.value.get("counter"), Some(&json!(1)));
    assert!(
        stored.value.get("migrated").is_none(),
        "the copied value is the stored value, not a migrated one"
    );

    // Materializing through the version-3 definition migrates exactly once and
    // commits a version-3 base.
    let seen = session
        .doc(
            child_doc.clone(),
            migration_definition(KIND),
            ctx.clone(),
            |value| value.clone(),
        )
        .await
        .expect("migrate child document");
    assert_eq!(
        seen.get("migrated"),
        Some(&json!(true)),
        "the edit sees the migrated value"
    );
    assert_eq!(FORK_MIGRATIONS.load(Ordering::SeqCst), 1);
    let after = session
        .snapshot(&child_doc, &ctx)
        .await
        .expect("snapshot")
        .expect("child document");
    assert_eq!(after.get("counter"), Some(&json!(1)));
    assert_eq!(after.get("migrated"), Some(&json!(true)));

    session.close().await.expect("close");
}

/// Upstream "copies only conversation documents, leaving Session and task
/// documents in their original scopes".
#[tokio::test(flavor = "current_thread")]
async fn a_fork_copies_only_conversation_documents() {
    const COPIED_KIND: &str = "demo.fork.scope.conversation";
    const SESSION_KIND: &str = "demo.fork.scope.session";
    const TASK_KIND: &str = "demo.fork.scope.task";

    let storage = Arc::new(MemoryStorage::new());
    let session = open(storage.clone());
    let ctx = Context::background();
    let parent = parent_conversation(&session, &ctx).await;

    let task_definition = TaskDefinition::define("demo.fork.scope.work", 1, vec!["run".to_owned()])
        .expect("task definition");
    let task_id = session
        .create_task(
            &task_definition,
            json!({ "n": 1 }),
            TaskOptions {
                trace: None,
                conversation_id: Some(parent.id),
                ownership: TaskOwnership::Conversation,
                background: false,
                abandon_on_restart: false,
                submission_ids: Vec::new(),
            },
            ctx.clone(),
        )
        .await
        .expect("task");

    let copied = address(COPIED_KIND, CURRENT, parent.id);
    set_counter(&session, &copied, CURRENT, 5, &ctx).await;

    let session_address =
        ResolvedAddress::new(SESSION_KIND, DocumentSemantics::Session, None, None)
            .expect("session address");
    session
        .doc(
            session_address.clone(),
            CommonDocDefinition::session(SESSION_KIND, 1)
                .expect("session definition")
                .behaviour(initial, None, DocCheckpointWhen::never()),
            ctx.clone(),
            |value| {
                value.insert("counter".to_owned(), json!(7));
            },
        )
        .await
        .expect("session document");

    let task_address = ResolvedAddress::new(
        TASK_KIND,
        DocumentSemantics::Task,
        Some(DocOwner::Task(task_id)),
        None,
    )
    .expect("task address");
    session
        .doc(
            task_address.clone(),
            CommonDocDefinition::task(TASK_KIND, 1)
                .expect("task document definition")
                .behaviour(initial, None, DocCheckpointWhen::never()),
            ctx.clone(),
            |value| {
                value.insert("counter".to_owned(), json!(9));
            },
        )
        .await
        .expect("task document");

    let fork_entry = session
        .append_entry(parent.id, entry_draft("point"), ctx.clone())
        .await
        .expect("fork point");
    let child = session
        .create_conversation(
            ConversationOwnership::Ownerless,
            Some((parent.id, fork_entry.id)),
            ctx.clone(),
        )
        .await
        .expect("fork conversation");

    assert_eq!(
        read_counter(&session, &address(COPIED_KIND, CURRENT, child.id), &ctx).await,
        Some(5),
        "the conversation document was copied"
    );
    assert_eq!(
        read_counter(&session, &session_address, &ctx).await,
        Some(7),
        "the session document stays in the session scope"
    );
    assert_eq!(
        read_counter(&session, &task_address, &ctx).await,
        Some(9),
        "the task document stays in the task scope"
    );

    let children = storage
        .scan_documents(
            DocumentQuery {
                scope: DocumentScope::Conversation {
                    conversation_id: child.id,
                },
                at: DocumentPointQuery::from(DocumentPoint::Current),
                kind: None,
            },
            100,
            None,
        )
        .await
        .expect("scan child documents");
    let kinds: Vec<&str> = children
        .items
        .iter()
        .map(|record| record.kind.as_str())
        .collect();
    assert_eq!(
        kinds,
        vec![COPIED_KIND],
        "only the conversation document reaches the child scope"
    );

    session.close().await.expect("close");
}
