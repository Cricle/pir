//! Bounded, ordered backlog for fire-and-forget extension dispatches.
//!
//! Some extension hooks run deep inside synchronous host callbacks that must
//! never block on QuickJS — the per-chunk `provider_stream_event` hook is the
//! primary one (it fires from the provider stream's poll). The pre-existing
//! pattern (`tokio::spawn` per event) keeps the caller unblocked but leaves
//! the queued work unbounded in memory: a slow JS handler lets the provider
//! stream enqueue tasks without limit.
//!
//! [`EventBacklog`] bounds that queue instead: one lazily started worker task
//! drains a bounded channel IN ORDER, one dispatch in flight, so N events cost
//! ONE task (not N). [`EventBacklog::push_with`] is synchronous and never
//! blocks — the caller (the provider stream) is never delayed by JS. When the
//! bounded queue is full the newest item is dropped and counted
//! ([`EventBacklog::dropped`]), which is the only alternative to delaying the
//! stream or growing memory without bound.
//!
//! The worker exits when the last [`EventBacklog`] clone drops (the channel's
//! only sender lives in the shared inner state, which the worker does NOT
//! retain): already-queued items are still drained, then `recv` returns
//! `None`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use tokio::sync::mpsc;

use crate::agent::agent_loop::BoxFuture;

/// The dispatch callback of one [`EventBacklog`]: consumes one item, returns
/// the (boxed) dispatch future the single worker awaits.
type BacklogHandler<T> = Arc<dyn Fn(T) -> BoxFuture<'static, ()> + Send + Sync>;

/// Counters shared with the worker: one cache line's worth of plain atomics,
/// no locking. Read through the accessor methods (test/profiling probes).
struct BacklogWorker<T> {
    handler: BacklogHandler<T>,
    worker_starts: AtomicU64,
    accepted: AtomicU64,
    dropped: AtomicU64,
    processed: AtomicU64,
}

struct BacklogInner<T> {
    bound: usize,
    /// The channel's only sender; created on the first accepted push (no
    /// worker task and no channel for a hook that never fires). Dropping the
    /// last `EventBacklog` clone drops it, which closes the channel after its
    /// buffered items are drained.
    sender: OnceLock<mpsc::Sender<T>>,
    worker: Arc<BacklogWorker<T>>,
}

/// See the module docs. `T: Send + 'static` (the items cross into the worker
/// task).
pub struct EventBacklog<T> {
    inner: Arc<BacklogInner<T>>,
}

impl<T> Clone for EventBacklog<T> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<T: Send + 'static> EventBacklog<T> {
    /// Create a backlog declared to hold at most `bound` queued items, drained
    /// by one worker that awaits `handler` per item (in order, one in flight).
    /// No task is spawned until the first accepted push inside a tokio
    /// runtime.
    pub fn new(bound: usize, handler: BacklogHandler<T>) -> Self {
        Self {
            inner: Arc::new(BacklogInner {
                bound,
                sender: OnceLock::new(),
                worker: Arc::new(BacklogWorker {
                    handler,
                    worker_starts: AtomicU64::new(0),
                    accepted: AtomicU64::new(0),
                    dropped: AtomicU64::new(0),
                    processed: AtomicU64::new(0),
                }),
            }),
        }
    }

    /// Queue one item built lazily by `make` (the builder runs only when the
    /// push is actually accepted, so an overflow never pays for an event
    /// nobody will dispatch). Synchronous and non-blocking by contract.
    ///
    /// Returns `true` when the item was accepted, `false` when it was dropped
    /// (queue full, channel closed, or no tokio runtime to run the worker —
    /// in the last case a later push still creates the worker).
    pub fn push_with(&self, make: impl FnOnce() -> T) -> bool {
        let worker = &self.inner.worker;
        let sender = match self.inner.sender.get() {
            Some(sender) => sender,
            None => {
                // The worker task holds only `worker` (never the sender), so
                // the channel closes when the backlog itself drops.
                let Ok(handle) = tokio::runtime::Handle::try_current() else {
                    worker.dropped.fetch_add(1, Ordering::Relaxed);
                    return false;
                };
                self.inner.sender.get_or_init(|| {
                    let (sender, mut receiver) = mpsc::channel(self.inner.bound);
                    let worker = Arc::clone(worker);
                    worker.worker_starts.fetch_add(1, Ordering::Relaxed);
                    handle.spawn(async move {
                        while let Some(item) = receiver.recv().await {
                            (worker.handler)(item).await;
                            worker.processed.fetch_add(1, Ordering::Relaxed);
                        }
                    });
                    sender
                })
            }
        };
        // Single sender: a zero capacity can only mean the bounded queue is
        // full (the worker holds no sender), so the item is dropped without
        // building it.
        if sender.capacity() == 0 {
            worker.dropped.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        let item = make();
        match sender.try_send(item) {
            Ok(()) => {
                worker.accepted.fetch_add(1, Ordering::Relaxed);
                true
            }
            Err(_) => {
                // Full or closed (the worker never closes it while the sender
                // lives; a full queue can still race the capacity read).
                worker.dropped.fetch_add(1, Ordering::Relaxed);
                false
            }
        }
    }

    /// Test/profiling probe (`#[doc(hidden)]` semantics): how many worker
    /// tasks this backlog started. N accepted pushes cost ONE task — the
    /// spawn-per-event pattern this replaces cost N.
    pub fn worker_starts(&self) -> u64 {
        self.inner.worker.worker_starts.load(Ordering::Relaxed)
    }

    /// Test/profiling probe: how many items were accepted into the queue.
    pub fn accepted(&self) -> u64 {
        self.inner.worker.accepted.load(Ordering::Relaxed)
    }

    /// Test/profiling probe: how many items were dropped because the bounded
    /// queue was full (or no worker runtime existed).
    pub fn dropped(&self) -> u64 {
        self.inner.worker.dropped.load(Ordering::Relaxed)
    }

    /// Test/profiling probe: how many accepted items the worker finished
    /// dispatching.
    pub fn processed(&self) -> u64 {
        self.inner.worker.processed.load(Ordering::Relaxed)
    }
}
