//! JSON-line RPC client (upstream `modes/rpc/rpc-client.ts`).
//!
//! The coding agent's RPC mode speaks one JSON object per line on
//! stdin/stdout: command frames in, `{type:"response", id, command,
//! success, data|error}` replies and BARE session events out. This module is
//! the Rust counterpart of upstream's `RpcClient` class
//! (`rpc-client.ts:28-70`):
//!
//! - [`RpcClient::spawn`] starts `pir rpc` as a child process (upstream
//!   `start()` spawning `node dist/cli.js --mode rpc`);
//! - [`RpcClient::with_streams`] runs the same protocol over any async
//!   reader/writer pair, so embedders and tests can host the server
//!   in-process;
//! - every command is correlated by a client-issued `req_N` id
//!   (`pendingRequests`), with a per-request timeout;
//! - events are delivered to [`RpcClient::on_event`] listeners and to
//!   [`RpcClient::events`] receivers. The async fan-out is bounded by both
//!   event count and serialized bytes; a receiver that falls behind is given
//!   an explicit `event_stream_lagged` value (with the exact `missing` count)
//!   before the next retained event instead of silently losing the gap;
//! - [`RpcClient::wait_for_idle`]/[`RpcClient::wait_for_settled`] wait for the
//!   `agent_settled` event, [`RpcClient::collect_events`] and
//!   [`RpcClient::prompt_and_wait`] are the upstream `collectEvents` /
//!   `promptAndWait` equivalents;
//! - [`RpcClient::abort`] sends the abort command (the agent keeps running);
//!   [`RpcClient::close`] ends the transport and reaps the child.
//!
//! Responses whose id is unknown (a late reply after a timeout) are
//! dispatched to the event listeners like upstream's `handleLine`, never
//! silently dropped. A transport that dies rejects every pending request
//! with the same error (upstream `rejectPendingRequests`) and marks the
//! client closed.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::task::JoinHandle;

/// Default bound on one request/response round trip (upstream 30 s).
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Default bound on [`RpcClient::wait_for_idle`] / `collect_events`
/// (upstream 60 s).
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// How many events the [`RpcClient::events`] fan-out retains (the listener
/// callbacks never lag: they run on the reader task).
pub const EVENT_CHANNEL_CAPACITY: usize = 1024;

/// Byte cap for the [`RpcClient::events`] fan-out: the sum of the serialized
/// sizes of all retained events stays at or below this many bytes. Event
/// frames are capped at [`MAX_PROTOCOL_LINE_BYTES`] each, so a count-only
/// 1024-entry ring could pin a gigabyte of snapshots; this bound keeps the
/// retained payload at 16 MiB and drops the oldest events instead.
///
/// The accounting unit is the **compact JSON encoding** of each event (exactly
/// what `serde_json` emits: no whitespace, UTF-8, no trailing newline). It is
/// deliberately not an RSS/heap measurement: the in-memory [`Value`] adds an
/// enum tag, `Vec`/`Map` allocations and per-element overhead that are not
/// counted, so actual heap use can be a multiple of
/// [`RpcEventBufferStats::buffered_bytes`] (worst for numeric arrays, whose
/// JSON is 1-2 bytes per element). Treat this as a wire-format budget, not a
/// hard process-memory bound.
pub const EVENT_CHANNEL_BYTE_CAPACITY: usize = 16 * 1024 * 1024;

/// Largest single event admitted to the [`RpcClient::events`] fan-out,
/// measured as its serialized JSON size. A larger value is replaced by an
/// `event_stream_lagged` marker (`reason: "event_too_large"`) instead of
/// being retained. Well-formed peer lines are already bounded by
/// [`MAX_PROTOCOL_LINE_BYTES`], so this only guards in-process producers.
pub const MAX_EVENT_BYTES: usize = 2 * MAX_PROTOCOL_LINE_BYTES;

/// Default grace period [`RpcClient::close`] gives the child to exit after
/// stdin closes before killing it.
pub const CLOSE_GRACE: Duration = Duration::from_secs(1);

/// Bound on the captured stderr buffer (upstream accumulates without a
/// bound; a library should not grow without limit on a chatty child).
pub const STDERR_CAPTURE_LIMIT: usize = 1024 * 1024;

/// Maximum bytes of one stdout protocol line, excluding its `\n`/`\r\n`
/// terminator. A child that emits an unterminated line can no longer grow the
/// reader's buffer without bound; the limit matches the MCP message
/// (`mcp::transport::DEFAULT_MAX_MESSAGE_BYTES`) and SSE line caps.
pub const MAX_PROTOCOL_LINE_BYTES: usize = 1024 * 1024;

/// How an [`RpcClient`] process is started
/// (upstream `RpcClientOptions`).
#[derive(Debug, Clone)]
pub struct RpcClientOptions {
    /// Path to the `pir` executable (upstream `cliPath`).
    pub cli_path: PathBuf,
    /// Working directory for the child (upstream `cwd`).
    pub cwd: Option<PathBuf>,
    /// Environment variables added to the child's environment.
    pub env: Vec<(String, String)>,
    /// `--provider` (upstream `provider`).
    pub provider: Option<String>,
    /// `--model` (upstream `model`).
    pub model: Option<String>,
    /// Additional CLI arguments (upstream `args`).
    pub args: Vec<String>,
    /// Bound on one request/response round trip.
    pub request_timeout: Duration,
    /// Grace period before the child is killed on [`RpcClient::close`].
    pub close_grace: Duration,
}

impl RpcClientOptions {
    /// Options for the `pir` executable at `cli_path`.
    pub fn new(cli_path: impl Into<PathBuf>) -> Self {
        Self {
            cli_path: cli_path.into(),
            cwd: None,
            env: Vec::new(),
            provider: None,
            model: None,
            args: Vec::new(),
            request_timeout: REQUEST_TIMEOUT,
            close_grace: CLOSE_GRACE,
        }
    }
}

impl Default for RpcClientOptions {
    /// The current executable with no extra arguments — useful when the
    /// caller is itself a `pir` binary (`pir rpc`) or passes
    /// [`RpcClientOptions::new`] explicitly.
    fn default() -> Self {
        Self::new(std::env::current_exe().unwrap_or_else(|_| PathBuf::from("pir")))
    }
}

/// Everything an [`RpcClient`] operation can fail with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RpcClientError {
    /// The client was closed (transport gone, child exited, or
    /// [`RpcClient::close`] called).
    Closed,
    /// The transport failed to start or died mid-frame.
    Io(String),
    /// A request/response round trip exceeded its bound.
    Timeout { operation: &'static str },
    /// The peer sent a frame the protocol cannot use.
    Protocol(String),
    /// The peer sent a stdout line larger than [`MAX_PROTOCOL_LINE_BYTES`];
    /// the transport is closed (pending requests fail with this error).
    LineTooLong { limit: usize },
    /// The command answered `success: false` (upstream `getData` throwing the
    /// response's `error` text).
    Server { command: String, message: String },
}

impl std::fmt::Display for RpcClientError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RpcClientError::Closed => write!(formatter, "rpc client is closed"),
            RpcClientError::Io(message) => write!(formatter, "rpc transport error: {message}"),
            RpcClientError::Timeout { operation } => {
                write!(formatter, "rpc {operation} timed out")
            }
            RpcClientError::Protocol(message) => write!(formatter, "rpc protocol error: {message}"),
            RpcClientError::LineTooLong { limit } => {
                write!(
                    formatter,
                    "rpc protocol line exceeds the {limit}-byte limit"
                )
            }
            RpcClientError::Server { command, message } => {
                write!(formatter, "rpc command {command} failed: {message}")
            }
        }
    }
}

impl std::error::Error for RpcClientError {}

type RpcResult = Result<Value, RpcClientError>;
type EventListener = Arc<dyn Fn(&Value) + Send + Sync>;

/// Subscription handle from [`RpcClient::on_event`]; dropping it removes the
/// listener.
pub struct RpcEventSubscription {
    inner: Arc<RpcInner>,
    id: u64,
}

impl Drop for RpcEventSubscription {
    fn drop(&mut self) {
        let mut listeners = self
            .inner
            .listeners
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        listeners.retain(|(id, _)| *id != self.id);
    }
}

/// Snapshot of the bounded [`RpcClient::events`] fan-out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RpcEventBufferStats {
    /// Events currently retained by the shared ring.
    pub buffered_events: usize,
    /// Compact JSON bytes currently retained by the shared ring (see
    /// [`EVENT_CHANNEL_BYTE_CAPACITY`]). This is a serialized-byte budget,
    /// not an estimate of the `Value` heap or process RSS.
    pub buffered_bytes: usize,
    /// Events evicted because the count or byte cap was exceeded.
    pub dropped_events: u64,
    /// Single events rejected for exceeding [`MAX_EVENT_BYTES`].
    pub oversized_events: u64,
    /// Sum of the `missing` counts handed to receivers in
    /// `event_stream_lagged` values so far.
    pub reported_missing: u64,
    /// Number of `event_stream_lagged` values handed to receivers so far.
    pub lag_notices: u64,
}

/// One retained event in the fan-out ring.
struct BufferedEvent {
    /// Monotonic sequence assigned when the event entered the ring. Evicted
    /// sequence numbers are exactly what a lagging receiver is told it missed.
    sequence: u64,
    value: Value,
    /// Exact compact JSON size counted by [`EVENT_CHANNEL_BYTE_CAPACITY`].
    bytes: usize,
}

#[derive(Default)]
struct EventHubState {
    buffered: VecDeque<BufferedEvent>,
    /// Sequence number the next stored event will receive.
    next_sequence: u64,
    /// Sum of `bytes` over `buffered`.
    bytes: usize,
    /// Number of live [`RpcEventReceiver`]s.
    ///
    /// Guarded by the same mutex as `next_sequence`: [`EventHub::subscribe`]
    /// snapshots the cursor and registers here in one critical section, and
    /// [`EventHub::send`] decides whether to store under this lock. That is
    /// what makes "cursor snapshot + registration" atomic, so no event can be
    /// appended between reading the cursor and becoming a subscriber (which
    /// would silently skip it for the new receiver without a gap marker).
    subscribers: usize,
    closed: bool,
}

/// Bounded event fan-out for [`RpcClient::events`].
///
/// Unlike `tokio::sync::broadcast`, the ring is bounded by BOTH event count
/// and serialized bytes: a burst of large snapshots cannot pin a gigabyte of
/// memory just because it stayed at or below the 1024-entry count. Dropped
/// events are never silent: a receiver that missed events gets an in-band
/// `event_stream_lagged` value (with the exact `missing` count) before the
/// next retained event, and the loss is counted in
/// [`RpcClient::event_buffer_stats`].
struct EventHub {
    state: Mutex<EventHubState>,
    /// Wake-up channel only; event values live in `state` (authoritative).
    /// Lagging this channel is harmless: receivers re-check the state.
    wake: broadcast::Sender<()>,
    count_capacity: usize,
    byte_capacity: usize,
    max_event_bytes: usize,
    dropped_events: AtomicU64,
    oversized_events: AtomicU64,
    reported_missing: AtomicU64,
    lag_notices: AtomicU64,
}

thread_local! {
    /// Test-only hook used by [`RpcClient::events_with_registration_hook_for_test`].
    ///
    /// It runs inside [`EventHub::subscribe`] after the cursor snapshot and
    /// before the subscriber count is bumped, with the state lock held. The
    /// regression test uses it to inject a concurrent `send` into the
    /// registration window and assert that it cannot observe zero subscribers.
    static REGISTRATION_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
}

fn run_registration_hook() {
    let hook = REGISTRATION_HOOK.with(|slot| slot.borrow_mut().take());
    if let Some(hook) = hook {
        hook();
    }
}

impl EventHub {
    fn new(count_capacity: usize, byte_capacity: usize, max_event_bytes: usize) -> Self {
        let (wake, _) = broadcast::channel(64);
        Self {
            state: Mutex::new(EventHubState::default()),
            wake,
            count_capacity,
            byte_capacity,
            max_event_bytes,
            dropped_events: AtomicU64::new(0),
            oversized_events: AtomicU64::new(0),
            reported_missing: AtomicU64::new(0),
            lag_notices: AtomicU64::new(0),
        }
    }

    fn subscribe(self: &Arc<Self>) -> RpcEventReceiver {
        // Arming the wake channel, snapshotting the cursor and registering
        // the subscriber all happen while holding `state`. `send` decides
        // whether to store under that same lock, so a concurrent send either
        // commits before this registration (its value was never claimed by
        // this receiver) or commits after it with a sequence >= this cursor
        // (the receiver observes it, or an explicit lag marker if it was
        // evicted later). There is no "cursor read but not yet registered"
        // window in which a stored event could be skipped without a marker.
        //
        // Holding `state` across `wake.subscribe()` is deliberate: the wake
        // subscription must be armed before registration so a later send's
        // `wake.send` reaches this receiver, and doing it inside the critical
        // section moves the registration point to lock acquisition instead of
        // after the wake subscription. `send`/`close` only call `wake.send`
        // after releasing `state`, so the lock order cannot invert.
        //
        // "Events emitted before `events()`" are still not replayed: the
        // cursor starts at the sequence after everything already stored.
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let wake = self.wake.subscribe();
        let next_sequence = state.next_sequence;
        // `#[doc(hidden)]` test seam: runs between the cursor snapshot and the
        // registration while this lock is held, so a concurrent send cannot
        // observe an intermediate (unregistered) state.
        run_registration_hook();
        state.subscribers += 1;
        drop(state);
        RpcEventReceiver {
            hub: Arc::clone(self),
            wake,
            next_sequence,
        }
    }

    fn has_subscribers(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .subscribers
            > 0
    }

    fn send(&self, value: Value) {
        // `broadcast::Sender::send` does not retain events when nobody is
        // subscribed; do the same and avoid serializing/storing a large
        // snapshot. This first check is only a fast path; the authoritative
        // one is repeated below under the `state` lock, the same critical
        // section that registers subscribers.
        if !self.has_subscribers() {
            return;
        }
        let mut value = value;
        let mut bytes = serialized_event_bytes(&value);
        if bytes > self.max_event_bytes || bytes > self.byte_capacity {
            // A single event that cannot fit the budget is rejected: keep a
            // small observable marker instead of pinning the oversized value.
            self.oversized_events.fetch_add(1, Ordering::SeqCst);
            value = lag_event(1, "event_too_large");
            bytes = serialized_event_bytes(&value);
        }
        let evicted = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // Re-check under the lock: `subscribe` registers under this same
            // lock, so a receiver that registers before this point is
            // guaranteed to see the event (its cursor was snapshotted before
            // this sequence was assigned); a receiver that registers after
            // returns without the value.
            if state.closed || state.subscribers == 0 {
                return;
            }
            let sequence = state.next_sequence;
            state.next_sequence += 1;
            state.bytes += bytes;
            state.buffered.push_back(BufferedEvent {
                sequence,
                value,
                bytes,
            });
            let mut evicted = 0_u64;
            // Drop oldest first: count and byte budgets are both enforced.
            while state.bytes > self.byte_capacity || state.buffered.len() > self.count_capacity {
                let Some(front) = state.buffered.pop_front() else {
                    break;
                };
                state.bytes -= front.bytes;
                evicted += 1;
            }
            evicted
        };
        if evicted > 0 {
            self.dropped_events.fetch_add(evicted, Ordering::SeqCst);
        }
        let _ = self.wake.send(());
    }

    fn close(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.closed = true;
        drop(state);
        let _ = self.wake.send(());
    }

    fn record_lag(&self, missing: u64) {
        self.reported_missing.fetch_add(missing, Ordering::SeqCst);
        self.lag_notices.fetch_add(1, Ordering::SeqCst);
    }

    fn stats(&self) -> RpcEventBufferStats {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        RpcEventBufferStats {
            buffered_events: state.buffered.len(),
            buffered_bytes: state.bytes,
            dropped_events: self.dropped_events.load(Ordering::SeqCst),
            oversized_events: self.oversized_events.load(Ordering::SeqCst),
            reported_missing: self.reported_missing.load(Ordering::SeqCst),
            lag_notices: self.lag_notices.load(Ordering::SeqCst),
        }
    }
}

/// Error returned by [`RpcEventReceiver::recv`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RpcEventRecvError {
    /// The client went away and every retained event was already delivered.
    Closed,
}

impl std::fmt::Display for RpcEventRecvError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RpcEventRecvError::Closed => write!(formatter, "rpc event stream is closed"),
        }
    }
}

impl std::error::Error for RpcEventRecvError {}

/// Error returned by [`RpcEventReceiver::try_recv`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RpcEventTryRecvError {
    /// No event is currently buffered.
    Empty,
    /// The client went away and every retained event was already delivered.
    Closed,
}

/// Async event stream returned by [`RpcClient::events`].
///
/// The stream is bounded by both event count ([`EVENT_CHANNEL_CAPACITY`]) and
/// serialized bytes ([`EVENT_CHANNEL_BYTE_CAPACITY`]). When a consumer falls
/// behind, the next `recv` yields an explicit value
/// `{"type":"event_stream_lagged","missing":N,"reason":"..."}` before the
/// oldest retained event, so the gap is observable instead of silent.
pub struct RpcEventReceiver {
    hub: Arc<EventHub>,
    wake: broadcast::Receiver<()>,
    next_sequence: u64,
}

impl RpcEventReceiver {
    /// Wait for the next event, or an explicit gap marker when the consumer
    /// fell behind the bounded ring.
    pub async fn recv(&mut self) -> Result<Value, RpcEventRecvError> {
        loop {
            match self.try_recv() {
                Ok(value) => return Ok(value),
                Err(RpcEventTryRecvError::Closed) => return Err(RpcEventRecvError::Closed),
                Err(RpcEventTryRecvError::Empty) => {}
            }
            match self.wake.recv().await {
                // The wake payload is irrelevant; the state is authoritative.
                Ok(()) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => {
                    return match self.try_recv() {
                        Ok(value) => Ok(value),
                        Err(_) => Err(RpcEventRecvError::Closed),
                    };
                }
            }
        }
    }

    /// Non-blocking [`Self::recv`].
    pub fn try_recv(&mut self) -> Result<Value, RpcEventTryRecvError> {
        let state = self
            .hub
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(front) = state.buffered.front() {
            if self.next_sequence < front.sequence {
                let missing = front.sequence - self.next_sequence;
                self.next_sequence = front.sequence;
                drop(state);
                self.hub.record_lag(missing);
                return Ok(lag_event(missing, "buffer_overflow"));
            }
            let index = (self.next_sequence - front.sequence) as usize;
            if let Some(event) = state.buffered.get(index) {
                self.next_sequence += 1;
                return Ok(event.value.clone());
            }
        } else if self.next_sequence < state.next_sequence {
            // Everything was evicted (or already skipped) before this receipt.
            let missing = state.next_sequence - self.next_sequence;
            self.next_sequence = state.next_sequence;
            drop(state);
            self.hub.record_lag(missing);
            return Ok(lag_event(missing, "buffer_overflow"));
        }
        if state.closed {
            Err(RpcEventTryRecvError::Closed)
        } else {
            Err(RpcEventTryRecvError::Empty)
        }
    }
}

impl Drop for RpcEventReceiver {
    fn drop(&mut self) {
        // Paired with `EventHub::subscribe`: the count lives under the state
        // lock so `send`'s under-lock check is exact.
        let mut state = self
            .hub
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.subscribers = state.subscribers.saturating_sub(1);
    }
}

/// Exact compact JSON size of one event, counted without allocating the
/// encoded form (large snapshots would otherwise be serialized twice).
///
/// This is the budget unit documented on [`EVENT_CHANNEL_BYTE_CAPACITY`]:
/// serialized bytes, not the `Value` heap.
fn serialized_event_bytes(value: &Value) -> usize {
    struct Counter(usize);

    impl std::io::Write for Counter {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            self.0 = self.0.saturating_add(buffer.len());
            Ok(buffer.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let mut counter = Counter(0);
    match serde_json::to_writer(&mut counter, value) {
        Ok(()) => counter.0,
        // A value that cannot be serialized cannot be measured; treat it as
        // oversized so it never enters the ring.
        Err(_) => usize::MAX,
    }
}

/// The in-band gap notification emitted by the client (never by the peer).
fn lag_event(missing: u64, reason: &str) -> Value {
    json!({
        "type": "event_stream_lagged",
        "missing": missing,
        "reason": reason,
    })
}

struct RpcInner {
    /// Writer task channel; `None` once the client is closed.
    writer: tokio::sync::Mutex<Option<mpsc::Sender<String>>>,
    /// Requests awaiting a response, keyed by the client-issued id.
    pending: Mutex<HashMap<String, oneshot::Sender<RpcResult>>>,
    /// Event fan-out for async consumers (count- and byte-bounded).
    events: Arc<EventHub>,
    /// Event listeners (upstream `eventListeners`).
    listeners: RwLock<Vec<(u64, EventListener)>>,
    next_listener: AtomicU64,
    next_request: AtomicU64,
    closed: AtomicBool,
    request_timeout: Duration,
    close_grace: Duration,
    /// Captured child stderr (upstream `stderr`).
    stderr: Mutex<String>,
    child: tokio::sync::Mutex<Option<Child>>,
    reader_task: tokio::sync::Mutex<Option<JoinHandle<()>>>,
    writer_task: tokio::sync::Mutex<Option<JoinHandle<()>>>,
    stderr_task: tokio::sync::Mutex<Option<JoinHandle<()>>>,
}

impl RpcInner {
    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// Mark the transport closed and fail every pending request (upstream
    /// `rejectPendingRequests`). Idempotent.
    fn close_with(&self, error: RpcClientError, close_writer: bool) {
        self.closed.store(true, Ordering::SeqCst);
        let pending = {
            let mut pending = self
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            std::mem::take(&mut *pending)
        };
        for (_, sender) in pending {
            let _ = sender.send(Err(error.clone()));
        }
        if close_writer {
            // Dropping the sender ends the writer task, which drops the
            // child's stdin. `try_lock` is enough: the only await on this
            // mutex is a `send` clone acquisition.
            if let Ok(mut writer) = self.writer.try_lock() {
                writer.take();
            }
        }
    }

    fn push_stderr(&self, chunk: &str) {
        let Ok(mut stderr) = self.stderr.lock() else {
            return;
        };
        if stderr.len() >= STDERR_CAPTURE_LIMIT {
            return;
        }
        let remaining = STDERR_CAPTURE_LIMIT - stderr.len();
        if chunk.len() <= remaining {
            stderr.push_str(chunk);
        } else {
            let mut cut = remaining;
            while cut > 0 && !chunk.is_char_boundary(cut) {
                cut -= 1;
            }
            stderr.push_str(&chunk[..cut]);
        }
    }

    fn dispatch_event(&self, value: Value) {
        // Broadcast first (a full/without-receivers channel is fine: upstream
        // listeners are reached below regardless).
        self.events.send(value.clone());
        let listeners = self
            .listeners
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        for (_, listener) in listeners {
            listener(&value);
        }
    }
}

/// A connected JSON-line RPC client.
pub struct RpcClient {
    inner: Arc<RpcInner>,
    /// External clone count. The writer/reader tasks hold strong `RpcInner`
    /// references, so `Arc::strong_count` cannot tell when the last user
    /// handle goes away; this counter can.
    external: Arc<AtomicUsize>,
}

impl Clone for RpcClient {
    fn clone(&self) -> Self {
        self.external.fetch_add(1, Ordering::SeqCst);
        Self {
            inner: Arc::clone(&self.inner),
            external: Arc::clone(&self.external),
        }
    }
}

impl Drop for RpcClient {
    fn drop(&mut self) {
        if self.external.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.inner.close_now();
        }
    }
}

impl RpcClient {
    /// Start `pir rpc` as a child process and wire the protocol
    /// (upstream `RpcClient.start`).
    pub async fn spawn(options: RpcClientOptions) -> Result<Self, RpcClientError> {
        let mut command = Command::new(&options.cli_path);
        command.arg("rpc");
        if let Some(provider) = &options.provider {
            command.arg("--provider").arg(provider);
        }
        if let Some(model) = &options.model {
            command.arg("--model").arg(model);
        }
        command.args(&options.args);
        command
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        if let Some(cwd) = &options.cwd {
            command.current_dir(cwd);
        }
        for (key, value) in &options.env {
            command.env(key, value);
        }
        let mut child = command.spawn().map_err(|error| {
            RpcClientError::Io(format!(
                "cannot start {}: {error}",
                options.cli_path.display()
            ))
        })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| RpcClientError::Io("child stdin is not piped".to_owned()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| RpcClientError::Io("child stdout is not piped".to_owned()))?;
        let stderr = child.stderr.take();
        let client = Self::with_streams_and_timeouts(
            stdout,
            stdin,
            options.request_timeout,
            options.close_grace,
        );
        *client.inner.child.lock().await = Some(child);
        if let Some(stderr) = stderr {
            let inner = Arc::clone(&client.inner);
            let task = tokio::spawn(async move {
                let mut reader = BufReader::new(stderr);
                let mut line = String::new();
                loop {
                    line.clear();
                    match reader.read_line(&mut line).await {
                        Ok(0) | Err(_) => return,
                        Ok(_) => inner.push_stderr(&line),
                    }
                }
            });
            *client.inner.stderr_task.lock().await = Some(task);
        }
        Ok(client)
    }

    /// Run the protocol over an existing reader/writer pair (for an
    /// in-process host or a test). Must be called inside a tokio runtime.
    pub fn with_streams<R, W>(reader: R, writer: W) -> Self
    where
        R: tokio::io::AsyncRead + Unpin + Send + 'static,
        W: tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        Self::with_streams_and_timeouts(reader, writer, REQUEST_TIMEOUT, CLOSE_GRACE)
    }

    /// [`Self::with_streams`] with explicit bounds.
    pub fn with_streams_and_timeouts<R, W>(
        reader: R,
        writer: W,
        request_timeout: Duration,
        close_grace: Duration,
    ) -> Self
    where
        R: tokio::io::AsyncRead + Unpin + Send + 'static,
        W: tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let events = Arc::new(EventHub::new(
            EVENT_CHANNEL_CAPACITY,
            EVENT_CHANNEL_BYTE_CAPACITY,
            MAX_EVENT_BYTES,
        ));
        let (line_tx, mut line_rx) = mpsc::channel::<String>(256);
        let inner = Arc::new(RpcInner {
            writer: tokio::sync::Mutex::new(Some(line_tx)),
            pending: Mutex::new(HashMap::new()),
            events,
            listeners: RwLock::new(Vec::new()),
            next_listener: AtomicU64::new(0),
            next_request: AtomicU64::new(0),
            closed: AtomicBool::new(false),
            request_timeout,
            close_grace,
            stderr: Mutex::new(String::new()),
            child: tokio::sync::Mutex::new(None),
            reader_task: tokio::sync::Mutex::new(None),
            writer_task: tokio::sync::Mutex::new(None),
            stderr_task: tokio::sync::Mutex::new(None),
        });

        // Writer task: one protocol line per item, flushed per line.
        let writer_inner = Arc::clone(&inner);
        let writer_task = tokio::spawn(async move {
            let mut writer = writer;
            while let Some(mut line) = line_rx.recv().await {
                line.push('\n');
                if let Err(error) = writer.write_all(line.as_bytes()).await {
                    writer_inner
                        .close_with(RpcClientError::Io(format!("write failed: {error}")), false);
                    return;
                }
                if let Err(error) = writer.flush().await {
                    writer_inner
                        .close_with(RpcClientError::Io(format!("flush failed: {error}")), false);
                    return;
                }
            }
        });
        // Reader task: route responses by id, everything else as an event.
        let reader_inner = Arc::clone(&inner);
        let reader_task = tokio::spawn(async move {
            let mut reader = BufReader::new(reader);
            let mut bytes = Vec::new();
            loop {
                bytes.clear();
                // Read one line, but never buffer more than the cap plus its
                // two-byte terminator. `take` caps `read_until`'s fills, so an
                // unterminated line cannot grow `bytes` past this bound.
                let limit = MAX_PROTOCOL_LINE_BYTES as u64 + 2;
                match (&mut reader)
                    .take(limit)
                    .read_until(b'\n', &mut bytes)
                    .await
                {
                    Ok(0) => {
                        reader_inner
                            .close_with(RpcClientError::Io("rpc stdout closed".to_owned()), false);
                        return;
                    }
                    Ok(_) => {}
                    Err(error) => {
                        reader_inner
                            .close_with(RpcClientError::Io(format!("read failed: {error}")), false);
                        return;
                    }
                }
                // Strip the line terminator before measuring: a line may carry
                // at most `MAX_PROTOCOL_LINE_BYTES` of JSON text.
                let terminated_by = if bytes.ends_with(b"\r\n") {
                    2
                } else {
                    usize::from(bytes.ends_with(b"\n"))
                };
                if bytes.len() - terminated_by > MAX_PROTOCOL_LINE_BYTES {
                    reader_inner.close_with(
                        RpcClientError::LineTooLong {
                            limit: MAX_PROTOCOL_LINE_BYTES,
                        },
                        false,
                    );
                    // Drop the writer so the peer observes EOF too. `close_with`
                    // only tries the lock (it may race a finishing send), so the
                    // reader task awaits it here instead.
                    reader_inner.writer.lock().await.take();
                    return;
                }
                let Ok(line) = std::str::from_utf8(&bytes) else {
                    reader_inner.close_with(
                        RpcClientError::Io("read failed: invalid UTF-8".to_owned()),
                        false,
                    );
                    return;
                };
                let line = line.trim_end_matches(['\r', '\n']);
                if line.is_empty() {
                    continue;
                }
                let Ok(value) = serde_json::from_str::<Value>(line) else {
                    // Upstream ignores non-JSON lines (`handleLine` catch).
                    continue;
                };
                let response_id = (value.get("type").and_then(Value::as_str) == Some("response"))
                    .then(|| value.get("id").and_then(Value::as_str).map(str::to_owned))
                    .flatten();
                if let Some(id) = response_id {
                    let pending = reader_inner
                        .pending
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .remove(&id);
                    if let Some(sender) = pending {
                        let _ = sender.send(Ok(value));
                        continue;
                    }
                }
                // A late/unknown response or a session event: dispatch it
                // (upstream treats every non-pending line as an event).
                reader_inner.dispatch_event(value);
            }
        });
        // The tasks cannot be installed with `await` in a sync constructor;
        // `block_in_place` is wrong here, so park the handles behind a
        // short-lived runtime handoff: `tokio::spawn` already schedules them.
        // Store them lazily via `try_lock` (no contention at construction).
        if let Ok(mut slot) = inner.reader_task.try_lock() {
            *slot = Some(reader_task);
        }
        if let Ok(mut slot) = inner.writer_task.try_lock() {
            *slot = Some(writer_task);
        }
        Self {
            inner,
            external: Arc::new(AtomicUsize::new(1)),
        }
    }

    /// Subscribe to agent events as an async stream. Events emitted before
    /// this call are not replayed — register first, then send the command
    /// (upstream `onEvent` has the same ordering requirement).
    ///
    /// The cursor snapshot and the subscriber registration happen in one
    /// critical section, so once this call returns every event the dispatcher
    /// stores reaches this receiver (or triggers an explicit lag marker);
    /// there is no "cursor read but not yet registered" window.
    ///
    /// The stream is bounded by [`EVENT_CHANNEL_CAPACITY`] entries and
    /// [`EVENT_CHANNEL_BYTE_CAPACITY`] serialized bytes. A consumer that falls
    /// behind receives an explicit `event_stream_lagged` value carrying the
    /// number of missed events before the next retained event.
    pub fn events(&self) -> RpcEventReceiver {
        self.inner.events.subscribe()
    }

    /// Test-only variant of [`Self::events`]: `hook` runs inside
    /// [`EventHub::subscribe`] between the cursor snapshot and the subscriber
    /// registration, while the state lock is held.
    ///
    /// Regression tests use it to force a concurrent `send` into the old
    /// "cursor read but not yet registered" window. Production callers must
    /// use [`Self::events`]; the hook is consumed by this call and never
    /// stored.
    #[doc(hidden)]
    pub fn events_with_registration_hook_for_test<F>(&self, hook: F) -> RpcEventReceiver
    where
        F: FnOnce() + 'static,
    {
        REGISTRATION_HOOK.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(hook));
        });
        let receiver = self.inner.events.subscribe();
        // The hook is taken by `subscribe`; clear a leftover from a panicking
        // hook so it cannot leak into the next subscription on this thread.
        REGISTRATION_HOOK.with(|slot| {
            *slot.borrow_mut() = None;
        });
        receiver
    }

    /// Snapshot of the event fan-out bounds and loss counters (see
    /// [`RpcEventBufferStats`]).
    pub fn event_buffer_stats(&self) -> RpcEventBufferStats {
        self.inner.events.stats()
    }

    /// Register a synchronous event listener (upstream `onEvent`). The
    /// returned handle removes it on drop.
    pub fn on_event<F>(&self, listener: F) -> RpcEventSubscription
    where
        F: Fn(&Value) + Send + Sync + 'static,
    {
        let id = self.inner.next_listener.fetch_add(1, Ordering::SeqCst);
        self.inner
            .listeners
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((id, Arc::new(listener)));
        RpcEventSubscription {
            inner: Arc::clone(&self.inner),
            id,
        }
    }

    /// Captured child stderr (upstream `getStderr`), bounded by
    /// [`STDERR_CAPTURE_LIMIT`].
    pub fn stderr(&self) -> String {
        self.inner
            .stderr
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Whether the transport is closed (child exited, stream ended, or
    /// [`Self::close`] called).
    pub fn is_closed(&self) -> bool {
        self.inner.is_closed()
    }

    // ------------------------------------------------------------------
    // Transport
    // ------------------------------------------------------------------

    /// Send one command and await its response frame. The response is
    /// returned verbatim, including `success: false` (upstream `send`);
    /// [`Self::send_ok`] is the `getData`-style variant.
    pub async fn send(&self, command: Value) -> RpcResult {
        if self.inner.is_closed() {
            return Err(RpcClientError::Closed);
        }
        let Value::Object(mut object) = command else {
            return Err(RpcClientError::Protocol(
                "a command must be a JSON object".to_owned(),
            ));
        };
        let id = format!(
            "req_{}",
            self.inner.next_request.fetch_add(1, Ordering::SeqCst) + 1
        );
        object.insert("id".to_owned(), json!(id));
        let command = Value::Object(object);
        let (sender, receiver) = oneshot::channel();
        self.inner
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id.clone(), sender);
        // Close may have landed between the open check and the registration:
        // its pending sweep could not see this id, so answer it here instead of
        // waiting out the request timeout.
        if self.inner.is_closed() {
            self.drop_pending(&id);
            return Err(RpcClientError::Closed);
        }
        let line = match serde_json::to_string(&command) {
            Ok(line) => line,
            Err(error) => {
                self.drop_pending(&id);
                return Err(RpcClientError::Protocol(format!(
                    "cannot serialize command: {error}"
                )));
            }
        };
        let writer = {
            let writer = self.inner.writer.lock().await;
            writer.clone()
        };
        let Some(writer) = writer else {
            self.drop_pending(&id);
            return Err(RpcClientError::Closed);
        };
        // Enqueueing must share the request bound with the response wait: a
        // peer that stops reading stdin keeps the writer task blocked in
        // `write_all`, so a full 256-slot channel would otherwise park this
        // send forever. Timing out cancels the queue wait, so an unsent line
        // cannot be written later.
        match tokio::time::timeout(self.inner.request_timeout, writer.send(line)).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => {
                self.drop_pending(&id);
                return Err(RpcClientError::Closed);
            }
            Err(_) => {
                self.drop_pending(&id);
                return Err(RpcClientError::Timeout { operation: "send" });
            }
        }
        match tokio::time::timeout(self.inner.request_timeout, receiver).await {
            Ok(Ok(response)) => response,
            Ok(Err(_)) => Err(RpcClientError::Closed),
            Err(_) => {
                self.drop_pending(&id);
                Err(RpcClientError::Timeout {
                    operation: "response",
                })
            }
        }
    }

    /// [`Self::send`] with the upstream `getData` contract: a `success: false`
    /// response becomes [`RpcClientError::Server`], and a successful response
    /// yields its `data` (or `null` when absent).
    pub async fn send_ok(&self, command: Value) -> RpcResult {
        let command_type = command
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let mut response = self.send(command).await?;
        if response.get("success").and_then(Value::as_bool) != Some(true) {
            let message = response
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("rpc command failed")
                .to_owned();
            return Err(RpcClientError::Server {
                command: command_type,
                message,
            });
        }
        Ok(response
            .get_mut("data")
            .map(Value::take)
            .unwrap_or(Value::Null))
    }

    fn drop_pending(&self, id: &str) {
        self.inner
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(id);
    }

    /// Wait for one event matching `predicate` within `timeout`
    /// (upstream listener + timer). Subscribe before the triggering command.
    pub async fn wait_for_event<F>(
        &self,
        predicate: F,
        timeout: Duration,
    ) -> Result<Value, RpcClientError>
    where
        F: Fn(&Value) -> bool + Send + 'static,
    {
        let mut receiver = self.events();
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(RpcClientError::Timeout { operation: "event" });
            }
            match tokio::time::timeout(remaining, receiver.recv()).await {
                Ok(Ok(value)) => {
                    if predicate(&value) {
                        return Ok(value);
                    }
                }
                Ok(Err(RpcEventRecvError::Closed)) => return Err(RpcClientError::Closed),
                Err(_) => return Err(RpcClientError::Timeout { operation: "event" }),
            }
        }
    }

    /// Wait for the agent to become idle: the `agent_settled` session event
    /// (upstream `waitForIdle`). Call it BEFORE the run starts (a broadcast
    /// receiver sees only events emitted after it subscribed).
    pub async fn wait_for_idle(&self, timeout: Duration) -> Result<Value, RpcClientError> {
        self.wait_for_settled(timeout).await
    }

    /// [`Self::wait_for_idle`] under its event-name spelling.
    pub async fn wait_for_settled(&self, timeout: Duration) -> Result<Value, RpcClientError> {
        self.wait_for_event(
            |value| value.get("type").and_then(Value::as_str) == Some("agent_settled"),
            timeout,
        )
        .await
    }

    /// Collect events until `agent_settled` (upstream `collectEvents`).
    pub async fn collect_events(&self, timeout: Duration) -> Result<Vec<Value>, RpcClientError> {
        let mut receiver = self.events();
        let deadline = tokio::time::Instant::now() + timeout;
        let mut events = Vec::new();
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(RpcClientError::Timeout {
                    operation: "collect_events",
                });
            }
            match tokio::time::timeout(remaining, receiver.recv()).await {
                Ok(Ok(value)) => {
                    let settled =
                        value.get("type").and_then(Value::as_str) == Some("agent_settled");
                    events.push(value);
                    if settled {
                        return Ok(events);
                    }
                }
                Ok(Err(RpcEventRecvError::Closed)) => return Err(RpcClientError::Closed),
                Err(_) => {
                    return Err(RpcClientError::Timeout {
                        operation: "collect_events",
                    })
                }
            }
        }
    }

    /// Send a prompt and collect the run's events until `agent_settled`
    /// (upstream `promptAndWait`). A `"handled"` disposition means no run
    /// starts, so an empty event list is returned (upstream documents the same
    /// and leaves the caller not to wait for `agent_settled`).
    pub async fn prompt_and_wait(
        &self,
        text: &str,
        images: Option<&[Value]>,
        timeout: Duration,
    ) -> Result<Vec<Value>, RpcClientError> {
        let mut receiver = self.events();
        let disposition = self.prompt_with(text, images, None).await?;
        if disposition == "handled" {
            return Ok(Vec::new());
        }
        collect_from(&mut receiver, timeout).await
    }

    // ------------------------------------------------------------------
    // Commands (upstream rpc-client.ts command methods)
    // ------------------------------------------------------------------

    /// `hello`: protocol version and name.
    pub async fn hello(&self) -> RpcResult {
        self.send_ok(json!({ "type": "hello" })).await
    }

    /// `list`: sessions in the host's session directory.
    pub async fn list(&self) -> RpcResult {
        self.send_ok(json!({ "type": "list" })).await
    }

    /// `create`: start a fresh session (optionally in `cwd`).
    pub async fn create(&self, cwd: Option<&str>) -> RpcResult {
        let mut command = json!({ "type": "create" });
        if let Some(cwd) = cwd {
            command["cwd"] = json!(cwd);
        }
        self.send_ok(command).await
    }

    /// `attach`: attach a session by file name/path.
    pub async fn attach(&self, session: Option<&str>) -> RpcResult {
        let mut command = json!({ "type": "attach" });
        if let Some(session) = session {
            command["session"] = json!(session);
        }
        self.send_ok(command).await
    }

    /// Send a prompt (upstream `prompt`). Returns the session's disposition
    /// string; events stream through [`Self::events`]/[`Self::on_event`].
    pub async fn prompt(&self, message: &str) -> Result<String, RpcClientError> {
        self.prompt_with(message, None, None).await
    }

    /// [`Self::prompt`] with images (`[{data, mimeType}]`) and an optional
    /// `streamingBehavior` (`"steer"`/`"followUp"`).
    pub async fn prompt_with(
        &self,
        message: &str,
        images: Option<&[Value]>,
        streaming_behavior: Option<&str>,
    ) -> Result<String, RpcClientError> {
        let mut command = json!({ "type": "prompt", "message": message });
        if let Some(images) = images {
            command["images"] = Value::Array(images.to_vec());
        }
        if let Some(behavior) = streaming_behavior {
            command["streamingBehavior"] = json!(behavior);
        }
        disposition(self.send_ok(command).await?)
    }

    /// Queue a steering message into a live run.
    pub async fn steer(&self, message: &str) -> Result<String, RpcClientError> {
        self.queued_input("steer", message).await
    }

    /// [`Self::steer`] with images (`[{data, mimeType}]`, upstream
    /// `steer(message, images?)`).
    pub async fn steer_with(
        &self,
        message: &str,
        images: &[Value],
    ) -> Result<String, RpcClientError> {
        self.queued_input_with("steer", message, Some(images)).await
    }

    /// Queue a follow-up message for after the run finishes.
    pub async fn follow_up(&self, message: &str) -> Result<String, RpcClientError> {
        self.queued_input("follow_up", message).await
    }

    /// [`Self::follow_up`] with images (upstream `followUp(message, images?)`).
    pub async fn follow_up_with(
        &self,
        message: &str,
        images: &[Value],
    ) -> Result<String, RpcClientError> {
        self.queued_input_with("follow_up", message, Some(images))
            .await
    }

    async fn queued_input(
        &self,
        command_type: &str,
        message: &str,
    ) -> Result<String, RpcClientError> {
        self.queued_input_with(command_type, message, None).await
    }

    async fn queued_input_with(
        &self,
        command_type: &str,
        message: &str,
        images: Option<&[Value]>,
    ) -> Result<String, RpcClientError> {
        let mut command = json!({ "type": command_type, "message": message });
        if let Some(images) = images {
            command["images"] = Value::Array(images.to_vec());
        }
        let data = self.send_ok(command).await?;
        disposition(data)
    }

    /// Abort the current operation (upstream `abort`). The agent keeps
    /// serving; events (including `agent_settled`) still stream.
    pub async fn abort(&self) -> Result<(), RpcClientError> {
        self.send_ok(json!({ "type": "abort" })).await.map(|_| ())
    }

    /// Clear queued steering/follow-up messages (upstream `clearQueue`).
    pub async fn clear_queue(&self) -> RpcResult {
        self.send_ok(json!({ "type": "clear_queue" })).await
    }

    /// Get the session state snapshot.
    pub async fn get_state(&self) -> RpcResult {
        self.send_ok(json!({ "type": "get_state" })).await
    }

    /// Compact the session (upstream `compact`): the full `CompactionResult`
    /// (`summary`, `firstKeptEntryId`, `tokensBefore`, `estimatedTokensAfter`,
    /// `usage`, `details`).
    pub async fn compact(&self, custom_instructions: Option<&str>) -> RpcResult {
        let mut command = json!({ "type": "compact" });
        if let Some(instructions) = custom_instructions {
            command["customInstructions"] = json!(instructions);
        }
        self.send_ok(command).await
    }

    /// `get_messages`: the complete session message list by default, or the
    /// LLM projection with `view = "llm"` (see [`Self::get_messages_view`]).
    pub async fn get_messages(&self) -> RpcResult {
        self.send_ok(json!({ "type": "get_messages" })).await
    }

    /// `get_messages` with an explicit view: `"full"` (upstream
    /// `session.messages`) or `"llm"` (the context projection).
    pub async fn get_messages_view(&self, view: &str) -> RpcResult {
        self.send_ok(json!({ "type": "get_messages", "view": view }))
            .await
    }

    /// `get_commands`: extension/prompt/skill commands with `sourceInfo`.
    pub async fn get_commands(&self) -> RpcResult {
        self.send_ok(json!({ "type": "get_commands" })).await
    }

    /// `get_entries`: the session entries (optionally after `since`).
    pub async fn get_entries(&self, since: Option<&str>) -> RpcResult {
        let mut command = json!({ "type": "get_entries" });
        if let Some(since) = since {
            command["since"] = json!(since);
        }
        self.send_ok(command).await
    }

    /// `get_tree`: the session entry tree.
    pub async fn get_tree(&self) -> RpcResult {
        self.send_ok(json!({ "type": "get_tree" })).await
    }

    /// `get_session_stats`.
    pub async fn get_session_stats(&self) -> RpcResult {
        self.send_ok(json!({ "type": "get_session_stats" })).await
    }

    /// `get_fork_messages`: user messages available for forking.
    pub async fn get_fork_messages(&self) -> RpcResult {
        self.send_ok(json!({ "type": "get_fork_messages" })).await
    }

    /// `get_last_assistant_text`.
    pub async fn get_last_assistant_text(&self) -> RpcResult {
        self.send_ok(json!({ "type": "get_last_assistant_text" }))
            .await
    }

    /// `set_session_name`.
    pub async fn set_session_name(&self, name: &str) -> Result<(), RpcClientError> {
        self.send_ok(json!({ "type": "set_session_name", "name": name }))
            .await
            .map(|_| ())
    }

    /// `bash`: run one command through the session's bash tool.
    pub async fn bash(&self, command: &str) -> RpcResult {
        self.bash_with_options(command, false).await
    }

    /// [`Self::bash`] with `excludeFromContext` (upstream `RpcCommand`
    /// carries the optional flag).
    pub async fn bash_with_options(&self, command: &str, exclude_from_context: bool) -> RpcResult {
        let mut request = json!({ "type": "bash", "command": command });
        if exclude_from_context {
            request["excludeFromContext"] = json!(true);
        }
        self.send_ok(request).await
    }

    /// `abort_bash`.
    pub async fn abort_bash(&self) -> Result<(), RpcClientError> {
        self.send_ok(json!({ "type": "abort_bash" }))
            .await
            .map(|_| ())
    }

    /// `set_model`.
    pub async fn set_model(&self, provider: &str, model_id: &str) -> RpcResult {
        self.send_ok(json!({
            "type": "set_model",
            "provider": provider,
            "modelId": model_id,
        }))
        .await
    }

    /// `get_available_models`.
    pub async fn get_available_models(&self) -> RpcResult {
        self.send_ok(json!({ "type": "get_available_models" }))
            .await
    }

    /// `cycle_model`.
    pub async fn cycle_model(&self) -> RpcResult {
        self.send_ok(json!({ "type": "cycle_model" })).await
    }

    /// `set_thinking_level`.
    pub async fn set_thinking_level(&self, level: &str) -> Result<(), RpcClientError> {
        self.send_ok(json!({ "type": "set_thinking_level", "level": level }))
            .await
            .map(|_| ())
    }

    /// `cycle_thinking_level`.
    pub async fn cycle_thinking_level(&self) -> RpcResult {
        self.send_ok(json!({ "type": "cycle_thinking_level" }))
            .await
    }

    /// `get_available_thinking_levels` (upstream
    /// `getAvailableThinkingLevels`): `{ levels: [...] }`.
    pub async fn get_available_thinking_levels(&self) -> RpcResult {
        self.send_ok(json!({ "type": "get_available_thinking_levels" }))
            .await
    }

    /// `set_steering_mode` (`"all"` / `"one-at-a-time"`).
    pub async fn set_steering_mode(&self, mode: &str) -> Result<(), RpcClientError> {
        self.send_ok(json!({ "type": "set_steering_mode", "mode": mode }))
            .await
            .map(|_| ())
    }

    /// `set_follow_up_mode` (`"all"` / `"one-at-a-time"`).
    pub async fn set_follow_up_mode(&self, mode: &str) -> Result<(), RpcClientError> {
        self.send_ok(json!({ "type": "set_follow_up_mode", "mode": mode }))
            .await
            .map(|_| ())
    }

    /// `set_auto_compaction`.
    pub async fn set_auto_compaction(&self, enabled: bool) -> Result<(), RpcClientError> {
        self.send_ok(json!({ "type": "set_auto_compaction", "enabled": enabled }))
            .await
            .map(|_| ())
    }

    /// `set_auto_retry`.
    pub async fn set_auto_retry(&self, enabled: bool) -> Result<(), RpcClientError> {
        self.send_ok(json!({ "type": "set_auto_retry", "enabled": enabled }))
            .await
            .map(|_| ())
    }

    /// `abort_retry`.
    pub async fn abort_retry(&self) -> Result<(), RpcClientError> {
        self.send_ok(json!({ "type": "abort_retry" }))
            .await
            .map(|_| ())
    }

    /// `new_session` (optionally recording a parent session).
    pub async fn new_session(&self, parent_session: Option<&str>) -> RpcResult {
        let mut command = json!({ "type": "new_session" });
        if let Some(parent) = parent_session {
            command["parentSession"] = json!(parent);
        }
        self.send_ok(command).await
    }

    /// `switch_session`.
    pub async fn switch_session(&self, session_path: &str) -> RpcResult {
        self.send_ok(json!({ "type": "switch_session", "sessionPath": session_path }))
            .await
    }

    /// `fork` from one entry.
    pub async fn fork(&self, entry_id: &str) -> RpcResult {
        self.send_ok(json!({ "type": "fork", "entryId": entry_id }))
            .await
    }

    /// `clone`.
    pub async fn clone_session(&self) -> RpcResult {
        self.send_ok(json!({ "type": "clone" })).await
    }

    /// `export_html`.
    pub async fn export_html(&self, output_path: Option<&str>) -> RpcResult {
        let mut command = json!({ "type": "export_html" });
        if let Some(path) = output_path {
            command["outputPath"] = json!(path);
        }
        self.send_ok(command).await
    }

    // ------------------------------------------------------------------
    // Teardown
    // ------------------------------------------------------------------

    /// Close the transport (upstream `stop`): end stdin, give the child
    /// [`RpcClientOptions::close_grace`] to exit, then kill it. Pending
    /// requests fail with [`RpcClientError::Closed`]. Idempotent.
    pub async fn close(&self) -> Result<(), RpcClientError> {
        let was_closed = self.inner.closed.swap(true, Ordering::SeqCst);
        let writer = {
            let mut writer = self.inner.writer.lock().await;
            writer.take()
        };
        drop(writer);
        // Reject anything still awaiting a response (upstream clears its map).
        let pending = {
            let mut pending = self
                .inner
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            std::mem::take(&mut *pending)
        };
        for (_, sender) in pending {
            let _ = sender.send(Err(RpcClientError::Closed));
        }
        // Give the writer task a chance to end stdin before reaping.
        if let Some(task) = self.inner.writer_task.lock().await.take() {
            let _ = tokio::time::timeout(Duration::from_millis(100), task).await;
        }
        let child = self.inner.child.lock().await.take();
        if let Some(mut child) = child {
            match tokio::time::timeout(self.inner.close_grace, child.wait()).await {
                Ok(Ok(_)) => {}
                _ => {
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                }
            }
        }
        if let Some(task) = self.inner.reader_task.lock().await.take() {
            task.abort();
        }
        if let Some(task) = self.inner.stderr_task.lock().await.take() {
            task.abort();
        }
        if !was_closed {
            self.inner.close_with(RpcClientError::Closed, false);
        }
        Ok(())
    }

    /// [`Self::close`] under the upstream method name (`stop`).
    pub async fn stop(&self) -> Result<(), RpcClientError> {
        self.close().await
    }
}

impl RpcInner {
    /// Best-effort synchronous teardown for the last external `RpcClient`
    /// handle: the background tasks hold strong `RpcInner` references, so the
    /// normal `Drop` would otherwise never run.
    fn close_now(&self) {
        self.closed.store(true, Ordering::SeqCst);
        // End the async event stream too: receivers drain what is retained,
        // then observe `RpcEventRecvError::Closed`.
        self.events.close();
        if let Ok(mut writer) = self.writer.try_lock() {
            writer.take();
        }
        if let Ok(mut child) = self.child.try_lock() {
            if let Some(child) = child.as_mut() {
                let _ = child.start_kill();
            }
        }
        if let Ok(mut task) = self.reader_task.try_lock() {
            if let Some(task) = task.take() {
                task.abort();
            }
        }
        if let Ok(mut task) = self.writer_task.try_lock() {
            if let Some(task) = task.take() {
                task.abort();
            }
        }
        if let Ok(mut task) = self.stderr_task.try_lock() {
            if let Some(task) = task.take() {
                task.abort();
            }
        }
    }
}

impl Drop for RpcInner {
    fn drop(&mut self) {
        // Fallback for clients constructed through paths that never held an
        // external RpcClient handle (the external-count Drop normally wins).
        self.close_now();
    }
}

fn disposition(data: Value) -> Result<String, RpcClientError> {
    data.get("disposition")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| RpcClientError::Protocol("response carries no disposition".to_owned()))
}

/// Collect from an already-subscribed receiver until `agent_settled`.
async fn collect_from(
    receiver: &mut RpcEventReceiver,
    timeout: Duration,
) -> Result<Vec<Value>, RpcClientError> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut events = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(RpcClientError::Timeout {
                operation: "collect_events",
            });
        }
        match tokio::time::timeout(remaining, receiver.recv()).await {
            Ok(Ok(value)) => {
                let settled = value.get("type").and_then(Value::as_str) == Some("agent_settled");
                events.push(value);
                if settled {
                    return Ok(events);
                }
            }
            Ok(Err(RpcEventRecvError::Closed)) => return Err(RpcClientError::Closed),
            Err(_) => {
                return Err(RpcClientError::Timeout {
                    operation: "collect_events",
                })
            }
        }
    }
}
