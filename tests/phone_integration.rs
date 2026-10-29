//! End-to-end tests for the phone server <-> session bridge wiring.
//!
//! These exercise the real HTTP/WS listener started by [`PhonePlugin`] and a
//! raw RFC 6455 client (no browser, no model, `127.0.0.1` only, ephemeral
//! port). A recording [`HostCommands`] adapter proves the host seam:
//!
//! * boot `status`/`catalog` frames travel through the bridge;
//! * `refresh` is forwarded to the host and its snapshot broadcast reaches the
//!   client through the bridge's per-client queue;
//! * a `rpc prompt` reaches the recording host and its response returns to the
//!   originating client;
//! * `PhonePlugin::on_event` payloads are mapped by `SessionBridge` and
//!   delivered as phone envelopes;
//! * stop closes the port and the bridge.
#![cfg(feature = "phone")]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use pir::plugins::builtin::phone::session::{
    ForwardedCommand, HostCommands, HostFuture, HostReply, PhoneCommand, SessionBridge,
};
use pir::plugins::builtin::phone::{PhonePlugin, SubscribeError, PHONE_HOST_EVENT_NAMES};
use pir::plugins::{AgentPlugin, PluginEvent, PluginEventOutcome};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const IO_TIMEOUT: Duration = Duration::from_secs(5);
const READ_TIMEOUT: Duration = Duration::from_secs(3);

// ---------------------------------------------------------------------------
// Raw WebSocket client
// ---------------------------------------------------------------------------

struct WsClient {
    stream: TcpStream,
    buffer: Vec<u8>,
}

impl WsClient {
    async fn connect(port: u16) -> Self {
        let stream = tokio::time::timeout(IO_TIMEOUT, TcpStream::connect(("127.0.0.1", port)))
            .await
            .expect("connect timed out")
            .expect("connect refused");
        let mut client = Self {
            stream,
            buffer: Vec::new(),
        };
        let request = format!(
            "GET /ws HTTP/1.1\r\n\
             Host: 127.0.0.1:{port}\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
             Sec-WebSocket-Version: 13\r\n\r\n"
        );
        client.write(request.as_bytes()).await;
        let head = client.read_through(b"\r\n\r\n").await;
        let head = String::from_utf8_lossy(&head);
        assert!(
            head.starts_with("HTTP/1.1 101 Switching Protocols"),
            "handshake failed: {head}"
        );
        client
    }

    async fn write(&mut self, bytes: &[u8]) {
        tokio::time::timeout(IO_TIMEOUT, self.stream.write_all(bytes))
            .await
            .expect("write timed out")
            .expect("write failed");
        tokio::time::timeout(IO_TIMEOUT, self.stream.flush())
            .await
            .expect("flush timed out")
            .expect("flush failed");
    }

    async fn fill(&mut self) {
        let mut chunk = [0u8; 4096];
        let read = tokio::time::timeout(IO_TIMEOUT, self.stream.read(&mut chunk))
            .await
            .expect("read timed out")
            .expect("read failed");
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

    async fn read_exact(&mut self, count: usize) -> Vec<u8> {
        while self.buffer.len() < count {
            self.fill().await;
        }
        let out = self.buffer[..count].to_vec();
        self.buffer.drain(..count);
        out
    }

    async fn send_frame(&mut self, opcode: u8, payload: &[u8]) {
        let mask = [0x37u8, 0xfa, 0x21, 0x3d];
        let mut frame = Vec::with_capacity(14 + payload.len());
        frame.push(0x80 | opcode);
        let length = payload.len();
        if length < 126 {
            frame.push(0x80 | length as u8);
        } else if length <= u16::MAX as usize {
            frame.push(0x80 | 126);
            frame.extend_from_slice(&(length as u16).to_be_bytes());
        } else {
            frame.push(0x80 | 127);
            frame.extend_from_slice(&(length as u64).to_be_bytes());
        }
        frame.extend_from_slice(&mask);
        for (index, byte) in payload.iter().enumerate() {
            frame.push(byte ^ mask[index % 4]);
        }
        self.write(&frame).await;
    }

    async fn send_json(&mut self, value: &Value) {
        self.send_frame(0x1, value.to_string().as_bytes()).await;
    }

    /// Read one server frame with a deadline.
    async fn read_frame(&mut self) -> (u8, Vec<u8>) {
        let read = async {
            let header = self.read_exact(2).await;
            let opcode = header[0] & 0x0f;
            let masked = header[1] & 0x80 != 0;
            assert!(!masked, "server frames must not be masked");
            let mut length = u64::from(header[1] & 0x7f);
            if length == 126 {
                let extended = self.read_exact(2).await;
                length = u64::from(u16::from_be_bytes([extended[0], extended[1]]));
            } else if length == 127 {
                let extended = self.read_exact(8).await;
                let mut bytes = [0u8; 8];
                bytes.copy_from_slice(&extended);
                length = u64::from_be_bytes(bytes);
            }
            let payload = self.read_exact(length as usize).await;
            (opcode, payload)
        };
        tokio::time::timeout(READ_TIMEOUT, read)
            .await
            .expect("timed out waiting for a server frame")
    }

    async fn read_json(&mut self) -> Value {
        let (opcode, payload) = self.read_frame().await;
        assert_eq!(opcode, 0x1, "expected a text frame");
        serde_json::from_slice(&payload).expect("server frame is not JSON")
    }
}

// ---------------------------------------------------------------------------
// Recording host
// ---------------------------------------------------------------------------

#[derive(Default)]
struct RecordingHost {
    seen: Mutex<Vec<ForwardedCommand>>,
}

impl RecordingHost {
    fn commands(&self) -> Vec<PhoneCommand> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .map(|entry| entry.command.clone())
            .collect()
    }
}

impl HostCommands for RecordingHost {
    fn dispatch(&self, command: ForwardedCommand) -> HostFuture {
        self.seen.lock().unwrap().push(command.clone());
        let reply = match &command.command {
            // A resync request: answer with a broadcast snapshot, which the
            // bridge maps and fans out to the subscriber.
            PhoneCommand::Refresh => HostReply {
                data: None,
                broadcast: vec![json!({
                    "type": "snapshot",
                    "state": {
                        "sessionId": "parent-session-1",
                        "sessionName": "Integration",
                        "model": { "id": "mock", "name": "Mock", "provider": "mock" },
                        "thinkingLevel": "off",
                        "isStreaming": false,
                        "isCompacting": false,
                    },
                    "messages": [{ "role": "user", "content": "hello from history" }],
                    "liveAssistantMessage": Value::Null,
                    "liveTools": [],
                })],
            },
            // Lifecycle commands are acknowledged with catalog/snapshot
            // frames in the reference; no direct response.
            PhoneCommand::SessionSpawn { .. }
            | PhoneCommand::SessionSelect { .. }
            | PhoneCommand::SessionParentNew
            | PhoneCommand::LoadEarlierMessages { .. } => HostReply::default(),
            _ => HostReply::data(json!({ "ok": true })),
        };
        Box::pin(async move { Ok(reply) })
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn start_with_host() -> (PhonePlugin, Arc<RecordingHost>, u16) {
    let plugin = PhonePlugin::new();
    let host = Arc::new(RecordingHost::default());
    plugin.set_host_commands(host.clone());
    let port = plugin.start_for_test("0 -").await.expect("ephemeral start");
    assert!(plugin.is_running());
    (plugin, host, port)
}

async fn assert_port_closed(port: u16) {
    let mut closed = false;
    for _ in 0..40 {
        match TcpStream::connect(("127.0.0.1", port)).await {
            Ok(_) => tokio::time::sleep(Duration::from_millis(50)).await,
            Err(_) => {
                closed = true;
                break;
            }
        }
    }
    assert!(closed, "port {port} still accepts connections after stop");
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bridge_drives_refresh_prompt_and_host_events() {
    let (plugin, host, port) = start_with_host().await;
    let bridge: SessionBridge = plugin.bridge_for_test().expect("running bridge");
    let mut client = WsClient::connect(port).await;

    // Boot frame: the server status travels through the bridge's mapping.
    let status = client.read_json().await;
    assert_eq!(status["channel"], "server");
    assert_eq!(status["event"], "status");
    assert_eq!(status["data"]["isRunning"], true);

    // With a live host installed the placeholder catalog is not fabricated;
    // the refresh round-trip proves the bridge is the envelope source.
    client.send_json(&json!({ "kind": "refresh" })).await;
    let snapshot = client.read_json().await;
    assert_eq!(snapshot["channel"], "snapshot");
    assert_eq!(snapshot["sessionWorkerId"], "parent-session");
    assert_eq!(
        snapshot["state"]["sessionId"],
        Value::from("parent-session-1")
    );
    assert_eq!(snapshot["messages"][0]["content"], "hello from history");
    assert!(
        host.commands()
            .iter()
            .any(|command| matches!(command, PhoneCommand::Refresh)),
        "refresh was not forwarded to the host"
    );

    // A prompt from the phone reaches the recording host and its response is
    // routed only to the originating client.
    client
        .send_json(&json!({
            "kind": "rpc",
            "command": {
                "type": "prompt",
                "message": "hello from the phone",
                "images": [],
                "streamingBehavior": "steer",
            },
        }))
        .await;
    let response = client.read_json().await;
    assert_eq!(response["channel"], "rpc");
    assert_eq!(response["payload"]["type"], "response");
    assert_eq!(response["payload"]["command"], "prompt");
    assert_eq!(response["payload"]["success"], true);
    assert_eq!(response["payload"]["data"]["ok"], true);

    let commands = host.commands();
    assert!(matches!(
        commands.last(),
        Some(PhoneCommand::Prompt {
            message,
            streaming_behavior: Some(behavior),
            ..
        }) if message == "hello from the phone" && behavior == "steer"
    ));

    // Host events reach the bridge through the plugin's event feed and are
    // mapped by session.rs (no mapping duplicated here).
    let mut payload = json!({ "type": "agent_start" });
    assert!(matches!(
        plugin
            .on_event(PluginEvent::new("agent_start", &mut payload))
            .await,
        PluginEventOutcome::None
    ));
    let agent_start = client.read_json().await;
    assert_eq!(agent_start["channel"], "rpc");
    assert_eq!(agent_start["payload"]["type"], "agent_start");

    // `status` carries no type in its payload; on_event supplies it.
    let mut payload = json!({ "data": { "isStreaming": true } });
    plugin
        .on_event(PluginEvent::new("status", &mut payload))
        .await;
    let status = client.read_json().await;
    assert_eq!(status["channel"], "server");
    assert_eq!(status["event"], "status");
    assert_eq!(status["data"]["isStreaming"], true);

    // Stop closes the listener and the bridge.
    assert!(plugin.stop_for_test().await);
    assert!(!plugin.is_running());
    assert_port_closed(port).await;
    assert!(matches!(bridge.subscribe(), Err(SubscribeError::Closed)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn on_event_covers_every_bridge_mapped_event() {
    let (plugin, _host, port) = start_with_host().await;
    let mut client = WsClient::connect(port).await;
    assert_eq!(client.read_json().await["event"], "status");

    let interest = plugin.events();
    for name in PHONE_HOST_EVENT_NAMES {
        assert!(interest.matches(name), "missing event interest for {name}");
    }

    // (event name, payload, expected channel, expected rpc payload type)
    let cases: Vec<(&str, Value, &str, Option<&str>)> = vec![
        (
            "agent_start",
            json!({ "type": "agent_start" }),
            "rpc",
            Some("agent_start"),
        ),
        (
            "agent_end",
            json!({ "type": "agent_end", "messages": [] }),
            "rpc",
            Some("agent_end"),
        ),
        (
            "message_start",
            json!({ "type": "message_start", "message": { "role": "assistant", "content": [] } }),
            "rpc",
            Some("message_start"),
        ),
        (
            "message_update",
            json!({
                "type": "message_update",
                "message": { "role": "assistant", "timestamp": 1 },
                "assistantMessageEvent": { "type": "text_delta", "delta": "hi" },
            }),
            "rpc",
            Some("message_update"),
        ),
        (
            "message_end",
            json!({ "type": "message_end", "message": { "role": "assistant", "content": [] } }),
            "rpc",
            Some("message_end"),
        ),
        (
            "tool_execution_start",
            json!({
                "type": "tool_execution_start",
                "toolCallId": "tool-1",
                "toolName": "read",
                "args": {},
            }),
            "rpc",
            Some("tool_execution_start"),
        ),
        (
            "tool_execution_update",
            json!({
                "type": "tool_execution_update",
                "toolCallId": "tool-1",
                "toolName": "read",
                "partialResult": { "content": [{ "type": "text", "text": "partial" }] },
            }),
            "rpc",
            Some("tool_execution_update"),
        ),
        (
            "tool_execution_end",
            json!({
                "type": "tool_execution_end",
                "toolCallId": "tool-1",
                "toolName": "read",
                "result": { "content": [{ "type": "text", "text": "done" }] },
                "isError": false,
            }),
            "rpc",
            Some("tool_execution_end"),
        ),
        (
            "queue_update",
            json!({ "type": "queue_update", "steering": [], "followUp": [] }),
            "rpc",
            Some("queue_update"),
        ),
        (
            "compaction_start",
            json!({ "type": "compaction_start", "reason": "manual" }),
            "rpc",
            Some("compaction_start"),
        ),
        (
            "compaction_end",
            json!({
                "type": "compaction_end",
                "reason": "manual",
                "result": Value::Null,
                "aborted": false,
                "willRetry": false,
            }),
            "rpc",
            Some("compaction_end"),
        ),
        (
            "auto_retry_start",
            json!({ "type": "auto_retry_start", "attempt": 1 }),
            "rpc",
            Some("auto_retry_start"),
        ),
        (
            "auto_retry_end",
            json!({ "type": "auto_retry_end", "attempt": 1, "success": true }),
            "rpc",
            Some("auto_retry_end"),
        ),
        (
            "extension_ui_request",
            json!({ "type": "extension_ui_request", "method": "confirm", "id": "ui-1" }),
            "rpc",
            Some("extension_ui_request"),
        ),
        (
            "status",
            json!({ "data": { "isStreaming": false } }),
            "server",
            None,
        ),
    ];

    for (name, mut payload, channel, payload_type) in cases {
        let outcome = plugin.on_event(PluginEvent::new(name, &mut payload)).await;
        assert!(
            matches!(outcome, PluginEventOutcome::None),
            "{name}: unexpected outcome"
        );
        let envelope = client.read_json().await;
        assert_eq!(envelope["channel"], channel, "{name}: channel");
        if let Some(expected) = payload_type {
            assert_eq!(envelope["payload"]["type"], expected, "{name}: type");
        } else {
            assert_eq!(envelope["event"], name, "{name}: server event");
        }
    }

    assert!(plugin.stop_for_test().await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forwards_every_documented_phone_command() {
    let (plugin, host, port) = start_with_host().await;
    let mut client = WsClient::connect(port).await;
    assert_eq!(client.read_json().await["event"], "status");

    let commands = [
        json!({ "kind": "rpc", "command": { "type": "prompt", "message": "hi", "images": [] } }),
        json!({ "kind": "rpc", "command": { "type": "abort" } }),
        json!({ "kind": "rpc", "command": { "type": "set_model", "provider": "mock", "modelId": "mock-1" } }),
        json!({ "kind": "rpc", "command": { "type": "set_thinking_level", "level": "high" } }),
        json!({ "kind": "rpc", "command": { "type": "compact", "customInstructions": "tight" } }),
        json!({ "kind": "local-command", "command": "reload" }),
        json!({ "kind": "rpc", "command": { "type": "get_session_stats" } }),
        json!({ "kind": "rpc", "command": { "type": "phone_get_tree" } }),
        json!({ "kind": "rpc", "command": { "type": "fork", "entryId": "entry-1" } }),
        json!({ "kind": "rpc", "command": { "type": "phone_list_sessions", "includeSubagents": true } }),
        json!({ "kind": "session-spawn", "sessionFile": "sessions/mock.jsonl" }),
    ];

    for command in &commands {
        client.send_json(command).await;
    }

    // Every command except `session-spawn` produces a response envelope.
    let expected = [
        ("prompt", "response"),
        ("abort", "response"),
        ("set_model", "response"),
        ("set_thinking_level", "response"),
        ("compact", "response"),
        ("reload", "response"),
        ("get_session_stats", "response"),
        ("phone_get_tree", "response"),
        ("fork", "response"),
        ("phone_list_sessions", "response"),
    ];
    for (command_name, payload_type) in expected {
        let response = client.read_json().await;
        assert_eq!(response["channel"], "rpc", "{command_name}: channel");
        assert_eq!(
            response["payload"]["type"], payload_type,
            "{command_name}: payload type"
        );
        assert_eq!(
            response["payload"]["command"], command_name,
            "{command_name}: command name"
        );
        assert_eq!(response["payload"]["success"], true);
    }

    // `session-spawn` produces no response, so the tenth response can race
    // with its delivery to the host; wait for the recording to catch up.
    for _ in 0..200 {
        if host.commands().len() >= commands.len() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let seen = host.commands();
    assert_eq!(
        seen.len(),
        commands.len(),
        "not every command was forwarded"
    );
    assert!(matches!(&seen[0], PhoneCommand::Prompt { .. }));
    assert_eq!(seen[1], PhoneCommand::Abort);
    assert!(matches!(
        &seen[2],
        PhoneCommand::SetModel { provider, model_id }
            if provider == "mock" && model_id == "mock-1"
    ));
    assert!(matches!(
        &seen[3],
        PhoneCommand::SetThinkingLevel { level } if level == "high"
    ));
    assert!(matches!(
        &seen[4],
        PhoneCommand::Compact { custom_instructions: Some(text) } if text == "tight"
    ));
    assert_eq!(seen[5], PhoneCommand::Reload);
    assert_eq!(seen[6], PhoneCommand::GetSessionStats);
    assert_eq!(seen[7], PhoneCommand::PhoneGetTree);
    assert!(matches!(
        &seen[8],
        PhoneCommand::Fork { entry_id: Some(entry) } if entry == "entry-1"
    ));
    assert!(matches!(
        &seen[9],
        PhoneCommand::PhoneListSessions {
            include_subagents: true
        }
    ));
    assert!(matches!(
        &seen[10],
        PhoneCommand::SessionSpawn { session_file: Some(path) }
            if path == "sessions/mock.jsonl"
    ));

    assert!(plugin.stop_for_test().await);
}
