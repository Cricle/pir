//! Security-fix regression tests, kept separate from the `g_*` suites:
//! 1. the JSONL RPC stdin line limit (16 MiB) reports a `parse` error and
//!    disconnects instead of buffering newline-free bytes without bound;
//! 2. the bash spill temp file is created exclusively under an
//!    unpredictable `pir-shell-{pid}-{seq}-{uuid}` name and stays readable
//!    after the run (the `full_output_path` contract);
//! 3. a reconnect that fails to the top emits `Disconnected` exactly once.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

// ---------------------------------------------------------------------------
// 1. RPC stdin line limit
// ---------------------------------------------------------------------------

#[tokio::test]
async fn oversized_stdin_line_gets_parse_error_and_disconnect() {
    use pir::coding_agent::rpc::{RpcServer, SessionFactory};

    let (server_input, mut client_write) = tokio::io::duplex(64 * 1024);
    let (client_read, server_output) = tokio::io::duplex(64 * 1024);
    // The factory is never invoked: the oversized line fails before any
    // command dispatch, so a never-succeeding factory keeps the test free of
    // provider/session wiring.
    let factory: SessionFactory = Arc::new(|_| Err(anyhow::anyhow!("no session in this test")));
    let server = RpcServer::new(std::env::temp_dir(), factory);
    let serve_task = tokio::spawn(async move { server.serve(server_input, server_output).await });

    // One byte past the 16 MiB line limit, no newline: the server must
    // answer with the `parse` error response and end the session instead of
    // growing its read buffer without bound.
    let oversized = vec![b'a'; 16 * 1024 * 1024 + 1];
    // The server tears the input down once the limit trips, so the tail of
    // this write may hit a closed pipe; either way the excess arrived.
    let _ = client_write.write_all(&oversized).await;
    let _ = client_write.flush().await;
    drop(client_write);

    let mut reader = BufReader::new(client_read);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .await
        .expect("read the parse-error response line");
    let response: serde_json::Value =
        serde_json::from_str(line.trim_end()).expect("response line is JSON");
    assert_eq!(response["type"], "response", "got: {line}");
    assert_eq!(response["command"], "parse", "got: {line}");
    assert_eq!(response["success"], false, "got: {line}");
    assert!(
        response["error"]
            .as_str()
            .unwrap_or_default()
            .contains("parse"),
        "got: {line}"
    );

    // The session must be disconnected after the parse error: serve returned
    // and dropped the output half, so the next read is EOF.
    let mut byte = [0_u8; 1];
    let read = reader.read(&mut byte).await.expect("read to EOF");
    assert_eq!(read, 0, "session must disconnect after the oversized line");

    serve_task
        .await
        .expect("serve task join")
        .expect("serve ends cleanly");
}

// ---------------------------------------------------------------------------
// 2. bash spill temp file: exclusive, unpredictable, readable after the run
// ---------------------------------------------------------------------------

#[tokio::test]
async fn bash_spill_file_is_unpredictable_and_stays_readable() {
    use pir::agent::tool_execution::{ToolContext, ToolRegistry};
    use pir::coding_agent::tools::register_core_tools;

    // The shell tool needs a real shell; without one, skip (pass).
    if pir::coding_agent::tools::resolve_shell_config(None).is_err() {
        return;
    }

    let dir = std::env::temp_dir().join(format!("pir-security-fixes-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create test cwd");

    let mut registry = ToolRegistry::new();
    register_core_tools(&mut registry);
    let bash = registry.get("bash").expect("bash tool registered");

    // ~200 KB of output: the rolling buffer (keep = 2x50 KB) spills and the
    // 50 KB tail truncation attaches `fullOutputPath`.
    let command = "head -c 200000 /dev/zero | tr '\\0' x";
    let ctx = ToolContext::new(dir.clone());
    let mut names = Vec::new();
    for _ in 0..2 {
        let result = tokio::time::timeout(Duration::from_secs(30), async {
            bash.run(&serde_json::json!({ "command": command }), &ctx)
                .await
        })
        .await
        .expect("bash run must not hang");
        assert!(!result.is_error, "bash run failed: {}", result.content);
        let details = result.details.expect("truncated run carries details");
        let path = PathBuf::from(
            details["fullOutputPath"]
                .as_str()
                .expect("fullOutputPath string"),
        );

        // The path handed to the model must still be readable after the run
        // (deleting it would break the full_output_path contract).
        assert!(
            path.exists(),
            "spill file must survive the run: {}",
            path.display()
        );

        // Name shape: `pir-shell-{pid}-{seq}-{32 hex chars}` — an in-process
        // counter plus a fixed-length random suffix, not a timestamp.
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .expect("utf8 file name")
            .to_owned();
        let prefix = format!("pir-shell-{}-", std::process::id());
        assert!(name.starts_with(&prefix), "unexpected name: {name}");
        let mut parts = name[prefix.len()..].splitn(2, '-');
        let seq = parts.next().expect("seq component");
        assert!(
            !seq.is_empty() && seq.chars().all(|c| c.is_ascii_digit()),
            "expected an in-process counter in: {name}"
        );
        let random = parts.next().expect("random suffix");
        assert_eq!(random.len(), 32, "fixed-length random suffix: {name}");
        assert!(
            random.chars().all(|c| c.is_ascii_hexdigit()),
            "random suffix must be hex: {name}"
        );
        names.push(name);
    }
    assert_ne!(names[0], names[1], "concurrent runs must not share a name");

    // The test created these spill files; clean them up here (production
    // deliberately keeps them for the model's follow-up read).
    for name in &names {
        let _ = std::fs::remove_file(dir.join(name));
    }
    let _ = std::fs::remove_dir(&dir);
}

// ---------------------------------------------------------------------------
// 3. reconnect give-up emits Disconnected exactly once
// ---------------------------------------------------------------------------

#[tokio::test]
async fn reconnect_give_up_emits_disconnected_only_once() {
    use pir::client::{Client, ClientTimeouts, ConnectionState, ReconnectPolicy};
    use pir::protocol::{
        encode_server_envelope, FrameOptions, ServerEnvelope, ServerHelloV8, PROTOCOL_VERSION,
    };

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind scripted peer");
    let address = listener.local_addr().expect("peer address");
    // Scripted peer: complete exactly one hello handshake, then exit. When
    // the task ends, the listener is gone and reconnect's TCP connects are
    // refused, driving it to the give-up path.
    let peer = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let mut header = [0_u8; 4];
        stream.read_exact(&mut header).await.expect("hello header");
        let length = u32::from_be_bytes(header) as usize;
        let mut payload = vec![0_u8; length];
        stream
            .read_exact(&mut payload)
            .await
            .expect("hello payload");
        let reply = encode_server_envelope(
            &ServerEnvelope::Hello(ServerHelloV8 {
                type_: "hello".to_owned(),
                version: PROTOCOL_VERSION,
                server_id: uuid::Uuid::new_v4().to_string(),
            }),
            FrameOptions::default(),
        )
        .expect("encode hello reply");
        stream.write_all(&reply).await.expect("write hello reply");
        // Dropping the stream and the listener here is the point: the
        // reconnect below must fail to connect.
    });

    let timeouts = ClientTimeouts {
        connect: Duration::from_millis(500),
        request: Duration::from_millis(500),
        update: Duration::from_millis(500),
    };
    let mut client = Client::connect_with_token_and_timeouts(address, None, timeouts)
        .await
        .expect("hello handshake");

    let events: Arc<Mutex<Vec<pir::client::ConnectionStateChange>>> =
        Arc::new(Mutex::new(Vec::new()));
    {
        let sink = Arc::clone(&events);
        client.on_connection_state_change(move |change| {
            sink.lock().unwrap().push(change.clone());
        });
    }

    peer.await.expect("scripted peer task");

    let policy = ReconnectPolicy {
        max_attempts: 3,
        initial_backoff: Duration::ZERO,
        backoff_multiplier: 1,
        max_backoff: Duration::ZERO,
    };
    let error = client
        .reconnect_with_policy(policy)
        .await
        .expect_err("reconnect against a dead endpoint must fail");
    let _ = error;

    let events = events.lock().unwrap();
    let disconnects = events
        .iter()
        .filter(|change| change.state == ConnectionState::Disconnected)
        .count();
    assert_eq!(
        disconnects, 1,
        "expected exactly one Disconnected event, got {events:?}"
    );
    assert_eq!(events[0].state, ConnectionState::Disconnected);
}
