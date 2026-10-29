//! The durable task scheduler.
//!
//! Port of upstream `packages/durable/src/harness/scheduler.ts` (2507 lines)
//! against the Rust session kernel in [`crate::durable::session`]. The
//! normative text is upstream `docs/spec.md` §5 (5.1–5.6); the pir digest is
//! `docs/durable-spec.md` §5.
//!
//! | module | contents |
//! |---|---|
//! | [`crate::durable::harness::task`] | `TaskDefinition`, phases, erasure |
//! | [`mod@self`] | scheduler state, invocation, indexes, catalogue, lifecycle |
//! | [`reserve`] | `#reserve`: path from a durable record to a live invocation |
//! | [`run`] | `#run`/`#step`/`#decide`: phases and their one-commit decisions |
//! | [`reconcile`] | ownership tree, cascades, finalization, waits, wakeups |
//! | [`runtime`] | [`TaskRuntime`]: the gated surface handlers use |
//!
//! # Invariants (spec §5.4, §5.5)
//!
//! 1. **One invocation per task.** `Indexes::invocations` has at most one entry
//!    per task id, created inside the reservation commit and removed by the
//!    step that decides the invocation's end.
//! 2. **Every task transition is decided by one commit on the session line.**
//!    Reservation, abort marks, runtime commits, handover, fault settlement and
//!    finalization each stage their write inside exactly one
//!    [`Session::commit_scoped`] callback. Handlers and joins run off the line.
//! 3. **A runtime commit after the invocation ended, or after the task left
//!    `running`, rejects and lands nothing.** See [`runtime::TaskRuntime`].
//! 4. **Abort is bottom-up.** An abort invocation starts only after the task's
//!    ordinary owned work drained; a background subtree is a boundary.
//! 5. **A terminal outcome with live owned work is held as `completing`**, and
//!    the final commit happens after that work drains, judged on the commit's
//!    candidates.
//! 6. **New owned work needs a live owner.** The session's assembly rejects a
//!    creation under a `completing`, terminal, or abort-marked owner; the
//!    runtime never bypasses it.
//! 7. **Sleeping is durable.** `sleep_until` resolves when stored; the task
//!    resumes at its checkpoint when the clock reaches `until`; an abort mark
//!    ends a sleep early. There is no sleep inside a phase.
//! 8. **Open counts, close does not.** Open rewrites surviving `running`
//!    records to `pending` with `restarts + 1`; [`TaskScheduler::join`] writes
//!    the tasks it stopped back as `pending` without a count.
//! 9. **`settled` is bounded by live ancestry.** A terminal task keeps a
//!    `settled` entry exactly while at least one live task's owner chain passes
//!    through it. [`Indexes::settled`] is the only place a walk can learn the
//!    ownership fields of a terminal node; [`relist`] drops the entry as soon
//!    as the node stops being listed in [`Indexes::below`], which happens when
//!    its last live descendant ends or unlists. A lookup that misses therefore
//!    means no live work depends on the node; every walk treats the miss as
//!    "unreachable/terminal" and never panics.
//!
//! # Where the scheduler's state lives
//!
//! Upstream keeps the scheduler's indexes in a single-threaded object and
//! mutates them from Session-line callbacks. The Rust port keeps the same
//! indexes in [`Indexes`], behind one `Mutex`. *Every* mutation happens inside
//! a line job — a commit callback, [`SessionHandle::read_on_line`], the commit
//! listener, or a `maintain` job — and the line serializes those jobs, so a
//! sequence of short lock sections inside one job is atomic with respect to
//! every other job. No lock is held across an `await`.
//!
//! [`SessionHandle::read_on_line`]: crate::durable::session::SessionHandle::read_on_line
//!
//! # Telemetry
//!
//! The scheduler emits through [`HarnessTelemetry`]: a `pi.durable.phase`
//! span brackets each handler invocation and its decision commit, reporting
//! [`HarnessTelemetry::phase_duration`] at settlement; live-task transitions
//! feed `pi.durable.tasks.active`; a runtime commit that changes the running
//! checkpoint's phase raises `pi.durable.checkpoint`; and every scheduler
//! report goes through [`HarnessTelemetry::record_report`]. The handle comes
//! from [`SessionOptions::telemetry`], so a scheduler opened by a
//! [`Harness`](crate::durable::harness::Harness) shares its host sink. With
//! [`HarnessTelemetry::none`] every emission is one branch on an absent sink:
//! no allocation, no virtual call.
//!
//! Stored trace carriers are persisted on [`TaskRecord`] (`trace` and
//! `links`). A phase span extracts `record.trace` as its remote parent and
//! turns the invocation's first-run creation links (sourced from
//! `record.links`; not repeated after a restart) into span links; when neither
//! is available (or no sink is installed) it starts as a root. A handler that
//! creates a child task threads its own carrier into the child's creation
//! record, so the child's phase spans descend from the same chain.
//!
//! # Deliberate simplifications of this lane
//!
//! * **Bounded chain pruning.** Upstream keeps owner edges and ended task
//!   nodes only while work is listed below them and prunes them with a sweep.
//!   The port keeps every *live* node and prunes `settled` incrementally in
//!   [`relist`]: a terminal node is dropped as soon as it has no live
//!   descendant, so the map is bounded by the live owner chains rather than by
//!   the number of tasks the process ever saw. Traversal results are
//!   identical.
//! * **No submission withdrawal.** `abort_conversation` marks tasks and waits
//!   for idle. Withdrawing queued inputs (§6) is the submissions lane's commit.
//! * **No harness cleanup hook.** Upstream calls `settleOutcome` in the commit
//!   that makes a scheduler-written `faulted`/`orphaned` terminal. This lane
//!   holds no run-control state to settle, so the hook is absent.
//! * **A scheduler commit that throws stops scheduling.** Upstream fails the
//!   Session; the facade exposes no `fail` entry point, so a non-storage throw
//!   is reported and scheduling stops. A storage failure still fails the
//!   Session through the guard, which is the case that matters.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use serde_json::Value;
use tokio::sync::{oneshot, Notify};

use crate::ai::types::AbortSignal;
use crate::durable::context::Context;
use crate::durable::errors::{
    HostCallback, ReportInfo, ReportTask, StorageError, TaskFaultReason, TaskOutdatedReason,
};
use crate::durable::harness::settings::{RestartLimit, Settings, DEFAULT_RESTART_LIMIT};
use crate::durable::harness::task::{ErasedTask, Task};
use crate::durable::harness::telemetry::{HarnessTelemetry, SpanHandle, TraceCarrier};
use crate::durable::ids::{ConversationId, TaskId};
use crate::durable::session::facade::Session;
use crate::durable::session::line::{SessionOptions, SessionState};
use crate::durable::session::publication::{CommitListener, CommitPublication};
use crate::durable::session::transaction::TransactionScope;
use crate::durable::session::Transaction;
use crate::durable::timers::{TimerHandle, Timers};
use crate::durable::types::{
    ConversationRecord, JoinPolicy, Storage, StorageWrite, TaskOutcome, TaskOutcomeError,
    TaskQuery, TaskRecord, TaskState, TaskStatus,
};

/// The future a scheduler line job returns: its own value, before the line
/// boxes it for the reply channel.
pub(super) type SchedulerFuture<'a, T> =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<T, StorageError>> + Send + 'a>>;

pub mod reconcile;
pub mod reserve;
pub mod run;
pub mod runtime;

pub use runtime::{DocumentWatch, TaskRuntime, TaskRuntimeError};

/// Page size of every task scan (upstream `SCAN_PAGE_SIZE`).
pub const SCAN_PAGE_SIZE: usize = 256;

/// The statuses of a task that can still run code (upstream `LIVE_STATUSES`).
pub const LIVE_STATUSES: [TaskStatus; 5] = [
    TaskStatus::Pending,
    TaskStatus::Running,
    TaskStatus::Waiting,
    TaskStatus::Sleeping,
    TaskStatus::Completing,
];

/// The `abortReason` of an abandonment after a restart (spec §5.4).
pub const REASON_RESTART: &str = "restart";

// ─── Catalogue ──────────────────────────────────────────────────────────────

/// An immutable snapshot of the executable definitions a scheduler can run.
///
/// Upstream resolves definitions from a registry snapshot. That registry
/// (`crate::durable::harness::registry`) holds *metadata* tasks; the scheduler
/// needs the executable erasure ([`ErasedTask`]), so it keeps its own
/// catalogue. A host bridges the two by installing its registry's tasks here.
#[derive(Clone, Default)]
pub struct TaskCatalogue {
    tasks: Arc<BTreeMap<String, Arc<dyn ErasedTask>>>,
    /// The extension that installed each definition, by task name. Absent for
    /// a built-in task.
    installers: Arc<BTreeMap<String, String>>,
}

impl std::fmt::Debug for TaskCatalogue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaskCatalogue")
            .field("tasks", &self.tasks.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl TaskCatalogue {
    /// Build a catalogue from executable definitions. A later definition with
    /// the same name replaces an earlier one.
    #[must_use]
    pub fn new(tasks: Vec<Task>) -> Self {
        Self::from_erased(tasks.into_iter().map(Task::erased).collect())
    }

    /// Build a catalogue from already-erased definitions.
    #[must_use]
    pub fn from_erased(tasks: Vec<Arc<dyn ErasedTask>>) -> Self {
        Self::from_installations(tasks.into_iter().map(|task| (task, None)).collect())
    }

    /// Build a catalogue from erased definitions, each with the extension that
    /// installed it (`None` for a built-in task).
    ///
    /// The installer is what a task's reports name as the code that ran
    /// (upstream `installerOf`).
    #[must_use]
    pub fn from_installations(installations: Vec<(Arc<dyn ErasedTask>, Option<String>)>) -> Self {
        let mut tasks = BTreeMap::new();
        let mut installers = BTreeMap::new();
        for (task, extension) in installations {
            let name = task.name().to_owned();
            if let Some(extension) = extension {
                installers.insert(name.clone(), extension);
            } else {
                installers.remove(&name);
            }
            tasks.insert(name, task);
        }
        Self {
            tasks: Arc::new(tasks),
            installers: Arc::new(installers),
        }
    }

    /// The definition registered under `name`.
    #[must_use]
    pub fn task(&self, name: &str) -> Option<Arc<dyn ErasedTask>> {
        self.tasks.get(name).cloned()
    }

    /// The extension that installed the definition under `name`, when known.
    #[must_use]
    pub fn installer(&self, name: &str) -> Option<&str> {
        self.installers.get(name).map(String::as_str)
    }

    /// Every defined task, in name order.
    #[must_use]
    pub fn tasks(&self) -> Vec<Arc<dyn ErasedTask>> {
        self.tasks.values().cloned().collect()
    }

    /// A process-local identity for the definition under `name`, or `0`.
    ///
    /// Upstream compares definition *objects* to decide whether a failed
    /// migration or an outdated definition should be reconsidered. The pointer
    /// of the shared erasure is the Rust equivalent: a reinstall builds a new
    /// `Arc`, so the pointer changes.
    #[must_use]
    pub fn identity(&self, name: &str) -> usize {
        self.tasks
            .get(name)
            .map_or(0, |task| Arc::as_ptr(task) as *const () as usize)
    }
}

/// A live, replaceable catalogue with change listeners.
///
/// Installing tasks replaces the snapshot synchronously and notifies listeners,
/// which the scheduler uses to reconsider blocked tasks (spec §5.4: a catalogue
/// change is what lets a blocked task run).
/// A catalogue change listener.
pub type CatalogueListener = Arc<dyn Fn() + Send + Sync>;

#[derive(Clone)]
pub struct CatalogueHandle {
    inner: Arc<RwLock<TaskCatalogue>>,
    listeners: Arc<Mutex<Vec<CatalogueListener>>>,
}

impl std::fmt::Debug for CatalogueHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CatalogueHandle").finish_non_exhaustive()
    }
}

impl CatalogueHandle {
    /// A handle over `tasks`.
    #[must_use]
    pub fn new(tasks: Vec<Task>) -> Self {
        Self::from_catalogue(TaskCatalogue::new(tasks))
    }

    /// A handle over an existing snapshot.
    #[must_use]
    pub fn from_catalogue(catalogue: TaskCatalogue) -> Self {
        Self {
            inner: Arc::new(RwLock::new(catalogue)),
            listeners: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// The current snapshot.
    #[must_use]
    pub fn snapshot(&self) -> TaskCatalogue {
        self.inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Add or replace definitions by name.
    ///
    /// The definitions carry no installer, so reports cannot name extension
    /// code that runs them. A host installing through an extension uses
    /// [`CatalogueHandle::install_from`] instead.
    pub fn install(&self, tasks: Vec<Task>) {
        self.install_with(tasks, None);
    }

    /// Add or replace definitions by name, recording `extension` as the
    /// installer of each (upstream installs a task definition through its
    /// extension).
    pub fn install_from(&self, extension: impl AsRef<str>, tasks: Vec<Task>) {
        self.install_with(tasks, Some(extension.as_ref()));
    }

    fn install_with(&self, tasks: Vec<Task>, extension: Option<&str>) {
        let current = self.snapshot();
        let mut merged: Vec<(Arc<dyn ErasedTask>, Option<String>)> = current
            .tasks()
            .into_iter()
            .map(|task| {
                let installer = current.installer(task.name()).map(str::to_owned);
                (task, installer)
            })
            .collect();
        for task in tasks {
            let name = task.definition().name.clone();
            merged.retain(|(held, _)| held.name() != name.as_str());
            merged.push((task.erased(), extension.map(str::to_owned)));
        }
        self.publish(TaskCatalogue::from_installations(merged));
    }

    /// Replace the catalogue wholesale.
    pub fn replace(&self, catalogue: TaskCatalogue) {
        self.publish(catalogue);
    }

    fn publish(&self, catalogue: TaskCatalogue) {
        *self
            .inner
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = catalogue;
        let listeners = self
            .listeners
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        for listener in listeners {
            listener();
        }
    }

    /// Call `listener` after every publication.
    pub fn subscribe(&self, listener: CatalogueListener) {
        self.listeners
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(listener);
    }
}

// ─── Ownership tree ─────────────────────────────────────────────────────────

/// A node of the ownership tree: a task, or a conversation (spec §5.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Node {
    /// A task node.
    Task(TaskId),
    /// A conversation node.
    Conversation(ConversationId),
}

/// The immutable ownership fields of a task.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TaskNode {
    /// Conversation that owns the task.
    pub conversation_id: ConversationId,
    /// Owning task; absent for a conversation-owned task.
    pub owner: Option<TaskId>,
    /// Whether the task is excluded from idle waits and cascades.
    pub background: bool,
}

/// Where a walk up the ownership tree continues.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Up {
    /// An owner task.
    Task(TaskId),
    /// A conversation.
    Conversation(ConversationId),
}

/// The node a task hangs from: its owner task, or its conversation.
#[must_use]
pub fn parent_node(node: TaskNode) -> Node {
    node.owner
        .map_or(Node::Conversation(node.conversation_id), Node::Task)
}

/// The ownership fields of a stored record.
#[must_use]
pub fn node_of(record: &TaskRecord) -> TaskNode {
    TaskNode {
        conversation_id: record.conversation_id,
        owner: record.owner,
        background: record.background,
    }
}

/// Where a walk up from `node` continues.
#[must_use]
pub fn parent_of(node: TaskNode) -> Up {
    match node.owner {
        Some(owner) => Up::Task(owner),
        None => Up::Conversation(node.conversation_id),
    }
}

// ─── Invocations ────────────────────────────────────────────────────────────

/// What an invocation runs (spec §5.4: volatile, derived from the durable mark).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InvocationMode {
    /// Runs phases.
    Run,
    /// Runs the abort handler once.
    Abort,
}

impl InvocationMode {
    /// The upstream mode string.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Run => "run",
            Self::Abort => "abort",
        }
    }
}

/// One in-memory execution of a task in run or abort mode.
pub struct Invocation {
    /// The task.
    pub id: TaskId,
    /// The task's registered name, as of its reservation.
    pub kind: String,
    /// The extension that installed the definition this invocation runs, as of
    /// its reservation (upstream `Invocation.extension`); `None` for a built-in
    /// task.
    pub extension: Option<String>,
    /// The conversation that owns the task.
    pub conversation_id: ConversationId,
    /// Run or abort.
    pub mode: InvocationMode,
    /// First invocation of a task loaded by this process's open recovery.
    pub resumed: bool,
    /// Signalled when the invocation ends; handler contexts inherit it.
    pub signal: Arc<AbortSignal>,
    /// The task's stored trace carrier at reservation: its first submission's
    /// carrier, or the owner carrier inherited at dispatch. A child task
    /// created by this invocation carries it forward.
    trace: Option<TraceCarrier>,
    /// The task's creation-time link carriers, only while this is its first
    /// run (`started_at` is unset at reservation). Restarts do not repeat
    /// creation spans, matching upstream `Invocation.links`.
    links: Vec<TraceCarrier>,
    /// The active `pi.durable.phase` span handle, when telemetry is installed.
    phase_span: AtomicU64,
    /// The `tool.name` / `tool.call_id` end attributes of the active phase,
    /// published by the tool host at settlement and taken when the span ends.
    phase_tool: Mutex<Option<(String, String)>>,
    /// Watches acquired through the runtime; stopped at invocation end.
    watches: Mutex<Vec<DocumentWatch>>,
    /// Timers armed through the runtime; disarmed at invocation end.
    timers: Mutex<Vec<Arc<TimerHandle>>>,
    ended: AtomicBool,
    done: Arc<Notify>,
    /// The task running this invocation's loop, once started.
    join: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl std::fmt::Debug for Invocation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Invocation")
            .field("id", &self.id)
            .field("kind", &self.kind)
            .field("extension", &self.extension)
            .field("mode", &self.mode)
            .field("ended", &self.ended())
            .finish()
    }
}

impl Invocation {
    fn new(
        record: &TaskRecord,
        mode: InvocationMode,
        extension: Option<String>,
        resumed: bool,
    ) -> Self {
        Self {
            id: record.id,
            kind: record.kind.clone(),
            extension,
            conversation_id: record.conversation_id,
            mode,
            resumed,
            signal: Arc::new(AbortSignal::new()),
            trace: record.trace.clone(),
            links: if record.started_at.is_none() {
                record.links.clone()
            } else {
                Vec::new()
            },
            phase_span: AtomicU64::new(0),
            phase_tool: Mutex::new(None),
            watches: Mutex::new(Vec::new()),
            timers: Mutex::new(Vec::new()),
            ended: AtomicBool::new(false),
            done: Arc::new(Notify::new()),
            join: Mutex::new(None),
        }
    }

    /// Whether the invocation already ended.
    #[must_use]
    pub fn ended(&self) -> bool {
        self.ended.load(Ordering::Acquire)
    }

    /// The context handlers run with: the invocation signal, no caller
    /// cancellation, and the active phase span when one was started.
    #[must_use]
    pub fn handler_context(&self) -> Context {
        let context = Context::with_shared_signal(self.signal.clone());
        match self.phase_span() {
            Some(span) => context.with_span_handle(span),
            None => context,
        }
    }

    /// Record the active `pi.durable.phase` span for the running handler.
    pub(super) fn set_phase_span(&self, span: Option<SpanHandle>) {
        self.phase_span
            .store(span.map_or(0, SpanHandle::id), Ordering::Release);
    }

    /// The active `pi.durable.phase` span, if a handler is running with one.
    #[must_use]
    pub(super) fn phase_span(&self) -> Option<SpanHandle> {
        match self.phase_span.load(Ordering::Acquire) {
            0 => None,
            id => Some(SpanHandle::new(id)),
        }
    }

    /// The task's stored trace carrier, as of its reservation.
    #[must_use]
    pub(super) fn trace_carrier(&self) -> Option<&TraceCarrier> {
        self.trace.as_ref()
    }

    /// Creation-time link carriers for this invocation (empty after the first
    /// run of a task).
    #[must_use]
    pub(super) fn links(&self) -> &[TraceCarrier] {
        &self.links
    }

    /// Record the tool identity of the active phase for its end attributes.
    ///
    /// Called by the tool host when a `pi.tool` phase settles; the pair is
    /// consumed by `SchedulerInner::finish_phase` when the span ends. Each
    /// phase has at most one tool identity, so a later value replaces an
    /// earlier one.
    pub(super) fn set_phase_tool(&self, tool_name: &str, call_id: &str) {
        let mut slot = self
            .phase_tool
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *slot = Some((tool_name.to_owned(), call_id.to_owned()));
    }

    /// Take the pending tool identity of the active phase, clearing the slot.
    pub(super) fn take_phase_tool(&self) -> Option<(String, String)> {
        self.phase_tool
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }

    pub(super) fn add_watch(&self, watch: DocumentWatch) {
        let mut watches = self
            .watches
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.ended() {
            drop(watches);
            watch.stop();
            return;
        }
        watches.push(watch);
    }

    fn set_join(&self, handle: tokio::task::JoinHandle<()>) {
        *self
            .join
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(handle);
    }

    pub(super) fn add_timer(&self, timer: Arc<TimerHandle>) {
        {
            let mut timers = self
                .timers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // `finish` swaps `ended` before draining the timer list. Checking
            // it under the same lock closes the add-after-finish race: either
            // this push is seen by `finish`'s drain, or the timer is cancelled
            // below and can never fire after the invocation ended.
            if !self.ended() {
                timers.push(timer);
                return;
            }
        }
        timer.cancel();
    }

    /// Resolve once the invocation ended. Safe to call before or after the end.
    pub async fn wait_done(&self) {
        // `Notify::notified` registers the waiter when it is first polled or
        // explicitly enabled, and the waiter list is checked before the future
        // suspends. Arming with `enable` *before* the flag check closes the
        // window in which an end between the two would strand this caller.
        loop {
            let notified = self.done.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.ended() {
                return;
            }
            notified.await;
        }
    }

    /// End the invocation: its runtime operations reject from now on, its
    /// signal aborts, its watches stop, its timers are disarmed.
    pub(super) fn finish(&self) {
        if self.ended.swap(true, Ordering::AcqRel) {
            return;
        }
        for watch in self
            .watches
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .drain(..)
        {
            watch.stop();
        }
        for timer in self
            .timers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .drain(..)
        {
            timer.cancel();
        }
        self.signal.abort();
        self.done.notify_waiters();
    }
}

/// One reservation: the invocation created on the line, and what it runs.
#[derive(Clone)]
pub struct Reservation {
    /// The in-memory invocation.
    pub invocation: Arc<Invocation>,
    /// The definition resolved at reservation.
    pub task: Arc<dyn ErasedTask>,
    /// The catalogue snapshot taken at reservation.
    pub catalogue: TaskCatalogue,
}

// ─── State a task commits for itself ────────────────────────────────────────

/// What a runtime commit replaces the task's state with (upstream
/// `NextTaskState`). `pending` is never returned: only reconciliation and
/// handover write it.
#[derive(Clone, Debug, PartialEq)]
pub enum NextTaskState {
    /// Keep running from a (possibly new) checkpoint.
    Running {
        /// Complete durable state.
        checkpoint: Value,
    },
    /// Park until every task in `on` is terminal.
    Waiting {
        /// Complete durable state.
        checkpoint: Value,
        /// Tasks to wait for.
        on: Vec<TaskId>,
        /// How the wait treats failures.
        policy: JoinPolicy,
    },
    /// Park until the harness clock reaches `until`.
    Sleeping {
        /// Complete durable state.
        checkpoint: Value,
        /// Wake-up time in milliseconds on the harness clock.
        until: i64,
    },
    /// Decide the outcome.
    Terminal {
        /// The decided outcome.
        outcome: TaskOutcome,
    },
}

impl NextTaskState {
    /// The durable state this becomes.
    #[must_use]
    pub fn to_state(&self) -> TaskState {
        match self {
            Self::Running { checkpoint } => TaskState::Running {
                checkpoint: checkpoint.clone(),
            },
            Self::Waiting {
                checkpoint,
                on,
                policy,
            } => TaskState::Waiting {
                checkpoint: checkpoint.clone(),
                on: on.clone(),
                policy: *policy,
            },
            Self::Sleeping { checkpoint, until } => TaskState::Sleeping {
                checkpoint: checkpoint.clone(),
                until: *until,
            },
            Self::Terminal { outcome } => TaskState::Terminal {
                outcome: outcome.clone(),
            },
        }
    }

    /// The phase name of a checkpoint-bearing state.
    #[must_use]
    pub fn phase(&self) -> Option<&str> {
        let checkpoint = match self {
            Self::Running { checkpoint }
            | Self::Waiting { checkpoint, .. }
            | Self::Sleeping { checkpoint, .. } => checkpoint,
            Self::Terminal { .. } => return None,
        };
        checkpoint.get("phase").and_then(Value::as_str)
    }
}

// ─── Wake and inspection ────────────────────────────────────────────────────

/// When the scheduler next needs a running process (spec §5.6).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Wake {
    /// Some task can run now or is running, or a reconcile pass has work left.
    pub active: bool,
    /// The earliest sleep not yet due.
    pub at: Option<i64>,
}

/// Why a task cannot be reserved (upstream `BlockedReason`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BlockedReason {
    /// No registered definition has the task's name.
    MissingTask,
    /// The stored version is newer than the definition's.
    TaskTooOld,
    /// The stored version is older and the migration failed.
    MigrationFailed,
    /// `restarts` reached `settings.restartLimit`.
    RestartLimit,
}

impl BlockedReason {
    /// The persisted `reason` string of an orphaned outcome.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MissingTask => "missing_task",
            Self::TaskTooOld => "task_too_old",
            Self::MigrationFailed => "migration_failed",
            Self::RestartLimit => "restart_limit",
        }
    }
}

/// Whether scheduling is running, paused, or closing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SchedulingState {
    /// `resume()` was called and close has not begun.
    Running,
    /// Opened and inspecting, but not scheduling.
    Paused,
    /// Close began.
    Closing,
}

/// What the scheduler would do with one live task (upstream `TaskInspection`).
#[derive(Clone, Debug, PartialEq)]
pub enum TaskInspectionState {
    /// An invocation is active.
    Running,
    /// The next pass reserves it; `migrates` when a migration will run.
    Ready {
        /// Whether the definition is newer and has a migration.
        migrates: bool,
    },
    /// Waits for these live tasks.
    Waiting {
        /// Live tasks it waits for.
        on: Vec<TaskId>,
    },
    /// Sleeps until the harness clock reaches `until`.
    Sleeping {
        /// The deadline.
        until: i64,
    },
    /// Outcome decided; waits for owned work.
    Completing,
    /// Cannot be reserved.
    Blocked {
        /// Why.
        reason: BlockedReason,
    },
}

/// One live task and what the scheduler would do with it.
#[derive(Clone, Debug, PartialEq)]
pub struct TaskInspection {
    /// The committed record.
    pub record: TaskRecord,
    /// The derived state.
    pub state: TaskInspectionState,
}

/// Scheduling state and every live task.
#[derive(Clone, Debug)]
pub struct Inspection {
    /// Whether scheduling runs.
    pub scheduling: SchedulingState,
    /// Every live task, in id order.
    pub tasks: Vec<TaskInspection>,
    /// Whether a reconcile pass still has work.
    pub reconciling: bool,
}

/// The outcome of [`TaskScheduler::abort`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AbortResult {
    /// The mark is durable (or the task already settled).
    Marked,
    /// The task was already terminal.
    Terminal,
}

// ─── Scheduler options ──────────────────────────────────────────────────────

/// A report sink: `(message, info)`.
pub type ReportFn = Arc<dyn Fn(&str, ReportInfo) + Send + Sync>;

/// Options for opening a scheduler.
#[derive(Clone, Default)]
pub struct TaskSchedulerOptions {
    /// The harness clock in epoch milliseconds. Every process that opens the
    /// same storage must use the same clock (sleeps store deadlines on it).
    pub now: Option<Arc<dyn Fn() -> i64 + Send + Sync>>,
    /// Resolved settings, read at each use.
    pub settings: Option<Arc<dyn Fn() -> Settings + Send + Sync>>,
    /// Host timers. Defaults to the native timer.
    pub timers: Option<Timers>,
    /// Replaces the built-in sleep timer: called with the new [`Wake`].
    pub on_wake: Option<Arc<dyn Fn(Wake) + Send + Sync>>,
    /// Receives failures that do not fail the calling operation.
    pub report: Option<ReportFn>,
    /// Options for the session the scheduler opens over the storage. Its
    /// `telemetry` handle is also the scheduler's emission handle.
    pub session: Option<SessionOptions>,
}

impl std::fmt::Debug for TaskSchedulerOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaskSchedulerOptions")
            .field("on_wake", &self.on_wake.is_some())
            .field("report", &self.report.is_some())
            .finish_non_exhaustive()
    }
}

// ─── Scheduler state ────────────────────────────────────────────────────────

/// Every index the scheduler keeps in memory.
///
/// Upstream's `#live`, `#settled`, `#edges`, `#below`, `#roots`, `#unloaded`,
/// `#intent`, `#runnable`, `#waiters`, `#sleeping`, `#finalizeChecks`,
/// `#failFastChecks`, `#abandoned`, `#parkedTasks` and `#invocations`.
#[derive(Debug, Default)]
pub struct Indexes {
    /// Committed non-terminal tasks.
    pub live: BTreeMap<TaskId, TaskRecord>,
    /// Ownership fields of terminal tasks that are ancestors of live work, so
    /// walks pass through them. [`relist`] drops an entry once the task has no
    /// live descendant left; the map is therefore bounded by live owner chains,
    /// not by the number of ended tasks. See invariant 9 in the module docs.
    pub settled: HashMap<TaskId, TaskNode>,
    /// Owner task of each known conversation; `None` for an ownerless one.
    pub edges: HashMap<ConversationId, Option<TaskId>>,
    /// The tree downward around live work.
    pub below: HashMap<Node, BTreeSet<Node>>,
    /// Ownerless conversations with work below them.
    pub roots: BTreeSet<ConversationId>,
    /// Known tasks whose chain above may not be loaded.
    pub unloaded: BTreeSet<TaskId>,
    /// Live tasks with cancellation intent (abort mark or held non-`completed`
    /// outcome).
    pub intent: BTreeSet<TaskId>,
    /// Live tasks that are not `completing`: reservation candidates.
    pub runnable: BTreeSet<TaskId>,
    /// Live waiting tasks by each task in their `on`.
    pub waiters: HashMap<TaskId, BTreeSet<TaskId>>,
    /// Sleeping live tasks, due or not.
    pub sleeping: BTreeSet<TaskId>,
    /// `completing` tasks the next finalize pass checks.
    pub finalize_checks: BTreeSet<TaskId>,
    /// `failFast` waiters the next reconcile checks.
    pub fail_fast_checks: BTreeSet<TaskId>,
    /// Live `abandonOnRestart` tasks found at open, marked by the first pass.
    pub abandoned: BTreeSet<TaskId>,
    /// Tasks a pass left out as parked.
    pub parked: BTreeSet<TaskId>,
    /// The one live invocation per task.
    pub invocations: HashMap<TaskId, Arc<Invocation>>,
    /// The definition identity that failed to migrate, per task, and its error.
    pub failed_migrations: HashMap<TaskId, (usize, String)>,
    /// Tasks resumed by this process (open's recovery).
    pub resumed: BTreeSet<TaskId>,
    /// A cascade of abort marks is pending.
    pub cascade_pending: bool,
    /// A reconcile pass is scheduled.
    pub reconcile_scheduled: bool,
    /// Whether scheduling is enabled.
    pub enabled: bool,
    /// Whether close began.
    pub closing: bool,
    /// Whether a reservation pass is due.
    pub dirty: bool,
    /// Whether a drain loop is running.
    pub draining: bool,
}

impl Indexes {
    /// A live task, if it is still live.
    #[must_use]
    pub fn live(&self, id: TaskId) -> Option<&TaskRecord> {
        self.live.get(&id)
    }
}

// ─── Await registry ─────────────────────────────────────────────────────────

type TaskWaiter = oneshot::Sender<Result<TaskRecord, StorageError>>;
type IdleWaiter = oneshot::Sender<Result<(), StorageError>>;

// ─── Shared scheduler ───────────────────────────────────────────────────────

/// The facts [`SchedulerInner::track`] and `observe` read of the record a
/// publication replaces.
///
/// Holding a borrow of the live record is impossible across `track`'s `&mut
/// Indexes`, and cloning the whole [`TaskRecord`] would copy its input and
/// checkpoint payloads on every committed change. These fields are all either
/// caller reads, and only the (small, usually empty) wait list is allocated.
#[derive(Clone, Debug, Default)]
pub(super) struct PriorRecord {
    /// Whether the record was terminal.
    terminal: bool,
    /// Whether its state was a `Waiting` state and, if so, what it waits for.
    waiting_on: Vec<TaskId>,
    /// Whether the task carried a durable abort mark.
    abort_requested: bool,
    /// Whether the mark carried a reason.
    abort_reason: bool,
    /// Whether the state was `Completing`.
    completing: bool,
    /// Whether the state was `Waiting` (any policy).
    waiting: bool,
    /// Whether the state carried a failed outcome.
    failed: bool,
}

impl PriorRecord {
    fn of(record: &TaskRecord) -> Self {
        let (waiting, waiting_on) = match &record.state {
            TaskState::Waiting { on, .. } => (true, on.clone()),
            _ => (false, Vec::new()),
        };
        Self {
            terminal: record.state.is_terminal(),
            waiting_on,
            abort_requested: record.abort_requested,
            abort_reason: record.abort_reason.is_some(),
            completing: matches!(record.state, TaskState::Completing { .. }),
            waiting,
            failed: failed_outcome(record),
        }
    }
}

/// The scheduler's shared state; handlers and handles hold an `Arc` of it.
pub struct SchedulerInner {
    pub(super) session: Session,
    pub(super) catalogue: CatalogueHandle,
    pub(super) now: Arc<dyn Fn() -> i64 + Send + Sync>,
    pub(super) settings: Arc<dyn Fn() -> Settings + Send + Sync>,
    pub(super) timers: Timers,
    pub(super) on_wake: Option<Arc<dyn Fn(Wake) + Send + Sync>>,
    pub(super) report: ReportFn,
    /// The host telemetry handle; [`HarnessTelemetry::none`] when absent.
    pub(super) telemetry: HarnessTelemetry,
    pub(super) state: Mutex<Indexes>,
    /// Waiters by task id.
    task_waiters: Mutex<HashMap<TaskId, Vec<TaskWaiter>>>,
    /// Idle waiters by conversation (`None`: every ownerless root).
    idle_waiters: Mutex<Vec<(Option<ConversationId>, IdleWaiter)>>,
    /// Coalesces wake scheduling.
    wake_scheduled: AtomicBool,
    /// Last wake delivered to `on_wake`.
    last_wake: Mutex<Option<Wake>>,
    /// The armed sleep timer.
    timer: Mutex<Option<TimerHandle>>,
    /// A scheduler commit failed for a reason that is not a storage failure.
    failed: AtomicBool,
}

impl std::fmt::Debug for SchedulerInner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SchedulerInner").finish_non_exhaustive()
    }
}

impl SchedulerInner {
    /// Run `f` with the indexes locked. Never `await` inside `f`.
    pub(super) fn with_state<T>(&self, f: impl FnOnce(&mut Indexes) -> T) -> T {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        f(&mut state)
    }

    /// A clone of one live task record.
    pub(super) fn live_record(&self, id: TaskId) -> Option<TaskRecord> {
        self.with_state(|state| state.live.get(&id).cloned())
    }

    /// The current harness clock.
    ///
    /// A host clock that panics is reported as a `host_error` for `now` and
    /// read as `0`, so one bad reading does not unwind into the scheduler.
    pub(super) fn now(&self) -> i64 {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (self.now)())) {
            Ok(now) => now,
            Err(payload) => {
                self.report(
                    panic_message(payload),
                    ReportInfo::HostError {
                        callback: HostCallback::Now,
                        task: None,
                    },
                );
                0
            }
        }
    }

    /// Whether close began.
    pub(super) fn is_closing(&self) -> bool {
        self.with_state(|state| state.closing)
    }

    /// The effective restart limit; `None` never parks.
    ///
    /// Host settings that panic are reported as a `host_error` for `settings`
    /// and read as the default limit, as upstream `#restartLimit` does.
    pub(super) fn restart_limit(&self) -> Option<u32> {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (self.settings)())) {
            Ok(settings) => match settings.restart_limit {
                RestartLimit::Infinite => None,
                RestartLimit::Limit(limit) => Some(limit),
            },
            Err(payload) => {
                self.report(
                    panic_message(payload),
                    ReportInfo::HostError {
                        callback: HostCallback::Settings,
                        task: None,
                    },
                );
                Some(DEFAULT_RESTART_LIMIT)
            }
        }
    }

    /// Whether `record` is parked at the restart limit.
    pub(super) fn parked(&self, record: &TaskRecord) -> bool {
        if record.abort_requested {
            return false;
        }
        self.restart_limit()
            .is_some_and(|limit| record.restarts.unwrap_or(0) >= i64::from(limit))
    }

    /// Whether an abort invocation of a task cut off at the restart limit must
    /// be settled `faulted` instead of run again (spec §5.4).
    pub(super) fn abort_cut_off(&self, record: &TaskRecord) -> bool {
        self.restart_limit()
            .is_some_and(|limit| record.restarts.unwrap_or(0) >= i64::from(limit))
    }

    /// Report a failure to the host.
    ///
    /// The report is also recorded through
    /// [`HarnessTelemetry::record_report`] — the `pi.durable.report` event and
    /// the `pi.durable.reports` counter — as upstream's `safeReport` does.
    /// The sink's own panic is dropped, never fatal (upstream `contained`).
    pub(super) fn report(&self, message: impl Into<String>, info: ReportInfo) {
        self.report_with_span(None, message, info);
    }

    /// [`SchedulerInner::report`] attached to `span`, when one is active.
    pub(super) fn report_with_span(
        &self,
        span: Option<SpanHandle>,
        message: impl Into<String>,
        info: ReportInfo,
    ) {
        let message = message.into();
        if self.telemetry.enabled() {
            let (report_type, reason, extension, task_id) = report_telemetry(&info);
            self.telemetry
                .record_report(span, report_type, reason, extension, task_id);
        }
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            (self.report)(&message, info);
        }));
    }

    /// The extension that installed the definition of `kind`, when known.
    pub(super) fn installer(&self, kind: &str) -> Option<String> {
        self.catalogue.snapshot().installer(kind).map(str::to_owned)
    }

    /// The report payload for a task.
    pub(super) fn about_task(&self, record: &TaskRecord) -> ReportTask {
        ReportTask {
            id: record.id,
            kind: record.kind.clone(),
            conversation_id: record.conversation_id,
        }
    }

    // ─── Line helpers ───────────────────────────────────────────────────────

    /// Run one scheduler commit on the session line.
    ///
    /// The callback may `await` inside the commit (a chain load read), and its
    /// value is boxed for the line's reply channel here, so call sites return
    /// ordinary `Result<T, StorageError>`.
    pub(super) async fn commit<T, F>(&self, run: F) -> Result<T, StorageError>
    where
        T: Send + 'static,
        F: for<'a> FnOnce(&'a mut Transaction, &'a SessionState<'_>) -> SchedulerFuture<'a, T>
            + Send
            + 'static,
    {
        self.session
            .commit_scoped(
                TransactionScope::default(),
                Context::background(),
                move |tx, state| {
                    Box::pin(async move {
                        let value = run(tx, state).await?;
                        Ok(Box::new(value) as crate::durable::session::line::JobValue)
                    })
                },
            )
            .await
    }

    /// Run one read job on the line.
    pub(super) async fn read_on_line<T, F>(&self, run: F) -> Result<T, StorageError>
    where
        T: Send + 'static,
        F: for<'a> FnOnce(&'a SessionState<'_>) -> SchedulerFuture<'a, T> + Send + 'static,
    {
        self.session
            .handle()
            .read_on_line(move |state| {
                Box::pin(async move {
                    let value = run(state).await?;
                    Ok(Box::new(value) as crate::durable::session::line::JobValue)
                })
            })
            .await
    }

    /// Read one committed task.
    pub(super) async fn read_task(&self, id: TaskId) -> Result<Option<TaskRecord>, StorageError> {
        self.session.task(id, &Context::background()).await
    }

    /// Read one committed conversation.
    pub(super) async fn read_conversation(
        &self,
        id: ConversationId,
    ) -> Result<Option<ConversationRecord>, StorageError> {
        self.session.conversation(id, &Context::background()).await
    }

    /// Stage one task record, stamping lifecycle times the way the Session's
    /// `set_task` does (spec §5.3).
    ///
    /// The record is a replacement of one the scheduler read from the
    /// committed table, so `created` is false. `started_at` is stamped at the
    /// first change to `running` and `ended_at` at the change to `terminal`;
    /// once set, each carries over from the replaced (candidate or committed)
    /// record, so `started_at` survives waits, a hold and reopen.
    pub(super) fn stage(&self, tx: &mut Transaction, mut record: TaskRecord) {
        let now = self.now();
        // Only the two lifecycle stamps carry over; cloning the whole record
        // (input and checkpoint payloads included) on every commit is not
        // needed.
        let (previous_started, previous_ended) = match tx.staged_task(record.id) {
            Some(previous) => (previous.started_at, previous.ended_at),
            None => self.with_state(|state| {
                state.live.get(&record.id).map_or((None, None), |previous| {
                    (previous.started_at, previous.ended_at)
                })
            }),
        };
        let running = matches!(record.state, TaskState::Running { .. });
        let terminal = record.state.is_terminal();
        record.started_at = previous_started
            .or(record.started_at)
            .or_else(|| running.then_some(now));
        record.ended_at = previous_ended
            .or(record.ended_at)
            .or_else(|| terminal.then_some(now));
        tx.stage_task(record, false);
    }

    // ─── Tracking ───────────────────────────────────────────────────────────

    /// Bring the indexes in line with one committed record (upstream `#track`).
    pub(super) fn track(
        &self,
        state: &mut Indexes,
        previous: Option<&PriorRecord>,
        record: &TaskRecord,
    ) {
        let id = record.id;
        if let Some(previous) = previous {
            for member in &previous.waiting_on {
                remove_from(&mut state.waiters, *member, id);
            }
        }
        if record.state.is_terminal() {
            if !state.closing && previous.is_some_and(|previous| !previous.terminal) {
                self.telemetry.tasks_active(&record.kind, -1);
            }
            let node = node_of(record);
            state.live.remove(&id);
            state.sleeping.remove(&id);
            state.intent.remove(&id);
            state.runnable.remove(&id);
            state.finalize_checks.remove(&id);
            state.parked.remove(&id);
            state.unloaded.remove(&id);
            state.settled.insert(id, node);
            release(state, node);
            if let Some(waiters) = state.waiters.get(&id).cloned() {
                for waiter in waiters {
                    state.runnable.insert(waiter);
                }
            }
            relist(state, Node::Task(id));
            return;
        }
        state.live.insert(id, record.clone());
        if previous.is_none() {
            if !state.closing {
                self.telemetry.tasks_active(&record.kind, 1);
            }
            if !known(state, parent_node(node_of(record))) {
                state.unloaded.insert(id);
            }
            relist(state, Node::Task(id));
        }
        if cancellation_intent(record) {
            state.intent.insert(id);
        } else {
            state.intent.remove(&id);
        }
        if matches!(record.state, TaskState::Completing { .. }) {
            state.runnable.remove(&id);
            state.finalize_checks.insert(id);
        } else {
            state.runnable.insert(id);
        }
        if let TaskState::Waiting { on, .. } = &record.state {
            for member in on {
                add_to(&mut state.waiters, *member, id);
            }
        }
        if matches!(record.state, TaskState::Sleeping { .. }) {
            state.sleeping.insert(id);
        } else {
            state.sleeping.remove(&id);
        }
    }

    /// Apply a publication synchronously on the line (upstream `#observe`).
    fn observe(self: &Arc<Self>, publication: &CommitPublication) {
        use crate::durable::session::publication::{CommitChange, TableCommitChange};
        // Edges first, so tasks created in a new conversation hang from it.
        for change in &publication.changes {
            if let CommitChange::Table(TableCommitChange::Conversation(record)) = change {
                let mut state = self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if let std::collections::hash_map::Entry::Vacant(slot) =
                    state.edges.entry(record.id)
                {
                    let owner = record.owner.as_ref().map(|owner| owner.task_id);
                    slot.insert(owner);
                    relist(&mut state, Node::Conversation(record.id));
                }
            }
        }
        let mut failed: Vec<TaskId> = Vec::new();
        let mut changed = false;
        let mut reconcile = false;
        let mut signal: Vec<TaskId> = Vec::new();
        let mut updated: Vec<TaskRecord> = Vec::new();
        {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for change in &publication.changes {
                let CommitChange::Table(TableCommitChange::Task(record)) = change else {
                    continue;
                };
                changed = true;
                // Derive every decision from a small snapshot: the previous
                // record is gone from `live` after the insert and cloning it
                // would copy its payloads on every commit.
                let previous = state.live.get(&record.id).map(PriorRecord::of);
                if failed_outcome(record) && previous.as_ref().is_none_or(|prev| !prev.failed) {
                    failed.push(record.id);
                }
                self.track(&mut state, previous.as_ref(), record);
                if record.state.is_terminal() {
                    state.failed_migrations.remove(&record.id);
                    state.fail_fast_checks.remove(&record.id);
                    drop(state);
                    self.resolve_task_waiters(record.id, Ok(record.clone()));
                    state = self
                        .state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    reconcile = true;
                    continue;
                }
                if record.abort_requested
                    && previous.as_ref().is_none_or(|prev| !prev.abort_requested)
                {
                    state.cascade_pending = true;
                    signal.push(record.id);
                }
                if previous.as_ref().is_some_and(|prev| prev.abort_reason)
                    && record.abort_reason.is_none()
                {
                    state.cascade_pending = true;
                }
                if matches!(record.state, TaskState::Completing { .. })
                    && previous.as_ref().is_none_or(|prev| !prev.completing)
                {
                    if cancellation_intent(record) {
                        state.cascade_pending = true;
                    }
                    reconcile = true;
                }
                if let TaskState::Waiting {
                    policy: JoinPolicy::FailFast,
                    ..
                } = record.state
                {
                    if previous.as_ref().is_none_or(|prev| !prev.waiting) {
                        state.fail_fast_checks.insert(record.id);
                        reconcile = true;
                    }
                }
                if previous.is_none() {
                    updated.push(record.clone());
                }
            }
            for id in &failed {
                for waiter in state.waiters.get(id).cloned().unwrap_or_default() {
                    if state.live.get(&waiter).is_some_and(|record| {
                        matches!(
                            record.state,
                            TaskState::Waiting {
                                policy: JoinPolicy::FailFast,
                                ..
                            }
                        )
                    }) {
                        state.fail_fast_checks.insert(waiter);
                        reconcile = true;
                    }
                }
            }
        }
        for id in signal {
            let marked = self.with_state(|state| {
                state
                    .live
                    .get(&id)
                    .is_some_and(|record| record.abort_requested)
            });
            if !marked {
                continue;
            }
            if let Some(invocation) = self.with_state(|state| state.invocations.get(&id).cloned()) {
                if invocation.mode == InvocationMode::Run {
                    invocation.signal.abort();
                }
            }
        }
        for record in &updated {
            let needs = self.with_state(|state| {
                if state.unloaded.contains(&record.id) {
                    true
                } else {
                    !record.background
                        && !record.abort_requested
                        && below_cancelled(state, parent_of(node_of(record)))
                }
            });
            if needs {
                reconcile = true;
            }
        }
        if self.with_state(|state| state.cascade_pending) {
            reconcile = true;
        }
        if reconcile {
            self.schedule_reconcile();
        }
        self.settle_idle();
        if changed {
            self.kick();
        }
    }

    /// Close listener: seal the scheduler (spec §5.4 `close()`).
    fn seal(&self) {
        let was_open = self.with_state(|state| {
            let was_open = !state.closing;
            state.closing = true;
            state.enabled = false;
            was_open
        });
        // Live tasks were counted by `track`; close is the settlement that
        // releases every one still live. Only the first seal decrements, and
        // the list is built only with a sink installed.
        if was_open && self.telemetry.enabled() {
            let tasks: Vec<String> = self.with_state(|state| {
                state
                    .live
                    .values()
                    .map(|record| record.kind.clone())
                    .collect()
            });
            for task in tasks {
                self.telemetry.tasks_active(&task, -1);
            }
        }
        if let Some(timer) = self
            .timer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            timer.cancel();
        }
        let error = StorageError::closed("Session");
        let waiters = std::mem::take(
            &mut *self
                .task_waiters
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        for (_, senders) in waiters {
            for sender in senders {
                let _ = sender.send(Err(error.clone()));
            }
        }
        let idle = std::mem::take(
            &mut *self
                .idle_waiters
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        for (_, sender) in idle {
            let _ = sender.send(Err(error.clone()));
        }
    }

    /// Resolve one task's terminal waiters.
    fn resolve_task_waiters(&self, id: TaskId, result: Result<TaskRecord, StorageError>) {
        let senders = self
            .task_waiters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&id);
        if let Some(senders) = senders {
            for sender in senders {
                let _ = sender.send(result.clone());
            }
        }
    }

    /// Resolve idle waiters whose scope became idle (upstream `#settleIdle`).
    fn settle_idle(&self) {
        let mut idle = self
            .idle_waiters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if idle.is_empty() {
            return;
        }
        let waiters = std::mem::take(&mut *idle);
        let mut remaining = Vec::with_capacity(waiters.len());
        for (scope, sender) in waiters {
            if self.idle(scope) {
                let _ = sender.send(Ok(()));
            } else {
                remaining.push((scope, sender));
            }
        }
        *idle = remaining;
    }

    /// No live non-background task in the scope (upstream `#idle`).
    pub(super) fn idle(&self, conversation: Option<ConversationId>) -> bool {
        self.with_state(|state| {
            // A task whose chain is not loaded yet counts as inside unless its
            // walk up decides otherwise.
            for id in &state.unloaded {
                let Some(record) = state.live.get(id) else {
                    continue;
                };
                if record.background {
                    continue;
                }
                if in_scope(state, parent_of(node_of(record)), conversation) != Some(false) {
                    return false;
                }
            }
            let starts: Vec<ConversationId> = match conversation {
                Some(conversation) => vec![conversation],
                None => state.roots.iter().copied().collect(),
            };
            for start in starts {
                for item in walk_down(state, Node::Conversation(start), None, false, None) {
                    if item.live.is_some() && !item.node.background {
                        return false;
                    }
                }
            }
            true
        })
    }

    // ─── Dispatch ───────────────────────────────────────────────────────────

    /// Mark scheduling dirty and start a drain pass.
    pub(super) fn kick(self: &Arc<Self>) {
        self.schedule_wake();
        let start = self.with_state(|state| {
            state.dirty = true;
            if state.draining || !state.enabled || state.closing {
                return false;
            }
            state.draining = true;
            true
        });
        if start {
            let inner = self.clone();
            tokio::spawn(async move { inner.drain().await });
        }
    }

    /// Reserve and start eligible tasks until nothing is dirty.
    async fn drain(self: Arc<Self>) {
        loop {
            let run = self.with_state(|state| {
                if state.closing || !state.enabled || !state.dirty {
                    state.draining = false;
                    return false;
                }
                state.dirty = false;
                true
            });
            if !run {
                return;
            }
            match self.reserve().await {
                Ok(reservations) => {
                    for reservation in reservations {
                        self.start(reservation);
                    }
                }
                Err(error) => {
                    self.fail_scheduling(&error);
                    self.with_state(|state| state.draining = false);
                    return;
                }
            }
        }
    }

    /// Start one reserved invocation off the line.
    fn start(self: &Arc<Self>, reservation: Reservation) {
        let inner = self.clone();
        let invocation = reservation.invocation.clone();
        let running = invocation.clone();
        let handle = tokio::spawn(async move {
            let result = match running.mode {
                InvocationMode::Run => inner.run(reservation).await,
                InvocationMode::Abort => inner.run_abort(reservation).await,
            };
            if let Err(message) = result {
                inner.report(
                    message,
                    ReportInfo::TaskError {
                        task: ReportTask {
                            id: running.id,
                            kind: running.kind.clone(),
                            conversation_id: running.conversation_id,
                        },
                        extension: running.extension.clone(),
                    },
                );
            }
            inner.end(&running);
            inner.kick();
        });
        invocation.set_join(handle);
        self.schedule_wake();
    }

    /// A scheduler commit failed: report it and stop scheduling.
    ///
    /// Upstream fails the Session here. The facade has no `fail` entry point, so
    /// the scheduler stops instead; a storage failure has already failed the
    /// Session through the guard.
    pub(super) fn fail_scheduling(&self, error: &StorageError) {
        if self.failed.swap(true, Ordering::AcqRel) {
            return;
        }
        self.with_state(|state| {
            state.enabled = false;
            state.draining = false;
        });
        // A storage failure has already failed the Session through the guard.
        // Anything else is a bug in the scheduler's own commit; the report names
        // the Session failure so a host can tell the two apart.
        self.report(error.to_string(), ReportInfo::SessionFailed);
    }

    /// Recompute the wake in the background (upstream `#scheduleWake`).
    pub(super) fn schedule_wake(self: &Arc<Self>) {
        if self.wake_scheduled.swap(true, Ordering::AcqRel) {
            return;
        }
        let inner = self.clone();
        tokio::spawn(async move {
            inner.wake_scheduled.store(false, Ordering::Release);
            if inner.is_closing() {
                return;
            }
            let wake = inner.wake();
            if let Some(on_wake) = &inner.on_wake {
                let last = *inner
                    .last_wake
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if last == Some(wake) {
                    return;
                }
                // Only a delivered wake counts: after a throw, the next change
                // or kick offers it again (upstream catches and reports it).
                match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| on_wake(wake))) {
                    Ok(()) => {}
                    Err(payload) => {
                        inner.report(
                            panic_message(payload),
                            ReportInfo::HostError {
                                callback: HostCallback::OnWake,
                                task: None,
                            },
                        );
                        return;
                    }
                }
                *inner
                    .last_wake
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(wake);
            } else {
                let at = if inner.with_state(|state| state.enabled) {
                    wake.at
                } else {
                    None
                };
                inner.arm_timer(at);
            }
        });
    }

    /// Arm the foreground timer for the earliest sleep (upstream `#armTimer`).
    fn arm_timer(self: &Arc<Self>, at: Option<i64>) {
        let mut slot = self
            .timer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(timer) = slot.take() {
            timer.cancel();
        }
        let Some(at) = at else {
            return;
        };
        if self.is_closing() {
            return;
        }
        let inner = self.clone();
        let delay = at.saturating_sub(self.now()).max(0);
        let timer = self.timers.foreground(
            Arc::new(move || {
                inner
                    .timer
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take();
                // Recheck the clock when it fires: a wake before `until` only
                // arms it again.
                inner.wake_due();
                inner.kick();
            }),
            delay,
        );
        *slot = Some(timer);
    }

    /// Make every sleeping task whose time has come a candidate (upstream
    /// `#wakeDue`).
    pub(super) fn wake_due(self: &Arc<Self>) {
        let now = self.now();
        let due: Vec<TaskId> = self.with_state(|state| {
            state
                .sleeping
                .iter()
                .filter(|id| {
                    state
                        .live
                        .get(id)
                        .is_some_and(|record| !not_due(record, now))
                })
                .copied()
                .collect()
        });
        if due.is_empty() {
            return;
        }
        self.with_state(|state| {
            for id in due {
                state.runnable.insert(id);
            }
        });
    }

    /// Queue a reconcile pass off the line (upstream `#scheduleReconcile`).
    pub(super) fn schedule_reconcile(self: &Arc<Self>) {
        self.request_reconcile();
    }

    /// Queue a reconcile pass from any scheduler handle.
    pub(super) fn request_reconcile(self: &Arc<Self>) {
        let start = self.with_state(|state| {
            if state.reconcile_scheduled || state.closing {
                return false;
            }
            state.reconcile_scheduled = true;
            true
        });
        if !start {
            return;
        }
        let inner = self.clone();
        tokio::spawn(async move {
            inner.with_state(|state| state.reconcile_scheduled = false);
            inner.reconcile().await;
        });
    }

    /// End an invocation (upstream `#end`).
    ///
    /// The invocation's runtime operations reject from now on, its signal
    /// aborts, its watches stop, and its task becomes a reservation candidate
    /// again.
    pub(super) fn end(&self, invocation: &Invocation) {
        if invocation.ended() {
            return;
        }
        // One invocation per task, so the map entry for this id is this
        // invocation's; `finish` is idempotent either way.
        self.with_state(|state| {
            state.invocations.remove(&invocation.id);
            if state.live.contains_key(&invocation.id) {
                state.runnable.insert(invocation.id);
            }
        });
        invocation.finish();
    }

    /// The `Wake` value (spec §5.6).
    pub(super) fn wake(&self) -> Wake {
        reconcile::wake(self)
    }
}

// ─── The scheduler handle ───────────────────────────────────────────────────

/// Durable task scheduler of one storage.
///
/// Cheap to clone (one `Arc`). The host opens it, drives it with
/// [`resume`](TaskScheduler::resume), [`abort`](TaskScheduler::abort) and
/// [`join`](TaskScheduler::join).
#[derive(Clone)]
pub struct TaskScheduler {
    pub(super) inner: Arc<SchedulerInner>,
}

impl std::fmt::Debug for TaskScheduler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaskScheduler")
            .field("scheduling", &self.scheduling_state())
            .finish_non_exhaustive()
    }
}

impl TaskScheduler {
    /// Open a scheduler over `storage`, registering `tasks`.
    ///
    /// Loads every live task, changes surviving `running` tasks back to
    /// `pending` counting the restart, and arms recovery: a reconcile pass
    /// derives marks a crash left unapplied and finalizes held outcomes.
    /// Nothing is dispatched until [`TaskScheduler::resume`].
    pub async fn open(storage: Arc<dyn Storage>, tasks: Vec<Task>) -> Result<Self, StorageError> {
        Self::open_with(TaskSchedulerOptions::default(), storage, tasks).await
    }

    /// [`TaskScheduler::open`] with explicit options.
    pub async fn open_with(
        options: TaskSchedulerOptions,
        storage: Arc<dyn Storage>,
        tasks: Vec<Task>,
    ) -> Result<Self, StorageError> {
        let now: Arc<dyn Fn() -> i64 + Send + Sync> =
            options.now.clone().unwrap_or_else(|| Arc::new(system_now));
        let session_options = options.session.clone().unwrap_or(SessionOptions {
            now: Some(now.clone()),
            document_retention_ms: None,
            persist_within_ms: None,
            behaviours: None,
            telemetry: None,
            conversation_created: None,
        });
        // The scheduler shares the session's handle: `Harness` puts its
        // `HarnessOptions.telemetry` into `SessionOptions`, so phase spans,
        // counters and report events land in the same host sink as the
        // session's entry/submission events. Standalone schedulers can install
        // one through `TaskSchedulerOptions::session`.
        let telemetry = session_options
            .telemetry
            .clone()
            .unwrap_or_else(HarnessTelemetry::none);
        let catalogue = CatalogueHandle::new(tasks);
        let report = options
            .report
            .clone()
            .unwrap_or_else(|| Arc::new(|_, _| {}));
        let report_for_timers: Arc<dyn Fn(String) + Send + Sync> = {
            let report = report.clone();
            let telemetry = telemetry.clone();
            Arc::new(move |message: String| {
                // A native-timer callback that throws is a scheduler report
                // that never passes through `SchedulerInner::report`, so it
                // records its own telemetry shape (a task-less host error).
                if telemetry.enabled() {
                    telemetry.record_report(None, "host_error", None, None, None);
                }
                report(
                    &message,
                    ReportInfo::HostError {
                        callback: crate::durable::errors::HostCallback::SetTimer,
                        task: None,
                    },
                );
            })
        };
        let timers = options
            .timers
            .clone()
            .unwrap_or_else(|| Timers::native(report_for_timers));
        // The scheduler owns the session, so its own report sink and timer set
        // are installed here: document eviction and failure reports otherwise
        // bypass the host (upstream `createSession({setTimer, onReport})`).
        let session = Session::open_with_report_and_timers(
            storage,
            session_options,
            Some(report.clone()),
            Some(timers.clone()),
        );
        let inner = Arc::new(SchedulerInner {
            session,
            catalogue: catalogue.clone(),
            now,
            settings: options
                .settings
                .clone()
                .unwrap_or_else(|| Arc::new(Settings::default)),
            timers,
            on_wake: options.on_wake.clone(),
            report,
            telemetry,
            state: Mutex::new(Indexes::default()),
            task_waiters: Mutex::new(HashMap::new()),
            idle_waiters: Mutex::new(Vec::new()),
            wake_scheduled: AtomicBool::new(false),
            last_wake: Mutex::new(None),
            timer: Mutex::new(None),
            failed: AtomicBool::new(false),
        });

        // The internal commit listener must not panic: a panic would fail the
        // Session. Everything it does is lock-and-copy.
        let observer_listener: CommitListener = {
            let weak = Arc::downgrade(&inner);
            Arc::new(move |publication, _context| {
                if let Some(inner) = weak.upgrade() {
                    inner.observe(publication);
                }
            })
        };
        inner
            .session
            .handle()
            .maintain(move |line| {
                line.internal_listeners.add(observer_listener);
            })
            .await?;

        let scheduler = Self { inner };
        let closer_inner = scheduler.inner.clone();
        let closer: Arc<dyn Fn() + Send + Sync> = Arc::new(move || closer_inner.seal());
        scheduler
            .inner
            .session
            .handle()
            .maintain(move |line| line.close_listeners.push(closer.clone()))
            .await?;

        // Upstream's `Harness.beforeClose` runs `scheduler.join()`: signal
        // every invocation, wait for them, then write the tasks they ran back
        // as `pending` through `writeAtClose` — all after close sealed
        // admission and before the line closes storage. The hook holds the
        // scheduler weakly, so it never keeps the session alive past its owner.
        let joiner = Arc::downgrade(&scheduler.inner);
        let before_close: crate::durable::session::line::BeforeCloseHook = Box::new(move || {
            Box::pin(async move {
                if let Some(inner) = joiner.upgrade() {
                    TaskScheduler { inner }.join().await;
                }
            })
        });
        scheduler
            .inner
            .session
            .handle()
            .register_before_close(before_close)?;

        let catalogue_inner = Arc::downgrade(&scheduler.inner);
        scheduler.inner.catalogue.subscribe(Arc::new(move || {
            if let Some(inner) = catalogue_inner.upgrade() {
                inner.kick();
            }
        }));

        scheduler.inner.open_commit().await?;
        scheduler
            .inner
            .with_state(|state| state.cascade_pending = true);
        scheduler.inner.schedule_reconcile();
        scheduler.inner.schedule_wake();
        Ok(scheduler)
    }

    /// The session the scheduler owns.
    #[must_use]
    pub fn session(&self) -> &Session {
        &self.inner.session
    }

    /// The live task catalogue.
    #[must_use]
    pub fn catalogue(&self) -> CatalogueHandle {
        self.inner.catalogue.clone()
    }

    /// Run one reservation pass now (upstream `kick` + `drain`).
    ///
    /// Sets the dirty flag and drains when scheduling is enabled; the drain
    /// runs on a background task, exactly as a commit listener kicks it.
    pub fn kick(&self) {
        self.inner.kick();
    }

    /// Reserve and start eligible tasks until nothing is dirty.
    ///
    /// Normally the scheduler runs this itself after every change; calling it
    /// directly is useful for a host that wants a deterministic pass, such as a
    /// test.
    pub async fn drain(&self) {
        self.inner.clone().drain().await;
    }

    /// Enable scheduling and make due sleeps runnable (spec §5.6).
    ///
    /// Idempotent. A host wakeup, such as an alarm, calls this.
    pub fn resume(&self) {
        self.inner.with_state(|state| {
            if !state.closing {
                state.enabled = true;
            }
        });
        self.inner.wake_due();
        self.inner.kick();
    }

    /// Pause scheduling without closing. Dispatch already started finishes; no
    /// new reservation runs.
    pub fn pause(&self) {
        self.inner.with_state(|state| state.enabled = false);
    }

    /// Whether scheduling is running, paused or closing.
    #[must_use]
    pub fn scheduling_state(&self) -> SchedulingState {
        self.inner.with_state(|state| {
            if state.closing {
                SchedulingState::Closing
            } else if state.enabled {
                SchedulingState::Running
            } else {
                SchedulingState::Paused
            }
        })
    }

    /// Run a task parked by the restart limit again: clears its restart count
    /// and enables scheduling. Does nothing for a task that is not parked;
    /// rejects for an unknown one.
    pub async fn retry_task(&self, id: TaskId) -> Result<(), StorageError> {
        let inner = self.inner.clone();
        self.inner
            .commit(move |tx, state| {
                Box::pin(async move {
                    let Some(current) = reconcile::read_task_in(state, id).await? else {
                        return Err(StorageError::invalid(format!("Task {id} does not exist")));
                    };
                    if current.state.is_terminal()
                        || matches!(current.state, TaskState::Completing { .. })
                        || !inner.parked(&current)
                    {
                        return Ok(());
                    }
                    tx.stage_task(progressed(&current), false);
                    Ok(())
                })
            })
            .await?;
        self.resume();
        Ok(())
    }

    /// Ask why a live task cannot run here, if it cannot (spec §5.4).
    ///
    /// A running task, an abort-marked one, and one holding its outcome are
    /// never blocked.
    #[must_use]
    pub fn blocked_reason(&self, record: &TaskRecord) -> Option<BlockedReason> {
        if matches!(record.state, TaskState::Running { .. })
            || matches!(record.state, TaskState::Completing { .. })
            || record.state.is_terminal()
        {
            return None;
        }
        // An abort request runs its handler or orphans the task; a `restart`
        // mark waits for the definition, blocked.
        if record.abort_requested && record.abort_reason.is_none() {
            return None;
        }
        if self.inner.parked(record) {
            return Some(BlockedReason::RestartLimit);
        }
        match reconcile::fit(&self.inner, record, &self.inner.catalogue.snapshot()) {
            Err(reason) => Some(reason),
            Ok(fit) => {
                if fit.migrates {
                    let has_migration = self
                        .inner
                        .catalogue
                        .snapshot()
                        .task(&record.kind)
                        .is_some_and(|task| task.migration().is_some());
                    if !has_migration {
                        return Some(BlockedReason::MigrationFailed);
                    }
                }
                None
            }
        }
    }

    /// When the scheduler next needs a running process (spec §5.6).
    ///
    /// Synchronous, from memory.
    #[must_use]
    pub fn wake(&self) -> Wake {
        self.inner.wake()
    }

    /// Signal every invocation and wait for them, then write the tasks they ran
    /// back as `pending` through the session's close-time write path (upstream
    /// `scheduler.join()` → `writeAtClose`), **without** incrementing
    /// `restarts` (spec §5.4).
    ///
    /// `close` runs this as a before-close hook, so it also happens when a host
    /// closes the session directly. A host that calls it early gets the same
    /// effect: the stopped set is taken, so a later close writes nothing more.
    ///
    /// The write is a normal commit, and a host may call this before the
    /// facade's seal; after the seal (the hook path) it uses
    /// [`SessionHandle::write_at_close`](crate::durable::session::line::SessionHandle::write_at_close)
    /// so the batch still reaches storage with nothing published.
    pub async fn join(&self) {
        // Seal first: an in-flight step then applies rule 2 and ends its
        // invocation, and no fresh phase or abort invocation starts. The
        // write-back below is deliberately not a scheduler commit: it runs
        // after the session sealed admission.
        self.inner.seal();
        // A reservation commit already admitted before the seal may still
        // create an invocation; repeat until the map stays empty.
        let mut stopped: Vec<Arc<Invocation>> = Vec::new();
        loop {
            let batch: Vec<Arc<Invocation>> = self
                .inner
                .with_state(|state| state.invocations.values().cloned().collect());
            let fresh: Vec<Arc<Invocation>> = batch
                .into_iter()
                .filter(|invocation| !stopped.iter().any(|seen| Arc::ptr_eq(seen, invocation)))
                .collect();
            if fresh.is_empty() {
                break;
            }
            for invocation in &fresh {
                invocation.signal.abort();
            }
            for invocation in &fresh {
                invocation.wait_done().await;
            }
            stopped.extend(fresh);
        }
        if stopped.is_empty() {
            return;
        }
        let inner = self.inner.clone();
        let ids: Vec<TaskId> = stopped.iter().map(|invocation| invocation.id).collect();
        // The close-time batch is computed from the scheduler's live mirror at
        // write time (upstream passes a callback to `writeAtClose` for exactly
        // this reason), so a commit that settled between the wait and the
        // write is reflected.
        let write = self
            .inner
            .session
            .handle()
            .write_at_close(move |_state| {
                let mut pending = Vec::new();
                for id in &ids {
                    if let Some(record) = inner.live_record(*id) {
                        if matches!(record.state, TaskState::Running { .. }) {
                            pending.push(record);
                        }
                    }
                }
                pending
                    .into_iter()
                    .map(|record| {
                        let mut next = record.clone();
                        next.state = TaskState::Pending {
                            checkpoint: checkpoint_of(&record.state).clone(),
                        };
                        // No `restarts` increment: a close is not a crash.
                        StorageWrite::Task { value: next }
                    })
                    .collect()
            })
            .await;
        let _ = write;
    }

    /// Cancel every invocation, write the tasks they ran back as `pending`, and
    /// close the session the scheduler owns.
    ///
    /// The write-back runs as the session's before-close hook (upstream
    /// `Harness.beforeClose` → `scheduler.join()`), so an explicit
    /// `session.close()` on this scheduler's session gets it too. `join` here
    /// is idempotent: an invocation already stopped is not written twice.
    pub async fn close(&self) -> Result<(), StorageError> {
        self.join().await;
        self.inner.session.close().await
    }

    /// Mark a task for abort (spec §5.4).
    ///
    /// Commits the abort mark (or settles a task no definition can take as
    /// `orphaned` when nothing it owns is live), then joins the run invocation
    /// seen on the line. The abort invocation starts once the task's ordinary
    /// owned work is gone. A `completing` task is only marked.
    pub async fn abort(&self, id: TaskId) -> Result<AbortResult, StorageError> {
        self.inner.abort(id, false).await
    }

    /// [`TaskScheduler::abort`] keeping a `restart` mark (an owner's own
    /// cleanup).
    pub async fn abort_keep_restart(&self, id: TaskId) -> Result<AbortResult, StorageError> {
        self.inner.abort(id, true).await
    }

    /// `Conversation.abort()`: mark every live non-background task ordinary
    /// traversal from the conversation reaches, then wait until the scope is
    /// idle.
    ///
    /// With `background`, traversal crosses background boundaries and the wait
    /// covers every task it reached.
    pub async fn abort_conversation(
        &self,
        conversation: ConversationId,
        background: bool,
    ) -> Result<(), StorageError> {
        let reached = self
            .inner
            .mark_conversation(conversation, background)
            .await?;
        if background {
            // The commit's marks are the operation's admission boundary:
            // background work created afterwards is neither marked nor awaited.
            for id in reached {
                let _ = self.inner.wait_for_task(id).await;
            }
        }
        self.inner.wait_for_idle(Some(conversation)).await
    }

    /// Wait for a terminal receipt (spec §5.4).
    ///
    /// Checks and registers on the line so no terminal publication falls
    /// between them.
    pub async fn wait_for_task(&self, id: TaskId) -> Result<TaskRecord, StorageError> {
        self.inner.wait_for_task(id).await
    }

    /// Resolve when ordinary traversal from the conversation, or from every
    /// ownerless conversation, reaches no live non-background task.
    pub async fn wait_for_idle(
        &self,
        conversation: Option<ConversationId>,
    ) -> Result<(), StorageError> {
        self.inner.wait_for_idle(conversation).await
    }

    /// Scheduling state and every live task with its derived state.
    ///
    /// Runs no task code. A pending migration shows as `Ready` with
    /// `migrates`; only a migration the scheduler already tried shows as
    /// `Blocked`.
    pub async fn inspect(&self) -> Result<Inspection, StorageError> {
        self.inner.load_scopes().await?;
        let catalogue = self.inner.catalogue.snapshot();
        let (live, scheduling, reconciling) = self.inner.with_state(|state| {
            (
                state.live.values().cloned().collect::<Vec<_>>(),
                if state.closing {
                    SchedulingState::Closing
                } else if state.enabled {
                    SchedulingState::Running
                } else {
                    SchedulingState::Paused
                },
                state.cascade_pending
                    || !state.finalize_checks.is_empty()
                    || !state.fail_fast_checks.is_empty(),
            )
        });
        let tasks = live
            .iter()
            .map(|record| TaskInspection {
                record: record.clone(),
                state: reconcile::inspect_task(self.inner.as_ref(), record, &catalogue),
            })
            .collect();
        Ok(Inspection {
            scheduling,
            tasks,
            reconciling,
        })
    }

    /// Create a task of one of the scheduler's definitions.
    ///
    /// A convenience for hosts that hold executable definitions: the session's
    /// own `create_task` takes metadata (`TaskDefinitionLike`), while the
    /// scheduler's catalogue carries the `initial` function.
    pub async fn create_task(
        &self,
        definition: &Arc<dyn ErasedTask>,
        input: Value,
        options: crate::durable::session::transaction::TaskOptions,
    ) -> Result<TaskId, StorageError> {
        let adapter = ErasedDefinitionLike {
            kind: definition.name().to_owned(),
            version: definition.version(),
            initial: definition.as_definition().initial,
        };
        self.inner
            .session
            .create_task(&adapter, input, options, Context::background())
            .await
    }

    /// A committed task record.
    pub async fn get_task(&self, id: TaskId) -> Result<Option<TaskRecord>, StorageError> {
        self.inner.read_task(id).await
    }

    /// A conversation handle bound to this scheduler.
    #[must_use]
    pub fn conversation(&self, id: ConversationId) -> ConversationHandle {
        ConversationHandle {
            inner: self.inner.clone(),
            id,
        }
    }
}

/// A conversation handle bound to a scheduler.
#[derive(Clone, Debug)]
pub struct ConversationHandle {
    inner: Arc<SchedulerInner>,
    id: ConversationId,
}

impl ConversationHandle {
    /// The conversation identifier.
    #[must_use]
    pub fn id(&self) -> ConversationId {
        self.id
    }

    /// `Conversation.abort()` (spec §5.4).
    pub async fn abort(&self, background: bool) -> Result<(), StorageError> {
        let reached = self.inner.mark_conversation(self.id, background).await?;
        if background {
            for id in reached {
                let _ = self.inner.wait_for_task(id).await;
            }
        }
        self.inner.wait_for_idle(Some(self.id)).await
    }

    /// Resolve when the conversation's ordinary scope is idle.
    pub async fn wait_for_idle(&self) -> Result<(), StorageError> {
        self.inner.wait_for_idle(Some(self.id)).await
    }
}

/// An adapter presenting an erased definition to `Session::create_task`.
struct ErasedDefinitionLike {
    kind: String,
    version: i64,
    initial: crate::durable::harness::task::InitialFn,
}

impl crate::durable::session::facade::TaskDefinitionLike for ErasedDefinitionLike {
    fn kind(&self) -> &str {
        &self.kind
    }

    fn version(&self) -> i64 {
        self.version
    }

    fn initial(&self, input: &Value) -> Value {
        (self.initial)(input)
    }
}

// ─── Open and chain loading ─────────────────────────────────────────────────

impl SchedulerInner {
    /// Load live tasks and rewrite surviving `running` tasks (spec §5.4).
    async fn open_commit(self: &Arc<Self>) -> Result<(), StorageError> {
        let inner = self.clone();
        let limit = self.restart_limit();
        let parked: Arc<Mutex<Vec<TaskRecord>>> = Arc::new(Mutex::new(Vec::new()));
        let parked_out = parked.clone();
        self.commit(move |tx, state| {
            let inner = inner.clone();
            let parked_out = parked_out.clone();
            Box::pin(async move {
                let mut records = Vec::new();
                for status in LIVE_STATUSES {
                    records.extend(
                        scan_all(
                            state,
                            TaskQuery {
                                status: Some(status),
                                ..TaskQuery::default()
                            },
                        )
                        .await?,
                    );
                }
                let mut indexes = inner
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                for record in records {
                    inner.track(&mut indexes, None, &record);
                    indexes.resumed.insert(record.id);
                    if record.abandon_on_restart == Some(true) {
                        indexes.abandoned.insert(record.id);
                    }
                    if matches!(record.state, TaskState::Running { .. }) {
                        let restarts = record.restarts.unwrap_or(0) + 1;
                        let mut pending = record.clone();
                        pending.state = TaskState::Pending {
                            checkpoint: checkpoint_of(&record.state).clone(),
                        };
                        pending.restarts = Some(restarts);
                        tx.stage_task(pending, false);
                        if limit.is_some_and(|limit| restarts >= i64::from(limit))
                            && !record.abort_requested
                        {
                            parked_out
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .push(record.clone());
                        }
                    }
                    if matches!(
                        record.state,
                        TaskState::Waiting {
                            policy: JoinPolicy::FailFast,
                            ..
                        }
                    ) {
                        indexes.fail_fast_checks.insert(record.id);
                    }
                }
                Ok(())
            })
        })
        .await?;
        let parked = parked
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        for record in parked {
            let restarts = record.restarts.unwrap_or(0) + 1;
            self.report(
                format!(
                    "Task {} ({}) was cut off {} times without progress; it is parked until retryTask()",
                    record.id, record.kind, restarts
                ),
                ReportInfo::TaskParked {
                    task: self.about_task(&record),
                    extension: self.installer(&record.kind),
                },
            );
        }
        Ok(())
    }

    /// Load every owner chain this process does not know yet.
    ///
    /// Upstream loads chains on demand and prunes them. The port loads each
    /// unknown chain once, up to an ownerless root, on the line.
    pub(super) async fn load_scopes(self: &Arc<Self>) -> Result<(), StorageError> {
        let inner = self.clone();
        self.read_on_line(move |state| {
            let inner = inner.clone();
            Box::pin(async move {
                inner.load_scopes_in(state, false).await?;
                Ok(())
            })
        })
        .await
    }

    /// Load every unknown owner chain inside a commit callback.
    pub(super) async fn load_scopes_in(
        &self,
        state: &SessionState<'_>,
        _queued: bool,
    ) -> Result<(), StorageError> {
        loop {
            let next = self.with_state(|indexes| indexes.unloaded.iter().next().copied());
            let Some(id) = next else {
                return Ok(());
            };
            self.load_chain_in(state, Up::Task(id)).await?;
            // A load that found nothing must not spin: drop the marker.
            self.with_state(|indexes| {
                indexes.unloaded.remove(&id);
            });
        }
    }

    /// Load the owner chain from `start` up to its ownerless root, inside a
    /// commit callback.
    pub(super) async fn load_chain_in(
        &self,
        state: &SessionState<'_>,
        start: Up,
    ) -> Result<(), StorageError> {
        let mut passed: Vec<TaskId> = Vec::new();
        let mut at = Some(start);
        while let Some(up) = at {
            match up {
                Up::Task(id) => {
                    let (node, loaded) = self.with_state(|indexes| {
                        let node = indexes
                            .live
                            .get(&id)
                            .map(node_of)
                            .or_else(|| indexes.settled.get(&id).copied());
                        let loaded = node.is_some() && !indexes.unloaded.contains(&id);
                        (node, loaded)
                    });
                    if loaded {
                        break;
                    }
                    let node = match node {
                        Some(node) => node,
                        None => {
                            let Some(record) = reconcile::read_task_in(state, id).await? else {
                                return Ok(());
                            };
                            let node = node_of(&record);
                            if record.state.is_terminal() {
                                self.with_state(|indexes| {
                                    indexes.settled.insert(id, node);
                                    indexes.unloaded.insert(id);
                                    // Link the freshly known node under its
                                    // parent, so a chain loaded above a live
                                    // task stays connected. `relist` drops the
                                    // entry again if nothing live is below it.
                                    relist(indexes, Node::Task(id));
                                });
                            }
                            node
                        }
                    };
                    passed.push(id);
                    at = Some(parent_of(node));
                }
                Up::Conversation(conversation) => {
                    let edge = self.with_state(|indexes| indexes.edges.get(&conversation).copied());
                    let edge = match edge {
                        Some(edge) => edge,
                        None => {
                            let owner = read_conversation_in(state, conversation)
                                .await?
                                .and_then(|record| record.owner.map(|owner| owner.task_id));
                            self.with_state(|indexes| {
                                indexes.edges.insert(conversation, owner);
                                relist(indexes, Node::Conversation(conversation));
                            });
                            owner
                        }
                    };
                    at = edge.map(Up::Task);
                }
            }
        }
        if !passed.is_empty() {
            self.with_state(|indexes| {
                for id in passed {
                    indexes.unloaded.remove(&id);
                }
            });
        }
        Ok(())
    }
}

// ─── Record helpers ─────────────────────────────────────────────────────────

/// The telemetry description of a report: its fixed type, reason, extension
/// and task id.
///
/// The strings are the wire values declared by
/// [`REPORT_TYPES`](crate::durable::harness::telemetry::REPORT_TYPES) and
/// [`REPORT_REASONS`](crate::durable::harness::telemetry::REPORT_REASONS);
/// matching here keeps the no-sink path free of serialization and allocation.
fn report_telemetry(
    info: &ReportInfo,
) -> (
    &'static str,
    Option<&'static str>,
    Option<&str>,
    Option<i64>,
) {
    match info {
        ReportInfo::SessionFailed => ("session_failed", None, None, None),
        ReportInfo::TaskFaulted {
            task,
            extension,
            reason,
        } => (
            "task_faulted",
            Some(match reason {
                TaskFaultReason::Threw => "threw",
                TaskFaultReason::RestartLimit => "restart_limit",
            }),
            extension.as_deref(),
            Some(task.id.as_i64()),
        ),
        ReportInfo::TaskParked { task, extension } => (
            "task_parked",
            None,
            extension.as_deref(),
            Some(task.id.as_i64()),
        ),
        ReportInfo::MigrationFailed { task, extension } => (
            "migration_failed",
            None,
            extension.as_deref(),
            Some(task.id.as_i64()),
        ),
        ReportInfo::TaskOutdated {
            task,
            extension,
            reason,
        } => (
            "task_outdated",
            Some(match reason {
                TaskOutdatedReason::MissingTask => "missing_task",
                TaskOutdatedReason::IncompatibleTask => "incompatible_task",
            }),
            extension.as_deref(),
            Some(task.id.as_i64()),
        ),
        ReportInfo::TaskError { task, extension } => (
            "task_error",
            None,
            extension.as_deref(),
            Some(task.id.as_i64()),
        ),
        ReportInfo::ExtensionError {
            extension, task, ..
        } => (
            "extension_error",
            None,
            extension.as_deref(),
            task.as_ref().map(|task| task.id.as_i64()),
        ),
        ReportInfo::HostError { task, .. } => (
            "host_error",
            None,
            None,
            task.as_ref().map(|task| task.id.as_i64()),
        ),
        ReportInfo::ListenerError => ("listener_error", None, None, None),
    }
}

/// The checkpoint of a checkpoint-bearing state.
#[must_use]
pub fn checkpoint_of(state: &TaskState) -> &Value {
    match state {
        TaskState::Pending { checkpoint }
        | TaskState::Running { checkpoint }
        | TaskState::Waiting { checkpoint, .. }
        | TaskState::Sleeping { checkpoint, .. } => checkpoint,
        // `completing` and `terminal` carry an outcome, not a checkpoint.
        TaskState::Completing { .. } | TaskState::Terminal { .. } => &Value::Null,
    }
}

/// The record with `state`; memos and `restarts` disappear once an outcome is
/// decided (upstream `withState`).
#[must_use]
pub fn with_state(record: &TaskRecord, state: TaskState) -> TaskRecord {
    let mut next = record.clone();
    if state.is_terminal() || matches!(state, TaskState::Completing { .. }) {
        next.memos = None;
        next.restarts = None;
    }
    next.state = state;
    next
}

/// The record with an abort mark (upstream `withAbortMark`).
///
/// `restart` for an abandonment; no reason for a request, which replaces a
/// `restart` mark. The restart count starts over: it now counts cut-offs of the
/// abort handler.
#[must_use]
pub fn with_abort_mark(record: &TaskRecord, reason: Option<&str>) -> TaskRecord {
    let mut next = record.clone();
    next.abort_requested = true;
    next.abort_reason = reason.map(str::to_owned);
    next.restarts = None;
    next
}

/// The record without its restart count, as after task-committed progress.
#[must_use]
pub fn progressed(record: &TaskRecord) -> TaskRecord {
    let mut next = record.clone();
    next.restarts = None;
    next
}

/// A live owner's durable cancellation intent (spec §5.4).
#[must_use]
pub fn cancellation_intent(record: &TaskRecord) -> bool {
    !record.state.is_terminal() && (record.abort_requested || failed_outcome(record))
}

/// Whether the record holds or ended with an outcome other than `completed`.
#[must_use]
pub fn failed_outcome(record: &TaskRecord) -> bool {
    match &record.state {
        TaskState::Completing { outcome } | TaskState::Terminal { outcome } => {
            !matches!(outcome, TaskOutcome::Completed { .. })
        }
        _ => false,
    }
}

/// Own memo entry only.
#[must_use]
pub fn memo_of(record: Option<&TaskRecord>, name: &str) -> Option<Value> {
    record.and_then(|record| record.memos.as_ref()?.get(name).cloned())
}

/// A `faulted` outcome with a message.
#[must_use]
pub fn faulted(message: impl Into<String>) -> TaskOutcome {
    TaskOutcome::Faulted {
        error: TaskOutcomeError {
            message: message.into(),
            detail: None,
        },
    }
}

/// A `faulted` outcome with a message and structured detail.
#[must_use]
pub fn faulted_with_detail(message: impl Into<String>, detail: Value) -> TaskOutcome {
    TaskOutcome::Faulted {
        error: TaskOutcomeError {
            message: message.into(),
            detail: Some(detail),
        },
    }
}

/// The message of an error value, rendered without any chance of failing.
#[must_use]
pub fn error_message(error: &(dyn std::error::Error + 'static)) -> String {
    error.to_string()
}

/// An `orphaned` outcome with a reason.
#[must_use]
pub fn orphaned(reason: impl Into<String>) -> TaskOutcome {
    TaskOutcome::Orphaned {
        reason: reason.into(),
    }
}

/// Structural equality of two JSON values.
#[must_use]
pub fn json_equal(left: &Value, right: &Value) -> bool {
    left == right
}

/// The message of a panic payload (Rust's "throw").
#[must_use]
pub fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|text| (*text).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "task handler panicked".to_owned())
}

// ─── Tree index helpers ─────────────────────────────────────────────────────

/// Whether the chain above `node` is loaded, without walking.
fn known(state: &Indexes, node: Node) -> bool {
    match node {
        Node::Task(id) => {
            (state.live.contains_key(&id) || state.settled.contains_key(&id))
                && !state.unloaded.contains(&id)
        }
        Node::Conversation(conversation) => match state.edges.get(&conversation) {
            None => false,
            Some(None) => true,
            Some(Some(owner)) => known(state, Node::Task(*owner)),
        },
    }
}

/// Drop the `settled` entry of a terminal task no live work depends on.
///
/// [`relist`] calls this when a task node is neither live nor listed in
/// [`Indexes::below`]. At that point no live descendant can reach it, so no
/// owner-chain walk will ever ask for its ownership fields again; keeping the
/// entry would only grow with the number of ended tasks. Removing the
/// `unloaded` marker too keeps the two "known node" indexes consistent.
fn forget_settled(state: &mut Indexes, node: Node) {
    if let Node::Task(id) = node {
        if !state.live.contains_key(&id) {
            state.settled.remove(&id);
            state.unloaded.remove(&id);
        }
    }
}

/// List `start` under its parent while it is live or has nodes below it, and
/// unlist it otherwise; then its parent, as far up as that changes
/// (upstream `#relist`). A loop, as chains can be deeper than the call stack.
///
/// This is also the pruning point of [`Indexes::settled`]: a terminal task
/// that is unlisted (no live descendant, hence no `below` entry) is dropped
/// here, and the walk stops as soon as a parent keeps other children. The work
/// is proportional to the entries actually removed, never to the size of the
/// map.
fn relist(state: &mut Indexes, start: Node) {
    let mut node = start;
    loop {
        let needed = match node {
            Node::Task(id) => state.live.contains_key(&id) || state.below.contains_key(&node),
            Node::Conversation(_) => state.below.contains_key(&node),
        };
        let parent: Node = match node {
            Node::Conversation(conversation) => match state.edges.get(&conversation) {
                None => return,
                Some(None) => {
                    if needed {
                        state.roots.insert(conversation);
                    } else {
                        state.roots.remove(&conversation);
                    }
                    return;
                }
                Some(Some(owner)) => Node::Task(*owner),
            },
            Node::Task(id) => {
                let node = state
                    .live
                    .get(&id)
                    .map(node_of)
                    .or_else(|| state.settled.get(&id).copied());
                let Some(node) = node else {
                    return;
                };
                parent_node(node)
            }
        };
        if state
            .below
            .get(&parent)
            .is_some_and(|children| children.contains(&node))
            == needed
        {
            if !needed {
                forget_settled(state, node);
            }
            return;
        }
        if needed {
            let first = !state.below.contains_key(&parent);
            state.below.entry(parent).or_default().insert(node);
            if !first {
                return;
            }
        } else {
            let empty = state.below.get_mut(&parent).is_some_and(|children| {
                children.remove(&node);
                children.is_empty()
            });
            if empty {
                state.below.remove(&parent);
            }
            forget_settled(state, node);
            if state.below.contains_key(&parent) {
                return;
            }
        }
        node = parent;
    }
}

/// The commit's candidate records, which override committed records in walks.
#[derive(Clone, Debug, Default)]
pub(super) struct Overlay {
    pub tasks: HashMap<TaskId, TaskRecord>,
    pub edges: HashMap<ConversationId, Option<TaskId>>,
}

impl Overlay {
    /// Build the overlay from a transaction's staged writes.
    pub(super) fn of(tx: &Transaction) -> Self {
        let mut tasks = HashMap::new();
        for record in tx.staged_tasks() {
            tasks.insert(record.id, record.clone());
        }
        let mut edges = HashMap::new();
        for write in tx.writes() {
            if let StorageWrite::Conversation { value } = write {
                edges.insert(value.id, value.owner.as_ref().map(|owner| owner.task_id));
            }
        }
        Self { tasks, edges }
    }
}

/// One item of a walk down the ownership tree.
#[derive(Clone, Debug)]
pub struct WalkDownItem {
    /// The task reached.
    pub id: TaskId,
    /// Its ownership fields.
    pub node: TaskNode,
    /// Its committed (or staged) record, when it is not terminal.
    pub live: Option<TaskRecord>,
}

/// The tasks below `start` that a walk down reaches (upstream `#walkDown`).
///
/// Background tasks are reached but not entered unless `cross_background`, and
/// neither are the tasks `enter` rejects.
pub(super) fn walk_down(
    state: &Indexes,
    start: Node,
    overlay: Option<&Overlay>,
    cross_background: bool,
    enter: Option<&dyn Fn(TaskId) -> bool>,
) -> Vec<WalkDownItem> {
    let mut out = Vec::new();
    let mut pending: Vec<Node> = vec![start];
    let mut seen: BTreeSet<Node> = BTreeSet::new();
    while let Some(at) = pending.pop() {
        let mut children: BTreeSet<Node> = state.below.get(&at).cloned().unwrap_or_default();
        if let Some(overlay) = overlay {
            for (id, record) in &overlay.tasks {
                if state.live.contains_key(id) || state.settled.contains_key(id) {
                    continue;
                }
                if parent_node(node_of(record)) == at {
                    children.insert(Node::Task(*id));
                }
            }
            if let Node::Task(owner) = at {
                for (conversation, edge) in &overlay.edges {
                    if *edge == Some(owner) && !state.edges.contains_key(conversation) {
                        children.insert(Node::Conversation(*conversation));
                    }
                }
            }
        }
        for child in children {
            match child {
                Node::Conversation(conversation) => {
                    if seen.insert(Node::Conversation(conversation)) {
                        pending.push(Node::Conversation(conversation));
                    }
                }
                Node::Task(id) => {
                    let record = overlay
                        .and_then(|overlay| overlay.tasks.get(&id))
                        .cloned()
                        .or_else(|| state.live.get(&id).cloned());
                    let node = record
                        .as_ref()
                        .map(node_of)
                        .or_else(|| state.settled.get(&id).copied());
                    let Some(node) = node else {
                        continue;
                    };
                    let live = record.filter(|record| !record.state.is_terminal());
                    out.push(WalkDownItem { id, node, live });
                    let entered = cross_background || !node.background;
                    if entered && enter.is_none_or(|enter| enter(id)) && seen.insert(Node::Task(id))
                    {
                        pending.push(Node::Task(id));
                    }
                }
            }
        }
    }
    out
}

/// The live ordinary owned work of `owner` (spec §5.5).
pub(super) fn owned_live(
    state: &Indexes,
    owner: TaskId,
    overlay: Option<&Overlay>,
    first: bool,
    done: Option<&BTreeSet<TaskId>>,
) -> Vec<TaskId> {
    let enter = done.map(|done| {
        let done = done.clone();
        move |id: TaskId| !done.contains(&id)
    });
    let enter_ref: Option<&dyn Fn(TaskId) -> bool> = enter
        .as_ref()
        .map(|closure| closure as &dyn Fn(TaskId) -> bool);
    let mut found = Vec::new();
    for item in walk_down(state, Node::Task(owner), overlay, false, enter_ref) {
        if item.live.is_none() || item.node.background {
            continue;
        }
        found.push(item.id);
        if first {
            break;
        }
    }
    found
}

/// Whether `owner` has live ordinary owned work.
pub(super) fn has_owned_live(
    state: &Indexes,
    owner: TaskId,
    overlay: Option<&Overlay>,
    done: Option<&BTreeSet<TaskId>>,
) -> bool {
    !owned_live(state, owner, overlay, true, done).is_empty()
}

/// The nearest live task above ended ordinary work (upstream
/// `#nearestLiveAbove`). Work at or below a background task holds nothing above
/// it.
pub(super) fn nearest_live_above(
    state: &Indexes,
    node: TaskNode,
    overlay: Option<&Overlay>,
) -> Option<TaskRecord> {
    let mut current = node;
    while !current.background {
        let owner = match current.owner {
            Some(owner) => owner,
            None => match overlay {
                Some(overlay) if overlay.edges.contains_key(&current.conversation_id) => overlay
                    .edges
                    .get(&current.conversation_id)
                    .copied()
                    .flatten()?,
                _ => state
                    .edges
                    .get(&current.conversation_id)
                    .copied()
                    .flatten()?,
            },
        };
        let record = overlay
            .and_then(|overlay| overlay.tasks.get(&owner))
            .cloned()
            .or_else(|| state.live.get(&owner).cloned());
        match record {
            Some(record) if !record.state.is_terminal() => return Some(record),
            Some(record) => current = node_of(&record),
            None => {
                let above = state.settled.get(&owner).copied()?;
                current = above;
            }
        }
    }
    None
}

/// The nearest live owner above `start` whose cancellation intent reaches it.
pub(super) fn cancelling_owner(state: &Indexes, start: Up) -> Option<TaskId> {
    if state.intent.is_empty() {
        return None;
    }
    let mut at = Some(start);
    while let Some(up) = at {
        match up {
            Up::Conversation(conversation) => {
                at = state
                    .edges
                    .get(&conversation)
                    .copied()
                    .flatten()
                    .map(Up::Task);
            }
            Up::Task(id) => {
                let node = state
                    .live
                    .get(&id)
                    .map(node_of)
                    .or_else(|| state.settled.get(&id).copied())?;
                if state.live.get(&id).is_some_and(cancellation_intent) {
                    return Some(id);
                }
                if node.background {
                    return None;
                }
                at = Some(parent_of(node));
            }
        }
    }
    None
}

/// Whether a live owner's cancellation intent reaches `start`.
pub(super) fn below_cancelled(state: &Indexes, start: Up) -> bool {
    cancelling_owner(state, start).is_some()
}

/// Whether ordinary traversal from `scope` reaches `start` (upstream `#inScope`).
///
/// `None` while an edge is not loaded.
pub(super) fn in_scope(state: &Indexes, start: Up, scope: Option<ConversationId>) -> Option<bool> {
    let mut at = Some(start);
    while let Some(up) = at {
        match up {
            Up::Conversation(conversation) => {
                if scope == Some(conversation) {
                    return Some(true);
                }
                match state.edges.get(&conversation) {
                    None => return None,
                    Some(None) => return Some(scope.is_none()),
                    Some(Some(owner)) => at = Some(Up::Task(*owner)),
                }
            }
            Up::Task(id) => {
                let node = state
                    .live
                    .get(&id)
                    .map(node_of)
                    .or_else(|| state.settled.get(&id).copied())?;
                if node.background {
                    return Some(false);
                }
                at = Some(parent_of(node));
            }
        }
    }
    Some(scope.is_none())
}

/// Whether ended work frees the nearest live task above it (upstream
/// `#release`).
fn release(state: &mut Indexes, node: TaskNode) {
    if let Some(above) = nearest_live_above(state, node, None) {
        if matches!(above.state, TaskState::Completing { .. }) {
            state.finalize_checks.insert(above.id);
        } else if above.abort_requested {
            state.runnable.insert(above.id);
        }
    }
}

/// A sleep whose `until` the clock has not reached; an abort mark ends a sleep.
pub(super) fn not_due(record: &TaskRecord, now: i64) -> bool {
    matches!(&record.state, TaskState::Sleeping { until, .. } if !record.abort_requested && *until > now)
}

pub(super) fn add_to(map: &mut HashMap<TaskId, BTreeSet<TaskId>>, key: TaskId, value: TaskId) {
    map.entry(key).or_default().insert(value);
}

pub(super) fn remove_from(map: &mut HashMap<TaskId, BTreeSet<TaskId>>, key: TaskId, value: TaskId) {
    if let Some(values) = map.get_mut(&key) {
        values.remove(&value);
        if values.is_empty() {
            map.remove(&key);
        }
    }
}

/// Scan every page of a task query, inside a commit callback.
async fn scan_all(
    state: &SessionState<'_>,
    query: TaskQuery,
) -> Result<Vec<TaskRecord>, StorageError> {
    let mut out = Vec::new();
    let mut cursor = None;
    loop {
        let storage = state.storage.storage();
        let page = state
            .storage
            .read(
                &Context::background(),
                storage.scan_tasks(query.clone(), SCAN_PAGE_SIZE, cursor),
            )
            .await?;
        out.extend(page.items);
        match page.next {
            Some(next) => cursor = Some(next),
            None => return Ok(out),
        }
    }
}

/// Read one conversation inside a commit callback.
async fn read_conversation_in(
    state: &SessionState<'_>,
    id: ConversationId,
) -> Result<Option<ConversationRecord>, StorageError> {
    let storage = state.storage.storage();
    state
        .storage
        .read(&Context::background(), storage.conversation(id))
        .await
}

/// The system clock in epoch milliseconds.
fn system_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|delta| delta.as_millis() as i64)
        .unwrap_or(0)
}
