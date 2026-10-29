//! Background tool-call ownership: registry, limit, sink, completion delivery
//! and abort APIs (design: `TOOL-BACKGROUND-SCOPE.md` §2.3/§2.4).
//!
//! Invariants (every one of them reachable ONLY through `background: true`):
//! - A backgrounded tool call gets a SYNTHESIZED placeholder tool result in
//!   the batch it was requested from; the real result is NEVER a second
//!   `ToolResultMessage` (provider pairing, `src/ai/api/transform.rs`), it is
//!   delivered as a `backgroundToolResult` custom entry plus a
//!   [`SessionEvent::BackgroundToolEnd`].
//! - ORDERING (A1): a completion's `backgroundToolResult` entry is persisted
//!   only after the batch's placeholder `tool_result` for the enclosing
//!   call was persisted. The agent loop releases a per-call commit gate right
//!   after its `persist_message` write ([`crate::agent::commit::CommitTracker`]);
//!   a custom entry that projected before its assistant/tool_result pair
//!   would leave the placeholder orphaned on resume (custom messages project
//!   as user messages in `session_manager::projection`).
//! - ORDERING (B1): a completion is persisted only at a tool-batch BOUNDARY.
//!   The loop holds the same runtime's batch barrier from the assistant
//!   message that carries a tool batch through that batch's last result, so a
//!   completion can never split an open batch (which would project as a user
//!   message between a tool call and its result).
//! - IDEMPOTENCE: one completion id delivers at most once (persist AND
//!   queue), so a completion racing the settle path cannot land twice.
//! - The detached future carries its OWN [`AbortSignal`], never the run
//!   signal: Esc (`AgentSession::abort`) must not cancel background calls.
//! - At most [`BackgroundRegistry::limit`] calls are in flight per session;
//!   over the limit the sink refuses and the scheduler synthesizes the
//!   refusal error (never a silent foreground fallback).
//! - Entries are keyed by a monotone `seq`, so deregistration on completion
//!   can never drop a concurrent sibling's abort handle (`bash_abort`
//!   pattern, `src/agent_session/bash.rs`). A completion whose entry is gone
//!   (aborted, session torn down) delivers NOTHING.
//! - The registry's gate table and batch barrier are PER SESSION, never
//!   process-wide ([`CommitTracker`] lives inside this registry), and a
//!   session-store swap / teardown bumps the registry generation so an
//!   in-flight call cannot deliver into a transcript that replaced it.
//!
//! The completion task owns plain `Arc` handles ([`BackgroundDelivery`]) — no
//! `Arc<AgentSession>` self-reference — and a [`DeliverySlot`] token whose
//! `Drop` releases the delivery count, so the registry's accounting is
//! symmetric on every exit path.

use super::inbox::Inbox;
use super::*;

use crate::agent::commit::{
    commit_keys_for, CommitTracker, PlaceholderCommitGate, PLACEHOLDER_COMMIT_WAIT,
};
use crate::agent::tool_batch::{DetachedToolCall, FinalizedToolCall, ToolResult};
use crate::agent::tool_execution::DetachSink;

/// One in-flight background tool call.
pub(crate) struct BackgroundCallEntry {
    /// Generation identity (see module docs).
    seq: u64,
    /// Registry generation at admission time. A session-store swap or a
    /// session teardown (`abort_all`) bumps the registry generation; a
    /// completion whose reservation predates the current generation delivers
    /// NOTHING (its result belongs to the transcript that is gone).
    generation: u64,
    /// Provider tool-call id of the call that was detached.
    call_id: String,
    /// Tool name, for status reporting and the completion frame.
    tool_name: String,
    /// The requesting call id when another tool made the detached call
    /// ([`DetachedToolCall::parent_call_id`]); `None` for model-issued
    /// calls. Delivered as `parentToolCallId` on the completion payload.
    parent_call_id: Option<String>,
    /// The detached future's OWN abort signal (never the run signal).
    abort: AbortSignal,
    /// Placeholder-commit gate (A1): the completion waits on it before
    /// persisting its `backgroundToolResult` entry, so the custom entry can
    /// never precede the batch's placeholder `tool_result`. Released by the
    /// agent loop after its `persist_message` write for the batch.
    commit: Arc<PlaceholderCommitGate>,
}

/// Read-only snapshot of one in-flight background call, as returned by
/// [`AgentSession::background_calls`].
#[derive(Debug, Clone, serde::Serialize)]
pub struct BackgroundCallStatus {
    /// Provider tool-call id of the detached call.
    pub call_id: String,
    /// Tool name.
    pub tool_name: String,
    /// The requesting call id when another tool made the detached call
    /// (`ctx.executeTool(name, args, { background: true })`); `None` for
    /// model-issued calls. This is the adoption-time value the completion
    /// payload reports as `parentToolCallId`, so a host can attribute an
    /// in-flight call to its caller before the completion arrives.
    pub parent_call_id: Option<String>,
}

/// Per-session registry of in-flight background tool calls.
///
/// The registry OWNS the runtime's [`CommitTracker`]: its gate table and its
/// batch barrier are scoped to this session (or to the print host), never to
/// the process, so two concurrent sessions that happen to use the same
/// provider call ids cannot interfere. The tracker is handed to the agent loop
/// through [`crate::agent::agent_loop::LoopDeps::commit_tracker`].
pub(crate) struct BackgroundRegistry {
    /// Maximum concurrent background calls (foreman decision: 4, the
    /// [`DEFAULT_BACKGROUND_CALL_LIMIT`]). Interior-mutable so a host can
    /// retune a live session ([`AgentSession::set_max_background_calls`]).
    limit: AtomicUsize,
    seq: AtomicU64,
    /// Bumped by [`Self::abort_all`] (session teardown / storage swap): every
    /// reservation from an older generation is stale and delivers nothing.
    generation: AtomicU64,
    entries: Mutex<Vec<BackgroundCallEntry>>,
    /// Bounded ring of completion ids already delivered (see
    /// [`Self::mark_delivered`]): makes persist + queue idempotent for one
    /// completion even if delivery is entered twice.
    delivered: Mutex<DeliveredIds>,
    /// Single-owner transition between a completion's follow-up enqueue and
    /// the settle path's drain (see [`Self::queue_follow_up`]). Closed under
    /// this mutex BEFORE the drain is issued, so a completion that observed a
    /// stale `run_settling` value can never enqueue after the drain and leave
    /// a stale frame for the next prompt.
    follow_up_queue: Mutex<FollowUpQueueState>,
    /// Per-runtime placeholder-commit gates and the tool-batch barrier.
    commit: Arc<CommitTracker>,
    /// Calls whose completion task has not finished DELIVERING yet: one per
    /// [`Self::try_reserve`], released by [`DeliverySlot`] when the completion
    /// task (or the sink's rollback path) drops it. The abortable `entries`
    /// count drains when a call settles, but its delivery (the event, the
    /// persisted entry, the follow-up injection) may still be in flight for
    /// one scheduler tick — the print host's exit wait must cover the
    /// delivery, so it waits on THIS count, never on `entries`.
    in_flight: AtomicUsize,
    /// Wakes [`Self::wait_for_idle`] waiters on every count change.
    idle: tokio::sync::Notify,
}

/// Bound of [`BackgroundRegistry::delivered`]. Completions are rare relative
/// to deltas; remembering the recent ids is enough to absorb a duplicate
/// delivery while keeping the set bounded.
const DELIVERED_IDS_BOUND: usize = 512;

/// Bounded completion-id ring (see [`BackgroundRegistry::mark_delivered`]).
#[derive(Default)]
struct DeliveredIds {
    ids: std::collections::HashSet<String>,
    order: std::collections::VecDeque<String>,
}

impl DeliveredIds {
    /// Record `call_id`; `false` when it was already present (a duplicate
    /// delivery that must not persist or queue a second time).
    fn insert(&mut self, call_id: &str) -> bool {
        if !self.ids.insert(call_id.to_owned()) {
            return false;
        }
        self.order.push_back(call_id.to_owned());
        while self.ids.len() > DELIVERED_IDS_BOUND {
            if let Some(oldest) = self.order.pop_front() {
                self.ids.remove(&oldest);
            }
        }
        true
    }
}

/// The delivery-side enqueue gate for background follow-up frames.
///
/// A store-backed run settles in two steps: the agent loop reaches its
/// terminal result (`mark_settling`) and the session drains the background
/// inbox so no stale frame leaks into the NEXT prompt (the durable
/// `backgroundToolResult` entry already projects into its context). Checking
/// an `AtomicBool` after persistence and then sending is a TOCTOU: a
/// completion can read "not settling", lose the CPU, and enqueue after the
/// drain — the frame would then be injected on top of the entry's projection
/// (the same completion delivered twice). `close` and `enqueue` therefore
/// share this mutex: whichever runs first wins the transition, and a send that
/// starts before `close` completes is always visible to the drain issued
/// after it.
#[derive(Default)]
pub(crate) struct FollowUpQueueState {
    closed: bool,
}

impl FollowUpQueueState {
    fn open(&mut self) {
        self.closed = false;
    }

    fn close(&mut self) {
        self.closed = true;
    }
}

/// One reservation's release token: the delivery count is decremented exactly
/// once, when the token drops (normal completion, an early return, a panic
/// unwind, or the sink's rollback path). Holding it in the spawned task —
/// instead of calling a bare `finish_done` at the end — is what makes the
/// pending count symmetric with [`BackgroundRegistry::try_reserve`].
pub(crate) struct DeliverySlot {
    registry: Arc<BackgroundRegistry>,
    released: bool,
}

impl DeliverySlot {
    fn release(&mut self) {
        if self.released {
            return;
        }
        self.released = true;
        self.registry.release_delivery();
    }
}

impl Drop for DeliverySlot {
    fn drop(&mut self) {
        self.release();
    }
}

/// Default concurrent-background-call limit (§2.6).
pub(crate) const DEFAULT_BACKGROUND_CALL_LIMIT: usize = 4;

impl BackgroundRegistry {
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            // A zero limit would make `background: true` unusable; keep at
            // least one slot so the configured knob can never wedge the
            // feature off silently.
            limit: AtomicUsize::new(limit.max(1)),
            seq: AtomicU64::new(0),
            generation: AtomicU64::new(0),
            entries: Mutex::new(Vec::new()),
            delivered: Mutex::new(DeliveredIds::default()),
            follow_up_queue: Mutex::new(FollowUpQueueState::default()),
            commit: CommitTracker::new(),
            in_flight: AtomicUsize::new(0),
            idle: tokio::sync::Notify::new(),
        }
    }

    /// This runtime's commit tracker (the agent loop installs it on
    /// `LoopDeps::commit_tracker`).
    pub(crate) fn commit_tracker(&self) -> Arc<CommitTracker> {
        Arc::clone(&self.commit)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<BackgroundCallEntry>> {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Number of in-flight calls.
    pub(crate) fn len(&self) -> usize {
        self.lock().len()
    }

    /// Number of calls whose completion task has not finished delivering yet
    /// (see the field docs): `len()` plus the settled calls mid-`finish`.
    /// Only the print host polls/wait for this; a hosted session waits through
    /// its own run loop.
    #[cfg(feature = "print")]
    pub(crate) fn in_flight(&self) -> usize {
        self.in_flight.load(Ordering::SeqCst)
    }

    /// Wait until every reserved call has finished delivering, or `budget`
    /// elapses. Returns whether the registry drained inside the budget.
    /// `true` with nothing ever reserved is the instantaneous no-op.
    #[cfg(feature = "print")]
    pub(crate) async fn wait_for_idle(&self, budget: std::time::Duration) -> bool {
        let deadline = tokio::time::Instant::now() + budget;
        loop {
            if self.in_flight() == 0 {
                return true;
            }
            // Register the waiter BEFORE re-checking, so a delivery finishing
            // between the check and the await cannot be missed (`notify_waiters`
            // only wakes waiters already registered).
            let notified = self.idle.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.in_flight() == 0 {
                return true;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return self.in_flight() == 0;
            }
        }
    }

    /// Release one reserved call's delivery count (the other half of
    /// [`Self::try_reserve`]). Called exactly once per reservation: by
    /// [`DeliverySlot::drop`] on every exit path of the spawned completion
    /// task, and by the sink's rollback path when no task was spawned.
    fn release_delivery(&self) {
        let previous = self.in_flight.fetch_sub(1, Ordering::SeqCst);
        debug_assert!(previous > 0, "delivery released without a matching reserve");
        self.idle.notify_waiters();
    }

    /// Record that one completion id was delivered (persisted + queued).
    /// `false` means this id was already delivered: the caller must NOT
    /// persist or queue it a second time (idempotent delivery for a
    /// completion that races the settle path).
    pub(crate) fn mark_delivered(&self, call_id: &str) -> bool {
        self.delivered
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(call_id)
    }

    /// Number of completion ids currently remembered by the bounded delivery
    /// ring (observation/testing only; never grows past
    /// [`DELIVERED_IDS_BOUND`]).
    pub(crate) fn delivered_len(&self) -> usize {
        self.delivered
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .ids
            .len()
    }

    /// Open the follow-up enqueue gate at the start of a run.
    pub(crate) fn open_follow_up_queue(&self) {
        self.follow_up_queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .open();
    }

    /// Close the follow-up enqueue gate before the settle path drains the
    /// inbox (the single-owner half of the transition; see
    /// [`FollowUpQueueState`]).
    pub(crate) fn close_follow_up_queue(&self) {
        self.follow_up_queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .close();
    }

    /// Enqueue one settled completion's follow-up frame IFF the run is still
    /// live and has not started settling.
    ///
    /// The gate lock is held across the enqueue, so the settle transition
    /// (`close_follow_up_queue` + drain) is atomic with respect to this send:
    /// a send that wins the lock is queued before the drain command and
    /// dropped by it; a send that loses sees the closed state and does
    /// nothing. Returns whether the frame was enqueued.
    pub(crate) fn queue_follow_up(
        &self,
        session_state: &AtomicU8,
        inbox: &Inbox,
        framed: crate::agent::agent_loop::PromptMessage,
    ) -> bool {
        let gate = self
            .follow_up_queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if gate.closed || session_state.load(Ordering::SeqCst) != SESSION_RUNNING {
            return false;
        }
        inbox.send(framed);
        true
    }

    /// The current limit.
    pub(crate) fn limit(&self) -> usize {
        self.limit.load(Ordering::Relaxed)
    }

    /// Retune the limit for future admissions; in-flight calls are never
    /// cancelled by a shrink.
    pub(crate) fn set_limit(&self, limit: usize) {
        self.limit.store(limit.max(1), Ordering::Relaxed);
    }

    /// Admit a new call, returning its generation id plus the reservation's
    /// release token, or `None` when the limit is reached (the caller then
    /// refuses the detach and the scheduler synthesizes the refusal error
    /// result).
    ///
    /// `parent_call_id` is the requesting call id for a nested detach
    /// ([`DetachedToolCall::parent_call_id`], `ctx.executeTool` with
    /// `background: true`) and `None` for model-issued calls; it is stored
    /// with the entry so the completion delivery reports the same
    /// `parentToolCallId` the rest of the call's lifecycle does.
    pub(crate) fn try_reserve(
        self: &Arc<Self>,
        call_id: &str,
        tool_name: &str,
        parent_call_id: Option<&str>,
        abort: AbortSignal,
    ) -> Option<(u64, DeliverySlot)> {
        let mut guard = self.lock();
        if guard.len() >= self.limit() {
            return None;
        }
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        // A1: create the placeholder-commit gate in THIS runtime's tracker.
        // The key set is derived from the call's requesting chain, so the
        // agent loop can open it with the id of the tool result it persists.
        let commit = self
            .commit
            .register(commit_keys_for(call_id, parent_call_id));
        guard.push(BackgroundCallEntry {
            seq,
            generation: self.generation.load(Ordering::SeqCst),
            call_id: call_id.to_owned(),
            tool_name: tool_name.to_owned(),
            parent_call_id: parent_call_id.map(str::to_owned),
            abort,
            commit,
        });
        // The delivery count starts at reservation time: from here on the
        // registry is "busy" until the returned [`DeliverySlot`] drops (the
        // token makes the count symmetric with this increment on every exit
        // path, including a panic unwind).
        self.in_flight.fetch_add(1, Ordering::SeqCst);
        Some((
            seq,
            DeliverySlot {
                registry: Arc::clone(self),
                released: false,
            },
        ))
    }

    /// Deregister a settled call, handing its entry to the completion: the
    /// adoption-time metadata (the `parent_call_id` included) travels with it,
    /// so the delivery reports exactly what was registered. `None` means the
    /// call was aborted (session switch / Drop) — the completion must then
    /// deliver nothing.
    ///
    /// The registry's delivery count is NOT released here: the completion task
    /// is still delivering (or intentionally delivering nothing) and releases
    /// it through [`Self::finish_done`].
    pub(crate) fn settle(&self, seq: u64) -> Option<BackgroundCallEntry> {
        let mut guard = self.lock();
        let index = guard.iter().position(|entry| entry.seq == seq)?;
        let entry = guard.remove(index);
        self.idle.notify_waiters();
        Some(entry)
    }

    /// Abort and drop every in-flight call (session switch / Drop).
    /// Returns how many calls were cancelled.
    ///
    /// The registry generation is bumped first: a completion that already
    /// settled its entry (and is waiting for the batch boundary) observes the
    /// new generation and delivers nothing, so no stale result can reach a
    /// transcript that no longer belongs to the call.
    pub(crate) fn abort_all(&self) -> usize {
        self.generation.fetch_add(1, Ordering::SeqCst);
        let mut guard = self.lock();
        let count = guard.len();
        // A1: an aborted call delivers nothing, so its placeholder gate can
        // be dropped instead of waited on.
        let gates: Vec<Arc<PlaceholderCommitGate>> = guard
            .iter()
            .map(|entry| Arc::clone(&entry.commit))
            .collect();
        for entry in guard.drain(..) {
            entry.abort.abort();
        }
        drop(guard);
        for gate in gates {
            self.commit.release(&gate);
        }
        self.idle.notify_waiters();
        count
    }

    /// Abort one call by its provider tool-call id. Returns whether it was in
    /// flight (mirrors `AgentSession::abort_bash`, per id).
    pub(crate) fn abort(&self, call_id: &str) -> bool {
        let mut guard = self.lock();
        let mut found = false;
        let mut gate = None;
        guard.retain(|entry| {
            if !found && entry.call_id == call_id {
                found = true;
                entry.abort.abort();
                gate = Some(Arc::clone(&entry.commit));
                return false;
            }
            true
        });
        drop(guard);
        if let Some(gate) = gate {
            self.commit.release(&gate);
        }
        if found {
            self.idle.notify_waiters();
        }
        found
    }

    /// Whether a reservation belongs to a superseded registry generation
    /// (a session teardown / storage swap happened while it was delivering).
    fn is_stale(&self, generation: u64) -> bool {
        generation != self.generation.load(Ordering::SeqCst)
    }

    /// Snapshot of the in-flight calls.
    pub(crate) fn statuses(&self) -> Vec<BackgroundCallStatus> {
        self.lock()
            .iter()
            .map(|entry| BackgroundCallStatus {
                call_id: entry.call_id.clone(),
                tool_name: entry.tool_name.clone(),
                parent_call_id: entry.parent_call_id.clone(),
            })
            .collect()
    }
}

/// The session host's [`BackgroundControl`] adapter: the model-facing
/// `background_status` / `background_abort` tools read and abort through it.
///
/// One adapter is built per prompt iteration over the session's own registry
/// (`prompt_loop.rs`), so the model sees exactly the calls this session owns.
/// A subagent's child session builds its OWN registry (`build.rs`), hence its
/// own adapter: a child can never observe or abort the parent's calls.
pub(crate) struct SessionBackgroundControl {
    registry: Arc<BackgroundRegistry>,
}

impl SessionBackgroundControl {
    pub(crate) fn new(registry: Arc<BackgroundRegistry>) -> Self {
        Self { registry }
    }
}

impl crate::agent::tool_execution::BackgroundControl for SessionBackgroundControl {
    fn calls(&self) -> Vec<crate::agent::tool_execution::BackgroundCallInfo> {
        self.registry
            .statuses()
            .into_iter()
            .map(|status| crate::agent::tool_execution::BackgroundCallInfo {
                call_id: status.call_id,
                tool_name: status.tool_name,
                parent_call_id: status.parent_call_id,
            })
            .collect()
    }

    fn abort(&self, call_id: &str) -> bool {
        self.registry.abort(call_id)
    }

    fn abort_all(&self) -> usize {
        self.registry.abort_all()
    }
}

/// Host persistence sink for one delivered background result: the framed
/// transcript text plus the entry `details` payload, appended as a
/// `backgroundToolResult` custom entry. `None` on a store-less host
/// (ephemeral session, print `--no-session`) — the delivered event is then
/// the only durable surface.
///
/// The `details` payload is `{callId, toolName, status, durationMs, details}`
/// plus `usage` when the `tool_result` handler reported one (the custom entry
/// has no `ToolResultMessage.usage` field, so the key is the durable carrier
/// for that usage; `SessionEvent::BackgroundToolEnd.usage` mirrors it live).
///
/// The session host builds its closure over its `Arc<Mutex<SessionManager>>`
/// (`build.rs`); the print host builds its own over the print-mode
/// `SharedSession` (`coding_agent/print.rs`). The closure carries the exact
/// append semantics both share (`background_result_frame` content,
/// `display: false`, the entry details object).
pub(crate) type BackgroundPersistFn = Arc<
    dyn Fn(String, JsonValue) -> crate::agent::tool_execution::BoxFuture<'static, ()> + Send + Sync,
>;

/// Owned handles a background completion task needs (§2.3: no `Arc<Self>`
/// self-reference, so the task can outlive the borrowing batch and the
/// registry slot is released by the task itself).
#[derive(Clone)]
pub(crate) struct BackgroundDelivery {
    /// Host storage sink (see [`BackgroundPersistFn`]); `None` on a
    /// store-less host.
    persist: Option<BackgroundPersistFn>,
    /// Session-event subscribers (the RPC host / TUI / print host listen here).
    session_events: Arc<std::sync::RwLock<Vec<SessionEventHandler>>>,
    /// The follow-up queue, for streaming-time injection (§2.4.2).
    follow_up_inbox: Inbox,
    /// The session's run/compaction claim: only a RUNNING turn gets the
    /// framed follow-up; an idle session's completion lands in the transcript
    /// for the next prompt instead (the host's idle loop is what starts it).
    session_state: Arc<AtomicU8>,
    /// JS extension runtime for the `tool_result` dispatch (§2.4.4).
    extensions: Option<crate::extensions::ExtensionHandle>,
    /// Unified plugin host for the native half of the same `tool_result`
    /// dispatch (A9). `None` keeps the JS-only route; when either face is
    /// present the completion is dispatched through
    /// [`crate::agent::tool_execution::emit_tool_results_with_host`], so a
    /// Rust plugin's `tool_result` handler observes background completions
    /// exactly like the in-loop path. Attached by session construction
    /// (`agent_session/build.rs`, [`Self::with_plugins`]); the print host
    /// builds no unified host and keeps `None`.
    plugins: Option<Arc<PluginHost>>,
    /// The owning run's settling flag (`agent_session`'s `run_settling`).
    /// `Some` marks a SESSION host: its settle path closes the registry's
    /// follow-up enqueue gate (under the same mutex a completion uses to
    /// enqueue) and then drains the inbox, so a completion that lands after
    /// the loop's terminal result is persisted WITHOUT being queued (the
    /// durable custom entry is projected into the next prompt's context;
    /// queueing it too would deliver the same frame twice). `None` marks a
    /// host with no settle drain (print mode), which keeps the historical
    /// queue-while-running behavior.
    run_settling: Option<Arc<AtomicBool>>,
}

impl BackgroundDelivery {
    /// Bundles the session's owned handles for the completion task (called
    /// once at session construction, before the scheduler takes the sink).
    /// The native plugin face is attached separately with
    /// [`Self::with_plugins`].
    pub(crate) fn new(
        persist: Option<BackgroundPersistFn>,
        session_events: Arc<std::sync::RwLock<Vec<SessionEventHandler>>>,
        follow_up_inbox: Inbox,
        session_state: Arc<AtomicU8>,
        extensions: Option<crate::extensions::ExtensionHandle>,
    ) -> Self {
        Self {
            persist,
            session_events,
            follow_up_inbox,
            session_state,
            extensions,
            plugins: None,
            run_settling: None,
        }
    }

    /// Attach the owning run's settling flag (see the field docs). Kept
    /// separate from [`Self::new`] so the print host keeps its existing
    /// construction shape; session construction chains this in
    /// `agent_session/build.rs`.
    #[allow(dead_code)]
    pub(crate) fn with_run_settling(mut self, flag: Arc<AtomicBool>) -> Self {
        self.run_settling = Some(flag);
        self
    }

    /// Attach the session's unified plugin host (A9): the completion's
    /// `tool_result` dispatch then reaches native handlers as well as the JS
    /// extension face. Kept separate from [`Self::new`] so the print host —
    /// which builds no unified host — keeps its existing construction shape;
    /// session construction chains this at the `BackgroundDelivery::new` call
    /// site in `agent_session/build.rs`.
    // The session-construction call site lives out of this lane's file scope;
    // the allow covers the window until that one-line chain lands.
    #[allow(dead_code)]
    pub(crate) fn with_plugins(mut self, plugins: Option<Arc<PluginHost>>) -> Self {
        self.plugins = plugins;
        self
    }

    /// Deliver one settled background result: extension merge first (§2.4.4),
    /// then the session event, the `backgroundToolResult` entry, and the
    /// streaming follow-up injection.
    ///
    /// The settled call's registry entry is taken here, so the emitted
    /// `tool_result` reports the call's own id as `toolCallId` (the id the
    /// placeholder envelope carried) and, for a nested detach, the requesting
    /// call id as `parentToolCallId` — the adoption-time value the registry
    /// recorded; model-issued calls omit the key, exactly like the in-loop
    /// emission.
    ///
    /// Idempotent per completion id: the persist + queue pair runs at most
    /// once (`BackgroundRegistry::mark_delivered`), so a completion that is
    /// reached twice — a retry after a settle race, or a delivery racing the
    /// settle path — can neither duplicate the transcript entry nor the
    /// injected frame.
    // The delivery bundles the settled call's identity, metadata and result;
    // grouping them into a struct only to satisfy the lint would obscure the
    // call site for no readability gain (repo precedent: terminal.rs).
    #[allow(clippy::too_many_arguments)]
    async fn finish(
        &self,
        registry: &Arc<BackgroundRegistry>,
        seq: u64,
        call_id: &str,
        tool_name: &str,
        args: &JsonValue,
        result: ToolResult,
        duration_ms: Option<u64>,
    ) {
        // Aborted or torn down: the session no longer owns this call, so its
        // result must not reach the transcript (§2.5). The taken entry carries
        // the adoption-time metadata the delivery reports (`parent_call_id`).
        let Some(entry) = registry.settle(seq) else {
            return;
        };
        // A session teardown / storage swap while this call was in flight: the
        // transcript it belonged to is gone, so the result belongs nowhere.
        if registry.is_stale(entry.generation) {
            registry.commit.release(&entry.commit);
            return;
        }
        let mut finalized = FinalizedToolCall {
            index: 0,
            call_id: call_id.to_owned(),
            tool_name: tool_name.to_owned(),
            result,
            // A detached future returns only the ToolResult (the session
            // delivery is a background completion, not a tool-result message),
            // so afterToolCall usage has no carrier here.
            usage: None,
            duration_ms,
        };
        // §2.4.4: the `tool_result` handler runs before persistence, so a
        // late merge is part of the stored content (mirrors the in-loop
        // sequence in `agent_loop.rs`). Best-effort: dispatch failures are
        // ignored by `emit_tool_results_with_host` itself.
        //
        // The handler-reported usage is part of the merge payload. A
        // FOREGROUND result carries it on its `ToolResultMessage`, the only
        // message shape `session_stats` scans for tool-result usage; a
        // background completion is a `backgroundToolResult` custom entry, not
        // a tool-result message, so the parsed usage rides the completion
        // event and the persisted entry details instead. Parsing uses the
        // same lenient rule as the in-loop path (`parse_usage_json`), so an
        // unparseable payload is dropped exactly like a foreground one.
        let mut usage = None;
        // A9: the unified dispatcher runs the JS face AND the host's native
        // `tool_result` handlers (ordered-list order), exactly like the
        // in-loop path. Gate on either face so a host-less, extension-less
        // session keeps the old no-dispatch fast path.
        if self.extensions.is_some() || self.plugins.is_some() {
            let mut inputs = std::collections::HashMap::new();
            inputs.insert(call_id.to_owned(), args.clone());
            let merges = crate::agent::tool_execution::emit_tool_results_with_host(
                self.extensions.as_ref(),
                self.plugins.as_deref(),
                &inputs,
                std::slice::from_ref(&finalized),
                entry.parent_call_id.as_deref(),
            )
            .await;
            if let Some(merge) = merges.get(call_id) {
                usage = crate::agent::tool_execution::apply_tool_result_merge(
                    &mut finalized.result,
                    merge,
                )
                .and_then(|value| crate::agent::tool_execution::parse_usage_json(&value));
            }
        }
        let output = finalized.result.content.clone();
        let is_error = finalized.result.is_error;
        let details = finalized
            .result
            .details
            .clone()
            .unwrap_or_else(|| JsonValue::Object(serde_json::Map::new()));
        emit_session_event_to(
            &self.session_events,
            SessionEvent::BackgroundToolEnd {
                call_id: call_id.to_owned(),
                tool_name: tool_name.to_owned(),
                // The adoption-time attribution: a nested detach reports its
                // requesting call, a model-issued call carries no parent.
                parent_call_id: entry.parent_call_id.clone(),
                is_error,
                duration_ms,
                output: output.clone(),
                // The result's non-text surfaces travel with the completion:
                // a host replacing the placeholder block needs them, and the
                // event is otherwise the only non-transcript delivery surface.
                images: finalized.result.images.clone(),
                details: finalized.result.details.clone(),
                structured_content: finalized.result.structured_content.clone(),
                // See the usage note above: the completion event is the
                // real-time carrier (and the only surface on ephemeral /
                // `--no-session` hosts).
                usage,
            },
        );
        // Store-less hosts keep no transcript; the event above is the only
        // delivery surface then. The sink owns the append semantics through
        // [`Self::persist`] (the session's manager handle and the print
        // host's shared session both arrive here as the same closure shape);
        // the entry details are composed HERE so both hosts store the one
        // upstream shape `{callId, toolName, status, durationMs, details}`
        // plus `usage` when the handler reported it.
        let framed = background_result_frame(call_id, tool_name, is_error, duration_ms, &output);
        // Idempotent persist + queue for one completion id: a completion that
        // reaches this point twice (a settle race, a retried delivery) must
        // not duplicate the durable entry or the injected frame.
        if !registry.mark_delivered(call_id) {
            registry.commit.release(&entry.commit);
            return;
        }
        if let Some(persist) = self.persist.as_ref() {
            // A1 ordering barrier: custom messages project as user messages,
            // so a `backgroundToolResult` written before the batch's
            // placeholder `tool_result` would sit between the assistant tool
            // call and its result on resume and orphan the placeholder. The
            // agent loop commits this call's gate right after its
            // `persist_message` write for the placeholder, and holds the batch
            // barrier (B1) until the batch's last result, so this write lands
            // at a batch boundary. The bounded wait is a liveness valve for a
            // run torn down mid-write; it never fires on the ordered path.
            if tokio::time::timeout(PLACEHOLDER_COMMIT_WAIT, entry.commit.wait())
                .await
                .is_err()
            {
                tracing::warn!(
                    call_id = %call_id,
                    "background completion persisted before its placeholder was committed"
                );
            }
            let mut entry_details = serde_json::json!({
                "callId": call_id,
                "toolName": tool_name,
                "status": if is_error { "error" } else { "ok" },
                "durationMs": duration_ms,
                "details": details,
            });
            if let Some(usage) = usage {
                // The durable carrier for a handler-reported usage: a
                // `backgroundToolResult` entry is not a `ToolResultMessage`,
                // so the usage key here is the only place a store-backed
                // session keeps it (`SessionEvent::BackgroundToolEnd.usage`
                // is the live mirror). Omitted entirely when unreported,
                // matching the tool-result message's `skip_serializing_if`.
                entry_details["usage"] = serde_json::json!(usage);
            }
            // B1: never write inside an open tool batch. The barrier is
            // acquired here (after the A1 gate) and released as soon as the
            // write returns, so the entry lands strictly between batches.
            {
                let _batch_boundary = registry.commit.wait_for_batch_boundary().await;
                persist(framed.clone(), entry_details).await;
            }
        }
        // The gate has served its purpose (or this host keeps no store):
        // drop it so the runtime's table never retains a settled call.
        registry.commit.release(&entry.commit);
        // §2.4.2: a running turn is reached through the follow-up inbox (the
        // turn context is a snapshot, so an appended entry alone is invisible
        // to it); the loop polls it at every tool-batch boundary (and at a
        // terminal turn), so the framed result reaches a model that keeps
        // calling tools and always lands AFTER the batch's tool results. The
        // `mark_delivered` above makes this enqueue happen at most once.
        //
        // A SETTLING run has no poll left: queueing the frame there would
        // deliver it once through the next prompt's context projection (the
        // durable entry) and once through the stale inbox frame. Persist
        // only, then. The session host's enqueue goes through the registry's
        // single-owner gate: `mark_settling` closes it BEFORE the settle drain,
        // so the check and the send cannot be split by the transition (a
        // plain flag read here would be a TOCTOU).
        if self.run_settling.is_some() {
            registry.queue_follow_up(
                &self.session_state,
                &self.follow_up_inbox,
                crate::agent::agent_loop::PromptMessage::text(framed),
            );
        } else if self.session_state.load(Ordering::SeqCst) == SESSION_RUNNING {
            // Print host: no settle drain exists; the run claim alone decides
            // whether a live loop can poll the frame.
            self.follow_up_inbox
                .send(crate::agent::agent_loop::PromptMessage::text(framed));
        }
    }
}

/// The custom-message type tag of a delivered background result (§2.4.1).
///
/// The entry's `details` carry `{callId, toolName, status, durationMs,
/// details}`, plus `usage` when the extension `tool_result` handler reported
/// usage for the completion. The model-facing `content` stays the framed
/// `<background-tool-result>` text; the usage is accounting metadata and is
/// never injected into the follow-up frame.
pub(crate) const BACKGROUND_TOOL_RESULT_TYPE: &str = "backgroundToolResult";

/// Frame one background result as the model-facing text (§2.4.1/§2.4.6): the
/// tag makes a replayed transcript self-describing, and any transcript
/// position is legal for a custom entry.
pub(crate) fn background_result_frame(
    call_id: &str,
    tool_name: &str,
    is_error: bool,
    duration_ms: Option<u64>,
    output: &str,
) -> String {
    let status = if is_error { "error" } else { "ok" };
    let duration = duration_ms.map(|ms| ms.to_string()).unwrap_or_default();
    format!(
        "<background-tool-result tool=\"{tool_name}\" callId=\"{call_id}\" status=\"{status}\" durationMs=\"{duration}\">\n{output}\n</background-tool-result>"
    )
}

/// Opening marker of [`background_result_frame`].
pub(crate) const BACKGROUND_RESULT_FRAME_OPEN: &str = "<background-tool-result ";
/// Closing marker of [`background_result_frame`].
pub(crate) const BACKGROUND_RESULT_FRAME_CLOSE: &str = "</background-tool-result>";

/// Whether `text` is one of this module's completion frames.
///
/// The agent loop uses this to recognize a background-origin follow-up (A5):
/// the frame is already the durable content of the `backgroundToolResult`
/// custom entry, so the live injection must not ALSO persist a normal user
/// message carrying the same text (both project into the resumed context).
pub(crate) fn is_background_result_frame(text: &str) -> bool {
    let text = text.trim();
    text.starts_with(BACKGROUND_RESULT_FRAME_OPEN) && text.ends_with(BACKGROUND_RESULT_FRAME_CLOSE)
}

/// Build the scheduler detach sink: it adopts a `background: true` call when
/// the registry has capacity and spawns its future as an owned task.
///
/// The sink must be non-async (the scheduler calls it inside the batch), so it
/// spawns; `false` makes the scheduler synthesize the refusal error, never a
/// silent foreground fallback. The abort signal installed in the registry is
/// the CALL's own signal (already inside the future's [`ToolContext`]).
pub(crate) fn background_sink(
    registry: Arc<BackgroundRegistry>,
    delivery: BackgroundDelivery,
) -> DetachSink {
    Arc::new(move |call: &mut DetachedToolCall| {
        let Some(future) = call.take_future() else {
            return false;
        };
        let Some((seq, slot)) = registry.try_reserve(
            &call.call_id,
            &call.tool_name,
            call.parent_call_id.as_deref(),
            call.abort.clone(),
        ) else {
            return false;
        };
        // The batch only ever runs inside the agent loop's runtime; without
        // one nothing can run the call in the background, so refuse (the
        // future is dropped here — the call is answered with the refusal).
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            // Roll the reservation back: nothing adopted this call, and no
            // task will ever release the delivery count (or the A1 gate). The
            // drop of `slot` releases the count; the entry and its gate are
            // released here explicitly.
            if let Some(entry) = registry.settle(seq) {
                registry.commit.release(&entry.commit);
            }
            return false;
        };
        let registry = Arc::clone(&registry);
        let delivery = delivery.clone();
        let call_id = call.call_id.clone();
        let tool_name = call.tool_name.clone();
        let args = call.args.clone();
        handle.spawn(async move {
            // The token releases the registry's delivery count on EVERY exit
            // of this task: normal completion, an aborted call (`finish`
            // returns without delivering), a panic unwind. That symmetry is
            // what keeps `wait_for_idle` from hanging (or returning early).
            let _slot = slot;
            let started = std::time::Instant::now();
            let result = future.await;
            let duration_ms =
                Some(u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX));
            delivery
                .finish(
                    &registry,
                    seq,
                    &call_id,
                    &tool_name,
                    &args,
                    result,
                    duration_ms,
                )
                .await;
        });
        // The slot moved into the spawned task. If the runtime refused the
        // spawn (a shutting-down runtime), `handle.spawn` would have panicked
        // before this point only on a closed runtime — tokio's `spawn` on a
        // shutting-down handle still runs the future to completion or drops it
        // with the runtime, and the slot's `Drop` releases the count either
        // way.
        true
    })
}

/// The completion-follow-up channel of a host that drives the agent loop
/// itself (print mode): the delivery-side enqueue handle is the SAME inbox the
/// host hands the loop through `LoopDeps::follow_up`, so a completion that
/// settles while the host's run is live is injected as a follow-up message at
/// the next tool-batch boundary (and at a terminal turn) — exactly the
/// session's §2.4.2 route, without the session's steering/user-message queue
/// sharing the channel.
///
/// Print polls in `All` mode: a completion must not wait behind an earlier one
/// for a terminal turn that may never come (the loop also reaches its
/// follow-up poll after every completed tool batch).
#[cfg(feature = "print")]
pub(crate) struct BackgroundFollowUpChannel {
    inbox: Inbox,
}

#[cfg(feature = "print")]
impl BackgroundFollowUpChannel {
    pub(crate) fn new() -> Self {
        Self {
            inbox: Inbox::new(),
        }
    }

    /// The delivery-side enqueue handle (handed to [`BackgroundDelivery::new`]).
    pub(crate) fn inbox(&self) -> Inbox {
        self.inbox.clone()
    }

    /// The `LoopDeps::follow_up` poll over this channel.
    pub(crate) fn follow_up_fn(&self) -> crate::agent::agent_loop::FollowUpFn {
        let inbox = self.inbox.clone();
        Box::new(move || {
            let inbox = inbox.clone();
            Box::pin(async move { inbox.drain(QueueMode::All).await })
        })
    }
}

impl AgentSession {
    /// Abort one in-flight background tool call by its provider tool-call id
    /// (the `abort_bash` mirror). Returns whether the call was in flight; a
    /// call that already settled is a no-op `false`.
    pub fn abort_background(&self, call_id: &str) -> bool {
        self.background.abort(call_id)
    }

    /// Abort every in-flight background tool call, returning how many were
    /// cancelled (the session-switch / Drop path, §2.5).
    pub fn abort_all_background(&self) -> usize {
        self.background.abort_all()
    }

    /// Snapshot of the in-flight background tool calls.
    pub fn background_calls(&self) -> Vec<BackgroundCallStatus> {
        self.background.statuses()
    }

    /// How many background calls are in flight right now. The count-only
    /// accessor a status indicator polls; [`Self::background_calls`] allocates
    /// the full status snapshot, this one does not.
    pub fn background_call_count(&self) -> usize {
        self.background.len()
    }

    /// How many background calls may run concurrently
    /// ([`DEFAULT_BACKGROUND_CALL_LIMIT`] unless retuned).
    pub fn max_background_calls(&self) -> usize {
        self.background.limit()
    }

    /// Retune the concurrent background-call limit for future admissions;
    /// already-running calls are unaffected. `AgentSessionConfig` has no
    /// field for this yet (adding one would break every struct-literal
    /// construction site), so the host calls this right after construction —
    /// the default is [`DEFAULT_BACKGROUND_CALL_LIMIT`].
    pub fn set_max_background_calls(&self, limit: usize) {
        self.background.set_limit(limit);
    }
}

/// `#[doc(hidden)]` deterministic harness over the registry's follow-up
/// enqueue gate and bounded delivery ring.
///
/// The enqueue/close pair is the single-owner transition the settle path
/// relies on; integration tests drive it directly instead of racing a real
/// run (the mutex ordering is the property under test, and it is impossible
/// to interleave deterministically from the outside).
#[doc(hidden)]
pub struct FollowUpQueueProbe {
    registry: Arc<BackgroundRegistry>,
    state: Arc<AtomicU8>,
    inbox: Inbox,
}

#[doc(hidden)]
impl FollowUpQueueProbe {
    pub fn new() -> Self {
        Self {
            registry: Arc::new(BackgroundRegistry::new(DEFAULT_BACKGROUND_CALL_LIMIT)),
            state: Arc::new(AtomicU8::new(SESSION_RUNNING)),
            inbox: Inbox::new(),
        }
    }

    /// What `SessionStreamingGuard::mark_settling` does for a store-backed
    /// run: close the enqueue gate before the drain.
    pub fn close(&self) {
        self.registry.close_follow_up_queue();
    }

    /// What the next `prompt()` does before installing its guard: claim the
    /// session for the new run ([`SESSION_RUNNING`], the same order as
    /// `start_run`'s CAS) and only then reopen the enqueue gate. The state
    /// store is load-bearing: an idle session refuses frames even with the
    /// gate open, so reopening alone would leave the next run's queue closed
    /// to the completions it is supposed to accept.
    pub fn open(&self) {
        self.state.store(SESSION_RUNNING, Ordering::SeqCst);
        self.registry.open_follow_up_queue();
    }

    /// A completion's enqueue attempt (`BackgroundDelivery::finish` tail).
    pub fn try_queue(&self, text: &str) -> bool {
        self.registry.queue_follow_up(
            &self.state,
            &self.inbox,
            crate::agent::agent_loop::PromptMessage::text(text),
        )
    }

    /// Marks the run idle (what the guard's Drop does before the next run).
    pub fn mark_idle(&self) {
        self.state.store(SESSION_IDLE, Ordering::SeqCst);
    }

    /// Frames queued but not drained.
    pub async fn queued(&self) -> Vec<String> {
        self.inbox
            .peek()
            .await
            .into_iter()
            .map(|prompt| prompt.text_content())
            .collect()
    }

    /// The settle drain.
    pub async fn drain_all(&self) -> Vec<String> {
        self.inbox
            .drain_all()
            .await
            .into_iter()
            .map(|prompt| prompt.text_content())
            .collect()
    }

    /// The completion-id marker (`BackgroundDelivery::finish`).
    pub fn mark_delivered(&self, call_id: &str) -> bool {
        self.registry.mark_delivered(call_id)
    }

    /// Current size of the bounded delivery ring.
    pub fn delivered_len(&self) -> usize {
        self.registry.delivered_len()
    }
}

impl Default for FollowUpQueueProbe {
    fn default() -> Self {
        Self::new()
    }
}
