//! Durable/session lane-E regressions: retention overflow + Infinity, deferred
//! hold deadlines, the version-change base rule, and session-path cycles.
//!
//! Each test pins one fix:
//!
//! * `document_retention_millis(f64::INFINITY)` must mean "keep documents"
//!   (`i64::MAX`), not the ten-minute default, and the document cache's
//!   deadline arithmetic must not overflow on that value;
//! * a deferred commit whose deadline is at the edge of the `i64` clock must
//!   stay held instead of overflowing and failing the session;
//! * `plan_document` must write a BASE for ANY definition-version change (the
//!   documented rule), including a downgrade;
//! * `build_session_path` must terminate on a corrupt `parentId` cycle.

use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use pir::durable::session::documents::{document_retention, document_retention_millis};
use pir::durable::session::line::JobValue;
use pir::durable::session::{
    plan_document, CommitOptions, ConversationOwnership, DeferDeadline, DocumentCache,
    LoadedDocument, Session, SessionOptions, StagedDocument,
};
use pir::durable::{
    address_id, CommonDocDefinition, Context, ConversationId, DocDefinition, DocOwner,
    DocumentAddress, DocumentContent, DocumentFork, DocumentId, DocumentPoint, DocumentRecord,
    DocumentScope, DocumentSemantics, JsonObject, MemoryStorage, ResolvedAddress, Seq, Storage,
    StorageWrite,
};
use pir::session_manager::projection::build_session_path;
use pir::session_manager::{CustomEntry, FileEntry};

// ── helpers ─────────────────────────────────────────────────────────────────

fn object(value: Value) -> JsonObject {
    match value {
        Value::Object(map) => map,
        other => panic!("expected an object, got {other}"),
    }
}

fn loaded_document(address: &str, record_id: i64) -> LoadedDocument {
    LoadedDocument {
        address_id: address.to_owned(),
        record: DocumentRecord {
            id: DocumentId::from_raw(record_id),
            kind: "probe".to_owned(),
            key: None,
            created_at: Seq::from_raw(1),
            retired_at: None,
            scope: DocumentScope::Session,
            history: None,
            fork: None,
        },
        stored_version: 1,
        value_version: 1,
        deltas_since_base: 0,
        value: object(json!({ "count": 0 })),
    }
}

fn probe_initial() -> JsonObject {
    object(json!({ "count": 0 }))
}

fn probe_definition() -> pir::durable::AnyDocDefinition {
    CommonDocDefinition::conversation_latest("probe", 1, DocumentFork::Initial)
        .expect("definition")
        .behaviour(probe_initial, None, None)
}

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

fn custom(id: &str, parent: Option<&str>) -> FileEntry {
    FileEntry::Custom(CustomEntry {
        custom_type: "probe".to_owned(),
        data: None,
        id: id.to_owned(),
        parent_id: parent.map(str::to_owned),
        timestamp: "2026-01-01T00:00:00.000Z".to_owned(),
    })
}

// ── retention: Infinity keeps documents, huge retentions do not overflow ────

#[test]
fn an_infinite_retention_setting_is_kept_as_forever() {
    assert_eq!(
        document_retention_millis(f64::INFINITY),
        Some(i64::MAX),
        "Infinity must mean 'keep documents', not the ten-minute default"
    );
    assert_eq!(document_retention_millis(600_000.0), Some(600_000));
    assert_eq!(
        document_retention_millis(1e300),
        Some(i64::MAX),
        "a value beyond the i64 range saturates instead of wrapping"
    );
    assert_eq!(document_retention_millis(f64::NAN), None);
    assert_eq!(document_retention_millis(-1.0), None);
    // The cache's own normalization keeps a non-negative value as given.
    assert_eq!(document_retention(Some(i64::MAX)), i64::MAX);
    assert_eq!(document_retention(None), 600_000);
}

#[test]
fn an_infinite_retention_never_evicts_and_never_overflows() {
    // `i64::MAX` is how an infinite settings retention reaches the cache (the
    // `HarnessOptions.document_retention_ms` escape hatch can also pass it).
    let mut cache = DocumentCache::new(i64::MAX);
    cache.insert(loaded_document("probe", 1), 1_000);
    cache.note_use("probe", 2_000);
    assert!(cache.get("probe").is_some(), "a fresh document is cached");
    assert_eq!(
        cache.sweep_deadline(),
        Some(i64::MAX),
        "the deadline saturates instead of overflowing"
    );
    assert!(cache.sweep(3_000).is_empty());
    assert!(cache.sweep(i64::MAX).is_empty());
    assert!(
        cache.get("probe").is_some(),
        "an infinite retention never evicts"
    );
    // A later retention change must also stay overflow-free.
    cache.set_retention_ms(i64::MAX, 10_000);
    assert_eq!(cache.sweep_deadline(), Some(i64::MAX));
}

// ── deferred commit deadline overflow ──────────────────────────────────────

#[tokio::test]
async fn a_deferred_commit_with_an_extreme_deadline_stays_held() {
    let storage = Arc::new(MemoryStorage::new());
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

    // `i64::MAX` milliseconds is the largest representable "later"; `now +
    // i64::MAX` used to overflow in debug builds, panic the mutation line and
    // fail the whole session instead of holding the commit.
    let edited = session
        .doc_with_options(
            address.clone(),
            probe_definition(),
            ctx.clone(),
            CommitOptions::deferred_within(DeferDeadline::After(i64::MAX as u64)),
            |draft| {
                draft.insert("count".to_owned(), json!(1));
            },
        )
        .await;
    assert!(
        edited.is_ok(),
        "an extreme deferred deadline must not fail the session: {:?}",
        edited.err()
    );
    assert!(
        session.handle().has_deferred(),
        "the edit is held in memory"
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
    assert!(
        !session.handle().has_deferred(),
        "the read flushed the hold"
    );
}

// ── immediate adoption installs the committed value ────────────────────────

#[tokio::test]
async fn a_raw_commit_refreshes_the_cached_document_value_and_doc_base() {
    let storage = Arc::new(MemoryStorage::new());
    let backend: Arc<dyn Storage> = storage.clone();
    let session = Session::open(backend, SessionOptions::default());
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

    // `doc()` invalidates the cache after its commit; `snapshot` loads the
    // committed incarnation back into it.
    let before = session
        .snapshot(&address, &ctx)
        .await
        .expect("snapshot")
        .expect("probe is live");
    assert_eq!(before.get("count").and_then(Value::as_i64), Some(0));

    // Read the committed incarnation directly to build a raw draft, the way a
    // scheduler/worker commit does, without going through `Session::doc`.
    let record = storage
        .find_document(address.address.clone(), DocumentPoint::Current)
        .await
        .expect("find document")
        .expect("probe record");
    let stored = storage
        .document(record.id, DocumentPoint::Current)
        .await
        .expect("read document")
        .expect("probe value");
    let loaded = LoadedDocument {
        address_id: address.id.clone(),
        record,
        stored_version: stored.version,
        value_version: stored.version,
        deltas_since_base: stored.deltas_since_base,
        value: stored.value,
    };
    let identity = DocDefinition {
        kind: "probe".to_owned(),
        version: 1,
        semantics: DocumentSemantics::ConversationLatest {
            fork: DocumentFork::Initial,
        },
    };
    let commit_address = address.clone();
    let _: () = session
        .commit(ctx.clone(), move |tx, _state| {
            Box::pin(async move {
                tx.stage_document(StagedDocument {
                    address: commit_address.address,
                    address_id: commit_address.id,
                    definition: Some(identity),
                    family: None,
                    value: object(json!({ "count": 1 })),
                    created: false,
                    loaded: Some(loaded),
                    retire: false,
                });
                Ok(Box::new(()) as JobValue)
            })
        })
        .await
        .expect("raw commit");

    // The immediate commit adopted the new value, not only its version: a
    // stale cache would still answer `0` here.
    let after = session
        .snapshot(&address, &ctx)
        .await
        .expect("snapshot")
        .expect("probe is live");
    assert_eq!(
        after.get("count").and_then(Value::as_i64),
        Some(1),
        "snapshot must observe the raw commit's value"
    );

    // A later `doc()` must fork its draft from that same new base.
    let seen = Arc::new(Mutex::new(None));
    let observed = seen.clone();
    session
        .doc(
            address.clone(),
            probe_definition(),
            ctx.clone(),
            move |draft| {
                *observed
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) =
                    draft.get("count").and_then(Value::as_i64);
                draft.insert("count".to_owned(), json!(2));
            },
        )
        .await
        .expect("edit after the raw commit");
    assert_eq!(
        *seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        Some(1),
        "doc must build its draft from the adopted value"
    );
    let final_value = session
        .snapshot(&address, &ctx)
        .await
        .expect("snapshot")
        .expect("probe is live");
    assert_eq!(final_value.get("count").and_then(Value::as_i64), Some(2));

    session.close().await.expect("close");
}

// ── any definition-version change writes a base ────────────────────────────

#[test]
fn a_plan_for_a_downgraded_definition_version_writes_a_base() {
    let address = DocumentAddress {
        kind: "probe".to_owned(),
        scope: DocumentScope::Session,
        key: None,
    };
    let address_id = address_id(&address);
    let loaded = LoadedDocument {
        address_id: address_id.clone(),
        record: DocumentRecord {
            id: DocumentId::from_raw(7),
            kind: "probe".to_owned(),
            key: None,
            created_at: Seq::from_raw(1),
            retired_at: None,
            scope: DocumentScope::Session,
            history: None,
            fork: None,
        },
        // Stored at version 2, now handled by a version-1 definition.
        stored_version: 2,
        value_version: 2,
        deltas_since_base: 0,
        value: object(json!({ "count": 1 })),
    };
    let staged = StagedDocument {
        address,
        address_id,
        definition: Some(DocDefinition {
            kind: "probe".to_owned(),
            version: 1,
            semantics: DocumentSemantics::Session,
        }),
        family: None,
        value: object(json!({ "count": 2 })),
        created: false,
        loaded: Some(loaded),
        retire: false,
    };

    let plan = plan_document(&staged).expect("a loaded incarnation plans");
    assert_eq!(plan.version, 1);
    assert!(!plan.same_version);
    match plan.content {
        Some(StorageWrite::DocumentChange {
            content: DocumentContent::Base { version, value },
            ..
        }) => {
            assert_eq!(version, 1, "the base carries the definition version");
            assert_eq!(
                value.get("count").and_then(Value::as_i64),
                Some(2),
                "the base carries the prepared value"
            );
        }
        other => panic!(
            "a version change must write a base, not {other:?}: a delta is only \
             valid at the stored revision chain's version"
        ),
    }
}

// ── session path: parent cycles terminate ──────────────────────────────────

#[test]
fn a_parent_cycle_yields_a_finite_session_path() {
    // a -> b -> a (a corrupt two-entry cycle).
    let entries = vec![custom("a", Some("b")), custom("b", Some("a"))];
    let path = build_session_path(&entries, Some("a"));
    let ids: Vec<&str> = path.iter().filter_map(|entry| entry.id()).collect();
    assert_eq!(path.len(), 2, "the walk stops at the repeated id");
    assert_eq!(ids, vec!["b", "a"]);
    assert_eq!(
        build_session_path(&entries, Some("b"))
            .iter()
            .filter_map(|entry| entry.id())
            .collect::<Vec<_>>(),
        vec!["a", "b"],
        "entering the cycle from the other side is symmetric"
    );

    // A self-parent is the one-entry form of the same cycle.
    let entries = vec![custom("s", Some("s"))];
    assert_eq!(
        build_session_path(&entries, Some("s"))
            .iter()
            .filter_map(|entry| entry.id())
            .collect::<Vec<_>>(),
        vec!["s"]
    );

    // A valid chain is unaffected.
    let entries = vec![custom("a", None), custom("b", Some("a"))];
    assert_eq!(
        build_session_path(&entries, Some("b"))
            .iter()
            .filter_map(|entry| entry.id())
            .collect::<Vec<_>>(),
        vec!["a", "b"]
    );
}
