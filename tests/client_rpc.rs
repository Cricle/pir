//! Coverage for the JSON-line RPC client library
//! (`pir::client::rpc`, upstream `modes/rpc/rpc-client.ts`).
//!
//! Two transports are exercised:
//! - in-process (`RpcClient::with_streams` over a duplex with a real
//!   `RpcServer`): command/response correlation, typed helpers, listener +
//!   broadcast events, server-error mapping, close semantics;
//! - the real `pir rpc` binary (`RpcClient::spawn`): hello/create, a prompt
//!   run streamed to `agent_settled` (`prompt_and_wait`), the full
//!   `get_messages` view, and process reaping on `close`.
//!
//! The spawned test points the model catalog at the shared instant-401
//! sandbox endpoint (`common::sandbox_endpoint`), so the run settles fast and
//! deterministically.

mod common;

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::json;

use pir::agent::tool_execution::ToolRegistry;
use pir::agent_session::{AgentSession, AgentSessionConfig};
use pir::ai::types::*;
use pir::client::{RpcClient, RpcClientError, RpcClientOptions};
use pir::coding_agent::rpc::{RpcServer, SessionFactory};
use pir::session_manager::SessionManager;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-client-rpc-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&path).expect("create temp dir");
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
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

/// Session factory with no provider traffic needed: `create`/`get_state`/
/// `get_commands`/queue commands never call the model.
fn idle_factory() -> SessionFactory {
    Arc::new(|session: Option<SessionManager>| {
        Ok(Arc::new(AgentSession::new(AgentSessionConfig {
            provider_registry: Arc::new(pir::ai::provider::ProviderRegistry::default()),
            model: fake_model(),
            api_key: None,
            thinking: ThinkingLevel::Off,
            tools: ToolRegistry::new(),
            session,
            compaction: None,
            max_turns: 2,
            system_prompt: None,
            cwd: None,
            event_stream: None,
            extensions: None,
            tool_selection: None,
        })))
    })
}

// ---------------------------------------------------------------------------
// In-process transport
// ---------------------------------------------------------------------------

#[tokio::test]
async fn jsonl_client_round_trips_commands_events_and_close() {
    let temp = TempDir::new("in-process");
    let server = RpcServer::new(temp.0.clone(), idle_factory());
    let (client_end, server_end) = tokio::io::duplex(64 * 1024);
    let (server_read, server_write) = tokio::io::split(server_end);
    tokio::spawn(async move {
        let _ = server.serve(server_read, server_write).await;
    });
    let (client_read, client_write) = tokio::io::split(client_end);
    let client = RpcClient::with_streams(client_read, client_write);

    // hello / create / get_state through the typed helpers.
    let hello = client.hello().await.expect("hello");
    assert_eq!(hello["protocol"], json!("pir-rpc-jsonl"), "{hello}");
    let created = client.create(None).await.expect("create");
    assert!(created["id"].is_string(), "{created}");
    let state = client.get_state().await.expect("get_state");
    assert!(state["sessionId"].is_string(), "{state}");
    let commands = client.get_commands().await.expect("get_commands");
    assert!(commands["commands"].is_array(), "{commands}");

    // A server-side rejection maps to RpcClientError::Server (upstream
    // getData throwing the response's error).
    let error = client
        .get_messages_view("definitely-not-a-view")
        .await
        .expect_err("invalid view is a command error");
    match error {
        RpcClientError::Server { command, message } => {
            assert_eq!(command, "get_messages", "{message}");
            assert!(message.contains("view"), "{message}");
        }
        other => panic!("expected a server error, got {other:?}"),
    }

    // Listener events (sync callbacks on the reader task) + the broadcast
    // stream both see the queue_update emitted by `steer`.
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let listener_seen = Arc::clone(&seen);
    let _subscription = client.on_event(move |value| {
        listener_seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(value["type"].as_str().unwrap_or_default().to_owned());
    });
    let mut events = client.events();
    let disposition = client.steer("queued text").await.expect("steer");
    assert_eq!(disposition, "handled");
    let event = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let value = events.recv().await.expect("event");
            if value["type"] == "queue_update" {
                break value;
            }
        }
    })
    .await
    .expect("queue_update must reach the broadcast receiver");
    assert_eq!(event["type"], json!("queue_update"));
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if seen
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
                .any(|kind| kind == "queue_update")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("queue_update must reach the on_event listener");

    // wait_for_event uses its own subscription and matches on the predicate.
    // The wait future is polled first (tokio::join!), so it subscribes before
    // the steer can emit its queue_update — the same "subscribe before the
    // triggering command" ordering upstream `waitForIdle` requires.
    let waited = client.wait_for_event(
        |value| value["type"] == "queue_update",
        Duration::from_secs(5),
    );
    let steered = client.steer("second queued text");
    let (matched, disposition) = tokio::join!(waited, steered);
    assert_eq!(disposition.expect("steer 2"), "handled");
    let matched = matched.expect("wait_for_event");
    assert_eq!(matched["type"], json!("queue_update"));

    // abort answers bare success.
    client.abort().await.expect("abort");

    // close ends the transport; further sends fail with Closed.
    client.close().await.expect("close");
    assert!(client.is_closed());
    match client.send(json!({ "type": "hello" })).await {
        Err(RpcClientError::Closed) => {}
        other => panic!("send after close must be Closed, got {other:?}"),
    }
    // close is idempotent.
    client.close().await.expect("second close");
}

// ---------------------------------------------------------------------------
// Spawned binary
// ---------------------------------------------------------------------------

fn sandbox_models_json() -> String {
    let base_url = common::sandbox_endpoint();
    format!(
        r#"{{"providers":{{"bench":{{"name":"rpc client test","baseUrl":"{base_url}","apiKey":"x","api":"openai-completions","models":[{{"id":"bench-model","name":"bench","api":"openai-completions","baseUrl":"{base_url}","reasoning":false,"contextWindow":4096,"maxTokens":256,"input":["text"]}}]}}}}}}"#
    )
}

#[tokio::test]
async fn jsonl_client_spawns_pir_rpc_and_waits_for_settled() {
    let home = TempDir::new("spawn");
    let agent_dir = home.0.join("agent");
    std::fs::create_dir_all(&agent_dir).expect("create agent dir");
    std::fs::write(agent_dir.join("models.json"), sandbox_models_json())
        .expect("write models.json");

    let mut options = RpcClientOptions::new(env!("CARGO_BIN_EXE_pir"));
    options.cwd = Some(home.0.clone());
    options.env = vec![
        ("HOME".to_owned(), home.0.to_string_lossy().into_owned()),
        (
            "USERPROFILE".to_owned(),
            home.0.to_string_lossy().into_owned(),
        ),
        (
            "PIR_AGENT_DIR".to_owned(),
            agent_dir.to_string_lossy().into_owned(),
        ),
    ];
    let client = RpcClient::spawn(options).await.expect("spawn pir rpc");

    let hello = client.hello().await.expect("hello");
    assert_eq!(hello["protocol"], json!("pir-rpc-jsonl"), "{hello}");

    let created = client.create(None).await.expect("create");
    assert!(created["id"].is_string(), "{created}");

    // The prompt run fails fast at the instant-401 sandbox endpoint, but it
    // still streams through `agent_settled` — the client's wait path.
    let events = client
        .prompt_and_wait("hello agent", None, Duration::from_secs(30))
        .await
        .expect("prompt_and_wait");
    assert!(!events.is_empty(), "the run must stream events");
    assert_eq!(
        events.last().map(|event| event["type"].clone()),
        Some(json!("agent_settled")),
        "the collected stream must end at agent_settled: {events:?}"
    );

    // The user message was persisted before the failed provider call, so the
    // full view carries it.
    let messages = client.get_messages().await.expect("get_messages");
    let list = messages["messages"]
        .as_array()
        .unwrap_or_else(|| panic!("{messages}"));
    assert!(
        list.iter().any(|message| message["role"] == "user"),
        "{messages}"
    );

    client.close().await.expect("close");
    assert!(client.is_closed());
    // The child is reaped: a second close and a later send both stay closed.
    client.close().await.expect("idempotent close");
    assert!(matches!(
        client.send(json!({ "type": "hello" })).await,
        Err(RpcClientError::Closed)
    ));
}

/// A response line below the protocol cap is accepted unchanged (the cap must
/// not truncate healthy large payloads such as a full `get_messages` reply).
#[tokio::test]
async fn jsonl_client_accepts_a_long_line_below_the_cap() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    use pir::client::rpc::MAX_PROTOCOL_LINE_BYTES;

    let (client_end, server_end) = tokio::io::duplex(64 * 1024);
    let (server_read, mut server_write) = tokio::io::split(server_end);
    let payload_len = MAX_PROTOCOL_LINE_BYTES - 2048;
    let server = tokio::spawn(async move {
        let mut reader = tokio::io::BufReader::new(server_read);
        let mut line = String::new();
        reader.read_line(&mut line).await.expect("read hello");
        let request: serde_json::Value = serde_json::from_str(line.trim()).expect("hello json");
        let id = request["id"].as_str().expect("request id").to_owned();
        let response = json!({
            "type": "response",
            "id": id,
            "command": "hello",
            "success": true,
            "data": { "protocol": "pir-rpc-jsonl", "blob": "x".repeat(payload_len) },
        });
        server_write
            .write_all(format!("{response}\n").as_bytes())
            .await
            .expect("write long response");
        server_write.flush().await.expect("flush");
    });

    let (client_read, client_write) = tokio::io::split(client_end);
    let client = RpcClient::with_streams(client_read, client_write);
    let hello = client.hello().await.expect("long line must be accepted");
    assert_eq!(hello["protocol"], json!("pir-rpc-jsonl"));
    assert_eq!(hello["blob"].as_str().map(str::len), Some(payload_len));
    client.close().await.expect("close");
    server.await.expect("server join");
}

/// An over-limit stdout line is a typed transport failure: the pending request
/// receives `LineTooLong` (naming the cap), the client closes, and the peer
/// sees EOF on its read side.
#[tokio::test]
async fn jsonl_client_rejects_and_closes_an_over_limit_line() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    use pir::client::rpc::MAX_PROTOCOL_LINE_BYTES;

    let (client_end, server_end) = tokio::io::duplex(64 * 1024);
    let (server_read, mut server_write) = tokio::io::split(server_end);
    let (closed_tx, closed_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let mut reader = tokio::io::BufReader::new(server_read);
        let mut line = String::new();
        reader.read_line(&mut line).await.expect("read hello");
        // Deliberately unterminated: the reader must stop at the cap instead of
        // buffering the whole line.
        let oversized = "x".repeat(MAX_PROTOCOL_LINE_BYTES + 4096);
        let _ = server_write.write_all(oversized.as_bytes()).await;
        let _ = server_write.flush().await;
        let mut rest = String::new();
        let closed = match reader.read_line(&mut rest).await {
            Ok(0) | Err(_) => true,
            Ok(_) => false,
        };
        let _ = closed_tx.send(closed);
    });

    let (client_read, client_write) = tokio::io::split(client_end);
    let client = RpcClient::with_streams(client_read, client_write);
    let error = client
        .hello()
        .await
        .expect_err("the over-limit line must fail the pending request");
    match &error {
        RpcClientError::LineTooLong { limit } => {
            assert_eq!(*limit, MAX_PROTOCOL_LINE_BYTES);
        }
        other => panic!("expected LineTooLong, got {other:?}"),
    }
    assert!(
        error
            .to_string()
            .contains(&MAX_PROTOCOL_LINE_BYTES.to_string()),
        "the error must name the limit: {error}"
    );
    assert!(client.is_closed(), "the transport must close on overflow");
    assert!(matches!(client.hello().await, Err(RpcClientError::Closed)));
    let closed = tokio::time::timeout(Duration::from_secs(5), closed_rx)
        .await
        .expect("the fixture must observe the close")
        .expect("fixture task");
    assert!(closed, "the client must close its write side on overflow");
    server.await.expect("server join");
}

/// The existing stderr capture bound still holds: a child that writes more
/// than `STDERR_CAPTURE_LIMIT` is truncated (the stdout line cap must not have
/// replaced that path).
#[tokio::test]
async fn jsonl_client_keeps_the_stderr_capture_cap() {
    use pir::client::rpc::STDERR_CAPTURE_LIMIT;

    let temp = TempDir::new("stderr-cap");
    let payload_path = temp.0.join("stderr.txt");
    let mut payload = String::with_capacity(2 * 1024 * 1024);
    for _ in 0..2048 {
        payload.push_str(&"e".repeat(1023));
        payload.push('\n');
    }
    std::fs::write(&payload_path, &payload).expect("write stderr payload");

    #[cfg(windows)]
    let script = {
        let path = temp.0.join("emit-stderr.bat");
        std::fs::write(
            &path,
            format!("@echo off\r\ntype \"{}\" 1>&2\r\n", payload_path.display()),
        )
        .expect("write stderr fixture");
        path
    };
    #[cfg(unix)]
    let script = {
        use std::os::unix::fs::PermissionsExt;

        let path = temp.0.join("emit-stderr.sh");
        std::fs::write(
            &path,
            format!("#!/bin/sh\ncat '{}' 1>&2\n", payload_path.display()),
        )
        .expect("write stderr fixture");
        let mut permissions = std::fs::metadata(&path)
            .expect("stat fixture")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&path, permissions).expect("chmod fixture");
        path
    };

    let client = RpcClient::spawn(RpcClientOptions::new(&script))
        .await
        .expect("spawn stderr fixture");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let captured = client.stderr();
        if captured.len() >= STDERR_CAPTURE_LIMIT {
            assert_eq!(captured.len(), STDERR_CAPTURE_LIMIT);
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "stderr capture stalled at {} bytes",
            captured.len()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    client.close().await.expect("close");
}

/// The request counter is monotonic and every response is correlated by id
/// even when several commands are in flight (the client sends them
/// concurrently).
#[tokio::test]
async fn jsonl_client_correlates_concurrent_requests() {
    let temp = TempDir::new("concurrent");
    let server = RpcServer::new(temp.0.clone(), idle_factory());
    let (client_end, server_end) = tokio::io::duplex(64 * 1024);
    let (server_read, server_write) = tokio::io::split(server_end);
    tokio::spawn(async move {
        let _ = server.serve(server_read, server_write).await;
    });
    let (client_read, client_write) = tokio::io::split(client_end);
    let client = RpcClient::with_streams(client_read, client_write);

    let calls: Arc<AtomicUsize> = Arc::new(AtomicUsize::new(0));
    let mut handles = Vec::new();
    for _ in 0..8 {
        let client = client.clone();
        let calls = Arc::clone(&calls);
        handles.push(tokio::spawn(async move {
            let hello = client.hello().await.expect("hello");
            assert_eq!(hello["protocol"], json!("pir-rpc-jsonl"), "{hello}");
            calls.fetch_add(1, Ordering::SeqCst);
        }));
    }
    for handle in handles {
        handle.await.expect("join");
    }
    assert_eq!(calls.load(Ordering::SeqCst), 8);
    client.close().await.expect("close");
}

/// The command bodies match upstream `rpc-client.ts`: steering/follow-up
/// images, `get_available_thinking_levels`, `excludeFromContext`, and the
/// `stop()` spelling of teardown.
#[tokio::test]
async fn jsonl_client_sends_upstream_command_bodies() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    let (client_end, server_end) = tokio::io::duplex(64 * 1024);
    let (server_read, mut server_write) = tokio::io::split(server_end);
    let seen = Arc::new(Mutex::new(Vec::<(String, serde_json::Value)>::new()));
    let seen_task = Arc::clone(&seen);
    let server = tokio::spawn(async move {
        let mut reader = tokio::io::BufReader::new(server_read);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            let Ok(value) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
                continue;
            };
            let id = value.get("id").cloned().unwrap_or(json!("0"));
            let command = value
                .get("type")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .to_owned();
            seen_task
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((command.clone(), value));
            let data = match command.as_str() {
                "steer" | "follow_up" => json!({ "disposition": "handled" }),
                "get_available_thinking_levels" => json!({ "levels": ["off", "low"] }),
                "bash" => json!({ "exitCode": 0 }),
                _ => json!({}),
            };
            let response = json!({
                "type": "response",
                "id": id,
                "command": command,
                "success": true,
                "data": data,
            });
            if server_write
                .write_all(format!("{response}\n").as_bytes())
                .await
                .is_err()
            {
                break;
            }
            let _ = server_write.flush().await;
        }
    });

    let (client_read, client_write) = tokio::io::split(client_end);
    let client = RpcClient::with_streams(client_read, client_write);
    let images = vec![json!({ "data": "aGk=", "mimeType": "image/png" })];

    assert_eq!(
        client.steer_with("look", &images).await.expect("steer"),
        "handled"
    );
    assert_eq!(
        client
            .follow_up_with("and this", &images)
            .await
            .expect("follow_up"),
        "handled"
    );
    let levels = client
        .get_available_thinking_levels()
        .await
        .expect("thinking levels");
    assert_eq!(levels["levels"], json!(["off", "low"]));
    client
        .bash_with_options("echo hi", true)
        .await
        .expect("bash");

    // The upstream teardown spelling.
    client.stop().await.expect("stop");
    assert!(client.is_closed());
    server.await.expect("fake server join");

    let seen = seen
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    assert_eq!(seen.len(), 4, "{seen:?}");
    assert_eq!(seen[0].1["type"], json!("steer"));
    assert_eq!(seen[0].1["images"], json!(images));
    assert_eq!(seen[1].1["type"], json!("follow_up"));
    assert_eq!(seen[1].1["images"], json!(images));
    assert_eq!(seen[2].1["type"], json!("get_available_thinking_levels"));
    assert_eq!(seen[3].1["type"], json!("bash"));
    assert_eq!(seen[3].1["excludeFromContext"], json!(true));
}
