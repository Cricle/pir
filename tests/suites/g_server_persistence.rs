//! G1 (P1-1): server-side session persistence hosting, multi-attachment, and
//! draining semantics — the pir counterpart of upstream
//! `packages/server/src/session-router.ts`:
//!
//! * `resolveSession`/`openSession` over durable sessions → the server state
//!   optionally backs every session with a `SessionManager` JSONL file; the
//!   list merges the directory and attaching a persisted session reopens it
//!   (survives disconnects and server restarts).
//! * `HostedSession.attachments: Set` → one session carries several
//!   attachments at once; every attached connection fences its calls with its
//!   own attachment id and broadcasts reach every subscriber.
//! * `isClosing`/`ServerDrainingError` → `begin_drain` rejects new work with
//!   the `server_draining` error code and aborts in-flight runs (their
//!   deferred prompt response reports the same code).

use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pir::ai::auth::ApiKeyAuth;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::types::{
    AssistantMessage, ContentBlock, Context, ErrorReason, Message, Model, ModelCost,
    ModelCostRates, StopReason, StreamOptions, TerminalReason, TextContent, UserMessage, UserRole,
};
use pir::client::{Client, ClientError, ClientTimeouts};
use pir::protocol::{Command, ModelRef, RpcTarget, SESSION_SERVICE_ID};
use pir::server::{empty_snapshot, Server, ServerSecurityConfig, ServerState};
use pir::session_manager::SessionManager;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::watch;

const WAIT_BUDGET: Duration = Duration::from_secs(10);

/// Per-test temp session directory.
struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-g-server-persistence-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&path).expect("create temp dir");
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Scripted model catalog entry (same shape as `tests/suites/server_prompt_abort.rs`).
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

fn text(text: &str) -> ContentBlock {
    ContentBlock::Text(TextContent {
        text: text.into(),
        text_signature: None,
    })
}

fn assistant(content: Vec<ContentBlock>, stop_reason: StopReason) -> AssistantMessage {
    AssistantMessage {
        api: "fake".into(),
        provider: "fake".into(),
        model: "fake-model".into(),
        content,
        stop_reason,
        timestamp: 1,
        ..Default::default()
    }
}

/// A provider that records every model request's context and answers
/// immediately — the harness for asserting the persisted transcript replays.
struct RecordingProvider {
    contexts: Mutex<Vec<Context>>,
}

impl Provider for RecordingProvider {
    fn id(&self) -> &str {
        "fake"
    }
    fn name(&self) -> &str {
        "Recording fake provider"
    }
    fn get_models(&self) -> Vec<Model> {
        vec![fake_model()]
    }
    fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
        vec![ApiKeyAuth::new("fake key", vec!["FAKE_TEST_KEY".into()])]
    }
    fn stream(
        &self,
        _model: &Model,
        context: Context,
        _options: StreamOptions,
    ) -> AssistantMessageEventStream {
        self.contexts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(context);
        let stream = AssistantMessageEventStream::new();
        let pusher = stream.clone();
        pusher.push(AssistantMessageEventStream::done_event(
            TerminalReason::Stop,
            assistant(vec![text("ok")], StopReason::Stop),
        ));
        stream
    }
}

fn recording_registry() -> (Arc<ProviderRegistry>, Arc<RecordingProvider>) {
    let provider = Arc::new(RecordingProvider {
        contexts: Mutex::new(Vec::new()),
    });
    let mut registry = ProviderRegistry::default();
    registry.set_provider(provider.clone());
    (Arc::new(registry), provider)
}

/// A provider whose stream only terminates when the shared gate opens or the
/// run's abort signal fires (same harness as `server_prompt_abort.rs`).
struct GatedProvider {
    gate: watch::Receiver<bool>,
    entered: Arc<AtomicUsize>,
}

impl Provider for GatedProvider {
    fn id(&self) -> &str {
        "fake"
    }
    fn name(&self) -> &str {
        "Gated fake provider"
    }
    fn get_models(&self) -> Vec<Model> {
        vec![fake_model()]
    }
    fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
        vec![ApiKeyAuth::new("fake key", vec!["FAKE_TEST_KEY".into()])]
    }
    fn stream(
        &self,
        _model: &Model,
        _context: Context,
        options: StreamOptions,
    ) -> AssistantMessageEventStream {
        self.entered.fetch_add(1, Ordering::SeqCst);
        let stream = AssistantMessageEventStream::new();
        let pusher = stream.clone();
        let mut gate = self.gate.clone();
        let signal = options.signal;
        tokio::spawn(async move {
            loop {
                if signal.as_ref().is_some_and(|signal| signal.is_aborted()) {
                    pusher.push(AssistantMessageEventStream::error_event(
                        ErrorReason::Aborted,
                        assistant(vec![text("aborted")], StopReason::Aborted),
                    ));
                    return;
                }
                if *gate.borrow() {
                    pusher.push(AssistantMessageEventStream::done_event(
                        TerminalReason::Stop,
                        assistant(vec![text("done")], StopReason::Stop),
                    ));
                    return;
                }
                if tokio::time::timeout(Duration::from_millis(20), gate.changed())
                    .await
                    .is_err()
                {
                    continue;
                }
            }
        });
        stream
    }
}

fn gated_registry(gate: watch::Receiver<bool>, entered: Arc<AtomicUsize>) -> Arc<ProviderRegistry> {
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(GatedProvider { gate, entered }));
    Arc::new(registry)
}

/// Poll the registry until the session's phase matches.
async fn wait_for_phase(state: &ServerState, session_id: &str, phase: &str) {
    for _ in 0..2000 {
        if let Some(snapshot) = state.get_session(session_id).await {
            if snapshot.phase == phase {
                return;
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("session {session_id} never entered phase {phase}");
}

fn fake_model_ref() -> Option<ModelRef> {
    Some(ModelRef {
        provider: "fake".to_owned(),
        id: "fake-model".to_owned(),
    })
}

/// Poll the registry until the session's phase matches (see above); shared
/// helper kept local to this suite.
async fn create_session(client: &mut Client) -> String {
    let created = client
        .request(Command::Create {
            cwd: None,
            name: None,
            model: fake_model_ref(),
            thinking_level: None,
        })
        .await
        .expect("create response");
    created["session"]["id"]
        .as_str()
        .expect("session id")
        .to_owned()
}

// ---------------------------------------------------------------------------
// 1. Persistence hosting
// ---------------------------------------------------------------------------

/// A session created on a server with a session directory is backed by a
/// JSONL file: the prompt's messages are on disk, the in-memory record is
/// released on disconnect while the session stays listed, and a RESTARTED
/// server bound to the same directory lists the session, reopens it on
/// attach, and replays the persisted transcript into the next model request.
#[tokio::test]
#[allow(clippy::await_holding_lock)] // the recording-provider lock must span the awaits
async fn persisted_session_survives_disconnect_and_restart() {
    let temp = TempDir::new("round-trip");
    let (registry, _provider) = recording_registry();
    let state =
        ServerState::with_providers(empty_snapshot(uuid::Uuid::new_v4().to_string()), registry)
            .with_session_dir(temp.0.clone());
    let server = Server::bind_with_state("127.0.0.1:0", state.clone())
        .await
        .expect("bind server");
    let address = server.local_addr().expect("local address");
    let accepting = tokio::spawn(async move { server.accept_once().await });

    let mut client = Client::connect(address).await.expect("hello handshake");
    let session_id = create_session(&mut client).await;
    client
        .request(Command::Attach {
            session_id: session_id.clone(),
        })
        .await
        .expect("attach");
    let prompt = tokio::time::timeout(
        WAIT_BUDGET,
        client.request(Command::Prompt {
            session_id: session_id.clone(),
            text: "hello persistence".to_owned(),
        }),
    )
    .await
    .expect("prompt completes")
    .expect("prompt ok");
    assert_eq!(prompt["command"], "prompt");

    // The durable JSONL file carries the conversation.
    let path = pir::session_manager::SessionManager::find_session_by_id(&temp.0, &session_id)
        .expect("session file persisted under the session directory");
    let file_text = std::fs::read_to_string(&path).expect("read session file");
    assert!(
        file_text.contains("hello persistence"),
        "the user message must be on disk: {file_text}"
    );

    // Disconnect: the in-memory record is released, the session stays listed
    // through the durable file.
    drop(client);
    tokio::time::timeout(WAIT_BUDGET, accepting)
        .await
        .expect("connection task settles")
        .expect("accept task join")
        .expect("serve client");
    let mut reaped = false;
    for _ in 0..2000 {
        if state.get_session(&session_id).await.is_none() {
            reaped = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(reaped, "in-memory record released after disconnect");
    assert!(
        state
            .session_metadata()
            .await
            .iter()
            .any(|metadata| metadata.id == session_id),
        "the session stays listed after disconnect via the durable file"
    );

    // Restart: a fresh server on the same directory lists and reopens it.
    let (registry2, provider2) = recording_registry();
    let state2 =
        ServerState::with_providers(empty_snapshot(uuid::Uuid::new_v4().to_string()), registry2)
            .with_session_dir(temp.0.clone());
    let server2 = Server::bind_with_state("127.0.0.1:0", state2)
        .await
        .expect("rebind server");
    let address2 = server2.local_addr().expect("second local address");
    let accepting2 = tokio::spawn(async move { server2.accept_once().await });
    let mut client2 = Client::connect(address2).await.expect("second handshake");
    let sessions = client2.list().await.expect("list after restart");
    assert!(
        sessions.iter().any(|s| s["id"] == session_id.as_str()),
        "persisted session listed after restart: {sessions:?}"
    );
    client2
        .request(Command::Attach {
            session_id: session_id.clone(),
        })
        .await
        .expect("attach reopens the persisted session");
    // The reopened record lost the create-time model (it is not a file
    // entry), so re-select it, then prompt: the persisted transcript must
    // reach the new model request.
    client2
        .request(Command::SetModel {
            session_id: session_id.clone(),
            model: fake_model_ref().expect("fake model ref"),
        })
        .await
        .expect("set_model on reopened session");
    tokio::time::timeout(
        WAIT_BUDGET,
        client2.request(Command::Prompt {
            session_id: session_id.clone(),
            text: "and again".to_owned(),
        }),
    )
    .await
    .expect("prompt after restart completes")
    .expect("prompt after restart ok");
    let contexts = provider2
        .contexts
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(
        contexts.len(),
        1,
        "exactly one model request after the restart"
    );
    let rendered = serde_json::to_string(&contexts[0]).expect("serialize the recorded context");
    assert!(
        rendered.contains("hello persistence"),
        "the reopened session must replay the persisted transcript: {rendered}"
    );
    drop(client2);
    tokio::time::timeout(WAIT_BUDGET, accepting2)
        .await
        .expect("second connection task settles")
        .expect("accept task join")
        .expect("serve client 2");
}

/// A real `pir server` child process bound to an ephemeral port, with its
/// `PIR_SESSION_DIR`/`PIR_AGENT_DIR` isolated per test.
struct SpawnedCliServer {
    child: tokio::process::Child,
    address: std::net::SocketAddr,
}

/// Spawn `pir server` and wait for its `pir server listening on <addr>` line.
/// The child's stderr is drained in the background afterwards so a long-lived
/// server can never block on a full pipe.
async fn spawn_cli_server(
    session_dir: &std::path::Path,
    agent_dir: &std::path::Path,
) -> SpawnedCliServer {
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_pir"))
        .arg("server")
        .current_dir(agent_dir)
        .env("PIR_SESSION_DIR", session_dir)
        .env("PIR_AGENT_DIR", agent_dir)
        .env("PIR_CODING_AGENT_DIR", agent_dir)
        .env("PIR_SERVER_ADDRESS", "127.0.0.1:0")
        .env("PIR_OFFLINE", "1")
        .env("PI_OFFLINE", "1")
        .env("PIR_NO_CONTEXT_FILES", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn pir server");
    let stderr = child.stderr.take().expect("server stderr is piped");
    let mut lines = BufReader::new(stderr).lines();
    let deadline = tokio::time::Instant::now() + WAIT_BUDGET;
    let mut seen = Vec::new();
    loop {
        let line = tokio::time::timeout_at(deadline, lines.next_line())
            .await
            .expect("pir server must announce its listener within the budget")
            .expect("read pir server stderr")
            .unwrap_or_else(|| panic!("pir server exited before listening; stderr: {seen:?}"));
        if let Some(address) = line.strip_prefix("pir server listening on ") {
            let address = address
                .trim()
                .parse()
                .expect("parse the announced listener address");
            tokio::spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} });
            return SpawnedCliServer { child, address };
        }
        seen.push(line);
    }
}

/// P0: the production CLI host must bind its durable session directory. The
/// `pir server` subcommand previously built `ServerState::default()`, so
/// persistence was never enabled: a session file in the configured
/// `PIR_SESSION_DIR` was invisible, and a restart lost every session. This
/// test spawns the real binary twice against the same directory and requires
/// the seeded session to be listed and attachable by both generations.
#[tokio::test]
async fn cli_server_host_enables_session_persistence_across_restarts() {
    let sessions = TempDir::new("cli-host-sessions");
    let agent = TempDir::new("cli-host-agent");
    let session_id = persist_session_file(&sessions.0, "cli-host-persisted", 2);

    for generation in 0..2 {
        let mut server = spawn_cli_server(&sessions.0, &agent.0).await;
        let mut client = Client::connect(server.address)
            .await
            .expect("connect to the pir server host");
        let listed = client.list().await.expect("list from the pir server host");
        assert!(
            listed
                .iter()
                .any(|session| session["id"] == session_id.as_str()),
            "generation {generation}: `pir server` must list the persisted session \
             (host wiring is missing `with_session_dir`): {listed:?}"
        );
        let attached = client
            .request(Command::Attach {
                session_id: session_id.clone(),
            })
            .await
            .expect("attach reopens the persisted session through the CLI host");
        assert_eq!(attached["session"]["id"], session_id.as_str());
        drop(client);
        server
            .child
            .kill()
            .await
            .expect("stop the pir server child");
        let _ = server.child.wait().await;
    }
}

// ---------------------------------------------------------------------------
// 2. Multiple attachments
// ---------------------------------------------------------------------------

/// One session carries several attachments at once (upstream
/// `HostedSession.attachments`): a second client attaches without a lock
/// error, both subscribers receive the run's service updates, detaching one
/// keeps the session attached for the other, and the surviving attachment
/// keeps prompting.
#[tokio::test]
async fn session_supports_multiple_attachments_and_broadcasts() {
    let (registry, _provider) = recording_registry();
    let state =
        ServerState::with_providers(empty_snapshot(uuid::Uuid::new_v4().to_string()), registry);
    let server = Arc::new(
        Server::bind_with_state("127.0.0.1:0", state.clone())
            .await
            .expect("bind server"),
    );
    let address = server.local_addr().expect("local address");
    let accepting_a = {
        let server = server.clone();
        tokio::spawn(async move { server.accept_once().await })
    };
    let accepting_b = {
        let server = server.clone();
        tokio::spawn(async move { server.accept_once().await })
    };

    let mut client_a = Client::connect(address).await.expect("client A handshake");
    let session_id = create_session(&mut client_a).await;
    client_a
        .request(Command::Attach {
            session_id: session_id.clone(),
        })
        .await
        .expect("client A attach");
    let attachment_a = client_a
        .attachment_id()
        .expect("A attachment id")
        .to_owned();

    let mut client_b = Client::connect(address).await.expect("client B handshake");
    client_b
        .request(Command::Attach {
            session_id: session_id.clone(),
        })
        .await
        .expect("client B attach must NOT be rejected as locked");
    let attachment_b = client_b
        .attachment_id()
        .expect("B attachment id")
        .to_owned();
    assert_ne!(
        attachment_a, attachment_b,
        "each attachment carries its own attachment id"
    );

    let subscription_a = client_a
        .subscribe(SESSION_SERVICE_ID)
        .await
        .expect("client A subscribe");
    let subscription_b = client_b
        .subscribe(SESSION_SERVICE_ID)
        .await
        .expect("client B subscribe");

    // A prompts; BOTH attachments observe the run's service updates.
    let prompt_id = client_a
        .start_call(
            RpcTarget::session(
                client_a.server_id(),
                session_id.clone(),
                attachment_a.clone(),
            ),
            SESSION_SERVICE_ID,
            "prompt",
            vec![serde_json::json!({"text": "broadcast me"})],
        )
        .await
        .expect("prompt call sent");
    let update_a = tokio::time::timeout(WAIT_BUDGET, client_a.next_update(&subscription_a))
        .await
        .expect("client A receives updates")
        .expect("client A update ok");
    assert!(
        !update_a.is_null(),
        "client A must receive the run's service updates: {update_a}"
    );
    let update_b = tokio::time::timeout(WAIT_BUDGET, client_b.next_update(&subscription_b))
        .await
        .expect("client B receives the same run's updates")
        .expect("client B update ok");
    assert!(
        !update_b.is_null(),
        "client B must receive the run's service updates: {update_b}"
    );
    let response = tokio::time::timeout(WAIT_BUDGET, client_a.wait_call(&prompt_id))
        .await
        .expect("prompt response arrives")
        .expect("prompt response ok");
    assert_eq!(response["command"], "prompt");

    // B detaches: A's attachment survives, and the session stays attached.
    client_b
        .request(Command::Detach {
            session_id: session_id.clone(),
        })
        .await
        .expect("client B detach");
    let snapshot = state.get_session(&session_id).await.expect("session live");
    assert!(
        snapshot.attached,
        "the session stays attached for the surviving attachment"
    );

    // One attachment leaving never disturbs the other's run.
    let follow_up = tokio::time::timeout(
        WAIT_BUDGET,
        client_a.request(Command::Prompt {
            session_id: session_id.clone(),
            text: "still here".to_owned(),
        }),
    )
    .await
    .expect("follow-up prompt completes")
    .expect("follow-up prompt ok");
    assert_eq!(follow_up["command"], "prompt");
    let _ = tokio::time::timeout(WAIT_BUDGET, client_a.next_update(&subscription_a))
        .await
        .expect("client A still receives updates after B left");

    drop(client_a);
    drop(client_b);
    tokio::time::timeout(WAIT_BUDGET, accepting_a)
        .await
        .expect("A connection task settles")
        .expect("accept task join")
        .expect("serve client A");
    tokio::time::timeout(WAIT_BUDGET, accepting_b)
        .await
        .expect("B connection task settles")
        .expect("accept task join")
        .expect("serve client B");
}

/// A disconnect of one of two attachments leaves the other fully functional
/// (upstream `disconnect` releases only the client's own attachment).
#[tokio::test]
async fn one_attachment_disconnecting_leaves_the_other_working() {
    let (registry, _provider) = recording_registry();
    let state =
        ServerState::with_providers(empty_snapshot(uuid::Uuid::new_v4().to_string()), registry);
    let server = Arc::new(
        Server::bind_with_state("127.0.0.1:0", state.clone())
            .await
            .expect("bind server"),
    );
    let address = server.local_addr().expect("local address");
    let accepting_a = {
        let server = server.clone();
        tokio::spawn(async move { server.accept_once().await })
    };

    let mut client_a = Client::connect(address).await.expect("client A handshake");
    let session_id = create_session(&mut client_a).await;
    client_a
        .request(Command::Attach {
            session_id: session_id.clone(),
        })
        .await
        .expect("client A attach");
    // Spawn B's acceptor only after A's connection is being served: with
    // both acceptors spawned up front, which one serves which connection is
    // unspecified, and joining `accepting_b` after dropping B can then wait
    // on A's still-open connection instead.
    let accepting_b = {
        let server = server.clone();
        tokio::spawn(async move { server.accept_once().await })
    };
    let mut client_b = Client::connect(address).await.expect("client B handshake");
    client_b
        .request(Command::Attach {
            session_id: session_id.clone(),
        })
        .await
        .expect("client B attach");

    // Hard-disconnect B mid-idle.
    drop(client_b);
    tokio::time::timeout(WAIT_BUDGET, accepting_b)
        .await
        .expect("B connection task settles")
        .expect("accept task join")
        .expect("serve client B");

    // A's attachment and prompting are unaffected.
    let follow_up = tokio::time::timeout(
        WAIT_BUDGET,
        client_a.request(Command::Prompt {
            session_id: session_id.clone(),
            text: "after peer disconnect".to_owned(),
        }),
    )
    .await
    .expect("prompt completes after the peer disconnect")
    .expect("prompt ok after the peer disconnect");
    assert_eq!(follow_up["command"], "prompt");
    let snapshot = state.get_session(&session_id).await.expect("session live");
    assert!(snapshot.attached, "A is still attached");

    drop(client_a);
    tokio::time::timeout(WAIT_BUDGET, accepting_a)
        .await
        .expect("A connection task settles")
        .expect("accept task join")
        .expect("serve client A");
}

// ---------------------------------------------------------------------------
// 3. Draining
// ---------------------------------------------------------------------------

/// Draining (upstream `isClosing`/`ServerDrainingError`): new creates and
/// attaches are rejected with the `server_draining` error code, and an
/// in-flight prompt run is aborted with the same code in its deferred
/// response.
#[tokio::test]
async fn draining_rejects_new_work_and_reports_on_inflight_runs() {
    let (gate_tx, gate_rx) = watch::channel(false);
    let entered = Arc::new(AtomicUsize::new(0));
    let registry = gated_registry(gate_rx, entered);
    let state =
        ServerState::with_providers(empty_snapshot(uuid::Uuid::new_v4().to_string()), registry);
    let server = Server::bind_with_state("127.0.0.1:0", state.clone())
        .await
        .expect("bind server");
    let address = server.local_addr().expect("local address");
    let accepting = tokio::spawn(async move { server.accept_once().await });

    let mut client = Client::connect(address).await.expect("hello handshake");
    let session_id = create_session(&mut client).await;
    client
        .request(Command::Attach {
            session_id: session_id.clone(),
        })
        .await
        .expect("attach");
    let attachment = client.attachment_id().expect("attachment id").to_owned();
    let prompt_id = client
        .start_call(
            RpcTarget::session(client.server_id(), session_id.clone(), attachment.clone()),
            SESSION_SERVICE_ID,
            "prompt",
            vec![serde_json::json!({"text": "long run"})],
        )
        .await
        .expect("prompt call sent");
    wait_for_phase(&state, &session_id, "turn").await;

    state.begin_drain().await;
    assert!(state.is_draining(), "draining flag set");

    // The in-flight run is aborted and its deferred response reports
    // `server_draining`.
    let error = tokio::time::timeout(WAIT_BUDGET, client.wait_call(&prompt_id))
        .await
        .expect("the aborted run's response must arrive")
        .expect_err("drained run reports an error");
    assert!(
        matches!(&error, ClientError::Server { code, .. } if code == "server_draining"),
        "expected server_draining, got {error:?}"
    );

    // New work is rejected with the same code.
    let attach_error = client
        .request(Command::Attach {
            session_id: session_id.clone(),
        })
        .await
        .expect_err("attach during drain is rejected");
    assert!(
        matches!(&attach_error, ClientError::Server { code, .. } if code == "server_draining"),
        "expected server_draining, got {attach_error:?}"
    );
    let create_error = client
        .request(Command::Create {
            cwd: None,
            name: None,
            model: None,
            thinking_level: None,
        })
        .await
        .expect_err("create during drain is rejected");
    assert!(
        matches!(&create_error, ClientError::Server { code, .. } if code == "server_draining"),
        "expected server_draining, got {create_error:?}"
    );

    drop(client);
    drop(gate_tx);
    tokio::time::timeout(WAIT_BUDGET, accepting)
        .await
        .expect("connection task settles")
        .expect("accept task join")
        .expect("serve client");
}

// ---------------------------------------------------------------------------
// 5. Virtual-model resume (CL30)
// ---------------------------------------------------------------------------

/// A provider exposing a SECOND model (`fake-other`) so a virtual registration
/// can route `virtual-x` to it; it records the model each stream call names.
struct TwoModelProvider {
    models: Mutex<Vec<String>>,
}

impl Provider for TwoModelProvider {
    fn id(&self) -> &str {
        "fake"
    }
    fn name(&self) -> &str {
        "Two-model fake provider"
    }
    fn get_models(&self) -> Vec<Model> {
        let mut other = fake_model();
        other.id = "fake-other".into();
        other.name = "Fake other".into();
        vec![fake_model(), other]
    }
    fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
        vec![ApiKeyAuth::new("fake key", vec!["FAKE_TEST_KEY".into()])]
    }
    fn stream(
        &self,
        model: &Model,
        _context: Context,
        _options: StreamOptions,
    ) -> AssistantMessageEventStream {
        self.models
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(model.id.clone());
        let stream = AssistantMessageEventStream::new();
        let pusher = stream.clone();
        let mut message = assistant(vec![text("ok")], StopReason::Stop);
        message.model = model.id.clone();
        pusher.push(AssistantMessageEventStream::done_event(
            TerminalReason::Stop,
            message,
        ));
        stream
    }
}

fn two_model_registry() -> (Arc<ProviderRegistry>, Arc<TwoModelProvider>) {
    let provider = Arc::new(TwoModelProvider {
        models: Mutex::new(Vec::new()),
    });
    let mut registry = ProviderRegistry::default();
    registry.set_provider(provider.clone());
    (Arc::new(registry), provider)
}

/// A persisted session that records a VIRTUAL selection (`model_change` +
/// `pi.virtual-model-state`) is resumed on attach, and the next prompt routes
/// through the restored registration to its PHYSICAL target.
///
/// The server resolves `projection.model` in `open_persisted_session`; the
/// constructor's own restore runs only when an agent is later built, so before
/// the fix the reopened record kept the create-time default model.
#[tokio::test]
async fn persisted_virtual_model_session_resumes_and_routes() {
    let temp = TempDir::new("virtual-resume");
    let session_id = "virtual-resume-session";
    let cwd = "C:\\virtual-resume";
    {
        let mut manager =
            pir::session_manager::SessionManager::create_with_id(cwd, &temp.0, session_id)
                .expect("create session file");
        manager
            .append_custom_entry(
                pir::ai::virtual_::VIRTUAL_MODEL_STATE_ENTRY,
                Some(serde_json::json!({
                    "provider": "fake",
                    "modelId": "virtual-x",
                    "state": {
                        "id": "virtual-x",
                        "provider": "fake",
                        "name": "Virtual X",
                        "physical": { "provider": "fake", "id": "fake-other" },
                    }
                })),
            )
            .expect("append virtual registration");
        manager
            .append_message(pir::ai::types::Message::User(pir::ai::types::UserMessage {
                role: pir::ai::types::UserRole::User,
                content: vec![text("carried over")],
                tools_added: None,
                tools_removed: None,
                timestamp: 0,
                sections: None,
            }))
            .expect("append user message");
        manager
            .append_model_change("fake", "virtual-x")
            .expect("record the virtual selection");
    }

    let (registry, provider) = two_model_registry();
    let state =
        ServerState::with_providers(empty_snapshot(uuid::Uuid::new_v4().to_string()), registry)
            .with_session_dir(temp.0.clone());
    let server = Server::bind_with_state("127.0.0.1:0", state.clone())
        .await
        .expect("bind server");
    let address = server.local_addr().expect("local address");
    let accepting = tokio::spawn(async move { server.accept_once().await });

    let mut client = Client::connect(address).await.expect("hello handshake");
    // Attach reopens the persisted session; the create-time model is never set
    // by this client — only the branch's recorded selection can supply it.
    client
        .request(Command::Attach {
            session_id: session_id.to_owned(),
        })
        .await
        .expect("attach reopens the persisted session");

    // The reopened record already carries the restored virtual selection.
    let snapshot = state.get_session(session_id).await.expect("live record");
    assert_eq!(
        (snapshot.model.provider.as_str(), snapshot.model.id.as_str()),
        ("fake", "virtual-x"),
        "the reopened record must carry the recorded virtual selection"
    );

    tokio::time::timeout(
        WAIT_BUDGET,
        client.request(Command::Prompt {
            session_id: session_id.to_owned(),
            text: "resume please".to_owned(),
        }),
    )
    .await
    .expect("prompt completes")
    .expect("prompt ok");

    assert_eq!(
        provider
            .models
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone(),
        vec!["fake-other".to_owned()],
        "the resumed virtual selection must route to its physical target"
    );

    drop(client);
    tokio::time::timeout(WAIT_BUDGET, accepting)
        .await
        .expect("connection task settles")
        .expect("accept task join")
        .expect("serve client");
}

// ---------------------------------------------------------------------------
// Lane D: registry lock scope, header-only listing, caps, off-thread attach,
// and lagged-broadcast resync
// ---------------------------------------------------------------------------

fn persisted_user_message(text: &str) -> Message {
    Message::User(UserMessage {
        role: UserRole::User,
        content: vec![text_block(text)],
        tools_added: None,
        tools_removed: None,
        timestamp: pir::ai::util::now_ms(),
        sections: None,
    })
}

fn text_block(text: &str) -> ContentBlock {
    ContentBlock::Text(TextContent {
        text: text.into(),
        text_signature: None,
    })
}

/// Create a real persisted session file with `turns` user/assistant pairs.
fn persist_session_file(dir: &std::path::Path, id: &str, turns: usize) -> String {
    let mut manager =
        SessionManager::create_with_id(".", dir, id).expect("create persisted session");
    for index in 0..turns {
        manager
            .append_message(persisted_user_message(&format!("user turn {index}")))
            .expect("append user message");
        manager
            .append_message(Message::Assistant(AssistantMessage {
                api: "fake".into(),
                provider: "fake".into(),
                model: "fake-model".into(),
                content: vec![text_block(&format!("assistant turn {index}"))],
                stop_reason: StopReason::Stop,
                timestamp: 1,
                ..Default::default()
            }))
            .expect("append assistant message");
    }
    id.to_owned()
}

/// D1: `session_metadata` snapshots the in-memory metadata under the registry
/// lock and releases it BEFORE the directory scan. A registry write (create)
/// must not wait for a stalled scan.
#[tokio::test]
async fn server_list_does_not_hold_the_registry_lock_across_the_scan() {
    let temp = TempDir::new("list-lock-scope");
    let persisted = persist_session_file(&temp.0, "persisted-lock", 2);
    let state = ServerState::new(empty_snapshot(uuid::Uuid::new_v4().to_string()))
        .with_session_dir(temp.0.clone());

    pir::session_manager::stall_next_session_header_listing_for_test(
        &temp.0,
        Duration::from_millis(800),
    );
    let scanning_state = state.clone();
    let scanning = tokio::spawn(async move { scanning_state.session_metadata().await });
    // Let the scan reach the blocking listing (the read guard is already gone).
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!scanning.is_finished(), "the listing must still be stalled");

    // A registry mutation needs the WRITE lock: it can only complete promptly
    // if the scan no longer holds the read guard.
    let started = Instant::now();
    state
        .create_session(
            "lock-scope-probe".to_owned(),
            None,
            None,
            None,
            None,
            pir::ai::util::now_ms(),
        )
        .await
        .expect("create while the directory scan is stalled");
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_millis(400),
        "the registry lock must be free during the scan, create took {elapsed:?}"
    );

    let listed = scanning.await.expect("metadata task join");
    assert!(
        listed.iter().any(|metadata| metadata.id == persisted),
        "the stalled scan still returns the persisted session"
    );
}

/// D1: the directory part of `session_metadata` uses the header-only listing
/// (`stop` seam proves the path; names still populate).
#[tokio::test]
async fn session_metadata_uses_the_header_only_listing() {
    let temp = TempDir::new("header-only-listing");
    let id = persist_session_file(&temp.0, "persisted-header", 3);
    // The latest session_info is the display name the listing must carry.
    {
        let mut manager = SessionManager::open(
            SessionManager::find_session_by_id(&temp.0, &id).expect("session file"),
        )
        .expect("reopen session");
        manager
            .append_session_info("Header Only Name")
            .expect("name");
    }

    let state = ServerState::new(empty_snapshot(uuid::Uuid::new_v4().to_string()))
        .with_session_dir(temp.0.clone());
    pir::session_manager::reset_session_header_list_file_reads();
    pir::session_manager::stall_next_session_header_listing_for_test(
        &temp.0,
        Duration::from_millis(300),
    );
    let started = Instant::now();
    let metadata = state.session_metadata().await;
    let elapsed = started.elapsed();
    assert!(
        elapsed >= Duration::from_millis(250),
        "the header-only listing seam must be the one that scanned (took {elapsed:?})"
    );
    assert!(
        pir::session_manager::session_header_list_file_reads() >= 1,
        "the header-only read counter must record the scan"
    );
    let entry = metadata
        .iter()
        .find(|metadata| metadata.id == id)
        .expect("persisted session listed");
    assert_eq!(
        entry.session_name.as_deref(),
        Some("Header Only Name"),
        "the session_info name comes from the header-only scan"
    );
}

/// D7: the session/subscription caps and the idle timeout are configurable
/// through the documented environment variables, with range guards.
#[test]
fn server_session_and_subscription_caps_are_configurable() {
    let configured = ServerSecurityConfig::from_env_lookup(|name| match name {
        "PIR_SERVER_MAX_SESSIONS" => Some("7".to_owned()),
        "PIR_SERVER_MAX_SUBSCRIPTIONS" => Some("3".to_owned()),
        "PIR_SERVER_IDLE_TIMEOUT_SECS" => Some("42".to_owned()),
        _ => None,
    });
    assert_eq!(configured.max_sessions, 7);
    assert_eq!(configured.max_subscriptions, 3);
    assert_eq!(configured.idle_timeout, Duration::from_secs(42));

    // Out-of-range / unparsable values keep the built-in defaults.
    let defaults = ServerSecurityConfig::from_env_lookup(|_| None);
    for bad in ["0", "-1", "not-a-number", "999999999999999999999"] {
        let rejected = ServerSecurityConfig::from_env_lookup(|_| Some(bad.to_owned()));
        assert_eq!(
            rejected.max_sessions, defaults.max_sessions,
            "bad cap {bad}"
        );
        assert_eq!(
            rejected.max_subscriptions, defaults.max_subscriptions,
            "bad subscription cap {bad}"
        );
        assert_eq!(
            rejected.idle_timeout, defaults.idle_timeout,
            "bad idle timeout {bad}"
        );
    }
}

/// D7: attaching a PERSISTED session goes through the same `max_sessions`
/// admission as `create`; a directory with more files cannot bypass the cap.
#[tokio::test]
async fn attaching_persisted_sessions_respects_the_session_cap() {
    let temp = TempDir::new("attach-cap");
    let first = persist_session_file(&temp.0, "cap-first", 1);
    let second = persist_session_file(&temp.0, "cap-second", 1);
    let security = ServerSecurityConfig {
        max_sessions: 1,
        ..ServerSecurityConfig::from_env_lookup(|_| None)
    };
    let state = ServerState::new(empty_snapshot(uuid::Uuid::new_v4().to_string()))
        .with_session_dir(temp.0.clone())
        .with_security(security);

    state
        .attach_session(&first, "conn-a")
        .await
        .expect("the first persisted session is admitted");
    let error = state
        .attach_session(&second, "conn-b")
        .await
        .expect_err("the second attach exceeds the cap");
    assert!(
        matches!(error, pir::server::SessionCommandError::Limit),
        "expected the structured Limit error, got {error:?}"
    );
    // The rejected session was not admitted.
    assert!(
        state.get_session(&second).await.is_none(),
        "a cap-rejected attach must not insert the record"
    );
}

/// D8: the persisted-session open + projection runs on the blocking pool, so
/// a slow open cannot park the async runtime.
#[tokio::test]
async fn attaching_a_large_persisted_session_does_not_block_the_runtime() {
    let temp = TempDir::new("attach-off-thread");
    let id = persist_session_file(&temp.0, "big-attach", 40);
    let state = ServerState::new(empty_snapshot(uuid::Uuid::new_v4().to_string()))
        .with_session_dir(temp.0.clone());
    pir::server::stall_next_attach_open_for_test(id.clone(), Duration::from_millis(800));

    let attach_state = state.clone();
    let attach_id = id.clone();
    let attach =
        tokio::spawn(async move { attach_state.attach_session(&attach_id, "conn-attach").await });

    // `#[tokio::test]` is a current-thread runtime: if the open ran inline on
    // the runtime worker this timer could not fire until it finished.
    let probe = tokio::time::timeout(
        Duration::from_millis(200),
        tokio::time::sleep(Duration::from_millis(40)),
    )
    .await;
    assert!(
        probe.is_ok(),
        "the runtime must stay responsive while the persisted open is stalled"
    );
    assert!(
        !attach.is_finished(),
        "the attach should still be inside the stalled blocking open"
    );

    let snapshot = attach.await.expect("attach task join").expect("attach");
    assert_eq!(snapshot.id, id);
    state
        .detach_session(&id, "conn-attach")
        .await
        .expect("detach after attach");
}

/// D2: a connection whose broadcast receiver lagged gets a FULL snapshot per
/// live session instead of silently skipping the lost delta stream.
#[tokio::test]
async fn server_progress_resyncs_after_a_lagged_broadcast_receiver() {
    let (gate_tx, gate) = watch::channel(false);
    let registry = gated_registry(gate, Arc::new(AtomicUsize::new(0)));
    let state =
        ServerState::with_providers(empty_snapshot(uuid::Uuid::new_v4().to_string()), registry);
    let server = Server::bind_with_state("127.0.0.1:0", state.clone())
        .await
        .expect("bind server");
    let address = server.local_addr().expect("local address");
    let accepting = tokio::spawn(async move { server.accept_once().await });

    let timeouts = ClientTimeouts {
        connect: Duration::from_secs(2),
        request: Duration::from_secs(20),
        update: Duration::from_secs(5),
    };
    let mut client = Client::connect_with_timeouts(address, timeouts)
        .await
        .expect("hello handshake");
    let session_id = create_session(&mut client).await;
    client
        .request(Command::Attach {
            session_id: session_id.clone(),
        })
        .await
        .expect("attach");
    let attachment = client.attachment_id().expect("attachment id").to_owned();
    let subscription = client
        .subscribe(SESSION_SERVICE_ID)
        .await
        .expect("subscribe");

    // Start a run that the gated provider never answers; the phase stays
    // `turn` so steering is accepted for as long as the flood lasts.
    client
        .start_call(
            RpcTarget::session(state.server_id(), session_id.as_str(), attachment.as_str()),
            SESSION_SERVICE_ID,
            "prompt",
            vec![serde_json::json!({"text": "flood"})],
        )
        .await
        .expect("prompt start");
    // The prompt handler only flips the phase to `turn` once it processes the
    // frame; steering before that is an `invalid` "no active prompt".
    wait_for_phase(&state, &session_id, "turn").await;

    // Wait until the run has attached its agent: steering then goes to the
    // agent inbox and does not grow the published snapshot.
    let mut attached = false;
    for _ in 0..400 {
        let _ = state
            .steer(&session_id, &attachment, "warm".to_owned())
            .await;
        if let Some(snapshot) = state.get_session(&session_id).await {
            if snapshot.queued_steer_count == 0 {
                attached = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(attached, "the run must attach its agent before the flood");

    // The client never reads while this runs: its frame channel and socket
    // back up, the update forwarder blocks, and the broadcast ring laps its
    // 1024 slots.
    for index in 0..4000 {
        state
            .steer(&session_id, &attachment, format!("steer {index}"))
            .await
            .expect("steer during the active run");
    }
    let expected = state
        .get_session(&session_id)
        .await
        .expect("live record")
        .revision;

    // As the client resumes reading, the forwarder observes `Lagged` and must
    // resend a snapshot whose sequence is the CURRENT one (far beyond every
    // buffered pre-lag frame).
    let mut newest = 0_i64;
    for _ in 0..2000 {
        let update = client
            .next_update(&subscription)
            .await
            .expect("update after the lag");
        if update.get("member").and_then(serde_json::Value::as_str) == Some("snapshot") {
            if let Some(sequence) = update.get("sequence").and_then(serde_json::Value::as_i64) {
                newest = newest.max(sequence);
                if sequence >= expected {
                    break;
                }
            }
        }
    }
    assert!(
        newest >= expected,
        "a lagged receiver must be resynced with the current snapshot \
         (newest {newest}, expected at least {expected})"
    );

    drop(client);
    let _ = tokio::time::timeout(WAIT_BUDGET, accepting).await;
    drop(gate_tx);
}

/// D9: the FIRST model request fails after a gate opens; later runs answer
/// with a plain stop. Every context is recorded so the test can prove the
/// steers queued during the failed run appear exactly once afterwards.
struct FailOnceGatedProvider {
    gate: watch::Receiver<bool>,
    calls: AtomicUsize,
    contexts: Mutex<Vec<Context>>,
}

impl Provider for FailOnceGatedProvider {
    fn id(&self) -> &str {
        "fake"
    }
    fn name(&self) -> &str {
        "Fail-once gated fake provider"
    }
    fn get_models(&self) -> Vec<Model> {
        vec![fake_model()]
    }
    fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
        vec![ApiKeyAuth::new("fake key", vec!["FAKE_TEST_KEY".into()])]
    }
    fn stream(
        &self,
        _model: &Model,
        context: Context,
        _options: StreamOptions,
    ) -> AssistantMessageEventStream {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        self.contexts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(context);
        let stream = AssistantMessageEventStream::new();
        let pusher = stream.clone();
        if call == 0 {
            let mut gate = self.gate.clone();
            tokio::spawn(async move {
                while !*gate.borrow() {
                    if tokio::time::timeout(Duration::from_millis(20), gate.changed())
                        .await
                        .is_err()
                    {
                        continue;
                    }
                }
                // No error_message: not classified as retryable, so the run
                // settles immediately with StopReason::Error.
                pusher.push(AssistantMessageEventStream::error_event(
                    ErrorReason::Error,
                    assistant(vec![text("first run failed")], StopReason::Error),
                ));
            });
        } else {
            pusher.push(AssistantMessageEventStream::done_event(
                TerminalReason::Stop,
                assistant(vec![text("ok")], StopReason::Stop),
            ));
        }
        stream
    }
}

/// D9: a failed run must neither lose steering queued during it nor replay
/// steering the agent already consumed. The run is held inside the provider
/// until both texts are queued, then fails; the next prompt must carry each
/// text exactly once and leave no session-level steering backlog.
#[tokio::test]
async fn a_failed_run_neither_loses_nor_replays_queued_steering() {
    let (gate_tx, gate) = watch::channel(false);
    let provider = Arc::new(FailOnceGatedProvider {
        gate,
        calls: AtomicUsize::new(0),
        contexts: Mutex::new(Vec::new()),
    });
    let mut registry = ProviderRegistry::default();
    registry.set_provider(provider.clone());
    let state = ServerState::with_providers(
        empty_snapshot(uuid::Uuid::new_v4().to_string()),
        Arc::new(registry),
    );
    let session_id = "d9-failed-steering".to_owned();
    state
        .create_session(session_id.clone(), None, None, fake_model_ref(), None, 1)
        .await
        .expect("create session");
    state
        .attach_session(&session_id, "conn-d9")
        .await
        .expect("attach session");

    let (finished_tx, finished_rx) = tokio::sync::oneshot::channel();
    state
        .prompt(
            &session_id,
            "conn-d9",
            "first prompt".to_owned(),
            finished_tx,
        )
        .await
        .expect("start the failing prompt");
    wait_for_phase(&state, &session_id, "turn").await;

    // The provider is gated, so the run cannot settle while these land. A
    // pre-attach steer takes the record-queue path; a post-attach one goes
    // straight to the agent inbox. Both must end up delivered exactly once.
    state
        .steer(&session_id, "conn-d9", "D9-ALPHA".to_owned())
        .await
        .expect("first steer");
    state
        .steer(&session_id, "conn-d9", "D9-BETA".to_owned())
        .await
        .expect("second steer");

    gate_tx.send(true).expect("release the failing request");
    let outcome = tokio::time::timeout(WAIT_BUDGET, finished_rx)
        .await
        .expect("the failed prompt settles")
        .expect("finished channel");
    assert!(
        matches!(outcome, Err(pir::server::SessionCommandError::Provider(_))),
        "the first run must fail, got {outcome:?}"
    );
    assert_eq!(
        state
            .get_session(&session_id)
            .await
            .expect("live record")
            .queued_steer_count,
        0,
        "a failed run must not leave a session-level steering backlog"
    );

    let (finished_tx, finished_rx) = tokio::sync::oneshot::channel();
    state
        .prompt(
            &session_id,
            "conn-d9",
            "second prompt".to_owned(),
            finished_tx,
        )
        .await
        .expect("start the follow-up prompt");
    let outcome = tokio::time::timeout(WAIT_BUDGET, finished_rx)
        .await
        .expect("the follow-up prompt settles")
        .expect("finished channel");
    assert!(
        outcome.is_ok(),
        "the follow-up prompt must succeed: {outcome:?}"
    );

    assert_eq!(
        provider.calls.load(Ordering::SeqCst),
        2,
        "exactly one model request per run (the failed one must not retry)"
    );
    let contexts = provider
        .contexts
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let last = contexts
        .last()
        .expect("the follow-up run reached the provider");
    let rendered = serde_json::to_string(last).expect("serialize the last context");
    for steer in ["D9-ALPHA", "D9-BETA"] {
        assert_eq!(
            rendered.matches(steer).count(),
            1,
            "steering {steer} must appear exactly once in the follow-up context: {rendered}"
        );
    }
}

/// D9: steering restored by a provider-missing run is OLDER than a steer that
/// lands while the next prompt's agent is attaching, so it must reach the
/// agent inbox first (prompt order, not delivery order). Neither text may be
/// replayed: each appears exactly once in the follow-up request.
#[tokio::test]
async fn restored_steering_stays_ahead_of_the_attach_window() {
    let (registry, provider) = recording_registry();
    let state =
        ServerState::with_providers(empty_snapshot(uuid::Uuid::new_v4().to_string()), registry);
    let session_id = "d9-steering-order".to_owned();
    // A model the registry does not know: the first run never attaches an
    // agent, which is exactly the provider-missing path that restores the
    // session-level queue.
    let missing_model = ModelRef {
        provider: "absent".to_owned(),
        id: "absent-model".to_owned(),
    };
    state
        .create_session(session_id.clone(), None, None, Some(missing_model), None, 1)
        .await
        .expect("create session");
    state
        .attach_session(&session_id, "conn-d9")
        .await
        .expect("attach session");

    let (finished_tx, finished_rx) = tokio::sync::oneshot::channel();
    state
        .prompt(
            &session_id,
            "conn-d9",
            "missing provider".to_owned(),
            finished_tx,
        )
        .await
        .expect("start the provider-missing prompt");
    // The phase is already `turn` and no agent can be built, so this steer
    // takes the session-queue path.
    state
        .steer(&session_id, "conn-d9", "D9-OLDER".to_owned())
        .await
        .expect("steer during the provider-missing run");
    let outcome = tokio::time::timeout(WAIT_BUDGET, finished_rx)
        .await
        .expect("the provider-missing prompt settles")
        .expect("finished channel");
    assert!(
        matches!(outcome, Err(pir::server::SessionCommandError::Provider(_))),
        "the first run must fail on the missing provider, got {outcome:?}"
    );
    assert_eq!(
        state
            .get_session(&session_id)
            .await
            .expect("live record")
            .queued_steer_count,
        1,
        "the provider-missing run must keep the steering it never delivered"
    );
    state
        .set_model(
            &session_id,
            "conn-d9",
            fake_model_ref().expect("fake model ref"),
        )
        .await
        .expect("install a registered model");

    let (finished_tx, finished_rx) = tokio::sync::oneshot::channel();
    state
        .prompt(
            &session_id,
            "conn-d9",
            "follow-up prompt".to_owned(),
            finished_tx,
        )
        .await
        .expect("start the follow-up prompt");
    // No intervening await: the spawned run has not polled yet, so
    // `record.agent` is still None and this text lands in the attach window,
    // AFTER the text `prompt()` restored from the session queue.
    state
        .steer(&session_id, "conn-d9", "D9-NEWER".to_owned())
        .await
        .expect("steer while the agent is attaching");
    let outcome = tokio::time::timeout(WAIT_BUDGET, finished_rx)
        .await
        .expect("the follow-up prompt settles")
        .expect("finished channel");
    assert!(
        outcome.is_ok(),
        "the follow-up prompt must succeed: {outcome:?}"
    );

    let contexts = provider
        .contexts
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let first = contexts
        .first()
        .expect("the follow-up run reached the provider");
    let rendered = serde_json::to_string(first).expect("serialize the request context");
    let older = rendered
        .find("D9-OLDER")
        .expect("the restored steering must reach the follow-up request");
    let newer = rendered
        .find("D9-NEWER")
        .expect("the attach-window steering must reach the follow-up request");
    assert!(
        older < newer,
        "steering queued before the prompt must precede the attach-window text: {rendered}"
    );
    for steer in ["D9-OLDER", "D9-NEWER"] {
        assert_eq!(
            rendered.matches(steer).count(),
            1,
            "steering {steer} must appear exactly once in the follow-up context: {rendered}"
        );
    }
}
