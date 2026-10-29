//! Terminals on a pi-env daemon (upstream `packages/env/src/terminal.ts`).
//!
//! [`open_terminal`] starts a program in a new pseudo-terminal; the returned
//! [`RemoteTerminal`] streams its raw output through the `on_data` callback,
//! accepts input ([`RemoteTerminal::write`]), reports/changes its size
//! ([`RemoteTerminal::resize`]), names the foreground job
//! ([`RemoteTerminal::foreground`]), and settles with the program's exit code
//! ([`RemoteTerminal::exited`]). `close()` cancels the `pty` request, which
//! makes the daemon kill the program and its whole session, and stops the
//! serial writer: the queue is closed, later writes report `aborted`, and every
//! queued operation still waiting for the daemon is failed. A daemon that
//! stops answering cannot wedge the writer forever: each `ptyWrite`/
//! `ptyResize` carries an abort signal and a [`REQUEST_TIMEOUT`], after which
//! the request is cancelled and reported as a `timeout` failure while the
//! writer moves on to the next queued operation.
//!
//! Wire shape (daemon `src/env_daemon/pty.rs`, upstream `terminal.ts`):
//!
//! - `pty {cwd, argv?, env?, inheritEnv?, cols, rows}` — the long-running
//!   request; its `started` event carries the pid, its `output` events carry
//!   the raw bytes, and its result carries `{exitCode}`.
//! - `ptyWrite {target}` + payload — input, in ≤1 MiB chunks (frames are
//!   bounded at 16 MiB).
//! - `ptyResize {target, cols, rows}` — the program receives `SIGWINCH`.
//! - `ptyForeground {target}` — `{foreground: null}` or `{foreground:
//!   {pid, name?}}` for the job a shell started.
//!
//! The writes/resize/foreground requests are pinned to the session that
//! opened the terminal ([`RequestOptions::session`]), so they can only reach
//! the daemon that owns it.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Map, Value};
use tokio::sync::{mpsc, oneshot, watch};

use crate::ai::types::AbortSignal;

use super::connection::{Connection, RequestOptions};
use super::errors::RemoteError;

/// Input larger than this goes to the daemon in several writes; frames are at
/// most 16 MiB.
pub const WRITE_CHUNK: usize = 1024 * 1024;

/// Default limit on how long one queued `ptyWrite`/`ptyResize` may stay
/// unanswered before the serial writer cancels it (`timeout` error).
///
/// Consistent with the connection's own liveness policy: the client already
/// treats 30 s without a daemon frame as a lost connection, so a request the
/// daemon has not consumed within that window is just as unresponsive. The
/// limit is per operation and deliberately bounded: it caps how long one
/// wedged request can hold the writer's queue head, without declaring the
/// terminal dead (a later write may still be answered).
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Serial-writer queue depth (operations) past which the client reports a
/// stalled writer.
///
/// Parking the enqueue is not an option: [`RemoteTerminal::write`] is
/// synchronous (its call order IS the wire order) and is called from the
/// caller's async task, so blocking would stall that task's runtime. The queue
/// is therefore bounded (`WRITER_QUEUE_MAX_DEPTH` / `WRITER_QUEUE_MAX_BYTES`)
/// with an explicit drop policy — the NEWEST operation is refused, the queued
/// ones keep their wire order — reported at this watermark and at
/// [`WRITER_QUEUE_MAX_DEPTH`]. A healthy daemon answers one operation per
/// drain, so the depth tracks the caller's fire-and-forget backlog; each
/// operation is also bounded by [`REQUEST_TIMEOUT`], which bounds how long a
/// wedged daemon keeps the queue from draining.
const WRITER_QUEUE_WARN_DEPTH: usize = 256;

/// Hard cap on queued operations per terminal (see
/// [`WRITER_QUEUE_WARN_DEPTH`]): the newest operation is refused past it.
const WRITER_QUEUE_MAX_DEPTH: usize = 1024;

/// Hard cap on queued payload BYTES per terminal. One operation may carry up
/// to [`WRITE_CHUNK`], so an operation-count cap alone would still allow
/// gigabytes of queued input; this keeps the queue's memory bounded on a
/// small device (an 8 MiB backlog is already far past interactive input).
const WRITER_QUEUE_MAX_BYTES: usize = 8 * 1024 * 1024;

/// Diagnostic cadence: the first water-mark crossing is reported, then one
/// line per this many further crossings (same shape as the connection outbox).
const WRITER_QUEUE_REPORT_EVERY: u64 = 64;

/// One terminal's raw output callback (`onData: (bytes) => void`).
pub type TerminalDataFn = Arc<dyn Fn(&[u8]) + Send + Sync>;

/// Options for [`open_terminal`] (upstream `TerminalOptions`).
#[derive(Clone)]
pub struct TerminalOptions {
    /// Working directory of the program; it must exist.
    pub cwd: String,
    /// Program and arguments. `None` runs the user's shell as a login shell
    /// (`SHELL` in `env` or the daemon's environment).
    pub argv: Option<Vec<String>>,
    /// Added to the daemon's environment, or the whole environment with
    /// `inherit_env: Some(false)`. Set `TERM` here.
    pub env: BTreeMap<String, String>,
    pub inherit_env: Option<bool>,
    pub cols: u16,
    pub rows: u16,
    /// The terminal's raw output, in order.
    pub on_data: TerminalDataFn,
    /// How long one `ptyWrite`/`ptyResize` may stay unanswered before the
    /// serial writer cancels it and reports a `timeout` error
    /// ([`REQUEST_TIMEOUT`] by default).
    pub request_timeout: Duration,
}

impl TerminalOptions {
    pub fn new(
        cwd: impl Into<String>,
        cols: u16,
        rows: u16,
        on_data: impl Fn(&[u8]) + Send + Sync + 'static,
    ) -> Self {
        Self {
            cwd: cwd.into(),
            argv: None,
            env: BTreeMap::new(),
            inherit_env: None,
            cols,
            rows,
            on_data: Arc::new(on_data),
            request_timeout: REQUEST_TIMEOUT,
        }
    }

    /// Override the per-operation timeout (see [`TerminalOptions::request_timeout`]).
    pub fn request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = timeout;
        self
    }

    pub fn argv(mut self, argv: Vec<String>) -> Self {
        self.argv = Some(argv);
        self
    }

    pub fn env(mut self, env: BTreeMap<String, String>) -> Self {
        self.env = env;
        self
    }

    pub fn inherit_env(mut self, inherit: bool) -> Self {
        self.inherit_env = Some(inherit);
        self
    }

    fn request_json(&self) -> Value {
        let mut json = Map::new();
        json.insert("cwd".to_owned(), Value::String(self.cwd.clone()));
        if let Some(argv) = &self.argv {
            json.insert("argv".to_owned(), json!(argv));
        }
        if !self.env.is_empty() {
            json.insert("env".to_owned(), json!(self.env));
        }
        if let Some(inherit_env) = self.inherit_env {
            json.insert("inheritEnv".to_owned(), Value::Bool(inherit_env));
        }
        json.insert("cols".to_owned(), json!(self.cols));
        json.insert("rows".to_owned(), json!(self.rows));
        Value::Object(json)
    }
}

/// The foreground job of a terminal: its process group id, and its leader's
/// name when the system tells (upstream `TerminalForeground`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalForeground {
    pub pid: u32,
    pub name: Option<String>,
}

/// A terminal's settlement: the program's exit code, or the daemon error
/// (`aborted` after [`RemoteTerminal::close`], or a connection loss).
///
/// Cheap to clone and await from several places; the value is retained after
/// the daemon's reply arrives.
#[derive(Clone)]
pub struct Exited {
    receiver: watch::Receiver<Option<Result<i32, RemoteError>>>,
}

impl Exited {
    /// Wait for the program to end.
    pub async fn wait(&self) -> Result<i32, RemoteError> {
        let mut receiver = self.receiver.clone();
        loop {
            if let Some(result) = receiver.borrow().clone() {
                return result;
            }
            if receiver.changed().await.is_err() {
                return Err(RemoteError::lost("pi-env connection lost"));
            }
        }
    }

    /// The settled value, when already known.
    pub fn try_get(&self) -> Option<Result<i32, RemoteError>> {
        self.receiver.borrow().clone()
    }
}

/// One terminal operation waiting in the serial writer's queue. `reply`
/// carries the request result back to the awaiting API (`None` for the
/// fire-and-forget [`RemoteTerminal::write`] / [`RemoteTerminal::resize`]).
enum TerminalOp {
    Write {
        payload: Vec<u8>,
        reply: Option<oneshot::Sender<Result<(), RemoteError>>>,
    },
    Resize {
        cols: u16,
        rows: u16,
        reply: Option<oneshot::Sender<Result<(), RemoteError>>>,
    },
}

impl TerminalOp {
    /// The wire operation this queued entry sends (for diagnostics).
    fn name(&self) -> &'static str {
        match self {
            TerminalOp::Write { .. } => "ptyWrite",
            TerminalOp::Resize { .. } => "ptyResize",
        }
    }

    /// Payload bytes this queued entry holds (the queue's byte budget).
    fn payload_bytes(&self) -> usize {
        match self {
            TerminalOp::Write { payload, .. } => payload.len(),
            TerminalOp::Resize { .. } => 0,
        }
    }

    /// Take the awaiting caller's reply slot, if any.
    fn take_reply(self) -> Option<oneshot::Sender<Result<(), RemoteError>>> {
        match self {
            TerminalOp::Write { reply, .. } | TerminalOp::Resize { reply, .. } => reply,
        }
    }
}

/// The error for an operation refused after [`RemoteTerminal::close`].
fn closed_error() -> RemoteError {
    RemoteError::new("aborted", "terminal closed")
}

/// The error for an operation refused because the serial writer's bounded
/// queue is full (see [`WRITER_QUEUE_MAX_DEPTH`]/
/// [`WRITER_QUEUE_MAX_BYTES`]): the caller may retry once the daemon drains,
/// and no queued input was reordered or dropped.
fn overloaded_error() -> RemoteError {
    RemoteError::new(
        "EAGAIN",
        "terminal input queue is full (daemon not draining)",
    )
}

/// The serial writer's queue bounds `(max operations, max payload bytes)`, as
/// enforced by [`RemoteTerminal::enqueue`]. Exposed for the low-resource
/// tests that pin the bound's shape (the queue must stay bounded on a
/// 512 MB-class device).
#[doc(hidden)]
pub fn writer_queue_limits() -> (usize, usize) {
    (WRITER_QUEUE_MAX_DEPTH, WRITER_QUEUE_MAX_BYTES)
}

/// A program running in a pseudo-terminal of a pi-env daemon. It ends when
/// the program exits, [`RemoteTerminal::close`] kills it, or the connection
/// is lost (the daemon then kills it).
#[derive(Clone)]
pub struct RemoteTerminal {
    connection: Connection,
    id: u32,
    session: u32,
    pid: u32,
    abort: AbortSignal,
    exited: Exited,
    /// Serial writer queue shared by every clone of this terminal (see
    /// [`RemoteTerminal::write`]).
    writer: mpsc::UnboundedSender<TerminalOp>,
    /// Queued operations not yet taken by the writer task (the water-mark
    /// input; see [`WRITER_QUEUE_WARN_DEPTH`]).
    queued: Arc<AtomicUsize>,
    /// Queued payload bytes not yet taken by the writer task (the byte budget;
    /// see [`WRITER_QUEUE_MAX_BYTES`]).
    queued_bytes: Arc<AtomicUsize>,
    /// Refused operations (the bound was hit); diagnostics only.
    refused: Arc<AtomicU64>,
    /// Water-mark crossings already reported (diagnostic cadence).
    warn_crossings: Arc<AtomicU64>,
    /// Set by [`RemoteTerminal::close`]: input is refused from then on, even
    /// before the writer task has observed the abort.
    closed: Arc<AtomicBool>,
}

impl RemoteTerminal {
    /// The program's process id (also its process group's: it leads the
    /// terminal's session).
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// The exit code; errors with the daemon's error (`aborted` after
    /// [`RemoteTerminal::close`]).
    pub fn exited(&self) -> Exited {
        self.exited.clone()
    }

    /// Send input; once the terminal has ended, input is dropped (upstream
    /// `write` ignores request errors the same way).
    ///
    /// Synchronous for API parity: every chunk frame is pushed into the
    /// terminal's serial outbox ON THE CALLING THREAD, so the wire order is
    /// the call order — both the chunks of one large write and writes mixed
    /// with [`RemoteTerminal::resize`] calls. Use
    /// [`RemoteTerminal::write_async`] to await delivery.
    ///
    /// After [`RemoteTerminal::close`] no chunk is accepted (the queue is
    /// closed); this API has nothing to report that on, so the input is
    /// dropped like any other post-close write. The same applies when the
    /// bounded outbox is full (a wedged daemon: see
    /// [`WRITER_QUEUE_MAX_DEPTH`]): the newest chunk is dropped and the
    /// diagnostic records it, never reordered ahead of what is already queued.
    /// The fallible variants ([`RemoteTerminal::write_async`] /
    /// [`RemoteTerminal::resize_async`]) return the explicit `aborted` /
    /// `EAGAIN` error instead.
    pub fn write(&self, data: impl AsRef<[u8]>) {
        // The serial writer lives on the runtime that opened the terminal;
        // outside one this stays the pre-outbox no-op.
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        for chunk in data.as_ref().chunks(WRITE_CHUNK) {
            if self
                .enqueue(TerminalOp::Write {
                    payload: chunk.to_vec(),
                    reply: None,
                })
                .is_err()
            {
                // Closed (or the writer is gone): no later chunk is accepted.
                break;
            }
        }
    }

    /// [`RemoteTerminal::write`] with the requests awaited. The chunks enter
    /// the same serial outbox in order; each one is awaited before the next
    /// is queued, so one large write is contiguous on the wire.
    pub async fn write_async(&self, data: impl AsRef<[u8]>) -> Result<(), RemoteError> {
        for chunk in data.as_ref().chunks(WRITE_CHUNK) {
            let (reply, receiver) = oneshot::channel();
            self.enqueue(TerminalOp::Write {
                payload: chunk.to_vec(),
                reply: Some(reply),
            })?;
            match receiver.await {
                Ok(result) => result?,
                // The writer dropped the reply without answering (it exited
                // on close, or the whole queue was discarded): report which.
                Err(_) => return Err(self.enqueue_failure()),
            }
        }
        Ok(())
    }

    /// Change the size; the program receives `SIGWINCH` (fire-and-forget for
    /// API parity; see [`RemoteTerminal::resize_async`]). The frame enters the
    /// same serial outbox as [`RemoteTerminal::write`], so a resize never
    /// overtakes a write made before it (or vice versa).
    pub fn resize(&self, cols: u16, rows: u16) {
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let _ = self.enqueue(TerminalOp::Resize {
            cols,
            rows,
            reply: None,
        });
    }

    /// [`RemoteTerminal::resize`] with the request awaited.
    pub async fn resize_async(&self, cols: u16, rows: u16) -> Result<(), RemoteError> {
        let (reply, receiver) = oneshot::channel();
        self.enqueue(TerminalOp::Resize {
            cols,
            rows,
            reply: Some(reply),
        })?;
        match receiver.await {
            Ok(result) => result,
            Err(_) => Err(self.enqueue_failure()),
        }
    }

    /// Push one operation onto the serial outbox.
    ///
    /// The queue is bounded in both operations and payload bytes (see
    /// [`WRITER_QUEUE_MAX_DEPTH`] / [`WRITER_QUEUE_MAX_BYTES`]). When the bound
    /// is hit the NEWEST operation is refused with [`overloaded_error`]: the
    /// already-queued operations keep their arrival order on the wire, so a
    /// bounded backlog never reorders input. [`RemoteTerminal::write`] drops
    /// the refused chunk (it has no error channel) and the diagnostic records
    /// it; the fallible variants propagate the error.
    fn enqueue(&self, op: TerminalOp) -> Result<(), RemoteError> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(closed_error());
        }
        let bytes = op.payload_bytes();
        let depth = self.queued.fetch_add(1, Ordering::Relaxed) + 1;
        let queued_bytes = self.queued_bytes.fetch_add(bytes, Ordering::Relaxed) + bytes;
        if depth > WRITER_QUEUE_MAX_DEPTH || queued_bytes > WRITER_QUEUE_MAX_BYTES {
            self.queued.fetch_sub(1, Ordering::Relaxed);
            self.queued_bytes.fetch_sub(bytes, Ordering::Relaxed);
            let refused = self.refused.fetch_add(1, Ordering::Relaxed) + 1;
            if refused == 1 || refused.is_multiple_of(WRITER_QUEUE_REPORT_EVERY) {
                crate::ext_diag!(
                    "[env-client] terminal writer queue full ({depth} ops / \
                     {queued_bytes} bytes); refused the newest operation \
                     ({refused} total)"
                );
            }
            return Err(overloaded_error());
        }
        if self.writer.send(op).is_err() {
            self.queued.fetch_sub(1, Ordering::Relaxed);
            self.queued_bytes.fetch_sub(bytes, Ordering::Relaxed);
            // The only way to lose the receiver while a sender is alive is
            // close() (or a writer panic); either way input no longer flows.
            return Err(closed_error());
        }
        if depth >= WRITER_QUEUE_WARN_DEPTH {
            let crossings = self.warn_crossings.fetch_add(1, Ordering::Relaxed) + 1;
            if crossings == 1 || crossings.is_multiple_of(WRITER_QUEUE_REPORT_EVERY) {
                crate::ext_diag!(
                    "[env-client] terminal writer queue at {depth} operations \
                     (daemon not draining; {} crossings)",
                    crossings
                );
            }
        }
        Ok(())
    }

    /// The error for a queued operation whose reply was dropped, before the
    /// writer could answer: `aborted` after a close, otherwise the connection
    /// went away with the writer task.
    fn enqueue_failure(&self) -> RemoteError {
        if self.closed.load(Ordering::SeqCst) {
            closed_error()
        } else {
            RemoteError::lost("pi-env connection lost")
        }
    }

    /// Wait until the serial writer task has exited (after
    /// [`RemoteTerminal::close`], or when every clone of the terminal was
    /// dropped). Awaiting it joins the terminal's outbox teardown; pending
    /// [`RemoteTerminal::write_async`] / [`RemoteTerminal::resize_async`]
    /// calls have been answered by then.
    pub async fn writer_closed(&self) {
        self.writer.closed().await;
    }

    /// Kill the program and everything in its session.
    ///
    /// Also closes the serial writer: the queue stops accepting input, the
    /// in-flight request is cancelled (it never holds the teardown), queued
    /// operations are answered with `aborted`, and the writer task exits. The
    /// `pty` request is cancelled as before, so the daemon kills the program.
    pub fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.abort.abort();
    }

    /// The job in the terminal's foreground when it is not the program
    /// itself: what a shell runs, such as an editor. `None` while the program
    /// is in front (a shell at its prompt). Fails once the terminal ended.
    pub async fn foreground(&self) -> Result<Option<TerminalForeground>, RemoteError> {
        let options = RequestOptions {
            session: Some(self.session),
            ..Default::default()
        };
        let reply = self
            .connection
            .request("ptyForeground", json!({ "target": self.id }), options)
            .await?;
        let foreground = reply.json.get("foreground");
        let Some(Value::Object(foreground)) = foreground else {
            return Ok(None);
        };
        let pid = foreground.get("pid").and_then(Value::as_u64).unwrap_or(0) as u32;
        let name = foreground
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            .map(str::to_owned);
        Ok(Some(TerminalForeground { pid, name }))
    }
}

/// Start a program in a new pseudo-terminal of the daemon behind `connection`
/// (upstream `openTerminal`).
///
/// The future resolves when the daemon reports the program has started; a
/// failure before that rejects the open. Afterwards, failures settle
/// [`RemoteTerminal::exited`].
/// Reply channel for the daemon's "started" event: `(id, session, pid)`.
type StartedSender = Arc<Mutex<Option<oneshot::Sender<Result<(u32, u32, u32), RemoteError>>>>>;

pub async fn open_terminal(
    connection: Connection,
    options: TerminalOptions,
) -> Result<RemoteTerminal, RemoteError> {
    let abort = AbortSignal::new();
    let started_state: Arc<Mutex<Option<(u32, u32)>>> = Arc::new(Mutex::new(None));
    let started_sender: StartedSender = Arc::new(Mutex::new(None));
    let (started_tx, started_rx) = oneshot::channel();
    *started_sender
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(started_tx);
    let (exit_tx, exit_rx) = watch::channel(None);

    let on_start_state = Arc::clone(&started_state);
    let on_start = Arc::new(move |id: u32, session: u32| {
        *on_start_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((id, session));
    });
    let on_event_state = Arc::clone(&started_state);
    let on_event_sender = Arc::clone(&started_sender);
    let on_event_data = Arc::clone(&options.on_data);
    let on_event = Arc::new(move |json: &Value, payload: &[u8]| {
        match json.get("kind").and_then(Value::as_str) {
            Some("output") => on_event_data(payload),
            Some("started") => {
                let target = *on_event_state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let Some((id, session)) = target else {
                    return;
                };
                let pid = json.get("pid").and_then(Value::as_u64).unwrap_or(0) as u32;
                let sender = on_event_sender
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take();
                if let Some(sender) = sender {
                    let _ = sender.send(Ok((id, session, pid)));
                }
            }
            // Unknown event kinds are ignored, as upstream's `onEvent` does.
            _ => {}
        }
    });

    let request_options = RequestOptions {
        signal: Some(abort.clone()),
        on_start: Some(on_start),
        on_event: Some(on_event),
        ..Default::default()
    };
    let request_json = options.request_json();
    let request_connection = connection.clone();
    let exit_sender = Arc::clone(&started_sender);
    tokio::spawn(async move {
        let result = request_connection
            .request("pty", request_json, request_options)
            .await;
        let outcome = match result {
            Ok(reply) => reply
                .json
                .get("exitCode")
                .and_then(Value::as_i64)
                .map(|code| code as i32)
                .ok_or_else(|| RemoteError::new("unknown", "pty result carries no exitCode")),
            Err(error) => Err(error),
        };
        if let Err(error) = &outcome {
            // Failing before `started` rejects the open; afterwards the
            // settlement carries it.
            let sender = exit_sender
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            if let Some(sender) = sender {
                let _ = sender.send(Err(error.clone()));
            }
        }
        let _ = exit_tx.send(Some(outcome));
    });

    let (id, session, pid) = match started_rx.await {
        Ok(Ok(started)) => started,
        Ok(Err(error)) => return Err(error),
        Err(_) => return Err(RemoteError::lost("pi-env connection lost")),
    };
    // One serial writer per terminal: it drains the `(write chunk, resize)`
    // queue FIFO on a single task, so the caller's push order IS the wire
    // order. The previous shape spawned one task per chunk (and one per
    // resize), whose scheduling could reorder both the chunks of a large
    // write and a resize relative to the writes around it.
    //
    // The writer is bounded in TIME and in DEPTH: every queued operation
    // carries an abort signal and runs under `request_timeout`, so a daemon
    // that stops answering fails that one request (`timeout`) instead of
    // parking the writer (and every later write) forever, and the outbox
    // refuses the NEWEST operation past `WRITER_QUEUE_MAX_DEPTH` /
    // `WRITER_QUEUE_MAX_BYTES` (never reordering the accepted ones).
    // `close()` aborts the same signal the `pty` request uses, which also
    // stops this task and closes the queue.
    let (writer, mut writer_ops) = mpsc::unbounded_channel::<TerminalOp>();
    let queued = Arc::new(AtomicUsize::new(0));
    let queued_bytes = Arc::new(AtomicUsize::new(0));
    let refused = Arc::new(AtomicU64::new(0));
    let warn_crossings = Arc::new(AtomicU64::new(0));
    let closed = Arc::new(AtomicBool::new(false));
    {
        let connection = connection.clone();
        let stop = abort.clone();
        let writer_closed = Arc::clone(&closed);
        let writer_queued = Arc::clone(&queued);
        let writer_bytes = Arc::clone(&queued_bytes);
        let request_timeout = options.request_timeout;
        tokio::spawn(async move {
            loop {
                let op = tokio::select! {
                    biased;
                    // `close()` wins over a still-queued operation: nothing is
                    // sent after the terminal ended, and the queue below is
                    // answered with the closed error.
                    _ = stop.cancelled() => break,
                    op = writer_ops.recv() => match op {
                        Some(op) => op,
                        // Every `RemoteTerminal` (and so every sender) is gone.
                        None => break,
                    },
                };
                writer_queued.fetch_sub(1, Ordering::Relaxed);
                writer_bytes.fetch_sub(op.payload_bytes(), Ordering::Relaxed);
                let op_name = op.name();
                // Per-request cancellation: aborting the signal sends the
                // protocol's `cancel` frame, so a daemon that eventually wakes
                // drops the abandoned request instead of applying it late.
                let op_abort = AbortSignal::new();
                let (request, reply) = match op {
                    TerminalOp::Write { payload, reply } => {
                        let options = RequestOptions {
                            session: Some(session),
                            signal: Some(op_abort.clone()),
                            payload,
                            ..Default::default()
                        };
                        (
                            connection.request("ptyWrite", json!({ "target": id }), options),
                            reply,
                        )
                    }
                    TerminalOp::Resize { cols, rows, reply } => {
                        let options = RequestOptions {
                            session: Some(session),
                            signal: Some(op_abort.clone()),
                            ..Default::default()
                        };
                        (
                            connection.request(
                                "ptyResize",
                                json!({ "target": id, "cols": cols, "rows": rows }),
                                options,
                            ),
                            reply,
                        )
                    }
                };
                let result = tokio::select! {
                    biased;
                    _ = stop.cancelled() => {
                        op_abort.abort();
                        Err(closed_error())
                    }
                    result = request => result.map(|_| ()),
                    _ = tokio::time::sleep(request_timeout) => {
                        op_abort.abort();
                        Err(RemoteError::new(
                            "timeout",
                            format!(
                                "{op_name} did not answer within {} ms",
                                request_timeout.as_millis()
                            ),
                        ))
                    }
                };
                if let Some(reply) = reply {
                    let _ = reply.send(result);
                }
                if writer_closed.load(Ordering::SeqCst) || stop.is_aborted() {
                    break;
                }
            }
            // Close the queue: no later `send` can enqueue into a queue whose
            // owner is gone. Then answer every operation still parked behind
            // the one that stopped the writer with the explicit closed error,
            // so a caller awaiting `write_async` never sees a bare "connection
            // lost" (or hangs) after `close()`.
            writer_ops.close();
            while let Ok(op) = writer_ops.try_recv() {
                writer_queued.fetch_sub(1, Ordering::Relaxed);
                writer_bytes.fetch_sub(op.payload_bytes(), Ordering::Relaxed);
                if let Some(reply) = op.take_reply() {
                    let _ = reply.send(Err(closed_error()));
                }
            }
        });
    }
    Ok(RemoteTerminal {
        connection,
        id,
        session,
        pid,
        abort,
        exited: Exited { receiver: exit_rx },
        writer,
        queued,
        queued_bytes,
        refused,
        warn_crossings,
        closed,
    })
}

/// Whether an error means the terminal was closed by the caller (upstream
/// reports an `aborted` code; the client maps it through `RemoteError`).
pub fn was_closed(error: &RemoteError) -> bool {
    error.code == "aborted"
}

impl std::fmt::Debug for RemoteTerminal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RemoteTerminal")
            .field("pid", &self.pid)
            .field("id", &self.id)
            .field("session", &self.session)
            .finish_non_exhaustive()
    }
}
