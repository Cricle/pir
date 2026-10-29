//! W13-I: the phone `/ws` transport is `tokio-tungstenite`'s.
//!
//! The wire contract itself is pinned by `tests/ws_rfc6455.rs` (18 tests) and
//! `tests/phone_server.rs` (14 tests), which drive the server with a raw
//! `TcpStream`. This suite covers the pieces that only exist because the
//! handshake was moved into the crate:
//!
//! - the 101 is the crate's response (status line + `Sec-WebSocket-Accept`);
//! - the replayed request keeps accepting the RFC 7230 token-list spellings
//!   (`Upgrade: h2c, websocket`) that the crate's literal comparison rejects,
//!   while still answering the 400/426 policy responses;
//! - a frame pipelined in the same TCP write as the upgrade is not lost by
//!   the hand-off from the hand-written HTTP reader to the crate;
//! - close 4009 still travels as a library close code with its reason.
#![cfg(feature = "phone")]

use std::collections::HashMap;
use std::time::Duration;

use pir::plugins::builtin::phone::PhonePlugin;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Serializes the tests: each one starts a full server, and loopback clients
/// contend for the accept backlog when several run at once.
static TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const IO_TIMEOUT: Duration = Duration::from_secs(10);
const SAMPLE_KEY: &str = "dGhlIHNhbXBsZSBub25jZQ==";
/// RFC 6455 1.3: the accept value for `SAMPLE_KEY`.
const SAMPLE_ACCEPT: &str = "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=";
const MASK: [u8; 4] = [0x37, 0xfa, 0x21, 0x3d];

// ---------------------------------------------------------------------------
// Tiny buffered raw client
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

struct RawResponse {
    status: u16,
    headers: HashMap<String, String>,
}

async fn send_raw_request(port: u16, request: &str) -> RawResponse {
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
    RawResponse { status, headers }
}

fn upgrade_request(
    port: u16,
    path: &str,
    upgrade: &str,
    connection: &str,
    key: &str,
    version: &str,
) -> String {
    format!(
        "GET {path} HTTP/1.1\r\n\
         Host: 127.0.0.1:{port}\r\n\
         Upgrade: {upgrade}\r\n\
         Connection: {connection}\r\n\
         Sec-WebSocket-Key: {key}\r\n\
         Sec-WebSocket-Version: {version}\r\n\r\n"
    )
}

fn valid_upgrade_request(port: u16) -> String {
    upgrade_request(port, "/ws", "websocket", "Upgrade", SAMPLE_KEY, "13")
}

/// One masked client text frame, as RFC 6455 puts it on the wire.
fn masked_text(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(6 + payload.len());
    out.push(0x81);
    out.push(0x80 | payload.len() as u8);
    out.extend_from_slice(&MASK);
    out.extend(
        payload
            .iter()
            .enumerate()
            .map(|(index, byte)| byte ^ MASK[index % 4]),
    );
    out
}

/// Read exactly one server frame: `(opcode, payload)`.
async fn read_server_frame(conn: &mut Conn) -> (u8, Vec<u8>) {
    let header = conn.read_exact(2).await;
    assert_eq!(header[0] & 0x80, 0x80, "server frames are final");
    assert_eq!(header[1] & 0x80, 0, "server frames must not be masked");
    let opcode = header[0] & 0x0f;
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
    (opcode, payload)
}

/// Drain the two boot frames (status + catalog) every connection receives.
async fn drain_boot_frames(conn: &mut Conn) {
    for _ in 0..2 {
        let (opcode, _) = read_server_frame(conn).await;
        assert_eq!(opcode, 0x1, "expected a boot text frame");
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upgrade_101_is_the_crate_handshake() {
    let _serial = TEST_LOCK.lock().await;
    let plugin = PhonePlugin::new();
    let port = plugin.start_for_test("0 -").await.expect("ephemeral start");

    let response = send_raw_request(port, &valid_upgrade_request(port)).await;
    assert_eq!(response.status, 101);
    // RFC 6455 4.2.2, example 1.3: the accept value is derived by the crate.
    assert_eq!(
        response
            .headers
            .get("sec-websocket-accept")
            .map(String::as_str),
        Some(SAMPLE_ACCEPT)
    );
    assert_eq!(
        response
            .headers
            .get("upgrade")
            .map(|value| value.to_ascii_lowercase()),
        Some("websocket".to_owned())
    );
    // No extension is negotiated: the crate echoes none, and the client must
    // not be told otherwise.
    assert!(!response.headers.contains_key("sec-websocket-extensions"));

    assert!(plugin.stop_for_test().await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn token_list_upgrade_headers_still_reach_the_crate_handshake() {
    let _serial = TEST_LOCK.lock().await;
    let plugin = PhonePlugin::new();
    let port = plugin.start_for_test("0 -").await.expect("ephemeral start");

    // RFC 7230 lists: the HTTP layer accepts these; the crate compares the
    // `Upgrade` value literally, so the replayed request must normalize it.
    let response = send_raw_request(
        port,
        &upgrade_request(
            port,
            "/ws",
            "h2c, websocket",
            "keep-alive, Upgrade",
            SAMPLE_KEY,
            "13",
        ),
    )
    .await;
    assert_eq!(response.status, 101);
    assert_eq!(
        response
            .headers
            .get("sec-websocket-accept")
            .map(String::as_str),
        Some(SAMPLE_ACCEPT)
    );

    assert!(plugin.stop_for_test().await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn handshake_policy_responses_survive_the_transport_swap() {
    let _serial = TEST_LOCK.lock().await;
    let plugin = PhonePlugin::new();
    let port = plugin.start_for_test("0 -").await.expect("ephemeral start");

    // 426 (RFC 6455 4.2.2) must still name the supported version.
    for version in ["8", "13, 8", ""] {
        let response = send_raw_request(
            port,
            &upgrade_request(port, "/ws", "websocket", "Upgrade", SAMPLE_KEY, version),
        )
        .await;
        assert_eq!(response.status, 426, "version {version:?}");
        assert_eq!(
            response
                .headers
                .get("sec-websocket-version")
                .map(String::as_str),
            Some("13")
        );
    }

    // A malformed key is still this server's 400, not a silent drop.
    let response = send_raw_request(
        port,
        &upgrade_request(port, "/ws", "websocket", "Upgrade", "not-base64!!", "13"),
    )
    .await;
    assert_eq!(response.status, 400);

    assert!(plugin.stop_for_test().await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_frame_pipelined_with_the_upgrade_is_not_lost() {
    let _serial = TEST_LOCK.lock().await;
    let plugin = PhonePlugin::new();
    let port = plugin.start_for_test("0 -").await.expect("ephemeral start");

    // One write: the upgrade head and the first frame. The hand-written HTTP
    // reader consumes both; the bytes past the head must be replayed to the
    // crate instead of being dropped by the hand-off.
    let mut request = valid_upgrade_request(port).into_bytes();
    request.extend_from_slice(&masked_text(b"pipelined"));

    let mut conn = Conn::connect(port).await;
    conn.send(&request).await;
    let head = conn.read_through(b"\r\n\r\n").await;
    assert!(
        String::from_utf8_lossy(&head).starts_with("HTTP/1.1 101"),
        "handshake failed: {}",
        String::from_utf8_lossy(&head)
    );

    drain_boot_frames(&mut conn).await;
    let (opcode, payload) = read_server_frame(&mut conn).await;
    assert_eq!(opcode, 0x1);
    assert_eq!(payload, b"pipelined", "the pipelined frame was dropped");

    assert!(plugin.stop_for_test().await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replacement_close_4009_is_a_library_close_code() {
    let _serial = TEST_LOCK.lock().await;
    let plugin = PhonePlugin::new();
    let port = plugin.start_for_test("0 -").await.expect("ephemeral start");

    let mut previous = Conn::connect(port).await;
    previous.send(valid_upgrade_request(port).as_bytes()).await;
    let head = previous.read_through(b"\r\n\r\n").await;
    assert!(String::from_utf8_lossy(&head).starts_with("HTTP/1.1 101"));
    drain_boot_frames(&mut previous).await;

    // A second client replaces the first: 4009 is a library close code
    // (`CloseCode::Library(4009)`), so the crate sends it as-is.
    let mut current = Conn::connect(port).await;
    current.send(valid_upgrade_request(port).as_bytes()).await;
    let head = current.read_through(b"\r\n\r\n").await;
    assert!(String::from_utf8_lossy(&head).starts_with("HTTP/1.1 101"));

    let (opcode, payload) = read_server_frame(&mut previous).await;
    assert_eq!(opcode, 0x8, "the replaced client must receive a close");
    assert_eq!(&payload[..2], &4009u16.to_be_bytes());
    assert_eq!(&payload[2..], b"replaced-by-new-client");

    assert!(plugin.stop_for_test().await);
}
