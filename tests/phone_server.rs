//! Hermetic native pi-phone server tests (`features = "phone"`).
//!
//! Everything runs against `127.0.0.1` only: an ephemeral port is requested
//! through the bare `0` argument, so the suite never fights over 8787 and never
//! touches the network. Covers the first server slice:
//!
//! - the four command registrations (names, descriptions, Rust backends);
//! - start/status/token/stop idempotence and honest failures (invalid port,
//!   port in use);
//! - `/api/health`, static assets (mount rewrite + ETag/304), the unknown-API
//!   404, `/api/quota` (non-Codex short-circuit), `/api/image/*`, and the
//!   control-stop 403/405 behavior;
//! - the RFC 6455 upgrade, token gate (1008), text/binary echo, and the close
//!   handshake;
//! - stop closes the listener and resets the status line.
#![cfg(feature = "phone")]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use pir::extensions::CommandOutcome;
use pir::plugins::builtin::phone::server::{MAX_CONNECTIONS, MAX_WS_FRAME_BYTES};
use pir::plugins::builtin::phone::{
    PhonePlugin, COMMAND_NAMES, PHONE_START_DESCRIPTION, PHONE_STATUS_DESCRIPTION,
    PHONE_STOP_DESCRIPTION, PHONE_TOKEN_DESCRIPTION, STATUS_KEY,
};
use pir::plugins::{
    AgentPlugin, CommandBackend, CommandHandler, PluginRegistrations, RegistrationContext,
};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Serializes the tests: each one starts a full server (with the hardened
/// connection cap), and running sixteen of them in parallel makes the
/// loopback clients contend for the accept backlog.
static TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const IO_TIMEOUT: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------
// Tiny buffered test client
// ---------------------------------------------------------------------------

struct Conn {
    stream: TcpStream,
    buffer: Vec<u8>,
}

impl Conn {
    async fn connect(port: u16) -> Self {
        let stream = tokio::time::timeout(IO_TIMEOUT, TcpStream::connect(("127.0.0.1", port)))
            .await
            .expect("connect timed out")
            .expect("connect refused");
        Self {
            stream,
            buffer: Vec::new(),
        }
    }

    async fn send(&mut self, bytes: &[u8]) {
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

    /// Read through (and including) `needle`; returns the consumed bytes.
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
}

struct Response {
    status: u16,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

async fn send_raw_request(port: u16, request: &str, head_only: bool) -> Response {
    let mut conn = Conn::connect(port).await;
    conn.send(request.as_bytes()).await;

    let head = conn.read_through(b"\r\n\r\n").await;
    let text = String::from_utf8_lossy(&head);
    let mut lines = text.split("\r\n");
    let status_line = lines.next().expect("status line");
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .expect("status code")
        .parse()
        .expect("numeric status");
    let mut headers = HashMap::new();
    for line in lines {
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_owned());
        }
    }

    let length = headers
        .get("content-length")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    let body = if head_only || status == 101 || status == 304 || length == 0 {
        Vec::new()
    } else {
        conn.read_exact(length).await
    };

    Response {
        status,
        headers,
        body,
    }
}

async fn http_request(
    port: u16,
    method: &str,
    path: &str,
    extra_headers: &[(&str, &str)],
) -> Response {
    let mut request =
        format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n");
    for (name, value) in extra_headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("\r\n");
    send_raw_request(port, &request, method == "HEAD").await
}

async fn json_body(response: &Response) -> Value {
    serde_json::from_slice(&response.body).expect("JSON response body")
}

// ---------------------------------------------------------------------------
// WebSocket helpers
// ---------------------------------------------------------------------------

fn masked_frame(opcode: u8, payload: &[u8]) -> Vec<u8> {
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
    frame
}

async fn read_server_frame(conn: &mut Conn) -> (bool, u8, Vec<u8>) {
    let header = conn.read_exact(2).await;
    let fin = header[0] & 0x80 != 0;
    let opcode = header[0] & 0x0f;
    let masked = header[1] & 0x80 != 0;
    assert!(!masked, "server frames must not be masked");
    let mut length = u64::from(header[1] & 0x7f);
    if length == 126 {
        let extended = conn.read_exact(2).await;
        length = u64::from(u16::from_be_bytes([extended[0], extended[1]]));
    } else if length == 127 {
        let extended = conn.read_exact(8).await;
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(&extended);
        length = u64::from_be_bytes(bytes);
    }
    let payload = conn.read_exact(length as usize).await;
    (fin, opcode, payload)
}

async fn ws_handshake(port: u16, path: &str) -> (Conn, String) {
    let mut conn = Conn::connect(port).await;
    let request = format!(
        "GET {path} HTTP/1.1\r\n\
         Host: 127.0.0.1:{port}\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
         Sec-WebSocket-Version: 13\r\n\r\n"
    );
    conn.send(request.as_bytes()).await;
    let head = conn.read_through(b"\r\n\r\n").await;
    (conn, String::from_utf8_lossy(&head).into_owned())
}

fn header_value<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        if key.trim().eq_ignore_ascii_case(name) {
            Some(value.trim())
        } else {
            None
        }
    })
}

fn command_handler(registrations: &PluginRegistrations, name: &str) -> Option<CommandHandler> {
    registrations
        .commands
        .iter()
        .find(|(def, _)| def.name == name)
        .and_then(|(_, backend)| match backend {
            Some(CommandBackend::Rust(handler)) => Some(Arc::clone(handler)),
            _ => None,
        })
}

async fn run(handler: &CommandHandler, args: &str) -> CommandOutcome {
    handler(args).await
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn command_registration_matches_the_reference() {
    let _serial = TEST_LOCK.lock().await;
    let plugin = PhonePlugin::new();
    let mut registrations = PluginRegistrations::default();
    let mut context = RegistrationContext::new(&mut registrations);
    plugin.register(&mut context).expect("registration");

    let names: Vec<&str> = registrations
        .commands
        .iter()
        .map(|(def, _)| def.name.as_str())
        .collect();
    assert_eq!(
        names,
        COMMAND_NAMES.to_vec(),
        "exactly the four pi-phone commands, in reference order"
    );

    let descriptions: Vec<Option<&str>> = registrations
        .commands
        .iter()
        .map(|(def, _)| def.description.as_deref())
        .collect();
    assert_eq!(
        descriptions,
        vec![
            Some(PHONE_START_DESCRIPTION),
            Some(PHONE_STOP_DESCRIPTION),
            Some(PHONE_STATUS_DESCRIPTION),
            Some(PHONE_TOKEN_DESCRIPTION),
        ]
    );
    for (_, backend) in &registrations.commands {
        assert!(
            matches!(backend, Some(CommandBackend::Rust(_))),
            "every command has a native handler"
        );
    }

    assert_eq!(plugin.info().name, "phone");
    assert_eq!(plugin.info().path, "builtin:phone");
    assert_eq!(
        plugin.status_entry(),
        (STATUS_KEY, "📱 ○ phone off".to_owned())
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_surface_and_lifecycle() {
    let _serial = TEST_LOCK.lock().await;
    let plugin = PhonePlugin::new();
    let mut registrations = PluginRegistrations::default();
    let mut context = RegistrationContext::new(&mut registrations);
    plugin.register(&mut context).expect("registration");

    let port = plugin.start_for_test("0 -").await.expect("ephemeral start");
    assert!(port > 0);
    assert!(plugin.is_running());

    // --- health ---------------------------------------------------------
    let health = http_request(port, "GET", "/api/health", &[]).await;
    assert_eq!(health.status, 200);
    assert_eq!(
        health.headers.get("content-type").map(String::as_str),
        Some("application/json; charset=utf-8")
    );
    let health_json = json_body(&health).await;
    assert_eq!(health_json["isRunning"], Value::Bool(true));
    assert_eq!(health_json["port"], Value::from(port));
    assert_eq!(health_json["host"], Value::from("127.0.0.1"));
    assert_eq!(health_json["hasToken"], Value::Bool(false));
    assert_eq!(health_json["pid"], Value::from(std::process::id()));
    assert_eq!(health_json["sessionCount"], Value::from(0));
    assert_eq!(health_json["controlOwner"], Value::from("cli"));

    // Health is method-agnostic in the reference server.
    let posted = http_request(port, "POST", "/api/health", &[]).await;
    assert_eq!(posted.status, 200);

    // Mount-prefixed health works too.
    let mounted = http_request(port, "GET", &format!("/phone-{port}/api/health"), &[]).await;
    assert_eq!(mounted.status, 200);

    // --- static client --------------------------------------------------
    let index = http_request(port, "GET", "/", &[]).await;
    assert_eq!(index.status, 200);
    assert_eq!(
        index.headers.get("content-type").map(String::as_str),
        Some("text/html; charset=utf-8")
    );
    assert_eq!(
        index.headers.get("cache-control").map(String::as_str),
        Some("no-cache")
    );
    let index_text = String::from_utf8_lossy(&index.body);
    assert!(index_text.contains("<title>Pi Phone</title>"));
    assert!(
        index_text.contains(&format!("window.PI_PHONE_BASE = \"/phone-{port}\";")),
        "index.html is rewritten to the per-port mount"
    );

    let app_js = http_request(port, "GET", "/app.js", &[]).await;
    assert_eq!(app_js.status, 200);
    assert_eq!(
        app_js.headers.get("content-type").map(String::as_str),
        Some("text/javascript; charset=utf-8")
    );
    assert!(String::from_utf8_lossy(&app_js.body).contains("./app/main.js"));

    // ETag / If-None-Match -> 304.
    let etag = index
        .headers
        .get("etag")
        .expect("index.html has an ETag")
        .clone();
    let revalidated = http_request(port, "GET", "/", &[("If-None-Match", etag.as_str())]).await;
    assert_eq!(revalidated.status, 304);
    assert!(revalidated.body.is_empty());

    // Missing static file falls back to index.html (reference behavior).
    let missing = http_request(port, "GET", "/definitely-missing.js", &[]).await;
    assert_eq!(missing.status, 200);
    assert_eq!(
        missing.headers.get("cache-control").map(String::as_str),
        Some("no-store")
    );

    // Unknown API path is an honest 404.
    let unknown_api = http_request(port, "GET", "/api/not-a-route", &[]).await;
    assert_eq!(unknown_api.status, 404);
    assert_eq!(
        json_body(&unknown_api).await["error"],
        Value::from("Not found")
    );

    // Traversal is rejected.
    let traversal = http_request(port, "GET", "/../Cargo.toml", &[]).await;
    assert_eq!(traversal.status, 403);

    // --- quota ----------------------------------------------------------
    let quota = http_request(port, "GET", "/api/quota?provider=mock&modelId=mock", &[]).await;
    assert_eq!(quota.status, 200);
    let quota_json = json_body(&quota).await;
    assert_eq!(quota_json["visible"], Value::Bool(false));
    assert_eq!(quota_json["primaryWindow"], Value::Null);

    let quota_post = http_request(port, "POST", "/api/quota", &[]).await;
    assert_eq!(quota_post.status, 405);

    // --- images ---------------------------------------------------------
    let key = plugin
        .store_image_for_test("aGVsbG8=", "text/plain")
        .expect("image key");
    let image = http_request(port, "GET", &format!("/api/image/{key}"), &[]).await;
    assert_eq!(image.status, 200);
    // `text/plain` is not an image type: it is served as an opaque download.
    assert_eq!(
        image.headers.get("content-type").map(String::as_str),
        Some("application/octet-stream")
    );
    assert_eq!(
        image.headers.get("cache-control").map(String::as_str),
        Some("public, max-age=31536000, immutable")
    );
    assert_eq!(image.body, b"hello");

    let invalid_key = http_request(port, "GET", "/api/image/bad!", &[]).await;
    assert_eq!(invalid_key.status, 400);
    let missing_image = http_request(port, "GET", "/api/image/AAAAAAAAAAAAAAAAAAAAAA", &[]).await;
    assert_eq!(missing_image.status, 404);

    // --- control stop ---------------------------------------------------
    let control = http_request(
        port,
        "POST",
        "/__pi_phone__/control/stop",
        &[("Authorization", "Bearer anything")],
    )
    .await;
    assert_eq!(control.status, 403);
    assert!(json_body(&control).await["error"]
        .as_str()
        .unwrap_or_default()
        .contains("Forbidden"));
    let control_get = http_request(port, "GET", "/__pi_phone__/control/stop", &[]).await;
    assert_eq!(control_get.status, 405);

    // Non-GET/HEAD on another route is 405.
    let post_root = http_request(port, "POST", "/", &[]).await;
    assert_eq!(post_root.status, 405);

    // --- command outcome text ------------------------------------------
    let status_handler = command_handler(&registrations, "phone-status").expect("status handler");
    let status_outcome = run(&status_handler, "").await;
    assert!(status_outcome.ok);
    let status_text = status_outcome.error.expect("status text");
    assert!(
        status_text.contains(&format!("http://127.0.0.1:{port}")),
        "{status_text}"
    );

    let token_handler = command_handler(&registrations, "phone-token").expect("token handler");
    let token_outcome = run(&token_handler, "").await;
    assert!(token_outcome.ok);
    assert_eq!(
        token_outcome.error.as_deref(),
        Some("Pi Phone token is disabled for this server.")
    );
    assert_eq!(
        plugin.status_entry(),
        (STATUS_KEY, "📱 ● phone on".to_owned())
    );

    // Idempotent start: same config while running returns the same port.
    let same_port = plugin
        .start_for_test(&format!("{port} -"))
        .await
        .expect("idempotent start");
    assert_eq!(same_port, port);

    // --- stop -----------------------------------------------------------
    let stop_handler = command_handler(&registrations, "phone-stop").expect("stop handler");
    let stop_outcome = run(&stop_handler, "").await;
    assert!(stop_outcome.ok);
    assert_eq!(stop_outcome.error.as_deref(), Some("Pi Phone stopped"));
    assert!(!plugin.is_running());
    assert_eq!(
        plugin.status_entry(),
        (STATUS_KEY, "📱 ○ phone off".to_owned())
    );

    let stop_again = run(&stop_handler, "").await;
    assert!(stop_again.ok);
    assert_eq!(
        stop_again.error.as_deref(),
        Some("Pi Phone is already stopped.")
    );

    // The listener is gone.
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

    // --- restart + honest errors ---------------------------------------
    let restarted = plugin
        .start_for_test(&format!("{port} -"))
        .await
        .expect("restart on the same port");
    assert_eq!(restarted, port);

    // Invalid port (the running server is left untouched by a parse error).
    let invalid = plugin.start_for_test("--port 70000").await;
    assert!(invalid.is_err(), "70000 is not a valid port");
    assert!(invalid.unwrap_err().contains("Invalid port"));
    assert!(plugin.is_running());

    assert!(plugin.stop_for_test().await);
    assert!(!plugin.stop_for_test().await);

    // Port in use: hold a listener and ask for its port explicitly.
    let occupied = TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("occupied bind");
    let occupied_port = occupied.local_addr().expect("occupied addr").port();
    let in_use = plugin.start_for_test(&format!("{occupied_port} -")).await;
    assert!(in_use.is_err(), "binding a busy port must fail");
    assert!(in_use.unwrap_err().contains("already in use"));
    drop(occupied);
    assert!(!plugin.is_running());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn websocket_handshake_echo_and_close() {
    let _serial = TEST_LOCK.lock().await;
    let plugin = PhonePlugin::new();
    let port = plugin.start_for_test("0 -").await.expect("ephemeral start");

    let (mut conn, head) = ws_handshake(port, "/ws").await;
    assert!(
        head.starts_with("HTTP/1.1 101 Switching Protocols"),
        "{head}"
    );
    assert_eq!(
        header_value(&head, "Sec-WebSocket-Accept"),
        Some("s3pPLMBiTxaQ9kYGzzhZRbK+xOo="),
        "RFC 6455 sample-key accept value"
    );
    assert_eq!(
        header_value(&head, "Upgrade").map(str::to_ascii_lowercase),
        Some("websocket".to_owned())
    );

    // The server announces its status and an empty session catalog.
    let (fin, opcode, payload) = read_server_frame(&mut conn).await;
    assert!(fin);
    assert_eq!(opcode, 0x1);
    let status: Value = serde_json::from_slice(&payload).expect("status JSON");
    assert_eq!(status["channel"], Value::from("server"));
    assert_eq!(status["event"], Value::from("status"));
    assert_eq!(status["data"]["isRunning"], Value::Bool(true));

    let (_, opcode, payload) = read_server_frame(&mut conn).await;
    assert_eq!(opcode, 0x1);
    let catalog: Value = serde_json::from_slice(&payload).expect("catalog JSON");
    assert_eq!(catalog["channel"], Value::from("sessions"));
    assert_eq!(catalog["event"], Value::from("catalog"));

    // Text round-trip (masked client frame -> unmasked server echo).
    conn.send(&masked_frame(0x1, b"hello phone")).await;
    let (fin, opcode, payload) = read_server_frame(&mut conn).await;
    assert!(fin);
    assert_eq!(opcode, 0x1);
    assert_eq!(payload, b"hello phone");

    // Binary round-trip.
    conn.send(&masked_frame(0x2, &[0x00, 0x01, 0xfe, 0xff]))
        .await;
    let (_, opcode, payload) = read_server_frame(&mut conn).await;
    assert_eq!(opcode, 0x2);
    assert_eq!(payload, vec![0x00, 0x01, 0xfe, 0xff]);

    // Ping -> pong.
    conn.send(&masked_frame(0x9, b"ping")).await;
    let (_, opcode, payload) = read_server_frame(&mut conn).await;
    assert_eq!(opcode, 0xa);
    assert_eq!(payload, b"ping");

    // JSON refresh serves the three boot frames.
    conn.send(&masked_frame(0x1, br#"{"kind":"refresh"}"#))
        .await;
    let mut channels = Vec::new();
    for _ in 0..3 {
        let (_, opcode, payload) = read_server_frame(&mut conn).await;
        assert_eq!(opcode, 0x1);
        let frame: Value = serde_json::from_slice(&payload).expect("refresh frame");
        channels.push(frame["channel"].as_str().unwrap_or_default().to_owned());
    }
    assert_eq!(
        channels,
        vec![
            "server".to_owned(),
            "sessions".to_owned(),
            "snapshot".to_owned()
        ]
    );

    // `get_commands` round-trip returns the four commands.
    conn.send(&masked_frame(
        0x1,
        br#"{"kind":"rpc","command":{"type":"get_commands"}}"#,
    ))
    .await;
    let (_, opcode, payload) = read_server_frame(&mut conn).await;
    assert_eq!(opcode, 0x1);
    let reply: Value = serde_json::from_slice(&payload).expect("rpc reply");
    assert_eq!(reply["channel"], Value::from("rpc"));
    assert_eq!(reply["payload"]["type"], Value::from("response"));
    assert_eq!(reply["payload"]["command"], Value::from("get_commands"));
    assert_eq!(reply["payload"]["success"], Value::Bool(true));
    let names: Vec<&str> = reply["payload"]["data"]["commands"]
        .as_array()
        .expect("commands array")
        .iter()
        .filter_map(|command| command["name"].as_str())
        .collect();
    assert_eq!(names, COMMAND_NAMES.to_vec());

    // Client close -> server close handshake, then EOF.
    let mut close_payload = 1000u16.to_be_bytes().to_vec();
    close_payload.extend_from_slice(b"bye");
    conn.send(&masked_frame(0x8, &close_payload)).await;
    let (_, opcode, payload) = read_server_frame(&mut conn).await;
    assert_eq!(opcode, 0x8);
    assert_eq!(&payload[..2], &1000u16.to_be_bytes());

    assert!(plugin.stop_for_test().await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn websocket_token_gate() {
    let _serial = TEST_LOCK.lock().await;
    let plugin = PhonePlugin::new();
    let port = plugin
        .start_for_test("0 secret-token")
        .await
        .expect("ephemeral start");

    let health = http_request(port, "GET", "/api/health", &[]).await;
    assert_eq!(health.status, 200);
    assert_eq!(json_body(&health).await["hasToken"], Value::Bool(true));

    // No token: the handshake completes and the server closes 1008.
    let (mut conn, head) = ws_handshake(port, "/ws").await;
    assert!(head.starts_with("HTTP/1.1 101"), "{head}");
    let (_, opcode, payload) = read_server_frame(&mut conn).await;
    assert_eq!(opcode, 0x8);
    assert_eq!(&payload[..2], &1008u16.to_be_bytes());
    assert_eq!(&payload[2..], b"invalid-token");

    // Wrong token behaves the same.
    let (mut conn, _) = ws_handshake(port, "/ws?token=nope").await;
    let (_, opcode, payload) = read_server_frame(&mut conn).await;
    assert_eq!(opcode, 0x8);
    assert_eq!(&payload[..2], &1008u16.to_be_bytes());

    // Correct token connects.
    let (mut conn, head) = ws_handshake(port, "/ws?token=secret-token").await;
    assert!(head.starts_with("HTTP/1.1 101"), "{head}");
    let (_, opcode, payload) = read_server_frame(&mut conn).await;
    assert_eq!(opcode, 0x1);
    let status: Value = serde_json::from_slice(&payload).expect("status JSON");
    assert_eq!(status["data"]["hasToken"], Value::Bool(true));

    // Mounted path + token also works.
    let (_, head) = ws_handshake(port, &format!("/phone-{port}/ws?token=secret-token")).await;
    assert!(head.starts_with("HTTP/1.1 101"), "{head}");

    assert!(plugin.stop_for_test().await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn connection_cap_answers_503() {
    let _serial = TEST_LOCK.lock().await;
    let plugin = PhonePlugin::new();
    let port = plugin.start_for_test("0 -").await.expect("ephemeral start");

    // Hold the cap open with idle sockets; the server must not spawn a task
    // per connection without a limit.
    let mut held = Vec::new();
    for _ in 0..MAX_CONNECTIONS {
        held.push(Conn::connect(port).await);
    }
    tokio::time::sleep(Duration::from_millis(500)).await;

    // An over-cap socket is either answered with 503 or closed outright; it
    // is never served. (A probe admitted by a scheduling race merely raises
    // the count, so retry.)
    let mut rejected = false;
    for _ in 0..3 {
        let mut stream = TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("probe connect");
        let mut buffer = [0u8; 128];
        match tokio::time::timeout(Duration::from_secs(1), stream.read(&mut buffer)).await {
            Ok(Ok(0)) | Ok(Err(_)) => {
                rejected = true;
                break;
            }
            Ok(Ok(count)) => {
                let head = String::from_utf8_lossy(&buffer[..count]);
                if head.starts_with("HTTP/1.1 503") {
                    rejected = true;
                    break;
                }
                // Admitted by the race: the server is waiting for a request.
            }
            Err(_) => {}
        }
    }
    assert!(rejected, "a connection over the cap must be rejected");

    drop(held);
    assert!(plugin.stop_for_test().await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_ws_frame_is_rejected_before_its_payload() {
    let _serial = TEST_LOCK.lock().await;
    let plugin = PhonePlugin::new();
    let port = plugin.start_for_test("0 -").await.expect("ephemeral start");
    let (mut conn, head) = ws_handshake(port, "/ws").await;
    assert!(head.starts_with("HTTP/1.1 101"), "{head}");
    read_server_frame(&mut conn).await; // status
    read_server_frame(&mut conn).await; // catalog

    // Declare one byte more than the frame cap and send no payload: the
    // server must answer 1009 before allocating or reading the body.
    let length = MAX_WS_FRAME_BYTES as u64 + 1;
    let mut frame = vec![0x81u8, 0x80 | 127];
    frame.extend_from_slice(&length.to_be_bytes());
    frame.extend_from_slice(&[0x11, 0x22, 0x33, 0x44]);
    conn.send(&frame).await;

    let (_, opcode, payload) = read_server_frame(&mut conn).await;
    assert_eq!(opcode, 0x8);
    assert_eq!(&payload[..2], &1009u16.to_be_bytes());

    assert!(plugin.stop_for_test().await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn single_client_replacement_aborts_the_old_connection() {
    let _serial = TEST_LOCK.lock().await;
    let plugin = PhonePlugin::new();
    let port = plugin.start_for_test("0 -").await.expect("ephemeral start");
    let bridge = plugin.bridge_for_test().expect("running bridge");

    let (mut previous, head) = ws_handshake(port, "/ws").await;
    assert!(head.starts_with("HTTP/1.1 101"), "{head}");
    read_server_frame(&mut previous).await; // status
    read_server_frame(&mut previous).await; // catalog

    // Repeated replacements must not leak subscriptions (no 1013).
    for round in 0..3 {
        let (mut current, head) = ws_handshake(port, "/ws").await;
        assert!(
            head.starts_with("HTTP/1.1 101"),
            "round {round}: replacement handshake failed: {head}"
        );
        read_server_frame(&mut current).await; // status
        read_server_frame(&mut current).await; // catalog

        let (_, opcode, payload) = read_server_frame(&mut previous).await;
        assert_eq!(opcode, 0x8, "round {round}: old client was not closed");
        assert_eq!(
            &payload[..2],
            &4009u16.to_be_bytes(),
            "round {round}: old client must see 4009"
        );

        // The replacement owns the socket and receives responses.
        current
            .send(&masked_frame(
                0x1,
                br#"{"kind":"rpc","command":{"type":"get_commands"}}"#,
            ))
            .await;
        let (_, opcode, payload) = read_server_frame(&mut current).await;
        assert_eq!(opcode, 0x1);
        let reply: Value = serde_json::from_slice(&payload).expect("rpc reply");
        assert_eq!(reply["payload"]["success"], Value::Bool(true));

        assert_eq!(
            bridge.client_count(),
            1,
            "round {round}: the old bridge subscription leaked"
        );
        previous = current;
    }

    assert!(plugin.stop_for_test().await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn host_and_origin_validation() {
    let _serial = TEST_LOCK.lock().await;
    let plugin = PhonePlugin::new();
    let port = plugin.start_for_test("0 -").await.expect("ephemeral start");
    let host = format!("127.0.0.1:{port}");
    let wrong_port = if port == u16::MAX { port - 1 } else { port + 1 };

    // Same-origin and loopback aliases are accepted.
    let request = format!("GET /api/health HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    assert_eq!(send_raw_request(port, &request, false).await.status, 200);
    let request =
        format!("GET /api/health HTTP/1.1\r\nHost: localhost:{port}\r\nConnection: close\r\n\r\n");
    assert_eq!(send_raw_request(port, &request, false).await.status, 200);

    // Foreign Host names and ports are rejected.
    let request = format!(
        "GET /api/health HTTP/1.1\r\nHost: evil.example:{port}\r\nConnection: close\r\n\r\n"
    );
    assert_eq!(send_raw_request(port, &request, false).await.status, 403);
    let request = format!(
        "GET /api/health HTTP/1.1\r\nHost: 127.0.0.1:{wrong_port}\r\nConnection: close\r\n\r\n"
    );
    assert_eq!(send_raw_request(port, &request, false).await.status, 403);

    // Absolute-form targets must agree with Host.
    let request = format!(
        "GET http://evil.example:{port}/api/health HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"
    );
    assert_eq!(send_raw_request(port, &request, false).await.status, 400);

    // Browser origins: cross-origin upgrades are refused before the 101.
    let ws_request = |origin: &str| {
        format!(
            "GET /ws HTTP/1.1\r\nHost: {host}\r\nUpgrade: websocket\r\n\
             Connection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
             Sec-WebSocket-Version: 13\r\nOrigin: {origin}\r\n\r\n"
        )
    };
    let response = send_raw_request(port, &ws_request("http://evil.example"), false).await;
    assert_eq!(response.status, 403);
    let response = send_raw_request(port, &ws_request(&format!("http://{host}")), true).await;
    assert_eq!(response.status, 101, "same-origin upgrade must be accepted");

    assert!(plugin.stop_for_test().await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn token_disabled_requires_a_loopback_bind() {
    let _serial = TEST_LOCK.lock().await;
    let plugin = PhonePlugin::new();

    let error = plugin
        .start_for_test("0 --host 0.0.0.0 -")
        .await
        .expect_err("a token-disabled wildcard bind must be refused");
    assert!(error.contains("token"), "{error}");
    assert!(!plugin.is_running());

    // The same bind is allowed once a token is present.
    let port = plugin
        .start_for_test("0 secret-token --host 0.0.0.0")
        .await
        .expect("token-protected wildcard bind");
    assert!(port > 0);
    assert!(plugin.is_running());
    assert!(plugin.stop_for_test().await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn image_mime_is_whitelisted() {
    let _serial = TEST_LOCK.lock().await;
    let plugin = PhonePlugin::new();
    let port = plugin.start_for_test("0 -").await.expect("ephemeral start");

    let key = plugin
        .store_image_for_test("aGVsbG8=", "image/png")
        .expect("png key");
    let image = http_request(port, "GET", &format!("/api/image/{key}"), &[]).await;
    assert_eq!(image.status, 200);
    assert_eq!(
        image.headers.get("content-type").map(String::as_str),
        Some("image/png")
    );
    assert_eq!(
        image
            .headers
            .get("x-content-type-options")
            .map(String::as_str),
        Some("nosniff")
    );
    assert_eq!(image.body, b"hello");

    // Unknown/HTML MIME types are stored as opaque bytes and never echoed;
    // a CRLF-bearing value must not split the response.
    let key = plugin
        .store_image_for_test("d29ybGQ=", "text/html\r\nX-Evil: 1")
        .expect("html key");
    let image = http_request(port, "GET", &format!("/api/image/{key}"), &[]).await;
    assert_eq!(image.status, 200);
    assert_eq!(
        image.headers.get("content-type").map(String::as_str),
        Some("application/octet-stream")
    );
    assert!(!image.headers.contains_key("x-evil"));

    // Parameters on a known image type are stripped, not copied.
    let key = plugin
        .store_image_for_test("cG5n", "IMAGE/PNG; charset=binary")
        .expect("parameterized png key");
    let image = http_request(port, "GET", &format!("/api/image/{key}"), &[]).await;
    assert_eq!(
        image.headers.get("content-type").map(String::as_str),
        Some("image/png")
    );

    assert!(plugin.stop_for_test().await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refresh_preserves_the_full_status_object() {
    let _serial = TEST_LOCK.lock().await;
    let plugin = PhonePlugin::new();
    let port = plugin.start_for_test("0 -").await.expect("ephemeral start");
    let (mut conn, head) = ws_handshake(port, "/ws").await;
    assert!(head.starts_with("HTTP/1.1 101"), "{head}");
    read_server_frame(&mut conn).await; // boot status
    read_server_frame(&mut conn).await; // boot catalog

    conn.send(&masked_frame(0x1, br#"{"kind":"refresh"}"#))
        .await;
    let mut status = None;
    for _ in 0..3 {
        let (_, opcode, payload) = read_server_frame(&mut conn).await;
        assert_eq!(opcode, 0x1);
        let frame: Value = serde_json::from_slice(&payload).expect("refresh frame");
        if frame["event"] == "status" {
            status = Some(frame);
        }
    }
    let status = status.expect("refresh must include a status frame");
    let data = &status["data"];
    assert_eq!(data["isRunning"], Value::Bool(true));
    assert_eq!(data["host"], Value::from("127.0.0.1"));
    assert_eq!(data["port"], Value::from(port));
    assert_eq!(data["hasToken"], Value::Bool(false));
    assert_eq!(data["connectedClients"], Value::from(1));
    assert_eq!(data["singleClientMode"], Value::Bool(true));
    assert!(
        data["cwd"].as_str().is_some_and(|cwd| !cwd.is_empty()),
        "refresh must preserve the cwd: {data}"
    );
    assert!(
        data.get("pendingMessages").is_some(),
        "refresh must preserve the host-owned status fields: {data}"
    );

    assert!(plugin.stop_for_test().await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn path_suggestions_response_matches_the_client_name() {
    let _serial = TEST_LOCK.lock().await;
    let plugin = PhonePlugin::new();
    let port = plugin.start_for_test("0 -").await.expect("ephemeral start");
    let (mut conn, head) = ws_handshake(port, "/ws").await;
    assert!(head.starts_with("HTTP/1.1 101"), "{head}");
    read_server_frame(&mut conn).await; // status
    read_server_frame(&mut conn).await; // catalog

    // The bundled client sends `local-command` with a hyphenated type but
    // routes the response on `payload.command === "path_suggestions"`.
    conn.send(&masked_frame(
        0x1,
        br#"{"kind":"local-command","command":{"type":"path-suggestions","mode":"mention","query":"src","requestId":42}}"#,
    ))
    .await;
    let (_, opcode, payload) = read_server_frame(&mut conn).await;
    assert_eq!(opcode, 0x1);
    let reply: Value = serde_json::from_slice(&payload).expect("reply");
    assert_eq!(reply["payload"]["command"], Value::from("path_suggestions"));
    assert_eq!(reply["payload"]["success"], Value::Bool(true));
    assert_eq!(reply["payload"]["data"]["requestId"], Value::from(42));
    assert_eq!(reply["payload"]["data"]["suggestions"], json!([]));

    // The rpc spelling is normalized the same way.
    conn.send(&masked_frame(
        0x1,
        br#"{"kind":"rpc","command":{"type":"path-suggestions","mode":"cd","query":"x","requestId":7}}"#,
    ))
    .await;
    let (_, _, payload) = read_server_frame(&mut conn).await;
    let reply: Value = serde_json::from_slice(&payload).expect("reply");
    assert_eq!(reply["payload"]["command"], Value::from("path_suggestions"));
    assert_eq!(reply["payload"]["data"]["requestId"], Value::from(7));

    assert!(plugin.stop_for_test().await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_parser_rejects_ambiguous_framing_and_oversized_heads() {
    let _serial = TEST_LOCK.lock().await;
    let plugin = PhonePlugin::new();
    let port = plugin.start_for_test("0 -").await.expect("ephemeral start");
    let host = format!("127.0.0.1:{port}");

    let both = format!(
        "POST / HTTP/1.1\r\nHost: {host}\r\nContent-Length: 0\r\n\
         Transfer-Encoding: chunked\r\nConnection: close\r\n\r\n"
    );
    assert_eq!(send_raw_request(port, &both, false).await.status, 400);

    let chunked = format!(
        "POST / HTTP/1.1\r\nHost: {host}\r\nTransfer-Encoding: chunked\r\n\
         Connection: close\r\n\r\n"
    );
    assert_eq!(send_raw_request(port, &chunked, false).await.status, 400);

    let huge_body = format!(
        "POST / HTTP/1.1\r\nHost: {host}\r\nContent-Length: 99999999\r\n\
         Connection: close\r\n\r\n"
    );
    assert_eq!(send_raw_request(port, &huge_body, false).await.status, 413);

    // Header-count cap.
    let mut many_headers = format!("GET / HTTP/1.1\r\nHost: {host}\r\n");
    for index in 0..101 {
        many_headers.push_str(&format!("X-Pad-{index}: x\r\n"));
    }
    many_headers.push_str("Connection: close\r\n\r\n");
    assert_eq!(
        send_raw_request(port, &many_headers, false).await.status,
        400
    );

    // Bare LF is not accepted as a line ending.
    let bare_lf = format!("GET / HTTP/1.1\nHost: {host}\r\nConnection: close\r\n\r\n");
    assert_eq!(send_raw_request(port, &bare_lf, false).await.status, 400);

    assert!(plugin.stop_for_test().await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn phone_start_auto_scans_free_ports() {
    let _serial = TEST_LOCK.lock().await;
    // No explicit port: the server scans 8787..=8987. This asserts the scan
    // contract without requiring 8787 to be free: the first bound port is in
    // the documented range and the status text reports it.
    let plugin = PhonePlugin::new();
    let port = plugin.start_for_test("").await.expect("auto-port start");
    assert!(
        (8787..=8987).contains(&port),
        "auto-selected port {port} is outside 8787..=8987"
    );
    assert!(plugin.is_running());
    assert!(plugin.stop_for_test().await);
}

#[test]
fn token_generation_shape() {
    let token = pir::plugins::builtin::phone::generate_token();
    // 6-6-4 base64url: 16 payload characters plus the two separators. The
    // payload alphabet itself contains `-`, so do not split on it.
    assert_eq!(token.len(), 16 + 2, "6-6-4 with two dashes: {token}");
    assert_eq!(token.as_bytes()[6], b'-');
    assert_eq!(token.as_bytes()[13], b'-');
    assert!(token
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-'));
    assert_ne!(token, pir::plugins::builtin::phone::generate_token());
}

#[test]
fn parse_args_mirrors_the_reference() {
    use pir::plugins::builtin::phone::parse_start_args;
    use pir::plugins::builtin::phone::PhoneConfig;

    let base = PhoneConfig {
        host: "127.0.0.1".to_owned(),
        port: 8787,
        token: String::new(),
        cwd: std::env::current_dir().unwrap(),
        idle_timeout_ms: 7_200_000,
    };

    let parsed = parse_start_args("9000 tok --host 0.0.0.0 --idle-mins 5", &base).unwrap();
    assert_eq!(parsed.config.port, 9000);
    assert_eq!(parsed.config.token, "tok");
    assert_eq!(parsed.config.host, "0.0.0.0");
    assert_eq!(parsed.config.idle_timeout_ms, 300_000);
    assert!(parsed.port_specified && parsed.token_specified && parsed.idle_specified);

    let disabled = parse_start_args("--port=9001 --token=- --idle-secs 2", &base).unwrap();
    assert_eq!(disabled.config.port, 9001);
    assert!(disabled.config.token.is_empty());
    assert_eq!(disabled.config.idle_timeout_ms, 2_000);

    assert!(parse_start_args("--port nope", &base).is_err());
    assert!(parse_start_args("--port 70000", &base).is_err());

    // Negative idle values are ignored, like the reference parser.
    let negative_idle = parse_start_args("--idle-mins -1", &base).unwrap();
    assert_eq!(negative_idle.config.idle_timeout_ms, base.idle_timeout_ms);

    // The base config is never mutated.
    let untouched = parse_start_args("", &base).unwrap();
    assert_eq!(untouched.config, base);
}
