//! W13-B residuals for the MCP client and manager (the P1/P2 findings of
//! the W12 code-review findings that touch `src/mcp/**`).
//!
//! Every block names the defect it pins:
//!
//! * **A server-side JSON-RPC error is not a retryable closure.** A `tools/call`
//!   the server answers with an `error` object must reach the caller as
//!   [`McpError::Server`], and the call must reach the server exactly once:
//!   `McpManager::call_tool` retries `ConnectionClosed`, so a misclassified
//!   server error would run the tool twice.
//! * **The deadline and abort reach the dispatch, and the timed-out request is
//!   cancelled on the server** (`notifications/cancelled`, upstream
//!   `requestInternal`).
//! * **A progress notification restarts the idle deadline** (upstream resets
//!   its request timer on progress), so a slow-but-reporting server is not cut
//!   off.
//! * **OAuth scope challenges** (`insufficient_scope`) are recognized as their
//!   own case: a refresh may narrow a grant but never widen it (RFC 6749 §6),
//!   so the manager routes them to the step-up path (granted + challenged
//!   scopes) instead of a refresh.
//! * **`shutdown()` closes the spawn window** (the shared `shutting_down`
//!   flag): no connection is left `Connected` after the teardown returns, and a
//!   fresh use after it still works (the `/reload` contract).
//! * **`get_client` does not hold the slot lock across the transport probe**, so
//!   a closed transport reconnects without stalling the close path.
//!
//! Hermetic: every server here is a child entry of this test binary speaking
//! stdio JSON-RPC on its own pipes; no network, no `~/.pi` writes.

#![cfg(feature = "mcp")]

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use pir::config::mcp::McpServerConfig;
use pir::mcp::error::McpError;
use pir::mcp::manager::{
    authorization_challenge, bounded_reason, is_scope_challenge, ConnectionState, McpManager,
    DEFAULT_STARTUP_DISCOVERY_BUDGET,
};
use pir::mcp::oauth::{parse_www_authenticate, step_up_scope, OAuthChallenge};
use pir::mcp::registry::{McpServerEntry, McpServerSource};
use pir::plugins::builtin::McpPlugin;
use pir::plugins::{AgentPlugin, PluginHost};

// ---------------------------------------------------------------------------
// The mock stdio server (child entry of this test binary)
// ---------------------------------------------------------------------------

/// Server behaviour, selected by `PIR_W13_MOCK`.
const MOCK_SERVER_ERROR: &str = "server-error";
const MOCK_SLOW: &str = "slow";
const MOCK_PROGRESS: &str = "progress";
/// A server that accepts the transport but NEVER answers a request (the
/// never-answering server the startup budget must not wait out).
const MOCK_SILENT: &str = "silent";
/// A well-behaved server (answers `initialize` and `tools/list` immediately).
pub(crate) const MOCK_HEALTHY: &str = "healthy";

/// Where a child records the `notifications/cancelled` it received.
const CANCEL_LOG_ENV: &str = "PIR_W13_CANCEL_LOG";
/// Where a child counts the `tools/call` requests it received.
const COUNT_FILE_ENV: &str = "PIR_W13_COUNT_FILE";
/// How long the child holds `initialize` before answering (a deterministic
/// window for the "shutdown races an in-flight connect" test).
const INIT_DELAY_ENV: &str = "PIR_W13_INIT_DELAY_MS";

/// Where a child records its own pid (the close/teardown liveness probe).
const PID_FILE_ENV: &str = "PIR_W13_PID_FILE";

#[test]
fn w13b_mock_stdio_server() {
    let Ok(mode) = std::env::var("PIR_W13_MOCK") else {
        return;
    };
    if let Ok(path) = std::env::var(PID_FILE_ENV) {
        crate::common::write_file_atomically(path, &std::process::id().to_string());
    }
    let delay_ms: u64 = std::env::var("PIR_W13_DELAY_MS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let init_delay_ms: u64 = std::env::var(INIT_DELAY_ENV)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    serve_mock(&mode, delay_ms, init_delay_ms);
}

/// The line-delimited JSON-RPC loop of the mock server.
///
/// A `tools/call` that takes time runs on a worker thread while this loop keeps
/// reading stdin, so the `notifications/cancelled` the client sends for a
/// timed-out/aborted request is still observed (a server that blocked its read
/// loop for the whole call could never record it).
fn serve_mock(mode: &str, delay_ms: u64, init_delay_ms: u64) {
    use std::io::BufRead;
    use std::sync::{Arc, Mutex};

    let stdin = std::io::stdin();
    let stdout = Arc::new(Mutex::new(std::io::stdout()));
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let method = value.get("method").and_then(Value::as_str).unwrap_or("");
        let Some(id) = value.get("id").cloned() else {
            // A notification: record the `notifications/cancelled` the client
            // sent for a timed-out or aborted request.
            if method == "notifications/cancelled" {
                if let Ok(path) = std::env::var(CANCEL_LOG_ENV) {
                    // Atomic publish: the parent polls this path and parses it
                    // immediately, so a truncate-then-write would let the poll
                    // read an empty file (`cancel log json: EOF`).
                    crate::common::write_file_atomically(path, &value.to_string());
                }
            }
            continue;
        };
        if method == "tools/call" {
            // Accumulate across child processes: a retry reconnects (a fresh
            // child), so a per-process counter would hide the second dispatch.
            let count = std::env::var(COUNT_FILE_ENV)
                .ok()
                .and_then(|path| std::fs::read_to_string(path).ok())
                .and_then(|text| text.trim().parse::<u64>().ok())
                .unwrap_or(0)
                + 1;
            if let Ok(path) = std::env::var(COUNT_FILE_ENV) {
                crate::common::write_file_atomically(path, &count.to_string());
            }
        }
        let reply = match method {
            "initialize" => {
                if mode == MOCK_SILENT {
                    // Holds the handshake open forever: the client's connect
                    // future stays pending until its own timeout (or the
                    // startup budget cancels it).
                    continue;
                }
                if init_delay_ms > 0 {
                    std::thread::sleep(Duration::from_millis(init_delay_ms));
                }
                Some(json!({
                    "protocolVersion": "2025-06-18",
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "w13b-mock", "version": "1.0.0" },
                }))
            }
            "tools/list" => Some(json!({
                "tools": [{ "name": "work", "inputSchema": { "type": "object" } }],
            })),
            "tools/call" => {
                match mode {
                    // A JSON-RPC error object: the server RAN (and failed) the
                    // call, so it must never be retried as a transport closure.
                    MOCK_SERVER_ERROR => {
                        let line = json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "error": { "code": -32000, "message": "tool exploded" },
                        });
                        write_line(&stdout, &line);
                        continue;
                    }
                    // Answers only after the delay; the client's request
                    // deadline is shorter, so the call times out.
                    MOCK_SLOW => {
                        let stdout = Arc::clone(&stdout);
                        std::thread::spawn(move || {
                            std::thread::sleep(Duration::from_millis(delay_ms));
                            write_line(
                                &stdout,
                                &json!({
                                    "jsonrpc": "2.0",
                                    "id": id,
                                    "result": { "content": [{ "type": "text", "text": "late" }] },
                                }),
                            );
                        });
                        continue;
                    }
                    // Reports progress across the delay: a client with an idle
                    // deadline must let the call finish.
                    MOCK_PROGRESS => {
                        let token = value
                            .get("params")
                            .and_then(|params| params.get("_meta"))
                            .and_then(|meta| meta.get("progressToken"))
                            .cloned();
                        let stdout = Arc::clone(&stdout);
                        std::thread::spawn(move || {
                            let steps = 6u64;
                            let step = delay_ms / steps.max(1);
                            for index in 0..steps {
                                std::thread::sleep(Duration::from_millis(step));
                                if let Some(token) = &token {
                                    write_line(
                                        &stdout,
                                        &json!({
                                            "jsonrpc": "2.0",
                                            "method": "notifications/progress",
                                            "params": {
                                                "progressToken": token,
                                                "progress": index + 1,
                                            },
                                        }),
                                    );
                                }
                            }
                            write_line(
                                &stdout,
                                &json!({
                                    "jsonrpc": "2.0",
                                    "id": id,
                                    "result": { "content": [{ "type": "text", "text": "done" }] },
                                }),
                            );
                        });
                        continue;
                    }
                    _ => continue,
                }
            }
            _ => Some(json!({})),
        };
        if let Some(reply) = reply {
            write_line(
                &stdout,
                &json!({ "jsonrpc": "2.0", "id": id, "result": reply }),
            );
        }
    }
    // The workers are deliberately NOT joined: a call still sleeping its delay
    // must not hold the child after stdin closes, or the teardown would wait
    // out the whole delay. Returning from `main` ends the process.
}

/// Write one JSON-RPC line under the shared stdout lock and flush it.
fn write_line(stdout: &std::sync::Mutex<std::io::Stdout>, line: &Value) {
    use std::io::Write;

    let Ok(mut stdout) = stdout.lock() else {
        return;
    };
    let _ = writeln!(stdout, "{line}");
    let _ = stdout.flush();
}

/// Command + args that re-invoke this test binary as the mock child.
///
/// `--exact` needs the FULL test path: `mcp_residuals_w13::w13b_mock_stdio_server`.
/// `module_path!()` must NOT be prefixed: inside the merged `g_agent` harness
/// it expands to `g_agent::mcp_residuals_w13`, but libtest reports (and
/// `--list` prints) the name as `mcp_residuals_w13::w13b_mock_stdio_server` —
/// the file's own module path, without the binary's name. A prefixed filter
/// matches nothing, the child runs `0 tests` and exits, the handshake sees EOF,
/// and every call collapses to `ConnectionClosed` (measured: 7 failures).
/// Command + args that spawn a fresh mock MCP server. `pub(crate)` so the
/// W18 toggle suite can write a FILE-backed server (the `/mcp remove` ->
/// `enable` path restores from `mcp.json`, not from an in-memory
/// registration).
///
/// When the small `pir-mock-mcp` helper binary exists (Cargo `[[bin]]`, see
/// `tests/support/mock_mcp_server.rs`) it is preferred: the helper links only
/// `serde_json`, so a mock child no longer re-executes this ~330 MB test
/// binary. `option_env!` keeps the tree working before the manifest entry
/// lands: the fallback is the old in-binary child entry.
pub(crate) fn mock_spawn() -> (String, Vec<String>) {
    if let Some(exe) = option_env!("CARGO_BIN_EXE_pir-mock-mcp") {
        return (exe.to_owned(), Vec::new());
    }
    let exe = std::env::current_exe().expect("current exe");
    (
        exe.to_string_lossy().into_owned(),
        vec![
            "mcp_residuals_w13::w13b_mock_stdio_server".to_owned(),
            "--exact".to_owned(),
            "--nocapture".to_owned(),
        ],
    )
}

/// A unique temporary directory removed on drop.
///
/// `pub(crate)` for the W18 pruning suite, which reuses this module's mock
/// stdio server instead of duplicating it.
pub(crate) struct Tree(pub(crate) std::path::PathBuf);

impl Tree {
    pub(crate) fn new(label: &str) -> Tree {
        let path = std::env::temp_dir().join(format!(
            "pir-w13b-{label}-{}-{}",
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
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub(crate) fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime")
}

/// Register a mock stdio server with the manager. `timeout` is the per-server
/// request deadline (seconds), i.e. the client's idle deadline.
pub(crate) fn register_mock(
    manager: &McpManager,
    name: &str,
    mode: &str,
    timeout_secs: u64,
    delay_ms: u64,
    extra: &[(&str, &str)],
) {
    let (command, args) = mock_spawn();
    let mut env = serde_json::Map::new();
    env.insert("PIR_W13_MOCK".to_owned(), json!(mode));
    env.insert("PIR_W13_DELAY_MS".to_owned(), json!(delay_ms.to_string()));
    for (key, value) in extra {
        env.insert((*key).to_owned(), json!(value));
    }
    let config: McpServerConfig = serde_json::from_value(json!({
        "command": command,
        "args": args,
        "env": Value::Object(env),
        "timeout": timeout_secs,
    }))
    .expect("mock server config");
    manager.registry().register(McpServerEntry {
        name: name.to_owned(),
        config,
        source: McpServerSource::Extension("w13b".to_owned()),
    });
}

/// [`register_mock`] plus a `initialize` delay of `init_delay_ms`.
fn register_slow_handshake(manager: &McpManager, name: &str, init_delay_ms: u64) {
    let delay = init_delay_ms.to_string();
    register_mock(
        manager,
        name,
        MOCK_SLOW,
        20,
        200,
        &[(INIT_DELAY_ENV, delay.as_str())],
    );
}

// ---------------------------------------------------------------------------
// A server-side error is never a retryable closure
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// A server-side error is never a retryable closure
// ---------------------------------------------------------------------------

#[test]
fn a_server_side_tool_error_keeps_its_code_and_message() {
    let manager = McpManager::without_log(&std::env::current_dir().expect("cwd"));
    register_mock(&manager, "failing", MOCK_SERVER_ERROR, 20, 0, &[]);
    runtime().block_on(async {
        let error = manager
            .call_tool("failing", "work", &json!({}), None)
            .await
            .expect_err("the server answered an error object");
        match &error {
            McpError::Server { code, message, .. } => {
                assert_eq!(*code, -32000, "the JSON-RPC code survives: {error}");
                assert_eq!(message, "tool exploded", "the message survives: {error}");
            }
            other => panic!("expected McpError::Server, got {other:?}"),
        }
        assert!(
            !matches!(error, McpError::ConnectionClosed),
            "a server-side error is not a retryable closure: {error}"
        );
        manager.shutdown().await;
    });
}

#[test]
fn a_failed_tool_call_reaches_the_server_exactly_once() {
    // The retry arms of `McpManager::call_tool` retry `ConnectionClosed` and
    // `SessionExpired` once. A server-side error must not be among them: the
    // call already had its side effects.
    let tree = Tree::new("one-dispatch");
    let counter = tree.path("calls");
    let manager = McpManager::without_log(&std::env::current_dir().expect("cwd"));
    register_mock(
        &manager,
        "counted",
        MOCK_SERVER_ERROR,
        20,
        0,
        &[(COUNT_FILE_ENV, counter.as_str())],
    );
    runtime().block_on(async {
        let _ = manager.call_tool("counted", "work", &json!({}), None).await;
        let count: u64 = std::fs::read_to_string(&counter)
            .expect("the mock counted its calls")
            .trim()
            .parse()
            .expect("the counter is a number");
        assert_eq!(count, 1, "the failed call was dispatched exactly once");
        manager.shutdown().await;
    });
}

// ---------------------------------------------------------------------------
// The deadline / abort end the request, and cancel it server-side
// ---------------------------------------------------------------------------

/// Read the mock child's cancel record, or `None` while it is absent or not
/// yet a complete JSON document.
///
/// The child publishes atomically (temp file + rename), so a complete file is
/// the normal case; this still retries a transient partial read instead of
/// turning it into the `cancel log json: EOF` flake the scheduler used to see.
fn read_cancel_log(path: &str) -> Option<Value> {
    let text = std::fs::read_to_string(path).ok()?;
    if text.trim().is_empty() {
        return None;
    }
    serde_json::from_str(&text).ok()
}

#[test]
fn a_timed_out_request_is_cancelled_on_the_server() {
    let tree = Tree::new("timeout-cancel");
    let cancel_log = tree.path("cancelled.json");
    // A 2 s request deadline against a server that holds the call for 30 s.
    // The deadline must outlast the mock child's spawn + handshake (~1.1 s on
    // an idle box, more when the merged binary starts many children at once);
    // the 30 s hold keeps the timeout semantics unambiguous. 2 s still leaves
    // ~0.9 s over that measured spawn+handshake cost, and the deadline is pure
    // wall time for the test, so it is kept as tight as the fixture allows.
    let manager = McpManager::without_log(&std::env::current_dir().expect("cwd"));
    register_mock(
        &manager,
        "slow",
        MOCK_SLOW,
        2,
        30_000,
        &[(CANCEL_LOG_ENV, cancel_log.as_str())],
    );
    runtime().block_on(async {
        let started = Instant::now();
        let error = manager
            .call_tool("slow", "work", &json!({}), None)
            .await
            .expect_err("the call cannot finish inside its deadline");
        assert!(
            matches!(error, McpError::Timeout { .. }),
            "a timed-out call reports Timeout: {error:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "the deadline ends the call, took {:?}",
            started.elapsed()
        );
        // The server was told to stop (upstream `cancelPending`): a long
        // `tools/call` must not keep running unobserved.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            // A read that lands mid-publish (or an empty file) is retried, not
            // parsed: the child now renames into place, but a retry keeps the
            // check honest if a reader ever catches a partial write anyway.
            if let Some(value) = read_cancel_log(&cancel_log) {
                assert_eq!(value["method"], json!("notifications/cancelled"));
                assert_eq!(value["params"]["reason"], json!("Request timed out"));
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the timed-out request was never cancelled on the server"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        manager.shutdown().await;
    });
}

#[test]
fn aborting_a_call_reports_aborted_without_waiting_for_the_server() {
    let tree = Tree::new("abort-cancel");
    let cancel_log = tree.path("cancelled.json");
    let count_file = tree.path("calls.txt");
    let manager = McpManager::without_log(&std::env::current_dir().expect("cwd"));
    register_mock(
        &manager,
        "slow-abort",
        MOCK_SLOW,
        60,
        20_000,
        &[
            (CANCEL_LOG_ENV, cancel_log.as_str()),
            (COUNT_FILE_ENV, count_file.as_str()),
        ],
    );
    runtime().block_on(async {
        let signal = pir::ai::types::AbortSignal::new();
        let abort = signal.clone();
        tokio::spawn(async move {
            // Abort only once the server has actually RECEIVED `tools/call`.
            // The mock child is this same test binary, and process start plus
            // the `initialize` handshake measured 1123 ms here — longer than
            // any fixed sleep would be safe for. Aborting before dispatch is a
            // DIFFERENT upstream path: `requestInternal` checks
            // `options.signal?.aborted` before sending (`client.ts:403`) and
            // throws without notifying, so a fixed-delay test would assert the
            // cancel of a request that was never sent.
            let deadline = Instant::now() + Duration::from_secs(30);
            loop {
                let dispatched = std::fs::read_to_string(&count_file)
                    .ok()
                    .and_then(|text| text.trim().parse::<u64>().ok())
                    .is_some_and(|count| count >= 1);
                if dispatched || Instant::now() >= deadline {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            abort.abort();
        });
        let started = Instant::now();
        let error = manager
            .call_tool("slow-abort", "work", &json!({}), Some(&signal))
            .await
            .expect_err("the caller aborted");
        assert!(
            matches!(error, McpError::Aborted),
            "an aborted call reports Aborted: {error:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the abort does not wait for the server, took {:?}",
            started.elapsed()
        );
        // The abort cancels the server-side work with its own reason.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(value) = read_cancel_log(&cancel_log) {
                assert_eq!(value["method"], json!("notifications/cancelled"));
                assert_eq!(value["params"]["reason"], json!("Aborted"));
                break;
            }
            assert!(Instant::now() < deadline, "the abort was never sent");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        manager.shutdown().await;
    });
}

#[test]
fn progress_notifications_keep_a_slow_request_alive() {
    // Idle deadline 1 s, ~1.2 s of work, progress every ~0.2 s: a fixed
    // deadline would cut this off, an idle deadline restarted by progress
    // finishes it (upstream resets its request timer on progress). The 1 s
    // ceiling stays below the 1.2 s of work (so a broken idle reset still
    // fails) while leaving ~0.8 s for a progress tick delayed by a loaded box.
    // The shape (deadline < work, progress period < deadline) is what the test
    // pins; the absolute durations are only wall time.
    let manager = McpManager::without_log(&std::env::current_dir().expect("cwd"));
    register_mock(&manager, "reporting", MOCK_PROGRESS, 1, 1_200, &[]);
    runtime().block_on(async {
        let result = manager
            .call_tool("reporting", "work", &json!({}), None)
            .await
            .expect("progress restarts the idle deadline");
        assert_eq!(
            result
                .content
                .first()
                .and_then(|block| block.text.as_deref()),
            Some("done"),
            "the call completed: {result:?}"
        );
        manager.shutdown().await;
    });
}

// ---------------------------------------------------------------------------
// OAuth scope challenges
// ---------------------------------------------------------------------------

#[test]
fn a_scope_challenge_is_recovered_from_the_transport_error_text() {
    // rmcp renders the `WWW-Authenticate` header into its message and
    // `http::map_service_error` keeps it; the manager recovers the challenge to
    // decide between a refresh and a step-up.
    let error = McpError::Transport(
        "connection reset: insufficient scope: Bearer scope=\"files write\" error=\"insufficient_scope\""
            .to_owned(),
    );
    let challenge = authorization_challenge(&error).expect("the challenge is recovered");
    assert_eq!(challenge.scope.as_deref(), Some("files write"));
    assert_eq!(challenge.error.as_deref(), Some("insufficient_scope"));
    assert!(
        is_scope_challenge(&error, &challenge),
        "an insufficient_scope challenge routes to the step-up path"
    );

    // A 403 whose body carries the challenge (the `McpError::Http` shape).
    let http = McpError::Http {
        status: 403,
        body: "insufficient scope: Bearer scope=\"admin\"".to_owned(),
    };
    let challenge = authorization_challenge(&http).expect("the 403 challenge is recovered");
    assert_eq!(challenge.scope.as_deref(), Some("admin"));
    assert!(is_scope_challenge(&http, &challenge));

    // The scheme is case-insensitive and a DPoP challenge is recognized too.
    let dpop = McpError::Transport("DPoP scope=\"files\"".to_owned());
    let challenge = authorization_challenge(&dpop).expect("the DPoP challenge is recovered");
    assert_eq!(challenge.scope.as_deref(), Some("files"));
}

#[test]
fn a_plain_401_challenge_is_not_a_scope_challenge() {
    // A 401 with no scope asks for a token, not for more scope: refreshing is
    // the right (and cheaper) answer.
    let error = McpError::Http {
        status: 401,
        body: "authorization required: Bearer resource_metadata=\"https://mcp.example/.well-known/oauth-protected-resource\""
            .to_owned(),
    };
    let challenge = authorization_challenge(&error).expect("the challenge is recovered");
    assert_eq!(
        challenge.resource_metadata_url.as_deref(),
        Some("https://mcp.example/.well-known/oauth-protected-resource")
    );
    assert_eq!(challenge.scope, None);
    assert!(
        !is_scope_challenge(&error, &challenge),
        "a plain 401 keeps the refresh path"
    );
    // An error with no challenge text at all yields no challenge.
    assert_eq!(
        authorization_challenge(&McpError::ConnectionClosed),
        None,
        "a closure carries no challenge"
    );
    assert_eq!(
        authorization_challenge(&McpError::Transport("connection reset".to_owned())),
        None
    );
}

#[test]
fn the_step_up_asks_for_the_granted_scopes_plus_the_challenged_ones() {
    // What the step-up authorizes with: the granted scope union the challenged
    // one (upstream `stepUpScope`), never the challenge alone — the narrow
    // missing set would drop what the token already had.
    let granted = "files read";
    let challenge = parse_www_authenticate(Some("Bearer scope=\"write admin\""));
    assert_eq!(challenge.scope.as_deref(), Some("write admin"));
    assert_eq!(
        step_up_scope(Some(granted), challenge.scope.as_deref()),
        Some("files read write admin".to_owned())
    );
    assert_eq!(
        step_up_scope(Some(granted), Some("files read")),
        Some(granted.to_owned()),
        "a challenge naming only granted scopes adds nothing"
    );
    assert_eq!(
        step_up_scope(None, challenge.scope.as_deref()),
        Some("write admin".to_owned()),
        "nothing granted yet: the challenged scopes are requested"
    );
    assert_eq!(OAuthChallenge::default().scope, None);
}

// ---------------------------------------------------------------------------
// shutdown closes the spawn window
// ---------------------------------------------------------------------------

#[test]
fn no_connection_stays_connected_after_the_teardown_returns() {
    // The defect: a `connection()`/`get_client()` racing `shutdown()` could
    // spawn a child AFTER the drain, so nothing ever closed it. With the
    // `shutting_down` flag set before the drain, every racer either lands in
    // the drained map (and is closed) or fails closed.
    //
    // The mock delays `initialize`, so the connects are deterministically still
    // in flight when the teardown runs (no sleep-based timing on the parent
    // side).
    let manager = McpManager::without_log(&std::env::current_dir().expect("cwd"));
    register_slow_handshake(&manager, "raced", 500);
    runtime().block_on(async {
        let mut tasks = Vec::new();
        for _ in 0..4 {
            let cloned = manager.clone();
            tasks.push(tokio::spawn(
                async move { cloned.get_client("raced").await },
            ));
        }
        // Let every caller get as far as the handshake, then tear down under
        // them. The connects answer ~0.5 s from now (150 ms is still well
        // inside the delayed `initialize`), so every racer is in flight.
        tokio::time::sleep(Duration::from_millis(150)).await;
        manager.shutdown().await;
        for task in tasks {
            let result = task.await.expect("no task panicked");
            // A connect that finished before the drain is closed by it (its
            // client reports closed); one that observed the flag fails closed.
            match result {
                Ok(client) => assert!(
                    client.transport_is_closed().await || client.ping().await.is_err(),
                    "a client that survived the teardown is closed"
                ),
                Err(error) => assert!(
                    matches!(error, McpError::ConnectionClosed),
                    "a racing connect fails closed: {error:?}"
                ),
            }
        }
        let states = manager.connection_states().await;
        for (name, state) in &states {
            assert!(
                !matches!(
                    state,
                    ConnectionState::Connected | ConnectionState::Connecting
                ),
                "server {name} is {state:?} after shutdown returned: \
                 the teardown did not reach a connection it missed"
            );
        }
        manager.shutdown().await;
    });
}

#[test]
fn a_fresh_use_after_shutdown_still_connects() {
    // `/reload` calls `on_shutdown` and then reuses the same manager
    // (`McpPlugin::on_reload`), so the flag must not latch shut.
    let manager = McpManager::without_log(&std::env::current_dir().expect("cwd"));
    register_mock(&manager, "reload", MOCK_SLOW, 20, 5, &[]);
    runtime().block_on(async {
        assert!(manager.get_client("reload").await.is_ok(), "first use");
        manager.shutdown().await;
        assert!(
            manager.connection_states().await.is_empty(),
            "the teardown drained the map"
        );
        let after = manager
            .get_client("reload")
            .await
            .expect("a use after the teardown is a fresh start");
        assert!(
            !after.transport_is_closed().await,
            "the fresh client is live"
        );
        manager.shutdown().await;
    });
}

// ---------------------------------------------------------------------------
// get_client does not hold the slot lock across the transport probe
// ---------------------------------------------------------------------------

#[test]
fn a_closed_transport_is_replaced_by_a_new_client() {
    // The probe (`transport_is_closed`) runs outside the slot lock, and the
    // replacement is a NEW client (the readiness cache keys on its instance id).
    let manager = McpManager::without_log(&std::env::current_dir().expect("cwd"));
    register_mock(&manager, "reconnect", MOCK_SLOW, 20, 5, &[]);
    runtime().block_on(async {
        let first = manager
            .get_client("reconnect")
            .await
            .expect("first connect");
        first.close().await;
        assert!(first.transport_is_closed().await, "the client is closed");
        let started = Instant::now();
        let second = manager
            .get_client("reconnect")
            .await
            .expect("a closed transport reconnects");
        assert!(
            !std::sync::Arc::ptr_eq(&first, &second),
            "the replacement is a new client"
        );
        assert!(
            second.instance_id() > first.instance_id(),
            "instance ids are never reused ({} then {})",
            first.instance_id(),
            second.instance_id()
        );
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "the reconnect is bounded, took {:?}",
            started.elapsed()
        );
        manager.shutdown().await;
    });
}

// ---------------------------------------------------------------------------
// CL18 P8a: the session-start discovery is bounded and its failures surface
// ---------------------------------------------------------------------------

/// The reason a startup diagnostic carries is one bounded line: a connect
/// error can embed a server's whole stderr tail or a long HTTP body, and the
/// diagnostic must name the server, not dump its config.
#[test]
fn a_diagnostic_reason_is_one_bounded_line() {
    assert_eq!(bounded_reason("boom"), "boom");
    assert_eq!(
        bounded_reason("first line\nsecond line\nthird"),
        "first line",
        "only the first line (the cause) survives"
    );
    let long = "x".repeat(500);
    let bounded = bounded_reason(&long);
    assert_eq!(bounded.chars().count(), 201, "200 chars plus the ellipsis");
    assert!(bounded.ends_with('…'), "{bounded:?}");
    // A multi-line, config-shaped payload is cut at its first line.
    assert_eq!(
        bounded_reason("failed: {\"env\":{\"TOKEN\":\"secret\"}}\nmore"),
        "failed: {\"env\":{\"TOKEN\":\"secret\"}}"
    );
}

/// The upstream default budget is the documented 10 s
/// (`DEFAULT_STARTUP_WAIT_MS`, extensions/mcp/index.ts:93).
#[test]
fn the_default_startup_budget_is_upstreams_ten_seconds() {
    assert_eq!(
        DEFAULT_STARTUP_DISCOVERY_BUDGET,
        Duration::from_millis(10_000)
    );
    assert_eq!(
        McpPlugin::with_cwd(
            &std::env::temp_dir().join("pir-w13b-default-budget"),
            &std::env::temp_dir().join("pir-w13b-default-budget"),
        )
        .startup_budget(),
        DEFAULT_STARTUP_DISCOVERY_BUDGET,
        "the plugin applies the upstream default unless a test shrinks it"
    );
}

/// A server that never answers must not hold the startup discovery past the
/// aggregate budget, while a healthy sibling still registers its tools.
///
/// Upstream `waitForDirectServers` races its startup wait against the
/// connections; without the cap this batch waits the silent server's whole
/// per-server timeout (60 s by default).
#[test]
fn a_never_answering_server_cannot_hold_startup_past_the_budget() {
    let manager = McpManager::without_log(&std::env::current_dir().expect("cwd"));
    // The silent server keeps the default 60 s per-server timeout, so only
    // the aggregate budget can end the batch.
    register_mock(&manager, "silent", MOCK_SILENT, 60, 0, &[]);
    register_mock(&manager, "healthy", MOCK_HEALTHY, 60, 0, &[]);
    runtime().block_on(async {
        // 3 s: the silent server's own timeout is 60 s, so only the aggregate
        // budget can end the batch; the healthy sibling's spawn+handshake
        // measured ~1.1 s, leaving ~1.9 s of margin on a loaded box (the
        // deadline itself is pure test wall time, hence kept as tight as the
        // fixture safely allows).
        let budget = Duration::from_secs(3);
        let started = Instant::now();
        let report = manager.discover_all_bounded(budget).await;
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(20),
            "the aggregate budget bounds startup (per-server timeout is 60 s), took {elapsed:?}"
        );
        assert!(
            elapsed >= budget,
            "the batch did not end before its budget, took {elapsed:?}"
        );
        assert_eq!(
            report.pending,
            vec!["silent".to_owned()],
            "the still-connecting server is reported pending: {report:?}"
        );
        assert!(report.failed.is_empty(), "{report:?}");
        let healthy = report
            .snapshot
            .get("healthy")
            .expect("the healthy sibling discovered inside the budget");
        assert_eq!(
            healthy
                .iter()
                .map(|tool| tool.tool.name.as_str())
                .collect::<Vec<_>>(),
            vec!["work"],
            "the healthy sibling still registers its tools"
        );
        assert!(
            manager.background_discovery_count() > 0,
            "the pending server is handed to the background continuation"
        );
        manager.shutdown().await;
        assert_eq!(
            manager.background_discovery_count(),
            0,
            "the teardown aborts the continuations"
        );
    });
}

/// A failing server is recorded with a bounded reason and the healthy sibling
/// still registers its tools (one dead server never takes the batch down).
#[test]
fn a_failing_server_is_reported_and_the_sibling_still_registers() {
    let manager = McpManager::without_log(&std::env::current_dir().expect("cwd"));
    // A command that cannot be spawned: the connect fails immediately.
    let missing: McpServerConfig = serde_json::from_value(json!({
        "command": "pir-w18-cl24-definitely-not-a-command",
        "timeout": 5,
    }))
    .expect("failing server config");
    manager.registry().register(McpServerEntry {
        name: "broken".to_owned(),
        config: missing,
        source: McpServerSource::Extension("w18".to_owned()),
    });
    register_mock(&manager, "healthy", MOCK_HEALTHY, 60, 0, &[]);
    runtime().block_on(async {
        let report = manager.discover_all_bounded(Duration::from_secs(30)).await;
        assert_eq!(report.failed.len(), 1, "{report:?}");
        let problem = &report.failed[0];
        assert_eq!(problem.server, "broken");
        assert!(
            !problem.message.is_empty() && !problem.message.contains('\n'),
            "the reason is one bounded line: {:?}",
            problem.message
        );
        assert!(
            matches!(
                manager.connection_states().await.get("broken"),
                Some(ConnectionState::Failed { .. })
            ),
            "a failed server keeps its Failed state"
        );
        assert!(
            report
                .snapshot
                .get("healthy")
                .is_some_and(|tools| tools.len() == 1),
            "the healthy sibling registers despite the failure: {report:?}"
        );
        assert!(
            report.pending.is_empty(),
            "a failure is not a pending server: {report:?}"
        );
        manager.shutdown().await;
    });
}

/// The production seam: a `session_start` dispatch through the `mcp` builtin
/// returns within the plugin's budget, registers the healthy server's
/// `mcp__*` tools, and keeps the pending server visible on the startup
/// report (surfaced by name, never as a bare "Tool not found in registry").
#[tokio::test(flavor = "current_thread")]
async fn session_start_is_bounded_and_reports_its_failures() {
    let tree = Tree::new("startup-budget");
    let agent_dir = std::env::temp_dir().join(format!(
        "pir-w13b-startup-agent-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    let _guard = crate::common::AgentDirEnvGuard::at(&agent_dir, &[]).await;

    let (command, args) = mock_spawn();
    let entry = |mode: &str| {
        json!({
            "command": command,
            "args": args,
            "env": { "PIR_W13_MOCK": mode },
            "timeout": 60,
        })
    };
    std::fs::write(
        tree.0.join("mcp.json"),
        json!({ "mcpServers": { "silent": entry(MOCK_SILENT), "healthy": entry(MOCK_HEALTHY) } })
            .to_string(),
    )
    .expect("write mcp.json");

    let plugin =
        Arc::new(McpPlugin::with_cwd(&tree.0, &tree.0).with_startup_budget(Duration::from_secs(3)));
    let host = PluginHost::build(None, vec![Arc::clone(&plugin) as Arc<dyn AgentPlugin>]);
    assert_eq!(plugin.manager().registry().list().len(), 2);

    let started = Instant::now();
    let mut payload = json!({ "type": "session_start" });
    host.emit("session_start", &mut payload)
        .await
        .expect("session_start dispatch");
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(20),
        "session_start cannot block on the silent server, took {elapsed:?}"
    );

    let report = plugin.startup_report().expect("the plugin kept its report");
    assert_eq!(report.pending, vec!["silent".to_owned()], "{report:?}");
    let lines = plugin.report_startup_problems(&report);
    assert!(
        lines
            .iter()
            .any(|line| line.contains("still connecting") && line.contains("silent")),
        "the pending server is surfaced by name: {lines:?}"
    );

    // The healthy server's tools reached the registry through the host's
    // post-dispatch rebuild.
    let names: Vec<String> = host
        .tool_defs()
        .iter()
        .map(|def| def.name.clone())
        .collect();
    assert!(
        names.iter().any(|name| name == "mcp__healthy__work"),
        "the healthy server's tools registered: {names:?}"
    );

    plugin.manager().shutdown().await;
    let _ = std::fs::remove_dir_all(&agent_dir);
}

/// The failure surface: a failed server's bounded reason becomes an `[ext]`
/// diagnostic naming the server (and only that server's reason — no config
/// dump), next to the one upstream-shaped notice for the pending servers.
#[test]
fn a_failed_server_produces_a_visible_named_diagnostic() {
    use pir::mcp::manager::{StartupDiscoveryProblem, StartupDiscoveryReport};
    let dir = std::env::temp_dir().join("pir-w13b-diag");
    let plugin = McpPlugin::with_cwd(&dir, &dir);
    let report = StartupDiscoveryReport {
        failed: vec![StartupDiscoveryProblem {
            server: "docs".to_owned(),
            message: "MCP server failed to connect: spawn failed".to_owned(),
        }],
        pending: vec!["slow".to_owned()],
        ..StartupDiscoveryReport::default()
    };
    let lines = plugin.report_startup_problems(&report);
    assert!(
        lines.iter().any(|line| line == "[ext] MCP server \"docs\" failed to start: MCP server failed to connect: spawn failed"),
        "the failure names the server and its bounded cause: {lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|line| line.contains("still connecting") && line.contains("slow")),
        "the pending server is named too: {lines:?}"
    );
    // A clean batch is silent.
    assert!(plugin
        .report_startup_problems(&StartupDiscoveryReport::default())
        .is_empty());
}

/// The cap does NOT silently disable later discovery: a server skipped at
/// startup is retried by the background continuation, and once connected its
/// tools land in the discovered snapshot and mark the registry dirty — the
/// same publication the first prompt would have seen.
///
/// Without the continuation the server would stay undiscovered until a tool
/// call happened to reach it, so this pins the "pending, not disabled"
/// contract of the bounded startup.
#[test]
fn a_pending_server_is_discovered_after_startup() {
    let manager = McpManager::without_log(&std::env::current_dir().expect("cwd"));
    // A handshake slower than the budget but far inside the per-server
    // timeout: skipped at startup, connected by the continuation.
    let manager = Arc::clone(&manager);
    register_slow_handshake(&manager, "late", 500);
    runtime().block_on(async {
        let report = manager
            .discover_all_bounded(Duration::from_millis(200))
            .await;
        assert_eq!(report.pending, vec!["late".to_owned()], "{report:?}");
        assert!(
            !report.snapshot.contains_key("late"),
            "nothing is published for the pending server yet: {report:?}"
        );
        // The continuation connects it; poll because the continuation is a
        // spawned task, not part of the batch's future.
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let discovered = manager.discovered_tools();
            if let Some(tools) = discovered.get("late") {
                assert_eq!(
                    tools
                        .iter()
                        .map(|tool| tool.tool.name.as_str())
                        .collect::<Vec<_>>(),
                    vec!["work"],
                    "the late server's tools were published"
                );
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the pending server was never discovered after startup"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            manager.take_tools_dirty(),
            "the late publication marks the registry dirty (its bridges register at the next rebuild)"
        );
        manager.shutdown().await;
    });
}

// ---------------------------------------------------------------------------
// The `mcp_servers` system-prompt section (CL18 audit item 11)
// ---------------------------------------------------------------------------
//
// Upstream renders the section on every `before_agent_start`
// (`extensions/mcp/index.ts:156-228`, `:1176-1184`); pir hard-wired
// `custom_sections: Vec::new()`, so the model never saw which servers were
// ready, failed or still connecting. These tests pin the builder path: the
// section is built from the MCP manager's SYNCHRONOUS status snapshot (no
// connect, no disk read per prompt), cached with the other prompt inputs, and
// absent entirely when MCP is off or has no server.

/// A session model fixture (the prompt path never streams in these tests).
pub(crate) fn prompt_test_model() -> pir::ai::types::Model {
    use pir::ai::types::{ModelCost, ModelCostRates};
    pir::ai::types::Model {
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
        context_window: 8192,
        max_tokens: 512,
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

/// Register an enabled server whose command cannot be spawned (an immediate
/// connect failure, the same shape `a_failing_server_is_reported_...` uses).
fn register_unstartable(manager: &McpManager, name: &str) {
    let config: McpServerConfig = serde_json::from_value(json!({
        "command": "pir-w19-cl27-definitely-not-a-command",
        "timeout": 5,
    }))
    .expect("failing server config");
    manager.registry().register(McpServerEntry {
        name: name.to_owned(),
        config,
        source: McpServerSource::Extension("cl27".to_owned()),
    });
}

/// Build a context-derived session around `plugin`, then return its system
/// prompt. `system_prompt: None` keeps the context builder as the owner.
fn session_prompt_with(plugin: Arc<McpPlugin>, cwd: &std::path::Path) -> (String, Arc<McpManager>) {
    let manager = Arc::clone(plugin.manager());
    let session = pir::agent_session::AgentSession::new_with_plugins(
        pir::agent_session::AgentSessionConfig {
            provider_registry: Arc::new(pir::ai::provider::ProviderRegistry::default()),
            model: prompt_test_model(),
            api_key: None,
            thinking: pir::ai::types::ThinkingLevel::Off,
            tools: pir::agent::tool_execution::ToolRegistry::new(),
            session: None,
            compaction: None,
            max_turns: 1,
            system_prompt: None,
            cwd: Some(cwd.to_path_buf()),
            event_stream: None,
            extensions: None,
            tool_selection: None,
        },
        vec![plugin as Arc<dyn AgentPlugin>],
    );
    let prompt = session
        .system_prompt_snapshot()
        .expect("the session always sends the base prompt");
    (prompt, manager)
}

/// A healthy and a failed server are both named in the session prompt, with
/// their states (upstream's `connected · N tools` / `failed: <reason>` shape).
#[test]
#[cfg_attr(target_os = "linux", ignore = "asserts Windows spawn-error text")]
fn the_session_prompt_lists_ready_and_failed_servers() {
    let tree = Tree::new("prompt-section");
    let agent_dir = tree.0.join("agent");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    let plugin = Arc::new(
        McpPlugin::with_cwd(&agent_dir, &tree.0).with_startup_budget(Duration::from_millis(500)),
    );
    register_mock(plugin.manager(), "healthy", MOCK_HEALTHY, 60, 0, &[]);
    register_unstartable(plugin.manager(), "broken");
    runtime().block_on(async {
        let report = plugin
            .manager()
            .discover_all_bounded(Duration::from_secs(30))
            .await;
        assert_eq!(report.failed.len(), 1, "{report:?}");

        let (prompt, manager) = session_prompt_with(Arc::clone(&plugin), &tree.0);
        assert!(
            prompt.contains("<mcp_servers>") && prompt.contains("</mcp_servers>"),
            "the session prompt carries the section: {prompt}"
        );
        assert!(
            prompt.contains("- mcp__healthy (direct): connected · 1 tool"),
            "the ready server and its tool count are listed: {prompt}"
        );
        assert!(
            prompt.contains("- mcp__broken (direct): failed: "),
            "the failed server and its bounded reason are listed: {prompt}"
        );
        assert!(
            prompt.contains("program not found"),
            "the reason carries the bounded cause (one line): {prompt}"
        );
        // The section never dumps a full config or a multi-line stderr tail.
        assert!(
            !prompt.contains("stderr:") && !prompt.contains("\"command\""),
            "no config/env/stderr dump: {prompt}"
        );
        manager.shutdown().await;
    });
}

/// A server still connecting when the startup budget elapsed is listed as
/// connecting, not silently dropped and not reported as failed.
#[test]
fn the_session_prompt_lists_a_pending_server_as_connecting() {
    let tree = Tree::new("prompt-pending");
    let agent_dir = tree.0.join("agent");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    let plugin = Arc::new(McpPlugin::with_cwd(&agent_dir, &tree.0));
    register_mock(plugin.manager(), "slow", MOCK_SILENT, 60, 0, &[]);
    runtime().block_on(async {
        let report = plugin
            .manager()
            .discover_all_bounded(Duration::from_millis(200))
            .await;
        assert_eq!(report.pending, vec!["slow".to_owned()], "{report:?}");

        let (prompt, manager) = session_prompt_with(Arc::clone(&plugin), &tree.0);
        assert!(
            prompt.contains("- mcp__slow (direct): connecting"),
            "the pending server is listed as connecting: {prompt}"
        );
        assert!(
            !prompt.contains("mcp__slow (direct): failed"),
            "a pending server is not a failure: {prompt}"
        );
        manager.shutdown().await;
    });
}

/// With MCP off (no `mcp` builtin on the host) the prompt has no
/// `mcp_servers` section, and it is byte-identical to the same prompt built
/// with no section — the section cannot perturb `<tools>`/`<rules>`/`<cwd>`.
#[test]
fn no_mcp_means_no_section_and_identical_bytes() {
    let tree = Tree::new("prompt-off");
    let session = pir::agent_session::AgentSession::new_with_plugins(
        pir::agent_session::AgentSessionConfig {
            provider_registry: Arc::new(pir::ai::provider::ProviderRegistry::default()),
            model: prompt_test_model(),
            api_key: None,
            thinking: pir::ai::types::ThinkingLevel::Off,
            tools: pir::agent::tool_execution::ToolRegistry::new(),
            session: None,
            compaction: None,
            max_turns: 1,
            system_prompt: None,
            cwd: Some(tree.0.clone()),
            event_stream: None,
            extensions: None,
            tool_selection: None,
        },
        Vec::new(),
    );
    let prompt = session
        .system_prompt_snapshot()
        .expect("the session always sends the base prompt");
    assert!(
        !prompt.contains("mcp_servers"),
        "no MCP on the host, no section: {prompt}"
    );
    // Byte-identical to the builder's MCP-less output for the same inputs.
    let expected =
        pir::coding_agent::context::session_system_prompt_with_hidden_and_options_and_inputs(
            &tree.0,
            &[],
            &[],
            &pir::coding_agent::context::ContextOptions {
                disabled: true,
                agent_dir: &tree.0,
                package_dir: None,
            },
        )
        .0;
    assert_eq!(
        prompt, expected,
        "an MCP-less prompt is unchanged by this feature"
    );
}

/// An enabled server with no connection started yet is `connecting` (previously
/// the model saw nothing at all about it).
#[test]
fn a_configured_never_connected_server_is_connecting() {
    let tree = Tree::new("prompt-idle");
    let agent_dir = tree.0.join("agent");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    let plugin = Arc::new(McpPlugin::with_cwd(&agent_dir, &tree.0));
    register_mock(plugin.manager(), "unused", MOCK_HEALTHY, 60, 0, &[]);
    let section = plugin.prompt_section().expect("a configured server lists");
    assert!(
        section.contains("- mcp__unused (direct): connecting"),
        "an idle configured server is listed as connecting: {section}"
    );
    // Disabled servers are the user's choice, not a row.
    let disabled: McpServerConfig = serde_json::from_value(json!({
        "command": "unused",
        "enabled": false,
    }))
    .expect("disabled config");
    plugin.manager().registry().register(McpServerEntry {
        name: "off".to_owned(),
        config: disabled,
        source: McpServerSource::Extension("cl27".to_owned()),
    });
    let section = plugin
        .prompt_section()
        .expect("the enabled server still lists");
    assert!(
        !section.contains("mcp__off"),
        "disabled servers are not listed: {section}"
    );
    runtime().block_on(plugin.manager().shutdown());
}

// ---------------------------------------------------------------------------
// E2: MCP result mapping — embedded resource blobs
// ---------------------------------------------------------------------------

fn embedded_blob(uri: &str, mime: &str, blob: &str) -> Value {
    json!({
        "type": "resource",
        "resource": { "uri": uri, "mimeType": mime, "blob": blob },
    })
}

/// An empty embedded blob carries no bytes: it must not create (and advertise
/// under `details.fullOutputPath`) an empty spill file.
#[test]
fn mcp_result_does_not_spill_or_advertise_an_empty_blob() {
    use pir::mcp::protocol::CallToolResult;
    use pir::plugins::builtin::convert_mcp_result;

    let result: CallToolResult = serde_json::from_value(json!({
        "content": [embedded_blob("file:///empty.bin", "application/octet-stream", "")],
    }))
    .expect("call result");
    let converted = convert_mcp_result("srv", "tool", result);
    let details = converted.details.expect("details");
    assert!(
        details.get("fullOutputPath").is_none(),
        "an empty blob must not advertise a spill file: {details}"
    );
    assert!(
        !converted.content.contains("saved to"),
        "no spill label for an empty blob: {}",
        converted.content
    );
    assert!(converted.images.is_empty());
    assert!(!converted.is_error);
}

/// `details.fullOutputPath` is singular: with two binary blobs it must
/// advertise the FIRST spill (the earlier file would otherwise be orphaned
/// and unreachable from `details`), while both labels keep their own path.
#[test]
fn mcp_result_advertises_the_first_binary_blob_spill() {
    use pir::mcp::protocol::CallToolResult;
    use pir::plugins::builtin::convert_mcp_result;

    // "first-blob" / "second-blob" in standard base64.
    let result: CallToolResult = serde_json::from_value(json!({
        "content": [
            embedded_blob("file:///a.bin", "application/octet-stream", "Zmlyc3QtYmxvYg=="),
            embedded_blob("file:///b.bin", "application/octet-stream", "c2Vjb25kLWJsb2I="),
        ],
    }))
    .expect("call result");
    let converted = convert_mcp_result("srv", "tool", result);
    let details = converted.details.expect("details");
    let advertised = details["fullOutputPath"]
        .as_str()
        .expect("a blob spill is advertised")
        .to_owned();

    let marker = "(saved to ";
    let spill_paths: Vec<String> = converted
        .content
        .match_indices(marker)
        .map(|(index, _)| {
            let start = index + marker.len();
            let end = converted.content[start..]
                .find(')')
                .map(|offset| start + offset)
                .expect("closing parenthesis");
            converted.content[start..end].to_owned()
        })
        .collect();
    assert_eq!(
        spill_paths.len(),
        2,
        "both blobs spill: {}",
        converted.content
    );
    assert_ne!(spill_paths[0], spill_paths[1]);
    assert_eq!(
        advertised, spill_paths[0],
        "details must advertise the first blob's spill, not the last"
    );

    for path in spill_paths {
        let _ = std::fs::remove_file(path);
    }
}

// ---------------------------------------------------------------------------
// E6: a completed close owns the child teardown; the second close must not
// re-signal a pid the transport guard already reaped
// ---------------------------------------------------------------------------

#[cfg(windows)]
fn mock_child_alive(pid: u32) -> bool {
    use std::os::windows::process::CommandExt;

    let Ok(output) = std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
        .creation_flags(0x0800_0000)
        .output()
    else {
        // Conservative: an unreadable probe never claims the child is gone.
        return true;
    };
    String::from_utf8_lossy(&output.stdout).contains(&format!("\"{pid}\""))
}

#[cfg(unix)]
fn mock_child_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn read_mock_pid(path: &std::path::Path) -> u32 {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if let Ok(text) = std::fs::read_to_string(path) {
            if let Ok(pid) = text.trim().parse::<u32>() {
                return pid;
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("the mock child never wrote its pid to {}", path.display());
}

/// `close()` completed the rmcp service shutdown, which drops the transport
/// and runs the stdio `ChildGuard`'s tree kill + reap. The client-side
/// pid-based sweep must therefore NOT be repeated (a recycled pid would
/// receive it); the guard alone leaves no child, and a second `close()` is a
/// no-op that still allows a reconnect.
#[test]
fn closing_a_client_twice_leaves_no_stdio_child_and_reconnects() {
    let tree = Tree::new("close-idempotent");
    let pid_file = tree.0.join("child.pid");
    let pid_path = pid_file.to_string_lossy().into_owned();
    let manager = McpManager::without_log(&tree.0);
    register_mock(
        &manager,
        "closer",
        MOCK_HEALTHY,
        20,
        0,
        &[(PID_FILE_ENV, pid_path.as_str())],
    );

    runtime().block_on(async {
        let client = manager.get_client("closer").await.expect("connect");
        let pid = read_mock_pid(&pid_file);
        assert!(mock_child_alive(pid), "the mock child starts alive");

        client.close().await;
        // Idempotent: the service slot is already empty and the transport
        // guard already reaped the child.
        client.close().await;

        let deadline = Instant::now() + Duration::from_secs(5);
        while mock_child_alive(pid) && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(
            !mock_child_alive(pid),
            "the closed client's stdio child must be killed and reaped"
        );

        let again = manager
            .get_client("closer")
            .await
            .expect("a use after close is a fresh start");
        assert!(
            again.list_all_tools().await.is_ok(),
            "the replacement client still works"
        );
        manager.shutdown().await;
    });
}
