//! Model-facing background-call control tools (`background_status` /
//! `background_abort`): the provider-visible surface over a host's background
//! registry.
//!
//! Coverage:
//! 1. A bare `ToolContext` (`session_env: None`): the tools answer the
//!    registry-less text (a legal host shape, never an error), and
//!    `background_abort` still enforces its argument contract.
//! 2. An injected [`BackgroundControl`]: the status JSON shape
//!    (`callId`/`toolName`/`parentToolCallId`/`count`) and the abort
//!    semantics (single id, unknown id as a successful no-op, `all: true`).
//! 3. Registration/visibility: both tools are registered, model-visible
//!    (`Direct` exposure), default-active, and listed in `CORE_TOOL_NAMES`.
//! 4. A real `AgentSession` with a scripted provider: a `background: true`
//!    bash-shaped call is visible through `background_status` and is actually
//!    cancelled through `background_abort` (the session adapter is wired).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::json;

use pir::agent::agent_loop::{AgentEvent, ProviderRequest};
use pir::agent::tool_batch::ToolResult;
use pir::agent::tool_execution::{
    AgentTool, BackgroundCallInfo, BackgroundControl, BoxFuture, ToolContext, ToolExposure,
    ToolRegistry, ToolSessionEnv,
};
use pir::agent_session::{AgentSession, AgentSessionConfig, EventHandler, SharedEventStreamFn};
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::ProviderRegistry;
use pir::ai::types::{
    AssistantMessage, ContentBlock, Message, Model, ModelCost, ModelCostRates, StopReason,
    TerminalReason, TextContent, ThinkingLevel, ToolCall,
};
use pir::coding_agent::tools::{register_core_tools, tool_definitions, CORE_TOOL_NAMES};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn fake_model() -> Model {
    Model {
        id: "bg-tools-test-model".into(),
        name: "Background tools test model".into(),
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

fn context(session_env: Option<Arc<ToolSessionEnv>>) -> ToolContext {
    ToolContext {
        cwd: std::env::temp_dir(),
        signal: None,
        nested: None,
        session_env,
        held_mutation_keys: None,
        on_update: None,
        background: false,
    }
}

fn env_with(background: Option<Arc<dyn BackgroundControl>>) -> Arc<ToolSessionEnv> {
    Arc::new(ToolSessionEnv {
        provider_registry: Arc::new(ProviderRegistry::default()),
        model: fake_model(),
        thinking: ThinkingLevel::Off,
        session_id: None,
        session_file: None,
        base_tools: Arc::new(ToolRegistry::new()),
        has_ui: false,
        project_trusted: true,
        user_agents_dir: None,
        agent_dir: None,
        credential: None,
        on_update: None,
        live_tools: None,
        active_override: None,
        background,
    })
}

fn registry() -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    register_core_tools(&mut registry);
    registry
}

async fn run_status(ctx: &ToolContext) -> ToolResult {
    let tool = registry().get("background_status").expect("registered");
    tool.run(&json!({}), ctx).await
}

async fn run_abort(args: serde_json::Value, ctx: &ToolContext) -> ToolResult {
    let tool = registry().get("background_abort").expect("registered");
    tool.run(&args, ctx).await
}

/// A local control surface: enough to pin the tools' projection and abort
/// semantics without the session/print hosts.
#[derive(Default)]
struct FakeControl {
    calls: Mutex<Vec<BackgroundCallInfo>>,
    aborted: Mutex<Vec<String>>,
    abort_all_count: AtomicUsize,
}

impl FakeControl {
    fn with_calls(calls: Vec<BackgroundCallInfo>) -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(calls),
            aborted: Mutex::new(Vec::new()),
            abort_all_count: AtomicUsize::new(0),
        })
    }
}

impl BackgroundControl for FakeControl {
    fn calls(&self) -> Vec<BackgroundCallInfo> {
        self.calls.lock().unwrap().clone()
    }

    fn abort(&self, call_id: &str) -> bool {
        let mut calls = self.calls.lock().unwrap();
        let before = calls.len();
        calls.retain(|call| call.call_id != call_id);
        if calls.len() != before {
            self.aborted.lock().unwrap().push(call_id.to_owned());
            true
        } else {
            false
        }
    }

    fn abort_all(&self) -> usize {
        let count = self.calls.lock().unwrap().len();
        self.calls.lock().unwrap().clear();
        self.abort_all_count.store(count, Ordering::SeqCst);
        count
    }
}

// ---------------------------------------------------------------------------
// 1. Registry-less hosts
// ---------------------------------------------------------------------------

#[tokio::test]
async fn bare_context_reports_no_registry_and_keeps_the_argument_contract() {
    let ctx = context(None);

    let status = run_status(&ctx).await;
    assert!(!status.is_error, "a registry-less host is not an error");
    assert_eq!(status.content, "No background call registry in this host.");

    let missing_selector = run_abort(json!({}), &ctx).await;
    assert!(
        missing_selector.is_error,
        "background_abort without a selector must be an argument error"
    );
    assert_eq!(
        missing_selector.content,
        "background_abort requires either \"callId\" or \"all\": true"
    );

    let by_id = run_abort(json!({ "callId": "call-1" }), &ctx).await;
    assert!(!by_id.is_error);
    assert_eq!(by_id.content, "No background call registry in this host.");

    let all = run_abort(json!({ "all": true }), &ctx).await;
    assert!(!all.is_error);
    assert_eq!(all.content, "No background call registry in this host.");
}

#[tokio::test]
async fn env_without_background_handle_is_also_registry_less() {
    let ctx = context(Some(env_with(None)));
    let status = run_status(&ctx).await;
    assert!(!status.is_error);
    assert_eq!(status.content, "No background call registry in this host.");
}

// ---------------------------------------------------------------------------
// 2. Injected control surface
// ---------------------------------------------------------------------------

#[tokio::test]
async fn status_projects_every_in_flight_call() {
    let control = FakeControl::with_calls(vec![
        BackgroundCallInfo {
            call_id: "call-a".into(),
            tool_name: "bash".into(),
            parent_call_id: None,
        },
        BackgroundCallInfo {
            call_id: "call-b".into(),
            tool_name: "read".into(),
            parent_call_id: Some("call-parent".into()),
        },
    ]);
    let ctx = context(Some(env_with(Some(control))));

    let result = run_status(&ctx).await;
    assert!(!result.is_error);
    let parsed: serde_json::Value = serde_json::from_str(&result.content).expect("status is JSON");
    assert_eq!(parsed["count"], json!(2));
    assert_eq!(parsed["calls"][0]["callId"], json!("call-a"));
    assert_eq!(parsed["calls"][0]["toolName"], json!("bash"));
    assert!(
        parsed["calls"][0]["parentToolCallId"].is_null(),
        "a model-issued call has no parent: {parsed}"
    );
    assert_eq!(parsed["calls"][1]["callId"], json!("call-b"));
    assert_eq!(parsed["calls"][1]["toolName"], json!("read"));
    assert_eq!(parsed["calls"][1]["parentToolCallId"], json!("call-parent"));
}

#[tokio::test]
async fn abort_by_id_is_a_no_op_for_unknown_ids() {
    let control = FakeControl::with_calls(vec![BackgroundCallInfo {
        call_id: "call-a".into(),
        tool_name: "bash".into(),
        parent_call_id: None,
    }]);
    let ctx = context(Some(env_with(Some(control.clone()))));

    let aborted = run_abort(json!({ "callId": "call-a" }), &ctx).await;
    assert!(!aborted.is_error);
    let parsed: serde_json::Value = serde_json::from_str(&aborted.content).unwrap();
    assert_eq!(parsed["aborted"], json!(1));
    assert_eq!(parsed["callId"], json!("call-a"));

    // Already settled / never known: a successful no-op, matching the RPC
    // `abort_background` contract.
    let unknown = run_abort(json!({ "callId": "call-a" }), &ctx).await;
    assert!(!unknown.is_error);
    let parsed: serde_json::Value = serde_json::from_str(&unknown.content).unwrap();
    assert_eq!(parsed["aborted"], json!(0));
    assert_eq!(parsed["callId"], json!("call-a"));
}

#[tokio::test]
async fn abort_all_reports_the_cancelled_count() {
    let control = FakeControl::with_calls(vec![
        BackgroundCallInfo {
            call_id: "call-a".into(),
            tool_name: "bash".into(),
            parent_call_id: None,
        },
        BackgroundCallInfo {
            call_id: "call-b".into(),
            tool_name: "grep".into(),
            parent_call_id: None,
        },
    ]);
    let ctx = context(Some(env_with(Some(control))));

    let result = run_abort(json!({ "all": true }), &ctx).await;
    assert!(!result.is_error);
    let parsed: serde_json::Value = serde_json::from_str(&result.content).unwrap();
    assert_eq!(parsed["aborted"], json!(2));

    // The registry is empty now: a second sweep reports zero.
    let again = run_abort(json!({ "all": true }), &ctx).await;
    let parsed: serde_json::Value = serde_json::from_str(&again.content).unwrap();
    assert_eq!(parsed["aborted"], json!(0));
}

// ---------------------------------------------------------------------------
// 3. Registration and model visibility
// ---------------------------------------------------------------------------

#[test]
fn both_tools_are_registered_model_visible_and_default_active() {
    let registry = registry();
    for name in ["background_status", "background_abort"] {
        assert!(
            registry.get(name).is_some(),
            "{name} must be registered by register_core_tools"
        );
        assert_eq!(
            registry.exposure_of(name),
            ToolExposure::Direct,
            "{name} must be model-visible, not hidden/model-only"
        );
        assert!(
            CORE_TOOL_NAMES.contains(&name),
            "{name} must be part of CORE_TOOL_NAMES"
        );
    }
    let active = registry.active_names(None);
    for name in ["background_status", "background_abort"] {
        assert!(
            active.iter().any(|active| active == name),
            "{name} must be active by default: {active:?}"
        );
    }
    let declared: Vec<String> = tool_definitions(&registry)
        .into_iter()
        .map(|tool| tool.name)
        .collect();
    for name in ["background_status", "background_abort"] {
        assert!(
            declared.iter().any(|declared| declared == name),
            "{name} must appear in the model-facing tool definitions: {declared:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// 4. Real session host
// ---------------------------------------------------------------------------

/// A probe that parks until its own abort signal fires (the detached-call
/// fixture pattern from `tests/suites/background_tool_calls.rs`).
#[derive(Default)]
struct ParkingProbe {
    entered: Arc<AtomicUsize>,
    aborted: Arc<AtomicUsize>,
}

impl AgentTool for ParkingProbe {
    fn name(&self) -> &'static str {
        "parking-probe"
    }

    fn description(&self) -> &'static str {
        "parks until its own abort signal fires"
    }

    fn parameters(&self) -> Option<serde_json::Value> {
        Some(json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        }))
    }

    fn run(&self, _args: &serde_json::Value, ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        self.entered.fetch_add(1, Ordering::SeqCst);
        let signal = ctx.signal.clone();
        let aborted = Arc::clone(&self.aborted);
        Box::pin(async move {
            match signal {
                Some(signal) => signal.cancelled().await,
                None => std::future::pending::<()>().await,
            }
            aborted.fetch_add(1, Ordering::SeqCst);
            ToolResult::ok("parking-probe aborted")
        })
    }
}

fn tool_call(id: &str, name: &str, args: serde_json::Value) -> ContentBlock {
    ContentBlock::ToolCall(ToolCall {
        id: id.to_owned(),
        name: name.to_owned(),
        arguments: args
            .as_object()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .collect(),
        thought_signature: None,
        namespace: None,
        arguments_error: None,
    })
}

fn assistant(content: Vec<ContentBlock>, stop_reason: StopReason) -> AssistantMessage {
    AssistantMessage {
        api: "fake".into(),
        provider: "fake".into(),
        model: "bg-tools-test-model".into(),
        content,
        stop_reason,
        timestamp: 1,
        ..Default::default()
    }
}

fn text_turn(text: &str) -> AssistantMessage {
    assistant(
        vec![ContentBlock::Text(TextContent {
            text: text.to_owned(),
            text_signature: None,
        })],
        StopReason::Stop,
    )
}

/// Four scripted turns: start a background call, inspect it with
/// `background_status`, cancel it with `background_abort`, then finish.
fn scripted_stream(requests: Arc<Mutex<Vec<Vec<Message>>>>) -> SharedEventStreamFn {
    let turns = Arc::new(vec![
        assistant(
            vec![tool_call(
                "call-bg",
                "parking-probe",
                json!({ "background": true }),
            )],
            StopReason::ToolUse,
        ),
        assistant(
            vec![tool_call("call-status", "background_status", json!({}))],
            StopReason::ToolUse,
        ),
        assistant(
            vec![tool_call(
                "call-abort",
                "background_abort",
                json!({ "callId": "call-bg" }),
            )],
            StopReason::ToolUse,
        ),
        text_turn("done"),
    ]);
    Arc::new(move |request: ProviderRequest| {
        let turns = Arc::clone(&turns);
        let requests = Arc::clone(&requests);
        Box::pin(async move {
            let index = {
                let mut captured = requests.lock().unwrap();
                captured.push(request.context.messages.clone());
                captured.len() - 1
            };
            let message = turns
                .get(index)
                .or_else(|| turns.last())
                .cloned()
                .expect("at least one scripted turn");
            let stream = AssistantMessageEventStream::new();
            stream.push(AssistantMessageEventStream::done_event(
                TerminalReason::Stop,
                message,
            ));
            stream
        })
    })
}

/// The text of the tool result addressed to `call_id` in the last captured
/// request.
fn tool_result_text(requests: &[Vec<Message>], call_id: &str) -> String {
    let last = requests.last().expect("at least one request");
    last.iter()
        .find_map(|message| match message {
            Message::ToolResult(result) if result.tool_call_id == call_id => Some(
                result
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text(text) => Some(text.text.clone()),
                        _ => None,
                    })
                    .collect::<String>(),
            ),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no tool result for {call_id}"))
}

#[tokio::test]
async fn session_host_exposes_and_aborts_a_background_call_through_the_tools() {
    let probe = Arc::new(ParkingProbe::default());
    let mut tools = registry();
    tools.register(probe.clone());

    let requests: Arc<Mutex<Vec<Vec<Message>>>> = Arc::new(Mutex::new(Vec::new()));
    let session = AgentSession::new(AgentSessionConfig {
        provider_registry: Arc::new(ProviderRegistry::default()),
        model: fake_model(),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools,
        session: None,
        compaction: None,
        max_turns: 8,
        system_prompt: Some("background model tools test".to_owned()),
        cwd: None,
        event_stream: Some(scripted_stream(Arc::clone(&requests))),
        extensions: None,
        tool_selection: None,
    });

    let handler: EventHandler = Arc::new(|_event: AgentEvent| {});
    session
        .prompt("start", handler)
        .await
        .expect("the prompt must complete");

    let captured = requests.lock().unwrap().clone();
    assert_eq!(captured.len(), 4, "one request per scripted turn");

    // `background_status` saw the detached call: the JSON projection carries
    // the id, the tool name and a count of one.
    let status_text = tool_result_text(&captured, "call-status");
    let status: serde_json::Value =
        serde_json::from_str(&status_text).expect("background_status must return JSON");
    assert_eq!(status["count"], json!(1), "status output: {status_text}");
    assert_eq!(status["calls"][0]["callId"], json!("call-bg"));
    assert_eq!(status["calls"][0]["toolName"], json!("parking-probe"));
    assert!(status["calls"][0]["parentToolCallId"].is_null());

    // `background_abort` actually cancelled it (not merely reported zero).
    let abort_text = tool_result_text(&captured, "call-abort");
    let abort: serde_json::Value =
        serde_json::from_str(&abort_text).expect("background_abort must return JSON");
    assert_eq!(abort["aborted"], json!(1), "abort output: {abort_text}");
    assert_eq!(abort["callId"], json!("call-bg"));
    assert!(
        session.background_calls().is_empty(),
        "the aborted call must leave the registry"
    );

    // The detached call observed its own abort signal (bounded wait).
    for _ in 0..400 {
        if probe.aborted.load(Ordering::SeqCst) > 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert_eq!(probe.entered.load(Ordering::SeqCst), 1);
    assert_eq!(
        probe.aborted.load(Ordering::SeqCst),
        1,
        "background_abort must fire the detached call's own signal"
    );
}

/// A scripted stream whose first turn starts a background call and every
/// later turn is plain text — used by the subagent isolation check below.
fn scripted_background_only(requests: Arc<Mutex<Vec<Vec<Message>>>>) -> SharedEventStreamFn {
    let turns = Arc::new(vec![
        assistant(
            vec![tool_call(
                "call-parent-bg",
                "parking-probe",
                json!({ "background": true }),
            )],
            StopReason::ToolUse,
        ),
        text_turn("done"),
    ]);
    Arc::new(move |request: ProviderRequest| {
        let turns = Arc::clone(&turns);
        let requests = Arc::clone(&requests);
        Box::pin(async move {
            let index = {
                let mut captured = requests.lock().unwrap();
                captured.push(request.context.messages.clone());
                captured.len() - 1
            };
            let message = turns
                .get(index)
                .or_else(|| turns.last())
                .cloned()
                .expect("at least one scripted turn");
            let stream = AssistantMessageEventStream::new();
            stream.push(AssistantMessageEventStream::done_event(
                TerminalReason::Stop,
                message,
            ));
            stream
        })
    })
}

/// The session host's status projection reflects the session's OWN registry:
/// two sessions in one process never see each other's calls.
#[tokio::test]
async fn each_session_reports_only_its_own_background_calls() {
    let probe = Arc::new(ParkingProbe::default());

    let build = |requests: Arc<Mutex<Vec<Vec<Message>>>>| {
        let mut tools = registry();
        tools.register(probe.clone());
        AgentSession::new(AgentSessionConfig {
            provider_registry: Arc::new(ProviderRegistry::default()),
            model: fake_model(),
            api_key: None,
            thinking: ThinkingLevel::Off,
            tools,
            session: None,
            compaction: None,
            max_turns: 4,
            system_prompt: Some("background isolation test".to_owned()),
            cwd: None,
            event_stream: Some(scripted_background_only(requests)),
            extensions: None,
            tool_selection: None,
        })
    };

    let first_requests: Arc<Mutex<Vec<Vec<Message>>>> = Arc::new(Mutex::new(Vec::new()));
    let second_requests: Arc<Mutex<Vec<Vec<Message>>>> = Arc::new(Mutex::new(Vec::new()));
    let first = build(Arc::clone(&first_requests));
    let second = build(Arc::clone(&second_requests));

    let handler: EventHandler = Arc::new(|_event: AgentEvent| {});
    first.prompt("start", handler.clone()).await.expect("first");
    second.prompt("start", handler).await.expect("second");

    // Each session owns exactly its own detached call (same scripted id, but
    // separate registries), so neither status view is contaminated.
    assert_eq!(first.background_calls().len(), 1);
    assert_eq!(second.background_calls().len(), 1);
    assert_eq!(first.background_calls()[0].call_id, "call-parent-bg");
    assert_eq!(second.background_calls()[0].call_id, "call-parent-bg");

    // Tear down: abort each session's own call and confirm both drain.
    assert_eq!(first.abort_all_background(), 1);
    assert_eq!(second.abort_all_background(), 1);
    assert!(first.background_calls().is_empty());
    assert!(second.background_calls().is_empty());
}
