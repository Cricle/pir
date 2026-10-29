//! Per-connection serving: greeting, framing, and the inbound message loop.
//!
//! Resource bounds applied here:
//!
//! * **Idle timeout** — every read wait after the handshake is bounded by
//!   [`ServerSecurityConfig::idle_timeout`] (reset on activity). A client
//!   that stops reading or writing is reaped instead of holding its
//!   connection slot (slowloris).
//! * **Handshake deadline** — the whole pre-greeting phase (auth preface +
//!   hello) is additionally bounded by [`DEFAULT_HANDSHAKE_TIMEOUT`],
//!   mirroring upstream `DEFAULT_HANDSHAKE_TIMEOUT_MS` (server.ts:42): a
//!   connection that never completes the handshake is closed with the
//!   `hello_error{invalid_request, "Handshake timeout"}` path instead of
//!   holding its slot for the idle timeout on every read (slowloris).
//! * **Bounded frame channel** — outgoing frames flow through a channel of
//!   [`FRAME_CHANNEL_CAPACITY`] slots. Reader-driven sends use `try_send`
//!   and a full channel closes the connection (disconnect on full); the
//!   broadcast forwarder awaits capacity (backpressure). A non-reading
//!   client therefore cannot grow server memory without bound, and the
//!   writer's own writes are bounded by the same idle timeout.
//! * **Undeliverable deferred responses** — a response produced outside the
//!   read loop (the deferred prompt response) that cannot be enqueued
//!   because the channel is full disconnects the connection via the
//!   per-connection shutdown signal rather than silently losing it.

use std::collections::HashMap;
use std::io;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{broadcast, mpsc, RwLock};
use uuid::Uuid;

use crate::protocol::schemas::ProtocolError;
use crate::protocol::{
    encode_cbor, encode_frame, encode_server_envelope, CborOptions, ClientMessageDecoder,
    FrameOptions, ProtocolError as CodecProtocolError, RpcTarget, ServerEnvelope, ServerHelloError,
    SESSION_SERVICE_ID,
};

use super::dispatch::{handle_cancel, handle_request};
use super::handshake::{authenticate, handle_hello};
use super::policy::{ServerSecurityConfig, DEFAULT_HANDSHAKE_TIMEOUT};
use super::session::ServerState;
use super::ServerError;

const SERVICE_UPDATE_TYPE: &str = "service_update";
const CANCEL_TYPE: &str = "cancel";
const HELLO_ERROR_TYPE: &str = "hello_error";

/// Slot count of the per-connection outbound frame channel. Bounded so a
/// client that stops reading cannot grow server memory without bound; the
/// reader-side send path disconnects when the channel is full and the
/// broadcast forwarder applies backpressure instead.
pub(super) const FRAME_CHANNEL_CAPACITY: usize = 128;

/// Serve one byte stream (TCP or a Unix-domain socket; both are plain
/// AsyncRead+AsyncWrite) through the greeting/framing/message loop. `S` is
/// `'static` because the detached writer task owns one half of the split.
pub(super) async fn serve_connection<S: AsyncRead + AsyncWrite + Unpin + Send + 'static>(
    stream: S,
    state: ServerState,
    active: Arc<RwLock<HashMap<String, ()>>>,
    options: FrameOptions,
    security: ServerSecurityConfig,
) -> Result<(), ServerError> {
    let connection_id = Uuid::new_v4().to_string();
    {
        let mut connections = active.write().await;
        if connections.len() >= security.max_connections {
            return Err(ServerError::ConnectionLimit);
        }
        connections.insert(connection_id.clone(), ());
    }
    let result = serve_connection_inner(stream, state, options, &connection_id, &security).await;
    // serve_connection_inner already released the attachment leases and
    // aborted any active runs via `disconnect`; only the connection-slot
    // bookkeeping remains here.
    active.write().await.remove(&connection_id);
    result
}

pub(super) type FrameSender = mpsc::Sender<Vec<u8>>;
pub(super) type Subscriptions = Arc<RwLock<HashMap<String, String>>>;
/// Requests that are still cancellable, keyed by request id (upstream
/// `ConnectionState.activeRequests`): the in-flight deferred prompt runs,
/// carrying the routed target of their request. A `cancel` frame matches an
/// entry by id + identical target (upstream `sameTarget`); the entry is
/// dropped when the run settles, so a stale id can never cancel a later run.
pub(super) type ActiveRequests = Arc<RwLock<HashMap<String, RpcTarget>>>;

/// Per-connection routing context shared by the message loop and handlers.
pub(super) struct ConnectionCtx<'a> {
    pub(super) state: &'a ServerState,
    pub(super) frame_tx: &'a FrameSender,
    pub(super) subscriptions: &'a Subscriptions,
    /// The connection's still-cancellable requests (in-flight prompt runs),
    /// keyed by request id: a `cancel` frame naming one of these ids aborts
    /// exactly that run (upstream `ConnectionState.activeRequests`).
    pub(super) active: &'a ActiveRequests,
    pub(super) options: FrameOptions,
    pub(super) connection_id: &'a str,
    pub(super) security: &'a ServerSecurityConfig,
    /// One-shot disconnect signal: a handler that cannot deliver a response
    /// (deferred prompt on a full frame channel) notifies this to close the
    /// connection instead of silently losing the response.
    pub(super) shutdown: &'a std::sync::Arc<tokio::sync::Notify>,
}

async fn serve_connection_inner<S: AsyncRead + AsyncWrite + Unpin + Send + 'static>(
    stream: S,
    state: ServerState,
    options: FrameOptions,
    connection_id: &str,
    security: &ServerSecurityConfig,
) -> Result<(), ServerError> {
    let (mut reader, mut writer) = tokio::io::split(stream);
    // The whole pre-greeting phase (auth preface + hello) is bounded by one
    // deadline measured from connection start — upstream arms its handshake
    // timer at accept (server.ts:147-152) and clears it once the hello
    // response is sent. After the greeting the per-read idle timeout governs.
    let handshake_deadline = tokio::time::Instant::now() + DEFAULT_HANDSHAKE_TIMEOUT;
    if let Some(token) = security.auth_token.as_deref() {
        authenticate(&mut reader, token).await?;
    }
    let mut decoder = ClientMessageDecoder::new(options)?;
    let (frame_tx, mut frame_rx) = mpsc::channel::<Vec<u8>>(FRAME_CHANNEL_CAPACITY);
    let subscriptions: Subscriptions = Arc::new(RwLock::new(HashMap::new()));
    let active_requests: ActiveRequests = Arc::new(RwLock::new(HashMap::new()));
    let idle_timeout = security.idle_timeout;
    let shutdown = Arc::new(tokio::sync::Notify::new());

    // Writer task: serializes every outgoing frame through one channel so
    // responses, attachment updates, and service updates keep their order.
    // Each write is bounded by the idle timeout so a peer that stops reading
    // cannot pin the task (and its queued frames) forever. The task is
    // DETACHED (never awaited): it drains the channel until the last sender
    // drops, and teardown must not block on it (see the teardown comment).
    tokio::spawn(async move {
        while let Some(frame) = frame_rx.recv().await {
            let write = async {
                writer.write_all(&frame).await?;
                writer.flush().await?;
                Ok::<(), io::Error>(())
            };
            if !matches!(tokio::time::timeout(idle_timeout, write).await, Ok(Ok(()))) {
                break;
            }
        }
    });
    // Service update forwarder: broadcasts session state changes to this
    // connection's pir.session subscriptions as service_update frames. The
    // bounded channel applies backpressure here (send().await) instead of
    // letting a non-reading client buffer frames without limit.
    //
    // The subscription map is only SNAPSHOT under its read lock; the lock is
    // released before the first frame send, so a slow subscriber blocked on
    // the full frame channel can never block the connection's control plane
    // (`subscribe`/`unsubscribe` take the write lock on the same map).
    let mut updates_rx = state.updates.subscribe();
    let forwarder = {
        let frame_tx = frame_tx.clone();
        let subscriptions = subscriptions.clone();
        let state = state.clone();
        tokio::spawn(async move {
            loop {
                match updates_rx.recv().await {
                    Ok(event) => {
                        if !forward_service_update(&subscriptions, &frame_tx, &event, options).await
                        {
                            return;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        // Progress updates are incremental deltas and the
                        // terminal state rides the same channel. Skipping the
                        // lost updates leaves this subscriber's transcript
                        // progress permanently gapped: the snapshot transcript
                        // is empty, so neither a later delta nor a snapshot can
                        // reconstruct the missed transcript spans. The FULL
                        // snapshot resend below therefore restores only the
                        // snapshot-level state (phase/revision/queued steer)
                        // and keeps subsequent snapshot updates coherent; the
                        // transcript gap itself must be reloaded by the
                        // consumer from its own source. Drain whatever the ring
                        // still holds and resend once more: a snapshot taken
                        // while the forwarder was blocked can otherwise be
                        // older than the last updates queued behind it, which
                        // is exactly the "lagged receiver" case (the final
                        // snapshot must name the current revision).
                        tracing::warn!(
                            skipped,
                            "service update receiver lagged; resending session snapshots"
                        );
                        // Discard the stale delta backlog (the snapshots below
                        // supersede every one of them) and snapshot only after
                        // the ring is momentarily empty. Forwarding the drained
                        // deltas first would queue them ahead of the snapshot
                        // and could leave the client's newest snapshot older
                        // than the final revision. The drain is bounded so a
                        // continuously updating session cannot pin this
                        // forwarder forever.
                        let mut discarded = 0usize;
                        loop {
                            let mut saw_lag = false;
                            loop {
                                match updates_rx.try_recv() {
                                    Ok(_) => {
                                        discarded += 1;
                                        if discarded >= 100_000 {
                                            break;
                                        }
                                    }
                                    Err(broadcast::error::TryRecvError::Lagged(_)) => {
                                        saw_lag = true;
                                        continue;
                                    }
                                    Err(broadcast::error::TryRecvError::Empty) => break,
                                    Err(broadcast::error::TryRecvError::Closed) => return,
                                }
                            }
                            for update in state.snapshot_updates().await {
                                if !forward_service_update(
                                    &subscriptions,
                                    &frame_tx,
                                    &update,
                                    options,
                                )
                                .await
                                {
                                    return;
                                }
                            }
                            if !saw_lag || discarded >= 100_000 {
                                break;
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        })
    };

    let ctx = ConnectionCtx {
        state: &state,
        frame_tx: &frame_tx,
        subscriptions: &subscriptions,
        active: &active_requests,
        options,
        connection_id,
        security,
        shutdown: &shutdown,
    };
    let result = serve_connection_loop(&mut reader, &mut decoder, handshake_deadline, &ctx).await;

    // Tear down the background tasks and release the connection's state.
    // The disconnect runs BEFORE the teardown: the deferred prompt response
    // spawn holds a frame-sender clone until its run settles, so blocking on
    // the writer here would block on the run — while the disconnect (which
    // aborts the run) could not run yet. A mid-run disconnect wedged the
    // whole serve loop that way; disconnecting first aborts the run, the run
    // settles, and the writer drains on its own. The writer task is
    // therefore detached on purpose (teardown must never depend on run
    // behavior); dropping the last sender closes the channel once every
    // pending response has been handed to it.
    forwarder.abort();
    drop(frame_tx);
    state.disconnect(connection_id).await;
    result
}

/// Fan one broadcast update out to this connection's `pir.session`
/// subscriptions.
///
/// The map is read only to snapshot the matching `(subscription_id,
/// service_id)` pairs; the read guard is dropped before any `send().await`,
/// so backpressure on a slow subscriber's frame channel never extends into the
/// subscription write lock (`subscribe` / `unsubscribe` take it on the read
/// loop).
///
/// The update body is encoded to CBOR ONCE per broadcast and spliced into each
/// per-subscription envelope, instead of cloning the `Value` into a fresh
/// `ServiceUpdateFrame` per subscription and re-encoding it. Returns `false`
/// when the connection's frame channel is closed (the forwarder stops).
async fn forward_service_update(
    subscriptions: &Subscriptions,
    frame_tx: &FrameSender,
    event: &Value,
    options: FrameOptions,
) -> bool {
    let targets: Vec<(String, String)> = {
        let subs = subscriptions.read().await;
        subs.iter()
            .filter(|(_, service_id)| service_id.as_str() == SESSION_SERVICE_ID)
            .map(|(subscription_id, service_id)| (subscription_id.clone(), service_id.clone()))
            .collect()
    };
    if targets.is_empty() {
        return true;
    }
    // Same limits the protocol encoder derives from `FrameOptions`; the
    // update value sits one level below the envelope map, so the depth budget
    // is reduced by that outer level.
    let cbor_options = CborOptions {
        max_byte_length: options.max_frame_length,
        max_depth: CborOptions::default().max_depth.saturating_sub(1),
        ..CborOptions::default()
    };
    let Ok(update_cbor) = encode_cbor(event, cbor_options) else {
        // An unencodable update is dropped exactly like the old
        // `encode_server_envelope(..).ok()` path.
        return true;
    };
    for (subscription_id, _service_id) in &targets {
        let Ok(frame) = encode_service_update_frame(subscription_id, &update_cbor, options) else {
            continue;
        };
        if frame_tx.send(frame).await.is_err() {
            return false;
        }
    }
    true
}

/// Encode one `service_update` envelope around an already-encoded update body.
///
/// Byte-identical to [`encode_server_envelope`] for
/// `ServerEnvelope::ServiceUpdate(ServiceUpdateFrame { type_: "service_update",
/// subscription_id, update })`: the protocol's serde shape is the three-key
/// CBOR map `{"type", "subscriptionId", "update"}` in declaration order
/// (`serde_json` is built with `preserve_order`), which is what this writes.
/// The expensive part — the update body — is shared by every subscription of
/// the broadcast; only the small outer map differs.
fn encode_service_update_frame(
    subscription_id: &str,
    update_cbor: &[u8],
    options: FrameOptions,
) -> Result<Vec<u8>, CodecProtocolError> {
    let mut payload = Vec::with_capacity(update_cbor.len() + subscription_id.len() + 64);
    write_cbor_head(&mut payload, 5, 3);
    write_cbor_text(&mut payload, "type");
    write_cbor_text(&mut payload, SERVICE_UPDATE_TYPE);
    write_cbor_text(&mut payload, "subscriptionId");
    write_cbor_text(&mut payload, subscription_id);
    write_cbor_text(&mut payload, "update");
    payload.extend_from_slice(update_cbor);
    if payload.len() > options.max_frame_length {
        return Err(CodecProtocolError::Framing(
            crate::protocol::FrameError::Limit {
                length: payload.len(),
                limit: options.max_frame_length,
            },
        ));
    }
    Ok(encode_frame(&payload)?)
}

/// Definite-length CBOR item head (`major` + argument in the minimal width),
/// mirroring the protocol's encoder for the small outer map above.
fn write_cbor_head(out: &mut Vec<u8>, major: u8, argument: u64) {
    let head = major << 5;
    if argument <= 23 {
        out.push(head | argument as u8);
    } else if argument <= 0xff {
        out.push(head | 24);
        out.push(argument as u8);
    } else if argument <= 0xffff {
        out.push(head | 25);
        out.extend_from_slice(&(argument as u16).to_be_bytes());
    } else if argument <= 0xffff_ffff {
        out.push(head | 26);
        out.extend_from_slice(&(argument as u32).to_be_bytes());
    } else {
        out.push(head | 27);
        out.extend_from_slice(&argument.to_be_bytes());
    }
}

fn write_cbor_text(out: &mut Vec<u8>, text: &str) {
    write_cbor_head(out, 3, text.len() as u64);
    out.extend_from_slice(text.as_bytes());
}

async fn serve_connection_loop<R: AsyncRead + Unpin>(
    reader: &mut R,
    decoder: &mut ClientMessageDecoder,
    handshake_deadline: tokio::time::Instant,
    ctx: &ConnectionCtx<'_>,
) -> Result<(), ServerError> {
    let security = ctx.security;
    let idle_timeout: Duration = security.idle_timeout;
    let mut buffer = [0_u8; 8192];
    let mut greeted = false;
    let mut connection_bytes = 0_usize;
    loop {
        let remaining = security
            .max_connection_body_bytes
            .saturating_sub(connection_bytes);
        if remaining == 0 {
            return Err(ServerError::BodyLimit);
        }
        let read_len = buffer.len().min(remaining);
        // Every read wait is bounded by the idle timeout; activity resets it
        // because each loop iteration arms a fresh timer. Before the greeting
        // completes, the independent handshake deadline additionally caps the
        // WHOLE pre-greet phase (drip-fed reads cannot extend it): on expiry
        // the connection is closed through the existing handshake error path
        // with upstream's `invalid_request`/"Handshake timeout" hello_error
        // (server.ts:147-152). A handler-initiated shutdown (undeliverable
        // response) preempts the read immediately.
        let read = tokio::select! {
            biased;
            _ = ctx.shutdown.notified() => return Ok(()),
            _ = tokio::time::sleep_until(handshake_deadline), if !greeted => {
                send_envelope(
                    ctx.frame_tx,
                    &ServerEnvelope::HelloError(ServerHelloError {
                        type_: HELLO_ERROR_TYPE.to_owned(),
                        error: ProtocolError {
                            code: "invalid_request".to_owned(),
                            message: "Handshake timeout".to_owned(),
                            details: None,
                        },
                    }),
                    ctx.options,
                )?;
                return Ok(());
            }
            read = tokio::time::timeout(idle_timeout, reader.read(&mut buffer[..read_len])) => {
                match read {
                    Ok(result) => result?,
                    Err(_) => return Err(ServerError::IdleTimeout),
                }
            }
        };
        connection_bytes = connection_bytes.saturating_add(read);
        if read == 0 {
            decoder.end()?;
            return Ok(());
        }
        let messages = decoder.push(&buffer[..read])?;
        for value in messages {
            let message_type = value.get("type").and_then(Value::as_str);
            if !greeted {
                if !handle_hello(ctx, &value).await? {
                    return Ok(());
                }
                greeted = true;
                continue;
            }

            match message_type {
                Some("request") => handle_request(ctx, &value).await?,
                Some(CANCEL_TYPE) => handle_cancel(ctx, &value).await?,
                _ => continue,
            }
        }
    }
}

pub(super) fn send_envelope(
    frame_tx: &FrameSender,
    envelope: &ServerEnvelope,
    options: FrameOptions,
) -> Result<(), ServerError> {
    let frame = encode_server_envelope(envelope, options)?;
    // Disconnect on full: blocking the read loop on a non-reading client
    // would let one slow peer stall request handling entirely.
    match frame_tx.try_send(frame) {
        Ok(()) => Ok(()),
        Err(mpsc::error::TrySendError::Full(_)) => Err(ServerError::FrameChannelFull),
        Err(mpsc::error::TrySendError::Closed(_)) => {
            Err(ServerError::Io(io::Error::from(io::ErrorKind::BrokenPipe)))
        }
    }
}
