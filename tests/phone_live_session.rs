//! End-to-end test for the native phone host installed over a real
//! [`AgentSession`] (adversarial review finding 1 + 3).
//!
//! The session is driven by a scripted stream (no provider, no keys); the real
//! `builtin:phone` plugin is attached and the session is built through
//! `AgentSession::new_arc_with_plugins`, which is the host path that binds the
//! `HostCommands` adapter. Commands travel through the real
//! `SessionBridge` parse/dispatch path (no WebSocket socket is needed: the
//! bridge is the transport seam), and the published status/snapshot/catalog
//! are asserted to be live session data instead of the standalone placeholders.

#![cfg(feature = "phone")]

use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{oneshot, Mutex};

use pir::agent::tool_execution::ToolRegistry;
use pir::agent_session::{AgentSession, AgentSessionConfig, SharedEventStreamFn};
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::ProviderRegistry;
use pir::ai::types::*;
use pir::plugins::builtin::phone::session::BridgeSubscriber;
use pir::plugins::builtin::phone::PhonePlugin;
use pir::plugins::AgentPlugin;

const WAIT: Duration = Duration::from_secs(5);

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

/// A stream override that blocks until the test releases it, then finishes
/// with one scripted assistant message. The gate makes "the run is streaming"
/// deterministic (the run-start status is asserted before the reply lands).
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

async fn recv_json(client: &mut BridgeSubscriber) -> Value {
    let line = tokio::time::timeout(WAIT, client.recv())
        .await
        .expect("bridge frame timed out")
        .expect("bridge closed");
    serde_json::from_str(&line).expect("bridge frame is JSON")
}

/// Read frames until the response envelope for `command` arrives.
async fn recv_response(client: &mut BridgeSubscriber, command: &str) -> Value {
    for _ in 0..40 {
        let frame = recv_json(client).await;
        if frame["channel"] == "rpc"
            && frame["payload"]["type"] == "response"
            && frame["payload"]["command"] == command
        {
            return frame;
        }
    }
    panic!("never received the `{command}` response envelope");
}

fn session_config(release: Arc<Mutex<Option<oneshot::Receiver<()>>>>) -> AgentSessionConfig {
    AgentSessionConfig {
        provider_registry: Arc::new(ProviderRegistry::default()),
        model: fake_model(),
        api_key: None,
        thinking: ThinkingLevel::Low,
        tools: ToolRegistry::new(),
        session: None,
        compaction: None,
        max_turns: 4,
        system_prompt: Some("phone live-session test".into()),
        cwd: std::env::current_dir().ok(),
        event_stream: Some(gated_stream(release)),
        extensions: None,
        tool_selection: None,
    }
}

// ---------------------------------------------------------------------------
// Minimal HTTP + WebSocket client (the server surface under test)
// ---------------------------------------------------------------------------

async fn http_json(port: u16, path: &str) -> Value {
    let mut stream = tokio::time::timeout(WAIT, TcpStream::connect(("127.0.0.1", port)))
        .await
        .expect("connect timed out")
        .expect("connect refused");
    let request =
        format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .await
        .expect("request write");
    let mut bytes = Vec::new();
    tokio::time::timeout(WAIT, stream.read_to_end(&mut bytes))
        .await
        .expect("response read timed out")
        .expect("response read");
    let text = String::from_utf8_lossy(&bytes);
    let (head, body) = text.split_once("\r\n\r\n").expect("HTTP head terminator");
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    serde_json::from_str(body).expect("JSON response body")
}

/// A buffered WebSocket test client: sends masked text frames and reads the
/// server's unmasked frames (the boot path is a real `/ws` upgrade).
struct WsClient {
    stream: TcpStream,
    buffer: Vec<u8>,
}

impl WsClient {
    async fn connect(port: u16, path: &str) -> Self {
        let mut stream = tokio::time::timeout(WAIT, TcpStream::connect(("127.0.0.1", port)))
            .await
            .expect("connect timed out")
            .expect("connect refused");
        let request = format!(
            "GET {path} HTTP/1.1\r\n\
             Host: 127.0.0.1:{port}\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
             Sec-WebSocket-Version: 13\r\n\r\n"
        );
        stream
            .write_all(request.as_bytes())
            .await
            .expect("handshake write");
        stream.flush().await.expect("handshake flush");
        let mut client = Self {
            stream,
            buffer: Vec::new(),
        };
        let head = client.read_through(b"\r\n\r\n").await;
        assert!(
            String::from_utf8_lossy(&head).starts_with("HTTP/1.1 101"),
            "handshake failed: {}",
            String::from_utf8_lossy(&head)
        );
        client
    }

    async fn fill(&mut self) {
        let mut chunk = [0u8; 4096];
        let read = tokio::time::timeout(WAIT, self.stream.read(&mut chunk))
            .await
            .expect("frame read timed out")
            .expect("frame read");
        assert!(read > 0, "unexpected EOF");
        self.buffer.extend_from_slice(&chunk[..read]);
    }

    async fn read_through(&mut self, needle: &[u8]) -> Vec<u8> {
        loop {
            if let Some(position) = self
                .buffer
                .windows(needle.len())
                .position(|window| window == needle)
            {
                let end = position + needle.len();
                let out = self.buffer[..end].to_vec();
                self.buffer.drain(..end);
                return out;
            }
            self.fill().await;
        }
    }

    async fn read_exact_n(&mut self, count: usize) -> Vec<u8> {
        while self.buffer.len() < count {
            self.fill().await;
        }
        let out = self.buffer[..count].to_vec();
        self.buffer.drain(..count);
        out
    }

    async fn read_json(&mut self) -> Value {
        let header = self.read_exact_n(2).await;
        assert_eq!(header[0] & 0x0f, 0x1, "expected a text frame");
        assert_eq!(header[1] & 0x80, 0, "server frames must not be masked");
        let mut length = usize::from(header[1] & 0x7f);
        if length == 126 {
            let extended = self.read_exact_n(2).await;
            length = usize::from(u16::from_be_bytes([extended[0], extended[1]]));
        } else if length == 127 {
            let extended = self.read_exact_n(8).await;
            let mut bytes = [0u8; 8];
            bytes.copy_from_slice(&extended);
            length = usize::try_from(u64::from_be_bytes(bytes)).expect("frame length");
        }
        let payload = self.read_exact_n(length).await;
        serde_json::from_slice(&payload).expect("server frame is JSON")
    }

    async fn send_json(&mut self, json: &str) {
        let mask = [0x37u8, 0xfa, 0x21, 0x3d];
        let payload = json.as_bytes();
        let mut frame = Vec::with_capacity(14 + payload.len());
        frame.push(0x81);
        if payload.len() < 126 {
            frame.push(0x80 | payload.len() as u8);
        } else {
            frame.push(0x80 | 126);
            frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        }
        frame.extend_from_slice(&mask);
        for (index, byte) in payload.iter().enumerate() {
            frame.push(byte ^ mask[index % 4]);
        }
        self.stream.write_all(&frame).await.expect("frame write");
        self.stream.flush().await.expect("frame flush");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_phone_host_mirrors_the_real_session() {
    let (release_tx, release_rx) = oneshot::channel();
    let release = Arc::new(Mutex::new(Some(release_rx)));

    let plugin = Arc::new(PhonePlugin::new());
    let session = AgentSession::new_arc_with_plugins(
        session_config(release),
        vec![plugin.clone() as Arc<dyn AgentPlugin>],
    );

    // --- installation seam ------------------------------------------------
    assert!(
        plugin.has_host_commands(),
        "new_with_plugins must install the live HostCommands adapter"
    );
    assert!(
        plugin.has_live_session(),
        "the installed adapter must be bound to the session Arc"
    );
    assert!(
        session.phone_host_bound(),
        "the session must report a bound phone host"
    );

    let port = plugin.start_for_test("0 -").await.expect("ephemeral start");
    assert!(port > 0);
    let bridge = plugin.bridge_for_test().expect("running bridge");
    let mut client = bridge.subscribe().expect("subscribe");

    // --- a phone `rpc prompt` reaches the session --------------------------
    let prompt_line =
        r#"{"kind":"rpc","command":{"type":"prompt","message":"hello phone","id":"p1"}}"#;
    assert!(bridge.handle_client_line(client.id(), prompt_line).await);
    let response = recv_response(&mut client, "prompt").await;
    assert_eq!(response["payload"]["success"], true);
    assert_eq!(response["payload"]["data"]["disposition"], "started");

    wait_until(|| session.is_streaming(), "the prompt run to start").await;
    wait_until(
        || {
            plugin
                .published_status()
                .is_some_and(|status| status["isStreaming"] == true)
        },
        "the run-start status",
    )
    .await;

    // The run-start status is real session state, and streaming reports the
    // CLI as the owner (the minimal ownership rule).
    let status = plugin
        .published_status()
        .expect("status published at the run boundary");
    assert_eq!(status["model"]["id"], "fake-model");
    assert_eq!(status["provider"], "fake");
    assert_eq!(status["isStreaming"], true);
    assert_eq!(status["controlOwner"], "cli");
    assert_eq!(status["sessionCount"], 1);
    assert!(status["cwd"].as_str().is_some_and(|cwd| !cwd.is_empty()));
    assert!(status["pendingMessages"].is_array());

    // Release the scripted reply and let the run settle.
    let _ = release_tx.send(());
    wait_until(|| !session.is_streaming(), "the run to settle").await;

    let transcript = session.transcript_json().await;
    assert!(
        transcript.iter().any(|message| {
            message["role"] == "assistant" && message.to_string().contains("scripted reply")
        }),
        "the scripted assistant reply must be produced by the real session"
    );

    // --- the settled snapshot is live data, not the placeholder ----------
    wait_until(
        || {
            plugin
                .published_status()
                .is_some_and(|status| status["isStreaming"] == false)
        },
        "the settled status",
    )
    .await;
    let status = plugin.published_status().expect("settled status");
    assert_eq!(status["isStreaming"], false);
    assert_eq!(status["controlOwner"], "phone");
    assert_eq!(status["thinkingLevel"], "low");

    let snapshot = plugin.published_snapshot().expect("settled snapshot");
    assert_eq!(snapshot["state"]["model"]["id"], "fake-model");
    assert_eq!(snapshot["state"]["isStreaming"], false);
    assert!(
        snapshot["state"]["usageTotals"]["input"]
            .as_u64()
            .unwrap_or(0)
            >= 7,
        "usage totals come from the real run: {}",
        snapshot["state"]["usageTotals"]
    );
    let messages = snapshot["messages"]
        .as_array()
        .expect("snapshot messages array");
    assert!(messages
        .iter()
        .any(|message| message.to_string().contains("hello phone")));
    assert!(messages
        .iter()
        .any(|message| message.to_string().contains("scripted reply")));
    let commands = snapshot["commands"]
        .as_array()
        .expect("snapshot command catalog");
    assert!(
        !commands.is_empty(),
        "the command catalog is the live plugin registry, not the four-command fallback"
    );
    assert!(commands
        .iter()
        .any(|command| command["name"] == "phone-start"));

    let catalog = plugin.published_catalog().expect("catalog");
    assert_eq!(catalog["activeSessionId"], "parent-session");
    assert_eq!(catalog["sessions"][0]["kind"], "parent");
    assert_eq!(catalog["sessions"][0]["isStreaming"], false);

    // --- model/thinking commands reach the session and republish ---------
    let thinking_line =
        r#"{"kind":"rpc","command":{"type":"set_thinking_level","level":"high","id":"p2"}}"#;
    assert!(bridge.handle_client_line(client.id(), thinking_line).await);
    let response = recv_response(&mut client, "set_thinking_level").await;
    assert_eq!(response["payload"]["success"], true);
    assert_eq!(session.thinking_level(), ThinkingLevel::High);
    wait_until(
        || {
            plugin
                .published_status()
                .is_some_and(|status| status["thinkingLevel"] == "high")
        },
        "the thinking level to be republished",
    )
    .await;

    // `/api/health` needs exactly this host half; server-owned fields stay
    // server-owned and are merged by the server lane.
    let health = plugin.published_health_json();
    assert_eq!(health["model"]["id"], "fake-model");
    assert_eq!(health["sessionCount"], 1);
    assert_eq!(health["isStreaming"], false);
    assert!(health.get("pid").is_none());
    assert!(health.get("hasToken").is_none());

    // --- honest errors stay honest ---------------------------------------
    let reload_line = r#"{"kind":"local-command","command":"reload","id":"p3"}"#;
    assert!(bridge.handle_client_line(client.id(), reload_line).await);
    let response = recv_response(&mut client, "reload").await;
    assert_eq!(response["payload"]["success"], false);
    assert!(response["payload"]["error"]
        .as_str()
        .is_some_and(|error| error.contains("no session reload seam")));

    assert!(plugin.stop_for_test().await);
}

/// The TUI construction path: `Arc::new(AgentSession::new_with_plugins(..))`
/// followed by `install_phone_host()` (RPC uses `new_arc`, the same binding).
///
/// A session built that way must serve LIVE state through the real server:
/// `/api/health` and the boot `status` frame carry the host half merged over
/// the server-owned fields (finding 3), and a new `/ws` subscription receives
/// the real catalog and snapshot immediately instead of having to `refresh`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tui_style_arc_binding_serves_live_health_and_boot_status() {
    let (_release_tx, release_rx) = oneshot::channel();
    let release = Arc::new(Mutex::new(Some(release_rx)));
    let plugin = Arc::new(PhonePlugin::new());

    // TUI path (terminal.rs): build, wrap, then install immediately.
    let session = Arc::new(AgentSession::new_with_plugins(
        session_config(release),
        vec![plugin.clone() as Arc<dyn AgentPlugin>],
    ));
    session.install_phone_host();

    assert!(plugin.has_host_commands());
    assert!(plugin.has_live_session(), "install_phone_host must bind");
    assert!(session.phone_host_bound());

    let port = plugin.start_for_test("0 -").await.expect("ephemeral start");

    // The install-time publication is spawned on the runtime; wait for it so
    // the health merge and the boot replay have live data.
    wait_until(
        || plugin.published_status().is_some() && plugin.published_snapshot().is_some(),
        "the install-time phone publication",
    )
    .await;

    // --- `/api/health` reports the live host half -------------------------
    let health = http_json(port, "/api/health").await;
    assert_eq!(health["isRunning"], Value::Bool(true));
    assert_eq!(health["model"]["id"], "fake-model");
    assert_eq!(health["provider"], "fake");
    assert_eq!(health["isStreaming"], Value::Bool(false));
    assert_eq!(health["controlOwner"], Value::from("cli"));
    assert_eq!(health["sessionCount"], Value::from(1));
    // Server-owned fields stay the server's.
    assert_eq!(health["pid"], Value::from(std::process::id()));
    assert_eq!(health["port"], Value::from(port));
    assert_eq!(health["host"], Value::from("127.0.0.1"));
    assert_eq!(health["hasToken"], Value::Bool(false));
    assert_eq!(health["idleTimeoutMs"], Value::from(7_200_000u64));

    // --- boot frames replay the real status/catalog/snapshot -------------
    let mut ws = WsClient::connect(port, "/ws").await;

    let status = ws.read_json().await;
    assert_eq!(status["channel"], Value::from("server"));
    assert_eq!(status["event"], Value::from("status"));
    assert_eq!(status["data"]["model"]["id"], "fake-model");
    assert_eq!(status["data"]["isStreaming"], Value::Bool(false));
    assert_eq!(status["data"]["controlOwner"], Value::from("cli"));
    assert_eq!(status["data"]["isRunning"], Value::Bool(true));
    assert_eq!(status["data"]["port"], Value::from(port));
    let published = plugin.published_status().expect("published status");
    for key in ["model", "isStreaming", "controlOwner", "sessionCount"] {
        assert_eq!(
            status["data"][key], published[key],
            "boot status `{key}` must carry the published host status"
        );
    }

    let catalog = ws.read_json().await;
    assert_eq!(catalog["channel"], Value::from("sessions"));
    assert_eq!(catalog["event"], Value::from("catalog"));
    assert_eq!(catalog["data"]["activeSessionId"], "parent-session");

    let snapshot = ws.read_json().await;
    assert_eq!(snapshot["channel"], Value::from("snapshot"));
    assert_eq!(snapshot["state"]["model"]["id"], "fake-model");

    // --- a phone command over the real transport reaches the session -----
    ws.send_json(r#"{"kind":"rpc","command":{"type":"get_state"}}"#)
        .await;
    let mut response = None;
    for _ in 0..20 {
        let frame = ws.read_json().await;
        if frame["channel"] == "rpc" && frame["payload"]["command"] == "get_state" {
            response = Some(frame);
            break;
        }
    }
    let response = response.expect("get_state response envelope");
    assert_eq!(response["payload"]["success"], Value::Bool(true));
    assert_eq!(response["payload"]["data"]["model"]["id"], "fake-model");
    assert_eq!(response["payload"]["data"]["thinkingLevel"], "low");

    assert!(plugin.stop_for_test().await);
}
