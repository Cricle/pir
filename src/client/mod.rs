//! Remote TCP client for the protocol v8 generic RPC envelope.
//!
//! The client performs the v8 hello handshake (`hello {version: 8}` and the
//! server's `{hello, version: 8, serverId}` reply), then issues generic RPC
//! requests: [`Client::call`] sends `request {id, target, call}` frames with
//! chord service calls and resolves the matching `response`, while
//! [`Client::subscribe`] registers a `$chord.service` subscription and
//! [`Client::next_update`] yields the resulting `service_update` frames.
//!
//! Requests are serialized on one connection; `service_update` and
//! `attachment` frames that arrive while a response is pending are queued and
//! surfaced by [`Client::next_update`]. Responses that arrive while the caller
//! is not waiting for them (a `start_call` prompt terminal consumed during
//! `next_update`) are buffered by request id and resolved by the later
//! [`Client::wait_call`]. Disconnecting drops the stream and therefore releases
//! the server-side connection entry and attachment leases.
//!
//! Reconnection: [`Client::reconnect`] drops the broken transport, re-runs
//! the versioned hello handshake (re-sending the stored authentication
//! preface) and restores session state — it re-attaches to the session that
//! was attached before the disconnect and re-issues the active subscriptions.
//! Attempts are bounded by a [`ReconnectPolicy`] (conservative defaults) and
//! every attempt reuses the same [`ClientTimeouts`] as the initial connect,
//! so a reconnect never bypasses the connect/hello bounds. Transport
//! transitions are reported as typed [`ConnectionStateChange`] events through
//! [`Client::on_connection_state_change`]. Restoration is best-effort and
//! reported through [`ReconnectReport`]: a session that the server removed on
//! disconnect cannot be re-attached, in which case the stale attach state is
//! dropped and `restored_session` is `None`.
//!
//! The concrete v1 command surface (`list`, `request(Command)`) is retained
//! as a compatibility layer mapped onto the v8 `pir.session` service.
//!
//! ## JSON-line RPC client
//!
//! [`rpc::RpcClient`] is a different transport: the local agent's RPC mode
//! (`pir rpc`, upstream `modes/rpc/rpc-client.ts`), one JSON object per line
//! over a child process's stdin/stdout. Use it for embedding the local agent
//! (prompt/steer/abort, `get_state`/`get_messages`/`get_commands`, event
//! streaming, `agent_settled` waits); use [`Client`] for the remote TCP
//! protocol v8 server.

pub mod rpc;

pub use rpc::{RpcClient, RpcClientError, RpcClientOptions, RpcEventSubscription};

use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{lookup_host, TcpStream, ToSocketAddrs};
use tokio::time::timeout;
use uuid::Uuid;

use crate::protocol::schemas::field_opt;
use crate::protocol::{
    encode_client_envelope, parse_service_state_update, ClientEnvelope, ClientHello, Command,
    FrameOptions, ProtocolError, RpcCancel, RpcRequest, RpcTarget, ServerMessageDecoder,
    ServiceCall, ServiceUpdate, StateUpdateDecoder, PROTOCOL_VERSION, SERVICE_CONTROL_ID,
    SESSION_SERVICE_ID,
};

/// Default bound on the initial TCP connect in [`Client::connect`] and
/// [`Client::connect_with_token`].
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Default bound on the hello handshake and each request/response frame read.
///
/// The bound is per read: a peer that stalls mid-request fails after this
/// duration, while a peer that keeps streaming frames is not interrupted.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Default bound on each subscription frame read in [`Client::next_update`].
///
/// Each call bounds one update wait; a caller that wants to keep waiting can
/// simply re-invoke `next_update`.
pub const UPDATE_TIMEOUT: Duration = Duration::from_secs(30);

/// Maximum number of `service_update` frames buffered while waiting for
/// responses (drop-oldest beyond the bound, so a chatty subscription cannot
/// grow the client's memory without limit).
///
/// Drop-oldest is the Node-shaped choice here: a subscription's updates are a
/// "latest state" signal (`next_update` returns the newest buffered update for
/// the subscription it asks about), and Node's own subscription plumbing keeps
/// no replay log — a consumer that stopped reading misses intermediate states
/// either way. A drop is NEVER silent: the lost count is recorded per
/// subscription and the next [`Client::next_update`] for that subscription
/// fails with [`ClientError::UpdateBufferOverflow`] before returning any
/// buffered delta, so the consumer knows its progress stream has a gap. A
/// protocol `SessionSnapshot` cannot repair that gap (v8 snapshots carry no
/// transcript), so the consumer must reload the transcript from its own
/// source; the typed error says so.
pub const MAX_PENDING_UPDATES: usize = 1024;

/// Maximum number of unmatched `response` frames buffered by request id while
/// the caller is consuming updates or waiting for another request. Responses
/// for [`Client::start_call`] requests routinely arrive while
/// [`Client::next_update`] is reading; they are kept here so the later
/// [`Client::wait_call`] finds the terminal frame instead of waiting out the
/// request timeout for a response that was already consumed.
///
/// The table only ever holds frames for request ids this client issued and has
/// not retired, so it is naturally bounded by the caller's outstanding calls;
/// the explicit cap protects against a caller issuing an unbounded burst of
/// un-awaited `start_call`s.
pub const MAX_PENDING_RESPONSES: usize = 128;

/// Diagnostic cadence of the `pending_updates` drop counter: the first drop is
/// reported, then one line per this many further drops.
pub const PENDING_UPDATES_REPORT_EVERY: u64 = 1024;

/// Default bound on reconnect attempts in [`Client::reconnect`].
pub const RECONNECT_MAX_ATTEMPTS: u32 = 3;

/// Default delay before the second reconnect attempt.
pub const RECONNECT_INITIAL_BACKOFF: Duration = Duration::from_millis(500);

/// Default growth factor applied to each successive retry delay.
pub const RECONNECT_BACKOFF_MULTIPLIER: u32 = 2;

/// Default ceiling on a single retry delay.
pub const RECONNECT_MAX_BACKOFF: Duration = Duration::from_secs(8);

/// Retry schedule for [`Client::reconnect`].
///
/// The defaults are deliberately conservative: three attempts (the first try
/// included), a 500ms initial backoff growing by 2x up to a hard 8s ceiling.
/// Every attempt reuses the client's [`ClientTimeouts`] for the TCP connect
/// and the hello handshake — reconnect never bypasses the timeout system.
#[derive(Debug, Clone)]
pub struct ReconnectPolicy {
    /// Total handshake attempts (the first try included); clamped to at
    /// least 1.
    pub max_attempts: u32,
    /// Delay before the second attempt.
    pub initial_backoff: Duration,
    /// Growth factor applied to the delay after each failed attempt.
    pub backoff_multiplier: u32,
    /// Ceiling on a single retry delay.
    pub max_backoff: Duration,
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self {
            max_attempts: RECONNECT_MAX_ATTEMPTS,
            initial_backoff: RECONNECT_INITIAL_BACKOFF,
            backoff_multiplier: RECONNECT_BACKOFF_MULTIPLIER,
            max_backoff: RECONNECT_MAX_BACKOFF,
        }
    }
}

impl ReconnectPolicy {
    /// A single-attempt policy: reconnect once and surface the error with no
    /// backoff sleep in between.
    pub fn single_attempt() -> Self {
        Self {
            max_attempts: 1,
            initial_backoff: Duration::ZERO,
            backoff_multiplier: 1,
            max_backoff: Duration::ZERO,
        }
    }
}

/// Lifecycle of the client's transport, reported as typed
/// [`ConnectionStateChange`] events (never bare strings).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    /// A handshake is in flight (initial connect).
    Connecting,
    /// The handshake completed; requests may be issued.
    Connected,
    /// A reconnect attempt is being made after a broken connection.
    Reconnecting,
    /// The transport is down (no successful handshake).
    Disconnected,
}

/// One typed connection-state transition delivered to listeners registered
/// with [`Client::on_connection_state_change`].
#[derive(Debug, Clone)]
pub struct ConnectionStateChange {
    /// The state entered by this transition.
    pub state: ConnectionState,
    /// For [`ConnectionState::Reconnecting`]: the 1-based attempt number.
    pub attempt: Option<u32>,
    /// Formatted error for transitions caused by a failure.
    pub error: Option<String>,
}

/// Handle returned by [`Client::on_connection_state_change`]; pass it to
/// [`Client::remove_connection_state_listener`] to unsubscribe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectionListenerId(u64);

type ConnectionListener = Box<dyn Fn(&ConnectionStateChange) + Send + Sync>;

/// Summary of one successful [`Client::reconnect`].
#[derive(Debug, Clone)]
pub struct ReconnectReport {
    /// Handshake attempts consumed (1 = the first try succeeded).
    pub attempts: u32,
    /// Session re-attached on the new connection, if a session was attached
    /// before the disconnect and still exists server-side. When the server
    /// dropped the session (current server semantics remove sessions owned by
    /// a disconnected connection), the stale attach state is cleared instead.
    pub restored_session: Option<String>,
    /// Number of subscriptions re-issued on the new connection.
    pub restored_subscriptions: usize,
}

/// Per-operation timeouts applied by a [`Client`].
///
/// Defaults match [`CONNECT_TIMEOUT`], [`REQUEST_TIMEOUT`], and
/// [`UPDATE_TIMEOUT`]. Tests pass sub-second values through the
/// `*_with_timeouts` constructors instead of mutating process state.
#[derive(Debug, Clone, Copy)]
pub struct ClientTimeouts {
    /// Bound on the initial TCP connect.
    pub connect: Duration,
    /// Bound on the hello handshake and each request/response frame read.
    pub request: Duration,
    /// Bound on each subscription update frame read.
    pub update: Duration,
}

impl Default for ClientTimeouts {
    fn default() -> Self {
        Self {
            connect: CONNECT_TIMEOUT,
            request: REQUEST_TIMEOUT,
            update: UPDATE_TIMEOUT,
        }
    }
}

/// Information returned by the server's hello handshake.
#[derive(Debug, Clone, PartialEq)]
pub struct HelloInfo {
    /// The logical server id echoed in the v8 hello.
    pub server_id: String,
    /// Compatibility snapshot derived from the hello frame.
    pub snapshot: Value,
}

/// A registered service subscription.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subscription {
    /// The client-issued subscription id (`sub-N`); re-issued with a fresh id
    /// by [`Client::reconnect`].
    pub subscription_id: String,
    /// The subscribed service id.
    pub service_id: String,
}

/// Tracks the monotonic `sequence` of `pir.session` SNAPSHOT state updates per
/// session instance (the `revision` of the wire `SessionSnapshot`).
///
/// A snapshot update is a full replacement, so the consumer only needs the
/// newest one; the tracker exists to NOTICE a gap: the server's broadcast ring
/// can drop updates for a slow subscriber (`broadcast::RecvError::Lagged`), and
/// progress updates are incremental deltas whose own `sequence` restarts per
/// prompt run, so they cannot prove (or repair) what was missed. A skipped
/// snapshot sequence is the observable symptom; once it is seen the consumer
/// should fetch a fresh session snapshot ([`Client::resync_session_snapshot`])
/// before trusting queued steer/phase state.
///
/// The tracker is deliberately separate from the delta decoder: it only reads
/// the piracy-visible `sequence`/`instance` fields and never materializes the
/// session state.
#[derive(Debug, Default)]
pub struct SnapshotSequenceTracker {
    last: HashMap<String, i64>,
    gaps: HashSet<String>,
}

impl SnapshotSequenceTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// The newest snapshot `sequence` observed for `session_id`.
    pub fn last_sequence(&self, session_id: &str) -> Option<i64> {
        self.last.get(session_id).copied()
    }

    /// Whether a sequence gap was observed for `session_id` and has not been
    /// cleared by a resync.
    pub fn needs_resync(&self, session_id: &str) -> bool {
        self.gaps.contains(session_id)
    }

    /// Clear one session's gap (a fresh snapshot was fetched/applied).
    pub fn clear(&mut self, session_id: &str) {
        self.gaps.remove(session_id);
    }

    /// Restart the high-water mark of `session_id` at `sequence` after a
    /// fresh full snapshot was applied (attach/resync). The snapshot is a
    /// replacement, so any gap recorded before it is repaired; a sequence
    /// LOWER than the previous high-water mark is a restarted stream (server
    /// restart or a re-created session id), not an old snapshot.
    pub fn reset_to(&mut self, session_id: &str, sequence: i64) {
        self.last.insert(session_id.to_owned(), sequence);
        self.gaps.remove(session_id);
    }

    /// Drain the sessions that still need a resync.
    pub fn take_gaps(&mut self) -> Vec<String> {
        std::mem::take(&mut self.gaps).into_iter().collect()
    }

    /// Feed one service-update body. Returns `true` when this update was a
    /// snapshot whose `sequence` skipped past the previous one (the gap is
    /// recorded; a full snapshot was still applied by the caller).
    pub fn observe(&mut self, update: &Value) -> bool {
        if update.get("type").and_then(Value::as_str) != Some("state") {
            return false;
        }
        if update.get("member").and_then(Value::as_str) != Some("snapshot") {
            return false;
        }
        let Some(sequence) = update.get("sequence").and_then(Value::as_i64) else {
            return false;
        };
        let Some(session_id) = update
            .get("instance")
            .and_then(|instance| instance.get("key"))
            .and_then(Value::as_str)
            .filter(|key| !key.is_empty())
        else {
            return false;
        };
        // Sequences are monotonic per session; observing the same update
        // twice must not reset the high-water mark.
        let skipped = match self.last.get(session_id) {
            Some(&previous) if sequence > previous => {
                self.last.insert(session_id.to_owned(), sequence);
                sequence > previous.saturating_add(1)
            }
            Some(&previous) if sequence < previous => {
                // A LOWER sequence is a restarted stream, not a replay: the
                // server restarted, or the session id was removed and created
                // again. Restart the high-water mark (and repair the old gap)
                // so future skips in the new stream are still detected.
                self.last.insert(session_id.to_owned(), sequence);
                self.gaps.remove(session_id);
                false
            }
            Some(_) => false,
            None => {
                self.last.insert(session_id.to_owned(), sequence);
                false
            }
        };
        if skipped {
            self.gaps.insert(session_id.to_owned());
        }
        skipped
    }
}

/// The snapshot `sequence` when `update` is a full `session_id` snapshot.
fn session_snapshot_sequence(update: &Value, session_id: &str) -> Option<i64> {
    if update.get("type").and_then(Value::as_str) != Some("state")
        || update.get("member").and_then(Value::as_str) != Some("snapshot")
        || update
            .get("instance")
            .and_then(|instance| instance.get("key"))
            .and_then(Value::as_str)
            != Some(session_id)
    {
        return None;
    }
    update.get("sequence").and_then(Value::as_i64)
}

/// Whether `update` is a `session_id` snapshot at or before `revision` — a
/// full replacement already superseded by an attach response's snapshot.
fn is_superseded_session_snapshot(update: &Value, session_id: &str, revision: i64) -> bool {
    session_snapshot_sequence(update, session_id).is_some_and(|sequence| sequence <= revision)
}

/// Retires one request id when the wait ends — response, error, timeout, or
/// the caller dropping the [`Client::await_response`] future.
struct PendingRequestGuard {
    pending: Arc<std::sync::Mutex<HashSet<String>>>,
    id: String,
}

impl Drop for PendingRequestGuard {
    fn drop(&mut self) {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.id);
    }
}

/// A connected remote protocol client.
pub struct Client {
    /// Transport halves; `None` once the transport was torn down (broken
    /// connection, or dropped by a reconnect before the next handshake).
    reader: Option<tokio::net::tcp::OwnedReadHalf>,
    writer: Option<tokio::net::tcp::OwnedWriteHalf>,
    decoder: ServerMessageDecoder,
    frame_options: FrameOptions,
    next_id: u64,
    next_subscription: u64,
    hello: HelloInfo,
    /// Client-side connection identifier (compat surface and fence default).
    connection_id: String,
    /// Attachment id leased by the server for the attached session route.
    attachment_id: Option<String>,
    /// Service updates received while waiting for responses.
    pending_updates: VecDeque<(String, Value)>,
    /// Updates dropped by the [`MAX_PENDING_UPDATES`] bound per subscription
    /// id. Reported once through [`ClientError::UpdateBufferOverflow`] on the
    /// next [`Client::next_update`] for that subscription — the bound may not
    /// truncate a progress stream silently.
    pending_updates_lost: HashMap<String, u64>,
    /// Updates dropped by the [`MAX_PENDING_UPDATES`] bound (observation: the
    /// bound may not grow silently).
    pending_updates_dropped: u64,
    /// Complete `response` frames whose request ids were not being awaited
    /// when they arrived (issued with [`Client::start_call`], consumed while
    /// [`Client::next_update`] or another wait was reading). Keyed by request
    /// id and bounded by [`MAX_PENDING_RESPONSES`]; [`Client::wait_call`]
    /// checks here before reading.
    pending_responses: HashMap<String, Value>,
    /// Response ids evicted by the [`MAX_PENDING_RESPONSES`] bound; a wait
    /// for one fails with [`ClientError::ResponseBufferOverflow`] instead of
    /// blocking until the request timeout.
    pending_response_lost: VecDeque<String>,
    /// Fully decoded frames waiting to be consumed (one TCP read can carry
    /// several protocol frames). Naturally bounded: `read_message` only ever
    /// extends it with the frames decoded from ONE 8 KiB socket read, so at
    /// most `8192 / min-frame-size` entries — no explicit cap is needed (and a
    /// cap would have to drop frames, which no consumer could detect).
    pending_frames: VecDeque<Value>,
    /// Request ids whose response is still awaited (upstream
    /// `ConnectionState.activeRequests` / client `#pendingRequests`). An id
    /// leaves the table when its response is consumed (directly from the
    /// socket or from [`Self::pending_responses`]) or the transport is torn
    /// down; [`Client::cancel`] consults it so a cancel for an expired id
    /// returns immediately instead of waiting out the request timeout for a
    /// response the server (which ignores stale cancel ids) will never send.
    pending_requests: Arc<std::sync::Mutex<HashSet<String>>>,
    /// Snapshot sequence tracker over the updates this client has consumed:
    /// exposes broadcast gaps (see [`SnapshotSequenceTracker`]).
    snapshot_sequences: SnapshotSequenceTracker,
    /// Per-operation timeouts captured at construction.
    timeouts: ClientTimeouts,
    /// Resolved server address; `None` for clients built from an existing
    /// stream (`from_stream_*`), which therefore cannot reconnect.
    address: Option<SocketAddr>,
    /// Authentication preface re-sent on every reconnect handshake.
    token: Option<String>,
    /// Current transport state, mirrored to listeners on transitions.
    state: ConnectionState,
    /// Session id attached before a disconnect; restored by reconnect.
    attached_session: Option<String>,
    /// Active subscriptions; re-issued after a reconnect.
    subscriptions: Vec<Subscription>,
    /// Typed connection-state listeners.
    listeners: Vec<(ConnectionListenerId, ConnectionListener)>,
    next_listener: u64,
}

impl Client {
    /// Connect and complete the versioned hello handshake.
    pub async fn connect<A: ToSocketAddrs>(address: A) -> Result<Self, ClientError> {
        Self::connect_with_token_and_timeouts(address, None, ClientTimeouts::default()).await
    }

    /// Connect and authenticate using the server's optional preface token.
    pub async fn connect_with_token<A: ToSocketAddrs>(
        address: A,
        token: Option<&str>,
    ) -> Result<Self, ClientError> {
        Self::connect_with_token_and_timeouts(address, token, ClientTimeouts::default()).await
    }

    /// Connect with explicit per-operation timeouts.
    pub async fn connect_with_timeouts<A: ToSocketAddrs>(
        address: A,
        timeouts: ClientTimeouts,
    ) -> Result<Self, ClientError> {
        Self::connect_with_token_and_timeouts(address, None, timeouts).await
    }

    /// Connect and authenticate using the server's optional preface token,
    /// with explicit per-operation timeouts.
    pub async fn connect_with_token_and_timeouts<A: ToSocketAddrs>(
        address: A,
        token: Option<&str>,
        timeouts: ClientTimeouts,
    ) -> Result<Self, ClientError> {
        // Resolve once so reconnect can target the same endpoint later. The
        // lookup and the TCP connect each stay inside the connect bound.
        let resolved = match timeout(timeouts.connect, lookup_host(address)).await {
            Ok(result) => match result?.next() {
                Some(addr) => addr,
                None => {
                    return Err(ClientError::Io(io::Error::new(
                        io::ErrorKind::AddrNotAvailable,
                        "address resolved to nothing",
                    )))
                }
            },
            Err(_) => {
                return Err(ClientError::Timeout {
                    operation: "connect",
                })
            }
        };
        let stream = match timeout(timeouts.connect, TcpStream::connect(resolved)).await {
            Ok(result) => result?,
            Err(_) => {
                return Err(ClientError::Timeout {
                    operation: "connect",
                })
            }
        };
        let mut client = Self::from_stream_with_token_and_timeouts(stream, token, timeouts).await?;
        client.address = Some(resolved);
        Ok(client)
    }

    /// Complete the handshake with an optional authentication preface.
    pub async fn from_stream_with_token(
        stream: TcpStream,
        token: Option<&str>,
    ) -> Result<Self, ClientError> {
        Self::from_stream_with_token_and_timeouts(stream, token, ClientTimeouts::default()).await
    }

    /// Complete the handshake with an optional authentication preface and
    /// explicit per-operation timeouts.
    pub async fn from_stream_with_token_and_timeouts(
        stream: TcpStream,
        token: Option<&str>,
        timeouts: ClientTimeouts,
    ) -> Result<Self, ClientError> {
        let options = FrameOptions::default();
        let (reader, writer) = stream.into_split();
        let mut client = Self {
            reader: Some(reader),
            writer: Some(writer),
            decoder: ServerMessageDecoder::new(options)?,
            frame_options: options,
            next_id: 1,
            next_subscription: 1,
            hello: HelloInfo {
                server_id: String::new(),
                snapshot: Value::Null,
            },
            connection_id: Uuid::new_v4().to_string(),
            attachment_id: None,
            pending_updates: VecDeque::new(),
            pending_updates_lost: HashMap::new(),
            pending_updates_dropped: 0,
            pending_responses: HashMap::new(),
            pending_response_lost: VecDeque::new(),
            pending_frames: VecDeque::new(),
            pending_requests: Arc::new(std::sync::Mutex::new(HashSet::new())),
            snapshot_sequences: SnapshotSequenceTracker::new(),
            timeouts,
            address: None,
            token: token.map(str::to_owned),
            state: ConnectionState::Connecting,
            attached_session: None,
            subscriptions: Vec::new(),
            listeners: Vec::new(),
            next_listener: 0,
        };
        if let Some(token) = token {
            let writer = client.writer.as_mut().ok_or(ClientError::Disconnected)?;
            writer
                .write_all(format!("PIR-AUTH {token}\n").as_bytes())
                .await?;
            writer.flush().await?;
        }
        client
            .send(&ClientEnvelope::Hello(ClientHello {
                type_: "hello".to_owned(),
                version: PROTOCOL_VERSION,
            }))
            .await?;
        // The handshake reply is bounded so a peer that accepts the
        // connection but never greets cannot stall the connect.
        let value = client
            .read_message_with_timeout(timeouts.request, "hello")
            .await?;
        match value.get("type").and_then(Value::as_str) {
            Some("hello") => {
                let server_id = value
                    .get("serverId")
                    .and_then(Value::as_str)
                    .ok_or_else(|| ClientError::InvalidResponse("hello lacks serverId".to_owned()))?
                    .to_owned();
                client.hello = HelloInfo {
                    snapshot: serde_json::json!({
                        "serverId": server_id,
                        "protocolVersion": PROTOCOL_VERSION,
                    }),
                    server_id,
                };
                client.state = ConnectionState::Connected;
                Ok(client)
            }
            Some("hello_error") => {
                // Malformed error frames share one decoding path with
                // `await_response`: missing/non-string `code` values fall back
                // to the server's `internal_error` vocabulary.
                Err(server_error(value.get("error"), "server rejected hello"))
            }
            _ => Err(ClientError::InvalidResponse(
                "first server message was not hello".to_owned(),
            )),
        }
    }

    /// Return the logical server id assigned during the handshake.
    pub fn server_id(&self) -> &str {
        &self.hello.server_id
    }

    /// Return the client-side connection identifier.
    ///
    /// Protocol v8 does not carry a server-assigned connection id; this is the
    /// locally generated identifier used for subscriptions.
    pub fn connection_id(&self) -> &str {
        &self.connection_id
    }

    /// Return the hello-derived compatibility snapshot.
    pub fn snapshot(&self) -> &Value {
        &self.hello.snapshot
    }

    /// Return the attachment id currently leased to this connection, if any.
    pub fn attachment_id(&self) -> Option<&str> {
        self.attachment_id.as_deref()
    }

    /// Return the current transport state.
    pub fn connection_state(&self) -> ConnectionState {
        self.state
    }

    /// Return the session id that was attached before a disconnect and is
    /// still being restored by reconnect; `None` once the attach state was
    /// dropped (detached, or the session no longer exists server-side).
    pub fn attached_session(&self) -> Option<&str> {
        self.attached_session.as_deref()
    }

    /// Return the subscriptions registered on the current connection.
    ///
    /// After a reconnect the subscription ids are re-issued, so consumers of
    /// [`Client::next_update`] must refresh their [`Subscription`] handles
    /// from this list.
    pub fn subscriptions(&self) -> &[Subscription] {
        &self.subscriptions
    }

    /// Test/profiling probe (`#[doc(hidden)]`): how many buffered
    /// `service_update` frames the [`MAX_PENDING_UPDATES`] bound dropped. Zero
    /// on any client whose consumer keeps up; nonzero proves the bound engaged
    /// (and the loss was reported through
    /// [`ClientError::UpdateBufferOverflow`] on the next `next_update`).
    #[doc(hidden)]
    pub fn pending_updates_dropped(&self) -> u64 {
        self.pending_updates_dropped
    }

    /// Test/profiling probe (`#[doc(hidden)]`): how many unmatched response
    /// frames are currently buffered for later [`Client::wait_call`]s.
    #[doc(hidden)]
    pub fn pending_responses_count(&self) -> usize {
        self.pending_responses.len()
    }

    /// Test/profiling probe (`#[doc(hidden)]`): how many request ids are
    /// currently awaiting a response. A timed-out/abandoned request must not
    /// stay in this table — a stale id makes [`Client::cancel`] wait out
    /// another full request timeout for a response that can never arrive.
    #[doc(hidden)]
    pub fn pending_requests_count(&self) -> usize {
        self.pending_requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    /// Newest snapshot `sequence` observed for `session_id` (see
    /// [`SnapshotSequenceTracker`]).
    pub fn snapshot_sequence(&self, session_id: &str) -> Option<i64> {
        self.snapshot_sequences.last_sequence(session_id)
    }

    /// Whether a snapshot sequence gap was observed for `session_id` and not
    /// yet cleared by [`Client::resync_session_snapshot`]. A `true` result
    /// means at least one broadcast update was lost (a lagged receiver), so
    /// phase/steer state may be stale.
    pub fn snapshot_sequence_gap(&self, session_id: &str) -> bool {
        self.snapshot_sequences.needs_resync(session_id)
    }

    /// Drain the sessions currently flagged as needing a resync.
    pub fn take_snapshot_sequence_gaps(&mut self) -> Vec<String> {
        self.snapshot_sequences.take_gaps()
    }

    /// Fetch a fresh session **state** snapshot after a detected sequence
    /// gap.
    ///
    /// Reuses the existing `attach` call (its response carries the current
    /// `SessionSnapshot`, and the server broadcasts a snapshot update to every
    /// subscriber), so no wire-version change is needed. On success the
    /// session's gap flag is cleared and the `session` snapshot value is
    /// returned; on failure the flag stays set so the caller can retry.
    ///
    /// This is not a transcript recovery operation: protocol v8 delivers the
    /// transcript as incremental `progress` updates and a `SessionSnapshot`'s
    /// `transcript` array is always empty. The resync restores the lifecycle
    /// view (`phase`, `revision`, queued steer) only; progress deltas lost to a
    /// [`MAX_PENDING_UPDATES`] overflow ([`ClientError::UpdateBufferOverflow`])
    /// remain lost and must be reloaded from another source.
    pub async fn resync_session_snapshot(
        &mut self,
        session_id: &str,
    ) -> Result<Value, ClientError> {
        let result = self
            .call(
                RpcTarget::server(self.server_id()),
                SESSION_SERVICE_ID,
                "attach",
                vec![serde_json::json!({"sessionId": session_id})],
            )
            .await?;
        if let Some(attachment_id) = result.get("attachmentId").and_then(Value::as_str) {
            self.attachment_id = Some(attachment_id.to_owned());
        }
        self.attached_session = Some(session_id.to_owned());
        Ok(self.apply_attach_snapshot(session_id, &result))
    }

    /// Apply the full snapshot an `attach` response carries (the resync and
    /// reconnect paths share it).
    ///
    /// Snapshot updates are full replacements, so the response's snapshot is
    /// a recovery point for the SNAPSHOT state stream: queued snapshots it
    /// already supersedes are dropped (returning one later would regress the
    /// consumer's state), and the sequence tracker restarts at the response's
    /// `revision` — a LOWER value than the previous mark means the stream
    /// restarted, and a gap recorded before this fresh snapshot is repaired.
    /// It is not a transcript replay point: v8 snapshots carry an empty
    /// `transcript`, and lost `progress` updates are never reconstructed here.
    ///
    /// A queued snapshot AHEAD of the response (it was broadcast after the
    /// server captured the response snapshot but before the client read the
    /// response) is newer than the recovery point and stays queued. The
    /// tracker's mark must account for it: otherwise the next update after
    /// that snapshot would look like a sequence gap even though the queued
    /// full snapshot already carries the missing state.
    fn apply_attach_snapshot(&mut self, session_id: &str, result: &Value) -> Value {
        let session = result.get("session").cloned().unwrap_or(Value::Null);
        match session.get("revision").and_then(Value::as_i64) {
            Some(revision) => {
                let mut newest = revision;
                self.pending_updates.retain(|(_, update)| {
                    if is_superseded_session_snapshot(update, session_id, revision) {
                        return false;
                    }
                    if let Some(sequence) = session_snapshot_sequence(update, session_id) {
                        newest = newest.max(sequence);
                    }
                    true
                });
                self.snapshot_sequences.reset_to(session_id, newest);
            }
            // A response without a revision still carries a full snapshot;
            // clear the gap and let the next observed update set the mark.
            None => self.snapshot_sequences.clear(session_id),
        }
        session
    }

    /// Register a typed connection-state listener; returns an id that can be
    /// passed to [`Client::remove_connection_state_listener`].
    ///
    /// The initial `Connected` transition of [`Client::connect`] happens
    /// before listeners can be registered, so the first event a listener sees
    /// is a disconnect or a `Reconnecting` attempt.
    pub fn on_connection_state_change<F>(&mut self, listener: F) -> ConnectionListenerId
    where
        F: Fn(&ConnectionStateChange) + Send + Sync + 'static,
    {
        let id = ConnectionListenerId(self.next_listener);
        self.next_listener += 1;
        self.listeners.push((id, Box::new(listener)));
        id
    }

    /// Remove a listener previously registered with
    /// [`Client::on_connection_state_change`].
    pub fn remove_connection_state_listener(&mut self, id: ConnectionListenerId) {
        self.listeners.retain(|(listener_id, _)| listener_id != &id);
    }

    /// Drop the current transport and re-establish it against the originally
    /// resolved server address using the default [`ReconnectPolicy`], then
    /// restore session state. See [`Client::reconnect_with_policy`].
    pub async fn reconnect(&mut self) -> Result<ReconnectReport, ClientError> {
        self.reconnect_with_policy(ReconnectPolicy::default()).await
    }

    /// Drop the current transport and re-establish it against the originally
    /// resolved server address, bounded by `policy`:
    ///
    /// - every attempt re-runs the full versioned hello handshake, re-sending
    ///   the stored authentication preface, under the client's own
    ///   [`ClientTimeouts`] (connect bound for the TCP connect, request bound
    ///   for the hello reply) — reconnect never bypasses the timeout system;
    /// - failed attempts sleep for the policy's backoff (growing
    ///   multiplicatively up to `max_backoff`) before retrying, up to
    ///   `max_attempts` in total;
    /// - on success the client re-attaches to the session attached before the
    ///   disconnect (if the session still exists server-side) and re-issues
    ///   the active subscriptions; the outcome is summarized in the returned
    ///   [`ReconnectReport`].
    ///
    /// Clients built from an existing stream ([`Client::from_stream_with_token`])
    /// have no stored address and fail with [`ClientError::ReconnectUnavailable`].
    pub async fn reconnect_with_policy(
        &mut self,
        policy: ReconnectPolicy,
    ) -> Result<ReconnectReport, ClientError> {
        let address = self.address.ok_or(ClientError::ReconnectUnavailable)?;
        let max_attempts = policy.max_attempts.max(1);
        if self.state == ConnectionState::Connected {
            // The old transport is about to be torn down on purpose; report
            // the transition so listeners observe the full sequence.
            self.emit_state(ConnectionState::Disconnected, None, None);
        }
        self.drop_transport();
        let mut attempts = 0_u32;
        let mut backoff = policy.initial_backoff;
        loop {
            attempts += 1;
            self.emit_state(ConnectionState::Reconnecting, Some(attempts), None);
            match self.establish(address).await {
                Ok(()) => {
                    let report = self.restore_session_state(attempts).await;
                    self.emit_state(ConnectionState::Connected, None, None);
                    return Ok(report);
                }
                Err(error) => {
                    if attempts >= max_attempts {
                        // The teardown before the loop (the explicit emit
                        // above, or `drop_transport`'s own guarded emit)
                        // already reported the single `Disconnected`
                        // transition for this sequence. The reconnect
                        // attempts overwrote `state`, so re-checking `state`
                        // here cannot tell — emitting again would repeat the
                        // event. Land the state silently and surface the
                        // failure through the returned error instead.
                        self.state = ConnectionState::Disconnected;
                        return Err(error);
                    }
                    if !backoff.is_zero() {
                        tokio::time::sleep(backoff).await;
                    }
                    backoff = (backoff * policy.backoff_multiplier).min(policy.max_backoff);
                }
            }
        }
    }

    /// Tear down the current transport halves and any buffered frames from
    /// the dead connection.
    fn drop_transport(&mut self) {
        self.reader = None;
        self.writer = None;
        self.pending_updates.clear();
        self.pending_updates_lost.clear();
        self.pending_responses.clear();
        self.pending_response_lost.clear();
        self.pending_frames.clear();
        // Requests from the dead connection can never receive a response;
        // dropping them keeps `cancel` from sending a cancel frame for an id
        // the new connection never issued.
        self.pending_requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        if self.state != ConnectionState::Disconnected {
            self.emit_state(ConnectionState::Disconnected, None, None);
        }
    }

    /// Establish one fresh transport against `address`, swapping in the new
    /// connection-scoped state on success. The client's identity, counters,
    /// listeners, and session bookkeeping are preserved.
    async fn establish(&mut self, address: SocketAddr) -> Result<(), ClientError> {
        let fresh =
            Self::connect_with_token_and_timeouts(address, self.token.as_deref(), self.timeouts)
                .await?;
        self.reader = fresh.reader;
        self.writer = fresh.writer;
        self.decoder = fresh.decoder;
        self.frame_options = fresh.frame_options;
        self.hello = fresh.hello;
        // The old lease died with the old connection; restore_session_state
        // re-attaches and leases a fresh one.
        self.attachment_id = None;
        self.pending_updates = fresh.pending_updates;
        self.pending_updates_lost = fresh.pending_updates_lost;
        self.pending_responses = fresh.pending_responses;
        self.pending_response_lost = fresh.pending_response_lost;
        self.pending_frames = fresh.pending_frames;
        self.state = ConnectionState::Connected;
        Ok(())
    }

    /// Best-effort restoration after a successful reconnect: re-attach to the
    /// pre-disconnect session (if it still exists server-side) and re-issue
    /// the active subscriptions.
    async fn restore_session_state(&mut self, attempts: u32) -> ReconnectReport {
        let mut report = ReconnectReport {
            attempts,
            restored_session: None,
            restored_subscriptions: 0,
        };
        if let Some(session_id) = self.attached_session.clone() {
            let attach = self
                .call(
                    RpcTarget::server(self.server_id()),
                    SESSION_SERVICE_ID,
                    "attach",
                    vec![serde_json::json!({"sessionId": session_id})],
                )
                .await;
            match attach {
                Ok(result) => {
                    if let Some(attachment_id) = result.get("attachmentId").and_then(Value::as_str)
                    {
                        self.attachment_id = Some(attachment_id.to_owned());
                    }
                    // The attach response carries a full snapshot: it repairs
                    // a sequence gap observed on the old connection and is the
                    // new high-water mark for the restored session.
                    let _ = self.apply_attach_snapshot(&session_id, &result);
                    report.restored_session = Some(session_id);
                }
                Err(_) => {
                    // The session is gone server-side (the current server
                    // removes sessions owned by a disconnected connection) or
                    // the fresh connection already failed. Drop the stale
                    // attach state instead of failing the whole reconnect;
                    // the loss is observable through `attached_session()`.
                    self.attached_session = None;
                    self.attachment_id = None;
                }
            }
        }
        let pending = std::mem::take(&mut self.subscriptions);
        for previous in pending {
            match self.send_subscribe(&previous.service_id).await {
                Ok(subscription) => {
                    self.subscriptions.push(subscription);
                    report.restored_subscriptions += 1;
                }
                Err(_) => {
                    // Unrestorable on this connection; dropped from the
                    // registry so callers see the surviving set via
                    // `subscriptions()`.
                }
            }
        }
        report
    }

    /// Enter `state` and fire the registered connection-state listeners.
    fn emit_state(&mut self, state: ConnectionState, attempt: Option<u32>, error: Option<String>) {
        self.state = state;
        if self.listeners.is_empty() {
            return;
        }
        let change = ConnectionStateChange {
            state,
            attempt,
            error,
        };
        for (_, listener) in &self.listeners {
            listener(&change);
        }
    }

    /// Issue one generic RPC request and return the response result.
    pub async fn call(
        &mut self,
        target: RpcTarget,
        service_id: &str,
        member: &str,
        args: Vec<Value>,
    ) -> Result<Value, ClientError> {
        let id = self.next_request_id();
        let call = ServiceCall {
            service_id: service_id.to_owned(),
            member: member.to_owned(),
            args,
            instance: None,
        };
        self.send_request(id.clone(), target, call).await?;
        self.await_response(&id).await
    }

    /// Issue one generic RPC request **without** waiting for its response,
    /// returning the request id. Pair with [`Client::wait_call`]: this is how
    /// a client drives the server's long-running calls (e.g. `prompt`) while
    /// still issuing `steer`/`abort`/`cancel` in between.
    pub async fn start_call(
        &mut self,
        target: RpcTarget,
        service_id: &str,
        member: &str,
        args: Vec<Value>,
    ) -> Result<String, ClientError> {
        let id = self.next_request_id();
        let call = ServiceCall {
            service_id: service_id.to_owned(),
            member: member.to_owned(),
            args,
            instance: None,
        };
        self.send_request(id.clone(), target, call).await?;
        Ok(id)
    }

    /// Wait for the response of a request issued with [`Client::start_call`].
    ///
    /// Only one wait may be outstanding at a time (requests are serialized on
    /// one connection). A response consumed earlier by [`Client::next_update`]
    /// (or while waiting for another request) is buffered by request id and
    /// resolved here immediately; responses for other request ids are kept
    /// the same way, so a later wait never misses a terminal frame that
    /// already arrived.
    pub async fn wait_call(&mut self, id: &str) -> Result<Value, ClientError> {
        self.await_response(id).await
    }

    /// Send a cancel frame for a previously issued request id.
    ///
    /// The pending table is consulted first: an id with no request still
    /// awaiting a response can never produce one — the server silently
    /// ignores stale cancel ids (there is no ack for a cancel frame), so the
    /// old code waited out the full request timeout for nothing. Upstream
    /// cleans the request up locally and fires the cancel frame without
    /// waiting (client.ts `onAbort` → `sendCancel`); here a stale id lands
    /// the same terminal `cancelled` outcome immediately, and a live id
    /// keeps the existing behavior: send the cancel frame and wait for the
    /// cancelled request's own terminal response.
    pub async fn cancel(&mut self, id: &str, target: RpcTarget) -> Result<Value, ClientError> {
        let still_pending = self
            .pending_requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(id);
        if !still_pending {
            return Err(ClientError::Server {
                code: "cancelled".to_owned(),
                message: "RPC request cancelled".to_owned(),
            });
        }
        if let Err(error) = self
            .send(&ClientEnvelope::Cancel(RpcCancel {
                type_: "cancel".to_owned(),
                id: id.to_owned(),
                target,
            }))
            .await
        {
            // The cancel itself never reached the server: the request cannot
            // be terminated through this connection, so retires locally (a
            // later cancel must not wait for an impossible response).
            self.pending_requests
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(id);
            return Err(error);
        }
        self.await_response(id).await
    }

    /// Subscribe to a service's updates through the built-in control service.
    ///
    /// The subscription is recorded so [`Client::reconnect`] re-issues it on
    /// the new connection (with a fresh subscription id).
    pub async fn subscribe(&mut self, service_id: &str) -> Result<Subscription, ClientError> {
        let subscription = self.send_subscribe(service_id).await?;
        self.subscriptions.push(subscription.clone());
        Ok(subscription)
    }

    /// Issue one subscribe call on the current connection without recording
    /// it (used both by [`Client::subscribe`] and reconnect restoration).
    async fn send_subscribe(&mut self, service_id: &str) -> Result<Subscription, ClientError> {
        let subscription_id = format!("sub-{}", self.next_subscription);
        self.next_subscription += 1;
        let id = self.next_request_id();
        let call = ServiceCall {
            service_id: SERVICE_CONTROL_ID.to_owned(),
            member: "subscribe".to_owned(),
            args: vec![
                Value::from(subscription_id.clone()),
                Value::from(service_id.to_owned()),
                Value::from("singleton"),
            ],
            instance: None,
        };
        self.send_request(id.clone(), RpcTarget::server(self.server_id()), call)
            .await?;
        self.await_response(&id).await?;
        Ok(Subscription {
            subscription_id,
            service_id: service_id.to_owned(),
        })
    }

    /// Cancel one subscription through the built-in control service.
    pub async fn unsubscribe(&mut self, subscription: &Subscription) -> Result<Value, ClientError> {
        self.subscriptions
            .retain(|registered| registered.subscription_id != subscription.subscription_id);
        let id = self.next_request_id();
        let call = ServiceCall {
            service_id: SERVICE_CONTROL_ID.to_owned(),
            member: "unsubscribe".to_owned(),
            args: vec![Value::from(subscription.subscription_id.clone())],
            instance: None,
        };
        self.send_request(id.clone(), RpcTarget::server(self.server_id()), call)
            .await?;
        self.await_response(&id).await
    }

    /// Return the next `service_update` for a subscription.
    ///
    /// Updates received while a response was pending are returned first;
    /// otherwise the client reads frames until a matching update arrives.
    ///
    /// If the [`MAX_PENDING_UPDATES`] bound had to drop updates for this
    /// subscription, the call fails with [`ClientError::UpdateBufferOverflow`]
    /// BEFORE replaying the surviving (non-contiguous) delta stream, so the
    /// gap is never silent. The error is delivered once per overflow episode;
    /// later calls continue with the surviving buffered updates.
    pub async fn next_update(&mut self, subscription: &Subscription) -> Result<Value, ClientError> {
        if let Some(dropped) = self
            .pending_updates_lost
            .remove(&subscription.subscription_id)
        {
            return Err(ClientError::UpdateBufferOverflow {
                subscription_id: subscription.subscription_id.clone(),
                dropped,
            });
        }
        if let Some(position) = self
            .pending_updates
            .iter()
            .position(|(id, _)| id == &subscription.subscription_id)
        {
            if let Some((_, update)) = self.pending_updates.remove(position) {
                self.invalidate_on_closed(&update);
                // The update was already observed when it was queued
                // (`queue_push`); observing a replayed, OLDER snapshot again
                // would move the high-water mark backwards and read as a
                // restarted stream (clearing a real gap).
                return Ok(update);
            }
        }
        let update_timeout = self.timeouts.update;
        loop {
            let value = self
                .read_message_with_timeout(update_timeout, "update")
                .await?;
            match value.get("type").and_then(Value::as_str) {
                Some("service_update")
                    if value.get("subscriptionId").and_then(Value::as_str)
                        == Some(subscription.subscription_id.as_str()) =>
                {
                    let update = value.get("update").cloned().unwrap_or(Value::Null);
                    self.invalidate_on_closed(&update);
                    self.snapshot_sequences.observe(&update);
                    return Ok(update);
                }
                _ => self.queue_push(&value),
            }
        }
    }

    /// [`Client::next_update`] decoded through
    /// [`decode_service_update`]: both pir's `{type:"state",...}` updates and
    /// upstream chord `{t:"state",...}` frames are accepted, and delta ops
    /// arrive as typed [`crate::protocol::StateOp`] values.
    ///
    /// Each call starts a fresh delta path dictionary; a producer that
    /// interns paths across updates needs
    /// [`Client::next_state_update_with`].
    pub async fn next_state_update(
        &mut self,
        subscription: &Subscription,
    ) -> Result<ServiceUpdate, ClientError> {
        self.next_state_update_with(subscription, &mut StateUpdateDecoder::new())
            .await
    }

    /// [`Client::next_state_update`] using a caller-owned stateful decoder,
    /// which retains chord delta path ids across the subscription's updates.
    pub async fn next_state_update_with(
        &mut self,
        subscription: &Subscription,
        decoder: &mut StateUpdateDecoder,
    ) -> Result<ServiceUpdate, ClientError> {
        let update = self.next_update(subscription).await?;
        decoder
            .decode(&update)
            .map_err(|error| ClientError::InvalidResponse(error.to_string()))
    }

    /// Drop the attach state when a `closed` service-provider update
    /// (`{"type":"closed","instance":{key,generation}}` — the removal
    /// invalidation of upstream `invalidate`, session-router.ts:302-311)
    /// names the attached session. A removed session is not a transport
    /// failure: the connection state stays `Connected` and no
    /// [`ConnectionStateChange`] is emitted; only the dead session route's
    /// attach bookkeeping is dropped, observable through
    /// [`Client::attached_session`] (a later reconnect will no longer try
    /// to restore it). The update itself is still returned to the caller.
    fn invalidate_on_closed(&mut self, update: &Value) {
        if update.get("type").and_then(Value::as_str) != Some("closed") {
            return;
        }
        let key = update
            .get("instance")
            .and_then(|instance| instance.get("key"))
            .and_then(Value::as_str);
        if let Some(session_id) = self.attached_session.as_deref() {
            if key == Some(session_id) {
                self.attached_session = None;
                self.attachment_id = None;
            }
        }
    }

    /// Queue push frames (service updates, attachment routes, unmatched
    /// responses) encountered while waiting for a response. Service updates
    /// are drop-oldest bounded at [`MAX_PENDING_UPDATES`] and a drop is
    /// recorded per subscription for [`Client::next_update`] to report;
    /// attachment frames lacking a valid, non-empty `attachmentId` are
    /// ignored (they must not reset the current lease — detach clears it
    /// through the `Detach` response path). Responses for request ids this
    /// client issued but is not awaiting right now are buffered by id
    /// (bounded at [`MAX_PENDING_RESPONSES`]) so [`Client::wait_call`] can
    /// still resolve them.
    fn queue_push(&mut self, value: &Value) {
        match value.get("type").and_then(Value::as_str) {
            Some("service_update") => {
                let frame_id = value
                    .get("subscriptionId")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let update = value.get("update").cloned().unwrap_or(Value::Null);
                self.snapshot_sequences.observe(&update);
                self.pending_updates
                    .push_back((frame_id.to_owned(), update));
                while self.pending_updates.len() > MAX_PENDING_UPDATES {
                    let Some((dropped_subscription, _)) = self.pending_updates.pop_front() else {
                        break;
                    };
                    // Only registered subscriptions can ever be asked for via
                    // `next_update`; dropping frames for an unknown id must not
                    // grow the loss map unboundedly under an adversarial peer.
                    if self
                        .subscriptions
                        .iter()
                        .any(|sub| sub.subscription_id == dropped_subscription)
                    {
                        *self
                            .pending_updates_lost
                            .entry(dropped_subscription)
                            .or_insert(0) += 1;
                    }
                    self.pending_updates_dropped = self.pending_updates_dropped.saturating_add(1);
                    let dropped = self.pending_updates_dropped;
                    if dropped == 1 || dropped.is_multiple_of(PENDING_UPDATES_REPORT_EVERY) {
                        crate::ext_diag!(
                            "[client] subscription update buffer full ({} updates): dropped the oldest updates ({} so far); the next next_update reports UpdateBufferOverflow",
                            MAX_PENDING_UPDATES,
                            dropped
                        );
                    }
                }
            }
            Some("attachment") => {
                if let Some(id) = value
                    .get("attachment")
                    .and_then(|target| target.get("attachmentId"))
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                {
                    self.attachment_id = Some(id.to_owned());
                }
            }
            Some("response") => {
                let Some(id) = value
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                else {
                    return;
                };
                // Only responses to requests still in the pending table can
                // ever have a waiter: a late/unknown id is dropped exactly as
                // before (not letting an adversarial peer grow the map).
                let awaiting = self
                    .pending_requests
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .contains(id);
                if !awaiting || self.pending_responses.contains_key(id) {
                    return;
                }
                if self.pending_responses.len() >= MAX_PENDING_RESPONSES {
                    self.pending_response_lost.push_back(id.to_owned());
                    while self.pending_response_lost.len() > MAX_PENDING_RESPONSES {
                        self.pending_response_lost.pop_front();
                    }
                    crate::ext_diag!(
                        "[client] response buffer full ({} responses): dropped the response for request {}",
                        MAX_PENDING_RESPONSES,
                        id
                    );
                    return;
                }
                self.pending_responses.insert(id.to_owned(), value.clone());
            }
            _ => {}
        }
    }

    /// Read frames until the response with the given id arrives, queueing
    /// service updates and attachment route changes along the way. The
    /// matching response retires the id from the pending table (success and
    /// error responses alike).
    ///
    /// EVERY abandoned wait retires the id too: on a timeout, a broken
    /// transport, or a malformed response the request can never produce a
    /// response through this connection, and leaving it in the table made a
    /// later [`Client::cancel`] believe it was still in flight (waiting out a
    /// second full request timeout) and a late response for the id could be
    /// mistaken for the next request's.
    async fn await_response(&mut self, id: &str) -> Result<Value, ClientError> {
        // Retire the id on EVERY exit, including a caller dropping this
        // future (an outer timeout or `select!`): a guard owns the shared
        // table, so no abandoned wait can leave an id that makes a later
        // `cancel` wait for a response that will never arrive.
        let _guard = PendingRequestGuard {
            pending: Arc::clone(&self.pending_requests),
            id: id.to_owned(),
        };
        // A response consumed by `next_update` (or another wait) while this
        // request was outstanding is buffered by id; resolve it before
        // reading, or the wait would block for a frame that already arrived.
        if let Some(position) = self
            .pending_response_lost
            .iter()
            .position(|lost| lost.as_str() == id)
        {
            self.pending_response_lost.remove(position);
            self.pending_requests
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(id);
            return Err(ClientError::ResponseBufferOverflow { id: id.to_owned() });
        }
        if let Some(value) = self.pending_responses.remove(id) {
            self.pending_requests
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(id);
            return response_outcome(&value);
        }
        self.await_response_inner(id).await
    }

    async fn await_response_inner(&mut self, id: &str) -> Result<Value, ClientError> {
        let request_timeout = self.timeouts.request;
        loop {
            let value = self
                .read_message_with_timeout(request_timeout, "request")
                .await?;
            if value.get("type").and_then(Value::as_str) == Some("response")
                && value.get("id").and_then(Value::as_str) == Some(id)
            {
                self.pending_requests
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(id);
                return response_outcome(&value);
            }
            self.queue_push(&value);
        }
    }

    // -----------------------------------------------------------------------
    // Compatibility surface: the concrete v1 command catalog mapped onto the
    // v8 pir.session service.
    // -----------------------------------------------------------------------

    /// Query the current session metadata snapshot.
    pub async fn list(&mut self) -> Result<Vec<Value>, ClientError> {
        let value = self
            .call(
                RpcTarget::server(self.server_id()),
                SESSION_SERVICE_ID,
                "list",
                Vec::new(),
            )
            .await?;
        let sessions = value
            .get("sessions")
            .and_then(Value::as_array)
            .ok_or_else(|| ClientError::InvalidResponse("list result lacks sessions".to_owned()))?;
        Ok(sessions.clone())
    }

    /// Send one v1 command, translated to a `pir.session` service call.
    pub async fn request(&mut self, request: Command) -> Result<Value, ClientError> {
        let server_target = RpcTarget::server(self.server_id());
        let session_target = |client: &Self, session_id: &str| {
            RpcTarget::session(
                client.server_id(),
                session_id,
                client
                    .attachment_id
                    .clone()
                    .unwrap_or_else(|| client.connection_id.clone()),
            )
        };
        match request {
            Command::List => {
                self.call(server_target, SESSION_SERVICE_ID, "list", Vec::new())
                    .await
            }
            Command::Create {
                cwd,
                name,
                model,
                thinking_level,
            } => {
                let mut args = serde_json::Map::new();
                if let Some(cwd) = cwd {
                    args.insert("cwd".to_owned(), Value::from(cwd));
                }
                if let Some(name) = name {
                    args.insert("name".to_owned(), Value::from(name));
                }
                if let Some(model) = model {
                    args.insert("model".to_owned(), serde_json::to_value(model)?);
                }
                if let Some(thinking_level) = thinking_level {
                    args.insert("thinkingLevel".to_owned(), Value::from(thinking_level));
                }
                self.call(
                    server_target,
                    SESSION_SERVICE_ID,
                    "create",
                    vec![Value::Object(args)],
                )
                .await
            }
            Command::Attach { session_id } => {
                let result = self
                    .call(
                        server_target,
                        SESSION_SERVICE_ID,
                        "attach",
                        vec![serde_json::json!({"sessionId": session_id})],
                    )
                    .await;
                if let Ok(result) = &result {
                    if let Some(attachment_id) = result.get("attachmentId").and_then(Value::as_str)
                    {
                        self.attachment_id = Some(attachment_id.to_owned());
                    }
                    // The response's snapshot is a fresh recovery point (the
                    // same rule `resync_session_snapshot` applies).
                    let _ = self.apply_attach_snapshot(&session_id, result);
                    // Remember the attach so a later reconnect can restore it.
                    self.attached_session = Some(session_id.clone());
                }
                result
            }
            Command::Detach { session_id } => {
                let target = session_target(self, &session_id);
                let result = self
                    .call(target, SESSION_SERVICE_ID, "detach", Vec::new())
                    .await;
                if result.is_ok() {
                    self.attachment_id = None;
                    self.attached_session = None;
                }
                result
            }
            Command::Prompt { session_id, text } => {
                let target = session_target(self, &session_id);
                self.call(
                    target,
                    SESSION_SERVICE_ID,
                    "prompt",
                    vec![serde_json::json!({"text": text})],
                )
                .await
            }
            Command::Steer { session_id, text } => {
                let target = session_target(self, &session_id);
                self.call(
                    target,
                    SESSION_SERVICE_ID,
                    "steer",
                    vec![serde_json::json!({"text": text})],
                )
                .await
            }
            Command::Abort { session_id } => {
                let target = session_target(self, &session_id);
                self.call(target, SESSION_SERVICE_ID, "abort", Vec::new())
                    .await
            }
            Command::SetModel { session_id, model } => {
                let target = session_target(self, &session_id);
                self.call(
                    target,
                    SESSION_SERVICE_ID,
                    "set_model",
                    vec![serde_json::json!({"model": model})],
                )
                .await
            }
            Command::SetThinking {
                session_id,
                thinking_level,
            } => {
                let target = session_target(self, &session_id);
                self.call(
                    target,
                    SESSION_SERVICE_ID,
                    "set_thinking",
                    vec![serde_json::json!({"thinkingLevel": thinking_level})],
                )
                .await
            }
            Command::Remove { session_id } => {
                // Removal reuses the session-target fencing: the caller must
                // hold the session's attachment. On success the server evicts
                // the session and emits the out-of-band null attachment
                // (upstream `publishAttachment(undefined)`); like the Detach
                // path the lease is cleared from the response outcome — a
                // null attachment frame carries no `attachmentId`, so the
                // queue ignores it and never resets the lease by itself.
                let target = session_target(self, &session_id);
                let result = self
                    .call(target, SESSION_SERVICE_ID, "remove_session", Vec::new())
                    .await;
                if result.is_ok() {
                    self.attachment_id = None;
                    self.attached_session = None;
                }
                result
            }
        }
    }

    fn next_request_id(&mut self) -> String {
        let id = self.next_id.to_string();
        self.next_id = self.next_id.saturating_add(1);
        id
    }

    async fn send_request(
        &mut self,
        id: String,
        target: RpcTarget,
        call: ServiceCall,
    ) -> Result<(), ClientError> {
        self.send(&ClientEnvelope::Request(RpcRequest {
            type_: "request".to_owned(),
            id: id.clone(),
            target,
            call,
        }))
        .await?;
        // Record the request so `cancel` can tell a live id from an expired
        // one (a failed send never registers: nothing is in flight).
        self.pending_requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id);
        Ok(())
    }

    async fn send(&mut self, message: &ClientEnvelope) -> Result<(), ClientError> {
        let frame = encode_client_envelope(message, self.frame_options)?;
        // The write is bounded by the request timeout: a peer that never
        // reads (full socket buffer) must fail the send instead of blocking
        // the caller forever. A torn-down transport fails fast.
        let write = async {
            let writer = self.writer.as_mut().ok_or(ClientError::Disconnected)?;
            writer.write_all(&frame).await?;
            writer.flush().await?;
            Ok::<(), ClientError>(())
        };
        match timeout(self.timeouts.request, write).await {
            Ok(result) => result,
            Err(_) => Err(ClientError::Timeout { operation: "send" }),
        }
    }

    /// Read one message, failing with [`ClientError::Timeout`] if no complete
    /// frame arrives within `duration`. `operation` names the stalled step
    /// (`"hello"`, `"request"`, or `"update"`) in the error.
    ///
    /// `AsyncReadExt::read` is cancellation safe, so timing out a pending
    /// read never consumes buffered socket data.
    async fn read_message_with_timeout(
        &mut self,
        duration: Duration,
        operation: &'static str,
    ) -> Result<Value, ClientError> {
        match timeout(duration, self.read_message()).await {
            Ok(result) => result,
            Err(_) => Err(ClientError::Timeout { operation }),
        }
    }

    async fn read_message(&mut self) -> Result<Value, ClientError> {
        if let Some(value) = self.pending_frames.pop_front() {
            return Ok(value);
        }
        let mut buffer = [0_u8; 8192];
        loop {
            let reader = self.reader.as_mut().ok_or(ClientError::Disconnected)?;
            let read = reader.read(&mut buffer).await?;
            if read == 0 {
                // The peer closed the connection: report the broken transport
                // as a typed `Disconnected` transition (once) before failing.
                let end = self.decoder.end();
                if self.state != ConnectionState::Disconnected {
                    self.emit_state(ConnectionState::Disconnected, None, None);
                }
                end?;
                return Err(ClientError::Disconnected);
            }
            let messages = self.decoder.push(&buffer[..read])?;
            if messages.is_empty() {
                continue;
            }
            let mut queue = messages.into_iter();
            let Some(first) = queue.next() else {
                continue;
            };
            self.pending_frames.extend(queue);
            return Ok(first);
        }
    }
}

/// Decode one `service_update` payload into typed operations, accepting both
/// pir's `{type:"state", member, sequence, ops}` update and an upstream chord
/// `{t:"state", hold, m, as, value?, ops?}` frame. Delta ops in either shape
/// are returned as [`crate::protocol::DeltaOp`].
pub fn decode_service_update(update: &Value) -> Result<ServiceUpdate, ClientError> {
    parse_service_state_update(update)
        .map_err(|error| ClientError::InvalidResponse(error.to_string()))
}

/// Build a `ClientError::Server` from one protocol error value. A missing or
/// malformed error frame falls back to the server's `internal_error` code
/// vocabulary; `default_message` covers a missing/non-string `message`.
fn server_error(error: Option<&Value>, default_message: &str) -> ClientError {
    let part = |name: &str| {
        error
            .and_then(Value::as_object)
            .and_then(|obj| field_opt(obj, name))
            .and_then(Value::as_str)
    };
    ClientError::Server {
        code: part("code").unwrap_or("internal_error").to_owned(),
        message: part("message").unwrap_or(default_message).to_owned(),
    }
}

/// Turn one `response` frame into the call's result or typed server error.
fn response_outcome(value: &Value) -> Result<Value, ClientError> {
    if value.get("ok").and_then(Value::as_bool) == Some(true) {
        return Ok(value.get("result").cloned().unwrap_or(Value::Null));
    }
    let error = value
        .get("error")
        .ok_or_else(|| ClientError::InvalidResponse("failed response lacks error".to_owned()))?;
    Err(server_error(Some(error), "server request failed"))
}

/// Errors from connection, framing, schema, and server request handling.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("protocol error: {0}")]
    Protocol(ProtocolError),
    #[error("invalid server response: {0}")]
    InvalidResponse(String),
    #[error("server returned {code}: {message}")]
    Server { code: String, message: String },
    #[error("server disconnected")]
    Disconnected,
    /// Reconnect was requested on a client built from an existing stream
    /// (`from_stream_*`), so no server address is known to dial again.
    #[error("reconnect unavailable: client has no stored server address")]
    ReconnectUnavailable,
    /// The [`MAX_PENDING_UPDATES`] bound dropped buffered updates for
    /// `subscription_id`; `dropped` counts them since the previous report.
    ///
    /// This is the typed gap marker for a progress stream that was truncated
    /// while the consumer was not reading. It is NOT repairable with a
    /// protocol snapshot: a v8 `SessionSnapshot` carries an empty `transcript`
    /// (transcript items are incremental `progress` updates), so a resync only
    /// restores lifecycle/steer state. The consumer must reload the lost
    /// transcript from its own source (or restart the session) to recover it.
    #[error(
        "subscription update buffer overflowed for {subscription_id}: {dropped} update(s) lost; a snapshot resync cannot restore the lost transcript progress"
    )]
    UpdateBufferOverflow {
        /// Subscription whose buffered update stream lost frames.
        subscription_id: String,
        /// Number of dropped updates reported by this overflow episode.
        dropped: u64,
    },
    /// A response for `id` was evicted by the [`MAX_PENDING_RESPONSES`]
    /// buffer bound before its wait started.
    #[error("response buffer overflowed: the response for request {id} was dropped")]
    ResponseBufferOverflow {
        /// Request id whose terminal frame is gone.
        id: String,
    },
    /// A client operation exceeded its configured timeout.
    #[error("client operation timed out: {operation}")]
    Timeout {
        /// Stalled operation: `connect`, `hello`, `request`, `update`, or
        /// `send`.
        operation: &'static str,
    },
}

impl From<ProtocolError> for ClientError {
    fn from(error: ProtocolError) -> Self {
        Self::Protocol(error)
    }
}

impl From<crate::protocol::framing::FrameError> for ClientError {
    fn from(error: crate::protocol::framing::FrameError) -> Self {
        Self::Protocol(ProtocolError::Framing(error))
    }
}

impl From<serde_json::Error> for ClientError {
    fn from(error: serde_json::Error) -> Self {
        Self::InvalidResponse(format!("serialization failed: {error}"))
    }
}
