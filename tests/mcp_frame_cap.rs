//! P0-1 / X2 regression tests for the MCP builtin:
//!
//! - the stdio transport enforces a hard per-frame byte cap: a 2 MiB line
//!   with no newline fails the handshake with `McpError::MessageTooLarge`,
//!   names the 1 MiB cap, kills and reaps the child, and leaves the manager
//!   usable for a second, healthy server (whose normal 1 KiB response still
//!   succeeds);
//! - a 100 MiB unterminated stream is stopped at the cap, not buffered: the
//!   child is killed after roughly 1 MiB of writes (progress file), so no
//!   reader ever grows toward 100 MiB;
//! - `McpPlugin::on_event` returns before any registry/connection work for
//!   events it does not handle (`input`, `tool_result`), while
//!   `session_start` still flushes queued changes and runs discovery.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use pir::config::mcp::McpServerConfig;
use pir::mcp::error::McpError;
use pir::mcp::manager::McpManager;
use pir::mcp::registry::{McpServerEntry, McpServerSource};
use pir::plugins::builtin::McpPlugin;
use pir::plugins::plugin::{AgentPlugin, PluginEvent, PluginEventOutcome};

/// The default stdio frame cap under test.
const ONE_MIB: usize = 1024 * 1024;

// ---------------------------------------------------------------------------
// Temp dirs
// ---------------------------------------------------------------------------

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-mcp-frame-{label}-{}-{}",
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

/// Command + args that re-invoke this test binary as the mock server child.
fn frame_mock_spawn() -> (String, Vec<String>) {
    let exe = std::env::current_exe().expect("current exe");
    (
        exe.to_string_lossy().into_owned(),
        vec![
            "mock_mcp_frame_child".to_owned(),
            "--exact".to_owned(),
            "--nocapture".to_owned(),
        ],
    )
}

/// Build one stdio server config pointing at the mock child.
fn frame_server_config(mode: &str, env: &[(&str, &Path)]) -> McpServerConfig {
    let (command, args) = frame_mock_spawn();
    let mut resolved = serde_json::Map::new();
    resolved.insert("PIR_MOCK_FRAME_MODE".to_owned(), json!(mode));
    for (key, value) in env {
        resolved.insert(
            (*key).to_owned(),
            json!(value.to_string_lossy().into_owned()),
        );
    }
    serde_json::from_value(json!({
        "command": command,
        "args": args,
        "env": resolved,
        "timeout": 20,
    }))
    .expect("mock server config")
}

fn register_server(manager: &McpManager, name: &str, config: McpServerConfig) {
    manager.registry().register(McpServerEntry {
        name: name.to_owned(),
        config,
        source: McpServerSource::Extension("frame-test".to_owned()),
    });
}

/// The child entry: behavior is selected by `PIR_MOCK_FRAME_MODE`. The test
/// harness prints its progress line without a trailing newline, so the child
/// starts with one explicit newline to keep the JSON-RPC stream line-clean.
#[test]
fn mock_mcp_frame_child() {
    let Ok(mode) = std::env::var("PIR_MOCK_FRAME_MODE") else {
        return;
    };
    {
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(b"\n");
        let _ = out.flush();
    }
    if let Ok(path) = std::env::var("PIR_MOCK_PID_FILE") {
        let _ = std::fs::write(path, std::process::id().to_string());
    }
    match mode.as_str() {
        "healthy" => serve_frame_mock(),
        "oversized" | "huge" => serve_frame_overflow(&mode),
        _ => {}
    }
}

/// A normal MCP server: initialize, one `echo` tool, ~1 KiB responses.
fn serve_frame_mock() {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(id) = value.get("id").cloned() else {
            continue; // notification
        };
        let method = value
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let response = match method {
            "initialize" => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": "2025-06-18",
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "frame-mock", "version": "1.0.0" },
                },
            }),
            "tools/list" => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "tools": [{
                        "name": "echo",
                        "description": "Echo the text argument.",
                        "inputSchema": { "type": "object" },
                    }],
                },
            }),
            "tools/call" => {
                let text = value
                    .get("params")
                    .and_then(|params| params.get("arguments"))
                    .and_then(|arguments| arguments.get("text"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "content": [{ "type": "text", "text": format!("echo:{text}") }],
                        "isError": false,
                    },
                })
            }
            _ => json!({ "jsonrpc": "2.0", "id": id, "result": {} }),
        };
        let mut out = stdout.lock();
        let _ = writeln!(out, "{response}");
        let _ = out.flush();
    }
}

/// Emit one unterminated frame of `PIR_MOCK_FRAME_MODE`'s size after reading
/// the initialize request. Never writes the terminating newline, so the
/// client must stop at its configured cap. `PIR_MOCK_PROGRESS_FILE` records
/// the bytes the child actually wrote (one line per MiB), which proves the
/// reader did not consume a 100 MiB payload.
fn serve_frame_overflow(mode: &str) {
    let mut initialize = String::new();
    let _ = std::io::stdin().lock().read_line(&mut initialize);
    let total: usize = if mode == "huge" {
        100 * 1024 * 1024
    } else {
        2 * 1024 * 1024
    };
    let progress_path = std::env::var("PIR_MOCK_PROGRESS_FILE").ok();
    let mut progress = progress_path
        .as_ref()
        .and_then(|path| std::fs::File::create(path).ok());
    let chunk = vec![b'x'; 64 * 1024];
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let mut written = 0usize;
    while written < total {
        let take = chunk.len().min(total - written);
        if out.write_all(&chunk[..take]).is_err() {
            return;
        }
        written += take;
        if written.is_multiple_of(1024 * 1024) {
            if let Some(file) = progress.as_mut() {
                let _ = writeln!(file, "{written}");
                let _ = file.flush();
            }
        }
    }
    let _ = out.flush();
    // Wait to be killed; a surviving child must fail the parent's reap check.
    std::thread::sleep(Duration::from_secs(120));
}

// ---------------------------------------------------------------------------
// Process helpers
// ---------------------------------------------------------------------------

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

fn read_pid_file(path: &Path) -> u32 {
    for _ in 0..100 {
        if let Ok(text) = std::fs::read_to_string(path) {
            if let Ok(pid) = text.trim().parse::<u32>() {
                return pid;
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("the mock child never wrote its pid to {}", path.display());
}

fn wait_for_exit(pid: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !process_alive(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    !process_alive(pid)
}

fn assert_frame_cap_error(error: &McpError, expected_limit: usize) {
    match error {
        McpError::MessageTooLarge { limit } => assert_eq!(*limit, expected_limit),
        other => panic!("expected MessageTooLarge, got {other:?}"),
    }
    let text = error.to_string();
    assert!(
        text.contains(&expected_limit.to_string()),
        "the error names the byte cap: {text}"
    );
}

// ---------------------------------------------------------------------------
// P0-1: oversized frames
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stdio_frame_cap_returns_bounded_error_and_reaps_child() {
    let dir = TempDir::new("oversized");
    let pid_file = dir.0.join("child.pid");
    let manager = McpManager::without_log(&dir.0);
    register_server(
        &manager,
        "oversized",
        frame_server_config("oversized", &[("PIR_MOCK_PID_FILE", &pid_file)]),
    );

    let started = Instant::now();
    let error = manager
        .get_client("oversized")
        .await
        .expect_err("a 2 MiB line must fail the handshake");
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the cap must fail within a bounded time, took {:?}",
        started.elapsed()
    );
    assert_frame_cap_error(&error, ONE_MIB);
    let text = error.to_string();
    assert!(
        text.contains("1 MiB"),
        "the message names the 1 MiB cap: {text}"
    );

    // The child is killed and reaped by the transport teardown, not left
    // writing the rest of the oversized frame.
    let pid = read_pid_file(&pid_file);
    assert!(
        wait_for_exit(pid, Duration::from_secs(5)),
        "the oversized server (pid {pid}) is still alive"
    );

    // A second, healthy server on the same manager still works, including a
    // normal ~1 KiB response.
    register_server(&manager, "healthy", frame_server_config("healthy", &[]));
    let client = manager
        .get_client("healthy")
        .await
        .expect("a healthy server still connects");
    let tools = client.list_all_tools().await.expect("tools/list");
    assert_eq!(tools.len(), 1, "the healthy server lists its tool");
    let text = "z".repeat(1024);
    let result = client
        .call_tool("echo", &json!({ "text": text }))
        .await
        .expect("a normal call succeeds");
    let reply = result
        .content
        .first()
        .and_then(|block| block.text.clone())
        .expect("text content");
    assert_eq!(reply, format!("echo:{text}"));
    manager.shutdown().await;
}

#[tokio::test]
async fn stdio_frame_cap_stops_a_hundred_mib_stream_at_the_cap() {
    let dir = TempDir::new("huge");
    let pid_file = dir.0.join("child.pid");
    let progress_file = dir.0.join("progress.txt");
    let manager = McpManager::without_log(&dir.0);
    register_server(
        &manager,
        "huge",
        frame_server_config(
            "huge",
            &[
                ("PIR_MOCK_PID_FILE", &pid_file),
                ("PIR_MOCK_PROGRESS_FILE", &progress_file),
            ],
        ),
    );

    let started = Instant::now();
    let error = manager
        .get_client("huge")
        .await
        .expect_err("a 100 MiB line must fail at the cap");
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the cap must fail within a bounded time, took {:?}",
        started.elapsed()
    );
    assert_frame_cap_error(&error, ONE_MIB);

    let pid = read_pid_file(&pid_file);
    assert!(
        wait_for_exit(pid, Duration::from_secs(5)),
        "the huge-frame server (pid {pid}) is still alive"
    );

    // The child counts the bytes it successfully wrote. The reader stopped
    // after ~1 MiB, so the pipe blocked and the child never approached the
    // 100 MiB it wanted to send: growth is incremental, not a declared-size
    // preallocation.
    let written: usize = std::fs::read_to_string(&progress_file)
        .ok()
        .and_then(|text| text.lines().last().and_then(|line| line.parse().ok()))
        .unwrap_or(0);
    assert!(
        written > 0,
        "the child must have started writing (progress file {:?})",
        progress_file
    );
    assert!(
        written < 16 * 1024 * 1024,
        "the reader consumed at most the cap plus the pipe buffer, child wrote {written} bytes"
    );
    manager.shutdown().await;
}

// ---------------------------------------------------------------------------
// X2: event dispatch is cheap for events the plugin does not handle
// ---------------------------------------------------------------------------

#[tokio::test]
async fn mcp_plugin_event_dispatch_skips_unrelated_events() {
    let dir = TempDir::new("events");
    let plugin = McpPlugin::with_cwd(&dir.0, &dir.0);
    let config: McpServerConfig =
        serde_json::from_value(json!({ "command": "pir-test-no-such-binary-xyz" }))
            .expect("config");
    plugin.manager().registry().register(McpServerEntry {
        name: "blocked".to_owned(),
        config,
        source: McpServerSource::Extension("event-test".to_owned()),
    });

    // `flush_server_changes` drains this queue (`McpManager::registry` is the
    // enqueue point); a still-queued change after a non-handled event proves
    // the handler returned before any registry work.
    let mut payload = json!({ "type": "input", "text": "hi" });
    let outcome = plugin
        .on_event(PluginEvent::new("input", &mut payload))
        .await;
    assert!(matches!(outcome, PluginEventOutcome::None));
    assert_eq!(
        plugin.manager().registry().drain_changes().len(),
        1,
        "input must not flush the registry change queue"
    );
    assert!(
        !plugin.manager().has_connection("blocked").await,
        "input must not reconcile or connect servers"
    );

    // Same for tool_result (the hot path on every result batch).
    plugin.manager().registry().register(McpServerEntry {
        name: "blocked".to_owned(),
        config: serde_json::from_value(json!({ "command": "pir-test-no-such-binary-xyz" }))
            .expect("config"),
        source: McpServerSource::Extension("event-test".to_owned()),
    });
    let mut payload = json!({ "type": "tool_result" });
    let outcome = plugin
        .on_event(PluginEvent::new("tool_result", &mut payload))
        .await;
    assert!(matches!(outcome, PluginEventOutcome::None));
    assert_eq!(
        plugin.manager().registry().drain_changes().len(),
        1,
        "tool_result must not flush the registry change queue"
    );
    assert!(!plugin.manager().has_connection("blocked").await);

    // session_start keeps the reconcile semantics: it runs discovery (the
    // spawn fails, but the connection object is observable). The queued
    // `mcp_servers_change` entries are drained by the host's refresh tail,
    // not inside this dispatch: a flush here could not reach the unified
    // native fan-out, so the host tail is the single emit point (covered by
    // tests/hook_coverage.rs).
    plugin.manager().registry().register(McpServerEntry {
        name: "blocked".to_owned(),
        config: serde_json::from_value(json!({ "command": "pir-test-no-such-binary-xyz" }))
            .expect("config"),
        source: McpServerSource::Extension("event-test".to_owned()),
    });
    let mut payload = json!({ "type": "session_start" });
    let outcome = plugin
        .on_event(PluginEvent::new("session_start", &mut payload))
        .await;
    assert!(matches!(outcome, PluginEventOutcome::None));
    assert!(
        plugin.manager().has_connection("blocked").await,
        "session_start runs discovery for the configured server"
    );
    // The change queue remains for the host refresh-tail flush.
    let _ = plugin.manager().registry().drain_changes();
    plugin.manager().shutdown().await;
}
