//! The mutation line: the single owner task of one session's state.
//!
//! Port of upstream `session.ts`'s promise chain (`#tail`, `#enqueue`,
//! `#runCommit`) to Rust ownership. `SessionInner` is created by the line task
//! and never leaves it; callers hold a [`SessionHandle`] and send commands.
//!
//! # Why a task and not a lock
//!
//! Upstream's guarantees are structural facts about a single-threaded chain,
//! not conventions:
//!
//! * a commit callback's staging, preparation, storage settlement, adoption and
//!   publication all happen while the line is held;
//! * commit listeners run synchronously on the line;
//! * close seals admission before listeners run, and work admitted earlier
//!   still settles.
//!
//! A `Mutex<SessionInner>` held across `await` breaks the first guarantee as
//! soon as a listener (or a failure latch) re-enters, which upstream explicitly
//! forbids and Rust would deadlock on. An owner task makes re-entry impossible
//! instead: [`Transaction`] carries no handle, so a callback cannot send a
//! command even by mistake.
//!
//! # Pipeline (upstream `#runCommit`, `session.ts:716-768`)
//!
//! ```text
//! assert healthy
//!   -> [immediate commits] store what deferred commits hold
//!   -> callback(staged transaction)
//!   -> assert healthy again (a swallowed failed read must not commit)
//!   -> assemble + validate + prepare (every draft revoked here)
//!   -> [deferred + holdable] adopt in memory + publish + return
//!   -> storage.commit                          [admitted: cancellation is over]
//!   -> adopt (cache values and pointer bookkeeping)
//!   -> publish (internal listeners, then host listeners)
//!   -> sweep idle documents
//! ```
//!
//! A callback failure or an assembly failure aborts every draft and stages
//! nothing. A storage failure fails the session (the guard latches it) and the
//! transaction is discarded.
//!
//! # Deferred commits
//!
//! [`CommitPersistence::Deferred`] is the one opt-in exception to "every
//! visible progress is durable": a holdable commit is published before storage
//! sees it and kept in [`SessionInner::holds`] until the next document read, an
//! immediate commit, close, or its deadline. The default is
//! [`CommitPersistence::Immediate`], so nothing that does not ask for it can be
//! lost in a crash.

use std::any::Any;
use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use futures_util::FutureExt;
use tokio::sync::{mpsc, oneshot, watch, Notify, OwnedSemaphorePermit, Semaphore};

use crate::durable::context::Context;
use crate::durable::errors::{HostCallback, ReportInfo, StorageError};
use crate::durable::harness::telemetry::{HarnessTelemetry, SpanHandle};
use crate::durable::ids::{DocumentId, Seq};
use crate::durable::session::assemble::{
    adopt, assemble_with, DocBehaviours, DocumentCreateOrRecord, DocumentPlan, NoDocBehaviours,
};
use crate::durable::session::attach::{deliver, Attached};
use crate::durable::session::documents::{DocumentCache, LoadedDocument};
use crate::durable::session::guard::{
    FailureLatch, FailureLatchCore, GuardedStorage, SessionReport,
};
use crate::durable::session::publication::{
    run_contained, CommitChange, CommitPublication, ListenerSet, TableCommitChange,
};
use crate::durable::session::transaction::{Transaction, TransactionScope};
use crate::durable::timers::{TimerCallback, TimerHandle, Timers};
use crate::durable::types::{
    DocumentCommitChange, DocumentContent, DocumentFork, DocumentHistory, DocumentRecord,
    JsonObject, Storage, StorageWrite, SubmissionStatus,
};

/// How many ordinary line commands may be in flight at once.
///
/// The command channel stays unbounded so that `Shutdown` can always be
/// enqueued (close must never wait on producers) and so the timer callback,
/// which cannot await, can post its bookkeeping sweep. Backpressure comes from
/// this permit count instead: a caller takes one permit before sending and the
/// permit travels with the job, so it is released only when the line settles
/// the job — not when a caller stops waiting. 1024 leaves room for a wide
/// fan-out of concurrent writers while keeping the retained command memory
/// bounded (one boxed job plus one oneshot per slot).
const LINE_CAPACITY: usize = 1024;

/// A value a line job returns to its caller.
pub type JobValue = Box<dyn Any + Send>;

/// The future a before-close hook returns.
pub type BeforeCloseFuture = std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>;

/// A hook run **once** after close sealed admission and every admitted commit
/// settled, but before the line closes storage (upstream `beforeClose`).
///
/// The hook runs off the line, so it may call back into the session — in
/// particular [`SessionHandle::write_at_close`], which is exactly what upstream
/// does for close-time bookkeeping (the harness writes the tasks its
/// invocations ran back as `pending`). A hook that panics is contained and
/// reported like a close listener; it never aborts the close.
pub type BeforeCloseHook = Box<dyn FnOnce() -> BeforeCloseFuture + Send + Sync>;

/// The hooks registered for one session's close.
///
/// Shared between the line (where they are registered through `maintain`) and
/// the handle (which takes and runs them once, off the line, between sealing
/// admission and closing storage).
pub type BeforeCloseSet = Arc<Mutex<Vec<BeforeCloseHook>>>;

/// The reply payload of one line job.
pub type JobReply = Result<JobValue, StorageError>;

/// The boxed future a commit callback returns.
pub type CommitFuture<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = JobReply> + Send + 'a>>;

/// A commit callback: reads and stages through the transaction, returns a value.
pub type CommitFn =
    dyn for<'a> FnOnce(&'a mut Transaction, &'a SessionState) -> CommitFuture<'a> + Send + 'static;

/// A listener called synchronously when close begins (upstream
/// `subscribeClose`). It must not block or call session APIs; a panic is
/// reported and never prevents the close.
pub type CloseListener = Arc<dyn Fn() + Send + Sync + 'static>;

/// The default deadline of a deferred commit (upstream
/// `settings.progress.persistWithinMs`): 1,000 ms.
pub const DEFAULT_PERSIST_WITHIN_MS: u64 = 1000;

/// How long a deferred commit may stay in memory before it is stored.
///
/// Upstream's `withinMs` is `number | Infinity`; the three Rust cases are
/// named, and [`DeferDeadline::Default`] takes the session's
/// `SessionOptions::persist_within_ms`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DeferDeadline {
    /// The session's `persist_within_ms` (1,000 ms unless configured).
    #[default]
    Default,
    /// Store after this many milliseconds.
    After(u64),
    /// Never store on a deadline: the next read, immediate commit or close
    /// stores the held changes (upstream `withinMs: Infinity`).
    Never,
}

/// When a commit reaches storage (upstream `CommitPersistence`).
///
/// `immediate` (the default) stores before `commit()` resolves. `deferred`
/// adopts a commit in memory, shows it to readers, watchers and listeners, and
/// resolves, but stores it later when every write of the commit changes an
/// existing conversation document declared `history: "latest"`,
/// `fork: "initial"` at its stored version (no creation, migration, copy or
/// retirement). A commit that writes anything else is stored immediately, as
/// it always was.
///
/// A crash loses whatever a deferred commit still holds, so only state whose
/// loss is acceptable — streamed progress, for example — should be deferred.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CommitPersistence {
    /// Store before the commit resolves.
    #[default]
    Immediate,
    /// Adopt in memory first; store later (see the enum docs).
    Deferred {
        /// How long the commit may stay in memory.
        within_ms: DeferDeadline,
    },
}

impl CommitPersistence {
    /// A deferred commit at the session's default deadline.
    #[must_use]
    pub fn deferred() -> Self {
        Self::Deferred {
            within_ms: DeferDeadline::Default,
        }
    }

    /// Whether this commit is stored before it resolves.
    #[must_use]
    pub fn is_immediate(&self) -> bool {
        matches!(self, Self::Immediate)
    }
}

/// Options of one session commit (upstream `CommitOptions`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CommitOptions {
    /// When the commit reaches storage.
    pub persistence: CommitPersistence,
}

impl CommitOptions {
    /// Options for an immediate commit.
    #[must_use]
    pub fn immediate() -> Self {
        Self {
            persistence: CommitPersistence::Immediate,
        }
    }

    /// Options for a commit deferred at the session's default deadline.
    #[must_use]
    pub fn deferred() -> Self {
        Self {
            persistence: CommitPersistence::deferred(),
        }
    }

    /// Options for a commit deferred with an explicit deadline.
    #[must_use]
    pub fn deferred_within(within_ms: DeferDeadline) -> Self {
        Self {
            persistence: CommitPersistence::Deferred { within_ms },
        }
    }
}

/// One document whose changes a deferred commit holds in memory.
///
/// The hold is pinned in the [`DocumentCache`], so it cannot be evicted before
/// it is stored. Merging a later deferred commit appends its operations and
/// moves the value forward; the stored batch is the concatenation (or one base
/// once a checkpoint selected one).
#[derive(Debug)]
pub struct HeldDocument {
    /// Stable identity of the logical address.
    pub address_id: String,
    /// The incarnation the changes apply to.
    pub record: DocumentRecord,
    /// Definition version of the held value.
    pub version: i64,
    /// The newest in-memory value.
    pub value: JsonObject,
    /// Operations queued since the last store, in commit order.
    pub ops: Vec<crate::durable::delta::Op>,
    /// Whether one held commit selected a base; storing then writes a base.
    pub base: bool,
    /// Latest time the hold may stay in memory; `None` never times out.
    pub deadline: Option<i64>,
}

/// Upstream `PendingOps`'s overflow analogue: past this many queued operations
/// the hold stores one complete base instead of replaying the queue.
pub const MAX_HELD_OPERATIONS: usize = 512;

/// Why a session ended (upstream `SessionEnd`).
///
/// `closed` is `close()` (or a storage failure that closed the session);
/// `failed` carries the first error that ended it.
#[derive(Clone, Debug)]
pub enum SessionEnd {
    /// The session closed normally.
    Closed,
    /// The session failed; storage included.
    Failed(StorageError),
}

impl SessionEnd {
    /// The failure that ended the session, if it failed.
    #[must_use]
    pub fn failure(&self) -> Option<&StorageError> {
        match self {
            Self::Closed => None,
            Self::Failed(error) => Some(error),
        }
    }

    /// Whether the session failed.
    #[must_use]
    pub fn is_failed(&self) -> bool {
        matches!(self, Self::Failed(_))
    }
}

/// The boxed future a read-only line job returns.
pub type ReadFuture<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = JobReply> + Send + 'a>>;

/// A read-only job that observes one committed state.
pub type ReadFn = dyn for<'a> FnOnce(&'a SessionState) -> ReadFuture<'a> + Send + 'static;

/// One command executed on the mutation line.
pub enum LineCommand {
    /// Run a commit callback and settle it.
    Commit(Box<CommitJob>),
    /// Run a read-only job that needs one committed state.
    ReadOnLine(Box<ReadJob>),
    /// Write a batch straight to storage with nothing published.
    WriteAtClose(Box<CloseWriteJob>),
    /// Fail the session (the line latches it and stops admitting).
    Fail(Box<StorageError>),
    /// Run a bookkeeping job on the line with mutable access to the state.
    Maintain(Box<MaintainJob>),
    /// Store what deferred commits currently hold.
    FlushHolds(Box<FlushHoldsJob>),
    /// Register a close listener on the line.
    SubscribeClose(Box<CloseSubscribeJob>),
    /// Drop one close subscription by id.
    UnsubscribeClose(u64),
    /// Stop the line.
    Shutdown(oneshot::Sender<Result<(), StorageError>>),
}

impl std::fmt::Debug for LineCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            Self::Commit(_) => "Commit",
            Self::ReadOnLine(_) => "ReadOnLine",
            Self::WriteAtClose(_) => "WriteAtClose",
            Self::Fail(_) => "Fail",
            Self::Maintain(_) => "Maintain",
            Self::FlushHolds(_) => "FlushHolds",
            Self::SubscribeClose(_) => "SubscribeClose",
            Self::UnsubscribeClose(_) => "UnsubscribeClose",
            Self::Shutdown(_) => "Shutdown",
        };
        f.write_str(name)
    }
}

/// A commit callback plus its reply channel.
pub struct CommitJob {
    /// The callback.
    pub run: Box<CommitFn>,
    /// The context the commit was admitted with.
    pub context: Context,
    /// Defaults bound to the commit.
    pub scope: TransactionScope,
    /// When the commit reaches storage.
    pub persistence: CommitPersistence,
    /// Where the callback's value goes.
    pub reply: oneshot::Sender<JobReply>,
    /// Capacity slot, held until the line settles this job.
    _permit: OwnedSemaphorePermit,
}

/// A read-only job plus its reply channel.
pub struct ReadJob {
    /// The job.
    pub run: Box<ReadFn>,
    /// Where the job's value goes.
    pub reply: oneshot::Sender<JobReply>,
    /// Capacity slot, held until the line settles this job.
    _permit: OwnedSemaphorePermit,
}

/// A close-time write batch builder.
pub type CloseWriteFn = dyn FnOnce(&SessionState) -> Vec<StorageWrite> + Send + 'static;

/// A close-time storage write with nothing published.
pub struct CloseWriteJob {
    /// Produces the batch from the final state.
    pub writes: Box<CloseWriteFn>,
    /// Completion signal.
    pub reply: oneshot::Sender<Result<(), StorageError>>,
    /// Capacity slot, held until the line settles this job.
    _permit: OwnedSemaphorePermit,
}

/// A synchronous bookkeeping job on the line (`&mut` access, no storage call).
pub struct MaintainJob {
    /// The job. It receives the line-owned state, so an internal component can
    /// update its bookkeeping under the same exclusivity as a commit.
    pub run: Box<dyn FnOnce(&mut SessionInner) + Send>,
    /// Completion signal. `Err` means the job panicked and failed the session.
    pub reply: oneshot::Sender<Result<(), StorageError>>,
    /// Capacity slot. `None` only for the timer's structural sweep command,
    /// which cannot await capacity.
    _permit: Option<OwnedSemaphorePermit>,
}

/// A store-what-is-held job.
pub struct FlushHoldsJob {
    /// Completion signal, when a caller waits; the deadline timer drops it.
    pub reply: Option<oneshot::Sender<Result<(), StorageError>>>,
    /// The arming token this job was posted with, or [`UNCONDITIONAL_FLUSH`]
    /// for an explicit flush.
    pub token: u64,
    /// Capacity slot. `None` only for the deadline timer, which cannot await
    /// capacity.
    _permit: Option<OwnedSemaphorePermit>,
}

/// The token an explicit [`SessionHandle::flush_deferred`] job carries: it is
/// never invalidated by a re-armed deadline timer.
pub const UNCONDITIONAL_FLUSH: u64 = u64::MAX;

/// A close-listener registration.
pub struct CloseSubscribeJob {
    /// The subscription id the handle allocated.
    pub id: u64,
    /// The listener.
    pub listener: CloseListener,
}

/// Options for opening a session.
#[derive(Clone, Default)]
pub struct SessionOptions {
    /// Wall clock in epoch milliseconds. Defaults to the system clock.
    pub now: Option<Arc<dyn Fn() -> i64 + Send + Sync>>,
    /// How long an unobserved document stays loaded; `None` uses the default
    /// (ten minutes), `0` drops it right after use.
    pub document_retention_ms: Option<i64>,
    /// How long a deferred commit may stay in memory when its
    /// [`CommitPersistence::Deferred`] carries [`DeferDeadline::Default`];
    /// `None` uses [`DEFAULT_PERSIST_WITHIN_MS`] (1,000 ms).
    pub persist_within_ms: Option<u64>,
    /// Document-definition behaviours assembly consults (`checkpointWhen`).
    ///
    /// The line owns assembly ([`run_commit`]), so it — not the caller's
    /// callback — needs the definitions to plan document writes and to adopt
    /// them into publications. Defaults to no behaviours.
    pub behaviours: Option<Arc<dyn DocBehaviours>>,
    /// Host telemetry. `None` (the default) emits nothing: committed-entry and
    /// settled-submission events and listener-error reports are only recorded
    /// when a handle carrying a sink is installed.
    /// [`HarnessTelemetry::from_telemetry`] installs a sink that forwards them
    /// to the crate's portable telemetry core.
    pub telemetry: Option<HarnessTelemetry>,
    /// Host hook run inside every conversation-creating commit (upstream
    /// `conversationCreated`).
    ///
    /// `None` (the default) leaves creation to the kernel. A harness installs
    /// the built-in documents here so a conversation and its `pi.live`,
    /// `pi.inbox`, `pi.usage`, `pi.provider` and `pi.agent` documents commit
    /// atomically: a crash can never leave a conversation without them.
    pub conversation_created: Option<Arc<crate::durable::session::facade::ConversationCreatedHook>>,
}

impl std::fmt::Debug for SessionOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionOptions")
            .field("document_retention_ms", &self.document_retention_ms)
            .finish_non_exhaustive()
    }
}

/// Read-only view of the session state, handed to commit callbacks and line
/// jobs.
///
/// A callback gets this instead of `&mut SessionInner` on purpose: it may read
/// the storage (through the guard, which applies the failure rule) and the
/// clock, but it cannot mutate listener sets, the cache, or the closing flag.
#[derive(Clone, Copy)]
pub struct SessionState<'a> {
    /// The failure-guarded storage.
    pub storage: &'a GuardedStorage,
    /// The wall clock.
    pub now: &'a (dyn Fn() -> i64 + Send + Sync),
}

impl SessionState<'_> {
    /// The current time on the session clock.
    #[must_use]
    pub fn now(&self) -> i64 {
        (self.now)()
    }
}

/// The session state, owned exclusively by the line task.
pub struct SessionInner {
    /// The storage behind its failure guard.
    pub storage: GuardedStorage,
    /// The loaded document cache.
    pub documents: DocumentCache,
    /// Host commit listeners.
    pub commit_listeners: ListenerSet,
    /// Internal listeners (a panic fails the session).
    pub internal_listeners: ListenerSet,
    /// Live document observers; retirement is delivered through
    /// [`attach::deliver`](crate::durable::session::attach::deliver) so their
    /// pins are released and the retired incarnation leaves the cache.
    pub attachments: Vec<Attached>,
    /// Close listeners; run synchronously when close begins.
    pub close_listeners: Vec<Arc<dyn Fn() + Send + Sync>>,
    /// Host close subscriptions (`subscribe_close`), with their ids.
    pub close_subscriptions: Vec<(u64, CloseListener)>,
    /// Before-close hooks; taken and run once between the admission seal and
    /// the backend close (upstream `beforeClose`).
    pub before_close_hooks: BeforeCloseSet,
    /// The wall clock.
    pub now: Arc<dyn Fn() -> i64 + Send + Sync>,
    /// The last assigned sequence.
    pub last_seq: Seq,
    /// Whether close began.
    pub closing: bool,
    /// The failure latch.
    pub latch: Arc<FailureLatchCore>,
    /// Document-definition behaviours assembly consults.
    pub behaviours: Arc<dyn DocBehaviours>,
    /// Where reports that never fail their caller go (upstream `report`).
    pub report: SessionReport,
    /// Host telemetry; `None` makes every emission a branch, not a call.
    pub telemetry: Option<HarnessTelemetry>,
    /// Timers for background work such as the document eviction sweep.
    pub timers: Timers,
    /// Documents whose deferred commits are held in memory, by incarnation.
    pub holds: std::collections::BTreeMap<DocumentId, HeldDocument>,
    /// Deferred deadline used by [`DeferDeadline::Default`].
    persist_within_ms: u64,
    /// The held-commit count, published to handles so a read can skip a
    /// pointless flush round trip.
    held_count: Arc<AtomicUsize>,
    /// The deadline the armed hold timer stands for.
    hold_armed_at: Option<i64>,
    /// The armed hold timer.
    hold_timer: Option<TimerHandle>,
    /// Invalidates a hold-timer callback queued before a re-arm or disarm.
    hold_token: u64,
    /// Sends a bookkeeping command from a timer callback back onto the line.
    sender: mpsc::UnboundedSender<LineCommand>,
    /// The session-end signal every `closed()` receiver awaits.
    end_tx: watch::Sender<Option<SessionEnd>>,
    /// Admission slots for ordinary commands (see [`LINE_CAPACITY`]).
    permits: Arc<Semaphore>,
    /// The eviction deadline the armed timer stands for.
    sweep_armed_at: Option<i64>,
    /// The armed eviction timer.
    sweep_timer: Option<TimerHandle>,
    /// Invalidates a timer callback that was already queued when the timer was
    /// re-armed or disarmed.
    sweep_token: u64,
    /// Set once the host declined a background timer: then none is kept, and
    /// the end of each commit evicts what expired (upstream
    /// `#documentTimersUnavailable`).
    document_timers_unavailable: bool,
}

impl SessionInner {
    /// The current time on the session clock.
    #[must_use]
    pub fn now(&self) -> i64 {
        (self.now)()
    }

    /// The read-only view commit callbacks receive.
    #[must_use]
    pub fn state(&self) -> SessionState<'_> {
        SessionState {
            storage: &self.storage,
            now: &*self.now,
        }
    }

    /// The committed sequence.
    #[must_use]
    pub fn seq(&self) -> Seq {
        self.last_seq
    }

    /// Fail the session with the first error.
    pub fn fail(&mut self, error: StorageError) {
        self.latch.fail(error);
        self.fail_close();
    }

    /// Latch a failure without running the close half.
    ///
    /// `publish` uses this so a retirement publication can still reach
    /// [`deliver_retired_attachments`] before `fail_close` ends observers:
    /// the close listener must not preempt the terminal retirement frame.
    fn latch_failure(&mut self, error: StorageError) {
        self.latch.fail(error);
    }

    /// The close half of a failure (upstream `fail` → close): stop admitting,
    /// cancel the eviction and hold timers and run every close listener so
    /// observers end with `SessionFailed` without waiting for an explicit
    /// close.
    ///
    /// The session-end signal is published here so a `closed()` receiver
    /// settles even when nobody calls `close()` after the failure (upstream
    /// `fail()` closes in the background).
    pub fn fail_close(&mut self) {
        if self.closing {
            return;
        }
        self.closing = true;
        self.disarm_document_sweep();
        self.disarm_hold_timer();
        self.clear_holds();
        run_close_listeners(self);
        if let Some(error) = self.latch.failure() {
            let _ = self.end_tx.send(Some(SessionEnd::Failed(error)));
        }
    }

    /// Report an error that must never fail what ran it.
    pub fn report(&self, message: &str, info: ReportInfo) {
        self.report_with_span(message, info, None);
    }

    /// [`SessionInner::report`] attached to `span`, when one is active.
    ///
    /// The report also raises `pi.durable.report` and the `pi.durable.reports`
    /// counter through the session's [`SessionOptions::telemetry`], matching
    /// the harness and scheduler report paths. Only listener errors are
    /// reported on this line today; other kinds stay host-only until a caller
    /// needs them.
    pub fn report_with_span(&self, message: &str, info: ReportInfo, span: Option<SpanHandle>) {
        if matches!(&info, ReportInfo::ListenerError) {
            if let Some(telemetry) = self.telemetry.as_ref() {
                telemetry.record_report(span, "listener_error", None, None, None);
            }
        }
        (self.report)(message, info);
    }

    /// Evict expired idle documents and re-arm the sweep.
    pub fn sweep_documents(&mut self) {
        let now = self.now();
        self.documents.sweep(now);
        self.arm_document_sweep();
    }

    /// Arm the eviction of the oldest idle document at its retention deadline
    /// (upstream `#scheduleDocumentSweep`, `session.ts:626-670`).
    ///
    /// The timer is a **background** one: it only frees memory and may be
    /// declined by the host. Where it is declined none is kept, and the end of
    /// each commit evicts what expired.
    pub fn arm_document_sweep(&mut self) {
        if self.closing {
            return;
        }
        let Some(at) = self.documents.sweep_deadline() else {
            self.disarm_document_sweep();
            return;
        };
        // A pending sweep for the oldest document comes first unless the
        // retention was shortened since; then it moves up.
        if self.sweep_armed_at.is_some_and(|armed| armed <= at) {
            return;
        }
        self.disarm_document_sweep();
        // Saturating: the deadline is `last use + retention`, and an infinite
        // retention is `i64::MAX`.
        let delay = at.saturating_sub(self.now());
        self.sweep_token = self.sweep_token.wrapping_add(1);
        let token = self.sweep_token;
        let sender = self.sender.clone();
        let permits = self.permits.clone();
        let run: crate::durable::timers::TimerCallback = Arc::new(move || {
            // A timer callback cannot await. Take a slot when one is free, but
            // send even when saturated: this path posts at most one command
            // per armed sweep (`sweep_armed_at` admits one arm at a time), so
            // it cannot grow the queue without bound, and dropping the command
            // would lose the sweep entirely.
            let permit = permits.clone().try_acquire_owned().ok();
            let (reply, _answer) = oneshot::channel();
            let _ = sender.send(LineCommand::Maintain(Box::new(MaintainJob {
                run: Box::new(move |inner: &mut SessionInner| {
                    if inner.sweep_token != token {
                        return;
                    }
                    inner.sweep_timer = None;
                    inner.sweep_armed_at = None;
                    inner.sweep_documents();
                }),
                reply,
                _permit: permit,
            })));
        });
        self.sweep_armed_at = Some(at);
        if delay <= 0 {
            // The retention already passed: sweep as soon as the line is free.
            run();
            return;
        }
        if self.document_timers_unavailable {
            self.sweep_armed_at = None;
            return;
        }
        match self.timers.background(run, delay) {
            Some(timer) => self.sweep_timer = Some(timer),
            None => {
                self.document_timers_unavailable = true;
                self.sweep_armed_at = None;
            }
        }
    }

    /// Cancel the armed eviction timer, if any.
    pub fn disarm_document_sweep(&mut self) {
        if let Some(timer) = self.sweep_timer.take() {
            timer.cancel();
        }
        self.sweep_armed_at = None;
        self.sweep_token = self.sweep_token.wrapping_add(1);
    }

    /// Arm the store timer at the earliest held deadline (upstream
    /// `#armHoldTimer`, `session.ts:922-948`).
    ///
    /// This is a **foreground** timer: it paces durability, not just memory,
    /// so a host that declines it falls back to the native timer. When it
    /// fires it stores every hold in one storage transaction — a deadline is
    /// the latest time, and the others cost little more in the same batch.
    pub fn arm_hold_timer(&mut self) {
        if self.closing {
            return;
        }
        let at = self.holds.values().filter_map(|hold| hold.deadline).min();
        let Some(at) = at else {
            self.disarm_hold_timer();
            return;
        };
        if self.hold_armed_at.is_some_and(|armed| armed <= at) {
            return;
        }
        self.disarm_hold_timer();
        // Saturating: a hold deadline may be far in the future (a host can ask
        // for "never"), and an absolute `i64::MAX` deadline must not overflow.
        let delay = at.saturating_sub(self.now()).max(0);
        self.hold_token = self.hold_token.wrapping_add(1);
        let token = self.hold_token;
        let sender = self.sender.clone();
        let permits = self.permits.clone();
        let run: TimerCallback = Arc::new(move || {
            let permit = permits.clone().try_acquire_owned().ok();
            let (reply, _answer) = oneshot::channel();
            let _ = sender.send(LineCommand::FlushHolds(Box::new(FlushHoldsJob {
                reply: Some(reply),
                token,
                _permit: permit,
            })));
        });
        self.hold_armed_at = Some(at);
        self.hold_timer = Some(self.timers.foreground(run, delay));
    }

    /// Cancel the armed hold timer, if any.
    pub fn disarm_hold_timer(&mut self) {
        if let Some(timer) = self.hold_timer.take() {
            timer.cancel();
        }
        self.hold_armed_at = None;
        self.hold_token = self.hold_token.wrapping_add(1);
    }

    /// Publish the held-document count to handles.
    pub fn sync_held_count(&self) {
        self.held_count.store(self.holds.len(), Ordering::Release);
    }

    /// Drop every hold without storing it (a failed session).
    pub fn clear_holds(&mut self) {
        if self.holds.is_empty() {
            self.sync_held_count();
            return;
        }
        self.disarm_hold_timer();
        let now = self.now();
        for (_, hold) in std::mem::take(&mut self.holds) {
            self.documents.unpin(hold.record.id, &hold.address_id, now);
        }
        self.sync_held_count();
    }
}

impl FailureLatch for SessionInner {
    fn failure(&self) -> Option<StorageError> {
        self.latch.failure()
    }

    fn fail(&self, error: StorageError) {
        FailureLatch::fail(&*self.latch, error);
    }
}

/// The handle callers hold. Cheap to clone.
#[derive(Clone)]
pub struct SessionHandle {
    sender: mpsc::UnboundedSender<LineCommand>,
    /// Admission slots for ordinary commands (see [`LINE_CAPACITY`]).
    permits: Arc<Semaphore>,
    latch: Arc<FailureLatchCore>,
    /// Set once close began: no command is admitted after this instant.
    closing: Arc<AtomicBool>,
    /// Serializes admission checks with the close seal and the send that makes
    /// a command admitted. A command that passes the check under this lock is
    /// enqueued before `Shutdown`; a command that loses the race is rejected.
    admission: Arc<Mutex<()>>,
    /// Admitted ordinary jobs that have not settled yet. `close` waits for
    /// this count to reach zero before sending `Shutdown`, so a commit that
    /// already obtained its permit cannot be dropped behind the close command.
    in_flight: Arc<AtomicUsize>,
    /// Fired when the last admitted job settles (`in_flight` reaches zero).
    /// `close` awaits it instead of yielding in a loop, so waiting for a long
    /// commit costs no CPU wake-ups.
    in_flight_idle: Arc<Notify>,
    /// Before-close hooks, run once by `close`. See [`BeforeCloseHook`].
    before_close: BeforeCloseSet,
    /// Set by the first `close`: the before-close hooks run exactly once even
    /// when several callers close concurrently, and a failed session's close
    /// (from `FailureLatchCore`'s hook) never re-runs them.
    before_close_claimed: Arc<AtomicBool>,
    /// How many documents a deferred commit currently holds; a read uses it to
    /// skip a pointless flush round trip.
    held_count: Arc<AtomicUsize>,
    /// Receives the session-end signal for `closed()`.
    end_rx: watch::Receiver<Option<SessionEnd>>,
    /// The next close-subscription id.
    next_close_subscription: Arc<AtomicU64>,
}

/// Decrements the session's in-flight count even if the caller future is
/// cancelled or the line job never replies.
struct InFlightGuard {
    counter: Arc<AtomicUsize>,
    idle: Arc<Notify>,
}

impl InFlightGuard {
    fn new(counter: Arc<AtomicUsize>, idle: Arc<Notify>) -> Self {
        counter.fetch_add(1, Ordering::AcqRel);
        Self { counter, idle }
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        if self.counter.fetch_sub(1, Ordering::AcqRel) == 1 {
            // The last admitted job settled: wake the closing waiter(s).
            self.idle.notify_waiters();
        }
    }
}

impl std::fmt::Debug for SessionHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionHandle")
            .field("failed", &self.is_failed())
            .field("closing", &self.is_closing())
            .field("in_flight", &self.in_flight.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}

/// What a commit callback may return.
///
/// A callback erases its value **once**: the reply channel carries a
/// [`JobValue`], so `Ok(value.into_job_value())` (equivalently
/// `Ok(Box::new(value) as JobValue)`) is the whole contract. [`SessionHandle`]
/// does not call `into_job_value` itself — it only recovers the type the
/// caller asked for.
pub trait IntoJobValue: Send + 'static {
    /// Erase the value for the reply channel.
    fn into_job_value(self) -> JobValue;
}

impl<T: Send + 'static> IntoJobValue for T {
    fn into_job_value(self) -> JobValue {
        Box::new(self)
    }
}

/// Recover the concrete value a line job erased into its reply.
///
/// The reply channel is untyped ([`JobValue`]), so a job erases the value it
/// returns and the caller names the type it asked for. The erased value is
/// normally `T` itself (`Box<T>`); a job routed through a facade wrapper may
/// hand back an already-erased `JobValue` (`Box<Box<T>>`), because the facade
/// passes the callback's reply through unchanged instead of re-boxing it.
/// Accepting either depth keeps the typed reply independent of which layer
/// performed the erasure — the previous single-level `downcast::<T>()`
/// rejected the second shape with "unexpected value type" even though the
/// value was the requested one. A genuinely different type still fails with
/// that diagnostic.
fn typed_reply<T: Send + 'static>(value: JobValue, what: &str) -> Result<T, StorageError> {
    let value = match value.downcast::<T>() {
        Ok(value) => return Ok(*value),
        Err(value) => value,
    };
    let inner = value
        .downcast::<JobValue>()
        .map_err(|_| unexpected_reply(what))?;
    inner
        .downcast::<T>()
        .map(|value| *value)
        .map_err(|_| unexpected_reply(what))
}

/// The error both typed replies return when the erased value is not the
/// caller's type.
fn unexpected_reply(what: &str) -> StorageError {
    StorageError::invalid(format!("{what} returned an unexpected value type"))
}

impl SessionHandle {
    /// Whether the session already failed.
    #[must_use]
    pub fn is_failed(&self) -> bool {
        self.latch.is_failed()
    }

    /// Whether close began (new admission is refused).
    #[must_use]
    pub fn is_closing(&self) -> bool {
        self.closing.load(Ordering::Acquire)
    }

    /// The first failure, if any.
    #[must_use]
    pub fn failure(&self) -> Option<StorageError> {
        self.latch.failure()
    }

    /// Run one commit callback and return its value.
    ///
    /// The callback erases its value into the reply channel (`CommitFuture`
    /// yields a [`JobReply`]); `T` is the type the caller expects back. It is
    /// recovered with [`typed_reply`], so erasing by hand and erasing through
    /// a wrapper are both accepted.
    ///
    /// Admission is checked here, before the send, so a caller that races a
    /// close gets a typed rejection instead of a command the line never runs.
    /// Once the send succeeded the commit is admitted and always settles,
    /// regardless of caller cancellation (spec §4).
    pub async fn commit<T, F>(
        &self,
        scope: TransactionScope,
        context: Context,
        run: F,
    ) -> Result<T, StorageError>
    where
        T: Send + 'static,
        F: for<'a> FnOnce(&'a mut Transaction, &'a SessionState<'_>) -> CommitFuture<'a>
            + Send
            + 'static,
    {
        self.commit_with_options(scope, context, CommitOptions::default(), run)
            .await
    }

    /// [`SessionHandle::commit`] with an explicit [`CommitOptions`].
    ///
    /// `options.persistence` selects upstream's `CommitPersistence`: an
    /// immediate commit (the default) stores before this resolves; a deferred
    /// one adopts its document changes in memory, publishes them, and resolves
    /// before storage sees them.
    pub async fn commit_with_options<T, F>(
        &self,
        scope: TransactionScope,
        context: Context,
        options: CommitOptions,
        run: F,
    ) -> Result<T, StorageError>
    where
        T: Send + 'static,
        F: for<'a> FnOnce(&'a mut Transaction, &'a SessionState<'_>) -> CommitFuture<'a>
            + Send
            + 'static,
    {
        if self.is_failed() {
            return Err(self.session_failed());
        }
        if self.is_closing() {
            return Err(StorageError::closed("Session"));
        }
        let permit = self.admit().await?;
        let (reply, answer) = oneshot::channel();
        // Take the admission gate only after a permit is held: `close` uses
        // the same gate, so a commit that passes the re-check below enqueues
        // its command before Shutdown can be sent.
        let _inflight = {
            let _gate = self
                .admission
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let inflight = InFlightGuard::new(self.in_flight.clone(), self.in_flight_idle.clone());
            if self.is_failed() {
                return Err(self.session_failed());
            }
            if self.is_closing() {
                return Err(StorageError::closed("Session"));
            }
            let job = CommitJob {
                run: Box::new(run),
                context,
                scope,
                persistence: options.persistence,
                reply,
                _permit: permit,
            };
            self.sender
                .send(LineCommand::Commit(Box::new(job)))
                .map_err(|_| StorageError::closed("Session"))?;
            inflight
        };
        let answered = answer.await;
        match answered {
            Ok(Ok(value)) => typed_reply::<T>(value, "commit"),
            Ok(Err(error)) => Err(error),
            Err(_) => Err(self.session_failed()),
        }
    }

    /// Whether a deferred commit currently holds document changes in memory.
    #[must_use]
    pub fn has_deferred(&self) -> bool {
        self.held_count.load(Ordering::Acquire) > 0
    }

    /// Store every held change now and wait until it is durable.
    ///
    /// A no-op when nothing is held. This is the explicit form of the flush
    /// that reads, immediate commits and close perform on their own.
    pub async fn flush_deferred(&self) -> Result<(), StorageError> {
        if self.is_failed() {
            return Err(self.session_failed());
        }
        if self.is_closing() {
            return Err(StorageError::closed("Session"));
        }
        if !self.has_deferred() {
            return Ok(());
        }
        let permit = self.admit().await?;
        let (reply, answer) = oneshot::channel();
        self.sender
            .send(LineCommand::FlushHolds(Box::new(FlushHoldsJob {
                reply: Some(reply),
                token: UNCONDITIONAL_FLUSH,
                _permit: Some(permit),
            })))
            .map_err(|_| StorageError::closed("Session"))?;
        answer.await.map_err(|_| self.session_failed())?
    }

    /// A receiver for the session-end signal (see [`SessionEnd`]).
    #[must_use]
    pub fn closed_receiver(&self) -> watch::Receiver<Option<SessionEnd>> {
        self.end_rx.clone()
    }

    /// Register a close listener (upstream `subscribeClose`).
    ///
    /// The listener runs synchronously on the line when close begins — also
    /// when a storage failure closes the session — in registration order. It
    /// must not block or call session APIs. Returns the id
    /// [`SessionHandle::unsubscribe_close`] removes.
    pub fn subscribe_close(&self, listener: CloseListener) -> Result<u64, StorageError> {
        if self.is_failed() {
            return Err(self.session_failed());
        }
        if self.is_closing() {
            return Err(StorageError::closed("Session"));
        }
        let id = self.next_close_subscription.fetch_add(1, Ordering::AcqRel);
        // No permit: registration is synchronous bookkeeping and must not wait
        // behind commits, exactly like upstream's set insertion.
        self.sender
            .send(LineCommand::SubscribeClose(Box::new(CloseSubscribeJob {
                id,
                listener,
            })))
            .map_err(|_| StorageError::closed("Session"))?;
        Ok(id)
    }

    /// Remove one close subscription by id. Unknown ids are ignored.
    pub fn unsubscribe_close(&self, id: u64) {
        let _ = self.sender.send(LineCommand::UnsubscribeClose(id));
    }

    /// Run a read-only job on the line, observing one committed state.
    ///
    /// `T` is the type the job erased into its reply; it is recovered with
    /// [`typed_reply`] exactly as [`SessionHandle::commit`] does.
    pub async fn read_on_line<T, F>(&self, run: F) -> Result<T, StorageError>
    where
        T: Send + 'static,
        F: for<'a> FnOnce(&'a SessionState<'_>) -> ReadFuture<'a> + Send + 'static,
    {
        if let Some(failure) = self.failure() {
            return Err(StorageError::from(crate::durable::errors::SessionFailed {
                cause: failure,
            }));
        }
        if self.is_closing() {
            return Err(StorageError::closed("Session"));
        }
        let permit = self.admit().await?;
        let (reply, answer) = oneshot::channel();
        let _inflight = {
            let _gate = self
                .admission
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let inflight = InFlightGuard::new(self.in_flight.clone(), self.in_flight_idle.clone());
            if let Some(failure) = self.failure() {
                return Err(StorageError::from(crate::durable::errors::SessionFailed {
                    cause: failure,
                }));
            }
            if self.is_closing() {
                return Err(StorageError::closed("Session"));
            }
            self.sender
                .send(LineCommand::ReadOnLine(Box::new(ReadJob {
                    run: Box::new(run),
                    reply,
                    _permit: permit,
                })))
                .map_err(|_| StorageError::closed("Session"))?;
            inflight
        };
        match answer.await {
            Ok(Ok(value)) => typed_reply::<T>(value, "read job"),
            Ok(Err(error)) => Err(error),
            Err(_) => Err(self.session_failed()),
        }
    }

    /// Run a bookkeeping job on the line and wait for it.
    pub async fn maintain<F>(&self, run: F) -> Result<(), StorageError>
    where
        F: FnOnce(&mut SessionInner) + Send + 'static,
    {
        if self.is_failed() {
            return Err(self.session_failed());
        }
        if self.is_closing() {
            return Err(StorageError::closed("Session"));
        }
        let permit = self.admit().await?;
        let (reply, answer) = oneshot::channel();
        let _inflight = {
            let _gate = self
                .admission
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let inflight = InFlightGuard::new(self.in_flight.clone(), self.in_flight_idle.clone());
            if self.is_failed() {
                return Err(self.session_failed());
            }
            if self.is_closing() {
                return Err(StorageError::closed("Session"));
            }
            self.sender
                .send(LineCommand::Maintain(Box::new(MaintainJob {
                    run: Box::new(run),
                    reply,
                    _permit: Some(permit),
                })))
                .map_err(|_| StorageError::closed("Session"))?;
            inflight
        };
        match answer.await {
            Ok(result) => result,
            Err(_) => Err(self.session_failed()),
        }
    }

    /// Seal admission, run the before-close hooks once, wait for admitted
    /// commands, run close listeners, and close the backend.
    ///
    /// The order is upstream's `close()` (`session.ts:490-512`): the seal comes
    /// first so no commit can be admitted behind the close, then the
    /// `beforeClose` hooks (which may themselves use
    /// [`SessionHandle::write_at_close`]), then `Shutdown` on the line. The
    /// hooks run off the line, so a hook that writes at close cannot deadlock
    /// against the line that would be running it.
    pub async fn close(&self) -> Result<(), StorageError> {
        // Seal under the admission gate: a commit either enqueues before the
        // seal (and is counted in `in_flight`) or sees `closing` and rejects.
        {
            let _gate = self
                .admission
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            self.closing.store(true, Ordering::Release);
        }
        // Everything already admitted must settle before Shutdown reaches the
        // line; otherwise Shutdown could overtake a queued commit and drop it.
        // The last InFlightGuard notifies `in_flight_idle`, so this wait is
        // event-driven: a long commit costs no CPU wake-ups (the former
        // `yield_now` loop spun on a single-threaded runtime).
        while self.in_flight.load(Ordering::Acquire) > 0 {
            let notified = self.in_flight_idle.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.in_flight.load(Ordering::Acquire) == 0 {
                break;
            }
            notified.await;
        }
        self.run_before_close().await;
        let (reply, answer) = oneshot::channel();
        if self.sender.send(LineCommand::Shutdown(reply)).is_err() {
            // A second close can find the line already gone: joining it is a
            // successful close unless the session is latched failed.
            return if self.is_failed() {
                Err(self.session_failed())
            } else {
                Ok(())
            };
        }
        match answer.await {
            Ok(result) => result,
            Err(_) if self.is_failed() => Err(self.session_failed()),
            Err(_) => Ok(()),
        }
    }

    /// Register a hook to run exactly once when the session closes
    /// (upstream `beforeClose`).
    ///
    /// `close` seals admission, waits for every admitted command, then runs the
    /// hooks in registration order — before the line clears its listeners and
    /// closes the backend. A hook may itself call
    /// [`SessionHandle::write_at_close`], which is how close-time bookkeeping
    /// reaches storage after the seal (upstream's harness writes the tasks it
    /// stopped back as `pending` this way).
    ///
    /// Registration after the seal is refused; a hook whose registration lost
    /// that race never runs, which is why the push re-checks `closing` under
    /// the admission gate.
    pub fn register_before_close(&self, hook: BeforeCloseHook) -> Result<(), StorageError> {
        let _gate = self
            .admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.closing.load(Ordering::Acquire) {
            return Err(StorageError::closed("Session"));
        }
        self.before_close
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(hook);
        Ok(())
    }

    /// Take and run the registered before-close hooks, exactly once.
    ///
    /// A second caller (or a failed session's automatic close) finds the set
    /// already claimed and returns. Each hook is awaited in registration order
    /// and a panicking hook is reported, never propagated: close must still
    /// reach storage.
    async fn run_before_close(&self) {
        if self.before_close_claimed.swap(true, Ordering::AcqRel) {
            return;
        }
        let hooks: Vec<BeforeCloseHook> = {
            let mut guard = self
                .before_close
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            std::mem::take(&mut *guard)
        };
        for hook in hooks {
            // Contain a synchronous panic from the hook factory and an async
            // one from the future itself. Close must always reach storage; the
            // hook's own component reports its failures.
            if let Ok(future) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(hook)) {
                let _ = AssertUnwindSafe(future).catch_unwind().await;
            }
        }
    }

    /// Write `writes()` straight to storage on the line, publishing nothing
    /// (upstream `writeAtClose`).
    ///
    /// For close-time bookkeeping such as writing stopped tasks back as
    /// `pending`: because it is an ordinary line command, it runs after every
    /// commit admitted before the seal settled, so a queued abort mark
    /// survives. Admission is deliberately not consulted — upstream calls it
    /// after the seal — but a failed session writes nothing.
    pub async fn write_at_close<F>(&self, writes: F) -> Result<(), StorageError>
    where
        F: FnOnce(&SessionState<'_>) -> Vec<StorageWrite> + Send + 'static,
    {
        let (reply, answer) = oneshot::channel();
        let permit = self.admit().await?;
        self.sender
            .send(LineCommand::WriteAtClose(Box::new(CloseWriteJob {
                writes: Box::new(writes),
                reply,
                _permit: permit,
            })))
            .map_err(|_| StorageError::closed("Session"))?;
        answer.await.map_err(|_| self.session_failed())?
    }

    /// Wait for one command slot.
    ///
    /// The permit moves into the job, so it is released only when the line
    /// settles the job; a caller that stops waiting cannot free the slot while
    /// its command is still queued.
    async fn admit(&self) -> Result<OwnedSemaphorePermit, StorageError> {
        self.permits
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| self.session_failed())
    }

    fn session_failed(&self) -> StorageError {
        match self.latch.failure() {
            Some(failure) => {
                StorageError::from(crate::durable::errors::SessionFailed { cause: failure })
            }
            None => StorageError::closed("Session"),
        }
    }
}

/// Open a session over `storage`, returning its handle and the line task.
pub fn open(
    storage: Arc<dyn Storage>,
    options: SessionOptions,
) -> (SessionHandle, tokio::task::JoinHandle<()>) {
    open_with_report(storage, options, None)
}

/// Open a session that reports failures to `report` (upstream
/// `HarnessOptions.onReport`); `None` drops them like a plain Session.
pub fn open_with_report(
    storage: Arc<dyn Storage>,
    options: SessionOptions,
    report: Option<SessionReport>,
) -> (SessionHandle, tokio::task::JoinHandle<()>) {
    open_with_report_and_timers(storage, options, report, None)
}

/// Open a session with the host timers it arms its background work with
/// (upstream `createSession({setTimer})`); `None` uses the native timer.
pub fn open_with_report_and_timers(
    storage: Arc<dyn Storage>,
    options: SessionOptions,
    report: Option<SessionReport>,
    timers: Option<Timers>,
) -> (SessionHandle, tokio::task::JoinHandle<()>) {
    let report: SessionReport = match report {
        Some(report) => report,
        None => Arc::new(|_: &str, _: ReportInfo| {}),
    };
    let latch = Arc::new(FailureLatchCore::with_report(report.clone()));
    let guarded = GuardedStorage::new(storage, latch.clone());
    let now: Arc<dyn Fn() -> i64 + Send + Sync> = options.now.unwrap_or_else(|| {
        Arc::new(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|delta| delta.as_millis() as i64)
                .unwrap_or(0)
        })
    });
    let retention =
        crate::durable::session::documents::document_retention(options.document_retention_ms);
    let (sender, receiver) = mpsc::unbounded_channel();
    let permits = Arc::new(Semaphore::new(LINE_CAPACITY));
    let held_count = Arc::new(AtomicUsize::new(0));
    let (end_tx, end_rx) = watch::channel(None);
    // The latch owns the first-failure signal. Mark admission closed at once
    // and post the close half onto the line: the failing call may be an awaited
    // storage read/write, so the line may only be free after it settles. The
    // session-end signal is published by `fail_close` (after every close
    // listener ran), never here: a `closed()` receiver must not settle before
    // close observers saw the end.
    let closing = Arc::new(AtomicBool::new(false));
    {
        let closing = closing.clone();
        let sender = sender.clone();
        latch.set_fail_hook(Arc::new(move |error: StorageError| {
            closing.store(true, Ordering::Release);
            let _ = sender.send(LineCommand::Fail(Box::new(error)));
        }));
    }
    let timer_report = {
        let report = report.clone();
        Arc::new(move |message: String| {
            report(
                &message,
                ReportInfo::HostError {
                    callback: HostCallback::SetTimer,
                    task: None,
                },
            );
        }) as Arc<dyn Fn(String) + Send + Sync>
    };
    let before_close: BeforeCloseSet = Arc::new(Mutex::new(Vec::new()));
    let inner = SessionInner {
        storage: guarded,
        documents: DocumentCache::new(retention),
        commit_listeners: ListenerSet::default(),
        internal_listeners: ListenerSet::default(),
        attachments: Vec::new(),
        close_listeners: Vec::new(),
        close_subscriptions: Vec::new(),
        before_close_hooks: before_close.clone(),
        now,
        last_seq: Seq::from_raw(0),
        closing: false,
        latch: latch.clone(),
        behaviours: options
            .behaviours
            .unwrap_or_else(|| Arc::new(NoDocBehaviours)),
        report,
        telemetry: options.telemetry,
        timers: timers.unwrap_or_else(|| Timers::native(timer_report)),
        holds: std::collections::BTreeMap::new(),
        persist_within_ms: options
            .persist_within_ms
            .unwrap_or(DEFAULT_PERSIST_WITHIN_MS),
        held_count: held_count.clone(),
        hold_armed_at: None,
        hold_timer: None,
        hold_token: 0,
        sender: sender.clone(),
        end_tx,
        permits: permits.clone(),
        sweep_armed_at: None,
        sweep_timer: None,
        sweep_token: 0,
        document_timers_unavailable: false,
    };
    let closing_handle = closing.clone();
    let admission = Arc::new(Mutex::new(()));
    let in_flight = Arc::new(AtomicUsize::new(0));
    let handle = SessionHandle {
        sender,
        permits,
        latch,
        closing: closing_handle,
        admission,
        in_flight,
        in_flight_idle: Arc::new(Notify::new()),
        before_close: before_close.clone(),
        before_close_claimed: Arc::new(AtomicBool::new(false)),
        held_count,
        end_rx,
        next_close_subscription: Arc::new(AtomicU64::new(1)),
    };
    let task = tokio::spawn(async move {
        run_line(inner, receiver).await;
    });
    (handle, task)
}

async fn run_line_loop(
    inner: &mut SessionInner,
    receiver: &mut mpsc::UnboundedReceiver<LineCommand>,
) {
    while let Some(command) = receiver.recv().await {
        match command {
            LineCommand::Commit(job) => run_commit(&mut *inner, *job).await,
            LineCommand::ReadOnLine(job) => {
                let ReadJob {
                    run,
                    reply,
                    _permit,
                } = *job;
                let result = if let Some(failure) = inner.latch.failure() {
                    Err(StorageError::from(crate::durable::errors::SessionFailed {
                        cause: failure,
                    }))
                } else {
                    let outcome = {
                        let state = inner.state();
                        // Contain a panic while the callback *builds* its
                        // future as well as one while it runs.
                        let built =
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(&state)));
                        match built {
                            Ok(future) => AssertUnwindSafe(future).catch_unwind().await,
                            Err(payload) => Err(payload),
                        }
                    };
                    match outcome {
                        Ok(result) => result,
                        Err(payload) => Err(fail_callback_panic(
                            &mut *inner,
                            "A session read job",
                            payload,
                        )),
                    }
                };
                let _ = reply.send(result);
            }
            LineCommand::WriteAtClose(job) => {
                let CloseWriteJob {
                    writes,
                    reply,
                    _permit,
                } = *job;
                // A failed session writes nothing; the batch is computed only
                // once the line is sure it may write (upstream `writeAtClose`).
                let result = if inner.latch.is_failed() {
                    Ok(())
                } else {
                    // Close-time bookkeeping must be based on stored state, so
                    // a held commit is stored first (it would otherwise sit in
                    // front of these writes forever).
                    match flush_holds(&mut *inner).await {
                        Ok(()) => {
                            let outcome =
                                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                    writes(&inner.state())
                                }));
                            match outcome {
                                Ok(writes) if writes.is_empty() => Ok(()),
                                Ok(writes) => {
                                    let storage = inner.storage.storage();
                                    inner
                                        .storage
                                        .write(storage.commit(writes))
                                        .await
                                        .map(|_| ())
                                }
                                Err(payload) => Err(fail_callback_panic(
                                    &mut *inner,
                                    "A session close-write job",
                                    payload,
                                )),
                            }
                        }
                        Err(error) => Err(error),
                    }
                };
                let _ = reply.send(result);
            }
            LineCommand::Maintain(job) => {
                let MaintainJob {
                    run,
                    reply,
                    _permit,
                } = *job;
                let result = if let Some(failure) = inner.latch.failure() {
                    Err(StorageError::from(crate::durable::errors::SessionFailed {
                        cause: failure,
                    }))
                } else {
                    let outcome =
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(&mut *inner)));
                    match outcome {
                        Ok(()) => Ok(()),
                        Err(payload) => Err(fail_callback_panic(
                            &mut *inner,
                            "A session maintain job",
                            payload,
                        )),
                    }
                };
                let _ = reply.send(result);
            }
            LineCommand::FlushHolds(job) => {
                let FlushHoldsJob {
                    reply,
                    token,
                    _permit,
                } = *job;
                let result = if let Some(failure) = inner.latch.failure() {
                    Err(StorageError::from(crate::durable::errors::SessionFailed {
                        cause: failure,
                    }))
                } else if token != UNCONDITIONAL_FLUSH && token != inner.hold_token {
                    // A later hold re-armed (or disarmed) the timer; this
                    // queued callback is stale and must not store early.
                    Ok(())
                } else {
                    if token == inner.hold_token {
                        inner.hold_timer = None;
                        inner.hold_armed_at = None;
                    }
                    let due = inner
                        .holds
                        .values()
                        .any(|hold| hold.deadline.is_some_and(|at| at <= inner.now()));
                    if token == UNCONDITIONAL_FLUSH || due {
                        flush_holds(&mut *inner).await
                    } else {
                        // The wall clock is behind the armed deadline (a fake
                        // clock, or clock skew): keep the timer alive.
                        inner.arm_hold_timer();
                        Ok(())
                    }
                };
                if let Some(reply) = reply {
                    let _ = reply.send(result);
                }
            }
            LineCommand::SubscribeClose(job) => {
                let CloseSubscribeJob { id, listener } = *job;
                // A subscription that raced the close still observes it, so it
                // runs immediately instead of being silently dropped.
                if inner.closing {
                    run_close_listener(&mut *inner, &listener);
                } else {
                    inner.close_subscriptions.push((id, listener));
                }
            }
            LineCommand::UnsubscribeClose(id) => {
                inner.close_subscriptions.retain(|(key, _)| *key != id);
            }
            LineCommand::Fail(error) => inner.fail(*error),
            LineCommand::Shutdown(reply) => {
                // Close listeners contain their own panics; this also contains
                // a panicking host storage `close`, which would otherwise
                // unwind the line and drop the close reply.
                let outcome = AssertUnwindSafe(shutdown(&mut *inner)).catch_unwind().await;
                let result = match outcome {
                    Ok(result) => result,
                    Err(payload) => Err(fail_callback_panic(
                        &mut *inner,
                        "A session shutdown callback",
                        payload,
                    )),
                };
                let _ = reply.send(result);
                break;
            }
        }
    }
}

/// Run the line and turn a line-task panic into the normal failure path.
///
/// The catch deliberately lives here, where `SessionInner` is still in scope:
/// latching alone is not enough because the failure must also mark the line
/// closing and run every close listener. If the report callback panics while
/// `fail` is latching, the fallback still runs `fail_close` directly.
async fn run_line(mut inner: SessionInner, mut receiver: mpsc::UnboundedReceiver<LineCommand>) {
    if let Err(payload) = AssertUnwindSafe(run_line_loop(&mut inner, &mut receiver))
        .catch_unwind()
        .await
    {
        let error = StorageError::invalid(format!(
            "The session line panicked: {}",
            panic_message(payload)
        ));
        if AssertUnwindSafe(async { inner.fail(error) })
            .catch_unwind()
            .await
            .is_err()
        {
            let _ = AssertUnwindSafe(async { inner.fail_close() })
                .catch_unwind()
                .await;
        }
    }
    // Wake producers still waiting for a slot: the line is gone, so their
    // sends would fail anyway.
    inner.permits.close();
    // The line is the only owner of the end sender: publish the end here when
    // no path did (a panic while closing), so `closed()` never waits for a
    // sender that is about to be dropped.
    let end = match inner.latch.failure() {
        Some(error) => SessionEnd::Failed(error),
        None => SessionEnd::Closed,
    };
    let _ = inner.end_tx.send(Some(end));
}

/// The absolute deadline of a deferred commit admitted at `now`, after
/// `after_ms` milliseconds.
///
/// A `u64` delay beyond the `i64` clock range means "effectively never"; it
/// saturates to [`i64::MAX`] instead of wrapping to a negative offset (which
/// would make the hold due immediately and silently defeat the deferral), and
/// the addition saturates so `now + delay` cannot overflow either.
fn hold_deadline(now: i64, after_ms: u64) -> i64 {
    let after = i64::try_from(after_ms).unwrap_or(i64::MAX);
    now.saturating_add(after)
}

/// Whether a commit's assembled batch may be held in memory
/// (`Transaction.holdable()`, upstream `transaction.ts:757-775`).
///
/// Every write must be a `document.change` of an existing conversation
/// document declared `history: "latest"`, `fork: "initial"`, at its stored
/// definition version. A creation, copy, retirement, migration or any table
/// write makes the commit immediate.
fn holdable_plans(plans: &[DocumentPlan], writes: &[StorageWrite]) -> bool {
    if writes.is_empty() {
        return false;
    }
    if !writes
        .iter()
        .all(|write| matches!(write, StorageWrite::DocumentChange { .. }))
    {
        return false;
    }
    let mut planned = 0usize;
    for plan in plans {
        if plan.retire {
            return false;
        }
        let Some(content) = &plan.content else {
            continue;
        };
        if !matches!(content, StorageWrite::DocumentChange { .. }) {
            return false;
        }
        let DocumentCreateOrRecord::Record(record) = &plan.record else {
            return false;
        };
        if !plan.same_version {
            return false;
        }
        if record.scope.conversation_id().is_none() {
            return false;
        }
        if record.history != Some(DocumentHistory::Latest) {
            return false;
        }
        if record.fork != Some(DocumentFork::Initial) {
            return false;
        }
        if plan.publication_value().is_none() {
            return false;
        }
        // A no-op delta cannot be held meaningfully; only a base carries a
        // complete value without operations.
        if plan.ops.is_empty()
            && !matches!(
                content,
                StorageWrite::DocumentChange {
                    content: DocumentContent::Base { .. },
                    ..
                }
            )
        {
            return false;
        }
        planned += 1;
    }
    planned == writes.len()
}

/// The loaded incarnations of this transaction's staged drafts, by incarnation.
///
/// A hold adopts a value whose stored metadata (delta count, stored version)
/// comes from the draft's load; the cache may not hold the incarnation if it
/// was evicted or if this commit is the first to touch it.
fn staged_loaded_documents(transaction: &Transaction) -> HashMap<DocumentId, LoadedDocument> {
    let mut loaded = HashMap::new();
    for document in transaction.documents() {
        if let Some(document) = &document.loaded {
            loaded
                .entry(document.record.id)
                .or_insert_with(|| document.clone());
        }
    }
    loaded
}

/// Adopt a holdable commit in memory and publish it (upstream `#hold`).
///
/// The cache advances to the new value, the incarnation is pinned so it cannot
/// be evicted before it is stored, and the change joins the incarnation's
/// pending operations (or starts a hold) with the earliest deadline. Nothing
/// reaches storage; the caller resolves the commit.
fn hold_commit(
    inner: &mut SessionInner,
    plans: &[DocumentPlan],
    loaded: &HashMap<DocumentId, LoadedDocument>,
    deadline: Option<i64>,
    context: &Context,
) -> Result<(), StorageError> {
    let now = inner.now();
    let mut changes = Vec::new();
    for plan in plans {
        let Some(StorageWrite::DocumentChange { id, content }) = &plan.content else {
            continue;
        };
        let (version, is_base) = match content {
            DocumentContent::Base { version, .. } => (*version, true),
            DocumentContent::Delta { version, .. } => (*version, false),
        };
        let DocumentCreateOrRecord::Record(record) = &plan.record else {
            continue;
        };
        let Some(value) = plan.publication_value() else {
            continue;
        };
        // 1. Adopt in memory: the value moves forward, while the stored version
        //    and delta count stay, because nothing was stored.
        let mut document = inner
            .documents
            .get(&plan.address_id)
            .cloned()
            .or_else(|| loaded.get(id).cloned())
            .unwrap_or_else(|| LoadedDocument {
                address_id: plan.address_id.clone(),
                record: record.clone(),
                stored_version: version,
                value_version: version,
                deltas_since_base: 0,
                value: value.clone(),
            });
        document.value = value.clone();
        document.value_version = version;
        inner.documents.insert(document, now);

        // 2. Pin the incarnation for the life of the hold.
        let fresh = !inner.holds.contains_key(id);
        if fresh {
            let cached = inner.documents.get(&plan.address_id).cloned();
            if let Some(cached) = cached {
                let _ = inner.documents.pin(&cached);
            }
        }

        // 3. Queue the operations and move the deadline to the earliest.
        let hold = inner.holds.entry(*id).or_insert_with(|| HeldDocument {
            address_id: plan.address_id.clone(),
            record: record.clone(),
            version,
            value: value.clone(),
            ops: Vec::new(),
            base: false,
            deadline,
        });
        hold.version = version;
        hold.value = value.clone();
        hold.base |= is_base;
        hold.ops.extend(plan.ops.iter().cloned());
        if hold.base || hold.ops.len() > MAX_HELD_OPERATIONS {
            // One complete base replaces an unmergeable or oversized queue
            // (upstream `PendingOps` overflow).
            hold.base = true;
            hold.ops.clear();
        }
        hold.deadline = match (hold.deadline, deadline) {
            (Some(current), Some(next)) => Some(current.min(next)),
            (current, next) => current.or(next),
        };

        changes.push(DocumentCommitChange::Document {
            record: record.clone(),
            conversation_id: plan
                .conversation_id
                .or_else(|| record.scope.conversation_id()),
            version: Some(version),
            value: Some(value.clone()),
            ops: plan.ops.clone(),
        });
    }
    inner.sync_held_count();
    inner.arm_hold_timer();
    if changes.is_empty() {
        return Ok(());
    }
    // 4. Show it to every observer: the commit is final in memory even though
    //    it is not yet durable. Listeners that mirror stored state observe the
    //    store when it happens (a flush publishes nothing, so no frame is
    //    delivered twice).
    let publication = CommitPublication {
        seq: inner.last_seq,
        changes: changes.into_iter().map(CommitChange::Document).collect(),
    };
    match publish(inner, &publication, context) {
        Ok(()) => Ok(()),
        Err(message) => {
            if !inner.latch.is_failed() {
                inner.fail(StorageError::invalid(format!(
                    "A session commit listener failed: {message}"
                )));
            }
            let cause = inner
                .latch
                .failure()
                .unwrap_or_else(|| StorageError::invalid(message));
            Err(crate::durable::errors::SessionFailed { cause }.into())
        }
    }
}

/// Store every held change in one storage transaction (upstream `#storeHolds`).
///
/// Memory already has the changes, so nothing is published: observers saw them
/// when the hold adopted them. A storage failure fails the session, as every
/// storage failure does; the held changes are lost and a reopen recovers what
/// was stored before the hold.
async fn flush_holds(inner: &mut SessionInner) -> Result<(), StorageError> {
    if inner.holds.is_empty() {
        return Ok(());
    }
    inner.disarm_hold_timer();
    if let Some(failure) = inner.latch.failure() {
        inner.clear_holds();
        return Err(crate::durable::errors::SessionFailed { cause: failure }.into());
    }
    let holds: Vec<HeldDocument> = std::mem::take(&mut inner.holds).into_values().collect();
    let mut writes = Vec::with_capacity(holds.len());
    for hold in &holds {
        let content = if hold.base {
            DocumentContent::Base {
                version: hold.version,
                value: hold.value.clone(),
            }
        } else {
            DocumentContent::Delta {
                version: hold.version,
                ops: hold.ops.clone(),
            }
        };
        writes.push(StorageWrite::DocumentChange {
            id: hold.record.id,
            content,
        });
    }
    let storage = inner.storage.storage();
    let result = inner.storage.write(storage.commit(writes)).await;
    inner.sync_held_count();
    let now = inner.now();
    match result {
        Ok(seq) => {
            inner.last_seq = seq;
            for hold in holds {
                inner.documents.apply_adopted(
                    &hold.address_id,
                    hold.version,
                    hold.base,
                    Some(&hold.value),
                );
                inner.documents.unpin(hold.record.id, &hold.address_id, now);
            }
            inner.arm_document_sweep();
            Ok(())
        }
        Err(error) => {
            // The guard already latched the failure. Release the pins: the
            // held changes are gone with the failed session.
            for hold in holds {
                inner.documents.unpin(hold.record.id, &hold.address_id, now);
            }
            inner.arm_document_sweep();
            Err(error)
        }
    }
}

/// The message carried by a caught panic payload.
fn panic_message(payload: Box<dyn Any + Send>) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|text| (*text).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "panicked".to_owned())
}

/// Fail the session after a host callback panicked and build the reply error.
///
/// The latch keeps the first error; the reply names this callback's panic even
/// when an earlier failure won the latch.
fn fail_callback_panic(
    inner: &mut SessionInner,
    callback: &str,
    payload: Box<dyn Any + Send>,
) -> StorageError {
    let cause = StorageError::invalid(format!("{callback} panicked: {}", panic_message(payload)));
    inner.fail(cause.clone());
    StorageError::from(crate::durable::errors::SessionFailed { cause })
}

/// The commit pipeline (upstream `#runCommit`).
async fn run_commit(inner: &mut SessionInner, job: CommitJob) {
    if let Some(failure) = inner.latch.failure() {
        let _ = job.reply.send(Err(StorageError::from(
            crate::durable::errors::SessionFailed { cause: failure },
        )));
        return;
    }
    // Admission-time caller cancellation rejects the commit before any write.
    if job.context.is_aborted() && !job.context.is_detached() {
        let _ = job.reply.send(Err(StorageError::Request(
            crate::durable::errors::StorageRequestError::new("Commit context was aborted"),
        )));
        return;
    }
    // An immediate commit is based on stored state, so every held change is
    // stored first (upstream `takeHeld` in front of a commit's own writes). A
    // deferred commit does not flush: it either merges its document changes
    // into the holds or, if it turns out not to be holdable, flushes after
    // assembly and before its own batch.
    if job.persistence.is_immediate() && !inner.holds.is_empty() {
        if let Err(error) = flush_holds(inner).await {
            let _ = job.reply.send(Err(error));
            return;
        }
    }
    let mut transaction = Transaction::new(job.context.clone(), job.scope)
        .with_storage(inner.storage.storage(), inner.latch.clone());
    let outcome = {
        let state = inner.state();
        // Contain a panic while the callback *builds* its future as well as
        // one while it runs.
        let built = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            (job.run)(&mut transaction, &state)
        }));
        match built {
            Ok(future) => AssertUnwindSafe(future).catch_unwind().await,
            Err(payload) => Err(payload),
        }
    };
    let value = match outcome {
        Ok(Ok(value)) => value,
        Ok(Err(error)) => {
            transaction.seal();
            inner.sweep_documents();
            let _ = job.reply.send(Err(error));
            return;
        }
        Err(payload) => {
            // A panicking callback must fail the session, not unwind the line:
            // otherwise the latch stays clean and every queued reply is lost.
            transaction.seal();
            inner.sweep_documents();
            let _ = job.reply.send(Err(fail_callback_panic(
                inner,
                "A session commit callback",
                payload,
            )));
            return;
        }
    };
    // A callback that caught a failed read must not commit.
    if let Some(failure) = inner.latch.failure() {
        transaction.seal();
        let _ = job.reply.send(Err(StorageError::from(
            crate::durable::errors::SessionFailed { cause: failure },
        )));
        return;
    }
    transaction.seal();

    // ── assemble (upstream `#assemble`, `:784-876`) ──────────────────────────
    //
    // Assembly belongs to the line, not to the caller's callback: it reads
    // committed state, mints document ids, validates owners and settles
    // submissions, so it must run against the state this commit is admitted
    // on. Doing it in a facade left `adopt` unreachable and document writes
    // unobservable.
    let (writes, plans) = if transaction.documents().is_empty()
        && transaction.staged_plans().is_empty()
        && transaction.staged_retirements().is_empty()
        && transaction.staged_tasks().is_empty()
        && transaction.staged_submissions().next().is_none()
    {
        // Nothing to plan: the callback's table writes are the whole batch.
        (transaction.take_writes(), Vec::new())
    } else {
        match assemble_with(
            &mut transaction,
            &inner.state(),
            &inner.storage,
            &*inner.behaviours,
        )
        .await
        {
            Ok(assembled) => assembled,
            Err(error) => {
                inner.sweep_documents();
                let _ = job.reply.send(Err(error));
                return;
            }
        }
    };
    // A failed read during assembly must not commit either.
    if let Some(failure) = inner.latch.failure() {
        inner.sweep_documents();
        let _ = job.reply.send(Err(StorageError::from(
            crate::durable::errors::SessionFailed { cause: failure },
        )));
        return;
    }
    if writes.is_empty() && plans.is_empty() {
        inner.sweep_documents();
        let _ = job.reply.send(Ok(value));
        return;
    }
    // ── deferred commit (upstream `CommitPersistence`) ──────────────────────
    //
    // Only a commit whose every write changes an existing `history: "latest"`,
    // `fork: "initial"` conversation document at its stored version may stay in
    // memory; anything else is stored now. The hold adopts the change in
    // memory, publishes it, and resolves — storage sees it on the next read,
    // immediate commit, close or deadline.
    if !job.persistence.is_immediate() && holdable_plans(&plans, &writes) {
        let deadline = match job.persistence {
            CommitPersistence::Deferred { within_ms } => match within_ms {
                DeferDeadline::Default => Some(hold_deadline(inner.now(), inner.persist_within_ms)),
                DeferDeadline::After(ms) => Some(hold_deadline(inner.now(), ms)),
                DeferDeadline::Never => None,
            },
            CommitPersistence::Immediate => None,
        };
        let loaded = staged_loaded_documents(&transaction);
        match hold_commit(inner, &plans, &loaded, deadline, &job.context) {
            Ok(()) => {
                inner.sweep_documents();
                let _ = job.reply.send(Ok(value));
            }
            Err(error) => {
                let _ = job.reply.send(Err(error));
            }
        }
        return;
    }
    // A deferred commit that is not holdable behaves as an immediate one; it
    // still stores every earlier hold first, so its own deltas apply on top.
    if !inner.holds.is_empty() {
        if let Err(error) = flush_holds(inner).await {
            let _ = job.reply.send(Err(error));
            return;
        }
    }
    // ADMITTED. Caller cancellation no longer applies: the line owns the
    // settlement, and the guard records a failure instead of unwinding it.
    //
    // With no observer there is nothing to publish, so the table half is not
    // built at all: a commit's cost stays the storage call plus adoption, and
    // the batch never gets copied. With an observer, the table half is built
    // from `&writes` **before** the batch moves into storage, so the document
    // content writes (the bulk of a batch) are never cloned — only the table
    // records the publication must own are copied, once. `seq` is filled in
    // when storage returns it and the adopted document changes are appended
    // after adoption, in plan order — exactly the order
    // `CommitPublication::build` produces.
    let observed = inner
        .telemetry
        .as_ref()
        .is_some_and(crate::durable::harness::telemetry::HarnessTelemetry::enabled)
        || !inner.internal_listeners.is_empty()
        || !inner.commit_listeners.is_empty();
    let mut publication = observed.then(|| CommitPublication::build(inner.last_seq, &writes));
    let storage = inner.storage.storage();
    let seq = match inner.storage.write(storage.commit(writes)).await {
        Ok(seq) => seq,
        Err(error) => {
            inner.sweep_documents();
            let _ = job.reply.send(Err(error));
            return;
        }
    };
    inner.last_seq = seq;

    // ── adopt (cache values and pointer bookkeeping, no reads) ──────────────
    let documents = adopt(&plans, seq);
    for plan in &plans {
        if plan.retire {
            inner.documents.mark_retired(&plan.address_id, seq);
        } else if plan.publishes() {
            let is_base = matches!(
                plan.content,
                Some(StorageWrite::DocumentCreate { .. })
                    | Some(StorageWrite::DocumentChange {
                        content: crate::durable::types::DocumentContent::Base { .. },
                        ..
                    })
                    | Some(StorageWrite::DocumentCopy { .. })
            );
            inner.documents.apply_adopted(
                &plan.address_id,
                plan.version,
                is_base,
                plan.publication_value(),
            );
        }
    }

    // ── publish ─────────────────────────────────────────────────────────────
    // Finish the publication built before the storage call: the committed
    // sequence and, after the table changes (in write order), the adopted
    // document changes (in plan order). With no observer this is one `None`
    // branch and the adopted changes are dropped without being collected.
    let published = if !(inner.internal_listeners.is_empty() && inner.commit_listeners.is_empty()) {
        match publication.as_mut() {
            Some(publication) => {
                publication.seq = seq;
                publication
                    .changes
                    .extend(documents.into_iter().map(CommitChange::Document));
                publish(inner, publication, &job.context)
            }
            // Unreachable: `observed` is true whenever a listener is registered.
            None => Ok(()),
        }
    } else {
        Ok(())
    };
    // Retirement is terminal for an attached incarnation: release its pin and
    // evict the retired cache entry even when a listener panic failed this
    // commit. The observer keeps the terminal value it already received.
    if let Some(publication) = &publication {
        deliver_retired_attachments(inner, publication);
    }
    if let Err(message) = published {
        if !inner.latch.is_failed() {
            inner.fail(StorageError::invalid(format!(
                "A session commit listener failed: {message}"
            )));
        }
        let cause = inner
            .latch
            .failure()
            .unwrap_or_else(|| StorageError::invalid(message));
        let _ = job
            .reply
            .send(Err(crate::durable::errors::SessionFailed { cause }.into()));
        return;
    }
    // ── telemetry ───────────────────────────────────────────────────────────
    //
    // Observational only: with no configured handle this is one `None` branch,
    // and the typed helpers allocate nothing and reach no sink when the handle
    // carries none. The events read the already-built publication, so its
    // contents and ordering are untouched.
    if let Some(telemetry) = inner.telemetry.as_ref() {
        if let Some(publication) = &publication {
            emit_commit_events(telemetry, publication, &job.context);
        }
    }
    inner.sweep_documents();
    let _ = job.reply.send(Ok(value));
}

/// Record the committed-entry and settled-submission events of `publication`.
///
/// Events attach to the commit context's span when it carries one; otherwise
/// the context's trace carrier is offered to the sink as a remote parent.
/// `HarnessTelemetry` skips the sink entirely when it has none, so no
/// attributes are built and nothing is allocated in that configuration.
fn emit_commit_events(
    telemetry: &HarnessTelemetry,
    publication: &CommitPublication,
    context: &Context,
) {
    let span = context
        .span_handle()
        .filter(|span| !span.is_none())
        .or_else(|| telemetry.extract(context.trace_carrier()));
    for change in &publication.changes {
        match change {
            CommitChange::Table(TableCommitChange::Entry(entry)) => {
                telemetry.entry_committed(span, entry.id.as_i64(), &entry.kind);
            }
            CommitChange::Table(TableCommitChange::Submission(submission)) => {
                let status = match submission.status {
                    SubmissionStatus::Done => "done",
                    SubmissionStatus::Unanswered => "unanswered",
                    // Queued and placed submissions are not settled yet.
                    SubmissionStatus::Queued | SubmissionStatus::Placed => continue,
                };
                telemetry.submission_settled(
                    span,
                    submission.id.as_i64(),
                    status,
                    submission.answer.map(|answer| answer.as_i64()),
                );
            }
            // Conversations, tasks and documents carry no entry/submission
            // event in the durable telemetry vocabulary.
            CommitChange::Table(
                TableCommitChange::Conversation(_) | TableCommitChange::Task(_),
            )
            | CommitChange::Document(_) => {}
        }
    }
}

/// Run the close listeners, drain in-flight calls, close the backend.
async fn shutdown(inner: &mut SessionInner) -> Result<(), StorageError> {
    inner.closing = true;
    // The pending eviction only frees memory; close cancels it (upstream
    // `#notifyClose`).
    inner.disarm_document_sweep();
    inner.disarm_hold_timer();
    run_close_listeners(inner);
    // Close stores what deferred commits still hold, after every admitted
    // commit settled and every close listener ran, and before the backend
    // closes (upstream `close()`'s `#storeHolds`).
    if !inner.holds.is_empty() && !inner.latch.is_failed() {
        // A failure is already the session's end reason and the held changes
        // are lost; otherwise a failed store latches the failure itself.
        let _ = flush_holds(inner).await;
    }
    inner.clear_holds();
    inner.commit_listeners.clear();
    inner.internal_listeners.clear();
    inner.attachments.clear();
    inner.documents.clear();
    let result = inner.storage.close().await;
    if let Err(error) = &result {
        // A backend that cannot close is a failed one; an earlier failure
        // stays the cause.
        if !inner.latch.is_failed() {
            inner.fail(error.clone());
        }
    }
    // Settle `closed()`: a normal close ends `Closed`; a failure that reached
    // this point (a failing backend close) ends `Failed`. The failure path that
    // did not reach `shutdown` settles in `fail_close`, after its close
    // listeners ran.
    let end = match inner.latch.failure() {
        Some(error) => SessionEnd::Failed(error),
        None => SessionEnd::Closed,
    };
    let _ = inner.end_tx.send(Some(end));
    result
}

/// Run every registered close listener, containing panics like any host
/// callback. Shared by explicit close and by the fail→close path.
fn run_close_listeners(inner: &mut SessionInner) {
    let listeners = std::mem::take(&mut inner.close_listeners);
    for listener in listeners {
        run_close_listener(inner, &listener);
    }
    let subscriptions = std::mem::take(&mut inner.close_subscriptions);
    for (_, listener) in subscriptions {
        run_close_listener(inner, &listener);
    }
}

/// Run one close listener, containing its panic and reporting it.
fn run_close_listener(inner: &mut SessionInner, listener: &CloseListener) {
    // Each runs on its own: one that fails is reported, and the rest still
    // run (upstream `#notifyClose`).
    if let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| listener())) {
        let message = payload
            .downcast_ref::<&str>()
            .map(|text| (*text).to_owned())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "close listener panicked".to_owned());
        inner.report(&message, ReportInfo::ListenerError);
    }
}

/// Release every attachment whose incarnation retired in `publication`.
///
/// The registered internal listener already advanced the observer to the
/// terminal frame. This pass runs on the line, where [`deliver`] can unpin the
/// incarnation and drop its retired cache entry (a listener cannot reach
/// [`SessionInner`](crate::durable::session::line::SessionInner)).
fn deliver_retired_attachments(inner: &mut SessionInner, publication: &CommitPublication) {
    let retired = publication.changes.iter().any(|change| {
        matches!(
            change,
            CommitChange::Document(DocumentCommitChange::Document { value: None, .. })
        )
    });
    if !retired {
        return;
    }
    let attachments = inner.attachments.clone();
    for attached in &attachments {
        deliver(inner, attached, publication);
    }
}

/// The publication step, run synchronously on the line after adoption.
///
/// An **internal** listener panic fails the session, because its own state (the
/// scheduler's mirror of live tasks, the submission queue) would otherwise lag
/// behind storage — but it must not abort the fan-out: every remaining internal
/// listener and every host listener still receives the publication, exactly as
/// upstream `#publish` does (`session.ts:807-817`). A host listener's panic is
/// reported and never fails the commit.
pub fn publish(
    inner: &mut SessionInner,
    publication: &CommitPublication,
    context: &Context,
) -> Result<(), String> {
    let span = inner.telemetry.as_ref().and_then(|telemetry| {
        context
            .span_handle()
            .filter(|span| !span.is_none())
            .or_else(|| telemetry.extract(context.trace_carrier()))
    });
    let mut first_error = None;
    let internal = inner.internal_listeners.snapshot();
    for listener in &internal {
        if let Err(message) = run_contained(listener, publication, context) {
            if first_error.is_none() {
                // Latch the first failure immediately, but defer the close half
                // until retirement has been delivered below: a close listener
                // must not preempt the terminal retirement frame.
                inner.latch_failure(StorageError::invalid(format!(
                    "A session commit listener failed: {message}"
                )));
                first_error = Some(message.clone());
            }
            // Every internal listener that panics is reported; dispatch still
            // continues through the remaining internal and host listeners.
            inner.report_with_span(&message, ReportInfo::ListenerError, span);
        }
    }
    let host = inner.commit_listeners.snapshot();
    for listener in &host {
        // A failing host listener neither fails the commit nor skips the
        // others; it is reported as a listener error (upstream `contained`).
        if let Err(message) = run_contained(listener, publication, context) {
            inner.report_with_span(&message, ReportInfo::ListenerError, span);
        }
    }
    match first_error {
        Some(message) => {
            // A retirement publication must reach `deliver` before fail_close
            // ends the attachment sources.
            deliver_retired_attachments(inner, publication);
            inner.fail_close();
            Err(message)
        }
        None => Ok(()),
    }
}
