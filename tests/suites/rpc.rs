//! Process-level RPC coverage: stdin/stdout JSON lines stay schema-valid.

use crate::cli_entry_contract::common;

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

/// A minimal models.json pointed at the shared instant-401 sandbox: the
/// startup session the RPC host now attaches unconditionally
/// (upstream `main.ts:950-952`) is built through the real session factory,
/// which needs a resolvable model.
fn sandbox_models_json() -> String {
    let base_url = common::sandbox_endpoint();
    format!(
        r#"{{"providers":{{"bench":{{"name":"rpc test","baseUrl":"{base_url}","apiKey":"x","api":"openai-completions","models":[{{"id":"bench-model","name":"bench","api":"openai-completions","baseUrl":"{base_url}","reasoning":false,"contextWindow":4096,"maxTokens":256,"input":["text"]}}]}}}}}}"#
    )
}

/// One hermetic home per spawned process (tests in this binary run in
/// parallel). Returns the home dir; the caller keeps it alive for the run and
/// owns cleanup.
fn spawn_home() -> std::path::PathBuf {
    static SPAWN_SEQ: AtomicUsize = AtomicUsize::new(0);
    let seq = SPAWN_SEQ.fetch_add(1, Ordering::SeqCst);
    let home = std::env::temp_dir().join(format!("pir-rpc-startup-{}-{seq}", std::process::id()));
    let _ = std::fs::create_dir_all(home.join("agent"));
    let _ = std::fs::write(
        home.join("agent").join("models.json"),
        sandbox_models_json(),
    );
    home
}

fn spawn_rpc_with(
    args: &[&str],
    home: &std::path::Path,
) -> (
    std::process::Child,
    BufReader<std::process::ChildStdout>,
    std::process::ChildStdin,
) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_pir"));
    command
        .arg("rpc")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("PIR_AGENT_DIR", home.join("agent"))
        .env("PI_CODING_AGENT_SESSION_DIR", home.join("sessions"));
    for (_provider, keys) in pir::ai::auth::env_api_keys::all() {
        for key in keys {
            command.env_remove(key);
        }
    }
    let mut child = command.spawn().expect("pir rpc should start");
    let stdout = child.stdout.take().expect("rpc stdout");
    let stdin = child.stdin.take().expect("rpc stdin");
    // Drain stderr: a full pipe would block the child mid-command.
    if let Some(stderr) = child.stderr.take() {
        std::thread::spawn(move || {
            use std::io::Read as _;
            let mut stderr = stderr;
            let mut buffer = [0u8; 4096];
            while let Ok(read) = stderr.read(&mut buffer) {
                if read == 0 {
                    return;
                }
            }
        });
    }
    (child, BufReader::new(stdout), stdin)
}

fn spawn_rpc() -> (
    std::process::Child,
    BufReader<std::process::ChildStdout>,
    std::process::ChildStdin,
) {
    spawn_rpc_with(&[], &spawn_home())
}

fn send(stdin: &mut std::process::ChildStdin, line: &str) {
    stdin
        .write_all(format!("{line}\n").as_bytes())
        .expect("write rpc line");
    stdin.flush().expect("flush rpc line");
}

/// Reap the rpc child after its stdin was closed.
///
/// A child that never attached a session exits at once; after a prompt the
/// server's bounded writer drain may sit in its full 5 s grace window even
/// though every response under test has already been delivered. Give a short
/// grace, then kill and reap (a leaked child keeps `pir.exe` locked on
/// Windows). Clean shutdown is asserted separately by
/// `rpc_extension_ui::extension_shutdown_*`.
fn wait_or_kill(child: &mut std::process::Child) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(250);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return;
            }
        }
    }
}

fn read_line(reader: &mut BufReader<std::process::ChildStdout>) -> serde_json::Value {
    let mut line = String::new();
    reader.read_line(&mut line).expect("read rpc line");
    assert!(!line.is_empty(), "rpc stdout closed before a response");
    serde_json::from_str(line.trim()).expect("rpc line must be valid JSON")
}

#[test]
fn rpc_process_completes_hello_and_list() {
    let (mut child, mut reader, mut stdin) = spawn_rpc();

    send(&mut stdin, r#"{"type":"hello"}"#);
    let hello = read_line(&mut reader);
    assert_eq!(hello["type"], "response", "{hello}");
    assert_eq!(hello["command"], "hello", "{hello}");
    assert!(
        hello.get("data").is_some(),
        "hello must identify the protocol: {hello}"
    );

    send(&mut stdin, r#"{"type":"list"}"#);
    let list = read_line(&mut reader);
    assert_eq!(list["command"], "list", "{list}");
    assert_eq!(list["success"], true, "{list}");
    assert!(list["data"].get("sessions").is_some(), "{list}");

    drop(stdin);
    wait_or_kill(&mut child);
}

#[test]
fn rpc_unknown_command_reports_failure_without_dying() {
    let (mut child, mut reader, mut stdin) = spawn_rpc();

    send(&mut stdin, r#"{"type":"hello"}"#);
    let _ = read_line(&mut reader);

    send(&mut stdin, r#"{"type":"definitely-not-a-command"}"#);
    let response = read_line(&mut reader);
    assert_eq!(response["success"], false, "{response}");

    // The process must survive and keep serving.
    send(&mut stdin, r#"{"type":"hello"}"#);
    let hello = read_line(&mut reader);
    assert_eq!(hello["command"], "hello", "{hello}");

    drop(stdin);
    wait_or_kill(&mut child);
}

#[test]
fn rpc_rejected_prompt_error_carries_dropped_disposition() {
    // A rejected prompt (here: empty text, refused at preflight) is a plain
    // error response: upstream rpc-mode.ts emits `error(id, "prompt",
    // e.message)` when the prompt preflight fails, with no data payload and
    // no disposition.
    let (mut child, mut reader, mut stdin) = spawn_rpc();

    send(&mut stdin, r#"{"type":"hello"}"#);
    let _ = read_line(&mut reader);

    send(&mut stdin, r#"{"type":"prompt","id":"p1","text":""}"#);
    let response = read_line(&mut reader);
    assert_eq!(response["type"], "response", "{response}");
    assert_eq!(response["command"], "prompt", "{response}");
    assert_eq!(response["success"], false, "{response}");
    assert!(
        response["error"].is_string(),
        "the rejection names its cause: {response}"
    );
    assert!(
        response.get("data").is_none(),
        "a rejected prompt carries no data payload (no dropped disposition): {response}"
    );

    // The process must survive and keep serving.
    send(&mut stdin, r#"{"type":"hello"}"#);
    let hello = read_line(&mut reader);
    assert_eq!(hello["command"], "hello", "{hello}");

    drop(stdin);
    wait_or_kill(&mut child);
}

// ---------------------------------------------------------------------------
// §2 #4: the CLI's session is attached at RPC startup (upstream
// `main.ts:950-952` -> `runRpcMode(runtime)`); an explicit attach/create
// still replaces it.
// ---------------------------------------------------------------------------

/// Seed one persisted session (a header-only file is not discoverable: the
/// file materializes with its first conversation message).
fn seed_session(session_dir: &std::path::Path, cwd: &str, text: &str) -> String {
    use pir::ai::types::{ContentBlock, Message, TextContent, UserMessage, UserRole};
    let mut session = pir::session_manager::SessionManager::create(cwd, session_dir)
        .expect("create seed session");
    let id = session.id().to_owned();
    session
        .append_message(Message::User(UserMessage {
            role: UserRole::User,
            content: vec![ContentBlock::Text(TextContent {
                text: text.to_owned(),
                text_signature: None,
            })],
            tools_added: None,
            tools_removed: None,
            timestamp: 1,
            sections: None,
        }))
        .expect("persist seed message");
    id
}

/// Read protocol lines until the response for `command` arrives (startup may
/// in principle emit session events before the first response).
fn read_response(
    reader: &mut BufReader<std::process::ChildStdout>,
    command: &str,
) -> serde_json::Value {
    loop {
        let line = read_line(reader);
        if line["type"] == "response" && line["command"] == command {
            return line;
        }
    }
}

#[test]
fn rpc_startup_auto_attaches_the_selected_session() {
    let home = spawn_home();
    let cwd = std::env::current_dir().expect("test cwd");
    let session_dir = home.join("sessions");
    std::fs::create_dir_all(&session_dir).expect("create session dir");
    let id = seed_session(&session_dir, &cwd.to_string_lossy(), "startup seed");
    let path = pir::session_manager::SessionManager::find_session_by_id(&session_dir, &id)
        .expect("seed session file");
    let path_text = path.to_string_lossy().into_owned();

    let (mut child, mut reader, mut stdin) = spawn_rpc_with(&["--session", &path_text], &home);

    // No attach/create first: the selected session is already attached.
    send(&mut stdin, r#"{"type":"get_state","id":"s0"}"#);
    let state = read_response(&mut reader, "get_state");
    assert_eq!(state["success"], true, "{state}");
    assert_eq!(state["data"]["sessionId"], id, "{state}");
    assert_eq!(
        state["data"]["messageCount"], 1,
        "the attached storage carries the seeded message: {state}"
    );

    // The old explicit attach path stays idempotent: re-attaching the same
    // session succeeds and keeps the same id.
    send(
        &mut stdin,
        &format!(
            r#"{{"type":"attach","id":"a1","session":{}}}"#,
            serde_json::json!(path_text)
        ),
    );
    let attached = read_response(&mut reader, "attach");
    assert_eq!(attached["success"], true, "{attached}");
    assert_eq!(attached["data"]["id"], id, "{attached}");
    send(&mut stdin, r#"{"type":"get_state","id":"s1"}"#);
    let state = read_response(&mut reader, "get_state");
    assert_eq!(state["data"]["sessionId"], id, "{state}");

    drop(stdin);
    wait_or_kill(&mut child);
}

#[test]
fn rpc_startup_auto_attaches_the_newest_session_for_resume() {
    let home = spawn_home();
    let cwd = std::env::current_dir().expect("test cwd");
    let cwd_text = cwd.to_string_lossy().into_owned();
    let session_dir = home.join("sessions");
    std::fs::create_dir_all(&session_dir).expect("create session dir");
    let _older = seed_session(&session_dir, &cwd_text, "older");
    std::thread::sleep(std::time::Duration::from_millis(120));
    let newer = seed_session(&session_dir, &cwd_text, "newer");

    // `--resume` in a headless host selects the project's most recent session
    // (no picker) and attaches it before serving.
    let (mut child, mut reader, mut stdin) = spawn_rpc_with(&["--resume"], &home);

    send(&mut stdin, r#"{"type":"get_state","id":"s0"}"#);
    let state = read_response(&mut reader, "get_state");
    assert_eq!(state["success"], true, "{state}");
    assert_eq!(state["data"]["sessionId"], newer, "{state}");

    drop(stdin);
    wait_or_kill(&mut child);
}

#[test]
fn rpc_startup_attaches_a_fresh_session_without_any_attach_flag() {
    let home = spawn_home();
    let (mut child, mut reader, mut stdin) = spawn_rpc_with(&[], &home);

    // Upstream always creates a session for a run (`createSessionManager`
    // falls through to `SessionManager.create`), so the RPC host starts
    // attached even with no session flag.
    send(&mut stdin, r#"{"type":"get_state","id":"s0"}"#);
    let state = read_response(&mut reader, "get_state");
    assert_eq!(state["success"], true, "{state}");
    let session_id = state["data"]["sessionId"]
        .as_str()
        .unwrap_or_else(|| panic!("a startup session id: {state}"));
    assert!(!session_id.is_empty(), "{state}");
    let session_file = state["data"]["sessionFile"]
        .as_str()
        .unwrap_or_else(|| panic!("a startup session file: {state}"));
    assert!(
        session_file.ends_with(&format!("_{session_id}.jsonl")),
        "the planned new session carries its id: {session_file}"
    );

    // `prompt` no longer fails with the unattached-session error: it reaches
    // the run preflight (and only rejects the empty text). No model request
    // is needed to prove the session is attached.
    send(&mut stdin, r#"{"type":"prompt","id":"p1","text":""}"#);
    let prompt = read_response(&mut reader, "prompt");
    assert_eq!(prompt["success"], false, "{prompt}");
    let error = prompt["error"].as_str().unwrap_or_default();
    assert!(
        error.contains("non-empty"),
        "the empty prompt reaches the attached session's preflight: {prompt}"
    );
    assert!(
        !error.contains("no attached session"),
        "the startup session is attached: {prompt}"
    );

    drop(stdin);
    wait_or_kill(&mut child);
}

// ---------------------------------------------------------------------------
// F2/F7: a stalled client must not wedge a one-worker runtime, and per-line
// dispatch is bounded.
// ---------------------------------------------------------------------------

/// The dispatch admission policy: one line per task, but at most a fixed
/// number in flight (a client that pipelines lines without reading responses
/// must not grow tasks without bound; the extra line gets a structured error).
#[cfg(feature = "rpc")]
#[test]
fn rpc_command_dispatch_is_bounded() {
    use pir::coding_agent::rpc::command_dispatch_admits;

    // The cap is 16 (the serve loop answers line 17 with an error).
    for running in 0..16 {
        assert!(
            command_dispatch_admits(running),
            "{running} in-flight commands are below the cap"
        );
    }
    for running in [16, 17, 100, usize::MAX] {
        assert!(
            !command_dispatch_admits(running),
            "{running} in-flight commands are past the cap"
        );
    }
}

/// The prompt streaming claim is released by its RAII guard even when the run
/// never starts: `handle_prompt` builds the guard before queueing the
/// acknowledgement (which can fail when the output writer died) and moves it
/// into the spawned future, so a future that is dropped before its first poll
/// (the run's thread or runtime could not be created) still releases it.
#[cfg(feature = "rpc")]
#[test]
fn prompt_claim_guard_releases_a_never_polled_run() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    use pir::coding_agent::rpc::PromptClaimGuard;

    let claim = Arc::new(AtomicBool::new(true));
    let guard = PromptClaimGuard(Arc::clone(&claim));
    assert!(
        claim.load(Ordering::SeqCst),
        "the claim stays held while the guard lives"
    );
    drop(guard);
    assert!(
        !claim.load(Ordering::SeqCst),
        "a never-polled prompt run must release the streaming claim"
    );
}

/// A client that never reads stdout and a prompt run whose event stream fills
/// both bounded bridges: the run parks on backpressure, but the serve loop
/// must keep running on a single-worker runtime so stdin EOF (and with it
/// teardown) is still observed. Before the fix the run lived on the only
/// worker and blocked inside the event handler, so `serve` never returned.
#[cfg(feature = "rpc")]
#[test]
fn a_stalled_rpc_client_does_not_deadlock_a_single_worker_runtime() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::task::{Context as TaskContext, Poll};
    use std::time::Duration;

    use pir::agent::tool_execution::ToolRegistry;
    use pir::agent_session::{AgentSession, AgentSessionConfig};
    use pir::ai::auth::ApiKeyAuth;
    use pir::ai::event_stream::AssistantMessageEventStream;
    use pir::ai::provider::{Provider, ProviderRegistry};
    use pir::ai::types::*;
    use pir::coding_agent::rpc::{RpcServer, SessionFactory};
    use tokio::io::{AsyncWrite, AsyncWriteExt as _};

    /// The output side that never becomes ready: the writer thread parks
    /// forever, exactly like a client that stopped draining the pipe.
    struct NeverWriter;

    impl AsyncWrite for NeverWriter {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut TaskContext<'_>,
            _buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            Poll::Pending
        }

        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut TaskContext<'_>,
        ) -> Poll<std::io::Result<()>> {
            Poll::Pending
        }

        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut TaskContext<'_>,
        ) -> Poll<std::io::Result<()>> {
            Poll::Pending
        }
    }

    fn fake_model() -> Model {
        Model {
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
            context_window: 4096,
            max_tokens: 256,
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

    fn assistant_text(text: &str) -> AssistantMessage {
        AssistantMessage {
            api: "fake".into(),
            provider: "fake".into(),
            model: "fake-model".into(),
            content: vec![ContentBlock::Text(TextContent {
                text: text.into(),
                text_signature: None,
            })],
            stop_reason: StopReason::Stop,
            timestamp: 1,
            ..Default::default()
        }
    }

    /// Emits many deltas (one agent event each), far past the two 256-entry
    /// bridges, so the run's event handler has to park.
    struct FloodProvider {
        started: Arc<AtomicUsize>,
    }

    impl Provider for FloodProvider {
        fn id(&self) -> &str {
            // The test model below is `fake`; the registry resolves by
            // `model.provider`, so this provider must register under the same
            // id or the run fails before reaching the flood stream.
            "fake"
        }
        fn name(&self) -> &str {
            "Flood provider"
        }
        fn get_models(&self) -> Vec<Model> {
            vec![fake_model()]
        }
        fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
            vec![ApiKeyAuth::new("flood key", vec!["FLOOD_TEST_KEY".into()])]
        }
        fn stream(
            &self,
            _model: &Model,
            _context: Context,
            _options: StreamOptions,
        ) -> AssistantMessageEventStream {
            self.started.fetch_add(1, Ordering::SeqCst);
            let stream = AssistantMessageEventStream::new();
            let mut start = assistant_text("");
            start.content.clear();
            stream.push(AssistantMessageEvent::Start {
                partial: Arc::new(start),
            });
            stream.push(AssistantMessageEvent::TextStart {
                content_index: 0,
                partial: Arc::new(assistant_text("")),
            });
            let mut accumulated = String::new();
            for _ in 0..2000 {
                accumulated.push('x');
                stream.push(AssistantMessageEvent::TextDelta {
                    content_index: 0,
                    delta: "x".into(),
                    partial: Arc::new(assistant_text(&accumulated)),
                });
            }
            stream.push(AssistantMessageEvent::TextEnd {
                content_index: 0,
                content: accumulated.clone(),
                partial: Arc::new(assistant_text(&accumulated)),
            });
            stream.push(AssistantMessageEventStream::done_event(
                TerminalReason::Stop,
                assistant_text(&accumulated),
            ));
            stream
        }
    }

    fn flood_factory(started: Arc<AtomicUsize>) -> SessionFactory {
        Arc::new(move |session| {
            let mut registry = ProviderRegistry::default();
            registry.set_provider(Arc::new(FloodProvider {
                started: Arc::clone(&started),
            }));
            Ok(Arc::new(AgentSession::new(AgentSessionConfig {
                provider_registry: Arc::new(registry),
                model: fake_model(),
                api_key: None,
                thinking: ThinkingLevel::Off,
                tools: ToolRegistry::new(),
                session,
                compaction: None,
                max_turns: 1,
                system_prompt: None,
                cwd: None,
                event_stream: None,
                extensions: None,
                tool_selection: None,
            })))
        })
    }

    let started = Arc::new(AtomicUsize::new(0));
    let session_dir = std::env::temp_dir().join(format!(
        "pir-rpc-stalled-client-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&session_dir).expect("session dir");
    let server = RpcServer::new(session_dir.clone(), flood_factory(Arc::clone(&started)));
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("a one-worker runtime");
    let outcome = runtime.block_on(async move {
        let (mut input, server_input) = tokio::io::duplex(64 * 1024);
        let serve = tokio::spawn(async move { server.serve(server_input, NeverWriter).await });
        input
            .write_all(b"{\"type\":\"create\"}\n")
            .await
            .expect("create command");
        input
            .write_all(b"{\"type\":\"prompt\",\"id\":\"p1\",\"text\":\"flood\"}\n")
            .await
            .expect("prompt command");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while started.load(Ordering::SeqCst) == 0 && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            started.load(Ordering::SeqCst) > 0,
            "the prompt run never reached the provider"
        );
        // Let the event bridges fill and the run park on backpressure.
        tokio::time::sleep(Duration::from_millis(300)).await;
        // stdin EOF: the serve loop must observe this and tear down even
        // though the client never read a byte and the run is stalled.
        drop(input);
        tokio::time::timeout(Duration::from_secs(20), serve).await
    });
    let joined = outcome.expect("serve must not deadlock on a single-worker runtime");
    joined.expect("serve task joins").expect("serve result");
    let _ = std::fs::remove_dir_all(&session_dir);
}
