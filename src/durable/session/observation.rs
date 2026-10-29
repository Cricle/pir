//! Committed document observation: states and watches.
//!
//! Port of upstream `session/observation.ts`. Two observer faces sit on top of
//! the session's commit publications:
//!
//! * [`CommittedStateSource`] — upstream's chord `ReplicatedStateSource`. A
//!   consumer attaches once: it gets an atomic snapshot of the value at the
//!   attachment boundary plus every later committed frame, and the session
//!   never invokes consumer code inline (delivery is buffered and drained off
//!   the line).
//! * [`CommittedWatch`] — the serialized exact-frame watch. It never starts two
//!   callbacks at once, and after `MAX_PENDING_WATCH_FRAMES` undelivered frames
//!   it replaces the whole pending suffix with one root replacement, so a slow
//!   consumer sees convergent state rather than an unbounded queue.
//!
//! Both are bound to one concrete incarnation: retirement queues the terminal
//! `[["r", null]]` frame and then closes the observer; it never follows a
//! replacement at the same logical address.

use std::any::Any;
use std::sync::{Arc, Mutex};

use serde_json::{Map, Value};

use crate::durable::context::Context;
use crate::durable::delta::Op;
use crate::durable::errors::StorageError;

/// Upstream `MAX_PENDING_WATCH_FRAMES`: the undelivered suffix is collapsed to
/// one root replacement past this many frames.
pub const MAX_PENDING_WATCH_FRAMES: usize = 100;

/// The terminal frame of a retired incarnation (upstream
/// `RETIREMENT_OPERATIONS`).
#[must_use]
pub fn retirement_operations() -> Vec<Op> {
    vec![Op::Replace(Value::Null)]
}

/// What an observer holds: one exact committed revision plus its operations.
#[derive(Clone, Debug)]
pub struct ObservedFrame {
    /// The immutable committed value; `None` after retirement.
    pub value: Option<Map<String, Value>>,
    /// The exact operation batch that produced it.
    pub ops: Vec<Op>,
    /// Definition version of `value`.
    pub version: i64,
    /// The commit's context, without the producer's cancellation for a state
    /// and with the watch's own cancellation for a watch.
    pub context: Context,
}

/// Why an observer ended.
#[derive(Clone, Debug, PartialEq)]
pub enum WatchEnd {
    /// The caller stopped it.
    Stopped,
    /// The caller's context was cancelled.
    Cancelled,
    /// The session closed.
    SessionClosed,
    /// The incarnation was retired.
    Retired,
    /// A listener failed.
    ListenerError(StorageError),
    /// The session failed; every observer ends with the same reason.
    SessionFailed(StorageError),
}

/// The frame queue of one observer, bounded by
/// [`MAX_PENDING_WATCH_FRAMES`].
///
/// Collapsing is a root replacement built from the newest value, exactly as
/// upstream does: no value copy, no op replay, no re-diff.
#[derive(Debug)]
struct PendingFrames {
    frames: Vec<ObservedFrame>,
    overflowed: bool,
}

impl PendingFrames {
    fn new() -> Self {
        Self {
            frames: Vec::new(),
            overflowed: false,
        }
    }

    fn push(&mut self, frame: ObservedFrame) {
        if self.frames.len() >= MAX_PENDING_WATCH_FRAMES {
            // Replace the complete undelivered suffix with one self-contained
            // root replacement built from this newest revision.
            let replacement = ObservedFrame {
                value: frame.value.clone(),
                ops: vec![Op::Replace(
                    frame
                        .value
                        .clone()
                        .map(Value::Object)
                        .unwrap_or(Value::Null),
                )],
                version: frame.version,
                context: frame.context.clone(),
            };
            self.frames.clear();
            self.frames.push(replacement);
            self.overflowed = true;
            return;
        }
        self.frames.push(frame);
    }

    fn pop(&mut self) -> Option<ObservedFrame> {
        if self.frames.is_empty() {
            None
        } else {
            Some(self.frames.remove(0))
        }
    }
}

/// The shared half of both observer faces: the current revision and the frame
/// queue.
#[derive(Debug)]
pub struct ObserverCore {
    current: Option<Map<String, Value>>,
    pending: PendingFrames,
    version: i64,
    ended: Option<WatchEnd>,
}

impl ObserverCore {
    /// A core holding `value` at `version`.
    #[must_use]
    pub fn new(value: Option<Map<String, Value>>, version: i64) -> Self {
        Self {
            current: value,
            pending: PendingFrames::new(),
            version,
            ended: None,
        }
    }

    /// The current revision.
    #[must_use]
    pub fn value(&self) -> Option<&Map<String, Value>> {
        self.current.as_ref()
    }

    /// Its definition version.
    #[must_use]
    pub fn version(&self) -> i64 {
        self.version
    }

    /// Whether the observer already ended.
    #[must_use]
    pub fn ended(&self) -> Option<&WatchEnd> {
        self.ended.as_ref()
    }

    /// Whether frames were collapsed (diagnostics).
    #[must_use]
    pub fn overflowed(&self) -> bool {
        self.pending.overflowed
    }

    /// Advance to one committed revision.
    ///
    /// The operations are relative to the observer's own version: a consumer
    /// hydrated under another definition version receives the new value as a
    /// root replacement instead of operations for a shape it does not hold
    /// (upstream `observedOperations`).
    pub fn advance(
        &mut self,
        value: Option<Map<String, Value>>,
        ops: Vec<Op>,
        version: i64,
        context: Context,
    ) {
        if self.ended.is_some() {
            return;
        }
        let retiring = value.is_none();
        let ops = if retiring {
            retirement_operations()
        } else if version != self.version {
            self.version = version;
            vec![Op::Replace(
                value.clone().map(Value::Object).unwrap_or(Value::Null),
            )]
        } else {
            ops
        };
        self.current = value.clone();
        self.pending.push(ObservedFrame {
            value,
            ops,
            version,
            context,
        });
        if retiring {
            // The terminal frame is delivered from the queue, but the watch is
            // already terminal for `is_stopped`/`ended` purposes.
            self.ended = Some(WatchEnd::Retired);
        }
    }

    /// Take the next pending frame.
    pub fn next_frame(&mut self) -> Option<ObservedFrame> {
        self.pending.pop()
    }

    /// Advance to the terminal retirement frame and record the end reason.
    ///
    /// Retirement is terminal and must survive a close/failure that raced
    /// ahead of the listener: unlike [`Self::advance`], this queues the
    /// canonical `[["r", null]]` frame even when the observer already ended,
    /// and leaves that frame pending for the consumer rather than clearing it.
    pub fn advance_retirement(&mut self, version: i64, context: Context) {
        self.current = None;
        self.version = version;
        self.pending.push(ObservedFrame {
            value: None,
            ops: retirement_operations(),
            version,
            context,
        });
        self.ended = Some(WatchEnd::Retired);
    }

    /// End the observer; the first reason wins.
    pub fn end(&mut self, reason: WatchEnd) {
        if self.ended.is_none() {
            self.ended = Some(reason);
            self.pending.frames.clear();
        }
    }
}

/// A consumer of one incarnation's committed stream.
#[derive(Clone, Debug)]
pub struct CommittedStateSource {
    core: Arc<Mutex<ObserverCore>>,
}

impl CommittedStateSource {
    /// A source over one incarnation.
    #[must_use]
    pub fn new(value: Option<Map<String, Value>>, version: i64) -> Self {
        Self {
            core: Arc::new(Mutex::new(ObserverCore::new(value, version))),
        }
    }

    /// The shared core, for the session's publication fan-out.
    #[must_use]
    pub fn core(&self) -> Arc<Mutex<ObserverCore>> {
        self.core.clone()
    }

    /// The current revision.
    #[must_use]
    pub fn value(&self) -> Option<Map<String, Value>> {
        self.core
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .value()
            .cloned()
    }

    /// Advance this source with one committed change.
    pub fn advance(
        &self,
        value: Option<Map<String, Value>>,
        ops: Vec<Op>,
        version: i64,
        context: Context,
    ) {
        self.core
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .advance(value, ops, version, context);
    }

    /// Advance this source to its terminal retirement frame.
    ///
    /// This records [`WatchEnd::Retired`] while keeping the terminal frame
    /// pending, so a consumer can still drain `[["r", null]]` after the end.
    pub fn advance_retirement(&self, version: i64, context: Context) {
        self.core
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .advance_retirement(version, context);
    }

    /// End the source (session close, retirement, failure).
    pub fn end(&self, reason: WatchEnd) {
        self.core
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .end(reason);
    }
}

/// A watch's start callback; returning `false` ends the watch.
type WatchStartFn = dyn Fn(ObservedFrame) -> bool + Send + Sync + 'static;

/// The message carried by a caught panic payload.
fn panic_message(payload: Box<dyn Any + Send>) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|text| (*text).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "panicked".to_owned())
}

/// A serialized exact-frame watch over one incarnation.
///
/// Only one callback runs at a time; a callback that is still running is never
/// joined by `stop()` (caller-owned after it started), matching upstream
/// §9.2's contract.
#[derive(Clone)]
pub struct CommittedWatch {
    core: Arc<Mutex<ObserverCore>>,
    started: Arc<Mutex<Option<Box<WatchStartFn>>>>,
}

impl std::fmt::Debug for CommittedWatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CommittedWatch")
            .field(
                "started",
                &self
                    .started
                    .lock()
                    .map(|slot| slot.is_some())
                    .unwrap_or(false),
            )
            .finish_non_exhaustive()
    }
}

impl CommittedWatch {
    /// A watch over one incarnation.
    #[must_use]
    pub fn new(value: Option<Map<String, Value>>, version: i64) -> Self {
        Self {
            core: Arc::new(Mutex::new(ObserverCore::new(value, version))),
            started: Arc::new(Mutex::new(None)),
        }
    }

    /// The shared core.
    #[must_use]
    pub fn core(&self) -> Arc<Mutex<ObserverCore>> {
        self.core.clone()
    }

    /// Whether the watch already ended, for any reason.
    #[must_use]
    pub fn is_stopped(&self) -> bool {
        self.core
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .ended()
            .is_some()
    }

    /// Install the sole listener; it is invoked for each pending and later
    /// frame, one at a time. It returns `false` to end the watch.
    pub fn start<F>(&self, listener: F)
    where
        F: Fn(ObservedFrame) -> bool + Send + Sync + 'static,
    {
        let mut slot = self
            .started
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *slot = Some(Box::new(listener));
        drop(slot);
        self.drain();
    }

    /// Deliver pending frames to the listener, one at a time.
    pub fn drain(&self) {
        let listener = {
            let slot = self
                .started
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            slot.as_ref().map(|_| ())
        };
        if listener.is_none() {
            return;
        }
        loop {
            let frame = {
                let mut core = self
                    .core
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                // Take the frame exactly once: an ended core may still hold the
                // terminal retirement frame, which must be delivered.
                core.next_frame()
            };
            let Some(frame) = frame else {
                return;
            };
            let retiring = frame.value.is_none();
            let outcome = {
                let slot = self
                    .started
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                match slot.as_ref() {
                    Some(listener) => {
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| listener(frame)))
                    }
                    None => return,
                }
            };
            match outcome {
                // A null value is the terminal retirement frame: the watch
                // ends `Retired` whether the listener asked to continue or
                // panicked while observing it.
                _ if retiring => {
                    self.core
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .end(WatchEnd::Retired);
                    return;
                }
                Ok(true) => {}
                Ok(false) => {
                    // A listener that returns `false` is a caller-requested
                    // stop; only a panic is a listener error.
                    self.core
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .end(WatchEnd::Stopped);
                    return;
                }
                Err(payload) => {
                    self.core
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .end(WatchEnd::ListenerError(StorageError::invalid(format!(
                            "watch listener panicked: {}",
                            panic_message(payload)
                        ))));
                    return;
                }
            }
        }
    }

    /// Idempotent stop: discard pending frames and end the watch.
    pub fn stop(&self, reason: WatchEnd) {
        self.core
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .end(reason);
    }
}

/// One observed incarnation inside the session: the observer plus the pin that
/// keeps the incarnation cached.
#[derive(Debug)]
pub struct Observation {
    /// The observer.
    pub source: CommittedStateSource,
    /// The pinned incarnation.
    pub record_id: crate::durable::ids::DocumentId,
    /// The logical address it lives at.
    pub address_id: String,
}
