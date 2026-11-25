//! Per-session task slots and the replay cache.
//!
//! The store is pure host-independent state: an LRU-bounded map of session
//! slots plus the active-session pointer. It owns the mutation seam (live
//! commits and transcript replay) while the reducer stays pure.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, RwLock};

use serde_json::Value as JsonValue;

use crate::constants::MAX_SESSION_SLOTS;
use crate::envelope::{build_response, TodoResponse};
use crate::mutation::apply_mutation;
use crate::params::{parse_batch_items, BatchItem, TodoParams};
use crate::replay::{scan_entries, snapshot_from_last_match};
use crate::types::{TaskAction, TodoOp, TodoState};

/// Counters backing CPU assertions and diagnostics. All are monotonic; tests
/// compare deltas.
#[derive(Debug, Default)]
pub struct TodoStats {
    /// Context pushes seen by the host.
    pub pushes: AtomicU64,
    /// Entries passes performed.
    pub scans: AtomicU64,
    /// Pushes whose fingerprint equaled the cached one (state untouched).
    pub replay_skips: AtomicU64,
    /// Entries examined across all passes (the O(entries) cost).
    pub entries_visited: AtomicU64,
    /// Task snapshots deserialized by replay.
    pub deserializations: AtomicU64,
    /// Slot replacements (replay or live commit).
    pub state_touches: AtomicU64,
    /// Tool calls executed.
    pub tool_calls: AtomicU64,
    /// Host event dispatches that did work.
    pub handled_events: AtomicU64,
}

/// Plain snapshot of [`TodoStats`] for assertions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TodoStatsSnapshot {
    pub pushes: u64,
    pub scans: u64,
    pub replay_skips: u64,
    pub entries_visited: u64,
    pub deserializations: u64,
    pub state_touches: u64,
    pub tool_calls: u64,
    pub handled_events: u64,
}

impl TodoStats {
    /// Read every counter (relaxed: counters, not synchronization).
    pub fn snapshot(&self) -> TodoStatsSnapshot {
        TodoStatsSnapshot {
            pushes: self.pushes.load(Ordering::Relaxed),
            scans: self.scans.load(Ordering::Relaxed),
            replay_skips: self.replay_skips.load(Ordering::Relaxed),
            entries_visited: self.entries_visited.load(Ordering::Relaxed),
            deserializations: self.deserializations.load(Ordering::Relaxed),
            state_touches: self.state_touches.load(Ordering::Relaxed),
            tool_calls: self.tool_calls.load(Ordering::Relaxed),
            handled_events: self.handled_events.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Clone)]
struct SessionSlot {
    state: TodoState,
    /// Fingerprint of the entries this slot was replayed from; `None` = never
    /// replayed (or invalidated by `session_start` / `/reload`).
    fingerprint: Option<u64>,
    /// LRU stamp.
    touched: u64,
}

/// Per-session task slots plus the active-session pointer.
pub struct TodoStore {
    sessions: Mutex<BTreeMap<String, SessionSlot>>,
    active: RwLock<String>,
    stats: TodoStats,
    lru: AtomicU64,
}

impl Default for TodoStore {
    fn default() -> Self {
        Self::new()
    }
}

impl TodoStore {
    pub fn new() -> Self {
        Self {
            sessions: Mutex::new(BTreeMap::new()),
            active: RwLock::new(String::new()),
            stats: TodoStats::default(),
            lru: AtomicU64::new(0),
        }
    }

    /// The session key live calls commit to (`""` before the first selection).
    pub fn active_session(&self) -> String {
        self.active
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Select the session slot live calls commit to.
    pub fn set_active_session(&self, session: &str) {
        *self
            .active
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = session.to_owned();
    }

    /// Counters for diagnostics and tests.
    pub fn stats(&self) -> &TodoStats {
        &self.stats
    }

    /// The number of live session slots (bounded by [`MAX_SESSION_SLOTS`]).
    #[doc(hidden)]
    pub fn session_slot_count(&self) -> usize {
        self.sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    /// Current state of a session (a fresh empty state when the slot is
    /// absent — never stored by the read).
    pub fn state(&self, session: &str) -> TodoState {
        self.sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(session)
            .map(|slot| slot.state.clone())
            .unwrap_or_default()
    }

    /// Run `f` against the session's state without cloning it. `None` when the
    /// session has no slot.
    pub fn with_state<R>(&self, session: &str, f: impl FnOnce(&TodoState) -> R) -> Option<R> {
        self.sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(session)
            .map(|slot| f(&slot.state))
    }

    /// Commit a whole state (replay replacement or an embedder/test seam).
    pub fn replace_state(&self, session: &str, state: TodoState) {
        let stamp = self.lru.fetch_add(1, Ordering::Relaxed);
        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Self::evict_over_capacity(&mut sessions, session);
        let entry = sessions
            .entry(session.to_owned())
            .or_insert_with(|| SessionSlot {
                state: TodoState::empty(),
                fingerprint: None,
                touched: stamp,
            });
        entry.state = state;
        entry.touched = stamp;
        self.stats.state_touches.fetch_add(1, Ordering::Relaxed);
    }

    /// Invalidate the active slot's replay cache so the next entries push is
    /// authoritative.
    pub fn invalidate_replay_cache(&self) {
        let active = self.active_session();
        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(slot) = sessions.get_mut(&active) {
            slot.fingerprint = None;
        }
    }

    /// Drop every slot.
    pub fn clear(&self) {
        self.sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        self.set_active_session("");
    }

    /// LRU-evict the least recently touched slot when the map is at capacity
    /// and the slot being written is new.
    fn evict_over_capacity(sessions: &mut BTreeMap<String, SessionSlot>, keep: &str) {
        if sessions.contains_key(keep) {
            return;
        }
        while sessions.len() >= MAX_SESSION_SLOTS {
            let victim = sessions
                .iter()
                .filter(|(key, _)| key.as_str() != keep)
                .min_by_key(|(_, slot)| slot.touched)
                .map(|(key, _)| key.clone());
            match victim {
                Some(victim) => {
                    sessions.remove(&victim);
                }
                None => break,
            }
        }
    }

    /// Replay one entries push into `session`'s slot: a cheap fingerprint pass
    /// first, and a state replacement — with exactly one deserialization of
    /// the last matching snapshot — only when the fingerprint moved (or the
    /// cache was invalidated). Returns whether the state was replaced.
    pub fn replay(&self, session: &str, entries: &[JsonValue]) -> bool {
        self.stats.scans.fetch_add(1, Ordering::Relaxed);
        let index = scan_entries(entries);
        self.stats
            .entries_visited
            .fetch_add(index.entries_visited, Ordering::Relaxed);
        let stamp = self.lru.fetch_add(1, Ordering::Relaxed);
        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Self::evict_over_capacity(&mut sessions, session);
        let slot = sessions
            .entry(session.to_owned())
            .or_insert_with(|| SessionSlot {
                state: TodoState::empty(),
                fingerprint: None,
                touched: stamp,
            });
        slot.touched = stamp;
        if slot.fingerprint == Some(index.fingerprint) {
            self.stats.replay_skips.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        // Phase two runs only on a moved fingerprint: deserialize the LAST
        // matching snapshot instead of every historical one.
        let (state, deserializations) = snapshot_from_last_match(entries, index.last_match);
        self.stats
            .deserializations
            .fetch_add(deserializations, Ordering::Relaxed);
        slot.state = state.unwrap_or_default();
        // More than one attempt means the last shape-valid snapshot failed to
        // deserialize and an earlier one won; that fallback is outside the
        // last-only fingerprint, so never cache it.
        slot.fingerprint = (deserializations <= 1).then_some(index.fingerprint);
        self.stats.state_touches.fetch_add(1, Ordering::Relaxed);
        true
    }

    /// Run one action against the active session slot.
    pub fn execute(
        &self,
        args: &JsonValue,
        action: TaskAction,
        params: &TodoParams,
    ) -> TodoResponse {
        self.stats.tool_calls.fetch_add(1, Ordering::Relaxed);
        let session = self.active_session();
        let stamp = self.lru.fetch_add(1, Ordering::Relaxed);
        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Self::evict_over_capacity(&mut sessions, &session);
        let slot = sessions.entry(session).or_insert_with(|| SessionSlot {
            state: TodoState::empty(),
            fingerprint: None,
            touched: stamp,
        });
        slot.touched = stamp;
        let op = apply_mutation(&mut slot.state, action, params);
        if !op.is_error() {
            self.stats.state_touches.fetch_add(1, Ordering::Relaxed);
        }
        // Serialize under the lock: one O(tasks) pass, no state clone.
        build_response(action.as_str(), args, &slot.state, &op)
    }

    /// Apply a parsed batch atomically: every item reduces against a scratch
    /// copy, and only a fully successful batch replaces the live state.
    pub fn execute_batch(&self, args: &JsonValue, items: &[BatchItem]) -> TodoResponse {
        self.stats.tool_calls.fetch_add(1, Ordering::Relaxed);
        let session = self.active_session();
        let stamp = self.lru.fetch_add(1, Ordering::Relaxed);
        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Self::evict_over_capacity(&mut sessions, &session);
        let slot = sessions.entry(session).or_insert_with(|| SessionSlot {
            state: TodoState::empty(),
            fingerprint: None,
            touched: stamp,
        });
        slot.touched = stamp;

        // A one-item batch needs no scratch copy: `apply_mutation` is
        // no-touch-on-error, so a failure leaves `slot.state` intact.
        if items.len() == 1 {
            let item = &items[0];
            let op = apply_mutation(&mut slot.state, item.action, &item.params);
            if let TodoOp::Error { message } = &op {
                let message = format!("batch item 1 ({}): {message}", item.action);
                return build_response(
                    TaskAction::Batch.as_str(),
                    args,
                    &slot.state,
                    &TodoOp::Error { message },
                );
            }
            self.stats.state_touches.fetch_add(1, Ordering::Relaxed);
            return build_response(TaskAction::Batch.as_str(), args, &slot.state, &op);
        }
        // `slot.state` stays untouched until the whole batch succeeds, so the
        // failure envelope can read it directly; only the working copy is new.
        let mut working = slot.state.clone();
        let mut ops = Vec::with_capacity(items.len());
        for (index, item) in items.iter().enumerate() {
            let op = apply_mutation(&mut working, item.action, &item.params);
            if let TodoOp::Error { message } = &op {
                let message = format!("batch item {} ({}): {message}", index + 1, item.action);
                return build_response(
                    TaskAction::Batch.as_str(),
                    args,
                    &slot.state,
                    &TodoOp::Error { message },
                );
            }
            ops.push(op);
        }
        slot.state = working;
        self.stats.state_touches.fetch_add(1, Ordering::Relaxed);
        build_response(
            TaskAction::Batch.as_str(),
            args,
            &slot.state,
            &TodoOp::Batch { ops },
        )
    }

    /// Parse and execute one raw argument object (the pure tool entry point).
    pub fn dispatch(&self, args: &JsonValue) -> TodoResponse {
        let action_name = args.get("action").and_then(JsonValue::as_str).unwrap_or("");
        let Some(action) = TaskAction::parse(action_name) else {
            let state = self.state(&self.active_session());
            let op = TodoOp::error(format!("invalid params: unknown action \"{action_name}\""));
            return build_response(action_name, args, &state, &op);
        };
        let params = match TodoParams::parse(args) {
            Ok(params) => params,
            Err(message) => {
                let state = self.state(&self.active_session());
                return build_response(action.as_str(), args, &state, &TodoOp::error(message));
            }
        };
        if action == TaskAction::Batch {
            let items = match parse_batch_items(args) {
                Ok(items) => items,
                Err(message) => {
                    let state = self.state(&self.active_session());
                    return build_response("batch", args, &state, &TodoOp::error(message));
                }
            };
            return self.execute_batch(args, &items);
        }
        self.execute(args, action, &params)
    }
}
