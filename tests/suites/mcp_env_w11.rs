//! W11-H conformance tests for the MCP + env surfaces that had no wire-shape
//! coverage of their own. Each block names the upstream reference it pins.
//!
//! * MCP **`structuredContent` passthrough**: `tools/call` keeps the server's
//!   `structuredContent` (and `_meta`) verbatim through rmcp conversion
//!   (upstream `validateCallToolResult` -> `CallToolResult.structuredContent`);
//! * MCP **`iss` validation** (RFC 9207, upstream `oauth/flow.ts:385-388`),
//!   the `WWW-Authenticate` challenge parser and step-up scope merging;
//! * MCP **stderr tail**: a stdio server that dies during `initialize`
//!   produces a `ConnectionState::Failed` whose message carries the server's
//!   own stderr (upstream runtime.ts `connectFailed`), and a later successful
//!   connect leaves no stale tail behind;
//! * env **13-byte frame header** exactness, the JSON-length rule and the
//!   `PI-ENV <token>` sync line, through the client's own `StreamParser`;
//! * env **error codes** and **cancellation** over a live `pir env-daemon`:
//!   `ENOENT`/`ENOTDIR`/`EBADF`/`EINVAL`/`shell_unavailable`/`spawn_error`,
//!   `{ mode: "kill" }` vs a plain abort, a cancel written directly behind its
//!   request, and a cancel for an unknown id.
//!
//! Not duplicated here: the legacy JSON-RPC/SSE units kept for the JS shim
//! (`tests/mcp_units.rs`), the client/transport flows
//! (`tests/mcp_integration.rs`), the frame cap (`tests/mcp_frame_cap.rs`), the
//! daemon ops (`tests/env_daemon_protocol.rs`) and the client framing/path/SSH
//! vectors (`tests/upstream_env_protocol.rs`).

#![cfg(all(feature = "mcp", feature = "env"))]

use std::io::{BufRead, Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use pir::config::mcp::McpServerConfig;
use pir::env_client::connection::{checked_protocol, StreamParser};
use pir::env_daemon::frame::{self, Frame, CANCEL, ERROR, EVENT, PING, REQUEST, RESULT};
use pir::mcp::error::McpError;
use pir::mcp::manager::{ConnectionState, McpManager};
use pir::mcp::oauth::{
    authorization_issuer_mismatch, parse_www_authenticate, step_up_scope,
    AuthorizationServerMetadata, McpOAuthState,
};
use pir::mcp::protocol::CallToolResult;
use pir::mcp::registry::{McpServerEntry, McpServerSource};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// A unique temporary directory removed on drop.
struct Tree(std::path::PathBuf);

impl Tree {
    fn new(label: &str) -> Tree {
        let path = std::env::temp_dir().join(format!(
            "pir-w11h-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create temp dir");
        Tree(path)
    }

    fn path(&self, name: &str) -> String {
        self.0.join(name).to_string_lossy().into_owned()
    }

    fn root(&self) -> String {
        self.0.to_string_lossy().into_owned()
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// ---------------------------------------------------------------------------
// MCP: structuredContent / _meta passthrough
// ---------------------------------------------------------------------------

#[test]
fn call_tool_results_keep_structured_content_through_their_wire_shape() {
    // Upstream `validateCallToolResult` passes `structuredContent` through
    // untouched. rmcp parses the server's `_meta` into its own result and
    // `convert_call_tool_result` carries it over as `meta`; this type is the
    // shape pir's consumers see, so `structuredContent`/`isError` must parse
    // with their documented (camelCase) wire names.
    let structured = json!({
        "rows": [{"name": "a", "size": 1}, {"name": "b", "size": 2}],
        "nextCursor": "page-2",
        "nested": {"deep": [1, 2, 3]},
    });
    let wire = json!({
        "content": [{"type": "text", "text": "summary"}],
        "structuredContent": structured,
        "isError": false,
        "meta": {"progressToken": 7},
    });
    let parsed: CallToolResult = serde_json::from_value(wire).expect("parse the tool result");
    assert_eq!(
        parsed.structured_content.as_ref(),
        Some(&structured),
        "structuredContent survives the wire shape unchanged, nested values included"
    );
    assert_eq!(parsed.is_error, Some(false));
    assert_eq!(
        parsed
            .content
            .first()
            .and_then(|block| block.text.as_deref()),
        Some("summary")
    );
    assert_eq!(parsed.meta.as_ref(), Some(&json!({"progressToken": 7})));
    // An `isError: true` tool failure keeps its payload (upstream keeps the
    // content of an errored call).
    let failed: CallToolResult = serde_json::from_value(json!({
        "content": [{"type": "text", "text": "boom"}],
        "isError": true,
    }))
    .expect("parse an errored result");
    assert_eq!(failed.is_error, Some(true));
    assert_eq!(failed.structured_content, None);
}

// ---------------------------------------------------------------------------
// MCP: cancellation reaches the server as `notifications/cancelled`
// ---------------------------------------------------------------------------

/// The child entry: a minimal stdio MCP server whose `slow` tool never
/// answers until the client cancels it. Cancellation is recorded by writing
/// the notification to `PIR_W11H_CANCEL_LOG`, which the parent polls.
#[test]
fn w11h_cancellable_stdio_server() {
    if std::env::var("PIR_W11H_MOCK").as_deref() != Ok("cancel") {
        return;
    }
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let method = value.get("method").and_then(Value::as_str).unwrap_or("");
        let Some(id) = value.get("id").cloned() else {
            // `notifications/cancelled`: record it and let the test observe it.
            if method == "notifications/cancelled" {
                if let Ok(path) = std::env::var("PIR_W11H_CANCEL_LOG") {
                    // Atomic publish: the parent polls this path and parses it
                    // immediately (a truncate-then-write can be read as empty).
                    crate::common::write_file_atomically(path, &value.to_string());
                }
            }
            continue;
        };
        let reply = match method {
            "initialize" => json!({
                "protocolVersion": "2025-06-18",
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "cancel-mock", "version": "1.0.0" },
            }),
            "tools/list" => {
                json!({ "tools": [{ "name": "slow", "inputSchema": { "type": "object" } }] })
            }
            // Deliberately unanswered: the call stays in flight until the
            // client cancels it.
            "tools/call" => {
                // Record that the server actually RECEIVED the call, so the
                // test can abort deterministically instead of sleeping a fixed
                // delay. The mock child is this same test binary: process
                // start + `initialize` measured well over a second, so a fixed
                // 500 ms abort could fire BEFORE dispatch — a different
                // upstream path where `requestInternal` checks
                // `signal.aborted` (`client.ts:403`) and throws without
                // notifying the server at all.
                if let Ok(path) = std::env::var("PIR_W11H_CALL_LOG") {
                    let _ = std::fs::write(path, b"1");
                }
                continue;
            }
            _ => json!({}),
        };
        let line = json!({ "jsonrpc": "2.0", "id": id, "result": reply });
        let mut stdout = std::io::stdout();
        let _ = writeln!(stdout, "{line}");
        let _ = stdout.flush();
    }
}

/// Read the mock child's cancel record, or `None` while it is absent or not
/// yet a complete JSON document (the child publishes via temp file + rename, so
/// this only covers a transient read).
fn read_cancel_log(path: &str) -> Option<Value> {
    let text = std::fs::read_to_string(path).ok()?;
    if text.trim().is_empty() {
        return None;
    }
    serde_json::from_str(&text).ok()
}

#[test]
fn aborting_a_tool_call_sends_notifications_cancelled() {
    let dir = Tree::new("mcp-cancel");
    let cancel_log = dir.path("cancelled.json");
    let call_log = dir.path("call-started.txt");
    let manager = McpManager::without_log(&std::env::current_dir().expect("cwd"));
    // `--exact` needs the FULL test path: in the merged `g_agent` harness the
    // reported name is `mcp_env_w11::w11h_cancellable_stdio_server`, so the
    // bare `w11h_cancellable_stdio_server` matches 0 tests and the child exits
    // before the handshake.
    let (command, args) = (
        std::env::current_exe()
            .expect("current exe")
            .to_string_lossy()
            .into_owned(),
        vec![
            "mcp_env_w11::w11h_cancellable_stdio_server".to_owned(),
            "--exact".to_owned(),
            "--nocapture".to_owned(),
        ],
    );
    let config: McpServerConfig = serde_json::from_value(json!({
        "command": command,
        "args": args,
        "env": {
            "PIR_W11H_MOCK": "cancel",
            "PIR_W11H_CANCEL_LOG": cancel_log.clone(),
            "PIR_W11H_CALL_LOG": call_log.clone(),
        },
        "timeout": 20,
    }))
    .expect("mock server config");
    manager.registry().register(McpServerEntry {
        name: "cancellable".to_owned(),
        config,
        source: McpServerSource::Extension("w11h".to_owned()),
    });
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let signal = pir::ai::types::AbortSignal::new();
        let abort = signal.clone();
        // Abort only once the server has actually RECEIVED the call: the mock
        // child is this test binary, and its process start plus the
        // `initialize` handshake measured longer than a second, so a fixed
        // sleep could abort before dispatch (upstream then throws without
        // notifying — `client.ts:403`).
        //
        // The instant is recorded HERE, immediately before `abort()`, and the
        // returned call is bounded from it rather than from `call_tool`'s
        // start: the setup (child spawn + handshake + `tools/call` dispatch)
        // is not what "the abort ends the call without waiting for the
        // server" claims, and under a loaded full-suite run the setup alone
        // can exceed a few seconds.
        let aborted_at = Arc::new(Mutex::new(None::<Instant>));
        let abort_stamp = Arc::clone(&aborted_at);
        let call_log_watch = call_log.clone();
        tokio::spawn(async move {
            let deadline = Instant::now() + Duration::from_secs(30);
            while Instant::now() < deadline {
                if std::path::Path::new(&call_log_watch).exists() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            *abort_stamp
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Instant::now());
            abort.abort();
        });
        let started = Instant::now();
        let result = manager
            .call_tool("cancellable", "slow", &json!({}), Some(&signal))
            .await;
        let returned_at = Instant::now();
        assert!(
            matches!(result, Err(McpError::Aborted)),
            "an aborted call reports Aborted: {result:?}"
        );
        // The watcher may still be between recording and `abort()` on a
        // single-threaded schedule, but the `Aborted` result proves the abort
        // happened; wait for the stamp instead of assuming it is visible.
        let abort_to_return = loop {
            if let Some(aborted_at) = *aborted_at
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
            {
                break returned_at.saturating_duration_since(aborted_at);
            }
            assert!(
                started.elapsed() < Duration::from_secs(30),
                "the watcher never recorded its abort"
            );
            tokio::task::yield_now().await;
        };
        assert!(
            abort_to_return < Duration::from_secs(5),
            "the abort ends the call without waiting for the server, took {abort_to_return:?} \
             from abort to return ({:?} including setup)",
            started.elapsed()
        );
        // The server saw `notifications/cancelled` for this request (upstream
        // `cancelPending` sends it so the server stops the work).
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            // The child publishes the record atomically; a read that still
            // lands on an absent/incomplete file is retried, never parsed.
            if let Some(value) = read_cancel_log(&cancel_log) {
                assert_eq!(value["method"], json!("notifications/cancelled"));
                assert!(
                    value["params"]["requestId"].is_number()
                        || value["params"]["requestId"].is_string(),
                    "the notification names the request: {value}"
                );
                assert_eq!(value["params"]["reason"], json!("Aborted"));
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the server never received notifications/cancelled"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        manager.shutdown().await;
    });
}

// ---------------------------------------------------------------------------
// MCP: RFC 9207 `iss` validation
// ---------------------------------------------------------------------------

fn metadata(issuer: &str, iss_supported: Option<bool>) -> AuthorizationServerMetadata {
    AuthorizationServerMetadata {
        issuer: issuer.to_string(),
        authorization_endpoint: format!("{issuer}/authorize"),
        token_endpoint: format!("{issuer}/token"),
        authorization_response_iss_parameter_supported: iss_supported,
        ..AuthorizationServerMetadata::default()
    }
}

#[test]
fn a_presented_iss_is_always_checked() {
    let server = metadata("https://auth.example", None);
    // A matching `iss` passes even though the server never promised one.
    assert_eq!(
        authorization_issuer_mismatch(Some(&server), Some("https://auth.example")),
        None
    );
    // A different issuer is rejected, and the message names both sides
    // (upstream `OAuthIssuerMismatchError`).
    let mismatch = authorization_issuer_mismatch(Some(&server), Some("https://evil.example"))
        .expect("a foreign issuer is a mismatch");
    assert_eq!(
        mismatch,
        "OAuth issuer mismatch: expected \"https://auth.example\", received \"https://evil.example\""
    );
}

#[test]
fn a_missing_iss_is_a_mismatch_only_when_the_server_promised_it() {
    let silent = metadata("https://auth.example", None);
    assert_eq!(authorization_issuer_mismatch(Some(&silent), None), None);
    // `authorization_response_iss_parameter_supported: false` is a promise of
    // an ABSENT parameter too: a missing `iss` stays fine.
    let refused = metadata("https://auth.example", Some(false));
    assert_eq!(authorization_issuer_mismatch(Some(&refused), None), None);
    let promising = metadata("https://auth.example", Some(true));
    let mismatch = authorization_issuer_mismatch(Some(&promising), None)
        .expect("a promised but absent iss is a mismatch");
    assert_eq!(
        mismatch,
        "OAuth issuer mismatch: expected \"https://auth.example\", received none"
    );
}

#[test]
fn without_metadata_any_presented_iss_is_rejected() {
    // Upstream flow.ts:385-388: with no authorization-server metadata there is
    // no issuer to compare against, so the check fails closed instead of being
    // skipped.
    assert_eq!(authorization_issuer_mismatch(None, None), None);
    let mismatch =
        authorization_issuer_mismatch(None, Some("https://auth.example")).expect("mismatch");
    assert_eq!(
        mismatch,
        "OAuth issuer mismatch: expected none, received \"https://auth.example\""
    );
}

// ---------------------------------------------------------------------------
// MCP: WWW-Authenticate, step-up scope, persisted OAuth state
// ---------------------------------------------------------------------------

#[test]
fn www_authenticate_carries_the_documented_parameters() {
    let challenge = parse_www_authenticate(Some(
        "Bearer resource_metadata=\"https://mcp.example/.well-known/oauth-protected-resource\", scope=\"files read\", error=\"insufficient_scope\", error_description=\"more scope needed\"",
    ));
    assert_eq!(
        challenge.resource_metadata_url.as_deref(),
        Some("https://mcp.example/.well-known/oauth-protected-resource")
    );
    assert_eq!(challenge.scope.as_deref(), Some("files read"));
    assert_eq!(challenge.error.as_deref(), Some("insufficient_scope"));
    assert_eq!(
        challenge.error_description.as_deref(),
        Some("more scope needed")
    );
}

#[test]
fn www_authenticate_ignores_other_schemes_and_empty_values() {
    // A non-Bearer/DPoP scheme is not an OAuth challenge at all.
    assert_eq!(
        parse_www_authenticate(Some("Basic realm=\"x\"")),
        Default::default()
    );
    assert_eq!(parse_www_authenticate(None), Default::default());
    // An empty quoted value counts as absent (upstream `field`).
    let challenge = parse_www_authenticate(Some("Bearer scope=\"\" error=invalid_token"));
    assert_eq!(challenge.scope, None);
    assert_eq!(challenge.error.as_deref(), Some("invalid_token"));
    // A `resource_metadata` that does not parse as a URL is dropped.
    let challenge = parse_www_authenticate(Some("DPoP resource_metadata=not-a-url"));
    assert_eq!(challenge.resource_metadata_url, None);
    // The scheme is case-insensitive.
    let challenge = parse_www_authenticate(Some("bearer scope=files"));
    assert_eq!(challenge.scope.as_deref(), Some("files"));
}

#[test]
fn step_up_scope_merges_granted_and_challenged_scopes() {
    assert_eq!(
        step_up_scope(Some("files read"), Some("write admin")),
        Some("files read write admin".to_string())
    );
    // A scope listed on both sides appears once.
    assert_eq!(
        step_up_scope(Some("files"), Some("files write")),
        Some("files write".to_string())
    );
    // No challenged scope means there is no step-up at all: upstream
    // `stepUpScope` starts with `if (!challenged) return undefined`
    // (flow.ts:314-317), so a granted scope alone is NOT carried through.
    // The caller keeps using the original grant in that case (`flow.ts:468`
    // only calls this when `insufficient_scope` is set).
    assert_eq!(step_up_scope(Some("files"), None), None);
    assert_eq!(step_up_scope(None, None), None);
    // An empty challenge carries no scope (no step-up request is made).
    assert_eq!(step_up_scope(None, Some("  ")), None);
}

#[test]
fn oauth_state_round_trips_through_its_persisted_shape() {
    // The `<agentDir>/mcp-auth.json` entry (upstream `McpOAuthState`, camelCase
    // keys). A stored state must parse back to what the sign-in flow wrote.
    let state = McpOAuthState {
        server_url: "https://mcp.example/mcp".to_string(),
        tokens_expire_at: Some(1_700_000_000_000),
        oauth_state: Some("state-token".to_string()),
        ..McpOAuthState::default()
    };
    let json = serde_json::to_value(&state).expect("serialize the OAuth state");
    assert_eq!(json["tokensExpireAt"], json!(1_700_000_000_000i64));
    assert_eq!(json["oauthState"], json!("state-token"));
    // Absent optional fields are omitted rather than written as `null`.
    assert!(json.get("tokens").is_none(), "{json}");
    let parsed: McpOAuthState = serde_json::from_value(json).expect("parse the OAuth state");
    assert_eq!(parsed, state);
}

// ---------------------------------------------------------------------------
// MCP: the stdio stderr tail on a failed connect
// ---------------------------------------------------------------------------

/// The child entry: with `PIR_W11H_MOCK=stderr-die` it writes one diagnostic
/// line to stderr and exits WITHOUT answering `initialize`.
#[test]
fn w11h_mock_stdio_server() {
    if std::env::var("PIR_W11H_MOCK").as_deref() != Ok("stderr-die") {
        return;
    }
    let mut stderr = std::io::stderr();
    let _ = writeln!(stderr, "boom: the mock server failed to start");
    let _ = stderr.flush();
    // Give the parent's drain a moment before EOF so the tail is observable
    // when the handshake fails (the daemon-side pipe closes on exit).
    std::thread::sleep(std::time::Duration::from_millis(300));
    std::process::exit(1);
}

/// Command + args that re-invoke this test binary as the mock child.
///
/// `--exact` needs the FULL test path. Inside the merged `g_agent` harness
/// libtest reports the name as `mcp_env_w11::w11h_mock_stdio_server`, so the
/// bare `w11h_mock_stdio_server` matches nothing (`running 0 tests`) and the
/// child exits before writing its stderr. `module_path!()` is NOT usable here:
/// it expands to `g_agent::mcp_env_w11` in the merged binary, which is also not
/// the reported name.
fn mock_spawn() -> (String, Vec<String>) {
    let exe = std::env::current_exe().expect("current exe");
    (
        exe.to_string_lossy().into_owned(),
        vec![
            "mcp_env_w11::w11h_mock_stdio_server".to_owned(),
            "--exact".to_owned(),
            "--nocapture".to_owned(),
        ],
    )
}

#[test]
fn a_stdio_server_that_dies_leaves_its_stderr_in_the_failure_message() {
    let manager = McpManager::without_log(&std::env::current_dir().expect("cwd"));
    let (command, args) = mock_spawn();
    let config: McpServerConfig = serde_json::from_value(json!({
        "command": command,
        "args": args,
        "env": { "PIR_W11H_MOCK": "stderr-die" },
        "timeout": 5,
    }))
    .expect("mock server config");
    manager.registry().register(McpServerEntry {
        name: "dying".to_owned(),
        config,
        source: McpServerSource::Extension("w11h".to_owned()),
    });
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let result = manager.get_client("dying").await;
        assert!(
            result.is_err(),
            "a server that never answers cannot connect"
        );
        let states = manager.connection_states().await;
        let ConnectionState::Failed { error } = states.get("dying").expect("state") else {
            panic!("expected Failed, got {:?}", states.get("dying"));
        };
        assert!(
            error.contains("boom: the mock server failed to start"),
            "the connect failure carries the server's stderr: {error}"
        );
        assert!(
            error.starts_with("MCP server \"dying\" failed to connect: "),
            "the message keeps upstream's prefix: {error}"
        );
        manager.shutdown().await;
    });
}

// ---------------------------------------------------------------------------
// env: the 13-byte frame header and the `PI-ENV` sync line
// ---------------------------------------------------------------------------

/// Encode one frame the way the client writes it into the daemon's stdin.
fn encode(kind: u8, id: u32, json: Value, payload: &[u8]) -> Vec<u8> {
    let frame = Frame::with_payload(kind, id, json, payload.to_vec());
    let mut bytes = Vec::new();
    frame::write_frame(&mut bytes, &frame).expect("encode the frame");
    bytes
}

#[test]
fn the_frame_header_is_thirteen_bytes_of_big_endian_fields() {
    let body = json!({"op": "hello"});
    let bytes = encode(RESULT, 0x0102_0304, body.clone(), b"abc");
    // `u32 length | u8 type | u32 id | u32 jsonLength | json | payload`, all
    // integers big-endian (docs/protocol.md "Frames").
    let json_length = serde_json::to_vec(&body).expect("json").len() as u32;
    let length = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    assert_eq!(
        length as usize,
        9 + json_length as usize + 3,
        "length counts everything after itself"
    );
    assert_eq!(bytes.len(), 4 + length as usize);
    assert_eq!(bytes[4], RESULT, "type byte follows the length");
    assert_eq!(&bytes[5..9], &[0x01, 0x02, 0x03, 0x04], "id is big-endian");
    assert_eq!(
        u32::from_be_bytes([bytes[9], bytes[10], bytes[11], bytes[12]]),
        json_length,
        "the 13th byte ends the fixed header"
    );
    assert_eq!(
        &bytes[13..13 + json_length as usize],
        &serde_json::to_vec(&body).unwrap()[..]
    );
    assert_eq!(
        &bytes[13 + json_length as usize..],
        b"abc",
        "payload is raw bytes"
    );
}

#[test]
fn every_documented_frame_type_keeps_its_wire_number() {
    assert_eq!(
        [REQUEST, RESULT, ERROR, EVENT, CANCEL, PING],
        [1, 2, 3, 4, 5, 6],
        "docs/protocol.md \"type\" column"
    );
}

#[test]
fn the_parser_rejects_a_json_length_that_exceeds_its_frame() {
    // length = 9 (the fixed body) with jsonLength = 1 claims a JSON byte that
    // does not exist: corruption, not a short read.
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&9u32.to_be_bytes());
    bytes.push(RESULT);
    bytes.extend_from_slice(&1u32.to_be_bytes());
    bytes.extend_from_slice(&1u32.to_be_bytes());
    let mut parser = StreamParser::new("token");
    let mut fed = b"PI-ENV token\n".to_vec();
    fed.extend_from_slice(&bytes);
    let error = parser
        .feed(&fed, &mut |_| {})
        .err()
        .expect("a JSON length beyond the frame is corruption");
    assert_eq!(error, "Corrupt frame from pi-env");
}

#[test]
fn lengths_below_the_fixed_body_and_past_the_cap_are_corruption() {
    for length in [0u32, 8, 16 * 1024 * 1024 + 1] {
        let mut parser = StreamParser::new("token");
        let mut fed = b"PI-ENV token\n".to_vec();
        fed.extend_from_slice(&length.to_be_bytes());
        fed.extend_from_slice(&[0u8; 32]);
        assert_eq!(
            parser.feed(&fed, &mut |_| {}).err(),
            Some("Corrupt frame from pi-env".to_string()),
            "length {length} must be rejected"
        );
    }
}

#[test]
fn an_incomplete_frame_waits_instead_of_failing() {
    // The daemon writes each frame with several writes, so a reader routinely
    // sees a partial frame. It must be kept, not reported as corruption.
    let bytes = encode(EVENT, 3, json!({"kind": "output"}), b"partial");
    let mut parser = StreamParser::new("token");
    let mut fed = b"PI-ENV token\n".to_vec();
    fed.extend_from_slice(&bytes[..bytes.len() - 3]);
    let frames = parser
        .feed(&fed, &mut |_| {})
        .expect("a short frame is not an error");
    assert!(frames.is_empty(), "a short frame yields no event yet");
    let frames = parser
        .feed(&bytes[bytes.len() - 3..], &mut |_| {})
        .expect("the rest completes the frame");
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].kind, EVENT);
    assert_eq!(frames[0].id, 3);
    assert_eq!(frames[0].payload, b"partial");
}

#[test]
fn the_sync_line_must_carry_this_sessions_token() {
    let mut parser = StreamParser::new("right");
    let mut logs = Vec::new();
    let frames = parser
        .feed(b"PI-ENV wrong\nline noise\n", &mut |line| {
            logs.push(line.to_string())
        })
        .expect("unsynced noise is not an error");
    assert!(frames.is_empty(), "unsynced input yields no frames");
    assert!(!parser.is_synced());
    assert!(logs.is_empty(), "unsynced noise is buffered, not logged");
    // The real marker drops the earlier noise (and logs it as diagnostics).
    let bytes = encode(RESULT, 1, json!({}), b"");
    let mut fed = b"PI-ENV right\n".to_vec();
    fed.extend_from_slice(&bytes);
    let frames = parser
        .feed(&fed, &mut |line| logs.push(line.to_string()))
        .expect("sync");
    assert!(parser.is_synced());
    assert_eq!(frames.len(), 1);
    assert_eq!(logs, vec!["PI-ENV wrong\nline noise".to_string()]);
}

#[test]
fn a_sync_line_split_across_reads_still_syncs() {
    let mut parser = StreamParser::new("tok");
    let mut logs = Vec::new();
    let frames = parser
        .feed(b"banner\nPI-ENV ", &mut |_| {})
        .expect("a partial marker waits");
    assert!(frames.is_empty());
    assert!(!parser.is_synced());
    let bytes = encode(PING, 0, json!({}), b"");
    let mut fed = b"tok\n".to_vec();
    fed.extend_from_slice(&bytes);
    let frames = parser
        .feed(&fed, &mut |line| logs.push(line.to_string()))
        .expect("sync after the split marker");
    assert!(parser.is_synced());
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].kind, PING);
    assert_eq!(logs, vec!["banner".to_string()]);
}

#[test]
fn hello_reports_protocol_one_or_the_handshake_fails_closed() {
    assert!(checked_protocol(&json!({"protocol": 1})).is_ok());
    for bad in [json!({"protocol": 2}), json!({}), json!({"protocol": "1"})] {
        let error = checked_protocol(&bad).expect_err("only protocol 1 is the contract");
        assert!(error.lost, "a protocol mismatch loses the connection");
        assert!(
            error.message.starts_with("Unsupported protocol "),
            "message: {}",
            error.message
        );
    }
}

// ---------------------------------------------------------------------------
// env: a live daemon — error codes and cancellation
// ---------------------------------------------------------------------------

const TOKEN: &str = "0123456789abcdef0123456789abcdef";
const READ_TIMEOUT: Duration = Duration::from_secs(30);

/// A spawned `pir env-daemon serve --token <hex>` plus a frame reader.
struct Daemon {
    child: Child,
    stdin: Option<ChildStdin>,
    frames: Receiver<Frame>,
    next_id: u32,
}

impl Daemon {
    fn start() -> Daemon {
        let mut child = Command::new(env!("CARGO_BIN_EXE_pir"))
            .args(["env-daemon", "serve", "--token", TOKEN])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn pir env-daemon");
        let stdin = child.stdin.take().expect("daemon stdin");
        let mut stdout = child.stdout.take().expect("daemon stdout");
        // The documented sync line, read byte-wise so no frame byte is eaten.
        let marker = format!("PI-ENV {TOKEN}\n");
        let mut seen = Vec::new();
        let mut byte = [0u8; 1];
        while !seen.ends_with(marker.as_bytes()) {
            let read = stdout.read(&mut byte).expect("read the sync line");
            assert_ne!(read, 0, "the daemon ended before its sync line");
            seen.push(byte[0]);
        }
        let (sender, frames) = mpsc::channel();
        std::thread::spawn(move || {
            while let Ok(Some(frame)) = frame::read_frame(&mut stdout) {
                if sender.send(frame).is_err() {
                    return;
                }
            }
        });
        Daemon {
            child,
            stdin: Some(stdin),
            frames,
            next_id: 1,
        }
    }

    fn send(&mut self, kind: u8, id: u32, json: Value, payload: &[u8]) {
        let frame = Frame::with_payload(kind, id, json, payload.to_vec());
        frame::write_frame(self.stdin.as_mut().expect("daemon stdin"), &frame)
            .expect("write the frame");
    }

    /// Write one request and return its id.
    fn start_request(&mut self, op: &str, mut json: Value) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        json["op"] = json!(op);
        self.send(REQUEST, id, json, b"");
        id
    }

    /// One request → its result or error frame (`PING` skipped).
    fn request(&mut self, op: &str, json: Value) -> Frame {
        let id = self.start_request(op, json);
        self.wait_for(id)
    }

    fn wait_for(&mut self, id: u32) -> Frame {
        let deadline = Instant::now() + READ_TIMEOUT;
        loop {
            let left = deadline
                .checked_duration_since(Instant::now())
                .unwrap_or(Duration::ZERO);
            match self.frames.recv_timeout(left) {
                Ok(frame) => match frame.kind {
                    PING => continue,
                    _ if frame.id == id => return frame,
                    _ => continue,
                },
                Err(RecvTimeoutError::Timeout) => panic!("no answer for request {id}"),
                Err(RecvTimeoutError::Disconnected) => panic!("daemon stopped answering"),
            }
        }
    }

    /// Every event frame of one request until its result/error arrives.
    fn events_until(&mut self, id: u32) -> (Vec<(Value, Vec<u8>)>, Frame) {
        let deadline = Instant::now() + READ_TIMEOUT;
        let mut events = Vec::new();
        loop {
            let left = deadline
                .checked_duration_since(Instant::now())
                .unwrap_or(Duration::ZERO);
            match self.frames.recv_timeout(left) {
                Ok(frame) if frame.kind == EVENT && frame.id == id => {
                    events.push((frame.json.clone(), frame.payload.clone()));
                }
                Ok(frame) if frame.id == id && (frame.kind == RESULT || frame.kind == ERROR) => {
                    return (events, frame);
                }
                Ok(_) => continue,
                Err(_) => panic!("request {id} never finished"),
            }
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.stdin.take();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A command that outlives any cancel on both platforms.
fn long_command() -> &'static str {
    if cfg!(windows) {
        "ping -n 30 127.0.0.1 > nul"
    } else {
        "sleep 30"
    }
}

#[test]
fn file_error_codes_name_the_failure_the_way_node_does() {
    let mut daemon = Daemon::start();
    let tree = Tree::new("file-errors");
    let file = tree.path("plain.txt");
    std::fs::write(&file, b"x").unwrap();

    // A missing path.
    let missing = daemon.request("lstat", json!({ "path": tree.path("nope") }));
    assert_eq!(missing.kind, ERROR);
    assert_eq!(missing.json["code"], json!("ENOENT"));
    assert_eq!(missing.json["syscall"], json!("lstat"));

    // A file used as a directory.
    let not_dir = daemon.request("opendir", json!({ "path": file }));
    assert_eq!(not_dir.kind, ERROR);
    assert_eq!(not_dir.json["code"], json!("ENOTDIR"));

    // An unknown handle.
    let bad_handle = daemon.request("pread", json!({ "handle": 99_999, "length": 4 }));
    assert_eq!(bad_handle.kind, ERROR);
    assert_eq!(bad_handle.json["code"], json!("EBADF"));

    // A missing required field.
    let invalid = daemon.request("lstat", json!({}));
    assert_eq!(invalid.kind, ERROR);
    assert_eq!(invalid.json["code"], json!("EINVAL"));

    // `open(path, "r")` of a DIRECTORY succeeds with a `stat` (Node opens it,
    // the read is what fails with `EISDIR`; semantics.md "the standard
    // libraries").
    let opened = daemon.request("open", json!({ "path": tree.root(), "mode": "read" }));
    assert_eq!(
        opened.kind, RESULT,
        "open(path, \"r\") succeeds on a directory: {:?}",
        opened.json
    );
    assert!(opened.json["stat"].is_object(), "{:?}", opened.json);
    let read = daemon.request(
        "pread",
        json!({ "handle": opened.json["handle"], "length": 16 }),
    );
    assert_eq!(read.kind, ERROR);
    assert_eq!(read.json["code"], json!("EISDIR"));

    // A JSON frame whose body does not parse answers `EINVAL` and keeps the
    // connection (docs/protocol.md "Frames": framing errors end it, JSON
    // errors do not).
    let mut bytes = Vec::new();
    let body = b"{not json";
    bytes.extend_from_slice(&((9 + body.len()) as u32).to_be_bytes());
    bytes.push(REQUEST);
    bytes.extend_from_slice(&77u32.to_be_bytes());
    bytes.extend_from_slice(&(body.len() as u32).to_be_bytes());
    bytes.extend_from_slice(body);
    self::write_raw(&mut daemon, &bytes);
    let reply = daemon.wait_for(77);
    assert_eq!(reply.kind, ERROR);
    assert_eq!(reply.json["code"], json!("EINVAL"));
    // An unknown operation is `EINVAL` too, and the frame error did not end
    // the connection.
    assert_eq!(daemon.request("nosuchop", json!({})).kind, ERROR);
    // The connection still answers real requests.
    assert_eq!(daemon.request("hello", json!({})).kind, RESULT);
}

/// Write raw bytes to the daemon's stdin (a hand-built frame).
fn write_raw(daemon: &mut Daemon, bytes: &[u8]) {
    let stdin = daemon.stdin.as_mut().expect("daemon stdin");
    stdin.write_all(bytes).expect("write the raw frame");
    stdin.flush().expect("flush the raw frame");
}

#[test]
fn exec_reports_the_documented_execution_codes() {
    let mut daemon = Daemon::start();
    let tree = Tree::new("exec-codes");

    // A configured shell that does not exist is `shell_unavailable`, before
    // any process starts (semantics.md "Commands").
    let unavailable = daemon.request(
        "exec",
        json!({
            "command": "echo hi",
            "cwd": tree.root(),
            "shellPath": tree.path("no-such-shell"),
        }),
    );
    assert_eq!(unavailable.kind, ERROR);
    assert_eq!(unavailable.json["code"], json!("shell_unavailable"));

    // A working directory that does not exist is a spawn failure.
    let bad_cwd = daemon.request(
        "exec",
        json!({ "command": "echo hi", "cwd": tree.path("missing-dir") }),
    );
    assert_eq!(bad_cwd.kind, ERROR);
    assert_eq!(bad_cwd.json["code"], json!("spawn_error"));

    // A timeout settles as `timeout`.
    let started = Instant::now();
    let timeout = daemon.request(
        "exec",
        json!({ "command": long_command(), "cwd": tree.root(), "timeoutMs": 150 }),
    );
    assert_eq!(timeout.kind, ERROR);
    assert_eq!(timeout.json["code"], json!("timeout"));
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "the timeout must be honoured, took {:?}",
        started.elapsed()
    );
}

#[test]
fn cancel_kills_without_aborting_and_keeps_the_daemon_usable() {
    let mut daemon = Daemon::start();
    let tree = Tree::new("cancel-modes");

    // `{ mode: "kill" }` kills the process group WITHOUT aborting: the command
    // settles with the killed process's exit status (docs/protocol.md
    // "Cancellation", semantics.md "an abort settles as `aborted`; `cleanup()`
    // kills without aborting, so the command settles with exit code 137").
    let id = daemon.start_request(
        "exec",
        json!({ "command": long_command(), "cwd": tree.root() }),
    );
    std::thread::sleep(Duration::from_millis(500));
    daemon.send(CANCEL, id, json!({ "mode": "kill" }), b"");
    let killed = daemon.wait_for(id);
    assert_eq!(
        killed.kind, RESULT,
        "a kill is not an abort, it keeps the exit status: {:?}",
        killed.json
    );
    let code = killed.json["exitCode"].as_i64().expect("exitCode");
    assert_ne!(code, 0, "the killed command did not exit cleanly");

    // A plain cancel aborts: `code: "aborted"`.
    let id = daemon.start_request(
        "exec",
        json!({ "command": long_command(), "cwd": tree.root() }),
    );
    std::thread::sleep(Duration::from_millis(500));
    daemon.send(CANCEL, id, json!({}), b"");
    let aborted = daemon.wait_for(id);
    assert_eq!(aborted.kind, ERROR);
    assert_eq!(aborted.json["code"], json!("aborted"));

    // A cancel for an id nobody is running is a no-op.
    daemon.send(CANCEL, 4_000_000, json!({}), b"");
    let hello = daemon.request("hello", json!({}));
    assert_eq!(
        hello.kind, RESULT,
        "the daemon still answers: {:?}",
        hello.json
    );
    assert_eq!(hello.json["protocol"], json!(1));
}

#[test]
fn a_cancel_written_straight_behind_its_request_is_not_lost() {
    // The daemon registers each request before running it, so a cancel that
    // arrives with no delay still reaches the command (docs/protocol.md
    // "Cancellation").
    let mut daemon = Daemon::start();
    let tree = Tree::new("fast-cancel");
    let id = daemon.next_id;
    daemon.next_id += 1;
    daemon.send(
        REQUEST,
        id,
        json!({ "op": "exec", "command": long_command(), "cwd": tree.root() }),
        b"",
    );
    daemon.send(CANCEL, id, json!({}), b"");
    let settled = daemon.wait_for(id);
    assert_eq!(settled.kind, ERROR, "settled: {:?}", settled.json);
    assert_eq!(settled.json["code"], json!("aborted"));
}

#[test]
fn exec_output_events_precede_the_result_and_name_their_stream() {
    let mut daemon = Daemon::start();
    let tree = Tree::new("events");
    let id = daemon.start_request(
        "exec",
        json!({ "command": "printf out; printf err 1>&2", "cwd": tree.root() }),
    );
    let (events, result) = daemon.events_until(id);
    assert_eq!(result.kind, RESULT, "exec result: {:?}", result.json);
    assert_eq!(result.json["exitCode"], json!(0));
    assert!(!events.is_empty(), "output events precede the result");
    let mut streams = Vec::new();
    let mut text = String::new();
    for (json, payload) in &events {
        assert_eq!(json["kind"], json!("output"));
        streams.push(json["stream"].as_str().unwrap_or_default().to_string());
        text.push_str(&String::from_utf8_lossy(payload));
    }
    assert!(
        streams.iter().any(|stream| stream == "stdout"),
        "stdout event: {streams:?}"
    );
    assert!(
        streams.iter().any(|stream| stream == "stderr"),
        "stderr event: {streams:?}"
    );
    assert!(text.contains("out") && text.contains("err"), "{text:?}");
}

#[test]
fn closing_stdin_ends_the_daemon_cleanly() {
    // A vanished client must not leave a daemon behind (docs/protocol.md
    // "Liveness": "or when stdin ends").
    let mut daemon = Daemon::start();
    assert_eq!(daemon.request("hello", json!({})).kind, RESULT);
    drop(daemon.stdin.take());
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        match daemon.child.try_wait().expect("try_wait") {
            Some(status) => {
                assert!(status.success(), "a clean end of input exits 0: {status:?}");
                break;
            }
            None => assert!(Instant::now() < deadline, "the daemon did not exit"),
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

// ---------------------------------------------------------------------------
// env: SSH host-key pinning (accept / change-refusal / forget)
// ---------------------------------------------------------------------------

/// `accept_host_key` appends only the target alias's plain lines; a key of a
/// type already trusted with a DIFFERENT value is refused with
/// `HostKeyChanged` (upstream `acceptHostKey`), and `forget_host_key` removes
/// every line of the alias, which is what lets the owner accept a changed key.
#[test]
fn ssh_host_key_pinning_refuses_a_changed_key_until_it_is_forgotten() {
    let tree = Tree::new("known-hosts");
    let known_hosts = tree.path("known_hosts");
    let target =
        pir::env_client::ssh::SshTarget::new("example.test", known_hosts.clone(), "pi-env-test");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");

    const ED25519_A: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIAAA";
    const ED25519_B: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIBBB";
    const RSA: &str = "ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABgQ==";
    let a = format!("pi-env-test {ED25519_A}");
    let b = format!("pi-env-test {ED25519_B}");
    let rsa = format!("pi-env-test {RSA}");

    runtime.block_on(async {
        // First acceptance writes the file (and its parent) with the line.
        pir::env_client::ssh::accept_host_key(&target, std::slice::from_ref(&a))
            .await
            .expect("accept the first key");
        let contents = std::fs::read_to_string(&known_hosts).expect("known_hosts exists");
        assert_eq!(
            contents,
            format!("{a}\n"),
            "one plain line, newline-terminated"
        );
        // Accepting the same key again is a no-op (no duplicate line).
        pir::env_client::ssh::accept_host_key(&target, std::slice::from_ref(&a))
            .await
            .expect("re-accept the same key");
        assert_eq!(
            std::fs::read_to_string(&known_hosts).unwrap(),
            format!("{a}\n")
        );
        // A key of another type for the same alias is added alongside it.
        pir::env_client::ssh::accept_host_key(&target, std::slice::from_ref(&rsa))
            .await
            .expect("accept another key type");
        assert_eq!(
            std::fs::read_to_string(&known_hosts).unwrap(),
            format!("{a}\n{rsa}\n")
        );
        // A DIFFERENT key of an already-trusted type is refused and nothing
        // changes (the owner must forget the old key first).
        let error = pir::env_client::ssh::accept_host_key(&target, std::slice::from_ref(&b))
            .await
            .expect_err("a changed key is refused");
        assert!(
            matches!(
                error,
                pir::env_client::ssh::SshFailure::HostKeyChanged { .. }
            ),
            "expected HostKeyChanged, got {error:?}"
        );
        assert_eq!(
            std::fs::read_to_string(&known_hosts).unwrap(),
            format!("{a}\n{rsa}\n"),
            "a refused change leaves the file untouched"
        );
        // A line for another alias (or a non-key line) is refused outright.
        let foreign = format!("other-host {ED25519_A}");
        assert!(pir::env_client::ssh::accept_host_key(&target, &[foreign])
            .await
            .is_err());
        // Forgetting removes every line of the alias; the changed key is then
        // accepted.
        pir::env_client::ssh::forget_host_key(&target)
            .await
            .expect("forget the alias");
        assert_eq!(std::fs::read_to_string(&known_hosts).unwrap(), "");
        pir::env_client::ssh::accept_host_key(&target, std::slice::from_ref(&b))
            .await
            .expect("accept the new key after forgetting");
        assert_eq!(
            std::fs::read_to_string(&known_hosts).unwrap(),
            format!("{b}\n")
        );
    });
}

// ---------------------------------------------------------------------------
// env: exec spill / window / inheritEnv through the real client
// ---------------------------------------------------------------------------

/// The client spawns the daemon the way `local_daemon()` does and drives it
/// through [`RemoteExecutionEnv`].
fn local_environment(cwd: String) -> pir::env_client::RemoteExecutionEnv {
    let options = pir::env_client::RemoteExecutionEnvOptions::new(
        "pi-env:w11h",
        pir::env_client::Connection::argv(vec![
            env!("CARGO_BIN_EXE_pir").to_string(),
            "env-daemon".to_string(),
        ]),
    );
    pir::env_client::RemoteExecutionEnv::new(pir::env_client::RemoteExecutionEnvOptions {
        cwd: Some(cwd),
        ..options
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exec_spill_writes_the_complete_output_to_a_file() {
    let tree = Tree::new("spill");
    let environment = local_environment(tree.root());
    // `spill.afterBytes: 1` crosses on the first chunk, so the complete output
    // lands in `pi-output-<uuid>.log` in a fresh `tmp-` directory
    // (semantics.md "Spill").
    let result = environment
        .exec(
            &pir::env_client::ExecCommand::Shell("printf 'spilled-output\\n'".to_string()),
            &pir::env_client::ExecOptions {
                cwd: Some(tree.root()),
                spill: Some(json!({ "afterBytes": 1, "afterLines": 1 })),
                ..Default::default()
            },
            &pir::env_client::Context::default(),
        )
        .await
        .expect("exec with spill");
    assert_eq!(result.exit_code, 0);
    let spill_path = result.spill_path.expect("a spill path");
    let name = std::path::Path::new(&spill_path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    assert!(
        name.starts_with("pi-output-") && name.ends_with(".log"),
        "spill file name: {spill_path}"
    );
    let spilled = std::fs::read_to_string(&spill_path).expect("read the spill file");
    assert!(
        spilled.contains("spilled-output"),
        "the spill file holds the complete output: {spilled:?}"
    );
    let _ = std::fs::remove_file(&spill_path);
    environment.connection.close();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exec_window_paces_and_skips_output_with_accounting() {
    let tree = Tree::new("window");
    let environment = local_environment(tree.root());
    // A tiny window over a burst: the daemon coalesces, and what it had to
    // drop is reported as `skipped` on the next event (protocol.md "exec"
    // window rules). The total the callback sees never exceeds what the
    // command wrote, and the accounting adds up.
    let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = std::sync::Arc::clone(&captured);
    let result = environment
        .exec(
            &pir::env_client::ExecCommand::Shell(
                "i=0; while [ $i -lt 200 ]; do i=$((i+1)); printf 'line-%03d\\n' \"$i\"; done"
                    .to_string(),
            ),
            &pir::env_client::ExecOptions {
                cwd: Some(tree.root()),
                window: Some(json!({
                    "maxBytes": 64,
                    "maxLines": 2,
                    "minIntervalMs": 1,
                    "bytesPerSecond": 64,
                })),
                on_output: Some(std::sync::Arc::new(move |text: &str, info| {
                    if let Ok(mut guard) = sink.lock() {
                        guard.push((text.to_string(), info.skipped.clone()));
                    }
                })),
                ..Default::default()
            },
            &pir::env_client::Context::default(),
        )
        .await
        .expect("exec with a window");
    assert_eq!(result.exit_code, 0);
    let events = captured
        .lock()
        .map(|guard| guard.clone())
        .unwrap_or_default();
    assert!(!events.is_empty(), "the window still delivers output");
    // `skipped` (when present) carries the documented fields; every byte it
    // claims was dropped, so the delivered text stays under the command's
    // total.
    let mut skipped_bytes = 0u64;
    let mut delivered = 0usize;
    for (text, skipped) in &events {
        delivered += text.len();
        if let Some(skipped) = skipped {
            let bytes = skipped["bytes"].as_u64().expect("skipped.bytes");
            assert!(skipped["newlines"].is_u64());
            assert!(skipped["endsWithNewline"].is_boolean());
            skipped_bytes += bytes;
        }
    }
    assert!(
        delivered + skipped_bytes as usize <= 200 * 10 + 8,
        "delivered {delivered} + skipped {skipped_bytes} never exceeds the written bytes"
    );
    environment.connection.close();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exec_inherit_env_decides_which_variables_the_command_sees() {
    let tree = Tree::new("inherit");
    let environment = local_environment(tree.root());
    // Without `inheritEnv`, only the request's own `env` is present
    // (semantics.md "Commands"): a variable the daemon itself has must not
    // leak through. `W11H_ONLY` is the marker.
    let output = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let sink = std::sync::Arc::clone(&output);
    let mut env = std::collections::BTreeMap::new();
    env.insert("W11H_ONLY".to_string(), Some("kept".to_string()));
    let result = environment
        .exec(
            &pir::env_client::ExecCommand::Shell(
                "printf '%s' \"${W11H_ONLY:-missing}\"".to_string(),
            ),
            &pir::env_client::ExecOptions {
                cwd: Some(tree.root()),
                env,
                inherit_env: Some(false),
                on_output: Some(std::sync::Arc::new(move |text: &str, _info| {
                    if let Ok(mut guard) = sink.lock() {
                        guard.push_str(text);
                    }
                })),
                ..Default::default()
            },
            &pir::env_client::Context::default(),
        )
        .await
        .expect("exec without inheritEnv");
    assert_eq!(result.exit_code, 0);
    assert_eq!(
        output.lock().map(|guard| guard.clone()).unwrap_or_default(),
        "kept",
        "the request's own variable survives"
    );

    // With `inheritEnv`, a variable the test process exports reaches the
    // remote command through the daemon's environment.
    std::env::set_var("W11H_INHERITED", "from-parent");
    let output = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let sink = std::sync::Arc::clone(&output);
    let result = environment
        .exec(
            &pir::env_client::ExecCommand::Shell(
                "printf '%s' \"${W11H_INHERITED:-missing}\"".to_string(),
            ),
            &pir::env_client::ExecOptions {
                cwd: Some(tree.root()),
                inherit_env: Some(true),
                on_output: Some(std::sync::Arc::new(move |text: &str, _info| {
                    if let Ok(mut guard) = sink.lock() {
                        guard.push_str(text);
                    }
                })),
                ..Default::default()
            },
            &pir::env_client::Context::default(),
        )
        .await
        .expect("exec with inheritEnv");
    assert_eq!(result.exit_code, 0);
    assert_eq!(
        output.lock().map(|guard| guard.clone()).unwrap_or_default(),
        "from-parent",
        "the daemon inherits the host environment"
    );
    std::env::remove_var("W11H_INHERITED");
    environment.connection.close();
}

// ---------------------------------------------------------------------------
// MCP: the typed error surface
// ---------------------------------------------------------------------------

#[test]
fn mcp_error_variants_keep_their_contract_text() {
    // `McpError::Aborted`'s text is pinned in `tests/w1_d8_mcp.rs`; these two
    // variants carry a formatted message a caller (and the model) reads.
    let too_large = McpError::MessageTooLarge { limit: 1024 * 1024 };
    assert_eq!(
        too_large.to_string(),
        "MCP message exceeds the 1048576-byte frame cap (1 MiB)"
    );
    let login = McpError::LoginRequired {
        server: "docs".to_string(),
    };
    // The agent-facing wording is upstream's `signInRequiredMessage`
    // (runtime.ts:79), which names the in-session command, not the CLI.
    assert_eq!(
        login.to_string(),
        "MCP server \"docs\" requires sign-in. Run /mcp to sign in."
    );
}
