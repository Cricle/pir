//! G4 (P1-4): client reconnect, typed connection-state callbacks, timeout
//! cooperation, and best-effort session-state restoration.
//!
//! Two fixtures are used:
//! - a scripted raw listener that handshakes every accepted connection and
//!   answers each `request` frame with a canned ok response (full control
//!   over what survives a disconnect), and
//! - the real `Server`, to pin the reconnect behavior under the current
//!   server semantics (sessions owned by a disconnected connection are
//!   removed server-side, so their attach state cannot be restored).

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pir::client::{
    Client, ClientError, ClientTimeouts, ConnectionState, ConnectionStateChange, ReconnectPolicy,
    Subscription, MAX_PENDING_UPDATES, MAX_PENDING_UPDATE_SUBSCRIPTION_ID_BYTES,
    MAX_SNAPSHOT_SESSION_KEY_BYTES, MAX_TRACKED_SNAPSHOT_SESSIONS,
};
use pir::protocol::{
    encode_server_envelope, ClientMessageDecoder, Command, FrameOptions, ResponseFrame, RpcTarget,
    ServerEnvelope, ServerHelloV8, ServiceUpdateFrame, PROTOCOL_VERSION, SESSION_SERVICE_ID,
};
use pir::server::{Server, ServerSecurityConfig};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Bounds small enough to keep the suite fast and stable.
fn short_timeouts() -> ClientTimeouts {
    ClientTimeouts {
        connect: Duration::from_millis(500),
        request: Duration::from_millis(500),
        update: Duration::from_millis(250),
    }
}

type EventLog = Arc<Mutex<Vec<(ConnectionState, Option<u32>)>>>;

/// Collect `(state, attempt)` pairs from the typed connection-state events.
fn recorder() -> (
    EventLog,
    impl Fn(&ConnectionStateChange) + Send + Sync + 'static,
) {
    let log: EventLog = Arc::new(Mutex::new(Vec::new()));
    let sink = log.clone();
    let listener = move |change: &ConnectionStateChange| {
        sink.lock().unwrap().push((change.state, change.attempt));
    };
    (log, listener)
}

/// Scripted peer: completes the hello handshake on every accepted connection
/// and answers each `request` frame with a canned ok response (`attach`
/// results carry a fresh `attachmentId`, `list` an empty session array).
/// One connection is served at a time; a client-side drop (EOF) ends it and
/// the next connection is accepted, which is exactly the reconnect flow.
async fn scripted_accept_loop(listener: TcpListener, server_id: String) {
    loop {
        let Ok((mut stream, _)) = listener.accept().await else {
            return;
        };
        let Ok(mut decoder) = ClientMessageDecoder::new(FrameOptions::default()) else {
            return;
        };
        let hello = ServerEnvelope::Hello(ServerHelloV8 {
            type_: "hello".to_owned(),
            version: PROTOCOL_VERSION,
            server_id: server_id.clone(),
        });
        let Ok(frame) = encode_server_envelope(&hello, FrameOptions::default()) else {
            return;
        };
        if stream.write_all(&frame).await.is_err() {
            return;
        }
        let mut buffer = [0_u8; 8192];
        loop {
            let Ok(read) = stream.read(&mut buffer).await else {
                return;
            };
            if read == 0 {
                // The client dropped the transport; accept the next one.
                break;
            }
            let Ok(messages) = decoder.push(&buffer[..read]) else {
                return;
            };
            for value in messages {
                if value.get("type").and_then(|t| t.as_str()) != Some("request") {
                    continue;
                }
                let Some(id) = value.get("id").and_then(|v| v.as_str()) else {
                    continue;
                };
                let member = value
                    .get("call")
                    .and_then(|call| call.get("member"))
                    .and_then(|m| m.as_str())
                    .unwrap_or_default()
                    .to_owned();
                let result = match member.as_str() {
                    "attach" => serde_json::json!({
                        "command": "attach",
                        "attachmentId": "att-restored",
                    }),
                    "list" => serde_json::json!({"sessions": []}),
                    _ => serde_json::json!({}),
                };
                let response = ServerEnvelope::Response(ResponseFrame {
                    type_: "response".to_owned(),
                    id: id.to_owned(),
                    ok: true,
                    result: Some(result),
                    error: None,
                });
                let Ok(frame) = encode_server_envelope(&response, FrameOptions::default()) else {
                    return;
                };
                if stream.write_all(&frame).await.is_err() {
                    return;
                }
            }
        }
    }
}

/// Reconnect re-runs the versioned hello handshake, restores the
/// pre-disconnect attach (with the fresh attachment id from the re-attach),
/// re-issues the active subscriptions, and emits the typed transition
/// sequence `Disconnected -> Reconnecting(1) -> Connected`.
#[tokio::test]
async fn reconnect_restores_attach_and_subscriptions_and_emits_events() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fixture listener");
    let address = listener.local_addr().expect("fixture address");
    let fixture = tokio::spawn(scripted_accept_loop(
        listener,
        uuid::Uuid::new_v4().to_string(),
    ));

    let mut client = Client::connect_with_timeouts(address, short_timeouts())
        .await
        .expect("initial handshake");
    client
        .request(Command::Attach {
            session_id: "s-1".to_owned(),
        })
        .await
        .expect("attach against the scripted fixture");
    client
        .subscribe(SESSION_SERVICE_ID)
        .await
        .expect("subscribe");
    assert_eq!(client.attached_session(), Some("s-1"));
    assert_eq!(client.connection_state(), ConnectionState::Connected);

    let (log, listener_callback) = recorder();
    client.on_connection_state_change(listener_callback);

    let report = client.reconnect().await.expect("reconnect");
    assert_eq!(report.attempts, 1, "the first attempt must succeed");
    assert_eq!(report.restored_session.as_deref(), Some("s-1"));
    assert_eq!(report.restored_subscriptions, 1);
    assert_eq!(client.attached_session(), Some("s-1"));
    assert_eq!(client.connection_state(), ConnectionState::Connected);
    assert_eq!(client.subscriptions().len(), 1);
    // The attachment lease is the fresh one leased by the re-attach.
    assert_eq!(client.attachment_id(), Some("att-restored"));
    // The re-established connection serves normal RPC.
    let sessions = client.list().await.expect("list after reconnect");
    assert!(sessions.is_empty());
    // The initial connect happened before the listener was registered, so
    // the first observed event is the deliberate teardown of the old
    // transport, followed by the (successful) reconnect attempt.
    let events = log.lock().unwrap().clone();
    assert_eq!(
        events,
        vec![
            (ConnectionState::Disconnected, None),
            (ConnectionState::Reconnecting, Some(1)),
            (ConnectionState::Connected, None),
        ]
    );
    fixture.abort();
}

/// Under the current server semantics a session owned by the disconnected
/// connection is removed server-side, so the client drops the stale attach
/// state (best-effort restore, surfaced in the report) while the connection
/// itself and the subscriptions are restored.
#[tokio::test]
async fn reconnect_drops_attach_state_when_the_server_removed_the_session() {
    let server =
        Server::bind_with_security("127.0.0.1:0", ServerSecurityConfig::unauthenticated_local())
            .await
            .expect("bind server");
    let address = server.local_addr().expect("server address");
    let accept_loop = tokio::spawn(async move {
        loop {
            if server.accept_once().await.is_err() {
                break;
            }
        }
    });

    let mut client = Client::connect_with_timeouts(address, short_timeouts())
        .await
        .expect("initial handshake");
    let created = client
        .request(Command::Create {
            cwd: None,
            name: Some("reconnect-owned".to_owned()),
            model: None,
            thinking_level: None,
        })
        .await
        .expect("create response");
    let session_id = created["session"]["id"].as_str().unwrap().to_owned();
    client
        .request(Command::Attach {
            session_id: session_id.clone(),
        })
        .await
        .expect("attach response");
    client
        .subscribe(SESSION_SERVICE_ID)
        .await
        .expect("subscribe");

    let report = client.reconnect().await.expect("reconnect");
    assert_eq!(report.attempts, 1);
    // The owned session was removed on disconnect; the attach state is
    // dropped instead of failing the reconnect.
    assert_eq!(report.restored_session, None);
    assert_eq!(client.attached_session(), None);
    // Subscriptions survive regardless of session ownership.
    assert_eq!(report.restored_subscriptions, 1);
    assert_eq!(client.subscriptions().len(), 1);
    let sessions = client.list().await.expect("list after reconnect");
    assert!(
        sessions.is_empty(),
        "the session owned by the disconnected connection must be gone"
    );
    accept_loop.abort();
}

/// A reconnect against a dead endpoint fails after the bounded attempts with
/// the backoff sleeps in between, ending in a `Disconnected` event; every
/// attempt is bounded by the same `ClientTimeouts` as the initial connect.
#[tokio::test]
async fn reconnect_fails_after_bounded_backoff_attempts() {
    let server =
        Server::bind_with_security("127.0.0.1:0", ServerSecurityConfig::unauthenticated_local())
            .await
            .expect("bind server");
    let address = server.local_addr().expect("server address");
    // `bind` only opens the listener; nothing accepts until `run()` /
    // `accept_once()`, so the initial handshake needs an acceptor spawned
    // first (every sibling test does this). Detached on purpose: the served
    // connection ends when the reconnect tears the transport down, and the
    // moved-in Server (and its listener) drop with it.
    let accepting = tokio::spawn(async move { server.accept_once().await });
    let mut client = Client::connect_with_timeouts(address, short_timeouts())
        .await
        .expect("initial handshake");
    // Detached on purpose (see above): dropping the JoinHandle leaves the
    // acceptor serving until the reconnect tears the transport down.
    drop(accepting);

    let (log, listener_callback) = recorder();
    client.on_connection_state_change(listener_callback);
    let policy = ReconnectPolicy {
        max_attempts: 3,
        initial_backoff: Duration::from_millis(10),
        ..Default::default()
    };
    let started = Instant::now();
    let error = client
        .reconnect_with_policy(policy)
        .await
        .expect_err("the endpoint is gone");
    // The endpoint is either already closed (connect refusal → Io) or its
    // acceptor is still draining the torn-down first connection while
    // attempt 1 dials (bounded hello timeout → Timeout); both are the
    // policy's bounded connect failure.
    assert!(
        matches!(error, ClientError::Io(_) | ClientError::Timeout { .. }),
        "expected a bounded connect failure, got {error:?}"
    );
    let elapsed = started.elapsed();
    assert!(
        elapsed >= Duration::from_millis(25),
        "the backoff sleeps (10ms + 20ms) must actually run, took {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(3),
        "bounded retries must stay bounded, took {elapsed:?}"
    );
    let events = log.lock().unwrap().clone();
    // No trailing Disconnected: the reconnect teardown already reported the
    // single Disconnected transition for the sequence, and a failed
    // reconnect lands its final state silently (surfaced through the
    // returned error) rather than repeating the event.
    assert_eq!(
        events,
        vec![
            (ConnectionState::Disconnected, None),
            (ConnectionState::Reconnecting, Some(1)),
            (ConnectionState::Reconnecting, Some(2)),
            (ConnectionState::Reconnecting, Some(3)),
        ]
    );
    assert_eq!(client.connection_state(), ConnectionState::Disconnected);
}

/// Clients built from an existing stream have no stored address; reconnect
/// fails with the typed `ReconnectUnavailable` error instead of guessing.
#[tokio::test]
async fn reconnect_is_unavailable_without_a_stored_address() {
    let server =
        Server::bind_with_security("127.0.0.1:0", ServerSecurityConfig::unauthenticated_local())
            .await
            .expect("bind server");
    let address = server.local_addr().expect("server address");
    let accepting = tokio::spawn(async move { server.accept_once().await });
    let stream = TcpStream::connect(address).await.expect("raw connect");
    let mut client = Client::from_stream_with_token(stream, None)
        .await
        .expect("handshake");
    let error = client.reconnect().await.expect_err("no stored address");
    assert!(matches!(error, ClientError::ReconnectUnavailable));
    drop(client);
    accepting
        .await
        .expect("accept task join")
        .expect("serve client");
}

/// A server-side close (EOF) is reported as a typed `Disconnected` event and
/// leaves the client in the `Disconnected` state; the next read surfaces
/// `ClientError::Disconnected`. A later reconnect can still recover against
/// the same address.
#[tokio::test]
async fn server_side_close_emits_a_disconnected_event() {
    let server = Server::bind_with_security(
        "127.0.0.1:0",
        ServerSecurityConfig {
            idle_timeout: Duration::from_millis(150),
            ..ServerSecurityConfig::unauthenticated_local()
        },
    )
    .await
    .expect("bind idle-timeout server");
    let address = server.local_addr().expect("server address");
    let accepting = tokio::spawn(async move { server.accept_once().await });

    let mut client = Client::connect_with_timeouts(address, short_timeouts())
        .await
        .expect("initial handshake");
    let subscription = client
        .subscribe(SESSION_SERVICE_ID)
        .await
        .expect("subscribe");
    let (log, listener_callback) = recorder();
    client.on_connection_state_change(listener_callback);

    // Wait past the idle deadline; the server reaps the silent connection.
    tokio::time::sleep(Duration::from_millis(400)).await;
    // `next_update` only reads, so the observed failure is the peer's EOF.
    let error = client
        .next_update(&subscription)
        .await
        .expect_err("the idle connection was reaped");
    assert!(
        matches!(error, ClientError::Disconnected),
        "expected the EOF, got {error:?}"
    );
    assert_eq!(client.connection_state(), ConnectionState::Disconnected);
    let events = log.lock().unwrap().clone();
    assert_eq!(events, vec![(ConnectionState::Disconnected, None)]);
    drop(client);
    accepting
        .await
        .expect("accept task join")
        .expect_err("the idle connection must be closed by the server");
}

// ---------------------------------------------------------------------------
// D3: abandoned requests retire their pending id
// ---------------------------------------------------------------------------

/// Scripted peer that completes the handshake and then NEVER answers a
/// request (the connection stays open). The client's own request timeout is
/// the only way the wait can end.
async fn silent_accept_loop(listener: TcpListener, server_id: String) {
    let Ok((mut stream, _)) = listener.accept().await else {
        return;
    };
    let Ok(mut decoder) = ClientMessageDecoder::new(FrameOptions::default()) else {
        return;
    };
    let hello = ServerEnvelope::Hello(ServerHelloV8 {
        type_: "hello".to_owned(),
        version: PROTOCOL_VERSION,
        server_id,
    });
    let Ok(frame) = encode_server_envelope(&hello, FrameOptions::default()) else {
        return;
    };
    if stream.write_all(&frame).await.is_err() {
        return;
    }
    let mut buffer = [0_u8; 8192];
    loop {
        match stream.read(&mut buffer).await {
            Ok(0) | Err(_) => return,
            Ok(read) => {
                // Decode only so the framing stays consumed; requests are
                // deliberately never answered.
                let _ = decoder.push(&buffer[..read]);
            }
        }
    }
}

#[tokio::test]
async fn client_request_timeout_retires_the_pending_request() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fixture listener");
    let address = listener.local_addr().expect("fixture address");
    let fixture = tokio::spawn(silent_accept_loop(
        listener,
        uuid::Uuid::new_v4().to_string(),
    ));

    let mut client = Client::connect_with_timeouts(address, short_timeouts())
        .await
        .expect("initial handshake");
    let target = RpcTarget::server(client.server_id());
    let error = client
        .call(target, SESSION_SERVICE_ID, "list", Vec::new())
        .await
        .expect_err("the silent peer never answers");
    assert!(
        matches!(
            error,
            ClientError::Timeout {
                operation: "request"
            }
        ),
        "expected the request timeout, got {error:?}"
    );
    assert_eq!(
        client.pending_requests_count(),
        0,
        "a timed-out request must not stay in the pending table"
    );
    fixture.abort();
}

#[tokio::test]
async fn client_cancel_of_a_timed_out_request_returns_at_once() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fixture listener");
    let address = listener.local_addr().expect("fixture address");
    let fixture = tokio::spawn(silent_accept_loop(
        listener,
        uuid::Uuid::new_v4().to_string(),
    ));

    let mut client = Client::connect_with_timeouts(address, short_timeouts())
        .await
        .expect("initial handshake");
    let target = RpcTarget::server(client.server_id());
    let id = client
        .start_call(target.clone(), SESSION_SERVICE_ID, "list", Vec::new())
        .await
        .expect("send without waiting");
    let timed_out = client
        .wait_call(&id)
        .await
        .expect_err("the silent peer never answers");
    assert!(
        matches!(timed_out, ClientError::Timeout { .. }),
        "expected the request timeout, got {timed_out:?}"
    );
    assert_eq!(
        client.pending_requests_count(),
        0,
        "the timed-out request must be retired before the cancel"
    );

    // The old behavior sent a cancel for the stale id and waited out ANOTHER
    // full request timeout (500ms here) for a response that cannot exist.
    let started = Instant::now();
    let outcome = client.cancel(&id, target).await;
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_millis(250),
        "cancel of a retired request must return immediately, took {elapsed:?}"
    );
    match outcome {
        Err(ClientError::Server { code, .. }) => assert_eq!(code, "cancelled"),
        other => panic!("expected the local cancelled outcome, got {other:?}"),
    }
    fixture.abort();
}

/// D3: dropping the FUTURE that waits for a response (an outer timeout or
/// `select!`) must retire the id just like the internal timeout path. A stale
/// id would make a later `cancel` send a frame and wait out another full
/// request timeout for a response that cannot exist.
#[tokio::test]
async fn dropping_a_waited_request_future_retires_the_pending_id() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fixture listener");
    let address = listener.local_addr().expect("fixture address");
    let fixture = tokio::spawn(silent_accept_loop(
        listener,
        uuid::Uuid::new_v4().to_string(),
    ));

    let mut client = Client::connect_with_timeouts(address, short_timeouts())
        .await
        .expect("initial handshake");
    let target = RpcTarget::server(client.server_id());
    let id = client
        .start_call(target.clone(), SESSION_SERVICE_ID, "list", Vec::new())
        .await
        .expect("send without waiting");

    // The internal request timeout is 500ms; the outer 50ms wins and drops
    // the wait future without giving `await_response` a chance to return.
    let mut wait = Box::pin(client.wait_call(&id));
    let dropped = tokio::time::timeout(Duration::from_millis(50), &mut wait).await;
    assert!(dropped.is_err(), "the silent peer never answers");
    drop(wait);
    assert_eq!(
        client.pending_requests_count(),
        0,
        "dropping the wait future must retire the pending id"
    );

    let started = Instant::now();
    let outcome = client.cancel(&id, target).await;
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_millis(250),
        "cancel of a dropped request must return immediately, took {elapsed:?}"
    );
    match outcome {
        Err(ClientError::Server { code, .. }) => assert_eq!(code, "cancelled"),
        other => panic!("expected the local cancelled outcome, got {other:?}"),
    }
    fixture.abort();
}

// ---------------------------------------------------------------------------
// D2: client-side snapshot sequence gaps
// ---------------------------------------------------------------------------

/// Scripted peer that answers `subscribe`, then pushes two snapshot updates
/// whose sequences SKIP a value (the observable symptom of a lagged server
/// broadcast), and answers a resync `attach` with a fresh snapshot.
async fn sequence_gap_accept_loop(listener: TcpListener, server_id: String) {
    let Ok((mut stream, _)) = listener.accept().await else {
        return;
    };
    let Ok(mut decoder) = ClientMessageDecoder::new(FrameOptions::default()) else {
        return;
    };
    let hello = ServerEnvelope::Hello(ServerHelloV8 {
        type_: "hello".to_owned(),
        version: PROTOCOL_VERSION,
        server_id,
    });
    let Ok(frame) = encode_server_envelope(&hello, FrameOptions::default()) else {
        return;
    };
    if stream.write_all(&frame).await.is_err() {
        return;
    }
    let mut buffer = [0_u8; 8192];
    loop {
        let Ok(read) = stream.read(&mut buffer).await else {
            return;
        };
        if read == 0 {
            return;
        }
        let Ok(messages) = decoder.push(&buffer[..read]) else {
            return;
        };
        for value in messages {
            if value.get("type").and_then(|t| t.as_str()) != Some("request") {
                continue;
            }
            let Some(id) = value.get("id").and_then(|v| v.as_str()) else {
                continue;
            };
            let member = value
                .get("call")
                .and_then(|call| call.get("member"))
                .and_then(|m| m.as_str())
                .unwrap_or_default();
            if member == "subscribe" {
                let subscription_id = value
                    .get("call")
                    .and_then(|call| call.get("args"))
                    .and_then(|args| args.as_array())
                    .and_then(|args| args.first())
                    .and_then(|arg| arg.as_str())
                    .unwrap_or("sub-1")
                    .to_owned();
                let response = ServerEnvelope::Response(ResponseFrame {
                    type_: "response".to_owned(),
                    id: id.to_owned(),
                    ok: true,
                    result: Some(serde_json::json!({
                        "serviceId": SESSION_SERVICE_ID,
                        "mode": "singleton",
                        "instances": [],
                    })),
                    error: None,
                });
                let Ok(frame) = encode_server_envelope(&response, FrameOptions::default()) else {
                    return;
                };
                if stream.write_all(&frame).await.is_err() {
                    return;
                }
                for sequence in [1_i64, 5] {
                    let update = serde_json::json!({
                        "type": "state",
                        "member": "snapshot",
                        "sequence": sequence,
                        "instance": {"key": "sess-gap", "generation": 1},
                        "ops": [["r", {"id": "sess-gap"}]],
                    });
                    let envelope = ServerEnvelope::ServiceUpdate(ServiceUpdateFrame {
                        type_: "service_update".to_owned(),
                        subscription_id: subscription_id.clone(),
                        update,
                    });
                    let Ok(frame) = encode_server_envelope(&envelope, FrameOptions::default())
                    else {
                        return;
                    };
                    if stream.write_all(&frame).await.is_err() {
                        return;
                    }
                }
                continue;
            }
            let result = if member == "attach" {
                serde_json::json!({
                    "command": "attach",
                    "attachmentId": "att-gap",
                    "session": {"id": "sess-gap", "name": null},
                })
            } else {
                serde_json::json!({})
            };
            let response = ServerEnvelope::Response(ResponseFrame {
                type_: "response".to_owned(),
                id: id.to_owned(),
                ok: true,
                result: Some(result),
                error: None,
            });
            let Ok(frame) = encode_server_envelope(&response, FrameOptions::default()) else {
                return;
            };
            if stream.write_all(&frame).await.is_err() {
                return;
            }
        }
    }
}

#[tokio::test]
async fn client_sequence_gap_requests_a_snapshot_resync() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fixture listener");
    let address = listener.local_addr().expect("fixture address");
    let fixture = tokio::spawn(sequence_gap_accept_loop(
        listener,
        uuid::Uuid::new_v4().to_string(),
    ));

    let mut client = Client::connect_with_timeouts(address, short_timeouts())
        .await
        .expect("initial handshake");
    let subscription = client
        .subscribe(SESSION_SERVICE_ID)
        .await
        .expect("subscribe");

    let first = client
        .next_update(&subscription)
        .await
        .expect("first snapshot");
    assert_eq!(first["sequence"], 1);
    assert!(
        !client.snapshot_sequence_gap("sess-gap"),
        "the first observed snapshot cannot be a gap"
    );

    let second = client
        .next_update(&subscription)
        .await
        .expect("second snapshot");
    assert_eq!(second["sequence"], 5);
    assert_eq!(client.snapshot_sequence("sess-gap"), Some(5));
    assert!(
        client.snapshot_sequence_gap("sess-gap"),
        "a skipped snapshot sequence must flag the session for a resync"
    );

    // Requesting a fresh snapshot reuses the existing attach call (no wire
    // version change) and clears the gap.
    let snapshot = client
        .resync_session_snapshot("sess-gap")
        .await
        .expect("resync snapshot");
    assert_eq!(snapshot["id"], "sess-gap");
    assert!(
        !client.snapshot_sequence_gap("sess-gap"),
        "a fetched snapshot clears the pending resync"
    );
    assert_eq!(client.attachment_id(), Some("att-gap"));
    assert_eq!(client.attached_session(), Some("sess-gap"));
    fixture.abort();
}

/// D2: a lower sequence is a RESTARTED stream (server restart, or a removed
/// session id created again), not a duplicate observation. The high-water
/// mark must reset so skips in the new stream are still detected.
#[test]
fn snapshot_tracker_treats_a_lower_sequence_as_a_restarted_stream() {
    use pir::client::SnapshotSequenceTracker;

    let snapshot = |sequence: i64| {
        serde_json::json!({
            "type": "state",
            "member": "snapshot",
            "sequence": sequence,
            "instance": {"key": "sess-restart", "generation": 1},
            "ops": [["r", {"id": "sess-restart"}]],
        })
    };
    let mut tracker = SnapshotSequenceTracker::new();
    assert!(!tracker.observe(&snapshot(10)));
    assert!(tracker.observe(&snapshot(12)));
    assert!(tracker.needs_resync("sess-restart"));

    assert!(
        !tracker.observe(&snapshot(1)),
        "the first snapshot of a restarted stream is not a gap"
    );
    assert!(!tracker.needs_resync("sess-restart"));
    assert_eq!(tracker.last_sequence("sess-restart"), Some(1));
    assert!(
        tracker.observe(&snapshot(3)),
        "skips after the restart must be detected again"
    );
}

/// C2-03: tracker keys come straight from the peer. A crafted snapshot stream
/// with unique `instance.key` values must not grow the tracker without bound:
/// entries past the cap are ignored and counted, and over-long keys are
/// rejected.
#[test]
fn snapshot_tracker_caps_tracked_sessions_and_counts_drops() {
    use pir::client::SnapshotSequenceTracker;

    let snapshot = |key: &str, sequence: i64| {
        serde_json::json!({
            "type": "state",
            "member": "snapshot",
            "sequence": sequence,
            "instance": {"key": key, "generation": 1},
            "ops": [],
        })
    };
    let mut tracker = SnapshotSequenceTracker::new();
    for index in 0..(MAX_TRACKED_SNAPSHOT_SESSIONS + 250) {
        assert!(
            !tracker.observe(&snapshot(&format!("forged-{index}"), 1)),
            "the first snapshot of each forged stream is not a gap"
        );
    }
    assert_eq!(
        tracker.tracked_sessions(),
        MAX_TRACKED_SNAPSHOT_SESSIONS,
        "the tracker must stop growing at the entry cap"
    );
    assert_eq!(
        tracker.dropped_observations(),
        250,
        "observations past the cap must be counted"
    );

    // An existing key still updates at the cap (no new entry, no drop).
    assert!(tracker.observe(&snapshot("forged-0", 3)));
    assert_eq!(tracker.last_sequence("forged-0"), Some(3));
    assert_eq!(tracker.dropped_observations(), 250);

    // Over-long keys are rejected and counted even when there is room.
    let mut fresh = SnapshotSequenceTracker::new();
    let long_key = "k".repeat(MAX_SNAPSHOT_SESSION_KEY_BYTES + 1);
    assert!(!fresh.observe(&snapshot(&long_key, 1)));
    assert_eq!(fresh.tracked_sessions(), 0);
    assert_eq!(fresh.dropped_observations(), 1);
}

/// Scripted peer for the resync purge: answers `subscribe`, then on `attach`
/// pushes stale snapshots (1, 5, 3) BEFORE the response whose `session`
/// revision is 6, and a fresh snapshot 7 after it.
async fn resync_purge_accept_loop(listener: TcpListener, server_id: String) {
    let Ok((mut stream, _)) = listener.accept().await else {
        return;
    };
    let Ok(mut decoder) = ClientMessageDecoder::new(FrameOptions::default()) else {
        return;
    };
    let hello = ServerEnvelope::Hello(ServerHelloV8 {
        type_: "hello".to_owned(),
        version: PROTOCOL_VERSION,
        server_id,
    });
    let Ok(frame) = encode_server_envelope(&hello, FrameOptions::default()) else {
        return;
    };
    if stream.write_all(&frame).await.is_err() {
        return;
    }
    let snapshot_update = |sequence: i64| {
        serde_json::json!({
            "type": "state",
            "member": "snapshot",
            "sequence": sequence,
            "instance": {"key": "sess-resync", "generation": 1},
            "ops": [["r", {"id": "sess-resync"}]],
        })
    };
    let mut subscription_id = String::from("sub-1");
    let mut buffer = [0_u8; 8192];
    loop {
        let Ok(read) = stream.read(&mut buffer).await else {
            return;
        };
        if read == 0 {
            return;
        }
        let Ok(messages) = decoder.push(&buffer[..read]) else {
            return;
        };
        for value in messages {
            if value.get("type").and_then(|t| t.as_str()) != Some("request") {
                continue;
            }
            let Some(id) = value.get("id").and_then(|v| v.as_str()) else {
                continue;
            };
            let member = value
                .get("call")
                .and_then(|call| call.get("member"))
                .and_then(|m| m.as_str())
                .unwrap_or_default()
                .to_owned();
            if member == "subscribe" {
                subscription_id = value
                    .get("call")
                    .and_then(|call| call.get("args"))
                    .and_then(|args| args.as_array())
                    .and_then(|args| args.first())
                    .and_then(|arg| arg.as_str())
                    .unwrap_or("sub-1")
                    .to_owned();
                let response = ServerEnvelope::Response(ResponseFrame {
                    type_: "response".to_owned(),
                    id: id.to_owned(),
                    ok: true,
                    result: Some(serde_json::json!({
                        "serviceId": SESSION_SERVICE_ID,
                        "mode": "singleton",
                        "instances": [],
                    })),
                    error: None,
                });
                let Ok(frame) = encode_server_envelope(&response, FrameOptions::default()) else {
                    return;
                };
                if stream.write_all(&frame).await.is_err() {
                    return;
                }
                continue;
            }
            if member == "attach" {
                // Stale full snapshots the attach response supersedes.
                for sequence in [1_i64, 5, 3] {
                    let envelope = ServerEnvelope::ServiceUpdate(ServiceUpdateFrame {
                        type_: "service_update".to_owned(),
                        subscription_id: subscription_id.clone(),
                        update: snapshot_update(sequence),
                    });
                    let Ok(frame) = encode_server_envelope(&envelope, FrameOptions::default())
                    else {
                        return;
                    };
                    if stream.write_all(&frame).await.is_err() {
                        return;
                    }
                }
                let response = ServerEnvelope::Response(ResponseFrame {
                    type_: "response".to_owned(),
                    id: id.to_owned(),
                    ok: true,
                    result: Some(serde_json::json!({
                        "command": "attach",
                        "attachmentId": "att-resync",
                        "session": {"id": "sess-resync", "revision": 6},
                    })),
                    error: None,
                });
                let Ok(frame) = encode_server_envelope(&response, FrameOptions::default()) else {
                    return;
                };
                if stream.write_all(&frame).await.is_err() {
                    return;
                }
                // The next real snapshot for the session.
                let envelope = ServerEnvelope::ServiceUpdate(ServiceUpdateFrame {
                    type_: "service_update".to_owned(),
                    subscription_id: "sub-1".to_owned(),
                    update: snapshot_update(7),
                });
                let Ok(frame) = encode_server_envelope(&envelope, FrameOptions::default()) else {
                    return;
                };
                if stream.write_all(&frame).await.is_err() {
                    return;
                }
                continue;
            }
            let response = ServerEnvelope::Response(ResponseFrame {
                type_: "response".to_owned(),
                id: id.to_owned(),
                ok: true,
                result: Some(serde_json::json!({})),
                error: None,
            });
            let Ok(frame) = encode_server_envelope(&response, FrameOptions::default()) else {
                return;
            };
            if stream.write_all(&frame).await.is_err() {
                return;
            }
        }
    }
}

/// D2: snapshots already queued behind the resync attach are superseded by
/// the response's full snapshot and must never be returned to the caller
/// afterwards (returning one would regress the consumer's state).
#[tokio::test]
async fn resync_drops_superseded_queued_snapshots() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fixture listener");
    let address = listener.local_addr().expect("fixture address");
    let fixture = tokio::spawn(resync_purge_accept_loop(
        listener,
        uuid::Uuid::new_v4().to_string(),
    ));

    let mut client = Client::connect_with_timeouts(address, short_timeouts())
        .await
        .expect("initial handshake");
    let subscription = client
        .subscribe(SESSION_SERVICE_ID)
        .await
        .expect("subscribe");
    let snapshot = client
        .resync_session_snapshot("sess-resync")
        .await
        .expect("resync snapshot");
    assert_eq!(snapshot["revision"], 6);
    assert_eq!(client.snapshot_sequence("sess-resync"), Some(6));
    assert!(!client.snapshot_sequence_gap("sess-resync"));

    let update = client
        .next_update(&subscription)
        .await
        .expect("fresh snapshot after the resync");
    assert_eq!(
        update["sequence"], 7,
        "queued snapshots at or below the resync revision must be dropped"
    );
    assert_eq!(client.snapshot_sequence("sess-resync"), Some(7));
    fixture.abort();
}

/// Scripted peer whose FIRST connection pushes a gap (snapshots 1 then 5),
/// and whose attach responses carry a fresh `session.revision` of 6. A second
/// connection only sees the attach/subscribe responses.
async fn reconnect_gap_accept_loop(listener: TcpListener, server_id: String) {
    let mut first_connection = true;
    loop {
        let Ok((mut stream, _)) = listener.accept().await else {
            return;
        };
        let Ok(mut decoder) = ClientMessageDecoder::new(FrameOptions::default()) else {
            return;
        };
        let hello = ServerEnvelope::Hello(ServerHelloV8 {
            type_: "hello".to_owned(),
            version: PROTOCOL_VERSION,
            server_id: server_id.clone(),
        });
        let Ok(frame) = encode_server_envelope(&hello, FrameOptions::default()) else {
            return;
        };
        if stream.write_all(&frame).await.is_err() {
            return;
        }
        let push_gap = std::mem::take(&mut first_connection);
        let snapshot_update = |sequence: i64| {
            serde_json::json!({
                "type": "state",
                "member": "snapshot",
                "sequence": sequence,
                "instance": {"key": "s-gap", "generation": 1},
                "ops": [["r", {"id": "s-gap"}]],
            })
        };
        let mut buffer = [0_u8; 8192];
        loop {
            let Ok(read) = stream.read(&mut buffer).await else {
                return;
            };
            if read == 0 {
                break;
            }
            let Ok(messages) = decoder.push(&buffer[..read]) else {
                return;
            };
            for value in messages {
                if value.get("type").and_then(|t| t.as_str()) != Some("request") {
                    continue;
                }
                let Some(id) = value.get("id").and_then(|v| v.as_str()) else {
                    continue;
                };
                let member = value
                    .get("call")
                    .and_then(|call| call.get("member"))
                    .and_then(|m| m.as_str())
                    .unwrap_or_default()
                    .to_owned();
                let result = match member.as_str() {
                    "attach" => serde_json::json!({
                        "command": "attach",
                        "attachmentId": "att-gap",
                        "session": {"id": "s-gap", "revision": 6},
                    }),
                    "subscribe" => {
                        let subscription_id = value
                            .get("call")
                            .and_then(|call| call.get("args"))
                            .and_then(|args| args.as_array())
                            .and_then(|args| args.first())
                            .and_then(|arg| arg.as_str())
                            .unwrap_or("sub-1")
                            .to_owned();
                        let response = ServerEnvelope::Response(ResponseFrame {
                            type_: "response".to_owned(),
                            id: id.to_owned(),
                            ok: true,
                            result: Some(serde_json::json!({
                                "serviceId": SESSION_SERVICE_ID,
                                "mode": "singleton",
                                "instances": [],
                            })),
                            error: None,
                        });
                        let Ok(frame) = encode_server_envelope(&response, FrameOptions::default())
                        else {
                            return;
                        };
                        if stream.write_all(&frame).await.is_err() {
                            return;
                        }
                        if push_gap {
                            for sequence in [1_i64, 5] {
                                let envelope = ServerEnvelope::ServiceUpdate(ServiceUpdateFrame {
                                    type_: "service_update".to_owned(),
                                    subscription_id: subscription_id.clone(),
                                    update: snapshot_update(sequence),
                                });
                                let Ok(frame) =
                                    encode_server_envelope(&envelope, FrameOptions::default())
                                else {
                                    return;
                                };
                                if stream.write_all(&frame).await.is_err() {
                                    return;
                                }
                            }
                        }
                        continue;
                    }
                    _ => serde_json::json!({}),
                };
                let response = ServerEnvelope::Response(ResponseFrame {
                    type_: "response".to_owned(),
                    id: id.to_owned(),
                    ok: true,
                    result: Some(result),
                    error: None,
                });
                let Ok(frame) = encode_server_envelope(&response, FrameOptions::default()) else {
                    return;
                };
                if stream.write_all(&frame).await.is_err() {
                    return;
                }
            }
        }
    }
}

/// D2: the reconnect's successful attach carries a full snapshot, so it must
/// clear a gap observed on the old connection (the consumer would otherwise
/// keep resyncing a session that is already fresh).
#[tokio::test]
async fn reconnect_attach_clears_a_snapshot_sequence_gap() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fixture listener");
    let address = listener.local_addr().expect("fixture address");
    let fixture = tokio::spawn(reconnect_gap_accept_loop(
        listener,
        uuid::Uuid::new_v4().to_string(),
    ));

    let mut client = Client::connect_with_timeouts(address, short_timeouts())
        .await
        .expect("initial handshake");
    client
        .request(Command::Attach {
            session_id: "s-gap".to_owned(),
        })
        .await
        .expect("attach");
    let subscription = client
        .subscribe(SESSION_SERVICE_ID)
        .await
        .expect("subscribe");
    let first = client
        .next_update(&subscription)
        .await
        .expect("first snapshot");
    assert_eq!(first["sequence"], 1);
    let second = client
        .next_update(&subscription)
        .await
        .expect("second snapshot");
    assert_eq!(second["sequence"], 5);
    assert!(
        client.snapshot_sequence_gap("s-gap"),
        "the skipped sequence must be flagged before the reconnect"
    );

    let report = client.reconnect().await.expect("reconnect");
    assert_eq!(report.restored_session.as_deref(), Some("s-gap"));
    assert!(
        !client.snapshot_sequence_gap("s-gap"),
        "the reconnect attach snapshot repairs the gap"
    );
    assert_eq!(client.snapshot_sequence("s-gap"), Some(6));
    fixture.abort();
}

/// Scripted peer that queues snapshots 1 and 5 behind a response: both are
/// observed (and the 1 → 5 skip is a gap) before `next_update` returns them.
async fn queued_gap_accept_loop(listener: TcpListener, server_id: String) {
    let Ok((mut stream, _)) = listener.accept().await else {
        return;
    };
    let Ok(mut decoder) = ClientMessageDecoder::new(FrameOptions::default()) else {
        return;
    };
    let hello = ServerEnvelope::Hello(ServerHelloV8 {
        type_: "hello".to_owned(),
        version: PROTOCOL_VERSION,
        server_id,
    });
    let Ok(frame) = encode_server_envelope(&hello, FrameOptions::default()) else {
        return;
    };
    if stream.write_all(&frame).await.is_err() {
        return;
    }
    let snapshot_update = |sequence: i64| {
        serde_json::json!({
            "type": "state",
            "member": "snapshot",
            "sequence": sequence,
            "instance": {"key": "sess-queued", "generation": 1},
            "ops": [["r", {"id": "sess-queued"}]],
        })
    };
    let mut subscription_id = String::from("sub-1");
    let mut buffer = [0_u8; 8192];
    loop {
        let Ok(read) = stream.read(&mut buffer).await else {
            return;
        };
        if read == 0 {
            return;
        }
        let Ok(messages) = decoder.push(&buffer[..read]) else {
            return;
        };
        for value in messages {
            if value.get("type").and_then(|t| t.as_str()) != Some("request") {
                continue;
            }
            let Some(id) = value.get("id").and_then(|v| v.as_str()) else {
                continue;
            };
            let member = value
                .get("call")
                .and_then(|call| call.get("member"))
                .and_then(|m| m.as_str())
                .unwrap_or_default()
                .to_owned();
            if member == "subscribe" {
                subscription_id = value
                    .get("call")
                    .and_then(|call| call.get("args"))
                    .and_then(|args| args.as_array())
                    .and_then(|args| args.first())
                    .and_then(|arg| arg.as_str())
                    .unwrap_or("sub-1")
                    .to_owned();
                let response = ServerEnvelope::Response(ResponseFrame {
                    type_: "response".to_owned(),
                    id: id.to_owned(),
                    ok: true,
                    result: Some(serde_json::json!({
                        "serviceId": SESSION_SERVICE_ID,
                        "mode": "singleton",
                        "instances": [],
                    })),
                    error: None,
                });
                let Ok(frame) = encode_server_envelope(&response, FrameOptions::default()) else {
                    return;
                };
                if stream.write_all(&frame).await.is_err() {
                    return;
                }
                continue;
            }
            // Queue two snapshots (a skipped sequence) BEHIND this response.
            for sequence in [1_i64, 5] {
                let envelope = ServerEnvelope::ServiceUpdate(ServiceUpdateFrame {
                    type_: "service_update".to_owned(),
                    subscription_id: subscription_id.clone(),
                    update: snapshot_update(sequence),
                });
                let Ok(frame) = encode_server_envelope(&envelope, FrameOptions::default()) else {
                    return;
                };
                if stream.write_all(&frame).await.is_err() {
                    return;
                }
            }
            let response = ServerEnvelope::Response(ResponseFrame {
                type_: "response".to_owned(),
                id: id.to_owned(),
                ok: true,
                result: Some(serde_json::json!({"sessions": []})),
                error: None,
            });
            let Ok(frame) = encode_server_envelope(&response, FrameOptions::default()) else {
                return;
            };
            if stream.write_all(&frame).await.is_err() {
                return;
            }
        }
    }
}

/// D2: snapshots queued behind a response are observed at arrival. Returning
/// the OLDER one first must not move the high-water mark backwards (that would
/// read as a restarted stream and clear the gap the arrival already proved).
#[tokio::test]
async fn replaying_a_queued_snapshot_keeps_the_high_water_mark_and_gap() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fixture listener");
    let address = listener.local_addr().expect("fixture address");
    let fixture = tokio::spawn(queued_gap_accept_loop(
        listener,
        uuid::Uuid::new_v4().to_string(),
    ));

    let mut client = Client::connect_with_timeouts(address, short_timeouts())
        .await
        .expect("initial handshake");
    let subscription = client
        .subscribe(SESSION_SERVICE_ID)
        .await
        .expect("subscribe");
    client
        .call(
            RpcTarget::server(client.server_id()),
            SESSION_SERVICE_ID,
            "list",
            Vec::new(),
        )
        .await
        .expect("probe call");
    assert_eq!(client.snapshot_sequence("sess-queued"), Some(5));
    assert!(
        client.snapshot_sequence_gap("sess-queued"),
        "the skipped sequence must be flagged while the snapshots are queued"
    );

    let first = client
        .next_update(&subscription)
        .await
        .expect("the older queued snapshot");
    assert_eq!(first["sequence"], 1);
    assert_eq!(
        client.snapshot_sequence("sess-queued"),
        Some(5),
        "replaying an older queued snapshot must not lower the high-water mark"
    );
    assert!(
        client.snapshot_sequence_gap("sess-queued"),
        "replaying an older queued snapshot must not clear the detected gap"
    );

    let second = client
        .next_update(&subscription)
        .await
        .expect("the newer queued snapshot");
    assert_eq!(second["sequence"], 5);
    assert_eq!(client.snapshot_sequence("sess-queued"), Some(5));
    assert!(client.snapshot_sequence_gap("sess-queued"));
    fixture.abort();
}

/// Scripted peer for the ahead-of-revision case: an attach pushes snapshot 7
/// BEFORE the response whose `session.revision` is 6, then snapshot 8 after it.
async fn resync_ahead_accept_loop(listener: TcpListener, server_id: String) {
    let Ok((mut stream, _)) = listener.accept().await else {
        return;
    };
    let Ok(mut decoder) = ClientMessageDecoder::new(FrameOptions::default()) else {
        return;
    };
    let hello = ServerEnvelope::Hello(ServerHelloV8 {
        type_: "hello".to_owned(),
        version: PROTOCOL_VERSION,
        server_id,
    });
    let Ok(frame) = encode_server_envelope(&hello, FrameOptions::default()) else {
        return;
    };
    if stream.write_all(&frame).await.is_err() {
        return;
    }
    let snapshot_update = |sequence: i64| {
        serde_json::json!({
            "type": "state",
            "member": "snapshot",
            "sequence": sequence,
            "instance": {"key": "sess-ahead", "generation": 1},
            "ops": [["r", {"id": "sess-ahead"}]],
        })
    };
    let mut subscription_id = String::from("sub-1");
    let mut buffer = [0_u8; 8192];
    loop {
        let Ok(read) = stream.read(&mut buffer).await else {
            return;
        };
        if read == 0 {
            return;
        }
        let Ok(messages) = decoder.push(&buffer[..read]) else {
            return;
        };
        for value in messages {
            if value.get("type").and_then(|t| t.as_str()) != Some("request") {
                continue;
            }
            let Some(id) = value.get("id").and_then(|v| v.as_str()) else {
                continue;
            };
            let member = value
                .get("call")
                .and_then(|call| call.get("member"))
                .and_then(|m| m.as_str())
                .unwrap_or_default()
                .to_owned();
            if member == "subscribe" {
                subscription_id = value
                    .get("call")
                    .and_then(|call| call.get("args"))
                    .and_then(|args| args.as_array())
                    .and_then(|args| args.first())
                    .and_then(|arg| arg.as_str())
                    .unwrap_or("sub-1")
                    .to_owned();
                let response = ServerEnvelope::Response(ResponseFrame {
                    type_: "response".to_owned(),
                    id: id.to_owned(),
                    ok: true,
                    result: Some(serde_json::json!({
                        "serviceId": SESSION_SERVICE_ID,
                        "mode": "singleton",
                        "instances": [],
                    })),
                    error: None,
                });
                let Ok(frame) = encode_server_envelope(&response, FrameOptions::default()) else {
                    return;
                };
                if stream.write_all(&frame).await.is_err() {
                    return;
                }
                continue;
            }
            if member == "attach" {
                // A snapshot NEWER than the response's revision was queued
                // first (broadcast while the attach was being answered).
                let ahead = ServerEnvelope::ServiceUpdate(ServiceUpdateFrame {
                    type_: "service_update".to_owned(),
                    subscription_id: subscription_id.clone(),
                    update: snapshot_update(7),
                });
                let Ok(frame) = encode_server_envelope(&ahead, FrameOptions::default()) else {
                    return;
                };
                if stream.write_all(&frame).await.is_err() {
                    return;
                }
                let response = ServerEnvelope::Response(ResponseFrame {
                    type_: "response".to_owned(),
                    id: id.to_owned(),
                    ok: true,
                    result: Some(serde_json::json!({
                        "command": "attach",
                        "attachmentId": "att-ahead",
                        "session": {"id": "sess-ahead", "revision": 6},
                    })),
                    error: None,
                });
                let Ok(frame) = encode_server_envelope(&response, FrameOptions::default()) else {
                    return;
                };
                if stream.write_all(&frame).await.is_err() {
                    return;
                }
                let next = ServerEnvelope::ServiceUpdate(ServiceUpdateFrame {
                    type_: "service_update".to_owned(),
                    subscription_id: subscription_id.clone(),
                    update: snapshot_update(8),
                });
                let Ok(frame) = encode_server_envelope(&next, FrameOptions::default()) else {
                    return;
                };
                if stream.write_all(&frame).await.is_err() {
                    return;
                }
                continue;
            }
            let response = ServerEnvelope::Response(ResponseFrame {
                type_: "response".to_owned(),
                id: id.to_owned(),
                ok: true,
                result: Some(serde_json::json!({})),
                error: None,
            });
            let Ok(frame) = encode_server_envelope(&response, FrameOptions::default()) else {
                return;
            };
            if stream.write_all(&frame).await.is_err() {
                return;
            }
        }
    }
}

/// D2: a snapshot queued AHEAD of an attach response's revision is not
/// superseded by that response. The tracker must adopt it as the high-water
/// mark, otherwise the following contiguous update would look like a gap.
#[tokio::test]
async fn resync_accounts_for_a_queued_snapshot_ahead_of_the_response() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fixture listener");
    let address = listener.local_addr().expect("fixture address");
    let fixture = tokio::spawn(resync_ahead_accept_loop(
        listener,
        uuid::Uuid::new_v4().to_string(),
    ));

    let mut client = Client::connect_with_timeouts(address, short_timeouts())
        .await
        .expect("initial handshake");
    let subscription = client
        .subscribe(SESSION_SERVICE_ID)
        .await
        .expect("subscribe");
    let snapshot = client
        .resync_session_snapshot("sess-ahead")
        .await
        .expect("resync snapshot");
    assert_eq!(snapshot["revision"], 6);
    assert_eq!(
        client.snapshot_sequence("sess-ahead"),
        Some(7),
        "the queued snapshot ahead of the response is already observed"
    );
    assert!(!client.snapshot_sequence_gap("sess-ahead"));

    let queued = client
        .next_update(&subscription)
        .await
        .expect("the queued snapshot ahead of the response");
    assert_eq!(queued["sequence"], 7);
    let next = client
        .next_update(&subscription)
        .await
        .expect("the snapshot after the resync");
    assert_eq!(next["sequence"], 8);
    assert_eq!(client.snapshot_sequence("sess-ahead"), Some(8));
    assert!(
        !client.snapshot_sequence_gap("sess-ahead"),
        "a queue ahead of the resync revision must not read as a gap"
    );
    fixture.abort();
}

// ---------------------------------------------------------------------------
// P0: response buffering + progress-overflow typing
// ---------------------------------------------------------------------------

/// Scripted peer for the start_call ordering test: answers `subscribe`, then
/// for the next request sends the response FIRST and immediately pushes one
/// service_update behind it — so the caller's `next_update` consumes the
/// response before its `wait_call` starts.
async fn response_then_update_accept_loop(listener: TcpListener, server_id: String) {
    let Ok((mut stream, _)) = listener.accept().await else {
        return;
    };
    let Ok(mut decoder) = ClientMessageDecoder::new(FrameOptions::default()) else {
        return;
    };
    let hello = ServerEnvelope::Hello(ServerHelloV8 {
        type_: "hello".to_owned(),
        version: PROTOCOL_VERSION,
        server_id,
    });
    let Ok(frame) = encode_server_envelope(&hello, FrameOptions::default()) else {
        return;
    };
    if stream.write_all(&frame).await.is_err() {
        return;
    }
    let mut subscription_id = String::from("sub-1");
    let mut buffer = [0_u8; 8192];
    loop {
        let Ok(read) = stream.read(&mut buffer).await else {
            return;
        };
        if read == 0 {
            return;
        }
        let Ok(messages) = decoder.push(&buffer[..read]) else {
            return;
        };
        for value in messages {
            if value.get("type").and_then(|t| t.as_str()) != Some("request") {
                continue;
            }
            let Some(id) = value.get("id").and_then(|v| v.as_str()) else {
                continue;
            };
            let member = value
                .get("call")
                .and_then(|call| call.get("member"))
                .and_then(|m| m.as_str())
                .unwrap_or_default()
                .to_owned();
            if member == "subscribe" {
                subscription_id = value
                    .get("call")
                    .and_then(|call| call.get("args"))
                    .and_then(|args| args.as_array())
                    .and_then(|args| args.first())
                    .and_then(|arg| arg.as_str())
                    .unwrap_or("sub-1")
                    .to_owned();
                let response = ServerEnvelope::Response(ResponseFrame {
                    type_: "response".to_owned(),
                    id: id.to_owned(),
                    ok: true,
                    result: Some(serde_json::json!({
                        "serviceId": SESSION_SERVICE_ID,
                        "mode": "singleton",
                        "instances": [],
                    })),
                    error: None,
                });
                let Ok(frame) = encode_server_envelope(&response, FrameOptions::default()) else {
                    return;
                };
                if stream.write_all(&frame).await.is_err() {
                    return;
                }
                continue;
            }
            // The response to the `start_call` request...
            let response = ServerEnvelope::Response(ResponseFrame {
                type_: "response".to_owned(),
                id: id.to_owned(),
                ok: true,
                result: Some(serde_json::json!({"sessions": []})),
                error: None,
            });
            let Ok(frame) = encode_server_envelope(&response, FrameOptions::default()) else {
                return;
            };
            if stream.write_all(&frame).await.is_err() {
                return;
            }
            // ...and one update behind it, consumed while the response is
            // still queued by the client.
            let update = serde_json::json!({
                "type": "state",
                "member": "snapshot",
                "sequence": 1,
                "instance": {"key": "sess-buffered", "generation": 1},
                "ops": [["r", {"id": "sess-buffered"}]],
            });
            let envelope = ServerEnvelope::ServiceUpdate(ServiceUpdateFrame {
                type_: "service_update".to_owned(),
                subscription_id: subscription_id.clone(),
                update,
            });
            let Ok(frame) = encode_server_envelope(&envelope, FrameOptions::default()) else {
                return;
            };
            if stream.write_all(&frame).await.is_err() {
                return;
            }
        }
    }
}

/// P0: `start_call` + `next_update` must not lose the call's terminal
/// response. Before the by-id buffer, `queue_push` dropped unmatched response
/// frames, so `wait_call` waited out the full request timeout for a response
/// that had already been read.
#[tokio::test]
async fn start_call_response_is_buffered_across_next_update() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fixture listener");
    let address = listener.local_addr().expect("fixture address");
    let fixture = tokio::spawn(response_then_update_accept_loop(
        listener,
        uuid::Uuid::new_v4().to_string(),
    ));

    let mut client = Client::connect_with_timeouts(address, short_timeouts())
        .await
        .expect("initial handshake");
    let subscription = client
        .subscribe(SESSION_SERVICE_ID)
        .await
        .expect("subscribe");
    let request_id = client
        .start_call(
            RpcTarget::server(client.server_id()),
            SESSION_SERVICE_ID,
            "list",
            Vec::new(),
        )
        .await
        .expect("start list without waiting");

    let update = client
        .next_update(&subscription)
        .await
        .expect("the update queued behind the response");
    assert_eq!(update["member"], "snapshot");
    assert_eq!(
        client.pending_responses_count(),
        1,
        "the response consumed by next_update must be buffered by id"
    );

    let result = client
        .wait_call(&request_id)
        .await
        .expect("the buffered terminal response resolves without a second read");
    assert_eq!(result["sessions"], serde_json::json!([]));
    assert_eq!(client.pending_responses_count(), 0);
    fixture.abort();
}

/// Body shape used by the update-flood fixture. Tests reuse it so the exact
/// serialized length the client accounts for is computable on the test side.
fn progress_update(sequence: i64, payload_bytes: usize) -> serde_json::Value {
    serde_json::json!({
        "type": "state",
        "member": "progress",
        "sequence": sequence,
        "instance": {"key": "sess-overflow", "generation": 1},
        "ops": [],
        "payload": "x".repeat(payload_bytes),
    })
}

/// Snapshot body shape used by the attach byte-accounting fixture.
fn budget_snapshot_update(sequence: i64) -> serde_json::Value {
    serde_json::json!({
        "type": "state",
        "member": "snapshot",
        "sequence": sequence,
        "instance": {"key": "sess-budget", "generation": 1},
        "ops": [["r", {"id": "sess-budget"}]],
    })
}

/// Scripted peer for the update-buffer tests: answers `subscribe`, floods
/// `update_count` progress updates (each carrying a `payload_bytes` string)
/// behind the next response, and answers an `attach` (the resync) with a
/// snapshot that carries an empty transcript. `flood_subscription_id`
/// overrides the id put on flooded frames (the client-issued `sub-N` is used
/// when `None`).
async fn update_flood_accept_loop(
    listener: TcpListener,
    server_id: String,
    update_count: usize,
    payload_bytes: usize,
    flood_subscription_id: Option<String>,
) {
    let Ok((mut stream, _)) = listener.accept().await else {
        return;
    };
    let Ok(mut decoder) = ClientMessageDecoder::new(FrameOptions::default()) else {
        return;
    };
    let hello = ServerEnvelope::Hello(ServerHelloV8 {
        type_: "hello".to_owned(),
        version: PROTOCOL_VERSION,
        server_id,
    });
    let Ok(frame) = encode_server_envelope(&hello, FrameOptions::default()) else {
        return;
    };
    if stream.write_all(&frame).await.is_err() {
        return;
    }
    let mut subscription_id = String::from("sub-1");
    let mut buffer = [0_u8; 8192];
    loop {
        let Ok(read) = stream.read(&mut buffer).await else {
            return;
        };
        if read == 0 {
            return;
        }
        let Ok(messages) = decoder.push(&buffer[..read]) else {
            return;
        };
        for value in messages {
            if value.get("type").and_then(|t| t.as_str()) != Some("request") {
                continue;
            }
            let Some(id) = value.get("id").and_then(|v| v.as_str()) else {
                continue;
            };
            let member = value
                .get("call")
                .and_then(|call| call.get("member"))
                .and_then(|m| m.as_str())
                .unwrap_or_default()
                .to_owned();
            if member == "subscribe" {
                subscription_id = value
                    .get("call")
                    .and_then(|call| call.get("args"))
                    .and_then(|args| args.as_array())
                    .and_then(|args| args.first())
                    .and_then(|arg| arg.as_str())
                    .unwrap_or("sub-1")
                    .to_owned();
                let response = ServerEnvelope::Response(ResponseFrame {
                    type_: "response".to_owned(),
                    id: id.to_owned(),
                    ok: true,
                    result: Some(serde_json::json!({
                        "serviceId": SESSION_SERVICE_ID,
                        "mode": "singleton",
                        "instances": [],
                    })),
                    error: None,
                });
                let Ok(frame) = encode_server_envelope(&response, FrameOptions::default()) else {
                    return;
                };
                if stream.write_all(&frame).await.is_err() {
                    return;
                }
                continue;
            }
            if member == "attach" {
                let response = ServerEnvelope::Response(ResponseFrame {
                    type_: "response".to_owned(),
                    id: id.to_owned(),
                    ok: true,
                    result: Some(serde_json::json!({
                        "command": "attach",
                        "attachmentId": "att-overflow",
                        "session": {
                            "id": "sess-overflow",
                            "revision": 3,
                            "transcript": [],
                        },
                    })),
                    error: None,
                });
                let Ok(frame) = encode_server_envelope(&response, FrameOptions::default()) else {
                    return;
                };
                if stream.write_all(&frame).await.is_err() {
                    return;
                }
                continue;
            }
            // Flood the buffer, then answer the request. The caller's `call`
            // reads the updates while waiting, so the drop-oldest bound engages.
            let flood_id = flood_subscription_id
                .as_deref()
                .unwrap_or(&subscription_id)
                .to_owned();
            for sequence in 1..=update_count as i64 {
                let update = progress_update(sequence, payload_bytes);
                let envelope = ServerEnvelope::ServiceUpdate(ServiceUpdateFrame {
                    type_: "service_update".to_owned(),
                    subscription_id: flood_id.clone(),
                    update,
                });
                let Ok(frame) = encode_server_envelope(&envelope, FrameOptions::default()) else {
                    return;
                };
                if stream.write_all(&frame).await.is_err() {
                    return;
                }
            }
            let response = ServerEnvelope::Response(ResponseFrame {
                type_: "response".to_owned(),
                id: id.to_owned(),
                ok: true,
                result: Some(serde_json::json!({"sessions": []})),
                error: None,
            });
            let Ok(frame) = encode_server_envelope(&response, FrameOptions::default()) else {
                return;
            };
            if stream.write_all(&frame).await.is_err() {
                return;
            }
        }
    }
}

/// P0: hitting [`MAX_PENDING_UPDATES`] must produce a typed gap signal, not a
/// silent truncation. A resync afterwards returns the protocol snapshot
/// (whose transcript is empty) and must not pretend the lost progress was
/// restored.
#[tokio::test]
async fn update_overflow_is_reported_and_snapshot_resync_stays_transcript_empty() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fixture listener");
    let address = listener.local_addr().expect("fixture address");
    let fixture = tokio::spawn(update_flood_accept_loop(
        listener,
        uuid::Uuid::new_v4().to_string(),
        MAX_PENDING_UPDATES + 5,
        0,
        None,
    ));

    let mut client = Client::connect_with_timeouts(address, short_timeouts())
        .await
        .expect("initial handshake");
    let subscription = client
        .subscribe(SESSION_SERVICE_ID)
        .await
        .expect("subscribe");
    // The call drains the flood while waiting for its response.
    client
        .call(
            RpcTarget::server(client.server_id()),
            SESSION_SERVICE_ID,
            "list",
            Vec::new(),
        )
        .await
        .expect("list behind the flood");
    let dropped = client.pending_updates_dropped();
    assert_eq!(
        dropped, 5,
        "the overflow must be counted (MAX_PENDING_UPDATES + 5 frames arrived)"
    );

    let error = client
        .next_update(&subscription)
        .await
        .expect_err("the truncated progress stream must be reported before its surviving tail");
    match error {
        ClientError::UpdateBufferOverflow {
            subscription_id,
            dropped: reported,
        } => {
            assert_eq!(subscription_id, subscription.subscription_id);
            assert_eq!(reported, 5);
        }
        other => panic!("expected UpdateBufferOverflow, got {other:?}"),
    }

    // The protocol snapshot carries no transcript; resync must not claim to
    // repair the lost progress, and the loss counter survives it.
    let snapshot = client
        .resync_session_snapshot("sess-overflow")
        .await
        .expect("state resync");
    assert_eq!(snapshot["transcript"], serde_json::json!([]));
    assert_eq!(
        client.pending_updates_dropped(),
        5,
        "a snapshot resync must not erase the recorded progress truncation"
    );

    // The surviving tail of the delta stream is still consumable after the
    // one-time typed report.
    let tail = client
        .next_update(&subscription)
        .await
        .expect("surviving updates after the overflow report");
    assert_eq!(tail["sequence"], 6, "the oldest five updates were dropped");
    fixture.abort();
}

/// C2-03: the per-entry count bound alone still lets a peer buffer 1024
/// near-frame-cap values. The serialized-byte budget must cap the queue's
/// payload bytes and report dropped frames through the same typed signal.
#[tokio::test]
async fn update_byte_overflow_keeps_the_queue_within_its_budget() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fixture listener");
    let address = listener.local_addr().expect("fixture address");
    let fixture = tokio::spawn(update_flood_accept_loop(
        listener,
        uuid::Uuid::new_v4().to_string(),
        8,
        4096,
        None,
    ));

    let mut client = Client::connect_with_timeouts(address, short_timeouts())
        .await
        .expect("initial handshake");
    let byte_limit = 10 * 1024;
    client.set_pending_updates_byte_limit(byte_limit);
    let subscription = client
        .subscribe(SESSION_SERVICE_ID)
        .await
        .expect("subscribe");
    // The call drains the flood while waiting for its response.
    client
        .call(
            RpcTarget::server(client.server_id()),
            SESSION_SERVICE_ID,
            "list",
            Vec::new(),
        )
        .await
        .expect("list behind the flood");

    let dropped = client.pending_updates_dropped();
    assert_eq!(
        dropped, 6,
        "three 4 KiB updates cannot fit in 10 KiB; each push evicts one older frame"
    );
    let bytes = client.pending_updates_bytes();
    assert!(
        bytes <= byte_limit,
        "buffered {bytes} bytes exceeds the {byte_limit}-byte budget"
    );
    assert_eq!(
        client.pending_updates_count(),
        2,
        "only the two newest updates fit the byte budget"
    );
    assert!(
        client.pending_updates_count() <= MAX_PENDING_UPDATES,
        "the entry bound still holds"
    );

    let error = client
        .next_update(&subscription)
        .await
        .expect_err("a byte-bound drop must be reported like a count-bound drop");
    match error {
        ClientError::UpdateBufferOverflow {
            subscription_id,
            dropped: reported,
        } => {
            assert_eq!(subscription_id, subscription.subscription_id);
            assert_eq!(reported, 6);
        }
        other => panic!("expected UpdateBufferOverflow, got {other:?}"),
    }
    // The surviving tail is still consumable after the one-time report.
    let tail = client
        .next_update(&subscription)
        .await
        .expect("surviving updates after the byte overflow report");
    assert_eq!(tail["sequence"], 7, "the oldest six updates were dropped");
    fixture.abort();
}

/// C2-03 regression: with a 16 MiB frame cap a peer could park 1024 ids of
/// ~16 MiB each on tiny `null` bodies (~16 GiB). Ids above the length cap are
/// dropped before enqueueing and counted, so the count-bound flood leaves an
/// empty, zero-byte queue.
#[tokio::test]
async fn oversized_subscription_ids_are_dropped_and_counted_at_the_entry_bound() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fixture listener");
    let address = listener.local_addr().expect("fixture address");
    let oversized_id = "i".repeat(MAX_PENDING_UPDATE_SUBSCRIPTION_ID_BYTES + 1);
    let fixture = tokio::spawn(update_flood_accept_loop(
        listener,
        uuid::Uuid::new_v4().to_string(),
        MAX_PENDING_UPDATES,
        0,
        Some(oversized_id),
    ));

    let mut client = Client::connect_with_timeouts(address, short_timeouts())
        .await
        .expect("initial handshake");
    client
        .call(
            RpcTarget::server(client.server_id()),
            SESSION_SERVICE_ID,
            "list",
            Vec::new(),
        )
        .await
        .expect("list behind the oversized-id flood");

    assert_eq!(
        client.pending_updates_dropped(),
        MAX_PENDING_UPDATES as u64,
        "every oversized-id frame must increment the overflow counter"
    );
    assert_eq!(
        client.pending_updates_count(),
        0,
        "no oversized-id frame may occupy a queue slot"
    );
    assert_eq!(
        client.pending_updates_bytes(),
        0,
        "rejected frames must not contribute to the byte budget"
    );
    fixture.abort();
}

/// C2-03 regression: the byte budget must account the subscription id too.
/// The body alone fits the limit exactly; counting the id pushes the entry
/// over it and the drop counter fires.
#[tokio::test]
async fn pending_update_bytes_include_the_subscription_id() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fixture listener");
    let address = listener.local_addr().expect("fixture address");
    let subscription_id = "i".repeat(256);
    let body_len = serde_json::to_vec(&progress_update(1, 16))
        .expect("measure flood body")
        .len();
    let fixture = tokio::spawn(update_flood_accept_loop(
        listener,
        uuid::Uuid::new_v4().to_string(),
        1,
        16,
        Some(subscription_id),
    ));

    let mut client = Client::connect_with_timeouts(address, short_timeouts())
        .await
        .expect("initial handshake");
    // Body-only accounting would accept this entry (bytes == limit); the id
    // bytes must push it over.
    client.set_pending_updates_byte_limit(body_len);
    client
        .call(
            RpcTarget::server(client.server_id()),
            SESSION_SERVICE_ID,
            "list",
            Vec::new(),
        )
        .await
        .expect("list behind the update");

    assert_eq!(
        client.pending_updates_dropped(),
        1,
        "the subscription id must count against the byte budget"
    );
    assert_eq!(client.pending_updates_count(), 0);
    assert_eq!(client.pending_updates_bytes(), 0);
    fixture.abort();
}

/// Normal client-issued ids still pass: the accounted bytes are exactly the
/// body plus the id, and consuming the queued entry returns the budget to 0.
#[tokio::test]
async fn normal_subscription_id_passes_and_is_accounted() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fixture listener");
    let address = listener.local_addr().expect("fixture address");
    let body_len = serde_json::to_vec(&progress_update(1, 16))
        .expect("measure flood body")
        .len();
    let fixture = tokio::spawn(update_flood_accept_loop(
        listener,
        uuid::Uuid::new_v4().to_string(),
        1,
        16,
        None,
    ));

    let mut client = Client::connect_with_timeouts(address, short_timeouts())
        .await
        .expect("initial handshake");
    let subscription = client
        .subscribe(SESSION_SERVICE_ID)
        .await
        .expect("subscribe");
    let id_len = subscription.subscription_id.len();
    client.set_pending_updates_byte_limit(body_len + id_len);
    client
        .call(
            RpcTarget::server(client.server_id()),
            SESSION_SERVICE_ID,
            "list",
            Vec::new(),
        )
        .await
        .expect("list behind the update");

    assert_eq!(client.pending_updates_dropped(), 0);
    assert_eq!(client.pending_updates_count(), 1);
    assert_eq!(
        client.pending_updates_bytes(),
        body_len + id_len,
        "accounted bytes = serialized body + subscription id"
    );
    let update = client
        .next_update(&subscription)
        .await
        .expect("the queued update");
    assert_eq!(update["sequence"], 1);
    assert_eq!(client.pending_updates_count(), 0);
    assert_eq!(client.pending_updates_bytes(), 0);
    fixture.abort();
}

/// Scripted peer for the attach byte-accounting boundary: queues one snapshot
/// AHEAD of the attach response's revision under an at-cap subscription id, so
/// the attach purge keeps it and must recompute `pending_updates_bytes` from
/// body + id.
async fn attach_snapshot_budget_accept_loop(listener: TcpListener, server_id: String) {
    let Ok((mut stream, _)) = listener.accept().await else {
        return;
    };
    let Ok(mut decoder) = ClientMessageDecoder::new(FrameOptions::default()) else {
        return;
    };
    let hello = ServerEnvelope::Hello(ServerHelloV8 {
        type_: "hello".to_owned(),
        version: PROTOCOL_VERSION,
        server_id,
    });
    let Ok(frame) = encode_server_envelope(&hello, FrameOptions::default()) else {
        return;
    };
    if stream.write_all(&frame).await.is_err() {
        return;
    }
    let long_id = "i".repeat(MAX_PENDING_UPDATE_SUBSCRIPTION_ID_BYTES);
    let mut buffer = [0_u8; 8192];
    loop {
        let Ok(read) = stream.read(&mut buffer).await else {
            return;
        };
        if read == 0 {
            return;
        }
        let Ok(messages) = decoder.push(&buffer[..read]) else {
            return;
        };
        for value in messages {
            if value.get("type").and_then(|t| t.as_str()) != Some("request") {
                continue;
            }
            let Some(id) = value.get("id").and_then(|v| v.as_str()) else {
                continue;
            };
            let member = value
                .get("call")
                .and_then(|call| call.get("member"))
                .and_then(|m| m.as_str())
                .unwrap_or_default()
                .to_owned();
            if member == "attach" {
                let ahead = ServerEnvelope::ServiceUpdate(ServiceUpdateFrame {
                    type_: "service_update".to_owned(),
                    subscription_id: long_id.clone(),
                    update: budget_snapshot_update(7),
                });
                let Ok(frame) = encode_server_envelope(&ahead, FrameOptions::default()) else {
                    return;
                };
                if stream.write_all(&frame).await.is_err() {
                    return;
                }
                let response = ServerEnvelope::Response(ResponseFrame {
                    type_: "response".to_owned(),
                    id: id.to_owned(),
                    ok: true,
                    result: Some(serde_json::json!({
                        "command": "attach",
                        "attachmentId": "att-budget",
                        "session": {"id": "sess-budget", "revision": 6},
                    })),
                    error: None,
                });
                let Ok(frame) = encode_server_envelope(&response, FrameOptions::default()) else {
                    return;
                };
                if stream.write_all(&frame).await.is_err() {
                    return;
                }
                continue;
            }
            let response = ServerEnvelope::Response(ResponseFrame {
                type_: "response".to_owned(),
                id: id.to_owned(),
                ok: true,
                result: Some(serde_json::json!({})),
                error: None,
            });
            let Ok(frame) = encode_server_envelope(&response, FrameOptions::default()) else {
                return;
            };
            if stream.write_all(&frame).await.is_err() {
                return;
            }
        }
    }
}

/// Attach purge boundary: the retained ahead snapshot's accounted bytes must
/// include its (at-cap) subscription id after `apply_attach_snapshot`
/// recomputes the queue total.
#[tokio::test]
async fn attach_purge_recomputes_pending_bytes_with_subscription_ids() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fixture listener");
    let address = listener.local_addr().expect("fixture address");
    let fixture = tokio::spawn(attach_snapshot_budget_accept_loop(
        listener,
        uuid::Uuid::new_v4().to_string(),
    ));

    let mut client = Client::connect_with_timeouts(address, short_timeouts())
        .await
        .expect("initial handshake");
    let snapshot = client
        .resync_session_snapshot("sess-budget")
        .await
        .expect("attach snapshot");
    assert_eq!(snapshot["revision"], 6);
    let body_len = serde_json::to_vec(&budget_snapshot_update(7))
        .expect("measure snapshot body")
        .len();
    assert_eq!(client.pending_updates_count(), 1);
    assert_eq!(
        client.pending_updates_bytes(),
        body_len + MAX_PENDING_UPDATE_SUBSCRIPTION_ID_BYTES,
        "attach purge must recompute body + subscription id for retained entries"
    );

    let subscription = Subscription {
        subscription_id: "i".repeat(MAX_PENDING_UPDATE_SUBSCRIPTION_ID_BYTES),
        service_id: SESSION_SERVICE_ID.to_owned(),
    };
    let update = client
        .next_update(&subscription)
        .await
        .expect("the queued ahead snapshot");
    assert_eq!(update["sequence"], 7);
    assert_eq!(client.pending_updates_count(), 0);
    assert_eq!(client.pending_updates_bytes(), 0);
    fixture.abort();
}

/// First connection queues one update behind a `list` response; the second
/// (post-reconnect) connection only completes the handshake. The reconnect
/// teardown must clear the queue and its byte total.
async fn queued_update_then_reconnect_accept_loop(listener: TcpListener, server_id: String) {
    let Ok((mut stream, _)) = listener.accept().await else {
        return;
    };
    let Ok(mut decoder) = ClientMessageDecoder::new(FrameOptions::default()) else {
        return;
    };
    let hello = ServerEnvelope::Hello(ServerHelloV8 {
        type_: "hello".to_owned(),
        version: PROTOCOL_VERSION,
        server_id: server_id.clone(),
    });
    let Ok(frame) = encode_server_envelope(&hello, FrameOptions::default()) else {
        return;
    };
    if stream.write_all(&frame).await.is_err() {
        return;
    }
    let mut buffer = [0_u8; 8192];
    loop {
        let Ok(read) = stream.read(&mut buffer).await else {
            return;
        };
        if read == 0 {
            break;
        }
        let Ok(messages) = decoder.push(&buffer[..read]) else {
            return;
        };
        for value in messages {
            if value.get("type").and_then(|t| t.as_str()) != Some("request") {
                continue;
            }
            let Some(id) = value.get("id").and_then(|v| v.as_str()) else {
                continue;
            };
            let member = value
                .get("call")
                .and_then(|call| call.get("member"))
                .and_then(|m| m.as_str())
                .unwrap_or_default();
            if member == "list" {
                let update = ServerEnvelope::ServiceUpdate(ServiceUpdateFrame {
                    type_: "service_update".to_owned(),
                    subscription_id: "sub-1".to_owned(),
                    update: progress_update(1, 32),
                });
                let Ok(frame) = encode_server_envelope(&update, FrameOptions::default()) else {
                    return;
                };
                if stream.write_all(&frame).await.is_err() {
                    return;
                }
            }
            let response = ServerEnvelope::Response(ResponseFrame {
                type_: "response".to_owned(),
                id: id.to_owned(),
                ok: true,
                result: Some(serde_json::json!({"sessions": []})),
                error: None,
            });
            let Ok(frame) = encode_server_envelope(&response, FrameOptions::default()) else {
                return;
            };
            if stream.write_all(&frame).await.is_err() {
                return;
            }
        }
    }
    // Second connection: handshake, then hold until the test aborts us.
    let Ok((mut stream, _)) = listener.accept().await else {
        return;
    };
    let hello = ServerEnvelope::Hello(ServerHelloV8 {
        type_: "hello".to_owned(),
        version: PROTOCOL_VERSION,
        server_id,
    });
    let Ok(frame) = encode_server_envelope(&hello, FrameOptions::default()) else {
        return;
    };
    if stream.write_all(&frame).await.is_err() {
        return;
    }
    let mut buffer = [0_u8; 8192];
    while let Ok(read) = stream.read(&mut buffer).await {
        if read == 0 {
            return;
        }
    }
}

/// Reconnect boundary: transport teardown clears the pending-update queue and
/// the byte total accumulated on the dead connection.
#[tokio::test]
async fn reconnect_clears_pending_update_bytes() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fixture listener");
    let address = listener.local_addr().expect("fixture address");
    let fixture = tokio::spawn(queued_update_then_reconnect_accept_loop(
        listener,
        uuid::Uuid::new_v4().to_string(),
    ));

    let mut client = Client::connect_with_timeouts(address, short_timeouts())
        .await
        .expect("initial handshake");
    client
        .call(
            RpcTarget::server(client.server_id()),
            SESSION_SERVICE_ID,
            "list",
            Vec::new(),
        )
        .await
        .expect("list behind the update");
    let body_len = serde_json::to_vec(&progress_update(1, 32))
        .expect("measure update body")
        .len();
    assert_eq!(client.pending_updates_count(), 1);
    assert_eq!(client.pending_updates_bytes(), body_len + "sub-1".len());

    let report = client.reconnect().await.expect("reconnect");
    assert_eq!(report.attempts, 1);
    assert_eq!(
        client.pending_updates_count(),
        0,
        "the dead connection's queue must be cleared"
    );
    assert_eq!(
        client.pending_updates_bytes(),
        0,
        "the dead connection's byte total must be cleared"
    );
    fixture.abort();
}

// ---------------------------------------------------------------------------
// R1-L2-002: a timed-out write tears the transport down (no half-frame reuse)
// ---------------------------------------------------------------------------

/// Scripted peer that completes the handshake and then never reads again.
/// The client's socket send buffers fill, so a large write can only end
/// through the request timeout.
async fn stalled_read_accept_loop(listener: TcpListener, server_id: String) {
    let Ok((mut stream, _)) = listener.accept().await else {
        return;
    };
    let hello = ServerEnvelope::Hello(ServerHelloV8 {
        type_: "hello".to_owned(),
        version: PROTOCOL_VERSION,
        server_id,
    });
    let Ok(frame) = encode_server_envelope(&hello, FrameOptions::default()) else {
        return;
    };
    if stream.write_all(&frame).await.is_err() {
        return;
    }
    // Deliberately no reads: hold the connection open until the test ends.
    tokio::time::sleep(Duration::from_secs(30)).await;
}

/// A write that hits the request timeout may have emitted a partial frame
/// (`write_all` is not cancellation safe). The client must drop the transport
/// (reader/writer/pending + a `Disconnected` transition) instead of reusing
/// the connection, and a later call must fail fast with `Disconnected`
/// instead of queueing another frame behind the torn one.
#[tokio::test]
async fn client_write_timeout_drops_transport_and_never_reuses_half_frame() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind stalled listener");
    let address = listener.local_addr().expect("fixture address");
    let fixture = tokio::spawn(stalled_read_accept_loop(
        listener,
        uuid::Uuid::new_v4().to_string(),
    ));

    let stream = TcpStream::connect(address).await.expect("dial fixture");
    let timeouts = ClientTimeouts {
        connect: Duration::from_millis(500),
        request: Duration::from_millis(200),
        update: Duration::from_millis(200),
    };
    let mut client = Client::from_stream_with_token_and_timeouts(stream, None, timeouts)
        .await
        .expect("handshake over the stalled peer");
    let target = RpcTarget::server(client.server_id());

    // Frames bigger than any loopback socket buffer: the earlier ones may
    // still fit in the kernel buffers, a later one cannot be written while
    // the peer refuses to read. Three attempts keep the fixture deterministic
    // on auto-tuned stacks without changing what is asserted (the timeout is
    // the only terminal outcome).
    let payload = "x".repeat(15 * 1024 * 1024);
    let mut timed_out = false;
    for _ in 0..3 {
        match client
            .start_call(
                target.clone(),
                SESSION_SERVICE_ID,
                "list",
                vec![serde_json::json!({"blob": &payload})],
            )
            .await
        {
            Ok(_) => continue,
            Err(ClientError::Timeout { operation: "send" }) => {
                timed_out = true;
                break;
            }
            Err(other) => panic!("unexpected failure while writing: {other:?}"),
        }
    }
    assert!(
        timed_out,
        "the stalled peer must eventually hit the write timeout"
    );
    assert_eq!(
        client.pending_requests_count(),
        0,
        "a write timeout must clear the pending table with the transport"
    );
    assert_eq!(
        client.connection_state(),
        ConnectionState::Disconnected,
        "a write timeout must report the transport as disconnected"
    );

    // The broken connection is not reused: the next call fails immediately
    // with a typed Disconnected instead of writing behind the half frame.
    let started = Instant::now();
    let second = client
        .call(target, SESSION_SERVICE_ID, "list", Vec::new())
        .await;
    assert!(
        matches!(second, Err(ClientError::Disconnected)),
        "expected Disconnected after the dropped transport, got {second:?}"
    );
    assert!(
        started.elapsed() < Duration::from_millis(200),
        "the next call must not wait out another request timeout"
    );
    fixture.abort();
}

// ---------------------------------------------------------------------------
// R1-L2-012: connect falls back to every resolved address
// ---------------------------------------------------------------------------

/// `lookup_host` may return several addresses (typically AAAA + A). When the
/// first one is unreachable the client must try the rest inside the same
/// connect deadline and remember the address that actually worked.
#[tokio::test]
async fn connect_tries_every_resolved_address() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind hello server");
    let real_addr = listener.local_addr().expect("fixture address");
    let server_id = uuid::Uuid::new_v4().to_string();
    let fixture = tokio::spawn(scripted_accept_loop(listener, server_id.clone()));

    // An unspecified (0.0.0.0) connect address fails IMMEDIATELY with
    // WSAEADDRNOTAVAIL on Windows; connecting to a closed loopback port can
    // instead HANG behind the loopback firewall, consuming the whole connect
    // deadline before the live address is tried. On Unix the 0.0.0.0 route
    // maps to localhost and the unused discard port refuses at once.
    let addrs = [SocketAddr::from(([0, 0, 0, 0], 9)), real_addr];
    let client = Client::connect_with_timeouts(&addrs[..], short_timeouts())
        .await
        .expect("connect must fall back to the live address");
    assert_eq!(client.server_id(), server_id);
    assert_eq!(
        client.address(),
        Some(real_addr),
        "the address that accepted the connection is kept for reconnect"
    );
    fixture.abort();
}
