//! Cancel-frame semantics and the handshake deadline (upstream
//! `Server.handleCancel` server.ts:298-304 + the aborted request's terminal
//! `{code:"cancelled"}` response server.ts:394-400, `DEFAULT_HANDSHAKE_TIMEOUT_MS`
//! server.ts:42):
//!
//! * a `cancel` frame never produces a response for its own id — the
//!   cancelled prompt answers with the `cancelled` error on ITS request id,
//!   and nothing further may follow for that id (an unowned response would
//!   make an upstream client fail the whole connection, client.ts:333-337);
//! * a stale/unknown cancel id is silently ignored and the connection stays
//!   usable;
//! * a connection that never completes the greeting is closed by the
//!   handshake deadline with `hello_error{invalid_request, "Handshake
//!   timeout"}` instead of holding its slot for the idle timeout.
//!
//! Manual run: `cargo test --test server_cancel_fix` (the handshake test
//! takes ~5s by design — the deadline is a fixed policy constant).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use pir::ai::auth::ApiKeyAuth;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::types::{
    AssistantMessage, ContentBlock, Context, ErrorReason, Model, ModelCost, ModelCostRates,
    StopReason, StreamOptions, TerminalReason, TextContent,
};
use pir::client::{Client, ClientError};
use pir::protocol::{
    encode_client_value, FrameOptions, ModelRef, RpcTarget, ServerMessageDecoder, PROTOCOL_VERSION,
    SESSION_SERVICE_ID,
};
use pir::server::{empty_snapshot, Server, ServerState};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::watch;

const WAIT_BUDGET: Duration = Duration::from_secs(10);

/// Scripted model catalog entry (same shape as `server_prompt_abort.rs`).
fn fake_model() -> Model {
    Model {
        id: "fake-model".into(),
        name: "Fake model".into(),
        api: "fake-api".into(),
        provider: "fake".into(),
        base_url: "http://localhost".into(),
        reasoning: false,
        thinking_level_map: None,
        input: vec!["text".into()],
        cost: ModelCost {
            rates: ModelCostRates {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            tiers: Vec::new(),
        },
        context_window: 4096,
        max_tokens: 256,
        sampling_params: None,
        headers: None,
        thinking_token_budget_field: None,
        input_limits: None,
        prompt_cache: None,
        sampling_params_by_thinking_level: None,
        model_type: None,
        compat: None,
    }
}

fn text(text: &str) -> ContentBlock {
    ContentBlock::Text(TextContent {
        text: text.into(),
        text_signature: None,
    })
}

fn assistant(content: Vec<ContentBlock>, stop_reason: StopReason) -> AssistantMessage {
    AssistantMessage {
        api: "fake".into(),
        provider: "fake".into(),
        model: "fake-model".into(),
        content,
        stop_reason,
        timestamp: 1,
        ..Default::default()
    }
}

/// A provider whose stream only terminates when the shared gate opens or the
/// run's abort signal fires (whichever comes first): a prompt against it
/// stays in flight until the server cancels it.
struct GatedProvider {
    gate: watch::Receiver<bool>,
    entered: Arc<AtomicUsize>,
}

impl Provider for GatedProvider {
    fn id(&self) -> &str {
        "fake"
    }
    fn name(&self) -> &str {
        "Gated fake provider"
    }
    fn get_models(&self) -> Vec<Model> {
        vec![fake_model()]
    }
    fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
        vec![ApiKeyAuth::new("fake key", vec!["FAKE_TEST_KEY".into()])]
    }
    fn stream(
        &self,
        _model: &Model,
        _context: Context,
        options: StreamOptions,
    ) -> AssistantMessageEventStream {
        self.entered.fetch_add(1, Ordering::SeqCst);
        let stream = AssistantMessageEventStream::new();
        let pusher = stream.clone();
        let mut gate = self.gate.clone();
        let signal = options.signal;
        tokio::spawn(async move {
            loop {
                if signal.as_ref().is_some_and(|signal| signal.is_aborted()) {
                    pusher.push(AssistantMessageEventStream::error_event(
                        ErrorReason::Aborted,
                        assistant(vec![text("aborted")], StopReason::Aborted),
                    ));
                    return;
                }
                if *gate.borrow() {
                    pusher.push(AssistantMessageEventStream::done_event(
                        TerminalReason::Stop,
                        assistant(vec![text("done")], StopReason::Stop),
                    ));
                    return;
                }
                if tokio::time::timeout(Duration::from_millis(20), gate.changed())
                    .await
                    .is_err()
                {
                    continue;
                }
            }
        });
        stream
    }
}

/// Bind a server whose sessions run against the gated provider.
async fn bind_gated_server(gate: watch::Receiver<bool>) -> (Server, Arc<AtomicUsize>) {
    let entered = Arc::new(AtomicUsize::new(0));
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(GatedProvider {
        gate,
        entered: Arc::clone(&entered),
    }));
    let state = ServerState::with_providers(
        empty_snapshot(uuid::Uuid::new_v4().to_string()),
        Arc::new(registry),
    )
    .with_security(pir::server::ServerSecurityConfig::default());
    let server = Server::bind_with_state("127.0.0.1:0", state)
        .await
        .expect("bind gated test server");
    (server, entered)
}

/// Create a session against the gated provider and attach the client.
async fn create_and_attach(client: &mut Client) -> (String, String) {
    let created = client
        .request(pir::protocol::Command::Create {
            cwd: None,
            name: None,
            model: Some(ModelRef {
                provider: "fake".to_owned(),
                id: "fake-model".to_owned(),
            }),
            thinking_level: None,
        })
        .await
        .expect("create response");
    let session_id = created["session"]["id"]
        .as_str()
        .expect("session id")
        .to_owned();
    client
        .request(pir::protocol::Command::Attach {
            session_id: session_id.clone(),
        })
        .await
        .expect("attach response");
    let attachment = client
        .attachment_id()
        .expect("attachment id leased at attach")
        .to_owned();
    (session_id, attachment)
}

/// Poll the registry until the session's phase matches.
async fn wait_for_phase(state: &ServerState, session_id: &str, phase: &str) {
    for _ in 0..2000 {
        if let Some(snapshot) = state.get_session(session_id).await {
            if snapshot.phase == phase {
                return;
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("session {session_id} never entered phase {phase}");
}

/// The cancel frame produces NO response of its own: the cancelled prompt's
/// single terminal response is `{ok:false, code:"cancelled"}` on the PROMPT
/// request id, and no further response may follow for that id. The session
/// recovers for a follow-up prompt.
#[tokio::test]
async fn cancel_frame_yields_one_cancelled_terminal_response() {
    let (gate_tx, gate_rx) = watch::channel(false);
    let (server, entered) = bind_gated_server(gate_rx).await;
    let state = server.state().clone();
    let address = server.local_addr().expect("local address");
    let accepting = tokio::spawn(async move { server.accept_once().await });

    let mut client = Client::connect(address).await.expect("hello handshake");
    let (session_id, attachment) = create_and_attach(&mut client).await;
    let target = RpcTarget::session(client.server_id(), session_id.clone(), attachment);

    let prompt_id = client
        .start_call(
            target.clone(),
            SESSION_SERVICE_ID,
            "prompt",
            vec![serde_json::json!({"text": "cancel me"})],
        )
        .await
        .expect("prompt call sent");
    wait_for_phase(&state, &session_id, "turn").await;
    assert!(
        entered.load(Ordering::SeqCst) >= 1,
        "the gated provider stream must have started"
    );

    // `Client::cancel` sends the cancel frame and waits for the response of
    // the CANCELLED request: the terminal `cancelled` error must arrive (no
    // ack for the cancel frame, no hang).
    let outcome = tokio::time::timeout(WAIT_BUDGET, client.cancel(&prompt_id, target))
        .await
        .expect("the cancelled prompt's terminal response must arrive");
    let error = outcome.expect_err("a cancelled prompt must not report success");
    assert!(
        matches!(error, ClientError::Server { ref code, .. } if code == "cancelled"),
        "expected the `cancelled` error code, got {error:?}"
    );

    // No second response may follow for the same id (an unowned response
    // fails an upstream client's whole connection).
    let extra =
        tokio::time::timeout(Duration::from_millis(500), client.wait_call(&prompt_id)).await;
    assert!(
        extra.is_err(),
        "the cancel id must carry exactly one terminal response"
    );

    let snapshot = state
        .get_session(&session_id)
        .await
        .expect("session still registered");
    assert_eq!(
        snapshot.phase, "idle",
        "cancel must return the phase to idle"
    );

    // The session remains fully usable after the cancellation.
    gate_tx.send(true).expect("gate open");
    let follow_up = tokio::time::timeout(
        WAIT_BUDGET,
        client.request(pir::protocol::Command::Prompt {
            session_id: session_id.clone(),
            text: "after cancel".to_owned(),
        }),
    )
    .await
    .expect("follow-up prompt completes")
    .expect("follow-up prompt ok");
    assert_eq!(follow_up["command"], "prompt");

    drop(client);
    let _ = tokio::time::timeout(WAIT_BUDGET, accepting)
        .await
        .expect("accept task join")
        .expect("serve client");
}

/// A raw framed connection with frame coalescing handled explicitly.
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

    /// Read the next frame within `budget`; `None` when none arrived.
    async fn read_within(&mut self, budget: Duration) -> Option<Value> {
        if let Some(value) = self.pending.pop_front() {
            return Some(value);
        }
        let mut buffer = [0_u8; 8192];
        let deadline = tokio::time::Instant::now() + budget;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            let read = tokio::time::timeout(remaining, self.reader.read(&mut buffer)).await;
            let Ok(Ok(read)) = read else {
                return None;
            };
            assert!(read > 0, "server closed the connection");
            for message in self.decoder.push(&buffer[..read]).expect("decode frame") {
                self.pending.push_back(message);
            }
            if let Some(value) = self.pending.pop_front() {
                return Some(value);
            }
        }
    }

    async fn handshake(&mut self) -> String {
        self.send(&serde_json::json!({"type": "hello", "version": PROTOCOL_VERSION}))
            .await;
        let hello = self
            .read_within(Duration::from_secs(5))
            .await
            .expect("hello response");
        assert_eq!(hello["type"], "hello");
        hello["serverId"].as_str().expect("serverId").to_owned()
    }
}

/// A cancel frame naming an unknown request is silently ignored (upstream
/// `activeRequests.get(id)` miss): no response, no disconnect, and the
/// connection keeps serving.
#[tokio::test]
async fn stale_cancel_id_is_ignored_and_connection_stays_usable() {
    let server = Server::bind("127.0.0.1:0").await.expect("bind server");
    let address = server.local_addr().expect("local address");
    let accepting = tokio::spawn(async move { server.accept_once().await });

    let mut conn = Conn::connect(address).await;
    let server_id = conn.handshake().await;

    conn.send(&serde_json::json!({
        "type": "cancel",
        "id": "never-issued",
        "target": {
            "serverId": server_id,
            "sessionId": "some-session",
            "attachmentId": "some-attachment",
        },
    }))
    .await;
    assert!(
        conn.read_within(Duration::from_millis(500)).await.is_none(),
        "a cancel for an unknown id must not produce any frame"
    );

    // The connection is still live: a list request round-trips.
    conn.send(&serde_json::json!({
        "type": "request",
        "id": "l1",
        "target": {"serverId": server_id},
        "call": {"serviceId": SESSION_SERVICE_ID, "member": "list", "args": []},
    }))
    .await;
    let response = conn
        .read_within(Duration::from_secs(5))
        .await
        .expect("list response after an ignored cancel");
    assert_eq!(response["id"], "l1");
    assert_eq!(response["ok"], true, "list failed: {response}");

    drop(conn);
    let _ = tokio::time::timeout(WAIT_BUDGET, accepting)
        .await
        .expect("accept task join")
        .expect("serve client");
}

/// `Client::cancel` for an id whose request already settled (or was never
/// issued) answers immediately with the local `cancelled` outcome: the server
/// silently ignores stale cancel ids and never answers the cancel frame, so
/// waiting for a response would burn the full request timeout (upstream
/// cleans the pending request up locally and fires the cancel frame without
/// awaiting, client.ts `onAbort`).
#[tokio::test]
async fn client_cancel_for_a_settled_request_returns_without_waiting() {
    let server = Server::bind("127.0.0.1:0").await.expect("bind server");
    let address = server.local_addr().expect("local address");
    let accepting = tokio::spawn(async move { server.accept_once().await });

    let mut client = Client::connect(address).await.expect("hello handshake");
    let target = RpcTarget::server(client.server_id());

    // A request that settles retires its id from the client's pending table.
    let id = client
        .start_call(target.clone(), SESSION_SERVICE_ID, "list", Vec::new())
        .await
        .expect("list call sent");
    client.wait_call(&id).await.expect("list completes");

    // Cancelling the expired id must return promptly (a 30s request-timeout
    // wait would fail this 2s bound).
    let outcome = tokio::time::timeout(Duration::from_secs(2), client.cancel(&id, target.clone()))
        .await
        .expect("an expired cancel must not wait for a response");
    let error = outcome.expect_err("an expired cancel is not a success");
    assert!(
        matches!(error, ClientError::Server { ref code, .. } if code == "cancelled"),
        "expected the `cancelled` code, got {error:?}"
    );

    // An id that was never issued behaves the same.
    let outcome = tokio::time::timeout(
        Duration::from_secs(2),
        client.cancel("never-issued", target),
    )
    .await
    .expect("an unknown cancel must not wait for a response");
    assert!(outcome.is_err(), "an unknown cancel is not a success");

    drop(client);
    let _ = tokio::time::timeout(WAIT_BUDGET, accepting)
        .await
        .expect("accept task join")
        .expect("serve client");
}

/// A connection that connects but never greets is closed by the handshake
/// deadline (`DEFAULT_HANDSHAKE_TIMEOUT`, 5s) with upstream's
/// `hello_error{invalid_request, "Handshake timeout"}`, instead of holding
/// its connection slot for the idle timeout (slowloris).
#[tokio::test]
async fn pre_greeting_connection_is_closed_by_the_handshake_deadline() {
    let server = Server::bind("127.0.0.1:0").await.expect("bind server");
    let address = server.local_addr().expect("local address");
    let accepting = tokio::spawn(async move { server.accept_once().await });

    let mut stream = TcpStream::connect(address).await.expect("connect");
    // Say nothing: the pre-greeting phase must expire on its own deadline.
    let mut decoder = ServerMessageDecoder::new(FrameOptions::default()).expect("decoder");
    let mut buffer = [0_u8; 8192];
    let error = loop {
        let read = tokio::time::timeout(WAIT_BUDGET, stream.read(&mut buffer))
            .await
            .expect("the handshake deadline must fire within the budget")
            .expect("read socket");
        assert!(read > 0, "server closed before the hello_error");
        let messages = decoder.push(&buffer[..read]).expect("decode frame");
        if let Some(message) = messages.into_iter().next() {
            break message;
        }
    };
    assert_eq!(error["type"], "hello_error");
    assert_eq!(error["error"]["code"], "invalid_request");
    assert_eq!(error["error"]["message"], "Handshake timeout");

    // The server then closes the connection.
    let read = tokio::time::timeout(WAIT_BUDGET, stream.read(&mut buffer))
        .await
        .expect("the server must close after the hello_error")
        .expect("read socket");
    assert_eq!(read, 0, "connection must be closed after the timeout frame");

    let _ = tokio::time::timeout(WAIT_BUDGET, accepting)
        .await
        .expect("accept task join")
        .expect("serve client");
}
