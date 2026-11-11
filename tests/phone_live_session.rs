//! End-to-end test for the remote-only phone plugin:
//! real `AgentSession` → pi-phone plugin → central `pir-phone-server` → phone
//! WebSocket client.

#![cfg(feature = "phone")]

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::net::TcpStream;
use tokio::sync::{oneshot, Mutex};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message as WsMessage;

use pir::agent::tool_execution::ToolRegistry;
use pir::agent_session::{AgentSession, AgentSessionConfig, SharedEventStreamFn};
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::ProviderRegistry;
use pir::ai::types::*;
use pir::plugins::builtin::phone::PhonePlugin;
use pir::plugins::AgentPlugin;
use pir_phone_server::config::Config as ServerConfig;
use pir_phone_server::http as phone_http;
use pir_phone_server::state::AppState as PhoneServerState;
use pir_phone_server::store::Store as PhoneStore;

// The pool branch spawns a real `pir --mode rpc` child. On Windows a cold
// debug binary takes several seconds to start, and `Command::spawn` blocks the
// current-thread test runtime for that duration, so per-frame deadlines that
// start before the spawn would elapse without any frame being lost.
const WAIT: Duration = Duration::from_secs(30);

fn fake_model() -> Model {
    Model {
        id: "fake-model".into(),
        name: "Fake model".into(),
        api: "fake-api".into(),
        provider: "fake".into(),
        base_url: "http://localhost".into(),
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

fn scripted_assistant(text: &str) -> AssistantMessage {
    AssistantMessage {
        api: "fake".into(),
        provider: "fake".into(),
        model: "fake-model".into(),
        content: vec![ContentBlock::Text(TextContent {
            text: text.into(),
            text_signature: None,
        })],
        usage: Usage {
            input: 7,
            output: 5,
            total_tokens: 12,
            ..Default::default()
        },
        stop_reason: StopReason::Stop,
        timestamp: 1,
        ..Default::default()
    }
}

fn gated_stream(release: Arc<Mutex<Option<oneshot::Receiver<()>>>>) -> SharedEventStreamFn {
    Arc::new(move |_request| {
        let release = Arc::clone(&release);
        Box::pin(async move {
            let stream = AssistantMessageEventStream::new();
            if let Some(receiver) = release.lock().await.take() {
                let _ = receiver.await;
            }
            stream.push(AssistantMessageEventStream::done_event(
                TerminalReason::Stop,
                scripted_assistant("scripted reply"),
            ));
            stream
        })
    })
}

fn session_config(release: Arc<Mutex<Option<oneshot::Receiver<()>>>>) -> AgentSessionConfig {
    AgentSessionConfig {
        performance: Default::default(),
        provider_registry: Arc::new(ProviderRegistry::default()),
        model: fake_model(),
        api_key: None,
        thinking: ThinkingLevel::Low,
        tools: ToolRegistry::new(),
        session: None,
        compaction: None,
        max_turns: 4,
        system_prompt: Some("phone relay test".into()),
        cwd: std::env::current_dir().ok(),
        event_stream: Some(gated_stream(release)),
        extensions: None,
        tool_selection: None,
    }
}

async fn wait_until<F>(mut predicate: F, label: &str)
where
    F: FnMut() -> bool,
{
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        if predicate() {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {label}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

type RelayWs = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>;

async fn recv_relay(ws: &mut RelayWs) -> Value {
    loop {
        let message = tokio::time::timeout(WAIT, ws.next())
            .await
            .expect("relay frame timed out");
        match message {
            Some(Ok(WsMessage::Text(text))) => {
                let frame: Value = serde_json::from_str(&text).expect("relay frame is JSON");
                // Every frame a phone receives in these flows must be benign:
                // a join/resync may never surface a synthetic session-select
                // for the parent session as `server/client-error`.
                assert!(
                    !(frame["channel"] == "server" && frame["event"] == "client-error"),
                    "unexpected server/client-error frame: {frame}"
                );
                return frame;
            }
            Some(Ok(WsMessage::Ping(_))) | Some(Ok(WsMessage::Pong(_))) => {}
            Some(Ok(WsMessage::Close(frame))) => panic!("relay closed: {frame:?}"),
            Some(Ok(other)) => panic!("unexpected relay frame: {other:?}"),
            Some(Err(error)) => panic!("relay error: {error}"),
            None => panic!("relay ended"),
        }
    }
}

async fn recv_relay_until(ws: &mut RelayWs, predicate: impl Fn(&Value) -> bool) -> Value {
    for _ in 0..80 {
        let frame = recv_relay(ws).await;
        if predicate(&frame) {
            return frame;
        }
    }
    panic!("expected relay frame not found");
}

struct LiveStack {
    dir: std::path::PathBuf,
    addr: std::net::SocketAddr,
    user_token: String,
    device_id: String,
    relay_arg: String,
    plugin: Arc<PhonePlugin>,
    _session: Arc<AgentSession>,
    server: tokio::task::JoinHandle<()>,
}

/// Real loopback stack: central relay HTTP/WS server, native phone plugin as an
/// agent, and an `AgentSession` driven by a scripted stream that completes
/// every turn immediately.
async fn start_live_stack(tag: &str) -> LiveStack {
    let dir = std::env::temp_dir().join(format!("pir-phone-relay-{tag}-{}", uuid::Uuid::new_v4()));
    let store = PhoneStore::open(&dir).expect("open phone store");
    let user = store.create_user("relay-user", None).expect("create user");
    let (user_token, _) = store
        .create_token(&user.id, "user", Some("test"), None)
        .expect("user token");
    let device = store
        .create_device(&user.id, "relay-device", None)
        .expect("create device");
    let (device_token, _) = store
        .create_token(&user.id, "device", Some("test"), Some(&device.id))
        .expect("device token");

    let config = ServerConfig::load(
        dir.clone(),
        "127.0.0.1:0".parse().expect("listen addr"),
        None,
        Vec::new(),
    )
    .expect("server config");
    let server_state = PhoneServerState::new(config, store);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind relay");
    let addr = listener.local_addr().expect("relay addr");
    let server = tokio::spawn(async move {
        let _ = phone_http::serve(listener, server_state).await;
    });

    let release: Arc<Mutex<Option<oneshot::Receiver<()>>>> = Arc::new(Mutex::new(None));
    let plugin = Arc::new(PhonePlugin::new());
    let session = AgentSession::new_arc_with_plugins(
        session_config(release),
        vec![plugin.clone() as Arc<dyn AgentPlugin>],
    );
    let relay_arg = format!(
        "ws://{addr} {device_token} --device-name relay-device --device-id {}",
        device.id
    );
    plugin
        .start_remote_for_test(&relay_arg)
        .await
        .expect("connect relay");
    wait_until(|| plugin.remote_connected_for_test(), "relay handshake").await;

    LiveStack {
        dir,
        addr,
        user_token,
        device_id: device.id,
        relay_arg,
        plugin,
        _session: session,
        server,
    }
}

async fn connect_phone(addr: std::net::SocketAddr, token: &str) -> RelayWs {
    let mut request = format!("ws://{addr}/ws")
        .into_client_request()
        .expect("ws request");
    request.headers_mut().insert(
        "Authorization",
        HeaderValue::from_str(&format!("Bearer {token}")).expect("auth header"),
    );
    tokio_tungstenite::connect_async(request)
        .await
        .expect("connect phone client")
        .0
}

async fn parent_session_id(ws: &mut RelayWs) -> String {
    let catalog = recv_relay_until(ws, |frame| {
        frame["channel"] == "sessions" && frame["event"] == "catalog"
    })
    .await;
    let session_id = catalog["data"]["sessions"][0]["id"]
        .as_str()
        .expect("relay catalog session")
        .to_owned();
    assert!(session_id.ends_with(":parent-session"));
    session_id
}

async fn select_session(ws: &mut RelayWs, session_id: &str) {
    ws.send(WsMessage::Text(
        serde_json::json!({"kind":"session-select","sessionId":session_id})
            .to_string()
            .into(),
    ))
    .await
    .expect("select session");
    recv_relay_until(ws, |frame| {
        frame["channel"] == "sessions" && frame["event"] == "selected"
    })
    .await;
}

/// Consume rpc frames until `agent_end`, returning the assistant `message_end`
/// payloads observed (one per completed assistant turn).
async fn assistant_ends_until_agent_end(ws: &mut RelayWs) -> Vec<Value> {
    let mut ends = Vec::new();
    loop {
        let frame = recv_relay(ws).await;
        if frame["channel"] != "rpc" {
            continue;
        }
        match frame["payload"]["type"].as_str() {
            Some("message_end")
                if frame["payload"]["message"]["role"].as_str() == Some("assistant") =>
            {
                ends.push(frame["payload"]["message"].clone());
            }
            Some("agent_end") => return ends,
            _ => {}
        }
    }
}

async fn prompt(ws: &mut RelayWs, id: &str, text: &str) {
    ws.send(WsMessage::Text(
        serde_json::json!({
            "kind": "rpc",
            "command": { "type": "prompt", "message": text, "id": id }
        })
        .to_string()
        .into(),
    ))
    .await
    .expect("send prompt");
    let response = recv_relay_until(ws, |frame| {
        frame["channel"] == "rpc"
            && frame["payload"]["type"] == "response"
            && frame["payload"]["id"] == id
    })
    .await;
    assert_eq!(response["payload"]["success"], Value::Bool(true));
}

fn pool_test_model_dir() -> Option<String> {
    std::env::var("PIR_PHONE_TEST_MODEL_DIR")
        .ok()
        .filter(|dir| !dir.trim().is_empty())
}

/// Point the pool child at the isolated agent dir and the just-built `pir`
/// binary. Same wiring the single-child pool branch uses.
fn install_pool_test_env(model_dir: &str) {
    std::env::set_var("PIR_AGENT_DIR", model_dir);
    if std::env::var_os("PI_PHONE_SESSION_POOL_EXECUTABLE").is_none() {
        let pir_binary = std::env::current_exe()
            .ok()
            .and_then(|exe| {
                exe.parent()
                    .and_then(|deps| deps.parent())
                    .map(|debug| debug.join("pir"))
            })
            .expect("target debug directory");
        std::env::set_var("PI_PHONE_SESSION_POOL_EXECUTABLE", pir_binary);
    }
}

/// Send one rpc command to the selected session and return its raw response
/// envelope (routed back only to this phone client).
async fn rpc_call(ws: &mut RelayWs, id: &str, mut command: Value) -> Value {
    command["id"] = Value::String(id.to_owned());
    ws.send(WsMessage::Text(
        serde_json::json!({ "kind": "rpc", "command": command })
            .to_string()
            .into(),
    ))
    .await
    .expect("send rpc command");
    recv_relay_until(ws, |frame| {
        frame["channel"] == "rpc"
            && frame["payload"]["type"] == "response"
            && frame["payload"]["id"] == id
    })
    .await
}

async fn rpc_ok(ws: &mut RelayWs, id: &str, command: Value) -> Value {
    let response = rpc_call(ws, id, command).await;
    assert_eq!(
        response["payload"]["success"],
        Value::Bool(true),
        "rpc {id} failed: {response}"
    );
    response["payload"]["data"].clone()
}

/// Wait for a catalog carrying exactly `expected` pool rows and return it.
async fn wait_parallel_catalog(ws: &mut RelayWs, expected: usize) -> Value {
    recv_relay_until(ws, |frame| {
        frame["channel"] == "sessions"
            && frame["event"] == "catalog"
            && frame["data"]["sessions"]
                .as_array()
                .map(|rows| rows.iter().filter(|row| row["kind"] == "parallel").count())
                .unwrap_or(0)
                == expected
    })
    .await
}

fn catalog_child_ids(catalog: &Value) -> Vec<String> {
    let mut ids: Vec<String> = catalog["data"]["sessions"]
        .as_array()
        .expect("catalog rows")
        .iter()
        .filter(|row| row["kind"] == "parallel")
        .map(|row| {
            row["id"]
                .as_str()
                .expect("parallel session id")
                .rsplit(':')
                .next()
                .expect("local session id")
                .to_owned()
        })
        .collect();
    ids.sort();
    ids
}

/// Consume rpc frames until `agent_end`, asserting every live pool event is
/// attributed to `worker` (the selected child), and return assistant
/// `message_end` payloads.
async fn assistant_ends_routed_to(ws: &mut RelayWs, worker: &str) -> Vec<Value> {
    let mut ends = Vec::new();
    loop {
        let frame = recv_relay(ws).await;
        if frame["channel"] != "rpc" {
            continue;
        }
        let payload_type = frame["payload"]["type"].as_str().unwrap_or("");
        if payload_type == "response" {
            continue;
        }
        assert_eq!(
            frame["sessionWorkerId"].as_str(),
            Some(worker),
            "rpc frame routed to a different pool session: {frame}"
        );
        match payload_type {
            "message_end" if frame["payload"]["message"]["role"].as_str() == Some("assistant") => {
                ends.push(frame["payload"]["message"].clone());
            }
            "agent_end" => return ends,
            _ => {}
        }
    }
}

/// Wait for `agent_end` after an abort, ignoring other frames.
async fn wait_agent_end(ws: &mut RelayWs) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let frame = recv_relay(ws).await;
        if frame["channel"] == "rpc" && frame["payload"]["type"] == "agent_end" {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "aborted run did not settle"
        );
    }
}

/// Multi-client + agent reconnect + revoke, all over real TCP/WS loopback.
/// Covers the resync-on-join path for the parent session (`server/client-error`
/// is rejected by `recv_relay` for every frame).
#[tokio::test]
async fn phone_multiclient_reconnect_and_revoke_share_state_without_errors() {
    let stack = start_live_stack("multi").await;

    let mut first = connect_phone(stack.addr, &stack.user_token).await;
    let session_id = parent_session_id(&mut first).await;
    select_session(&mut first, &session_id).await;

    // A second phone joins the same parent session; its resync must not
    // produce client-error, and both clients share one session state.
    let mut second = connect_phone(stack.addr, &stack.user_token).await;
    assert_eq!(parent_session_id(&mut second).await, session_id);
    select_session(&mut second, &session_id).await;

    prompt(&mut first, "p1", "hello multi client").await;
    let first_ends = assistant_ends_until_agent_end(&mut first).await;
    let second_ends = assistant_ends_until_agent_end(&mut second).await;
    assert_eq!(first_ends.len(), 1, "first client assistant message_end");
    assert_eq!(second_ends.len(), 1, "second client assistant message_end");
    assert_eq!(
        first_ends[0]["content"], second_ends[0]["content"],
        "both subscribers must observe the same assistant message"
    );

    // Agent disconnects: subscribers are told the device went offline.
    stack
        .plugin
        .stop_remote_for_test()
        .await
        .expect("disconnect relay");
    wait_until(
        || !stack.plugin.remote_connected_for_test(),
        "agent disconnect",
    )
    .await;
    for client in [&mut first, &mut second] {
        recv_relay_until(client, |frame| {
            frame["channel"] == "server" && frame["event"] == "device-offline"
        })
        .await;
    }

    // Agent reconnects with the same device identity; a new client join then
    // exercises the resync path again and a prompt still works end to end.
    stack
        .plugin
        .start_remote_for_test(&stack.relay_arg)
        .await
        .expect("reconnect relay");
    wait_until(
        || stack.plugin.remote_connected_for_test(),
        "agent reconnect",
    )
    .await;

    let mut third = connect_phone(stack.addr, &stack.user_token).await;
    assert_eq!(parent_session_id(&mut third).await, session_id);
    select_session(&mut third, &session_id).await;
    prompt(&mut third, "p2", "hello after reconnect").await;
    let third_ends = assistant_ends_until_agent_end(&mut third).await;
    assert_eq!(third_ends.len(), 1, "post-reconnect assistant message_end");

    // Revoking the device closes the agent link and notifies session
    // subscribers; a further prompt is rejected as agent-offline.
    let response = reqwest::Client::new()
        .delete(format!(
            "http://{}/api/devices/{}",
            stack.addr, stack.device_id
        ))
        .bearer_auth(&stack.user_token)
        .send()
        .await
        .expect("revoke request");
    assert!(
        response.status().is_success(),
        "revoke failed: {response:?}"
    );
    wait_until(
        || !stack.plugin.remote_connected_for_test(),
        "agent link closed by revoke",
    )
    .await;
    for client in [&mut first, &mut second, &mut third] {
        recv_relay_until(client, |frame| {
            frame["channel"] == "server" && frame["event"] == "device-offline"
        })
        .await;
    }
    third
        .send(WsMessage::Text(
            serde_json::json!({
                "kind": "rpc",
                "command": { "type": "prompt", "message": "after revoke", "id": "p3" }
            })
            .to_string()
            .into(),
        ))
        .await
        .expect("send prompt after revoke");
    let rejected = recv_relay_until(&mut third, |frame| {
        frame["channel"] == "server" && frame["event"] == "error"
    })
    .await;
    assert_eq!(
        rejected["data"]["code"], "agent-offline",
        "prompt after device revoke must be rejected: {rejected}"
    );

    drop(stack._session);
    stack.server.abort();
    let _ = std::fs::remove_dir_all(&stack.dir);
}

#[tokio::test]
async fn phone_plugin_connects_to_central_relay() {
    let dir = std::env::temp_dir().join(format!("pir-phone-relay-{}", uuid::Uuid::new_v4()));
    let store = PhoneStore::open(&dir).expect("open phone store");
    let user = store.create_user("relay-user", None).expect("create user");
    let (user_token, _) = store
        .create_token(&user.id, "user", Some("test"), None)
        .expect("user token");
    let device = store
        .create_device(&user.id, "relay-device", None)
        .expect("create device");
    let (device_token, _) = store
        .create_token(&user.id, "device", Some("test"), Some(&device.id))
        .expect("device token");

    let config = ServerConfig::load(
        dir.clone(),
        "127.0.0.1:0".parse().expect("listen addr"),
        None,
        Vec::new(),
    )
    .expect("server config");
    let server_state = PhoneServerState::new(config, store);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind relay");
    let addr = listener.local_addr().expect("relay addr");
    let server = tokio::spawn(async move {
        let _ = phone_http::serve(listener, server_state).await;
    });

    let real_pool_model_dir = std::env::var("PIR_PHONE_TEST_MODEL_DIR").ok();
    if let Some(model_dir) = &real_pool_model_dir {
        std::env::set_var("PIR_AGENT_DIR", model_dir);
        if std::env::var_os("PI_PHONE_SESSION_POOL_EXECUTABLE").is_none() {
            let pir_binary = std::env::current_exe()
                .ok()
                .and_then(|exe| {
                    exe.parent()
                        .and_then(|deps| deps.parent())
                        .map(|debug| debug.join("pir"))
                })
                .expect("target debug directory");
            std::env::set_var("PI_PHONE_SESSION_POOL_EXECUTABLE", pir_binary);
        }
    }

    let (release_tx, release_rx) = oneshot::channel();
    let release = Arc::new(Mutex::new(Some(release_rx)));
    let plugin = Arc::new(PhonePlugin::new());
    let session = AgentSession::new_arc_with_plugins(
        session_config(release),
        vec![plugin.clone() as Arc<dyn AgentPlugin>],
    );

    plugin
        .start_remote_for_test(&format!(
            "ws://{addr} {device_token} --device-name relay-device --device-id {}",
            device.id
        ))
        .await
        .expect("connect relay");
    wait_until(|| plugin.remote_connected_for_test(), "relay handshake").await;

    let mut request = format!("ws://{addr}/ws")
        .into_client_request()
        .expect("ws request");
    request.headers_mut().insert(
        "Authorization",
        HeaderValue::from_str(&format!("Bearer {user_token}")).expect("auth header"),
    );
    let (mut phone, _) = tokio_tungstenite::connect_async(request)
        .await
        .expect("connect phone client");

    let catalog = recv_relay_until(&mut phone, |frame| {
        frame["channel"] == "sessions" && frame["event"] == "catalog"
    })
    .await;
    let session_id = catalog["data"]["sessions"][0]["id"]
        .as_str()
        .expect("relay catalog session")
        .to_owned();
    assert!(session_id.ends_with(":parent-session"));

    phone
        .send(WsMessage::Text(
            serde_json::json!({"kind":"session-select","sessionId":session_id})
                .to_string()
                .into(),
        ))
        .await
        .expect("select session");
    recv_relay_until(&mut phone, |frame| {
        frame["channel"] == "sessions" && frame["event"] == "selected"
    })
    .await;

    phone
        .send(WsMessage::Text(
            serde_json::json!({
                "kind": "rpc",
                "command": { "type": "prompt", "message": "hello relay", "id": "p1" }
            })
            .to_string()
            .into(),
        ))
        .await
        .expect("send prompt");

    let response = recv_relay_until(&mut phone, |frame| {
        frame["channel"] == "rpc"
            && frame["payload"]["type"] == "response"
            && frame["payload"]["id"] == "p1"
    })
    .await;
    assert_eq!(response["payload"]["success"], Value::Bool(true));

    release_tx.send(()).expect("release scripted stream");
    let _ = recv_relay_until(&mut phone, |frame| {
        frame["channel"] == "rpc" && frame["payload"]["type"] == "agent_end"
    })
    .await;
    let snapshot = plugin.published_snapshot().expect("published snapshot");
    let text = snapshot.to_string();
    assert!(
        text.contains("scripted reply"),
        "snapshot must include reply: {text}"
    );

    if real_pool_model_dir.is_some() {
        phone
            .send(WsMessage::Text(
                serde_json::json!({ "kind": "session-spawn" })
                    .to_string()
                    .into(),
            ))
            .await
            .expect("send session-spawn");
        let pool_catalog = recv_relay_until(&mut phone, |frame| {
            frame["channel"] == "sessions"
                && frame["event"] == "catalog"
                && frame["data"]["sessions"]
                    .as_array()
                    .is_some_and(|sessions| {
                        sessions.iter().any(|session| session["kind"] == "parallel")
                    })
        })
        .await;
        let child_id = pool_catalog["data"]["sessions"]
            .as_array()
            .expect("catalog rows")
            .iter()
            .find(|session| session["kind"] == "parallel")
            .and_then(|session| session["id"].as_str())
            .expect("parallel session id")
            .to_owned();
        phone
            .send(WsMessage::Text(
                serde_json::json!({ "kind": "session-select", "sessionId": child_id })
                    .to_string()
                    .into(),
            ))
            .await
            .expect("select parallel session");
        recv_relay_until(&mut phone, |frame| {
            frame["channel"] == "sessions" && frame["event"] == "selected"
        })
        .await;
        phone
            .send(WsMessage::Text(
                serde_json::json!({
                    "kind": "rpc",
                    "command": { "type": "get_state", "id": "child-state" }
                })
                .to_string()
                .into(),
            ))
            .await
            .expect("child get_state");
        let child_state = recv_relay_until(&mut phone, |frame| {
            frame["channel"] == "rpc"
                && frame["payload"]["type"] == "response"
                && frame["payload"]["id"] == "child-state"
        })
        .await;
        assert_eq!(child_state["payload"]["success"], Value::Bool(true));
    }

    plugin
        .stop_remote_for_test()
        .await
        .expect("disconnect relay");
    drop(session);
    server.abort();
    let _ = std::fs::remove_dir_all(dir);
}

/// Full pool coverage over the real relay: three children spawned through the
/// phone protocol, catalog shape/order, per-child routing for prompt,
/// set_model, set_thinking_level, abort, get_state and the child RPC tree,
/// sibling isolation, a second-client resync, and device revoke.
///
/// Runs on a multi-thread runtime: `SessionPool::spawn` starts a real
/// `pir --mode rpc` child synchronously, and three cold Windows debug-binary
/// starts would otherwise block a current-thread test runtime long enough to
/// expire every frame deadline.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn phone_pool_multi_child_routing_isolation_and_resync() {
    let Some(model_dir) = pool_test_model_dir() else {
        eprintln!("PIR_PHONE_TEST_MODEL_DIR not set; skipping phone pool multi-child coverage");
        return;
    };
    install_pool_test_env(&model_dir);
    let stack = start_live_stack("pool-full").await;

    let mut phone = connect_phone(stack.addr, &stack.user_token).await;
    let parent_id = parent_session_id(&mut phone).await;
    select_session(&mut phone, &parent_id).await;

    // Three spawns in one batch: rows appear progressively, the final catalog
    // must carry all three children next to the parent.
    for _ in 0..3 {
        phone
            .send(WsMessage::Text(
                serde_json::json!({ "kind": "session-spawn" })
                    .to_string()
                    .into(),
            ))
            .await
            .expect("send session-spawn");
    }
    let catalog = wait_parallel_catalog(&mut phone, 3).await;
    let rows = catalog["data"]["sessions"]
        .as_array()
        .expect("catalog rows");
    assert_eq!(rows.len(), 4, "parent plus three children: {catalog}");
    // Relay catalog ids are qualified as `<device-id>:<local-id>`.
    let parent_rows: Vec<&Value> = rows.iter().filter(|row| row["kind"] == "parent").collect();
    assert_eq!(parent_rows.len(), 1, "exactly one parent row: {catalog}");
    assert!(
        parent_rows[0]["id"]
            .as_str()
            .is_some_and(|id| id.ends_with(":parent-session")),
        "parent row id: {}",
        parent_rows[0]["id"]
    );
    let parallel_rows: Vec<&Value> = rows
        .iter()
        .filter(|row| row["kind"] == "parallel")
        .collect();
    assert_eq!(parallel_rows.len(), 3, "three parallel rows: {catalog}");
    for row in &parallel_rows {
        assert!(
            row["label"].as_str().is_some_and(|label| !label.is_empty()),
            "parallel row needs a label: {row}"
        );
    }
    assert_eq!(
        catalog_child_ids(&catalog),
        [
            "active-session-1".to_owned(),
            "active-session-2".to_owned(),
            "active-session-3".to_owned()
        ]
    );
    // Catalog order is most-recently-active first (`sortedWorkers`); the relay
    // sorts the parent row in the same list.
    let activity: Vec<i64> = rows
        .iter()
        .map(|row| row["lastActivityAt"].as_i64().unwrap_or(0))
        .collect();
    assert!(
        activity.windows(2).all(|pair| pair[0] >= pair[1]),
        "catalog rows must be ordered by lastActivityAt desc: {activity:?}"
    );

    let catalog_id_for = |local: &str| -> String {
        rows.iter()
            .find(|row| {
                row["id"]
                    .as_str()
                    .is_some_and(|id| id.ends_with(&format!(":{local}")))
            })
            .and_then(|row| row["id"].as_str())
            .unwrap_or_else(|| panic!("catalog row for {local}"))
            .to_owned()
    };
    let child1_catalog = catalog_id_for("active-session-1");
    let child2_catalog = catalog_id_for("active-session-2");
    let child1 = "active-session-1";
    let child2 = "active-session-2";
    select_session(&mut phone, &child1_catalog).await;
    let initial = rpc_ok(
        &mut phone,
        "state-1a",
        serde_json::json!({ "type": "get_state" }),
    )
    .await;
    assert_eq!(initial["model"]["id"], "gpt-5");
    assert_eq!(initial["thinkingLevel"], "low");
    assert_eq!(initial["isStreaming"], false);

    // Model/thinking changes land on the selected child only.
    rpc_ok(
        &mut phone,
        "model-1",
        serde_json::json!({ "type": "set_model", "provider": "openai", "modelId": "gpt-5-mini" }),
    )
    .await;
    rpc_ok(
        &mut phone,
        "thinking-1",
        serde_json::json!({ "type": "set_thinking_level", "level": "high" }),
    )
    .await;
    let changed = rpc_ok(
        &mut phone,
        "state-1b",
        serde_json::json!({ "type": "get_state" }),
    )
    .await;
    assert_eq!(changed["model"]["id"], "gpt-5-mini");
    assert_eq!(changed["thinkingLevel"], "high");

    select_session(&mut phone, &child2_catalog).await;
    let sibling = rpc_ok(
        &mut phone,
        "state-2a",
        serde_json::json!({ "type": "get_state" }),
    )
    .await;
    assert_eq!(
        sibling["model"]["id"], "gpt-5",
        "model switch must not leak into sibling"
    );
    assert_eq!(
        sibling["thinkingLevel"], "low",
        "thinking switch must not leak into sibling"
    );
    assert_eq!(sibling["isStreaming"], false);

    // Prompt routing: the run and its assistant message belong to child 2.
    prompt(&mut phone, "p-child2", "hello child two").await;
    let ends = assistant_ends_routed_to(&mut phone, child2).await;
    assert_eq!(ends.len(), 1, "child 2 assistant message_end");
    assert!(
        ends[0]["content"].to_string().contains("mock reply"),
        "child 2 must receive the mock reply: {}",
        ends[0]
    );

    // get_state and the phone tree on the selected child 1. `phone_get_tree`
    // is a phone-only wire name; the pool must translate it to the child RPC
    // `get_tree` and answer in the parent host's phone tree shape
    // (PIR-BIN-007 regression).
    select_session(&mut phone, &child1_catalog).await;
    let tree = rpc_ok(
        &mut phone,
        "tree-1",
        serde_json::json!({ "type": "get_tree" }),
    )
    .await;
    assert!(tree.get("tree").is_some(), "child tree payload: {tree}");
    assert!(tree.get("leafId").is_some(), "child leaf: {tree}");
    let phone_tree = rpc_ok(
        &mut phone,
        "phone-tree-1",
        serde_json::json!({ "type": "phone_get_tree" }),
    )
    .await;
    assert!(
        phone_tree.get("currentLeafId").is_some(),
        "phone tree leaf: {phone_tree}"
    );
    assert!(
        phone_tree["nodes"].is_array(),
        "phone tree nodes: {phone_tree}"
    );
    assert!(
        phone_tree["currentPathIds"].is_array(),
        "phone tree path: {phone_tree}"
    );
    let state_1c = rpc_ok(
        &mut phone,
        "state-1c",
        serde_json::json!({ "type": "get_state" }),
    )
    .await;
    assert_eq!(state_1c["model"]["id"], "gpt-5-mini");
    assert_eq!(state_1c["thinkingLevel"], "high");
    assert_eq!(state_1c["messageCount"], 0);

    // Abort only the selected child's in-flight run (slow scripted mock).
    rpc_ok(
        &mut phone,
        "model-slow",
        serde_json::json!({ "type": "set_model", "provider": "slowmock", "modelId": "slow-1" }),
    )
    .await;
    prompt(&mut phone, "p-slow", "start a slow reply").await;
    // Wait for streamed content (not just `message_start`, which the child
    // emits before the provider request) so the abort interrupts a live run.
    recv_relay_until(&mut phone, |frame| {
        frame["channel"] == "rpc"
            && frame["sessionWorkerId"] == "active-session-1"
            && frame["payload"]["type"] == "message_update"
    })
    .await;
    let abort = rpc_call(
        &mut phone,
        "abort-1",
        serde_json::json!({ "type": "abort" }),
    )
    .await;
    assert_eq!(abort["payload"]["success"], Value::Bool(true));
    wait_agent_end(&mut phone).await;
    // `agent_end` and the session's streaming flag settle asynchronously;
    // poll the child state until the abort has landed.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let mut attempt = 0u32;
    let mut settled = rpc_ok(
        &mut phone,
        "state-1d",
        serde_json::json!({ "type": "get_state" }),
    )
    .await;
    while settled["isStreaming"] == Value::Bool(true) && tokio::time::Instant::now() < deadline {
        attempt += 1;
        tokio::time::sleep(Duration::from_millis(200)).await;
        settled = rpc_ok(
            &mut phone,
            &format!("state-settle-{attempt}"),
            serde_json::json!({ "type": "get_state" }),
        )
        .await;
    }
    assert_eq!(
        settled["isStreaming"], false,
        "aborted child must stop streaming: {settled}"
    );

    // The sibling is unaffected: its next prompt still completes normally.
    select_session(&mut phone, &child2_catalog).await;
    prompt(&mut phone, "p-child2b", "hello again child two").await;
    let sibling_ends = assistant_ends_routed_to(&mut phone, child2).await;
    assert_eq!(sibling_ends.len(), 1, "sibling works after child 1 abort");

    // The phone tree of a child that has real entries: flat rows, kinds and
    // previews match the parent host's `phone_get_tree` shape, and the leaf
    // path ends at `currentLeafId`.
    let child2_tree = rpc_ok(
        &mut phone,
        "phone-tree-2",
        serde_json::json!({ "type": "phone_get_tree" }),
    )
    .await;
    let nodes = child2_tree["nodes"]
        .as_array()
        .unwrap_or_else(|| panic!("phone tree nodes: {child2_tree}"));
    assert!(!nodes.is_empty(), "child 2 phone tree: {child2_tree}");
    assert!(
        nodes.iter().any(|node| node["type"] == "assistant_message"),
        "assistant rows: {child2_tree}"
    );
    assert!(
        nodes.iter().any(|node| node["summary"]["preview"]
            .to_string()
            .contains("mock reply")),
        "assistant preview: {child2_tree}"
    );
    let leaf = child2_tree["currentLeafId"]
        .as_str()
        .unwrap_or_else(|| panic!("child 2 leaf: {child2_tree}"));
    let path = child2_tree["currentPathIds"]
        .as_array()
        .expect("current path ids");
    assert_eq!(
        path.last().and_then(Value::as_str),
        Some(leaf),
        "path must end at the leaf: {child2_tree}"
    );

    // Resync: a second phone client sees the same catalog and the child state
    // survives the rejoin (no server/client-error frames by construction).
    let mut second = connect_phone(stack.addr, &stack.user_token).await;
    let second_catalog = recv_relay_until(&mut second, |frame| {
        frame["channel"] == "sessions" && frame["event"] == "catalog"
    })
    .await;
    assert_eq!(
        catalog_child_ids(&second_catalog),
        catalog_child_ids(&catalog)
    );
    select_session(&mut second, &child1_catalog).await;
    let resynced = rpc_ok(
        &mut second,
        "state-resync",
        serde_json::json!({ "type": "get_state" }),
    )
    .await;
    assert_eq!(
        resynced["model"]["id"], "slow-1",
        "child model survives resync"
    );
    assert_eq!(
        resynced["thinkingLevel"], "high",
        "child thinking survives resync"
    );
    assert_eq!(resynced["isStreaming"], false);

    // Device revoke closes the socket; both clients see device-offline and a
    // later command is rejected as agent-offline.
    let response = reqwest::Client::new()
        .delete(format!(
            "http://{}/api/devices/{}",
            stack.addr, stack.device_id
        ))
        .bearer_auth(&stack.user_token)
        .send()
        .await
        .expect("revoke request");
    assert!(
        response.status().is_success(),
        "revoke failed: {response:?}"
    );
    wait_until(
        || !stack.plugin.remote_connected_for_test(),
        "pool agent link closed by revoke",
    )
    .await;
    for client in [&mut phone, &mut second] {
        recv_relay_until(client, |frame| {
            frame["channel"] == "server" && frame["event"] == "device-offline"
        })
        .await;
    }
    second
        .send(WsMessage::Text(
            serde_json::json!({
                "kind": "rpc",
                "command": { "type": "get_state", "id": "after-revoke" }
            })
            .to_string()
            .into(),
        ))
        .await
        .expect("send command after revoke");
    let rejected = recv_relay_until(&mut second, |frame| {
        frame["channel"] == "server" && frame["event"] == "error"
    })
    .await;
    assert_eq!(
        rejected["data"]["code"], "agent-offline",
        "command after pool device revoke must be rejected: {rejected}"
    );

    let stack_dir = stack.dir.clone();
    stack.server.abort();
    drop(stack);
    let _ = std::fs::remove_dir_all(&stack_dir);
}

/// Drain every relay frame for `window`, returning the rpc response frames.
async fn drain_rpc_responses(ws: &mut RelayWs, window: Duration) -> Vec<Value> {
    let deadline = tokio::time::Instant::now() + window;
    let mut responses = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return responses;
        }
        match tokio::time::timeout(remaining, ws.next()).await {
            Ok(Some(Ok(WsMessage::Text(text)))) => {
                let frame: Value = serde_json::from_str(&text).expect("relay frame is JSON");
                if frame["channel"] == "rpc" && frame["payload"]["type"] == "response" {
                    responses.push(frame);
                }
            }
            Ok(Some(Ok(WsMessage::Ping(_) | WsMessage::Pong(_)))) => {}
            Ok(Some(Ok(WsMessage::Close(frame)))) => panic!("relay closed: {frame:?}"),
            Ok(Some(Ok(other))) => panic!("unexpected relay frame: {other:?}"),
            Ok(Some(Err(error))) => panic!("relay error: {error}"),
            Ok(None) => panic!("relay ended"),
            Err(_) => return responses,
        }
    }
}

/// A correlated pool-child RPC must produce exactly one phone-facing response
/// frame. The pool's stdout reader also parses the child's raw `response` line;
/// unless that correlated copy is suppressed it is fanned out as an rpc event
/// (with the relay's `forwardId`, since the first response consumed the
/// pending mapping) next to the bridge's own response — the same command and
/// payload twice, and visible to every subscriber of the session.
#[tokio::test]
async fn phone_pool_child_correlated_response_is_emitted_once() {
    let Some(model_dir) = pool_test_model_dir() else {
        eprintln!(
            "PIR_PHONE_TEST_MODEL_DIR not set; skipping phone pool response duplication coverage"
        );
        return;
    };
    install_pool_test_env(&model_dir);
    let stack = start_live_stack("pool-response-once").await;

    let mut phone = connect_phone(stack.addr, &stack.user_token).await;
    let parent_id = parent_session_id(&mut phone).await;
    select_session(&mut phone, &parent_id).await;

    phone
        .send(WsMessage::Text(
            serde_json::json!({ "kind": "session-spawn" })
                .to_string()
                .into(),
        ))
        .await
        .expect("send session-spawn");
    let catalog = wait_parallel_catalog(&mut phone, 1).await;
    let child_catalog = catalog["data"]["sessions"]
        .as_array()
        .expect("catalog rows")
        .iter()
        .find(|row| row["kind"] == "parallel")
        .and_then(|row| row["id"].as_str())
        .expect("parallel row id")
        .to_owned();
    assert!(child_catalog.ends_with(":active-session-1"));
    select_session(&mut phone, &child_catalog).await;

    let response = rpc_call(
        &mut phone,
        "dedupe-state-1",
        serde_json::json!({ "type": "get_state" }),
    )
    .await;
    assert_eq!(response["payload"]["success"], Value::Bool(true));

    let extra = drain_rpc_responses(&mut phone, Duration::from_millis(1500)).await;
    assert!(
        extra.is_empty(),
        "a correlated child rpc response must be emitted once; got {} extra response frame(s): {extra:?}",
        extra.len()
    );

    let stack_dir = stack.dir.clone();
    stack.server.abort();
    drop(stack);
    let _ = std::fs::remove_dir_all(&stack_dir);
}

/// Two phone clients subscribed to the same pool child issue the same child
/// command at the same time. Relay routing must deliver each response only to
/// its origin, with that origin's client id, exactly once — no cross-client
/// leakage and no duplicated broadcast copy (the PIR-BIN-008 correlated
/// response suppression applies per request, not per session).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn phone_pool_two_clients_same_child_command_get_own_response_only() {
    let Some(model_dir) = pool_test_model_dir() else {
        eprintln!("PIR_PHONE_TEST_MODEL_DIR not set; skipping two-client pool response coverage");
        return;
    };
    install_pool_test_env(&model_dir);
    let stack = start_live_stack("pool-two-clients").await;

    let mut first = connect_phone(stack.addr, &stack.user_token).await;
    let parent_id = parent_session_id(&mut first).await;
    select_session(&mut first, &parent_id).await;

    first
        .send(WsMessage::Text(
            serde_json::json!({ "kind": "session-spawn" })
                .to_string()
                .into(),
        ))
        .await
        .expect("send session-spawn");
    let catalog = wait_parallel_catalog(&mut first, 1).await;
    let child_catalog = catalog["data"]["sessions"]
        .as_array()
        .expect("catalog rows")
        .iter()
        .find(|row| row["kind"] == "parallel")
        .and_then(|row| row["id"].as_str())
        .expect("parallel row id")
        .to_owned();
    select_session(&mut first, &child_catalog).await;

    // Second phone joins and selects the same child.
    let mut second = connect_phone(stack.addr, &stack.user_token).await;
    let second_catalog = recv_relay_until(&mut second, |frame| {
        frame["channel"] == "sessions" && frame["event"] == "catalog"
    })
    .await;
    assert!(
        catalog_child_ids(&second_catalog)
            .contains(&child_catalog.rsplit(':').next().unwrap().to_owned()),
        "second client catalog must contain the same child: {second_catalog}"
    );
    select_session(&mut second, &child_catalog).await;

    // Same command, distinct client ids, issued back-to-back before either
    // response can settle.
    first
        .send(WsMessage::Text(
            serde_json::json!({
                "kind": "rpc",
                "command": { "type": "get_state", "id": "two-client-a" }
            })
            .to_string()
            .into(),
        ))
        .await
        .expect("send first rpc");
    second
        .send(WsMessage::Text(
            serde_json::json!({
                "kind": "rpc",
                "command": { "type": "get_state", "id": "two-client-b" }
            })
            .to_string()
            .into(),
        ))
        .await
        .expect("send second rpc");

    let (first_responses, second_responses) = tokio::join!(
        drain_rpc_responses(&mut first, Duration::from_millis(2000)),
        drain_rpc_responses(&mut second, Duration::from_millis(2000)),
    );
    assert_eq!(
        first_responses.len(),
        1,
        "first client must receive exactly its own response: {first_responses:?}"
    );
    assert_eq!(
        second_responses.len(),
        1,
        "second client must receive exactly its own response: {second_responses:?}"
    );
    assert_eq!(first_responses[0]["payload"]["id"], "two-client-a");
    assert_eq!(second_responses[0]["payload"]["id"], "two-client-b");
    assert_eq!(first_responses[0]["payload"]["success"], Value::Bool(true));
    assert_eq!(second_responses[0]["payload"]["success"], Value::Bool(true));
    assert_eq!(
        first_responses[0]["sessionWorkerId"], second_responses[0]["sessionWorkerId"],
        "both responses must carry the same selected child worker id"
    );
    assert_eq!(
        first_responses[0]["payload"]["data"]["isStreaming"],
        Value::Bool(false)
    );

    let stack_dir = stack.dir.clone();
    stack.server.abort();
    drop(stack);
    let _ = std::fs::remove_dir_all(&stack_dir);
}

/// Collect every relay frame received during `window` (raw; used to inspect
/// stream-frame attribution instead of stopping at the first expected event).
async fn collect_relay_frames(ws: &mut RelayWs, window: Duration) -> Vec<Value> {
    let deadline = tokio::time::Instant::now() + window;
    let mut frames = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return frames;
        }
        match tokio::time::timeout(remaining, ws.next()).await {
            Ok(Some(Ok(WsMessage::Text(text)))) => {
                frames.push(serde_json::from_str(&text).expect("relay frame is JSON"));
            }
            Ok(Some(Ok(WsMessage::Ping(_) | WsMessage::Pong(_)))) => {}
            Ok(Some(Ok(WsMessage::Close(frame)))) => panic!("relay closed: {frame:?}"),
            Ok(Some(Ok(other))) => panic!("unexpected relay frame: {other:?}"),
            Ok(Some(Err(error))) => panic!("relay error: {error}"),
            Ok(None) => panic!("relay ended"),
            Err(_) => return frames,
        }
    }
}

/// Two phone clients subscribed to the same pool child send a prompt at the
/// same time. Responses must stay per-origin, while live run events must be
/// broadcast to both subscribers with the same child worker id, the same
/// order, and exactly one `message_end` per accepted prompt.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn phone_pool_two_clients_prompt_same_child_stream_attribution() {
    let Some(model_dir) = pool_test_model_dir() else {
        eprintln!("PIR_PHONE_TEST_MODEL_DIR not set; skipping dual-prompt pool coverage");
        return;
    };
    install_pool_test_env(&model_dir);
    let stack = start_live_stack("pool-dual-prompt").await;

    let mut first = connect_phone(stack.addr, &stack.user_token).await;
    let parent_id = parent_session_id(&mut first).await;
    select_session(&mut first, &parent_id).await;
    first
        .send(WsMessage::Text(
            serde_json::json!({ "kind": "session-spawn" })
                .to_string()
                .into(),
        ))
        .await
        .expect("send session-spawn");
    let catalog = wait_parallel_catalog(&mut first, 1).await;
    let child_catalog = catalog["data"]["sessions"]
        .as_array()
        .expect("catalog rows")
        .iter()
        .find(|row| row["kind"] == "parallel")
        .and_then(|row| row["id"].as_str())
        .expect("parallel row id")
        .to_owned();
    assert!(child_catalog.ends_with(":active-session-1"));
    select_session(&mut first, &child_catalog).await;

    let mut second = connect_phone(stack.addr, &stack.user_token).await;
    let second_catalog = recv_relay_until(&mut second, |frame| {
        frame["channel"] == "sessions" && frame["event"] == "catalog"
    })
    .await;
    assert!(
        catalog_child_ids(&second_catalog).contains(&"active-session-1".to_owned()),
        "second client must see the existing child: {second_catalog}"
    );
    select_session(&mut second, &child_catalog).await;

    // Both prompts hit the same child before either response can settle.
    first
        .send(WsMessage::Text(
            serde_json::json!({
                "kind": "rpc",
                "command": { "type": "prompt", "message": "dual prompt A", "id": "p-dual-a" }
            })
            .to_string()
            .into(),
        ))
        .await
        .expect("send prompt A");
    second
        .send(WsMessage::Text(
            serde_json::json!({
                "kind": "rpc",
                "command": { "type": "prompt", "message": "dual prompt B", "id": "p-dual-b" }
            })
            .to_string()
            .into(),
        ))
        .await
        .expect("send prompt B");

    let (first_frames, second_frames) = tokio::join!(
        collect_relay_frames(&mut first, Duration::from_secs(6)),
        collect_relay_frames(&mut second, Duration::from_secs(6)),
    );

    for (label, frames) in [("first", &first_frames), ("second", &second_frames)] {
        for frame in frames {
            assert!(
                !(frame["channel"] == "server" && frame["event"] == "client-error"),
                "{label} received server/client-error: {frame}"
            );
            assert!(
                !(frame["channel"] == "server" && frame["event"] == "error"),
                "{label} received server/error: {frame}"
            );
        }
    }

    let responses = |frames: &[Value]| -> Vec<Value> {
        frames
            .iter()
            .filter(|frame| frame["channel"] == "rpc" && frame["payload"]["type"] == "response")
            .cloned()
            .collect()
    };
    let first_responses = responses(&first_frames);
    let second_responses = responses(&second_frames);
    assert_eq!(
        first_responses.len(),
        1,
        "first client must get exactly its own prompt response: {first_responses:?}"
    );
    assert_eq!(
        second_responses.len(),
        1,
        "second client must get exactly its own prompt response: {second_responses:?}"
    );
    assert_eq!(first_responses[0]["payload"]["id"], "p-dual-a");
    assert_eq!(second_responses[0]["payload"]["id"], "p-dual-b");
    let ok_a = first_responses[0]["payload"]["success"] == Value::Bool(true);
    let ok_b = second_responses[0]["payload"]["success"] == Value::Bool(true);
    assert!(
        ok_a || ok_b,
        "at least one of the two prompts must be accepted: {first_responses:?} / {second_responses:?}"
    );
    if !ok_a {
        let error = first_responses[0]["payload"]["error"].to_string();
        assert!(
            !error.is_empty(),
            "rejected prompt A needs a clear error: {first_responses:?}"
        );
    }
    if !ok_b {
        let error = second_responses[0]["payload"]["error"].to_string();
        assert!(
            !error.is_empty(),
            "rejected prompt B needs a clear error: {second_responses:?}"
        );
    }

    let events = |frames: &[Value]| -> Vec<Value> {
        frames
            .iter()
            .filter(|frame| frame["channel"] == "rpc" && frame["payload"]["type"] != "response")
            .cloned()
            .collect()
    };
    let first_events = events(&first_frames);
    let second_events = events(&second_frames);
    let message_key = |frame: &Value| -> String {
        frame["payload"]["message"]["id"]
            .as_str()
            .map(str::to_owned)
            .or_else(|| {
                frame["payload"]["message"]["timestamp"]
                    .as_i64()
                    .map(|stamp| format!("#{stamp}"))
            })
            .unwrap_or_default()
    };
    let signature = |events: &[Value]| -> Vec<String> {
        events
            .iter()
            .map(|frame| {
                format!(
                    "{}:{}",
                    frame["payload"]["type"].as_str().unwrap_or(""),
                    message_key(frame)
                )
            })
            .collect()
    };
    assert_eq!(
        signature(&first_events),
        signature(&second_events),
        "both clients must observe the same live event order\nfirst={first_events:?}\nsecond={second_events:?}"
    );
    for (label, events) in [("first", &first_events), ("second", &second_events)] {
        for frame in events {
            let payload_type = frame["payload"]["type"].as_str().unwrap_or("");
            if matches!(
                payload_type,
                "message_start" | "message_update" | "message_end" | "agent_start" | "agent_end"
            ) {
                assert_eq!(
                    frame["sessionWorkerId"].as_str(),
                    Some("active-session-1"),
                    "{label} live event attributed to the wrong worker: {frame}"
                );
            }
        }
    }

    let assistant_end_ids = |events: &[Value]| -> Vec<String> {
        events
            .iter()
            .filter(|frame| {
                frame["payload"]["type"] == "message_end"
                    && frame["payload"]["message"]["role"] == "assistant"
            })
            .map(message_key)
            .collect()
    };
    let first_ends = assistant_end_ids(&first_events);
    let second_ends = assistant_end_ids(&second_events);
    assert_eq!(
        first_ends, second_ends,
        "both clients must see the same assistant message_end ids"
    );
    let mut unique_ends = first_ends.clone();
    unique_ends.sort();
    unique_ends.dedup();
    assert_eq!(
        unique_ends.len(),
        first_ends.len(),
        "assistant message_end must not be duplicated: {first_ends:?}"
    );
    let accepted = usize::from(ok_a) + usize::from(ok_b);
    assert_eq!(
        first_ends.len(),
        accepted,
        "every accepted prompt must produce exactly one assistant message_end (accepted={accepted}, ends={first_ends:?})"
    );
    let agent_ends = |events: &[Value]| -> usize {
        events
            .iter()
            .filter(|frame| frame["payload"]["type"] == "agent_end")
            .count()
    };
    assert_eq!(
        agent_ends(&first_events),
        accepted,
        "one agent_end per accepted prompt"
    );
    assert_eq!(
        agent_ends(&second_events),
        accepted,
        "one agent_end per accepted prompt"
    );

    let update_ids = |events: &[Value]| -> Vec<String> {
        events
            .iter()
            .filter(|frame| frame["payload"]["type"] == "message_update")
            .map(message_key)
            .collect()
    };
    let first_updates = update_ids(&first_events);
    assert_eq!(
        first_updates,
        update_ids(&second_events),
        "both clients must see the same message_update stream"
    );
    if !first_ends.is_empty() {
        let first_update_pos = first_events
            .iter()
            .position(|frame| frame["payload"]["type"] == "message_update")
            .unwrap_or(usize::MAX);
        let first_end_pos = first_events
            .iter()
            .position(|frame| {
                frame["payload"]["type"] == "message_end"
                    && frame["payload"]["message"]["role"] == "assistant"
            })
            .unwrap_or(usize::MAX);
        assert!(
            first_update_pos < first_end_pos,
            "assistant message_end must be preceded by message_update frames"
        );
    }

    eprintln!(
        "dual-prompt: ok_a={ok_a} ok_b={ok_b} ends={:?} updates={} agent_ends={}",
        first_ends,
        first_updates.len(),
        agent_ends(&first_events)
    );
    eprintln!(
        "dual-prompt responses: first={} second={}",
        first_responses[0], second_responses[0]
    );
    eprintln!("dual-prompt signature: {:?}", signature(&first_events));

    let stack_dir = stack.dir.clone();
    stack.server.abort();
    drop(stack);
    let _ = std::fs::remove_dir_all(&stack_dir);
}

// ---------------------------------------------------------------------------
// Queued prompt paths (`streamingBehavior: "steer" | "followUp"`)
// ---------------------------------------------------------------------------

/// The shared pool fixture defaults to the fast fixed mock. These cases need a
/// model that keeps streaming while a second client enqueues a message, so
/// they only run when the slow scripted mock (port 57321 from the pool
/// fixture's agent dir) is up and the harness opted in with
/// `PIR_PHONE_TEST_SLOW=1`.
fn slow_pool_test_model_dir() -> Option<String> {
    match std::env::var("PIR_PHONE_TEST_SLOW") {
        Ok(value) if value.trim() == "1" => pool_test_model_dir(),
        _ => None,
    }
}

async fn send_prompt_command(ws: &mut RelayWs, id: &str, text: &str, behavior: Option<&str>) {
    let mut command = serde_json::json!({
        "type": "prompt",
        "message": text,
        "id": id,
    });
    if let Some(behavior) = behavior {
        command["streamingBehavior"] = Value::String(behavior.to_owned());
    }
    ws.send(WsMessage::Text(
        serde_json::json!({ "kind": "rpc", "command": command })
            .to_string()
            .into(),
    ))
    .await
    .expect("send prompt");
}

/// Read frames until the first assistant streaming update, keeping every
/// frame so the caller can still assert on the full sequence.
async fn wait_for_first_stream_update(ws: &mut RelayWs, seen: &mut Vec<Value>) {
    loop {
        let frame = recv_relay(ws).await;
        let hit = frame["channel"] == "rpc" && frame["payload"]["type"] == "message_update";
        seen.push(frame);
        if hit {
            return;
        }
    }
}

/// Collect every frame through the run's `agent_end` (or panic on timeout).
async fn collect_frames_until_agent_end(
    ws: &mut RelayWs,
    mut frames: Vec<Value>,
    window: Duration,
) -> Vec<Value> {
    let deadline = tokio::time::Instant::now() + window;
    loop {
        let finished = frames
            .iter()
            .any(|frame| frame["channel"] == "rpc" && frame["payload"]["type"] == "agent_end");
        if finished {
            return frames;
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(
            !remaining.is_zero(),
            "timed out waiting for agent_end; got frames={frames:?}"
        );
        match tokio::time::timeout(remaining, ws.next()).await {
            Ok(Some(Ok(WsMessage::Text(text)))) => {
                frames.push(serde_json::from_str(&text).expect("relay frame is JSON"));
            }
            Ok(Some(Ok(WsMessage::Ping(_) | WsMessage::Pong(_)))) => {}
            Ok(Some(Ok(WsMessage::Close(frame)))) => panic!("relay closed: {frame:?}"),
            Ok(Some(Ok(other))) => panic!("unexpected relay frame: {other:?}"),
            Ok(Some(Err(error))) => panic!("relay error: {error}"),
            Ok(None) => panic!("relay ended"),
            Err(_) => panic!("timed out waiting for agent_end; got frames={frames:?}"),
        }
    }
}

/// Start the real relay stack, spawn one pool child, join two clients to it,
/// and point the child at the slow scripted model.
async fn start_slow_pool_with_two_clients(tag: &str) -> (LiveStack, RelayWs, RelayWs) {
    let model_dir = slow_pool_test_model_dir().expect("slow pool model dir");
    start_pool_with_two_clients(tag, &model_dir).await
}

/// Shared pool-child + two-client setup against an arbitrary isolated agent
/// dir (the spawn target and model provider come from that dir's models.json).
async fn start_pool_with_two_clients(tag: &str, model_dir: &str) -> (LiveStack, RelayWs, RelayWs) {
    install_pool_test_env(model_dir);
    let stack = start_live_stack(tag).await;

    let mut first = connect_phone(stack.addr, &stack.user_token).await;
    let parent_id = parent_session_id(&mut first).await;
    select_session(&mut first, &parent_id).await;
    first
        .send(WsMessage::Text(
            serde_json::json!({ "kind": "session-spawn" })
                .to_string()
                .into(),
        ))
        .await
        .expect("send session-spawn");
    let catalog = wait_parallel_catalog(&mut first, 1).await;
    let child_catalog = catalog["data"]["sessions"]
        .as_array()
        .expect("catalog rows")
        .iter()
        .find(|row| row["kind"] == "parallel")
        .and_then(|row| row["id"].as_str())
        .expect("parallel row id")
        .to_owned();
    select_session(&mut first, &child_catalog).await;

    let mut second = connect_phone(stack.addr, &stack.user_token).await;
    let second_catalog = recv_relay_until(&mut second, |frame| {
        frame["channel"] == "sessions" && frame["event"] == "catalog"
    })
    .await;
    assert!(
        catalog_child_ids(&second_catalog)
            .contains(&child_catalog.rsplit(':').next().unwrap().to_owned()),
        "second client must see the existing child: {second_catalog}"
    );
    select_session(&mut second, &child_catalog).await;

    rpc_ok(
        &mut first,
        "queue-set-model",
        serde_json::json!({
            "type": "set_model",
            "provider": "slowmock",
            "modelId": "slow-1",
        }),
    )
    .await;

    (stack, first, second)
}

fn trim_at_agent_end(frames: Vec<Value>) -> Vec<Value> {
    match frames
        .iter()
        .position(|frame| frame["channel"] == "rpc" && frame["payload"]["type"] == "agent_end")
    {
        Some(position) => frames[..=position].to_vec(),
        None => frames,
    }
}

fn rpc_parts(frames: &[Value]) -> (Vec<Value>, Vec<Value>) {
    let responses = frames
        .iter()
        .filter(|frame| frame["channel"] == "rpc" && frame["payload"]["type"] == "response")
        .cloned()
        .collect();
    let events = frames
        .iter()
        .filter(|frame| frame["channel"] == "rpc" && frame["payload"]["type"] != "response")
        .cloned()
        .collect();
    (responses, events)
}

fn message_key(frame: &Value) -> String {
    frame["payload"]["message"]["id"]
        .as_str()
        .map(str::to_owned)
        .or_else(|| {
            frame["payload"]["message"]["timestamp"]
                .as_i64()
                .map(|stamp| format!("#{stamp}"))
        })
        .unwrap_or_default()
}

fn message_text(frame: &Value) -> String {
    frame["payload"]["message"]["content"]
        .as_array()
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|block| block["text"].as_str())
                .collect::<String>()
        })
        .unwrap_or_default()
}

/// Order/identity signature for broadcast events: run lifecycle types keep
/// their role+message identity, queue updates keep both queue snapshots.
fn queue_event_signature(events: &[Value]) -> Vec<String> {
    events
        .iter()
        .map(|frame| {
            let payload = &frame["payload"];
            let kind = payload["type"].as_str().unwrap_or("");
            match kind {
                "message_start" | "message_update" | "message_end" => format!(
                    "{kind}:{}:{}",
                    payload["message"]["role"].as_str().unwrap_or(""),
                    message_key(frame)
                ),
                "queue_update" => format!(
                    "queue_update:{}{}",
                    payload["steering"], payload["followUp"]
                ),
                other => other.to_owned(),
            }
        })
        .collect()
}

/// Shared assertions for both queued paths: identical event order on both
/// clients, one queue_update carrying the marker, exactly one delivered user
/// message, two assistant turns, one run end, and no duplicate message ids.
fn assert_queued_run_events(
    label: &str,
    first_events: &[Value],
    second_events: &[Value],
    marker: &str,
    queue_field: &str,
) {
    assert_eq!(
        queue_event_signature(first_events),
        queue_event_signature(second_events),
        "{label}: both clients must observe the same event order\nfirst={first_events:?}\nsecond={second_events:?}"
    );
    for frame in first_events.iter().chain(second_events) {
        let payload_type = frame["payload"]["type"].as_str().unwrap_or("");
        if matches!(
            payload_type,
            "message_start"
                | "message_update"
                | "message_end"
                | "agent_start"
                | "agent_end"
                | "queue_update"
        ) {
            assert_eq!(
                frame["sessionWorkerId"].as_str(),
                Some("active-session-1"),
                "{label}: event attributed to the wrong worker: {frame}"
            );
        }
    }

    let queued_position = first_events
        .iter()
        .position(|frame| {
            frame["payload"]["type"] == "queue_update"
                && frame["payload"][queue_field]
                    .as_array()
                    .is_some_and(|texts| texts.iter().any(|text| text.as_str() == Some(marker)))
        })
        .unwrap_or_else(|| {
            panic!(
                "{label}: missing queue_update carrying {queue_field}={marker}: {first_events:?}"
            )
        });
    let delivered_ends: Vec<usize> = first_events
        .iter()
        .enumerate()
        .filter(|(_, frame)| {
            frame["payload"]["type"] == "message_end"
                && frame["payload"]["message"]["role"] == "user"
                && message_text(frame).contains(marker)
        })
        .map(|(index, _)| index)
        .collect();
    assert_eq!(
        delivered_ends.len(),
        1,
        "{label}: queued message must be delivered exactly once: {first_events:?}"
    );
    assert!(
        queued_position < delivered_ends[0],
        "{label}: queue_update must precede delivery (queued at {queued_position}, delivered at {})",
        delivered_ends[0]
    );

    let assistant_keys: Vec<String> = first_events
        .iter()
        .filter(|frame| {
            frame["payload"]["type"] == "message_end"
                && frame["payload"]["message"]["role"] == "assistant"
        })
        .map(message_key)
        .collect();
    assert_eq!(
        assistant_keys.len(),
        2,
        "{label}: the queued turn must add exactly one assistant message: {assistant_keys:?}"
    );
    let mut unique_keys = assistant_keys.clone();
    unique_keys.sort();
    unique_keys.dedup();
    assert_eq!(
        unique_keys.len(),
        2,
        "{label}: assistant message_end duplicated: {assistant_keys:?}"
    );
    let agent_ends = first_events
        .iter()
        .filter(|frame| frame["payload"]["type"] == "agent_end")
        .count();
    assert_eq!(
        agent_ends, 1,
        "{label}: one queue drain belongs to one run end"
    );
}

/// Client B sends `streamingBehavior: "steer"` while client A's prompt is
/// still streaming: the message must join the current run's steering queue,
/// be announced/delivered exactly once, and both clients must see the same
/// event order while each receives only its own response.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn phone_pool_two_clients_steer_queues_into_current_run() {
    let Some(_model_dir) = slow_pool_test_model_dir() else {
        eprintln!("PIR_PHONE_TEST_SLOW not set; skipping phone steer queue coverage");
        return;
    };
    let (stack, mut first, mut second) = start_slow_pool_with_two_clients("pool-steer-queue").await;

    send_prompt_command(&mut first, "p-steer-a", "start slow run A", None).await;
    let mut first_frames = Vec::new();
    wait_for_first_stream_update(&mut first, &mut first_frames).await;
    send_prompt_command(&mut second, "p-steer-b", "STEER-MARKER-B", Some("steer")).await;

    let (first_frames, second_frames) = tokio::join!(
        collect_frames_until_agent_end(&mut first, first_frames, Duration::from_secs(20)),
        collect_frames_until_agent_end(&mut second, Vec::new(), Duration::from_secs(20)),
    );
    let first_frames = trim_at_agent_end(first_frames);
    let second_frames = trim_at_agent_end(second_frames);
    let (first_responses, first_events) = rpc_parts(&first_frames);
    let (second_responses, second_events) = rpc_parts(&second_frames);

    assert_eq!(first_responses.len(), 1, "first: {first_responses:?}");
    assert_eq!(second_responses.len(), 1, "second: {second_responses:?}");
    assert_eq!(first_responses[0]["payload"]["id"], "p-steer-a");
    assert_eq!(second_responses[0]["payload"]["id"], "p-steer-b");
    assert_eq!(first_responses[0]["payload"]["success"], Value::Bool(true));
    assert_eq!(second_responses[0]["payload"]["success"], Value::Bool(true));
    assert_eq!(
        first_responses[0]["payload"]["data"]["disposition"],
        "started"
    );
    assert_eq!(
        second_responses[0]["payload"]["data"]["disposition"],
        "queued"
    );
    assert_eq!(first_responses[0]["sessionWorkerId"], "active-session-1");
    assert_eq!(second_responses[0]["sessionWorkerId"], "active-session-1");

    assert_queued_run_events(
        "steer",
        &first_events,
        &second_events,
        "STEER-MARKER-B",
        "steering",
    );
    eprintln!(
        "steer-queue: responses={first_responses:?} / {second_responses:?} events={:?}",
        queue_event_signature(&first_events)
    );

    let stack_dir = stack.dir.clone();
    stack.server.abort();
    drop(stack);
    let _ = std::fs::remove_dir_all(&stack_dir);
}

/// Client B sends `streamingBehavior: "followUp"` while client A's prompt is
/// still streaming: the message must wait in the follow-up queue and be
/// delivered only after the first assistant turn, without loss/duplication,
/// while responses stay attributed to their originating client.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn phone_pool_two_clients_follow_up_queues_next_turn() {
    let Some(_model_dir) = slow_pool_test_model_dir() else {
        eprintln!("PIR_PHONE_TEST_SLOW not set; skipping phone follow-up queue coverage");
        return;
    };
    let (stack, mut first, mut second) =
        start_slow_pool_with_two_clients("pool-follow-up-queue").await;

    send_prompt_command(&mut first, "p-follow-a", "start slow run B", None).await;
    let mut first_frames = Vec::new();
    wait_for_first_stream_update(&mut first, &mut first_frames).await;
    send_prompt_command(
        &mut second,
        "p-follow-b",
        "FOLLOWUP-MARKER-B",
        Some("followUp"),
    )
    .await;

    let (first_frames, second_frames) = tokio::join!(
        collect_frames_until_agent_end(&mut first, first_frames, Duration::from_secs(20)),
        collect_frames_until_agent_end(&mut second, Vec::new(), Duration::from_secs(20)),
    );
    let first_frames = trim_at_agent_end(first_frames);
    let second_frames = trim_at_agent_end(second_frames);
    let (first_responses, first_events) = rpc_parts(&first_frames);
    let (second_responses, second_events) = rpc_parts(&second_frames);

    assert_eq!(first_responses.len(), 1, "first: {first_responses:?}");
    assert_eq!(second_responses.len(), 1, "second: {second_responses:?}");
    assert_eq!(first_responses[0]["payload"]["id"], "p-follow-a");
    assert_eq!(second_responses[0]["payload"]["id"], "p-follow-b");
    assert_eq!(first_responses[0]["payload"]["success"], Value::Bool(true));
    assert_eq!(second_responses[0]["payload"]["success"], Value::Bool(true));
    assert_eq!(
        first_responses[0]["payload"]["data"]["disposition"],
        "started"
    );
    assert_eq!(
        second_responses[0]["payload"]["data"]["disposition"],
        "queued"
    );
    assert_eq!(first_responses[0]["sessionWorkerId"], "active-session-1");
    assert_eq!(second_responses[0]["sessionWorkerId"], "active-session-1");

    assert_queued_run_events(
        "followUp",
        &first_events,
        &second_events,
        "FOLLOWUP-MARKER-B",
        "followUp",
    );

    // The follow-up is delivered only after the first assistant turn has
    // completed (not while it streams): its user message_end must follow the
    // first assistant message_end.
    let first_assistant_end = first_events
        .iter()
        .position(|frame| {
            frame["payload"]["type"] == "message_end"
                && frame["payload"]["message"]["role"] == "assistant"
        })
        .expect("first assistant message_end");
    let follow_up_end = first_events
        .iter()
        .position(|frame| {
            frame["payload"]["type"] == "message_end"
                && frame["payload"]["message"]["role"] == "user"
                && message_text(frame).contains("FOLLOWUP-MARKER-B")
        })
        .expect("follow-up delivery");
    assert!(
        first_assistant_end < follow_up_end,
        "followUp must wait for the current turn to finish (assistant end {first_assistant_end}, delivery {follow_up_end})"
    );

    eprintln!(
        "follow-up-queue: responses={first_responses:?} / {second_responses:?} events={:?}",
        queue_event_signature(&first_events)
    );

    let stack_dir = stack.dir.clone();
    stack.server.abort();
    drop(stack);
    let _ = std::fs::remove_dir_all(&stack_dir);
}

// ---------------------------------------------------------------------------
// cycle36/37: tool-batch boundary queueing (steer vs followUp), multiple
// steering order, and abort/clear_queue semantics.
//
// Fixture: a scripted mock (`tool_mock_57322.py`) whose first step is a
// `bash` tool call running `sleep 4`, so a steering/follow-up message can be
// queued while the child's run is really executing a tool batch. The second
// and later steps are plain text replies (the last step repeats).
// ---------------------------------------------------------------------------

/// Opt-in tool-batch fixture: `PIR_PHONE_TEST_TOOL=1` plus the agent dir
/// whose `slowmock` provider points at the tool mock (port 57322).
fn tool_pool_test_model_dir() -> Option<String> {
    match std::env::var("PIR_PHONE_TEST_TOOL") {
        Ok(value) if value.trim() == "1" => std::env::var("PIR_PHONE_TEST_TOOL_DIR")
            .ok()
            .filter(|dir| !dir.trim().is_empty()),
        _ => None,
    }
}

fn tool_mock_log_path() -> Option<String> {
    std::env::var("PIR_PHONE_TEST_TOOL_LOG")
        .ok()
        .filter(|path| !path.trim().is_empty())
}

/// Opt-in All-mode fixture: `PIR_PHONE_TEST_TOOL_ALL=1` plus a separate agent
/// dir whose `settings.json` sets `steeringMode`/`followUpMode` to `all`.
fn tool_pool_all_test_model_dir() -> Option<String> {
    match std::env::var("PIR_PHONE_TEST_TOOL_ALL") {
        Ok(value) if value.trim() == "1" => std::env::var("PIR_PHONE_TEST_TOOL_ALL_DIR")
            .ok()
            .filter(|dir| !dir.trim().is_empty()),
        _ => None,
    }
}

fn tool_mock_all_log_path() -> Option<String> {
    std::env::var("PIR_PHONE_TEST_TOOL_ALL_LOG")
        .ok()
        .filter(|path| !path.trim().is_empty())
}

async fn start_tool_pool_all_with_two_clients(tag: &str) -> (LiveStack, RelayWs, RelayWs) {
    let model_dir = tool_pool_all_test_model_dir().expect("tool pool all model dir");
    start_pool_with_two_clients(tag, &model_dir).await
}

async fn start_tool_pool_with_two_clients(tag: &str) -> (LiveStack, RelayWs, RelayWs) {
    let model_dir = tool_pool_test_model_dir().expect("tool pool model dir");
    start_pool_with_two_clients(tag, &model_dir).await
}

/// Collect frames until the first `tool_execution_start` (the run is now
/// really executing the tool batch) and return its tool call id.
async fn wait_for_tool_execution_start(ws: &mut RelayWs, seen: &mut Vec<Value>) -> String {
    loop {
        let frame = recv_relay(ws).await;
        let is_start =
            frame["channel"] == "rpc" && frame["payload"]["type"] == "tool_execution_start";
        let call_id = frame["payload"]["toolCallId"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        seen.push(frame);
        if is_start {
            assert!(
                !call_id.is_empty(),
                "tool_execution_start without toolCallId"
            );
            return call_id;
        }
    }
}

fn event_index(frames: &[Value], predicate: impl Fn(&Value) -> bool) -> Option<usize> {
    frames.iter().position(predicate)
}

fn mock_requests_for(log_path: &str, first_user_needle: &str) -> Vec<Value> {
    let raw = std::fs::read_to_string(log_path).unwrap_or_default();
    raw.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|entry| {
            entry["first_user"]
                .as_str()
                .is_some_and(|text| text.contains(first_user_needle))
        })
        .collect()
}

/// Transcript view for queue-delivery assertions: role + concatenated text.
fn transcript_rows(data: &Value) -> Vec<(String, String)> {
    data["messages"]
        .as_array()
        .expect("get_messages data.messages")
        .iter()
        .map(|message| {
            let role = message["role"].as_str().unwrap_or("").to_owned();
            let text = message["content"]
                .as_array()
                .map(|blocks| {
                    blocks
                        .iter()
                        .filter_map(|block| block["text"].as_str())
                        .collect::<String>()
                })
                .unwrap_or_default();
            (role, text)
        })
        .collect()
}

fn delivered_user_end(frames: &[Value], marker: &str) -> Option<usize> {
    event_index(frames, |frame| {
        frame["payload"]["type"] == "message_end"
            && frame["payload"]["message"]["role"] == "user"
            && message_text(frame).contains(marker)
    })
}

fn assistant_end_positions(frames: &[Value]) -> Vec<usize> {
    frames
        .iter()
        .enumerate()
        .filter(|(_, frame)| {
            frame["payload"]["type"] == "message_end"
                && frame["payload"]["message"]["role"] == "assistant"
        })
        .map(|(index, _)| index)
        .collect()
}

/// Client B steers while client A's run is executing a real `bash` tool call:
/// the steering message must be injected only after the tool batch has
/// finished and before the next model request (never between the tool call
/// and its result), exactly once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn phone_pool_steer_waits_for_tool_batch_boundary() {
    let Some(_model_dir) = tool_pool_test_model_dir() else {
        eprintln!("PIR_PHONE_TEST_TOOL not set; skipping tool-batch steer coverage");
        return;
    };
    let (stack, mut first, mut second) = start_tool_pool_with_two_clients("pool-tool-steer").await;

    send_prompt_command(
        &mut first,
        "p-tool-steer-a",
        "TOOLBATCH-STEER-A start",
        None,
    )
    .await;
    let mut first_frames = Vec::new();
    let tool_call_id = wait_for_tool_execution_start(&mut first, &mut first_frames).await;
    send_prompt_command(
        &mut second,
        "p-tool-steer-b",
        "TOOL-STEER-MARKER",
        Some("steer"),
    )
    .await;

    let (first_frames, second_frames) = tokio::join!(
        collect_frames_until_agent_end(&mut first, first_frames, Duration::from_secs(30)),
        collect_frames_until_agent_end(&mut second, Vec::new(), Duration::from_secs(30)),
    );
    let first_frames = trim_at_agent_end(first_frames);
    let second_frames = trim_at_agent_end(second_frames);
    let (first_responses, first_events) = rpc_parts(&first_frames);
    let (second_responses, second_events) = rpc_parts(&second_frames);

    assert_eq!(first_responses.len(), 1, "first: {first_responses:?}");
    assert_eq!(second_responses.len(), 1, "second: {second_responses:?}");
    assert_eq!(
        first_responses[0]["payload"]["data"]["disposition"],
        "started"
    );
    assert_eq!(
        second_responses[0]["payload"]["data"]["disposition"],
        "queued"
    );
    assert_eq!(first_responses[0]["payload"]["id"], "p-tool-steer-a");
    assert_eq!(second_responses[0]["payload"]["id"], "p-tool-steer-b");

    assert_eq!(
        queue_event_signature(&first_events),
        queue_event_signature(&second_events),
        "both clients must observe the same event order"
    );

    let queue_at = event_index(&first_events, |frame| {
        frame["payload"]["type"] == "queue_update"
            && frame["payload"]["steering"]
                .as_array()
                .is_some_and(|texts| {
                    texts
                        .iter()
                        .any(|text| text.as_str() == Some("TOOL-STEER-MARKER"))
                })
    })
    .unwrap_or_else(|| panic!("missing steering queue_update: {first_events:?}"));
    let tool_start_at = event_index(&first_events, |frame| {
        frame["payload"]["type"] == "tool_execution_start"
            && frame["payload"]["toolCallId"].as_str() == Some(tool_call_id.as_str())
    })
    .expect("tool_execution_start");
    let tool_end_at = event_index(&first_events, |frame| {
        frame["payload"]["type"] == "tool_execution_end"
            && frame["payload"]["toolCallId"].as_str() == Some(tool_call_id.as_str())
    })
    .expect("tool_execution_end");
    assert!(
        tool_start_at < queue_at && queue_at < tool_end_at,
        "steer must be queued while the tool is still running (start {tool_start_at}, queue {queue_at}, end {tool_end_at})"
    );

    let delivered_at = delivered_user_end(&first_events, "TOOL-STEER-MARKER")
        .expect("steering delivery message_end");
    assert!(
        tool_end_at < delivered_at,
        "steering must be injected only after the tool batch ended (tool end {tool_end_at}, delivery {delivered_at})"
    );
    for frame in &first_events[tool_start_at..=tool_end_at] {
        assert!(
            !(frame["payload"]["type"] == "message_end"
                && frame["payload"]["message"]["role"] == "user"
                && message_text(frame).contains("TOOL-STEER-MARKER")),
            "steering must not be inserted between the tool call and its result: {frame}"
        );
    }
    assert_eq!(
        delivered_user_end_occurrences(&first_events, "TOOL-STEER-MARKER"),
        1
    );

    let assistant_ends = assistant_end_positions(&first_events);
    assert_eq!(
        assistant_ends.len(),
        2,
        "tool turn + steer turn: {first_events:?}"
    );
    assert_eq!(
        first_events
            .iter()
            .filter(|frame| frame["payload"]["type"] == "agent_end")
            .count(),
        1
    );

    // Session transcript: steer user message sits after the persisted
    // toolResult and before the final assistant message.
    let transcript = rpc_ok(
        &mut first,
        "tool-steer-messages",
        serde_json::json!({"type":"get_messages"}),
    )
    .await;
    let rows = transcript_rows(&transcript);
    let tool_result_at = rows
        .iter()
        .position(|(role, _)| role == "toolResult")
        .expect("persisted toolResult");
    let marker_at = rows
        .iter()
        .position(|(_, text)| text.contains("TOOL-STEER-MARKER"))
        .expect("persisted steering user message");
    assert_eq!(rows[marker_at].0, "user");
    let final_assistant_at = rows
        .iter()
        .enumerate()
        .filter(|(index, (role, _))| *index > marker_at && role == "assistant")
        .map(|(index, _)| index)
        .next()
        .expect("assistant turn after steering");
    assert!(
        tool_result_at < marker_at && marker_at < final_assistant_at,
        "transcript order toolResult({tool_result_at}) < steer({marker_at}) < assistant({final_assistant_at}): {rows:?}"
    );
    assert_eq!(
        rows.iter()
            .filter(|(_, text)| text.contains("TOOL-STEER-MARKER"))
            .count(),
        1
    );

    // The mock saw the steering text in exactly the second request of this
    // run (after the tool result), not in the first one.
    if let Some(log_path) = tool_mock_log_path() {
        let requests = mock_requests_for(&log_path, "TOOLBATCH-STEER-A");
        assert!(
            requests.len() >= 2,
            "expected tool + follow-up requests: {requests:?}"
        );
        assert_eq!(requests[0]["script_step"], 0);
        assert!(!requests[0]["last_user"]
            .as_str()
            .unwrap_or("")
            .contains("TOOL-STEER-MARKER"));
        let marker_requests: Vec<&Value> = requests
            .iter()
            .filter(|entry| {
                entry["last_user"]
                    .as_str()
                    .is_some_and(|text| text.contains("TOOL-STEER-MARKER"))
            })
            .collect();
        assert_eq!(
            marker_requests.len(),
            1,
            "steering reaches exactly one model request: {requests:?}"
        );
        assert_eq!(marker_requests[0]["script_step"], 1);
        assert!(marker_requests[0]["messages"].as_u64().unwrap_or(0) >= 4);
    }

    eprintln!(
        "tool-batch-steer: events={:?}",
        queue_event_signature(&first_events)
    );

    let stack_dir = stack.dir.clone();
    stack.server.abort();
    drop(stack);
    let _ = std::fs::remove_dir_all(&stack_dir);
}

fn delivered_user_end_occurrences(frames: &[Value], marker: &str) -> usize {
    frames
        .iter()
        .filter(|frame| {
            frame["payload"]["type"] == "message_end"
                && frame["payload"]["message"]["role"] == "user"
                && message_text(frame).contains(marker)
        })
        .count()
}

/// Client B sends followUp while A's run executes a tool batch: it must NOT be
/// injected at the tool-batch boundary, only once the run would otherwise
/// stop (after the terminating text turn, upstream `getFollowUpMessages`),
/// and only then continue the same run with exactly one delivery.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn phone_pool_follow_up_waits_for_tool_batch_end() {
    let Some(_model_dir) = tool_pool_test_model_dir() else {
        eprintln!("PIR_PHONE_TEST_TOOL not set; skipping tool-batch follow-up coverage");
        return;
    };
    let (stack, mut first, mut second) = start_tool_pool_with_two_clients("pool-tool-follow").await;

    send_prompt_command(
        &mut first,
        "p-tool-follow-a",
        "TOOLBATCH-FOLLOW-A start",
        None,
    )
    .await;
    let mut first_frames = Vec::new();
    let tool_call_id = wait_for_tool_execution_start(&mut first, &mut first_frames).await;
    send_prompt_command(
        &mut second,
        "p-tool-follow-b",
        "TOOL-FOLLOW-MARKER",
        Some("followUp"),
    )
    .await;

    let (first_frames, second_frames) = tokio::join!(
        collect_frames_until_agent_end(&mut first, first_frames, Duration::from_secs(30)),
        collect_frames_until_agent_end(&mut second, Vec::new(), Duration::from_secs(30)),
    );
    let first_frames = trim_at_agent_end(first_frames);
    let second_frames = trim_at_agent_end(second_frames);
    let (first_responses, first_events) = rpc_parts(&first_frames);
    let (second_responses, second_events) = rpc_parts(&second_frames);

    assert_eq!(first_responses.len(), 1, "first: {first_responses:?}");
    assert_eq!(second_responses.len(), 1, "second: {second_responses:?}");
    assert_eq!(
        first_responses[0]["payload"]["data"]["disposition"],
        "started"
    );
    assert_eq!(
        second_responses[0]["payload"]["data"]["disposition"],
        "queued"
    );
    assert_eq!(
        queue_event_signature(&first_events),
        queue_event_signature(&second_events)
    );

    let tool_end_at = event_index(&first_events, |frame| {
        frame["payload"]["type"] == "tool_execution_end"
            && frame["payload"]["toolCallId"].as_str() == Some(tool_call_id.as_str())
    })
    .expect("tool_execution_end");
    // Assistant #1 is the tool-call turn; assistant #2 is the terminating
    // text turn. followUp is only polled after that terminating turn.
    let assistant_ends = assistant_end_positions(&first_events);
    assert_eq!(
        assistant_ends.len(),
        3,
        "tool turn + text turn + follow-up turn: {first_events:?}"
    );
    let delivered_at =
        delivered_user_end(&first_events, "TOOL-FOLLOW-MARKER").expect("follow-up delivery");
    assert!(
        tool_end_at < delivered_at,
        "followUp must not be injected at the tool-batch boundary"
    );
    assert!(
        assistant_ends[1] < delivered_at && delivered_at < assistant_ends[2],
        "followUp waits for the terminating turn (assistant ends {assistant_ends:?}, delivery {delivered_at})"
    );
    assert_eq!(
        delivered_user_end_occurrences(&first_events, "TOOL-FOLLOW-MARKER"),
        1
    );
    assert_eq!(
        first_events
            .iter()
            .filter(|frame| frame["payload"]["type"] == "agent_end")
            .count(),
        1,
        "follow-up continuation belongs to the same run"
    );

    let transcript = rpc_ok(
        &mut first,
        "tool-follow-messages",
        serde_json::json!({"type":"get_messages"}),
    )
    .await;
    let rows = transcript_rows(&transcript);
    let marker_at = rows
        .iter()
        .position(|(_, text)| text.contains("TOOL-FOLLOW-MARKER"))
        .expect("persisted follow-up user message");
    assert_eq!(rows[marker_at].0, "user");
    let assistant_rows: Vec<usize> = rows
        .iter()
        .enumerate()
        .filter(|(_, (role, _))| role == "assistant")
        .map(|(index, _)| index)
        .collect();
    assert_eq!(assistant_rows.len(), 3, "{rows:?}");
    assert!(
        assistant_rows[1] < marker_at && marker_at < assistant_rows[2],
        "transcript follow-up sits after the terminating turn: {rows:?}"
    );

    if let Some(log_path) = tool_mock_log_path() {
        let requests = mock_requests_for(&log_path, "TOOLBATCH-FOLLOW-A");
        let marker_requests: Vec<&Value> = requests
            .iter()
            .filter(|entry| {
                entry["last_user"]
                    .as_str()
                    .is_some_and(|text| text.contains("TOOL-FOLLOW-MARKER"))
            })
            .collect();
        assert_eq!(
            marker_requests.len(),
            1,
            "follow-up reaches exactly one request: {requests:?}"
        );
        assert_eq!(
            marker_requests[0]["script_step"], 1,
            "the repeating last script step carries the follow-up"
        );
    }

    eprintln!(
        "tool-batch-followup: events={:?}",
        queue_event_signature(&first_events)
    );

    let stack_dir = stack.dir.clone();
    stack.server.abort();
    drop(stack);
    let _ = std::fs::remove_dir_all(&stack_dir);
}

/// Two steer messages queued back-to-back while A's tool batch runs must be
/// delivered in FIFO order. The default steering mode is one-at-a-time, so
/// each drain carries exactly one message: STEER-ONE reaches the first model
/// request after the tool batch, STEER-TWO the next one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn phone_pool_multiple_steer_preserves_order() {
    let Some(_model_dir) = tool_pool_test_model_dir() else {
        eprintln!("PIR_PHONE_TEST_TOOL not set; skipping multi-steer order coverage");
        return;
    };
    let (stack, mut first, mut second) = start_tool_pool_with_two_clients("pool-multi-steer").await;

    send_prompt_command(&mut first, "p-multi-a", "TOOLBATCH-ORDER-A start", None).await;
    let mut first_frames = Vec::new();
    let tool_call_id = wait_for_tool_execution_start(&mut first, &mut first_frames).await;
    send_prompt_command(&mut second, "p-multi-b1", "STEER-ONE", Some("steer")).await;
    send_prompt_command(&mut second, "p-multi-b2", "STEER-TWO", Some("steer")).await;

    let (first_frames, second_frames) = tokio::join!(
        collect_frames_until_agent_end(&mut first, first_frames, Duration::from_secs(30)),
        collect_frames_until_agent_end(&mut second, Vec::new(), Duration::from_secs(30)),
    );
    let first_frames = trim_at_agent_end(first_frames);
    let second_frames = trim_at_agent_end(second_frames);
    let (first_responses, first_events) = rpc_parts(&first_frames);
    let (second_responses, second_events) = rpc_parts(&second_frames);

    assert_eq!(first_responses.len(), 1, "first: {first_responses:?}");
    assert_eq!(second_responses.len(), 2, "second: {second_responses:?}");
    assert_eq!(
        first_responses[0]["payload"]["data"]["disposition"],
        "started"
    );
    for response in &second_responses {
        assert_eq!(
            response["payload"]["data"]["disposition"], "queued",
            "{response}"
        );
    }
    assert_eq!(
        queue_event_signature(&first_events),
        queue_event_signature(&second_events)
    );

    let tool_end_at = event_index(&first_events, |frame| {
        frame["payload"]["type"] == "tool_execution_end"
            && frame["payload"]["toolCallId"].as_str() == Some(tool_call_id.as_str())
    })
    .expect("tool_execution_end");
    let one_at = delivered_user_end(&first_events, "STEER-ONE").expect("STEER-ONE delivery");
    let two_at = delivered_user_end(&first_events, "STEER-TWO").expect("STEER-TWO delivery");
    assert!(tool_end_at < one_at, "first steer after the tool batch");
    assert!(
        one_at < two_at,
        "steering order must be FIFO (one {one_at}, two {two_at})"
    );
    assert_eq!(
        delivered_user_end_occurrences(&first_events, "STEER-ONE"),
        1
    );
    assert_eq!(
        delivered_user_end_occurrences(&first_events, "STEER-TWO"),
        1
    );

    let assistant_ends = assistant_end_positions(&first_events);
    assert_eq!(
        assistant_ends.len(),
        3,
        "tool turn + one steer turn each: {first_events:?}"
    );
    assert!(
        assistant_ends[0] < one_at && one_at < assistant_ends[1],
        "STEER-ONE belongs to the first post-tool request"
    );
    assert!(
        assistant_ends[1] < two_at && two_at < assistant_ends[2],
        "STEER-TWO belongs to the second post-tool request"
    );
    assert_eq!(
        first_events
            .iter()
            .filter(|frame| frame["payload"]["type"] == "agent_end")
            .count(),
        1
    );

    if let Some(log_path) = tool_mock_log_path() {
        let requests = mock_requests_for(&log_path, "TOOLBATCH-ORDER-A");
        let one_requests: Vec<&Value> = requests
            .iter()
            .filter(|entry| {
                entry["last_user"]
                    .as_str()
                    .is_some_and(|text| text.contains("STEER-ONE"))
            })
            .collect();
        let two_requests: Vec<&Value> = requests
            .iter()
            .filter(|entry| {
                entry["last_user"]
                    .as_str()
                    .is_some_and(|text| text.contains("STEER-TWO"))
            })
            .collect();
        assert_eq!(one_requests.len(), 1, "{requests:?}");
        assert_eq!(two_requests.len(), 1, "{requests:?}");
        assert!(
            one_requests[0]["ts"].as_f64() < two_requests[0]["ts"].as_f64(),
            "requests must follow queue order: {requests:?}"
        );
        assert_eq!(one_requests[0]["script_step"], 1);
        assert_eq!(two_requests[0]["script_step"], 1);
    }

    eprintln!(
        "multi-steer: events={:?}",
        queue_event_signature(&first_events)
    );

    let stack_dir = stack.dir.clone();
    stack.server.abort();
    drop(stack);
    let _ = std::fs::remove_dir_all(&stack_dir);
}

/// All-mode counterpart of `phone_pool_multiple_steer_preserves_order`: the
/// child's `settings.json` sets `steeringMode: "all"`, so two steers queued
/// during the same tool batch must be drained together in one poll — one
/// emptied `queue_update`, both user messages back-to-back before the next
/// assistant message, one combined follow-up request, and a single
/// `agent_end`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn phone_pool_all_mode_drains_all_steers_in_one_batch() {
    let Some(_model_dir) = tool_pool_all_test_model_dir() else {
        eprintln!("PIR_PHONE_TEST_TOOL_ALL not set; skipping All-mode steer coverage");
        return;
    };
    let (stack, mut first, mut second) =
        start_tool_pool_all_with_two_clients("pool-all-steer").await;

    let state = rpc_ok(
        &mut first,
        "all-mode-state",
        serde_json::json!({"type": "get_state"}),
    )
    .await;
    assert_eq!(
        state["steeringMode"], "all",
        "settings.json must select All mode: {state}"
    );
    assert_eq!(
        state["followUpMode"], "all",
        "settings.json must select All mode: {state}"
    );

    send_prompt_command(&mut first, "p-all-a", "TOOLBATCH-ALL-A start", None).await;
    let mut first_frames = Vec::new();
    let tool_call_id = wait_for_tool_execution_start(&mut first, &mut first_frames).await;
    send_prompt_command(&mut second, "p-all-b1", "STEER-ALL-ONE", Some("steer")).await;
    send_prompt_command(&mut second, "p-all-b2", "STEER-ALL-TWO", Some("steer")).await;

    let (first_frames, second_frames) = tokio::join!(
        collect_frames_until_agent_end(&mut first, first_frames, Duration::from_secs(30)),
        collect_frames_until_agent_end(&mut second, Vec::new(), Duration::from_secs(30)),
    );
    let first_frames = trim_at_agent_end(first_frames);
    let second_frames = trim_at_agent_end(second_frames);
    let (first_responses, first_events) = rpc_parts(&first_frames);
    let (second_responses, second_events) = rpc_parts(&second_frames);

    assert_eq!(first_responses.len(), 1, "first: {first_responses:?}");
    assert_eq!(second_responses.len(), 2, "second: {second_responses:?}");
    assert_eq!(
        first_responses[0]["payload"]["data"]["disposition"],
        "started"
    );
    for response in &second_responses {
        assert_eq!(
            response["payload"]["data"]["disposition"], "queued",
            "{response}"
        );
    }
    assert_eq!(
        queue_event_signature(&first_events),
        queue_event_signature(&second_events)
    );

    let tool_end_at = event_index(&first_events, |frame| {
        frame["payload"]["type"] == "tool_execution_end"
            && frame["payload"]["toolCallId"].as_str() == Some(tool_call_id.as_str())
    })
    .expect("tool_execution_end");

    // All mode: exactly one drain after the tool batch, and it removed BOTH
    // queued texts at once (the emitted snapshot is empty).
    let post_tool_queues: Vec<usize> = first_events
        .iter()
        .enumerate()
        .filter(|(index, frame)| *index > tool_end_at && frame["payload"]["type"] == "queue_update")
        .map(|(index, _)| index)
        .collect();
    assert_eq!(
        post_tool_queues.len(),
        1,
        "All mode must drain the whole queue in a single poll: {first_events:?}"
    );
    let drain = &first_events[post_tool_queues[0]]["payload"];
    assert_eq!(
        drain["steering"],
        serde_json::json!([]),
        "queue emptied by one drain: {drain}"
    );
    assert_eq!(drain["followUp"], serde_json::json!([]), "{drain}");

    let one_at =
        delivered_user_end(&first_events, "STEER-ALL-ONE").expect("STEER-ALL-ONE delivery");
    let two_at =
        delivered_user_end(&first_events, "STEER-ALL-TWO").expect("STEER-ALL-TWO delivery");
    assert!(
        tool_end_at < one_at && one_at < two_at,
        "FIFO after the tool batch"
    );
    assert_eq!(
        delivered_user_end_occurrences(&first_events, "STEER-ALL-ONE"),
        1
    );
    assert_eq!(
        delivered_user_end_occurrences(&first_events, "STEER-ALL-TWO"),
        1
    );

    let assistant_ends = assistant_end_positions(&first_events);
    assert_eq!(
        assistant_ends.len(),
        2,
        "tool turn + one combined steer turn: {first_events:?}"
    );
    assert!(
        two_at < assistant_ends[1],
        "both steers belong to the same post-tool request (two {two_at}, assistant {})",
        assistant_ends[1]
    );
    assert!(
        !first_events[one_at..two_at].iter().any(|frame| {
            frame["payload"]["type"] == "message_end"
                && frame["payload"]["message"]["role"] == "assistant"
        }),
        "no assistant turn may split the two All-mode deliveries"
    );
    assert_eq!(
        first_events
            .iter()
            .filter(|frame| frame["payload"]["type"] == "agent_end")
            .count(),
        1
    );

    let transcript = rpc_ok(
        &mut first,
        "all-steer-messages",
        serde_json::json!({"type":"get_messages"}),
    )
    .await;
    let rows = transcript_rows(&transcript);
    let tool_result_at = rows
        .iter()
        .position(|(role, _)| role == "toolResult")
        .expect("persisted toolResult");
    let one_row = rows
        .iter()
        .position(|(_, text)| text.contains("STEER-ALL-ONE"))
        .expect("persisted STEER-ALL-ONE");
    let two_row = rows
        .iter()
        .position(|(_, text)| text.contains("STEER-ALL-TWO"))
        .expect("persisted STEER-ALL-TWO");
    assert_eq!(rows[one_row].0, "user");
    assert_eq!(rows[two_row].0, "user");
    let final_assistant = rows
        .iter()
        .enumerate()
        .filter(|(index, (role, _))| *index > two_row && role == "assistant")
        .map(|(index, _)| index)
        .next()
        .expect("assistant after both steers");
    assert!(
        tool_result_at < one_row && one_row < two_row && two_row < final_assistant,
        "transcript order toolResult < ONE < TWO < assistant: {rows:?}"
    );

    if let Some(log_path) = tool_mock_all_log_path() {
        let requests = mock_requests_for(&log_path, "TOOLBATCH-ALL-A");
        assert_eq!(
            requests.len(),
            2,
            "All mode must fold both steers into one request: {requests:?}"
        );
        assert_eq!(requests[0]["script_step"], 0);
        assert!(!requests[0]["last_user"]
            .as_str()
            .unwrap_or("")
            .contains("STEER-ALL-"));
        assert_eq!(requests[1]["script_step"], 1);
        assert_eq!(
            requests[1]["messages"].as_u64(),
            Some(6),
            "system+user+tool-call+toolResult+two steers: {requests:?}"
        );
        let last_user = requests[1]["last_user"].as_str().unwrap_or("");
        assert!(
            last_user.contains("STEER-ALL-TWO"),
            "last user is the second steer: {requests:?}"
        );
    }

    eprintln!(
        "all-mode-steer: events={:?}",
        queue_event_signature(&first_events)
    );

    let stack_dir = stack.dir.clone();
    stack.server.abort();
    drop(stack);
    let _ = std::fs::remove_dir_all(&stack_dir);
}

/// Aborting a run that is executing a tool batch must stop it without ever
/// injecting the queued steering text (no phantom user message, no extra
/// request). Upstream `abort` does not clear the queues; `clear_queue` then
/// reports the dropped text and emits an emptied queue snapshot. Both the
/// pre-clear pending count and the post-clear zero count come from the live
/// session state (`get_state`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn phone_pool_abort_stops_run_and_clear_queue_drops_steer() {
    let Some(_model_dir) = tool_pool_test_model_dir() else {
        eprintln!("PIR_PHONE_TEST_TOOL not set; skipping abort/clear_queue coverage");
        return;
    };
    let (stack, mut first, mut second) = start_tool_pool_with_two_clients("pool-abort-clear").await;

    send_prompt_command(&mut first, "p-abort-a", "TOOLBATCH-ABORT-A start", None).await;
    let mut first_frames = Vec::new();
    let tool_call_id = wait_for_tool_execution_start(&mut first, &mut first_frames).await;
    send_prompt_command(
        &mut second,
        "p-abort-b",
        "ABORT-STEER-MARKER",
        Some("steer"),
    )
    .await;
    // Wait until the child really accepted the steering message.
    recv_relay_until(&mut second, |frame| {
        frame["channel"] == "rpc"
            && frame["payload"]["type"] == "queue_update"
            && frame["payload"]["steering"]
                .as_array()
                .is_some_and(|texts| {
                    texts
                        .iter()
                        .any(|text| text.as_str() == Some("ABORT-STEER-MARKER"))
                })
    })
    .await;

    first
        .send(WsMessage::Text(
            serde_json::json!({"kind":"rpc","command":{"type":"abort","id":"abort-1"}})
                .to_string()
                .into(),
        ))
        .await
        .expect("send abort");
    let abort_frames =
        collect_frames_until_agent_end(&mut first, first_frames, Duration::from_secs(20)).await;
    let abort_frames = trim_at_agent_end(abort_frames);
    let (responses, events) = rpc_parts(&abort_frames);
    let abort_response = responses
        .iter()
        .find(|response| response["payload"]["id"] == "abort-1")
        .unwrap_or_else(|| panic!("abort response missing: {responses:?}"));
    assert_eq!(abort_response["payload"]["success"], Value::Bool(true));
    // Upstream rpc `abort` answers with bare success (no data payload).
    assert!(
        abort_response["payload"]["data"].is_null(),
        "bare abort success: {abort_response}"
    );
    assert_eq!(
        events
            .iter()
            .filter(|frame| frame["payload"]["type"] == "agent_end")
            .count(),
        1,
        "aborted run still settles with one agent_end"
    );
    assert!(
        delivered_user_end(&events, "ABORT-STEER-MARKER").is_none(),
        "queued steer must never be injected into an aborted run: {events:?}"
    );
    let tool_started = event_index(&events, |frame| {
        frame["payload"]["type"] == "tool_execution_start"
            && frame["payload"]["toolCallId"].as_str() == Some(tool_call_id.as_str())
    });
    assert!(
        tool_started.is_some(),
        "the tool batch really started before abort"
    );
    // The aborted tool must not be followed by another provider turn.
    let assistant_after_tool_end = events
        .iter()
        .enumerate()
        .filter(|(_, frame)| {
            frame["payload"]["type"] == "message_end"
                && frame["payload"]["message"]["role"] == "assistant"
        })
        .count();
    assert_eq!(
        assistant_after_tool_end, 1,
        "only the tool-call assistant message exists before abort: {events:?}"
    );

    let state = rpc_ok(
        &mut first,
        "abort-state",
        serde_json::json!({"type":"get_state"}),
    )
    .await;
    assert_eq!(state["isStreaming"], Value::Bool(false));
    assert_eq!(
        state["pendingMessageCount"].as_u64(),
        Some(1),
        "upstream abort keeps the queue until clear_queue: {state}"
    );
    assert_eq!(state["steeringMode"], "one-at-a-time");

    first
        .send(WsMessage::Text(
            serde_json::json!({"kind":"rpc","command":{"type":"clear_queue","id":"clear-1"}})
                .to_string()
                .into(),
        ))
        .await
        .expect("send clear_queue");
    let mut clear_frames = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(6);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, first.next()).await {
            Ok(Some(Ok(WsMessage::Text(text)))) => {
                clear_frames.push(serde_json::from_str(&text).expect("relay frame JSON"));
            }
            Ok(Some(Ok(_))) => {}
            Ok(Some(Err(error))) => panic!("relay error while clearing: {error}"),
            Ok(None) => break,
            Err(_) => break,
        }
    }
    eprintln!("clear-queue frames: {clear_frames:?}");
    let (clear_responses, clear_events) = rpc_parts(&clear_frames);
    let clear_response = clear_responses
        .iter()
        .find(|response| response["payload"]["id"] == "clear-1")
        .expect("clear_queue response");
    assert_eq!(
        clear_response["payload"]["data"]["steering"],
        serde_json::json!(["ABORT-STEER-MARKER"])
    );
    assert_eq!(
        clear_response["payload"]["data"]["followUp"],
        serde_json::json!([])
    );
    assert!(
        clear_events.iter().any(|frame| {
            frame["payload"]["type"] == "queue_update"
                && frame["payload"]["steering"]
                    .as_array()
                    .is_some_and(|texts| texts.is_empty())
        }),
        "clear_queue must emit an emptied queue snapshot: {clear_events:?}"
    );

    let state = rpc_ok(
        &mut first,
        "clear-state",
        serde_json::json!({"type":"get_state"}),
    )
    .await;
    assert_eq!(state["pendingMessageCount"].as_u64(), Some(0), "{state}");

    let transcript = rpc_ok(
        &mut first,
        "abort-messages",
        serde_json::json!({"type":"get_messages"}),
    )
    .await;
    let rows = transcript_rows(&transcript);
    assert!(
        !rows
            .iter()
            .any(|(_, text)| text.contains("ABORT-STEER-MARKER")),
        "dropped steering text must not reach the session transcript: {rows:?}"
    );

    eprintln!("abort-clear: responses={responses:?} state={state}");

    let stack_dir = stack.dir.clone();
    stack.server.abort();
    drop(stack);
    let _ = std::fs::remove_dir_all(&stack_dir);
}

/// Parent `hasPendingUiRequest` is a real bridge state, not a hardcoded
/// false: a dialog-class `extension_ui_request` flips both the relay catalog
/// row and the status frame, and answering it clears the flag again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parent_pending_ui_request_reports_through_catalog_and_status() {
    use pir::plugins::plugin::PluginEvent;

    let stack = start_live_stack("pending-ui").await;
    let mut ws = connect_phone(stack.addr, &stack.user_token).await;
    let session_id = parent_session_id(&mut ws).await;
    // The relay only forwards `refresh` once a session is active; an
    // unselected client gets `server/error no-session` (the PWA selects from
    // the catalog first). Select here so every refresh below reaches the
    // native session host and republishes status/catalog.
    select_session(&mut ws, &session_id).await;

    ws.send(WsMessage::Text(
        serde_json::json!({"kind":"refresh"}).to_string().into(),
    ))
    .await
    .expect("refresh");
    let catalog = recv_relay_until(&mut ws, |frame| {
        frame["channel"] == "sessions" && frame["event"] == "catalog"
    })
    .await;
    assert_eq!(
        catalog["data"]["sessions"][0]["hasPendingUiRequest"],
        Value::Bool(false)
    );

    // Simulate the parent session's extension-UI request reaching the native
    // phone plugin (same event the AgentSession fan-out emits).
    let mut payload = serde_json::json!({
        "id": "ui-parent-1",
        "method": "confirm",
        "title": "Continue?",
    });
    stack
        .plugin
        .on_event(PluginEvent::new("extension_ui_request", &mut payload))
        .await;
    tokio::time::sleep(Duration::from_millis(150)).await;

    ws.send(WsMessage::Text(
        serde_json::json!({"kind":"refresh"}).to_string().into(),
    ))
    .await
    .expect("refresh after dialog");
    // `publish_phone_state_now` emits status before catalog, so read the
    // status first and the catalog after it.
    let status = recv_relay_until(&mut ws, |frame| {
        frame["channel"] == "server"
            && frame["event"] == "status"
            && frame["data"]["hasPendingUiRequest"] == Value::Bool(true)
    })
    .await;
    assert_eq!(status["data"]["hasPendingUiRequest"], Value::Bool(true));
    let catalog = recv_relay_until(&mut ws, |frame| {
        frame["channel"] == "sessions"
            && frame["event"] == "catalog"
            && frame["data"]["sessions"][0]["hasPendingUiRequest"] == Value::Bool(true)
    })
    .await;
    assert_eq!(
        catalog["data"]["sessions"][0]["hasPendingUiRequest"],
        Value::Bool(true)
    );

    // `id` is the dialog request id itself (the relay preserves it and
    // correlates through `requestId`), so the response echoes `ui-parent-1`.
    let answered = rpc_ok(
        &mut ws,
        "ui-parent-1",
        serde_json::json!({
            "type": "extension_ui_response",
            "confirmed": true,
        }),
    )
    .await;
    assert_eq!(answered["answered"], Value::Bool(true));

    ws.send(WsMessage::Text(
        serde_json::json!({"kind":"refresh"}).to_string().into(),
    ))
    .await
    .expect("refresh after answer");
    let catalog = recv_relay_until(&mut ws, |frame| {
        frame["channel"] == "sessions"
            && frame["event"] == "catalog"
            && frame["data"]["sessions"][0]["hasPendingUiRequest"] == Value::Bool(false)
    })
    .await;
    assert_eq!(
        catalog["data"]["sessions"][0]["hasPendingUiRequest"],
        Value::Bool(false)
    );

    let stack_dir = stack.dir.clone();
    stack.server.abort();
    drop(stack);
    let _ = std::fs::remove_dir_all(&stack_dir);
}

fn pool_dialog_model_dir() -> Option<String> {
    match std::env::var("PIR_PHONE_TEST_DIALOG") {
        Ok(value) if value.trim() == "1" => std::env::var("PIR_PHONE_TEST_DIALOG_DIR")
            .ok()
            .filter(|dir| !dir.trim().is_empty()),
        _ => None,
    }
}

fn catalog_row_for_local_id<'a>(catalog: &'a Value, local_id: &str) -> Option<&'a Value> {
    catalog["data"]["sessions"].as_array()?.iter().find(|row| {
        row["id"]
            .as_str()
            .is_some_and(|id| id.rsplit(':').next() == Some(local_id))
    })
}

/// A pool child's real `ctx.ui.confirm` reaches the phone as an
/// `extension_ui_request`; answering it with the untouched dialog id must
/// resolve the child RPC dialog call and clear both the bridge pending flag and
/// the pool summary's `hasPendingUiRequest` (catalog row true -> false).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn phone_pool_extension_ui_dialog_round_trip_clears_pending_state() {
    let Some(model_dir) = pool_dialog_model_dir() else {
        eprintln!("PIR_PHONE_TEST_DIALOG not set; skipping pool dialog coverage");
        return;
    };
    install_pool_test_env(&model_dir);
    let stack = start_live_stack("pool-dialog").await;
    let mut ws = connect_phone(stack.addr, &stack.user_token).await;
    let parent_id = parent_session_id(&mut ws).await;
    select_session(&mut ws, &parent_id).await;
    ws.send(WsMessage::Text(
        serde_json::json!({ "kind": "session-spawn" })
            .to_string()
            .into(),
    ))
    .await
    .expect("send session-spawn");
    let catalog = wait_parallel_catalog(&mut ws, 1).await;
    let child_row = catalog["data"]["sessions"]
        .as_array()
        .expect("catalog rows")
        .iter()
        .find(|row| row["kind"] == "parallel")
        .and_then(|row| row["id"].as_str())
        .expect("parallel row id")
        .to_owned();
    let child_local = child_row.rsplit(':').next().expect("local id").to_owned();
    select_session(&mut ws, &child_row).await;

    prompt(&mut ws, "pd-prompt", "POOL-DIALOG-CONFIRM").await;
    let dialog = recv_relay_until(&mut ws, |frame| {
        frame["channel"] == "rpc" && frame["payload"]["type"] == "extension_ui_request"
    })
    .await;
    let dialog_id = dialog["payload"]["id"]
        .as_str()
        .expect("dialog request id")
        .to_owned();
    assert_eq!(dialog["payload"]["method"], "confirm");
    assert_eq!(
        dialog["sessionWorkerId"].as_str(),
        Some(child_local.as_str())
    );
    assert!(
        stack.plugin.has_pending_ui_request(&child_local),
        "bridge must track the child dialog as pending"
    );
    let pool = stack.plugin.pool().expect("pool");
    assert!(
        pool.list()
            .iter()
            .find(|summary| summary.id == child_local)
            .is_some_and(|summary| summary.has_pending_ui_request),
        "pool summary must report the pending dialog"
    );

    // Publishing a catalog while the dialog is open (a second spawn forces the
    // host catalog push) must show the child row as needing input.
    ws.send(WsMessage::Text(
        serde_json::json!({ "kind": "session-spawn" })
            .to_string()
            .into(),
    ))
    .await
    .expect("send second session-spawn");
    let catalog = recv_relay_until(&mut ws, |frame| {
        frame["channel"] == "sessions"
            && frame["event"] == "catalog"
            && frame["data"]["sessions"]
                .as_array()
                .map(|rows| rows.iter().filter(|row| row["kind"] == "parallel").count())
                == Some(2)
            && catalog_row_for_local_id(frame, &child_local)
                .is_some_and(|row| row["hasPendingUiRequest"] == Value::Bool(true))
    })
    .await;
    let row = catalog_row_for_local_id(&catalog, &child_local).expect("first child row");
    assert_eq!(row["hasPendingUiRequest"], Value::Bool(true), "{catalog}");

    // Answer using the dialog id itself: the relay preserves it and correlates
    // through `requestId`, so the child RPC and the bridge both match it.
    let answered = rpc_ok(
        &mut ws,
        &dialog_id,
        serde_json::json!({"type": "extension_ui_response", "confirmed": true}),
    )
    .await;
    assert_eq!(answered["answered"], Value::Bool(true));
    assert!(
        !stack.plugin.has_pending_ui_request(&child_local),
        "answering the dialog must clear the bridge pending flag"
    );
    assert!(
        !pool
            .list()
            .iter()
            .find(|summary| summary.id == child_local)
            .is_some_and(|summary| summary.has_pending_ui_request),
        "answering the dialog must clear the pool summary flag"
    );

    let ends = assistant_ends_routed_to(&mut ws, &child_local).await;
    let text: String = ends
        .iter()
        .flat_map(|message| message["content"].as_array().into_iter().flatten())
        .filter_map(|block| block["text"].as_str())
        .collect();
    assert!(text.contains("POOL-DIALOG-DONE"), "child reply: {text}");

    // A fresh catalog push after the answer must show false.
    ws.send(WsMessage::Text(
        serde_json::json!({ "kind": "session-spawn" })
            .to_string()
            .into(),
    ))
    .await
    .expect("send third session-spawn");
    let catalog = recv_relay_until(&mut ws, |frame| {
        frame["channel"] == "sessions"
            && frame["event"] == "catalog"
            && frame["data"]["sessions"]
                .as_array()
                .map(|rows| rows.iter().filter(|row| row["kind"] == "parallel").count())
                == Some(3)
            && catalog_row_for_local_id(frame, &child_local)
                .is_some_and(|row| row["hasPendingUiRequest"] == Value::Bool(false))
    })
    .await;
    let row = catalog_row_for_local_id(&catalog, &child_local).expect("first child row");
    assert_eq!(row["hasPendingUiRequest"], Value::Bool(false), "{catalog}");

    let stack_dir = stack.dir.clone();
    stack.server.abort();
    drop(stack);
    let _ = std::fs::remove_dir_all(&stack_dir);
}
