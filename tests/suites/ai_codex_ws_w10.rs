//! W10-G: Codex pooled WebSocket transport.
//!
//! Local mock servers only (127.0.0.1), no network. Covers:
//! - handshake headers (`originator`, `chatgpt-account-id`, `OpenAI-Beta`,
//!   auth, session/request id);
//! - `previous_response_id` continuation (new input items only) vs full
//!   history on a fresh session, over one reused connection;
//! - SSE fallback after the WebSocket is forced closed (reconnect attempted
//!   first, then the SSE body is served and parsed);
//! - event-stream equivalence: the WebSocket events equal the same scripted
//!   event sequence parsed through the SSE parser.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use pir::ai::api::openai_responses::codex::codex_ws;
use pir::ai::api::openai_responses::OpenAiCodexResponsesProvider;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::Provider;
use pir::ai::types::*;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::accept_hdr_async;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio_tungstenite::tungstenite::Message as WsMessage;

// ---------------------------------------------------------------------------
// Mock server
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct RecordedRequest {
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
    websocket: bool,
}

#[derive(Clone)]
enum WsMode {
    /// Reply to each client `response.create` with the next scripted turn on
    /// the same connection.
    Scripted { turns: Vec<Vec<Value>> },
    /// Close every accepted WebSocket immediately; HTTP requests are served
    /// the SSE body so the fallback can be observed.
    CloseHandshake,
}

struct ServerState {
    ws_mode: WsMode,
    recorded: Mutex<Vec<RecordedRequest>>,
    ws_handshakes: AtomicUsize,
    sse_body: String,
}

impl ServerState {
    fn scripted(turns: Vec<Vec<Value>>) -> Arc<Self> {
        Arc::new(Self {
            ws_mode: WsMode::Scripted { turns },
            recorded: Mutex::new(Vec::new()),
            ws_handshakes: AtomicUsize::new(0),
            sse_body: String::new(),
        })
    }

    fn close_ws_then_sse(sse_body: String) -> Arc<Self> {
        Arc::new(Self {
            ws_mode: WsMode::CloseHandshake,
            recorded: Mutex::new(Vec::new()),
            ws_handshakes: AtomicUsize::new(0),
            sse_body,
        })
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
    state.ws_handshakes.fetch_add(1, Ordering::SeqCst);
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

    if matches!(state.ws_mode, WsMode::CloseHandshake) {
        let _ = ws.close(None).await;
        return;
    }

    let headers = handshake_headers.lock().unwrap().clone();
    let mut turn = 0usize;
    while let Some(message) = ws.next().await {
        let Ok(WsMessage::Text(text)) = message else {
            break;
        };
        state.recorded.lock().unwrap().push(RecordedRequest {
            headers: headers.clone(),
            body: text.into_bytes(),
            websocket: true,
        });
        let WsMode::Scripted { turns } = &state.ws_mode else {
            break;
        };
        let Some(script) = turns.get(turn) else {
            break;
        };
        turn += 1;
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

fn stream_options(session_id: &str) -> StreamOptions {
    StreamOptions {
        api_key: Some(fake_jwt("acct_123")),
        session_id: Some(session_id.to_string()),
        max_retries: Some(0),
        ..StreamOptions::default()
    }
}

fn user_context(text: &str) -> Context {
    Context {
        messages: vec![Message::User(UserMessage {
            role: UserRole::User,
            content: vec![ContentBlock::Text(TextContent {
                text: text.to_string(),
                text_signature: None,
            })],
            tools_added: None,
            tools_removed: None,
            timestamp: 0,
            sections: None,
        })],
        ..Context::default()
    }
}

fn text_turn(response_id: &str, parts: &[&str]) -> Vec<Value> {
    let text = parts.concat();
    let mut events = vec![
        json!({"type": "response.created", "response": {"id": response_id}}),
        json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": {"type": "message", "id": format!("msg_{response_id}")}
        }),
    ];
    for part in parts {
        events.push(json!({
            "type": "response.output_text.delta",
            "output_index": 0,
            "delta": part
        }));
    }
    events.push(json!({
        "type": "response.output_item.done",
        "output_index": 0,
        "item": {
            "type": "message",
            "id": format!("msg_{response_id}"),
            "content": [{"type": "output_text", "text": text}]
        }
    }));
    events.push(json!({
        "type": "response.completed",
        "response": {
            "id": response_id,
            "status": "completed",
            "usage": {
                "input_tokens": 10,
                "output_tokens": 2,
                "total_tokens": 12
            }
        }
    }));
    events
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

fn done_message(events: &[AssistantMessageEvent]) -> AssistantMessage {
    events
        .iter()
        .find_map(|event| match event {
            AssistantMessageEvent::Done { message, .. } => Some(message.clone()),
            _ => None,
        })
        .expect("stream completed with a done message")
}

fn error_message(events: &[AssistantMessageEvent]) -> String {
    events
        .iter()
        .find_map(|event| match event {
            AssistantMessageEvent::Error { error, .. } => {
                Some(error.error_message.clone().unwrap_or_default())
            }
            _ => None,
        })
        .unwrap_or_else(|| "no error event".to_string())
}

/// Strip fields that legitimately differ between a live stream and scripted
/// parser output: `timestamp` (wall clock at parse time) and `durationMs`
/// (stamped by the live `AssistantMessageEventStream`; raw parser events have
/// no stream to time them).
fn strip_timestamps(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.remove("timestamp");
            map.remove("durationMs");
            for child in map.values_mut() {
                strip_timestamps(child);
            }
        }
        Value::Array(items) => {
            for child in items {
                strip_timestamps(child);
            }
        }
        _ => {}
    }
}

fn event_key(event: &AssistantMessageEvent) -> String {
    let mut value = serde_json::to_value(event).expect("event serializes");
    strip_timestamps(&mut value);
    serde_json::to_string(&value).expect("canonical event")
}

fn expected_events(model: &Model, script: &[Value]) -> Vec<AssistantMessageEvent> {
    use pir::ai::api::openai_responses::ResponsesSseParser;
    let mut parser = ResponsesSseParser::new(model);
    parser.codex_mode = true;
    let mut out = vec![AssistantMessageEvent::Start {
        partial: parser.snapshot(),
    }];
    for event in script {
        parser
            .handle_data(&event.to_string(), &mut out)
            .expect("scripted event parses");
    }
    parser.finish(&mut out).expect("script finishes");
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn codex_ws_handshake_headers_and_previous_response_id_continuation() {
    // This test pins POOL CONTINUITY: turn 2 must reuse turn 1's connection.
    // The pool is process-global, so a *global* close in a sibling suite
    // (`ai_adapters_w11::codex_websocket_sse_fallback_marker_skips_later_attempts`)
    // landing between the turns would drop the pooled connection; turn 2 would
    // then handshake fresh, the scripted server would re-serve turn 1, and the
    // done id would be `resp_1` instead of `resp_2` with no error event — the
    // recorded CL34 flake. Hold the shared pool lock for the whole test.
    let _pool = crate::common::codex_ws_pool_guard().await;
    codex_ws::close_codex_websocket_sessions(Some("sess-ws-1")).await;
    codex_ws::close_codex_websocket_sessions(Some("sess-ws-2")).await;
    let first_turn = text_turn("resp_1", &["Hello"]);
    let second_turn = text_turn("resp_2", &["Again"]);
    let state = ServerState::scripted(vec![first_turn.clone(), second_turn.clone()]);
    let (base, server) = spawn_server(state.clone()).await;
    let (provider, model) = provider(&base);

    let first_events = tokio::time::timeout(
        Duration::from_secs(10),
        collect(provider.stream(&model, user_context("hi"), stream_options("sess-ws-1"))),
    )
    .await
    .expect("first turn times out");
    let first_done = done_message(&first_events);
    assert_eq!(first_done.response_id.as_deref(), Some("resp_1"));

    let second_context = Context {
        messages: vec![
            Message::User(UserMessage {
                role: UserRole::User,
                content: vec![ContentBlock::Text(TextContent {
                    text: "hi".into(),
                    text_signature: None,
                })],
                tools_added: None,
                tools_removed: None,
                timestamp: 0,
                sections: None,
            }),
            Message::Assistant(first_done),
            Message::User(UserMessage {
                role: UserRole::User,
                content: vec![ContentBlock::Text(TextContent {
                    text: "again".into(),
                    text_signature: None,
                })],
                tools_added: None,
                tools_removed: None,
                timestamp: 0,
                sections: None,
            }),
        ],
        ..Context::default()
    };
    let second_events = tokio::time::timeout(
        Duration::from_secs(10),
        collect(provider.stream(&model, second_context, stream_options("sess-ws-1"))),
    )
    .await
    .expect("second turn times out");
    assert_eq!(
        done_message(&second_events).response_id.as_deref(),
        Some("resp_2"),
        "second turn failed: {}",
        error_message(&second_events)
    );

    let recorded = state.recorded.lock().unwrap().clone();
    let ws_requests: Vec<&RecordedRequest> = recorded
        .iter()
        .filter(|request| request.websocket)
        .collect();
    assert_eq!(ws_requests.len(), 2, "one response.create frame per turn");

    let first_body: Value = serde_json::from_slice(&ws_requests[0].body).expect("first body json");
    assert!(
        first_body.get("previous_response_id").is_none(),
        "fresh session sends the full history"
    );
    assert_eq!(
        first_body["input"].as_array().map(Vec::len),
        Some(1),
        "turn 1 input is the single user message"
    );

    let second_body: Value =
        serde_json::from_slice(&ws_requests[1].body).expect("second body json");
    assert_eq!(
        second_body["previous_response_id"], "resp_1",
        "continuation carries the previous response id"
    );
    let delta = second_body["input"].as_array().expect("delta input");
    assert_eq!(delta.len(), 1, "only the new user message is sent");
    assert_eq!(delta[0]["role"], "user");
    assert_eq!(delta[0]["content"][0]["text"], "again");

    let headers = &ws_requests[0].headers;
    assert_eq!(headers.get("originator").map(String::as_str), Some("pi"));
    assert_eq!(
        headers.get("chatgpt-account-id").map(String::as_str),
        Some("acct_123")
    );
    assert_eq!(
        headers.get("openai-beta").map(String::as_str),
        Some(codex_ws::OPENAI_BETA_RESPONSES_WEBSOCKETS)
    );
    assert!(headers
        .get("authorization")
        .is_some_and(|value| value.starts_with("Bearer ")));
    assert_eq!(
        headers.get("session-id").map(String::as_str),
        Some("sess-ws-1")
    );
    assert_eq!(
        headers.get("x-client-request-id").map(String::as_str),
        Some("sess-ws-1")
    );
    assert!(
        headers.get("accept").is_none(),
        "the SSE accept header is dropped for WebSocket"
    );

    assert_eq!(
        state.ws_handshakes.load(Ordering::SeqCst),
        1,
        "the pooled connection is reused across turns"
    );

    // A second session starts fresh: full history, no previous_response_id.
    let third_events = tokio::time::timeout(
        Duration::from_secs(10),
        collect(provider.stream(
            &model,
            user_context("new session"),
            stream_options("sess-ws-2"),
        )),
    )
    .await
    .expect("third turn times out");
    assert_eq!(
        done_message(&third_events).response_id.as_deref(),
        Some("resp_1"),
        "the scripted server answers a fresh connection from turn 1: {}",
        error_message(&third_events)
    );
    let recorded = state.recorded.lock().unwrap().clone();
    let third_body: Value =
        serde_json::from_slice(&recorded.iter().rfind(|r| r.websocket).unwrap().body)
            .expect("third body json");
    assert!(
        third_body.get("previous_response_id").is_none(),
        "a different session key never inherits the continuation"
    );
    assert_eq!(state.ws_handshakes.load(Ordering::SeqCst), 2);

    codex_ws::close_codex_websocket_sessions(Some("sess-ws-1")).await;
    codex_ws::close_codex_websocket_sessions(Some("sess-ws-2")).await;
    server.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn codex_ws_close_falls_back_to_sse_with_compressed_body() {
    // The handshake count pins that the forced close is retried on a FRESH
    // WebSocket; a sibling suite's global pool close would also raise it, so
    // serialize with those (see the guard in the test above).
    let _pool = crate::common::codex_ws_pool_guard().await;
    codex_ws::close_codex_websocket_sessions(Some("sess-sse-1")).await;
    let script = text_turn("resp_sse", &["Hello over SSE"]);
    let state = ServerState::close_ws_then_sse(sse_body(&script));
    let (base, server) = spawn_server(state.clone()).await;
    let (provider, model) = provider(&base);

    let events = tokio::time::timeout(
        Duration::from_secs(10),
        collect(provider.stream(
            &model,
            user_context("fallback"),
            stream_options("sess-sse-1"),
        )),
    )
    .await
    .expect("fallback turn times out");
    let done = done_message(&events);
    assert_eq!(done.response_id.as_deref(), Some("resp_sse"));
    assert_eq!(done.stop_reason, StopReason::Stop);
    assert!(
        state.ws_handshakes.load(Ordering::SeqCst) >= 2,
        "a forced close is retried on a fresh WebSocket before falling back"
    );

    let recorded = state.recorded.lock().unwrap().clone();
    let http_request = recorded
        .iter()
        .find(|request| !request.websocket)
        .expect("SSE fallback request observed");

    let body = if http_request
        .headers
        .get("content-encoding")
        .is_some_and(|value| value.eq_ignore_ascii_case("zstd"))
    {
        #[cfg(feature = "extensions")]
        {
            zstd::stream::decode_all(&http_request.body[..]).expect("zstd body decodes")
        }
        #[cfg(not(feature = "extensions"))]
        {
            panic!("content-encoding: zstd without the zstd codec");
        }
    } else {
        assert!(
            !codex_ws::zstd_request_compression_available(),
            "the SSE fallback compresses when the codec is compiled in"
        );
        http_request.body.clone()
    };
    let body: Value = serde_json::from_slice(&body).expect("SSE body json");
    assert!(
        body.get("previous_response_id").is_none(),
        "the SSE fallback always sends the full history"
    );
    assert_eq!(body["input"].as_array().map(Vec::len), Some(1));

    codex_ws::close_codex_websocket_sessions(Some("sess-sse-1")).await;
    server.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn codex_ws_events_match_the_scripted_sse_parse() {
    codex_ws::close_codex_websocket_sessions(Some("sess-eq-1")).await;
    let script = text_turn("resp_eq", &["Hello ", "world"]);
    let expected = expected_events(&codex_model("http://localhost"), &script);
    let state = ServerState::scripted(vec![script]);
    let (base, server) = spawn_server(state).await;
    let (provider, model) = provider(&base);

    let events = tokio::time::timeout(
        Duration::from_secs(10),
        collect(provider.stream(
            &model,
            user_context("equivalence"),
            stream_options("sess-eq-1"),
        )),
    )
    .await
    .expect("equivalence turn times out");

    assert_eq!(
        events.len(),
        expected.len(),
        "WebSocket and SSE event counts differ: {}",
        error_message(&events)
    );
    for (index, (actual, expected)) in events.iter().zip(expected.iter()).enumerate() {
        assert_eq!(
            event_key(actual),
            event_key(expected),
            "event {index} differs between WebSocket and SSE"
        );
    }
    let done = done_message(&events);
    assert_eq!(done.response_id.as_deref(), Some("resp_eq"));
    assert_eq!(done.content.len(), 1);

    codex_ws::close_codex_websocket_sessions(Some("sess-eq-1")).await;
    server.abort();
}
