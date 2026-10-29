//! The `Harness` facade: one durable session, its scheduler, and the
//! conversation conveniences hosts use.
//!
//! Port of upstream `packages/durable/src/harness/harness.ts`. Everything here
//! is composition: the session kernel
//! ([`Session`](crate::durable::session::facade::Session)), the scheduler
//! ([`TaskScheduler`](super::scheduler::TaskScheduler)), the built-in
//! conversation documents ([`documents`](super::documents)), the task graph
//! ([`task_graph`](super::task_graph)), the usage ledger
//! ([`usage`](super::usage)) and the provider identity
//! ([`provider`](super::provider)) already own their logic. This module wires
//! them into one handle and adds nothing of its own beyond the wiring.
//!
//! # One mutation line
//!
//! upstream's `HarnessImpl extends SessionImpl`: one Session, one line, and the
//! scheduler mutates through it. pir's [`TaskScheduler`] **opens and owns** its
//! [`Session`], so the harness takes `scheduler.session().clone()` rather than
//! opening a second one over the same storage — two lines over one storage
//! would be two writers and lose commits. That single fact also explains why
//! the conversation-created hook is installed through [`SessionOptions`]: the
//! harness configures the session the scheduler opens instead of decorating it
//! afterwards.
//!
//! # Deliberate deviations (all forced by the existing seams)
//!
//! 1. **A task-owned conversation inherits its owner's `pi.agent`.** The
//!    conversation-created hook copies the owner task's conversation agent
//!    through [`Transaction::committed_document`], which is the committed read
//!    surface the hook lacked before; a staged owner agent from the same commit
//!    wins (read-your-writes). The five built-in documents are staged
//!    atomically with the conversation, exactly upstream: the hook is installed
//!    through [`SessionOptions::conversation_created`], so even a conversation
//!    created by a task-owned path gets them — and the inherited agent — in its
//!    creating commit.
//! 2. **`Harness::closed` stays a predicate.** `Session` now has a `closed`
//!    future ([`Session::closed`]), but [`Harness::closed`] keeps its
//!    boolean signature for compatibility. Hosts that need the upstream
//!    awaitable form use the session's future.
//! 3. **`submission` returns the record; `submission_handle` returns the
//!    handle.** [`Harness::submission`] keeps its record-returning signature
//!    for compatibility. [`Harness::submission_handle`] and
//!    [`Harness::submit_handle`] return the upstream [`Submission`] face, with
//!    `status`, `wait` and `abort`, backed by the harness's own waiter
//!    registry.
//! 4. **`root` always uses the reserved id.** [`Session::create_root_conversation`]
//!    stages [`ROOT_CONVERSATION_ID`] (or returns the committed root) in the
//!    same commit, so a fresh storage and an imported session agree on which
//!    conversation is the root.
//! 5. **`submit`/`compact` are implemented** through commit-time creators:
//!    admission (`admitSubmission`) and `startRun` for the former, the task
//!    plus its `pi.live` status for the latter, each in one commit. A host
//!    that wants to *run* the created tasks still installs the executable
//!    definitions (`generation_task`/`compaction_task`).
//! 6. **`CommitPersistence` lives in the session kernel.** Deferred commits
//!    (`Session::commit_with_options` / `Session::doc_with_options`) are held
//!    per document with merged pending operations, published to listeners in
//!    memory, and stored on the next read, immediate commit, close or deadline.
//!    The harness itself keeps upstream's `immediate` default everywhere: a
//!    scheduler or task commit is never deferred, so the crash-recovery
//!    invariants are unchanged. See the `session` module docs for the exact
//!    deviation (host listeners observe a held commit at adoption, not at
//!    store).
//!
//! # Telemetry
//!
//! Upstream's `safeReport` counts reports through the telemetry schema. pir has
//! no built-in backend (see [`telemetry`](super::telemetry)); [`Harness::report`]
//! hands the report to the host and then records it through
//! [`HarnessTelemetry::record_report`] (the `pi.durable.report` event plus the
//! `pi.durable.reports` counter), and [`Harness::submit`] brackets its
//! admission commit with the `pi.durable.submit` span. Terminal
//! `pi.durable.submission` events are emitted once, by the session's commit
//! publication. With [`HarnessTelemetry::none`] every emission is one branch on
//! an absent sink: no allocation, no virtual call.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};

use serde_json::Value;
use tokio::sync::{oneshot, Notify};

use crate::durable::context::Context;
use crate::durable::delta::Op;
use crate::durable::documents::{
    address_id, AnyDocDefinition, DocCheckpointWhen, DocDefinition, ResolvedAddress,
};
use crate::durable::errors::{ReportInfo, StorageError, TaskFaultReason, TaskOutdatedReason};
use crate::durable::harness::agent::{configure, resolve_agent, Agent, AgentChange, AgentState};
use crate::durable::harness::compaction::{
    create_compaction_in_state, CompactionInput, CompactionReason,
};
use crate::durable::harness::context::{derive_context, ContextView};
use crate::durable::harness::documents::PROVIDER_KIND;
use crate::durable::harness::documents::{agent_definition, conversation_definitions, AGENT_KIND};
use crate::durable::harness::generation::{
    read_conversation_document_state, stage_conversation_document_state,
};
use crate::durable::harness::inbox::{InboxState, INBOX_DEFINITION, INBOX_KIND};
use crate::durable::harness::live::LIVE_KIND;
use crate::durable::harness::provider::ensure_provider_session_id;
use crate::durable::harness::registry::{builtin_tasks, RegistryReader, RegistrySnapshot};
use crate::durable::harness::scheduler::{
    AbortResult, Inspection, SchedulingState, TaskInspection, TaskScheduler, TaskSchedulerOptions,
    Wake,
};
use crate::durable::harness::settings::{resolve_settings, HarnessSettings, Settings};
use crate::durable::harness::submissions::{
    admit_submission_in, Admission, AdmissionStatus, Admitted, QueueModes, SubmissionDraft,
};
use crate::durable::harness::task::Task;
use crate::durable::harness::task_graph::{TaskGraphBlocking, TaskGraphView};
use crate::durable::harness::telemetry::{HarnessTelemetry, SpanHandle, SpanLink, SpanStatus};
use crate::durable::harness::usage::{add_usage_state, UsageState, USAGE_KIND};
use crate::durable::harness::util::Report;
use crate::durable::harness::view::{
    AttachedView, ConversationView, ConversationViews, Detach, ViewObserver,
};
use crate::durable::ids::{ConversationId, EntryId, SubmissionId, TaskId, ROOT_CONVERSATION_ID};
use crate::durable::session::assemble::DocBehaviours;
use crate::durable::session::attach::{attach_state, detach, Attached};
use crate::durable::session::facade::{
    ConversationCreatedFuture, ConversationCreatedHook, ConversationOwnership, Session,
};
use crate::durable::session::line::{CommitFuture, JobValue, SessionOptions, SessionState};
use crate::durable::session::observation::{CommittedWatch, WatchEnd, MAX_PENDING_WATCH_FRAMES};
use crate::durable::session::publication::{CommitChange, CommitPublication, TableCommitChange};
use crate::durable::session::transaction::{
    StagedDocument, SubmissionChange, Transaction, TransactionScope,
};
use crate::durable::timers::Timers;
use crate::durable::types::{
    ConversationQuery, ConversationRecord, Cursor, DocumentAddress, DocumentFork, DocumentScope,
    EntryQuery, EntryRecord, JsonObject, Page, ScanOrder, Storage, SubmissionQuery,
    SubmissionRecord, SubmissionStatus, SubmissionType,
};
use crate::durable::DocumentSemantics;

/// Page size of the harness's own scans (upstream `SCAN_PAGE_SIZE`).
pub const SCAN_PAGE_SIZE: usize = 256;

// ─── Errors ─────────────────────────────────────────────────────────────────

/// What a [`Harness`] operation can fail with.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HarnessError {
    /// A storage or session failure.
    #[error(transparent)]
    Storage(#[from] StorageError),
    /// A method whose upstream implementation needs a module this port does
    /// not have yet.
    ///
    /// The variant names the method and the upstream seam it needs, so a caller
    /// can tell "not built here" from "failed".
    #[error("Harness::{method} is not ported: it needs {needs}")]
    NotPorted {
        /// The method that was called.
        method: &'static str,
        /// The upstream seam the port lacks.
        needs: &'static str,
    },
    /// The harness was closed.
    #[error("Harness is closed")]
    Closed,
    /// The request was invalid.
    #[error("{0}")]
    Invalid(String),
}

impl From<crate::durable::errors::DocumentError> for HarnessError {
    fn from(error: crate::durable::errors::DocumentError) -> Self {
        Self::Storage(error.into())
    }
}

// ─── Options ────────────────────────────────────────────────────────────────

/// A report handler: the message and what it is about.
///
/// Upstream passes the original `unknown`; Rust has no such value, so the
/// message is the already-rendered error text. A handler's own panic is
/// contained, never fatal.
pub type ReportHandler = Arc<dyn Fn(&str, &ReportInfo) + Send + Sync>;

/// Everything [`Harness::open`] is configured with.
///
/// Upstream's `HarnessOptions` also carries `models` and `env`. Model access
/// and environments belong to the generation lane's own seams
/// ([`generation::GenerationModels`](super::generation::GenerationModels),
/// `ExecutionEnv`), so neither is duplicated here.
#[derive(Clone, Default)]
pub struct HarnessOptions {
    /// The live registry. Missing built-in task definitions fail `open`, as
    /// upstream does.
    pub registry: Option<Arc<dyn RegistryReader>>,
    /// The executable task definitions the scheduler may run.
    ///
    /// The registry holds *metadata* definitions
    /// ([`durable::tasks::Task`](crate::durable::tasks::Task)); the scheduler
    /// needs the executable erasure ([`Task`]). A host that built
    /// [`generation_task`](super::generation::generation_task) or
    /// [`compaction_task`](super::compaction::compaction_task) passes them here.
    /// An empty list is valid: the harness then runs no task code.
    pub tasks: Vec<Task>,
    /// Run settings, read at each use.
    pub settings: Option<Arc<dyn Fn() -> HarnessSettings + Send + Sync>>,
    /// The harness clock in epoch milliseconds. Every process opening the same
    /// storage must use the same clock (sleeps store deadlines on it).
    pub now: Option<Arc<dyn Fn() -> i64 + Send + Sync>>,
    /// Host timers. Defaults to the native timer.
    pub timers: Option<Timers>,
    /// Called after open and whenever [`Harness::wake`] changes.
    pub on_wake: Option<Arc<dyn Fn(Wake) + Send + Sync>>,
    /// Receives failures that do not fail the calling operation.
    pub on_report: Option<ReportHandler>,
    /// How long an unobserved loaded document stays in memory.
    pub document_retention_ms: Option<i64>,
    /// The telemetry handle hosts may emit through.
    ///
    /// [`Harness::submit`] emits the `pi.durable.submit` span; terminal
    /// `pi.durable.submission` events come from the session's commit
    /// publication. [`Harness::report`] records
    /// `pi.durable.report`/`pi.durable.reports`. Absent means
    /// [`HarnessTelemetry::none`]: every emission is skipped without allocating.
    /// [`HarnessTelemetry::from_telemetry`] installs
    /// [`NativeTelemetrySink`](crate::durable::harness::telemetry::NativeTelemetrySink),
    /// which forwards spans, links, events and metrics to the crate's portable
    /// telemetry core.
    pub telemetry: Option<HarnessTelemetry>,
}

impl std::fmt::Debug for HarnessOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HarnessOptions")
            .field("has_registry", &self.registry.is_some())
            .field("tasks", &self.tasks.len())
            .field("on_wake", &self.on_wake.is_some())
            .field("on_report", &self.on_report.is_some())
            .finish_non_exhaustive()
    }
}

/// The scheduling state and unsettled submissions of one session.
#[derive(Clone, Debug)]
pub struct HarnessInspection {
    /// Whether scheduling runs.
    pub scheduling: SchedulingState,
    /// Every live task with its derived state, in id order.
    pub tasks: Vec<TaskInspection>,
    /// Queued and placed submissions, in id order.
    pub submissions: Vec<SubmissionRecord>,
}

// ─── Conversation handle ────────────────────────────────────────────────────

/// One conversation of a harness.
///
/// A thin handle: every operation composes a module that already owns the
/// behaviour. It exposes upstream's `agent`, `configure`, `submit`, `reset`,
/// `compact`, `commit`, `context`, `entries`, `fork`, `abort`, `waitForIdle`
/// and `watch`; the view mount behind `watch` is
/// [`view::ConversationViews`](super::view::ConversationViews).
#[derive(Clone)]
pub struct Conversation {
    harness: Harness,
    id: ConversationId,
}

impl std::fmt::Debug for Conversation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Conversation")
            .field("id", &self.id)
            .finish()
    }
}

impl Conversation {
    /// The conversation identifier.
    #[must_use]
    pub fn id(&self) -> ConversationId {
        self.id
    }

    /// The harness this conversation belongs to.
    #[must_use]
    pub fn harness(&self) -> &Harness {
        &self.harness
    }

    /// Fork this conversation at `at` (spec §3.7).
    ///
    /// A fork copies every `asOf`/`current` document and starts `initial` ones
    /// empty, then gets its own built-in documents.
    pub async fn fork(
        &self,
        at: EntryId,
        ownership: ConversationOwnership,
        context: Context,
    ) -> Result<Conversation, HarnessError> {
        self.harness.fork(self.id, at, ownership, context).await
    }

    /// Mark every live non-background task reachable from this conversation
    /// (spec §5.4) and wait for it to be idle.
    pub async fn abort(&self, background: bool) -> Result<(), HarnessError> {
        self.harness.abort_conversation(self.id, background).await
    }

    /// Resolve when this conversation's ordinary ownership scope is idle.
    pub async fn wait_for_idle(&self) -> Result<(), HarnessError> {
        // Upstream `Conversation.waitForIdle` resumes first; otherwise a
        // pending task behind a paused scheduler would never reach idle.
        self.harness.inner.scheduler.resume();
        self.harness
            .scheduler()
            .wait_for_idle(Some(self.id))
            .await?;
        Ok(())
    }

    /// The conversation's committed `pi.agent` resolved against `snapshot`, or
    /// the current registry when `snapshot` is absent.
    pub async fn agent(
        &self,
        snapshot: Option<&RegistrySnapshot>,
        context: &Context,
    ) -> Result<Agent, HarnessError> {
        self.harness.agent(self.id, snapshot, context).await
    }

    /// Admit one submission (upstream `Conversation.submit`).
    pub async fn submit(
        &self,
        draft: SubmissionDraft,
        context: Context,
    ) -> Result<SubmissionRecord, HarnessError> {
        self.harness.submit(self.id, draft, context).await
    }

    /// Admit one submission and return its handle (upstream
    /// `Conversation.submit` returns a `Submission`).
    ///
    /// [`Conversation::submit`] keeps returning the committed record for
    /// compatibility; this is the awaitable/abortable form.
    pub async fn submit_handle(
        &self,
        draft: SubmissionDraft,
        context: Context,
    ) -> Result<Submission, HarnessError> {
        self.harness.submit_handle(self.id, draft, context).await
    }

    /// Admit a `pi.reset` entry, carrying `handoff` as a user message when
    /// given (upstream `Conversation.reset`).
    ///
    /// The write is admitted like any other submission: placed at once when
    /// idle, otherwise queued and placed at the next boundary. The reset's
    /// `head: "self"` starts a new context at its own entry, so entries before
    /// it leave the active range.
    pub async fn reset(
        &self,
        handoff: Option<String>,
        context: Context,
    ) -> Result<SubmissionRecord, HarnessError> {
        self.harness.reset(self.id, handoff, context).await
    }

    /// Committed raw active transcript and model context, or the context as of
    /// `at` (upstream `Conversation.context`).
    ///
    /// With `at`, the view is the one `fork(at)` would start from, without
    /// creating a conversation. An `at` that is not visible from this
    /// conversation is an error, exactly as upstream's bounds capture rejects
    /// it.
    pub async fn context(
        &self,
        context: Context,
        at: Option<EntryId>,
    ) -> Result<ContextView, HarnessError> {
        self.harness.read_context(self.id, at, &context).await
    }

    /// One page of this conversation's fork-aware entry history (upstream
    /// `Conversation.entries`).
    ///
    /// `query.conversation_id` is overwritten with this handle's id, so the
    /// caller supplies only the bounds and order.
    pub async fn entries(
        &self,
        mut query: EntryQuery,
        limit: usize,
        cursor: Option<Cursor>,
        context: &Context,
    ) -> Result<Page<EntryRecord>, HarnessError> {
        self.harness.assert_usable()?;
        query.conversation_id = self.id;
        Ok(self
            .harness
            .inner
            .session
            .scan_entries(query, limit, cursor, context)
            .await?)
    }

    /// One commit whose `tx.create_task()` defaults to this conversation
    /// (upstream `Conversation.commit`).
    pub async fn commit<T, F>(&self, context: Context, run: F) -> Result<T, HarnessError>
    where
        T: Send + 'static,
        F: for<'a> FnOnce(&'a mut Transaction, &'a SessionState<'_>) -> CommitFuture<'a>
            + Send
            + 'static,
    {
        self.harness.assert_usable()?;
        let scope = TransactionScope {
            conversation_id: Some(self.id),
            ..TransactionScope::default()
        };
        Ok(self
            .harness
            .inner
            .session
            .commit_scoped(scope, context, run)
            .await?)
    }

    /// The structural conversation view as a serialized exact-frame watch
    /// (upstream `Conversation.watch`).
    ///
    /// The initial revision is delivered on [`ConversationWatch::start`] and
    /// later commits arrive in commit order; returning `false` from the
    /// listener ends the watch. The acquisition context's cancellation stops
    /// the watch when a Tokio runtime is available.
    pub async fn watch(&self, context: &Context) -> Result<ConversationWatch, HarnessError> {
        self.harness.assert_usable()?;
        let attached = self
            .harness
            .inner
            .views
            .attach(self.id, ConversationWatchObserver::new, context)
            .await?;
        ConversationWatch::new(attached, context)
    }

    /// Request a manual compaction (upstream `Conversation.compact`).
    pub async fn compact(
        &self,
        instructions: Option<String>,
        context: Context,
    ) -> Result<TaskId, HarnessError> {
        self.harness.compact(self.id, instructions, context).await
    }

    /// Apply one `pi.agent` change in its own commit.
    pub async fn configure(
        &self,
        change: &AgentChange,
        context: Context,
    ) -> Result<(), HarnessError> {
        let change = change.clone();
        self.harness
            .inner
            .session
            .doc(
                agent_address(self.id),
                agent_definition(),
                context,
                move |value| {
                    let state: AgentState = decode_agent(value);
                    *value = encode_agent(&configure(&state, &change));
                },
            )
            .await?;
        Ok(())
    }
}

// ─── Document watch ─────────────────────────────────────────────────────────

/// Drives one [`CommittedWatch`]'s frame queue off the session's mutation line.
///
/// A commit listener runs synchronously on the line, so it must only append to
/// the watch core and wake this loop; it never calls `CommittedWatch::drain`
/// itself. Exactly one delivery task exists per watch and it is the only
/// drainer, which is what keeps callbacks serialized and frames in commit
/// order. [`Notify`] coalesces wakes: a wake that arrives while a pass runs
/// leaves one permit for the next pass, and that pass drains the whole queue.
struct WatchDelivery {
    watch: CommittedWatch,
    signal: Notify,
    /// The runtime the delivery task is spawned on. `None` only when the watch
    /// was built outside a tokio runtime; `wake` then drains inline, the only
    /// delivery available without a task.
    handle: Option<tokio::runtime::Handle>,
    /// The harness that owns this watch. The delivery task holds only a weak
    /// handle, so a dropped `Harness` cannot be kept alive by a parked watch.
    owner: Weak<Inner>,
    /// Closes when the owning `Inner` is dropped; the delivery loop selects on
    /// it so a parked task wakes and exits even though no watch signal fires.
    liveness: tokio::sync::watch::Receiver<()>,
    /// Reports a contained listener panic, like the commit-listener policy
    /// (`run_contained`) the watch used before delivery moved off the line.
    report: Report,
    /// Serializes every drain, including the one `CommittedWatch::start` runs
    /// internally, so an explicit [`DocumentWatch::start`] can never overlap a
    /// delivery pass.
    draining: std::sync::Mutex<()>,
}

impl WatchDelivery {
    fn new(
        watch: CommittedWatch,
        report: Report,
        owner: Weak<Inner>,
        liveness: tokio::sync::watch::Receiver<()>,
    ) -> Self {
        Self {
            watch,
            signal: Notify::new(),
            handle: tokio::runtime::Handle::try_current().ok(),
            owner,
            liveness,
            report,
            draining: std::sync::Mutex::new(()),
        }
    }

    /// Drain the watch under the delivery lock.
    fn drain(&self) {
        let _guard = self
            .draining
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.watch.drain();
    }

    /// Start the delivery loop.
    ///
    /// Called only after the listener is installed and [`CommittedWatch::start`]
    /// has drained the initial prefix, so its synchronous drain can never
    /// overlap the loop.
    fn start(self: &Arc<Self>) {
        let Some(handle) = &self.handle else {
            return;
        };
        let delivery = Arc::clone(self);
        drop(handle.spawn(async move {
            delivery.run().await;
        }));
    }

    /// Wake the loop: frames were queued, or the watch ended.
    ///
    /// `notify_one` stores a permit when the loop is not parked, so a wake
    /// cannot be lost between a pass finishing and the next `notified().await`.
    fn wake(&self) {
        if self.handle.is_some() {
            self.signal.notify_one();
        } else {
            self.drain();
        }
    }

    /// Wake a parked delivery task after the watch ended, without draining.
    ///
    /// `stop` may run from inside the listener, where a nested drain would try
    /// to re-lock the listener slot; in the no-task fallback there is nothing
    /// to wake either.
    fn notify_stop(&self) {
        if self.handle.is_some() {
            self.signal.notify_one();
        }
    }

    /// Deliver queued frames one at a time until the watch ends or its Harness
    /// is dropped.
    async fn run(self: Arc<Self>) {
        loop {
            if self.owner.upgrade().is_none() || self.ended() {
                return;
            }
            let mut liveness = self.liveness.clone();
            tokio::select! {
                _ = self.signal.notified() => {}
                changed = liveness.changed() => {
                    // The sender is dropped with the owning `Inner`. `changed`
                    // also returns `Err` if the Harness disappeared before the
                    // task reached this select.
                    if changed.is_err() || self.owner.upgrade().is_none() {
                        return;
                    }
                }
            }
            if self.owner.upgrade().is_none() {
                return;
            }
            // This task is the only drainer after `start`; `drain` pops FIFO
            // under the core lock and invokes the listener outside it. Frames
            // queued during the pass stay queued for the next wake.
            //
            // A panicking host listener is contained exactly as the old
            // synchronous commit listener was: the frame it was handling is
            // lost and the watch stays live for later frames.
            if let Err(payload) =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.drain()))
            {
                let message = payload
                    .downcast_ref::<&str>()
                    .map(|text| (*text).to_owned())
                    .or_else(|| payload.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "document watch listener panicked".to_owned());
                (self.report)(&message);
            }
        }
    }

    /// Whether the watch ended (stop, retirement, listener failure, or close).
    fn ended(&self) -> bool {
        self.watch
            .core()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .ended()
            .is_some()
    }
}

/// The shared stop state of a [`DocumentWatch`] and its clones.
///
/// Keeping the subscription and the close reason here, rather than in each
/// clone, makes explicit `stop()` and the last-handle `Drop` idempotent.
struct WatchStop {
    stopped: AtomicBool,
    session: Session,
    subscription: u64,
    watch: CommittedWatch,
    delivery: Arc<WatchDelivery>,
    cancel: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl WatchStop {
    fn stop_with(&self, reason: WatchEnd) {
        if !self.stopped.swap(true, Ordering::AcqRel) {
            self.session.unsubscribe_commits(self.subscription);
            self.watch.stop(reason);
            // A parked delivery task would otherwise wait forever on a watch
            // that can never wake it again.
            self.delivery.notify_stop();
        }
        // Always take the cancellation task: close/cancellation may have won
        // the race with installing it, and a later stop must still abort it.
        if let Some(task) = self
            .cancel
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            task.abort();
        }
    }

    fn stop(&self) {
        self.stop_with(WatchEnd::Stopped);
    }
}

/// A serialized exact-frame watch over one document incarnation.
///
/// Mirrors [`runtime::DocumentWatch`](super::scheduler::runtime::DocumentWatch)
/// but is owned by the harness rather than a task invocation, so a host can
/// watch a document outside any task.
///
/// Delivery is serialized by the watch's delivery task: the listener never
/// runs on the session's mutation line after `watch_doc` returns. Dropping the
/// last handle stops the watch and its delivery task.
pub struct DocumentWatch {
    watch: CommittedWatch,
    delivery: Arc<WatchDelivery>,
    stop: Arc<WatchStop>,
    /// Number of live `DocumentWatch` handles; the last one stops the watch.
    handles: Arc<AtomicUsize>,
}

impl Clone for DocumentWatch {
    fn clone(&self) -> Self {
        self.handles.fetch_add(1, Ordering::Relaxed);
        Self {
            watch: self.watch.clone(),
            delivery: self.delivery.clone(),
            stop: self.stop.clone(),
            handles: self.handles.clone(),
        }
    }
}

impl std::fmt::Debug for DocumentWatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DocumentWatch")
            .field("stopped", &self.is_stopped())
            .finish_non_exhaustive()
    }
}

impl Drop for DocumentWatch {
    fn drop(&mut self) {
        if self.handles.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.stop.stop();
        }
    }
}

impl DocumentWatch {
    /// The newest committed value, or `None` after retirement.
    #[must_use]
    pub fn value(&self) -> Option<JsonObject> {
        self.watch
            .core()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .value()
            .cloned()
    }

    /// Whether the watch was stopped or ended.
    #[must_use]
    pub fn is_stopped(&self) -> bool {
        self.stop.stopped.load(Ordering::Acquire)
            || self
                .watch
                .core()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .ended()
                .is_some()
    }

    /// Install the sole listener (upstream `WatchHandle.start`).
    ///
    /// The current undelivered prefix is drained on the calling thread;
    /// [`CommittedWatch`] offers no install-without-drain. Later frames are
    /// delivered by the watch's delivery task, never on the mutation line.
    /// Returning `false` ends the watch.
    pub fn start(&self, listener: impl Fn(&JsonObject) -> bool + Send + Sync + 'static) {
        let _guard = self
            .delivery
            .draining
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.watch.start(move |frame| match &frame.value {
            Some(value) => listener(value),
            None => false,
        });
    }

    /// Stop the watch. Idempotent, including across clones.
    pub fn stop(&self) {
        self.stop.stop();
    }
}

// ─── Submission waiters and handle ──────────────────────────────────────────

/// Parked `wait()` calls, keyed by submission.
#[derive(Default)]
struct WaiterRegistry {
    pending: HashMap<SubmissionId, Vec<(u64, oneshot::Sender<SubmissionRecord>)>>,
    closed: bool,
}

/// The submission-waiter registry of one harness.
///
/// Registration and settlement both happen on the session's mutation line
/// (registration in a read-on-line job, settlement in an internal commit
/// listener), so a settlement can never fall between a read of the record and
/// the registration of a wait.
#[derive(Default)]
struct SubmissionWaiters {
    state: Mutex<WaiterRegistry>,
    next: AtomicU64,
}

impl SubmissionWaiters {
    /// Register `sender`; `None` when the session is already closing.
    fn register(&self, id: SubmissionId, sender: oneshot::Sender<SubmissionRecord>) -> Option<u64> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.closed {
            return None;
        }
        let token = self.next.fetch_add(1, Ordering::AcqRel);
        state.pending.entry(id).or_default().push((token, sender));
        Some(token)
    }

    /// Withdraw one registration (a cancelled wait).
    fn cancel(&self, id: SubmissionId, token: u64) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(waiters) = state.pending.get_mut(&id) {
            waiters.retain(|(candidate, _)| *candidate != token);
            if waiters.is_empty() {
                state.pending.remove(&id);
            }
        }
    }

    /// Settle every wait for `record`; a no-op for a non-terminal record.
    fn resolve(&self, record: SubmissionRecord) {
        let waiters = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.pending.remove(&record.id)
        };
        if let Some(waiters) = waiters {
            for (_, sender) in waiters {
                let _ = sender.send(record.clone());
            }
        }
    }

    /// Reject every parked wait: closing drops the senders.
    fn close(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.closed = true;
        state.pending.clear();
    }
}

/// What [`Submission::abort`] did (upstream `"aborted" | "already_placed" |
/// "settled"`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubmissionAbortResult {
    /// A queued submission was settled `unanswered` and removed from the
    /// inbox.
    Aborted,
    /// The submission was already placed in the transcript; nothing changed.
    AlreadyPlaced,
    /// The submission was already terminal; nothing changed.
    Settled,
}

impl SubmissionAbortResult {
    /// The upstream wire value.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Aborted => "aborted",
            Self::AlreadyPlaced => "already_placed",
            Self::Settled => "settled",
        }
    }
}

/// Awaitable host object for one durably admitted submission (upstream
/// `Submission`).
///
/// [`Harness::submit`] keeps returning the committed record for compatibility;
/// [`Harness::submit_handle`] and [`Harness::submission_handle`] return this
/// handle. A handle is stateless: it reads the committed record on every call,
/// so comparing handles by [`Submission::id`] is enough.
#[derive(Clone)]
pub struct Submission {
    harness: Harness,
    id: SubmissionId,
}

impl std::fmt::Debug for Submission {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Submission").field("id", &self.id).finish()
    }
}

impl Submission {
    fn new(harness: Harness, id: SubmissionId) -> Self {
        Self { harness, id }
    }

    /// The submission identifier.
    #[must_use]
    pub fn id(&self) -> SubmissionId {
        self.id
    }

    /// The harness this submission belongs to.
    #[must_use]
    pub fn harness(&self) -> &Harness {
        &self.harness
    }

    /// The committed record; an error when the submission does not exist.
    pub async fn status(&self, context: &Context) -> Result<SubmissionRecord, HarnessError> {
        self.harness.assert_usable()?;
        match self
            .harness
            .inner
            .session
            .submission(self.id, context)
            .await?
        {
            Some(record) => Ok(record),
            None => Err(HarnessError::Invalid(format!(
                "Submission {} does not exist",
                self.id
            ))),
        }
    }

    /// Resolve once the submission is terminal (`done` or `unanswered`).
    ///
    /// Asking for a wait enables task scheduling, as upstream does: a queued
    /// submission only settles when the run that consumes it makes progress.
    pub async fn wait(&self, context: Context) -> Result<SubmissionRecord, HarnessError> {
        self.harness.assert_usable()?;
        self.harness.inner.scheduler.resume();
        let id = self.id;
        let waiters = self.harness.inner.submission_waiters.clone();
        let read_context = context.clone();
        let (sender, receiver) = oneshot::channel();
        let outcome = self
            .harness
            .inner
            .session
            .handle()
            .read_on_line(move |state| {
                Box::pin(async move {
                    let storage = state.storage.storage();
                    let record = state
                        .storage
                        .read(&read_context, storage.submission(id))
                        .await?;
                    let Some(record) = record else {
                        return Err(StorageError::invalid(format!(
                            "Submission {id} does not exist"
                        )));
                    };
                    let outcome = if is_settled(&record) {
                        WaitOutcome::Settled(record)
                    } else {
                        match waiters.register(id, sender) {
                            Some(token) => WaitOutcome::Registered(token),
                            None => WaitOutcome::Closed,
                        }
                    };
                    Ok(Box::new(outcome) as JobValue)
                })
            })
            .await?;
        match outcome {
            WaitOutcome::Settled(record) => Ok(record),
            WaitOutcome::Closed => Err(HarnessError::Closed),
            WaitOutcome::Registered(token) => {
                let waiters = self.harness.inner.submission_waiters.clone();
                let cancelled = async {
                    match context.signal() {
                        Some(signal) => signal.cancelled().await,
                        None => std::future::pending::<()>().await,
                    }
                };
                tokio::select! {
                    received = receiver => {
                        match received {
                            Ok(record) => Ok(record),
                            Err(_) => {
                                waiters.cancel(id, token);
                                Err(HarnessError::Closed)
                            }
                        }
                    }
                    _ = cancelled => {
                        waiters.cancel(id, token);
                        Err(HarnessError::Invalid(
                            "The wait context was aborted".to_owned(),
                        ))
                    }
                }
            }
        }
    }

    /// Withdraw a queued submission (upstream `Submission.abort`).
    ///
    /// A queued submission is settled `unanswered` with reason `aborted` and
    /// its inbox item is removed in the same commit. A placed input and a
    /// terminal submission are reported unchanged.
    pub async fn abort(&self, context: Context) -> Result<SubmissionAbortResult, HarnessError> {
        let id = self.id;
        self.harness
            .abort_submission(id, context, None)
            .await?
            .ok_or_else(|| HarnessError::Invalid(format!("Submission {id} does not exist")))
    }
}

/// Whether a submission can no longer change.
fn is_settled(record: &SubmissionRecord) -> bool {
    matches!(
        record.status,
        SubmissionStatus::Done | SubmissionStatus::Unanswered
    )
}

/// What one `wait` registration produced.
enum WaitOutcome {
    /// The submission was already terminal.
    Settled(SubmissionRecord),
    /// A waiter was registered under this token.
    Registered(u64),
    /// The session is closing; no waiter can settle.
    Closed,
}

// ─── Conversation watch ─────────────────────────────────────────────────────

/// One observed revision of a conversation (upstream `WatchHandle` frame).
#[derive(Clone, Debug)]
pub struct ConversationFrame {
    /// The immutable revision.
    pub value: Arc<ConversationView>,
    /// The exact operation batch that produced it.
    pub ops: Vec<Op>,
    /// The frame's context, without the producer's cancellation.
    pub context: Context,
}

/// A conversation-watch listener; returning `false` ends the watch.
type ConversationWatchListener = dyn Fn(ConversationFrame) -> bool + Send + Sync + 'static;

/// The view observer that queues exact frames for [`ConversationWatch`].
struct ConversationWatchObserver {
    current: Mutex<Option<Arc<ConversationView>>>,
    pending: Mutex<Vec<ConversationFrame>>,
    ended: Mutex<Option<WatchEnd>>,
    listener: Mutex<Option<Box<ConversationWatchListener>>>,
    /// Releases the mount registration when the watch ends itself (a listener
    /// returning `false`, close, or cancellation). Bound after `attach`.
    release: Mutex<Option<Detach>>,
    /// Single-flight delivery guard: only one thread runs the listener, so a
    /// reentrant `start`/`advance` queues work for the active drain instead of
    /// running a second delivery loop over the same frame.
    draining: AtomicBool,
}

impl ConversationWatchObserver {
    fn new(value: &ConversationView) -> Self {
        Self {
            current: Mutex::new(Some(Arc::new(value.clone()))),
            pending: Mutex::new(Vec::new()),
            ended: Mutex::new(None),
            listener: Mutex::new(None),
            release: Mutex::new(None),
            draining: AtomicBool::new(false),
        }
    }

    fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
        mutex
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn start(&self, listener: Box<ConversationWatchListener>) {
        {
            let mut slot = Self::lock(&self.listener);
            if Self::lock(&self.ended).is_some() {
                return;
            }
            *slot = Some(listener);
        }
        self.drain();
    }

    fn peek_frame(&self) -> Option<ConversationFrame> {
        Self::lock(&self.pending).first().cloned()
    }

    fn take_peeked(&self) {
        let mut pending = Self::lock(&self.pending);
        if !pending.is_empty() {
            pending.remove(0);
        }
    }

    fn next_frame(&self) -> Option<ConversationFrame> {
        let mut pending = Self::lock(&self.pending);
        if pending.is_empty() {
            None
        } else {
            Some(pending.remove(0))
        }
    }

    fn end(&self, reason: WatchEnd) {
        {
            let mut ended = Self::lock(&self.ended);
            if ended.is_some() {
                return;
            }
            *ended = Some(reason);
        }
        Self::lock(&self.pending).clear();
        // Released outside every other lock: detaching re-enters the view's
        // shared state and the mount.
        let release = Self::lock(&self.release).take();
        if let Some(release) = release {
            release.detach();
        }
    }

    /// Bind the mount registration this observer releases when it ends.
    fn bind_release(&self, release: Detach) {
        if Self::lock(&self.ended).is_some() {
            release.detach();
            return;
        }
        let mut slot = Self::lock(&self.release);
        if Self::lock(&self.ended).is_some() {
            drop(slot);
            release.detach();
            return;
        }
        *slot = Some(release);
    }

    fn ended(&self) -> Option<WatchEnd> {
        Self::lock(&self.ended).clone()
    }

    /// Deliver pending frames to the listener, one at a time.
    ///
    /// The listener is temporarily taken out of its slot while it runs, so a
    /// callback may call `start` (replace the listener), `stop`, or advance the
    /// view without deadlocking on the slot lock. `draining` keeps delivery
    /// single-flight: a reentrant call only leaves its frame queued for the
    /// active loop, so the same frame is never delivered twice.
    fn drain(&self) {
        if self.draining.swap(true, Ordering::AcqRel) {
            return;
        }
        loop {
            if self.ended().is_some() {
                break;
            }
            let Some(frame) = self.peek_frame() else {
                break;
            };
            let listener = Self::lock(&self.listener).take();
            let Some(listener) = listener else {
                break;
            };
            let keep_going = listener(frame);
            // A reentrant `start` may have installed a replacement; only
            // restore the old listener when the slot is still empty. After an
            // `end` the listener is dropped instead of being restored.
            if self.ended().is_none() {
                let mut slot = Self::lock(&self.listener);
                if slot.is_none() {
                    *slot = Some(listener);
                }
            }
            self.take_peeked();
            if !keep_going {
                self.end(WatchEnd::Stopped);
                break;
            }
        }
        self.draining.store(false, Ordering::Release);
    }
}

impl ViewObserver for ConversationWatchObserver {
    fn advance(&self, value: &ConversationView, ops: &[Op], context: &Context) {
        if Self::lock(&self.ended).is_some() {
            return;
        }
        let value = Arc::new(value.clone());
        {
            let mut pending = Self::lock(&self.pending);
            if pending.len() >= MAX_PENDING_WATCH_FRAMES {
                // Collapse the undelivered suffix into one root replacement
                // built from this newest revision.
                let root = serde_json::json!({
                    "conversation": &value.conversation,
                    "entries": value.entries.as_ref(),
                    "docs": value.docs.as_ref(),
                });
                pending.clear();
                pending.push(ConversationFrame {
                    value: value.clone(),
                    ops: vec![Op::Replace(root)],
                    context: context.clone(),
                });
            } else {
                pending.push(ConversationFrame {
                    value: value.clone(),
                    ops: ops.to_vec(),
                    context: context.clone(),
                });
            }
        }
        *Self::lock(&self.current) = Some(value);
        self.drain();
    }

    fn close_session(&self, failure: Option<&StorageError>) {
        self.end(match failure {
            Some(error) => WatchEnd::SessionFailed(error.clone()),
            None => WatchEnd::SessionClosed,
        });
    }
}

/// A serialized exact-frame watch of one conversation (upstream
/// `ConversationWatch`).
///
/// Dropping the last handle stops the watch and releases its mount
/// registration.
pub struct ConversationWatch {
    observer: Arc<ConversationWatchObserver>,
    detach: Detach,
    stopped: AtomicBool,
    /// The task awaiting this watch's acquisition-context cancellation, when
    /// one was installed.
    cancel: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Drop for ConversationWatch {
    fn drop(&mut self) {
        self.stop();
    }
}

impl ConversationWatch {
    fn new(
        attached: AttachedView<ConversationWatchObserver>,
        context: &Context,
    ) -> Result<Self, HarnessError> {
        let observer = attached.observer;
        let detach = attached.detach;
        observer.bind_release(detach.clone());
        let handle = Self {
            observer,
            detach,
            stopped: AtomicBool::new(false),
            cancel: Mutex::new(None),
        };
        if context.is_aborted() {
            handle.stop();
            return Err(HarnessError::Invalid(
                "The watch context was aborted".to_owned(),
            ));
        }
        if let Some(signal) = context.signal().cloned() {
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                let observer = Arc::downgrade(&handle.observer);
                let task = runtime.spawn(async move {
                    signal.cancelled().await;
                    if let Some(observer) = observer.upgrade() {
                        observer.end(WatchEnd::Cancelled);
                    }
                });
                *handle
                    .cancel
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(task);
            }
        }
        Ok(handle)
    }

    /// The newest revision.
    #[must_use]
    pub fn value(&self) -> Option<Arc<ConversationView>> {
        ConversationWatchObserver::lock(&self.observer.current).clone()
    }

    /// Why the watch ended, if it did.
    #[must_use]
    pub fn ended(&self) -> Option<WatchEnd> {
        self.observer.ended()
    }

    /// Whether the watch was stopped or ended.
    #[must_use]
    pub fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire) || self.observer.ended().is_some()
    }

    /// Install the frame listener (upstream `WatchHandle.start`). Returning
    /// `false` ends the watch.
    pub fn start(&self, listener: impl Fn(ConversationFrame) -> bool + Send + Sync + 'static) {
        self.observer.start(Box::new(listener));
    }

    /// The next undelivered frame, if any.
    pub fn next_frame(&self) -> Option<ConversationFrame> {
        self.observer.next_frame()
    }

    /// Stop the watch. Idempotent.
    pub fn stop(&self) {
        if self.stopped.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Some(task) = self
            .cancel
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            task.abort();
        }
        self.observer.end(WatchEnd::Stopped);
        self.detach.detach();
    }
}

// ─── Harness ────────────────────────────────────────────────────────────────

/// One durable session, its scheduler, and the conversation conveniences.
///
/// Cheap to clone: every field is shared behind one `Arc`.
#[derive(Clone)]
pub struct Harness {
    inner: Arc<Inner>,
}

struct Inner {
    session: Session,
    scheduler: TaskScheduler,
    task_graph: TaskGraphView,
    /// The conversation view mounts behind `Conversation::watch`.
    views: ConversationViews,
    registry: Arc<dyn RegistryReader>,
    settings: Arc<dyn Fn() -> Settings + Send + Sync>,
    now: Arc<dyn Fn() -> i64 + Send + Sync>,
    on_report: Option<ReportHandler>,
    telemetry: HarnessTelemetry,
    closed: AtomicBool,
    /// The root conversation, from the reserved id or the one the storage
    /// already had.
    root: tokio::sync::Mutex<Option<ConversationId>>,
    /// Closed when this `Inner` drops; watch delivery tasks select on it so a
    /// parked task cannot outlive the Harness.
    watch_liveness: tokio::sync::watch::Sender<()>,
    /// Parked `Submission::wait` calls, settled from the session's internal
    /// listener.
    submission_waiters: Arc<SubmissionWaiters>,
}

impl std::fmt::Debug for Harness {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Harness")
            .field("scheduling", &self.inner.scheduler.scheduling_state())
            .field("closed", &self.closed())
            .finish_non_exhaustive()
    }
}

impl Harness {
    /// Open a harness over `storage` (upstream `Harness.open`).
    ///
    /// Fails when the registry lacks one of the built-in task definitions, and
    /// closes the freshly opened session when the scheduler's open fails — the
    /// caller's context is dropped for that close, exactly upstream.
    pub async fn open(
        storage: Arc<dyn Storage>,
        options: HarnessOptions,
        context: Context,
    ) -> Result<Self, HarnessError> {
        if context.is_aborted() {
            return Err(HarnessError::Invalid(
                "The open context was aborted".to_owned(),
            ));
        }
        let registry = options
            .registry
            .clone()
            .unwrap_or_else(|| Arc::new(crate::durable::harness::registry::create_registry()));
        let snapshot = registry.snapshot();
        let builtin = builtin_tasks();
        let missing: Vec<&str> = builtin
            .iter()
            .filter(|task| snapshot.task(&task.definition.kind).is_none())
            .map(|task| task.definition.kind.as_str())
            .collect();
        if !missing.is_empty() {
            return Err(HarnessError::Invalid(format!(
                "Registry lacks built-in tasks {}; create it with create_registry()",
                missing.join(", ")
            )));
        }

        let now: Arc<dyn Fn() -> i64 + Send + Sync> =
            options.now.clone().unwrap_or_else(|| Arc::new(system_now));
        let harness_settings = options.settings.clone();
        let settings: Arc<dyn Fn() -> Settings + Send + Sync> =
            Arc::new(move || match &harness_settings {
                Some(provider) => resolve_settings(&provider()),
                None => Settings::default(),
            });
        let resolved_at_open = settings();
        // `settings.document_retention_ms` is an `f64` so upstream's
        // `Infinity` ("keep documents") is expressible; it must survive the
        // conversion to the cache's millisecond `i64` (the old
        // `is_finite().then_some(..)` turned Infinity into `None`, i.e. the
        // ten-minute default).
        let document_retention_ms = options.document_retention_ms.or_else(|| {
            crate::durable::session::documents::document_retention_millis(
                resolved_at_open.document_retention_ms,
            )
        });

        let conversation_created: Arc<ConversationCreatedHook> =
            Arc::new(stage_builtin_conversation_documents);
        let session_options = SessionOptions {
            now: Some(now.clone()),
            document_retention_ms,
            persist_within_ms: None,
            behaviours: Some(Arc::new(BuiltinBehaviours)),
            // The line records committed-entry and settled-submission events
            // through the same handle the harness emits through.
            telemetry: options.telemetry.clone(),
            // The built-in documents are staged by the same commit that
            // creates or forks the conversation (upstream
            // `conversationCreated`), so no crash window exists between the
            // conversation record and its documents.
            conversation_created: Some(conversation_created),
        };

        let report_scheduler: crate::durable::harness::scheduler::ReportFn = {
            let on_report = options.on_report.clone();
            Arc::new(move |message: &str, info: ReportInfo| {
                if let Some(handler) = &on_report {
                    // A throwing handler is dropped, never fatal (upstream
                    // `contained`).
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        handler(message, &info);
                    }));
                }
            })
        };

        let scheduler_options = TaskSchedulerOptions {
            now: Some(now.clone()),
            settings: Some(settings.clone()),
            timers: options.timers.clone(),
            on_wake: options.on_wake.clone(),
            report: Some(report_scheduler),
            session: Some(session_options),
        };
        let scheduler = TaskScheduler::open_with(scheduler_options, storage.clone(), Vec::new())
            .await
            .map_err(HarnessError::Storage)?;
        // Executable definitions arrive after open: the scheduler's own open
        // only needs the storage, settings and catalogue, and a host may install
        // more through `scheduler().catalogue()` later. Like upstream, open runs
        // recovery (`openTasks`) but does **not** dispatch: `resume()` does.
        if !options.tasks.is_empty() {
            scheduler.catalogue().install(options.tasks.clone());
        }

        let session = scheduler.session().clone();
        let task_graph = match TaskGraphView::open_with_report(
            session.clone(),
            Arc::new(scheduler.clone()) as Arc<dyn TaskGraphBlocking>,
            Arc::new(|_: &str| {}) as Report,
        )
        .await
        {
            Ok(view) => view,
            Err(error) => {
                let _ = scheduler.close().await;
                return Err(HarnessError::Storage(error));
            }
        };
        // The conversation view mount backs `Conversation::watch`. Its
        // listener failures follow the same report policy as the harness.
        let views_report: Report = {
            let on_report = options.on_report.clone();
            let telemetry = options
                .telemetry
                .clone()
                .unwrap_or_else(HarnessTelemetry::none);
            Arc::new(move |message: &str| {
                if let Some(handler) = &on_report {
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        handler(message, &ReportInfo::ListenerError);
                    }));
                }
                telemetry.record_report(None, "listener_error", None, None, None);
            })
        };
        let views = match ConversationViews::open_with_report(session.clone(), views_report).await {
            Ok(views) => views,
            Err(error) => {
                let _ = scheduler.close().await;
                return Err(HarnessError::Storage(error));
            }
        };

        let submission_waiters = Arc::new(SubmissionWaiters::default());
        let (watch_liveness, _) = tokio::sync::watch::channel(());
        let harness = Self {
            inner: Arc::new(Inner {
                session,
                scheduler,
                task_graph,
                views,
                registry,
                settings,
                now,
                on_report: options.on_report,
                telemetry: options.telemetry.unwrap_or_else(HarnessTelemetry::none),
                closed: AtomicBool::new(false),
                root: tokio::sync::Mutex::new(None),
                watch_liveness,
                submission_waiters: submission_waiters.clone(),
            }),
        };

        // Settle submission waiters from the session's internal listener: it
        // runs on the mutation line, before host listeners, and only sends on
        // a oneshot (no host code runs there).
        {
            let waiters = submission_waiters.clone();
            let listener: crate::durable::session::publication::InternalListener =
                Arc::new(move |publication, _context| {
                    for change in &publication.changes {
                        let CommitChange::Table(TableCommitChange::Submission(record)) = change
                        else {
                            continue;
                        };
                        if matches!(
                            record.status,
                            SubmissionStatus::Done | SubmissionStatus::Unanswered
                        ) {
                            waiters.resolve(record.clone());
                        }
                    }
                });
            if let Err(error) = harness
                .inner
                .session
                .handle()
                .maintain(move |inner| {
                    inner.internal_listeners.add(listener);
                })
                .await
            {
                let _ = harness.close().await;
                return Err(HarnessError::Storage(error));
            }
        }
        // A close rejects every parked wait with a dropped sender.
        {
            let waiters = submission_waiters.clone();
            if let Err(error) = harness
                .inner
                .session
                .handle()
                .maintain(move |inner| {
                    inner
                        .close_listeners
                        .push(Arc::new(move || waiters.close()));
                })
                .await
            {
                let _ = harness.close().await;
                return Err(HarnessError::Storage(error));
            }
        }
        Ok(harness)
    }

    /// The session the harness and its scheduler share.
    #[must_use]
    pub fn session(&self) -> &Session {
        &self.inner.session
    }

    /// The scheduler.
    #[must_use]
    pub fn scheduler(&self) -> &TaskScheduler {
        &self.inner.scheduler
    }

    /// The live registry this harness was opened with.
    #[must_use]
    pub fn registry(&self) -> &Arc<dyn RegistryReader> {
        &self.inner.registry
    }

    /// Resolved settings, read at each call.
    #[must_use]
    pub fn settings(&self) -> Settings {
        (self.inner.settings)()
    }

    /// The harness clock in epoch milliseconds.
    #[must_use]
    pub fn now(&self) -> i64 {
        (self.inner.now)()
    }

    /// The telemetry handle configured at open.
    ///
    /// Returns [`HarnessTelemetry::none`] when
    /// [`HarnessOptions::telemetry`] was absent. This module emits through the
    /// handle in [`Harness::submit`] and [`Harness::report`].
    #[must_use]
    pub fn telemetry(&self) -> HarnessTelemetry {
        self.inner.telemetry.clone()
    }

    // ─── Lifecycle ──────────────────────────────────────────────────────────

    /// Whether close began or the session failed.
    ///
    /// Upstream `closed` is a future settling with the session's end; see the
    /// module docs (deviation 3).
    #[must_use]
    pub fn closed(&self) -> bool {
        self.inner.closed.load(Ordering::Acquire)
            || self.inner.session.is_failed()
            || self.inner.session.handle().is_closing()
    }

    /// Enable task scheduling (spec §5.6). Idempotent; a no-op after close.
    pub fn resume(&self) {
        if !self.closed() {
            self.inner.scheduler.resume();
        }
    }

    /// When this session next needs a running process.
    ///
    /// Answers while scheduling is paused; a host arms its own wakeup from it
    /// and calls [`Harness::resume`] when it fires (upstream `onWake`).
    #[must_use]
    pub fn wake(&self) -> Wake {
        self.inner.scheduler.wake()
    }

    /// Signal and join task invocations, then close storage.
    ///
    /// Idempotent: a second close joins the first through the session.
    pub async fn close(&self) -> Result<(), HarnessError> {
        self.inner.closed.store(true, Ordering::Release);
        self.inner.scheduler.close().await?;
        Ok(())
    }

    // ─── Conversations ──────────────────────────────────────────────────────

    /// The reserved root conversation, created when absent (upstream
    /// `Harness.root`).
    ///
    /// The root always uses [`ROOT_CONVERSATION_ID`]; a storage that already
    /// has that conversation (for example an imported session) returns
    /// exactly that one.
    pub async fn root(&self, context: Context) -> Result<Conversation, HarnessError> {
        self.assert_usable()?;
        // One root per harness: the check and the creation share the lock, so
        // concurrent callers still create exactly one conversation. The
        // session's own commit rechecks committed state, so two harnesses over
        // one storage also agree.
        let mut root = self.inner.root.lock().await;
        if let Some(id) = *root {
            return Ok(self.conversation(id));
        }
        let record = self.inner.session.create_root_conversation(context).await?;
        debug_assert_eq!(
            record.id, ROOT_CONVERSATION_ID,
            "the root conversation uses the reserved id"
        );
        *root = Some(record.id);
        Ok(self.conversation(record.id))
    }

    /// Create an independent conversation (upstream `createConversation`).
    ///
    /// The conversation record and its five built-in documents are one commit:
    /// the session's conversation-created hook stages them before the
    /// creating transaction settles.
    pub async fn create_conversation(
        &self,
        ownership: ConversationOwnership,
        context: Context,
    ) -> Result<Conversation, HarnessError> {
        self.assert_usable()?;
        let record = self
            .inner
            .session
            .create_conversation(ownership, None, context)
            .await?;
        Ok(self.conversation(record.id))
    }

    /// Fork `parent` at `at` (upstream `Conversation.fork`).
    ///
    /// A fork copies every `asOf`/`current` document and starts `initial`
    /// ones empty in the creating commit; the conversation-created hook stages
    /// the child's own built-in documents in that same commit.
    pub async fn fork(
        &self,
        parent: ConversationId,
        at: EntryId,
        ownership: ConversationOwnership,
        context: Context,
    ) -> Result<Conversation, HarnessError> {
        self.assert_usable()?;
        let record = self
            .inner
            .session
            .create_conversation(ownership, Some((parent, at)), context)
            .await?;
        Ok(self.conversation(record.id))
    }

    /// A handle for an existing conversation.
    #[must_use]
    pub fn conversation(&self, id: ConversationId) -> Conversation {
        Conversation {
            harness: self.clone(),
            id,
        }
    }

    /// The conversation record, when it exists.
    pub async fn conversation_record(
        &self,
        id: ConversationId,
        context: &Context,
    ) -> Result<Option<ConversationRecord>, HarnessError> {
        self.assert_usable()?;
        Ok(self.inner.session.conversation(id, context).await?)
    }

    /// Resolve one conversation's agent (upstream `resolveAgent`).
    pub async fn agent(
        &self,
        id: ConversationId,
        snapshot: Option<&RegistrySnapshot>,
        context: &Context,
    ) -> Result<Agent, HarnessError> {
        self.assert_usable()?;
        let registry = snapshot
            .cloned()
            .unwrap_or_else(|| self.inner.registry.snapshot());
        let state = self
            .inner
            .session
            .snapshot(&agent_address(id), context)
            .await?
            .map(|value| decode_agent(&value))
            .unwrap_or_default();
        let settings = self.settings();
        Ok(resolve_agent(&registry, &settings, &state))
    }

    // ─── Submissions ────────────────────────────────────────────────────────

    /// Admit one submission (upstream `admitSubmission`).
    ///
    /// One commit appends the entry, mutates `pi.inbox`, creates the
    /// submission and starts the run when the conversation is idle. A known
    /// `requestId` returns the existing submission without writing; a
    /// `whenBusy: "reject"` input returns [`HarnessError::Invalid`] with the
    /// upstream busy message. The returned value is the committed record;
    /// [`Harness::submit_handle`] returns the awaitable/abortable handle.
    ///
    /// When telemetry is installed, the admission commit runs inside the
    /// `pi.durable.submit` span. A terminal submission's
    /// `pi.durable.submission` event is emitted once by the session's commit
    /// publication.
    pub async fn submit(
        &self,
        conversation: ConversationId,
        draft: SubmissionDraft,
        context: Context,
    ) -> Result<SubmissionRecord, HarnessError> {
        let (record, _) = self.submit_admitted(conversation, draft, context).await?;
        Ok(record)
    }

    /// Admit one submission and return its handle (upstream
    /// `Submissions.submit`).
    ///
    /// The same admission as [`Harness::submit`]; the handle adds
    /// [`Submission::wait`] and [`Submission::abort`].
    pub async fn submit_handle(
        &self,
        conversation: ConversationId,
        draft: SubmissionDraft,
        context: Context,
    ) -> Result<Submission, HarnessError> {
        let (record, _) = self.submit_admitted(conversation, draft, context).await?;
        Ok(Submission::new(self.clone(), record.id))
    }

    /// The admission body shared by [`Harness::submit`] and
    /// [`Harness::submit_handle`].
    async fn submit_admitted(
        &self,
        conversation: ConversationId,
        draft: SubmissionDraft,
        context: Context,
    ) -> Result<(SubmissionRecord, Admission), HarnessError> {
        self.assert_usable()?;
        // Upstream `Submissions.submit` resumes before admitting: a submission
        // that starts a run must not sit behind a paused scheduler.
        self.inner.scheduler.resume();
        let telemetry = &self.inner.telemetry;
        let (span, context) = submit_span(telemetry, context, &draft, conversation);
        let now = self.now();
        let queue_modes = QueueModes::from_settings(&self.settings());
        let mut slot = Some(draft);
        let scope = TransactionScope {
            conversation_id: Some(conversation),
            ..TransactionScope::default()
        };
        let admitted: Admitted = match self
            .inner
            .session
            .commit_scoped(scope, context.clone(), move |tx, state| {
                Box::pin(async move {
                    let draft = slot.take().ok_or_else(|| {
                        StorageError::invalid("the submission draft was consumed")
                    })?;
                    let admitted =
                        admit_submission_in(state, tx, conversation, draft, now, queue_modes)
                            .await?;
                    Ok(Box::new(admitted) as JobValue)
                })
            })
            .await
        {
            Ok(admitted) => admitted,
            Err(error) => {
                telemetry.end_submit_span(span, None, None, SpanStatus::error_only());
                return Err(HarnessError::Storage(error));
            }
        };
        let Some(id) = admitted.id else {
            telemetry.end_submit_span(span, None, None, SpanStatus::error_only());
            let Admission::Rejected(busy) = admitted.admission else {
                return Err(HarnessError::Invalid(
                    "the submission was rejected without a reason".to_owned(),
                ));
            };
            return Err(HarnessError::Invalid(busy.to_string()));
        };
        let record = match self.inner.session.submission(id, &context).await {
            Ok(Some(record)) => record,
            Ok(None) => {
                telemetry.end_submit_span(span, Some(id.as_i64()), None, SpanStatus::error_only());
                return Err(HarnessError::Invalid(format!(
                    "Submission {id} was not committed"
                )));
            }
            Err(error) => {
                telemetry.end_submit_span(span, Some(id.as_i64()), None, SpanStatus::error_only());
                return Err(HarnessError::Storage(error));
            }
        };
        let result = admitted.admission.status().map(|status| match status {
            AdmissionStatus::Queued => "queued",
            AdmissionStatus::Placed => "placed",
            AdmissionStatus::Done => "done",
        });
        telemetry.end_submit_span(span, Some(id.as_i64()), result, SpanStatus::Ok);
        Ok((record, admitted.admission))
    }

    /// One committed submission record (upstream `submission`).
    ///
    /// [`Harness::submission_handle`] is the awaitable/abortable form; this
    /// record-returning method stays for compatibility.
    pub async fn submission(
        &self,
        id: SubmissionId,
        context: &Context,
    ) -> Result<Option<SubmissionRecord>, HarnessError> {
        self.assert_usable()?;
        Ok(self.inner.session.submission(id, context).await?)
    }

    /// A handle for an existing submission (upstream `Submissions.get`).
    pub async fn submission_handle(
        &self,
        id: SubmissionId,
        context: &Context,
    ) -> Result<Option<Submission>, HarnessError> {
        self.assert_usable()?;
        match self.inner.session.submission(id, context).await? {
            Some(_) => Ok(Some(Submission::new(self.clone(), id))),
            None => Ok(None),
        }
    }

    /// Withdraw a queued submission (upstream `abortSubmission`).
    ///
    /// Returns `None` when the submission does not exist, or when
    /// `conversation` is supplied and the submission belongs to another
    /// conversation (upstream's `not_found`). A queued submission is settled
    /// `unanswered` with reason `aborted` and its inbox item is removed in the
    /// same commit; placed and terminal submissions are reported unchanged.
    pub async fn abort_submission(
        &self,
        id: SubmissionId,
        context: Context,
        conversation: Option<ConversationId>,
    ) -> Result<Option<SubmissionAbortResult>, HarnessError> {
        self.assert_usable()?;
        let result: Option<SubmissionAbortResult> = self
            .inner
            .session
            .commit(context, move |tx, state| {
                Box::pin(async move {
                    let context = tx.context().clone();
                    let storage = state.storage.storage();
                    let Some(record) = state.storage.read(&context, storage.submission(id)).await?
                    else {
                        let value: JobValue = Box::new(None::<SubmissionAbortResult>);
                        return Ok(value);
                    };
                    if conversation.is_some_and(|owner| owner != record.conversation_id) {
                        let value: JobValue = Box::new(None::<SubmissionAbortResult>);
                        return Ok(value);
                    }
                    let result = match record.status {
                        SubmissionStatus::Queued => {
                            let (inbox_value, committed) = read_conversation_document_state(
                                state,
                                tx,
                                &context,
                                &INBOX_DEFINITION,
                                record.conversation_id,
                            )
                            .await?;
                            let mut inbox: InboxState =
                                serde_json::from_value(Value::Object(inbox_value))
                                    .unwrap_or_default();
                            inbox.items.retain(|item| item.id() != id);
                            let value = match serde_json::to_value(&inbox) {
                                Ok(Value::Object(object)) => object,
                                _ => JsonObject::new(),
                            };
                            stage_conversation_document_state(
                                tx,
                                record.conversation_id,
                                &INBOX_DEFINITION,
                                value,
                                committed,
                            );
                            tx.stage_submission_change(
                                id,
                                SubmissionChange::Unanswered {
                                    reason: "aborted".to_owned(),
                                    detail: None,
                                },
                            );
                            SubmissionAbortResult::Aborted
                        }
                        SubmissionStatus::Placed => SubmissionAbortResult::AlreadyPlaced,
                        SubmissionStatus::Done | SubmissionStatus::Unanswered => {
                            SubmissionAbortResult::Settled
                        }
                    };
                    let value: JobValue = Box::new(Some(result));
                    Ok(value)
                })
            })
            .await?;
        Ok(result)
    }

    /// Request a manual compaction of `conversation` (upstream
    /// `Conversation.compact`).
    ///
    /// The task and its `pi.live` status are staged in **one** commit
    /// ([`create_compaction_in_state`]), so a crash cannot leave a runnable
    /// task without its presentation. Like upstream, the call resumes the
    /// task registry first: a manual compaction is a host request, not a
    /// scheduler decision.
    pub async fn compact(
        &self,
        conversation: ConversationId,
        instructions: Option<String>,
        context: Context,
    ) -> Result<TaskId, HarnessError> {
        self.assert_usable()?;
        self.inner.scheduler.resume();
        let input = CompactionInput {
            reason: CompactionReason::Manual,
            instructions,
        };
        let mut slot = Some(input);
        let scope = TransactionScope {
            conversation_id: Some(conversation),
            ..TransactionScope::default()
        };
        let id = self
            .inner
            .session
            .commit_scoped(scope, context, move |tx, state| {
                Box::pin(async move {
                    let input = slot.take().ok_or_else(|| {
                        StorageError::invalid("the compaction input was consumed")
                    })?;
                    let id = create_compaction_in_state(state, tx, conversation, input, None)
                        .await
                        .map_err(StorageError::invalid)?;
                    Ok(Box::new(id) as JobValue)
                })
            })
            .await?;
        Ok(id)
    }

    /// Mark every live non-background task reachable from `conversation`
    /// (spec §5.4) and wait until it is idle.
    pub async fn abort_conversation(
        &self,
        conversation: ConversationId,
        background: bool,
    ) -> Result<(), HarnessError> {
        self.assert_usable()?;
        // Upstream `Conversation.abort` resumes before marking, so the abort
        // handlers can actually run when the scheduler was paused.
        self.inner.scheduler.resume();
        self.inner
            .scheduler
            .abort_conversation(conversation, background)
            .await?;
        Ok(())
    }

    /// Mark one task for abort (upstream `abortTask`).
    pub async fn abort_task(&self, id: TaskId) -> Result<AbortResult, HarnessError> {
        self.assert_usable()?;
        Ok(self.inner.scheduler.abort(id).await?)
    }

    /// Run a task parked by the restart limit again.
    pub async fn retry_task(&self, id: TaskId) -> Result<(), HarnessError> {
        self.assert_usable()?;
        Ok(self.inner.scheduler.retry_task(id).await?)
    }

    // ─── Inspection ─────────────────────────────────────────────────────────

    /// Live tasks and unsettled submissions. Writes nothing and runs no task
    /// code.
    pub async fn inspect(&self, context: &Context) -> Result<HarnessInspection, HarnessError> {
        self.assert_usable()?;
        let Inspection {
            scheduling, tasks, ..
        } = self.inner.scheduler.inspect().await?;
        let mut submissions = Vec::new();
        for status in [SubmissionStatus::Queued, SubmissionStatus::Placed] {
            let query = SubmissionQuery {
                status: Some(status),
                ..SubmissionQuery::default()
            };
            submissions.extend(self.scan_submissions(query, context).await?);
        }
        submissions.sort_by_key(|record| record.id.as_i64());
        Ok(HarnessInspection {
            scheduling,
            tasks,
            submissions,
        })
    }

    /// Every live task with its owner edge and owned conversations (spec §9.5).
    pub async fn task_graph(
        &self,
        context: &Context,
    ) -> Result<crate::durable::harness::task_graph::TaskGraphStateHandle, HarnessError> {
        self.assert_usable()?;
        Ok(self.inner.task_graph.state(context).await?)
    }

    /// The task graph as a serialized exact-frame watch.
    pub async fn watch_task_graph(
        &self,
        context: &Context,
    ) -> Result<crate::durable::harness::task_graph::TaskGraphWatch, HarnessError> {
        self.assert_usable()?;
        Ok(self.inner.task_graph.watch(context).await?)
    }

    /// Session total: every conversation's committed `pi.usage` summed.
    ///
    /// Each document is read at its own point; totals only grow.
    pub async fn usage(&self, context: &Context) -> Result<UsageState, HarnessError> {
        self.assert_usable()?;
        let conversations = self
            .scan_conversations(ConversationQuery::default(), context)
            .await?;
        let mut total = UsageState::default();
        for record in conversations {
            let resolved = usage_address(record.id);
            if let Some(value) = self.inner.session.snapshot(&resolved, context).await? {
                let state: UsageState =
                    serde_json::from_value(Value::Object(value)).unwrap_or_default();
                add_usage_state(&mut total, &state);
            }
        }
        Ok(total)
    }

    // ─── Documents ──────────────────────────────────────────────────────────

    /// The committed value at `resolved`, if the document is live.
    pub async fn snapshot(
        &self,
        resolved: &ResolvedAddress,
        context: &Context,
    ) -> Result<Option<JsonObject>, HarnessError> {
        self.assert_usable()?;
        Ok(self.inner.session.snapshot(resolved, context).await?)
    }

    /// Load, create, edit and stage a document in one commit.
    pub async fn doc<F>(
        &self,
        resolved: ResolvedAddress,
        definition: AnyDocDefinition,
        context: Context,
        edit: F,
    ) -> Result<(), HarnessError>
    where
        F: FnOnce(&mut JsonObject) + Send + 'static,
    {
        self.assert_usable()?;
        self.inner
            .session
            .doc(resolved, definition, context, edit)
            .await?;
        Ok(())
    }

    /// Attach a committed-state observer to one incarnation (spec §9).
    ///
    /// The observer is bound to exactly one incarnation: a retirement is
    /// terminal and a replacement at the same address is a different observer.
    /// Release it with [`Harness::detach`].
    ///
    /// Returns `None` when no incarnation is live at the address.
    pub async fn document_state(
        &self,
        resolved: &ResolvedAddress,
        definition: &AnyDocDefinition,
        context: &Context,
    ) -> Result<Option<Attached>, HarnessError> {
        self.assert_usable()?;
        let Some(loaded) = self.load_document(resolved, definition, context).await? else {
            return Ok(None);
        };
        let address_id = resolved.id.clone();
        let slot = Arc::new(std::sync::Mutex::new(None));
        let write = slot.clone();
        let loaded_for_line = loaded.clone();
        self.inner
            .session
            .handle()
            .maintain(move |inner| {
                let now = inner.now();
                inner.documents.insert(loaded_for_line.clone(), now);
                let attached = attach_state(inner, address_id, &loaded_for_line);
                *write
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(attached);
            })
            .await?;
        let attached = slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        Ok(attached)
    }

    /// Release a [`Harness::document_state`] observer and its pin.
    pub async fn detach(&self, attached: &Attached) -> Result<(), HarnessError> {
        let attached = attached.clone();
        self.inner
            .session
            .handle()
            .maintain(move |inner| detach(inner, &attached))
            .await?;
        Ok(())
    }

    /// Watch one document incarnation (spec §9).
    ///
    /// `listener` is invoked serially for the current revision and every later
    /// committed revision of this incarnation (and its retirement); returning
    /// `false` ends the watch. The current revision is delivered before this
    /// call returns. Later frames are only queued on the commit line and are
    /// drained by a delivery task off the line, in commit order, so a slow
    /// listener never stalls a commit. Delivery is buffered, so a listener that
    /// runs long sees the frame queue collapse past
    /// [`MAX_PENDING_WATCH_FRAMES`](crate::durable::session::observation::MAX_PENDING_WATCH_FRAMES)
    /// frames instead of growing without bound. The acquisition context's
    /// cancellation stops the watch when a Tokio runtime is available; outside
    /// one, callers must stop it explicitly.
    pub async fn watch_doc(
        &self,
        resolved: &ResolvedAddress,
        definition: &AnyDocDefinition,
        context: &Context,
        listener: impl Fn(&JsonObject) -> bool + Send + Sync + 'static,
    ) -> Result<Option<DocumentWatch>, HarnessError> {
        self.assert_usable()?;
        // Attach to committed state, so a deferred commit is stored first.
        self.inner.session.flush_deferred(context).await?;
        let address = resolved.address.clone();
        let definition = definition.clone();
        // Capture the cancellation signal before the context moves into the
        // line job. Upstream's `CommittedWatch.observeCancellation` stops a
        // watch when its acquisition context aborts.
        let cancellation = context.signal().cloned();
        let context = context.clone();
        let session = self.inner.session.clone();
        let owner = Arc::downgrade(&self.inner);
        let liveness = self.inner.watch_liveness.subscribe();
        // The delivery task outlives any one call, so it holds `Inner` weakly:
        // a strong handle here would form a cycle through the session's commit
        // subscribers. A listener panic is still reported while the harness is
        // alive.
        let watch_report: Report = {
            let inner = Arc::downgrade(&self.inner);
            Arc::new(move |message: &str| {
                if let Some(inner) = inner.upgrade() {
                    Harness { inner }.report(message, ReportInfo::ListenerError);
                }
            })
        };
        // One line job reads the current committed revision and registers the
        // commit subscription at the same point on the line. The line runs
        // jobs one at a time, so no commit can land between the read and the
        // registration: a commit published before this job is already in the
        // value read here, and every commit published after it reaches the
        // listener.
        let watch: Option<(CommittedWatch, u64, Arc<WatchDelivery>)> = self
            .inner
            .session
            .handle()
            .read_on_line(move |state| {
                Box::pin(async move {
                    let storage = state.storage.storage();
                    let record = state
                        .storage
                        .read(
                            &context,
                            storage.find_document(
                                address,
                                crate::durable::types::DocumentPoint::Current,
                            ),
                        )
                        .await?;
                    let Some(record) = record else {
                        let value: JobValue =
                            Box::new(None::<(CommittedWatch, u64, Arc<WatchDelivery>)>);
                        return Ok(value);
                    };
                    let Some(stored) = state
                        .storage
                        .read(
                            &context,
                            storage
                                .document(record.id, crate::durable::types::DocumentPoint::Current),
                        )
                        .await?
                    else {
                        return Err(StorageError::invalid(format!(
                            "Document {} cannot be read",
                            record.id
                        )));
                    };
                    let value =
                        definition.materialize_value(&record, stored.version, stored.value)?;
                    let initial = Some(value);
                    let version = definition.common.version;
                    let watch = CommittedWatch::new(initial.clone(), version);
                    // A watch starts with the revision it was built from:
                    // queue it as the first frame, versioned like the value it
                    // carries, so `start` delivers the current revision before
                    // any later commit.
                    watch
                        .core()
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .advance(initial, Vec::new(), version, context.clone());
                    let incarnation = record.id;
                    // The commit listener only queues frames and wakes the
                    // delivery loop; it never invokes host code on the line.
                    let delivery = Arc::new(WatchDelivery::new(
                        watch.clone(),
                        watch_report,
                        owner,
                        liveness,
                    ));
                    let advance = delivery.clone();
                    let subscription =
                        session.subscribe_commits(Arc::new(move |publication, context| {
                            advance_watch(&advance, publication, incarnation, context);
                        }))?;
                    let value: JobValue = Box::new(Some((watch, subscription, delivery)));
                    Ok(value)
                })
            })
            .await?;
        let Some((watch, subscription, delivery)) = watch else {
            return Ok(None);
        };
        watch.start(move |frame| match &frame.value {
            Some(value) => listener(value),
            None => false,
        });
        let stop = Arc::new(WatchStop {
            stopped: AtomicBool::new(false),
            session: self.inner.session.clone(),
            subscription,
            watch: watch.clone(),
            delivery: delivery.clone(),
            cancel: std::sync::Mutex::new(None),
        });
        // A close ends the watch; a failure ends it with the same reason every
        // other observer uses.
        let closing_stop = stop.clone();
        let session = self.inner.session.clone();
        if let Err(error) = self
            .inner
            .session
            .handle()
            .maintain(move |inner| {
                inner.close_listeners.push(Arc::new(move || {
                    closing_stop.stop_with(match session.failure() {
                        Some(error) => WatchEnd::SessionFailed(error),
                        None => WatchEnd::SessionClosed,
                    });
                }));
            })
            .await
        {
            // The close listener could not be installed (the session is
            // already closing); no watcher would ever stop this watch, so
            // release it here instead of leaking the subscription.
            stop.stop_with(match self.inner.session.failure() {
                Some(error) => WatchEnd::SessionFailed(error),
                None => WatchEnd::SessionClosed,
            });
            return Err(error.into());
        }
        // The listener is installed and `start` drained the initial prefix;
        // only now may the delivery task run, so it can never overlap that
        // synchronous drain.
        delivery.start();
        let handle = DocumentWatch {
            watch,
            delivery,
            stop,
            handles: Arc::new(AtomicUsize::new(1)),
        };
        // `Context` exposes its cancellation signal; install the same stop
        // upstream's `observeCancellation` performs. Without a Tokio runtime
        // there is no task to await the signal, so the caller must stop the
        // watch explicitly.
        if let Some(signal) = cancellation {
            if signal.is_aborted() {
                handle.stop.stop_with(WatchEnd::Cancelled);
            } else if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                let stop = Arc::downgrade(&handle.stop);
                let task = runtime.spawn(async move {
                    signal.cancelled().await;
                    if let Some(stop) = stop.upgrade() {
                        stop.stop_with(WatchEnd::Cancelled);
                    }
                });
                *handle
                    .stop
                    .cancel
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(task);
            }
        }
        Ok(Some(handle))
    }

    /// The provider session id of `conversation`, minting one when absent.
    ///
    /// Upstream `ensureProviderSessionId`; see
    /// [`ensure_provider_session_id`](super::provider::ensure_provider_session_id).
    pub async fn provider_session_id(
        &self,
        conversation: ConversationId,
        context: Context,
    ) -> Result<String, HarnessError> {
        self.assert_usable()?;
        Ok(
            ensure_provider_session_id(&self.inner.session, conversation, self.now(), context)
                .await?,
        )
    }

    // ─── Reporting ──────────────────────────────────────────────────────────

    /// Report a failure that does not fail the calling operation.
    ///
    /// The host handler runs first, as upstream does. Afterwards the report is
    /// recorded through [`HarnessTelemetry::record_report`] — the
    /// `pi.durable.report` event and the `pi.durable.reports` counter — which
    /// is a no-op when no sink is installed.
    pub fn report(&self, message: impl AsRef<str>, info: ReportInfo) {
        if let Some(handler) = &self.inner.on_report {
            let message = message.as_ref();
            // A throwing handler is dropped, never fatal (upstream `contained`).
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                handler(message, &info);
            }));
        }
        // Upstream's `safeReport` counts the report after handing it to the
        // host. The helper takes only static strings and an id, so the no-sink
        // path allocates nothing.
        let (report_type, reason, extension, task_id) = report_telemetry(&info);
        self.inner
            .telemetry
            .record_report(None, report_type, reason, extension, task_id);
    }

    /// The installed report handler, if any.
    #[must_use]
    pub fn on_report(&self) -> Option<&ReportHandler> {
        self.inner.on_report.as_ref()
    }

    // ─── Internals ──────────────────────────────────────────────────────────

    fn assert_usable(&self) -> Result<(), HarnessError> {
        if self.closed() {
            return Err(HarnessError::Closed);
        }
        Ok(())
    }

    /// Admit a `pi.reset` write (upstream `Conversation.reset`).
    async fn reset(
        &self,
        conversation: ConversationId,
        handoff: Option<String>,
        context: Context,
    ) -> Result<SubmissionRecord, HarnessError> {
        let mut entry = JsonObject::new();
        entry.insert(
            "kind".to_owned(),
            Value::String(crate::durable::entries::RESET_ENTRY.kind.to_owned()),
        );
        entry.insert("head".to_owned(), Value::String("self".to_owned()));
        if let Some(handoff) = handoff {
            entry.insert(
                "model".to_owned(),
                serde_json::json!([{
                    "role": "user",
                    "content": handoff,
                    "timestamp": self.now(),
                }]),
            );
        }
        self.submit(conversation, SubmissionDraft::write(entry), context)
            .await
    }

    /// One conversation's committed context (upstream `readContext`).
    ///
    /// Scans the visible entries once, ascending, and derives the view from
    /// them; `at` both bounds the scan and selects the newest head marker at or
    /// before it. An `at` that is not visible from this conversation is an
    /// error, as upstream's bounds capture rejects it.
    async fn read_context(
        &self,
        conversation: ConversationId,
        at: Option<EntryId>,
        context: &Context,
    ) -> Result<ContextView, HarnessError> {
        self.assert_usable()?;
        let query = EntryQuery {
            conversation_id: conversation,
            min_entry_id: None,
            max_entry_id: at,
            order: Some(ScanOrder::Ascending),
        };
        let mut entries = Vec::new();
        let mut cursor: Option<Cursor> = None;
        loop {
            let page = self
                .inner
                .session
                .scan_entries(query.clone(), SCAN_PAGE_SIZE, cursor, context)
                .await?;
            entries.extend(page.items);
            match page.next {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        if let Some(at) = at {
            if !entries.iter().any(|entry| entry.id == at) {
                return Err(HarnessError::Invalid(format!(
                    "Entry {at} is not visible from conversation {conversation}"
                )));
            }
        }
        Ok(derive_context(&entries, at))
    }

    /// Load one committed incarnation and materialize it at the definition's
    /// version, for the observer faces.
    async fn load_document(
        &self,
        resolved: &ResolvedAddress,
        definition: &AnyDocDefinition,
        context: &Context,
    ) -> Result<Option<crate::durable::session::documents::LoadedDocument>, HarnessError> {
        // A read observes committed state: a deferred commit is stored first.
        self.inner.session.flush_deferred(context).await?;
        let address = resolved.address.clone();
        let address_id = resolved.id.clone();
        let definition = definition.clone();
        let context = context.clone();
        let loaded = self
            .inner
            .session
            .handle()
            .read_on_line(move |state| {
                Box::pin(async move {
                    let storage = state.storage.storage();
                    let record = state
                        .storage
                        .read(
                            &context,
                            storage.find_document(
                                address,
                                crate::durable::types::DocumentPoint::Current,
                            ),
                        )
                        .await?;
                    let Some(record) = record else {
                        let value: JobValue =
                            Box::new(None::<crate::durable::session::documents::LoadedDocument>);
                        return Ok(value);
                    };
                    let Some(stored) = state
                        .storage
                        .read(
                            &context,
                            storage
                                .document(record.id, crate::durable::types::DocumentPoint::Current),
                        )
                        .await?
                    else {
                        return Err(StorageError::invalid(format!(
                            "Document {} cannot be read",
                            record.id
                        )));
                    };
                    let value =
                        definition.materialize_value(&record, stored.version, stored.value)?;
                    let loaded = crate::durable::session::documents::LoadedDocument {
                        address_id,
                        record,
                        stored_version: stored.version,
                        value_version: definition.common.version,
                        deltas_since_base: stored.deltas_since_base,
                        value,
                    };
                    let value: JobValue = Box::new(Some(loaded));
                    Ok(value)
                })
            })
            .await?;
        Ok(loaded)
    }

    async fn scan_submissions(
        &self,
        query: SubmissionQuery,
        context: &Context,
    ) -> Result<Vec<SubmissionRecord>, HarnessError> {
        let mut items = Vec::new();
        let mut cursor: Option<Cursor> = None;
        loop {
            let page: Page<SubmissionRecord> = self
                .inner
                .session
                .scan_submissions(query.clone(), SCAN_PAGE_SIZE, cursor, context)
                .await?;
            items.extend(page.items);
            match page.next {
                Some(next) => cursor = Some(next),
                None => return Ok(items),
            }
        }
    }

    async fn scan_conversations(
        &self,
        query: ConversationQuery,
        context: &Context,
    ) -> Result<Vec<ConversationRecord>, HarnessError> {
        let mut items = Vec::new();
        let mut cursor: Option<Cursor> = None;
        loop {
            let page: Page<ConversationRecord> = self
                .inner
                .session
                .scan_conversations(query.clone(), SCAN_PAGE_SIZE, cursor, context)
                .await?;
            items.extend(page.items);
            match page.next {
                Some(next) => cursor = Some(next),
                None => return Ok(items),
            }
        }
    }
}

// ─── Conversation creation hook ─────────────────────────────────────────────

/// Stage a new conversation's built-in documents inside its creating commit.
///
/// Port of upstream `HarnessImpl.conversationCreated`
/// (`harness/harness.ts:410-418`): every commit that creates or forks a
/// conversation also writes `pi.live`, `pi.inbox`, `pi.usage`, `pi.provider`
/// and (for a fresh conversation) `pi.agent`. Running inside the creating
/// transaction is what closes the crash window: a conversation without its
/// documents can never be committed.
///
/// A fork already copied `pi.agent` (`fork: asOf`); staging a second
/// incarnation at that address would leave two live documents there, so the
/// agent is skipped when `record.parent` is set. The other four are
/// `fork: initial`, which is exactly why they are created fresh here.
///
/// A **task-owned** conversation copies the committed `pi.agent` of the
/// owner task's conversation into its own fresh agent draft, so a subagent
/// inherits the parent's model, thinking level, extensions, tool filters,
/// instructions and working directory (upstream `createAgent`,
/// `harness/agent.ts:172-182`). The copy happens in the same creating commit,
/// so a crash can never leave the child with an empty agent.
fn stage_builtin_conversation_documents<'a>(
    tx: &'a mut Transaction,
    record: &'a ConversationRecord,
) -> ConversationCreatedFuture<'a> {
    Box::pin(async move {
        for (kind, definition) in conversation_definitions() {
            if record.parent.is_some() && kind == AGENT_KIND {
                continue;
            }
            let address = DocumentAddress {
                kind: kind.to_owned(),
                scope: DocumentScope::Conversation {
                    conversation_id: record.id,
                },
                key: None,
            };
            let id = address_id(&address);
            // Only `pi.agent` has an inherited seed; the other built-in
            // documents start from their definition's initial value.
            let value = if kind == AGENT_KIND {
                match agent_seed_for(tx, record).await? {
                    Some(seed) => seed,
                    None => definition.initial_value(),
                }
            } else {
                definition.initial_value()
            };
            tx.stage_document(StagedDocument {
                address,
                address_id: id,
                definition: Some(DocDefinition {
                    kind: definition.common.kind.clone(),
                    version: definition.common.version,
                    semantics: definition.common.semantics,
                }),
                family: None,
                value,
                created: true,
                loaded: None,
                retire: false,
            });
        }
        Ok(())
    })
}

/// The `pi.agent` value a new task-owned conversation inherits, or `None` when
/// the conversation has no owner (a root or independent conversation).
///
/// Read-your-writes first: an owner agent staged earlier in this same commit
/// wins; otherwise the owner conversation's committed value is read through
/// [`Transaction::committed_document`]. An owner without a live agent document
/// yields `None`, so the child starts from the definition's initial `{}`
/// instead of failing the creation.
async fn agent_seed_for(
    tx: &mut Transaction,
    record: &ConversationRecord,
) -> Result<Option<JsonObject>, StorageError> {
    let Some(owner) = &record.owner else {
        return Ok(None);
    };
    let address = agent_address(owner.conversation_id);
    if let Some(staged) = tx
        .latest_document(&address.id)
        .filter(|document| !document.retire)
    {
        return Ok(Some(staged.value.clone()));
    }
    tx.committed_document(address.address).await
}

// ─── Behaviour wiring ───────────────────────────────────────────────────────

/// The checkpoint predicates of the built-in conversation documents.
///
/// [`StagedDocument`] carries only the identity form of a definition, so
/// assembly resolves `checkpointWhen` through this trait. Every built-in
/// definition already declares its predicate; this maps a kind to it.
struct BuiltinBehaviours;

impl DocBehaviours for BuiltinBehaviours {
    fn checkpoint_when(&self, _address_id: &str, kind: &str) -> Option<DocCheckpointWhen> {
        match kind {
            AGENT_KIND => agent_definition().checkpoint_when,
            LIVE_KIND => crate::durable::harness::live::LIVE_DEFINITION.checkpoint_when,
            INBOX_KIND => crate::durable::harness::inbox::INBOX_DEFINITION.checkpoint_when,
            PROVIDER_KIND => {
                crate::durable::harness::documents::provider_definition().checkpoint_when
            }
            USAGE_KIND => crate::durable::harness::usage::USAGE_DEFINITION.checkpoint_when,
            _ => None,
        }
    }
}

// ─── Addresses and codecs ───────────────────────────────────────────────────

/// The `pi.agent` address of one conversation.
#[must_use]
pub fn agent_address(conversation: ConversationId) -> ResolvedAddress {
    conversation_address(AGENT_KIND, conversation)
}

/// The `pi.usage` address of one conversation.
#[must_use]
pub fn usage_address(conversation: ConversationId) -> ResolvedAddress {
    conversation_address(USAGE_KIND, conversation)
}

/// A conversation-owned, latest-only document address of `kind`.
///
/// The owner is always supplied here, so the address cannot be rejected for
/// its scope; the expect below names that invariant.
fn conversation_address(kind: &str, conversation: ConversationId) -> ResolvedAddress {
    ResolvedAddress::new(
        kind,
        DocumentSemantics::ConversationLatest {
            fork: DocumentFork::Initial,
        },
        Some(crate::durable::documents::DocOwner::Conversation(
            conversation,
        )),
        None,
    )
    .unwrap_or_else(|error| {
        panic!("conversation document {kind} needs a conversation owner: {error:?}")
    })
}

fn decode_agent(value: &JsonObject) -> AgentState {
    serde_json::from_value(Value::Object(value.clone())).unwrap_or_default()
}

fn encode_agent(state: &AgentState) -> JsonObject {
    match serde_json::to_value(state) {
        Ok(Value::Object(object)) => object,
        _ => JsonObject::new(),
    }
}

/// The telemetry description of a report: its fixed type, reason, extension
/// and task id.
///
/// The type and reason strings are the wire values declared by
/// [`REPORT_TYPES`](super::telemetry::REPORT_TYPES) and
/// [`REPORT_REASONS`](super::telemetry::REPORT_REASONS); matching here keeps the
/// no-sink path free of serialization and allocation.
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

/// Feed one committed publication into a document watch.
///
/// Runs on the session's mutation line, so it must stay cheap: it appends the
/// matching frames to the watch core and wakes the delivery loop. The listener
/// itself runs on that loop, never here.
fn advance_watch(
    delivery: &WatchDelivery,
    publication: &CommitPublication,
    incarnation: crate::durable::ids::DocumentId,
    context: &Context,
) {
    let mut queued = false;
    for change in &publication.changes {
        let crate::durable::session::publication::CommitChange::Document(
            crate::durable::types::DocumentCommitChange::Document {
                record,
                version,
                value,
                ops,
                ..
            },
        ) = change
        else {
            continue;
        };
        if record.id != incarnation {
            continue;
        }
        delivery
            .watch
            .core()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .advance(
                value.clone(),
                ops.clone(),
                version.unwrap_or(0),
                context.clone(),
            );
        queued = true;
    }
    if queued {
        delivery.wake();
    }
}

/// The system clock in epoch milliseconds.
fn system_now() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

/// Start `pi.durable.submit` for one admission and overlay its carrier on the
/// admission context.
///
/// Upstream's shape: an **input** starts a root trace and links to its caller;
/// a **write** is the caller's child. Both relations are empty when the caller
/// carries no extractable context, and every step is a no-op without a sink.
///
/// The rewritten context is what makes the carrier durable: the admission
/// commit writes `tx.context().trace_carrier()` onto every submission record it
/// creates, and the generation task started from those submission ids inherits
/// the first one as its [`TaskRecord::trace`], so its phase spans parent to
/// this admission.
fn submit_span(
    telemetry: &HarnessTelemetry,
    context: Context,
    draft: &SubmissionDraft,
    conversation: ConversationId,
) -> (SpanHandle, Context) {
    if !telemetry.enabled() {
        return (SpanHandle::NONE, context);
    }
    let caller = telemetry.extract(context.trace_carrier());
    let type_name = match draft.submission_type() {
        SubmissionType::Input => "input",
        SubmissionType::Write => "write",
    };
    let (parent, links) = match draft.submission_type() {
        SubmissionType::Input => {
            let links: Vec<SpanLink<'_>> = caller.map(SpanLink::new).into_iter().collect();
            (None, links)
        }
        SubmissionType::Write => (caller, Vec::new()),
    };
    let span = telemetry.start_submit_span(type_name, conversation.as_i64(), parent, &links);
    let context = match telemetry.inject(span) {
        Some(carrier) => context.with_trace_carrier(carrier).with_span_handle(span),
        None => context,
    };
    (span, context)
}
