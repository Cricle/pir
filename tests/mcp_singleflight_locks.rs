//! L11 regressions for the MCP manager and log:
//! - a blocked single-flight connect (a silent stdio server) must not hold the
//!   connection slot across the handshake wait, so `disconnect` and another
//!   server's call proceed immediately;
//! - the bounded append log formats each line before taking its mutex, so a
//!   concurrent burst keeps one intact, correctly formatted line per entry.
#![cfg(feature = "mcp")]

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::json;

use pir::config::mcp::McpServerConfig;
use pir::mcp::log::McpLog;
use pir::mcp::manager::{ConnectionState, McpManager};
use pir::mcp::registry::{McpServerEntry, McpServerSource};

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-mcp-locks-{label}-{}-{}",
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

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The blocked MCP server child (this test binary re-invoked with `--exact`):
/// it holds the stdio pipe open and never answers `initialize`, so a connect
/// to it stays in flight until its configured handshake timeout — or until
/// the parent abandons the future.
#[test]
fn blocked_mcp_server_child() {
    if std::env::var("PIR_TEST_BLOCKED_MCP").as_deref() != Ok("1") {
        return;
    }
    let mut sink = String::new();
    let _ = std::io::stdin().read_to_string(&mut sink);
}

fn blocked_server_spawn() -> (String, Vec<String>) {
    let exe = std::env::current_exe().expect("current exe");
    (
        exe.to_string_lossy().into_owned(),
        vec![
            "blocked_mcp_server_child".to_owned(),
            "--exact".to_owned(),
            "--nocapture".to_owned(),
        ],
    )
}

fn stdio_config(
    command: String,
    args: Vec<String>,
    env: BTreeMap<String, String>,
) -> McpServerConfig {
    let mut value = json!({
        "command": command,
        "args": args,
        "timeout": 30,
    });
    value["env"] = json!(env);
    serde_json::from_value(value).expect("stdio config")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_blocked_connect_does_not_block_disconnect_or_another_server() {
    let manager = McpManager::without_log(&std::env::current_dir().expect("cwd"));
    let (command, args) = blocked_server_spawn();
    manager.registry().register(McpServerEntry {
        name: "blocked".to_owned(),
        config: stdio_config(
            command,
            args,
            BTreeMap::from([("PIR_TEST_BLOCKED_MCP".to_owned(), "1".to_owned())]),
        ),
        source: McpServerSource::Extension("test".to_owned()),
    });
    // A second server whose connect fails immediately (the command does not
    // exist): its call must not be caught behind the blocked server.
    manager.registry().register(McpServerEntry {
        name: "fast-failure".to_owned(),
        config: stdio_config(
            "pir-test-command-that-does-not-exist".to_owned(),
            Vec::new(),
            BTreeMap::new(),
        ),
        source: McpServerSource::Extension("test".to_owned()),
    });

    let connecting = {
        let manager = Arc::clone(&manager);
        tokio::spawn(async move { manager.get_client("blocked").await })
    };

    // Wait until the blocked connect owns the single-flight token.
    let mut states = manager.connection_states().await;
    let mut saw_connecting = false;
    for _ in 0..200 {
        states = manager.connection_states().await;
        if states.get("blocked") == Some(&ConnectionState::Connecting) {
            saw_connecting = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(saw_connecting, "connect reached Connecting: {states:?}");

    // Another server's call proceeds (its failure is immediate because the
    // command is missing, not because it waited for the blocked handshake).
    let fast = tokio::time::timeout(Duration::from_secs(2), manager.get_client("fast-failure"))
        .await
        .expect("another server's call must not wait for the blocked connect");
    assert!(fast.is_err(), "the missing command fails the connect");

    // Disconnect takes the client out under the slot lock and returns without
    // waiting for the 30s handshake: the old layout held the slot mutex across
    // the whole retry loop, so this call would block for the full timeout.
    let started = Instant::now();
    tokio::time::timeout(Duration::from_secs(2), manager.disconnect("blocked"))
        .await
        .expect("disconnect must not wait for the blocked connect");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "disconnect returned in {:?}",
        started.elapsed()
    );

    // The in-flight connect observed the detach; cancel it so the silent
    // child's 30s handshake budget is not spent in this test.
    connecting.abort();
    let _ = connecting.await;
    manager.shutdown().await;
}

#[test]
fn log_burst_keeps_one_intact_line_per_entry() {
    const WRITERS: usize = 4;
    const LINES_PER_WRITER: usize = 250;

    let dir = TempDir::new("burst");
    let log = Arc::new(McpLog::new(dir.path().join("mcp.log")));
    let writers: Vec<_> = (0..WRITERS)
        .map(|writer| {
            let log = Arc::clone(&log);
            std::thread::spawn(move || {
                for index in 0..LINES_PER_WRITER {
                    log.write_event(&format!("srv-{writer}"), &format!("line-{index:04}"));
                }
            })
        })
        .collect();
    for writer in writers {
        writer.join().expect("writer thread");
    }

    let text = std::fs::read_to_string(dir.path().join("mcp.log")).expect("read log");
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(
        lines.len(),
        WRITERS * LINES_PER_WRITER,
        "one line per entry (no interleaved or merged lines)"
    );

    let mut per_server: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for line in &lines {
        // Historical format: `YYYY-MM-DDTHH:MM:SSZ [server] message`.
        let (timestamp, rest) = line.split_once(' ').expect("timestamp separator");
        assert_eq!(timestamp.len(), 20, "timestamp shape: {line:?}");
        assert!(timestamp.ends_with('Z'), "timestamp shape: {line:?}");
        let rest = rest.strip_prefix('[').expect("server marker");
        let (server, message) = rest.split_once("] ").expect("server marker close");
        assert!(
            message.starts_with("line-"),
            "line carries its whole message: {line:?}"
        );
        assert_eq!(
            message.len(),
            "line-0000".len(),
            "no bytes lost around the line: {line:?}"
        );
        per_server
            .entry(server.to_owned())
            .or_default()
            .push(message.to_owned());
    }

    assert_eq!(per_server.len(), WRITERS);
    for (server, messages) in per_server {
        assert_eq!(
            messages.len(),
            LINES_PER_WRITER,
            "{server} wrote every line"
        );
        for (index, message) in messages.iter().enumerate() {
            assert_eq!(
                message,
                &format!("line-{index:04}"),
                "{server}'s lines keep their order"
            );
        }
    }
}
