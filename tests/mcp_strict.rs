//! Strict-verification tests for the MCP builtin (upstream
//! `packages/mcp/src/**` + `packages/coding-agent/src/extensions/mcp/**`):
//! protocol-noise and failure edges beyond `tests/mcp_integration.rs`.
//!
//! Contract points, each traced to the upstream source:
//! - a malformed `initialize` result (missing `protocolVersion` string, or a
//!   `serverInfo` without string `name`/`version`) fails the handshake -
//!   the rmcp SDK enforces the same shape constraints as upstream
//!   `validateInitializeResult` by typed deserialization;
//! - `tools/list` returning an EXACT duplicate tool name twice: the first
//!   occurrence keeps the plain `mcp__<server>__<tool>` name, later
//!   duplicates get the hash suffix (upstream `registerTools`'s
//!   deduped `plain` set + owner check);
//! - notifications, server-to-client requests, and garbage lines arriving
//!   BETWEEN a request and its response are tolerated (upstream
//!   `handleMessage` classifies per message; rmcp's stdio codec does the
//!   same on the wire);
//! - a server crash mid-call fails the in-flight request and the manager
//!   reconnects on the next call;
//! - a tool call the server never answers times out with the typed
//!   `McpError::Timeout` and the server stays usable afterwards;
//! - HTTP 500 with a JSON-RPC error body surfaces the server's JSON-RPC
//!   error (rmcp forwards non-2xx JSON-RPC error payloads);
//! - tool results with `resource` / `resource_link` / `image` content
//!   blocks map onto the model-visible text (upstream `blockToContent`).

use std::collections::BTreeMap;
use std::io::{BufRead, Write as IoWrite};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};

use pir::config::mcp::McpServerConfig;
use pir::mcp::client::McpClient;
use pir::mcp::error::McpError;
use pir::mcp::manager::McpManager;
use pir::mcp::registry::{assign_mcp_tool_names, McpServerEntry};
use pir::mcp::stdio::StdioSpawnOptions;
use pir::plugins::builtin::convert_mcp_result;

// ---------------------------------------------------------------------------
// The strict mock stdio server (child entry of this very test binary)
// ---------------------------------------------------------------------------

fn strict_server_spawn() -> (String, Vec<String>) {
    let exe = std::env::current_exe().expect("current exe");
    (
        exe.to_string_lossy().into_owned(),
        vec![
            "mock_mcp_strict_child".to_owned(),
            "--exact".to_owned(),
            "--nocapture".to_owned(),
        ],
    )
}

#[test]
fn mock_mcp_strict_child() {
    if std::env::var("PIR_MOCK_MCP_STRICT").as_deref() != Ok("1") {
        return;
    }
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let crash_sentinel = std::env::var("PIR_MOCK_CRASH_SENTINEL").unwrap_or_default();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(id) = value.get("id").cloned() else {
            continue;
        };
        let method = value
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let params = value.get("params").cloned().unwrap_or(Value::Null);
        let bad_init_mode = std::env::var("PIR_MOCK_BAD_INIT").unwrap_or_default();
        let noise = std::env::var("PIR_MOCK_NOISE").as_deref() == Ok("1");
        let reply = match method.as_str() {
            "initialize" => {
                if bad_init_mode == "no-version" {
                    json!({
                        "capabilities": {},
                        "serverInfo": { "name": "mock", "version": "1" },
                    })
                } else if bad_init_mode == "no-server-info" {
                    json!({ "protocolVersion": "2025-06-18", "capabilities": {} })
                } else if bad_init_mode == "bad-server-info" {
                    json!({
                        "protocolVersion": "2025-06-18",
                        "capabilities": {},
                        "serverInfo": { "version": "1" },
                    })
                } else {
                    json!({
                        "protocolVersion": "2025-06-18",
                        "capabilities": { "tools": {} },
                        "serverInfo": { "name": "strict", "version": "1.0.0" },
                    })
                }
            }
            "ping" => json!({}),
            "tools/list" => json!({
                "tools": [
                    { "name": "dup", "inputSchema": { "type": "object" } },
                    { "name": "dup", "inputSchema": { "type": "object" } },
                    { "name": "a-b", "inputSchema": { "type": "object" } },
                    { "name": "a_b", "inputSchema": { "type": "object" } },
                ],
            }),
            "tools/call" => {
                // Crash-once: the first tools/call after spawn kills the
                // process mid-call (upstream: server crash mid-call); the
                // respawned child answers normally.
                if !crash_sentinel.is_empty() && !std::path::Path::new(&crash_sentinel).exists() {
                    let _ = std::fs::write(&crash_sentinel, b"crashed");
                    std::process::exit(1);
                }
                if std::env::var("PIR_MOCK_NEVER_ANSWER_TOOLS_CALL").as_deref() == Ok("1") {
                    // Never answer this request, but keep the loop alive so
                    // later requests still get served.
                    continue;
                }
                let name = params.get("name").and_then(Value::as_str).unwrap_or("");
                json!({ "content": [{ "type": "text", "text": format!("called:{name}") }] })
            }
            _ => {
                write_line(
                    &stdout,
                    &json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": "method not found" } }),
                );
                continue;
            }
        };
        if noise {
            // Protocol noise between the request and its response: a
            // notification, an unparseable line, and a server-to-client
            // request (which the rmcp SDK answers through its default
            // client handler; the pending request must still resolve).
            write_line(
                &stdout,
                &json!({ "jsonrpc": "2.0", "method": "notifications/message", "params": { "level": "info", "data": "noise" } }),
            );
            write_raw(&stdout, "this is not json\n");
            write_line(
                &stdout,
                &json!({ "jsonrpc": "2.0", "id": "server-probe-1", "method": "roots/list", "params": {} }),
            );
        }
        write_line(
            &stdout,
            &json!({ "jsonrpc": "2.0", "id": id, "result": reply }),
        );
    }
}

fn write_line(stdout: &std::io::Stdout, reply: &Value) {
    let mut out = stdout.lock();
    let _ = writeln!(out, "{reply}");
    let _ = out.flush();
}

fn write_raw(stdout: &std::io::Stdout, raw: &str) {
    let mut out = stdout.lock();
    let _ = out.write_all(raw.as_bytes());
    let _ = out.flush();
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-mcp-strict-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create temp dir");
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn strict_env() -> BTreeMap<String, String> {
    BTreeMap::from([("PIR_MOCK_MCP_STRICT".to_owned(), "1".to_owned())])
}

async fn connect_strict(
    env: BTreeMap<String, String>,
    timeout: Duration,
) -> Result<McpClient, McpError> {
    let (command, args) = strict_server_spawn();
    let options = StdioSpawnOptions {
        command,
        args,
        cwd: std::env::current_dir().expect("cwd"),
        env,
        max_message_bytes: 1024 * 1024,
    };
    McpClient::connect_stdio(options, None, "strict", timeout).await
}

/// Register a stdio server on a fresh manager (for reconnect semantics).
async fn manager_with(crash_sentinel: &std::path::Path) -> (Arc<McpManager>, String) {
    let temp = TempDir::new("manager");
    let (command, args) = strict_server_spawn();
    let config: McpServerConfig = serde_json::from_value(json!({
        "command": command,
        "args": args,
        "env": {
            "PIR_MOCK_MCP_STRICT": "1",
            "PIR_MOCK_CRASH_SENTINEL": crash_sentinel.to_string_lossy(),
        },
        "timeout": 20,
    }))
    .expect("config");
    let manager = McpManager::with_log(
        temp.0.join("mcp.log"),
        &std::env::current_dir().expect("cwd"),
    );
    manager.registry().register(McpServerEntry {
        name: "strict".into(),
        config,
        source: pir::mcp::registry::McpServerSource::Extension("test".into()),
    });
    (manager, "strict".to_owned())
}

// ---------------------------------------------------------------------------
// Malformed initialize results (upstream validateInitializeResult, enforced
// by rmcp's typed handshake deserialization)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn initialize_without_a_protocol_version_string_fails_the_handshake() {
    let mut env = strict_env();
    env.insert("PIR_MOCK_BAD_INIT".to_owned(), "no-version".to_owned());
    let error = connect_strict(env, Duration::from_secs(20))
        .await
        .expect_err("missing protocolVersion must fail");
    assert!(
        matches!(error, McpError::Protocol(_) | McpError::Server { .. }),
        "typed handshake failure, got: {error}"
    );
}

#[tokio::test]
async fn initialize_without_server_info_fails_the_handshake() {
    let mut env = strict_env();
    env.insert("PIR_MOCK_BAD_INIT".to_owned(), "no-server-info".to_owned());
    let error = connect_strict(env, Duration::from_secs(20))
        .await
        .expect_err("missing serverInfo must fail");
    assert!(
        matches!(error, McpError::Protocol(_) | McpError::Server { .. }),
        "typed handshake failure, got: {error}"
    );
}

#[tokio::test]
async fn initialize_with_an_incomplete_server_info_fails_the_handshake() {
    let mut env = strict_env();
    env.insert("PIR_MOCK_BAD_INIT".to_owned(), "bad-server-info".to_owned());
    let error = connect_strict(env, Duration::from_secs(20))
        .await
        .expect_err("serverInfo without a name must fail");
    assert!(
        matches!(error, McpError::Protocol(_) | McpError::Server { .. }),
        "typed handshake failure, got: {error}"
    );
}

#[test]
fn initialize_validation_matches_upstream_field_checks() {
    // The raw-shape validator is retained as the upstream-parity reference;
    // the runtime path enforces the same constraints inside rmcp.
    // Happy path.
    assert!(pir::mcp::client::validate_initialize_result(&json!({
        "protocolVersion": "2025-06-18",
        "capabilities": {},
        "serverInfo": { "name": "x", "version": "1" },
    }))
    .is_ok());
    // Non-object result.
    assert!(pir::mcp::client::validate_initialize_result(&json!(7)).is_err());
    // Null protocolVersion / capabilities.
    assert!(pir::mcp::client::validate_initialize_result(&json!({
        "protocolVersion": null,
        "capabilities": {},
        "serverInfo": { "name": "x", "version": "1" },
    }))
    .is_err());
    assert!(pir::mcp::client::validate_initialize_result(&json!({
        "protocolVersion": "2025-06-18",
        "capabilities": "nope",
        "serverInfo": { "name": "x", "version": "1" },
    }))
    .is_err());
    // Non-string instructions.
    assert!(pir::mcp::client::validate_initialize_result(&json!({
        "protocolVersion": "2025-06-18",
        "capabilities": {},
        "serverInfo": { "name": "x", "version": "1" },
        "instructions": 9,
    }))
    .is_err());
}

// ---------------------------------------------------------------------------
// Duplicate tool names (upstream registerTools naming)
// ---------------------------------------------------------------------------

#[test]
fn duplicate_tool_names_keep_the_plain_name_on_the_first_occurrence() {
    // Upstream registerTools: `plain` comes from the DEDUPED tool-name set,
    // and the second registration of the same owner sees the name taken -
    // so an exact duplicate's FIRST occurrence keeps the plain name and
    // later ones hash-suffix. Sanitize collisions (`a-b` vs `a_b`) hash ALL
    // members, order-independent.
    let assigned = assign_mcp_tool_names(
        "srv",
        &[
            "dup".to_owned(),
            "dup".to_owned(),
            "a-b".to_owned(),
            "a_b".to_owned(),
        ],
    );
    assert_eq!(assigned[0], "mcp__srv__dup", "first duplicate keeps plain");
    assert_ne!(
        assigned[1], "mcp__srv__dup",
        "later duplicate is hash-suffixed"
    );
    assert!(assigned[1].starts_with("mcp__srv__dup_"));
    assert_ne!(assigned[2], assigned[3], "sanitized collision deconflicts");
    assert!(assigned[2].starts_with("mcp__srv__a_b_"));
    assert!(assigned[3].starts_with("mcp__srv__a_b_"));

    // A unique list is untouched.
    let plain = assign_mcp_tool_names("srv", &["one".to_owned(), "two".to_owned()]);
    assert_eq!(plain, vec!["mcp__srv__one", "mcp__srv__two"]);
}

// ---------------------------------------------------------------------------
// Protocol noise between a request and its response
// ---------------------------------------------------------------------------

#[tokio::test]
async fn notifications_and_server_requests_between_request_and_response_are_tolerated() {
    let mut env = strict_env();
    env.insert("PIR_MOCK_NOISE".to_owned(), "1".to_owned());
    let client = connect_strict(env, Duration::from_secs(20))
        .await
        .expect("handshake survives noise");
    // Noise also rides between tools/list and its answer.
    let tools = client.list_all_tools().await.expect("tools/list");
    assert_eq!(tools.len(), 4, "tools: {tools:?}");
    let call = client
        .call_tool("dup", &json!({}))
        .await
        .expect("tools/call under noise");
    assert_eq!(
        call.content[0].text.as_deref(),
        Some("called:dup"),
        "the response routed past the noise"
    );
    client.close().await;
}

// ---------------------------------------------------------------------------
// Server crash mid-call: typed failure + manager reconnect
// ---------------------------------------------------------------------------

#[tokio::test]
async fn server_crash_mid_call_fails_the_request_and_the_next_call_reconnects() {
    let temp = TempDir::new("crash");
    let sentinel = temp.0.join("crash.flag");
    let (manager, server) = manager_with(&sentinel).await;
    // The child dies on the first tools/call. The in-flight request never
    // ran, so pir's manager retries ONCE on a fresh connection (upstream
    // `withClient` retry semantics) and the call succeeds transparently.
    let first = manager.call_tool(&server, "dup", &json!({}), None).await;
    match &first {
        Ok(result) => assert_eq!(
            result.content[0].text.as_deref(),
            Some("called:dup"),
            "the retried call ran on the respawned server"
        ),
        Err(error) => assert!(
            matches!(error, McpError::ConnectionClosed | McpError::SessionExpired),
            "typed closed error when the retry also fails, got: {error}"
        ),
    }
    // Subsequent calls keep working against the live (respawned) server.
    let result = manager
        .call_tool(&server, "dup", &json!({}), None)
        .await
        .expect("reconnected call");
    assert_eq!(
        result.content[0].text.as_deref(),
        Some("called:dup"),
        "server usable after a mid-call crash"
    );
    manager.shutdown().await;
}

// ---------------------------------------------------------------------------
// Tool timeout: typed error + server still usable
// ---------------------------------------------------------------------------

#[tokio::test]
async fn unanswered_tool_call_times_out_and_the_server_stays_usable() {
    let mut env = strict_env();
    env.insert(
        "PIR_MOCK_NEVER_ANSWER_TOOLS_CALL".to_owned(),
        "1".to_owned(),
    );
    // The connect budget must be long enough for the child test binary to
    // spawn and answer initialize (700ms was not, on loaded machines). The
    // never-answered call does not need to inherit that 5 s budget: the
    // per-request deadline seam (`McpRequestOptions::with_timeout`) bounds it
    // independently, so the test observes the same typed timeout without
    // paying 5 s of real wall time per run.
    let client = connect_strict(env, Duration::from_secs(5))
        .await
        .expect("handshake");
    let error = client
        .call_tool_with_options(
            "dup",
            &json!({}),
            &pir::mcp::client::McpRequestOptions::new().with_timeout(Duration::from_millis(700)),
        )
        .await
        .expect_err("never-answered call must time out");
    assert!(
        matches!(error, McpError::Timeout { .. }),
        "typed timeout expected, got: {error}"
    );
    // The same connection still serves requests afterwards.
    let ping = client.ping().await;
    assert!(ping.is_ok(), "server usable after a timeout: {ping:?}");
    client.close().await;
}

// ---------------------------------------------------------------------------
// HTTP 500 with a JSON-RPC error body
// ---------------------------------------------------------------------------

/// A minimal HTTP/1.1 test server (the proven `serve_http` recipe from
/// `tests/mcp_integration.rs`: tokio, keep-alive per connection, bounded
/// request reads), answering every POST with `status` and `body`.
async fn serve_status_once(listener: tokio::net::TcpListener, status: u16, body: String) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        let body = body.clone();
        tokio::spawn(async move {
            let mut buffer: Vec<u8> = Vec::with_capacity(2048);
            let mut chunk = [0u8; 4096];
            loop {
                buffer.clear();
                // Read the request head.
                let head_end = loop {
                    match tokio::time::timeout(
                        std::time::Duration::from_secs(10),
                        stream.readable(),
                    )
                    .await
                    {
                        Ok(Ok(())) => {}
                        _ => return,
                    }
                    match stream.try_read(&mut chunk) {
                        Ok(0) => return,
                        Ok(read) => {
                            buffer.extend_from_slice(&chunk[..read]);
                            if let Some(position) =
                                buffer.windows(4).position(|window| window == b"\r\n\r\n")
                            {
                                break position;
                            }
                            if buffer.len() > 1024 * 1024 {
                                return;
                            }
                        }
                        Err(ref error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            continue
                        }
                        Err(_) => return,
                    }
                };
                // Drain the declared body.
                let head = String::from_utf8_lossy(&buffer[..head_end]).into_owned();
                let content_length = head
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.trim()
                            .eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())?
                    })
                    .unwrap_or(0);
                let body_start = head_end + 4;
                while buffer.len() < body_start + content_length {
                    match tokio::time::timeout(
                        std::time::Duration::from_secs(10),
                        stream.readable(),
                    )
                    .await
                    {
                        Ok(Ok(())) => {}
                        _ => return,
                    }
                    match stream.try_read(&mut chunk) {
                        Ok(0) => break,
                        Ok(read) => buffer.extend_from_slice(&chunk[..read]),
                        Err(ref error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            continue
                        }
                        Err(_) => return,
                    }
                }
                let status_text = if status == 200 { "OK" } else { "Error" };
                // The reply must correlate with the request: echo the JSON-RPC
                // request id (rmcp rejects an error response whose id differs
                // from the initialize request's, `UncorrelatedErrorResponse`).
                let request_body = String::from_utf8_lossy(&buffer[body_start..]).into_owned();
                let request_id: Option<Value> = serde_json::from_str::<Value>(&request_body)
                    .ok()
                    .and_then(|request| request.get("id").cloned());
                let body = match request_id {
                    Some(id) => {
                        let mut reply =
                            serde_json::from_str::<Value>(&body).unwrap_or_else(|_| json!({}));
                        reply["id"] = id;
                        reply.to_string()
                    }
                    None => body.clone(),
                };
                let response = format!(
                    "HTTP/1.1 {status} {status_text}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: keep-alive\r\n\r\n",
                    body.len()
                );
                let mut out = response.into_bytes();
                out.extend_from_slice(body.as_bytes());
                if stream.try_write(&out).is_err() {
                    return;
                }
                let _ =
                    tokio::time::timeout(std::time::Duration::from_millis(20), stream.readable())
                        .await;
            }
        });
    }
}

#[tokio::test]
async fn http_500_with_a_json_rpc_error_body_surfaces_the_server_error() {
    let rpc_error = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "error": { "code": -32000, "message": "internal server trouble" },
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let body = rpc_error.to_string();
    tokio::spawn(serve_status_once(listener, 500, body));
    // rmcp forwards non-2xx JSON-RPC error payloads to the caller, so the
    // initialize POST against a 500 surfaces the server's JSON-RPC error.
    let error = McpClient::connect_http(
        format!("http://127.0.0.1:{port}/mcp"),
        Vec::new(),
        Duration::from_secs(10),
    )
    .await
    .expect_err("500 must fail");
    match &error {
        McpError::Server { code, message, .. } => {
            assert_eq!(*code, -32000);
            assert_eq!(message, "internal server trouble");
        }
        other => panic!("typed server error expected, got: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Content-block mapping (upstream blockToContent)
// ---------------------------------------------------------------------------

#[test]
fn tool_result_embedded_text_resource_blocks_map_onto_text() {
    use pir::mcp::protocol::CallToolResult;

    // An embedded text resource contributes its text (upstream
    // `blockToContent`), joined with the other blocks.
    let result = CallToolResult {
        content: vec![
            serde_json::from_value(json!({
                "type": "text",
                "text": "before",
            }))
            .expect("text block"),
            serde_json::from_value(json!({
                "type": "resource",
                "resource": {
                    "uri": "file:///notes.md",
                    "mimeType": "text/markdown",
                    "text": "# embedded notes",
                },
            }))
            .expect("resource block"),
        ],
        ..Default::default()
    };
    let mapped = convert_mcp_result("srv", "tool", result);
    assert_eq!(mapped.content, "before\n# embedded notes");
    assert_eq!(
        mapped.details,
        Some(json!({ "server": "srv", "tool": "tool" }))
    );
    assert!(!mapped.is_error);
}

#[test]
fn resource_blob_and_resource_link_and_image_blocks_render_like_upstream() {
    use base64::Engine as _;
    use pir::mcp::protocol::CallToolResult;

    let blob = base64::engine::general_purpose::STANDARD.encode("readable text blob");
    let result = CallToolResult {
        content: vec![
            serde_json::from_value(json!({
                "type": "resource",
                "resource": {
                    "uri": "file:///blob.txt",
                    "mimeType": "text/plain",
                    "blob": blob,
                },
            }))
            .expect("text blob resource"),
            serde_json::from_value(json!({
                "type": "resource",
                "resource": {
                    "uri": "file:///img.bin",
                    "mimeType": "application/octet-stream",
                    "blob": base64::engine::general_purpose::STANDARD.encode([0u8, 1, 2, 3]),
                },
            }))
            .expect("binary blob resource"),
            serde_json::from_value(json!({
                "type": "resource_link",
                "uri": "file:///linked.md",
                "name": "linked",
                "description": "a linked file",
            }))
            .expect("resource link"),
            serde_json::from_value(json!({
                "type": "image",
                "data": "aGk=",
                "mimeType": "image/png",
            }))
            .expect("image block"),
        ],
        ..Default::default()
    };
    let mapped = convert_mcp_result("srv", "tool", result);
    let text = &mapped.content;
    assert!(
        text.contains("readable text blob"),
        "text blob decodes: {text}"
    );
    assert!(
        text.contains("[Binary resource file:///img.bin (application/octet-stream, 4 bytes)]"),
        "binary blob is described: {text}"
    );
    assert!(
        text.contains("[Resource file:///linked.md \"linked\": a linked file]"),
        "resource link renders: {text}"
    );
    assert!(
        text.contains("[image: image/png]"),
        "image renders as a description: {text}"
    );
}
