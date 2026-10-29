//! `durable::harness::view` — structural mounts of one conversation.
//!
//! Port of upstream `packages/durable/src/harness/view.ts` onto the Rust
//! session kernel (spec §9.3). A mount is created by its first observer, shared
//! by every later one, dropped with its last, hydrated from one committed state
//! on the line, and advanced by the committed publications only.
//!
//! # Why these tests need no sleeps
//!
//! Every observation point is deterministic:
//!
//! * [`ConversationViews::attach`] runs as a `read_on_line` job and registers
//!   its observer before the line accepts another commit, so the factory's
//!   value and the first `advance`/`publication` are ordered by the line.
//! * internal commit listeners run synchronously on the line **before** the
//!   commit's reply is sent (`line.rs` `run_commit`), so after
//!   `append_entry(..).await` returns the observers already ran.
//! * close listeners run synchronously during `shutdown` before the shutdown
//!   reply, and [`Session::close`] awaits that reply (and then joins the line),
//!   so after `close().await` every `close_session` has run.
//!
//! Tests therefore assert directly after the awaited operation instead of
//! masking a race with a fixed sleep.
//!
//! Tests live here as an integration suite rather than inline in the module:
//! the crate forbids inline tests.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, MutexGuard};

use serde_json::{json, Value};

use pir::durable::delta::{Op, Seg};
use pir::durable::harness::{
    agent_definition, conversation_definitions, summary_entry_draft, AttachedView,
    CompactionReason, ConversationView, ConversationViews, ViewObserver, AGENT_KIND,
    LIVE_DEFINITION, LIVE_KIND,
};
use pir::durable::session::transaction::EntryDraft;
use pir::durable::session::{
    CommitPublication, ConversationOwnership, Session, SessionOptions, TaskOptions,
};
use pir::durable::{
    CommonDocDefinition, Context, ConversationId, DocOwner, DocumentSemantics, EntryRecord,
    JsonObject, MemoryStorage, ResolvedAddress, Storage, StorageError, TaskDefinition,
};

// ── session / conversation helpers ─────────────────────────────────────────

fn open_session() -> Session {
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
    Session::open(storage, SessionOptions::default())
}

async fn create_conversation(session: &Session) -> ConversationId {
    session
        .create_conversation(
            ConversationOwnership::Ownerless,
            None,
            Context::background(),
        )
        .await
        .expect("create conversation")
        .id
}

fn user_draft(text: &str) -> EntryDraft {
    EntryDraft {
        kind: "user".to_owned(),
        model: Some(vec![json!({ "role": "user", "content": text })]),
        data: Some(json!({ "text": text })),
        head: None,
        edits: None,
    }
}

async fn append(session: &Session, conversation: ConversationId, text: &str) -> EntryRecord {
    append_draft(session, conversation, user_draft(text)).await
}

async fn append_draft(
    session: &Session,
    conversation: ConversationId,
    draft: EntryDraft,
) -> EntryRecord {
    session
        .append_entry(conversation, draft, Context::background())
        .await
        .expect("append entry")
}

// ── document helpers ──────────────────────────────────────────────────────

fn conversation_address(
    kind: &str,
    conversation: ConversationId,
    semantics: DocumentSemantics,
) -> ResolvedAddress {
    ResolvedAddress::new(
        kind,
        semantics,
        Some(DocOwner::Conversation(conversation)),
        None,
    )
    .expect("conversation document address")
}

fn empty_document() -> JsonObject {
    JsonObject::new()
}

/// Write all five built-in conversation documents, each carrying `owner`.
async fn write_builtins(session: &Session, conversation: ConversationId, owner: &str) {
    for (kind, definition) in conversation_definitions() {
        let resolved = conversation_address(kind, conversation, definition.common.semantics);
        let owner = owner.to_owned();
        session
            .doc(resolved, definition, Context::background(), move |draft| {
                draft.insert("owner".to_owned(), Value::String(owner));
            })
            .await
            .expect("write built-in conversation document");
    }
}

/// Write one conversation's `pi.agent` model section.
async fn write_agent_model(session: &Session, conversation: ConversationId, model: &str) {
    let definition = agent_definition();
    let resolved = conversation_address(AGENT_KIND, conversation, definition.common.semantics);
    let model = model.to_owned();
    session
        .doc(resolved, definition, Context::background(), move |draft| {
            draft.insert(
                "model".to_owned(),
                json!({ "provider": "anthropic", "modelId": model }),
            );
        })
        .await
        .expect("write pi.agent");
}

// ── observing helper ───────────────────────────────────────────────────────

#[derive(Default)]
struct Recorded {
    initial: Option<ConversationView>,
    advances: Vec<(ConversationView, Vec<Op>)>,
    publications: Vec<(ConversationView, Vec<Op>, CommitPublication)>,
    closes: Vec<Option<StorageError>>,
}

type RecordedLog = Arc<Mutex<Recorded>>;

fn log() -> RecordedLog {
    Arc::new(Mutex::new(Recorded::default()))
}

fn recorded(log: &RecordedLog) -> MutexGuard<'_, Recorded> {
    log.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

struct Recorder {
    log: RecordedLog,
}

impl ViewObserver for Recorder {
    fn advance(&self, value: &ConversationView, ops: &[Op], _context: &Context) {
        recorded(&self.log)
            .advances
            .push((value.clone(), ops.to_vec()));
    }

    fn publication(
        &self,
        _before: &ConversationView,
        after: &ConversationView,
        ops: &[Op],
        publication: &CommitPublication,
        _context: &Context,
    ) {
        recorded(&self.log)
            .publications
            .push((after.clone(), ops.to_vec(), publication.clone()));
    }

    fn close_session(&self, failure: Option<&StorageError>) {
        recorded(&self.log).closes.push(failure.cloned());
    }
}

/// A factory that records the revision the observer was created from.
fn recorder(log: &RecordedLog) -> impl FnOnce(&ConversationView) -> Recorder + Send + 'static {
    let log = log.clone();
    move |view| {
        recorded(&log).initial = Some(view.clone());
        Recorder { log }
    }
}

async fn attach(
    views: &ConversationViews,
    conversation: ConversationId,
    create: impl FnOnce(&ConversationView) -> Recorder + Send + 'static,
) -> AttachedView<Recorder> {
    views
        .attach::<Recorder, _>(conversation, create, &Context::background())
        .await
        .expect("attach observer")
}

fn initial(log: &RecordedLog) -> ConversationView {
    recorded(log).initial.clone().expect("factory ran")
}

fn entry_ids(value: &ConversationView) -> Vec<i64> {
    value
        .entries()
        .iter()
        .map(|entry| entry.id.as_i64())
        .collect()
}

// ── mount lifecycle (reference counting) ──────────────────────────────────

#[tokio::test(flavor = "current_thread")]
async fn a_mount_lives_while_it_has_an_observer_and_is_rebuilt_after_the_last_one_leaves() {
    let session = open_session();
    let conversation = create_conversation(&session).await;
    let views = ConversationViews::open(session.clone())
        .await
        .expect("open view adapter");

    assert_eq!(views.mount_count(), 0, "opening mounts nothing");
    assert!(!views.is_mounted(conversation));
    assert!(!views.is_closed());

    // The first observer builds the mount from the current committed state.
    let first_log = log();
    let first = attach(&views, conversation, recorder(&first_log)).await;
    assert!(views.is_mounted(conversation));
    assert_eq!(views.mount_count(), 1);
    assert!(recorded(&first_log).advances.is_empty());
    assert!(recorded(&first_log).closes.is_empty());

    let first_initial = initial(&first_log);
    assert_eq!(first_initial.conversation.id, conversation);
    assert!(first_initial.entries().is_empty(), "a fresh transcript");
    assert!(
        first_initial.docs().is_empty(),
        "no built-in document written yet"
    );

    // A commit reaches the observer.
    let first_entry = append(&session, conversation, "one").await;
    {
        let state = recorded(&first_log);
        assert_eq!(state.advances.len(), 1);
        assert_eq!(
            state.advances[0].0.entries().to_vec(),
            vec![first_entry.clone()]
        );
    }

    // A second observer joins the existing mount instead of building one: it
    // is created from the mount's current revision and receives no replay.
    let second_log = log();
    let second = attach(&views, conversation, recorder(&second_log)).await;
    assert_eq!(views.mount_count(), 1, "at most one mount per conversation");
    {
        let state = recorded(&second_log);
        assert_eq!(
            state
                .initial
                .as_ref()
                .expect("factory ran")
                .entries()
                .to_vec(),
            vec![first_entry.clone()],
            "a joining observer starts from the current revision"
        );
        assert!(
            state.advances.is_empty(),
            "no historical series is replayed"
        );
        assert!(
            state.publications.is_empty(),
            "the join itself publishes nothing"
        );
    }

    let second_entry = append(&session, conversation, "two").await;
    assert_eq!(recorded(&first_log).advances.len(), 2);
    assert_eq!(recorded(&second_log).advances.len(), 1);

    // Detaching one observer keeps the shared mount alive.
    first.detach.detach();
    assert!(views.is_mounted(conversation));
    assert_eq!(views.mount_count(), 1);
    // Detach is idempotent.
    first.detach.detach();
    assert!(views.is_mounted(conversation));

    let third_entry = append(&session, conversation, "three").await;
    assert_eq!(
        recorded(&first_log).advances.len(),
        2,
        "a detached observer receives nothing"
    );
    assert_eq!(recorded(&second_log).advances.len(), 2);

    // The last observer leaving drops the mount.
    second.detach.detach();
    assert!(!views.is_mounted(conversation));
    assert_eq!(views.mount_count(), 0);

    // A commit after the mount dropped is committed storage that nothing
    // observed; re-attaching must rebuild from it rather than reuse a stale
    // cached value.
    let fourth_entry = append(&session, conversation, "four").await;
    let third_log = log();
    let third = attach(&views, conversation, recorder(&third_log)).await;
    {
        let state = recorded(&third_log);
        assert_eq!(
            state
                .initial
                .as_ref()
                .expect("factory ran")
                .entries()
                .to_vec(),
            vec![
                first_entry.clone(),
                second_entry.clone(),
                third_entry.clone(),
                fourth_entry.clone(),
            ],
            "the rebuilt mount starts from committed state"
        );
        assert!(state.advances.is_empty(), "a rebuilt mount replays nothing");
    }
    assert_eq!(views.mount_count(), 1);

    third.detach.detach();
    assert_eq!(views.mount_count(), 0);

    // An unknown conversation builds nothing and leaves no mount behind.
    let missing = ConversationId::from_raw(900_000);
    let result = views
        .attach::<Recorder, _>(missing, recorder(&log()), &Context::background())
        .await;
    assert!(result.is_err(), "an unknown conversation cannot be mounted");
    assert_eq!(views.mount_count(), 0);

    session.close().await.expect("close");
}

// ── docs: only this conversation's built-in documents ─────────────────────

#[tokio::test(flavor = "current_thread")]
async fn docs_are_the_mounted_conversations_built_ins_only() {
    let session = open_session();
    let mounted = create_conversation(&session).await;
    let other = create_conversation(&session).await;

    write_builtins(&session, mounted, "mounted").await;
    // The other conversation has the same kinds at its own addresses.
    write_builtins(&session, other, "other").await;

    // Same kinds in the other two scopes: a task document and a session
    // document. Neither is a conversation document and neither may appear.
    let task_id = session
        .create_task(
            &TaskDefinition::define("demo", 1, vec!["run".to_owned()]).expect("task definition"),
            json!({ "n": 1 }),
            TaskOptions {
                conversation_id: Some(mounted),
                ..TaskOptions::default()
            },
            Context::background(),
        )
        .await
        .expect("create task");
    let task_definition = CommonDocDefinition::task(LIVE_KIND, 1)
        .expect("task document definition")
        .behaviour(empty_document, None, None);
    let task_address = ResolvedAddress::new(
        LIVE_KIND,
        DocumentSemantics::Task,
        Some(DocOwner::Task(task_id)),
        None,
    )
    .expect("task document address");
    session
        .doc(
            task_address,
            task_definition,
            Context::background(),
            |draft| {
                draft.insert("owner".to_owned(), json!("task"));
            },
        )
        .await
        .expect("write task document");

    let session_definition = CommonDocDefinition::session(LIVE_KIND, 1)
        .expect("session document definition")
        .behaviour(empty_document, None, None);
    let session_address = ResolvedAddress::new(LIVE_KIND, DocumentSemantics::Session, None, None)
        .expect("session document address");
    session
        .doc(
            session_address,
            session_definition,
            Context::background(),
            |draft| {
                draft.insert("owner".to_owned(), json!("session"));
            },
        )
        .await
        .expect("write session document");

    let views = ConversationViews::open(session.clone())
        .await
        .expect("open view adapter");
    let mounted_log = log();
    let _mounted = attach(&views, mounted, recorder(&mounted_log)).await;
    let view = initial(&mounted_log);

    let expected: BTreeSet<&str> = conversation_definitions()
        .into_iter()
        .map(|(kind, _)| kind)
        .collect();
    let actual: BTreeSet<&str> = view.docs().keys().map(String::as_str).collect();
    assert_eq!(
        actual, expected,
        "docs holds exactly the five conversation kinds; task- and session-scoped \
         documents of the same kind are not conversation documents"
    );

    for (kind, value) in view.docs() {
        assert_eq!(
            value.get("owner"),
            Some(&json!("mounted")),
            "document {kind} must come from the mounted conversation"
        );
    }
    assert_eq!(
        view.doc(LIVE_KIND).expect("mounted pi.live")["owner"],
        json!("mounted")
    );
    assert_ne!(
        view.doc(LIVE_KIND).expect("mounted pi.live")["owner"],
        json!("task")
    );
    assert_ne!(
        view.doc(LIVE_KIND).expect("mounted pi.live")["owner"],
        json!("session")
    );

    // The other conversation is not mounted and its values never applied.
    assert!(!views.is_mounted(other));
    assert_eq!(views.mount_count(), 1);

    session.close().await.expect("close");
}

#[tokio::test(flavor = "current_thread")]
async fn a_conversation_document_advances_the_mount() {
    let session = open_session();
    let conversation = create_conversation(&session).await;
    let definition = LIVE_DEFINITION.clone();
    let address = conversation_address(LIVE_KIND, conversation, definition.common.semantics);
    session
        .doc(
            address.clone(),
            definition.clone(),
            Context::background(),
            |draft| {
                draft.insert("count".to_owned(), json!(0));
            },
        )
        .await
        .expect("create pi.live");

    let views = ConversationViews::open(session.clone())
        .await
        .expect("open view adapter");
    let observer_log = log();
    let _observer = attach(&views, conversation, recorder(&observer_log)).await;
    assert_eq!(
        initial(&observer_log)
            .doc(LIVE_KIND)
            .expect("mounted pi.live")["count"],
        json!(0)
    );

    session
        .doc(address, definition, Context::background(), |draft| {
            draft.insert("count".to_owned(), json!(1));
        })
        .await
        .expect("edit pi.live");

    {
        let state = recorded(&observer_log);
        assert_eq!(state.advances.len(), 1);
        let (value, ops) = &state.advances[0];
        assert_eq!(
            value.doc(LIVE_KIND).expect("mounted pi.live")["count"],
            json!(1)
        );
        assert!(!ops.is_empty(), "the mount forwarded the document change");
        assert_eq!(
            ops[0],
            Op::Set(
                vec![
                    Seg::Key("docs".to_owned()),
                    Seg::Key(LIVE_KIND.to_owned()),
                    Seg::Key("count".to_owned()),
                ],
                json!(1),
            )
        );
    }
    session.close().await.expect("close");
}

// ── docs: task-scoped documents never enter a conversation mount ──────────

#[tokio::test(flavor = "current_thread")]
async fn a_task_document_with_a_builtin_kind_never_overrides_a_conversation_document() {
    let session = open_session();
    let conversation = create_conversation(&session).await;
    write_builtins(&session, conversation, "conversation").await;

    let views = ConversationViews::open(session.clone())
        .await
        .expect("open view adapter");
    let observer_log = log();
    let _observer = attach(&views, conversation, recorder(&observer_log)).await;
    assert_eq!(
        initial(&observer_log)
            .doc(LIVE_KIND)
            .expect("mounted pi.live")["owner"],
        json!("conversation")
    );

    let task_id = session
        .create_task(
            &TaskDefinition::define("demo", 1, vec!["run".to_owned()]).expect("task definition"),
            json!({ "n": 1 }),
            TaskOptions {
                conversation_id: Some(conversation),
                ..TaskOptions::default()
            },
            Context::background(),
        )
        .await
        .expect("create task");
    let task_definition = CommonDocDefinition::task(LIVE_KIND, 1)
        .expect("task document definition")
        .behaviour(empty_document, None, None);
    let task_address = ResolvedAddress::new(
        LIVE_KIND,
        DocumentSemantics::Task,
        Some(DocOwner::Task(task_id)),
        None,
    )
    .expect("task document address");
    session
        .doc(
            task_address,
            task_definition,
            Context::background(),
            |draft| {
                draft.insert("owner".to_owned(), json!("task"));
            },
        )
        .await
        .expect("write task document");

    // A task-scoped document is not one of the conversation's built-ins, even
    // when it shares a kind and its task lives in this conversation. A mount
    // filters by the document address scope, not just by conversation + kind.
    {
        let state = recorded(&observer_log);
        assert!(
            state.advances.is_empty(),
            "a task-scoped document must not advance the conversation view, got {:?}",
            state.advances
        );
    }
    // The close listener notifies this observer and needs the same log lock;
    // never hold the guard across `close()`.
    session.close().await.expect("close");
}

// ── entries: the active range starts at the head marker ───────────────────

#[tokio::test(flavor = "current_thread")]
async fn entries_start_at_the_newest_head_marker() {
    let session = open_session();
    let conversation = create_conversation(&session).await;
    let first = append(&session, conversation, "one").await;
    let second = append(&session, conversation, "two").await;
    let third = append(&session, conversation, "three").await;

    // A compaction summary selects `second` as the first kept entry; entries
    // before it are summarized away.
    let summary = append_draft(
        &session,
        conversation,
        summary_entry_draft(second.id, "summary", CompactionReason::Manual, 1_000),
    )
    .await;
    assert_eq!(summary.head, Some(second.id));
    assert_eq!(summary.kind, "pi.compaction");

    let views = ConversationViews::open(session.clone())
        .await
        .expect("open view adapter");
    let observer_log = log();
    let _observer = attach(&views, conversation, recorder(&observer_log)).await;
    let view = initial(&observer_log);

    assert_eq!(
        entry_ids(&view),
        vec![summary.id.as_i64(), second.id.as_i64(), third.id.as_i64(),],
        "the head marker first, then the non-head entries from its head"
    );
    assert_eq!(
        view.entries()[0].head,
        Some(second.id),
        "the first entry is the head marker itself"
    );
    assert!(
        !view.entries().iter().any(|entry| entry.id == first.id),
        "the entry before the head is outside the active range"
    );

    session.close().await.expect("close");
}

#[tokio::test(flavor = "current_thread")]
async fn a_head_marker_advance_keeps_only_the_suffix_from_its_head() {
    let session = open_session();
    let conversation = create_conversation(&session).await;
    let first = append(&session, conversation, "one").await;
    let second = append(&session, conversation, "two").await;
    let third = append(&session, conversation, "three").await;

    let views = ConversationViews::open(session.clone())
        .await
        .expect("open view adapter");
    let observer_log = log();
    let _observer = attach(&views, conversation, recorder(&observer_log)).await;

    let summary = append_draft(
        &session,
        conversation,
        summary_entry_draft(second.id, "summary", CompactionReason::Manual, 1_000),
    )
    .await;

    {
        let state = recorded(&observer_log);
        assert_eq!(state.advances.len(), 1);
        let (value, ops) = &state.advances[0];
        assert_eq!(
            entry_ids(value),
            vec![summary.id.as_i64(), second.id.as_i64(), third.id.as_i64(),],
            "the mount drops the summarized prefix, not just prepends the marker"
        );
        assert_eq!(
            ops,
            &vec![Op::Splice {
                path: vec![Seg::Key("entries".to_owned())],
                index: 0,
                remove: 1,
                items: vec![serde_json::to_value(&summary).expect("summary json")],
            }],
            "the splice removes `first` and inserts the marker at the front"
        );
        assert!(!value.entries().iter().any(|entry| entry.id == first.id));
    }

    session.close().await.expect("close");
}

// ── close: every observer exactly once ────────────────────────────────────

#[tokio::test(flavor = "current_thread")]
async fn close_notifies_every_observer_exactly_once() {
    let session = open_session();
    let first_conversation = create_conversation(&session).await;
    let second_conversation = create_conversation(&session).await;
    let views = ConversationViews::open(session.clone())
        .await
        .expect("open view adapter");

    let detached_log = log();
    let surviving_log = log();
    let other_log = log();
    let detached = attach(&views, first_conversation, recorder(&detached_log)).await;
    let _surviving = attach(&views, first_conversation, recorder(&surviving_log)).await;
    let _other = attach(&views, second_conversation, recorder(&other_log)).await;
    assert_eq!(views.mount_count(), 2, "one mount per conversation");

    // A detached observer is no longer in the mount and must not be notified.
    detached.detach.detach();
    assert_eq!(views.mount_count(), 2, "the other observers keep the mount");

    session.close().await.expect("close");

    assert!(views.is_closed());
    assert_eq!(views.mount_count(), 0);
    assert!(!views.is_mounted(first_conversation));
    assert!(!views.is_mounted(second_conversation));

    assert!(
        recorded(&detached_log).closes.is_empty(),
        "a detached observer is not closed a second time"
    );
    assert_eq!(
        recorded(&surviving_log).closes,
        vec![None],
        "the surviving observer of a shared mount closes exactly once"
    );
    assert_eq!(
        recorded(&other_log).closes,
        vec![None],
        "every mount closes exactly once"
    );

    // A closed adapter refuses new observers.
    let late_log = log();
    let late = views
        .attach::<Recorder, _>(
            first_conversation,
            recorder(&late_log),
            &Context::background(),
        )
        .await;
    assert!(late.is_err(), "a closed adapter cannot attach");
    assert!(recorded(&late_log).initial.is_none());
    assert_eq!(views.mount_count(), 0);
}

// ── late join: current value, no replay ───────────────────────────────────

#[tokio::test(flavor = "current_thread")]
async fn a_late_observer_starts_from_the_current_value_and_is_never_replayed() {
    let session = open_session();
    let conversation = create_conversation(&session).await;
    let views = ConversationViews::open(session.clone())
        .await
        .expect("open view adapter");

    let early_log = log();
    let early = attach(&views, conversation, recorder(&early_log)).await;

    // State changes before the late observer exists.
    let first = append(&session, conversation, "one").await;
    let second = append(&session, conversation, "two").await;
    write_agent_model(&session, conversation, "current-model").await;

    let late_log = log();
    let _late = attach(&views, conversation, recorder(&late_log)).await;
    {
        let state = recorded(&late_log);
        let view = state
            .initial
            .as_ref()
            .expect("the factory ran for the late observer")
            .clone();
        assert_eq!(
            view.entries().to_vec(),
            vec![first.clone(), second.clone()],
            "the late observer is created from the current revision"
        );
        assert_eq!(
            view.doc(AGENT_KIND).expect("mounted pi.agent")["model"]["modelId"],
            json!("current-model")
        );
        assert!(
            state.advances.is_empty(),
            "nothing that happened before the join is replayed"
        );
        assert!(
            state.publications.is_empty(),
            "the late observer was not created from a publication"
        );
    }
    // The early observer saw exactly the three pre-join publications (two
    // entries and the document).
    assert_eq!(recorded(&early_log).advances.len(), 3);

    // The late observer's first callback carries only the change *after* the
    // join, applied to the snapshot it started from.
    let third = append(&session, conversation, "three").await;
    {
        let state = recorded(&late_log);
        assert_eq!(state.advances.len(), 1);
        let (value, ops) = &state.advances[0];
        assert_eq!(value.entries().to_vec(), vec![first, second, third.clone()]);
        assert_eq!(
            ops,
            &vec![Op::Splice {
                path: vec![Seg::Key("entries".to_owned())],
                index: 2,
                remove: 0,
                items: vec![serde_json::to_value(&third).expect("entry json")],
            }],
            "one append, not the whole transcript"
        );
    }
    assert_eq!(recorded(&early_log).advances.len(), 4);

    early.detach.detach();
    session.close().await.expect("close");
}

// ── memory: a publication only copies what it changed ─────────────────────
//
// The view's containers are copy-on-write. These tests pin the sharing: a
// document delta must leave the transcript and every untouched document in the
// same allocation as the previous revision, and an entry write must leave the
// documents alone. A regression to the former whole-view clone breaks both.

#[tokio::test(flavor = "current_thread")]
async fn a_document_delta_shares_the_transcript_and_every_untouched_document() {
    let session = open_session();
    let conversation = create_conversation(&session).await;
    write_builtins(&session, conversation, "before").await;
    let entry = append(&session, conversation, "one").await;

    let views = ConversationViews::open(session.clone())
        .await
        .expect("open view adapter");
    let observer_log = log();
    let _observer = attach(&views, conversation, recorder(&observer_log)).await;
    let initial = initial(&observer_log);
    assert_eq!(initial.entries().to_vec(), vec![entry]);

    let entries_before = Arc::clone(&initial.entries);
    let agent_before = Arc::clone(initial.docs().get(AGENT_KIND).expect("pi.agent is mounted"));
    let live_before = Arc::clone(initial.docs().get(LIVE_KIND).expect("pi.live is mounted"));

    let definition = conversation_definitions()
        .into_iter()
        .find(|(kind, _)| *kind == LIVE_KIND)
        .map(|(_, definition)| definition)
        .expect("pi.live definition");
    let address = conversation_address(LIVE_KIND, conversation, definition.common.semantics);
    session
        .doc(address, definition, Context::background(), |draft| {
            draft.insert("owner".to_owned(), json!("after"));
        })
        .await
        .expect("edit pi.live");

    // Copy what the assertions need out of the lock: the guard must not live
    // across the `close().await` below.
    let (advance_count, after) = {
        let state = recorded(&observer_log);
        (state.advances.len(), state.advances[0].0.clone())
    };
    assert_eq!(advance_count, 1, "one publication reached the mount");
    let after = &after;
    assert!(
        Arc::ptr_eq(&entries_before, &after.entries),
        "a document delta must not copy the transcript"
    );
    assert!(
        Arc::ptr_eq(
            &agent_before,
            after.docs().get(AGENT_KIND).expect("pi.agent is mounted")
        ),
        "a document the publication did not touch stays in the same allocation"
    );
    assert!(
        !Arc::ptr_eq(
            &live_before,
            after.docs().get(LIVE_KIND).expect("pi.live is mounted")
        ),
        "the changed document is a new revision"
    );
    assert_eq!(
        after.doc(LIVE_KIND).expect("mounted pi.live")["owner"],
        json!("after")
    );

    session.close().await.expect("close");
}

#[tokio::test(flavor = "current_thread")]
async fn an_entry_publication_leaves_every_mounted_document_shared() {
    let session = open_session();
    let conversation = create_conversation(&session).await;
    write_builtins(&session, conversation, "stable").await;

    let views = ConversationViews::open(session.clone())
        .await
        .expect("open view adapter");
    let observer_log = log();
    let _observer = attach(&views, conversation, recorder(&observer_log)).await;
    let initial = initial(&observer_log);

    let entries_before = Arc::clone(&initial.entries);
    let docs_before = Arc::clone(&initial.docs);

    append(&session, conversation, "next").await;

    // Copy what the assertions need out of the lock: the guard must not live
    // across the `close().await` below.
    let (advance_count, after) = {
        let state = recorded(&observer_log);
        (state.advances.len(), state.advances[0].0.clone())
    };
    assert_eq!(advance_count, 1, "one publication reached the mount");
    let after = &after;
    assert!(
        !Arc::ptr_eq(&entries_before, &after.entries),
        "the entry list is the container this publication changed"
    );
    assert!(
        Arc::ptr_eq(&docs_before, &after.docs),
        "an entry write must not copy the mounted documents"
    );

    session.close().await.expect("close");
}
