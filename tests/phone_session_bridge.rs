//! Hermetic tests for the native pi-phone session bridge and pool.
//!
//! The pool tests re-enter the test binary as a mock `pir --mode rpc` child:
//! the `mock_rpc_child_entry` test below checks `PIR_PHONE_TEST_CHILD` and,
//! when set, speaks JSON lines on stdin/stdout. No real model, server, or
//! network is involved.
//!
//! Platform limit: the pool kills with `SIGKILL` on Unix and
//! `TerminateProcess` on Windows, neither of which a child can ignore, so a
//! portable "ignores the soft signal" mock is impossible. The escalation path
//! in `SessionHandle::shutdown` (abort the wedged supervisor after the kill
//! grace and emit `agent-exit`) is therefore covered structurally by the
//! existing stop tests rather than by an unkillable child.

#![cfg(feature = "phone")]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

use pir::plugins::builtin::phone::session::{
    BridgeConfig, BridgeSubscriber, CloseReason, ForwardedCommand, HostCommands, HostFuture,
    HostReply, PhoneCommand, PoolConfig, PoolError, SessionBridge, SessionPool,
};

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
        let data = match &command.command {
            PhoneCommand::Prompt { .. } => json!({ "accepted": true }),
            PhoneCommand::Abort => Value::Null,
            PhoneCommand::SetModel { .. } => json!({ "model": "mock" }),
            PhoneCommand::Compact { .. } => json!({ "started": true }),
            PhoneCommand::PhoneGetTree => json!({ "nodes": [] }),
            _ => Value::Null,
        };
        Box::pin(async move {
            if data.is_null() {
                Ok(HostReply::default())
            } else {
                Ok(HostReply::data(data))
            }
        })
    }
}

async fn recv_json(subscriber: &mut BridgeSubscriber) -> Value {
    let line = tokio::time::timeout(Duration::from_secs(2), subscriber.recv())
        .await
        .expect("timed out waiting for a bridge line")
        .expect("subscriber was closed");
    serde_json::from_str(&line).expect("bridge line is not JSON")
}

fn unique_log_path(tag: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "pir-phone-{tag}-{}-{nanos}.log",
        std::process::id()
    ))
}

fn spawn_count(path: &std::path::Path) -> usize {
    std::fs::read_to_string(path)
        .map(|text| text.lines().filter(|line| !line.is_empty()).count())
        .unwrap_or(0)
}

async fn wait_until(mut predicate: impl FnMut() -> bool, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if predicate() {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn bridge_fans_out_and_drops_a_slow_subscriber() {
    let host: Arc<dyn HostCommands> = Arc::new(RecordingHost::default());
    let (bridge, host_tx) = SessionBridge::start(
        BridgeConfig {
            max_clients: 2,
            client_queue_capacity: 4,
            ..BridgeConfig::default()
        },
        host,
    );

    let mut fast = bridge.subscribe_with_capacity(16).expect("fast client");
    let mut slow = bridge.subscribe_with_capacity(1).expect("slow client");

    for _ in 0..8 {
        host_tx
            .send(json!({ "type": "agent_start" }))
            .await
            .expect("host feed");
    }

    for _ in 0..8 {
        let line = recv_json(&mut fast).await;
        assert_eq!(line["channel"], "rpc");
        assert_eq!(line["payload"]["type"], "agent_start");
    }

    // The slow client had room for exactly one line; the next one dropped it.
    assert!(slow.try_recv().is_some(), "first line was buffered");
    let closed = tokio::time::timeout(Duration::from_secs(2), slow.recv())
        .await
        .expect("slow client should be closed");
    assert!(closed.is_none());
    assert_eq!(slow.close_reason(), Some(CloseReason::SlowConsumer));
    assert_eq!(bridge.client_count(), 1);
    assert_eq!(bridge.stats().slow_clients_dropped, 1);
}

#[tokio::test]
async fn bridge_forwards_commands_and_replies_only_to_the_origin() {
    let host = Arc::new(RecordingHost::default());
    let (bridge, _host_tx) = SessionBridge::start(BridgeConfig::default(), host.clone());
    let mut client = bridge.subscribe().expect("client");
    let mut other = bridge.subscribe().expect("other client");

    let lines = [
        json!({
            "kind": "rpc",
            "command": {
                "type": "prompt",
                "message": "hello",
                "images": [],
                "streamingBehavior": "steer",
            },
        }),
        json!({ "kind": "rpc", "command": { "type": "abort" } }),
        json!({
            "kind": "rpc",
            "command": { "type": "set_model", "provider": "mock", "modelId": "mock-1" },
        }),
        json!({ "kind": "rpc", "command": { "type": "compact" } }),
        json!({ "kind": "rpc", "command": { "type": "phone_get_tree" } }),
    ];

    for line in &lines {
        assert!(
            bridge
                .handle_client_line(client.id(), &line.to_string())
                .await,
            "line was not accepted"
        );
    }

    for expected in ["prompt", "abort", "set_model", "compact", "phone_get_tree"] {
        let envelope = recv_json(&mut client).await;
        assert_eq!(envelope["channel"], "rpc");
        assert_eq!(envelope["payload"]["type"], "response");
        assert_eq!(envelope["payload"]["command"], expected);
        assert_eq!(envelope["payload"]["success"], true);
    }

    let commands = host.commands();
    assert!(matches!(
        &commands[0],
        PhoneCommand::Prompt {
            message,
            streaming_behavior: Some(behavior),
            ..
        } if message == "hello" && behavior == "steer"
    ));
    assert_eq!(commands[1], PhoneCommand::Abort);
    assert!(matches!(
        &commands[2],
        PhoneCommand::SetModel { provider, model_id }
            if provider == "mock" && model_id == "mock-1"
    ));
    assert!(matches!(&commands[3], PhoneCommand::Compact { .. }));
    assert_eq!(commands[4], PhoneCommand::PhoneGetTree);

    // Responses are not broadcast.
    assert!(other.try_recv().is_none());

    // Invalid JSON and unsupported shapes become client-error frames.
    assert!(bridge.handle_client_line(client.id(), "{not json").await);
    let error = recv_json(&mut client).await;
    assert_eq!(error["channel"], "server");
    assert_eq!(error["event"], "client-error");
    assert!(error["data"]["message"].as_str().unwrap().contains("JSON"));
}

#[tokio::test]
async fn bridge_windows_snapshots_and_trims_tool_text() {
    let host: Arc<dyn HostCommands> = Arc::new(RecordingHost::default());
    let (bridge, host_tx) = SessionBridge::start(BridgeConfig::default(), host);
    let mut client = bridge.subscribe().expect("client");

    let messages: Vec<Value> = (0..65)
        .map(|index| json!({ "role": "user", "content": index.to_string() }))
        .collect();
    let snapshot = json!({
        "type": "snapshot",
        "state": { "isStreaming": false },
        "messages": messages,
        "commands": [{ "name": "mock-command" }],
        "liveAssistantMessage": Value::Null,
        "liveTools": [],
    });

    host_tx.send(snapshot.clone()).await.expect("snapshot");
    let first = recv_json(&mut client).await;
    assert_eq!(first["channel"], "snapshot");
    assert_eq!(first["sessionWorkerId"], "parent-session");
    assert_eq!(first["messages"].as_array().unwrap().len(), 60);
    assert_eq!(first["messages"][0]["content"], "5");
    assert_eq!(first["historyTruncated"], true);
    assert_eq!(first["totalMessageCount"], 65);
    assert_eq!(first["commands"][0]["name"], "mock-command");

    // An unchanged command catalogue is omitted from the next snapshot.
    host_tx.send(snapshot).await.expect("snapshot");
    let second = recv_json(&mut client).await;
    assert_eq!(second["messages"].as_array().unwrap().len(), 60);
    assert!(second.get("commands").is_none());

    let big_text = "x".repeat(40_000);
    host_tx
        .send(json!({
            "type": "tool_execution_end",
            "sessionWorkerId": "active-session-1",
            "toolCallId": "tool-1",
            "toolName": "read",
            "result": { "content": [{ "type": "text", "text": big_text }] },
            "isError": false,
        }))
        .await
        .expect("tool event");

    let tool = recv_json(&mut client).await;
    assert_eq!(tool["channel"], "rpc");
    assert_eq!(tool["sessionWorkerId"], "active-session-1");
    let text = tool["payload"]["result"]["content"][0]["text"]
        .as_str()
        .unwrap();
    assert!(text.contains("truncated"));
    assert!(text.chars().count() <= BridgeConfig::default().tool_text_cap_chars + 80);
}

#[tokio::test]
async fn bridge_bounds_an_update_flood_by_bytes_and_keeps_the_terminal_event() {
    let host: Arc<dyn HostCommands> = Arc::new(RecordingHost::default());
    let (bridge, host_tx) = SessionBridge::start(
        BridgeConfig {
            client_queue_capacity: 64,
            client_queue_bytes: 64 * 1024,
            total_queue_bytes: 256 * 1024,
            ..BridgeConfig::default()
        },
        host,
    );
    let mut client = bridge.subscribe().expect("client");

    for _ in 0..10_000 {
        host_tx
            .send(json!({
                "type": "message_update",
                "message": { "role": "assistant", "timestamp": 1 },
                "assistantMessageEvent": { "type": "text_delta", "delta": "x" },
            }))
            .await
            .expect("update event");
    }
    host_tx
        .send(json!({
            "type": "message_end",
            "message": { "role": "assistant", "content": "done" },
        }))
        .await
        .expect("terminal event");

    let mut updates = 0_usize;
    let mut saw_end = false;
    while let Ok(Some(line)) = tokio::time::timeout(Duration::from_millis(500), client.recv()).await
    {
        let value: Value = serde_json::from_str(&line).expect("json");
        match value["payload"]["type"].as_str() {
            Some("message_update") => updates += 1,
            Some("message_end") => {
                saw_end = true;
                break;
            }
            _ => {}
        }
    }

    assert!(saw_end, "message_end must survive an update flood");
    assert!(updates > 0, "at least one update was delivered");
    assert!(
        updates < 10_000,
        "updates were not coalesced ({updates} queued)"
    );
    let stats = bridge.stats();
    assert!(
        stats.peak_queued_bytes <= 64 * 1024,
        "peak queued bytes {} exceeded the client budget",
        stats.peak_queued_bytes
    );
    assert_eq!(stats.queued_bytes, 0, "queue drained");
    assert!(
        stats.coalesced_updates > 0,
        "the flood should have coalesced updates"
    );
}

#[tokio::test]
async fn bridge_evicts_updates_but_keeps_every_terminal_event() {
    let host: Arc<dyn HostCommands> = Arc::new(RecordingHost::default());
    let (bridge, host_tx) = SessionBridge::start(
        BridgeConfig {
            client_queue_capacity: 4,
            client_queue_bytes: 64 * 1024,
            ..BridgeConfig::default()
        },
        host,
    );
    let mut client = bridge.subscribe().expect("client");

    // Fill the queue exactly with coalescible updates.
    for _ in 0..4 {
        host_tx
            .send(json!({ "type": "message_update" }))
            .await
            .expect("update");
    }
    let terminals = ["message_end", "agent_end", "response", "tool_execution_end"];
    for terminal in terminals {
        host_tx
            .send(json!({
                "type": terminal,
                "message": { "role": "assistant" },
            }))
            .await
            .expect("terminal");
    }

    let mut seen = Vec::new();
    for _ in 0..4 {
        seen.push(recv_json(&mut client).await["payload"]["type"].to_string());
    }
    for terminal in terminals {
        assert!(
            seen.iter().any(|seen| seen.trim_matches('"') == terminal),
            "terminal event {terminal} was dropped; saw {seen:?}"
        );
    }
}

#[tokio::test]
async fn bridge_closes_a_slow_consumer_beyond_its_byte_budget() {
    let host: Arc<dyn HostCommands> = Arc::new(RecordingHost::default());
    let (bridge, host_tx) = SessionBridge::start(
        BridgeConfig {
            client_queue_capacity: 64,
            client_queue_bytes: 8 * 1024,
            total_queue_bytes: 1024 * 1024,
            ..BridgeConfig::default()
        },
        host,
    );
    let mut slow = bridge.subscribe().expect("slow client");
    let mut fast = bridge.subscribe().expect("fast client");

    let fast_task = tokio::spawn(async move {
        let mut received = 0_usize;
        while received < 16 {
            match tokio::time::timeout(Duration::from_secs(2), fast.recv()).await {
                Ok(Some(_)) => received += 1,
                _ => break,
            }
        }
        // Return the receiver too: it must stay subscribed until the
        // client-count assertion below (dropping it here would unsubscribe
        // before the slow-consumer close is observed).
        (received, fast)
    });

    let pad = "y".repeat(4096);
    for _ in 0..16 {
        host_tx
            .send(json!({ "type": "agent_start", "pad": pad }))
            .await
            .expect("large frame");
        // Give the fast reader a chance to drain between frames; the slow
        // reader never reads.
        tokio::time::sleep(Duration::from_millis(2)).await;
    }

    let (received, _fast) = fast_task.await.expect("fast task");
    assert_eq!(received, 16);
    assert!(
        slow.try_recv().is_some(),
        "the slow client buffered at least one frame"
    );
    let closed = tokio::time::timeout(Duration::from_secs(2), slow.recv())
        .await
        .expect("slow client should be closed");
    assert!(closed.is_none());
    assert_eq!(slow.close_reason(), Some(CloseReason::SlowConsumer));
    assert_eq!(bridge.client_count(), 1, "only the fast client remains");
}

fn mock_pool_config() -> PoolConfig {
    let mut config = PoolConfig::new(std::env::current_dir().expect("cwd"));
    config.executable = std::env::current_exe().expect("test executable");
    config.args = vec![
        "--exact".to_owned(),
        "mock_rpc_child_entry".to_owned(),
        "--nocapture".to_owned(),
    ];
    config
        .env
        .push(("PIR_PHONE_TEST_CHILD".to_owned(), "1".to_owned()));
    config.auto_restart = false;
    config.max_sessions = 1;
    config.request_timeout = Duration::from_secs(10);
    config.start_timeout = Duration::from_secs(10);
    config.kill_grace = Duration::from_secs(2);
    config
}

#[tokio::test]
async fn pool_spawns_correlates_requests_then_stops() {
    let (pool, mut events) = SessionPool::new(mock_pool_config());
    let session_id = pool.spawn(None).expect("spawn");
    assert!(pool.is_running(&session_id));
    assert_eq!(pool.list().len(), 1);
    assert!(pool.list()[0].child_pid.is_some());

    let response = pool
        .request(
            &session_id,
            json!({ "type": "get_state" }),
            Duration::from_secs(10),
        )
        .await
        .expect("get_state response");
    assert_eq!(response["success"], true);
    assert_eq!(response["data"]["sessionId"], "mock-session");

    let summary = pool.session(&session_id).expect("summary");
    assert_eq!(summary.kind, "parallel");
    assert_eq!(summary.session_id.as_deref(), Some("mock-session"));
    assert_eq!(summary.session_name.as_deref(), Some("Mock"));
    assert_eq!(summary.message_count, 2);

    // The mock child's live event reaches the pool consumer as an envelope.
    let mut saw_agent_start = false;
    for _ in 0..8 {
        match tokio::time::timeout(Duration::from_secs(2), events.recv()).await {
            Ok(Some(event)) => {
                if event.event["payload"]["type"] == "agent_start" {
                    saw_agent_start = true;
                    break;
                }
            }
            _ => break,
        }
    }
    assert!(saw_agent_start, "agent_start event was not forwarded");

    // The child-count cap is reported, not silently exceeded.
    assert!(pool.spawn(None).is_err());

    pool.stop(&session_id).await.expect("stop");
    assert!(pool.list().is_empty());
    assert!(!pool.is_running(&session_id));
    assert!(pool
        .request(
            &session_id,
            json!({ "type": "get_state" }),
            Duration::from_secs(1)
        )
        .await
        .is_err());
}

#[tokio::test]
async fn pool_stop_kills_a_child_that_never_answered() {
    let (pool, _events) = SessionPool::new(mock_pool_config());
    let session_id = pool.spawn(None).expect("spawn");
    // Let the mock child reach its stdin read loop, then kill it while idle.
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(pool.is_running(&session_id));

    pool.stop(&session_id).await.expect("stop");
    assert!(!pool.is_running(&session_id));
    assert!(pool.session(&session_id).is_none());
}

#[tokio::test]
async fn pool_cap_is_honest_and_slots_are_released_by_stop() {
    let mut config = mock_pool_config();
    config.max_sessions = 2;
    config.auto_restart = false;
    let (pool, _events) = SessionPool::new(config);

    let first = pool.spawn(None).expect("first session");
    let second = pool.spawn(None).expect("second session");
    assert_eq!(pool.session_count(), 2);

    let third = pool.spawn(None);
    assert!(
        matches!(third, Err(PoolError::TooManySessions { max: 2 })),
        "cap must be reported honestly, got {third:?}"
    );
    // The rejected spawn must not leak a reservation or a catalog row.
    assert_eq!(pool.session_count(), 2);
    assert_eq!(pool.list().len(), 2);

    pool.stop(&first).await.expect("stop first");
    assert_eq!(pool.session_count(), 1);
    let replacement = pool.spawn(None).expect("slot released by stop");
    assert_eq!(pool.session_count(), 2);

    pool.stop(&second).await.expect("stop second");
    pool.stop(&replacement).await.expect("stop replacement");
    assert_eq!(pool.session_count(), 0);
    assert!(pool.list().is_empty());
}

#[tokio::test]
async fn pool_restarts_an_abnormal_exit_then_stays_down_at_the_budget() {
    let log = unique_log_path("restart-budget");
    let mut config = mock_pool_config();
    config.auto_restart = true;
    config.max_restarts = 1;
    config.restart_delay = Duration::from_millis(25);
    config.restart_backoff_max = Duration::from_millis(25);
    config
        .env
        .push(("PIR_PHONE_TEST_CHILD_MODE".to_owned(), "crash".to_owned()));
    config.env.push((
        "PIR_PHONE_TEST_CHILD_LOG".to_owned(),
        log.display().to_string(),
    ));
    let (pool, mut events) = SessionPool::new(config);

    let session_id = pool.spawn(None).expect("spawn");
    // Exactly one automatic restart after the first abnormal exit.
    assert!(
        wait_until(|| spawn_count(&log) == 2, Duration::from_secs(15)).await,
        "expected 2 spawns, saw {}",
        spawn_count(&log)
    );
    // Exhausting the budget removes the row and releases the slot.
    assert!(
        wait_until(|| pool.session_count() == 0, Duration::from_secs(5)).await,
        "slot was not released after the restart budget"
    );
    assert!(pool.list().is_empty(), "dead rows must not linger");
    assert!(!pool.is_running(&session_id));

    let mut exits = 0;
    let mut exhausted = false;
    while let Ok(Some(event)) =
        tokio::time::timeout(Duration::from_millis(250), events.recv()).await
    {
        if event.event["event"] == "agent-exit" {
            exits += 1;
            if event.event["data"]["message"]
                .as_str()
                .unwrap_or("")
                .contains("restart budget exhausted")
            {
                exhausted = true;
            }
        }
    }
    assert_eq!(exits, 2, "one agent-exit per crash");
    assert!(
        exhausted,
        "the final agent-exit must report the exhausted budget"
    );

    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(spawn_count(&log), 2, "no spawn after the budget");

    // The slot is usable again (and shutdown reaps the new crash loop).
    let _replacement = pool.spawn(None).expect("slot released after budget");
    pool.shutdown().await;
    assert_eq!(pool.session_count(), 0);
}

#[tokio::test]
async fn pool_stop_during_restart_backoff_cancels_the_restart() {
    let log = unique_log_path("restart-cancel");
    let mut config = mock_pool_config();
    config.auto_restart = true;
    config.max_restarts = 5;
    config.restart_delay = Duration::from_secs(5);
    config.restart_backoff_max = Duration::from_secs(5);
    config
        .env
        .push(("PIR_PHONE_TEST_CHILD_MODE".to_owned(), "crash".to_owned()));
    config.env.push((
        "PIR_PHONE_TEST_CHILD_LOG".to_owned(),
        log.display().to_string(),
    ));
    let (pool, _events) = SessionPool::new(config);

    let session_id = pool.spawn(None).expect("spawn");
    assert!(
        wait_until(|| spawn_count(&log) == 1, Duration::from_secs(5)).await,
        "the crash child never started"
    );
    // Let the child exit and the supervisor enter its 5 s backoff.
    tokio::time::sleep(Duration::from_millis(250)).await;

    let started = std::time::Instant::now();
    pool.stop(&session_id)
        .await
        .expect("stop during the backoff");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "stop waited for the restart backoff to elapse"
    );

    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(spawn_count(&log), 1, "the session was restarted after stop");
    assert_eq!(pool.session_count(), 0);
    assert!(pool.list().is_empty());
}

/// When re-entered by the pool with `PIR_PHONE_TEST_CHILD=1`, act as a
/// `pir --mode rpc` child. In the parent test process the variable is unset
/// and this is a no-op test.
#[test]
fn mock_rpc_child_entry() {
    if std::env::var("PIR_PHONE_TEST_CHILD").as_deref() != Ok("1") {
        return;
    }

    use std::io::{BufRead, Write};

    // Optional spawn log: one pid per child start, so tests can prove how many
    // times the pool actually spawned (restart budget / backoff cancel).
    if let Ok(path) = std::env::var("PIR_PHONE_TEST_CHILD_LOG") {
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = writeln!(file, "{}", std::process::id());
            let _ = file.flush();
        }
    }
    // Simulate a child that always exits abnormally (bad session file,
    // missing model, ...) to exercise the restart budget.
    if std::env::var("PIR_PHONE_TEST_CHILD_MODE").as_deref() == Ok("crash") {
        std::process::exit(7);
    }

    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();

    for line in stdin.lock().lines() {
        let Ok(line) = line else {
            break;
        };
        let Ok(request) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let request_id = request.get("id").cloned().unwrap_or(Value::Null);
        let command = request
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_owned();

        let data = match command.as_str() {
            "get_state" => json!({
                "sessionId": "mock-session",
                "sessionName": "Mock",
                "sessionFile": Value::Null,
                "model": { "id": "mock", "name": "Mock", "provider": "mock" },
                "isStreaming": false,
                "isCompacting": false,
                "messageCount": 2,
                "pendingMessageCount": 0,
            }),
            "get_messages" => json!({
                "messages": [
                    { "role": "user", "content": "hello" },
                    { "role": "assistant", "content": "hi" }
                ]
            }),
            "get_commands" => json!({ "commands": [{ "name": "mock" }] }),
            _ => json!({}),
        };

        if command == "get_state" {
            let _ = writeln!(stdout, "{}", json!({ "type": "agent_start" }));
        }
        let response = json!({
            "type": "response",
            "id": request_id,
            "command": command,
            "success": true,
            "data": data,
        });
        let _ = writeln!(stdout, "{response}");
        let _ = stdout.flush();
    }
}
