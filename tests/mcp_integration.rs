//! MCP integration tests against the rmcp-backed client:
//! - a mock stdio MCP server run in-process (the test binary re-invoked with
//!   a dedicated entry test, per the in-process fixture preference), driven
//!   through the real rmcp stdio transport: initialize handshake, tools/list,
//!   tools/call round-trip, resources/prompts, ping, notification forwarding;
//! - the streamable HTTP transport against a hand-rolled TCP test server
//!   speaking the MCP wire protocol (session-id header handling, SSE response
//!   streaming, 202 notifications);
//! - lazy connect: plugin registration never spawns a server; session-start
//!   discovery does, and the discovered tools join the registry as
//!   `mcp__<server>__<tool>`;
//! - `mcp_servers_change` dispatch to JS extensions + `registerMcpServer` /
//!   `getMcpServers`;
//! - mid-session MCP synchronization: the definition-level dirty comparison
//!   that decides whether the plugin registry rebuilds, and the `mcp.json`
//!   config-sync follow-up (a removed or changed entry releases the snapshot,
//!   connection and bridges derived from the old entry);
//! - the `pir mcp check` CLI path.

use std::collections::BTreeMap;
use std::io::{BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde_json::{json, Value};

use pir::agent::tool_execution::{ToolContext, ToolExposure};
use pir::agent::AgentTool;
use pir::config::mcp::McpServerConfig;
use pir::mcp::client::McpClient;
use pir::mcp::error::McpError;
use pir::mcp::manager::{McpManager, ServerReadiness};
use pir::mcp::registry::{McpServerEntry, McpServerSource};
use pir::mcp::stdio::StdioSpawnOptions;
use pir::plugins::builtin::{McpPlugin, McpResourceTool, McpResourceToolKind};
use pir::plugins::services::{
    provider_auth_hook_registry, AuthPromptContext, ProviderAuth, ProviderAuthError,
    ProviderAuthHooks, ServiceFuture,
};
use pir::plugins::PluginHost;

// Keep the helper source type-checked even before the `[[bin]]` manifest entry
// exists (and let the in-binary child entries delegate to it later). It is
// self-contained (std + serde_json) and its `main` is dead code here.
#[path = "support/mock_mcp_server.rs"]
#[allow(dead_code)]
mod mock_mcp_helper;

// ---------------------------------------------------------------------------
// Environment serialization (env-var-dependent tests share one process)
// ---------------------------------------------------------------------------

static ENV_LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

async fn env_guard() -> tokio::sync::MutexGuard<'static, ()> {
    ENV_LOCK
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-mcp-int-{label}-{}-{}",
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

// ---------------------------------------------------------------------------
// The mock stdio MCP server (child entry of this very test binary)
// ---------------------------------------------------------------------------

/// Command + args that spawn a fresh mock MCP server. When the small
/// `pir-mock-mcp` helper binary exists (Cargo `[[bin]]`, see
/// `tests/support/mock_mcp_server.rs`) it is preferred: the mock child no
/// longer re-executes this ~330 MB test binary. `option_env!` keeps the tree
/// working before the manifest entry lands (fallback: the in-binary child).
fn mock_server_spawn() -> (String, Vec<String>) {
    if let Some(exe) = option_env!("CARGO_BIN_EXE_pir-mock-mcp") {
        return (exe.to_owned(), Vec::new());
    }
    let exe = std::env::current_exe().expect("current exe");
    (
        exe.to_string_lossy().into_owned(),
        vec![
            "mock_mcp_server_child".to_owned(),
            "--exact".to_owned(),
            "--nocapture".to_owned(),
        ],
    )
}

/// Command + args for the noisy-output child (garbage-line scenarios).
fn raw_server_spawn() -> (String, Vec<String>) {
    mock_server_spawn()
}

/// A silent child (handshake-failure lifecycle scenarios): never answers
/// anything, holds stdin open, and writes a sentinel file on exit so the
/// test can assert the process was actually terminated.
#[test]
fn mock_mcp_silent_child() {
    if std::env::var("PIR_MOCK_MCP_SILENT").as_deref() != Ok("1") {
        return;
    }
    let sentinel = std::env::var("PIR_MOCK_SENTINEL").unwrap_or_default();
    // Record this pid before blocking so the parent can prove termination even
    // when the teardown is a force-kill (which cannot write the sentinel).
    if let Ok(pid_file) = std::env::var("PIR_MOCK_PID") {
        let _ = std::fs::write(pid_file, std::process::id().to_string());
    }
    // Block until stdin closes (EOF) or the process is killed; a surviving
    // child never writes the sentinel.
    let mut sink = String::new();
    let _ = std::io::stdin().read_to_string(&mut sink);
    if !sentinel.is_empty() {
        let _ = std::fs::write(&sentinel, b"exited");
    }
}

fn mock_server_env() -> BTreeMap<String, String> {
    BTreeMap::from([("PIR_MOCK_MCP_SERVER".to_owned(), "1".to_owned())])
}

/// The request loop shared by the mock children: answers the MCP methods
/// over line-delimited JSON-RPC on stdin/stdout. Non-JSON lines (the libtest
/// banner, garbage) are skipped by the rmcp codec.
fn serve_mock_requests() {
    let stdin = std::io::stdin();
    // Optional behaviors driven by env (test scenarios):
    let delay_ms: u64 = std::env::var("PIR_MOCK_DELAY_MS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let protocol_version =
        std::env::var("PIR_MOCK_PROTOCOL_VERSION").unwrap_or_else(|_| "2025-06-18".to_owned());
    let paged = std::env::var("PIR_MOCK_PAGED").ok();
    let notify_tools_list_changed = std::env::var("PIR_MOCK_NOTIFY_TOOLS").as_deref() == Ok("1");
    // Third-party servers omit the spec-required `name` on resources and
    // templates; the client must keep listing them (upstream falls back to the
    // URI / URI template).
    let no_resource_name = std::env::var("PIR_MOCK_NO_RESOURCE_NAME").as_deref() == Ok("1");
    // Readiness-cache scenarios: count `tools/list` requests in a file (the
    // child is a separate process, so the counter must be observable from the
    // parent), and let the parent trigger ONE `tools/list_changed` on demand
    // by creating `PIR_MOCK_NOTIFY_FILE`; the child acknowledges with
    // `PIR_MOCK_NOTIFY_SENT` after writing the notification.
    let list_counter = std::env::var("PIR_MOCK_LIST_COUNTER").ok();
    if let Ok(trigger) = std::env::var("PIR_MOCK_NOTIFY_FILE") {
        let sent = std::env::var("PIR_MOCK_NOTIFY_SENT").ok();
        std::thread::spawn(move || {
            while !std::path::Path::new(&trigger).exists() {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            let _ = std::fs::remove_file(&trigger);
            write_mock_line(
                &std::io::stdout(),
                &json!({ "jsonrpc": "2.0", "method": "notifications/tools/list_changed" }),
            );
            if let Some(path) = sent {
                let _ = std::fs::write(path, b"sent");
            }
        });
    }
    // Stderr-drain scenarios: write a startup line and expose the `noisy`
    // tool, whose call writes another stderr line and exits on a write
    // failure (a closed pipe means the parent's drain died).
    let stderr_mode = std::env::var("PIR_MOCK_STDERR").as_deref() == Ok("1");
    if stderr_mode {
        let _ = emit_mock_stderr("mock-stderr:startup");
    }
    // Dispatch-deadline scenario: answer `initialize`, then stop reading stdin
    // entirely (the parent's writes eventually block on the pipe/channel).
    let stall_after_init = std::env::var("PIR_MOCK_STALL_AFTER_INIT").as_deref() == Ok("1");
    let mut requests = 0usize;
    let probe_enabled = std::env::var("PIR_MOCK_PROBE_METHOD").is_ok();
    let progress_enabled = std::env::var("PIR_MOCK_PROGRESS_MS").is_ok()
        || std::env::var("PIR_MOCK_PROGRESS_COUNT").is_ok();
    let mut reader = stdin.lock();
    let mut line = String::new();
    loop {
        line.clear();
        let Ok(read) = reader.read_line(&mut line) else {
            break;
        };
        if read == 0 {
            break;
        }
        requests += 1;
        if requests > 10_000 {
            break;
        }
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(id) = value.get("id").cloned() else {
            continue; // notification: nothing to answer
        };
        let method = value
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let params = value.get("params").cloned().unwrap_or(Value::Null);
        let delayed = delay_ms > 0
            && method == "tools/call"
            && params.get("name").and_then(Value::as_str) == Some("slow");
        let reply = match method.as_str() {
            "initialize" => json!({
                "protocolVersion": protocol_version,
                "capabilities": { "tools": {}, "resources": {}, "prompts": {} },
                "serverInfo": { "name": "mock", "version": "1.0.0" },
                "instructions": "Mock server instructions",
            }),
            "ping" => json!({}),
            "tools/list" => {
                if let Some(path) = &list_counter {
                    // One line per request; the parent counts them.
                    if let Ok(mut file) = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(path)
                    {
                        let _ = writeln!(file, "tools/list");
                    }
                }
                if notify_tools_list_changed {
                    // Protocol-noise scenarios: the server pushes a
                    // tools/list_changed notification before its answer.
                    write_mock_line(
                        &std::io::stdout(),
                        &json!({ "jsonrpc": "2.0", "method": "notifications/tools/list_changed" }),
                    );
                }
                if let Some(mode) = &paged {
                    // Pagination scenarios: `dup` repeats a cursor forever,
                    // `cap` walks an unbounded cursor sequence.
                    let next = match mode.as_str() {
                        "dup" => Some("dup".to_owned()),
                        "cap" => {
                            let page = params
                                .get("cursor")
                                .and_then(Value::as_str)
                                .and_then(|cursor| cursor.strip_prefix('p'))
                                .and_then(|number| number.parse::<u64>().ok())
                                .unwrap_or(0);
                            Some(format!("p{}", page + 1))
                        }
                        _ => None,
                    };
                    let mut page = json!({ "tools": [{ "name": "paged", "inputSchema": {} }] });
                    if let Some(next) = next {
                        page["nextCursor"] = json!(next);
                    }
                    page
                } else {
                    let mut tools = vec![
                        json!({
                            "name": "echo",
                            "description": "Echo the input text",
                            "inputSchema": {
                                "type": "object",
                                "properties": { "text": { "type": "string" } },
                                "required": ["text"],
                            },
                        }),
                        json!({
                            "name": "fail",
                            "description": "Always errors",
                            "inputSchema": { "type": "object" },
                        }),
                        json!({
                            "name": "slow",
                            "description": "Echoes after a delay",
                            "inputSchema": { "type": "object" },
                        }),
                    ];
                    if probe_enabled {
                        tools.push(json!({
                            "name": "probe",
                            "description": "Asks the client a server-initiated request",
                            "inputSchema": { "type": "object" },
                        }));
                    }
                    if progress_enabled {
                        tools.push(json!({
                            "name": "progress",
                            "description": "Reports progress before answering",
                            "inputSchema": { "type": "object" },
                        }));
                    }
                    if stderr_mode {
                        tools.push(json!({
                            "name": "noisy",
                            "description": "Writes stderr, then answers",
                            "inputSchema": { "type": "object" },
                        }));
                    }
                    json!({ "tools": tools })
                }
            }
            "tools/call" => {
                let name = params.get("name").and_then(Value::as_str).unwrap_or("");
                if name == "fail" {
                    json!({ "content": [{ "type": "text", "text": "boom" }], "isError": true })
                } else if name == "probe" {
                    // Server-initiated request coverage for the client's
                    // roots/list + setRequestHandler surface: ask the client
                    // and return its whole response object as the tool text.
                    let method = std::env::var("PIR_MOCK_PROBE_METHOD")
                        .unwrap_or_else(|_| "roots/list".to_owned());
                    let probe_params = std::env::var("PIR_MOCK_PROBE_PARAMS")
                        .ok()
                        .and_then(|text| serde_json::from_str::<Value>(&text).ok());
                    let answer = probe_client(&mut reader, &method, probe_params);
                    json!({
                        "content": [{ "type": "text", "text": answer.to_string() }],
                    })
                } else if name == "progress" {
                    // Send `notifications/progress` for the request's token
                    // before answering (per-request progress coverage).
                    let interval: u64 = std::env::var("PIR_MOCK_PROGRESS_MS")
                        .ok()
                        .and_then(|value| value.parse().ok())
                        .unwrap_or(40);
                    let count: u64 = std::env::var("PIR_MOCK_PROGRESS_COUNT")
                        .ok()
                        .and_then(|value| value.parse().ok())
                        .unwrap_or(3);
                    if let Some(token) = params.pointer("/_meta/progressToken") {
                        for step in 1..=count {
                            write_mock_line(
                                &std::io::stdout(),
                                &json!({
                                    "jsonrpc": "2.0",
                                    "method": "notifications/progress",
                                    "params": {
                                        "progressToken": token,
                                        "progress": step,
                                        "total": count,
                                        "message": format!("step {step}"),
                                    },
                                }),
                            );
                            std::thread::sleep(std::time::Duration::from_millis(interval));
                        }
                    }
                    json!({
                        "content": [{ "type": "text", "text": format!("progress:{count}") }],
                    })
                } else if name == "noisy" {
                    // Liveness probe for the stderr drain: a dead drain has
                    // closed the pipe, so this write fails and the child dies
                    // (which the caller observes as a broken connection).
                    if !emit_mock_stderr("mock-stderr:call") {
                        std::process::exit(3);
                    }
                    json!({ "content": [{ "type": "text", "text": "noisy-ok" }] })
                } else {
                    let args = params.get("arguments").cloned().unwrap_or(json!({}));
                    json!({
                        "content": [{
                            "type": "text",
                            "text": format!("echo:{}", args.get("text").and_then(Value::as_str).unwrap_or("")),
                        }],
                    })
                }
            }
            "resources/list" => {
                if no_resource_name {
                    json!({
                        "resources": [{ "uri": "mock://res/1", "mimeType": "text/plain" }],
                    })
                } else {
                    json!({
                        "resources": [{ "uri": "mock://res/1", "name": "one", "mimeType": "text/plain" }],
                    })
                }
            }
            "resources/templates/list" => {
                if std::env::var("PIR_MOCK_NO_TEMPLATES").as_deref() == Ok("1") {
                    // A server that does not implement the method: the client
                    // maps the answer to an empty listing (upstream
                    // `withoutTemplates`).
                    write_mock_line(
                        &std::io::stdout(),
                        &json!({ "jsonrpc": "2.0", "id": id.clone(), "error": { "code": -32601, "message": "method not found" } }),
                    );
                    continue;
                }
                let mut template = json!({
                    "uriTemplate": "mock://res/{id}",
                    "title": "One",
                    "description": "A mock template",
                    "mimeType": "text/plain",
                });
                if !no_resource_name {
                    template["name"] = json!("one");
                }
                json!({ "resourceTemplates": [template] })
            }
            "resources/read" => json!({
                "contents": [{ "uri": "mock://res/1", "mimeType": "text/plain", "text": "hello" }],
            }),
            "prompts/list" => json!({ "prompts": [{ "name": "greet" }] }),
            "prompts/get" => json!({
                "messages": [{ "role": "user", "content": { "type": "text", "text": "hi" } }],
            }),
            _ => {
                write_mock_line(
                    &std::io::stdout(),
                    &json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": "method not found" } }),
                );
                continue;
            }
        };
        let response = json!({ "jsonrpc": "2.0", "id": id, "result": reply });
        if delayed {
            // Reply from a side thread so a later request's answer overtakes
            // the stalled one (interleaving scenarios).
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(delay_ms));
                write_mock_line(&std::io::stdout(), &response);
            });
        } else {
            write_mock_line(&std::io::stdout(), &response);
        }
        if stall_after_init && method == "initialize" {
            // Keep the pipes open but never consume another request: the
            // parent's bounded peer channel + stdin pipe fill and its next
            // dispatch has to wait for writability.
            loop {
                std::thread::sleep(Duration::from_secs(60));
            }
        }
    }
}

/// The child entry: with `PIR_MOCK_MCP_SERVER=1` this test runs a blocking
/// JSON-RPC loop over stdin/stdout instead of returning.
#[test]
fn mock_mcp_server_child() {
    if std::env::var("PIR_MOCK_MCP_SERVER").as_deref() != Ok("1") {
        return;
    }
    serve_mock_requests();
}

/// A garbage-then-good stdout child: writes an unterminated run of
/// `PIR_MOCK_RAW_LEN` bytes and an invalid JSON line BEFORE answering
/// anything, then serves the standard mock methods. The rmcp codec must skip
/// the garbage and keep the session working.
#[test]
fn mock_mcp_raw_child() {
    if std::env::var("PIR_MOCK_MCP_RAW").as_deref() != Ok("1") {
        return;
    }
    let length: usize = std::env::var("PIR_MOCK_RAW_LEN")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(2048);
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let _ = out.write_all(&vec![b'x'; length]);
    let _ = out.write_all(b"\n");
    let _ = out.write_all(b"this is not json\n");
    let _ = out.flush();
    serve_mock_requests();
}

fn write_mock_line(stdout: &std::io::Stdout, reply: &Value) {
    let mut out = stdout.lock();
    let _ = writeln!(out, "{reply}");
    let _ = out.flush();
}

/// Send one server-initiated request to the client and wait for its response.
///
/// The mock loop owns stdin; this helper reads from the same reader so a
/// response cannot be consumed by the outer loop. Any other request that
/// arrives meanwhile is answered with method-not-found so it cannot hang.
fn probe_client(reader: &mut impl BufRead, method: &str, params: Option<Value>) -> Value {
    let id = format!("probe-{method}");
    write_mock_line(
        &std::io::stdout(),
        &json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }),
    );
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => return json!({ "error": "client closed" }),
            Ok(_) => {}
        }
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if value.get("id").and_then(Value::as_str) == Some(id.as_str()) {
            return value;
        }
        if value.get("method").is_some() {
            if let Some(other_id) = value.get("id") {
                write_mock_line(
                    &std::io::stdout(),
                    &json!({
                        "jsonrpc": "2.0",
                        "id": other_id,
                        "error": { "code": -32601, "message": "method not found" },
                    }),
                );
            }
        }
    }
}

/// Write one stderr line in the mock child; false when the pipe is closed
/// (the parent's drain died, which is exactly the regression under test).
fn emit_mock_stderr(line: &str) -> bool {
    let mut stderr = std::io::stderr();
    writeln!(stderr, "{line}")
        .and_then(|()| stderr.flush())
        .is_ok()
}

// ---------------------------------------------------------------------------
// Stdio client round-trip through the rmcp transport
// ---------------------------------------------------------------------------

async fn connect_mock_stdio() -> (Arc<McpClient>, Arc<McpManager>) {
    connect_mock_stdio_env(&[]).await
}

/// Connect the mock server with extra environment pairs merged into the
/// child's environment (scenario switches like `PIR_MOCK_NO_TEMPLATES`).
async fn connect_mock_stdio_env(extra_env: &[(&str, &str)]) -> (Arc<McpClient>, Arc<McpManager>) {
    let manager = McpManager::without_log(&std::env::current_dir().expect("cwd"));
    let (command, args) = mock_server_spawn();
    let mut env = BTreeMap::from([("PIR_MOCK_MCP_SERVER".to_owned(), "1".to_owned())]);
    for (key, value) in extra_env {
        env.insert((*key).to_owned(), (*value).to_owned());
    }
    let config: McpServerConfig = serde_json::from_value(json!({
        "command": command,
        "args": args,
        "env": env,
        "timeout": 20,
    }))
    .expect("config");
    manager.registry().register(McpServerEntry {
        name: "mock".into(),
        config,
        source: pir::mcp::registry::McpServerSource::Extension("test".into()),
    });
    let client = manager.get_client("mock").await.expect("connect");
    (client, manager)
}

#[tokio::test]
async fn stdio_client_handshake_and_round_trip() {
    let (client, manager) = connect_mock_stdio().await;
    // The child answers initialize with 2025-06-18; the client records it.
    assert_eq!(client.protocol_version(), Some("2025-06-18"));
    assert!(client.server_capabilities().is_some());
    assert_eq!(
        client.instructions(),
        Some("Mock server instructions"),
        "server instructions survive the handshake"
    );
    assert!(
        client.server_info().is_some_and(|info| info.name == "mock"),
        "server identity survives the handshake"
    );

    let tools = client.list_all_tools().await.expect("tools/list");
    let names: Vec<&str> = tools.iter().map(|tool| tool.name.as_str()).collect();
    assert_eq!(names, vec!["echo", "fail", "slow"]);
    let echo = &tools[0];
    assert_eq!(
        echo.input_schema["required"],
        json!(["text"]),
        "JSON-schema parameters arrive from tools/list"
    );

    let ping = client.ping().await;
    assert!(ping.is_ok(), "ping round-trip");

    let call = client
        .call_tool("echo", &json!({ "text": "hi" }))
        .await
        .expect("tools/call");
    assert!(!call.is_error.unwrap_or(false));
    assert!(
        !call.content.is_empty(),
        "content blocks: {:?}",
        call.content
    );
    let text = call
        .content
        .first()
        .and_then(|block| block.text.clone())
        .expect("text block");
    assert_eq!(text, "echo:hi");

    let failure = client
        .call_tool("fail", &json!({}))
        .await
        .expect("error results are results, not transport errors");
    assert_eq!(failure.is_error, Some(true));

    let resources = client.list_resources(None).await.expect("resources/list");
    assert_eq!(resources.resources.len(), 1);
    assert_eq!(resources.resources[0].uri, "mock://res/1");
    let templates = client
        .list_resource_templates(None)
        .await
        .expect("resources/templates/list");
    assert_eq!(templates.resource_templates.len(), 1);
    assert_eq!(
        templates.resource_templates[0].uri_template,
        "mock://res/{id}"
    );
    assert_eq!(templates.resource_templates[0].name, "one");
    assert_eq!(
        templates.resource_templates[0].title.as_deref(),
        Some("One")
    );
    assert_eq!(
        templates.resource_templates[0].mime_type.as_deref(),
        Some("text/plain")
    );
    assert!(templates.next_cursor.is_none());
    let read = client
        .read_resource("mock://res/1")
        .await
        .expect("resources/read");
    assert_eq!(read.contents[0].text.as_deref(), Some("hello"));

    let prompts = client.list_prompts(None).await.expect("prompts/list");
    assert_eq!(prompts.prompts.len(), 1);
    let prompt = client
        .get_prompt("greet", &json!({}))
        .await
        .expect("prompts/get");
    assert_eq!(prompt.messages.len(), 1);

    // Child lifecycle: shutdown closes the transport, so further requests
    // reconnect cleanly against a fresh mock server (no zombies: the old
    // child is killed and reaped by rmcp's close path plus the tree sweep).
    manager.shutdown().await;
    let states = manager.connection_states().await;
    assert!(
        states.is_empty(),
        "shutdown clears the connections: {states:?}"
    );
}

/// rmcp's `Resource`/`ResourceTemplate` require `name`, so a server that
/// omits it used to fail the whole page. Upstream accepts the entry and names
/// it by its URI/URI template (`packages/mcp/src/client.ts:112-120`); pir does
/// the same at the client conversion.
#[tokio::test]
async fn stdio_resources_without_names_fall_back_to_uri() {
    let (client, manager) = connect_mock_stdio_env(&[("PIR_MOCK_NO_RESOURCE_NAME", "1")]).await;

    let resources = client
        .list_resources(None)
        .await
        .expect("a resource without a name is listed");
    assert_eq!(resources.resources.len(), 1);
    assert_eq!(resources.resources[0].uri, "mock://res/1");
    assert_eq!(
        resources.resources[0].name, "mock://res/1",
        "a missing name falls back to the URI"
    );
    assert_eq!(
        resources.resources[0].mime_type.as_deref(),
        Some("text/plain"),
        "the rest of the entry survives"
    );

    let templates = client
        .list_resource_templates(None)
        .await
        .expect("a template without a name is listed");
    assert_eq!(templates.resource_templates.len(), 1);
    assert_eq!(
        templates.resource_templates[0].uri_template,
        "mock://res/{id}"
    );
    assert_eq!(
        templates.resource_templates[0].name, "mock://res/{id}",
        "a missing name falls back to the URI template"
    );
    assert_eq!(
        templates.resource_templates[0].title.as_deref(),
        Some("One"),
        "the rest of the template survives"
    );

    let read = client
        .read_resource("mock://res/1")
        .await
        .expect("resources/read");
    assert_eq!(read.contents[0].text.as_deref(), Some("hello"));
    manager.shutdown().await;
}

/// The aggregate resource tools keep listing a server whose entries omit
/// `name`: the payload carries the URI/URI-template fallback, not an error.
#[tokio::test]
async fn aggregate_resource_tools_list_servers_without_names() {
    let (_client, manager) = connect_mock_stdio_env(&[("PIR_MOCK_NO_RESOURCE_NAME", "1")]).await;
    let ctx = ToolContext::new(std::env::current_dir().expect("cwd"));

    let resources_tool = McpResourceTool::new(
        Arc::clone(&manager),
        McpResourceToolKind::ListResources,
        ToolExposure::Direct,
    );
    let result = resources_tool.run(&json!({}), &ctx).await;
    assert!(!result.is_error, "{}", result.content);
    let payload = result.structured_content.expect("structured payload");
    assert_eq!(payload["resources"][0]["uri"], "mock://res/1");
    assert_eq!(payload["resources"][0]["name"], "mock://res/1");

    let templates_tool = McpResourceTool::new(
        Arc::clone(&manager),
        McpResourceToolKind::ListResourceTemplates,
        ToolExposure::Direct,
    );
    let result = templates_tool.run(&json!({}), &ctx).await;
    assert!(!result.is_error, "{}", result.content);
    let payload = result.structured_content.expect("structured payload");
    assert_eq!(
        payload["resourceTemplates"][0]["uriTemplate"],
        "mock://res/{id}"
    );
    assert_eq!(payload["resourceTemplates"][0]["name"], "mock://res/{id}");
    manager.shutdown().await;
}

/// A server without `resources/templates/list` answers JSON-RPC
/// method-not-found; the client maps that to an empty listing (upstream
/// `withoutTemplates`), so one template-less server cannot fail a listing.
#[tokio::test]
async fn stdio_resource_templates_tolerate_servers_without_the_method() {
    let (client, manager) = connect_mock_stdio_env(&[("PIR_MOCK_NO_TEMPLATES", "1")]).await;
    let templates = client
        .list_resource_templates(None)
        .await
        .expect("method-not-found is tolerated");
    assert!(
        templates.resource_templates.is_empty(),
        "no templates: {templates:?}"
    );
    manager.shutdown().await;
}

/// `list_mcp_resource_templates` lists through the real
/// `resources/templates/list` round trip: the aggregate payload tags every
/// template with its server, and a named server selects one page.
#[tokio::test]
async fn aggregate_template_tool_lists_through_the_client() {
    let (_client, manager) = connect_mock_stdio().await;
    let tool = McpResourceTool::new(
        Arc::clone(&manager),
        McpResourceToolKind::ListResourceTemplates,
        ToolExposure::Direct,
    );
    let ctx = ToolContext::new(std::env::current_dir().expect("cwd"));
    let result = tool.run(&json!({}), &ctx).await;
    assert!(!result.is_error, "{}", result.content);
    let payload = result.structured_content.expect("structured payload");
    assert_eq!(payload["resourceTemplates"][0]["server"], "mock");
    assert_eq!(
        payload["resourceTemplates"][0]["uriTemplate"],
        "mock://res/{id}"
    );
    assert_eq!(payload["resourceTemplates"][0]["name"], "one");
    assert_eq!(payload["resourceTemplates"][0]["title"], "One");
    assert_eq!(payload["resourceTemplates"][0]["mimeType"], "text/plain");

    let result = tool.run(&json!({ "server": "mock" }), &ctx).await;
    assert!(!result.is_error, "{}", result.content);
    let payload = result.structured_content.expect("structured payload");
    assert_eq!(payload["server"], "mock");
    assert_eq!(payload["resourceTemplates"][0]["name"], "one");
    assert!(payload.get("nextCursor").is_none());
    manager.shutdown().await;
}

#[tokio::test]
async fn client_requests_fail_after_close() {
    let (command, args) = mock_server_spawn();
    let options = StdioSpawnOptions {
        command,
        args,
        cwd: std::env::current_dir().expect("cwd"),
        env: mock_server_env(),
        max_message_bytes: 1024 * 1024,
    };
    let client = McpClient::connect_stdio(options, None, "lifecycle", Duration::from_secs(20))
        .await
        .expect("handshake");
    assert!(client.ping().await.is_ok());
    client.close().await;
    // Requests after close fail with the typed closed error.
    let ping = client.ping().await;
    assert!(
        matches!(ping, Err(McpError::ConnectionClosed)),
        "ping after close: {ping:?}"
    );
    assert!(
        client.transport_is_closed().await,
        "the transport reports closed after close()"
    );
}

#[tokio::test]
async fn stdio_slow_call_does_not_block_a_concurrent_ping() {
    let (command, args) = mock_server_spawn();
    let mut env = mock_server_env();
    env.insert("PIR_MOCK_DELAY_MS".to_owned(), "500".to_owned());
    let options = StdioSpawnOptions {
        command,
        args,
        cwd: std::env::current_dir().expect("cwd"),
        env,
        max_message_bytes: 1024 * 1024,
    };
    let client = Arc::new(
        McpClient::connect_stdio(options, None, "p1", Duration::from_secs(20))
            .await
            .expect("handshake"),
    );
    // Park a slow request in flight, then verify a concurrent ping resolves
    // long before the stalled tool call (the transport does not serialize on
    // one pending response).
    let slow_client = Arc::clone(&client);
    let slow = tokio::spawn(async move {
        slow_client
            .call_tool("slow", &json!({ "text": "first" }))
            .await
    });
    let start = std::time::Instant::now();
    client
        .ping()
        .await
        .expect("ping resolves while a call is pending");
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_millis(400),
        "ping blocked behind the slow call for {elapsed:?}"
    );
    // The slow call still gets its own (delayed) response.
    let slow = slow.await.expect("join").expect("slow call");
    let slow_text = slow.content[0].text.clone().expect("text");
    assert_eq!(slow_text, "echo:first");
    client.close().await;
}

#[tokio::test]
async fn stdio_concurrent_requests_keep_their_own_responses() {
    let _guard = env_guard().await;
    let dir = TempDir::new("concurrent");
    std::env::set_var("PIR_AGENT_DIR", &dir.0);
    let manager = McpManager::without_log(&std::env::current_dir().expect("cwd"));
    let (command, args) = mock_server_spawn();
    let config: McpServerConfig = serde_json::from_value(json!({
        "command": command,
        "args": args,
        "env": {
            "PIR_MOCK_MCP_SERVER": "1",
            "PIR_MOCK_DELAY_MS": "300",
        },
        "timeout": 20,
    }))
    .expect("config");
    manager.registry().register(McpServerEntry {
        name: "mock".into(),
        config,
        source: pir::mcp::registry::McpServerSource::Extension("test".into()),
    });
    let client = manager.get_client("mock").await.expect("connect");
    // The slow call's answer arrives after the echo call's; both must get
    // their own response (id correlation, owned by rmcp now).
    let slow_args = json!({ "text": "first" });
    let echo_args = json!({ "text": "second" });
    let (slow, echo) = tokio::join!(
        client.call_tool("slow", &slow_args),
        client.call_tool("echo", &echo_args),
    );
    let slow = slow.expect("slow call");
    let echo = echo.expect("echo call");
    let slow_text = slow.content[0].text.clone().expect("text");
    let echo_text = echo.content[0].text.clone().expect("text");
    assert_eq!(slow_text, "echo:first", "slow got its own response");
    assert_eq!(echo_text, "echo:second", "echo got its own response");
    manager.shutdown().await;
}

#[tokio::test]
async fn stdio_tolerates_garbage_lines_within_the_frame_cap() {
    let (command, args) = raw_server_spawn();
    let mut env = BTreeMap::new();
    env.insert("PIR_MOCK_MCP_RAW".to_owned(), "1".to_owned());
    // A garbage line UNDER the 1 KiB frame cap: the codec drops it and the
    // session works end to end. A line OVER the cap is a protocol failure by
    // design and is covered by tests/mcp_frame_cap.rs.
    env.insert("PIR_MOCK_RAW_LEN".to_owned(), "512".to_owned());
    let options = StdioSpawnOptions {
        command,
        args,
        cwd: std::env::current_dir().expect("cwd"),
        env,
        max_message_bytes: 1024,
    };
    // The child spews 512 unterminated bytes and an invalid JSON line before
    // answering anything: the rmcp codec drops them and the session works end
    // to end.
    let client = McpClient::connect_stdio(options, None, "raw", Duration::from_secs(20))
        .await
        .expect("handshake survives garbage");
    let tools = client.list_all_tools().await.expect("tools/list");
    assert_eq!(tools.len(), 3, "valid responses survive the garbage");
    let call = client
        .call_tool("echo", &json!({ "text": "hi" }))
        .await
        .expect("tools/call after garbage");
    assert_eq!(
        call.content.first().and_then(|block| block.text.clone()),
        Some("echo:hi".to_owned())
    );
    client.close().await;
}

#[cfg(windows)]
#[tokio::test]
async fn stdio_close_kills_the_whole_process_tree() {
    use std::process::Command;
    // The server is a `.cmd` shim (what `npx`/`uvx` resolve to on Windows);
    // it runs a long-lived grandchild (ping) that a direct-child kill would
    // orphan. The shim never speaks MCP, so the handshake times out and the
    // failure path must sweep the whole tree.
    let dir = TempDir::new("tree");
    let script = dir.0.join("server.cmd");
    // The marker proves the shim reached the grandchild step: the handshake
    // failure path may sweep the tree synchronously (before `connect_stdio`
    // returns), so "ping.exe is still running" is NOT observable afterwards.
    let started_flag = dir.0.join("grandchild.started");
    std::fs::write(
        &script,
        format!(
            "@echo off\r\necho started > \"{}\"\r\nping -n 77 127.0.0.1 > NUL\r\n",
            started_flag.to_string_lossy()
        ),
    )
    .expect("write script");
    let options = StdioSpawnOptions {
        command: "cmd.exe".to_owned(),
        args: vec!["/c".to_owned(), script.to_string_lossy().into_owned()],
        cwd: dir.0.clone(),
        env: BTreeMap::new(),
        max_message_bytes: 1024 * 1024,
    };
    match McpClient::connect_stdio(options, None, "tree", Duration::from_secs(1)).await {
        Err(McpError::Timeout { .. }) => {}
        other => panic!("expected a typed handshake timeout, got {other:?}"),
    }

    fn ping_grandchild_running() -> bool {
        let output = Command::new("powershell")
            .args([
                "-NoProfile",
                "-Command",
                "Get-CimInstance Win32_Process -Filter \"Name='ping.exe'\" \
                 | Select-Object -ExpandProperty CommandLine",
            ])
            .output();
        match output {
            Ok(output) => String::from_utf8_lossy(&output.stdout).contains("-n 77"),
            Err(_) => false,
        }
    }
    assert!(
        started_flag.exists(),
        "precondition: the shim's grandchild started (marker file)"
    );
    // The tree kill reaches the grandchild; poll briefly for it to disappear.
    let mut gone = false;
    for _ in 0..12 {
        if !ping_grandchild_running() {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert!(gone, "the shim's grandchild outlived the failed handshake");
}

// ---------------------------------------------------------------------------
// Stderr drain: the child's stderr is actually read (tail + log) and stays
// open, so a server whose stderr write fails cannot kill the session.
// ---------------------------------------------------------------------------

/// Regression: the drain used a `Vec::with_capacity(1024)` as its read buffer;
/// `read` got a zero-length slice, returned `Ok(0)` at once, and the task
/// ended, dropping the read end of the child's stderr pipe. This drives the
/// full client path: the `noisy` tool writes a stderr line after the handshake
/// and exits if that write fails, so the call succeeding and the server still
/// answering prove the drain is alive and the pipe open.
#[tokio::test]
async fn stdio_stderr_drain_logs_lines_and_keeps_the_server_alive() {
    let _guard = env_guard().await;
    let dir = TempDir::new("stderr");
    let log_path = dir.0.join("mcp.log");
    let manager = McpManager::with_log(log_path.clone(), &std::env::current_dir().expect("cwd"));
    let (command, args) = mock_server_spawn();
    let config: McpServerConfig = serde_json::from_value(json!({
        "command": command,
        "args": args,
        "env": { "PIR_MOCK_MCP_SERVER": "1", "PIR_MOCK_STDERR": "1" },
        "timeout": 20,
    }))
    .expect("config");
    manager.registry().register(McpServerEntry {
        name: "noisy".into(),
        config,
        source: pir::mcp::registry::McpServerSource::Extension("test".into()),
    });
    let client = manager.get_client("noisy").await.expect("connect");

    let call = client
        .call_tool("noisy", &json!({}))
        .await
        .expect("the per-call stderr write must succeed (open pipe)");
    assert_eq!(call.content[0].text.as_deref(), Some("noisy-ok"));

    // Both the startup line and the per-call line were mirrored to the log.
    let mut logged = String::new();
    for _ in 0..80 {
        logged = std::fs::read_to_string(&log_path).unwrap_or_default();
        if logged.contains("mock-stderr:startup") && logged.contains("mock-stderr:call") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        logged.contains("mock-stderr:startup"),
        "the startup stderr chunk reached the log: {logged:?}"
    );
    assert!(
        logged.contains("mock-stderr:call"),
        "the per-call stderr chunk reached the log: {logged:?}"
    );

    // No reconnect storm: the same session is still connected and answering.
    assert!(
        client.ping().await.is_ok(),
        "server survives its own stderr writes"
    );
    assert_eq!(
        manager.connection_states().await.get("noisy"),
        Some(&pir::mcp::manager::ConnectionState::Connected)
    );
    manager.shutdown().await;
}

/// The drain also retains the bounded tail (`StdioConnection::stderr_tail`),
/// which the client's connect-failure diagnostics seam reads.
#[tokio::test]
async fn stdio_stderr_drain_fills_the_bounded_tail() {
    let dir = TempDir::new("stderr-tail");
    let log = Arc::new(pir::mcp::log::McpLog::new(dir.0.join("mcp.log")));
    let (command, args) = mock_server_spawn();
    let options = StdioSpawnOptions {
        command,
        args,
        cwd: std::env::current_dir().expect("cwd"),
        env: BTreeMap::from([
            ("PIR_MOCK_MCP_SERVER".to_owned(), "1".to_owned()),
            ("PIR_MOCK_STDERR".to_owned(), "1".to_owned()),
        ]),
        max_message_bytes: 1024 * 1024,
    };
    let connection = pir::mcp::stdio::spawn(options, Some(log), "tail").expect("spawn");
    let mut tail: Vec<u8> = Vec::new();
    for _ in 0..80 {
        tail = connection
            .stderr_tail
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if String::from_utf8_lossy(&tail).contains("mock-stderr:startup") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let text = String::from_utf8_lossy(&tail).into_owned();
    assert!(
        text.contains("mock-stderr:startup"),
        "the tail holds the drained stderr: {text:?}"
    );
    // The tail stays bounded by the documented cap (64 KiB).
    assert!(
        tail.len() <= 64 * 1024,
        "the tail is bounded, got {} bytes",
        tail.len()
    );
    drop(connection);
}

// ---------------------------------------------------------------------------
// Protocol version negotiation + pagination bounds
// ---------------------------------------------------------------------------

#[tokio::test]
async fn client_rejects_unsupported_protocol_versions() {
    let (command, args) = mock_server_spawn();
    let mut env = mock_server_env();
    env.insert(
        "PIR_MOCK_PROTOCOL_VERSION".to_owned(),
        "1999-01-01".to_owned(),
    );
    let options = StdioSpawnOptions {
        command,
        args,
        cwd: std::env::current_dir().expect("cwd"),
        env,
        max_message_bytes: 1024 * 1024,
    };
    match McpClient::connect_stdio(options, None, "version", Duration::from_secs(10)).await {
        Err(McpError::Protocol(message)) => {
            assert!(
                message.contains("unsupported protocol version") && message.contains("1999-01-01"),
                "rejection names the version: {message}"
            );
        }
        other => panic!("expected a protocol error, got {other:?}"),
    }
}

#[tokio::test]
async fn connect_failure_closes_the_transport_and_kills_the_child() {
    #[cfg(windows)]
    fn process_alive(pid: u32) -> bool {
        let output = std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
            .output();
        match output {
            Ok(output) => String::from_utf8_lossy(&output.stdout).contains(&format!("\"{pid}\"")),
            Err(_) => false,
        }
    }
    #[cfg(unix)]
    fn process_alive(pid: u32) -> bool {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }
    // A server that never answers initialize: the handshake times out, and
    // connect must tear the transport (and thereby the child process) down
    // instead of leaking a live server.
    let dir = TempDir::new("silent");
    let sentinel = dir.0.join("child-exited");
    let exe = std::env::current_exe().expect("current exe");
    let mut env = BTreeMap::new();
    env.insert("PIR_MOCK_MCP_SILENT".to_owned(), "1".to_owned());
    env.insert(
        "PIR_MOCK_SENTINEL".to_owned(),
        sentinel.to_string_lossy().into_owned(),
    );
    let pid_file = dir.0.join("child-pid");
    env.insert(
        "PIR_MOCK_PID".to_owned(),
        pid_file.to_string_lossy().into_owned(),
    );
    let options = StdioSpawnOptions {
        command: exe.to_string_lossy().into_owned(),
        args: vec![
            "mock_mcp_silent_child".to_owned(),
            "--exact".to_owned(),
            "--nocapture".to_owned(),
        ],
        cwd: std::env::current_dir().expect("cwd"),
        env,
        max_message_bytes: 1024 * 1024,
    };
    // The handshake budget must outlast the child's own boot (a fresh test
    // exe can take >300ms to reach its stdin read under AV scan / load):
    // the sentinel is only written by a child that reached its read loop
    // before the connect-timeout teardown, so a too-tight budget here makes
    // the teardown race unfalsifiable (the child dies mid-boot, force-killed,
    // and can never write the sentinel).
    let error =
        McpClient::connect_stdio(options, None, "silent", Duration::from_millis(1200)).await;
    let error = error.expect_err("initialize never answers");
    assert!(
        matches!(error, McpError::Timeout { .. }),
        "handshake failure is the typed timeout: {error:?}"
    );
    // The child was terminated. A graceful teardown (stdin EOF) writes the
    // sentinel; a force-kill cannot, so the pid recorded at startup is the
    // authoritative liveness check.
    let mut exited = false;
    let pid = std::fs::read_to_string(&pid_file)
        .ok()
        .and_then(|text| text.trim().parse::<u32>().ok());
    for _ in 0..48 {
        let sentinel_gone = sentinel.exists();
        let pid_gone = pid.is_some_and(|pid| !process_alive(pid));
        if sentinel_gone || pid_gone {
            exited = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert!(
        exited,
        "the stdio child was terminated after the handshake failure"
    );
}

#[tokio::test]
async fn list_all_tools_rejects_duplicate_cursors() {
    let (command, args) = mock_server_spawn();
    let mut env = mock_server_env();
    env.insert("PIR_MOCK_PAGED".to_owned(), "dup".to_owned());
    let options = StdioSpawnOptions {
        command,
        args,
        cwd: std::env::current_dir().expect("cwd"),
        env,
        max_message_bytes: 1024 * 1024,
    };
    let client = McpClient::connect_stdio(options, None, "paged", Duration::from_secs(20))
        .await
        .expect("handshake");
    let error = client
        .list_all_tools()
        .await
        .expect_err("a repeating cursor is an error, not a silent truncation");
    match &error {
        McpError::Protocol(message) => {
            assert!(
                message.contains("duplicate cursor"),
                "error names the duplicate cursor: {message}"
            );
        }
        other => panic!("expected a protocol error, got {other:?}"),
    }
    client.close().await;
}

#[tokio::test]
async fn list_all_tools_errors_at_the_page_cap() {
    let (command, args) = mock_server_spawn();
    let mut env = mock_server_env();
    env.insert("PIR_MOCK_PAGED".to_owned(), "cap".to_owned());
    let options = StdioSpawnOptions {
        command,
        args,
        cwd: std::env::current_dir().expect("cwd"),
        env,
        max_message_bytes: 1024 * 1024,
    };
    let client = McpClient::connect_stdio(options, None, "paged", Duration::from_secs(60))
        .await
        .expect("handshake");
    let error = client
        .list_all_tools()
        .await
        .expect_err("an unbounded cursor errors at the page cap");
    match &error {
        McpError::Protocol(message) => {
            assert!(
                message.contains("exceeded 1000 pages"),
                "error names the page cap: {message}"
            );
        }
        other => panic!("expected a protocol error, got {other:?}"),
    }
    client.close().await;
}

// ---------------------------------------------------------------------------
// Server notification forwarding
// ---------------------------------------------------------------------------

#[tokio::test]
async fn server_notifications_are_recorded_for_the_refresh_path() {
    let (command, args) = mock_server_spawn();
    let mut env = mock_server_env();
    env.insert("PIR_MOCK_NOTIFY_TOOLS".to_owned(), "1".to_owned());
    let options = StdioSpawnOptions {
        command,
        args,
        cwd: std::env::current_dir().expect("cwd"),
        env,
        max_message_bytes: 1024 * 1024,
    };
    let client = McpClient::connect_stdio(options, None, "notify", Duration::from_secs(20))
        .await
        .expect("handshake");
    let _ = client.list_all_tools().await.expect("tools/list");
    // The mock pushed notifications/tools/list_changed before its answer;
    // the client records it for the mcp_servers_change refresh path.
    let events = client.take_notifications().await;
    assert!(
        events.contains(&pir::mcp::client::ServerEvent::ToolsListChanged),
        "tools/list_changed is forwarded: {events:?}"
    );
    client.close().await;
}

// ---------------------------------------------------------------------------
// Lazy connect + plugin flow
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn plugin_registers_lazily_and_exposes_tools() {
    let _guard = env_guard().await;
    let dir = TempDir::new("plugin");
    std::env::set_var("PIR_AGENT_DIR", &dir.0);

    let (command, args) = mock_server_spawn();
    let plugin = Arc::new(McpPlugin::new());
    let host = PluginHost::build(None, vec![plugin.clone()]);

    // Registration must not have started any server (lazy connect).
    let states = plugin.manager().connection_states().await;
    assert!(
        states.is_empty(),
        "no server connects at registration: {states:?}"
    );
    assert!(
        host.tool_defs()
            .iter()
            .all(|def| !def.name.starts_with("mcp__")),
        "no mcp tools before discovery"
    );

    // Register a server directly into the shared registry (as
    // `mcp.json` loading would). The claimed source file must EXIST with the
    // same entry: every plugin rebuild runs `sync_config_files`, which
    // unregisters config-sourced entries whose name is absent from their
    // source file.
    let config_value = json!({
        "command": command,
        "args": args,
        "env": { "PIR_MOCK_MCP_SERVER": "1" },
        "timeout": 20,
    });
    std::fs::write(
        dir.0.join("mcp.json"),
        serde_json::json!({ "mock": config_value }).to_string(),
    )
    .expect("write mcp.json");
    let config: McpServerConfig = serde_json::from_value(config_value).expect("config");
    plugin.manager().registry().register(McpServerEntry {
        name: "mock".into(),
        config,
        source: pir::mcp::registry::McpServerSource::GlobalConfig(dir.0.join("mcp.json")),
    });

    // First use (session start): the server connects and its tools join
    // the registry under the upstream naming scheme.
    let mut payload = json!({ "type": "session_start" });
    host.emit("session_start", &mut payload)
        .await
        .expect("emit");

    let mut names: Vec<String> = host
        .tool_defs()
        .into_iter()
        .map(|def| def.name)
        .filter(|name| name.starts_with("mcp__"))
        .collect();
    names.sort();
    assert_eq!(
        names,
        vec!["mcp__mock__echo", "mcp__mock__fail", "mcp__mock__slow"],
        "discovered tools are registered as mcp__<server>__<tool>"
    );
    let echo_def = host
        .tool_defs()
        .into_iter()
        .find(|def| def.name == "mcp__mock__echo")
        .expect("echo tool def");
    assert_eq!(echo_def.execution_mode, "sequential");
    assert_eq!(
        echo_def.parameters["required"],
        json!(["text"]),
        "JSON-schema parameters from tools/list"
    );
    // Exposure mapping (upstream `toToolExposure`): the default `codemode`
    // exposure is not declared to the model, and tools share the
    // `mcp__<server>` namespace.
    assert_eq!(
        echo_def.exposure, "deferred",
        "MCP tools are not declared to the model by default"
    );
    assert_eq!(
        echo_def
            .namespace
            .as_ref()
            .map(|namespace| namespace.name.as_str()),
        Some("mcp__mock"),
        "tools carry their server's namespace"
    );

    // tools/call round-trip through the unified registry.
    let result = host
        .call_tool("mcp__mock__echo", "call-1", &json!({ "text": "hello" }))
        .await
        .expect("call_tool");
    let text = result
        .content
        .first()
        .map(|item| item.text.clone())
        .unwrap_or_default();
    assert_eq!(text, "echo:hello");

    // Server-side errors map onto error results.
    let error = host
        .call_tool("mcp__mock__fail", "call-2", &json!({}))
        .await
        .expect_err("isError results fail the unified call");
    assert!(error.to_string().contains("boom"), "error text: {error}");

    let states = plugin.manager().connection_states().await;
    assert_eq!(
        states.get("mock"),
        Some(&pir::mcp::manager::ConnectionState::Connected)
    );

    // Teardown kills the child.
    host.reload()
        .await
        .expect("reload triggers plugin shutdown hook");
    let states = plugin.manager().connection_states().await;
    assert!(states.is_empty(), "shutdown closes connections: {states:?}");
}

#[tokio::test(flavor = "current_thread")]
async fn plugin_maps_exposure_config_and_deferred_tools_stay_callable() {
    let _guard = env_guard().await;
    let dir = TempDir::new("exposure");
    std::env::set_var("PIR_AGENT_DIR", &dir.0);

    let (command, args) = mock_server_spawn();
    let plugin = Arc::new(McpPlugin::new());
    let host = PluginHost::build(None, vec![plugin.clone()]);
    let make_config = |exposure: &str| -> McpServerConfig {
        serde_json::from_value(json!({
            "command": command,
            "args": args,
            "env": { "PIR_MOCK_MCP_SERVER": "1" },
            "timeout": 20,
            "exposure": exposure,
        }))
        .expect("config")
    };
    // Both entries claim `dir/mcp.json` as their source, so the file must
    // actually contain them: every plugin rebuild runs `sync_config_files`,
    // which unregisters config-sourced entries absent from their file.
    std::fs::write(
        dir.0.join("mcp.json"),
        serde_json::json!({
            "exp-direct": make_config("direct"),
            "exp-deferred": make_config("deferred"),
        })
        .to_string(),
    )
    .expect("write mcp.json");
    plugin.manager().registry().register(McpServerEntry {
        name: "exp-direct".into(),
        config: make_config("direct"),
        source: pir::mcp::registry::McpServerSource::GlobalConfig(dir.0.join("mcp.json")),
    });
    plugin.manager().registry().register(McpServerEntry {
        name: "exp-deferred".into(),
        config: make_config("deferred"),
        source: pir::mcp::registry::McpServerSource::GlobalConfig(dir.0.join("mcp.json")),
    });

    let mut payload = json!({ "type": "session_start" });
    host.emit("session_start", &mut payload)
        .await
        .expect("emit");

    let defs = host.tool_defs();
    let direct = defs
        .iter()
        .find(|def| def.name == "mcp__exp_direct__echo")
        .expect("direct-exposure tool def");
    let deferred = defs
        .iter()
        .find(|def| def.name == "mcp__exp_deferred__echo")
        .expect("deferred-exposure tool def");
    assert_eq!(direct.exposure, "direct", "direct stays declared");
    assert_eq!(
        deferred.exposure, "deferred",
        "deferred is not declared to the model"
    );
    assert_eq!(
        deferred
            .namespace
            .as_ref()
            .map(|namespace| namespace.name.as_str()),
        Some("mcp__exp_deferred")
    );

    // A deferred tool is not declared to the model, but it is callable.
    let result = host
        .call_tool(
            "mcp__exp_deferred__echo",
            "call-deferred",
            &json!({ "text": "hi" }),
        )
        .await
        .expect("deferred tool is callable");
    let text = result
        .content
        .first()
        .map(|item| item.text.clone())
        .unwrap_or_default();
    assert_eq!(text, "echo:hi");

    host.reload().await.expect("teardown");
}

// ---------------------------------------------------------------------------
// HTTP transport against a hand-rolled MCP wire-protocol mock server
// ---------------------------------------------------------------------------

struct RecordedRequest {
    path: String,
    headers: BTreeMap<String, String>,
    body: Value,
}

type HttpHandler =
    Arc<dyn Fn(&RecordedRequest) -> (u16, Vec<(String, String)>, Vec<u8>) + Send + Sync>;

/// A minimal HTTP/1.1 test server: bounded request reads, keep-alive per
/// connection, content-length framing.
async fn serve_http(
    listener: tokio::net::TcpListener,
    handler: HttpHandler,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        let handler = handler.clone();
        let requests = requests.clone();
        tokio::spawn(async move {
            let mut buffer: Vec<u8> = Vec::with_capacity(2048);
            let mut chunk = [0u8; 4096];
            // Keep-alive: serve requests on this connection until it closes.
            loop {
                buffer.clear();
                // Read the head.
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
                            if let Some(position) = find_head_end(&buffer) {
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
                let head = String::from_utf8_lossy(&buffer[..head_end]).into_owned();
                let mut lines = head.lines();
                let request_line = lines.next().unwrap_or("").to_owned();
                let mut headers = BTreeMap::new();
                for line in lines {
                    if let Some((name, value)) = line.split_once(':') {
                        headers.insert(name.trim().to_lowercase(), value.trim().to_owned());
                    }
                }
                let content_length: usize = headers
                    .get("content-length")
                    .and_then(|value| value.parse().ok())
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
                let body = String::from_utf8_lossy(
                    &buffer[body_start..(body_start + content_length).min(buffer.len())],
                )
                .into_owned();
                let parsed: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
                let path = request_line
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("")
                    .to_owned();
                let recorded = RecordedRequest {
                    path,
                    headers,
                    body: parsed,
                };
                let (status, extra_headers, response_body) = handler(&recorded);
                requests
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(recorded);
                let status_text = if status == 200 || status == 202 {
                    "OK"
                } else {
                    "Error"
                };
                let mut response = format!("HTTP/1.1 {status} {status_text}\r\n");
                for (name, value) in &extra_headers {
                    response.push_str(&format!("{name}: {value}\r\n"));
                }
                response.push_str(&format!("content-length: {}\r\n", response_body.len()));
                response.push_str("connection: keep-alive\r\n");
                response.push_str("\r\n");
                let mut out: Vec<u8> = response.into_bytes();
                out.extend_from_slice(&response_body);
                if stream.try_write(&out).is_err() {
                    return;
                }
                // Give the client a beat to read the response off the socket
                // before we loop for the next request on the same
                // connection.
                let _ =
                    tokio::time::timeout(std::time::Duration::from_millis(20), stream.readable())
                        .await;
            }
        });
    }
}

fn find_head_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

fn json_response(
    body: Value,
    extra: Vec<(String, String)>,
) -> (u16, Vec<(String, String)>, Vec<u8>) {
    let mut headers = vec![("content-type".to_owned(), "application/json".to_owned())];
    headers.extend(extra);
    (
        200,
        headers,
        serde_json::to_vec(&body).expect("serialize response"),
    )
}

/// Spec-correct acknowledgement of a JSON-RPC notification over streamable
/// HTTP: 202 Accepted with no body.
fn accepted_response() -> (u16, Vec<(String, String)>, Vec<u8>) {
    (202, Vec::new(), Vec::new())
}

/// An MCP wire-protocol HTTP handler: initialize assigns a session id,
/// notifications are 202-acked, tools/call answers with an SSE-framed
/// response, and non-POST requests (rmcp's optional GET stream probe) get
/// the spec's 405.
fn wire_http_handler() -> HttpHandler {
    Arc::new(|request: &RecordedRequest| {
        if request.body.is_null() {
            // GET stream probe: no server-to-client stream in this mock.
            return (405, Vec::new(), b"method not allowed".to_vec());
        }
        let method = request
            .body
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("");
        let id = request.body.get("id").cloned().unwrap_or(Value::Null);
        let reply = |result: Value| {
            json_response(
                json!({ "jsonrpc": "2.0", "id": id, "result": result }),
                Vec::new(),
            )
        };
        match method {
            "initialize" => json_response(
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "protocolVersion": "2025-06-18",
                        "capabilities": { "tools": {} },
                        "serverInfo": { "name": "mock-http", "version": "1.0.0" },
                    },
                }),
                vec![("mcp-session-id".to_owned(), "sess-abc".to_owned())],
            ),
            "notifications/initialized" => accepted_response(),
            "ping" => reply(json!({})),
            "tools/list" => reply(json!({
                "tools": [{ "name": "stream_echo", "inputSchema": { "type": "object" } }],
            })),
            "tools/call" => {
                // SSE-framed response (streamable HTTP may answer with an
                // event stream).
                let event = json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "content": [{ "type": "text", "text": "streamed:hi" }],
                    },
                });
                let sse = format!("event: message\ndata: {event}\n\n");
                (
                    200,
                    vec![("content-type".to_owned(), "text/event-stream".to_owned())],
                    sse.into_bytes(),
                )
            }
            _ => (404, Vec::new(), b"not found".to_vec()),
        }
    })
}

#[tokio::test]
async fn http_transport_session_id_and_sse_streaming() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    let requests: Arc<Mutex<Vec<RecordedRequest>>> = Arc::new(Mutex::new(Vec::new()));
    let server = tokio::spawn(serve_http(listener, wire_http_handler(), requests.clone()));

    let client = McpClient::connect_http(
        format!("http://{addr}/mcp"),
        Vec::new(),
        Duration::from_secs(10),
    )
    .await
    .expect("handshake");

    // Session id captured from initialize and replayed on later requests.
    // (rmcp also probes the optional server-to-client GET stream; the mock
    // answers it with the spec's 405, so request counts beyond the two
    // handshake POSTs are not asserted.)
    {
        let requests = requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(requests[0].path, "/mcp");
        assert_eq!(
            requests[0].body.get("method").and_then(Value::as_str),
            Some("initialize")
        );
    }
    let tools = client.list_all_tools().await.expect("tools/list");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "stream_echo");
    {
        let requests = requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let listed = requests
            .iter()
            .find(|request| {
                request.body.get("method").and_then(Value::as_str) == Some("tools/list")
            })
            .expect("tools/list request recorded");
        assert_eq!(
            listed.headers.get("mcp-session-id").map(String::as_str),
            Some("sess-abc"),
            "the session id header is replayed: {:?}",
            listed.headers
        );
    }
    let call = client
        .call_tool("stream_echo", &json!({}))
        .await
        .expect("tools/call");
    let text = call
        .content
        .first()
        .and_then(|block| block.text.clone())
        .expect("text");
    assert_eq!(text, "streamed:hi", "SSE-framed responses are decoded");

    client.close().await;
    server.abort();
}

#[tokio::test]
async fn manager_checks_http_server() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    let requests: Arc<Mutex<Vec<RecordedRequest>>> = Arc::new(Mutex::new(Vec::new()));
    let handler: HttpHandler = Arc::new(|request: &RecordedRequest| {
        if request.body.is_null() {
            return (405, Vec::new(), b"method not allowed".to_vec());
        }
        let method = request
            .body
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("");
        let id = request.body.get("id").cloned().unwrap_or(Value::Null);
        match method {
            "initialize" => json_response(
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "protocolVersion": "2025-06-18",
                        "capabilities": { "tools": {} },
                        "serverInfo": { "name": "mock-http", "version": "1.0.0" },
                    },
                }),
                Vec::new(),
            ),
            "notifications/initialized" => accepted_response(),
            "tools/list" => json_response(
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "tools": [{ "name": "one", "inputSchema": { "type": "object" } }],
                    },
                }),
                Vec::new(),
            ),
            _ => (404, Vec::new(), b"no".to_vec()),
        }
    });
    let server = tokio::spawn(serve_http(listener, handler, requests.clone()));

    let manager = McpManager::without_log(&std::env::current_dir().expect("cwd"));
    let config: McpServerConfig = serde_json::from_value(json!({
        "type": "http",
        "url": format!("http://{addr}/mcp"),
        "timeout": 10,
    }))
    .expect("config");
    manager.registry().register(McpServerEntry {
        name: "httpmock".into(),
        config,
        source: pir::mcp::registry::McpServerSource::Extension("test".into()),
    });
    let tools = manager
        .check_server("httpmock", Duration::from_secs(15))
        .await
        .expect("check_server");
    assert_eq!(tools, vec!["one"]);
    manager.shutdown().await;
    server.abort();
}

// ---------------------------------------------------------------------------
// `auth: { provider }` bearer tokens
// ---------------------------------------------------------------------------

/// An MCP wire-protocol HTTP handler that requires `Bearer <accepted>` and
/// serves one `echo` tool. Other credentials get a 401 with a
/// `WWW-Authenticate` challenge, so rmcp classifies it as an auth failure.
fn auth_wire_http_handler(accepted: Arc<Mutex<String>>) -> HttpHandler {
    Arc::new(move |request: &RecordedRequest| {
        let expected = format!(
            "Bearer {}",
            accepted
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        );
        if request.headers.get("authorization") != Some(&expected) {
            return (
                401,
                vec![("www-authenticate".to_owned(), "Bearer".to_owned())],
                b"unauthorized".to_vec(),
            );
        }
        if request.body.is_null() {
            return (405, Vec::new(), b"method not allowed".to_vec());
        }
        let method = request
            .body
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("");
        let id = request.body.get("id").cloned().unwrap_or(Value::Null);
        let reply = |result: Value| {
            json_response(
                json!({ "jsonrpc": "2.0", "id": id, "result": result }),
                Vec::new(),
            )
        };
        match method {
            "initialize" => json_response(
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "protocolVersion": "2025-06-18",
                        "capabilities": { "tools": {} },
                        "serverInfo": { "name": "auth-mock", "version": "1.0.0" },
                    },
                }),
                vec![("mcp-session-id".to_owned(), "sess-auth".to_owned())],
            ),
            "notifications/initialized" => accepted_response(),
            "tools/list" => reply(json!({
                "tools": [{ "name": "echo", "inputSchema": { "type": "object" } }],
            })),
            "tools/call" => reply(json!({
                "content": [{ "type": "text", "text": "authorized" }],
            })),
            _ => (404, Vec::new(), b"no".to_vec()),
        }
    })
}

fn write_auth_json(dir: &Path, provider: &str, key: &str) {
    std::fs::write(
        dir.join("auth.json"),
        serde_json::to_vec(&json!({
            provider: { "type": "api_key", "key": key },
        }))
        .expect("serialize auth.json"),
    )
    .expect("write auth.json");
}

fn write_oauth_auth_json(dir: &Path, provider: &str, access: &str, refresh: &str, expires: i64) {
    std::fs::write(
        dir.join("auth.json"),
        serde_json::to_vec(&json!({
            provider: { "type": "oauth", "access": access, "refresh": refresh, "expires": expires },
        }))
        .expect("serialize auth.json"),
    )
    .expect("write auth.json");
}

/// A native provider-auth hook that only refreshes (login reports
/// `Unavailable`), counting the refresh calls. Registered under a unique
/// provider id per test because the hook table is process-wide.
struct RefreshOnlyHooks {
    calls: Arc<AtomicUsize>,
    result: Mutex<Result<ProviderAuth, ProviderAuthError>>,
}

impl RefreshOnlyHooks {
    fn new(calls: Arc<AtomicUsize>, result: Result<ProviderAuth, ProviderAuthError>) -> Arc<Self> {
        Arc::new(Self {
            calls,
            result: Mutex::new(result),
        })
    }
}

impl ProviderAuthHooks for RefreshOnlyHooks {
    fn login<'a>(
        &'a self,
        _context: AuthPromptContext<'a>,
    ) -> ServiceFuture<'a, Result<ProviderAuth, ProviderAuthError>> {
        Box::pin(async { Err(ProviderAuthError::Unavailable) })
    }

    fn refresh(
        &self,
        _current: &ProviderAuth,
    ) -> ServiceFuture<'static, Result<ProviderAuth, ProviderAuthError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let result = self
            .result
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        Box::pin(async move { result })
    }
}

/// Installs a test hook and unregisters it on drop (even on a panic).
struct ProviderHookGuard {
    provider: String,
    hooks: Arc<dyn ProviderAuthHooks>,
}

impl ProviderHookGuard {
    fn install(provider: &str, hooks: Arc<dyn ProviderAuthHooks>) -> Self {
        provider_auth_hook_registry().register(provider, Arc::clone(&hooks));
        Self {
            provider: provider.to_owned(),
            hooks,
        }
    }
}

impl Drop for ProviderHookGuard {
    fn drop(&mut self) {
        provider_auth_hook_registry().unregister_if(&self.provider, &self.hooks);
    }
}

#[tokio::test]
async fn auth_provider_attaches_the_provider_bearer_token() {
    let dir = TempDir::new("auth-provider");
    write_auth_json(&dir.0, "fake-provider", "provider-token");
    let accepted = Arc::new(Mutex::new("provider-token".to_owned()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    let requests: Arc<Mutex<Vec<RecordedRequest>>> = Arc::new(Mutex::new(Vec::new()));
    let server = tokio::spawn(serve_http(
        listener,
        auth_wire_http_handler(accepted),
        requests.clone(),
    ));

    let manager = McpManager::new(&dir.0, &std::env::current_dir().expect("cwd"));
    let config: McpServerConfig = serde_json::from_value(json!({
        "type": "http",
        "url": format!("http://{addr}/mcp"),
        "timeout": 10,
        "auth": { "provider": "fake-provider" },
    }))
    .expect("config");
    manager.registry().register(McpServerEntry {
        name: "authed".into(),
        config,
        source: pir::mcp::registry::McpServerSource::Extension("test".into()),
    });
    let tools = manager
        .check_server("authed", Duration::from_secs(15))
        .await
        .expect("check_server");
    assert_eq!(tools, vec!["echo"]);
    {
        let recorded = requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(!recorded.is_empty(), "the server was contacted");
        for request in recorded.iter() {
            assert_eq!(
                request.headers.get("authorization").map(String::as_str),
                Some("Bearer provider-token"),
                "every request carries the provider token: {:?}",
                request.headers
            );
        }
    }
    manager.shutdown().await;
    server.abort();
}

#[tokio::test]
async fn auth_provider_rereads_the_token_after_a_401() {
    let dir = TempDir::new("auth-provider-401");
    write_auth_json(&dir.0, "fake-provider", "token-one");
    let accepted = Arc::new(Mutex::new("token-one".to_owned()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    let requests: Arc<Mutex<Vec<RecordedRequest>>> = Arc::new(Mutex::new(Vec::new()));
    let server = tokio::spawn(serve_http(
        listener,
        auth_wire_http_handler(Arc::clone(&accepted)),
        requests.clone(),
    ));

    let manager = McpManager::new(&dir.0, &std::env::current_dir().expect("cwd"));
    let config: McpServerConfig = serde_json::from_value(json!({
        "type": "http",
        "url": format!("http://{addr}/mcp"),
        "timeout": 10,
        "auth": { "provider": "fake-provider" },
    }))
    .expect("config");
    manager.registry().register(McpServerEntry {
        name: "authed".into(),
        config,
        source: pir::mcp::registry::McpServerSource::Extension("test".into()),
    });
    manager
        .check_server("authed", Duration::from_secs(15))
        .await
        .expect("the first token connects");

    // The provider rotates its token (a `/login` or refresh elsewhere): the
    // server now rejects the connection's copy.
    *accepted
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = "token-two".to_owned();
    write_auth_json(&dir.0, "fake-provider", "token-two");

    let result = manager
        .call_tool("authed", "echo", &json!({}), None)
        .await
        .expect("the call succeeds after re-reading the token");
    assert_eq!(tool_result_text(&result), "authorized");
    assert!(
        manager
            .connection_states()
            .await
            .get("authed")
            .is_some_and(pir::mcp::manager::ConnectionState::is_connected),
        "the retried connection is live"
    );
    let reconnected_with_new_token = requests
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .any(|request| {
            request.body.get("method").and_then(Value::as_str) == Some("initialize")
                && request.headers.get("authorization").map(String::as_str)
                    == Some("Bearer token-two")
        });
    assert!(
        reconnected_with_new_token,
        "the retry opened a new connection with the provider's current token"
    );

    manager.shutdown().await;
    server.abort();
}

#[tokio::test]
async fn auth_provider_401_reports_needs_auth_with_the_provider_login() {
    let dir = TempDir::new("auth-provider-needs-auth");
    write_auth_json(&dir.0, "fake-provider", "rejected-token");
    // The server accepts nothing: every attempt is a 401.
    let accepted = Arc::new(Mutex::new("some-other-token".to_owned()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    let requests: Arc<Mutex<Vec<RecordedRequest>>> = Arc::new(Mutex::new(Vec::new()));
    let server = tokio::spawn(serve_http(
        listener,
        auth_wire_http_handler(accepted),
        requests,
    ));

    let manager = McpManager::new(&dir.0, &std::env::current_dir().expect("cwd"));
    let config: McpServerConfig = serde_json::from_value(json!({
        "type": "http",
        "url": format!("http://{addr}/mcp"),
        "timeout": 10,
        "auth": { "provider": "fake-provider" },
    }))
    .expect("config");
    manager.registry().register(McpServerEntry {
        name: "authed".into(),
        config,
        source: pir::mcp::registry::McpServerSource::Extension("test".into()),
    });
    let error = manager
        .check_server("authed", Duration::from_secs(15))
        .await
        .expect_err("a rejected provider token needs a sign-in");
    assert!(
        matches!(error, McpError::ProviderLoginRequired { .. }),
        "the typed error names the provider login: {error:?}"
    );
    assert_eq!(
        error.to_string(),
        "MCP server \"authed\" requires sign-in. Run /login fake-provider to sign in."
    );
    let state = manager
        .connection_states()
        .await
        .remove("authed")
        .expect("state");
    match state {
        pir::mcp::manager::ConnectionState::NeedsAuth { message } => assert_eq!(
            message,
            "MCP server \"authed\" requires sign-in. Run /login fake-provider to sign in."
        ),
        other => panic!("expected NeedsAuth, got {other:?}"),
    }
    manager.shutdown().await;
    server.abort();
}

#[tokio::test]
async fn auth_provider_401_refreshes_the_stored_oauth_credential() {
    const PROVIDER: &str = "mcp-oauth-refresh-provider";
    let dir = TempDir::new("auth-provider-oauth-refresh");
    let expires = pir::ai::util::now_ms() as i64 + 3_600_000;
    write_oauth_auth_json(&dir.0, PROVIDER, "token-one", "refresh-token", expires);
    let calls = Arc::new(AtomicUsize::new(0));
    let _hook = ProviderHookGuard::install(
        PROVIDER,
        RefreshOnlyHooks::new(
            Arc::clone(&calls),
            Ok(ProviderAuth::oauth("token-two", "refresh-token-2", expires)),
        ),
    );
    let accepted = Arc::new(Mutex::new("token-one".to_owned()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    let requests: Arc<Mutex<Vec<RecordedRequest>>> = Arc::new(Mutex::new(Vec::new()));
    let server = tokio::spawn(serve_http(
        listener,
        auth_wire_http_handler(Arc::clone(&accepted)),
        requests.clone(),
    ));

    let manager = McpManager::new(&dir.0, &std::env::current_dir().expect("cwd"));
    let config: McpServerConfig = serde_json::from_value(json!({
        "type": "http",
        "url": format!("http://{addr}/mcp"),
        "timeout": 10,
        "auth": { "provider": PROVIDER },
    }))
    .expect("config");
    manager.registry().register(McpServerEntry {
        name: "authed".into(),
        config,
        source: pir::mcp::registry::McpServerSource::Extension("test".into()),
    });
    manager
        .check_server("authed", Duration::from_secs(15))
        .await
        .expect("the first access token connects");

    // The provider revokes/rotates the access token: the stored credential is
    // still unexpired, so only the 401 can trigger the refresh.
    *accepted
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = "token-two".to_owned();

    let result = manager
        .call_tool("authed", "echo", &json!({}), None)
        .await
        .expect("the call succeeds after the provider refresh");
    assert_eq!(tool_result_text(&result), "authorized");
    assert_eq!(calls.load(Ordering::SeqCst), 1, "one refresh for the 401");
    // The rotated credential was persisted through the credential-store lock.
    let stored = pir::ai::auth::FileAuthStorage::in_agent_dir(&dir.0)
        .read_stored(PROVIDER)
        .expect("read auth.json")
        .expect("stored credential");
    match stored {
        pir::ai::auth::StoredCredential::OAuth(credential) => {
            assert_eq!(credential.access, "token-two");
            assert_eq!(credential.refresh, "refresh-token-2");
        }
        other => panic!("expected a stored OAuth credential, got {other:?}"),
    }
    let reconnected_with_refreshed_token = requests
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .any(|request| {
            request.body.get("method").and_then(Value::as_str) == Some("initialize")
                && request.headers.get("authorization").map(String::as_str)
                    == Some("Bearer token-two")
        });
    assert!(
        reconnected_with_refreshed_token,
        "the retry opened a new connection with the refreshed token"
    );

    manager.shutdown().await;
    server.abort();
}

#[tokio::test]
async fn auth_provider_401_after_a_failed_refresh_reports_needs_auth() {
    const PROVIDER: &str = "mcp-failing-refresh-provider";
    let dir = TempDir::new("auth-provider-failing-refresh");
    let expires = pir::ai::util::now_ms() as i64 + 3_600_000;
    write_oauth_auth_json(&dir.0, PROVIDER, "token-one", "refresh-token", expires);
    let calls = Arc::new(AtomicUsize::new(0));
    let _hook = ProviderHookGuard::install(
        PROVIDER,
        RefreshOnlyHooks::new(
            Arc::clone(&calls),
            Err(ProviderAuthError::Failed("invalid_grant".to_owned())),
        ),
    );
    let accepted = Arc::new(Mutex::new("token-one".to_owned()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    let requests: Arc<Mutex<Vec<RecordedRequest>>> = Arc::new(Mutex::new(Vec::new()));
    let server = tokio::spawn(serve_http(
        listener,
        auth_wire_http_handler(Arc::clone(&accepted)),
        requests,
    ));

    let manager = McpManager::new(&dir.0, &std::env::current_dir().expect("cwd"));
    let config: McpServerConfig = serde_json::from_value(json!({
        "type": "http",
        "url": format!("http://{addr}/mcp"),
        "timeout": 10,
        "auth": { "provider": PROVIDER },
    }))
    .expect("config");
    manager.registry().register(McpServerEntry {
        name: "authed".into(),
        config,
        source: pir::mcp::registry::McpServerSource::Extension("test".into()),
    });
    manager
        .check_server("authed", Duration::from_secs(15))
        .await
        .expect("the first access token connects");

    *accepted
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = "no-token".to_owned();

    let error = manager
        .call_tool("authed", "echo", &json!({}), None)
        .await
        .expect_err("a refresh that cannot produce a token needs a provider sign-in");
    assert!(
        matches!(error, McpError::ProviderLoginRequired { .. }),
        "the typed error names the provider login: {error:?}"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1, "one refresh attempt");
    let state = manager
        .connection_states()
        .await
        .remove("authed")
        .expect("state");
    match state {
        pir::mcp::manager::ConnectionState::NeedsAuth { message } => assert_eq!(
            message,
            format!("MCP server \"authed\" requires sign-in. Run /login {PROVIDER} to sign in.")
        ),
        other => panic!("expected NeedsAuth, got {other:?}"),
    }

    manager.shutdown().await;
    server.abort();
}

// ---------------------------------------------------------------------------
// HTTP transport: deadline-bounded SSE drain (slow-drip server)
// ---------------------------------------------------------------------------

/// An HTTP server that answers `tools/call` with an SSE body written in slow
/// chunks. When `stall_after_chunks` is set, the body stops mid-way and the
/// connection is held open (a slow-drip server).
async fn serve_slow_drip(
    listener: tokio::net::TcpListener,
    chunk_delay_ms: u64,
    stall_after_chunks: Option<usize>,
) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        tokio::spawn(async move {
            let mut buffer: Vec<u8> = Vec::with_capacity(2048);
            let mut chunk = [0u8; 4096];
            'connection: loop {
                // Read one request (head + content-length body).
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
                            if let Some(position) = find_head_end(&buffer) {
                                break position;
                            }
                        }
                        Err(ref error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            continue
                        }
                        Err(_) => return,
                    }
                };
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
                let body_text = String::from_utf8_lossy(
                    &buffer[body_start..(body_start + content_length).min(buffer.len())],
                )
                .into_owned();
                let request: Value = serde_json::from_str(&body_text).unwrap_or(Value::Null);
                if request.is_null() {
                    // GET stream probe.
                    let response = "HTTP/1.1 405 Method Not Allowed\r\ncontent-length: 0\r\n\r\n";
                    if stream.try_write(response.as_bytes()).is_err() {
                        return;
                    }
                    let _ = tokio::time::timeout(
                        std::time::Duration::from_millis(20),
                        stream.readable(),
                    )
                    .await;
                    continue 'connection;
                }
                let method = request
                    .get("method")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                let id = request.get("id").cloned().unwrap_or(Value::Null);
                buffer.clear();
                match method.as_str() {
                    "initialize" | "tools/list" | "notifications/initialized" => {
                        let result = if method == "initialize" {
                            json!({
                                "protocolVersion": "2025-06-18",
                                "capabilities": { "tools": {} },
                                "serverInfo": { "name": "slow-drip", "version": "1.0.0" },
                            })
                        } else if method == "tools/list" {
                            json!({ "tools": [{ "name": "drip", "inputSchema": {} }] })
                        } else {
                            json!({})
                        };
                        if method == "notifications/initialized" {
                            let response = "HTTP/1.1 202 Accepted\r\ncontent-length: 0\r\n\r\n";
                            if stream.try_write(response.as_bytes()).is_err() {
                                return;
                            }
                        } else {
                            let body = serde_json::to_vec(
                                &json!({ "jsonrpc": "2.0", "id": id, "result": result }),
                            )
                            .expect("serialize");
                            let response = format!(
                                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n",
                                body.len()
                            );
                            let mut out = response.into_bytes();
                            out.extend_from_slice(&body);
                            if stream.try_write(&out).is_err() {
                                return;
                            }
                        }
                        let _ = tokio::time::timeout(
                            std::time::Duration::from_millis(20),
                            stream.readable(),
                        )
                        .await;
                        continue 'connection;
                    }
                    _ => {
                        // tools/call: SSE body, dribbled.
                        let event = json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": { "content": [{ "type": "text", "text": "dripped" }] },
                        });
                        let body = format!("event: message\ndata: {event}\n\n");
                        let bytes = body.as_bytes();
                        let head = format!(
                            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n",
                            bytes.len()
                        );
                        if stream.try_write(head.as_bytes()).is_err() {
                            return;
                        }
                        let mut sent = 0usize;
                        let mut chunks = 0usize;
                        while sent < bytes.len() {
                            if let Some(cap) = stall_after_chunks {
                                if chunks >= cap {
                                    // Slow-drip: hold the connection open
                                    // without ever finishing the body.
                                    loop {
                                        match tokio::time::timeout(
                                            std::time::Duration::from_secs(3600),
                                            stream.readable(),
                                        )
                                        .await
                                        {
                                            Ok(Ok(())) => {}
                                            _ => return,
                                        }
                                        match stream.try_read(&mut chunk) {
                                            Ok(0) => return,
                                            Ok(_) => continue,
                                            Err(ref error)
                                                if error.kind()
                                                    == std::io::ErrorKind::WouldBlock =>
                                            {
                                                continue
                                            }
                                            Err(_) => return,
                                        }
                                    }
                                }
                            }
                            let end = (sent + 6).min(bytes.len());
                            if stream.try_write(&bytes[sent..end]).is_err() {
                                return;
                            }
                            sent = end;
                            chunks += 1;
                            tokio::time::sleep(std::time::Duration::from_millis(chunk_delay_ms))
                                .await;
                        }
                        // Body complete; keep the connection for reuse.
                        let _ = tokio::time::timeout(
                            std::time::Duration::from_millis(20),
                            stream.readable(),
                        )
                        .await;
                        continue 'connection;
                    }
                }
            }
        });
    }
}

#[tokio::test]
async fn http_slow_drip_response_is_bounded_by_the_request_deadline() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    let server = tokio::spawn(serve_slow_drip(listener, 100, Some(3)));

    // A slow-drip SSE body never completes: the request fails at its
    // deadline instead of stalling inside the drain.
    let client = McpClient::connect_http(
        format!("http://{addr}/mcp"),
        Vec::new(),
        Duration::from_secs(1),
    )
    .await
    .expect("handshake");
    let error = tokio::time::timeout(
        Duration::from_secs(10),
        client.call_tool("drip", &json!({})),
    )
    .await
    .expect("call finishes (deadline, not a hang)")
    .expect_err("slow-drip request fails");
    assert!(
        matches!(error, McpError::Timeout { .. }),
        "deadline-bound failure: {error:?}"
    );

    // A drip that finishes inside the deadline still delivers the response
    // (the SSE drain is owned by rmcp).
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    let completing = tokio::spawn(serve_slow_drip(listener, 60, None));
    let client = McpClient::connect_http(
        format!("http://{addr}/mcp"),
        Vec::new(),
        Duration::from_secs(15),
    )
    .await
    .expect("handshake");
    let call = client
        .call_tool("drip", &json!({}))
        .await
        .expect("dripped response arrives");
    let text = call
        .content
        .first()
        .and_then(|block| block.text.clone())
        .expect("text");
    assert_eq!(text, "dripped");
    client.close().await;
    completing.abort();
    server.abort();
}

// ---------------------------------------------------------------------------
// HTTP transport: error causes + retry classification
// ---------------------------------------------------------------------------

#[tokio::test]
async fn http_transport_errors_carry_their_cause() {
    // Unresolvable host: the DNS/connect failure carries its cause and is
    // not classified transient (retrying cannot make it faster).
    let error = McpClient::connect_http(
        "http://definitely-not-a-real-pir-host.invalid/mcp".to_owned(),
        Vec::new(),
        Duration::from_secs(10),
    )
    .await
    .expect_err("DNS failure");
    let message = match &error {
        McpError::Transport(message) => message.clone(),
        other => panic!("expected a Transport error carrying the cause, got {other:?}"),
    };
    assert!(
        message.starts_with("connection failed"),
        "cause is surfaced: {message}"
    );
    assert!(
        !pir::mcp::http::is_transient_transport_message(&message),
        "connect failures are not transient"
    );
}

#[tokio::test]
async fn manager_retries_resets_but_not_connect_failures() {
    // A server that accepts and immediately closes: reset-class failure,
    // retried through the connect backoff (initial + 2 retries).
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let reset_addr = listener.local_addr().expect("addr");
    let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = attempts.clone();
    let reset_server = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            drop(stream);
        }
    });
    let manager = McpManager::without_log(&std::env::current_dir().expect("cwd"));
    let config: McpServerConfig = serde_json::from_value(json!({
        "type": "http",
        "url": format!("http://{reset_addr}/mcp"),
        "timeout": 5,
    }))
    .expect("config");
    manager.registry().register(McpServerEntry {
        name: "reset".into(),
        config,
        source: pir::mcp::registry::McpServerSource::Extension("test".into()),
    });
    let error = manager.get_client("reset").await.expect_err("reset fails");
    assert_eq!(
        attempts.load(std::sync::atomic::Ordering::SeqCst),
        3,
        "reset-class failures are retried"
    );
    assert!(
        matches!(error, McpError::Transport(_)),
        "typed transport error: {error:?}"
    );

    // A closed port: connect failure, surfaced with its cause and not
    // retried (the retry backoff would cost over 1.25s).
    let dead = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let dead_addr = dead.local_addr().expect("addr");
    drop(dead);
    let config: McpServerConfig = serde_json::from_value(json!({
        "type": "http",
        "url": format!("http://{dead_addr}/mcp"),
        "timeout": 5,
    }))
    .expect("config");
    manager.registry().register(McpServerEntry {
        name: "closed".into(),
        config,
        source: pir::mcp::registry::McpServerSource::Extension("test".into()),
    });
    let start = std::time::Instant::now();
    let error = manager
        .get_client("closed")
        .await
        .expect_err("connect fails");
    let elapsed = start.elapsed();
    let message = match &error {
        McpError::Transport(message) => message.clone(),
        other => panic!("expected a Transport error carrying the cause, got {other:?}"),
    };
    assert!(
        message.starts_with("connection failed"),
        "cause is surfaced: {message}"
    );
    // A refused connect is not transient (the classifier above gates the
    // retry loop), so this is a single attempt: no retry-backoff sleeps are
    // added on top of one connect attempt.
    assert!(
        elapsed < Duration::from_secs(4),
        "connect failures are not retried (took {elapsed:?})"
    );
    // The failure is logged once through the manager's state machine.
    manager.shutdown().await;
    reset_server.abort();
}

// ---------------------------------------------------------------------------
// Read-only one-shot transient retry (upstream runtime.ts:274-301)
// ---------------------------------------------------------------------------

/// An MCP wire-protocol HTTP handler whose `fail_method` POST answers
/// `status` for the first `failures` calls (then normally). The returned
/// counter counts every POST of that method, retries included.
fn transient_failure_http_handler(
    fail_method: &'static str,
    status: u16,
    failures: usize,
) -> (HttpHandler, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    let handler: HttpHandler = Arc::new(move |request: &RecordedRequest| {
        if request.body.is_null() {
            return (405, Vec::new(), b"method not allowed".to_vec());
        }
        let method = request
            .body
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("");
        let id = request.body.get("id").cloned().unwrap_or(Value::Null);
        let reply = |result: Value| {
            json_response(
                json!({ "jsonrpc": "2.0", "id": id, "result": result }),
                Vec::new(),
            )
        };
        if method == fail_method {
            let seen = counter.fetch_add(1, Ordering::SeqCst);
            if seen < failures {
                return (status, Vec::new(), b"transient failure".to_vec());
            }
        }
        match method {
            "initialize" => json_response(
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "protocolVersion": "2025-06-18",
                        "capabilities": { "tools": {}, "resources": {} },
                        "serverInfo": { "name": "mock-http", "version": "1.0.0" },
                    },
                }),
                vec![("mcp-session-id".to_owned(), "sess-retry".to_owned())],
            ),
            "notifications/initialized" => accepted_response(),
            "tools/list" => reply(json!({
                "tools": [{ "name": "one", "inputSchema": { "type": "object" } }],
            })),
            "resources/list" => reply(json!({
                "resources": [{ "uri": "mock://res/1", "name": "one" }],
            })),
            "resources/templates/list" => reply(json!({
                "resourceTemplates": [{ "uriTemplate": "mock://res/{id}", "name": "one" }],
            })),
            "resources/read" => reply(json!({
                "contents": [{ "uri": "mock://res/1", "text": "hello" }],
            })),
            "tools/call" => reply(json!({
                "content": [{ "type": "text", "text": "ok" }],
            })),
            _ => (404, Vec::new(), b"not found".to_vec()),
        }
    });
    (handler, calls)
}

/// Connect an HTTP client to a fresh mock whose `fail_method` fails with
/// `status` for the first `failures` calls.
async fn connect_http_with_failures(
    fail_method: &'static str,
    status: u16,
    failures: usize,
) -> (McpClient, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    let (handler, calls) = transient_failure_http_handler(fail_method, status, failures);
    let requests: Arc<Mutex<Vec<RecordedRequest>>> = Arc::new(Mutex::new(Vec::new()));
    let server = tokio::spawn(serve_http(listener, handler, requests));
    let client = McpClient::connect_http(
        format!("http://{addr}/mcp"),
        Vec::new(),
        Duration::from_secs(10),
    )
    .await
    .expect("handshake");
    (client, calls, server)
}

/// Upstream `isTransientError` statuses: 408, 429 and 5xx except 501. Each is
/// retried exactly once after ~250ms, and the retried request succeeds.
#[tokio::test]
async fn read_only_requests_retry_transient_http_statuses_once() {
    for status in [408u16, 429, 500, 502, 503, 504] {
        let (client, calls, server) = connect_http_with_failures("resources/list", status, 1).await;
        let started = std::time::Instant::now();
        let page = client
            .list_resources(None)
            .await
            .unwrap_or_else(|error| panic!("status {status} retried then succeeded: {error:?}"));
        assert_eq!(page.resources.len(), 1, "status {status}");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "status {status} is retried exactly once"
        );
        assert!(
            started.elapsed() >= Duration::from_millis(200),
            "status {status} waits ~250ms before the retry (took {:?})",
            started.elapsed()
        );
        client.close().await;
        server.abort();
    }
}

/// The read-only retry is bounded to one attempt: a server that keeps failing
/// transiently sees exactly two POSTs.
#[tokio::test]
async fn read_only_retry_is_bounded_to_one_attempt() {
    for status in [429u16, 500, 503] {
        let (client, calls, server) =
            connect_http_with_failures("resources/list", status, usize::MAX).await;
        let error = client
            .list_resources(None)
            .await
            .expect_err("the server keeps failing");
        assert!(
            matches!(error, McpError::Http { status: actual, .. } if actual == status),
            "status {status} surfaces its typed HTTP error: {error:?}"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "status {status}: initial attempt + one retry"
        );
        client.close().await;
        server.abort();
    }
}

/// 501 and other 4xx statuses are not transient (upstream `isTransientError`):
/// they fail on the first attempt.
#[tokio::test]
async fn non_transient_http_statuses_are_not_retried() {
    for status in [400u16, 422, 501] {
        let (client, calls, server) =
            connect_http_with_failures("resources/list", status, usize::MAX).await;
        let error = client
            .list_resources(None)
            .await
            .expect_err("a non-transient status fails");
        assert!(
            matches!(error, McpError::Http { status: actual, .. } if actual == status),
            "status {status} surfaces its typed HTTP error: {error:?}"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "status {status} is not retried"
        );
        client.close().await;
        server.abort();
    }
}

/// `tools/call` may have run server-side, so a transient status is never
/// retried for it (upstream `withClient` passes `readOnly` only for reads).
#[tokio::test]
async fn tool_calls_are_never_retried() {
    let (client, calls, server) = connect_http_with_failures("tools/call", 503, 1).await;
    let error = client
        .call_tool("one", &json!({}))
        .await
        .expect_err("the first POST fails");
    assert!(
        matches!(error, McpError::Http { status: 503, .. }),
        "typed HTTP error: {error:?}"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "a write is dispatched at most once"
    );
    client.close().await;
    server.abort();
}

/// Every read-only client operation the builtin resource tools use retries
/// once: `resources/list`, `resources/templates/list`, `resources/read`, and
/// `tools/list` (the manager's discovery path).
#[tokio::test]
async fn every_read_only_operation_retries_once() {
    {
        let (client, calls, server) =
            connect_http_with_failures("resources/templates/list", 503, 1).await;
        client
            .list_resource_templates(None)
            .await
            .expect("templates retried then succeeded");
        assert_eq!(calls.load(Ordering::SeqCst), 2, "resources/templates/list");
        client.close().await;
        server.abort();
    }
    {
        let (client, calls, server) = connect_http_with_failures("resources/read", 503, 1).await;
        client
            .read_resource("mock://res/1")
            .await
            .expect("read retried then succeeded");
        assert_eq!(calls.load(Ordering::SeqCst), 2, "resources/read");
        client.close().await;
        server.abort();
    }
    {
        let (client, calls, server) = connect_http_with_failures("tools/list", 503, 1).await;
        let tools = client
            .list_all_tools()
            .await
            .expect("tools/list retried then succeeded");
        assert_eq!(tools.len(), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 2, "tools/list");
        client.close().await;
        server.abort();
    }
}

// ---------------------------------------------------------------------------
// `mcp_servers_change` dispatch + registerMcpServer / getMcpServers
// ---------------------------------------------------------------------------

#[tokio::test]
async fn js_extension_registers_servers_and_receives_change_events() {
    let _guard = env_guard().await;
    let dir = TempDir::new("js");
    std::env::set_var("PIR_AGENT_DIR", &dir.0);

    let extension = dir.0.join("mcp-ext.ts");
    std::fs::write(
        &extension,
        r#"
        export default function (pi) {
          globalThis.__mcpChanges = [];
          pi.on("mcp_servers_change", async (event) => {
            globalThis.__mcpChanges.push(JSON.stringify(event));
          });
          pi.registerMcpServer("js-server", {
            command: "definitely-not-a-real-command-12345",
          });
          globalThis.__servers = pi.getMcpServers();
        }
        "#,
    )
    .expect("write fixture");

    let config = pir::extensions::ExtensionConfig {
        cwd: dir.0.clone(),
        mode: "headless".to_owned(),
        has_ui: false,
        model: None,
        session_id: Some("mcp-session".to_owned()),
        system_prompt: String::new(),
        is_idle: true,
        limits: Some(Default::default()),
    };
    let js = pir::extensions::ExtensionHandle::start(config).expect("runtime");
    let result = js
        .load(std::slice::from_ref(&extension))
        .await
        .expect("load");
    assert!(result.errors.is_empty(), "load errors: {:?}", result.errors);

    // The extension registered a server; getMcpServers reports it.
    let servers = js
        .eval_json("JSON.stringify(globalThis.__servers)")
        .await
        .expect("probe");
    let servers: Value = match servers {
        Value::String(text) => serde_json::from_str(&text).expect("json"),
        value => value,
    };
    assert_eq!(servers[0]["name"], "js-server");

    let host = PluginHost::build(Some(js.clone()), vec![Arc::new(McpPlugin::new())]);

    // The builtin's registry consumed the JS registration.
    let manager = host.mcp_manager().expect("mcp manager");
    assert!(
        manager.registry().get("js-server").is_some(),
        "registerMcpServer lands in the builtin registry"
    );

    // Any dispatch flushes the queued mcp_servers_change to JS handlers.
    let mut payload = json!({ "type": "session_start" });
    host.emit("session_start", &mut payload)
        .await
        .expect("emit");
    let changes = js
        .eval_json("JSON.stringify(globalThis.__mcpChanges)")
        .await
        .expect("probe");
    let changes: Value = match changes {
        Value::String(text) => serde_json::from_str(&text).expect("json"),
        value => value,
    };
    assert!(
        !changes.as_array().expect("array").is_empty(),
        "mcp_servers_change reached the JS handler: {changes}"
    );
    // The fixture records `JSON.stringify(event)` per handler call. Upstream
    // payload shape (`McpServersChangeEvent`, types.ts:744-748):
    // `{type, servers: RegisteredMcpServer[]}` with each entry
    // `{name, config, extensionPath}` (mcp-servers.ts:307-313).
    let first = match &changes[0] {
        Value::String(text) => serde_json::from_str::<Value>(text).expect("json"),
        value => value.clone(),
    };
    assert_eq!(first["type"], "mcp_servers_change");
    let servers = first["servers"].as_array().expect("servers array");
    assert_eq!(
        servers.len(),
        1,
        "only the JS-registered server: {servers:?}"
    );
    assert_eq!(servers[0]["name"], "js-server");
    assert_eq!(
        servers[0]["config"]["command"],
        "definitely-not-a-real-command-12345"
    );
    assert!(
        servers[0]["extensionPath"]
            .as_str()
            .is_some_and(|path| path.ends_with("mcp-ext.ts")),
        "RegisteredMcpServer carries the registering extension path: {}",
        servers[0]["extensionPath"]
    );
}

// ---------------------------------------------------------------------------
// CLI: `pir mcp check` against the mock stdio server
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cli_check_connects_and_reports() {
    let _guard = env_guard().await;
    let dir = TempDir::new("cli");
    std::env::set_var("PIR_AGENT_DIR", &dir.0);
    std::env::remove_var("PIR_TRUST_PROJECT");
    let (command, args) = mock_server_spawn();
    let mcp_json = json!({
        "mock": {
            "command": command,
            "args": args,
            "env": { "PIR_MOCK_MCP_SERVER": "1" },
            "timeout": 20,
        },
    });
    std::fs::write(
        pir::config::mcp::global_mcp_path(&dir.0),
        serde_json::to_string_pretty(&mcp_json).expect("serialize"),
    )
    .expect("write mcp.json");

    let outcome =
        pir::coding_agent::cli::run_mcp_command(pir::coding_agent::cli::McpCommand::Check {
            server: Some("mock".into()),
            json: true,
        })
        .await;
    assert!(outcome.is_ok(), "check succeeded: {outcome:?}");

    // Unknown servers error.
    let outcome =
        pir::coding_agent::cli::run_mcp_command(pir::coding_agent::cli::McpCommand::Check {
            server: Some("nope".into()),
            json: false,
        })
        .await;
    assert!(outcome.is_err());
}

// ---------------------------------------------------------------------------
// `mcp_servers_change` delivery timing: the flush rides the rebuild
// ---------------------------------------------------------------------------

/// The extension config the flush-timing fixtures load under (headless, no UI,
/// idle: the same shape the JS fixture above uses).
fn flush_timing_extension_config(cwd: &Path, session: &str) -> pir::extensions::ExtensionConfig {
    pir::extensions::ExtensionConfig {
        cwd: cwd.to_path_buf(),
        mode: "headless".to_owned(),
        has_ui: false,
        model: None,
        session_id: Some(session.to_owned()),
        system_prompt: String::new(),
        is_idle: true,
        limits: Some(Default::default()),
    }
}

/// The `mcp_servers_change` payloads a fixture handler recorded, in order (the
/// fixtures push `JSON.stringify(event)` per handler call, so each element is
/// unwrapped once more into the payload object).
async fn recorded_mcp_changes(js: &pir::extensions::ExtensionHandle, global: &str) -> Vec<Value> {
    let probe = js
        .eval_json(&format!("JSON.stringify(globalThis.{global})"))
        .await
        .expect("probe");
    let records = match probe {
        Value::String(text) => serde_json::from_str::<Value>(&text).expect("probe json"),
        value => value,
    };
    records
        .as_array()
        .map(|records| {
            records
                .iter()
                .map(|record| match record {
                    Value::String(text) => serde_json::from_str(text).expect("record json"),
                    value => value.clone(),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Registration-time delivery: a `pi.registerMcpServer` that lands after the
/// first registry build (only the runtime's state knows it) is delivered by the
/// registry rebuild that re-reads those defs — in the SAME host step — and not
/// by the next arbitrary event dispatch.
///
/// The probe event is one no plugin observes ([`PluginHost::has_handlers`] is
/// false for it: the narrowed `mcp` interest does not take it and the runtime
/// registers no handler), so the delivery observed below cannot have come from
/// the event dispatch — it can only come from the rebuild-tail
/// `McpPlugin::flush_server_changes` that the enqueue point (`McpPlugin::register`)
/// now feeds directly. Before the flush was anchored there, the queued entry
/// survived the whole host step and waited for a *second* dispatch.
#[tokio::test(flavor = "current_thread")]
async fn mcp_server_change_is_delivered_by_the_registration_rebuild() {
    let _guard = env_guard().await;
    let dir = TempDir::new("flush-registration");
    std::env::set_var("PIR_AGENT_DIR", &dir.0);

    // The recorder loads first: it only records change payloads.
    let recorder = dir.0.join("mcp-recorder.ts");
    std::fs::write(
        &recorder,
        r#"
        export default function (pi) {
          globalThis.__mcpRegistered = [];
          pi.on("mcp_servers_change", async (event) => {
            globalThis.__mcpRegistered.push(JSON.stringify(event));
          });
        }
        "#,
    )
    .expect("write recorder");
    let js = pir::extensions::ExtensionHandle::start(flush_timing_extension_config(
        &dir.0,
        "flush-registration",
    ))
    .expect("runtime");
    let result = js
        .load(std::slice::from_ref(&recorder))
        .await
        .expect("load");
    assert!(result.errors.is_empty(), "load errors: {:?}", result.errors);

    let plugin = Arc::new(McpPlugin::with_cwd(&dir.0, &dir.0));
    let host = PluginHost::build(
        Some(js.clone()),
        vec![plugin.clone() as Arc<dyn pir::plugins::AgentPlugin>],
    );
    let manager = host.mcp_manager().expect("mcp manager");

    // A post-build registration: the def lives in the runtime until a rebuild.
    // `enabled: false` keeps the timing test offline — nothing ever connects.
    let late = dir.0.join("mcp-late.ts");
    std::fs::write(
        &late,
        r#"
        export default function (pi) {
          pi.registerMcpServer("late-server", {
            command: "definitely-not-a-real-command-67890",
            enabled: false,
          });
        }
        "#,
    )
    .expect("write late fixture");
    let result = js
        .load(std::slice::from_ref(&late))
        .await
        .expect("load late");
    assert!(result.errors.is_empty(), "load errors: {:?}", result.errors);
    assert!(
        manager.registry().get("late-server").is_none(),
        "the manager registry only learns the server at the next rebuild"
    );
    assert!(
        recorded_mcp_changes(&js, "__mcpRegistered")
            .await
            .is_empty(),
        "no delivery before any rebuild"
    );

    // The probe event has no consumer anywhere, so the delivery below is the
    // rebuild's, not the dispatch's.
    assert!(
        host.has_handlers("mcp_servers_change"),
        "the recorder handler is registered"
    );
    assert!(
        !host.has_handlers("agent_settled"),
        "the probe event has no consumer"
    );
    let mut payload = json!({ "type": "agent_settled" });
    let outcome = host
        .emit("agent_settled", &mut payload)
        .await
        .expect("emit");
    assert!(
        outcome.errors.is_empty(),
        "emit errors: {:?}",
        outcome.errors
    );
    assert!(
        manager.registry().get("late-server").is_some(),
        "the rebuild registered the server"
    );

    // Exactly one coalesced delivery, produced by that single host step.
    let changes = recorded_mcp_changes(&js, "__mcpRegistered").await;
    assert_eq!(
        changes.len(),
        1,
        "one delivery at the registration rebuild, not one per next event: {changes:?}"
    );
    assert_eq!(changes[0]["type"], "mcp_servers_change");
    let servers = changes[0]["servers"].as_array().expect("servers array");
    assert_eq!(
        servers.len(),
        1,
        "the extension-registered server only: {servers:?}"
    );
    assert_eq!(servers[0]["name"], "late-server");
    assert_eq!(
        servers[0]["config"]["command"],
        "definitely-not-a-real-command-67890"
    );
    assert!(
        servers[0]["extensionPath"]
            .as_str()
            .is_some_and(|path| path.ends_with("mcp-late.ts")),
        "the payload carries the registering extension path: {}",
        servers[0]["extensionPath"]
    );

    // A later event that queues nothing must not re-deliver the change.
    let mut payload = json!({ "type": "agent_settled" });
    host.emit("agent_settled", &mut payload)
        .await
        .expect("emit");
    assert_eq!(
        recorded_mcp_changes(&js, "__mcpRegistered").await.len(),
        1,
        "a rebuild that queues nothing delivers nothing"
    );
    host.shutdown().await;
}

/// The `reload()` rebuild point flushes too: a registration the runtime picked
/// up after the first registry build is delivered immediately after
/// [`PluginHost::reload`], with NO event dispatch in between (the old delivery
/// rode the next dispatch, which the narrowed `mcp` interest no longer takes;
/// this test dispatches no event at all).
#[tokio::test(flavor = "current_thread")]
async fn reload_publishes_queued_mcp_server_changes_without_an_event() {
    let _guard = env_guard().await;
    let dir = TempDir::new("flush-reload");
    std::env::set_var("PIR_AGENT_DIR", &dir.0);

    let extension = dir.0.join("mcp-reload.ts");
    std::fs::write(
        &extension,
        r#"
        export default function (pi) {
          globalThis.__mcpReloaded = [];
          pi.on("mcp_servers_change", async (event) => {
            globalThis.__mcpReloaded.push(JSON.stringify(event));
          });
          pi.registerMcpServer("reloaded-server", {
            command: "definitely-not-a-real-command-24680",
            enabled: false,
          });
        }
        "#,
    )
    .expect("write fixture");

    // Built BEFORE the extension loads, so the registration only ever reaches
    // the manager registry through a rebuild.
    let js = pir::extensions::ExtensionHandle::start(flush_timing_extension_config(
        &dir.0,
        "flush-reload",
    ))
    .expect("runtime");
    let plugin = Arc::new(McpPlugin::with_cwd(&dir.0, &dir.0));
    let host = PluginHost::build(
        Some(js.clone()),
        vec![plugin.clone() as Arc<dyn pir::plugins::AgentPlugin>],
    );
    let manager = host.mcp_manager().expect("mcp manager");
    let result = js
        .load(std::slice::from_ref(&extension))
        .await
        .expect("load");
    assert!(result.errors.is_empty(), "load errors: {:?}", result.errors);
    assert!(
        manager.registry().list().is_empty(),
        "no server before a rebuild"
    );
    assert!(
        recorded_mcp_changes(&js, "__mcpReloaded").await.is_empty(),
        "no delivery before any rebuild"
    );

    // Reload is a rebuild point: no event is dispatched anywhere here (the
    // fixture re-registers its handler and server on the fresh runtime).
    host.reload().await.expect("reload");

    let changes = recorded_mcp_changes(&js, "__mcpReloaded").await;
    assert_eq!(
        changes.len(),
        1,
        "the reload rebuild flushed the queued registration: {changes:?}"
    );
    assert_eq!(changes[0]["type"], "mcp_servers_change");
    let servers = changes[0]["servers"].as_array().expect("servers array");
    assert_eq!(servers.len(), 1, "servers: {servers:?}");
    assert_eq!(servers[0]["name"], "reloaded-server");
    assert_eq!(
        servers[0]["config"]["command"],
        "definitely-not-a-real-command-24680"
    );
    assert!(
        servers[0]["extensionPath"]
            .as_str()
            .is_some_and(|path| path.ends_with("mcp-reload.ts")),
        "the payload carries the registering extension path: {}",
        servers[0]["extensionPath"]
    );
    host.shutdown().await;
}

// The session-level flush point needs the session's own wait hook (the host it
// refreshes is private), plus the types its construction takes.
use pir::agent::tool_execution::ToolRegistry;
use pir::agent_session::{AgentSession, AgentSessionConfig};
use pir::ai::provider::ProviderRegistry;
use pir::ai::types::{Model, ModelCost, ModelCostRates, ThinkingLevel};
use pir::extensions::ExtensionHandle;

/// A model for the wait-hook session. No provider is consulted: the test never
/// runs a prompt turn, it only calls the session's wait hook.
fn flush_timing_model() -> Model {
    Model {
        id: "flush-timing-model".into(),
        name: "Flush timing model".into(),
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

/// Session config for the wait-hook test: an (empty) provider registry, no
/// stored session, no compaction, the given extension runtime and inline
/// plugins (the MCP builtin).
fn flush_timing_session_config(
    cwd: &Path,
    extensions: Option<ExtensionHandle>,
) -> AgentSessionConfig {
    AgentSessionConfig {
        provider_registry: Arc::new(ProviderRegistry::default()),
        model: flush_timing_model(),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools: ToolRegistry::new(),
        session: None,
        compaction: None,
        max_turns: 1,
        system_prompt: Some("flush timing".to_owned()),
        cwd: Some(cwd.to_path_buf()),
        event_stream: None,
        extensions,
        tool_selection: None,
    }
}

/// The wait hook is a rebuild point too: its tail runs
/// [`PluginHost::refresh_registry_if_changed`]
/// (`AgentSession::tool_call_wait_hook`, `agent_session/mcp_reach.rs`), so the
/// `mcp_servers_change` entries a rebuild queues are delivered AT the hook —
/// with no event dispatch anywhere in this test.
///
/// The batch is empty on purpose: the hook's per-call reach waits are covered
/// by the reach tests, while the registry refresh it anchors is the seam the
/// flush was aligned to. The stale-server-name trigger is a real runtime
/// registration landing after the session's registry build (a second extension
/// file), i.e. the same "only a rebuild picks it up" state the emit/reload
/// cases use.
#[tokio::test(flavor = "current_thread")]
async fn mcp_wait_hook_is_a_rebuild_point_that_flushes_queued_changes() {
    let _guard = env_guard().await;
    let dir = TempDir::new("flush-wait-hook");
    std::env::set_var("PIR_AGENT_DIR", &dir.0);

    let recorder = dir.0.join("mcp-hook-recorder.ts");
    std::fs::write(
        &recorder,
        r#"
        export default function (pi) {
          globalThis.__mcpHookChanges = [];
          pi.on("mcp_servers_change", async (event) => {
            globalThis.__mcpHookChanges.push(JSON.stringify(event));
          });
        }
        "#,
    )
    .expect("write recorder");
    let js = pir::extensions::ExtensionHandle::start(flush_timing_extension_config(
        &dir.0,
        "flush-wait-hook",
    ))
    .expect("runtime");
    let result = js
        .load(std::slice::from_ref(&recorder))
        .await
        .expect("load");
    assert!(result.errors.is_empty(), "load errors: {:?}", result.errors);

    let plugin = Arc::new(McpPlugin::with_cwd(&dir.0, &dir.0));
    let session = AgentSession::new_with_plugins(
        flush_timing_session_config(&dir.0, Some(js.clone())),
        vec![plugin.clone() as Arc<dyn pir::plugins::AgentPlugin>],
    );
    let manager = plugin.manager();

    // A runtime registration after the session's registry build: the host's
    // server-name snapshot is stale, so only a rebuild applies it.
    let late = dir.0.join("mcp-hook-late.ts");
    std::fs::write(
        &late,
        r#"
        export default function (pi) {
          pi.registerMcpServer("hook-server", {
            command: "definitely-not-a-real-command-13579",
            enabled: false,
          });
        }
        "#,
    )
    .expect("write late fixture");
    let result = js
        .load(std::slice::from_ref(&late))
        .await
        .expect("load late");
    assert!(result.errors.is_empty(), "load errors: {:?}", result.errors);
    assert!(
        manager.registry().get("hook-server").is_none(),
        "the manager registry only learns the server at the next rebuild"
    );
    assert!(
        recorded_mcp_changes(&js, "__mcpHookChanges")
            .await
            .is_empty(),
        "no delivery before any rebuild"
    );

    let hook = session
        .tool_call_wait_hook()
        .expect("a session with the mcp builtin has a wait hook");
    hook(Arc::new(ToolRegistry::new()), Vec::new()).await;

    assert!(
        manager.registry().get("hook-server").is_some(),
        "the hook's rebuild registered the server"
    );
    let changes = recorded_mcp_changes(&js, "__mcpHookChanges").await;
    assert_eq!(
        changes.len(),
        1,
        "the hook's rebuild flushed the queued change: {changes:?}"
    );
    assert_eq!(changes[0]["type"], "mcp_servers_change");
    let servers = changes[0]["servers"].as_array().expect("servers array");
    assert_eq!(servers.len(), 1, "servers: {servers:?}");
    assert_eq!(servers[0]["name"], "hook-server");
    assert_eq!(
        servers[0]["config"]["command"],
        "definitely-not-a-real-command-13579"
    );
    session.shutdown().await;
}

// ---------------------------------------------------------------------------
// Lazy-connect discovery publication:
// `McpPlugin::wait_for_tool_call` -> `McpManager::discover_server_and_publish`
// ---------------------------------------------------------------------------

use pir::agent::tool_batch::AgentToolCall;

/// The mock stdio server's `mcp.json` entry (what the tests below write to the
/// global config file before the plugin registers it).
fn mock_mcp_json_entry() -> Value {
    let (command, args) = mock_server_spawn();
    json!({
        "command": command,
        "args": args,
        "env": { "PIR_MOCK_MCP_SERVER": "1" },
        "timeout": 20,
    })
}

/// Write `dir/mcp.json` with the mock server, then build the `mcp` builtin
/// rooted at `dir` plus a plugin host around it. The entry is owned by the
/// FILE (every rebuild re-runs `sync_config_files`, which drops config-sourced
/// entries absent from their file), so the file must actually contain it.
fn lazy_mock_host(dir: &Path) -> (Arc<McpPlugin>, PluginHost) {
    std::fs::write(
        dir.join("mcp.json"),
        json!({ "mock": mock_mcp_json_entry() }).to_string(),
    )
    .expect("write mcp.json");
    let plugin = Arc::new(McpPlugin::with_cwd(dir, dir));
    let host = PluginHost::build(None, vec![plugin.clone()]);
    (plugin, host)
}

/// A tool registry holding the aggregate `list_mcp_resources` tool: a call to
/// it waits for every ENABLED server (upstream `RESOURCE_TOOL_NAMES`), which is
/// how a server that no `session_start` ever discovered gets first reached by a
/// tool call. The tool's `reaches_tools` is the default `None`, so the
/// resource-tool rule decides.
fn resource_tool_registry(manager: &Arc<McpManager>) -> Arc<ToolRegistry> {
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(McpResourceTool::new(
        Arc::clone(manager),
        McpResourceToolKind::ListResources,
        ToolExposure::Direct,
    )));
    Arc::new(registry)
}

/// [`resource_tool_registry`] as the single call the wait hooks receive.
fn resource_tool_call() -> Vec<AgentToolCall> {
    vec![AgentToolCall::new(
        0,
        "call-1",
        pir::mcp::registry::LIST_MCP_RESOURCES_TOOL,
        json!({}),
    )]
}

/// The lazy wait PUBLISHES what it listed: a server first reached by a tool
/// call (no `session_start` ran, so `discover_all` never saw it) lands in the
/// discovered snapshot and marks the registry dirty — the flag the host's
/// rebuild points consume.
///
/// Regression lock: the wait used to call the pure probe
/// (`McpManager::discover_server`), which listed the tools into a discarded
/// value — the snapshot stayed empty, nothing was marked dirty, and the
/// rebuild the wait hooks anchor right after the loop had no `mcp__*` bridges
/// to register, so such a server stayed unreachable for the whole session.
#[tokio::test(flavor = "current_thread")]
async fn lazy_wait_publishes_the_discovered_snapshot_and_marks_it_dirty() {
    let _guard = env_guard().await;
    let dir = TempDir::new("lazy-publish");
    std::env::set_var("PIR_AGENT_DIR", &dir.0);
    let (plugin, host) = lazy_mock_host(&dir.0);
    let manager = host.mcp_manager().expect("mcp manager");
    assert!(
        manager.registry().get("mock").is_some(),
        "the config pass registered the mock server"
    );
    // Precondition: nothing has been discovered (the pure-probe path would
    // leave this state behind too), and no bridge exists yet.
    assert!(
        manager.discovered_tools().is_empty(),
        "no discovery before any wait"
    );
    assert!(
        !manager.take_tools_dirty(),
        "no discovery flag before any wait"
    );
    assert!(
        host.tool_defs()
            .iter()
            .all(|def| !def.name.starts_with("mcp__")),
        "no mcp__ bridges before any discovery"
    );

    let registry = resource_tool_registry(manager);
    let waited = plugin
        .wait_for_tool_call(
            &registry,
            pir::mcp::registry::LIST_MCP_RESOURCES_TOOL,
            &json!({}),
        )
        .await;
    assert_eq!(
        waited,
        vec!["mock".to_owned()],
        "the wait targeted the enabled server"
    );

    let discovered = manager.discovered_tools();
    assert_eq!(discovered.len(), 1, "one server published: {discovered:?}");
    let names: Vec<&str> = discovered
        .get("mock")
        .expect("the wait published the discovered server")
        .iter()
        .map(|tool| tool.tool.name.as_str())
        .collect();
    assert_eq!(
        names,
        vec!["echo", "fail", "slow"],
        "the wait published the tools it listed"
    );
    assert!(
        manager.take_tools_dirty(),
        "publishing the discovery marked the registry dirty"
    );
    // Publishing is the wait's cue; rebuilding is the host's move (it only
    // happens at a rebuild point: `emit`'s tail, `reload`, the wait hook).
    assert!(
        host.tool_defs()
            .iter()
            .all(|def| !def.name.starts_with("mcp__")),
        "the publication alone registers no bridges"
    );
    manager.shutdown().await;
}

/// The publication becomes bridges at the next rebuild point: `emit`'s tail
/// runs `PluginHost::refresh_registry_if_changed` (the same refresh the
/// session's `tool_call_wait_hook` calls), which consumes the dirty flag and
/// re-registers — the freshly published `mcp__mock__*` bridges are namespaced,
/// carry the default `codemode` -> deferred exposure, and dispatch to the real
/// server.
#[tokio::test(flavor = "current_thread")]
async fn lazy_wait_publication_registers_callable_bridges_at_the_next_rebuild() {
    let _guard = env_guard().await;
    let dir = TempDir::new("lazy-bridges");
    std::env::set_var("PIR_AGENT_DIR", &dir.0);
    let (plugin, host) = lazy_mock_host(&dir.0);
    let manager = host.mcp_manager().expect("mcp manager");

    let registry = resource_tool_registry(manager);
    let waited = plugin
        .wait_for_tool_call(
            &registry,
            pir::mcp::registry::LIST_MCP_RESOURCES_TOOL,
            &json!({}),
        )
        .await;
    assert_eq!(waited, vec!["mock".to_owned()]);
    assert_eq!(
        manager.discovered_tools().len(),
        1,
        "the wait published before the rebuild"
    );

    // The rebuild point; the probe event has no consumer anywhere, so nothing
    // here dispatches an MCP handler — this is the registry refresh alone.
    let mut payload = json!({ "type": "agent_settled" });
    host.emit("agent_settled", &mut payload)
        .await
        .expect("emit");
    assert!(
        !manager.take_tools_dirty(),
        "the rebuild consumed the publication's dirty flag"
    );

    let defs = host.tool_defs();
    let mut bridges: Vec<String> = defs
        .iter()
        .map(|def| def.name.clone())
        .filter(|name| name.starts_with("mcp__"))
        .collect();
    bridges.sort();
    assert_eq!(
        bridges,
        vec!["mcp__mock__echo", "mcp__mock__fail", "mcp__mock__slow"],
        "the published discovery registered the mcp__* bridges"
    );
    let echo = defs
        .iter()
        .find(|def| def.name == "mcp__mock__echo")
        .expect("echo bridge def");
    assert_eq!(
        echo.exposure, "deferred",
        "MCP tools are not declared to the model by default"
    );
    assert_eq!(
        echo.namespace
            .as_ref()
            .map(|namespace| namespace.name.as_str()),
        Some("mcp__mock"),
        "the bridge carries its server's namespace"
    );

    // The bridge is real: it dispatches through the manager to the mock server.
    let result = host
        .call_tool("mcp__mock__echo", "lazy-bridge-1", &json!({ "text": "hi" }))
        .await
        .expect("call_tool through the lazily published bridge");
    assert_eq!(
        result
            .content
            .first()
            .map(|item| item.text.clone())
            .unwrap_or_default(),
        "echo:hi"
    );
    manager.shutdown().await;
}

/// The production seam end to end: the session's `tool_call_wait_hook` waits
/// (`McpPlugin::wait_for_tool_call`), publishes the discovery, and consumes the
/// flag itself in the same call (`PluginHost::refresh_registry_if_changed`) —
/// no event dispatch anywhere in this test.
///
/// Without the publication the hook's refresh would have nothing to consume:
/// the snapshot write is exactly what turns the wait into a rebuild.
#[tokio::test(flavor = "current_thread")]
async fn session_wait_hook_publishes_the_lazy_discovery_and_consumes_its_flag() {
    let _guard = env_guard().await;
    let dir = TempDir::new("lazy-hook");
    std::env::set_var("PIR_AGENT_DIR", &dir.0);
    std::fs::write(
        dir.0.join("mcp.json"),
        json!({ "mock": mock_mcp_json_entry() }).to_string(),
    )
    .expect("write mcp.json");

    let plugin = Arc::new(McpPlugin::with_cwd(&dir.0, &dir.0));
    let session = AgentSession::new_with_plugins(
        flush_timing_session_config(&dir.0, None),
        vec![plugin.clone() as Arc<dyn pir::plugins::AgentPlugin>],
    );
    let manager = plugin.manager();
    assert!(
        manager.registry().get("mock").is_some(),
        "the session's host registered the server from mcp.json"
    );
    assert!(
        manager.discovered_tools().is_empty(),
        "no discovery before any wait (no session_start ran)"
    );
    assert!(
        !manager.take_tools_dirty(),
        "no discovery flag before any wait"
    );

    let hook = session
        .tool_call_wait_hook()
        .expect("a session with the mcp builtin has a wait hook");
    hook(resource_tool_registry(manager), resource_tool_call()).await;

    let discovered = manager.discovered_tools();
    let names: Vec<&str> = discovered
        .get("mock")
        .expect("the hook's wait published the discovered server")
        .iter()
        .map(|tool| tool.tool.name.as_str())
        .collect();
    assert_eq!(names, vec!["echo", "fail", "slow"]);
    assert!(
        !manager.take_tools_dirty(),
        "the hook's own refresh consumed the publication's dirty flag"
    );
    assert_eq!(
        manager.connection_states().await.get("mock"),
        Some(&pir::mcp::manager::ConnectionState::Connected),
        "the lazy wait connected the real mock server"
    );
    session.shutdown().await;
}

/// The contrast the fix preserved: the pure probe publishes NOTHING. Both
/// `pir mcp check`'s `check_server` and `discover_server` (the probe
/// `discover_all` batches) connect the real mock server and list its tools
/// without touching the discovered snapshot, the dirty flag, or the registry —
/// so a check never registers `mcp__*` bridges behind the session's back.
#[tokio::test(flavor = "current_thread")]
async fn discover_server_probe_publishes_nothing_for_the_check_command() {
    let _guard = env_guard().await;
    let dir = TempDir::new("probe");
    std::env::set_var("PIR_AGENT_DIR", &dir.0);
    let (_plugin, host) = lazy_mock_host(&dir.0);
    let manager = host.mcp_manager().expect("mcp manager");

    // `pir mcp check`: connect the real mock server, report its tools.
    let names = manager
        .check_server("mock", Duration::from_secs(20))
        .await
        .expect("check_server connects the real mock server");
    assert_eq!(names, vec!["echo", "fail", "slow"]);
    // The probe variant lists too...
    let probed = manager
        .discover_server("mock")
        .await
        .expect("discover_server probes the real mock server");
    assert_eq!(probed.len(), 3, "the probe listed the server's tools");
    assert!(
        probed.iter().all(|tool| tool.server == "mock"),
        "probed tools carry their server"
    );

    // ...and publishes nothing.
    assert!(
        manager.discovered_tools().is_empty(),
        "the probe published no snapshot entry"
    );
    assert!(
        !manager.take_tools_dirty(),
        "the probe marked the registry clean"
    );
    // No bridge leaks at the next rebuild point either.
    let mut payload = json!({ "type": "agent_settled" });
    host.emit("agent_settled", &mut payload)
        .await
        .expect("emit");
    assert!(
        host.tool_defs()
            .iter()
            .all(|def| !def.name.starts_with("mcp__")),
        "a pure probe never registers mcp__ bridges"
    );
    manager.shutdown().await;
}

// ---------------------------------------------------------------------------
// Readiness cache: one `tools/list` per invalidation, not per tool call
// ---------------------------------------------------------------------------

/// The wait rule exercised below: the aggregate resource tool reaches every
/// enabled server, so one `wait_for_tool_call` covers the whole registry.
const RESOURCE_WAIT_TOOL: &str = pir::mcp::registry::LIST_MCP_RESOURCES_TOOL;

/// The number of `tools/list` requests the mock child has served so far (the
/// child appends one line per request to `PIR_MOCK_LIST_COUNTER`).
fn mock_list_count(path: &Path) -> usize {
    std::fs::read_to_string(path)
        .map(|text| text.lines().filter(|line| *line == "tools/list").count())
        .unwrap_or(0)
}

/// Register one stdio server config on the plugin's manager (an
/// extension-sourced entry: no config-file pass owns it).
fn register_stdio_server(
    plugin: &McpPlugin,
    name: &str,
    spawn: (String, Vec<String>),
    env: BTreeMap<String, String>,
    timeout_secs: u64,
) {
    let (command, args) = spawn;
    plugin.manager().registry().register(McpServerEntry {
        name: name.to_owned(),
        config: serde_json::from_value(json!({
            "command": command,
            "args": args,
            "env": env,
            "timeout": timeout_secs,
        }))
        .expect("stdio server config"),
        source: McpServerSource::Extension("mcp-readiness-test".to_owned()),
    });
}

/// [`register_stdio_server`] for the standard mock server.
fn register_mock_server(
    plugin: &McpPlugin,
    name: &str,
    extra_env: &[(&str, &str)],
    timeout_secs: u64,
) {
    let mut env = BTreeMap::from([("PIR_MOCK_MCP_SERVER".to_owned(), "1".to_owned())]);
    for (key, value) in extra_env {
        env.insert((*key).to_owned(), (*value).to_owned());
    }
    register_stdio_server(plugin, name, mock_server_spawn(), env, timeout_secs);
}

/// A child that never answers the handshake and holds stdin open: a connect
/// attempt pays the configured timeout.
fn silent_server_spawn() -> (String, Vec<String>) {
    mock_server_spawn()
}

/// (a) Two consecutive tool calls over an unchanged server perform ONE
/// `tools/list` (the first call), not one per call.
///
/// Revert caught: the wait going back to
/// `McpManager::discover_server_and_publish` per reached server (the audit's
/// P1-3): the mock's request counter moves to 2 after the second call.
#[tokio::test(flavor = "current_thread")]
async fn readiness_cache_lists_once_for_consecutive_calls() {
    let _guard = env_guard().await;
    let dir = TempDir::new("readiness-once");
    let counter = dir.0.join("tools-list-count");
    let counter_text = counter.to_string_lossy().into_owned();
    let plugin = McpPlugin::with_cwd(&dir.0, &dir.0);
    register_mock_server(
        &plugin,
        "mock",
        &[("PIR_MOCK_LIST_COUNTER", counter_text.as_str())],
        20,
    );
    let manager = plugin.manager().clone();
    let registry = resource_tool_registry(&manager);

    let first = plugin
        .wait_for_tool_call(&registry, RESOURCE_WAIT_TOOL, &json!({}))
        .await;
    assert_eq!(first, vec!["mock".to_owned()]);
    assert_eq!(
        mock_list_count(&counter),
        1,
        "the first call discovers the server once"
    );
    assert_eq!(
        manager.server_readiness("mock").await,
        Some(ServerReadiness::Ready),
        "the successful discovery records readiness"
    );

    let second = plugin
        .wait_for_tool_call(&registry, RESOURCE_WAIT_TOOL, &json!({}))
        .await;
    assert_eq!(second, first);
    assert_eq!(
        mock_list_count(&counter),
        1,
        "the second call must reuse the readiness cache (no second tools/list)"
    );
    assert_eq!(
        manager.server_readiness("mock").await,
        Some(ServerReadiness::Ready)
    );
    manager.shutdown().await;
}

/// (b) A `notifications/tools/list_changed` invalidates the cached readiness:
/// the next call re-lists exactly once, then the cache is warm again.
///
/// The mock emits the notification only when the test creates the trigger
/// file, so the event lands strictly between two calls.
#[tokio::test(flavor = "current_thread")]
async fn readiness_cache_relists_after_tools_list_changed() {
    let _guard = env_guard().await;
    let dir = TempDir::new("readiness-changed");
    let counter = dir.0.join("tools-list-count");
    let trigger = dir.0.join("notify-trigger");
    let sent = dir.0.join("notify-sent");
    let counter_text = counter.to_string_lossy().into_owned();
    let trigger_text = trigger.to_string_lossy().into_owned();
    let sent_text = sent.to_string_lossy().into_owned();
    let plugin = McpPlugin::with_cwd(&dir.0, &dir.0);
    register_mock_server(
        &plugin,
        "mock",
        &[
            ("PIR_MOCK_LIST_COUNTER", counter_text.as_str()),
            ("PIR_MOCK_NOTIFY_FILE", trigger_text.as_str()),
            ("PIR_MOCK_NOTIFY_SENT", sent_text.as_str()),
        ],
        20,
    );
    let manager = plugin.manager().clone();
    let registry = resource_tool_registry(&manager);

    plugin
        .wait_for_tool_call(&registry, RESOURCE_WAIT_TOOL, &json!({}))
        .await;
    assert_eq!(mock_list_count(&counter), 1);
    assert_eq!(
        manager.server_readiness("mock").await,
        Some(ServerReadiness::Ready)
    );

    // Ask the child for one `tools/list_changed` and wait for its write ack.
    std::fs::write(&trigger, b"1").expect("write notification trigger");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !sent.exists() && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(
        sent.exists(),
        "the mock acknowledged the tools/list_changed write"
    );

    // The client task records the notification asynchronously. The readiness
    // probe observes the generation bump (Ready -> Stale) once it lands, so
    // waiting for that state proves the event was recorded BEFORE the re-list
    // starts (no event can arrive mid-list and trigger a second re-list).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while manager.server_readiness("mock").await != Some(ServerReadiness::Stale)
        && tokio::time::Instant::now() < deadline
    {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        manager.server_readiness("mock").await,
        Some(ServerReadiness::Stale),
        "the list_changed notification invalidated the cached readiness"
    );

    plugin
        .wait_for_tool_call(&registry, RESOURCE_WAIT_TOOL, &json!({}))
        .await;
    assert_eq!(
        mock_list_count(&counter),
        2,
        "the list_changed notification caused exactly one re-list"
    );
    assert_eq!(
        manager.server_readiness("mock").await,
        Some(ServerReadiness::Ready)
    );

    plugin
        .wait_for_tool_call(&registry, RESOURCE_WAIT_TOOL, &json!({}))
        .await;
    assert_eq!(
        mock_list_count(&counter),
        2,
        "the refreshed discovery is cached again"
    );
    manager.shutdown().await;
}

/// (c) A second healthy server keeps working while the first is unreachable,
/// and the unreachable server's connect timeout is paid once, not per call:
/// the failed discovery enters its bounded backoff, so the next call returns
/// from the cached error immediately.
#[tokio::test(flavor = "current_thread")]
async fn readiness_cache_backs_off_an_unreachable_server() {
    let _guard = env_guard().await;
    let dir = TempDir::new("readiness-backoff");
    let counter = dir.0.join("tools-list-count");
    let counter_text = counter.to_string_lossy().into_owned();
    let plugin = McpPlugin::with_cwd(&dir.0, &dir.0);
    // 1 s connect timeout on a child that never answers the handshake.
    register_stdio_server(
        &plugin,
        "dead",
        silent_server_spawn(),
        BTreeMap::from([("PIR_MOCK_MCP_SILENT".to_owned(), "1".to_owned())]),
        1,
    );
    register_mock_server(
        &plugin,
        "mock",
        &[("PIR_MOCK_LIST_COUNTER", counter_text.as_str())],
        20,
    );
    let manager = plugin.manager().clone();
    let registry = resource_tool_registry(&manager);

    let first_started = std::time::Instant::now();
    let first = plugin
        .wait_for_tool_call(&registry, RESOURCE_WAIT_TOOL, &json!({}))
        .await;
    let first_elapsed = first_started.elapsed();
    assert_eq!(first, vec!["dead".to_owned(), "mock".to_owned()]);
    assert!(
        first_elapsed >= Duration::from_millis(800),
        "the first call paid the unreachable server's connect timeout: {first_elapsed:?}"
    );
    assert_eq!(
        mock_list_count(&counter),
        1,
        "the healthy server still discovered and published its tools"
    );
    assert!(
        manager.discovered_tools().contains_key("mock"),
        "the healthy server's tools were published despite the dead one"
    );
    assert_eq!(
        manager.server_readiness("dead").await,
        Some(ServerReadiness::Failed),
        "the failed discovery is cached with its backoff"
    );
    assert_eq!(
        manager.server_readiness("mock").await,
        Some(ServerReadiness::Ready)
    );

    let second_started = std::time::Instant::now();
    let second = plugin
        .wait_for_tool_call(&registry, RESOURCE_WAIT_TOOL, &json!({}))
        .await;
    let second_elapsed = second_started.elapsed();
    assert_eq!(second, first);
    assert!(
        second_elapsed < Duration::from_millis(500),
        "the failure backoff must skip the unreachable server's timeout: {second_elapsed:?}"
    );
    assert!(
        second_elapsed * 2 < first_elapsed,
        "the second call must not re-pay the connect timeout: first {first_elapsed:?}, second {second_elapsed:?}"
    );
    assert_eq!(
        mock_list_count(&counter),
        1,
        "the healthy server was not re-listed on the cached call"
    );
    manager.shutdown().await;
}

/// (d) A reconnect invalidates readiness: releasing the live connection (the
/// config-sync/`disconnect` path) makes the next call connect and list again.
///
/// Revert caught: a readiness record surviving the client replacement — the
/// next call would return without listing the NEW child's tools.
#[tokio::test(flavor = "current_thread")]
async fn readiness_cache_invalidates_on_reconnect() {
    let _guard = env_guard().await;
    let dir = TempDir::new("readiness-reconnect");
    let counter = dir.0.join("tools-list-count");
    let counter_text = counter.to_string_lossy().into_owned();
    let plugin = McpPlugin::with_cwd(&dir.0, &dir.0);
    register_mock_server(
        &plugin,
        "mock",
        &[("PIR_MOCK_LIST_COUNTER", counter_text.as_str())],
        20,
    );
    let manager = plugin.manager().clone();
    let registry = resource_tool_registry(&manager);

    plugin
        .wait_for_tool_call(&registry, RESOURCE_WAIT_TOOL, &json!({}))
        .await;
    assert_eq!(mock_list_count(&counter), 1);
    assert_eq!(
        manager.server_readiness("mock").await,
        Some(ServerReadiness::Ready)
    );

    manager.disconnect("mock").await;
    assert_eq!(
        manager.server_readiness("mock").await,
        None,
        "the reconnect path drops the connection (and its readiness record)"
    );

    plugin
        .wait_for_tool_call(&registry, RESOURCE_WAIT_TOOL, &json!({}))
        .await;
    assert_eq!(
        mock_list_count(&counter),
        2,
        "the reconnected server is listed again (reconnect invalidates readiness)"
    );
    assert_eq!(
        manager.server_readiness("mock").await,
        Some(ServerReadiness::Ready)
    );

    plugin
        .wait_for_tool_call(&registry, RESOURCE_WAIT_TOOL, &json!({}))
        .await;
    assert_eq!(
        mock_list_count(&counter),
        2,
        "the post-reconnect discovery is cached"
    );
    manager.shutdown().await;
}

// ---------------------------------------------------------------------------
// Definition-level dirty flag: `tool_definitions_differ` drives the rebuild
// ---------------------------------------------------------------------------

/// The tool names of one discovered list, in list order: what the
/// definition-comparison pins below hold fixed while a per-tool definition
/// moves.
fn discovered_tool_names(tools: &[pir::mcp::manager::DiscoveredTool]) -> Vec<&str> {
    tools.iter().map(|tool| tool.tool.name.as_str()).collect()
}

/// The `mcp__*` bridge names the host's registry currently exposes, sorted.
fn mcp_bridge_names(host: &PluginHost) -> Vec<String> {
    let mut names: Vec<String> = host
        .tool_defs()
        .iter()
        .map(|def| def.name.clone())
        .filter(|name| name.starts_with("mcp__"))
        .collect();
    names.sort();
    names
}

/// Site 1 of the definition comparison (`McpManager::discover_all`): a fresh
/// discovery joins the registry (dirty), an identical re-discovery does not
/// (no redundant rebuild). The second pass runs against the same live
/// connection, so the mock's deterministic `tools/list` is byte-identical.
///
/// Revert caught: the `Some(previous) => tool_definitions_differ(...)` /
/// `None => true` pair collapsing into "any discovery is dirty" (every wait
/// hook would schedule a rebuild) or into a comparison that never reports a
/// change.
#[tokio::test]
async fn discover_all_marks_fresh_discovery_dirty_and_identical_rediscovery_clean() {
    let (_client, manager) = connect_mock_stdio().await;

    let first = manager.discover_all().await;
    let tools = first
        .get("mock")
        .expect("the fresh discovery listed the mock");
    assert_eq!(discovered_tool_names(tools), vec!["echo", "fail", "slow"]);
    assert!(
        manager.take_tools_dirty(),
        "a server appearing in the snapshot marks the registry dirty"
    );

    let second = manager.discover_all().await;
    let tools = second
        .get("mock")
        .expect("the re-discovery listed the mock");
    assert_eq!(discovered_tool_names(tools), vec!["echo", "fail", "slow"]);
    assert!(
        !manager.take_tools_dirty(),
        "an identical re-discovery must not schedule a redundant rebuild"
    );
    manager.shutdown().await;
}

/// Site 2 of the definition comparison
/// (`McpManager::publish_discovered_tools`, the one-write-lock
/// read-compare-write shared by the lazy wait and the config-sync
/// re-discovery): the first publication of a server marks the registry dirty,
/// publishing the SAME list again leaves the flag clear.
///
/// Revert caught: the compare-and-publish collapsing into an unconditional
/// `tools_dirty.store(true)` (every lazy wait over an already-published server
/// would rebuild) or into a "the snapshot already has the server, so nothing
/// changed" shortcut that never re-marks.
#[tokio::test]
async fn publishing_the_same_tool_definitions_again_does_not_mark_dirty() {
    let (_client, manager) = connect_mock_stdio().await;

    let first = manager
        .discover_server_and_publish("mock")
        .await
        .expect("the first publication");
    assert_eq!(discovered_tool_names(&first), vec!["echo", "fail", "slow"]);
    assert!(
        manager.take_tools_dirty(),
        "a server joining the snapshot marks the registry dirty"
    );

    let second = manager
        .discover_server_and_publish("mock")
        .await
        .expect("the identical re-publication");
    assert_eq!(discovered_tool_names(&second), vec!["echo", "fail", "slow"]);
    assert!(
        !manager.take_tools_dirty(),
        "republishing unchanged tool definitions must not schedule a rebuild"
    );
    manager.shutdown().await;
}

/// `McpManager::forget_discovered` drops one server's snapshot entry — the
/// config-sync removal pass's release — and leaves the dirty flag alone: that
/// pass runs inside the rebuild whose registration pass reads the snapshot
/// right after, so a mark-dirty there would only queue a redundant second
/// rebuild.
///
/// Revert caught: `forget_discovered` becoming a no-op (a removed server's
/// tools stay in the snapshot and re-register at the removing rebuild, see the
/// mid-session removal test below) or growing a `tools_dirty.store(true)` (the
/// removal schedules the extra rebuild the method's contract rules out).
#[tokio::test]
async fn forget_discovered_drops_the_snapshot_entry_without_marking_dirty() {
    let (_client, manager) = connect_mock_stdio().await;
    manager.discover_all().await;
    assert!(
        manager.take_tools_dirty(),
        "precondition: the discovery marked the registry"
    );
    assert!(
        manager.discovered_tools().contains_key("mock"),
        "precondition: the server is published"
    );

    manager.forget_discovered("mock");
    assert!(
        manager.discovered_tools().is_empty(),
        "the release dropped the server's snapshot entry"
    );
    assert!(
        !manager.take_tools_dirty(),
        "dropping a snapshot entry must not schedule a rebuild of its own"
    );
    manager.shutdown().await;
}

/// Priority-1 regression pin, the half the fixture can reach: a publish that
/// keeps every tool NAME and moves only a per-tool DEFINITION must mark the
/// registry dirty. A single live mock cannot list two definitions under one
/// name (`serve_mock_requests` derives `tools/list` from env it reads once per
/// child), but every discovered tool's exposure is resolved from the server's
/// config entry on each publish — so an edit that keeps command/args/tools
/// identical and changes only the exposure exercises exactly the same
/// "names unchanged, definitions differ" comparison a description or schema
/// edit would.
///
/// Revert caught: dropping the `exposure` clause from `tool_definitions_differ`
/// (a names-only comparison) leaves the flag clear after the re-discovery, the
/// next rebuild point registers nothing, and the `mcp__mock__echo` bridge keeps
/// the old `deferred` exposure instead of the new `direct` one.
#[tokio::test(flavor = "current_thread")]
async fn an_exposure_only_definition_change_marks_the_registry_dirty() {
    let _guard = env_guard().await;
    let dir = TempDir::new("sync-exposure");
    std::env::set_var("PIR_AGENT_DIR", &dir.0);
    let (plugin, host) = lazy_mock_host(&dir.0);
    let manager = host.mcp_manager().expect("mcp manager");

    // Publish the discovery (the lazy wait) and register the bridges at the
    // next rebuild point.
    let registry = resource_tool_registry(manager);
    let waited = plugin
        .wait_for_tool_call(
            &registry,
            pir::mcp::registry::LIST_MCP_RESOURCES_TOOL,
            &json!({}),
        )
        .await;
    assert_eq!(waited, vec!["mock".to_owned()]);
    let mut payload = json!({ "type": "agent_settled" });
    host.emit("agent_settled", &mut payload)
        .await
        .expect("emit");
    assert!(
        !manager.take_tools_dirty(),
        "the rebuild consumed the discovery's flag"
    );
    assert_eq!(
        mcp_bridge_names(&host),
        vec!["mcp__mock__echo", "mcp__mock__fail", "mcp__mock__slow"]
    );
    let echo = host
        .tool_defs()
        .into_iter()
        .find(|def| def.name == "mcp__mock__echo")
        .expect("echo bridge");
    assert_eq!(
        echo.exposure, "deferred",
        "the default exposure is codemode -> deferred"
    );

    // Mid-session edit: same server, same tools, exposure only.
    let mut entry = mock_mcp_json_entry();
    entry["exposure"] = json!("direct");
    std::fs::write(dir.0.join("mcp.json"), json!({ "mock": entry }).to_string())
        .expect("rewrite mcp.json");

    // `reload` is a rebuild point: the config pass re-registers the changed
    // entry, and the rebuild-tail flush releases and re-discovers it (the same
    // tool list comes back, under the new exposure).
    host.reload().await.expect("reload");
    let discovered = manager.discovered_tools();
    let tools = discovered
        .get("mock")
        .expect("the rebuild tail re-discovered the server");
    assert_eq!(
        discovered_tool_names(tools),
        vec!["echo", "fail", "slow"],
        "the definition change kept every tool name"
    );
    assert!(
        tools
            .iter()
            .all(|tool| tool.exposure == pir::config::mcp::McpExposure::Direct),
        "the re-discovery resolved the new entry's exposure: {:?}",
        tools.iter().map(|tool| tool.exposure).collect::<Vec<_>>()
    );

    // The definition comparison is what schedules the re-registration.
    host.emit("agent_settled", &mut payload)
        .await
        .expect("emit");
    assert!(
        !manager.take_tools_dirty(),
        "the rebuild consumed the exposure-change flag"
    );
    let echo = host
        .tool_defs()
        .into_iter()
        .find(|def| def.name == "mcp__mock__echo")
        .expect("echo bridge");
    assert_eq!(
        echo.exposure, "direct",
        "the same-named tool was re-registered with the new definition"
    );
    manager.shutdown().await;
}

// ---------------------------------------------------------------------------
// Mid-session `mcp.json` sync: removed and changed entries release the state
// derived from them and re-register from the file
// ---------------------------------------------------------------------------

/// Load a JS extension that registers one OFFLINE MCP server: the fixture's
/// "a runtime registration landed after the host's snapshot" rebuild trigger
/// (the `mcp-late.ts` shape). `enabled: false` keeps it from ever connecting,
/// so the only effect is that the host's next emit rebuilds the registry
/// (`PluginHost::refresh_registry_if_changed`) and thereby re-runs
/// `McpPlugin::register` — the config-file pass a mid-session `mcp.json` edit
/// needs to be noticed.
async fn load_offline_server_registration(
    js: &ExtensionHandle,
    dir: &Path,
    file: &str,
    name: &str,
) {
    let path = dir.join(file);
    let body = format!(
        r#"
        export default function (pi) {{
          pi.registerMcpServer("{name}", {{
            command: "definitely-not-a-real-command-97531",
            enabled: false,
          }});
        }}
        "#
    );
    std::fs::write(&path, body).expect("write late fixture");
    let result = js
        .load(std::slice::from_ref(&path))
        .await
        .expect("load late");
    assert!(result.errors.is_empty(), "load errors: {:?}", result.errors);
}

/// Mid-session `mcp.json` REMOVAL, the observable contract: once the entry is
/// gone from the file, the next registry rebuild unregisters the server, drops
/// its discovered snapshot entry (`McpManager::forget_discovered`, so the SAME
/// rebuild's registration pass registers no `mcp__*` bridge for it), and the
/// rebuild-tail flush releases its live connection
/// (`McpManager::disconnect`, so the removed entry's child does not outlive
/// the entry).
///
/// Revert caught: skipping `forget_discovered` re-registers the removed
/// server's bridges from the stale snapshot at the removing rebuild; skipping
/// the removal pass leaves the registry entry itself; skipping the follow-up's
/// `disconnect` leaves the live connection behind (`has_connection` still
/// true).
#[tokio::test(flavor = "current_thread")]
async fn mid_session_mcp_json_removal_drops_the_snapshot_and_the_connection() {
    let _guard = env_guard().await;
    let dir = TempDir::new("sync-removal");
    std::env::set_var("PIR_AGENT_DIR", &dir.0);
    std::fs::write(
        dir.0.join("mcp.json"),
        json!({ "mock": mock_mcp_json_entry() }).to_string(),
    )
    .expect("write mcp.json");

    let js = ExtensionHandle::start(flush_timing_extension_config(&dir.0, "sync-removal"))
        .expect("runtime");
    let plugin = Arc::new(McpPlugin::with_cwd(&dir.0, &dir.0));
    let host = PluginHost::build(
        Some(js.clone()),
        vec![plugin.clone() as Arc<dyn pir::plugins::AgentPlugin>],
    );
    let manager = host.mcp_manager().expect("mcp manager");
    assert!(
        manager.registry().get("mock").is_some(),
        "the config pass registered the mock server"
    );

    // Publish through the lazy wait, then register the bridges — and the live
    // connection the removal must release.
    let registry = resource_tool_registry(manager);
    let waited = plugin
        .wait_for_tool_call(
            &registry,
            pir::mcp::registry::LIST_MCP_RESOURCES_TOOL,
            &json!({}),
        )
        .await;
    assert_eq!(waited, vec!["mock".to_owned()]);
    let mut payload = json!({ "type": "agent_settled" });
    host.emit("agent_settled", &mut payload)
        .await
        .expect("emit");
    assert_eq!(
        mcp_bridge_names(&host),
        vec!["mcp__mock__echo", "mcp__mock__fail", "mcp__mock__slow"]
    );
    assert!(
        manager.has_connection("mock").await,
        "the lazy discovery left a live connection"
    );

    // Mid-session edit: the entry is gone from the file.
    std::fs::write(dir.0.join("mcp.json"), json!({}).to_string()).expect("rewrite mcp.json");

    // The rebuild that notices the edit (the offline registration is only the
    // host-visible trigger).
    load_offline_server_registration(&js, &dir.0, "mcp-sync-removal.ts", "sync-probe").await;
    host.emit("agent_settled", &mut payload)
        .await
        .expect("emit");

    assert!(
        manager.registry().get("mock").is_none(),
        "the missing file entry unregistered the server"
    );
    assert!(
        manager.discovered_tools().is_empty(),
        "the removal released the snapshot entry: {:?}",
        manager.discovered_tools()
    );
    assert!(
        mcp_bridge_names(&host).is_empty(),
        "the removing rebuild registered no bridges for the removed server: {:?}",
        mcp_bridge_names(&host)
    );
    assert!(
        !manager.has_connection("mock").await,
        "the removal released the live connection"
    );
    manager.shutdown().await;
}

/// Mid-session `mcp.json` CHANGE: the entry keeps its name but its config
/// moves (here the env selecting the mock child's tool list), so the next
/// registry rebuild must release the connection created from the OLD entry
/// (`McpManager::disconnect`) and re-discover from the file's CURRENT entry —
/// publishing the new entry's tools, which the following rebuild point
/// registers as the new bridges.
///
/// Revert caught: without the release, `discover_server_and_publish` lists
/// through the STILL-LIVE connection created from the old entry, the tools
/// come back unchanged, the publication sets no flag, and no further rebuild
/// happens — the test then fails on the published names (the snapshot keeps
/// `echo`/`fail`/`slow`) and on the bridge names (`mcp__mock__paged` never
/// appears).
#[tokio::test(flavor = "current_thread")]
async fn mid_session_mcp_json_change_releases_the_stale_connection_and_registers_the_new_entry() {
    let _guard = env_guard().await;
    let dir = TempDir::new("sync-change");
    std::env::set_var("PIR_AGENT_DIR", &dir.0);
    std::fs::write(
        dir.0.join("mcp.json"),
        json!({ "mock": mock_mcp_json_entry() }).to_string(),
    )
    .expect("write mcp.json");

    let js = ExtensionHandle::start(flush_timing_extension_config(&dir.0, "sync-change"))
        .expect("runtime");
    let plugin = Arc::new(McpPlugin::with_cwd(&dir.0, &dir.0));
    let host = PluginHost::build(
        Some(js.clone()),
        vec![plugin.clone() as Arc<dyn pir::plugins::AgentPlugin>],
    );
    let manager = host.mcp_manager().expect("mcp manager");

    // Publish the OLD entry's tools, register its bridges — and its live
    // connection, which the change must release.
    let registry = resource_tool_registry(manager);
    let waited = plugin
        .wait_for_tool_call(
            &registry,
            pir::mcp::registry::LIST_MCP_RESOURCES_TOOL,
            &json!({}),
        )
        .await;
    assert_eq!(waited, vec!["mock".to_owned()]);
    let mut payload = json!({ "type": "agent_settled" });
    host.emit("agent_settled", &mut payload)
        .await
        .expect("emit");
    assert!(
        !manager.take_tools_dirty(),
        "the rebuild consumed the discovery's flag"
    );
    assert_eq!(
        mcp_bridge_names(&host),
        vec!["mcp__mock__echo", "mcp__mock__fail", "mcp__mock__slow"]
    );

    // Mid-session edit: the same server name, an entry that serves a different
    // tool list (`PIR_MOCK_PAGED` switches the mock child's `tools/list`).
    let mut entry = mock_mcp_json_entry();
    entry["env"]["PIR_MOCK_PAGED"] = json!("single");
    std::fs::write(dir.0.join("mcp.json"), json!({ "mock": entry }).to_string())
        .expect("rewrite mcp.json");

    // The rebuild that notices the edit: the config pass re-registers the
    // entry, the rebuild tail releases the old connection and re-discovers.
    load_offline_server_registration(&js, &dir.0, "mcp-sync-change.ts", "sync-probe").await;
    host.emit("agent_settled", &mut payload)
        .await
        .expect("emit");
    let discovered = manager.discovered_tools();
    assert_eq!(
        discovered_tool_names(
            discovered
                .get("mock")
                .expect("the rebuild tail re-discovered the server")
        ),
        vec!["paged"],
        "the re-discovery listed through the file's CURRENT entry"
    );

    // The next rebuild point consumes the publication and swaps the bridges.
    host.emit("agent_settled", &mut payload)
        .await
        .expect("emit");
    assert!(
        !manager.take_tools_dirty(),
        "the rebuild consumed the re-discovery's flag"
    );
    assert_eq!(
        mcp_bridge_names(&host),
        vec!["mcp__mock__paged"],
        "the old entry's bridges are gone; the new entry's bridge is registered"
    );
    let result = host
        .call_tool("mcp__mock__paged", "sync-change-1", &json!({}))
        .await
        .expect("the new entry's bridge dispatches to the reconnected server");
    assert_eq!(
        result
            .content
            .first()
            .map(|item| item.text.clone())
            .unwrap_or_default(),
        "echo:"
    );
    manager.shutdown().await;
}

// ---------------------------------------------------------------------------
// Mid-session JS-def sync: a re-registration replaces the entry, so the
// manager state derived from the replaced entry must be released too
// ---------------------------------------------------------------------------

/// The `pi.registerMcpServer("mock", ...)` fixture body: the real mock stdio
/// server as an EXTENSION-registered entry, with `extra_env` merged into the
/// mock's own switch (so a re-registration can move the child's `tools/list`,
/// like the `mcp.json` change test's `PIR_MOCK_PAGED` edit).
fn js_mock_server_registration(extra_env: &[(&str, &str)]) -> String {
    let (command, args) = mock_server_spawn();
    let mut env = BTreeMap::from([("PIR_MOCK_MCP_SERVER".to_owned(), "1".to_owned())]);
    for (key, value) in extra_env {
        env.insert((*key).to_owned(), (*value).to_owned());
    }
    format!(
        r#"
        export default function (pi) {{
          pi.registerMcpServer("mock", {{
            command: {},
            args: {},
            env: {},
            timeout: 20,
          }});
        }}
        "#,
        serde_json::to_string(&command).expect("command json"),
        serde_json::to_string(&args).expect("args json"),
        serde_json::to_string(&env).expect("env json"),
    )
}

/// Mid-session JS-def CHANGE, the JS-side counterpart of the `mcp.json`
/// change test above: a late-loaded extension file re-registers an existing
/// server under the same name with a new config. The JS pass replaces the
/// registry entry, so the rebuild tail must release the connection created
/// from the OLD entry and re-discover from the new one — the same follow-up
/// the config pass queues for its own added/changed entries.
///
/// Revert caught: without the follow-up the replaced entry leaves the live
/// connection and the discovered snapshot untouched (`ServerConnection` holds
/// the entry it was created with and is never refreshed), the re-discovery
/// never runs, and the session keeps listing the OLD child's tools — the
/// published names stay `echo`/`fail`/`slow` instead of `paged`, and the
/// bridges never swap.
///
/// The audit's spec asserted `!manager.has_connection("mock")` after the
/// change rebuild; that cannot hold with the fix, by design: the release is
/// immediately followed by the re-discovery, which maps a NEW connection
/// (`McpManager::connection` re-creates it from the registry's current
/// entry). The release is observed here the way the `mcp.json` change test
/// observes it — through the tool list only the NEW child can report — plus
/// the live new connection the discovery leaves behind.
#[tokio::test(flavor = "current_thread")]
async fn mid_session_js_config_change_releases_the_stale_connection() {
    let _guard = env_guard().await;
    let dir = TempDir::new("sync-js-change");
    std::env::set_var("PIR_AGENT_DIR", &dir.0);

    // The extension registers the mock server with env A (echo/fail/slow).
    let extension = dir.0.join("mcp-js-server.ts");
    std::fs::write(&extension, js_mock_server_registration(&[])).expect("write fixture");
    let js = ExtensionHandle::start(flush_timing_extension_config(&dir.0, "sync-js-change"))
        .expect("runtime");
    let result = js
        .load(std::slice::from_ref(&extension))
        .await
        .expect("load");
    assert!(result.errors.is_empty(), "load errors: {:?}", result.errors);

    let plugin = Arc::new(McpPlugin::with_cwd(&dir.0, &dir.0));
    let host = PluginHost::build(
        Some(js.clone()),
        vec![plugin.clone() as Arc<dyn pir::plugins::AgentPlugin>],
    );
    let manager = host.mcp_manager().expect("mcp manager");
    assert!(
        manager.registry().get("mock").is_some(),
        "the JS registration landed in the builtin registry at the build"
    );

    // Publish through the lazy wait, register the bridges — and the live
    // connection created from env A, which the change must release.
    let registry = resource_tool_registry(manager);
    let waited = plugin
        .wait_for_tool_call(
            &registry,
            pir::mcp::registry::LIST_MCP_RESOURCES_TOOL,
            &json!({}),
        )
        .await;
    assert_eq!(waited, vec!["mock".to_owned()]);
    let mut payload = json!({ "type": "agent_settled" });
    host.emit("agent_settled", &mut payload)
        .await
        .expect("emit");
    assert_eq!(
        mcp_bridge_names(&host),
        vec!["mcp__mock__echo", "mcp__mock__fail", "mcp__mock__slow"]
    );
    assert!(
        manager.has_connection("mock").await,
        "the lazy discovery left a live connection"
    );

    // Mid-session JS change: a late-loaded file re-registers the same name
    // with env B. The host's definition fingerprint notices the change and
    // re-runs the registration pass, whose JS-def arm replaces the entry.
    let late = dir.0.join("mcp-js-change.ts");
    std::fs::write(
        &late,
        js_mock_server_registration(&[("PIR_MOCK_PAGED", "single")]),
    )
    .expect("write late fixture");
    let result = js
        .load(std::slice::from_ref(&late))
        .await
        .expect("load late");
    assert!(result.errors.is_empty(), "load errors: {:?}", result.errors);

    // The rebuild that notices the re-registration: the JS pass replaces the
    // entry and queues the release + re-discovery; the rebuild tail releases
    // the old connection and lists through the NEW entry.
    host.emit("agent_settled", &mut payload)
        .await
        .expect("emit");
    let discovered = manager.discovered_tools();
    assert_eq!(
        discovered_tool_names(
            discovered
                .get("mock")
                .expect("the rebuild tail re-discovered the server")
        ),
        vec!["paged"],
        "the re-discovery listed through the JS re-registration's CURRENT entry"
    );
    assert_eq!(
        manager.connection_states().await.get("mock"),
        Some(&pir::mcp::manager::ConnectionState::Connected),
        "the released connection was replaced by one built from the new entry"
    );

    // The next rebuild point consumes the publication and swaps the bridges.
    host.emit("agent_settled", &mut payload)
        .await
        .expect("emit");
    assert!(
        !manager.take_tools_dirty(),
        "the rebuild consumed the re-discovery's flag"
    );
    assert_eq!(
        mcp_bridge_names(&host),
        vec!["mcp__mock__paged"],
        "the replaced entry's bridges are gone; the new entry's bridge is registered"
    );
    let result = host
        .call_tool("mcp__mock__paged", "sync-js-change-1", &json!({}))
        .await
        .expect("the new entry's bridge dispatches to the reconnected server");
    assert_eq!(
        result
            .content
            .first()
            .map(|item| item.text.clone())
            .unwrap_or_default(),
        "echo:"
    );
    manager.shutdown().await;
}

/// Mid-session re-ADD across the two passes, the other half of the JS-side
/// follow-up: a `mcp.json` entry is DELETED while the JS def it shadowed is
/// still registered, so the config pass removes the name (queuing the release)
/// and the JS pass re-registers it in the SAME rebuild. The removal's
/// `forget_discovered` already dropped the snapshot entry, so without
/// cancelling the queued removal with a re-discovery the re-added server never
/// comes back: no snapshot, no bridges, until the lazy path happens to reach
/// it.
///
/// Revert caught: the queued `removed` survives the JS re-registration, the
/// rebuild tail only disconnects, and `discovered_tools()` has no `mock`
/// entry after the rebuild — the re-added server's tools never re-register
/// (`expect("the JS re-add re-discovered the server")` panics).
#[tokio::test(flavor = "current_thread")]
async fn mid_session_js_readd_after_config_removal_rediscovers_the_server() {
    let _guard = env_guard().await;
    let dir = TempDir::new("sync-js-readd");
    std::env::set_var("PIR_AGENT_DIR", &dir.0);

    // The JS extension registers `mock` with env B, shadowed by the same-name
    // `mcp.json` entry (env A) for as long as the file carries it.
    let extension = dir.0.join("mcp-js-readd-server.ts");
    std::fs::write(
        &extension,
        js_mock_server_registration(&[("PIR_MOCK_PAGED", "single")]),
    )
    .expect("write fixture");
    std::fs::write(
        dir.0.join("mcp.json"),
        json!({ "mock": mock_mcp_json_entry() }).to_string(),
    )
    .expect("write mcp.json");
    let js = ExtensionHandle::start(flush_timing_extension_config(&dir.0, "sync-js-readd"))
        .expect("runtime");
    let result = js
        .load(std::slice::from_ref(&extension))
        .await
        .expect("load");
    assert!(result.errors.is_empty(), "load errors: {:?}", result.errors);

    let plugin = Arc::new(McpPlugin::with_cwd(&dir.0, &dir.0));
    let host = PluginHost::build(
        Some(js.clone()),
        vec![plugin.clone() as Arc<dyn pir::plugins::AgentPlugin>],
    );
    let manager = host.mcp_manager().expect("mcp manager");
    let entry = manager.registry().get("mock").expect("registered");
    assert!(
        matches!(
            entry.source,
            pir::mcp::registry::McpServerSource::GlobalConfig(_)
        ),
        "the mcp.json entry shadows the JS registration: {:?}",
        entry.source
    );

    // Publish the config entry's tools (env A) and register its bridges.
    let registry = resource_tool_registry(manager);
    let waited = plugin
        .wait_for_tool_call(
            &registry,
            pir::mcp::registry::LIST_MCP_RESOURCES_TOOL,
            &json!({}),
        )
        .await;
    assert_eq!(waited, vec!["mock".to_owned()]);
    let mut payload = json!({ "type": "agent_settled" });
    host.emit("agent_settled", &mut payload)
        .await
        .expect("emit");
    assert_eq!(
        mcp_bridge_names(&host),
        vec!["mcp__mock__echo", "mcp__mock__fail", "mcp__mock__slow"]
    );
    assert!(
        manager.has_connection("mock").await,
        "the lazy discovery left a live connection"
    );

    // Mid-session: the `mcp.json` entry is deleted. The config pass removes
    // the name and queues its release; the JS def, no longer shadowed,
    // re-registers it in the same rebuild (env B). The offline registration is
    // only the host-visible rebuild trigger.
    std::fs::write(dir.0.join("mcp.json"), json!({}).to_string()).expect("rewrite mcp.json");
    load_offline_server_registration(&js, &dir.0, "mcp-js-readd.ts", "readd-probe").await;
    host.emit("agent_settled", &mut payload)
        .await
        .expect("emit");

    // The re-add cancelled the queued removal: the re-discovery listed through
    // the JS entry (env B -> `paged`) instead of leaving the name without a
    // snapshot and bridges.
    let discovered = manager.discovered_tools();
    assert_eq!(
        discovered_tool_names(
            discovered
                .get("mock")
                .expect("the JS re-add re-discovered the server")
        ),
        vec!["paged"],
        "the re-discovery listed through the JS registration's entry"
    );

    // The next rebuild point consumes the publication and registers the
    // re-added server's bridges.
    host.emit("agent_settled", &mut payload)
        .await
        .expect("emit");
    assert_eq!(
        mcp_bridge_names(&host),
        vec!["mcp__mock__paged"],
        "the re-added server's bridge is registered"
    );
    manager.shutdown().await;
}

/// B3 regression: the registration pass must not bridge a discovered
/// snapshot entry whose server is absent from the registry. The state is
/// built directly (a publication for a still-registered server, then the
/// registry entry removed WITHOUT `forget_discovered`) because that is
/// exactly the state a publication landing after a removal's
/// `forget_discovered` leaves behind: snapshot present, registry entry gone.
///
/// The extension-sourced registration keeps both passes out of the way — the
/// config pass only removes config-sourced entries, and no JS runtime is
/// attached — so only the registration-pass filter can keep the bridges out.
///
/// Revert caught: without the filter the next rebuild re-registers
/// `mcp__mock__echo/fail/slow` from the stale snapshot, resurrecting a server
/// the registry no longer has (calls through those bridges would fail with
/// `UnknownServer`, and the session would believe the server is available).
#[tokio::test(flavor = "current_thread")]
async fn a_snapshot_entry_for_an_unregistered_server_registers_no_bridges() {
    let _guard = env_guard().await;
    let dir = TempDir::new("registry-filter");
    std::env::set_var("PIR_AGENT_DIR", &dir.0);

    let plugin = Arc::new(McpPlugin::with_cwd(&dir.0, &dir.0));
    let (command, args) = mock_server_spawn();
    plugin.manager().registry().register(McpServerEntry {
        name: "mock".into(),
        config: serde_json::from_value(json!({
            "command": command,
            "args": args,
            "env": { "PIR_MOCK_MCP_SERVER": "1" },
            "timeout": 20,
        }))
        .expect("config"),
        // Extension-sourced: `sync_config_files` leaves it alone, and no
        // JS runtime is attached, so nothing re-registers it.
        source: pir::mcp::registry::McpServerSource::Extension("registry-filter-test".to_owned()),
    });
    let host = PluginHost::build(
        None,
        vec![plugin.clone() as Arc<dyn pir::plugins::AgentPlugin>],
    );
    let manager = host.mcp_manager().expect("mcp manager");

    // Publish the discovery through the lazy wait: the snapshot entry exists
    // while the registry still knows the server.
    let registry = resource_tool_registry(manager);
    let waited = plugin
        .wait_for_tool_call(
            &registry,
            pir::mcp::registry::LIST_MCP_RESOURCES_TOOL,
            &json!({}),
        )
        .await;
    assert_eq!(waited, vec!["mock".to_owned()]);
    assert_eq!(
        discovered_tool_names(
            manager
                .discovered_tools()
                .get("mock")
                .expect("the wait published the discovery")
        ),
        vec!["echo", "fail", "slow"]
    );

    // The server leaves the registry with the snapshot left behind — the
    // post-race state described above. The dirty flag from the publication is
    // still pending, so the next rebuild re-runs the registration pass.
    manager.registry().unregister("mock");
    assert!(
        manager.registry().get("mock").is_none(),
        "precondition: the registry no longer has the server"
    );
    assert!(
        manager.discovered_tools().contains_key("mock"),
        "precondition: the snapshot still carries its tools"
    );

    let mut payload = json!({ "type": "agent_settled" });
    host.emit("agent_settled", &mut payload)
        .await
        .expect("emit");
    assert!(
        mcp_bridge_names(&host).is_empty(),
        "an unregistered server's stale snapshot entry must not register bridges: {:?}",
        mcp_bridge_names(&host)
    );
    manager.shutdown().await;
}

/// B2 regression: `disconnect` removes the live connection from the manager's
/// map and then shuts it down. A `call_tool` that already holds the removed
/// connection object used to reconnect through that orphan on its retry path
/// (`drop_client` + attempt 2) — with the orphan's own stale `entry` — and
/// since the object is out of the map, no later `McpManager::shutdown` could
/// ever stop that child. The detached flag makes every later `get_client` on
/// the object fail with `McpError::ConnectionClosed` instead.
///
/// Revert caught: without the flag the in-flight call's retry reconnects
/// through the orphan (same config, so it succeeds after the mock's delay),
/// returns `Ok`, and the child it spawned outlives the test's shutdown. The
/// assertion on the error fails instead.
#[tokio::test(flavor = "current_thread")]
async fn disconnect_detaches_the_connection_so_an_in_flight_call_cannot_reconnect() {
    let _guard = env_guard().await;
    let dir = TempDir::new("detach");
    std::env::set_var("PIR_AGENT_DIR", &dir.0);
    let manager = McpManager::without_log(&dir.0);
    let (command, args) = mock_server_spawn();
    let config: McpServerConfig = serde_json::from_value(json!({
        "command": command,
        "args": args,
        // `slow` answers only after the delay, so the call below is still
        // pending when the disconnect drops the connection underneath it.
        "env": { "PIR_MOCK_MCP_SERVER": "1", "PIR_MOCK_DELAY_MS": "2000" },
        "timeout": 20,
    }))
    .expect("config");
    manager.registry().register(McpServerEntry {
        name: "mock".into(),
        config,
        source: pir::mcp::registry::McpServerSource::Extension("detach-test".into()),
    });
    manager
        .get_client("mock")
        .await
        .expect("the mock server connects");

    let call = tokio::spawn({
        let manager = Arc::clone(&manager);
        async move { manager.call_tool("mock", "slow", &json!({}), None).await }
    });
    tokio::time::sleep(Duration::from_millis(500)).await;
    manager.disconnect("mock").await;
    assert!(
        !manager.has_connection("mock").await,
        "the disconnect removed the mapped connection"
    );
    let result = call.await.expect("the in-flight call joins");
    assert!(
        matches!(result, Err(McpError::ConnectionClosed)),
        "the detached connection's retry must fail closed instead of reconnecting: {result:?}"
    );
    manager.shutdown().await;
}

// ---------------------------------------------------------------------------
// The built-in `sd` MCP server (`pir mcp-sd`): the pir binary itself is the
// server here, spawned as a child process and driven through pir's own MCP
// client — the whole stdio surface end to end.
// ---------------------------------------------------------------------------

/// Spawn options for the built-in `sd` server: the binary under test, its
/// hidden `mcp-sd` subcommand, and the directory the tool may touch.
fn sd_server_spawn(root: &Path) -> StdioSpawnOptions {
    StdioSpawnOptions {
        command: env!("CARGO_BIN_EXE_pir").to_owned(),
        args: vec![
            "mcp-sd".to_owned(),
            "--root".to_owned(),
            root.to_string_lossy().into_owned(),
        ],
        cwd: root.to_path_buf(),
        ..StdioSpawnOptions::default()
    }
}

/// The text of the first content block of a tool result.
fn tool_result_text(result: &pir::mcp::protocol::CallToolResult) -> String {
    result
        .content
        .first()
        .and_then(|block| block.text.clone())
        .unwrap_or_default()
}

#[tokio::test]
async fn builtin_sd_server_serves_the_sd_tool_to_pirs_own_mcp_client() {
    let dir = TempDir::new("mcp-sd-e2e");
    std::fs::write(dir.0.join("notes.txt"), "hello world\n").expect("write fixture");

    let client =
        McpClient::connect_stdio(sd_server_spawn(&dir.0), None, "sd", Duration::from_secs(20))
            .await
            .expect("the built-in sd server completes the initialize handshake");

    // initialize: the server identifies itself and advertises tools.
    assert_eq!(
        client.server_info().expect("server info").name,
        "pir-mcp-sd"
    );
    assert!(
        client
            .server_capabilities()
            .expect("capabilities")
            .tools
            .is_some(),
        "the server must advertise the tools capability"
    );

    // tools/list: exactly the `sd` tool, with the full argument schema.
    let tools = client.list_all_tools().await.expect("tools/list");
    assert_eq!(tools.len(), 1, "one tool: {tools:?}");
    assert_eq!(tools[0].name, "sd");
    let schema = tools[0].input_schema.as_object().expect("object schema");
    assert_eq!(schema["required"], json!(["pattern", "replacement"]));
    assert_eq!(schema["properties"]["dry_run"]["default"], json!(true));
    assert_eq!(schema["properties"]["paths"]["type"], json!("array"));
    assert_eq!(schema["additionalProperties"], json!(false));

    // tools/call, dry run (the default): a summary comes back and NOTHING is
    // written to disk.
    let dry = client
        .call_tool("sd", &json!({"pattern": "world", "replacement": "there"}))
        .await
        .expect("dry-run call");
    assert_eq!(dry.is_error, Some(false));
    let structured = dry
        .structured_content
        .as_ref()
        .expect("the sd tool returns structured content");
    assert_eq!(structured["dry_run"], json!(true));
    assert_eq!(structured["files_scanned"], json!(1));
    assert_eq!(structured["files_changed"], json!(1));
    assert_eq!(structured["replacements"], json!(1));
    assert_eq!(structured["files"][0]["path"], json!("notes.txt"));
    let text = tool_result_text(&dry);
    assert!(text.contains("dry run"), "text: {text}");
    assert_eq!(
        std::fs::read_to_string(dir.0.join("notes.txt")).expect("read"),
        "hello world\n",
        "a dry run must leave the file untouched"
    );

    // tools/call, real run: the file is rewritten and the counts come back.
    let applied = client
        .call_tool(
            "sd",
            &json!({"pattern": "world", "replacement": "there", "dry_run": false}),
        )
        .await
        .expect("write call");
    assert_eq!(applied.is_error, Some(false));
    let structured = applied
        .structured_content
        .as_ref()
        .expect("structured content");
    assert_eq!(structured["dry_run"], json!(false));
    assert_eq!(structured["files_changed"], json!(1));
    assert_eq!(structured["replacements"], json!(1));
    assert_eq!(
        std::fs::read_to_string(dir.0.join("notes.txt")).expect("read"),
        "hello there\n"
    );

    client.close().await;
}

#[tokio::test]
async fn builtin_sd_server_refuses_paths_outside_its_root() {
    let root = TempDir::new("mcp-sd-root");
    let outside = TempDir::new("mcp-sd-outside");
    std::fs::write(root.0.join("inside.txt"), "hello world\n").expect("write");
    std::fs::write(outside.0.join("secret.txt"), "hello world\n").expect("write");

    let client = McpClient::connect_stdio(
        sd_server_spawn(&root.0),
        None,
        "sd",
        Duration::from_secs(20),
    )
    .await
    .expect("connect");

    // An absolute path outside the root, and a relative one that climbs out
    // of it with `..`.
    let outside_name = outside
        .0
        .file_name()
        .expect("temp dir name")
        .to_string_lossy()
        .into_owned();
    for escape in [
        outside.0.join("secret.txt").to_string_lossy().into_owned(),
        format!("../{outside_name}/secret.txt"),
    ] {
        let result = client
            .call_tool(
                "sd",
                &json!({
                    "pattern": "world",
                    "replacement": "changed",
                    "dry_run": false,
                    "paths": [escape],
                }),
            )
            .await
            .expect("the call is answered (as a tool-level error)");
        assert_eq!(
            result.is_error,
            Some(true),
            "a path outside --root must fail: {result:?}"
        );
        let text = tool_result_text(&result);
        assert!(
            text.contains("outside --root"),
            "the refusal must say why, got: {text}"
        );
    }

    // Nothing escaped: the file outside the root is byte-identical, and the
    // root's own file was not part of the refused calls either.
    assert_eq!(
        std::fs::read_to_string(outside.0.join("secret.txt")).expect("read"),
        "hello world\n"
    );
    assert_eq!(
        std::fs::read_to_string(root.0.join("inside.txt")).expect("read"),
        "hello world\n"
    );

    client.close().await;
}

// ---------------------------------------------------------------------------
// Client public surface: roots/list, setRequestHandler, per-request
// progress/timeout (upstream `pi-mcp/client.ts`)
// ---------------------------------------------------------------------------

use pir::mcp::client::{McpClientOptions, McpRequestOptions, McpRoot};

async fn connect_mock_with_options(
    server_name: &str,
    extra_env: &[(&str, &str)],
    options: McpClientOptions,
) -> McpClient {
    let (command, args) = mock_server_spawn();
    let mut env = mock_server_env();
    for (key, value) in extra_env {
        env.insert((*key).to_owned(), (*value).to_owned());
    }
    let spawn = StdioSpawnOptions {
        command,
        args,
        cwd: std::env::current_dir().expect("cwd"),
        env,
        max_message_bytes: 1024 * 1024,
    };
    McpClient::connect_stdio_with_options(
        spawn,
        None,
        server_name,
        Duration::from_secs(20),
        options,
    )
    .await
    .expect("handshake")
}

#[tokio::test]
async fn roots_capability_answers_a_server_roots_list_request() {
    let client = connect_mock_with_options(
        "roots",
        &[("PIR_MOCK_PROBE_METHOD", "roots/list")],
        McpClientOptions::new().with_roots(vec![
            McpRoot::new("file:///work").with_name("work"),
            McpRoot::new("file:///tmp"),
        ]),
    )
    .await;
    assert!(
        client.roots().is_some(),
        "the configured roots source is exposed"
    );
    let result = client
        .call_tool("probe", &json!({}))
        .await
        .expect("probe tool call");
    let answer: Value =
        serde_json::from_str(&tool_result_text(&result)).expect("probe answer is JSON");
    let roots = answer["result"]["roots"]
        .as_array()
        .unwrap_or_else(|| panic!("roots/list result: {answer}"));
    assert_eq!(roots.len(), 2, "{answer}");
    assert_eq!(roots[0]["uri"], json!("file:///work"));
    assert_eq!(roots[0]["name"], json!("work"));
    assert_eq!(roots[1]["uri"], json!("file:///tmp"));
    assert!(
        roots[1].get("name").is_none(),
        "an unnamed root omits name: {answer}"
    );
    client.close().await;
}

#[tokio::test]
async fn set_request_handler_answers_custom_methods_and_unregisters_on_drop() {
    let client = connect_mock_with_options(
        "custom",
        &[
            ("PIR_MOCK_PROBE_METHOD", "mock/custom"),
            ("PIR_MOCK_PROBE_PARAMS", r#"{"n":7}"#),
        ],
        McpClientOptions::new(),
    )
    .await;
    let seen = Arc::new(Mutex::new(Vec::<Value>::new()));
    let handler_seen = Arc::clone(&seen);
    let guard = client.set_request_handler("mock/custom", move |params, signal| {
        let seen = Arc::clone(&handler_seen);
        async move {
            assert!(!signal.is_aborted());
            seen.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(params.clone().unwrap_or(Value::Null));
            Ok(json!({ "echo": params }))
        }
    });

    let result = client
        .call_tool("probe", &json!({}))
        .await
        .expect("probe tool call");
    let answer: Value =
        serde_json::from_str(&tool_result_text(&result)).expect("probe answer is JSON");
    assert_eq!(answer["result"], json!({ "echo": { "n": 7 } }), "{answer}");
    assert_eq!(
        seen.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_slice(),
        &[json!({ "n": 7 })]
    );

    // Dropping the guard removes the handler: the next probe sees
    // JSON-RPC method-not-found (-32601).
    drop(guard);
    let result = client
        .call_tool("probe", &json!({}))
        .await
        .expect("second probe tool call");
    let answer: Value =
        serde_json::from_str(&tool_result_text(&result)).expect("probe answer is JSON");
    assert_eq!(
        answer["error"]["code"],
        json!(-32601),
        "unregistered method: {answer}"
    );
    client.close().await;
}

#[tokio::test]
async fn set_request_handler_overrides_roots_list_and_ping() {
    let client = connect_mock_with_options(
        "handlers",
        &[("PIR_MOCK_PROBE_METHOD", "roots/list")],
        McpClientOptions::new(),
    )
    .await;
    let guard = client.set_request_handler("roots/list", |_params, _signal| async {
        Ok(json!({ "roots": [{ "uri": "file:///handler" }] }))
    });
    let result = client
        .call_tool("probe", &json!({}))
        .await
        .expect("probe tool call");
    let answer: Value =
        serde_json::from_str(&tool_result_text(&result)).expect("probe answer is JSON");
    assert_eq!(
        answer["result"]["roots"][0]["uri"],
        json!("file:///handler"),
        "{answer}"
    );
    drop(guard);
    client.close().await;

    // A `ping` handler is invoked (the response stays the empty ping result).
    let ping_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let client = connect_mock_with_options(
        "ping-handler",
        &[("PIR_MOCK_PROBE_METHOD", "ping")],
        McpClientOptions::new(),
    )
    .await;
    let calls = Arc::clone(&ping_calls);
    let _guard = client.set_request_handler("ping", move |_params, _signal| {
        let calls = Arc::clone(&calls);
        async move {
            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(json!({}))
        }
    });
    let result = client
        .call_tool("probe", &json!({}))
        .await
        .expect("probe tool call");
    let answer: Value =
        serde_json::from_str(&tool_result_text(&result)).expect("probe answer is JSON");
    assert!(answer.get("result").is_some(), "{answer}");
    assert_eq!(
        ping_calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the ping handler ran once"
    );
    client.close().await;
}

#[tokio::test]
async fn per_request_timeout_overrides_the_client_default() {
    let client = connect_mock_with_options(
        "timeout",
        &[("PIR_MOCK_DELAY_MS", "1200")],
        McpClientOptions::new(),
    )
    .await;
    let start = std::time::Instant::now();
    let options = McpRequestOptions::new().with_timeout(Duration::from_millis(120));
    let result = client
        .call_tool_with_options("slow", &json!({ "text": "late" }), &options)
        .await;
    assert!(
        matches!(result, Err(McpError::Timeout { timeout_ms: 120 })),
        "per-request deadline: {result:?}"
    );
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "the request deadline, not the 20s client default, ended the call"
    );
    // The connection stays usable after a timed-out request.
    client.ping().await.expect("ping after timeout");
    client.close().await;
}

#[tokio::test]
async fn generic_request_and_notify_use_the_custom_method_path() {
    let client = connect_mock_with_options("raw", &[], McpClientOptions::new()).await;
    // A standard method sent through the generic surface returns its JSON
    // result (`ping` answers an empty object).
    let result = client
        .request("ping", None, &McpRequestOptions::new())
        .await
        .expect("generic ping");
    assert_eq!(result, json!({}));
    // A custom notification has no response; sending it must not fail.
    client
        .notify("notifications/custom", Some(json!({ "hello": "world" })))
        .await
        .expect("notify");
    client.close().await;
}

#[tokio::test]
async fn per_request_progress_reaches_the_callback_and_resets_the_idle_deadline() {
    let client = connect_mock_with_options(
        "progress",
        &[
            ("PIR_MOCK_PROGRESS_MS", "30"),
            ("PIR_MOCK_PROGRESS_COUNT", "5"),
        ],
        McpClientOptions::new(),
    )
    .await;
    let seen = Arc::new(Mutex::new(
        Vec::<(f64, Option<f64>, Option<String>, Value)>::new(),
    ));
    let progress_seen = Arc::clone(&seen);
    let options = McpRequestOptions::new()
        // Shorter than the 150ms total run: only progress resets keep this
        // request alive.
        .with_timeout(Duration::from_millis(80))
        .with_progress(move |progress| {
            progress_seen
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((
                    progress.progress,
                    progress.total,
                    progress.message,
                    progress.progress_token,
                ));
        });
    let result = client
        .call_tool_with_options("progress", &json!({}), &options)
        .await
        .expect("progress keeps the request alive");
    assert_eq!(tool_result_text(&result), "progress:5");
    let seen = seen
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    assert_eq!(seen.len(), 5, "one callback per notification: {seen:?}");
    assert_eq!(seen[0].0, 1.0);
    assert_eq!(seen[4].0, 5.0);
    assert_eq!(seen[4].1, Some(5.0));
    assert_eq!(seen[4].2.as_deref(), Some("step 5"));
    assert!(
        !seen[4].3.is_null(),
        "the progress token is delivered: {seen:?}"
    );
    client.close().await;
}

/// Regression: the per-request deadline must cover the DISPATCH, not only the
/// response wait. rmcp's peer channel is bounded (1024 slots); a server that
/// stops reading stdin after `initialize` lets the writer, the pipe and then
/// the channel fill. A request armed with a short deadline must still return
/// `McpError::Timeout` instead of parking inside `send_cancellable_request`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "blocked dispatch exhausts the test runtime; needs a dedicated dispatch-thread harness"]
async fn request_deadline_bounds_a_blocked_dispatch() {
    let (client, manager) = connect_mock_stdio_env(&[("PIR_MOCK_STALL_AFTER_INIT", "1")]).await;
    // More in-flight pings than the 1024-slot peer channel plus the stdin pipe
    // can hold, so the LAST dispatch below has nowhere to go.
    let mut inflight = Vec::new();
    for _ in 0..4096 {
        let client = Arc::clone(&client);
        inflight.push(tokio::spawn(async move {
            let _ = client
                .ping_with_options(&McpRequestOptions {
                    timeout: Some(Duration::from_secs(120)),
                    ..Default::default()
                })
                .await;
        }));
    }
    // Let the writer fill the child's stdin pipe and the rmcp peer channel.
    tokio::time::sleep(Duration::from_secs(2)).await;

    let bounded = tokio::time::timeout(
        Duration::from_secs(10),
        client
            .ping_with_options(&McpRequestOptions::new().with_timeout(Duration::from_millis(150))),
    )
    .await;
    let outcome = bounded.expect("the request deadline must also bound a blocked dispatch");
    assert!(
        matches!(outcome, Err(McpError::Timeout { .. })),
        "expected a typed timeout, got {outcome:?}"
    );

    for task in inflight {
        task.abort();
    }
    manager.shutdown().await;
}
