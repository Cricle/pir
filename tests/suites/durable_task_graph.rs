//! `durable::harness::task_graph` — the session's live task graph (spec §9.5).
//!
//! Port of upstream `packages/durable/src/harness/task-graph.ts`. The graph is a
//! structural mount over the session's task table: every live task with its
//! durable status, minus checkpoint and outcome payloads, plus the
//! conversations it owns and the reason it cannot run.
//!
//! # What these tests pin
//!
//! * **Membership is the live statuses only.** `pending`, `running`, `waiting`,
//!   `sleeping` and `completing` are in the graph; a task that settles leaves
//!   it with an exact `delete`.
//! * **The blocked reason is derived, not durable.** It is computed from the
//!   registry and the restart count, so a registry change republishes it
//!   without a commit ([`TaskGraphBlocking::subscribe_blocked`]).
//! * **Reference counting.** The mount is built by its first observer and
//!   dropped with its last; a later observer rebuilds from committed storage
//!   rather than reusing a stale value.
//! * **Watch framing.** Frames carry the exact operation batch; the undelivered
//!   suffix collapses to one root replacement past
//!   [`MAX_PENDING_WATCH_FRAMES`], and a listener that returns `false` ends the
//!   watch.
//!
//! # Why these tests need no sleeps
//!
//! Every observation point is ordered by the session line:
//!
//! * `attach` runs as a `read_on_line` job and registers its observer before
//!   the line accepts another commit.
//! * The view's internal commit listener runs synchronously on the line before
//!   the commit reply is sent, so after `set_task(..).await` the mount has
//!   advanced.
//! * `catalogue.install(..)` publishes the blocked listener synchronously on
//!   the calling thread, so the reason is republished before `install` returns.
//! * Close listeners run synchronously during shutdown before the shutdown
//!   reply, and `Session::close` awaits that reply.
//!
//! Tests live here as an integration suite rather than inline in the module:
//! the crate forbids inline tests.

use std::sync::{Arc, Mutex, MutexGuard};

use serde_json::{json, Value};

use pir::durable::delta::{Op, Seg};
use pir::durable::harness::scheduler::{BlockedReason, TaskScheduler, TaskSchedulerOptions};
use pir::durable::harness::settings::Settings;
use pir::durable::harness::task::{define_task, phase_handler, Task};
use pir::durable::harness::task_graph::{
    blocked_reason_of, BlockedSubject, NoBlocking, OutcomeStatus, TaskGraphBlocking,
    TaskGraphFrame, TaskGraphState, TaskGraphView, TaskGraphWatch,
};
use pir::durable::ids::{ConversationId, TaskId};
use pir::durable::session::facade::ConversationOwnership;
use pir::durable::session::observation::{WatchEnd, MAX_PENDING_WATCH_FRAMES};
use pir::durable::session::transaction::{TaskOptions, TaskOwnership};
use pir::durable::session::{Session, SessionOptions};
use pir::durable::storage::memory::MemoryStorage;
use pir::durable::tasks::TaskDefinition;
use pir::durable::types::{
    JoinPolicy, Storage, TaskOutcome, TaskOutcomeError, TaskRecord, TaskState,
};
use pir::durable::Context;

// ── session / task helpers ─────────────────────────────────────────────────

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

/// A metadata-only definition: enough for `create_task`.
fn definition(kind: &str, version: i64) -> TaskDefinition {
    TaskDefinition::define(kind, version, vec!["run".to_owned()]).expect("task definition")
}

async fn create_task(
    session: &Session,
    conversation: ConversationId,
    kind: &str,
    version: i64,
) -> TaskId {
    session
        .create_task(
            &definition(kind, version),
            json!({ "n": 1 }),
            TaskOptions {
                trace: None,
                conversation_id: Some(conversation),
                ownership: TaskOwnership::Conversation,
                background: false,
                abandon_on_restart: false,
                submission_ids: Vec::new(),
            },
            Context::background(),
        )
        .await
        .expect("create task")
}

async fn record(session: &Session, id: TaskId) -> TaskRecord {
    session
        .task(id, &Context::background())
        .await
        .expect("read task")
        .expect("task exists")
}

/// Replace a task's state, keeping the rest of the record.
async fn set_state(session: &Session, id: TaskId, state: TaskState) {
    let mut record = record(session, id).await;
    record.state = state;
    session
        .set_task(record, Context::background())
        .await
        .expect("set task state");
}

fn checkpoint(phase: &str) -> Value {
    json!({ "phase": phase })
}

// ── observer helpers ───────────────────────────────────────────────────────

#[derive(Default)]
struct Recorded {
    /// Every frame the listener was handed, in delivery order.
    frames: Vec<TaskGraphFrame>,
}

type RecordedLog = Arc<Mutex<Recorded>>;

fn log() -> RecordedLog {
    Arc::new(Mutex::new(Recorded::default()))
}

fn recorded(log: &RecordedLog) -> MutexGuard<'_, Recorded> {
    log.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// A watch whose listener keeps every frame and always continues.
async fn watch(view: &TaskGraphView, log: &RecordedLog) -> TaskGraphWatch {
    let watch = view
        .watch(&Context::background())
        .await
        .expect("watch task graph");
    let log = log.clone();
    watch.start(move |frame| {
        recorded(&log).frames.push(frame);
        true
    });
    watch
}

fn paths(ops: &[Op]) -> Vec<Vec<String>> {
    ops.iter()
        .map(|op| match op {
            Op::Set(path, _) | Op::Delete(path) => path_strings(path),
            // A collapapsed frame carries one root replacement instead of a
            // path batch; `Replace` is the whole value.
            Op::Replace(_) => vec!["<root>".to_owned()],
            _ => Vec::new(),
        })
        .collect()
}

fn path_strings(segments: &[Seg]) -> Vec<String> {
    segments
        .iter()
        .map(|seg| match seg {
            Seg::Key(key) => key.clone(),
            Seg::Index(index) => index.to_string(),
        })
        .collect()
}

fn task_path(key: &str) -> Vec<String> {
    vec!["tasks".to_owned(), key.to_owned()]
}

fn task_field_path(key: &str, field: &str) -> Vec<String> {
    vec!["tasks".to_owned(), key.to_owned(), field.to_owned()]
}

// ── node projection ────────────────────────────────────────────────────────

#[test]
fn outcome_status_names_every_outcome() {
    assert_eq!(
        OutcomeStatus::of(&TaskOutcome::Completed {
            result: Value::Null
        }),
        OutcomeStatus::Completed
    );
    assert_eq!(
        OutcomeStatus::of(&TaskOutcome::Failed {
            error: TaskOutcomeError {
                message: "boom".to_owned(),
                detail: None,
            },
            result: None,
        }),
        OutcomeStatus::Failed
    );
    assert_eq!(
        OutcomeStatus::of(&TaskOutcome::Aborted {
            reason: None,
            result: None,
        }),
        OutcomeStatus::Aborted
    );
    assert_eq!(
        OutcomeStatus::of(&TaskOutcome::Orphaned {
            reason: "missing_task".to_owned()
        }),
        OutcomeStatus::Orphaned
    );
    assert_eq!(
        OutcomeStatus::of(&TaskOutcome::Faulted {
            error: TaskOutcomeError {
                message: "fault".to_owned(),
                detail: None,
            }
        }),
        OutcomeStatus::Faulted
    );

    assert_eq!(OutcomeStatus::Completed.as_str(), "completed");
    assert_eq!(OutcomeStatus::Failed.as_str(), "failed");
    assert_eq!(OutcomeStatus::Aborted.as_str(), "aborted");
    assert_eq!(OutcomeStatus::Orphaned.as_str(), "orphaned");
    assert_eq!(OutcomeStatus::Faulted.as_str(), "faulted");
}

#[test]
fn every_blocked_reason_round_trips_through_its_wire_string() {
    for (text, reason) in [
        ("missing_task", BlockedReason::MissingTask),
        ("task_too_old", BlockedReason::TaskTooOld),
        ("migration_failed", BlockedReason::MigrationFailed),
        ("restart_limit", BlockedReason::RestartLimit),
    ] {
        assert_eq!(blocked_reason_of(text), Some(reason), "wire string {text}");
        assert_eq!(reason.as_str(), text, "round trip for {text}");
    }
    assert_eq!(blocked_reason_of("nonsense"), None);
    assert_eq!(blocked_reason_of(""), None);
}

#[test]
fn a_blocked_subject_carries_only_the_fields_the_decision_reads() {
    let record = TaskRecord {
        trace: None,
        links: Vec::new(),
        id: TaskId::from_raw(7),
        conversation_id: ConversationId::from_raw(3),
        kind: "demo".to_owned(),
        version: 4,
        input: json!({ "payload": "ignored" }),
        owner: Some(TaskId::from_raw(2)),
        background: true,
        abort_requested: true,
        abort_reason: Some("restart".to_owned()),
        abandon_on_restart: Some(true),
        started_at: Some(10),
        ended_at: None,
        restarts: Some(3),
        state: TaskState::Pending {
            checkpoint: checkpoint("run"),
        },
        memos: None,
    };
    let subject = BlockedSubject::of(&record);
    assert_eq!(subject.id, record.id);
    assert_eq!(subject.kind, "demo");
    assert_eq!(subject.version, 4);
    assert!(subject.abort_requested);
    assert_eq!(subject.abort_reason.as_deref(), Some("restart"));
    assert_eq!(subject.restarts, Some(3));
    assert_eq!(subject.status, record.state.status());

    // The rebuilt record keeps identity but no payload: no blocked-reason
    // decision reads a checkpoint, an input or an outcome.
    let rebuilt = subject.to_record();
    assert_eq!(rebuilt.id, record.id);
    assert_eq!(rebuilt.kind, record.kind);
    assert_eq!(rebuilt.version, record.version);
    assert_eq!(rebuilt.restarts, record.restarts);
    assert!(rebuilt.abort_requested);
    assert_eq!(rebuilt.abort_reason, record.abort_reason);
    assert_eq!(rebuilt.state.status(), subject.status);
}

#[test]
fn a_blocked_subject_rebuilds_a_waiting_and_a_terminal_record() {
    let waiting = BlockedSubject {
        id: TaskId::from_raw(1),
        kind: "demo".to_owned(),
        version: 1,
        abort_requested: false,
        abort_reason: None,
        restarts: None,
        status: pir::durable::types::TaskStatus::Waiting,
    };
    let rebuilt = waiting.to_record();
    assert!(matches!(rebuilt.state, TaskState::Waiting { .. }));

    let terminal = BlockedSubject {
        status: pir::durable::types::TaskStatus::Terminal,
        ..waiting
    };
    assert!(matches!(
        terminal.to_record().state,
        TaskState::Terminal { .. }
    ));
}

#[test]
fn the_default_blocking_source_never_blocks_and_never_subscribes() {
    assert_eq!(
        NoBlocking.blocked(&BlockedSubject {
            id: TaskId::from_raw(1),
            kind: "demo".to_owned(),
            version: 1,
            abort_requested: false,
            abort_reason: None,
            restarts: Some(99),
            status: pir::durable::types::TaskStatus::Pending,
        }),
        None
    );
    // Subscribing to `NoBlocking` must be a no-op rather than a panic.
    NoBlocking.subscribe_blocked(Arc::new(|| {}));
}

#[test]
fn a_node_serializes_its_blocked_reason_as_its_wire_string() {
    use pir::durable::harness::task_graph::TaskGraphNode;

    let node = TaskGraphNode {
        id: TaskId::from_raw(5),
        kind: "demo".to_owned(),
        conversation_id: ConversationId::from_raw(1),
        owner: None,
        background: false,
        abort_requested: false,
        state: TaskGraphState::Pending {
            phase: "run".to_owned(),
        },
        blocked: Some(BlockedReason::TaskTooOld),
        conversations: vec![ConversationId::from_raw(9)],
    };
    let wire = serde_json::to_value(&node).expect("serialize node");
    assert_eq!(wire["blocked"], json!("task_too_old"));
    let back: TaskGraphNode = serde_json::from_value(wire).expect("deserialize node");
    assert_eq!(back, node);

    // An unblocked node omits the field entirely.
    let unblocked = TaskGraphNode {
        blocked: None,
        ..node
    };
    let wire = serde_json::to_value(&unblocked).expect("serialize node");
    assert!(wire.get("blocked").is_none(), "absent, not null");
}

// ── mount lifecycle ────────────────────────────────────────────────────────

#[tokio::test(flavor = "current_thread")]
async fn the_graph_holds_every_live_task_and_drops_a_task_that_settles() {
    let session = open_session();
    let conversation = create_conversation(&session).await;
    let view = TaskGraphView::open(session.clone(), Arc::new(NoBlocking))
        .await
        .expect("open task graph");

    assert!(!view.is_mounted(), "opening mounts nothing");
    assert!(!view.is_closed());

    let pending = create_task(&session, conversation, "demo", 1).await;

    // A state handle builds the mount from the committed task table.
    let state = view
        .state(&Context::background())
        .await
        .expect("state handle");
    assert!(view.is_mounted());
    let graph = state.value().expect("current revision");
    let node = graph.node(pending).expect("the pending task is live");
    assert_eq!(node.id, pending);
    assert_eq!(node.kind, "demo");
    assert_eq!(node.conversation_id, conversation);
    assert_eq!(node.owner, None);
    assert_eq!(
        node.state,
        TaskGraphState::Pending {
            // The metadata-only definition declares no checkpoint, so the
            // phase normalizes to the empty string rather than being absent.
            phase: String::new()
        }
    );
    assert_eq!(node.blocked, None);
    assert!(node.conversations.is_empty());

    // A task that settles leaves the graph.
    set_state(
        &session,
        pending,
        TaskState::Terminal {
            outcome: TaskOutcome::Completed {
                result: json!({ "ok": true }),
            },
        },
    )
    .await;
    let graph = state.value().expect("current revision");
    assert!(graph.node(pending).is_none(), "a terminal task is not live");
    assert_eq!(graph.nodes().count(), 0);

    session.close().await.expect("close");
}

#[tokio::test(flavor = "current_thread")]
async fn a_mount_lives_while_it_has_an_observer_and_is_rebuilt_after_the_last_one_leaves() {
    let session = open_session();
    let conversation = create_conversation(&session).await;
    let view = TaskGraphView::open(session.clone(), Arc::new(NoBlocking))
        .await
        .expect("open task graph");

    let first_log = log();
    let first = watch(&view, &first_log).await;
    assert!(view.is_mounted());
    assert!(recorded(&first_log).frames.is_empty());

    // A task created after the mount advances it with one exact set.
    let id = create_task(&session, conversation, "demo", 1).await;
    {
        let state = recorded(&first_log);
        assert_eq!(state.frames.len(), 1);
        let frame = &state.frames[0];
        let node = frame.value.node(id).expect("task is live");
        assert_eq!(node.kind, "demo");
        let key = id.as_i64().to_string();
        assert_eq!(paths(&frame.ops), vec![task_path(&key)]);
        let Op::Set(_, payload) = &frame.ops[0] else {
            panic!("expected a set");
        };
        assert_eq!(payload["id"], json!(id.as_i64()));
        assert_eq!(payload["state"]["status"], json!("pending"));
    }

    // A second observer joins the same mount: it starts from the current
    // revision and receives no replay, because `start` drains what is pending
    // (here, nothing).
    let second_log = log();
    let second = watch(&view, &second_log).await;
    assert!(recorded(&second_log).frames.is_empty());
    assert_eq!(
        second.value().expect("current revision").nodes().count(),
        1,
        "a joining observer starts from the current revision"
    );

    // Every observer sees the next change.
    set_state(
        &session,
        id,
        TaskState::Running {
            checkpoint: checkpoint("work"),
        },
    )
    .await;
    assert_eq!(recorded(&first_log).frames.len(), 2);
    assert_eq!(recorded(&second_log).frames.len(), 1);
    assert_eq!(
        recorded(&second_log).frames[0]
            .value
            .node(id)
            .expect("live")
            .state,
        TaskGraphState::Running {
            phase: "work".to_owned()
        }
    );

    // Detaching one observer keeps the shared mount alive.
    second.stop();
    assert!(view.is_mounted());
    set_state(
        &session,
        id,
        TaskState::Sleeping {
            checkpoint: checkpoint("wait"),
            until: 5_000,
        },
    )
    .await;
    assert_eq!(recorded(&first_log).frames.len(), 3);
    assert_eq!(
        recorded(&second_log).frames.len(),
        1,
        "a stopped watch receives nothing"
    );

    // The last observer leaving drops the mount.
    first.stop();
    assert!(!view.is_mounted());
    assert!(!view.is_closed(), "stopping observers is not closing");

    // A change committed after the mount dropped is committed storage that
    // nothing observed; a later observer must rebuild from it.
    set_state(
        &session,
        id,
        TaskState::Pending {
            checkpoint: checkpoint("again"),
        },
    )
    .await;
    let third_log = log();
    let third = watch(&view, &third_log).await;
    assert!(view.is_mounted());
    assert_eq!(
        third
            .value()
            .expect("current revision")
            .node(id)
            .expect("live")
            .state,
        TaskGraphState::Pending {
            phase: "again".to_owned()
        },
        "the rebuild reads committed storage"
    );
    third.stop();

    session.close().await.expect("close");
}

#[tokio::test(flavor = "current_thread")]
async fn a_state_handle_is_disposable_and_ends_with_the_session() {
    let session = open_session();
    let conversation = create_conversation(&session).await;
    let view = TaskGraphView::open(session.clone(), Arc::new(NoBlocking))
        .await
        .expect("open task graph");

    let id = create_task(&session, conversation, "demo", 1).await;
    let state = view.state(&Context::background()).await.expect("state");
    assert!(state.ended().is_none());
    assert!(state.value().expect("current").node(id).is_some());

    set_state(
        &session,
        id,
        TaskState::Waiting {
            checkpoint: checkpoint("join"),
            on: vec![TaskId::from_raw(99)],
            policy: JoinPolicy::FailFast,
        },
    )
    .await;
    let graph = state.value().expect("current");
    let node = graph.node(id).expect("live");
    assert_eq!(
        node.state,
        TaskGraphState::Waiting {
            phase: "join".to_owned(),
            on: vec![TaskId::from_raw(99)],
            policy: JoinPolicy::FailFast,
        }
    );

    session.close().await.expect("close");

    // Close ends the observer with the session's own reason.
    assert_eq!(state.ended(), Some(WatchEnd::SessionClosed));
    assert_eq!(
        state
            .value()
            .expect("the last revision survives the close")
            .node(id)
            .map(|node| node.kind.clone()),
        Some("demo".to_owned())
    );
    assert!(view.is_closed());
    assert!(!view.is_mounted(), "close drops the mount");

    // A state handle after the close is refused.
    let error = view
        .state(&Context::background())
        .await
        .err()
        .expect("a closed session refuses a new observer");
    assert!(
        matches!(error, pir::durable::StorageError::Closed(_)),
        "unexpected error: {error:?}"
    );
    drop(state);
}

// ── ownership and conversations ────────────────────────────────────────────

#[tokio::test(flavor = "current_thread")]
async fn a_task_shows_the_conversations_it_owns() {
    let session = open_session();
    let conversation = create_conversation(&session).await;
    let view = TaskGraphView::open(session.clone(), Arc::new(NoBlocking))
        .await
        .expect("open task graph");
    let id = create_task(&session, conversation, "demo", 1).await;

    let state = view.state(&Context::background()).await.expect("state");
    assert!(
        state
            .value()
            .expect("current")
            .node(id)
            .expect("live")
            .conversations
            .is_empty(),
        "the task owns no conversation yet"
    );

    // A conversation created by the task appears on its node in one commit.
    let owned = session
        .create_conversation(ConversationOwnership::Task(id), None, Context::background())
        .await
        .expect("create owned conversation")
        .id;
    let graph = state.value().expect("current");
    assert_eq!(
        graph.node(id).expect("live").conversations,
        vec![owned],
        "the owned conversation is listed"
    );
    assert!(
        graph.node(id).expect("live").owner.is_none(),
        "a conversation-owned task has no task owner"
    );

    session.close().await.expect("close");
}

#[tokio::test(flavor = "current_thread")]
async fn a_conversation_created_for_a_settled_task_does_not_revive_its_node() {
    let session = open_session();
    let conversation = create_conversation(&session).await;
    let view = TaskGraphView::open(session.clone(), Arc::new(NoBlocking))
        .await
        .expect("open task graph");
    let id = create_task(&session, conversation, "demo", 1).await;

    let state = view.state(&Context::background()).await.expect("state");
    set_state(
        &session,
        id,
        TaskState::Terminal {
            outcome: TaskOutcome::Aborted {
                reason: None,
                result: None,
            },
        },
    )
    .await;
    assert!(state.value().expect("current").node(id).is_none());

    // Upstream's ownership check only requires the owner task to exist, so a
    // settled task can still own a conversation. The graph must not resurrect
    // its node for it: `advance` appends conversations only to nodes that are
    // live.
    let owned = session
        .create_conversation(ConversationOwnership::Task(id), None, Context::background())
        .await
        .expect("a settled owner still owns a conversation")
        .id;
    let graph = state.value().expect("current");
    assert!(
        graph.node(id).is_none(),
        "the settled task's node stays gone, so nothing was appended to it"
    );
    assert_eq!(graph.nodes().count(), 0);

    // The committed ownership edge is real; only the live graph is empty.
    let record = session
        .conversation(owned, &Context::background())
        .await
        .expect("read conversation")
        .expect("conversation exists");
    assert_eq!(
        record.owner.as_ref().map(|owner| owner.task_id),
        Some(id),
        "the durable owner edge was written"
    );

    session.close().await.expect("close");
}

// ── blocked reasons ────────────────────────────────────────────────────────

/// A real scheduler over its own session, so the blocked reason runs through the
/// production `TaskScheduler::blocked_reason`.
struct BlockingWorld {
    storage: Arc<MemoryStorage>,
    scheduler: TaskScheduler,
    conversation: ConversationId,
}

impl BlockingWorld {
    async fn new(tasks: Vec<Task>) -> Self {
        let storage = Arc::new(MemoryStorage::new());
        let settings = Arc::new(Settings::default());
        let options = TaskSchedulerOptions {
            settings: Some(Arc::new(move || settings.as_ref().clone())),
            ..TaskSchedulerOptions::default()
        };
        let scheduler = TaskScheduler::open_with(options, storage.clone(), tasks)
            .await
            .expect("open scheduler");
        let conversation = scheduler
            .session()
            .create_conversation(
                ConversationOwnership::Ownerless,
                None,
                Context::background(),
            )
            .await
            .expect("create conversation")
            .id;
        Self {
            storage,
            scheduler,
            conversation,
        }
    }

    async fn create(&self, kind: &str, version: i64) -> TaskId {
        // The session's `create_task` needs metadata only, so a task can be
        // committed for a kind no catalogue holds — exactly the state a
        // `missing_task` blocked reason describes.
        self.scheduler
            .session()
            .create_task(
                &definition(kind, version),
                json!({}),
                TaskOptions {
                    trace: None,
                    conversation_id: Some(self.conversation),
                    ownership: TaskOwnership::Conversation,
                    background: false,
                    abandon_on_restart: false,
                    submission_ids: Vec::new(),
                },
                Context::background(),
            )
            .await
            .expect("create task")
    }
}

/// A definition with one phase, so the catalogue holds it.
fn demo_task(kind: &str, version: i64) -> Task {
    define_task(kind, version, |_input| checkpoint("run"))
        .phase(
            "run",
            phase_handler(|_task, _runtime, _context| Box::pin(async {})),
        )
        .build()
        .expect("build task")
}

#[tokio::test(flavor = "current_thread")]
async fn a_task_with_no_registered_definition_is_blocked_missing_task() {
    let world = BlockingWorld::new(Vec::new()).await;
    // The session's `create_task` needs no catalogue entry, so this commits a
    // record no definition can take over.
    let id = world.create("ghost", 1).await;

    let view = TaskGraphView::open(
        world.scheduler.session().clone(),
        Arc::new(world.scheduler.clone()),
    )
    .await
    .expect("open task graph");
    let state = view.state(&Context::background()).await.expect("state");
    assert_eq!(
        state
            .value()
            .expect("current")
            .node(id)
            .expect("live")
            .blocked,
        Some(BlockedReason::MissingTask)
    );

    // Installing the definition clears the reason without a commit: the
    // catalogue's change listener republishes it.
    let installed = world.scheduler.catalogue();
    installed.install(vec![demo_task("ghost", 1)]);
    assert_eq!(
        state
            .value()
            .expect("current")
            .node(id)
            .expect("live")
            .blocked,
        None,
        "the catalogue change cleared the reason"
    );

    world.scheduler.session().close().await.expect("close");
    drop(world.storage);
}

#[tokio::test(flavor = "current_thread")]
async fn a_task_stored_above_its_definition_is_blocked_task_too_old() {
    let world = BlockingWorld::new(vec![demo_task("demo", 1)]).await;
    let id = world.create("demo", 5).await;

    let view = TaskGraphView::open(
        world.scheduler.session().clone(),
        Arc::new(world.scheduler.clone()),
    )
    .await
    .expect("open task graph");
    let state = view.state(&Context::background()).await.expect("state");
    assert_eq!(
        state
            .value()
            .expect("current")
            .node(id)
            .expect("live")
            .blocked,
        Some(BlockedReason::TaskTooOld)
    );

    world.scheduler.session().close().await.expect("close");
    drop(world.storage);
}

#[tokio::test(flavor = "current_thread")]
async fn a_newer_definition_without_a_migration_is_blocked_migration_failed() {
    let world = BlockingWorld::new(vec![demo_task("demo", 2)]).await;
    let id = world.create("demo", 1).await;

    let view = TaskGraphView::open(
        world.scheduler.session().clone(),
        Arc::new(world.scheduler.clone()),
    )
    .await
    .expect("open task graph");
    let state = view.state(&Context::background()).await.expect("state");
    assert_eq!(
        state
            .value()
            .expect("current")
            .node(id)
            .expect("live")
            .blocked,
        Some(BlockedReason::MigrationFailed),
        "a newer definition with no migration cannot take the task over"
    );

    // Installing a definition that does migrate clears the reason.
    let migrating = define_task("demo", 2, |_input| checkpoint("run"))
        .phase(
            "run",
            phase_handler(|_task, _runtime, _context| Box::pin(async {})),
        )
        .migrate(|_from, _checkpoint| Ok(checkpoint("run")))
        .build()
        .expect("build task");
    world.scheduler.catalogue().install(vec![migrating]);
    assert_eq!(
        state
            .value()
            .expect("current")
            .node(id)
            .expect("live")
            .blocked,
        None,
        "a migration present clears the reason"
    );

    world.scheduler.session().close().await.expect("close");
    drop(world.storage);
}

#[tokio::test(flavor = "current_thread")]
async fn a_blocked_reason_change_reaches_a_watch_as_a_field_set() {
    let world = BlockingWorld::new(Vec::new()).await;
    let id = world.create("ghost", 1).await;

    let view = TaskGraphView::open(
        world.scheduler.session().clone(),
        Arc::new(world.scheduler.clone()),
    )
    .await
    .expect("open task graph");
    let observer_log = log();
    let observer = watch(&view, &observer_log).await;
    assert_eq!(
        observer
            .value()
            .expect("current")
            .node(id)
            .expect("live")
            .blocked,
        Some(BlockedReason::MissingTask)
    );

    world
        .scheduler
        .catalogue()
        .install(vec![demo_task("ghost", 1)]);

    {
        let state = recorded(&observer_log);
        assert_eq!(state.frames.len(), 1, "one refresh, one frame");
        let key = id.as_i64().to_string();
        assert_eq!(
            paths(&state.frames[0].ops),
            vec![task_field_path(&key, "blocked")],
            "only the changed field is republished"
        );
        assert_eq!(
            state.frames[0].value.node(id).expect("live").blocked,
            None,
            "the frame carries the cleared reason"
        );
    }

    observer.stop();
    world.scheduler.session().close().await.expect("close");
    drop(world.storage);
}

#[tokio::test(flavor = "current_thread")]
async fn an_unchanged_reason_publishes_nothing() {
    let world = BlockingWorld::new(Vec::new()).await;
    let id = world.create("ghost", 1).await;

    let view = TaskGraphView::open(
        world.scheduler.session().clone(),
        Arc::new(world.scheduler.clone()),
    )
    .await
    .expect("open task graph");
    let observer_log = log();
    let observer = watch(&view, &observer_log).await;

    // Two catalogue changes that both leave the task missing: the second
    // refresh derives the same reason and publishes nothing.
    world
        .scheduler
        .catalogue()
        .install(vec![demo_task("other", 1)]);
    assert_eq!(recorded(&observer_log).frames.len(), 0);
    world
        .scheduler
        .catalogue()
        .install(vec![demo_task("another", 1)]);
    assert_eq!(
        recorded(&observer_log).frames.len(),
        0,
        "an unchanged reason is not republished"
    );
    assert_eq!(
        observer
            .value()
            .expect("current")
            .node(id)
            .expect("live")
            .blocked,
        Some(BlockedReason::MissingTask)
    );

    observer.stop();
    world.scheduler.session().close().await.expect("close");
    drop(world.storage);
}

// ── watch framing ──────────────────────────────────────────────────────────

#[tokio::test(flavor = "current_thread")]
async fn a_listener_that_returns_false_ends_the_watch() {
    let session = open_session();
    let conversation = create_conversation(&session).await;
    let view = TaskGraphView::open(session.clone(), Arc::new(NoBlocking))
        .await
        .expect("open task graph");

    let watch = view.watch(&Context::background()).await.expect("watch");
    let seen = Arc::new(Mutex::new(Vec::<usize>::new()));
    let seen_for_listener = seen.clone();
    watch.start(move |frame| {
        seen_for_listener
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(frame.value.nodes().count());
        false
    });

    let _first = create_task(&session, conversation, "demo", 1).await;
    assert!(watch.is_stopped(), "the watch stopped itself");
    assert_eq!(watch.ended(), Some(WatchEnd::Stopped));
    assert_eq!(
        *seen.lock().unwrap(),
        vec![1],
        "the listener saw exactly the frame that stopped it"
    );

    // A stopped watch receives nothing more.
    let _second = create_task(&session, conversation, "demo", 1).await;
    assert_eq!(seen.lock().unwrap().len(), 1);
    assert!(
        !view.is_mounted(),
        "stopping the last observer dropped the mount"
    );

    session.close().await.expect("close");
}

#[tokio::test(flavor = "current_thread")]
async fn an_undelivered_suffix_collapses_to_one_root_replacement() {
    let session = open_session();
    let conversation = create_conversation(&session).await;
    let view = TaskGraphView::open(session.clone(), Arc::new(NoBlocking))
        .await
        .expect("open task graph");

    // A watch with no listener queues frames instead of delivering them.
    let watch = view.watch(&Context::background()).await.expect("watch");
    let id = create_task(&session, conversation, "demo", 1).await;

    // Queue one frame per committed change, well past the bound.
    for step in 0..MAX_PENDING_WATCH_FRAMES {
        set_state(
            &session,
            id,
            TaskState::Running {
                checkpoint: checkpoint(&format!("step-{step}")),
            },
        )
        .await;
        set_state(
            &session,
            id,
            TaskState::Pending {
                checkpoint: checkpoint(&format!("step-{step}")),
            },
        )
        .await;
    }

    let mut delivered = 0;
    let mut collapsed = false;
    while let Some(frame) = watch.next_frame() {
        delivered += 1;
        if matches!(frame.ops.as_slice(), [Op::Replace(_)]) {
            collapsed = true;
            assert!(
                frame.value.node(id).is_some(),
                "the collapsed frame carries the newest revision"
            );
        }
    }
    assert!(
        delivered <= MAX_PENDING_WATCH_FRAMES,
        "the queue stayed bounded: {delivered} frames"
    );
    assert!(
        collapsed,
        "the undelivered suffix collapsed to one root replacement"
    );

    watch.stop();
    session.close().await.expect("close");
}

#[tokio::test(flavor = "current_thread")]
async fn a_close_ends_every_live_watch_with_the_session_reason() {
    let session = open_session();
    let view = TaskGraphView::open(session.clone(), Arc::new(NoBlocking))
        .await
        .expect("open task graph");

    let plain = view.watch(&Context::background()).await.expect("watch");
    let other_log = log();
    let observed = watch(&view, &other_log).await;

    session.close().await.expect("close");

    assert!(view.is_closed());
    assert!(!view.is_mounted(), "close drops the mount");
    assert_eq!(plain.ended(), Some(WatchEnd::SessionClosed));
    assert_eq!(observed.ended(), Some(WatchEnd::SessionClosed));
}

#[tokio::test(flavor = "current_thread")]
async fn an_aborted_context_is_refused_and_the_watch_never_registers() {
    let session = open_session();
    let view = TaskGraphView::open(session.clone(), Arc::new(NoBlocking))
        .await
        .expect("open task graph");

    let signal = pir::ai::types::AbortSignal::new();
    signal.abort();
    let context = Context::with_signal(signal);
    let error = view
        .watch(&context)
        .await
        .err()
        .expect("an aborted context is refused");
    assert!(
        matches!(error, pir::durable::StorageError::Request(_)),
        "unexpected error: {error:?}"
    );
    assert!(!view.is_mounted(), "nothing was registered");

    session.close().await.expect("close");
}

// ── reports ────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "current_thread")]
async fn a_panicking_observer_is_reported_and_does_not_starve_the_others() {
    let session = open_session();
    let conversation = create_conversation(&session).await;
    let reports: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let report_sink = reports.clone();
    let view = TaskGraphView::open_with_report(
        session.clone(),
        Arc::new(NoBlocking),
        Arc::new(move |message: &str| {
            report_sink
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(message.to_owned());
        }),
    )
    .await
    .expect("open task graph");

    let panicking = view.watch(&Context::background()).await.expect("watch");
    panicking.start(|_frame| panic!("observer exploded"));

    let surviving_log = log();
    let surviving = watch(&view, &surviving_log).await;

    let id = create_task(&session, conversation, "demo", 1).await;

    assert!(
        !reports
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty(),
        "the panic was reported"
    );
    assert_eq!(
        recorded(&surviving_log).frames.len(),
        1,
        "the healthy observer still got the revision"
    );
    assert!(
        recorded(&surviving_log).frames[0].value.node(id).is_some(),
        "and it carries the change"
    );

    panicking.stop();
    surviving.stop();
    session.close().await.expect("close");
}
