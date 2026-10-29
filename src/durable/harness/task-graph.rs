//! The session's live task graph (spec §9.5).
//!
//! Port of upstream `packages/durable/src/harness/task-graph.ts`. The graph is
//! a structural mount over the session's task table: every **live** task
//! (`pending`, `running`, `waiting`, `sleeping`, `completing`) with its durable
//! status, minus its checkpoint and outcome payloads, plus the conversations it
//! owns.
//!
//! # What drives it
//!
//! * A mount is built by its first observer on the Session line — one committed
//!   state for every live task and every owned conversation — and dropped with
//!   its last observer.
//! * [`TaskGraphView`] registers one **internal** commit listener, so a task
//!   change advances the mount synchronously on the line.
//! * The blocked reason is **not** durable: it is derived from the registry and
//!   the restart limit, and it can change without a commit. The
//!   [`TaskGraphBlocking`] source announces those changes
//!   ([`TaskGraphBlocking::subscribe_blocked`]) and the view re-derives every
//!   node's reason in one `read_on_line` job, publishing only the ones that
//!   changed — exactly upstream `#refreshBlocked`.
//!
//! # Deliberate deviations from upstream
//!
//! * **No chord state types.** Upstream's `state()`/`watch()` return a chord
//!   `AttachedReplicatedState`/`WatchHandle`. The Rust kernel's observation
//!   types are document-JSON specific, so [`TaskGraphState`] and
//!   [`TaskGraphWatch`] are typed handles over the same mount: both carry the
//!   current revision; the watch additionally queues exact frames, serializes
//!   its callbacks and collapses its undelivered suffix past
//!   [`MAX_PENDING_WATCH_FRAMES`].
//! * **The mount is typed, the operations are exact.** Upstream replays the
//!   emitted operations against a plain object with `applyImmutable`. The port
//!   mutates the typed mount directly while emitting the same immutable
//!   `set`/`delete` batches to observers.
//! * **`BlockedSubject` carries its status.** Upstream's `BlockedSubject` is a
//!   `Pick` of the record plus a `status` field; the Rust [`BlockedSubject`] is
//!   the same set of fields as an owned struct, and
//!   [`BlockedSubject::to_record`] rebuilds the minimal record
//!   [`TaskScheduler::blocked_reason`](crate::durable::harness::scheduler::TaskScheduler::blocked_reason)
//!   reads. Only identity, the abort mark, the restart count and the status
//!   discriminator are meaningful there.
//! * **Registry changes are the blocked signal.** Upstream also notifies on a
//!   failed migration; in this port the catalogue is the only non-commit source
//!   of a changed reason, and it is what the supplied [`TaskScheduler`]
//!   implementation subscribes to.
//! * **A change compares typed nodes.** Upstream compares `JSON.stringify` of
//!   the previous and next node; the port compares the typed nodes, which is
//!   the same equality with no serialization step.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::durable::context::Context;
use crate::durable::delta::{Op, Seg};
use crate::durable::errors::{StorageError, StorageRequestError};
use crate::durable::harness::scheduler::{BlockedReason, TaskScheduler};
use crate::durable::harness::util::{closed_error, contained, scan_all, Report};
use crate::durable::ids::{ConversationId, TaskId};
use crate::durable::session::facade::Session;
use crate::durable::session::line::{JobValue, SessionState};
use crate::durable::session::observation::{WatchEnd, MAX_PENDING_WATCH_FRAMES};
use crate::durable::session::publication::{
    CommitChange, CommitPublication, InternalListener, TableCommitChange,
};
use crate::durable::types::{
    ConversationQuery, JoinPolicy, TaskOutcome, TaskQuery, TaskRecord, TaskState, TaskStatus,
};

/// Page size of every scan (upstream `SCAN_PAGE_SIZE`).
const SCAN_PAGE_SIZE: usize = 256;

/// The statuses of a task that is still live, in the order they are scanned
/// (upstream `LIVE_STATUSES`).
pub const LIVE_STATUSES: [TaskStatus; 5] = [
    TaskStatus::Pending,
    TaskStatus::Running,
    TaskStatus::Waiting,
    TaskStatus::Sleeping,
    TaskStatus::Completing,
];

/// A live task's durable status without its checkpoint and outcome payloads.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "camelCase")]
pub enum TaskGraphState {
    /// Eligible for scheduling.
    Pending {
        /// The checkpoint's phase.
        phase: String,
    },
    /// Reserved by one in-memory invocation.
    Running {
        /// The checkpoint's phase.
        phase: String,
    },
    /// Parked until every task in `on` is terminal.
    Waiting {
        /// The checkpoint's phase.
        phase: String,
        /// The tasks it waits for.
        on: Vec<TaskId>,
        /// How the wait treats failures.
        policy: JoinPolicy,
    },
    /// Asleep until the harness clock reaches `until`.
    Sleeping {
        /// The checkpoint's phase.
        phase: String,
        /// Wake-up time in milliseconds on the harness clock.
        until: i64,
    },
    /// Outcome held until its ordinary owned work drains.
    Completing {
        /// The decided outcome's status.
        outcome: OutcomeStatus,
    },
}

/// The `status` discriminator of an outcome.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutcomeStatus {
    /// Completed with a result.
    Completed,
    /// Expected task or domain failure.
    Failed,
    /// Explicit cancellation.
    Aborted,
    /// The definition or migration is unavailable.
    Orphaned,
    /// Runtime-detected contract failure.
    Faulted,
}

impl OutcomeStatus {
    /// The status of a durable outcome.
    #[must_use]
    pub fn of(outcome: &TaskOutcome) -> Self {
        match outcome {
            TaskOutcome::Completed { .. } => Self::Completed,
            TaskOutcome::Failed { .. } => Self::Failed,
            TaskOutcome::Aborted { .. } => Self::Aborted,
            TaskOutcome::Orphaned { .. } => Self::Orphaned,
            TaskOutcome::Faulted { .. } => Self::Faulted,
        }
    }

    /// The wire string.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Aborted => "aborted",
            Self::Orphaned => "orphaned",
            Self::Faulted => "faulted",
        }
    }
}

/// One live task, as the graph shows it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskGraphNode {
    /// The task identifier.
    pub id: TaskId,
    /// Registered definition name.
    pub kind: String,
    /// Conversation that owns the task.
    pub conversation_id: ConversationId,
    /// Owner task; absent for a conversation-owned task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<TaskId>,
    /// Whether the task is excluded from idle waits and aborts.
    pub background: bool,
    /// Durable abort mark.
    pub abort_requested: bool,
    /// The durable status.
    pub state: TaskGraphState,
    /// Why it cannot run here, when it cannot: no registered definition takes
    /// it (`missing_task`, `task_too_old`, `migration_failed`), or the restart
    /// limit parked it (`restart_limit`). Follows registry changes.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_blocked",
        deserialize_with = "deserialize_blocked"
    )]
    pub blocked: Option<BlockedReason>,
    /// Conversations this task owns, in ID order.
    pub conversations: Vec<ConversationId>,
}

/// Every live task of the session (spec §9.5).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TaskGraph {
    /// Every live task, keyed by its decimal ID.
    pub tasks: BTreeMap<String, TaskGraphNode>,
}

impl TaskGraph {
    /// The node for `id`, if it is live.
    #[must_use]
    pub fn node(&self, id: TaskId) -> Option<&TaskGraphNode> {
        self.tasks.get(&id.as_i64().to_string())
    }

    /// Every live node, in ID order.
    pub fn nodes(&self) -> impl Iterator<Item = &TaskGraphNode> {
        self.tasks.values()
    }
}

fn serialize_blocked<S: serde::Serializer>(
    blocked: &Option<BlockedReason>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    match blocked {
        Some(reason) => serializer.serialize_str(reason.as_str()),
        None => serializer.serialize_none(),
    }
}

fn deserialize_blocked<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<BlockedReason>, D::Error> {
    let value = Option::<String>::deserialize(deserializer)?;
    match value.as_deref() {
        None => Ok(None),
        Some(text) => blocked_reason_of(text)
            .map(Some)
            .ok_or_else(|| serde::de::Error::custom("unknown blocked reason")),
    }
}

/// The reason a wire string names.
#[must_use]
pub fn blocked_reason_of(text: &str) -> Option<BlockedReason> {
    match text {
        "missing_task" => Some(BlockedReason::MissingTask),
        "task_too_old" => Some(BlockedReason::TaskTooOld),
        "migration_failed" => Some(BlockedReason::MigrationFailed),
        "restart_limit" => Some(BlockedReason::RestartLimit),
        _ => None,
    }
}

/// What the graph asks the scheduler: a task's blocked reason, and when reasons
/// may change without a commit.
///
/// Upstream types this structurally; a trait is the Rust form, and
/// [`TaskScheduler`] implements it.
pub trait TaskGraphBlocking: Send + Sync {
    /// Why `subject` cannot run here, if it cannot.
    fn blocked(&self, subject: &BlockedSubject) -> Option<BlockedReason>;

    /// Call `listener` when a blocked reason may have changed without a
    /// commit.
    fn subscribe_blocked(&self, listener: BlockedListener);
}

/// A blocked-reason change listener.
pub type BlockedListener = Arc<dyn Fn() + Send + Sync>;

/// What a blocked reason reads of a task: the task graph keeps only this of
/// each record (upstream `BlockedSubject`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockedSubject {
    /// The task identifier.
    pub id: TaskId,
    /// Registered definition name.
    pub kind: String,
    /// Stored definition version.
    pub version: i64,
    /// Durable abort mark.
    pub abort_requested: bool,
    /// Why the mark was set, when not by an abort request.
    pub abort_reason: Option<String>,
    /// Consecutive crashes that cut the task off while it ran.
    pub restarts: Option<i64>,
    /// The status discriminator.
    pub status: TaskStatus,
}

impl BlockedSubject {
    /// The fields `blockedReason()` reads of `record`.
    #[must_use]
    pub fn of(record: &TaskRecord) -> Self {
        Self {
            id: record.id,
            kind: record.kind.clone(),
            version: record.version,
            abort_requested: record.abort_requested,
            abort_reason: record.abort_reason.clone(),
            restarts: record.restarts,
            status: record.state.status(),
        }
    }

    /// The minimal record these fields describe, as the scheduler's
    /// `blocked_reason` reads it.
    ///
    /// No blocked-reason decision reads a checkpoint, an input or an outcome
    /// payload, so the placeholders here are never observed.
    #[must_use]
    pub fn to_record(&self) -> TaskRecord {
        let completed = || TaskOutcome::Completed {
            result: Value::Null,
        };
        let state = match self.status {
            TaskStatus::Pending => TaskState::Pending {
                checkpoint: Value::Null,
            },
            TaskStatus::Running => TaskState::Running {
                checkpoint: Value::Null,
            },
            TaskStatus::Waiting => TaskState::Waiting {
                checkpoint: Value::Null,
                on: Vec::new(),
                policy: JoinPolicy::FailFast,
            },
            TaskStatus::Sleeping => TaskState::Sleeping {
                checkpoint: Value::Null,
                until: 0,
            },
            TaskStatus::Completing => TaskState::Completing {
                outcome: completed(),
            },
            TaskStatus::Terminal => TaskState::Terminal {
                outcome: completed(),
            },
        };
        TaskRecord {
            id: self.id,
            conversation_id: ConversationId::from_raw(0),
            kind: self.kind.clone(),
            version: self.version,
            input: Value::Null,
            owner: None,
            background: false,
            abort_requested: self.abort_requested,
            abort_reason: self.abort_reason.clone(),
            abandon_on_restart: None,
            started_at: None,
            ended_at: None,
            restarts: self.restarts,
            state,
            memos: None,
            trace: None,
            links: Vec::new(),
        }
    }
}

impl TaskGraphBlocking for TaskScheduler {
    fn blocked(&self, subject: &BlockedSubject) -> Option<BlockedReason> {
        TaskScheduler::blocked_reason(self, &subject.to_record())
    }

    fn subscribe_blocked(&self, listener: BlockedListener) {
        // The catalogue's change listeners are the port's "reasons may have
        // changed without a commit" signal: installing or replacing a
        // definition is what lets a blocked task run (spec §5.4).
        self.catalogue().subscribe(listener);
    }
}

/// A blocking source that never blocks: the default when no scheduler is
/// attached.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoBlocking;

impl TaskGraphBlocking for NoBlocking {
    fn blocked(&self, _subject: &BlockedSubject) -> Option<BlockedReason> {
        None
    }

    fn subscribe_blocked(&self, _listener: BlockedListener) {}
}

/// One observed revision of the graph.
#[derive(Clone, Debug)]
pub struct TaskGraphFrame {
    /// The immutable revision.
    pub value: Arc<TaskGraph>,
    /// The exact operation batch that produced it.
    pub ops: Vec<Op>,
    /// The frame's context, without the producer's cancellation.
    pub context: Context,
}

/// Receives each revision of the graph, and the session's close.
trait GraphObserver: Send + Sync {
    fn advance(&self, value: Arc<TaskGraph>, ops: &[Op], context: &Context);
    fn close_session(&self, failure: Option<&StorageError>);

    /// Accept the mount registration this observer must release when it ends
    /// itself.
    ///
    /// Upstream's `CommittedWatch` is constructed with its `detach` and calls
    /// it from `#terminate`, so a watch that ends from its own listener
    /// releases the mount. A disposable state is released by its holder
    /// instead, so the default is a no-op that drops `release` immediately.
    fn bind_release(&self, _release: Box<dyn Fn() + Send + Sync>) {}
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn end_reason(failure: Option<&StorageError>) -> WatchEnd {
    match failure {
        Some(error) => WatchEnd::SessionFailed(error.clone()),
        None => WatchEnd::SessionClosed,
    }
}

/// The disposable state face: the newest revision.
#[derive(Debug)]
struct StateObserver {
    current: Mutex<Option<Arc<TaskGraph>>>,
    ended: Mutex<Option<WatchEnd>>,
}

impl StateObserver {
    fn new(value: Arc<TaskGraph>) -> Self {
        Self {
            current: Mutex::new(Some(value)),
            ended: Mutex::new(None),
        }
    }
}

impl GraphObserver for StateObserver {
    fn advance(&self, value: Arc<TaskGraph>, _ops: &[Op], _context: &Context) {
        if lock(&self.ended).is_some() {
            return;
        }
        *lock(&self.current) = Some(value);
    }

    fn close_session(&self, failure: Option<&StorageError>) {
        let mut ended = lock(&self.ended);
        if ended.is_none() {
            *ended = Some(end_reason(failure));
        }
    }
}

/// A watch listener; returning `false` ends the watch.
type WatchListener = dyn Fn(TaskGraphFrame) -> bool + Send + Sync + 'static;

/// The serialized exact-frame watch face.
struct WatchObserver {
    current: Mutex<Option<Arc<TaskGraph>>>,
    pending: Mutex<Vec<TaskGraphFrame>>,
    ended: Mutex<Option<WatchEnd>>,
    listener: Mutex<Option<Box<WatchListener>>>,
    /// Releases this observer's mount registration. Set once, by `attach`,
    /// after the registration exists; called by [`WatchObserver::end`].
    ///
    /// Upstream's `CommittedWatch#terminate` calls the same `detach` its
    /// constructor was given, so a watch that ends — from `stop`, from a
    /// listener returning `false`, or from the session closing — always
    /// unregisters. Without this the mount would outlive its last observer.
    release: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
}

impl WatchObserver {
    fn new(value: Arc<TaskGraph>) -> Self {
        Self {
            current: Mutex::new(Some(value)),
            pending: Mutex::new(Vec::new()),
            ended: Mutex::new(None),
            listener: Mutex::new(None),
            release: Mutex::new(None),
        }
    }

    /// Install the sole listener and drain what is pending.
    fn start(&self, listener: Box<dyn Fn(TaskGraphFrame) -> bool + Send + Sync>) {
        {
            let mut slot = lock(&self.listener);
            if lock(&self.ended).is_some() {
                return;
            }
            *slot = Some(listener);
        }
        self.drain();
    }

    /// Install the mount registration this observer must release.
    ///
    /// A watch that already ended releases immediately: `attach` can install
    /// the registration only after the observer exists, and close or
    /// cancellation may win that window.
    fn bind_release(&self, release: Box<dyn Fn() + Send + Sync>) {
        let ended = lock(&self.ended).is_some();
        if ended {
            release();
            return;
        }
        let mut slot = lock(&self.release);
        if lock(&self.ended).is_some() {
            drop(slot);
            release();
            return;
        }
        *slot = Some(release);
    }

    /// Take the next pending frame without consuming it.
    fn peek_frame(&self) -> Option<TaskGraphFrame> {
        lock(&self.pending).first().cloned()
    }

    /// Drop the frame [`WatchObserver::peek_frame`] returned.
    ///
    /// The pair exists because the listener runs with no lock held, so
    /// `advance` can push while it runs; removing by index-of-first can only
    /// ever drop the frame the listener already saw.
    fn take_peeked(&self) {
        let mut pending = lock(&self.pending);
        if !pending.is_empty() {
            pending.remove(0);
        }
    }

    /// Take the next pending frame.
    fn next_frame(&self) -> Option<TaskGraphFrame> {
        let mut pending = lock(&self.pending);
        if pending.is_empty() {
            None
        } else {
            Some(pending.remove(0))
        }
    }

    /// End the watch once: record the reason, drop the undelivered suffix and
    /// unregister the mount.
    fn end(&self, reason: WatchEnd) {
        {
            let mut ended = lock(&self.ended);
            if ended.is_some() {
                return;
            }
            *ended = Some(reason);
        }
        lock(&self.pending).clear();
        // Released outside every other lock: the release re-enters the view's
        // shared state and the mount.
        let release = lock(&self.release).take();
        if let Some(release) = release {
            release();
        }
    }

    /// Deliver pending frames to the listener, one at a time.
    ///
    /// Only this function invokes the listener and no lock is held across the
    /// call, so a watch never runs two callbacks at once.
    ///
    /// A frame is consumed only when a listener is installed: before `start`
    /// the queue must keep every frame, so that `start` delivers the whole
    /// prefix in order (upstream `CommittedWatch#drain` returns without
    /// touching `#pending` when `#started` is false).
    fn drain(&self) {
        loop {
            if lock(&self.ended).is_some() {
                return;
            }
            let Some(frame) = self.peek_frame() else {
                return;
            };
            let keep_going = {
                let listener = lock(&self.listener);
                match listener.as_ref() {
                    Some(listener) => listener(frame),
                    None => return,
                }
            };
            // The listener saw the frame; drop exactly that one.
            self.take_peeked();
            if !keep_going {
                self.end(WatchEnd::Stopped);
                return;
            }
        }
    }
}

impl GraphObserver for WatchObserver {
    fn advance(&self, value: Arc<TaskGraph>, ops: &[Op], context: &Context) {
        if lock(&self.ended).is_some() {
            return;
        }
        {
            let mut pending = lock(&self.pending);
            if pending.len() >= MAX_PENDING_WATCH_FRAMES {
                // Collapse the undelivered suffix into one root replacement
                // built from this newest revision (spec §9).
                let root = serde_json::to_value(&*value).unwrap_or(Value::Null);
                pending.clear();
                pending.push(TaskGraphFrame {
                    value: value.clone(),
                    ops: vec![Op::Replace(root)],
                    context: context.clone(),
                });
            } else {
                pending.push(TaskGraphFrame {
                    value: value.clone(),
                    ops: ops.to_vec(),
                    context: context.clone(),
                });
            }
        }
        *lock(&self.current) = Some(value);
        self.drain();
    }

    fn close_session(&self, failure: Option<&StorageError>) {
        // `close_mount` has already dropped the mount and taken every observer
        // out of it, so the release here finds nothing to unregister; it is
        // still called so an observer that was detached early stays consistent.
        self.end(end_reason(failure));
    }

    fn bind_release(&self, release: Box<dyn Fn() + Send + Sync>) {
        WatchObserver::bind_release(self, release);
    }
}

/// One mount: its current revision, the blocked subject of each live task, and
/// its observers.
struct Mount {
    value: Arc<TaskGraph>,
    observers: Vec<(u64, Arc<dyn GraphObserver>)>,
    /// What the blocked reason reads of each live task, so a registry change can
    /// derive it again without payloads.
    subjects: BTreeMap<String, BlockedSubject>,
}

struct Shared {
    mount: Option<Arc<Mutex<Mount>>>,
    closed: bool,
    next_registration: u64,
}

/// A removable graph observer registration.
#[derive(Clone)]
struct GraphDetach {
    shared: Arc<Mutex<Shared>>,
    registration: u64,
}

impl GraphDetach {
    fn detach(&self) {
        let mount = match lock(&self.shared).mount.clone() {
            Some(mount) => mount,
            None => return,
        };
        let empty = {
            let mut mount = lock(&mount);
            mount
                .observers
                .retain(|(registration, _)| *registration != self.registration);
            mount.observers.is_empty()
        };
        if empty {
            let mut shared = lock(&self.shared);
            if shared
                .mount
                .as_ref()
                .is_some_and(|held| Arc::ptr_eq(held, &mount))
            {
                shared.mount = None;
            }
        }
    }
}

/// A disposable read-only state of the graph (upstream `state()`).
pub struct TaskGraphStateHandle {
    state: Arc<StateObserver>,
    detach: GraphDetach,
}

impl TaskGraphStateHandle {
    /// The newest revision.
    #[must_use]
    pub fn value(&self) -> Option<Arc<TaskGraph>> {
        lock(&self.state.current).clone()
    }

    /// Why the observer ended, if it did.
    #[must_use]
    pub fn ended(&self) -> Option<WatchEnd> {
        lock(&self.state.ended).clone()
    }

    /// Drop the observer; the mount is dropped with its last observer.
    pub fn detach(&self) {
        self.detach.detach();
    }
}

/// A serialized exact-frame watch of the graph (upstream `TaskGraphWatch`).
pub struct TaskGraphWatch {
    watch: Arc<WatchObserver>,
    detach: GraphDetach,
    stopped: Arc<AtomicBool>,
    /// The task awaiting this watch's acquisition-context cancellation, when
    /// one was installed.
    cancel: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Drop for TaskGraphWatch {
    fn drop(&mut self) {
        self.stop();
    }
}

impl TaskGraphWatch {
    /// The newest revision.
    #[must_use]
    pub fn value(&self) -> Option<Arc<TaskGraph>> {
        lock(&self.watch.current).clone()
    }

    /// Why the watch ended, if it did.
    #[must_use]
    pub fn ended(&self) -> Option<WatchEnd> {
        lock(&self.watch.ended).clone()
    }

    /// Whether the watch was stopped or ended.
    #[must_use]
    pub fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire) || lock(&self.watch.ended).is_some()
    }

    /// Install the frame listener (upstream `observer.onMessage`). Returning
    /// `false` ends the watch.
    pub fn start(&self, listener: impl Fn(TaskGraphFrame) -> bool + Send + Sync + 'static) {
        self.watch.start(Box::new(listener));
    }

    /// The next undelivered frame, if any.
    pub fn next_frame(&self) -> Option<TaskGraphFrame> {
        self.watch.next_frame()
    }

    /// Stop the watch. Idempotent.
    pub fn stop(&self) {
        if self.stopped.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Some(task) = lock(&self.cancel).take() {
            task.abort();
        }
        self.watch.end(WatchEnd::Stopped);
        self.detach.detach();
    }
}

/// The task graph mount of one session.
///
/// Open one per session (upstream keys it on the harness). It installs an
/// internal commit listener, a close listener and (through
/// [`TaskGraphBlocking::subscribe_blocked`]) a blocked-reason listener for the
/// life of the session.
pub struct TaskGraphView {
    session: Session,
    blocking: Arc<dyn TaskGraphBlocking>,
    shared: Arc<Mutex<Shared>>,
    report: Report,
}

impl TaskGraphView {
    /// Install the graph adapter over `session`, deriving blocked reasons from
    /// `blocking`.
    pub async fn open(
        session: Session,
        blocking: Arc<dyn TaskGraphBlocking>,
    ) -> Result<Self, StorageError> {
        Self::open_with_report(session, blocking, Arc::new(|_| {})).await
    }

    /// [`TaskGraphView::open`] with a report sink for contained listener
    /// errors (upstream `session.report`).
    pub async fn open_with_report(
        session: Session,
        blocking: Arc<dyn TaskGraphBlocking>,
        report: Report,
    ) -> Result<Self, StorageError> {
        let shared = Arc::new(Mutex::new(Shared {
            mount: None,
            closed: false,
            next_registration: 1,
        }));

        // Upstream `session.observeCommits((publication, context) => ...)`.
        let listener_shared = Arc::downgrade(&shared);
        let listener_blocking = blocking.clone();
        let listener_report = report.clone();
        let listener: InternalListener = Arc::new(move |publication, context| {
            if let Some(shared) = listener_shared.upgrade() {
                advance_mount(
                    &shared,
                    publication,
                    context,
                    listener_blocking.as_ref(),
                    &listener_report,
                );
            }
        });
        session
            .handle()
            .maintain(move |inner| {
                inner.internal_listeners.add(listener);
            })
            .await?;

        // Upstream `blocking.subscribeBlocked(() => this.#refreshBlocked())`.
        {
            let weak = Arc::downgrade(&shared);
            let refreshing_with = blocking.clone();
            let report = report.clone();
            blocking.subscribe_blocked(Arc::new(move || {
                let Some(shared) = weak.upgrade() else {
                    return;
                };
                refresh_blocked(&shared, &refreshing_with, &report);
            }));
        }

        // Upstream `session.subscribeClose(() => ...)`.
        let close_shared = Arc::downgrade(&shared);
        let close_report = report.clone();
        let close_session = session.clone();
        session
            .handle()
            .maintain(move |inner| {
                let shared = close_shared.clone();
                let report = close_report.clone();
                let session = close_session.clone();
                inner.close_listeners.push(Arc::new(move || {
                    let failure = Session::failure(&session);
                    if let Some(shared) = shared.upgrade() {
                        close_mount(&shared, failure.as_ref(), &report);
                    }
                }));
            })
            .await?;

        Ok(Self {
            session,
            blocking,
            shared,
            report,
        })
    }

    /// Report a listener error (upstream `session.report(error)`).
    pub fn report(&self, message: &str) {
        (self.report)(message);
    }

    /// Whether the session closed and dropped the mount.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        lock(&self.shared).closed
    }

    /// Whether a mount is live.
    #[must_use]
    pub fn is_mounted(&self) -> bool {
        lock(&self.shared).mount.is_some()
    }

    /// A disposable read-only state of the graph.
    ///
    /// # Errors
    ///
    /// The session closed or failed, or the context was aborted.
    pub async fn state(&self, context: &Context) -> Result<TaskGraphStateHandle, StorageError> {
        let attached = self.attach(context, StateObserver::new).await?;
        Ok(TaskGraphStateHandle {
            state: attached.observer,
            detach: attached.detach,
        })
    }

    /// A serialized exact-frame watch of the graph.
    ///
    /// The acquisition context's cancellation stops the watch when a Tokio
    /// runtime is available. Outside one there is no task to await the signal,
    /// so callers must call [`TaskGraphWatch::stop`] explicitly.
    ///
    /// # Errors
    ///
    /// The session closed or failed, or the context was aborted.
    pub async fn watch(&self, context: &Context) -> Result<TaskGraphWatch, StorageError> {
        let attached = self.attach(context, WatchObserver::new).await?;
        let handle = TaskGraphWatch {
            watch: attached.observer,
            detach: attached.detach,
            stopped: Arc::new(AtomicBool::new(false)),
            cancel: Mutex::new(None),
        };
        if context.is_aborted() {
            handle.stop();
            return Err(StorageError::Request(StorageRequestError::new(
                "The watch context was aborted",
            )));
        }
        if let Some(signal) = context.signal().cloned() {
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                let watch = Arc::downgrade(&handle.watch);
                let task = runtime.spawn(async move {
                    signal.cancelled().await;
                    if let Some(watch) = watch.upgrade() {
                        watch.end(WatchEnd::Cancelled);
                    }
                });
                *lock(&handle.cancel) = Some(task);
            }
        }
        Ok(handle)
    }

    /// Register one observer on the line, building the mount first when this is
    /// its first observer.
    async fn attach<O, F>(
        &self,
        context: &Context,
        create: F,
    ) -> Result<AttachedGraph<O>, StorageError>
    where
        O: GraphObserver + 'static,
        F: FnOnce(Arc<TaskGraph>) -> O + Send + 'static,
    {
        if self.is_closed() {
            return Err(closed_error(&self.session));
        }
        let shared = self.shared.clone();
        let blocking = self.blocking.clone();
        let handle = self.session.handle().clone();
        let context = context.clone();
        self.session
            .handle()
            .read_on_line(move |state| {
                Box::pin(async move {
                    let existing = lock(&shared).mount.clone();
                    let mount = match existing {
                        Some(mount) => mount,
                        None => Arc::new(Mutex::new(
                            build_mount(state, blocking.as_ref(), &context).await?,
                        )),
                    };
                    let value = lock(&mount).value.clone();
                    let observer = Arc::new(create(value));

                    // Close or cancellation may begin while the mount builds;
                    // register nothing then.
                    if lock(&shared).closed {
                        return Err(closed_error(&handle));
                    }
                    if context.is_aborted() {
                        return Err(StorageError::Request(StorageRequestError::new(
                            "The attach context was aborted",
                        )));
                    }

                    let registration = {
                        // Registry first, then the mount: `GraphDetach`,
                        // `close_mount` and `advance_mount` use the same order.
                        let mut inner = lock(&shared);
                        let registration = inner.next_registration;
                        inner.next_registration = registration.wrapping_add(1);
                        lock(&mount)
                            .observers
                            .push((registration, observer.clone() as Arc<dyn GraphObserver>));
                        inner.mount = Some(mount.clone());
                        registration
                    };
                    let detach = GraphDetach {
                        shared,
                        registration,
                    };
                    // A watch that ends itself (its listener returns `false`)
                    // must release the mount; a state is released by whoever
                    // holds it, which is why the observer decides.
                    {
                        let release = detach.clone();
                        observer.bind_release(Box::new(move || release.detach()));
                    }
                    let value: JobValue = Box::new(AttachedGraph { observer, detach });
                    Ok(value)
                })
            })
            .await
    }
}

/// One registered observer and its registration.
struct AttachedGraph<O: GraphObserver> {
    observer: Arc<O>,
    detach: GraphDetach,
}

/// Upstream `#build`: every live task of the session, with the conversations it
/// owns and its blocked reason.
async fn build_mount(
    state: &SessionState<'_>,
    blocking: &dyn TaskGraphBlocking,
    context: &Context,
) -> Result<Mount, StorageError> {
    let storage = state.storage.storage();
    let mut records: Vec<TaskRecord> = Vec::new();
    for status in LIVE_STATUSES {
        records.extend(
            scan_all(|cursor| {
                state.storage.read(
                    context,
                    storage.scan_tasks(
                        TaskQuery {
                            status: Some(status),
                            ..TaskQuery::default()
                        },
                        SCAN_PAGE_SIZE,
                        cursor,
                    ),
                )
            })
            .await?,
        );
    }
    records.sort_by_key(|record| record.id.as_i64());

    let mut tasks = BTreeMap::new();
    let mut subjects = BTreeMap::new();
    for record in &records {
        let owned = scan_all(|cursor| {
            state.storage.read(
                context,
                storage.scan_conversations(
                    ConversationQuery {
                        owner_task_id: Some(record.id),
                        ..ConversationQuery::default()
                    },
                    SCAN_PAGE_SIZE,
                    cursor,
                ),
            )
        })
        .await?;
        let mut conversations: Vec<ConversationId> =
            owned.iter().map(|conversation| conversation.id).collect();
        conversations.sort_by_key(|id| id.as_i64());

        let subject = BlockedSubject::of(record);
        let key = record.id.as_i64().to_string();
        tasks.insert(
            key.clone(),
            node_of(record, conversations, blocking.blocked(&subject)),
        );
        subjects.insert(key, subject);
    }
    Ok(Mount {
        value: Arc::new(TaskGraph { tasks }),
        observers: Vec::new(),
        subjects,
    })
}

/// Derive one node from a record, its owned conversations and its blocked
/// reason.
fn node_of(
    record: &TaskRecord,
    conversations: Vec<ConversationId>,
    blocked: Option<BlockedReason>,
) -> TaskGraphNode {
    TaskGraphNode {
        id: record.id,
        kind: record.kind.clone(),
        conversation_id: record.conversation_id,
        owner: record.owner,
        background: record.background,
        abort_requested: record.abort_requested,
        state: state_of(record),
        blocked,
        conversations,
    }
}

fn state_of(record: &TaskRecord) -> TaskGraphState {
    match &record.state {
        TaskState::Pending { checkpoint } => TaskGraphState::Pending {
            phase: phase_of(checkpoint),
        },
        TaskState::Running { checkpoint } => TaskGraphState::Running {
            phase: phase_of(checkpoint),
        },
        TaskState::Waiting {
            checkpoint,
            on,
            policy,
        } => TaskGraphState::Waiting {
            phase: phase_of(checkpoint),
            on: on.clone(),
            policy: *policy,
        },
        TaskState::Sleeping { checkpoint, until } => TaskGraphState::Sleeping {
            phase: phase_of(checkpoint),
            until: *until,
        },
        // Terminal records never reach the graph: they leave it.
        TaskState::Completing { outcome } | TaskState::Terminal { outcome } => {
            TaskGraphState::Completing {
                outcome: OutcomeStatus::of(outcome),
            }
        }
    }
}

/// The checkpoint's `phase`.
///
/// Upstream reads `(checkpoint as { phase: string }).phase`, which is
/// `undefined` for a malformed checkpoint; an owning Rust field cannot be
/// absent, so a malformed checkpoint normalizes to the empty string.
fn phase_of(checkpoint: &Value) -> String {
    checkpoint
        .get("phase")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// Advance the mount with one publication (upstream `advance()`).
fn advance_mount(
    shared: &Arc<Mutex<Shared>>,
    publication: &CommitPublication,
    context: &Context,
    blocking: &dyn TaskGraphBlocking,
    report: &Report,
) {
    let mount = match lock(shared).mount.clone() {
        Some(mount) => mount,
        None => return,
    };
    let (ops, after, observers) = {
        let mut mount = lock(&mount);
        let mut ops = apply_tasks(&mut mount, publication, blocking);
        // After the tasks, so a conversation created with its owner task in one
        // commit finds the owner's node. Change order within a publication is
        // unspecified, so each owner's list is sorted again.
        ops.extend(apply_created_conversations(&mut mount, publication));
        let after = mount.value.clone();
        let observers: Vec<Arc<dyn GraphObserver>> = mount
            .observers
            .iter()
            .map(|(_, observer)| observer.clone())
            .collect();
        (ops, after, observers)
    };
    publish(&after, &after, &ops, &observers, context, report);
}

/// Apply every task change to the mount, returning the exact operations
/// emitted.
///
/// Two passes: derive each delta first (reading the current mount through
/// shared references), then `Arc::make_mut` only when something actually
/// changed. A publication that leaves the graph identical — the common case for
/// a `running` -> `running` write with the same phase — therefore copies
/// nothing and stays O(changes).
fn apply_tasks(
    mount: &mut Mount,
    publication: &CommitPublication,
    blocking: &dyn TaskGraphBlocking,
) -> Vec<Op> {
    let mut ops: Vec<Op> = Vec::new();
    if !publication
        .changes
        .iter()
        .any(|change| matches!(change, CommitChange::Table(TableCommitChange::Task(_))))
    {
        return ops;
    }
    let mut changes: Vec<(String, Option<TaskGraphNode>)> = Vec::new();
    let mut subjects: Vec<(String, BlockedSubject)> = Vec::new();
    let mut gone: Vec<String> = Vec::new();
    for change in &publication.changes {
        let CommitChange::Table(TableCommitChange::Task(record)) = change else {
            continue;
        };
        let key = record.id.as_i64().to_string();
        if record.state.is_terminal() {
            // The subject goes whether or not the node was ever in the graph:
            // a terminal task can no longer run, so no registry change can
            // unblock it.
            gone.push(key.clone());
            if mount.value.tasks.contains_key(&key) {
                changes.push((key, None));
            }
            continue;
        }
        let subject = BlockedSubject::of(record);
        let conversations = mount
            .value
            .tasks
            .get(&key)
            .map(|node| node.conversations.clone())
            .unwrap_or_default();
        let next_node = node_of(record, conversations, blocking.blocked(&subject));
        subjects.push((key.clone(), subject));
        if mount.value.tasks.get(&key) == Some(&next_node) {
            continue;
        }
        changes.push((key, Some(next_node)));
    }
    for (key, subject) in subjects {
        mount.subjects.insert(key, subject);
    }
    for key in gone {
        mount.subjects.remove(&key);
    }
    if changes.is_empty() {
        return ops;
    }
    let next = Arc::make_mut(&mut mount.value);
    for (key, node) in changes {
        match node {
            None => {
                next.tasks.remove(&key);
                ops.push(Op::Delete(task_path(&key)));
            }
            Some(node) => {
                ops.push(Op::Set(
                    task_path(&key),
                    serde_json::to_value(&node).unwrap_or(Value::Null),
                ));
                next.tasks.insert(key, node);
            }
        }
    }
    ops
}

/// Append the conversations this publication created for a live owner.
fn apply_created_conversations(mount: &mut Mount, publication: &CommitPublication) -> Vec<Op> {
    let mut created: BTreeMap<String, Vec<ConversationId>> = BTreeMap::new();
    for change in &publication.changes {
        let CommitChange::Table(TableCommitChange::Conversation(record)) = change else {
            continue;
        };
        let Some(owner) = record.owner.as_ref() else {
            continue;
        };
        let key = owner.task_id.as_i64().to_string();
        if mount.value.tasks.contains_key(&key) {
            created.entry(key).or_default().push(record.id);
        }
    }
    if created.is_empty() {
        return Vec::new();
    }
    let mut ops = Vec::new();
    let next = Arc::make_mut(&mut mount.value);
    for (key, ids) in created {
        let Some(node) = next.tasks.get_mut(&key) else {
            continue;
        };
        node.conversations.extend(ids);
        node.conversations.sort_by_key(|id| id.as_i64());
        ops.push(Op::Set(
            conversations_path(&key),
            serde_json::to_value(&node.conversations).unwrap_or(Value::Null),
        ));
    }
    ops
}

/// Hand `ops` to every observer; one that panics is reported and the others
/// still get the revision.
fn publish(
    _before: &Arc<TaskGraph>,
    after: &Arc<TaskGraph>,
    ops: &[Op],
    observers: &[Arc<dyn GraphObserver>],
    context: &Context,
    report: &Report,
) {
    if ops.is_empty() {
        return;
    }
    let frame_context = context.without_abort_signal();
    for observer in observers {
        contained(report, || {
            observer.advance(after.clone(), ops, &frame_context);
        });
    }
}

/// Re-derive every live node's blocked reason and publish the ones that changed
/// (upstream `#refreshBlocked`).
///
/// Upstream queues this through `readOnLine`, which serializes it with commits.
/// The port serializes it with publications through the mount mutex instead:
/// the commit listener holds that mutex for the whole of one publication, so a
/// refresh can never observe or publish a half-applied one. A refresh that runs
/// concurrently with a publication just lands before or after it, exactly as a
/// queued line job would.
fn refresh_blocked(
    shared: &Arc<Mutex<Shared>>,
    blocking: &Arc<dyn TaskGraphBlocking>,
    report: &Report,
) {
    if lock(shared).closed {
        return;
    }
    refresh_blocked_job(shared, blocking.as_ref(), report);
}

/// Derive, compare and publish.
fn refresh_blocked_job(
    shared: &Arc<Mutex<Shared>>,
    blocking: &dyn TaskGraphBlocking,
    report: &Report,
) {
    let mount = match lock(shared).mount.clone() {
        Some(mount) => mount,
        None => return,
    };
    let (ops, after, observers) = {
        let mut mount = lock(&mount);
        // Derive first, clone only when something changed. A registry change
        // that leaves every reason as it was copies no graph at all.
        let mut changes: Vec<(String, Option<BlockedReason>)> = Vec::new();
        for (key, subject) in &mount.subjects {
            let blocked = blocking.blocked(subject);
            if mount.value.tasks.get(key).map(|node| node.blocked) == Some(blocked) {
                continue;
            }
            changes.push((key.clone(), blocked));
        }
        let mut ops: Vec<Op> = Vec::with_capacity(changes.len());
        if !changes.is_empty() {
            let next = Arc::make_mut(&mut mount.value);
            for (key, blocked) in changes {
                if let Some(node) = next.tasks.get_mut(&key) {
                    node.blocked = blocked;
                }
                ops.push(match blocked {
                    Some(reason) => Op::Set(
                        blocked_path(&key),
                        Value::String(reason.as_str().to_owned()),
                    ),
                    None => Op::Delete(blocked_path(&key)),
                });
            }
        }
        let observers: Vec<Arc<dyn GraphObserver>> = mount
            .observers
            .iter()
            .map(|(_, observer)| observer.clone())
            .collect();
        (ops, mount.value.clone(), observers)
    };
    // A registry change carries no caller context; work that must never
    // inherit a caller's cancellation uses the background context.
    publish(
        &after,
        &after,
        &ops,
        &observers,
        &Context::background(),
        report,
    );
}

/// Close the mount: notify each observer, then forget it (upstream
/// `subscribeClose`).
fn close_mount(shared: &Arc<Mutex<Shared>>, failure: Option<&StorageError>, report: &Report) {
    let mount = {
        let mut inner = lock(shared);
        inner.closed = true;
        inner.mount.take()
    };
    let Some(mount) = mount else {
        return;
    };
    let observers: Vec<Arc<dyn GraphObserver>> = {
        let mut mount = lock(&mount);
        std::mem::take(&mut mount.observers)
            .into_iter()
            .map(|(_, observer)| observer)
            .collect()
    };
    for observer in observers {
        contained(report, || observer.close_session(failure));
    }
}

fn task_path(key: &str) -> Vec<Seg> {
    vec![Seg::Key("tasks".to_owned()), Seg::Key(key.to_owned())]
}

fn task_field_path(key: &str, field: &str) -> Vec<Seg> {
    vec![
        Seg::Key("tasks".to_owned()),
        Seg::Key(key.to_owned()),
        Seg::Key(field.to_owned()),
    ]
}

fn conversations_path(key: &str) -> Vec<Seg> {
    task_field_path(key, "conversations")
}

fn blocked_path(key: &str) -> Vec<Seg> {
    task_field_path(key, "blocked")
}
