//! Per-runtime placeholder-commit and tool-batch boundary tracking for
//! detached (background) tool calls.
//!
//! A backgrounded tool call is answered inline with a synthesized placeholder
//! `tool_result`; the real result is delivered later as a
//! `backgroundToolResult` custom entry plus a follow-up frame. Two ordering
//! constraints make that delivery safe:
//!
//! 1. **Placeholder before completion (A1).** The custom entry can only be
//!    appended after the batch's placeholder `tool_result` for the enclosing
//!    call was persisted. A completion task waits on a
//!    [`PlaceholderCommitGate`]; the agent loop opens the gate for a call id
//!    once the placeholder write for that id returned.
//! 2. **Batch boundary (B1).** A custom entry projects as a user message on
//!    resume (`session_manager::projection`), so appending it between the
//!    assistant message that carries a tool batch and that batch's
//!    `tool_result`s would split the batch: on resume the provider would see
//!    an assistant tool call separated from one of its results by a user
//!    message. Completions therefore persist only while no tool batch is
//!    open. The loop holds the batch barrier ([`CommitTracker::begin_batch`])
//!    from the assistant message write through the last result write of the
//!    batch; a completion acquires the same barrier
//!    ([`CommitTracker::wait_for_batch_boundary`]) before persisting, so it
//!    lands strictly between batches (or before the first one).
//!
//! One tracker belongs to one runtime (an `AgentSession`'s background
//! registry, or the print host's), never to the process: two sessions running
//! concurrently with the same provider call ids must not observe each other's
//! gates. Both structures are small and bounded (see
//! [`COMMITTED_CALL_IDS_BOUND`] / [`PENDING_KEYS_BOUND`]) because the only
//! callers are the in-flight background registry and the loop's own batches.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use tokio::sync::Notify;

/// Bound of the "already committed" call-id ring. It only needs to remember
/// recent writes so a background call that spawns a nested background call
/// *after* its own placeholder was persisted does not wait for a signal that
/// already happened.
const COMMITTED_CALL_IDS_BOUND: usize = 1024;

/// Bound of the pending (uncommitted) key table. Entries are registered per
/// in-flight background call and released at the next batch boundary, so this
/// is far above any legitimate depth; the eviction rule below is a memory
/// guard, never a normal path.
const PENDING_KEYS_BOUND: usize = 4096;

/// Wait bound for a completion whose placeholder signal never arrives (a run
/// torn down between the batch and its persistence writes). The ordered path
/// never waits this long; the bound only keeps a completion task from hanging
/// forever and logs the degradation.
pub(crate) const PLACEHOLDER_COMMIT_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// One call's "placeholder committed" gate.
///
/// Every call id whose placeholder write opens this gate is tracked in `keys`
/// (a nested detach registers its requesting chain, i.e. every `/`-delimited
/// prefix of its parent id; see [`commit_keys_for`]).
pub(crate) struct PlaceholderCommitGate {
    keys: Vec<String>,
    committed: AtomicBool,
    notify: Notify,
}

impl PlaceholderCommitGate {
    fn new(keys: Vec<String>) -> Self {
        Self {
            keys,
            committed: AtomicBool::new(false),
            notify: Notify::new(),
        }
    }

    /// Open the gate (idempotent).
    pub(crate) fn commit(&self) {
        if !self.committed.swap(true, Ordering::SeqCst) {
            self.notify.notify_waiters();
        }
    }

    /// Whether the gate has been opened (gate-observable state; [`GateProbe`]
    /// exposes it to tests).
    pub(crate) fn is_committed(&self) -> bool {
        self.committed.load(Ordering::SeqCst)
    }

    /// Wait until the gate is committed. Register-before-recheck so a commit
    /// racing the wait can never be missed (`notify_waiters` only wakes
    /// waiters already registered).
    pub(crate) async fn wait(&self) {
        if self.committed.load(Ordering::SeqCst) {
            return;
        }
        let notified = self.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if self.committed.load(Ordering::SeqCst) {
            return;
        }
        notified.await;
    }
}

/// Call-id bookkeeping for one runtime.
#[derive(Default)]
struct GateTable {
    pending: HashMap<String, Vec<Arc<PlaceholderCommitGate>>>,
    /// Insertion order of the keys that currently have pending gates (the
    /// eviction rule below pops the oldest).
    pending_order: VecDeque<String>,
    committed: HashSet<String>,
    committed_order: VecDeque<String>,
}

impl GateTable {
    fn mark_committed(&mut self, key: &str) {
        if !self.committed.insert(key.to_owned()) {
            return;
        }
        self.committed_order.push_back(key.to_owned());
        while self.committed.len() > COMMITTED_CALL_IDS_BOUND {
            if let Some(oldest) = self.committed_order.pop_front() {
                self.committed.remove(&oldest);
            }
        }
    }

    /// Evict the oldest pending key, opening its gates. Only reachable past
    /// [`PENDING_KEYS_BOUND`]; a gate opened early can at worst let one
    /// completion land a little sooner (never earlier than a placeholder that
    /// never commits).
    fn evict_oldest_pending(&mut self) {
        // Skip queue entries whose key was already committed and removed:
        // `pending_order` can retain no-longer-pending keys across the
        // remove-then-reinsert cycles below, and popping an empty one must not
        // stop the eviction walk early.
        while let Some(key) = self.pending_order.pop_front() {
            let Some(gates) = self.pending.remove(&key) else {
                continue;
            };
            for gate in gates {
                gate.commit();
                // Stale peer copies under the gate's other keys are harmless
                // (their commit is idempotent); they are dropped when those keys
                // commit or the gate is explicitly released.
            }
            return;
        }
    }

    /// Remove a pending key and its queue entry together. The queue is a
    /// separate FIFO only for deterministic eviction; leaving a removed key in
    /// it made `pending_order` grow by one entry per committed call for the
    /// lifetime of the runtime.
    fn remove_pending(&mut self, key: &str) -> Option<Vec<Arc<PlaceholderCommitGate>>> {
        let gates = self.pending.remove(key)?;
        self.pending_order.retain(|queued| queued != key);
        Some(gates)
    }

    fn register(&mut self, key: &str, gate: &Arc<PlaceholderCommitGate>) {
        if !self.pending.contains_key(key) {
            if self.pending.len() >= PENDING_KEYS_BOUND {
                self.evict_oldest_pending();
            }
            self.pending_order.push_back(key.to_owned());
        }
        self.pending
            .entry(key.to_owned())
            .or_default()
            .push(Arc::clone(gate));
    }

    fn remove_peer(&mut self, key: &str, gate: &PlaceholderCommitGate) {
        let mut emptied = false;
        if let Some(peer) = self.pending.get_mut(key) {
            peer.retain(|candidate| !std::ptr::eq(&**candidate, gate));
            emptied = peer.is_empty();
        }
        if emptied {
            self.pending.remove(key);
            self.pending_order.retain(|queued| queued != key);
        }
    }
}

/// Per-runtime gate table plus the tool-batch boundary barrier.
#[derive(Default)]
pub struct CommitTracker {
    table: Mutex<GateTable>,
    /// Held by the agent loop for the duration of one tool batch (from the
    /// assistant-message write through the last tool-result write) and
    /// acquired by a background completion immediately before it persists.
    /// The mutex is the barrier: a completion that acquires it while a batch
    /// is open waits until the batch closes (and the loop's next batch, if it
    /// starts first, is ordered after the waiting completion by tokio's FIFO
    /// handoff).
    batch: tokio::sync::Mutex<()>,
    /// Batch-open marker (observation and tests): `true` while the loop holds
    /// the barrier for a tool batch.
    batch_open: AtomicBool,
}

impl CommitTracker {
    /// Build an empty tracker (one per runtime).
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn lock_table(&self) -> std::sync::MutexGuard<'_, GateTable> {
        self.table.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Register a gate for `keys`; a key already committed means the enclosing
    /// placeholder was persisted before this (late) background call started,
    /// so the returned gate starts open.
    pub(crate) fn register(&self, keys: Vec<String>) -> Arc<PlaceholderCommitGate> {
        let gate = Arc::new(PlaceholderCommitGate::new(keys.clone()));
        let mut table = self.lock_table();
        if keys.iter().any(|key| table.committed.contains(key)) {
            gate.commit();
            return gate;
        }
        for key in &keys {
            table.register(key, &gate);
        }
        gate
    }

    /// `#[doc(hidden)]` test seam: register a gate for `keys` and hand back a
    /// probe with the observable half of its lifecycle (the gate type itself
    /// stays crate-private).
    #[doc(hidden)]
    pub fn register_for_test(&self, keys: Vec<String>) -> GateProbe {
        GateProbe {
            gate: self.register(keys),
        }
    }

    /// `#[doc(hidden)]` test seam: hold the batch barrier until the returned
    /// guard drops (what the agent loop does for one tool batch).
    #[doc(hidden)]
    pub async fn begin_batch_for_test(&self) -> BatchGuard<'_> {
        self.begin_batch().await
    }

    /// `#[doc(hidden)]` test seam: commit `key` the way the loop does after a
    /// batch's results are persisted.
    #[doc(hidden)]
    pub fn commit_key_for_test(&self, key: &str) {
        self.commit_key(key);
    }

    /// `#[doc(hidden)]` test seam: release a probe's gate without waiting.
    #[doc(hidden)]
    pub fn release_for_test(&self, probe: &GateProbe) {
        self.release(&probe.gate);
    }

    /// Release every gate registered for `key` (the agent loop calls this once
    /// the whole batch's results are persisted).
    pub(crate) fn commit_key(&self, key: &str) {
        let mut table = self.lock_table();
        // Mark even when no gate waits on this key: a background call that
        // spawns a nested background call later registers a gate under this id
        // and must see the placeholder as already written.
        table.mark_committed(key);
        let Some(list) = table.remove_pending(key) else {
            return;
        };
        for gate in list {
            gate.commit();
            for other in &gate.keys {
                if other == key {
                    continue;
                }
                table.remove_peer(other, &gate);
            }
        }
    }

    /// Drop a gate without waiting (aborted call / sink rollback / already
    /// delivered). A stale peer copy under another key is removed too.
    pub(crate) fn release(&self, gate: &PlaceholderCommitGate) {
        let mut table = self.lock_table();
        for key in &gate.keys {
            table.remove_peer(key, gate);
        }
        gate.commit();
    }

    /// Begin a tool batch: the returned guard holds the boundary barrier until
    /// it is dropped.
    pub(crate) async fn begin_batch(&self) -> BatchGuard<'_> {
        let guard = self.batch.lock().await;
        self.batch_open.store(true, Ordering::SeqCst);
        BatchGuard {
            tracker: self,
            _guard: guard,
        }
    }

    /// Wait until no tool batch is open, WITHOUT publishing the batch-open
    /// marker. This is the completion path (a `backgroundToolResult` write
    /// must land at a batch boundary); `begin_batch` is the agent loop's own
    /// hold, which additionally marks the batch as open for observation.
    pub(crate) async fn wait_for_batch_boundary(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.batch.lock().await
    }

    /// `#[doc(hidden)]` test seam for [`Self::wait_for_batch_boundary`].
    #[doc(hidden)]
    pub async fn wait_for_batch_boundary_for_test(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.wait_for_batch_boundary().await
    }

    /// Whether a tool batch is open right now (observation/testing only).
    pub fn batch_is_open(&self) -> bool {
        self.batch_open.load(Ordering::SeqCst)
    }

    /// Close the batch-open marker ([`BatchGuard`]'s drop).
    fn close_batch(&self) {
        self.batch_open.store(false, Ordering::SeqCst);
    }
}

/// RAII guard for one open tool batch: dropping it closes the observation
/// marker and releases the barrier (the barrier's own `MutexGuard` releases
/// when this guard's field drops).
pub struct BatchGuard<'a> {
    tracker: &'a CommitTracker,
    _guard: tokio::sync::MutexGuard<'a, ()>,
}

impl Drop for BatchGuard<'_> {
    fn drop(&mut self) {
        self.tracker.close_batch();
    }
}

/// The call ids a detached call's completion waits on.
///
/// Model-issued calls (`parent_call_id == None`) wait on their own id: that
/// is the id of the placeholder result the loop persists. Nested calls wait
/// on every `/`-delimited prefix of their requesting id (nested ids are
/// `<caller>/<n>`, so the prefixes are the ancestor chain up to and including
/// the model-issued call whose result is actually persisted — any provider id
/// that itself contains `/` is covered because every prefix is registered).
pub(crate) fn commit_keys_for(call_id: &str, parent_call_id: Option<&str>) -> Vec<String> {
    match parent_call_id {
        None => vec![call_id.to_owned()],
        Some(parent) => {
            let mut keys: Vec<String> = parent
                .match_indices('/')
                .map(|(index, _)| parent[..index].to_owned())
                .collect();
            keys.push(parent.to_owned());
            keys
        }
    }
}

/// `#[doc(hidden)]` observable handle over one registered gate (tests only):
/// crate-private gates stay private while their lifecycle is assertable.
#[doc(hidden)]
pub struct GateProbe {
    gate: Arc<PlaceholderCommitGate>,
}

impl GateProbe {
    /// Whether the gate has been opened.
    pub fn is_committed(&self) -> bool {
        self.gate.is_committed()
    }

    /// Wait (bounded by the caller) for the gate to open.
    pub async fn wait(&self) {
        self.gate.wait().await;
    }
}

impl CommitTracker {
    /// Pending-gate counts for the integration tests (`src/` must not carry
    /// inline test modules). Returns `(pending, pending_order)`.
    #[doc(hidden)]
    pub fn pending_counts_for_test(&self) -> (usize, usize) {
        let table = self.lock_table();
        (table.pending.len(), table.pending_order.len())
    }
}
