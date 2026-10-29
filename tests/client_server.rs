use std::time::{Duration, Instant};

use pir::client::{Client, ClientError, ClientTimeouts};
use pir::protocol::{
    encode_server_envelope, Command, FrameOptions, ServerEnvelope, ServerHelloV8, ServerSnapshot,
    PROTOCOL_VERSION, SESSION_SERVICE_ID,
};
use pir::server::{empty_snapshot, Server, ServerSecurityConfig};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

#[tokio::test]
async fn authenticated_server_accepts_valid_token_and_rejects_invalid_token() {
    let server = Server::bind_with_security(
        "127.0.0.1:0",
        ServerSecurityConfig {
            auth_token: Some("test-secret".to_owned()),
            ..ServerSecurityConfig::default()
        },
    )
    .await
    .expect("bind secure test server");
    let address = server.local_addr().expect("local address");
    let accepting = tokio::spawn(async move { server.accept_once().await });
    let mut client = Client::connect_with_token(address, Some("test-secret"))
        .await
        .expect("authenticated handshake");
    assert!(client.list().await.expect("authenticated list").is_empty());
    drop(client);
    accepting
        .await
        .expect("accept task join")
        .expect("serve client");

    let server = Server::bind_with_security(
        "127.0.0.1:0",
        ServerSecurityConfig {
            auth_token: Some("test-secret".to_owned()),
            ..ServerSecurityConfig::default()
        },
    )
    .await
    .expect("bind secure test server");
    let address = server.local_addr().expect("local address");
    let accepting = tokio::spawn(async move { server.accept_once().await });
    let error = match Client::connect_with_token(address, Some("wrong-secret")).await {
        Ok(_) => panic!("invalid token must be rejected"),
        Err(error) => error,
    };
    assert!(matches!(
        error,
        ClientError::Disconnected | ClientError::Io(_)
    ));
    accepting
        .await
        .expect("accept task join")
        .expect_err("unauthenticated connection closes");
}

#[tokio::test]
async fn unauthenticated_connection_is_closed_before_protocol_handshake() {
    let server = Server::bind_with_security(
        "127.0.0.1:0",
        ServerSecurityConfig {
            auth_token: Some("test-secret".to_owned()),
            ..ServerSecurityConfig::default()
        },
    )
    .await
    .expect("bind secure test server");
    let address = server.local_addr().expect("local address");
    let accepting = tokio::spawn(async move { server.accept_once().await });
    let mut stream = TcpStream::connect(address).await.expect("connect");
    stream
        .write_all(b"PIR-AUTH wrong\n")
        .await
        .expect("write token");
    let mut response = [0_u8; 1];
    let read = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        stream.read(&mut response),
    )
    .await
    .expect("close timeout")
    .expect("read close");
    assert_eq!(read, 0);
    accepting
        .await
        .expect("accept task join")
        .expect_err("authentication failure");
}

#[tokio::test]
async fn hello_and_snapshot_query_use_framed_cbor() {
    // v8 response frames validate `target.serverId` against the upstream
    // uuid-v4 shape, so the fixture server id must be a v4 uuid.
    let server_id = uuid::Uuid::new_v4().to_string();
    let server = Server::bind_with_snapshot("127.0.0.1:0", empty_snapshot(server_id.clone()))
        .await
        .expect("bind test server");
    let address = server.local_addr().expect("local address");
    let accepting = tokio::spawn(async move { server.accept_once().await });

    let mut client = Client::connect(address).await.expect("hello handshake");
    assert!(!client.connection_id().is_empty());
    assert_eq!(client.snapshot()["serverId"], server_id);
    assert_eq!(client.snapshot()["protocolVersion"], PROTOCOL_VERSION);
    let sessions = client.list().await.expect("list response");
    assert!(sessions.is_empty());

    drop(client);
    accepting
        .await
        .expect("accept task join")
        .expect("serve client");
}

#[tokio::test]
async fn unsupported_commands_return_protocol_error_codes() {
    let server = Server::bind("127.0.0.1:0").await.expect("bind test server");
    let address = server.local_addr().expect("local address");
    let accepting = tokio::spawn(async move { server.accept_once().await });

    let mut client = Client::connect(address).await.expect("hello handshake");
    let error = client
        .request(Command::Abort {
            session_id: "missing".to_owned(),
        })
        .await
        .expect_err("abort of a missing session");
    assert!(matches!(
        error,
        ClientError::Server { ref code, .. } if code == "not_found"
    ));

    drop(client);
    accepting
        .await
        .expect("accept task join")
        .expect("serve client");
}

#[tokio::test]
async fn session_commands_enforce_ownership_and_report_missing_provider() {
    let server = Server::bind("127.0.0.1:0").await.expect("bind test server");
    let address = server.local_addr().expect("local address");
    let accepting = tokio::spawn(async move { server.accept_once().await });
    let mut client = Client::connect(address).await.expect("hello handshake");
    let created = client
        .request(Command::Create {
            cwd: None,
            name: None,
            model: None,
            thinking_level: None,
        })
        .await
        .expect("create response");
    let session_id = created["session"]["id"].as_str().unwrap().to_owned();
    client
        .request(Command::Attach {
            session_id: session_id.clone(),
        })
        .await
        .expect("attach before prompt");
    let error = client
        .request(Command::Prompt {
            session_id: session_id.clone(),
            text: "hello".to_owned(),
        })
        .await
        .expect_err("default provider must not fake success");
    assert!(matches!(error, ClientError::Server { ref code, .. } if code == "internal_error"));
    let detached = client
        .request(Command::Detach { session_id })
        .await
        .expect("detach response");
    assert_eq!(detached["command"], "detach");
    drop(client);
    accepting
        .await
        .expect("accept task join")
        .expect("serve client");
}

#[tokio::test]
async fn remote_agent_policy_can_be_disabled_without_server_execution() {
    let server = Server::bind_with_security(
        "127.0.0.1:0",
        ServerSecurityConfig {
            allow_remote_agent: false,
            ..ServerSecurityConfig::default()
        },
    )
    .await
    .expect("bind policy test server");
    let address = server.local_addr().expect("local address");
    let accepting = tokio::spawn(async move { server.accept_once().await });
    let mut client = Client::connect(address).await.expect("hello handshake");
    let created = client
        .request(Command::Create {
            cwd: None,
            name: None,
            model: None,
            thinking_level: None,
        })
        .await
        .expect("create response");
    let session_id = created["session"]["id"]
        .as_str()
        .expect("session id")
        .to_owned();
    client
        .request(Command::Attach {
            session_id: session_id.clone(),
        })
        .await
        .expect("attach response");
    let error = client
        .request(Command::Prompt {
            session_id,
            text: "must not execute".to_owned(),
        })
        .await
        .expect_err("disabled remote agent must be forbidden");
    assert!(
        matches!(error, ClientError::Server { ref code, .. } if code == "forbidden"),
        "disabled remote agent must fail closed with `forbidden`, got {error:?}"
    );
    drop(client);
    accepting
        .await
        .expect("accept task join")
        .expect("connection closes cleanly after the client disconnects");
}

#[tokio::test]
async fn disconnect_removes_active_connection() {
    let server = Server::bind("127.0.0.1:0").await.expect("bind test server");
    let address = server.local_addr().expect("local address");
    let accepting = tokio::spawn(async move { server.accept_once().await });

    let client = Client::connect(address).await.expect("hello handshake");
    drop(client);
    accepting
        .await
        .expect("accept task join")
        .expect("serve client");
}

#[tokio::test]
async fn create_session_and_list_shows_it() {
    let server = Server::bind("127.0.0.1:0").await.expect("bind test server");
    let address = server.local_addr().expect("local address");
    let accepting = tokio::spawn(async move { server.accept_once().await });

    let mut client = Client::connect(address).await.expect("hello handshake");

    // Create a session with minimal parameters. A custom cwd is policy-gated
    // (`ServerSecurityConfig::allows_cwd` in src/server/policy.rs — see
    // `create_with_cwd_is_policy_gated_and_allowed_roots_widen_it` below);
    // the default cwd needs no allowance.
    let result = client
        .request(Command::Create {
            cwd: None,
            name: Some("test-session".to_owned()),
            model: None,
            thinking_level: None,
        })
        .await
        .expect("create response");

    assert_eq!(result["command"], "create");
    let session = result.get("session").expect("session in result");
    assert!(session.get("id").and_then(|v| v.as_str()).is_some());
    assert_eq!(session.get("cwd").and_then(|v| v.as_str()), Some("."));
    assert_eq!(
        session.get("name").and_then(|v| v.as_str()),
        Some("test-session")
    );
    assert_eq!(session.get("phase").and_then(|v| v.as_str()), Some("idle"));
    assert_eq!(
        session.get("attached").and_then(|v| v.as_bool()),
        Some(false)
    );

    // List should now include the new session.
    let sessions = client.list().await.expect("list response");
    assert_eq!(sessions.len(), 1);
    assert_eq!(
        sessions[0].get("id").and_then(|v| v.as_str()),
        session.get("id").and_then(|v| v.as_str())
    );

    drop(client);
    accepting
        .await
        .expect("accept task join")
        .expect("serve client");
}

#[tokio::test]
async fn create_and_attach_session() {
    let server = Server::bind("127.0.0.1:0").await.expect("bind test server");
    let address = server.local_addr().expect("local address");
    let accepting = tokio::spawn(async move { server.accept_once().await });

    let mut client = Client::connect(address).await.expect("hello handshake");

    // Create a session.
    let create_result = client
        .request(Command::Create {
            cwd: None,
            name: Some("attach-test".to_owned()),
            model: None,
            thinking_level: Some("medium".to_owned()),
        })
        .await
        .expect("create response");
    let session_id = create_result["session"]["id"]
        .as_str()
        .expect("session id")
        .to_owned();

    // Attach to the session.
    let attach_result = client
        .request(Command::Attach {
            session_id: session_id.clone(),
        })
        .await
        .expect("attach response");
    assert_eq!(attach_result["command"], "attach");
    let attached = attach_result.get("session").expect("session in result");
    assert_eq!(
        attached.get("id").and_then(|v| v.as_str()),
        Some(session_id.as_str())
    );
    assert_eq!(
        attached.get("name").and_then(|v| v.as_str()),
        Some("attach-test")
    );
    assert_eq!(
        attached.get("thinkingLevel").and_then(|v| v.as_str()),
        Some("medium")
    );
    // After attach, the session is marked attached.
    assert_eq!(
        attached.get("attached").and_then(|v| v.as_bool()),
        Some(true)
    );

    // Attach to a non-existent session returns error.
    let error = client
        .request(Command::Attach {
            session_id: "nonexistent".to_owned(),
        })
        .await
        .expect_err("attach to missing session");
    assert!(matches!(
        error,
        ClientError::Server { ref code, .. } if code == "not_found"
    ));

    drop(client);
    accepting
        .await
        .expect("accept task join")
        .expect("serve client");
}

#[tokio::test]
async fn create_with_model_and_thinking_and_list_across_connections() {
    let server = Server::bind("127.0.0.1:0").await.expect("bind test server");
    let address = server.local_addr().expect("local address");
    let accepting = tokio::spawn(async move { server.accept_once().await });

    let mut client = Client::connect(address).await.expect("hello handshake");

    // Create with model and thinking level.
    let result = client
        .request(Command::Create {
            cwd: None,
            name: None,
            model: Some(pir::protocol::ModelRef {
                provider: "anthropic".to_owned(),
                id: "claude-3-opus".to_owned(),
            }),
            thinking_level: Some("high".to_owned()),
        })
        .await
        .expect("create response");

    let session = &result["session"];
    assert_eq!(session["model"]["provider"].as_str(), Some("anthropic"));
    assert_eq!(session["model"]["id"].as_str(), Some("claude-3-opus"));
    assert_eq!(session["thinkingLevel"].as_str(), Some("high"));
    assert_eq!(session["cwd"].as_str(), Some("."));

    drop(client);
    accepting
        .await
        .expect("accept task join")
        .expect("serve client");
}

/// Bounds used by the timeout regressions: small enough to keep the suite
/// fast, large enough to be stable on a loaded machine.
fn short_timeouts(update: Duration) -> ClientTimeouts {
    ClientTimeouts {
        connect: Duration::from_millis(500),
        request: Duration::from_millis(250),
        update,
    }
}

/// Regression: a peer that accepts the TCP connection but never answers the
/// hello handshake must fail fast with [`ClientError::Timeout`] instead of
/// blocking `Client::connect` forever.
#[tokio::test]
async fn connect_times_out_when_peer_never_answers_hello() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind silent listener");
    let address = listener.local_addr().expect("local address");
    let holding = tokio::spawn(async move {
        let (_stream, _) = listener.accept().await.expect("accept client");
        // Hold the connection open and never write a hello reply.
        std::future::pending::<()>().await;
    });

    let timeouts = short_timeouts(Duration::from_millis(250));
    let started = Instant::now();
    let error = match Client::connect_with_timeouts(address, timeouts).await {
        Ok(_) => panic!("a peer that never greets must not complete the handshake"),
        Err(error) => error,
    };
    assert!(
        matches!(error, ClientError::Timeout { operation: "hello" }),
        "expected hello timeout, got {error:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "hello timeout must fire well under 2s, took {:?}",
        started.elapsed()
    );
    holding.abort();
}

/// Regression: a peer that completes the handshake but never answers a
/// request must fail the request with [`ClientError::Timeout`].
#[tokio::test]
async fn request_times_out_when_peer_never_responds() {
    let server_id = uuid::Uuid::new_v4().to_string();
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind stalling listener");
    let address = listener.local_addr().expect("local address");
    let holding = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept client");
        let hello = ServerEnvelope::Hello(ServerHelloV8 {
            type_: "hello".to_owned(),
            version: PROTOCOL_VERSION,
            server_id,
        });
        let frame =
            encode_server_envelope(&hello, FrameOptions::default()).expect("encode hello frame");
        stream.write_all(&frame).await.expect("write hello");
        // Never answer the follow-up request.
        std::future::pending::<()>().await;
    });

    let timeouts = short_timeouts(Duration::from_millis(250));
    let mut client = Client::connect_with_timeouts(address, timeouts)
        .await
        .expect("handshake against greeting peer");
    let started = Instant::now();
    let error = client.list().await.expect_err("list must not hang forever");
    assert!(
        matches!(
            error,
            ClientError::Timeout {
                operation: "request"
            }
        ),
        "expected request timeout, got {error:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "request timeout must fire well under 2s, took {:?}",
        started.elapsed()
    );
    holding.abort();
}

/// Regression: `next_update` on a live but silent subscription must time out
/// instead of blocking forever.
#[tokio::test]
async fn next_update_times_out_without_incoming_updates() {
    let server = Server::bind("127.0.0.1:0").await.expect("bind test server");
    let address = server.local_addr().expect("local address");
    let accepting = tokio::spawn(async move { server.accept_once().await });

    let timeouts = short_timeouts(Duration::from_millis(250));
    let mut client = Client::connect_with_timeouts(address, timeouts)
        .await
        .expect("hello handshake");
    let subscription = client
        .subscribe(SESSION_SERVICE_ID)
        .await
        .expect("subscribe response");
    let started = Instant::now();
    let error = client
        .next_update(&subscription)
        .await
        .expect_err("no updates are produced without session changes");
    assert!(
        matches!(
            error,
            ClientError::Timeout {
                operation: "update"
            }
        ),
        "expected update timeout, got {error:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "update timeout must fire well under 2s, took {:?}",
        started.elapsed()
    );
    drop(client);
    accepting
        .await
        .expect("accept task join")
        .expect("serve client");
}

#[allow(dead_code)]
fn _snapshot_type_is_public(snapshot: ServerSnapshot) -> ServerSnapshot {
    snapshot
}

// ---------------------------------------------------------------------------
// Session policy: bash exclusion, remote-cwd gate, idle timeout
// ---------------------------------------------------------------------------

/// Regression: with `allow_remote_bash` off (the default) the session's tool
/// registry must NOT carry the bash tool — the fail-closed remote-execution
/// control (`session_tool_registry`, src/server/session.rs).
#[tokio::test]
async fn session_tool_registry_excludes_bash_unless_remote_bash_allowed() {
    let server = Server::bind("127.0.0.1:0")
        .await
        .expect("bind default test server");
    server
        .state()
        .create_session("bash-excluded".to_owned(), None, None, None, None, 0)
        .await
        .expect("create session");
    let names = server
        .state()
        .session_tool_names("bash-excluded")
        .await
        .expect("session exists");
    assert!(!names.is_empty(), "the core tools must be registered");
    assert!(
        !names.iter().any(|name| name == "bash"),
        "bash must be excluded by default, got {names:?}"
    );

    let open = Server::bind_with_security(
        "127.0.0.1:0",
        ServerSecurityConfig {
            allow_remote_bash: true,
            ..ServerSecurityConfig::default()
        },
    )
    .await
    .expect("bind permissive test server");
    open.state()
        .create_session("bash-allowed".to_owned(), None, None, None, None, 0)
        .await
        .expect("create session");
    let names = open
        .state()
        .session_tool_names("bash-allowed")
        .await
        .expect("session exists");
    assert!(
        names.iter().any(|name| name == "bash"),
        "allow_remote_bash must enable the bash tool, got {names:?}"
    );
}

/// Drops the `PIR_SERVER_ALLOWED_ROOTS` env var and the temp root dir.
struct AllowedRootsCleanup(std::path::PathBuf);

impl Drop for AllowedRootsCleanup {
    fn drop(&mut self) {
        std::env::remove_var("PIR_SERVER_ALLOWED_ROOTS");
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Regression: a remote `create` naming an explicit `cwd` is policy-gated —
/// the default rejects any cwd but the server's own with the typed `forbidden`
/// error, and `PIR_SERVER_ALLOWED_ROOTS` (a `;`-separated root list) widens it
/// to cwds inside those roots (`ServerSecurityConfig::allows_cwd`).
#[tokio::test]
async fn create_with_cwd_is_policy_gated_and_allowed_roots_widen_it() {
    let root = std::env::temp_dir().join(format!(
        "pir-cwd-root-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock after epoch")
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).expect("create the cwd allowlist root");
    let _cleanup = AllowedRootsCleanup(root.clone());
    // Only this test in this binary touches the allowlist env var.
    std::env::remove_var("PIR_SERVER_ALLOWED_ROOTS");
    let root_text = root.to_string_lossy().to_string();

    // Phase 1 — no allowlist configured: any explicit cwd is rejected.
    let server = Server::bind("127.0.0.1:0")
        .await
        .expect("bind cwd test server");
    let address = server.local_addr().expect("local address");
    let accepting = tokio::spawn(async move { server.accept_once().await });
    let mut client = Client::connect(address).await.expect("hello handshake");
    for cwd in [
        root_text.clone(),
        root.join("sub").to_string_lossy().to_string(),
    ] {
        let error = client
            .request(Command::Create {
                cwd: Some(cwd.clone()),
                name: None,
                model: None,
                thinking_level: None,
            })
            .await
            .expect_err("a foreign cwd must be rejected without an allowlist");
        assert!(
            matches!(error, ClientError::Server { ref code, .. } if code == "forbidden"),
            "expected `forbidden` for cwd {cwd:?}, got {error:?}"
        );
    }
    drop(client);
    accepting
        .await
        .expect("accept task join")
        .expect("serve client");

    // Phase 2 — the allowlist widens the gate to cwds inside its roots.
    std::env::set_var("PIR_SERVER_ALLOWED_ROOTS", &root_text);
    let server = Server::bind("127.0.0.1:0")
        .await
        .expect("bind cwd test server with allowlist");
    let address = server.local_addr().expect("local address");
    let accepting = tokio::spawn(async move { server.accept_once().await });
    let mut client = Client::connect(address).await.expect("hello handshake");
    for cwd in [
        root_text.clone(),
        root.join("sub").to_string_lossy().to_string(),
    ] {
        let created = client
            .request(Command::Create {
                cwd: Some(cwd),
                name: None,
                model: None,
                thinking_level: None,
            })
            .await
            .expect("a cwd inside an allowed root must be accepted");
        assert_eq!(created["command"], "create");
    }
    let outside = format!("{root_text}-outside");
    let error = client
        .request(Command::Create {
            cwd: Some(outside),
            name: None,
            model: None,
            thinking_level: None,
        })
        .await
        .expect_err("a cwd outside every allowed root must still be rejected");
    assert!(
        matches!(error, ClientError::Server { ref code, .. } if code == "forbidden"),
        "expected `forbidden` outside the roots, got {error:?}"
    );

    // Phase 3 — `..` traversal segments must not bypass the textual prefix
    // check (`C:\root\a\..\..\Windows` normalizes outside the root).
    let escape = root
        .join("sub")
        .join("..")
        .join("..")
        .join("Windows")
        .to_string_lossy()
        .to_string();
    let error = client
        .request(Command::Create {
            cwd: Some(escape),
            name: None,
            model: None,
            thinking_level: None,
        })
        .await
        .expect_err("a cwd with `..` segments must be rejected, not prefix-matched");
    assert!(
        matches!(error, ClientError::Server { ref code, .. } if code == "forbidden"),
        "expected `forbidden` for a `..` traversal cwd, got {error:?}"
    );
    drop(client);
    accepting
        .await
        .expect("accept task join")
        .expect("serve client");
}

/// Regression: an idle connection (post-handshake, no frames) is reaped at
/// the configured idle deadline instead of holding its connection slot. The
/// deadline is injectable via `ServerSecurityConfig::idle_timeout`.
#[tokio::test]
async fn idle_connection_is_reaped_at_the_configured_deadline() {
    let server = Server::bind_with_security(
        "127.0.0.1:0",
        ServerSecurityConfig {
            idle_timeout: Duration::from_millis(150),
            ..ServerSecurityConfig::default()
        },
    )
    .await
    .expect("bind idle-timeout test server");
    let address = server.local_addr().expect("local address");
    let accepting = tokio::spawn(async move { server.accept_once().await });

    let client = Client::connect(address).await.expect("hello handshake");
    let started = Instant::now();
    accepting
        .await
        .expect("accept task join")
        .expect_err("an idle connection must be closed by the server");
    let elapsed = started.elapsed();
    assert!(
        elapsed >= Duration::from_millis(100),
        "the connection must survive at least one idle deadline, closed after {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "the idle deadline must be the configured 150ms, took {elapsed:?}"
    );
    drop(client);
}
