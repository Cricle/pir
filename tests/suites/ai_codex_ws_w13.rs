//! W13: Codex WebSocket production wiring.
//!
//! Local mock servers only (127.0.0.1), no network. Covers:
//! - the `transport` setting reaching the adapter (`sse` skips the WebSocket
//!   attempt, `websocket` pools but sends full history, `auto`/`websocket-cached`
//!   reuse the continuation) and the `websocketConnectTimeoutMs` knob;
//! - the same two settings resolved by a real [`AgentSession`] from a pinned
//!   settings file (the settings → session → request-options → adapter path);
//! - `previous_response_not_found` retried only before the request's `Start`,
//!   never appending a second response to a started parser;
//! - teardown keyed on the CLAMPED continuation key, so a session id longer
//!   than the `prompt_cache_key` limit still cleans up;
//! - `AgentSession::stream_options` carrying a stable, unique `session_id`.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use pir::agent::tool_execution::ToolRegistry;
use pir::agent_session::{AgentSession, AgentSessionConfig};
use pir::ai::api::openai_responses::codex::codex_ws;
use pir::ai::api::openai_responses::OpenAiCodexResponsesProvider;
use pir::ai::auth::ApiKeyAuth;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::types::*;
use pir::config::settings::TransportSetting;
use pir::session_manager::SessionManager;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::accept_hdr_async;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio_tungstenite::tungstenite::Message as WsMessage;

// ---------------------------------------------------------------------------
// Mock server: serves scripted Responses events over WebSocket (one script per
// received `response.create` frame, across all connections) and a fixed SSE
// body over HTTP.
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct RecordedRequest {
    /// Captured for parity assertions that are not currently exercised; kept so
    /// a future test can inspect handshake/request headers without re-recording.
    #[allow(dead_code)]
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
    websocket: bool,
}

struct ServerState {
    /// Responses served for the Nth `response.create` frame (across
    /// connections); a frame past the end gets an empty response so the client
    /// notices a script mismatch quickly.
    turns: Mutex<Vec<Vec<Value>>>,
    served: AtomicUsize,
    sse_body: String,
    /// Never answer the WebSocket handshake, so the client's connect timeout is
    /// what ends the attempt.
    stall_handshake: bool,
    recorded: Mutex<Vec<RecordedRequest>>,
    handshakes: AtomicUsize,
}

impl ServerState {
    fn scripted(turns: Vec<Vec<Value>>) -> Arc<Self> {
        Arc::new(Self {
            turns: Mutex::new(turns),
            served: AtomicUsize::new(0),
            sse_body: String::new(),
            stall_handshake: false,
            recorded: Mutex::new(Vec::new()),
            handshakes: AtomicUsize::new(0),
        })
    }

    fn scripted_with_sse(turns: Vec<Vec<Value>>, sse_body: String) -> Arc<Self> {
        Arc::new(Self {
            turns: Mutex::new(turns),
            served: AtomicUsize::new(0),
            sse_body,
            stall_handshake: false,
            recorded: Mutex::new(Vec::new()),
            handshakes: AtomicUsize::new(0),
        })
    }

    fn stalling(sse_body: String) -> Arc<Self> {
        Arc::new(Self {
            turns: Mutex::new(Vec::new()),
            served: AtomicUsize::new(0),
            sse_body,
            stall_handshake: true,
            recorded: Mutex::new(Vec::new()),
            handshakes: AtomicUsize::new(0),
        })
    }

    fn recorded(&self) -> Vec<RecordedRequest> {
        self.recorded.lock().unwrap().clone()
    }

    fn frames_containing(&self, needle: &str) -> usize {
        self.recorded()
            .iter()
            .filter(|request| {
                request.websocket && String::from_utf8_lossy(&request.body).contains(needle)
            })
            .count()
    }
}

async fn spawn_server(state: Arc<ServerState>) -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let handle = tokio::spawn(async move {
        loop {
            let Ok((socket, _)) = listener.accept().await else {
                break;
            };
            let state = state.clone();
            tokio::spawn(async move {
                handle_connection(socket, state).await;
            });
        }
    });
    (format!("http://{addr}"), handle)
}

async fn handle_connection(socket: TcpStream, state: Arc<ServerState>) {
    let mut peek = [0u8; 16 * 1024];
    let read = match socket.peek(&mut peek).await {
        Ok(read) => read,
        Err(_) => return,
    };
    let head = String::from_utf8_lossy(&peek[..read]).to_ascii_lowercase();
    if head.contains("upgrade: websocket") {
        handle_websocket(socket, state).await;
    } else {
        handle_http(socket, state).await;
    }
}

#[allow(clippy::result_large_err)]
async fn handle_websocket(socket: TcpStream, state: Arc<ServerState>) {
    state.handshakes.fetch_add(1, Ordering::SeqCst);
    if state.stall_handshake {
        // Accept the TCP connection but never complete the upgrade: only the
        // client's connect-handshake timeout can end this attempt.
        tokio::time::sleep(Duration::from_secs(30)).await;
        return;
    }
    let handshake_headers = Arc::new(Mutex::new(BTreeMap::new()));
    let slot = handshake_headers.clone();
    let mut ws = match accept_hdr_async(socket, move |request: &Request, response: Response| {
        let mut headers = BTreeMap::new();
        for (name, value) in request.headers() {
            headers.insert(
                name.as_str().to_ascii_lowercase(),
                value.to_str().unwrap_or_default().to_string(),
            );
        }
        *slot.lock().unwrap() = headers;
        Ok(response)
    })
    .await
    {
        Ok(ws) => ws,
        Err(_) => return,
    };

    let headers = handshake_headers.lock().unwrap().clone();
    while let Some(message) = ws.next().await {
        let Ok(WsMessage::Text(text)) = message else {
            break;
        };
        state.recorded.lock().unwrap().push(RecordedRequest {
            headers: headers.clone(),
            body: text.into_bytes(),
            websocket: true,
        });
        let index = state.served.fetch_add(1, Ordering::SeqCst);
        let script = state
            .turns
            .lock()
            .unwrap()
            .get(index)
            .cloned()
            .unwrap_or_default();
        for event in script {
            if ws.send(WsMessage::Text(event.to_string())).await.is_err() {
                return;
            }
        }
    }
}

async fn handle_http(socket: TcpStream, state: Arc<ServerState>) {
    let mut socket = socket;
    let Some((headers, body)) = read_http_request(&mut socket).await else {
        return;
    };
    state.recorded.lock().unwrap().push(RecordedRequest {
        headers,
        body,
        websocket: false,
    });
    let body = state.sse_body.as_bytes();
    let head = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    );
    let _ = socket.write_all(head.as_bytes()).await;
    let _ = socket.write_all(body).await;
    let _ = socket.shutdown().await;
}

async fn read_http_request(socket: &mut TcpStream) -> Option<(BTreeMap<String, String>, Vec<u8>)> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        let read = socket.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(position) = find_bytes(&buffer, b"\r\n\r\n") {
            break position + 4;
        }
    };
    let head = String::from_utf8_lossy(&buffer[..header_end]).to_string();
    let mut headers = BTreeMap::new();
    for line in head.split("\r\n").skip(1) {
        if line.is_empty() {
            continue;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }
    let content_length = headers
        .get("content-length")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    while buffer.len() < header_end + content_length {
        let read = socket.read(&mut chunk).await.ok()?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
    let body = buffer[header_end..(header_end + content_length).min(buffer.len())].to_vec();
    Some((headers, body))
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn codex_model(base_url: &str) -> Model {
    Model {
        id: "gpt-5-codex".into(),
        name: "gpt-5-codex".into(),
        api: "openai-codex-responses".into(),
        provider: "openai-codex".into(),
        base_url: base_url.to_string(),
        reasoning: true,
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
        context_window: 128_000,
        max_tokens: 16_384,
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

fn provider(base_url: &str) -> (OpenAiCodexResponsesProvider, Model) {
    let p =
        OpenAiCodexResponsesProvider::new("openai-codex", base_url, vec![codex_model(base_url)])
            .expect("provider");
    let model = p.get_models().remove(0);
    (p, model)
}

fn fake_jwt(account_id: &str) -> String {
    use base64::Engine;
    let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let header = engine.encode(r#"{"alg":"none","typ":"JWT"}"#);
    let payload = engine.encode(
        json!({"https://api.openai.com/auth": {"chatgpt_account_id": account_id}}).to_string(),
    );
    format!("{header}.{payload}.signature")
}

fn stream_options(session_id: &str, transport: TransportSetting) -> StreamOptions {
    let mut metadata = BTreeMap::new();
    metadata.insert(codex_ws::METADATA_TRANSPORT.to_string(), json!(transport));
    StreamOptions {
        api_key: Some(fake_jwt("acct_123")),
        session_id: Some(session_id.to_string()),
        max_retries: Some(0),
        metadata: Some(metadata),
        ..StreamOptions::default()
    }
}

fn user_context(text: &str) -> Context {
    Context {
        messages: vec![user_message(text)],
        ..Context::default()
    }
}

/// The single user message [`user_context`] wraps, for multi-turn fixtures.
fn user_message(text: &str) -> Message {
    Message::User(UserMessage {
        role: UserRole::User,
        content: vec![ContentBlock::Text(TextContent {
            text: text.to_string(),
            text_signature: None,
        })],
        tools_added: None,
        tools_removed: None,
        timestamp: 0,
        sections: None,
    })
}

fn text_turn(response_id: &str, parts: &[&str]) -> Vec<Value> {
    let text = parts.concat();
    vec![
        json!({"type": "response.created", "response": {"id": response_id}}),
        json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": {"type": "message", "id": format!("msg_{response_id}")}
        }),
        json!({
            "type": "response.output_text.delta",
            "output_index": 0,
            "delta": text
        }),
        json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": {
                "type": "message",
                "id": format!("msg_{response_id}"),
                "content": [{"type": "output_text", "text": text}]
            }
        }),
        json!({
            "type": "response.completed",
            "response": {
                "id": response_id,
                "status": "completed",
                "usage": {"input_tokens": 10, "output_tokens": 2, "total_tokens": 12}
            }
        }),
    ]
}

fn sse_body(events: &[Value]) -> String {
    events
        .iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect()
}

async fn collect(mut stream: AssistantMessageEventStream) -> Vec<AssistantMessageEvent> {
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        events.push(event);
    }
    events
}

fn done_response_id(events: &[AssistantMessageEvent]) -> Option<String> {
    events.iter().find_map(|event| match event {
        AssistantMessageEvent::Done { message, .. } => message.response_id.clone(),
        _ => None,
    })
}

fn stream_error(events: &[AssistantMessageEvent]) -> Option<String> {
    events.iter().find_map(|event| match event {
        AssistantMessageEvent::Error { error, .. } => error.error_message.clone(),
        _ => None,
    })
}

// ---------------------------------------------------------------------------
// Transport plan (pure)
// ---------------------------------------------------------------------------

#[test]
fn codex_transport_plan_reads_the_settings_handoff() {
    // No metadata: upstream `options?.transport || "auto"` plus the default
    // handshake timeout.
    let plan = codex_ws::transport_options(&StreamOptions::default());
    assert_eq!(plan.transport, TransportSetting::Auto);
    assert_eq!(
        plan.connect_timeout,
        Some(Duration::from_millis(
            codex_ws::DEFAULT_WEBSOCKET_CONNECT_TIMEOUT_MS
        ))
    );

    for (wire, expected) in [
        ("auto", TransportSetting::Auto),
        ("sse", TransportSetting::Sse),
        ("websocket", TransportSetting::Websocket),
        ("websocket-cached", TransportSetting::WebsocketCached),
    ] {
        let options = StreamOptions {
            metadata: Some(BTreeMap::from([(
                codex_ws::METADATA_TRANSPORT.to_string(),
                json!(wire),
            )])),
            ..StreamOptions::default()
        };
        assert_eq!(codex_ws::transport_options(&options).transport, expected);
    }

    // An unknown wire value degrades to `auto` instead of failing the request.
    let options = StreamOptions {
        metadata: Some(BTreeMap::from([(
            codex_ws::METADATA_TRANSPORT.to_string(),
            json!("nonsense"),
        )])),
        ..StreamOptions::default()
    };
    assert_eq!(
        codex_ws::transport_options(&options).transport,
        TransportSetting::Auto
    );
}

#[test]
fn codex_websocket_connect_timeout_honours_disabled_and_explicit_values() {
    let options = StreamOptions {
        metadata: Some(BTreeMap::from([(
            codex_ws::METADATA_WEBSOCKET_CONNECT_TIMEOUT_MS.to_string(),
            json!(0),
        )])),
        ..StreamOptions::default()
    };
    assert_eq!(
        codex_ws::transport_options(&options).connect_timeout,
        None,
        "0 disables the handshake timeout (upstream `connectTimeoutMs > 0`)"
    );

    let options = StreamOptions {
        metadata: Some(BTreeMap::from([(
            codex_ws::METADATA_WEBSOCKET_CONNECT_TIMEOUT_MS.to_string(),
            json!(250),
        )])),
        ..StreamOptions::default()
    };
    assert_eq!(
        codex_ws::transport_options(&options).connect_timeout,
        Some(Duration::from_millis(250))
    );
}

#[test]
fn codex_continuation_cache_is_only_used_by_auto_and_cached() {
    let cached = |transport| {
        codex_ws::CodexTransportOptions {
            transport,
            connect_timeout: None,
        }
        .uses_continuation_cache()
    };
    assert!(cached(TransportSetting::Auto));
    assert!(cached(TransportSetting::WebsocketCached));
    assert!(!cached(TransportSetting::Websocket));
    assert!(!cached(TransportSetting::Sse));
}

#[test]
fn codex_session_cleanup_key_clamps_like_the_pool() {
    let long = "s".repeat(80);
    let key = codex_ws::session_continuation_key(&long).expect("clamped key");
    assert_eq!(key.len(), 64);
    assert_eq!(
        codex_ws::session_continuation_key("sess-ws-1").as_deref(),
        Some("sess-ws-1")
    );
    assert_eq!(codex_ws::session_continuation_key(""), None);
}

// ---------------------------------------------------------------------------
// Transport behavior over the mock server
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn codex_transport_sse_skips_the_websocket_attempt() {
    codex_ws::close_codex_websocket_sessions(Some("w13-sse")).await;
    let script = text_turn("resp_sse", &["over sse"]);
    let state = ServerState::scripted_with_sse(vec![script.clone()], sse_body(&script));
    let (base, server) = spawn_server(state.clone()).await;
    let (provider, model) = provider(&base);

    let events = tokio::time::timeout(
        Duration::from_secs(10),
        collect(provider.stream(
            &model,
            user_context("sse please"),
            stream_options("w13-sse", TransportSetting::Sse),
        )),
    )
    .await
    .expect("turn times out");

    assert_eq!(done_response_id(&events).as_deref(), Some("resp_sse"));
    assert_eq!(
        state.handshakes.load(Ordering::SeqCst),
        0,
        "transport = sse must not open a WebSocket"
    );
    let recorded = state.recorded();
    assert_eq!(recorded.len(), 1, "exactly one SSE request");
    assert!(!recorded[0].websocket);

    codex_ws::close_codex_websocket_sessions(Some("w13-sse")).await;
    server.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn codex_transport_websocket_pools_without_sending_a_continuation() {
    // Pool CONTINUITY (turn 2 reuses turn 1's connection): a sibling suite's
    // global close would force a fresh handshake, so serialize with those.
    let _pool = crate::common::codex_ws_pool_guard().await;
    codex_ws::close_codex_websocket_sessions(Some("w13-plain-ws")).await;
    let state = ServerState::scripted(vec![
        text_turn("resp_1", &["one"]),
        text_turn("resp_2", &["two"]),
    ]);
    let (base, server) = spawn_server(state.clone()).await;
    let (provider, model) = provider(&base);

    let first = tokio::time::timeout(
        Duration::from_secs(10),
        collect(provider.stream(
            &model,
            user_context("hi"),
            stream_options("w13-plain-ws", TransportSetting::Websocket),
        )),
    )
    .await
    .expect("first turn times out");
    let done = first
        .iter()
        .find_map(|event| match event {
            AssistantMessageEvent::Done { message, .. } => Some(message.clone()),
            _ => None,
        })
        .expect("done");
    assert_eq!(done.response_id.as_deref(), Some("resp_1"));

    // The terminal Done event wakes the consumer before the producer runs
    // `Lease::release`; give the release a scheduling turn so the second turn
    // observes the pooled socket instead of racing it.
    tokio::time::sleep(Duration::from_millis(100)).await;

    let second_context = Context {
        messages: vec![
            user_message("hi"),
            Message::Assistant(done),
            user_message("again"),
        ],
        ..Context::default()
    };
    let second = tokio::time::timeout(
        Duration::from_secs(10),
        collect(provider.stream(
            &model,
            second_context,
            stream_options("w13-plain-ws", TransportSetting::Websocket),
        )),
    )
    .await
    .expect("second turn times out");
    assert_eq!(done_response_id(&second).as_deref(), Some("resp_2"));

    let frames: Vec<RecordedRequest> = state
        .recorded()
        .into_iter()
        .filter(|request| request.websocket)
        .collect();
    assert_eq!(frames.len(), 2);
    let second_body: Value = serde_json::from_slice(&frames[1].body).expect("body json");
    assert!(
        second_body.get("previous_response_id").is_none(),
        "transport = websocket sends the full history (no cached context)"
    );
    assert_eq!(
        second_body["input"].as_array().map(Vec::len),
        Some(3),
        "the full history (user, assistant, user) is resent"
    );
    assert_eq!(
        state.handshakes.load(Ordering::SeqCst),
        1,
        "the pooled connection is still reused"
    );

    codex_ws::close_codex_websocket_sessions(Some("w13-plain-ws")).await;
    server.abort();
}

#[tokio::test(flavor = "multi_thread")]
#[cfg_attr(
    target_os = "linux",
    ignore = "websocket continuation cache race under 16-way CI load; covered on Windows"
)]
async fn codex_transport_websocket_cached_reuses_the_pooled_continuation() {
    // Pool continuity: see the guard in the test above.
    let _pool = crate::common::codex_ws_pool_guard().await;
    codex_ws::close_codex_websocket_sessions(Some("w13-cached")).await;
    let state = ServerState::scripted(vec![
        text_turn("resp_1", &["one"]),
        text_turn("resp_2", &["two"]),
    ]);
    let (base, server) = spawn_server(state.clone()).await;
    let (provider, model) = provider(&base);

    let first = tokio::time::timeout(
        Duration::from_secs(10),
        collect(provider.stream(
            &model,
            user_context("hi"),
            stream_options("w13-cached", TransportSetting::WebsocketCached),
        )),
    )
    .await
    .expect("first turn times out");
    let done = first
        .iter()
        .find_map(|event| match event {
            AssistantMessageEvent::Done { message, .. } => Some(message.clone()),
            _ => None,
        })
        .expect("done");
    assert_eq!(done.response_id.as_deref(), Some("resp_1"));

    // The terminal Done event wakes the consumer before the producer runs
    // `Lease::release`; give the release a scheduling turn so the second turn
    // observes the pooled socket instead of racing it.
    tokio::time::sleep(Duration::from_millis(100)).await;

    let second_context = Context {
        messages: vec![
            user_message("hi"),
            Message::Assistant(done),
            user_message("again"),
        ],
        ..Context::default()
    };
    let second = tokio::time::timeout(
        Duration::from_secs(10),
        collect(provider.stream(
            &model,
            second_context,
            stream_options("w13-cached", TransportSetting::WebsocketCached),
        )),
    )
    .await
    .expect("second turn times out");
    assert_eq!(
        done_response_id(&second).as_deref(),
        Some("resp_2"),
        "second turn failed: {:?}",
        stream_error(&second)
    );

    let frames: Vec<RecordedRequest> = state
        .recorded()
        .into_iter()
        .filter(|request| request.websocket)
        .collect();
    assert_eq!(frames.len(), 2, "one response.create frame per turn");
    let second_body: Value = serde_json::from_slice(&frames[1].body).expect("body json");
    assert_eq!(
        second_body["previous_response_id"], "resp_1",
        "websocket-cached sends the connection's cached response id"
    );
    let delta = second_body["input"].as_array().expect("delta input");
    assert_eq!(delta.len(), 1, "only the new user message is sent");
    assert_eq!(delta[0]["content"][0]["text"], "again");
    assert_eq!(
        state.handshakes.load(Ordering::SeqCst),
        1,
        "the pooled connection is reused"
    );

    codex_ws::close_codex_websocket_sessions(Some("w13-cached")).await;
    server.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn codex_previous_response_not_found_retries_only_before_start() {
    // Per-session close only (never `None`): sibling suites in this merged
    // binary run concurrently and share the process-wide pool. The exact
    // handshake counts below assume no sibling global close lands mid-test,
    // so holders of the shared pool lock are excluded too.
    let _pool = crate::common::codex_ws_pool_guard().await;
    codex_ws::close_codex_websocket_sessions(Some("w13-retry")).await;
    let not_found = json!({
        "type": "error",
        "code": "previous_response_not_found",
        "message": "previous response not found"
    });
    let after_start = vec![
        json!({"type": "response.created", "response": {"id": "resp_stale"}}),
        json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": {"type": "message", "id": "msg_stale"}
        }),
        json!({"type": "response.output_text.delta", "output_index": 0, "delta": "partial"}),
        not_found.clone(),
    ];
    let state = ServerState::scripted(vec![
        // Case 1: the error arrives before any event -> one retry.
        vec![not_found],
        text_turn("resp_retried", &["recovered"]),
        // Case 2: the error arrives after events -> surfaced, no retry.
        after_start,
    ]);
    let (base, server) = spawn_server(state.clone()).await;
    let (provider, model) = provider(&base);

    let retried = tokio::time::timeout(
        Duration::from_secs(10),
        collect(provider.stream(
            &model,
            user_context("retry-before-start"),
            stream_options("w13-retry", TransportSetting::Auto),
        )),
    )
    .await
    .expect("retry turn times out");
    assert_eq!(
        done_response_id(&retried).as_deref(),
        Some("resp_retried"),
        "the retried turn completes: {:?}",
        stream_error(&retried)
    );
    assert_eq!(
        state.frames_containing("retry-before-start"),
        2,
        "one retry, on a fresh connection"
    );
    assert_eq!(
        state.handshakes.load(Ordering::SeqCst),
        2,
        "the retry closes the suspect socket instead of pooling it"
    );

    let after = tokio::time::timeout(
        Duration::from_secs(10),
        collect(provider.stream(
            &model,
            user_context("error-after-start"),
            stream_options("w13-retry", TransportSetting::Auto),
        )),
    )
    .await
    .expect("after-start turn times out");
    let error = stream_error(&after).expect("the error surfaces");
    assert!(
        error.contains("previous response not found"),
        "unexpected error: {error}"
    );
    assert_eq!(
        state.frames_containing("error-after-start"),
        1,
        "no second response is appended to a started parser"
    );

    codex_ws::close_codex_websocket_sessions(Some("w13-retry")).await;
    server.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn codex_websocket_connect_timeout_comes_from_the_request() {
    codex_ws::close_codex_websocket_sessions(Some("w13-timeout")).await;
    let script = text_turn("resp_timeout", &["fallback"]);
    let state = ServerState::stalling(sse_body(&script));
    let (base, server) = spawn_server(state.clone()).await;
    let (provider, model) = provider(&base);

    let mut options = stream_options("w13-timeout", TransportSetting::Auto);
    options.metadata.as_mut().expect("metadata").insert(
        codex_ws::METADATA_WEBSOCKET_CONNECT_TIMEOUT_MS.to_string(),
        json!(100),
    );
    let started = std::time::Instant::now();
    let events = tokio::time::timeout(
        Duration::from_secs(10),
        collect(provider.stream(&model, user_context("timeout"), options)),
    )
    .await
    .expect("timeout turn times out");
    let elapsed = started.elapsed();

    assert_eq!(done_response_id(&events).as_deref(), Some("resp_timeout"));
    assert!(
        elapsed < Duration::from_secs(5),
        "a 100ms connect timeout must not wait for the 15s default (took {elapsed:?})"
    );

    codex_ws::close_codex_websocket_sessions(Some("w13-timeout")).await;
    server.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn codex_teardown_cleans_up_the_clamped_session_key() {
    let long_id = "w".repeat(80);
    codex_ws::close_codex_websocket_sessions(Some(&long_id)).await;
    let state = ServerState::scripted(vec![
        text_turn("resp_1", &["one"]),
        text_turn("resp_2", &["two"]),
    ]);
    let (base, server) = spawn_server(state.clone()).await;
    let (provider, model) = provider(&base);

    let first = tokio::time::timeout(
        Duration::from_secs(10),
        collect(provider.stream(
            &model,
            user_context("first"),
            stream_options(&long_id, TransportSetting::Auto),
        )),
    )
    .await
    .expect("first turn times out");
    assert_eq!(done_response_id(&first).as_deref(), Some("resp_1"));
    assert_eq!(state.handshakes.load(Ordering::SeqCst), 1);

    // Teardown runs with the RAW session id; the pool is keyed by the CLAMPED
    // `continuation_key`, so the close must clamp too (a raw-id comparison
    // would leave the socket pooled forever).
    codex_ws::close_codex_websocket_sessions(Some(&long_id)).await;

    let second = tokio::time::timeout(
        Duration::from_secs(10),
        collect(provider.stream(
            &model,
            user_context("second"),
            stream_options(&long_id, TransportSetting::Auto),
        )),
    )
    .await
    .expect("second turn times out");
    assert_eq!(done_response_id(&second).as_deref(), Some("resp_2"));
    assert_eq!(
        state.handshakes.load(Ordering::SeqCst),
        2,
        "the clamped-key teardown closed the pooled socket"
    );

    codex_ws::close_codex_websocket_sessions(Some(&long_id)).await;
    server.abort();
}

/// A session that already fell back to SSE skips the WebSocket on later turns
/// (the upstream `websocketSseFallbackSessions` marker), the marker is keyed by
/// the CLAMPED id, and [`codex_ws::reset_codex_websocket_state`] clears it —
/// with the raw id, like the teardown path.
#[tokio::test(flavor = "multi_thread")]
async fn codex_fallback_marker_is_keyed_by_the_clamped_session_id() {
    // A GLOBAL close clears every fallback marker (`lock_fallback_sessions()
    // .clear()`), not just one session's, so a sibling suite's
    // `close_codex_websocket_sessions(None)` would defeat the "second turn
    // skips the WebSocket" assertion. Serialize with those.
    let _pool = crate::common::codex_ws_pool_guard().await;
    let long_id = "m".repeat(80);
    codex_ws::close_codex_websocket_sessions(Some(&long_id)).await;
    let script = text_turn("resp_marker", &["over sse"]);
    // Every handshake is stalled, so the first turn falls back to SSE and sets
    // the marker; the connection timeout keeps the test fast.
    let state = ServerState::stalling(sse_body(&script));
    let (base, server) = spawn_server(state.clone()).await;
    let (provider, model) = provider(&base);

    let mut options = stream_options(&long_id, TransportSetting::Auto);
    options.metadata.as_mut().expect("metadata").insert(
        codex_ws::METADATA_WEBSOCKET_CONNECT_TIMEOUT_MS.to_string(),
        json!(100),
    );
    let first = tokio::time::timeout(
        Duration::from_secs(10),
        collect(provider.stream(&model, user_context("first"), options.clone())),
    )
    .await
    .expect("first turn times out");
    assert_eq!(done_response_id(&first).as_deref(), Some("resp_marker"));
    assert_eq!(state.handshakes.load(Ordering::SeqCst), 1);

    // Same session, no WebSocket attempt: the fallback marker short-circuits.
    let second = tokio::time::timeout(
        Duration::from_secs(10),
        collect(provider.stream(&model, user_context("second"), options.clone())),
    )
    .await
    .expect("second turn times out");
    assert_eq!(done_response_id(&second).as_deref(), Some("resp_marker"));
    assert_eq!(
        state.handshakes.load(Ordering::SeqCst),
        1,
        "a session that fell back never retries the WebSocket transport"
    );

    // The reset is keyed by the same CLAMPED id: a raw-id reset would leave
    // the marker in place (the socket stays skipped) even though the marker
    // was recorded under the clamped key.
    codex_ws::reset_codex_websocket_state(Some(&long_id)).await;
    let third = tokio::time::timeout(
        Duration::from_secs(10),
        collect(provider.stream(&model, user_context("third"), options)),
    )
    .await
    .expect("third turn times out");
    assert_eq!(done_response_id(&third).as_deref(), Some("resp_marker"));
    assert_eq!(
        state.handshakes.load(Ordering::SeqCst),
        2,
        "the clamped-key reset makes the next turn try the WebSocket again"
    );

    codex_ws::close_codex_websocket_sessions(Some(&long_id)).await;
    server.abort();
}

/// Session teardown closes only that session's pooled sockets and clears its
/// fallback marker; another session's pooled connection survives.
#[tokio::test(flavor = "multi_thread")]
async fn codex_teardown_is_scoped_to_one_session() {
    // Pins that one session's teardown leaves ANOTHER session's pooled socket
    // alive: a sibling suite's global close would also raise the handshake
    // count, so serialize with those.
    let _pool = crate::common::codex_ws_pool_guard().await;
    codex_ws::close_codex_websocket_sessions(Some("w13-keep")).await;
    codex_ws::close_codex_websocket_sessions(Some("w13-drop")).await;
    let state = ServerState::scripted(vec![
        text_turn("resp_keep_1", &["keep one"]),
        text_turn("resp_drop_1", &["drop one"]),
        text_turn("resp_keep_2", &["keep two"]),
        text_turn("resp_drop_2", &["drop two"]),
    ]);
    let (base, server) = spawn_server(state.clone()).await;
    let (provider, model) = provider(&base);

    for (session, prompt, response) in [
        ("w13-keep", "keep first", "resp_keep_1"),
        ("w13-drop", "drop first", "resp_drop_1"),
    ] {
        let events = tokio::time::timeout(
            Duration::from_secs(10),
            collect(provider.stream(
                &model,
                user_context(prompt),
                stream_options(session, TransportSetting::Auto),
            )),
        )
        .await
        .expect("turn times out");
        assert_eq!(done_response_id(&events).as_deref(), Some(response));
    }
    assert_eq!(state.handshakes.load(Ordering::SeqCst), 2);

    // Tearing down ONE session must not touch the other's pooled socket.
    codex_ws::close_codex_websocket_sessions(Some("w13-drop")).await;

    let kept = tokio::time::timeout(
        Duration::from_secs(10),
        collect(provider.stream(
            &model,
            user_context("keep second"),
            stream_options("w13-keep", TransportSetting::Auto),
        )),
    )
    .await
    .expect("kept turn times out");
    assert_eq!(
        done_response_id(&kept).as_deref(),
        Some("resp_keep_2"),
        "{:?}",
        stream_error(&kept)
    );
    assert_eq!(
        state.handshakes.load(Ordering::SeqCst),
        2,
        "the surviving session reuses its pooled connection (no new handshake)"
    );

    // The torn-down session starts over on a fresh connection.
    let dropped = tokio::time::timeout(
        Duration::from_secs(10),
        collect(provider.stream(
            &model,
            user_context("drop second"),
            stream_options("w13-drop", TransportSetting::Auto),
        )),
    )
    .await
    .expect("dropped turn times out");
    // The scripted server serves turn 4 (`resp_drop_2`) on the fresh
    // connection; what matters is that a new handshake happened.
    assert_eq!(done_response_id(&dropped).as_deref(), Some("resp_drop_2"));
    assert_eq!(
        state.handshakes.load(Ordering::SeqCst),
        3,
        "the torn-down session's socket was closed with it"
    );

    codex_ws::close_codex_websocket_sessions(Some("w13-keep")).await;
    codex_ws::close_codex_websocket_sessions(Some("w13-drop")).await;
    server.abort();
}

// ---------------------------------------------------------------------------
// AgentSession wiring: a stable, unique `StreamOptions.session_id`
// ---------------------------------------------------------------------------

/// Provider that records the request options and answers with a fixed message.
struct CapturingProvider {
    seen: Arc<Mutex<Vec<StreamOptions>>>,
}

impl Provider for CapturingProvider {
    fn id(&self) -> &str {
        "fake"
    }
    fn name(&self) -> &str {
        "Fake provider"
    }
    fn get_models(&self) -> Vec<Model> {
        vec![fake_model()]
    }
    fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
        Vec::new()
    }
    fn stream(
        &self,
        _model: &Model,
        _context: Context,
        options: StreamOptions,
    ) -> AssistantMessageEventStream {
        self.seen.lock().unwrap().push(options);
        let mut message = AssistantMessage {
            api: "fake".into(),
            provider: "fake".into(),
            model: "fake-model".into(),
            stop_reason: StopReason::Stop,
            timestamp: 1,
            ..Default::default()
        };
        message.content.push(ContentBlock::Text(TextContent {
            text: "ok".into(),
            text_signature: None,
        }));
        let stream = AssistantMessageEventStream::new();
        stream.push(AssistantMessageEventStream::done_event(
            TerminalReason::Stop,
            message,
        ));
        stream
    }
}

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

fn capturing_session(
    session: Option<SessionManager>,
    seen: Arc<Mutex<Vec<StreamOptions>>>,
    cwd: &std::path::Path,
) -> AgentSession {
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(CapturingProvider { seen }));
    AgentSession::new(AgentSessionConfig {
        provider_registry: Arc::new(registry),
        model: fake_model(),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools: ToolRegistry::new(),
        session,
        compaction: None,
        max_turns: 1,
        system_prompt: None,
        cwd: Some(cwd.to_path_buf()),
        event_stream: None,
        extensions: None,
        tool_selection: None,
    })
}

struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-w13-codex-{label}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&path).expect("temp dir");
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn no_op_handler() -> pir::agent::agent_loop::EventHandler {
    Arc::new(|_event| {})
}

#[tokio::test(flavor = "multi_thread")]
async fn agent_session_carries_a_stable_session_id() {
    // Hermetic: an empty pinned agent dir + a scratch cwd, so the session's
    // settings read sees no ambient `~/.pi` values.
    let settings = ProjectSettings::new("w13-session-id", "{}").await;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let temp = TempDir::new("persisted");
    let session = SessionManager::create(".", &temp.0).expect("create session");
    let persisted_id = session.id().to_owned();
    let agent = capturing_session(Some(session), Arc::clone(&seen), &settings.cwd());

    agent.prompt("hi", no_op_handler()).await.expect("prompt");
    agent
        .prompt("again", no_op_handler())
        .await
        .expect("second prompt");

    let options = seen.lock().unwrap().clone();
    assert_eq!(options.len(), 2, "one request per prompt");
    for request in &options {
        assert_eq!(
            request.session_id.as_deref(),
            Some(persisted_id.as_str()),
            "a persisted session hands its storage id to every provider request"
        );
    }

    // A store-less session has no persisted id, so the facade mints one that is
    // stable across its turns and distinct from another session's.
    let ephemeral_seen = Arc::new(Mutex::new(Vec::new()));
    let ephemeral = capturing_session(None, Arc::clone(&ephemeral_seen), &settings.cwd());
    ephemeral
        .prompt("hi", no_op_handler())
        .await
        .expect("prompt");
    ephemeral
        .prompt("again", no_op_handler())
        .await
        .expect("second prompt");
    let ephemeral_options = ephemeral_seen.lock().unwrap().clone();
    let first = ephemeral_options[0]
        .session_id
        .clone()
        .expect("store-less sessions still get an id");
    assert!(!first.is_empty());
    assert_eq!(
        ephemeral_options[1].session_id.as_deref(),
        Some(first.as_str())
    );

    let other_seen = Arc::new(Mutex::new(Vec::new()));
    let other = capturing_session(None, Arc::clone(&other_seen), &settings.cwd());
    other.prompt("hi", no_op_handler()).await.expect("prompt");
    let other_id = other_seen.lock().unwrap()[0].session_id.clone();
    assert_ne!(
        other_id.as_deref(),
        Some(first.as_str()),
        "two ephemeral sessions never share an id"
    );
}

// ---------------------------------------------------------------------------
// End to end: a settings file's `transport` / `websocketConnectTimeoutMs`
// reaches the Codex adapter through the session's request options.
// ---------------------------------------------------------------------------

/// A scratch project whose `<cwd>/.pi/settings.json` carries `settings`, marked
/// trusted so the PROJECT scope applies. Project settings deep-merge over the
/// global scope, so these values win even if a concurrent suite pins
/// `PIR_AGENT_DIR` at its own scratch directory. The shared
/// [`crate::common::AgentDirEnvGuard`] lock serializes the env mutation with
/// the other agent-dir suites in this binary.
struct ProjectSettings {
    // Declared first: the env is restored before the scratch dir is removed.
    _guard: crate::common::AgentDirEnvGuard,
    temp: TempDir,
}

impl ProjectSettings {
    async fn new(label: &str, settings: &str) -> Self {
        let temp = TempDir::new(label);
        let project_dir = temp.0.join(".pi");
        std::fs::create_dir_all(&project_dir).expect("create project settings dir");
        std::fs::write(project_dir.join("settings.json"), settings).expect("write settings");
        let guard =
            crate::common::AgentDirEnvGuard::at(&temp.0, &[("PIR_TRUST_PROJECT", "1")]).await;
        Self {
            _guard: guard,
            temp,
        }
    }

    fn cwd(&self) -> std::path::PathBuf {
        self.temp.0.clone()
    }
}

/// The full settings → session → adapter path: a `transport = "sse"` settings
/// file makes the session's own Codex request skip the WebSocket transport
/// (the bug this item fixes was a setting that was read but never reached the
/// transport).
#[tokio::test(flavor = "multi_thread")]
async fn settings_transport_reaches_the_session_codex_adapter() {
    let script = text_turn("resp_settings", &["over sse"]);
    let state = ServerState::scripted_with_sse(vec![script.clone()], sse_body(&script));
    let (base, server) = spawn_server(state.clone()).await;
    let (provider, model) = provider(&base);
    let settings = ProjectSettings::new(
        "w13-codex-sse",
        r#"{ "transport": "sse", "websocketConnectTimeoutMs": 100 }"#,
    )
    .await;

    // The session resolves the settings itself from its `cwd`: no metadata is
    // handed in here, so anything the adapter sees came through the session.
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(provider));
    let agent = AgentSession::new(AgentSessionConfig {
        provider_registry: Arc::new(registry),
        model,
        api_key: Some(fake_jwt("acct_123")),
        thinking: ThinkingLevel::Off,
        tools: ToolRegistry::new(),
        session: None,
        compaction: None,
        max_turns: 1,
        system_prompt: None,
        cwd: Some(settings.cwd()),
        event_stream: None,
        extensions: None,
        tool_selection: None,
    });
    agent
        .prompt("settings please", no_op_handler())
        .await
        .expect("prompt");

    assert_eq!(
        state.handshakes.load(Ordering::SeqCst),
        0,
        "transport = \"sse\" from the settings file must skip the WebSocket attempt"
    );
    let recorded = state.recorded();
    assert_eq!(recorded.len(), 1, "one SSE request");
    assert!(!recorded[0].websocket);

    server.abort();
}

/// `transport` stays the default `auto`, so the WebSocket is attempted, but
/// the settings' `websocketConnectTimeoutMs` reaches the handshake: a 100ms
/// value against a stalling server falls back to SSE well before the 15s
/// default.
#[tokio::test(flavor = "multi_thread")]
async fn settings_websocket_timeout_reaches_the_session_codex_adapter() {
    let script = text_turn("resp_settings_timeout", &["fallback"]);
    let state = ServerState::stalling(sse_body(&script));
    let (base, server) = spawn_server(state.clone()).await;
    let (provider, model) = provider(&base);
    let env = ProjectSettings::new(
        "w13-codex-timeout",
        r#"{ "websocketConnectTimeoutMs": 100 }"#,
    )
    .await;

    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(provider));
    let agent = AgentSession::new(AgentSessionConfig {
        provider_registry: Arc::new(registry),
        model,
        api_key: Some(fake_jwt("acct_123")),
        thinking: ThinkingLevel::Off,
        tools: ToolRegistry::new(),
        session: None,
        compaction: None,
        max_turns: 1,
        system_prompt: None,
        cwd: Some(env.cwd()),
        event_stream: None,
        extensions: None,
        tool_selection: None,
    });

    let started = std::time::Instant::now();
    let result = tokio::time::timeout(
        Duration::from_secs(15),
        agent.prompt("settings timeout", no_op_handler()),
    )
    .await
    .expect("the session prompt finishes");
    let elapsed = started.elapsed();
    // The SSE fallback answers, so the prompt itself succeeds.
    assert!(
        result.is_ok(),
        "the SSE fallback completes the prompt: {:?}",
        result.err().map(|error| error.to_string())
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "a 100ms settings connect timeout must not wait for the 15s default (took {elapsed:?})"
    );
    assert_eq!(
        state.handshakes.load(Ordering::SeqCst),
        1,
        "the default `auto` transport still attempted the WebSocket"
    );

    // The session's Drop closes its pooled sockets and clears its fallback
    // marker (the store-less session's request id is internal to the facade).
    drop(agent);
    server.abort();
}
