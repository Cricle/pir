//! Protocol v8 coverage: generic RPC envelope handshake, request/response
//! round trips, cancel, service_update emission, attachment fencing, and the
//! upstream ServerId pattern.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use pir::protocol::{
    decode_value, encode_client_value, is_server_id, parse_client_value, parse_server_message,
    parse_service_provider_update, parse_session_snapshot_value, CborOptions, FrameOptions,
    ServerMessageDecoder, PROTOCOL_VERSION, SERVICE_CONTROL_ID, SESSION_SERVICE_ID,
};
use pir::server::{Server, ServerSecurityConfig, ServerState};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

// ---------------------------------------------------------------------------
// Schema-level tests (no I/O)
// ---------------------------------------------------------------------------

#[test]
fn encodes_and_validates_client_hello() {
    let frame = encode_client_value(
        &serde_json::json!({"type": "hello", "version": PROTOCOL_VERSION}),
        FrameOptions::default(),
    )
    .expect("valid hello");
    assert_eq!(
        u32::from_be_bytes([frame[0], frame[1], frame[2], frame[3]]) as usize,
        frame.len() - 4
    );
    let value = decode_value(&frame[4..], CborOptions::default()).expect("valid CBOR");
    assert_eq!(
        value,
        serde_json::json!({"type":"hello", "version": PROTOCOL_VERSION})
    );
}

#[test]
fn protocol_golden_vector_decodes_and_reencodes() {
    // Definite-length CBOR map: {"type":"hello", "version":8}.
    let payload = [
        0xa2, 0x64, b't', b'y', b'p', b'e', 0x65, b'h', b'e', b'l', b'l', b'o', 0x67, b'v', b'e',
        b'r', b's', b'i', b'o', b'n', 0x08,
    ];
    let value = decode_value(&payload, CborOptions::default()).unwrap();
    assert_eq!(
        value,
        serde_json::json!({"type":"hello", "version": PROTOCOL_VERSION})
    );

    let frame = encode_client_value(
        &serde_json::json!({"type": "hello", "version": PROTOCOL_VERSION}),
        FrameOptions::default(),
    )
    .unwrap();
    assert_eq!(&frame[4..], payload);
}

#[test]
fn server_id_pattern_matches_upstream_uuid_shape() {
    // Upstream protocol.ts pins a version-4 UUID nibble.
    assert!(is_server_id("123e4567-e89b-42d3-a456-426614174000"));
    assert!(!is_server_id("123e4567-e89b-72d3-a456-426614174000"));
    assert!(!is_server_id("123E4567-E89B-42D3-A456-426614174000"));
    assert!(!is_server_id("123e4567e89b42d3a456426614174000"));
    assert!(!is_server_id("pir-server"));
    assert!(!is_server_id(""));
    assert!(!is_server_id("123e4567-e89b-42d3-c456-426614174000"));
}

fn valid_server_id() -> String {
    "123e4567-e89b-42d3-a456-426614174000".to_owned()
}

#[test]
fn request_targets_follow_the_v8_envelope_schema() {
    let call = serde_json::json!({"serviceId": SESSION_SERVICE_ID, "member": "list", "args": []});
    // Server target.
    assert!(parse_client_value(&serde_json::json!({
        "type": "request",
        "id": "req-1",
        "target": {"serverId": valid_server_id()},
        "call": call,
    }))
    .is_ok());
    // Session target.
    assert!(parse_client_value(&serde_json::json!({
        "type": "request",
        "id": "req-2",
        "target": {
            "serverId": valid_server_id(),
            "sessionId": "s-1",
            "attachmentId": "a-1",
        },
        "call": call,
    }))
    .is_ok());

    for invalid in [
        // Non-conforming serverId.
        serde_json::json!({
            "type": "request",
            "id": "req-3",
            "target": {"serverId": "pir-server"},
            "call": call,
        }),
        // Session target missing attachmentId.
        serde_json::json!({
            "type": "request",
            "id": "req-4",
            "target": {"serverId": valid_server_id(), "sessionId": "s-1"},
            "call": call,
        }),
        // Unknown field in the envelope.
        serde_json::json!({
            "type": "request",
            "id": "req-5",
            "target": {"serverId": valid_server_id()},
            "call": call,
            "extra": true,
        }),
        // Empty id.
        serde_json::json!({
            "type": "request",
            "id": "",
            "target": {"serverId": valid_server_id()},
            "call": call,
        }),
    ] {
        assert!(
            parse_client_value(&invalid).is_err(),
            "expected rejection: {invalid}"
        );
    }
}

#[test]
fn cancel_frames_validate_target_and_id() {
    assert!(parse_client_value(&serde_json::json!({
        "type": "cancel",
        "id": "c-1",
        "target": {"serverId": valid_server_id()},
    }))
    .is_ok());
    assert!(parse_client_value(&serde_json::json!({
        "type": "cancel",
        "id": "c-1",
    }))
    .is_err());
    assert!(parse_client_value(&serde_json::json!({
        "type": "cancel",
        "id": "c-1",
        "target": {"serverId": "not-a-uuid"},
    }))
    .is_err());
}

#[test]
fn rejects_unknown_client_value_field() {
    let value = serde_json::json!({"type":"hello", "version":1, "extra":true});
    assert!(parse_client_value(&value).is_err());
}

// ---------------------------------------------------------------------------
// Wire-level tests against a live server
// ---------------------------------------------------------------------------

async fn bind_server() -> (Arc<Server>, std::net::SocketAddr) {
    let server = Server::bind("127.0.0.1:0").await.expect("bind test server");
    let address = server.local_addr().expect("local address");
    (Arc::new(server), address)
}

/// A raw framed connection with frame coalescing handled explicitly: one TCP
/// read can carry several protocol frames, so every decoded frame is queued.
struct Conn {
    reader: tokio::net::tcp::OwnedReadHalf,
    writer: tokio::net::tcp::OwnedWriteHalf,
    decoder: ServerMessageDecoder,
    pending: VecDeque<Value>,
}

impl Conn {
    async fn connect(address: std::net::SocketAddr) -> Self {
        let stream = TcpStream::connect(address).await.expect("connect");
        let (reader, writer) = stream.into_split();
        Self {
            reader,
            writer,
            decoder: ServerMessageDecoder::new(FrameOptions::default()).expect("decoder"),
            pending: VecDeque::new(),
        }
    }

    async fn send(&mut self, value: &Value) {
        let frame = encode_client_value(value, FrameOptions::default()).expect("encode client");
        self.writer.write_all(&frame).await.expect("write frame");
        self.writer.flush().await.expect("flush");
    }

    async fn read(&mut self) -> Value {
        if let Some(value) = self.pending.pop_front() {
            return value;
        }
        let mut buffer = [0_u8; 8192];
        loop {
            let read = tokio::time::timeout(Duration::from_secs(5), self.reader.read(&mut buffer))
                .await
                .expect("read timeout")
                .expect("read socket");
            assert!(read > 0, "server closed the connection");
            let messages = self.decoder.push(&buffer[..read]).expect("decode frame");
            for message in messages {
                self.pending.push_back(message);
            }
            if let Some(value) = self.pending.pop_front() {
                return value;
            }
        }
    }

    /// Assert the server sends NOTHING within `duration` (no pending frames,
    /// no socket data). Frames decoded in the window are still queued for a
    /// subsequent [`Self::read`], so a false negative cannot swallow traffic.
    async fn assert_silent(&mut self, duration: Duration) {
        if self.pending.pop_front().is_some() {
            panic!("expected silence, but an earlier frame is still pending");
        }
        let mut buffer = [0_u8; 8192];
        let outcome = tokio::time::timeout(duration, self.reader.read(&mut buffer)).await;
        match outcome {
            Err(_) => {} // no data within the window: silent, as expected
            Ok(Ok(0)) => panic!("server closed the connection"),
            Ok(Ok(read)) => {
                let messages = self.decoder.push(&buffer[..read]).expect("decode frame");
                assert!(
                    messages.is_empty(),
                    "expected silence, got frames: {messages:?}"
                );
            }
            Ok(Err(error)) => panic!("read socket: {error}"),
        }
    }

    async fn handshake(&mut self) -> String {
        self.send(&serde_json::json!({"type": "hello", "version": PROTOCOL_VERSION}))
            .await;
        let hello = self.read().await;
        assert_eq!(hello["type"], "hello");
        assert_eq!(hello["version"], PROTOCOL_VERSION);
        let server_id = hello["serverId"].as_str().expect("serverId").to_owned();
        assert!(
            is_server_id(&server_id),
            "hello serverId must be a UUID: {server_id}"
        );
        server_id
    }
}

fn request_envelope(id: &str, target: Value, call: Value) -> Value {
    serde_json::json!({"type": "request", "id": id, "target": target, "call": call})
}

fn session_call(member: &str, args: Value) -> Value {
    serde_json::json!({"serviceId": SESSION_SERVICE_ID, "member": member, "args": args})
}

fn control_call(member: &str, args: Value) -> Value {
    serde_json::json!({"serviceId": SERVICE_CONTROL_ID, "member": member, "args": args})
}

/// The snapshot payload of one pir.session snapshot state update, in EITHER
/// wire vocabulary: chord delta (`["r", value]`, the server's default) or
/// the legacy `{"op":"set","value":...}` object (compatibility mode).
fn snapshot_payload(update: &Value) -> Value {
    use pir::protocol::{parse_state_update, DeltaOp, StateOp};
    let parsed = parse_state_update(update).expect("typed state update");
    match parsed.ops.first().expect("one snapshot op") {
        StateOp::Delta(DeltaOp::Replace(value)) => value.clone(),
        StateOp::Legacy { op, value } if op == "set" => {
            value.clone().expect("legacy set must carry a value")
        }
        other => panic!("unexpected snapshot operation: {other:?}"),
    }
}

#[tokio::test]
async fn v8_hello_handshake_accepts_version_8_and_rejects_other_versions() {
    let (server, address) = bind_server().await;

    {
        let accepting = {
            let server = server.clone();
            tokio::spawn(async move { server.accept_once().await })
        };
        let mut conn = Conn::connect(address).await;
        let server_id = conn.handshake().await;
        assert!(!server_id.is_empty());
        drop(conn);
        accepting
            .await
            .expect("accept task join")
            .expect("serve client");
    }

    // A wrong version must produce hello_error with the "version" code; the
    // server then closes the (otherwise clean) connection.
    let accepting = {
        let server = server.clone();
        tokio::spawn(async move { server.accept_once().await })
    };
    let mut conn = Conn::connect(address).await;
    conn.send(&serde_json::json!({"type": "hello", "version": 7}))
        .await;
    let error = conn.read().await;
    assert_eq!(error["type"], "hello_error");
    assert_eq!(error["error"]["code"], "version");

    accepting
        .await
        .expect("accept task join")
        .expect("serve client");
}

#[tokio::test]
async fn hello_is_required_before_requests() {
    let (server, address) = bind_server().await;
    let accepting = {
        let server = server.clone();
        tokio::spawn(async move { server.accept_once().await })
    };
    let mut conn = Conn::connect(address).await;
    conn.send(&request_envelope(
        "r1",
        serde_json::json!({"serverId": valid_server_id()}),
        session_call("list", serde_json::json!([])),
    ))
    .await;
    let error = conn.read().await;
    assert_eq!(error["type"], "hello_error");
    assert_eq!(error["error"]["code"], "invalid_request");
    accepting
        .await
        .expect("accept task join")
        .expect("serve client");
}

#[tokio::test]
async fn request_response_round_trip_over_control_and_session_services() {
    let (server, address) = bind_server().await;
    let accepting = {
        let server = server.clone();
        tokio::spawn(async move { server.accept_once().await })
    };
    let mut conn = Conn::connect(address).await;
    let server_id = conn.handshake().await;
    let server_target = serde_json::json!({"serverId": server_id});

    // Control catalogue lists the session service.
    conn.send(&request_envelope(
        "c1",
        server_target.clone(),
        control_call("catalogue", serde_json::json!([])),
    ))
    .await;
    let response = conn.read().await;
    assert_eq!(response["type"], "response");
    assert_eq!(response["id"], "c1");
    assert_eq!(response["ok"], true);
    assert_eq!(response["result"][0]["serviceId"], SESSION_SERVICE_ID);
    assert_eq!(response["result"][0]["mode"], "singleton");

    // Session create through the server target.
    conn.send(&request_envelope(
        "s1",
        server_target.clone(),
        session_call(
            "create",
            // Custom cwds are policy-gated (see server_security.rs); the
            // default cwd needs no allowance.
            serde_json::json!([{"name": "round-trip"}]),
        ),
    ))
    .await;
    let response = conn.read().await;
    assert_eq!(response["ok"], true, "create failed: {response}");
    let session_id = response["result"]["session"]["id"]
        .as_str()
        .expect("session id")
        .to_owned();
    assert_eq!(response["result"]["session"]["cwd"], ".");

    // List contains the created session.
    conn.send(&request_envelope(
        "s2",
        server_target.clone(),
        session_call("list", serde_json::json!([])),
    ))
    .await;
    let response = conn.read().await;
    let sessions = response["result"]["sessions"].as_array().expect("sessions");
    assert!(sessions.iter().any(|s| s["id"] == session_id.as_str()));

    // Unknown member yields an error response.
    conn.send(&request_envelope(
        "s3",
        server_target.clone(),
        session_call("frobnicate", serde_json::json!([])),
    ))
    .await;
    let response = conn.read().await;
    assert_eq!(response["ok"], false);
    assert_eq!(response["error"]["code"], "not_found");

    // Unknown service yields an error response.
    conn.send(&request_envelope(
        "s4",
        server_target,
        serde_json::json!({"serviceId": "no.such.service", "member": "x", "args": []}),
    ))
    .await;
    let response = conn.read().await;
    assert_eq!(response["ok"], false);
    assert_eq!(response["error"]["code"], "not_found");

    drop(conn);
    accepting
        .await
        .expect("accept task join")
        .expect("serve client");
}

/// Create a session and attach it, returning (session_id, attachment_id).
async fn create_and_attach(conn: &mut Conn, server_id: &str) -> (String, String) {
    let server_target = serde_json::json!({"serverId": server_id});
    conn.send(&request_envelope(
        "s1",
        server_target,
        session_call("create", serde_json::json!([{}])),
    ))
    .await;
    let response = conn.read().await;
    let session_id = response["result"]["session"]["id"]
        .as_str()
        .expect("session id")
        .to_owned();
    conn.send(&request_envelope(
        "a1",
        serde_json::json!({"serverId": server_id}),
        session_call("attach", serde_json::json!([{"sessionId": session_id}])),
    ))
    .await;
    let attachment = conn.read().await;
    assert_eq!(attachment["type"], "attachment");
    assert_eq!(attachment["attachment"]["sessionId"], session_id.as_str());
    let leased = attachment["attachment"]["attachmentId"]
        .as_str()
        .expect("attachmentId")
        .to_owned();
    let response = conn.read().await;
    assert_eq!(response["ok"], true, "attach failed: {response}");
    (session_id, leased)
}

#[tokio::test]
async fn attachment_fencing_gates_session_target_calls() {
    let (server, address) = bind_server().await;
    let accepting = {
        let server = server.clone();
        tokio::spawn(async move { server.accept_once().await })
    };
    let mut conn = Conn::connect(address).await;
    let server_id = conn.handshake().await;
    let (session_id, leased) = create_and_attach(&mut conn, &server_id).await;

    // A session-target call before any attachment is fenced off.
    conn.send(&request_envelope(
        "f1",
        serde_json::json!({
            "serverId": server_id,
            "sessionId": session_id,
            "attachmentId": "not-attached",
        }),
        session_call("prompt", serde_json::json!([{"text": "hi"}])),
    ))
    .await;
    let response = conn.read().await;
    assert_eq!(response["ok"], false);
    assert_eq!(response["error"]["code"], "session_locked");

    // A wrong attachment id is rejected while the lease is live.
    conn.send(&request_envelope(
        "f2",
        serde_json::json!({
            "serverId": server_id,
            "sessionId": session_id,
            "attachmentId": "bogus",
        }),
        session_call("abort", serde_json::json!([])),
    ))
    .await;
    let response = conn.read().await;
    assert_eq!(response["ok"], false);
    assert_eq!(response["error"]["code"], "session_locked");

    // The correct attachment id fences the call through.
    conn.send(&request_envelope(
        "f3",
        serde_json::json!({
            "serverId": server_id,
            "sessionId": session_id,
            "attachmentId": leased,
        }),
        session_call("abort", serde_json::json!([])),
    ))
    .await;
    let response = conn.read().await;
    assert_eq!(response["ok"], true, "fenced abort failed: {response}");

    // A target addressing another server id is rejected.
    conn.send(&request_envelope(
        "f4",
        serde_json::json!({
            "serverId": valid_server_id(),
            "sessionId": session_id,
            "attachmentId": leased,
        }),
        session_call("abort", serde_json::json!([])),
    ))
    .await;
    let response = conn.read().await;
    assert_eq!(response["ok"], false);
    assert_eq!(response["error"]["code"], "not_found");

    // Detach releases the lease and reports an empty attachment route.
    conn.send(&request_envelope(
        "d1",
        serde_json::json!({
            "serverId": server_id,
            "sessionId": session_id,
            "attachmentId": leased,
        }),
        session_call("detach", serde_json::json!([])),
    ))
    .await;
    let attachment = conn.read().await;
    assert_eq!(attachment["type"], "attachment");
    assert!(attachment["attachment"].is_null());
    let response = conn.read().await;
    assert_eq!(response["ok"], true, "detach failed: {response}");
    assert_eq!(response["result"]["command"], "detach");

    drop(conn);
    accepting
        .await
        .expect("accept task join")
        .expect("serve client");
}

#[tokio::test]
async fn service_updates_are_emitted_to_subscribers_on_session_progress() {
    let (server, address) = bind_server().await;
    let accepting = {
        let server = server.clone();
        tokio::spawn(async move { server.accept_once().await })
    };
    let mut conn = Conn::connect(address).await;
    let server_id = conn.handshake().await;
    let (session_id, leased) = create_and_attach(&mut conn, &server_id).await;

    // Subscribe through the built-in control service.
    conn.send(&request_envelope(
        "sub1",
        serde_json::json!({"serverId": server_id}),
        control_call(
            "subscribe",
            serde_json::json!(["sub-1", SESSION_SERVICE_ID, "singleton"]),
        ),
    ))
    .await;
    let response = conn.read().await;
    assert_eq!(response["ok"], true, "subscribe failed: {response}");

    // A prompt whose provider is missing still drives the session through
    // phase transitions, and every transition is broadcast as a state update.
    conn.send(&request_envelope(
        "p1",
        serde_json::json!({
            "serverId": server_id,
            "sessionId": session_id,
            "attachmentId": leased,
        }),
        session_call("prompt", serde_json::json!([{"text": "hello"}])),
    ))
    .await;

    let mut updates = Vec::new();
    let mut saw_error_response = false;
    for _ in 0..8 {
        let frame = conn.read().await;
        match frame["type"].as_str().expect("frame type") {
            "service_update" => updates.push(frame.clone()),
            "response" => {
                assert_eq!(frame["id"], "p1");
                assert_eq!(frame["ok"], false, "prompt must fail without a provider");
                assert_eq!(frame["error"]["code"], "internal_error");
                saw_error_response = true;
            }
            other => panic!("unexpected frame type {other}: {frame}"),
        }
        if saw_error_response && updates.len() >= 2 {
            break;
        }
    }
    assert!(saw_error_response, "prompt response never arrived");
    assert!(
        !updates.is_empty(),
        "subscribed connection must receive service_update frames"
    );
    for update_frame in &updates {
        assert_eq!(update_frame["subscriptionId"], "sub-1");
        let update = &update_frame["update"];
        parse_service_provider_update(update)
            .unwrap_or_else(|error| panic!("invalid service update {update}: {error}"));
        assert_eq!(update["type"], "state");
        assert_eq!(update["instance"]["key"], session_id.as_str());
        assert_eq!(update["member"], "snapshot");
        let value = snapshot_payload(update);
        parse_session_snapshot_value(&value)
            .unwrap_or_else(|error| panic!("invalid session snapshot {value}: {error}"));
        assert_eq!(value["id"], session_id.as_str());
    }
    // The session passed through the active phase and back to idle.
    let phases: Vec<String> = updates
        .iter()
        .map(|frame| {
            snapshot_payload(&frame["update"])["phase"]
                .as_str()
                .expect("phase")
                .to_owned()
        })
        .collect();
    assert!(
        phases.iter().any(|phase| phase == "turn"),
        "phases: {phases:?}"
    );
    assert_eq!(phases.last().map(String::as_str), Some("idle"));

    drop(conn);
    accepting
        .await
        .expect("accept task join")
        .expect("serve client");
}

/// §6 #17: the server publishes pir.session state updates in the chord delta
/// vocabulary by default (`["r", <snapshot>]`), and the client-side
/// [`StateUpdateDecoder`](pir::protocol::StateUpdateDecoder) consumes the
/// stream.
#[tokio::test]
async fn server_emits_chord_delta_state_updates_by_default() {
    use pir::protocol::{DeltaOp, ServiceUpdate, StateOp, StateUpdateDecoder};

    let (server, address) = bind_server().await;
    let accepting = {
        let server = server.clone();
        tokio::spawn(async move { server.accept_once().await })
    };
    let mut conn = Conn::connect(address).await;
    let server_id = conn.handshake().await;
    let (session_id, leased) = create_and_attach(&mut conn, &server_id).await;

    conn.send(&request_envelope(
        "sub1",
        serde_json::json!({"serverId": server_id}),
        control_call(
            "subscribe",
            serde_json::json!(["sub-1", SESSION_SERVICE_ID, "singleton"]),
        ),
    ))
    .await;
    let response = conn.read().await;
    assert_eq!(response["ok"], true, "subscribe failed: {response}");

    conn.send(&request_envelope(
        "p1",
        serde_json::json!({
            "serverId": server_id,
            "sessionId": session_id,
            "attachmentId": leased,
        }),
        session_call("prompt", serde_json::json!([{"text": "hello"}])),
    ))
    .await;

    let mut decoder = StateUpdateDecoder::new();
    let mut phases = Vec::new();
    let mut saw_error_response = false;
    for _ in 0..8 {
        let frame = conn.read().await;
        match frame["type"].as_str().expect("frame type") {
            "service_update" => {
                let update = &frame["update"];
                parse_service_provider_update(update)
                    .unwrap_or_else(|error| panic!("invalid service update {update}: {error}"));
                // Delta tuple shape: the first op is `["r", <SessionSnapshot>]`.
                let ops = update["ops"].as_array().expect("ops array");
                assert_eq!(ops.len(), 1, "one base op per snapshot update: {update}");
                let tuple = ops[0]
                    .as_array()
                    .unwrap_or_else(|| panic!("delta op must be a tuple: {update}"));
                assert_eq!(tuple[0], "r", "snapshot updates are base replacements");
                match decoder
                    .decode(update)
                    .expect("client decoder consumes delta")
                {
                    ServiceUpdate::State(state) => {
                        assert_eq!(state.member, "snapshot");
                        assert!(state.sequence >= 1, "sequence must advance");
                        match &state.ops[0] {
                            StateOp::Delta(DeltaOp::Replace(value)) => {
                                parse_session_snapshot_value(value)
                                    .expect("valid snapshot body in the delta payload");
                                phases.push(value["phase"].as_str().expect("phase").to_owned());
                            }
                            other => panic!("expected a delta replacement, got {other:?}"),
                        }
                    }
                    other => panic!("expected a pir state update, got {other:?}"),
                }
            }
            "response" => {
                assert_eq!(frame["id"], "p1");
                assert_eq!(frame["ok"], false, "prompt must fail without a provider");
                saw_error_response = true;
            }
            other => panic!("unexpected frame type {other}: {frame}"),
        }
        if saw_error_response && phases.len() >= 2 {
            break;
        }
    }
    assert!(saw_error_response, "prompt response never arrived");
    assert!(phases.iter().any(|phase| phase == "turn"), "{phases:?}");
    assert_eq!(phases.last().map(String::as_str), Some("idle"));

    drop(conn);
    accepting
        .await
        .expect("accept task join")
        .expect("serve client");
}

/// §6 #17 compatibility switch: `with_legacy_state_updates()` keeps emitting
/// the historical `{op, value}` objects, still decoded by the same client
/// decoder.
#[tokio::test]
async fn server_legacy_state_update_switch_emits_object_ops() {
    use pir::protocol::{ServiceUpdate, StateOp, StateUpdateDecoder};

    let state = ServerState::default().with_legacy_state_updates();
    let server = Server::bind_with_state("127.0.0.1:0", state)
        .await
        .expect("bind legacy server");
    let address = server.local_addr().expect("local address");
    let server = Arc::new(server);
    let accepting = {
        let server = server.clone();
        tokio::spawn(async move { server.accept_once().await })
    };
    let mut conn = Conn::connect(address).await;
    let server_id = conn.handshake().await;
    let (session_id, leased) = create_and_attach(&mut conn, &server_id).await;

    conn.send(&request_envelope(
        "sub1",
        serde_json::json!({"serverId": server_id}),
        control_call(
            "subscribe",
            serde_json::json!(["sub-1", SESSION_SERVICE_ID, "singleton"]),
        ),
    ))
    .await;
    let response = conn.read().await;
    assert_eq!(response["ok"], true, "subscribe failed: {response}");

    conn.send(&request_envelope(
        "p1",
        serde_json::json!({
            "serverId": server_id,
            "sessionId": session_id,
            "attachmentId": leased,
        }),
        session_call("prompt", serde_json::json!([{"text": "hello"}])),
    ))
    .await;

    let mut decoder = StateUpdateDecoder::new();
    let mut saw_legacy_snapshot = false;
    let mut saw_error_response = false;
    for _ in 0..8 {
        let frame = conn.read().await;
        match frame["type"].as_str().expect("frame type") {
            "service_update" => {
                let update = &frame["update"];
                parse_service_provider_update(update)
                    .unwrap_or_else(|error| panic!("invalid service update {update}: {error}"));
                let ops = update["ops"].as_array().expect("ops array");
                let op = ops[0].as_object().expect("legacy op object");
                assert_eq!(op["op"], "set", "legacy snapshot op: {update}");
                parse_session_snapshot_value(&op["value"]).expect("legacy snapshot body");
                match decoder
                    .decode(update)
                    .expect("client decoder consumes legacy")
                {
                    ServiceUpdate::State(state) => match &state.ops[0] {
                        StateOp::Legacy { op, .. } => assert_eq!(op, "set"),
                        other => panic!("expected a legacy op, got {other:?}"),
                    },
                    other => panic!("expected a pir state update, got {other:?}"),
                }
                saw_legacy_snapshot = true;
            }
            "response" => {
                assert_eq!(frame["id"], "p1");
                assert_eq!(frame["ok"], false, "prompt must fail without a provider");
                saw_error_response = true;
            }
            other => panic!("unexpected frame type {other}: {frame}"),
        }
        if saw_error_response && saw_legacy_snapshot {
            break;
        }
    }
    assert!(saw_error_response, "prompt response never arrived");
    assert!(saw_legacy_snapshot, "no legacy snapshot update arrived");

    drop(conn);
    accepting
        .await
        .expect("accept task join")
        .expect("serve client");
}

#[tokio::test]
async fn cancel_frames_acknowledge_with_a_response() {
    let (server, address) = bind_server().await;
    let accepting = {
        let server = server.clone();
        tokio::spawn(async move { server.accept_once().await })
    };
    let mut conn = Conn::connect(address).await;
    let server_id = conn.handshake().await;
    let (session_id, leased) = create_and_attach(&mut conn, &server_id).await;

    // Upstream `handleCancel` (server.ts:298-304) NEVER answers a cancel
    // frame: the cancelled REQUEST answers for itself with the `cancelled`
    // error, and a cancel whose id matches no active request is silently
    // ignored. Neither cancel below matches an active request, so the wire
    // must stay silent.
    conn.send(&serde_json::json!({
        "type": "cancel",
        "id": "c1",
        "target": {
            "serverId": server_id,
            "sessionId": session_id,
            "attachmentId": leased,
        },
    }))
    .await;
    // Session-fenced cancel with no matching active request: no ack.
    conn.assert_silent(Duration::from_millis(300)).await;

    // Server-wide cancel has nothing to cancel and stays silent too.
    conn.send(&serde_json::json!({
        "type": "cancel",
        "id": "c2",
        "target": {"serverId": server_id},
    }))
    .await;
    conn.assert_silent(Duration::from_millis(300)).await;

    drop(conn);
    accepting
        .await
        .expect("accept task join")
        .expect("serve client");
}

#[tokio::test]
async fn client_library_round_trip_with_call_subscribe_and_updates() {
    use pir::client::{Client, ClientError};
    use pir::protocol::RpcTarget;

    let (server, address) = bind_server().await;
    let accepting = {
        let server = server.clone();
        tokio::spawn(async move { server.accept_once().await })
    };

    let mut client = Client::connect(address).await.expect("v8 handshake");
    assert_eq!(client.snapshot()["protocolVersion"], PROTOCOL_VERSION);
    assert!(!client.connection_id().is_empty());

    // Generic call against the control service.
    let catalogue = client
        .call(
            RpcTarget::server(client.server_id()),
            SERVICE_CONTROL_ID,
            "catalogue",
            Vec::new(),
        )
        .await
        .expect("catalogue");
    assert_eq!(catalogue[0]["serviceId"], SESSION_SERVICE_ID);

    // Create a session, attach, and subscribe.
    let created = client
        .call(
            RpcTarget::server(client.server_id()),
            SESSION_SERVICE_ID,
            "create",
            vec![serde_json::json!({"name": "library"})],
        )
        .await
        .expect("create");
    let session_id = created["session"]["id"]
        .as_str()
        .expect("session id")
        .to_owned();
    client
        .call(
            RpcTarget::server(client.server_id()),
            SESSION_SERVICE_ID,
            "attach",
            vec![serde_json::json!({"sessionId": session_id})],
        )
        .await
        .expect("attach");
    assert!(
        client.attachment_id().is_some(),
        "attach must lease an attachment id"
    );

    let subscription = client
        .subscribe(SESSION_SERVICE_ID)
        .await
        .expect("subscribe");

    // Prompt without a provider fails; the queued updates are surfaced by
    // next_update afterwards.
    let target = RpcTarget::session(
        client.server_id(),
        &session_id,
        client.attachment_id().expect("attachment"),
    );
    let error = client
        .call(
            target,
            SESSION_SERVICE_ID,
            "prompt",
            vec![serde_json::json!({"text": "hi"})],
        )
        .await
        .expect_err("prompt must fail without a provider");
    assert!(matches!(error, ClientError::Server { ref code, .. } if code == "internal_error"));

    let update = client.next_update(&subscription).await.expect("update");
    parse_service_provider_update(&update).expect("valid update shape");
    assert_eq!(update["member"], "snapshot");
    let snapshot = snapshot_payload(&update);
    parse_session_snapshot_value(&snapshot).expect("valid snapshot body");
    let final_update = client
        .next_update(&subscription)
        .await
        .expect("final update");
    assert_eq!(
        snapshot_payload(&final_update)["phase"]
            .as_str()
            .expect("phase"),
        "idle"
    );

    // Compat surface still works: list shows the created session.
    let sessions = client.list().await.expect("compat list");
    assert_eq!(sessions.len(), 1);

    // Unsubscribe stops future updates.
    client
        .unsubscribe(&subscription)
        .await
        .expect("unsubscribe");

    drop(client);
    accepting
        .await
        .expect("accept task join")
        .expect("serve client");
}

// ---------------------------------------------------------------------------
// ProtocolError schema alignment (P2-5)
// ---------------------------------------------------------------------------

#[test]
fn protocol_error_accepts_any_nonempty_code_and_rejects_details() {
    // Upstream ProtocolErrorSchema: StrictObject({code: IdSchema, message}).
    // IdSchema is any non-empty string, so codes outside the pir error
    // catalog must validate.
    let custom_code = serde_json::json!({
        "type": "response",
        "id": "req-1",
        "ok": false,
        "error": {"code": "totally_custom_extension_code", "message": "boom"}
    });
    assert!(parse_server_message(&custom_code).is_ok());

    // StrictObject rejects the upstream-unknown `details` field.
    let with_details = serde_json::json!({
        "type": "response",
        "id": "req-1",
        "ok": false,
        "error": {"code": "x", "message": "m", "details": {"k": 1}}
    });
    assert!(parse_server_message(&with_details).is_err());

    // Empty and non-string codes are rejected.
    let empty_code = serde_json::json!({
        "type": "response",
        "id": "req-1",
        "ok": false,
        "error": {"code": "", "message": "m"}
    });
    assert!(parse_server_message(&empty_code).is_err());
    let numeric_code = serde_json::json!({
        "type": "response",
        "id": "req-1",
        "ok": false,
        "error": {"code": 42, "message": "m"}
    });
    assert!(parse_server_message(&numeric_code).is_err());

    // The cataloged codes keep working.
    let cataloged = serde_json::json!({
        "type": "hello_error",
        "error": {"code": "version", "message": "unsupported"}
    });
    assert!(parse_server_message(&cataloged).is_ok());
}

// ---------------------------------------------------------------------------
// Chord delta state updates (§6 #17)
// ---------------------------------------------------------------------------

use pir::protocol::{
    apply_delta, is_base, parse_chord_state_update, parse_state_update,
    session_snapshot_delta_update, session_state_delta_update, ChordStateKind, DeltaDecoder,
    DeltaEncoder, DeltaError, DeltaOp, DeltaSeg, ModelRef, SessionSnapshot, StateOp,
};

fn delta_path(parts: &[&str]) -> Vec<DeltaSeg> {
    parts
        .iter()
        .map(|part| DeltaSeg::Key((*part).to_owned()))
        .collect()
}

#[test]
fn delta_encoder_and_decoder_round_trip_interning_and_short_forms() {
    let path = vec![
        DeltaSeg::Key("messages".to_owned()),
        DeltaSeg::Index(0),
        DeltaSeg::Key("text".to_owned()),
    ];
    let other = vec![
        DeltaSeg::Key("messages".to_owned()),
        DeltaSeg::Index(1),
        DeltaSeg::Key("text".to_owned()),
    ];
    let ops = vec![
        DeltaOp::Set(path.clone(), serde_json::json!("hello")),
        DeltaOp::Append(path.clone(), " world".to_owned()),
        DeltaOp::Set(path.clone(), serde_json::json!("reset")),
        DeltaOp::Set(other, serde_json::json!("second")),
        DeltaOp::Set(path.clone(), serde_json::json!("third")),
        DeltaOp::Replace(serde_json::json!({"messages": []})),
        DeltaOp::Splice {
            path: delta_path(&["messages"]),
            index: 0,
            remove: 0,
            items: vec![serde_json::json!({"text": "base"})],
        },
    ];

    let mut encoder = DeltaEncoder::new();
    let wire = encoder.encode(&ops);
    // Same path as the previous op: the arity-shortened forms.
    assert_eq!(wire[1], serde_json::json!(["a", " world"]));
    assert_eq!(wire[2], serde_json::json!(["s", "reset"]));
    // The third use of the first path is an explicit `#` definition followed
    // by a numeric reference.
    assert_eq!(wire[4][0], serde_json::json!("#"));
    assert_eq!(wire[4][1], serde_json::json!(0));
    assert_eq!(wire[5], serde_json::json!(["s", 0, "third"]));

    let decoded = DeltaDecoder::new()
        .decode(&wire)
        .unwrap_or_else(|error| panic!("decode failed: {error} ({wire:?})"));
    assert_eq!(decoded, ops);

    // A base batch is a recovery point: its replacement clears the path
    // dictionary, and a later batch can still intern a path normally.
    let mut decoder = DeltaDecoder::new();
    let base = encoder.encode(&[
        DeltaOp::Replace(serde_json::json!({"n": 1})),
        DeltaOp::Set(delta_path(&["n"]), serde_json::json!(2)),
    ]);
    assert!(is_base(&decoder.decode(&base).expect("base batch")));
    let followup = encoder.encode(&[DeltaOp::Set(delta_path(&["n"]), serde_json::json!(3))]);
    let mut value = serde_json::json!({"n": 1});
    for batch in [
        decoder.decode(&base).expect("base"),
        decoder.decode(&followup).expect("followup"),
    ] {
        apply_delta(&mut value, &batch).expect("apply");
    }
    assert_eq!(value, serde_json::json!({"n": 3}));
}

#[test]
fn delta_decoder_rejects_malformed_and_unsafe_operations() {
    let mut decoder = DeltaDecoder::new();
    for invalid in [
        serde_json::json!("not-a-tuple"),
        serde_json::json!(["x"]),
        serde_json::json!(["s", ["a"]]),
        serde_json::json!(["s", [], 1]),
        serde_json::json!(["d"]),
        serde_json::json!(["s", 7, 1]),
        serde_json::json!(["s", ["__proto__", "x"], 1]),
        serde_json::json!(["#", -1, ["a"]]),
        serde_json::json!(["m", ["xs"], [0, 0]]),
        serde_json::json!(["p", ["xs"], 0, 0]),
    ] {
        assert!(
            decoder.decode(std::slice::from_ref(&invalid)).is_err(),
            "expected rejection: {invalid}"
        );
    }
}

/// The decoder is fed provider/network bytes: malformed input must always come
/// back as a typed [`DeltaError`], never as a panic. Some of these inputs
/// cannot reach the defensive `_` match arms (the wire guard rejects them
/// first); the assertions pin the public error surface those arms exist for.
#[test]
fn delta_decoder_reports_shape_errors_instead_of_panicking() {
    let mut decoder = DeltaDecoder::new();
    let unknown = decoder
        .decode(&[serde_json::json!(["z", ["a"], 1])])
        .expect_err("an unknown verb is a shape error");
    assert!(
        matches!(&unknown, DeltaError::Shape(message) if message.contains("unknown op verb: z")),
        "unexpected error: {unknown}"
    );
    for (invalid, expected) in [
        (serde_json::json!(["r", {"ok": true}, 1]), "arity"),
        (serde_json::json!(["s", ["a"], 1, 2]), "arity"),
        (serde_json::json!(["p", ["a"], 0]), "arity"),
        (serde_json::json!(["m", ["a"], [0, 0]]), "bijection"),
    ] {
        let error = decoder
            .decode(std::slice::from_ref(&invalid))
            .expect_err("malformed batch must be rejected");
        assert!(
            matches!(&error, DeltaError::Shape(message) if message.contains(expected)),
            "expected a Shape error mentioning {expected:?} for {invalid}: {error}"
        );
    }
}

#[test]
fn delta_apply_reports_malformed_ops_without_panicking() {
    // `Replace` is the recovery point: it resets the whole value even when it
    // appears in the middle of a batch, and later ops apply to the replacement.
    let mut value = serde_json::json!({"old": true});
    apply_delta(
        &mut value,
        &[
            DeltaOp::Replace(serde_json::json!({"fresh": 1})),
            DeltaOp::Set(delta_path(&["fresh"]), serde_json::json!(2)),
        ],
    )
    .expect("replace then set");
    assert_eq!(value, serde_json::json!({"fresh": 2}));

    for (op, expected) in [
        (DeltaOp::Set(Vec::new(), serde_json::json!(1)), "root"),
        (DeltaOp::Delete(Vec::new()), "root"),
        (DeltaOp::Append(Vec::new(), "x".to_owned()), "root"),
        (DeltaOp::Truncate(Vec::new(), 0), "root"),
        (
            DeltaOp::Move {
                path: delta_path(&["xs"]),
                permutation: vec![1, 1, 2],
            },
            "bijection",
        ),
    ] {
        let mut target = serde_json::json!({"xs": [1, 2, 3]});
        let error = apply_delta(&mut target, &[op])
            .expect_err("malformed op must be rejected without panicking");
        assert!(
            matches!(&error, DeltaError::Shape(message) if message.contains(expected)),
            "expected a Shape error mentioning {expected:?}: {error}"
        );
    }
}

#[test]
fn delta_apply_supports_the_full_tuple_vocabulary() {
    let mut value = serde_json::json!({
        "xs": [1, 2, 3],
        "text": "abcdef",
        "obj": {"a": 1, "b": 2},
    });
    apply_delta(
        &mut value,
        &[
            DeltaOp::Set(delta_path(&["obj", "c"]), serde_json::json!(3)),
            DeltaOp::Delete(delta_path(&["obj", "a"])),
            DeltaOp::Append(delta_path(&["text"]), "gh".to_owned()),
            DeltaOp::Truncate(delta_path(&["text"]), 2),
            DeltaOp::Splice {
                path: delta_path(&["xs"]),
                index: 1,
                remove: 1,
                items: vec![serde_json::json!(9)],
            },
            DeltaOp::Move {
                path: delta_path(&["xs"]),
                permutation: vec![2, 0, 1],
            },
        ],
    )
    .expect("apply");
    assert_eq!(value["obj"], serde_json::json!({"b": 2, "c": 3}));
    assert_eq!(value["text"], serde_json::json!("cdefgh"));
    assert_eq!(value["xs"], serde_json::json!([3, 1, 9]));

    // Upstream truncation counts UTF-16 code units, so skipping 2 drops the
    // leading emoji (two code units) rather than one scalar.
    let mut text = serde_json::json!({"s": "😀x"});
    apply_delta(&mut text, &[DeltaOp::Truncate(delta_path(&["s"]), 2)]).expect("utf-16 truncate");
    assert_eq!(text["s"], serde_json::json!("x"));
}

#[test]
fn service_updates_accept_legacy_and_delta_operations() {
    // Legacy object vocabulary keeps validating.
    let legacy = session_state_delta_update(
        "s-1",
        "snapshot",
        1,
        &[serde_json::json!({"op": "set", "value": {"id": "s-1"}})],
    );
    parse_service_provider_update(&legacy).expect("legacy ops");
    let parsed = parse_state_update(&legacy).expect("typed legacy");
    assert_eq!(parsed.member, "snapshot");
    assert_eq!(
        parsed.ops,
        vec![StateOp::Legacy {
            op: "set".to_owned(),
            value: Some(serde_json::json!({"id": "s-1"})),
        }]
    );

    // Delta tuples validate and decode.
    let delta = session_state_delta_update(
        "s-1",
        "snapshot",
        2,
        &[
            serde_json::json!(["r", {"id": "s-1"}]),
            serde_json::json!(["s", ["revision"], 4]),
        ],
    );
    parse_service_provider_update(&delta).expect("delta ops");
    let parsed = parse_state_update(&delta).expect("typed delta");
    assert_eq!(parsed.sequence, 2);
    assert!(matches!(parsed.ops[0], StateOp::Delta(DeltaOp::Replace(_))));
    assert_eq!(
        parsed.ops[1],
        StateOp::Delta(DeltaOp::Set(
            vec![DeltaSeg::Key("revision".to_owned())],
            serde_json::json!(4)
        ))
    );

    // Mixing the vocabularies, or a malformed delta tuple, is rejected.
    let mixed = session_state_delta_update(
        "s-1",
        "snapshot",
        3,
        &[
            serde_json::json!({"op": "set", "value": 1}),
            serde_json::json!(["r", 1]),
        ],
    );
    assert!(parse_service_provider_update(&mixed).is_err());
    let malformed = session_state_delta_update(
        "s-1",
        "snapshot",
        4,
        &[
            serde_json::json!(["s", ["a"], 1]),
            serde_json::json!(["d", 12]),
        ],
    );
    assert!(parse_service_provider_update(&malformed).is_err());
    let scalar = session_state_delta_update("s-1", "snapshot", 5, &[serde_json::json!(1)]);
    assert!(parse_service_provider_update(&scalar).is_err());
}

#[test]
fn chord_state_frames_validate_on_the_v8_update_path() {
    let snap = serde_json::json!({
        "t": "state",
        "hold": 1,
        "m": "snapshot",
        "as": "snap",
        "value": {"id": "s-1"},
    });
    let parsed = parse_chord_state_update(&snap).expect("snap frame");
    assert_eq!(parsed.hold, 1);
    assert_eq!(parsed.kind, ChordStateKind::Snap);
    parse_service_provider_update(&snap).expect("provider update accepts chord frames");

    let ops = serde_json::json!({
        "t": "state",
        "hold": 2,
        "m": "messages",
        "as": "ops",
        "ops": [["s", ["messages", 0, "text"], "hi"], ["a", " there"]],
    });
    let parsed = parse_chord_state_update(&ops).expect("ops frame");
    assert_eq!(parsed.ops.len(), 2);
    assert!(matches!(parsed.ops[0], DeltaOp::Set(_, _)));
    assert!(matches!(parsed.ops[1], DeltaOp::Append(_, _)));

    let error = serde_json::json!({
        "t": "state",
        "hold": 3,
        "m": "messages",
        "as": "error",
        "error": {"code": "unavailable", "message": "gone"},
    });
    assert!(parse_chord_state_update(&error).is_ok());
    assert!(parse_service_provider_update(&error).is_ok());

    for invalid in [
        serde_json::json!({"t": "state", "hold": 0, "m": "m", "as": "snap"}),
        serde_json::json!({"t": "state", "hold": 1, "m": "m", "as": "bogus"}),
        serde_json::json!({"t": "state", "hold": 1, "m": "m", "as": "ops"}),
        serde_json::json!({"t": "state", "hold": 1, "m": "m", "as": "error"}),
        serde_json::json!({
            "t": "state", "hold": 1, "m": "m", "as": "ops",
            "ops": [["s", [], 1]],
        }),
        serde_json::json!({"t": "state", "hold": 1, "m": "", "as": "snap"}),
    ] {
        assert!(
            parse_chord_state_update(&invalid).is_err(),
            "expected rejection: {invalid}"
        );
    }
}

#[test]
fn delta_update_builders_emit_valid_updates() {
    let snapshot = SessionSnapshot {
        id: "s-1".to_owned(),
        name: None,
        cwd: "/tmp".to_owned(),
        created_at: 1,
        updated_at: 2,
        phase: "idle".to_owned(),
        model: ModelRef {
            provider: "p".to_owned(),
            id: "m".to_owned(),
        },
        thinking_level: "off".to_owned(),
        attached: false,
        locked: false,
        revision: 1,
        transcript: Vec::new(),
        queued_steer: Vec::new(),
        queued_steer_count: 0,
    };
    let update = session_snapshot_delta_update("s-1", &snapshot, 7);
    parse_service_provider_update(&update).expect("delta snapshot update");
    let parsed = parse_state_update(&update).expect("typed update");
    assert_eq!(
        parsed.ops,
        vec![StateOp::Delta(DeltaOp::Replace(
            serde_json::to_value(&snapshot).expect("snapshot json")
        ))]
    );

    // The format-selecting wrappers cover BOTH members the server publishes:
    // progress and snapshot.
    use pir::protocol::{
        session_progress_update_in, session_snapshot_update_in, StateUpdateFormat,
        TranscriptProgress,
    };
    let progress = TranscriptProgress::AssistantDelta {
        message_id: "m-1".to_owned(),
        content_index: 0,
        kind: "text".to_owned(),
        delta: "hi".to_owned(),
    };
    let progress_delta = session_progress_update_in("s-1", &progress, 1, StateUpdateFormat::Delta);
    assert_eq!(progress_delta["member"], "progress");
    assert_eq!(progress_delta["ops"][0], serde_json::json!(["r", progress]));
    let progress_legacy =
        session_progress_update_in("s-1", &progress, 2, StateUpdateFormat::Legacy);
    assert_eq!(progress_legacy["ops"][0]["op"], "append");
    assert_eq!(
        progress_legacy["ops"][0]["value"],
        serde_json::json!(progress)
    );

    let snapshot_legacy =
        session_snapshot_update_in("s-1", &snapshot, 3, StateUpdateFormat::Legacy);
    assert_eq!(snapshot_legacy["ops"][0]["op"], "set");
    let snapshot_delta = session_snapshot_update_in("s-1", &snapshot, 4, StateUpdateFormat::Delta);
    assert_eq!(snapshot_delta["ops"][0][0], "r");
    assert_eq!(
        snapshot_delta["ops"][0][1],
        serde_json::to_value(&snapshot).expect("snapshot json")
    );
    assert_eq!(StateUpdateFormat::default().as_str(), "delta");
    assert_eq!(StateUpdateFormat::Legacy.as_str(), "legacy");
}

#[test]
fn state_update_decoder_retains_path_ids_across_updates() {
    use pir::protocol::{ServiceUpdate, StateUpdateDecoder};

    let first = session_state_delta_update(
        "s-1",
        "messages",
        1,
        &[
            serde_json::json!(["#", 0, ["messages", 0, "text"]]),
            serde_json::json!(["s", 0, "hello"]),
        ],
    );
    // The second update references id 0 defined by the first batch.
    let second = session_state_delta_update(
        "s-1",
        "messages",
        2,
        &[serde_json::json!(["s", 0, " world"])],
    );

    let mut decoder = StateUpdateDecoder::new();
    decoder.decode(&first).expect("first batch");
    let parsed = decoder.decode(&second).expect("retained path id");
    match parsed {
        ServiceUpdate::State(state) => {
            assert_eq!(state.sequence, 2);
            assert_eq!(
                state.ops[0],
                StateOp::Delta(DeltaOp::Set(
                    vec![
                        DeltaSeg::Key("messages".to_owned()),
                        DeltaSeg::Index(0),
                        DeltaSeg::Key("text".to_owned())
                    ],
                    serde_json::json!(" world")
                ))
            );
        }
        other => panic!("expected a pir state update, got {other:?}"),
    }
    // The one-shot parser has no cross-update dictionary, and `forget`
    // drops the stream's decoder.
    assert!(parse_service_provider_update(&second).is_err());
    decoder.forget("pir:s-1:messages");
    assert!(decoder.decode(&second).is_err());
}

/// §6 #17: a server-style snapshot stream — a base replacement followed by
/// batches that define `#` path ids and reuse them from a LATER update — is
/// applied end-to-end by the client-side state decoder.
#[test]
fn server_snapshot_delta_stream_applies_cross_update_path_ids() {
    use pir::protocol::{ServiceUpdate, StateUpdateDecoder};

    let snapshot = SessionSnapshot {
        id: "s-1".to_owned(),
        name: None,
        cwd: "/tmp".to_owned(),
        created_at: 1,
        updated_at: 2,
        phase: "idle".to_owned(),
        model: ModelRef {
            provider: "p".to_owned(),
            id: "m".to_owned(),
        },
        thinking_level: "off".to_owned(),
        attached: true,
        locked: false,
        revision: 1,
        transcript: Vec::new(),
        queued_steer: Vec::new(),
        queued_steer_count: 0,
    };
    let base = session_snapshot_delta_update("s-1", &snapshot, 1);
    let interned = session_state_delta_update(
        "s-1",
        "snapshot",
        2,
        &[
            serde_json::json!(["#", 0, ["revision"]]),
            serde_json::json!(["s", 0, 2]),
            serde_json::json!(["#", 1, ["phase"]]),
            serde_json::json!(["s", 1, "turn"]),
        ],
    );
    // The follow-up names only the ids defined by the previous update.
    let follow_up = session_state_delta_update(
        "s-1",
        "snapshot",
        3,
        &[
            serde_json::json!(["s", 0, 3]),
            serde_json::json!(["s", 1, "idle"]),
        ],
    );

    let mut decoder = StateUpdateDecoder::new();
    let mut value = Value::Null;
    for update in [&base, &interned, &follow_up] {
        let parsed = decoder
            .decode(update)
            .unwrap_or_else(|error| panic!("decode {update}: {error}"));
        let ServiceUpdate::State(state) = parsed else {
            panic!("expected a pir state update");
        };
        let ops: Vec<DeltaOp> = state
            .ops
            .into_iter()
            .map(|op| match op {
                StateOp::Delta(op) => op,
                other => panic!("expected a delta op, got {other:?}"),
            })
            .collect();
        apply_delta(&mut value, &ops).expect("apply server delta batch");
    }
    assert_eq!(value["revision"], serde_json::json!(3));
    assert_eq!(value["phase"], serde_json::json!("idle"));

    // The cross-update reference is only resolvable through the decoder; a
    // one-shot parse (fresh dictionary) rejects it.
    assert!(parse_service_provider_update(&follow_up).is_err());
}

#[test]
fn client_library_decodes_both_state_update_shapes() {
    use pir::client::decode_service_update;
    use pir::protocol::ServiceUpdate;

    let delta = session_state_delta_update(
        "s-1",
        "snapshot",
        3,
        &[serde_json::json!(["r", {"id": "s-1"}])],
    );
    match decode_service_update(&delta).expect("pir state update") {
        ServiceUpdate::State(state) => {
            assert_eq!(state.member, "snapshot");
            assert_eq!(state.sequence, 3);
            assert!(matches!(state.ops[0], StateOp::Delta(DeltaOp::Replace(_))));
        }
        other => panic!("expected a pir state update, got {other:?}"),
    }

    let chord = serde_json::json!({
        "t": "state",
        "hold": 9,
        "m": "messages",
        "as": "ops",
        "ops": [["s", ["messages", 0, "text"], "hi"]],
    });
    match decode_service_update(&chord).expect("chord state update") {
        ServiceUpdate::Chord(state) => {
            assert_eq!(state.hold, 9);
            assert_eq!(state.kind, ChordStateKind::Ops);
            assert!(matches!(state.ops[0], DeltaOp::Set(_, _)));
        }
        other => panic!("expected a chord state update, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Fanout backpressure vs. the control plane (R5 #5)
// ---------------------------------------------------------------------------

/// A subscriber that stops reading fills its bounded frame channel and blocks
/// the service-update forwarder inside `send().await`. The forwarder must not
/// hold the subscriptions read lock across that await: the same connection's
/// `unsubscribe` has to reach the map (taking the write lock) even while the
/// forwarder is stuck.
///
/// Observable: with the forwarder blocked the unsubscribe response cannot be
/// enqueued (the frame channel is full), so the server closes the connection
/// through the existing "undeliverable response" path. Before the fix the read
/// loop was stuck on the write lock and the socket never closed.
#[tokio::test]
async fn slow_subscriber_does_not_block_unsubscribe_on_the_control_plane() {
    let security = ServerSecurityConfig {
        max_sessions: 1024,
        ..ServerSecurityConfig::default()
    };
    let server =
        Server::bind_with_state_and_security("127.0.0.1:0", ServerState::default(), security)
            .await
            .expect("bind slow-subscriber server");
    let address = server.local_addr().expect("local address");
    let server = Arc::new(server);

    let accepting_slow = {
        let server = server.clone();
        tokio::spawn(async move { server.accept_once().await })
    };
    let mut slow = Conn::connect(address).await;
    let slow_server_id = slow.handshake().await;
    slow.send(&request_envelope(
        "sub1",
        serde_json::json!({"serverId": slow_server_id}),
        control_call(
            "subscribe",
            serde_json::json!(["sub-1", SESSION_SERVICE_ID, "singleton"]),
        ),
    ))
    .await;
    let response = slow.read().await;
    assert_eq!(response["ok"], true, "subscribe failed: {response}");

    let accepting_controller = {
        let server = server.clone();
        tokio::spawn(async move { server.accept_once().await })
    };
    let mut controller = Conn::connect(address).await;
    let controller_server_id = controller.handshake().await;

    // One session, big enough that the per-update snapshot cannot be
    // absorbed by the socket buffers. Every set_thinking publishes another
    // snapshot update, so a few hundred of them park the forwarder on the full
    // per-connection frame channel.
    let name = "n".repeat(64 * 1024);
    controller
        .send(&request_envelope(
            "c0",
            serde_json::json!({"serverId": controller_server_id}),
            session_call("create", serde_json::json!([{ "name": name }])),
        ))
        .await;
    let response = controller.read().await;
    assert_eq!(response["ok"], true, "create failed: {response}");
    let session_id = response["result"]["session"]["id"]
        .as_str()
        .expect("session id")
        .to_owned();
    controller
        .send(&request_envelope(
            "a0",
            serde_json::json!({"serverId": controller_server_id}),
            session_call("attach", serde_json::json!([{"sessionId": session_id}])),
        ))
        .await;
    let attachment = controller.read().await;
    assert_eq!(attachment["type"], "attachment");
    let leased = attachment["attachment"]["attachmentId"]
        .as_str()
        .expect("attachmentId")
        .to_owned();
    let response = controller.read().await;
    assert_eq!(response["ok"], true, "attach failed: {response}");

    for index in 0..256 {
        controller
            .send(&request_envelope(
                &format!("t{index}"),
                serde_json::json!({
                    "serverId": controller_server_id,
                    "sessionId": session_id,
                    "attachmentId": leased,
                }),
                session_call(
                    "set_thinking",
                    serde_json::json!([{"thinkingLevel": if index % 2 == 0 { "low" } else { "high" }}]),
                ),
            ))
            .await;
        let response = controller.read().await;
        assert_eq!(response["ok"], true, "set_thinking {index}: {response}");
    }

    // Give the forwarder time to drain the broadcast into the full frame
    // channel and park inside send(). Do NOT read from `slow` yet.
    tokio::time::sleep(Duration::from_millis(200)).await;

    slow.send(&request_envelope(
        "u1",
        serde_json::json!({"serverId": slow_server_id}),
        control_call("unsubscribe", serde_json::json!(["sub-1"])),
    ))
    .await;
    // Same again after the request is on the wire: the server must process it
    // while the client is still not reading.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let mut drained = 0_usize;
    let closed = tokio::time::timeout(Duration::from_secs(10), async {
        let mut buffer = [0_u8; 16 * 1024];
        loop {
            match slow.reader.read(&mut buffer).await {
                Ok(0) | Err(_) => break,
                Ok(read) => drained += read,
            }
        }
    })
    .await;
    assert!(
        drained > 1024 * 1024,
        "the slow subscriber must have a visibly backed-up update stream (drained {drained} bytes)"
    );
    assert!(
        closed.is_ok(),
        "unsubscribe was blocked by the fanout read lock: the connection stayed open"
    );

    drop(controller);
    let slow_result = accepting_slow.await.expect("slow accept task");
    assert!(
        slow_result.is_err(),
        "the undeliverable unsubscribe response must close the slow connection"
    );
    accepting_controller
        .await
        .expect("controller accept task")
        .expect("serve controller client");
}

/// Two subscriptions on one connection each receive the same broadcast stream
/// in the same order: snapshotting the map before sending must not reorder or
/// duplicate an update per subscription.
#[tokio::test]
async fn service_updates_preserve_per_subscription_order_and_content() {
    let (server, address) = bind_server().await;
    let accepting = {
        let server = server.clone();
        tokio::spawn(async move { server.accept_once().await })
    };
    let mut conn = Conn::connect(address).await;
    let server_id = conn.handshake().await;
    let (session_id, leased) = create_and_attach(&mut conn, &server_id).await;

    for subscription in ["sub-a", "sub-b"] {
        conn.send(&request_envelope(
            subscription,
            serde_json::json!({"serverId": server_id}),
            control_call(
                "subscribe",
                serde_json::json!([subscription, SESSION_SERVICE_ID, "singleton"]),
            ),
        ))
        .await;
        let response = conn.read().await;
        assert_eq!(response["ok"], true, "subscribe {subscription}: {response}");
    }

    conn.send(&request_envelope(
        "p1",
        serde_json::json!({
            "serverId": server_id,
            "sessionId": session_id,
            "attachmentId": leased,
        }),
        session_call("prompt", serde_json::json!([{"text": "hello"}])),
    ))
    .await;

    let mut updates: Vec<(String, Value)> = Vec::new();
    let mut saw_error_response = false;
    for _ in 0..16 {
        let frame = conn.read().await;
        match frame["type"].as_str().expect("frame type") {
            "service_update" => {
                let subscription = frame["subscriptionId"]
                    .as_str()
                    .expect("subscriptionId")
                    .to_owned();
                updates.push((subscription, frame["update"].clone()));
            }
            "response" => {
                assert_eq!(frame["id"], "p1");
                assert_eq!(frame["ok"], false, "prompt without a provider fails");
                saw_error_response = true;
            }
            other => panic!("unexpected frame type {other}: {frame}"),
        }
        if saw_error_response
            && updates.iter().filter(|(sub, _)| sub == "sub-a").count() >= 2
            && updates.iter().filter(|(sub, _)| sub == "sub-b").count() >= 2
        {
            break;
        }
    }
    assert!(saw_error_response, "prompt response never arrived");

    let sub_a: Vec<Value> = updates
        .iter()
        .filter(|(sub, _)| sub == "sub-a")
        .map(|(_, update)| update.clone())
        .collect();
    let sub_b: Vec<Value> = updates
        .iter()
        .filter(|(sub, _)| sub == "sub-b")
        .map(|(_, update)| update.clone())
        .collect();
    assert!(
        !sub_a.is_empty() && !sub_b.is_empty(),
        "both subscribers got updates"
    );
    assert_eq!(
        sub_a, sub_b,
        "both subscriptions of one connection must receive the same ordered updates"
    );
    assert_eq!(
        sub_a.last().and_then(|update| {
            snapshot_payload(update)["phase"]
                .as_str()
                .map(str::to_owned)
        }),
        Some("idle".to_owned()),
        "the final update is the settled idle snapshot"
    );

    drop(conn);
    accepting
        .await
        .expect("accept task join")
        .expect("serve client");
}

// ---------------------------------------------------------------------------
// D12: wire-driven dictionaries are bounded
// ---------------------------------------------------------------------------

/// A delta stream may not grow its path-id dictionary past
/// [`pir::protocol::MAX_DELTA_PATH_IDS`]; a base batch (`r`) clears it, so a
/// legitimate producer can always replay. A new id past the cap is a
/// structured error, never a panic.
#[test]
fn delta_path_dictionary_is_bounded() {
    let mut decoder = pir::protocol::DeltaDecoder::new();
    let mut definitions = Vec::new();
    for id in 0..pir::protocol::MAX_DELTA_PATH_IDS {
        definitions.push(serde_json::json!(["#", id, [format!("key-{id}")]]));
    }
    decoder
        .decode(&definitions)
        .expect("definitions up to the cap are accepted");

    let over = vec![serde_json::json!([
        "#",
        pir::protocol::MAX_DELTA_PATH_IDS,
        ["overflow"]
    ])];
    let error = decoder
        .decode(&over)
        .expect_err("a new id past the cap must be rejected");
    assert!(
        matches!(error, pir::protocol::DeltaError::Shape(_)),
        "expected a structured shape error, got {error:?}"
    );

    // A base batch is the recovery point: it clears the dictionary, so the
    // same id becomes definable again.
    decoder
        .decode(&[serde_json::json!(["r", {}])])
        .expect("base batch");
    decoder
        .decode(&[serde_json::json!(["#", 0, ["fresh-path"]])])
        .expect("the dictionary is usable again after a base batch");
}

/// [`pir::protocol::StateUpdateDecoder`] keeps at most
/// [`pir::protocol::MAX_STATE_UPDATE_STREAMS`] stream dictionaries (LRU). An
/// evicted stream's path ids no longer resolve, which surfaces as a structured
/// error telling the producer to replay from a base batch.
#[test]
fn state_update_stream_map_is_bounded_and_evicts_the_least_recently_used() {
    let mut decoder = pir::protocol::StateUpdateDecoder::new();
    let define_first = serde_json::json!({
        "type": "state",
        "member": "snapshot",
        "sequence": 1,
        "instance": {"key": "first", "generation": 1},
        "ops": [["#", 0, ["path"]]],
    });
    decoder.decode(&define_first).expect("first stream");

    for index in 0..pir::protocol::MAX_STATE_UPDATE_STREAMS {
        let update = serde_json::json!({
            "type": "state",
            "member": "snapshot",
            "sequence": 1,
            "instance": {"key": format!("stream-{index}"), "generation": 1},
            "ops": [["r", {}]],
        });
        decoder.decode(&update).expect("stream admitted");
    }
    assert!(
        decoder.stream_count() <= pir::protocol::MAX_STATE_UPDATE_STREAMS,
        "the stream map must never exceed its cap"
    );

    // "first" was the least recently used stream and was evicted; referencing
    // its path id must fail with the base-replay error instead of resolving.
    let follow_up = serde_json::json!({
        "type": "state",
        "member": "snapshot",
        "sequence": 2,
        "instance": {"key": "first", "generation": 1},
        "ops": [["s", 0, 42]],
    });
    let error = decoder
        .decode(&follow_up)
        .expect_err("the evicted dictionary must not resolve stale path ids");
    assert!(
        error.to_string().contains("unresolvable path"),
        "expected the base-replay path error, got {error:?}"
    );
}
