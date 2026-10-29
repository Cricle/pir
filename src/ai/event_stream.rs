//! Event stream for assistant messages.
//!
//! Rust equivalent of the upstream `packages/ai/src/utils/event-stream.ts`.
//! Streams emit `start` before partial updates, then terminate with either:
//! - `Done` carrying the final successful `AssistantMessage`, or
//! - `Error` carrying the final `AssistantMessage` with stop reason `error`
//!   or `aborted` and an error message.
//!
//! # Response timing (`durationMs`)
//!
//! Every stream times the response it carries, exactly like upstream's
//! `AssistantMessageEventStream`: the terminal message of a `Done`/`Error`
//! event, or the result passed to [`end`](AssistantMessageEventStream::end),
//! receives `durationMs` measured with a monotonic clock from the stream's
//! creation. A message that already carries one keeps it (an inner provider
//! stream stamps first and a forwarding wrapper does not re-time it), and a
//! message whose `timestamp` predates the stream is a response that started
//! elsewhere (for example a deferred result fetched later) and stays untimed.
//! This is what makes a terminal message built by a provider adapter (through
//! [`ProviderRegistry::stream`](crate::ai::provider::ProviderRegistry::stream),
//! `stream_simple` or `complete`) carry the same shape the agent loop
//! publishes, without the agent loop having to add the field.
//!
//! # Bounded queue + producer backpressure
//!
//! Every non-terminal event owns a full cumulative `partial` snapshot (the
//! parser hand-out is an `Arc` clone; the next delta copy-on-writes because a
//! snapshot is outstanding), so an UNBOUNDED queue turns a consumer that is
//! slower than the provider into `O(deltas x partial-size)` retained bytes
//! (measured: ~745 MB RSS for a 10 000-delta / 126 KB reply consumed behind a
//! per-delta extension handler). The queue here is bounded
//! ([`DEFAULT_EVENT_QUEUE_CAPACITY`]) and producers apply backpressure
//! through [`AssistantMessageEventStream::push_await`]: a full queue parks
//! the producer, which parks the provider's body read loop, which stops the
//! network read — memory settles at `capacity + one chunk` snapshots instead
//! of the whole reply. Upstream's JS queue is also unbounded, but there each
//! queued event holds the SAME live message object (queueing allocates
//! nothing); pir's copy-on-write protocol freezes a snapshot per queued event
//! and therefore has to bound the queue itself.
//!
//! # Consumer liveness (a producer never parks forever)
//!
//! Backpressure alone has a failure mode: the producer's `sink` clone keeps
//! the shared queue alive after the CONSUMER has gone, so a producer parked on
//! a full queue would have nobody left to pop an event, pop-to-wake it, or
//! terminate the stream — it would stay parked forever, leaking the task (and,
//! through it, the transport read loop). Any consumer that abandons a stream
//! hits this: an explicit abort, a TUI teardown, or a caller that errors out
//! before draining.
//!
//! The signal is a consumer count plus explicitly non-counting producer
//! handles:
//!
//! - every handle obtained from [`AssistantMessageEventStream::new`],
//!   [`with_capacity`](AssistantMessageEventStream::with_capacity),
//!   `Default` or `Clone` is a CONSUMER handle and holds one unit of consumer
//!   liveness;
//! - [`producer`](AssistantMessageEventStream::producer) returns a handle that
//!   pushes/ends the same stream but does NOT count, so a producer task can
//!   hold it for the life of a request without keeping the stream "open" on
//!   the consumer's behalf;
//! - dropping the last consumer handle sets the queue's `abandoned` flag and
//!   wakes every parked producer;
//!   [`push_await`](AssistantMessageEventStream::push_await) re-checks the
//!   flag after every wake and returns `false` (dropping the event) once it is
//!   set. Termination is therefore bounded by one wake, not by a timeout poll,
//!   and the queue state is untouched.
//!
//! `Clone` defaults to a consumer handle on purpose: the safe direction is to
//! keep a stream open (the historical behaviour), never to close one a live
//! consumer still reads. A clone of a producer handle stays a producer handle.
//!
//! Alternatives weighed, and why they lost:
//!
//! - **`Weak`-based liveness** (a `Weak<Mutex<Inner>>` in the producer): a
//!   parked producer must keep `Inner` (and its waker slot) alive while it
//!   waits, so its own strong reference defeats the `Weak` upgrade check — the
//!   upgrade succeeds exactly when the producer is parked. Not viable without
//!   the same explicit consumer-side drop signal.
//! - **An explicit `close_consumer()` a drop guard calls**: the closest
//!   cousin of the chosen design, but it has to be installed at every consumer
//!   call site (including every `?`/error path) and is silently forgotten by
//!   the next consumer added — exactly the omission that caused this bug.
//!   Folding the signal into the handle's `Drop` is transparent to consumers:
//!   no call site changes and no guard to leak.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::task::Poll;
use std::task::Waker;
use std::time::Instant;

use crate::ai::types::{AssistantMessage, AssistantMessageEvent, ErrorReason, TerminalReason};

/// Default capacity of the per-stream event queue.
///
/// Deliberately small: the queue absorbs the jitter of a handful of deltas
/// (about one screen of streaming output) and no more. Queueing more buys
/// nothing — a consumer that is slower than the provider must slow the
/// provider down (backpressure) instead of freezing more snapshots in memory.
pub const DEFAULT_EVENT_QUEUE_CAPACITY: usize = 32;

/// A shared-queue stream of `AssistantMessageEvent`s.
///
/// Producers use [`push_await`](Self::push_await) (applies backpressure) and
/// terminate with [`end`](Self::end) or a terminal event. Consumers await the
/// final `AssistantMessage` via [`result`](Self::result) or iterate events as
/// a `futures_util::Stream`.
///
/// Clones share one queue, not a broadcast: each event is popped exactly once
/// by whichever `next()`/`try_next()` caller polls first, so concurrent
/// `next()` callers split events between them rather than each seeing all of
/// them. Only the terminal message is shared — every concurrent `result()`
/// caller observes the same final `AssistantMessage`.
///
/// A handle is a CONSUMER handle unless it came from
/// [`producer`](Self::producer): while at least one consumer handle is alive
/// the queue is reachable by someone who can drain it. When the last one is
/// dropped the stream is abandoned and parked producers return `false` from
/// [`push_await`](Self::push_await) instead of parking forever (see the
/// module docs on consumer liveness).
#[derive(Debug)]
pub struct AssistantMessageEventStream {
    inner: Arc<Mutex<Inner>>,
    /// `true` for a consumer handle (counts toward `Inner::consumers`), `false`
    /// for a producer handle (pushes and ends the stream, but its own life
    /// never keeps the stream open for a consumer).
    consumer: bool,
}

impl Clone for AssistantMessageEventStream {
    /// Cloning a consumer handle yields another consumer handle (the count is
    /// bumped); cloning a producer handle yields another producer handle.
    fn clone(&self) -> Self {
        if self.consumer {
            let mut inner = self.lock();
            inner.consumers = inner.consumers.saturating_add(1);
        }
        Self {
            inner: Arc::clone(&self.inner),
            consumer: self.consumer,
        }
    }
}

impl Drop for AssistantMessageEventStream {
    /// The consumer-liveness signal: losing the last consumer handle marks the
    /// stream abandoned and wakes every parked producer, so a producer parked
    /// in `push_await` returns after one wake instead of parking forever.
    fn drop(&mut self) {
        if !self.consumer {
            return;
        }
        // Wakers are taken under the lock and fired after it is released, so a
        // wake that re-enters this stream (an inline-polling executor) cannot
        // deadlock on the non-reentrant mutex.
        let wakers = {
            let mut inner = self.lock();
            inner.consumers = inner.consumers.saturating_sub(1);
            if inner.consumers == 0 {
                inner.abandoned = true;
                // A consumer parked in `next()`/`result()` has no handle left to
                // observe the abandonment through (it just dropped every one), so
                // there is nothing meaningful to wake there; clearing also drops
                // the stale wakers.
                inner.event_waiting.clear();
                std::mem::take(&mut inner.space_waiting)
            } else {
                VecDeque::new()
            }
        };
        for waker in wakers {
            waker.wake();
        }
    }
}

#[derive(Debug)]
struct Inner {
    /// Bounded event queue. Every queued non-terminal event owns a full
    /// cumulative `partial` snapshot, so the queue depth is the stream's
    /// frozen-snapshot retention; `capacity` bounds it. A terminal event is
    /// the one event allowed past the bound (it closes the stream and every
    /// consumer must still observe it in order). The per-delta snapshot copy
    /// is inherent to the "event carries the full partial" wire protocol; the
    /// terminal message is `Arc`-shared via `final_result` so every
    /// `result()` caller observes one allocation.
    queue: VecDeque<AssistantMessageEvent>,
    /// Maximum number of events the queue may hold before producers park.
    capacity: usize,
    /// Consumers parked in `poll_next` / `result` / `drain_and_result`,
    /// woken one per pushed event (and all on termination).
    event_waiting: VecDeque<Waker>,
    /// Producers parked in `push_await` waiting for queue space, woken one
    /// per popped event (all on termination, after which parked pushes return
    /// `true` as no-ops, and all when the last consumer handle drops, after
    /// which they return `false`).
    space_waiting: VecDeque<Waker>,
    done: bool,
    /// `true` once the last consumer handle was dropped. Parked producers
    /// re-check it after every wake and give up instead of parking again.
    abandoned: bool,
    /// Live consumer-handle count (producer handles excluded). Owned entirely
    /// by `Clone`/`Drop` on [`AssistantMessageEventStream`].
    consumers: usize,
    final_result: Option<Option<Arc<AssistantMessage>>>,
    /// Observability (same precedent as the parsers' `copy_count`): peak
    /// event-queue depth reached. With producers using `push_await` this
    /// stays `<= capacity` except for a single terminal event.
    max_depth: usize,
    /// How many `push_await` calls had to wait for queue space.
    push_waits: u64,
    /// Wall-clock milliseconds at stream creation (upstream `#startedAt`).
    /// A terminal message whose `timestamp` is older started outside this
    /// stream and must not be timed (see [`Inner::time_terminal_message`]).
    started_at_ms: u64,
    /// Monotonic clock at stream creation (upstream
    /// `#startedAtMonotonic`), the base of `durationMs`.
    started_at: Instant,
}

impl Inner {
    fn new(capacity: usize) -> Self {
        Self {
            queue: VecDeque::new(),
            capacity: capacity.max(1),
            event_waiting: VecDeque::new(),
            space_waiting: VecDeque::new(),
            done: false,
            abandoned: false,
            consumers: 0,
            final_result: None,
            max_depth: 0,
            push_waits: 0,
            started_at_ms: crate::ai::util::now_ms(),
            started_at: Instant::now(),
        }
    }

    /// Append an event and wake the next consumer. Capacity is the caller's
    /// concern (`push_await` waits, `try_push` refuses, `push` is lenient).
    fn enqueue(&mut self, event: AssistantMessageEvent) {
        self.queue.push_back(event);
        self.max_depth = self.max_depth.max(self.queue.len());
        if let Some(waker) = self.event_waiting.pop_front() {
            waker.wake();
        }
    }

    /// Release one unit of queue space, waking one parked producer.
    fn release_space(&mut self) {
        if let Some(waker) = self.space_waiting.pop_front() {
            waker.wake();
        }
    }

    /// Terminate: mark done, share the final result, enqueue the terminal
    /// event (bypassing the capacity bound) and wake every waiter.
    fn finish(
        &mut self,
        event: AssistantMessageEvent,
        final_result: Option<Arc<AssistantMessage>>,
    ) {
        self.done = true;
        self.enqueue(event);
        if let Some(final_result) = final_result {
            self.final_result = Some(Some(final_result));
        }
        for waker in std::mem::take(&mut self.event_waiting) {
            waker.wake();
        }
        for waker in std::mem::take(&mut self.space_waiting) {
            waker.wake();
        }
    }

    /// Stamp `durationMs` on the final message of a response this stream saw
    /// start (upstream `AssistantMessageEventStream.#time`,
    /// `utils/event-stream.ts:97-130`).
    ///
    /// The upstream rule, reproduced exactly:
    ///
    /// - a stream that already finished never stamps another message;
    /// - a message that already carries `durationMs` keeps it (the inner
    ///   provider stream stamps first, so a forwarding wrapper such as the
    ///   registry's outer stream does not shorten it);
    /// - a message whose `timestamp` predates the stream's creation is a
    ///   response that began elsewhere — a deferred result fetched later, or a
    ///   message forwarded from an earlier request — and stays untimed;
    /// - otherwise the elapsed monotonic time is recorded, rounded to the
    ///   nearest millisecond (upstream `Math.round`, clamped at zero).
    fn time_terminal_message(&self, message: &mut AssistantMessage) {
        if self.done || message.duration_ms.is_some() || message.timestamp < self.started_at_ms {
            return;
        }
        let elapsed = self.started_at.elapsed();
        let micros = elapsed.as_micros();
        let mut millis = micros / 1_000;
        // `Math.round(performance.now() - startedAtMonotonic)`.
        if micros % 1_000 >= 500 {
            millis = millis.saturating_add(1);
        }
        message.duration_ms = Some(u64::try_from(millis).unwrap_or(u64::MAX));
    }
}

impl AssistantMessageEventStream {
    /// Create an empty assistant message event stream with the default queue
    /// capacity ([`DEFAULT_EVENT_QUEUE_CAPACITY`]).
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_EVENT_QUEUE_CAPACITY)
    }

    /// Create an empty stream with an explicit queue capacity (clamped to at
    /// least 1). Tests use this to exercise the bound without pushing
    /// hundreds of events; production streams use [`new`](Self::new).
    pub fn with_capacity(capacity: usize) -> Self {
        let mut inner = Inner::new(capacity);
        inner.consumers = 1;
        Self {
            inner: Arc::new(Mutex::new(inner)),
            consumer: true,
        }
    }

    /// A PRODUCER handle over the same stream: it pushes, terminates, and
    /// observes state like any other handle, but it does not count toward
    /// consumer liveness, so a producer task may hold it for the request's
    /// whole life without keeping an abandoned stream alive.
    ///
    /// Producers should use this handle (typically `stream.producer()` moved
    /// into the forwarding task, with the original handle returned to the
    /// caller as the consumer) so dropping the consumer is observable. A
    /// producer driving events through a consumer handle still works, but then
    /// the consumer can never be observed as gone.
    pub fn producer(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
            consumer: false,
        }
    }

    /// The configured queue capacity (excluding the terminal event, which is
    /// always allowed in).
    pub fn capacity(&self) -> usize {
        self.lock().capacity
    }

    /// The terminal `final_result` payload for `event`, or `None` when the
    /// event is not terminal. Must be called AFTER [`Inner::time_terminal_message`]
    /// so the shared `Arc` carries the stamped `durationMs`.
    fn terminal_result(event: &AssistantMessageEvent) -> Option<Arc<AssistantMessage>> {
        match event {
            AssistantMessageEvent::Done { message, .. }
            | AssistantMessageEvent::Error { error: message, .. } => {
                Some(Arc::new(message.clone()))
            }
            _ => None,
        }
    }

    /// Mutable access to a terminal event's final message (`None` for a
    /// non-terminal event).
    fn terminal_message_mut(event: &mut AssistantMessageEvent) -> Option<&mut AssistantMessage> {
        match event {
            AssistantMessageEvent::Done { message, .. } => Some(message),
            AssistantMessageEvent::Error { error, .. } => Some(error),
            _ => None,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Push an event onto the stream WITHOUT backpressure.
    ///
    /// The synchronous compatibility/test entry: it never blocks and never
    /// fails. A non-terminal event is enqueued even when the queue is at
    /// capacity, so this entry can exceed the bound and is NOT the one
    /// provider producers use — they use [`push_await`](Self::push_await),
    /// which applies backpressure. Terminal events (`Done` / `Error`) end the
    /// stream and become the final result; events pushed after termination
    /// are ignored. On an abandoned stream (every consumer handle already
    /// dropped) a non-terminal event is ignored too — nobody could read it.
    pub fn push(&self, mut event: AssistantMessageEvent) {
        if Self::terminal_message_mut(&mut event).is_some() {
            self.finish_common(event);
            return;
        }
        let mut inner = self.lock();
        if inner.done || inner.abandoned {
            return;
        }
        inner.enqueue(event);
    }

    /// Push an event, waiting for queue space when the queue is full.
    ///
    /// This is the producer entry: while the queue is at capacity the future
    /// stays pending (it does not block the thread, drop the event, or grow
    /// the queue), so a slow consumer throttles the provider's body read loop
    /// instead of retaining an unbounded set of frozen `partial` snapshots.
    /// The waiting producer is woken by any consumer pop, by
    /// [`end`](Self::end), by a terminal event, and by the last consumer
    /// handle being dropped.
    ///
    /// Returns `true` when the event was enqueued (or the stream already
    /// terminated, in which case it is intentionally dropped) and `false` when
    /// the event was NOT delivered because every consumer handle is gone.
    ///
    /// A producer parked on a full queue therefore wakes as soon as the
    /// consumer abandons the stream — a vanished consumer is observed within
    /// one wake, not by a timeout poll — and the caller can stop its read loop
    /// instead of leaking the task. Callers that do not care for the signal
    /// (for example a producer whose sole completion is a terminal event) can
    /// ignore the value; terminal pushes still finish the stream either way,
    /// and nothing can read an abandoned stream's queue.
    ///
    /// Terminal events never wait: they finish the stream immediately (see
    /// [`push`](Self::push) for the terminal semantics). Pushing after the
    /// stream terminated is a no-op.
    pub async fn push_await(&self, mut event: AssistantMessageEvent) -> bool {
        if Self::terminal_message_mut(&mut event).is_some() {
            return self.finish_common(event);
        }
        // `poll_fn` is `FnMut`, so the event is moved into an `Option` and
        // taken exactly once on the ready path.
        let mut pending = Some(event);
        let mut waited = false;
        futures_util::future::poll_fn(|cx| {
            let mut inner = self.lock();
            if inner.done {
                return Poll::Ready(true);
            }
            // Re-checked on every wake: a parked producer must not re-park once
            // the consumer is gone (nobody would ever pop to wake it again).
            if inner.abandoned {
                pending = None;
                return Poll::Ready(false);
            }
            if inner.queue.len() < inner.capacity {
                if let Some(event) = pending.take() {
                    inner.enqueue(event);
                }
                return Poll::Ready(true);
            }
            if !waited {
                waited = true;
                inner.push_waits = inner.push_waits.saturating_add(1);
            }
            inner.space_waiting.push_back(cx.waker().clone());
            Poll::Pending
        })
        .await
    }

    /// Try to push an event without waiting.
    ///
    /// Returns `Err(event)` when the stream is already terminated, abandoned
    /// by every consumer, or the queue is at capacity (the caller keeps
    /// ownership of the event). Terminal events always succeed and finish the
    /// stream, like [`push`](Self::push).
    // The `Err` hands the caller's own event back (the documented ownership
    // contract); boxing it would change the public signature.
    #[allow(clippy::result_large_err)]
    pub fn try_push(&self, mut event: AssistantMessageEvent) -> Result<(), AssistantMessageEvent> {
        if Self::terminal_message_mut(&mut event).is_some() {
            self.finish_common(event);
            return Ok(());
        }
        let mut inner = self.lock();
        if inner.done || inner.abandoned || inner.queue.len() >= inner.capacity {
            return Err(event);
        }
        inner.enqueue(event);
        Ok(())
    }

    /// Terminate the stream, optionally providing a final result.
    ///
    /// A result passed here is timed like a terminal event's message (see
    /// [`Inner::time_terminal_message`]); it therefore carries `durationMs`
    /// when the response started inside this stream. The caller may still hold
    /// the handle it passed, so timing copy-on-writes through `Arc::make_mut`.
    pub fn end(&self, mut result: Option<Arc<AssistantMessage>>) {
        let mut inner = self.lock();
        if inner.done {
            return;
        }
        if let Some(result) = result.as_mut() {
            inner.time_terminal_message(Arc::make_mut(result));
        }
        inner.done = true;
        if let Some(result) = result {
            inner.final_result = Some(Some(result));
        }
        for waker in std::mem::take(&mut inner.event_waiting) {
            waker.wake();
        }
        for waker in std::mem::take(&mut inner.space_waiting) {
            waker.wake();
        }
    }

    /// Resolve the final assistant message.
    ///
    /// Does not consume queued events: concurrent `next()`/`try_next()`
    /// callers still observe every event (exactly once). Consumers that only
    /// ever want the final message must use
    /// [`drain_and_result`](Self::drain_and_result) instead — with a bounded
    /// queue, a producer parks once the queue is full and a consumer that
    /// never pops an event would wait forever (it only stops parking once this
    /// handle is dropped, which abandons the stream).
    ///
    /// Returns `None` if the stream was never terminated and never produced a
    /// terminal event. Production callers should use [`result_expect`](Self::result_expect)
    /// or handle `None` explicitly.
    pub async fn result(&self) -> Option<Arc<AssistantMessage>> {
        self.await_result(false).await
    }

    /// Resolve the final assistant message, popping (and discarding) queued
    /// events while waiting.
    ///
    /// The completion-oriented consumer entry: `complete`-style callers want
    /// nothing but the final message, and with a bounded queue a consumer
    /// that never pops cannot unblock a parked producer. Events discarded
    /// here would otherwise have been delivered to a `next()`/`try_next()`
    /// caller — only use this when this handle is the stream's only consumer.
    pub async fn drain_and_result(&self) -> Option<Arc<AssistantMessage>> {
        self.await_result(true).await
    }

    async fn await_result(&self, drain: bool) -> Option<Arc<AssistantMessage>> {
        futures_util::future::poll_fn(|cx| {
            let mut inner = self.lock();
            if drain {
                let mut popped = 0usize;
                while inner.queue.pop_front().is_some() {
                    popped += 1;
                }
                for _ in 0..popped {
                    inner.release_space();
                }
            }
            if inner.done {
                return Poll::Ready(inner.final_result.clone().flatten());
            }
            inner.event_waiting.push_back(cx.waker().clone());
            Poll::Pending
        })
        .await
    }

    /// Resolve the final assistant message, returning a descriptive error when
    /// the stream ended without a terminal event.
    pub async fn result_expect(&self) -> crate::ai::error::AiResult<Arc<AssistantMessage>> {
        self.result()
            .await
            .ok_or(crate::ai::error::AiError::StreamNotTerminated)
    }

    /// Take the next event from the queue, or `None` when the stream is
    /// finished and drained. Popping releases one unit of queue space, waking
    /// a parked producer.
    pub fn try_next(&self) -> Option<AssistantMessageEvent> {
        let mut inner = self.lock();
        let event = inner.queue.pop_front();
        if event.is_some() {
            inner.release_space();
        }
        event
    }

    /// Number of queued (not yet consumed) events.
    pub fn pending_count(&self) -> usize {
        self.lock().queue.len()
    }

    /// Returns true when the stream has been terminated.
    pub fn is_done(&self) -> bool {
        self.lock().done
    }

    /// Returns true when every consumer handle has been dropped (the stream is
    /// unreachable by any consumer, so `push_await` refuses to park).
    pub fn is_abandoned(&self) -> bool {
        self.lock().abandoned
    }

    /// Live consumer-handle count (producer handles from
    /// [`producer`](Self::producer) are not counted).
    pub fn consumer_count(&self) -> usize {
        self.lock().consumers
    }

    /// Peak event-queue depth reached so far (observability for the bounded
    /// queue; see the parsers' `copy_count()` precedent). Stays at or below
    /// [`capacity`](Self::capacity) unless a terminal event was enqueued on a
    /// full queue (capacity + 1) or a legacy [`push`](Self::push) was used.
    pub fn max_queue_depth(&self) -> usize {
        self.lock().max_depth
    }

    /// How many [`push_await`](Self::push_await) calls had to wait for queue
    /// space (observability for producer backpressure).
    pub fn push_wait_count(&self) -> u64 {
        self.lock().push_waits
    }

    /// Returns true when the stream was still open (the terminal event
    /// finished it) and false when it had already terminated.
    ///
    /// The terminal message is timed under the lock, before the shared final
    /// result is cloned, so the event and every `result()` caller observe the
    /// same `durationMs` (upstream mutates the one live message object).
    fn finish_common(&self, mut event: AssistantMessageEvent) -> bool {
        let mut inner = self.lock();
        if inner.done {
            return false;
        }
        if let Some(message) = Self::terminal_message_mut(&mut event) {
            inner.time_terminal_message(message);
        }
        let final_message = Self::terminal_result(&event);
        inner.finish(event, final_message);
        true
    }
}

impl Default for AssistantMessageEventStream {
    fn default() -> Self {
        Self::new()
    }
}

impl futures_util::Stream for AssistantMessageEventStream {
    type Item = AssistantMessageEvent;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let mut inner = self.lock();
        if let Some(event) = inner.queue.pop_front() {
            inner.release_space();
            return Poll::Ready(Some(event));
        }
        if inner.done {
            return Poll::Ready(None);
        }
        inner.event_waiting.push_back(cx.waker().clone());
        Poll::Pending
    }
}

/// Builders for terminal events, matching the upstream event protocol.
impl AssistantMessageEventStream {
    /// Create a `Done` terminal event.
    pub fn done_event(reason: TerminalReason, message: AssistantMessage) -> AssistantMessageEvent {
        AssistantMessageEvent::Done { reason, message }
    }

    /// Create an `Error` terminal event.
    pub fn error_event(reason: ErrorReason, error: AssistantMessage) -> AssistantMessageEvent {
        AssistantMessageEvent::Error { reason, error }
    }
}
