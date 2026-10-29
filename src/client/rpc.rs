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
//!   [`RpcClient::events`] broadcast receivers;
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

use std::collections::HashMap;
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

/// How many events a [`RpcClient::events`] receiver may fall behind before
/// `recv` reports `Lagged` (the listener callbacks never lag: they run on the
/// reader task).
pub const EVENT_CHANNEL_CAPACITY: usize = 1024;

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

struct RpcInner {
    /// Writer task channel; `None` once the client is closed.
    writer: tokio::sync::Mutex<Option<mpsc::Sender<String>>>,
    /// Requests awaiting a response, keyed by the client-issued id.
    pending: Mutex<HashMap<String, oneshot::Sender<RpcResult>>>,
    /// Event fan-out for async consumers.
    events: broadcast::Sender<Value>,
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
        let _ = self.events.send(value.clone());
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
        let (event_tx, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let (line_tx, mut line_rx) = mpsc::channel::<String>(256);
        let inner = Arc::new(RpcInner {
            writer: tokio::sync::Mutex::new(Some(line_tx)),
            pending: Mutex::new(HashMap::new()),
            events: event_tx,
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

    /// Subscribe to agent events as an async stream (broadcast). Events
    /// emitted before this call are not replayed — register first, then send
    /// the command (upstream `onEvent` has the same ordering requirement).
    pub fn events(&self) -> broadcast::Receiver<Value> {
        self.inner.events.subscribe()
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
        if writer.send(line).await.is_err() {
            self.drop_pending(&id);
            return Err(RpcClientError::Closed);
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
                Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
                Ok(Err(broadcast::error::RecvError::Closed)) => return Err(RpcClientError::Closed),
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
                Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
                Ok(Err(broadcast::error::RecvError::Closed)) => return Err(RpcClientError::Closed),
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
    receiver: &mut broadcast::Receiver<Value>,
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
            Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
            Ok(Err(broadcast::error::RecvError::Closed)) => return Err(RpcClientError::Closed),
            Err(_) => {
                return Err(RpcClientError::Timeout {
                    operation: "collect_events",
                })
            }
        }
    }
}
