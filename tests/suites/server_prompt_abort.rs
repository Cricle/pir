//! Abort/disconnect regressions for the server's spawned prompt runs.
//!
//! These cover the exact path the abort select loop in
//! `ServerState::run_prompt` drives: a prompt gated behind a fake provider
//! that never finishes on its own, an `abort` (or a disconnect) while the run
//! is in flight, then the terminal response must be delivered and the session
//! must recover for follow-up prompts. Before the one-shot abort fix this
//! path hot-spinned: once the abort flag was set, the biased select loop
//! re-polled the always-ready abort waiter instead of the run future, so the
//! response was never sent and every later prompt failed with `busy`.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use pir::ai::auth::ApiKeyAuth;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::types::{
    AssistantMessage, ContentBlock, Context, ErrorReason, Message, Model, ModelCost,
    ModelCostRates, StopReason, StreamOptions, TerminalReason, TextContent,
};
use pir::client::{Client, ClientError};
use pir::protocol::{Command, ModelRef, RpcTarget, SESSION_SERVICE_ID};
use pir::server::{empty_snapshot, Server, ServerSecurityConfig, ServerState};
use tokio::sync::watch;

const WAIT_BUDGET: Duration = Duration::from_secs(10);

/// Scripted model catalog entry (same shape as `tests/agent_session.rs`).
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

/// A provider whose stream only terminates when the shared gate opens or the
/// run's abort signal fires (whichever comes first). Without a gate open and
/// without an abort the run stays in flight forever, which is exactly the
/// state the abort/disconnect regressions need.
struct GatedProvider {
    gate: watch::Receiver<bool>,
    entered: Arc<AtomicUsize>,
    /// Every provider request context, in call order (steering assertions).
    contexts: Arc<std::sync::Mutex<Vec<Context>>>,
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
        context: Context,
        options: StreamOptions,
    ) -> AssistantMessageEventStream {
        self.entered.fetch_add(1, Ordering::SeqCst);
        self.contexts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(context);
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
                // Poll with a timeout so a gate change and an abort landing
                // together are both observed on the next iteration.
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

fn gated_registry_with_contexts(
    gate: watch::Receiver<bool>,
    entered: Arc<AtomicUsize>,
) -> (Arc<ProviderRegistry>, Arc<std::sync::Mutex<Vec<Context>>>) {
    let contexts = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(GatedProvider {
        gate,
        entered,
        contexts: Arc::clone(&contexts),
    }));
    (Arc::new(registry), contexts)
}

fn gated_registry(gate: watch::Receiver<bool>, entered: Arc<AtomicUsize>) -> Arc<ProviderRegistry> {
    gated_registry_with_contexts(gate, entered).0
}

/// Bind a server whose sessions run against the gated provider.
async fn bind_gated_server(gate: watch::Receiver<bool>) -> (Server, Arc<AtomicUsize>) {
    let (server, entered, _contexts) = bind_gated_server_recording(gate).await;
    (server, entered)
}

/// [`bind_gated_server`] plus the recorded provider request contexts.
async fn bind_gated_server_recording(
    gate: watch::Receiver<bool>,
) -> (
    Server,
    Arc<AtomicUsize>,
    Arc<std::sync::Mutex<Vec<Context>>>,
) {
    let entered = Arc::new(AtomicUsize::new(0));
    let (registry, contexts) = gated_registry_with_contexts(gate, Arc::clone(&entered));
    let state =
        ServerState::with_providers(empty_snapshot(uuid::Uuid::new_v4().to_string()), registry)
            .with_security(pir::server::ServerSecurityConfig::default());
    let server = Server::bind_with_state_and_security(
        "127.0.0.1:0",
        state,
        ServerSecurityConfig::unauthenticated_local(),
    )
    .await
    .expect("bind gated test server");
    (server, entered, contexts)
}

/// The text of the last user message in a provider request context.
fn last_user_text(context: &Context) -> Option<String> {
    context.messages.iter().rev().find_map(|message| {
        let Message::User(user) = message else {
            return None;
        };
        let text: String = user
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");
        (!text.is_empty()).then_some(text)
    })
}

/// Attach one client and return (session id, attachment id). The session is
/// created against the gated fake provider so a prompt actually runs.
async fn create_and_attach(client: &mut Client) -> (String, String) {
    let created = client
        .request(Command::Create {
            cwd: None,
            name: None,
            model: Some(ModelRef {
                provider: "fake".to_owned(),
                id: "fake-model".to_owned(),
            }),
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
    let attachment = client
        .attachment_id()
        .expect("attachment id leased at attach")
        .to_owned();
    (session_id, attachment)
}

/// Poll the registry until the session's phase matches (the prompt flips the
/// phase before the run task starts polling the provider).
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

/// Wait until the gated provider's stream has been entered. The phase flips
/// before the prompt task is polled, and the run now lives on its own
/// large-stack thread (MCP2), so the first provider poll can land after the
/// phase is visible; polling the counter keeps the test deterministic without
/// weakening the abort/disconnect assertions.
async fn wait_for_provider_entered(entered: &AtomicUsize) {
    for _ in 0..2000 {
        if entered.load(Ordering::SeqCst) >= 1 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the gated provider stream never started");
}

/// Regression: an abort while a prompt is in flight must deliver the
/// terminal response (no hot spin), return the phase to idle, and leave the
/// session usable for a follow-up prompt.
#[tokio::test]
async fn abort_during_prompt_delivers_response_and_session_recovers() {
    let (gate_tx, gate_rx) = watch::channel(false);
    let (server, entered) = bind_gated_server(gate_rx).await;
    let state = server.state().clone();
    let address = server.local_addr().expect("local address");
    let accepting = tokio::spawn(async move { server.accept_once().await });

    let mut client = Client::connect(address).await.expect("hello handshake");
    let (session_id, attachment) = create_and_attach(&mut client).await;
    let target = RpcTarget::session(client.server_id(), session_id.clone(), attachment.clone());

    // Start the prompt without waiting: the run is gated and stays in flight.
    let prompt_id = client
        .start_call(
            target,
            SESSION_SERVICE_ID,
            "prompt",
            vec![serde_json::json!({"text": "hi"})],
        )
        .await
        .expect("prompt call sent");
    wait_for_phase(&state, &session_id, "turn").await;
    wait_for_provider_entered(&entered).await;

    // Abort through the shared state (the same call the `abort`/`cancel`
    // frames route to). This is the point where the biased select loop used
    // to hot-spin and never answer.
    state
        .abort(&session_id, &attachment)
        .await
        .expect("abort in-flight prompt");

    let response = tokio::time::timeout(WAIT_BUDGET, client.wait_call(&prompt_id))
        .await
        .expect("prompt response must arrive after abort (no hot spin)")
        .expect("prompt response ok");
    assert_eq!(response["command"], "prompt");

    let snapshot = state
        .get_session(&session_id)
        .await
        .expect("session still registered");
    assert_eq!(
        snapshot.phase, "idle",
        "abort must return the phase to idle"
    );

    // A follow-up prompt must succeed (the old wedge answered `busy`
    // forever). Open the gate so the run can finish.
    gate_tx.send(true).expect("gate open");
    let follow_up = tokio::time::timeout(
        WAIT_BUDGET,
        client.request(Command::Prompt {
            session_id: session_id.clone(),
            text: "again".to_owned(),
        }),
    )
    .await
    .expect("follow-up prompt completes")
    .expect("follow-up prompt ok");
    assert_eq!(follow_up["command"], "prompt");
    assert_eq!(
        state
            .get_session(&session_id)
            .await
            .expect("session snapshot")
            .phase,
        "idle"
    );

    drop(client);
    tokio::time::timeout(WAIT_BUDGET, accepting)
        .await
        .expect("connection task settles")
        .expect("accept task join")
        .expect("serve client");
}

/// A18-F02: a mid-run `steer` must reach the live agent inbox, must be
/// reflected in the returned snapshot's `queuedSteer`/`queuedSteerCount`,
/// and must be delivered as the next model request in the same run.
#[tokio::test]
async fn steer_during_prompt_reaches_agent_and_snapshot_reports_it() {
    let (gate_tx, gate_rx) = watch::channel(false);
    let (server, entered, contexts) = bind_gated_server_recording(gate_rx).await;
    let state = server.state().clone();
    let address = server.local_addr().expect("local address");
    let accepting = tokio::spawn(async move { server.accept_once().await });

    let mut client = Client::connect(address).await.expect("hello handshake");
    let (session_id, attachment) = create_and_attach(&mut client).await;
    let target = RpcTarget::session(client.server_id(), session_id.clone(), attachment);

    let prompt_id = client
        .start_call(
            target,
            SESSION_SERVICE_ID,
            "prompt",
            vec![serde_json::json!({"text": "hi"})],
        )
        .await
        .expect("prompt call sent");
    wait_for_phase(&state, &session_id, "turn").await;
    wait_for_provider_entered(&entered).await;

    // The steer lands while the first provider request is still gated. The
    // response snapshot must expose the queued text instead of answering
    // `ok` with an empty queue.
    let steered = client
        .request(Command::Steer {
            session_id: session_id.clone(),
            text: "steer note alpha".to_owned(),
        })
        .await
        .expect("steer response");
    assert_eq!(steered["command"], "steer");
    assert_eq!(steered["session"]["phase"], "turn");
    assert_eq!(steered["session"]["queuedSteerCount"], 1);
    assert_eq!(
        steered["session"]["queuedSteer"][0]["content"][0]["text"],
        "steer note alpha"
    );
    // The same queue is visible through a direct registry snapshot.
    let snapshot = state
        .get_session(&session_id)
        .await
        .expect("session still registered");
    assert_eq!(snapshot.queued_steer_count, 1);
    match &snapshot.queued_steer[0].content[0] {
        pir::protocol::schemas::UserContent::Text(text) => {
            assert_eq!(text.text, "steer note alpha");
        }
        other => panic!("queued steer must be a text item, got {other:?}"),
    }

    // Let the first request finish; the queued steering must be delivered as
    // the next request of the same run.
    gate_tx.send(true).expect("gate open");
    let response = tokio::time::timeout(WAIT_BUDGET, client.wait_call(&prompt_id))
        .await
        .expect("steered prompt response must arrive")
        .expect("steered prompt response ok");
    assert_eq!(response["command"], "prompt");
    assert_eq!(response["session"]["phase"], "idle");
    assert_eq!(response["session"]["queuedSteerCount"], 0);

    for _ in 0..2000 {
        if entered.load(Ordering::SeqCst) >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        entered.load(Ordering::SeqCst) >= 2,
        "queued steering must start a follow-up provider request"
    );
    let contexts = contexts
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert!(contexts.len() >= 2, "expected two provider requests");
    assert_eq!(
        last_user_text(&contexts[1]).as_deref(),
        Some("steer note alpha"),
        "the second provider request must carry the steer text"
    );

    drop(client);
    tokio::time::timeout(WAIT_BUDGET, accepting)
        .await
        .expect("connection task settles")
        .expect("accept task join")
        .expect("serve client");
}

/// Regression: a disconnect while a prompt is in flight aborts the run and
/// reaps the session; the server keeps serving afterwards.
#[tokio::test]
async fn disconnect_during_prompt_aborts_run_and_reaps_session() {
    let (_gate_tx, gate_rx) = watch::channel(false);
    let (server, entered) = bind_gated_server(gate_rx).await;
    let state = server.state().clone();
    let address = server.local_addr().expect("local address");
    let mut accepting = tokio::spawn(async move { server.accept_once().await });

    let mut client = Client::connect(address).await.expect("hello handshake");
    let (session_id, attachment) = create_and_attach(&mut client).await;

    let target = RpcTarget::session(client.server_id(), session_id.clone(), attachment);
    let _prompt_id = client
        .start_call(
            target,
            SESSION_SERVICE_ID,
            "prompt",
            vec![serde_json::json!({"text": "hi"})],
        )
        .await
        .expect("prompt call sent");
    wait_for_phase(&state, &session_id, "turn").await;
    // The phase flips before the run thread polls the provider; wait for the
    // stream entry so the disconnect below really lands mid-run (same rule
    // as the abort regression above).
    wait_for_provider_entered(&entered).await;

    // Drop the connection mid-run: the server must abort the in-flight run
    // (the gated stream would otherwise hold it forever) and remove the
    // session owned by the disconnected client.
    drop(client);
    // The serve loop must end promptly (the disconnect aborts the in-flight
    // run; the teardown never blocks on run behavior).
    tokio::time::timeout(WAIT_BUDGET, &mut accepting)
        .await
        .expect("serve loop must not wedge after a mid-run disconnect")
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
    assert!(reaped, "disconnected client's session must be reaped");

    // The server keeps working for fresh clients (the run task's select loop
    // used to leave a spinning zombie behind).
    let (_gate_tx2, gate_rx2) = watch::channel(true);
    let (server2, _entered2) = bind_gated_server(gate_rx2).await;
    let address2 = server2.local_addr().expect("local address 2");
    let accepting2 = tokio::spawn(async move { server2.accept_once().await });
    let mut client2 = Client::connect(address2).await.expect("second handshake");
    let (session_id2, _attachment2) = create_and_attach(&mut client2).await;
    let follow_up = tokio::time::timeout(
        WAIT_BUDGET,
        client2.request(Command::Prompt {
            session_id: session_id2,
            text: "after disconnect".to_owned(),
        }),
    )
    .await
    .expect("next prompt completes")
    .expect("next prompt ok");
    assert_eq!(follow_up["command"], "prompt");
    drop(client2);
    tokio::time::timeout(WAIT_BUDGET, accepting2)
        .await
        .expect("connection task settles")
        .expect("accept task join")
        .expect("serve client 2");
}

/// The default run must surface provider errors instead of faking success
/// (guards the terminal-response mapping used by the abort regression).
#[tokio::test]
async fn provider_error_maps_to_internal_error_response() {
    // A registry with NO provider: the run finishes with a Provider error
    // response instead of hanging, and the phase returns to idle.
    let state = ServerState::with_providers(
        empty_snapshot(uuid::Uuid::new_v4().to_string()),
        Arc::new(ProviderRegistry::default()),
    )
    .with_security(pir::server::ServerSecurityConfig::default());
    let server = Server::bind_with_state_and_security(
        "127.0.0.1:0",
        state,
        ServerSecurityConfig::unauthenticated_local(),
    )
    .await
    .expect("bind test server");
    let address = server.local_addr().expect("local address");
    let accepting = tokio::spawn(async move { server.accept_once().await });
    let mut client = Client::connect(address).await.expect("hello handshake");
    let (session_id, _attachment) = create_and_attach(&mut client).await;
    let error = tokio::time::timeout(
        WAIT_BUDGET,
        client.request(Command::Prompt {
            session_id,
            text: "no provider".to_owned(),
        }),
    )
    .await
    .expect("prompt error response must arrive")
    .expect_err("missing provider must fail the prompt");
    assert!(
        matches!(error, ClientError::Server { ref code, .. } if code == "internal_error"),
        "expected internal_error, got {error:?}"
    );
    drop(client);
    tokio::time::timeout(WAIT_BUDGET, accepting)
        .await
        .expect("connection task settles")
        .expect("accept task join")
        .expect("serve client");
}
