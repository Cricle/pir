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

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pir::client::{
    Client, ClientError, ClientTimeouts, ConnectionState, ConnectionStateChange, ReconnectPolicy,
    MAX_PENDING_UPDATES,
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
    let server = Server::bind("127.0.0.1:0").await.expect("bind server");
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
    let server = Server::bind("127.0.0.1:0").await.expect("bind server");
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
    let server = Server::bind("127.0.0.1:0").await.expect("bind server");
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
            ..ServerSecurityConfig::default()
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

/// Scripted peer for the overflow test: answers `subscribe`, floods
/// `MAX_PENDING_UPDATES + 5` progress updates behind the next response, and
/// answers an `attach` (the resync) with a snapshot that carries an empty
/// transcript.
async fn update_overflow_accept_loop(listener: TcpListener, server_id: String) {
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
            for sequence in 1..=(MAX_PENDING_UPDATES as i64 + 5) {
                let update = serde_json::json!({
                    "type": "state",
                    "member": "progress",
                    "sequence": sequence,
                    "instance": {"key": "sess-overflow", "generation": 1},
                    "ops": [],
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
    let fixture = tokio::spawn(update_overflow_accept_loop(
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
